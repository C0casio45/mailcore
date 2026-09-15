//! Le consentement : ouvrir le navigateur, attendre la redirection, prendre le code.
//!
//! ## Pourquoi le bouclage et pas un `redirect_uri` hors machine
//!
//! La RFC 8252 le recommande pour une application de bureau : le code revient sur un port
//! local, donc il ne traverse aucun serveur intermédiaire. L'alternative — un
//! `urn:ietf:wg:oauth:2.0:oob` où l'utilisateur recopie un code à la main — a été retirée par
//! Google, et elle faisait passer le code par le presse-papiers.
//!
//! ## Ce que le bouclage n'empêche pas, et ce qu'on fait contre
//!
//! **Un autre processus de la machine peut écouter un port.** Trois choses le neutralisent, et
//! aucune ne suffirait seule :
//!
//! - le port est **éphémère** : le système le choisit, personne ne peut le réserver d'avance ;
//! - l'**état** anti-rejeu est vérifié : un code arrivé sans le bon état est jeté, donc
//!   quelqu'un qui ferait ouvrir notre URL de redirection avec un code de son choix — celui
//!   d'un autre compte, pour faire synchroniser une boîte qui n'est pas la nôtre — n'obtient
//!   rien ;
//! - **PKCE** rend le code inutilisable sans le vérificateur, qui ne quitte pas le processus.
//!
//! ## Un seul écouteur, un seul code, puis on ferme
//!
//! L'écouteur ne sert **qu'une** requête utile et s'arrête. Le laisser ouvert offrirait une
//! porte à qui passe par là, et il n'y a rien à y gagner : le consentement est un événement,
//! pas un service.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

use crate::{Error, Result};

/// Délai d'attente du consentement.
///
/// Cinq minutes : le temps de choisir un compte, de taper un mot de passe, de sortir son
/// téléphone pour la validation en deux étapes. Plus court serait pénible ; plus long
/// laisserait un port ouvert sans raison.
pub const CONSENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Plafond d'une ligne de requête.
///
/// Ce qui arrive ici vient d'un navigateur, mais peut venir de n'importe quoi d'autre : le
/// port est ouvert sur le bouclage. Un client qui n'envoie jamais de fin de ligne ne doit pas
/// faire grossir un tampon sans borne.
const LINE_LIMIT: usize = 16 * 1024;

/// Nombre de requêtes servies avant d'abandonner.
///
/// Un navigateur demande souvent `/favicon.ico` en plus de la redirection, et certains
/// préchargent. Servir quelques requêtes inutiles évite d'abandonner sur un `favicon` ; en
/// servir sans fin transformerait l'écouteur en service.
const MAX_REQUESTS: usize = 10;

/// Pas de scrutation de l'`accept` non bloquant.
///
/// `std` n'a pas d'`accept` avec délai. 50 ms est imperceptible pour un humain qui revient
/// du navigateur, et un socket qui dort ne coûte rien.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Un écouteur de bouclage, prêt à recevoir une redirection.
#[derive(Debug)]
pub struct Loopback {
    listener: TcpListener,
    port: u16,
}

impl Loopback {
    /// Ouvre un port éphémère sur `127.0.0.1`.
    ///
    /// ## `127.0.0.1` et pas `localhost`
    ///
    /// L'adresse littérale ne dépend d'aucune résolution de nom : un fichier `hosts` modifié
    /// ou un résolveur menteur ne peuvent pas faire pointer `localhost` ailleurs. Google
    /// accepte les deux ; il n'y a aucune raison de laisser le choix à un tiers.
    ///
    /// ## Le port est éphémère par défaut, et épinglable quand il faut
    ///
    /// Éphémère par défaut, parce qu'un port fixe se réserve : un processus lancé avant nous
    /// l'occuperait, recevrait le code, et notre écouteur échouerait à démarrer — ou pire,
    /// démarrerait sur un autre port et attendrait pour rien.
    ///
    /// Mais tous les fournisseurs n'autorisent pas le port à varier. Google le fait
    /// explicitement pour le bouclage ; **Microsoft n'a jamais été éprouvé ici** — voir
    /// `docs/PHASE-2.md` — et sa documentation est ambiguë entre `http://localhost`, dont le
    /// port est ignoré, et une adresse littérale de bouclage, dont il ne l'est peut-être pas.
    ///
    /// Un port éphémère sur un fournisseur qui compare l'URI à l'octet ne marcherait **jamais**,
    /// et le message d'erreur du fournisseur ne dirait pas pourquoi. `Some(port)` est la sortie
    /// de secours : enregistrer `http://127.0.0.1:PORT` chez le fournisseur, passer le même ici,
    /// et le consentement redevient possible sans rien deviner.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] si le port ne peut pas être ouvert. Sur un port épinglé, c'est le cas
    /// « déjà occupé », et le dire vaut mieux que de retomber en silence sur un port éphémère
    /// que le fournisseur refusera.
    pub fn open(pinned: Option<u16>) -> Result<Self> {
        let listener =
            TcpListener::bind(("127.0.0.1", pinned.unwrap_or(0))).map_err(Error::Network)?;
        let port = listener.local_addr().map_err(Error::Network)?.port();
        Ok(Self { listener, port })
    }

    /// L'URI de redirection à donner au fournisseur.
    ///
    /// Le chemin est `/` et rien d'autre : Google compare l'URI de redirection **à l'octet**
    /// avec celle enregistrée dans la console — sauf le port, qu'il autorise à varier pour le
    /// bouclage. Une barre oblique en trop et l'échange est refusé.
    #[must_use]
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Attend la redirection et rend le code d'autorisation.
    ///
    /// ## L'état est vérifié avant tout
    ///
    /// Un code arrivé avec un mauvais état, ou sans état, est **jeté**. C'est ce qui empêche
    /// quelqu'un de nous faire échanger un code qu'il a obtenu ailleurs.
    ///
    /// # Errors
    ///
    /// [`Error::Consent`] si le fournisseur a renvoyé une erreur — l'utilisateur a refusé, par
    /// exemple — ou si le délai est dépassé, ou si aucune requête exploitable n'arrive.
    /// [`Error::Network`] sur le socket.
    pub fn wait_for_code(&self, expected_state: &str) -> Result<String> {
        self.wait_until(std::time::Instant::now() + CONSENT_TIMEOUT, expected_state)
    }

    /// La même attente, avec une échéance choisie.
    ///
    /// Séparée pour une seule raison : **le dépassement de délai est testable**. Vérifier que
    /// cinq minutes finissent par expirer demanderait un test de cinq minutes, donc personne
    /// ne l'écrirait, donc le chemin qui compte — celui où l'utilisateur ferme l'onglet — ne
    /// serait jamais exécuté.
    fn wait_until(&self, deadline: std::time::Instant, expected_state: &str) -> Result<String> {
        // **L'attente porte sur `accept`, pas seulement sur la lecture.** Un délai posé
        // uniquement sur le socket accepté ne servirait à rien dans le cas qui arrive
        // vraiment : l'utilisateur ferme l'onglet, personne ne se connecte, et un `accept`
        // bloquant attend pour toujours. Le CLI resterait pendu sans rien dire.
        //
        // `std` n'a pas d'`accept` avec délai, donc le socket est non bloquant et on repasse.
        // 50 ms est imperceptible pour un humain qui revient du navigateur et ne coûte rien.
        self.listener
            .set_nonblocking(true)
            .map_err(Error::Network)?;
        let mut served = 0_usize;

        while served < MAX_REQUESTS {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                // Le message ne nomme pas la durée : `wait_until` ne la connaît pas, et une
                // durée inventée dans un message d'erreur est un mensonge de plus à
                // maintenir.
                return Err(Error::Consent {
                    reason: "délai d'attente dépassé sans redirection".to_owned(),
                });
            }

            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL_INTERVAL.min(remaining));
                    continue;
                }
                Err(error) => return Err(Error::Network(error)),
            };
            served += 1;

            // Le socket accepté hérite du mode non bloquant sur certaines plateformes, et un
            // `read` sur un socket non bloquant rend `WouldBlock` au lieu d'attendre les
            // octets. On le remet en mode bloquant, avec un délai.
            stream.set_nonblocking(false).map_err(Error::Network)?;
            stream.set_read_timeout(Some(remaining)).ok();
            stream.set_write_timeout(Some(remaining)).ok();

            match self.serve(stream, expected_state) {
                // Une requête sans code — un `favicon`, un préchargement — ne termine pas
                // l'attente.
                Ok(None) => continue,
                Ok(Some(code)) => return Ok(code),
                Err(error) => return Err(error),
            }
        }
        Err(Error::Consent {
            reason: format!("{MAX_REQUESTS} requêtes reçues sans code exploitable"),
        })
    }

    /// Sert une requête. `Ok(None)` quand elle ne portait pas de code.
    fn serve(&self, mut stream: TcpStream, expected_state: &str) -> Result<Option<String>> {
        let line = {
            let mut reader = BufReader::new(&stream);
            let mut raw = Vec::new();
            use std::io::Read as _;
            let read = reader
                .by_ref()
                .take(LINE_LIMIT as u64 + 1)
                .read_until(b'\n', &mut raw)
                .map_err(Error::Network)?;
            if read == 0 || read > LINE_LIMIT {
                return Ok(None);
            }
            String::from_utf8_lossy(&raw).into_owned()
        };

        let Some(target) = request_target(&line) else {
            return Ok(None);
        };
        let params = query_params(&target);

        // L'utilisateur a refusé, ou le fournisseur a refusé pour lui. Sa description est la
        // seule information utile, et elle ne contient aucun secret.
        if let Some(code) = params.get("error").map(String::as_str) {
            let description = params
                .get("error_description")
                .map_or("sans description", String::as_str);
            respond(
                &mut stream,
                "Autorisation refusée",
                "Le fournisseur a refusé l'autorisation. Rien n'a été enregistré.",
            );
            return Err(Error::Consent {
                reason: format!("{code} : {description}"),
            });
        }

        let Some(code) = params.get("code").filter(|it| !it.is_empty()) else {
            return Ok(None);
        };

        // **L'état d'abord.** Un code sans le bon état vient d'ailleurs.
        match params.get("state") {
            Some(state) if state == expected_state => {}
            _ => {
                respond(
                    &mut stream,
                    "Requête rejetée",
                    "L'état de la requête ne correspond pas. Rien n'a été enregistré.",
                );
                return Err(Error::Consent {
                    reason: "état anti-rejeu absent ou faux : redirection ignorée".to_owned(),
                });
            }
        }

        respond(
            &mut stream,
            "Compte autorisé",
            "Vous pouvez fermer cet onglet et revenir au terminal.",
        );
        Ok(Some(code.clone()))
    }
}

/// La cible d'une ligne de requête `GET /?code=… HTTP/1.1`.
fn request_target(line: &str) -> Option<String> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    if !method.eq_ignore_ascii_case("GET") {
        return None;
    }
    Some(parts.next()?.to_owned())
}

/// Les paramètres de requête d'une cible, décodés.
fn query_params(target: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some((_, query)) = target.split_once('?') else {
        return out;
    };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(key), percent_decode(value));
    }
    out
}

/// Décode une valeur encodée en pourcent.
///
/// ## Les deux formes de l'espace sont acceptées
///
/// `%20` et `+`. Le second est une convention des formulaires, pas de la RFC 3986, et un
/// navigateur peut envoyer l'un ou l'autre. Ne traiter que `%20` laisserait un `+` littéral
/// dans un code, qui serait alors refusé à l'échange.
///
/// Une séquence `%` mal formée est laissée **telle quelle** plutôt que jetée : le résultat
/// sera refusé par le fournisseur, ce qui est un meilleur diagnostic qu'un code
/// silencieusement tronqué.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|it| u8::from_str_radix(it, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Répond une petite page, et ferme.
///
/// Les échecs sont ignorés : le navigateur a peut-être déjà raccroché, et on a ce qu'on
/// voulait. Faire échouer le consentement parce que la page de remerciement n'est pas partie
/// serait absurde.
fn respond(stream: &mut TcpStream, title: &str, message: &str) {
    // Pas de CSS distant, pas de police, pas d'image : cette page est servie par nous et ne
    // doit rien aller chercher. C'est la même règle que pour le corps d'un message
    // (`docs/PRIVACY.md` §1), appliquée à notre propre page.
    let body = format!(
        "<!doctype html><html lang=\"fr\"><head><meta charset=\"utf-8\">\
         <title>{title}</title></head>\
         <body style=\"font-family:system-ui,sans-serif;margin:3rem;max-width:32rem\">\
         <h1 style=\"font-size:1.2rem\">{title}</h1><p>{message}</p>\
         <p style=\"color:#666;font-size:0.9rem\">mailcore</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Les lanceurs à essayer, dans l'ordre, pour la plateforme courante.
///
/// ## L'ordre de Windows est mesuré, pas déduit
///
/// Il l'a été deux fois, et la première fois il était faux. `cargo xtask browser-probe` ouvre
/// un port éphémère sur le bouclage, donne son URL à chaque lanceur candidat et attend une
/// **connexion entrante** : c'est la seule preuve qu'un navigateur s'est ouvert. Relevé du
/// 2026-09-08, longueurs 40, 420, 1000 et 2048 caractères, URL bourrée de `%3A`, de `%2F%2F`
/// et de `&`, navigateur par défaut Chrome :
///
/// | lanceur                                     | connexion | sortie |
/// |---------------------------------------------|-----------|--------|
/// | `rundll32.exe url.dll,FileProtocolHandler`   | oui, 0,2 s | 0     |
/// | `powershell -NoProfile -Command Start-Process` | oui, 0,5 s | 0   |
/// | `cmd /d /c start "" "<url>"`                 | oui, 0,2 s | 0     |
/// | `explorer.exe <url>`                         | **non**   | 1      |
/// | `rundll32.exe shell32.dll,ShellExec_RunDLL`  | **non**   | 0      |
///
/// Aux quatre longueurs, à l'identique. **L'hypothèse de la longueur est donc fausse** : la
/// note précédente donnait `rundll32` pour muet au-delà de 418 caractères et faisait passer
/// `explorer.exe` devant sur cette base. C'est l'inverse : `rundll32` ouvre l'onglet à 2048
/// caractères, et `explorer.exe` n'ouvre **jamais** le navigateur — il ouvre l'explorateur de
/// fichiers, et rend 1 en le faisant. Ce qui avait été pris pour une limite de longueur ne l'a
/// pas été reproduit ; on ne sait toujours pas ce qui avait été observé ce jour-là, et c'est
/// exactement pourquoi la sonde existe.
///
/// Les deux lanceurs qui échouent sont retirés. Un lanceur qui démarre sans rien ouvrir est
/// pire qu'un lanceur absent : `open_browser` s'arrête au premier `spawn` réussi, donc
/// `explorer.exe` en tête empêchait les suivants d'être essayés.
///
/// ## `cmd /c start` reste écarté, mais pas pour la raison qu'on croyait
///
/// La sonde le montre gagnant, URL reçue **intacte** : `cmd /c` ne développe pas `%2F%2F`,
/// contrairement à ce qui était écrit ici — la disparition des `%XX` non définis est le
/// comportement des fichiers de commandes, pas de la ligne de commande. La vraie raison est
/// mécanique : `start` traite le `&` comme un séparateur de commandes, et il faut donc des
/// guillemets **autour de l'URL dans la ligne de commande elle-même**. `Command::args` ne les
/// ajoute que pour les arguments contenant une espace, parce qu'il cite selon les règles de
/// `CommandLineToArgvW` — que `cmd` ne suit pas. Le faire correctement demanderait `raw_arg`,
/// c'est-à-dire écrire l'échappement d'un shell à la main sur le chemin d'un code
/// d'autorisation. La sonde le mesure, ce module ne l'utilise pas.
///
/// ## Pourquoi PowerShell en second et pas en premier
///
/// Il marche, mais il coûte une demi-seconde de démarrage de runtime là où `rundll32` répond
/// en deux dixièmes, et c'est un interpréteur : l'URL y voyage dans une chaîne entre
/// apostrophes. Une URL de consentement n'en contient jamais, mais l'invariant tient par
/// convention et pas par construction. Il est le filet pour une machine sans `rundll32`.
fn launchers(url: &str) -> Vec<(&'static str, Vec<String>)> {
    if cfg!(target_os = "windows") {
        vec![
            (
                "rundll32.exe",
                vec!["url.dll,FileProtocolHandler".to_owned(), url.to_owned()],
            ),
            (
                "powershell.exe",
                vec![
                    "-NoProfile".to_owned(),
                    "-NonInteractive".to_owned(),
                    "-Command".to_owned(),
                    format!("Start-Process '{url}'"),
                ],
            ),
        ]
    } else if cfg!(target_os = "macos") {
        vec![("open", vec![url.to_owned()])]
    } else {
        // `xdg-open` d'abord — la convention — puis les lanceurs des deux environnements de
        // bureau majeurs, pour une session sans `xdg-utils` installé.
        vec![
            ("xdg-open", vec![url.to_owned()]),
            ("gio", vec!["open".to_owned(), url.to_owned()]),
        ]
    }
}

/// Ouvre une URL dans le navigateur de l'utilisateur.
///
/// ## Ce que cette fonction ne peut pas promettre, et il faut le lire
///
/// **Rien ne prouve qu'un navigateur s'est ouvert.** `spawn` réussi veut dire « le processus a
/// démarré », rien de plus. Mesuré le 2026-09-08 : `rundll32.exe shell32.dll,ShellExec_RunDLL`
/// rend **0** sans qu'aucun onglet n'apparaisse, et `explorer.exe <url>` démarre très bien pour
/// ouvrir l'explorateur de fichiers. Un lanceur ne rend pas compte de ce que fait le shell
/// derrière lui, et son code de sortie encore moins.
///
/// C'est pour ça que l'appelant **affiche l'URL avant** d'appeler ici, et que l'échec n'est pas
/// fatal : un consentement qui demande un copier-coller reste un consentement. Faire dépendre
/// le consentement d'une promesse qu'on ne peut pas vérifier serait pire que de ne pas essayer.
///
/// Les lanceurs sont essayés dans l'ordre jusqu'à ce que l'un démarre. Ça ne rattrape que le
/// lanceur **absent** — pas le lanceur qui réussit sans rien faire, qui reste couvert par
/// l'affichage de l'URL.
///
/// # Errors
///
/// [`Error::Network`] si aucun lanceur ne démarre.
pub fn open_browser(url: &str) -> Result<()> {
    let mut last = None;
    for (program, args) in launchers(url) {
        match std::process::Command::new(program)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => return Ok(()),
            Err(error) => {
                tracing::debug!(program, %error, "lanceur indisponible");
                last = Some(error);
            }
        }
    }
    Err(Error::Network(last.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "aucun lanceur de navigateur pour cette plateforme",
        )
    })))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// Envoie une ligne de requête à l'écouteur et rend sa réponse.
    fn request(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        // **Un délai de lecture, obligatoirement.** Une connexion acceptée par le système mais
        // que l'écouteur ne servira jamais — parce qu'il a atteint `MAX_REQUESTS` — laisse ce
        // client attendre pour toujours. C'est ce qui a fait tourner cette suite pendant plus
        // de deux minutes avant d’être coupée. Une seconde suffit largement sur le bouclage.
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        write!(stream, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        stream.flush().unwrap();
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply);
        reply
    }

    #[test]
    fn the_redirect_uri_is_the_literal_loopback_with_a_trailing_slash() {
        // Google compare l'URI **à l'octet** avec celle enregistrée dans la console : une
        // barre en trop ou en moins fait refuser l'échange.
        let loopback = Loopback::open(None).unwrap();
        let uri = loopback.redirect_uri();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(uri.ends_with('/'), "{uri}");
        // Pas `localhost` : une résolution de nom est une dépendance de trop sur le chemin
        // d'un code d'autorisation.
        assert!(!uri.contains("localhost"), "{uri}");
    }

    #[test]
    fn two_listeners_get_two_ports() {
        // Un port fixe se réserve : un processus lancé avant nous recevrait le code.
        let first = Loopback::open(None).unwrap();
        let second = Loopback::open(None).unwrap();
        assert_ne!(first.redirect_uri(), second.redirect_uri());
    }

    #[test]
    fn a_redirect_with_the_right_state_yields_the_code() {
        let loopback = Loopback::open(None).unwrap();
        let port = loopback.port;
        let caller = std::thread::spawn(move || request(port, "/?code=4/ABC-def&state=etat42"));

        let code = loopback.wait_for_code("etat42").unwrap();
        assert_eq!(code, "4/ABC-def");

        let reply = caller.join().unwrap();
        assert!(reply.contains("200 OK"), "{reply}");
        assert!(reply.contains("Compte autorisé"), "{reply}");
    }

    #[test]
    fn a_percent_encoded_code_is_decoded() {
        // Un code de Google contient des `/`, que le navigateur encode. Non décodé, il est
        // refusé à l'échange.
        let loopback = Loopback::open(None).unwrap();
        let port = loopback.port;
        let caller = std::thread::spawn(move || request(port, "/?code=4%2F0AX4%2BBse&state=etat"));

        assert_eq!(loopback.wait_for_code("etat").unwrap(), "4/0AX4+Bse");
        caller.join().unwrap();
    }

    #[test]
    fn a_wrong_state_is_rejected() {
        // **Sans ça**, n'importe qui pourrait nous faire échanger un code obtenu ailleurs —
        // celui d'un autre compte, pour faire synchroniser une boîte qui n'est pas la nôtre.
        let loopback = Loopback::open(None).unwrap();
        let port = loopback.port;
        let caller = std::thread::spawn(move || request(port, "/?code=vole&state=pas-le-bon"));

        let error = loopback.wait_for_code("le-bon").unwrap_err();
        assert!(matches!(error, Error::Consent { .. }), "{error:?}");
        assert!(error.to_string().contains("anti-rejeu"), "{error}");

        let reply = caller.join().unwrap();
        assert!(reply.contains("rejetée"), "{reply}");
    }

    #[test]
    fn a_code_without_any_state_is_rejected() {
        let loopback = Loopback::open(None).unwrap();
        let port = loopback.port;
        let caller = std::thread::spawn(move || request(port, "/?code=sans-etat"));

        assert!(matches!(
            loopback.wait_for_code("le-bon"),
            Err(Error::Consent { .. })
        ));
        caller.join().unwrap();
    }

    #[test]
    fn a_provider_error_is_reported_with_its_description() {
        // L'utilisateur a cliqué « Annuler ». Le dire est mieux que d'attendre cinq minutes.
        let loopback = Loopback::open(None).unwrap();
        let port = loopback.port;
        let caller = std::thread::spawn(move || {
            request(
                port,
                "/?error=access_denied&error_description=L%27utilisateur%20a%20refus%C3%A9",
            )
        });

        let error = loopback.wait_for_code("etat").unwrap_err();
        assert!(matches!(error, Error::Consent { .. }), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("access_denied"), "{message}");
        assert!(message.contains("refusé"), "{message}");
        caller.join().unwrap();
    }

    #[test]
    fn a_favicon_request_does_not_end_the_wait() {
        // Un navigateur en demande un en plus de la redirection. Abandonner dessus ferait
        // échouer un consentement par ailleurs réussi.
        let loopback = Loopback::open(None).unwrap();
        let port = loopback.port;
        let caller = std::thread::spawn(move || {
            let first = request(port, "/favicon.ico");
            let second = request(port, "/?code=le-code&state=etat");
            (first, second)
        });

        assert_eq!(loopback.wait_for_code("etat").unwrap(), "le-code");
        caller.join().unwrap();
    }

    #[test]
    fn the_served_page_fetches_nothing_from_the_network() {
        // La même règle que pour le corps d'un message, appliquée à notre propre page : rien
        // ne doit sortir. Une page de remerciement qui charge une police est une requête vers
        // un tiers au moment le plus sensible.
        let loopback = Loopback::open(None).unwrap();
        let port = loopback.port;
        let caller = std::thread::spawn(move || request(port, "/?code=c&state=e"));
        loopback.wait_for_code("e").unwrap();
        let page = caller.join().unwrap();

        for forbidden in ["http://", "https://", "//fonts", "<img", "<script", "<link"] {
            assert!(
                !page.contains(forbidden),
                "la page contient {forbidden} : {page}"
            );
        }
    }

    #[test]
    fn nobody_connecting_ends_in_a_timeout_and_not_in_a_hang() {
        // **Le cas qui arrive vraiment** : l'utilisateur ferme l'onglet du navigateur sans
        // rien valider. Le premier jet posait le délai sur le socket *accepté*, donc `accept`
        // attendait pour toujours et le CLI restait pendu sans un mot.
        let loopback = Loopback::open(None).unwrap();
        let started = std::time::Instant::now();
        let error = loopback
            .wait_until(started + std::time::Duration::from_millis(300), "etat")
            .unwrap_err();

        assert!(matches!(error, Error::Consent { .. }), "{error:?}");
        assert!(error.to_string().contains("délai"), "{error}");
        // Le contrôle dans l'autre sens : un `wait_until` qui rendrait l'erreur sans attendre
        // passerait l'assertion ci-dessus en ne prouvant rien.
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(300),
            "l'attente a rendu la main trop tôt : {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_flood_of_useless_requests_does_not_keep_the_port_open_for_ever() {
        // L'écouteur est un événement, pas un service. Un client qui parle sans jamais
        // apporter de code doit finir par se faire raccrocher au nez.
        let loopback = Loopback::open(None).unwrap();
        let port = loopback.port;
        // **Exactement `MAX_REQUESTS`, et pas une de plus.** Une requête au-delà serait
        // acceptée par le système mais jamais servie, et le client attendrait son propre délai
        // — quatre secondes de plus dans une suite qui tourne à chaque commit, pour ne rien
        // prouver que la borne ne prouve déjà.
        let flood = std::thread::spawn(move || {
            for _ in 0..MAX_REQUESTS {
                let _ = std::panic::catch_unwind(|| request(port, "/favicon.ico"));
            }
        });

        let error = loopback
            .wait_until(
                std::time::Instant::now() + std::time::Duration::from_secs(30),
                "etat",
            )
            .unwrap_err();
        assert!(matches!(error, Error::Consent { .. }), "{error:?}");
        assert!(error.to_string().contains("sans code"), "{error}");

        // Fermer le port avant de rejoindre : les connexions restées dans la file d'attente du
        // système sont alors refusées d'un coup, au lieu d'attendre chacune son délai.
        drop(loopback);
        let _ = flood.join();
    }

    #[test]
    fn the_url_reaches_every_launcher_whole() {
        // Le point de tout ce module : une URL de consentement est pleine de `%XX` et de `&`.
        // Chaque lanceur la reçoit dans **un seul argument**, jamais découpée sur ses `&` —
        // c'est ce qui écarte `cmd /c start`, à qui `Command::args` ne saurait pas poser les
        // guillemets dont son propre redécoupage a besoin.
        let url = "https://accounts.google.com/o/oauth2/v2/auth?redirect_uri=http%3A%2F%2F127.0.0.1%3A63811%2F&scope=https%3A%2F%2Fmail.google.com%2F&state=a_b-c";
        let launchers = launchers(url);
        assert!(!launchers.is_empty(), "aucun lanceur pour cette plateforme");
        for (program, args) in &launchers {
            // `contains` et non l'égalité : PowerShell la reçoit enrobée dans
            // `Start-Process '…'`, ce qui reste un argument unique et une URL entière.
            assert!(
                args.iter().any(|it| it.contains(url)),
                "{program} ne reçoit pas l'URL entière : {args:?}"
            );
            assert!(
                !program.eq_ignore_ascii_case("cmd.exe") && !program.eq_ignore_ascii_case("cmd"),
                "`start` couperait l'URL sur son premier `&` faute de guillemets"
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn rundll32_is_first_and_the_two_launchers_that_open_nothing_are_gone() {
        // **Mesuré, pas déduit** : `cargo xtask browser-probe` attend une connexion sur le
        // bouclage, la seule preuve qu'un onglet s'est ouvert. Relevé du 2026-09-08 aux
        // longueurs 40, 420, 1000 et 2048 caractères, identique aux quatre :
        //
        // - `rundll32 url.dll,FileProtocolHandler` : connexion en 0,2 s, URL intacte ;
        // - `powershell … Start-Process` : connexion en 0,5 s, URL intacte ;
        // - `explorer.exe <url>` : jamais de connexion, sortie 1 — il ouvre l'explorateur ;
        // - `rundll32 shell32.dll,ShellExec_RunDLL` : jamais de connexion, sortie **0**.
        //
        // L'ordre n'est donc pas un détail de goût, et il ne peut pas non plus être rattrapé à
        // l'exécution : `open_browser` s'arrête au premier `spawn` réussi, et les deux
        // lanceurs muets démarrent parfaitement.
        let names: Vec<&str> = launchers("https://exemple.invalid/")
            .into_iter()
            .map(|(program, _)| program)
            .collect();
        assert_eq!(names, vec!["rundll32.exe", "powershell.exe"]);
        // La note précédente faisait passer `explorer.exe` en tête sur une limite de longueur
        // que la sonde n'a pas reproduite. Le verrou tient dans les deux sens.
        assert!(
            !names
                .iter()
                .any(|it| it.eq_ignore_ascii_case("explorer.exe")),
            "explorer.exe n'ouvre pas le navigateur : {names:?}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_powershell_fallback_does_not_let_the_url_escape_its_quotes() {
        // L'URL voyage entre apostrophes simples, où PowerShell n'interpole rien. Une
        // apostrophe dans l'URL romprait la chaîne — une URL de consentement n'en contient
        // jamais, et si un fournisseur s'y mettait, ce test le dirait avant l'utilisateur.
        let url = "https://accounts.google.com/o/oauth2/v2/auth?scope=https%3A%2F%2Fmail.google.com%2F&state=a_b-c";
        assert!(!url.contains('\''), "l'URL sondée doit être un cas propre");
        let (_, args) = launchers(url)
            .into_iter()
            .find(|(program, _)| *program == "powershell.exe")
            .unwrap();
        assert!(args.contains(&"-NoProfile".to_owned()), "{args:?}");
        assert!(
            args.iter()
                .any(|it| it == &format!("Start-Process '{url}'")),
            "{args:?}"
        );
    }

    // ------------------------------------------------------------------
    // Le découpage. L'entrée vient d'un port ouvert sur le bouclage.
    // ------------------------------------------------------------------

    #[test]
    fn only_a_get_is_read() {
        assert_eq!(
            request_target("GET /?code=a HTTP/1.1").as_deref(),
            Some("/?code=a")
        );
        assert_eq!(
            request_target("get /?code=a HTTP/1.1").as_deref(),
            Some("/?code=a")
        );
        assert!(request_target("POST / HTTP/1.1").is_none());
        assert!(request_target("").is_none());
        assert!(request_target("GET").is_none());
    }

    #[test]
    fn a_target_without_a_query_yields_no_parameter() {
        assert!(query_params("/").is_empty());
        assert!(query_params("/favicon.ico").is_empty());
    }

    #[test]
    fn parameters_are_split_and_decoded() {
        let params =
            query_params("/?code=4%2Fabc&state=xyz&scope=https%3A%2F%2Fmail.google.com%2F");
        assert_eq!(params.get("code").unwrap(), "4/abc");
        assert_eq!(params.get("state").unwrap(), "xyz");
        assert_eq!(params.get("scope").unwrap(), "https://mail.google.com/");
    }

    #[test]
    fn both_forms_of_the_space_are_decoded() {
        // `+` est une convention des formulaires, pas de la RFC 3986, et un navigateur peut
        // envoyer l'un ou l'autre.
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
    }

    #[test]
    fn a_malformed_percent_sequence_is_left_alone() {
        // Le résultat sera refusé par le fournisseur, ce qui est un meilleur diagnostic qu'un
        // code silencieusement tronqué.
        assert_eq!(percent_decode("a%"), "a%");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%zz"), "a%zz");
        assert_eq!(percent_decode("%"), "%");
    }

    #[test]
    fn a_decoded_value_may_carry_utf8() {
        assert_eq!(percent_decode("refus%C3%A9"), "refusé");
    }

    #[test]
    fn decoding_never_panics_whatever_the_input() {
        for candidate in ["", "%%%%", "%FF%FE", "+++", "%00", "a=b=c", "%C3"] {
            let _ = percent_decode(candidate);
        }
    }

    #[test]
    fn a_parameter_without_a_value_is_empty_not_missing() {
        let params = query_params("/?code=&state=x");
        assert_eq!(params.get("code").unwrap(), "");
        // Et un code vide ne compte pas comme un code : `serve` le filtre.
        assert!(params.get("code").filter(|it| !it.is_empty()).is_none());
    }
}

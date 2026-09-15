//! Quel lanceur ouvre **vraiment** le navigateur, sur cette machine, à cette longueur d'URL.
//!
//! ## Pourquoi cet outil existe
//!
//! `mailauth::consent::open_browser` essaie une liste de lanceurs jusqu'à ce que l'un
//! **démarre**. Or démarrer n'est pas ouvrir. L'ordre de cette liste avait été établi par
//! déduction — « `rundll32` doit avoir une limite de longueur », « `explorer.exe` doit
//! déléguer au shell de session » — et les deux déductions étaient fausses. Un lanceur qui
//! réussit sans rien faire est indistinguable d'un lanceur qui marche, vu de l'appelant. Il
//! fallait un observateur de l'autre côté.
//!
//! ## La preuve est la connexion, jamais le code de sortie
//!
//! On ouvre un écouteur sur un port éphémère du bouclage, on donne son URL au lanceur, et on
//! attend une connexion entrante. Si elle arrive, un navigateur a bien été ouvert **et** il a
//! résolu l'URL entière. Rien d'autre ne le prouve. Le code de sortie est affiché à côté,
//! uniquement pour montrer à quel point il est muet : deux des cinq candidats rendent 0 sans
//! rien ouvrir.
//!
//! ## L'URL est comparée à l'octet
//!
//! Le second soupçon portait sur `cmd`, qui développe les variables d'environnement : une URL
//! OAuth2 est pleine de `%XX` — `%3A`, `%2F` — et `%2F%2F` pourrait se lire comme une variable
//! vide qui disparaît. L'écouteur compare donc la cible reçue à celle envoyée, caractère par
//! caractère : un lanceur qui ouvre un onglet sur une URL tronquée est un échec, pas un
//! succès. **Mesuré, ça ne se produit pas** — l'effacement des variables non définies est le
//! comportement des fichiers de commandes, pas de `cmd /c`. La colonne reste, parce que c'est
//! elle qui a permis de le dire au lieu de le supposer.
//!
//! ## Un écouteur par candidat
//!
//! Chacun a donc son port. Un écouteur partagé rendrait le relevé ambigu au moment où il
//! compte : une connexion en retard du candidat précédent serait comptée pour le suivant, et
//! c'est précisément la ligne « aucune connexion » qui décide de l'ordre.
//!
//! ## Rien ne sort de la machine
//!
//! Aucun compte, aucun secret, aucune requête sortante : l'URL sondée pointe sur `127.0.0.1`.
//! Chaque lanceur qui fonctionne ouvre un onglet dans le navigateur de l'utilisateur — c'est le
//! résultat attendu, pas un effet de bord.
//!
//! ## Ce que le relevé du 2026-09-08 a donné
//!
//! Aux longueurs 40, 420, 1000 et 2048 caractères, à l'identique, navigateur par défaut
//! Chrome : `rundll32 url.dll,FileProtocolHandler` (0,2 s), `cmd /d /c start "" "<url>"`
//! (0,2 s) et `powershell … Start-Process` (0,5 s) ouvrent l'onglet avec l'URL intacte ;
//! `explorer.exe <url>` n'ouvre jamais le navigateur et rend 1 ; `rundll32
//! shell32.dll,ShellExec_RunDLL` n'ouvre rien et rend 0. **La longueur n'est pas un facteur.**

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// Pas de scrutation de l'`accept` non bloquant.
///
/// `std` n'a pas d'`accept` avec délai. 25 ms est cent fois plus fin que le démarrage d'un
/// navigateur et ne coûte rien.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Plafond d'une ligne de requête lue sur le bouclage.
///
/// Une URL de 420 caractères tient largement ; la borne est là parce que le port est ouvert et
/// qu'un client qui n'envoie jamais de fin de ligne ne doit pas faire grossir un tampon.
const LINE_LIMIT: usize = 16 * 1024;

/// Requêtes servies par candidat avant d'abandonner.
///
/// Un navigateur demande souvent `/favicon.ico` en plus de la page. Quelques requêtes
/// inutiles ne doivent pas faire conclure à un échec.
const MAX_REQUESTS: usize = 8;

/// Le motif dont la chaîne de requête est bourrée, répété jusqu'à la longueur demandée.
///
/// Il n'est pas fait de caractères neutres, et c'est délibéré : il porte les trois choses
/// soupçonnées de casser un lanceur — `%3A`, la paire `%2F%2F` qu'un shell pourrait avaler, et
/// le `&` que `start` prend pour un séparateur de commandes. Une URL bourrée de `aaaa` ne
/// mesurerait que la longueur, alors qu'on veut mesurer la longueur **et** l'intégrité, sans
/// quoi on ne saurait pas laquelle des deux a fait échouer un candidat.
const PAD_UNIT: &str = "k=a%3Ab%2F%2Fc&";

/// Le remplissage des derniers caractères, quand il reste moins d'un motif.
///
/// Une lettre ordinaire, jamais un `%` : une séquence de pourcent tronquée en fin d'URL serait
/// une variable de plus dans une mesure qui en a déjà deux.
const PAD_FILLER: char = 'z';

/// Un lanceur candidat.
struct Candidate {
    /// Ce qui s'affiche dans le tableau. La commande telle qu'on l'écrirait à la main.
    label: String,
    /// L'exécutable.
    program: &'static str,
    /// Ce qu'on lui passe.
    line: Line,
}

/// Ce qu'on donne à `Command` pour un candidat.
enum Line {
    /// Arguments passés un par un. `Command` les cite selon les règles de
    /// `CommandLineToArgvW`, donc chaque argument arrive **entier** chez un exécutable
    /// ordinaire.
    Args(Vec<String>),
    /// Ligne de commande brute.
    ///
    /// Nécessaire pour `cmd`, qui ne suit pas ces règles : il redécoupe lui-même, et il faut
    /// donc contrôler les guillemets au caractère près pour que le `&` de l'URL ne soit pas lu
    /// comme un séparateur de commandes. `raw_arg` n'est pas `unsafe` — c'est une extension
    /// Windows de `Command`, pas un contournement du typage.
    #[cfg(windows)]
    Raw(String),
}

/// Le relevé d'un candidat.
struct Outcome {
    label: String,
    /// Le lanceur a-t-il démarré ? Un `Err` ici veut dire « exécutable absent ».
    spawned: Result<(), String>,
    /// Son code de sortie, s'il s'est terminé avant l'échéance. Affiché pour montrer qu'il ne
    /// dit rien.
    status: Option<i32>,
    /// Le délai jusqu'à la connexion, si elle est venue. **La seule colonne qui prouve.**
    hit: Option<Duration>,
    /// La cible reçue, quand elle diffère de celle envoyée.
    mangled: Option<String>,
}

/// Mesure chaque lanceur candidat et affiche le tableau.
///
/// `length` est la longueur totale de l'URL sondée, en caractères. La valeur par défaut
/// reproduit une URL de consentement Google réelle ; une valeur courte sert de témoin pour
/// savoir si la longueur est bien le facteur.
///
/// # Errors
///
/// Si aucun port du bouclage ne peut être ouvert, ou si la longueur demandée est plus courte
/// que le préfixe incompressible de l'URL.
pub fn probe(length: usize, wait: Duration) -> Result<()> {
    println!("Sonde des lanceurs de navigateur");
    println!("  longueur d'URL visée : {length} caractères");
    println!("  attente par candidat : {:.1} s", wait.as_secs_f64());
    println!(
        "  la preuve est la connexion reçue sur le bouclage, pas le code de sortie du lanceur"
    );
    println!();

    // Le nombre de candidats ne dépend pas de l'URL, mais leur construction si : on en fait un
    // jeu jetable sur un port arbitraire juste pour le compter.
    let count = candidates(&build_url(0, length.max(40))?.0).len();
    if count == 0 {
        bail!("aucun lanceur candidat connu pour cette plateforme");
    }

    // Un écouteur **par candidat**, et donc un port par candidat. Un écouteur partagé rendrait
    // le relevé ambigu : une connexion tardive du lanceur précédent — un navigateur lent à
    // démarrer, un onglet restauré — serait comptée pour le suivant. Le port est l'étiquette.
    let mut outcomes = Vec::new();
    for index in 0..count {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let (url, target) = build_url(port, length)?;
        let Some(candidate) = candidates(&url).into_iter().nth(index) else {
            break;
        };
        println!("[{index}] {}", candidate.label);
        outcomes.push(run(&listener, candidate, &target, wait));
    }
    report(&outcomes, length);
    Ok(())
}

/// Lance un candidat et attend sa connexion.
fn run(listener: &TcpListener, candidate: Candidate, target: &str, wait: Duration) -> Outcome {
    let mut outcome = Outcome {
        label: candidate.label,
        spawned: Ok(()),
        status: None,
        hit: None,
        mangled: None,
    };

    let started = Instant::now();
    let mut child = match spawn(candidate.program, &candidate.line) {
        Ok(child) => child,
        Err(error) => {
            println!("      lanceur indisponible : {error}");
            outcome.spawned = Err(error.to_string());
            return outcome;
        }
    };

    // L'écouteur ne bloque pas : on veut rendre la main à l'échéance même si personne ne se
    // connecte, ce qui est précisément le cas qu'on cherche à détecter.
    if let Err(error) = listener.set_nonblocking(true) {
        outcome.spawned = Err(error.to_string());
        return outcome;
    }

    let deadline = started + wait;
    let mut served = 0_usize;
    while served < MAX_REQUESTS && Instant::now() < deadline {
        // Le code de sortie est relevé au passage. Il n'entre dans aucune décision : les deux
        // lanceurs qui échouent rendent 0.
        if outcome.status.is_none()
            && let Ok(Some(status)) = child.try_wait()
        {
            outcome.status = Some(status.code().unwrap_or(-1));
        }

        let Ok((stream, _)) = listener.accept() else {
            std::thread::sleep(POLL_INTERVAL);
            continue;
        };
        served += 1;
        let elapsed = started.elapsed();

        let Some(received) = serve(stream, target) else {
            continue;
        };
        // Une requête de `favicon` prouve qu'un navigateur est là, mais ce n'est pas la
        // preuve qu'on cherche : elle n'apprend rien sur l'intégrité de l'URL.
        if !received.starts_with("/?") {
            println!("      (requête annexe {received} ignorée)");
            continue;
        }

        outcome.hit = Some(elapsed);
        if received == target {
            println!(
                "      connexion en {:.2} s, URL intacte",
                elapsed.as_secs_f64()
            );
        } else {
            println!(
                "      connexion en {:.2} s, mais URL ALTÉRÉE ({} caractères reçus sur {})",
                elapsed.as_secs_f64(),
                received.len(),
                target.len()
            );
            outcome.mangled = Some(received);
        }
        break;
    }

    if outcome.hit.is_none() {
        println!("      aucune connexion en {:.1} s", wait.as_secs_f64());
    }
    // Le lanceur, pas le navigateur : `cmd` et `rundll32` ont rendu la main depuis longtemps,
    // et un lanceur encore vivant à l'échéance est une anomalie qu'on note plutôt qu'on tue.
    if let Ok(Some(status)) = child.try_wait() {
        outcome.status = Some(status.code().unwrap_or(-1));
    }
    outcome
}

/// Démarre un candidat.
fn spawn(program: &str, line: &Line) -> std::io::Result<Child> {
    let mut command = Command::new(program);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    match line {
        Line::Args(args) => {
            command.args(args);
        }
        #[cfg(windows)]
        Line::Raw(raw) => {
            use std::os::windows::process::CommandExt as _;
            command.raw_arg(raw);
        }
    }
    command.spawn()
}

/// Lit la ligne de requête, répond une petite page, ferme. Rend la cible demandée.
fn serve(mut stream: TcpStream, target: &str) -> Option<String> {
    // Le socket accepté hérite du mode non bloquant sur certaines plateformes, et un `read`
    // rendrait alors `WouldBlock` au lieu d'attendre les octets.
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();

    let line = {
        use std::io::Read as _;
        let mut reader = BufReader::new(&stream);
        let mut raw = Vec::new();
        let read = reader
            .by_ref()
            .take(LINE_LIMIT as u64 + 1)
            .read_until(b'\n', &mut raw)
            .ok()?;
        if read == 0 || read > LINE_LIMIT {
            return None;
        }
        String::from_utf8_lossy(&raw).into_owned()
    };

    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    if !method.eq_ignore_ascii_case("GET") {
        return None;
    }
    let received = parts.next()?.to_owned();

    let verdict = if received == target {
        "URL reçue intacte."
    } else {
        "URL reçue ALTÉRÉE par le lanceur."
    };
    // Aucune ressource distante : c'est la règle de `docs/PRIVACY.md`, et une sonde qui irait
    // chercher une police pour s'afficher serait une mauvaise blague.
    let body = format!(
        "<!doctype html><html lang=\"fr\"><head><meta charset=\"utf-8\">\
         <title>Sonde mailcore</title></head>\
         <body style=\"font-family:system-ui,sans-serif;margin:3rem;max-width:40rem\">\
         <h1 style=\"font-size:1.2rem\">Le lanceur a ouvert cet onglet</h1>\
         <p>{verdict} Vous pouvez fermer cet onglet.</p>\
         <p style=\"color:#666;font-size:0.9rem\">mailcore — sonde locale, rien n'est sorti de \
         la machine.</p></body></html>"
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
    Some(received)
}

/// Construit l'URL sondée et la cible HTTP qu'on devra retrouver à l'autre bout.
///
/// La longueur porte sur l'URL **entière**, schéma compris : c'est elle que le lanceur reçoit,
/// et c'est donc elle qui est censée le faire échouer.
fn build_url(port: u16, length: usize) -> Result<(String, String)> {
    let prefix = format!("http://127.0.0.1:{port}/?");
    if length < prefix.len() + 1 {
        bail!(
            "longueur {length} trop courte : le préfixe `{prefix}` en fait déjà {}",
            prefix.len()
        );
    }
    let mut pad = String::with_capacity(length - prefix.len());
    while pad.len() + PAD_UNIT.len() <= length - prefix.len() {
        pad.push_str(PAD_UNIT);
    }
    while pad.len() < length - prefix.len() {
        pad.push(PAD_FILLER);
    }
    let target = format!("/?{pad}");
    Ok((format!("{prefix}{pad}"), target))
}

/// Les lanceurs à essayer, pour la plateforme courante.
///
/// La liste est plus large que celle de `mailauth::consent::launchers` : on mesure aussi ce
/// qu'on n'utilise pas, sans quoi on ne saurait jamais qu'on a écarté le bon.
fn candidates(url: &str) -> Vec<Candidate> {
    #[cfg(windows)]
    {
        vec![
            Candidate {
                label: format!("rundll32.exe url.dll,FileProtocolHandler {url}"),
                program: "rundll32.exe",
                line: Line::Args(vec![
                    "url.dll,FileProtocolHandler".to_owned(),
                    url.to_owned(),
                ]),
            },
            Candidate {
                label: format!("rundll32.exe shell32.dll,ShellExec_RunDLL {url}"),
                program: "rundll32.exe",
                line: Line::Args(vec![
                    "shell32.dll,ShellExec_RunDLL".to_owned(),
                    url.to_owned(),
                ]),
            },
            Candidate {
                label: format!("explorer.exe {url}"),
                program: "explorer.exe",
                line: Line::Args(vec![url.to_owned()]),
            },
            Candidate {
                label: format!("cmd.exe /d /c start \"\" \"{url}\""),
                program: "cmd.exe",
                // Guillemets écrits à la main : `start` prendrait le `&` de l'URL pour un
                // séparateur de commandes sans eux. Ils ne protègent **pas** des `%XX`, ce que
                // la colonne « URL intacte » est là pour montrer.
                line: Line::Raw(format!(" /d /c start \"\" \"{url}\"")),
            },
            Candidate {
                label: format!("powershell -NoProfile -Command Start-Process '{url}'"),
                program: "powershell.exe",
                // Apostrophes simples : PowerShell n'y interpole rien. Une URL de consentement
                // n'en contient jamais, l'échappement n'a donc pas de cas à traiter.
                line: Line::Args(vec![
                    "-NoProfile".to_owned(),
                    "-NonInteractive".to_owned(),
                    "-Command".to_owned(),
                    format!("Start-Process '{url}'"),
                ]),
            },
        ]
    }
    #[cfg(target_os = "macos")]
    {
        vec![Candidate {
            label: format!("open {url}"),
            program: "open",
            line: Line::Args(vec![url.to_owned()]),
        }]
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        vec![
            Candidate {
                label: format!("xdg-open {url}"),
                program: "xdg-open",
                line: Line::Args(vec![url.to_owned()]),
            },
            Candidate {
                label: format!("gio open {url}"),
                program: "gio",
                line: Line::Args(vec!["open".to_owned(), url.to_owned()]),
            },
        ]
    }
}

/// Le tableau final.
fn report(outcomes: &[Outcome], length: usize) {
    println!();
    println!("URL de {length} caractères");
    println!(
        "{:<62}  {:<9}  {:<8}  {:<8}  sortie",
        "lanceur", "connexion", "délai", "intacte"
    );
    for outcome in outcomes {
        // Le tableau porte la commande, pas un surnom : on doit pouvoir recopier la ligne
        // dans un terminal pour rejouer le cas à la main.
        let label = short(&outcome.label);
        let (hit, delay, whole) = match (&outcome.spawned, outcome.hit) {
            (Err(_), _) => ("absent".to_owned(), "-".to_owned(), "-".to_owned()),
            (Ok(()), None) => ("NON".to_owned(), "-".to_owned(), "-".to_owned()),
            (Ok(()), Some(delay)) => (
                "oui".to_owned(),
                format!("{:.2} s", delay.as_secs_f64()),
                if outcome.mangled.is_some() {
                    "NON".to_owned()
                } else {
                    "oui".to_owned()
                },
            ),
        };
        let status = outcome
            .status
            .map_or_else(|| "(en cours)".to_owned(), |code| code.to_string());
        println!("{label:<62}  {hit:<9}  {delay:<8}  {whole:<8}  {status}");
    }

    for outcome in outcomes {
        if let Some(mangled) = &outcome.mangled {
            println!();
            println!("Altération relevée pour {}", short(&outcome.label));
            println!("  reçu : {mangled}");
        }
    }
    println!();
    println!(
        "Rappel : « sortie 0 » et « connexion NON » sur la même ligne est le cas qui a motivé \
         cette sonde."
    );
}

/// Raccourcit un libellé pour la colonne du tableau : l'URL y est du bruit, elle est la même
/// partout et elle est déjà décrite par la ligne de longueur.
fn short(label: &str) -> String {
    match label.find("http://127.0.0.1:") {
        Some(at) => format!("{}<url>", &label[..at]),
        None => label.to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_probed_url_has_exactly_the_requested_length() {
        for length in [40, 120, 420, 1000] {
            let (url, target) = build_url(65535, length).unwrap();
            assert_eq!(url.len(), length, "{url}");
            assert!(url.ends_with(&target[1..]), "{url} / {target}");
        }
    }

    #[test]
    fn a_length_below_the_prefix_is_refused_instead_of_silently_stretched() {
        // Une URL plus courte que demandé mesurerait autre chose que ce qu'on croit mesurer.
        assert!(build_url(65535, 10).is_err());
    }

    #[test]
    fn the_probed_url_carries_what_breaks_launchers() {
        let (url, _) = build_url(65535, 420).unwrap();
        // `%2F%2F` est ce que `cmd` peut avaler, `&` est ce que `start` prend pour un
        // séparateur. Une URL de bourrage neutre ne mesurerait que la longueur.
        assert!(url.contains("%2F%2F"), "{url}");
        assert!(url.contains("%3A"), "{url}");
        assert!(url.contains('&'), "{url}");
    }

    #[test]
    fn every_candidate_receives_the_whole_url() {
        let (url, _) = build_url(65535, 420).unwrap();
        let candidates = candidates(&url);
        assert!(!candidates.is_empty());
        for candidate in &candidates {
            let carried = match &candidate.line {
                Line::Args(args) => args.iter().any(|it| it.contains(&url)),
                #[cfg(windows)]
                Line::Raw(raw) => raw.contains(&url),
            };
            assert!(carried, "{} ne porte pas l'URL entière", candidate.label);
        }
    }
}

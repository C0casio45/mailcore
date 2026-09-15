//! **Critère 5 de `docs/PHASE-2.md`**, dans la seule forme que je peux vérifier honnêtement :
//! *notre code* n'ouvre de connexion sortante que là où c'est prévu.
//!
//! ## Ce que ce test ne prouve pas, et il faut le lire d'abord
//!
//! Le critère demande « aucune connexion sortante vers un hôte qui ne figure pas dans la
//! configuration des comptes ». **Il est faux tel qu'écrit, et c'est mesuré** : la
//! vérification de révocation de certificat du système ouvre une connexion vers l'autorité
//! de certification à chaque poignée de main TLS. Voir la correction du critère 5 dans
//! `docs/PHASE-2.md`.
//!
//! Observer *toutes* les connexions d'un processus demande une capture au niveau du système —
//! privilèges, pilote de filtrage, dépendance à la plateforme. Ce test fait autre chose, et
//! le dit : il vérifie **la surface de notre propre code**, qui est ce dont on répond.
//!
//! ## Pourquoi une analyse de source et pas une analyse du graphe de dépendances
//!
//! Le graphe contient `tokio`, `axum`, `rustls`, `keyring` : tous savent ouvrir un socket,
//! et aucun ne le fait de sa propre initiative. Un test qui les interdirait serait un test
//! qu'on désactive à la première dépendance légitime.
//!
//! Ce qui régresse vraiment, c'est **notre** code : quelqu'un ajoute un appel HTTP dans
//! `mailhtml` pour résoudre une image, ou une vérification de mise à jour dans la coquille.
//! C'est ça que ce test attrape, et il attrape aussi l'arrivée d'un client HTTP ou d'un SDK
//! de télémétrie dans un `Cargo.toml`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

/// Les crates **livrés**. Le critère porte sur eux.
///
/// `mailfake`, `mailprivacy`, les sondes et `xtask` sont de l'outillage de test : ils montent
/// des serveurs et des clients exprès, et c'est leur raison d'être. Les inclure rendrait le
/// test bruyant sans rien dire de ce qu'on installe.
const SHIPPED: &[&str] = &[
    "mailcore",
    "mailhtml",
    "mailimport",
    "mailapi",
    "mailmcp",
    "mailauth",
    "mailsync",
    "maild",
    "mail-cli",
    "mail-shell",
];

/// Les seuls fichiers livrés autorisés à ouvrir une connexion sortante.
///
/// Trois, et chacun a une raison écrite dans son en-tête :
///
/// - `mailapi/src/client.rs` — le client JSON-RPC vers le démon. L'adresse vient de
///   `--daemon`, donc de l'utilisateur ;
/// - `mailauth/src/http.rs` — le point de terminaison de jeton OAuth2. L'adresse vient d'une
///   **constante du code**, pas d'une donnée : un point de terminaison configurable serait le
///   moyen le plus simple de faire envoyer un jeton de rafraîchissement ailleurs ;
/// - `mailsync/src/tls.rs` — le client IMAP. L'adresse vient de la configuration du compte.
///
/// **Aucun des trois ne choisit son hôte librement.** C'est la formulation exacte de ce que le
/// critère peut garantir sur notre code.
///
/// Le troisième a été ajouté le 2026-09-04, et **ce test a échoué d'abord** : la surface
/// réseau d'un crate livré ne grandit pas sans que quelqu'un l'écrive ici.
const MAY_CONNECT: &[&str] = &[
    "mailapi/src/client.rs",
    "mailauth/src/http.rs",
    "mailsync/src/tls.rs",
];

/// Ce qui ouvre une connexion sortante, ou résout un nom pour en ouvrir une.
const OUTBOUND: &[&str] = &[
    "TcpStream::connect",
    "to_socket_addrs",
    "UdpSocket::bind",
    "UnixStream::connect",
];

/// Des clients HTTP et des SDK de télémétrie qu'on ne veut pas voir arriver.
///
/// La liste n'est pas exhaustive et ne peut pas l'être. Elle couvre ce qu'on ajouterait sans
/// y penser — et l'analyse de source ci-dessus rattrape le reste, parce qu'un client HTTP
/// finit toujours par ouvrir un socket.
const UNWANTED: &[&str] = &[
    "reqwest",
    "ureq",
    "isahc",
    "curl",
    "attohttpc",
    "sentry",
    "opentelemetry",
    "posthog",
    "analytics",
];

/// La racine du workspace.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("racine du workspace")
        .to_path_buf()
}

/// Tous les fichiers `.rs` d'un répertoire, récursivement.
fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|it| it == "rs") {
                out.push(path);
            }
        }
    }
    out
}

/// Une ligne débarrassée de son commentaire.
///
/// Sans ça, le test échouerait sur `//! Pas de reqwest : un client généreux en pool…` — une
/// phrase qui dit exactement le contraire de ce que le test chercherait. C'est arrivé au
/// premier jet.
fn code_only(line: &str) -> &str {
    line.split_once("//").map_or(line, |(before, _)| before)
}

/// Le code **livré** d'un fichier : tout ce qui précède son module de tests.
///
/// ## Pourquoi cette coupe existe
///
/// `mailauth/src/consent.rs` écoute sur le bouclage et **ne se connecte nulle part** — mais ses
/// tests, eux, se connectent à l'écouteur pour le mettre à l'épreuve. Ils sont dans le même
/// fichier, sous `#[cfg(test)]`, donc ils ne sont pas compilés dans ce qu'on installe.
///
/// Sans cette coupe, ce test a signalé `consent.rs` comme une nouvelle surface réseau le
/// 2026-09-04. Le choix était : déclarer un fichier qui n'ouvre aucune connexion en production
/// — ce qui aurait rendu `MAY_CONNECT` faux et la déclaration sans valeur — ou arrêter de lire
/// ce qui ne s'installe pas. La seconde option est la seule qui garde le sens du critère.
///
/// ## Ce que la coupe peut rater
///
/// Du vrai code écrit **après** le module de tests d'un fichier serait invisible ici. C'est
/// inhabituel — un `#[cfg(test)] mod tests` termine un fichier par convention — et le contrôle
/// positif du test principal reste la garde : les trois fichiers autorisés doivent être
/// trouvés, donc un lecteur qui coupe trop tôt se fait voir.
/// La coupe ne se fait qu'en **début de ligne**, donc sur un module de tests de premier
/// niveau. Un `#[cfg(test)]` indenté au fond d'un module imbriqué serait ignoré et son contenu
/// lu : ça produit un faux positif que quelqu'un doit regarder, ce qui est le bon sens de
/// l'erreur — l'inverse serait un fichier réel devenu invisible.
fn shipped_code(text: &str) -> &str {
    const MARKER: &str = "#[cfg(test)]";
    if text.starts_with(MARKER) {
        return "";
    }
    text.find("\n#[cfg(test)]")
        .map_or(text, |index| &text[..index])
}

/// Le chemin relatif à la racine, en `/`, pour comparer sur toutes les plateformes.
fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn only_two_shipped_files_open_an_outbound_connection() {
    let root = root();
    let mut found: Vec<String> = Vec::new();

    for crate_name in SHIPPED {
        let dir = root.join("crates").join(crate_name).join("src");
        for file in sources(&dir) {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let hit = shipped_code(&text)
                .lines()
                .map(code_only)
                .any(|line| OUTBOUND.iter().any(|needle| line.contains(needle)));
            if hit {
                found.push(relative(&file, &root));
            }
        }
    }
    found.sort();

    let expected: Vec<String> = MAY_CONNECT
        .iter()
        .map(|it| format!("crates/{it}"))
        .collect();

    // **Le contrôle positif.** Un chercheur qui ne trouve rien rendrait la liste vide, et le
    // test passerait en ne vérifiant rien. Les deux fichiers autorisés doivent être trouvés.
    assert_eq!(
        found, expected,
        "la surface réseau des crates livrés a changé.\n\
         Trouvé   : {found:?}\n\
         Attendu  : {expected:?}\n\
         Si c'est volontaire, ajouter le fichier à MAY_CONNECT **et** écrire dans son en-tête \
         d'où vient l'adresse qu'il contacte. Le critère 5 de docs/PHASE-2.md ne garantit rien \
         d'autre que ça."
    );
}

#[test]
fn no_shipped_crate_depends_on_an_http_client_or_a_telemetry_sdk() {
    let root = root();
    let mut offenders: Vec<String> = Vec::new();

    for crate_name in SHIPPED {
        let manifest = root.join("crates").join(crate_name).join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        for line in text
            .lines()
            .map(|it| it.split_once('#').map_or(it, |(a, _)| a))
        {
            for unwanted in UNWANTED {
                if line.contains(unwanted) {
                    offenders.push(format!("{crate_name} : {}", line.trim()));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "un client HTTP ou un SDK de télémétrie est arrivé dans un crate livré : {offenders:?}\n\
         Le critère 5 de docs/PHASE-2.md dit que l'application se connecte aux serveurs que \
         l'utilisateur a configurés et à rien d'autre."
    );
}

#[test]
fn the_manifest_scanner_actually_reads_the_manifests() {
    // **Le contrôle positif du test précédent.** Un lecteur cassé — mauvais chemin, mauvaise
    // extension — rendrait « aucun coupable » sur un workspace qui en serait plein.
    //
    // `rustls` est dans `mailsync` et ne partira pas : il n'y a pas de mode en clair.
    let root = root();
    let manifest = root.join("crates").join("mailsync").join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).expect("le manifeste de mailsync est lisible");
    assert!(
        text.lines()
            .map(|it| it.split_once('#').map_or(it, |(a, _)| a))
            .any(|line| line.contains("rustls")),
        "le lecteur de manifeste ne voit pas une dépendance qui est là"
    );
}

#[test]
fn the_test_module_of_a_file_is_not_part_of_its_shipped_surface() {
    // La coupe qui a été ajoutée le 2026-09-04, après que `consent.rs` a été signalé pour une
    // connexion qui n'existe que dans ses tests.
    let file = "//! Un écouteur.\nfn open() {}\n\n#[cfg(test)]\nmod tests {\n\
                    let s = TcpStream::connect(a);\n}\n";
    assert!(!shipped_code(file).contains("TcpStream::connect"));
    // **Et le contrôle dans l'autre sens.** Une coupe trop gourmande — au premier `#`, ou au
    // premier `mod` — rendrait une chaîne vide, et le test principal ne trouverait plus rien
    // tout en passant si on l'avait laissé comparer à une liste vide.
    assert!(shipped_code(file).contains("fn open()"));

    // Un fichier sans module de tests n'est pas coupé du tout.
    let whole = "fn open() {}\nlet s = TcpStream::connect(a);\n";
    assert_eq!(shipped_code(whole), whole);

    // Un `#[cfg(test)]` indenté n'est pas une coupe : seul un module de premier niveau l'est.
    let nested = "mod inner {\n    #[cfg(test)]\n    mod t { TcpStream::connect(a); }\n}\n";
    assert!(shipped_code(nested).contains("TcpStream::connect"));
}

#[test]
fn the_source_scanner_ignores_comments_but_not_code() {
    // Le premier jet de ce fichier échouait sur une **phrase** qui disait « pas de reqwest ».
    // Un chercheur qui compte les commentaires trouve des coupables imaginaires ; un
    // chercheur qui saute les lignes entières en rate des vrais.
    assert_eq!(code_only("//! Pas de reqwest : trop généreux"), "");
    assert_eq!(code_only("    // TcpStream::connect ici un jour"), "    ");
    assert_eq!(
        code_only("    let s = TcpStream::connect(a)?; // le vrai"),
        "    let s = TcpStream::connect(a)?; "
    );
    assert!(code_only("    let s = TcpStream::connect(a)?;").contains("TcpStream::connect"));
}

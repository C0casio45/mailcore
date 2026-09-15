//! L'API JSON-RPC servie en HTTP, vérifiée sur une socket réelle.
//!
//! Ce que les tests unitaires ne peuvent pas dire : que la couche de jeton est bien **devant**
//! `/api` et pas seulement devant `/mcp`, et qu'un client sans jeton se fait refuser avant
//! que le moindre octet de courrier soit lu. C'est le genre de chose qu'on ne découvre qu'en
//! faisant tourner le serveur — l'étape 6 l'a déjà appris avec le conteneur.
//!
//! Le client HTTP est écrit à la main, sur une `TcpStream`. Pas de `reqwest` : ajouter un
//! arbre de dépendances complet pour poster trois requêtes à notre propre serveur coûterait
//! plus que les quarante lignes que ça prend, et un client minimal ne masque rien de ce qui
//! est réellement envoyé sur le fil.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use mailcore::{FolderKind, MessageFlags, NewMessage, Store};

/// Une réponse HTTP, réduite à ce qu'on vérifie.
struct HttpResponse {
    status: u16,
    body: String,
}

/// Poste un corps sur un chemin, avec ou sans jeton.
fn post(address: SocketAddr, path: &str, token: Option<&str>, body: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(address).expect("connexion au démon");
    let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         {auth}Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len()
    );
    stream.write_all(request.as_bytes()).expect("envoi");
    stream.flush().expect("vidage");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("lecture");
    let raw = String::from_utf8_lossy(&raw).into_owned();

    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    // Le corps commence après la ligne vide qui clôt les en-têtes. `Connection: close` évite
    // d'avoir à interpréter un encodage par morceaux.
    let body = raw
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, rest)| {
            // Une réponse en morceaux commence par la taille du premier morceau en hexadécimal.
            rest.lines()
                .find(|line| line.starts_with('{') || line.starts_with('['))
                .unwrap_or("")
                .to_owned()
        });

    HttpResponse { status, body }
}

/// Un appel JSON-RPC, tel qu'un client l'écrirait.
fn rpc(method: &str, params: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{params}}}"#)
}

/// Monte un store minimal et démarre le démon dessus, sur un port libre.
///
/// Le processus fils est rendu enveloppé dans [`Daemon`] : il est donc tué et attendu à la
/// fin du test, y compris si celui-ci panique — sans quoi un `maild` orphelin resterait
/// accroché à son port et ferait échouer l'exécution suivante.
fn daemon() -> (tempfile::TempDir, SocketAddr, String, Daemon) {
    daemon_with_ui(false)
}

/// Comme [`daemon`], mais en servant en plus un front minimal si `ui` est vrai.
///
/// Le « front » est un `index.html` d'une ligne : ce qu'on teste est le comportement du
/// serveur de fichiers, pas le contenu de l'application.
fn daemon_with_ui(ui: bool) -> (tempfile::TempDir, SocketAddr, String, Daemon) {
    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();

    // Un message RFC 5322 réel, écrit dans le store de blobs : sans lui, `messages.get`
    // rendrait `null` — le cœur traite un blob absent comme un message absent — et le test
    // ne dirait rien du chemin de lecture complet.
    const RAW: &[u8] = b"Message-ID: <un@exemple.fr>\r\n\
        From: \"Expediteur\" <expediteur@exemple.fr>\r\n\
        To: destinataire@exemple.fr\r\n\
        Subject: sujet de test\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        \r\n\
        Le corps du message.\r\n";

    let store = Store::open(&root).unwrap();
    let blob = store.blobs().put(RAW).unwrap().hash;
    let writer = store.writer().unwrap();
    let account = writer.upsert_account("imap", "compte").unwrap();
    let folder = writer
        .upsert_folder(account, "INBOX", FolderKind::Inbox)
        .unwrap();
    let (id, _) = writer
        .insert_message(&NewMessage {
            blob,
            rfc822_id: Some("<un@exemple.fr>"),
            date: 1_700_000_000,
            from_addr: "expediteur@exemple.fr",
            from_name: Some("Expéditeur"),
            subject: "sujet de test",
            size: RAW.len() as u64,
            has_attachments: false,
        })
        .unwrap();
    writer
        .insert_ref(id, folder, 1_700_000_000, MessageFlags::empty())
        .unwrap();

    // Un deuxième message, piégé et en HTML, daté plus ancien pour qu'il arrive en second
    // dans la liste et ne déplace pas le premier. Il sert au test du rendu confiné.
    const TRAPPED: &[u8] = b"From: pisteur@exemple.fr\r\n\
        To: destinataire@exemple.fr\r\n\
        Subject: message piege\r\n\
        Content-Type: text/html; charset=utf-8\r\n\
        \r\n\
        <p>Votre facture</p>\
        <script>alert(document.cookie)</script>\
        <img src=\"https://click.list-manage.com/o.gif?email=destinataire%40exemple.fr\" \
        width=\"1\" height=\"1\">\
        <a href=\"https://exemple.fr/facture\">la facture</a>\r\n";

    let trapped_blob = store.blobs().put(TRAPPED).unwrap().hash;
    let (trapped_id, _) = writer
        .insert_message(&NewMessage {
            blob: trapped_blob,
            rfc822_id: None,
            date: 1_600_000_000,
            from_addr: "pisteur@exemple.fr",
            from_name: None,
            subject: "message piege",
            size: TRAPPED.len() as u64,
            has_attachments: false,
        })
        .unwrap();
    writer
        .insert_ref(trapped_id, folder, 1_600_000_000, MessageFlags::empty())
        .unwrap();

    writer.commit().unwrap();
    drop(store);

    // Un port libre choisi par le système, puis relâché : lier `:0` dans le démon lui-même
    // ne nous dirait pas quel port il a obtenu. La fenêtre de course est théorique sur une
    // machine de test, et l'alternative — un port en dur — casse dès deux exécutions
    // simultanées.
    let address = {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap()
    };

    // Un front minimal, dans un sous-répertoire du store : si le serveur de fichiers servait
    // un cran trop haut, le test verrait `index.sqlite` sortir par HTTP.
    let ui_dir = root.join("ui");
    if ui {
        std::fs::create_dir_all(ui_dir.as_std_path()).unwrap();
        std::fs::write(
            ui_dir.join("index.html").as_std_path(),
            "<!doctype html>FRONT",
        )
        .unwrap();
        std::fs::write(ui_dir.join("app.js").as_std_path(), "// js du front").unwrap();
    }

    let token = "jeton-de-test-suffisamment-long-pour-etre-realiste";
    let mut arguments: Vec<String> = vec![
        "--store".to_owned(),
        root.to_string(),
        "http".to_owned(),
        "--listen".to_owned(),
        address.to_string(),
    ];
    if ui {
        arguments.insert(2, "--ui-dir".to_owned());
        arguments.insert(3, ui_dir.to_string());
    }

    // Enveloppé tout de suite : entre le `spawn` et le premier `?` il ne doit exister aucun
    // chemin de code qui abandonne le processus.
    let child = Daemon(
        std::process::Command::new(env!("CARGO_BIN_EXE_maild"))
            .args(&arguments)
            .env("MAILCORE_TOKEN", token)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("démarrage de maild"),
    );

    // Attendre que la socket accepte, plutôt qu'un délai fixe qui serait soit trop court
    // sur une machine chargée, soit du temps perdu à chaque exécution.
    //
    // **Le budget est monté de 5 s à 20 s le 2026-09-09**, après un échec unique de
    // `without_a_front_directory_nothing_is_served_at_the_root` pendant un `cargo test
    // --workspace` : seize démons se lancent en parallèle, et l'un d'eux n'avait pas ouvert sa
    // socket en cinq secondes. La suite repassait seule et en bloc, ce qui dit que c'est le
    // budget et non le démon. Un test qui échoue une fois sur vingt selon la charge coûte plus
    // cher à diagnostiquer que les quinze secondes qu'il n'utilisera jamais.
    for _ in 0..400 {
        if TcpStream::connect(address).is_ok() {
            return (dir, address, token.to_owned(), child);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("le démon n'a pas ouvert sa socket");
}

/// Tue le démon à la fin du test, quoi qu'il arrive.
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_api_refuses_a_client_without_a_token() {
    let (_dir, address, token, _daemon) = daemon();

    // Sans jeton.
    let response = post(address, "/api", None, &rpc("folders.list", "null"));
    assert_eq!(response.status, 401, "servi sans jeton");
    assert!(
        !response.body.contains("INBOX"),
        "un dossier a fuité dans une réponse refusée"
    );

    // Avec un mauvais jeton.
    let response = post(
        address,
        "/api",
        Some("pas-le-bon-jeton-mais-la-bonne-longue"),
        &rpc("folders.list", "null"),
    );
    assert_eq!(response.status, 401, "servi avec un mauvais jeton");

    // Avec le bon.
    let response = post(address, "/api", Some(&token), &rpc("folders.list", "null"));
    assert_eq!(response.status, 200);
    assert!(response.body.contains("INBOX"), "corps : {}", response.body);
}

#[test]
fn both_surfaces_answer_on_the_same_port() {
    // Le point du montage : `/mcp` et `/api` cohabitent, et le même jeton ouvre les deux.
    let (_dir, address, token, _daemon) = daemon();

    let api = post(address, "/api", Some(&token), &rpc("server.hello", "null"));
    assert_eq!(api.status, 200);
    assert!(api.body.contains("mailcore"), "corps : {}", api.body);

    let mcp = post(
        address,
        "/mcp",
        Some(&token),
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
    );
    // `rmcp` exige une session négociée avant de répondre à `tools/list` : le code exact
    // dépend de sa version, ce qui compte ici est que la requête ait dépassé la couche de
    // jeton — donc tout sauf 401.
    assert_ne!(mcp.status, 401, "/mcp a refusé un jeton valide");
    assert_ne!(mcp.status, 404, "/mcp n'est pas monté");
}

#[test]
fn the_api_answers_the_calls_a_client_actually_makes() {
    let (_dir, address, token, _daemon) = daemon();

    // La séquence d'ouverture d'un client : se présenter, lister, paginer, ouvrir.
    let hello = post(address, "/api", Some(&token), &rpc("server.hello", "null"));
    let hello: serde_json::Value = serde_json::from_str(&hello.body).expect(&hello.body);
    assert_eq!(hello["result"]["protocol"], 1);
    assert_eq!(hello["result"]["messages"], 2);

    let folders = post(address, "/api", Some(&token), &rpc("folders.list", "null"));
    let folders: serde_json::Value = serde_json::from_str(&folders.body).expect(&folders.body);
    let folder = folders["result"][0]["id"].as_i64().expect("un dossier");

    let page = post(
        address,
        "/api",
        Some(&token),
        &rpc("messages.page", &format!(r#"{{"folder":{folder}}}"#)),
    );
    let page: serde_json::Value = serde_json::from_str(&page.body).expect(&page.body);
    let rows = page["result"]["rows"].as_array().expect("des lignes");
    assert_eq!(rows.len(), 2, "les deux messages du store");
    assert_eq!(rows[0]["subject"], "sujet de test");
    assert_eq!(rows[0]["unread"], true);
    // La date sort en secondes Unix, pas en chaîne : c'est le client qui la formate.
    assert_eq!(rows[0]["date"], 1_700_000_000_i64);
    assert!(page["result"]["next"].is_null(), "une seule page");

    let id = rows[0]["id"].as_i64().expect("un identifiant");
    let message = post(
        address,
        "/api",
        Some(&token),
        &rpc("messages.get", &format!(r#"{{"id":{id}}}"#)),
    );
    let message: serde_json::Value = serde_json::from_str(&message.body).expect(&message.body);
    let result = &message["result"];
    assert_eq!(result["row"]["id"], id);
    // `mail-parser` rend le `Message-ID` sans ses chevrons : c'est le décodage qui fait foi,
    // pas les octets de l'en-tête.
    assert_eq!(result["message_id"], "un@exemple.fr");
    assert_eq!(result["to"][0], "destinataire@exemple.fr");
    assert!(
        result["body"]
            .as_str()
            .unwrap_or_default()
            .contains("corps"),
        "corps absent : {result}"
    );
    // Les dossiers sortent avec leur état lu/non-lu : les drapeaux appartiennent à la
    // référence, pas au contenu.
    assert_eq!(result["folders"][0]["path"], "INBOX");
    assert_eq!(result["folders"][0]["unread"], true);
}

#[test]
fn a_trapped_message_crosses_the_api_already_sanitised() {
    // Le point du test : l'assainissement est **du côté du démon**. Un client qui demande le
    // HTML ne reçoit jamais de balisage brut, quelle que soit sa prudence — la barrière est
    // par défaut, pas à la charge de l'appelant.
    let (_dir, address, token, _daemon) = daemon();

    let folders = post(address, "/api", Some(&token), &rpc("folders.list", "null"));
    let folders: serde_json::Value = serde_json::from_str(&folders.body).expect(&folders.body);
    let folder = folders["result"][0]["id"].as_i64().expect("un dossier");

    let page = post(
        address,
        "/api",
        Some(&token),
        &rpc("messages.page", &format!(r#"{{"folder":{folder}}}"#)),
    );
    let page: serde_json::Value = serde_json::from_str(&page.body).expect(&page.body);
    let rows = page["result"]["rows"].as_array().expect("des lignes");
    let trapped = rows
        .iter()
        .find(|row| row["subject"] == "message piege")
        .expect("le message piégé");
    let id = trapped["id"].as_i64().expect("un identifiant");

    let response = post(
        address,
        "/api",
        Some(&token),
        &rpc("messages.get", &format!(r#"{{"id":{id},"body":"html"}}"#)),
    );
    let message: serde_json::Value = serde_json::from_str(&response.body).expect(&response.body);
    let html = &message["result"]["html"];

    let body = html["html"].as_str().expect("un corps HTML");
    assert!(body.contains("Votre facture"), "{body}");
    assert!(!body.contains("<script"), "script servi au client : {body}");
    assert!(!body.contains("alert("), "charge servie au client : {body}");
    assert!(
        !body.contains("list-manage.com"),
        "traceur servi au client : {body}"
    );
    // Le lien survit : sa cible doit être lisible avant le clic (`docs/PRIVACY.md` §6).
    assert!(body.contains("exemple.fr/facture"), "{body}");

    // La CSP et le `sandbox` voyagent avec le corps : le front n'en garde pas de copie qui
    // pourrait diverger de la source unique.
    assert!(
        html["csp"]
            .as_str()
            .is_some_and(|csp| csp.contains("connect-src 'none'")),
        "{html}"
    );
    assert!(
        html["sandbox"]
            .as_str()
            .is_some_and(|sandbox| !sandbox.contains("allow-scripts")),
        "{html}"
    );

    // De quoi afficher « Contenu distant bloqué — 1 image, 3 raisons de s'en méfier ».
    // Le pixel coche les trois signaux à la fois : il est de 1×1, il vient d'un domaine de
    // traçage connu, et il porte l'adresse du destinataire. L'ordre du tableau est stable
    // mais ce n'est pas un contrat — on vérifie l'ensemble, pas la première case.
    assert_eq!(html["blocked_images"], 1);
    let trackers = html["trackers"].as_array().expect("des traceurs");
    let kinds: Vec<&str> = trackers.iter().filter_map(|t| t["kind"].as_str()).collect();
    for attendu in ["pixel", "known_domain", "correlated_id"] {
        assert!(
            kinds.contains(&attendu),
            "signal manquant : {attendu} — {html}"
        );
    }
    assert!(
        trackers
            .iter()
            .all(|t| t["host"] == "click.list-manage.com"),
        "{html}"
    );

    // L'adresse du destinataire, que le traceur transportait, ne traverse pas le réseau.
    assert!(
        !response.body.contains("destinataire%40"),
        "l'URL du traceur a fuité"
    );
}

#[test]
fn the_html_body_is_not_served_unless_it_is_asked_for() {
    let (_dir, address, token, _daemon) = daemon();
    let folders = post(address, "/api", Some(&token), &rpc("folders.list", "null"));
    let folders: serde_json::Value = serde_json::from_str(&folders.body).expect(&folders.body);
    let folder = folders["result"][0]["id"].as_i64().expect("un dossier");

    let page = post(
        address,
        "/api",
        Some(&token),
        &rpc("messages.page", &format!(r#"{{"folder":{folder}}}"#)),
    );
    let page: serde_json::Value = serde_json::from_str(&page.body).expect(&page.body);
    let id = page["result"]["rows"][0]["id"].as_i64().expect("un id");

    let response = post(
        address,
        "/api",
        Some(&token),
        &rpc("messages.get", &format!(r#"{{"id":{id}}}"#)),
    );
    let message: serde_json::Value = serde_json::from_str(&response.body).expect(&response.body);
    assert!(
        message["result"]["html"].is_null(),
        "le HTML est servi sans qu'on l'ait demandé"
    );
}

/// Récupère un chemin en GET, sans jeton, et rend le statut et le corps.
fn get(address: SocketAddr, path: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(address).expect("connexion au démon");
    // Le chemin est écrit **tel quel** dans la requête, sans normalisation : c'est le point
    // d'un test de traversée, et un client qui nettoierait le chemin testerait le client.
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("envoi");
    stream.flush().expect("vidage");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("lecture");
    let raw = String::from_utf8_lossy(&raw).into_owned();

    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = raw
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, rest)| rest.to_owned());
    HttpResponse { status, body }
}

#[test]
fn the_front_is_served_without_a_token_but_the_data_is_not() {
    // **Le front doit être servi sans authentification** : un navigateur qui ouvre un onglet
    // ne peut pas poser d'en-tête `Authorization` sur sa première requête. Ce qui sort par là
    // est notre propre JavaScript — les données restent derrière `/api`.
    let (_dir, address, _token, _daemon) = daemon_with_ui(true);

    let index = get(address, "/");
    assert_eq!(index.status, 200);
    assert!(index.body.contains("FRONT"), "corps : {}", index.body);

    let asset = get(address, "/app.js");
    assert_eq!(asset.status, 200);
    assert!(asset.body.contains("js du front"));

    // Et l'API n'a pas bougé d'un pouce.
    let refused = post(address, "/api", None, &rpc("folders.list", "null"));
    assert_eq!(refused.status, 401, "l'API a été ouverte avec le front");
}

#[test]
fn no_path_escapes_the_front_directory() {
    // Servir des fichiers statiques veut dire répondre de la traversée de chemin. Le code
    // vient de `tower-http`, mais la configuration est à nous : c'est elle qu'on teste.
    let (_dir, address, _token, _daemon) = daemon_with_ui(true);

    for hostile in [
        "/../index.sqlite",
        "/../../index.sqlite",
        "/..%2findex.sqlite",
        "/%2e%2e%2findex.sqlite",
        "/%2e%2e/%2e%2e/Cargo.toml",
        "/....//index.sqlite",
        "/ui/../../index.sqlite",
        "/..\\index.sqlite",
    ] {
        let response = get(address, hostile);
        // Le repli SPA rend `index.html` pour tout chemin inconnu, donc un `200` est normal.
        // Ce qui compte est que le **contenu** ne vienne jamais d'en dehors du répertoire du
        // front : la seule réponse acceptable est le front lui-même, ou un refus.
        assert!(
            response.body.contains("FRONT") || response.status >= 400,
            "chemin {hostile:?} a rendu autre chose que le front : {} — {}",
            response.status,
            &response.body[..response.body.len().min(200)]
        );
        assert!(
            !response.body.contains("SQLite") && !response.body.contains("[workspace]"),
            "fuite de fichier par {hostile:?}"
        );
    }
}

#[test]
fn without_a_front_directory_nothing_is_served_at_the_root() {
    // Pas de service de fichiers par accident : sans `--ui-dir`, la racine ne rend rien.
    //
    // Le statut est `401` et non `404`, et c'est une conséquence de la structure : la couche
    // de jeton est posée devant **tout** le routeur protégé, donc un chemin inconnu est
    // refusé avant d'avoir pu être déclaré introuvable. C'est aussi le meilleur des deux
    // comportements — quelqu'un qui sonde le port apprend seulement qu'il y a un jeton, pas
    // quels chemins existent.
    let (_dir, address, _token, _daemon) = daemon();
    let index = get(address, "/");
    assert_eq!(
        index.status, 401,
        "réponse inattendue à la racine sans --ui-dir"
    );
    assert!(
        !index.body.contains("<!doctype"),
        "un front est servi alors qu'aucun n'est configuré"
    );
}

#[test]
fn a_client_cannot_name_a_path_to_import() {
    // **La garantie qui rend `jobs.start` acceptable.** Le démon de ce test est démarré sans
    // `--profile` : aucun import n'est possible, quoi que le client demande. Sans ce
    // fail closed, quiconque détient le jeton ferait lire n'importe quel fichier de la
    // machine du démon, rangé dans le store puis relisible par `search.query`.
    let (_dir, address, token, _daemon) = daemon();

    let sources = post(address, "/api", Some(&token), &rpc("jobs.sources", "null"));
    let sources: serde_json::Value = serde_json::from_str(&sources.body).expect(&sources.body);
    assert_eq!(
        sources["result"].as_array().map(Vec::len),
        Some(0),
        "des profils sont exposés alors qu'aucun n'est déclaré"
    );

    let refused = post(
        address,
        "/api",
        Some(&token),
        &rpc("jobs.start", r#"{"kind":"import","source":0}"#),
    );
    let refused: serde_json::Value = serde_json::from_str(&refused.body).expect(&refused.body);
    assert_eq!(refused["error"]["code"], -32_602, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("--profile")),
        "le refus ne dit pas quoi faire : {refused}"
    );

    // Et un chemin passé de force est ignoré : le paramètre n'existe pas dans le contrat.
    let forged = post(
        address,
        "/api",
        Some(&token),
        &rpc(
            "jobs.start",
            r#"{"kind":"import","path":"C:/Windows/System32/config/SAM"}"#,
        ),
    );
    let forged: serde_json::Value = serde_json::from_str(&forged.body).expect(&forged.body);
    assert!(forged.get("error").is_some(), "import accepté : {forged}");
}

#[test]
fn a_background_job_runs_and_can_be_followed() {
    let (_dir, address, token, _daemon) = daemon();

    let started = post(
        address,
        "/api",
        Some(&token),
        &rpc("jobs.start", r#"{"kind":"index"}"#),
    );
    let started: serde_json::Value = serde_json::from_str(&started.body).expect(&started.body);
    let id = started["result"]["id"].as_u64().expect("un identifiant");
    assert_eq!(started["result"]["kind"], "index");

    // Suivre jusqu'à la fin, comme le ferait une barre de progression.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let job = loop {
        let response = post(
            address,
            "/api",
            Some(&token),
            &rpc("jobs.get", &format!(r#"{{"id":{id}}}"#)),
        );
        let job: serde_json::Value = serde_json::from_str(&response.body).expect(&response.body);
        let state = job["result"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if matches!(state.as_str(), "done" | "failed" | "cancelled") {
            break job;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "la tâche n'a jamais fini"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    assert_eq!(job["result"]["state"], "done", "{job}");
    assert!(job["result"]["finished_at"].is_i64());
    assert!(
        job["result"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("indexés")),
        "{job}"
    );

    // Et elle apparaît dans la liste.
    let listed = post(address, "/api", Some(&token), &rpc("jobs.list", "null"));
    let listed: serde_json::Value = serde_json::from_str(&listed.body).expect(&listed.body);
    assert_eq!(listed["result"][0]["id"], id);
}

#[test]
fn an_unknown_job_kind_is_refused_rather_than_guessed() {
    let (_dir, address, token, _daemon) = daemon();
    let response = post(
        address,
        "/api",
        Some(&token),
        &rpc("jobs.start", r#"{"kind":"supprime-tout"}"#),
    );
    let value: serde_json::Value = serde_json::from_str(&response.body).expect(&response.body);
    assert_eq!(value["error"]["code"], -32_602, "{value}");
}

#[test]
fn indexing_through_the_api_makes_search_work_without_a_restart() {
    // Le démon a ouvert sa boîte sur un store sans index. Après une tâche d'indexation, la
    // recherche doit répondre — sinon le démon annoncerait « indexation terminée » puis
    // continuerait de chercher dans un index qui n'existe pas.
    let (_dir, address, token, _daemon) = daemon();

    let hello = post(address, "/api", Some(&token), &rpc("server.hello", "null"));
    let hello: serde_json::Value = serde_json::from_str(&hello.body).expect(&hello.body);
    let before = hello["result"]["search_available"].as_bool();

    let started = post(
        address,
        "/api",
        Some(&token),
        &rpc("jobs.start", r#"{"kind":"index"}"#),
    );
    let started: serde_json::Value = serde_json::from_str(&started.body).expect(&started.body);
    let id = started["result"]["id"].as_u64().expect("un identifiant");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let response = post(
            address,
            "/api",
            Some(&token),
            &rpc("jobs.get", &format!(r#"{{"id":{id}}}"#)),
        );
        let job: serde_json::Value = serde_json::from_str(&response.body).expect(&response.body);
        if job["result"]["state"] == "done" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "indexation sans fin");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let found = post(
        address,
        "/api",
        Some(&token),
        &rpc("search.query", r#"{"query":"sujet"}"#),
    );
    let found: serde_json::Value = serde_json::from_str(&found.body).expect(&found.body);
    assert_eq!(
        found["result"]["search_available"], true,
        "recherche indisponible après indexation (avant : {before:?}) — {found}"
    );
    assert!(
        found["result"]["rows"]
            .as_array()
            .is_some_and(|r| !r.is_empty()),
        "l'index construit ne trouve rien : {found}"
    );
}

#[test]
fn a_job_changes_the_revision_so_waiting_clients_wake_up() {
    // Le mécanisme d'abonnement de l'étape 7 doit marcher pour les tâches de fond sans qu'on
    // ait rien ajouté : la tâche écrit depuis sa propre connexion SQLite, donc la révision
    // que voient les lecteurs change.
    let (_dir, address, token, _daemon) = daemon();

    let before = post(
        address,
        "/api",
        Some(&token),
        &rpc("store.revision", "null"),
    );
    let before: serde_json::Value = serde_json::from_str(&before.body).expect(&before.body);
    let revision = before["result"]["revision"]
        .as_str()
        .expect("une révision")
        .to_owned();

    let started = post(
        address,
        "/api",
        Some(&token),
        &rpc("jobs.start", r#"{"kind":"thread"}"#),
    );
    let started: serde_json::Value = serde_json::from_str(&started.body).expect(&started.body);
    assert!(started["result"]["id"].is_u64(), "{started}");

    // Un long-poll court : si la révision ne bougeait pas, il expirerait avec `changed:false`.
    let waited = post(
        address,
        "/api",
        Some(&token),
        &rpc(
            "store.wait",
            &format!(r#"{{"revision":"{revision}","timeout_ms":20000}}"#),
        ),
    );
    let waited: serde_json::Value = serde_json::from_str(&waited.body).expect(&waited.body);
    assert_eq!(
        waited["result"]["changed"], true,
        "la tâche n'a pas réveillé le client : {waited}"
    );
}

#[test]
fn a_stale_message_id_is_null_and_not_a_failure() {
    // Le cas d'un client qui rouvre une page mise en cache après un `mail import` : un
    // identifiant peut avoir disparu. Ça ne doit pas ressembler à une panne du store.
    let (_dir, address, token, _daemon) = daemon();

    let response = post(
        address,
        "/api",
        Some(&token),
        &rpc("messages.get", r#"{"id":999999}"#),
    );
    assert_eq!(response.status, 200);
    let value: serde_json::Value = serde_json::from_str(&response.body).expect(&response.body);
    assert!(value["result"].is_null());
    assert!(value.get("error").is_none());
}

#[test]
fn an_unknown_method_is_a_protocol_error_not_a_dead_connection() {
    let (_dir, address, token, _daemon) = daemon();

    let response = post(
        address,
        "/api",
        Some(&token),
        &rpc("folders.delete", "null"),
    );
    // Le protocole a répondu : c'est un `200` qui porte une erreur JSON-RPC, pas un `500`.
    assert_eq!(response.status, 200);
    let value: serde_json::Value = serde_json::from_str(&response.body).expect(&response.body);
    assert_eq!(value["error"]["code"], -32_601);
}

#[test]
fn a_notification_gets_no_content_rather_than_an_empty_body() {
    let (_dir, address, token, _daemon) = daemon();

    let response = post(
        address,
        "/api",
        Some(&token),
        r#"{"jsonrpc":"2.0","method":"folders.list"}"#,
    );
    assert_eq!(response.status, 204);
}

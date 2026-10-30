//! `maild stdio` — le serveur MCP qu'un client lance de lui-même — ne fait que lire, vérifié sur
//! **le vrai binaire**.
//!
//! Le 2026-10-30, ce processus passait par `Service::open` : lancé sur le corpus de mesure, il l'a
//! migré, y a lancé la moisson et, soixante secondes plus tard, la passe de fond des PDF et une
//! mise à jour de l'index. Les outils MCP n'écrivaient rien ; c'est ce qui tournait autour d'eux.
//! Aucun test de `mailcore` ne peut dire ce que le binaire monte autour de la boîte : celui-ci
//! lance le processus, lui parle MCP, attend que la passe de fond **aurait** démarré —
//! `MAILCORE_VECTORS_DELAY_MS` la ramène à 300 ms —, et relit le store octet pour octet.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use mailcore::{FolderKind, MessageFlags, NewMessage, Store};

/// Combien de temps la passe de fond attendrait avant de démarrer, si elle était montée.
const BACKGROUND_DELAY_MS: &str = "300";

/// Combien de temps laisser vivre le processus : dix fois le délai de la passe de fond.
const LIFETIME: Duration = Duration::from_secs(3);

/// Un store au schéma courant, avec un message, indexé, **fermé** — comme le laisse l'application.
fn closed_store() -> (tempfile::TempDir, Utf8PathBuf) {
    const RAW: &[u8] = b"Message-ID: <un@exemple.fr>\r\n\
        From: \"Expediteur\" <expediteur@exemple.fr>\r\n\
        To: destinataire@exemple.fr\r\n\
        Subject: sujet de test\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        \r\n\
        Le corps du message.\r\n";

    let dir = tempfile::tempdir().unwrap();
    let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
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
    writer.commit().unwrap();
    mailcore::index::update(&store, &mailcore::Progress::new()).unwrap();
    drop(store);
    (dir, root)
}

/// Un relevé de fichiers : chemin relatif, et contenu — `None` pour un répertoire.
type Tree = BTreeMap<String, Option<Vec<u8>>>;

/// Tout ce qu'il y a sous `root` : un répertoire vaut `None`, un fichier son contenu. Les deux
/// fichiers que SQLite écrit pour coordonner un lecteur WAL sont rendus à part, pour que chaque
/// test dise ce qu'il en attend.
fn snapshot(root: &Utf8Path) -> (Tree, [Option<Vec<u8>>; 2]) {
    fn walk(root: &Utf8Path, dir: &Utf8Path, out: &mut Tree) {
        for entry in dir.read_dir_utf8().unwrap() {
            let path = entry.unwrap().into_path();
            let name = path.strip_prefix(root).unwrap().as_str().replace('\\', "/");
            if path.is_dir() {
                out.insert(name, None);
                walk(root, &path, out);
            } else {
                out.insert(name, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut tree = BTreeMap::new();
    walk(root, root, &mut tree);
    let wal = tree.remove("index.sqlite-wal").flatten();
    let shm = tree.remove("index.sqlite-shm").flatten();
    (tree, [wal, shm])
}

/// Ce qui a changé entre deux relevés, nommé, pour qu'un échec dise quoi.
fn changes(before: &Tree, after: &Tree) -> Vec<String> {
    let mut out = Vec::new();
    for (name, content) in after {
        match before.get(name) {
            None => out.push(format!("créé : {name}")),
            Some(old) if old != content => out.push(format!("modifié : {name}")),
            Some(_) => {}
        }
    }
    for name in before.keys() {
        if !after.contains_key(name) {
            out.push(format!("effacé : {name}"));
        }
    }
    out
}

/// Le processus, tué et attendu à la fin du test quoi qu'il arrive.
struct Maild(Child);

impl Drop for Maild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Maild {
    /// `maild --store <root> stdio`, sans `--protocol` : le défaut est celui que lance un client.
    fn spawn(root: &Utf8Path, stdin: Stdio) -> Self {
        Self(
            Command::new(env!("CARGO_BIN_EXE_maild"))
                .args(["--store", root.as_str(), "stdio"])
                .env(maild::vectors::DELAY_ENV, BACKGROUND_DELAY_MS)
                .env("RUST_LOG", "info,html5ever=error,tantivy=warn")
                .stdin(stdin)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("démarrage de maild"),
        )
    }

    /// La sortie d'erreur, lue jusqu'au bout dans un fil : un tuyau plein bloquerait le processus.
    fn stderr(&mut self) -> std::thread::JoinHandle<String> {
        let mut stderr = self.0.stderr.take().unwrap();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        })
    }

    /// Attend la fin du processus, au plus `budget`.
    fn wait(&mut self, budget: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "maild ne s'est pas arrêté");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Envoie un message MCP, une ligne de JSON.
fn send(stdin: &mut impl Write, message: &serde_json::Value) {
    writeln!(stdin, "{message}").unwrap();
    stdin.flush().unwrap();
}

/// Lit la réponse à la requête `id`, en sautant ce que le serveur envoie d'autre.
fn response(stdout: &mut BufReader<ChildStdout>, id: u64) -> serde_json::Value {
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "maild a fermé sa sortie avant de répondre à {id}"
        );
        let value: serde_json::Value = serde_json::from_str(&line).expect(&line);
        if value["id"] == id {
            return value;
        }
    }
}

#[test]
fn the_stdio_mcp_server_answers_and_writes_nothing() {
    let (_dir, root) = closed_store();
    let (files, journal) = snapshot(&root);
    assert_eq!(journal, [None, None], "précondition : un store fermé");

    let mut maild = Maild::spawn(&root, Stdio::piped());
    let stderr = maild.stderr();
    let started = Instant::now();
    let mut stdin = maild.0.stdin.take().unwrap();
    let mut stdout = BufReader::new(maild.0.stdout.take().unwrap());

    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }
        }),
    );
    let initialized = response(&mut stdout, 1);
    assert_eq!(
        initialized["result"]["serverInfo"]["name"], "mailcore",
        "{initialized}"
    );
    send(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );

    send(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    );
    let listed = response(&mut stdout, 2);
    let mut tools: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("des outils")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    tools.sort_unstable();
    assert_eq!(
        tools,
        [
            "get_contact_history",
            "get_message",
            "get_thread",
            "list_folders",
            "search_mail"
        ]
    );

    // Les outils lisent vraiment : une recherche passe par l'index, la liste par SQLite.
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "search_mail", "arguments": { "query": "sujet" } }
        }),
    );
    let found = response(&mut stdout, 3);
    assert!(
        found["result"]["isError"] != true && found.to_string().contains("sujet de test"),
        "{found}"
    );
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "list_folders", "arguments": {} }
        }),
    );
    let folders = response(&mut stdout, 4);
    assert!(folders.to_string().contains("INBOX"), "{folders}");

    // Le temps qu'une passe de fond, montée, aurait mis à démarrer — dix fois.
    if let Some(left) = LIFETIME.checked_sub(started.elapsed()) {
        std::thread::sleep(left);
    }
    drop(stdin);
    let status = maild.wait(Duration::from_secs(20));
    let log = stderr.join().unwrap();
    assert!(status.success(), "{status} — {log}");
    assert!(
        log.contains("en lecture seule"),
        "la ligne qui dit le mode manque : {log}"
    );

    let (after, [wal, shm]) = snapshot(&root);
    // Ni migration, ni moisson, ni passe de fond, ni facteur : rien d'autre n'a bougé, octet pour
    // octet — `index.sqlite`, l'index plein texte, les blobs. Aucun verrou d'écrivain créé.
    assert_eq!(changes(&files, &after), Vec::<String>::new(), "{log}");
    for lock in [
        "attachments.lock",
        "vectors.lock",
        "outbox.lock",
        "outbox.sending",
        "vectors.bin",
    ] {
        assert!(!root.join(lock).exists(), "{lock} créé");
    }
    // Ce que SQLite écrit pour qu'un lecteur lise une base WAL, et rien de plus : `-wal`, créé
    // vide et laissé ; `-shm`, l'index partagé du journal, créé et laissé. Voir
    // `mailcore::store::db::open_reader`.
    assert_eq!(wal.as_deref(), Some(&[][..]), "-wal");
    assert!(shm.is_some(), "-shm");
}

#[test]
fn the_stdio_mcp_server_refuses_an_older_store_and_leaves_it_alone() {
    let (_dir, root) = closed_store();
    // La base remplacée par **une vraie base d'avant**, montée par les pas de migration de
    // `mailcore` jusqu'au schéma précédent : l'ancien chemin la migrait, et c'est l'incident.
    // Réécrire la seule version d'un store à jour ne suffirait pas — la migration y échoue et se
    // défait, et un serveur qui migrerait passerait ce test sans rien écrire.
    let older = mailcore::store::migrations::SCHEMA_VERSION - 1;
    std::fs::remove_file(root.join("index.sqlite")).unwrap();
    {
        let conn = rusqlite::Connection::open(root.join("index.sqlite")).unwrap();
        mailcore::store::db::apply_pragmas(&conn).unwrap();
        mailcore::store::migrations::apply_up_to(&conn, older).unwrap();
    }
    let (files, journal) = snapshot(&root);
    assert_eq!(journal, [None, None], "précondition : un store fermé");

    // L'entrée fermée d'emblée : un serveur qui aurait ouvert le store attendrait sinon un client.
    let mut maild = Maild::spawn(&root, Stdio::null());
    let stderr = maild.stderr();
    let status = maild.wait(Duration::from_secs(20));
    let log = stderr.join().unwrap();

    assert!(!status.success(), "un store ancien a été servi : {log}");
    assert!(
        log.contains("ne migre jamais") && log.contains("avec la commande `mail`"),
        "le refus ne dit pas quoi faire : {log}"
    );
    assert!(
        log.contains(&format!("schéma {older}")),
        "le refus ne dit pas le schéma trouvé : {log}"
    );

    let (after, [wal, _shm]) = snapshot(&root);
    // `index.sqlite` compris, et `-wal` vide : une migration validée serait dans l'un ou l'autre.
    assert_eq!(changes(&files, &after), Vec::<String>::new(), "{log}");
    assert_eq!(
        wal.as_deref(),
        Some(&[][..]),
        "-wal : créé vide, rien de plus"
    );
    let version: u32 = rusqlite::Connection::open_with_flags(
        root.join("index.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row("PRAGMA user_version", [], |row| row.get(0))
    .unwrap();
    assert_eq!(version, older);
}

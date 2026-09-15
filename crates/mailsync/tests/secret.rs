//! **Critère 6 de `docs/PHASE-2.md`** : zéro identifiant en clair dans le store, dans les
//! journaux, dans les temporaires.
//!
//! ## Ce que ce fichier prouve, et ce qu'il ne prouve pas
//!
//! Il fait passer un mot de passe **sentinelle** par tout le chemin réel — connexion, `LOGIN`,
//! découverte des dossiers, moisson de trois messages — puis il **cherche cette sentinelle**
//! dans tout ce que le processus a écrit.
//!
//! Ce n'est pas une relecture de code. Une relecture dit « je ne vois pas où le secret
//! sortirait » ; ceci dit « il n'est pas sorti ». La différence compte parce que le secret
//! traverse une pile qu'on n'a pas écrite : `rustls`, `rusqlite`, `tracing`, et un jour la
//! bibliothèque OAuth2.
//!
//! Ce que ça ne prouve pas : qu'aucune version future ne le fera. C'est le rôle du test, pas
//! de la preuve — il tombera le jour où quelqu'un journalise le mot de passe pour déboguer.
//!
//! ## Deux contrôles positifs, et l'un des deux a servi tout de suite
//!
//! La leçon du critère 8 : **un zéro obtenu par un chercheur aveugle ne prouve rien.**
//!
//! 1. **Le chercheur de fichiers** doit trouver une sentinelle qu'on vient d'écrire. Sans ça,
//!    un scanner qui ne sait pas lire un fichier binaire rendrait « aucune fuite » sur un
//!    store qui en est plein.
//! 2. **Le tampon de journaux ne doit pas être vide.** Ce contrôle a immédiatement attrapé un
//!    test qui passait à vide, et le mécanisme vaut d'être écrit :
//!
//! `tracing` met en cache, **globalement au processus**, l'intérêt de chaque site d'appel. Un
//! `set_default` ne vaut que pour son fil ; quand un autre fil du même binaire n'a aucun
//! abonné, le site d'appel qu'il touche le premier est mis en cache comme « personne ne
//! s'y intéresse » — et les événements du fil qui écoute sont alors **sautés**.
//!
//! Résultat observé : le tampon ne contenait que les deux lignes des sites d'appel touchés
//! d'abord par le bon fil, et « aucun secret dans les journaux » passait sur des journaux
//! quasi vides. D'où un **seul test** dans ce fichier, et un `set_global_default` : un abonné
//! pour tout le binaire, donc un cache d'intérêt cohérent.
//!
//! ## Les journaux sont capturés au niveau `TRACE`
//!
//! Pas au niveau par défaut : au **plus bavard**. Un secret journalisé en `debug!` ne se
//! verrait pas autrement, et c'est précisément le genre de ligne qu'on ajoute « juste pour
//! voir » un jour de panne et qu'on oublie de retirer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use mailcore::Store;
use mailfake::{Config, Server};
use mailsync::client::Client;

/// Le mot de passe cherché.
///
/// Assez improbable pour qu'aucune coïncidence ne le produise — **c'est ce qui rend le
/// balayage du répertoire temporaire du système non ambigu** : aucun autre processus ne peut
/// écrire cette chaîne, donc la trouver quelque part est forcément une fuite d'ici.
///
/// Pas de caractère spécial : on cherche une fuite, pas un problème de citation. Celui-là a
/// son test dans `client`.
const SENTINEL: &str = "SENTINELLE-b7f3a91c2e5d4806-MOTDEPASSE";

/// Plafond de lecture d'un fichier, pour le balayage du répertoire temporaire.
///
/// 8 Mio. Le répertoire temporaire du système contient ce que les autres applications y
/// laissent, parfois des images disque : les lire en entier ferait durer le test des minutes.
/// La limite est une concession, et elle est nommée — un secret de quarante octets n'a aucune
/// raison de n'apparaître qu'après le huitième mégaoctet d'un fichier.
const SCAN_LIMIT: u64 = 8 * 1024 * 1024;

/// Un tampon de journal partagé, branché sur `tracing`.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        let guard = self.0.lock().unwrap_or_else(|it| it.into_inner());
        String::from_utf8_lossy(&guard).into_owned()
    }
}

impl Write for Captured {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.0.lock().unwrap_or_else(|it| it.into_inner());
        guard.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Les fichiers d'une arborescence qui contiennent la sentinelle.
///
/// Les fichiers sont lus **en octets et en entier**. Un secret peut se retrouver dans une page
/// libérée de SQLite, dans un fragment d'index tantivy, dans un temporaire d'écriture — donc
/// pas seulement là où on penserait à regarder.
fn leaks_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let needle = SENTINEL.as_bytes();
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path)
                && bytes.windows(needle.len()).any(|window| window == needle)
            {
                found.push(path);
            }
        }
    }
    found
}

/// Les fichiers du répertoire temporaire du système qui contiennent la sentinelle.
///
/// ## Pourquoi chercher la sentinelle plutôt que les fichiers apparus
///
/// La première version comparait la liste des fichiers avant et après. C'est **faux par
/// construction** : deux tests en parallèle, ou n'importe quelle autre application de la
/// machine, y créent des fichiers pendant la fenêtre de mesure. Le test échouait sur le
/// répertoire temporaire d'un autre test.
///
/// Chercher la sentinelle n'a pas ce défaut : **aucun autre processus ne connaît cette
/// chaîne**. La trouver, c'est une fuite d'ici, et ne pas la trouver ne dépend de personne
/// d'autre.
///
/// Non récursif et plafonné : voir [`SCAN_LIMIT`].
fn leaks_in_system_temp() -> Vec<std::path::PathBuf> {
    let needle = SENTINEL.as_bytes();
    std::fs::read_dir(std::env::temp_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|it| {
            it.metadata()
                .is_ok_and(|m| m.is_file() && m.len() <= SCAN_LIMIT)
        })
        .filter(|it| {
            std::fs::read(it.path())
                .is_ok_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
        })
        .map(|it| it.path())
        .collect()
}

/// **Un seul test dans ce fichier**, et c'est structurel : voir l'en-tête du module. Un
/// deuxième fil sans abonné `tracing` ferait passer la vérification des journaux à vide.
#[test]
fn a_password_never_reaches_the_store_the_logs_or_a_temporary_file() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("aucun abonné global ne doit être posé avant celui-ci");

    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
    let store = Store::open(&root).unwrap();
    let account = {
        let writer = store.writer().unwrap();
        let account = writer
            .upsert_imap_account(
                "Sentinelle",
                &mailcore::Server {
                    host: "127.0.0.1".to_owned(),
                    port: 993,
                    username: "marie@exemple.fr".to_owned(),
                    auth: mailcore::AuthKind::Password,
                    security: mailcore::Security::Tls,
                },
            )
            .unwrap();
        writer.commit().unwrap();
        account
    };

    // --- contrôle positif du chercheur de fichiers, avant toute chose ---
    let control = dir.path().join("controle-positif.txt");
    std::fs::write(&control, SENTINEL.as_bytes()).unwrap();
    assert!(
        leaks_under(dir.path()).contains(&control),
        "le chercheur ne trouve pas une sentinelle qu'on vient d'écrire : \
         son zéro ne prouverait rien"
    );
    std::fs::remove_file(&control).unwrap();
    assert!(
        leaks_under(dir.path()).is_empty(),
        "le contrôle positif n'a pas été retiré"
    );

    // --- le vrai chemin, avec le vrai secret ---
    let server = Server::start(Config {
        password: SENTINEL.to_owned(),
        ..Config::with_inbox()
    })
    .unwrap();

    // Le connecteur TLS refuserait `mailfake`, qui parle en clair — c'est son test à lui.
    // Ici on injecte le flux, et **le secret passe par le vrai `login`**, celui qui cite et
    // déguise, donc par le vrai chemin d'écriture sur la socket.
    let stream = TcpStream::connect(server.address()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut client = Client::greet(stream).unwrap();
    client.login("marie@exemple.fr", SENTINEL).unwrap();

    let folders = mailsync::discover(&mut client, &store, account).unwrap();
    for folder in &folders {
        mailsync::harvest(&mut client, &store, folder.folder, &folder.remote_name).unwrap();
    }
    client.logout();

    // Le store contient bien quelque chose : sans ça, y chercher une fuite ne dirait rien.
    let stats = store.stats().unwrap();
    assert_eq!(
        stats.messages, 3,
        "rien n'a été moissonné : le test est vide"
    );

    // --- les trois vérifications ---
    let found = leaks_under(dir.path());
    assert!(
        found.is_empty(),
        "le mot de passe est dans le store : {found:?}"
    );

    let logs = captured.text();
    // **Le contrôle du tampon vient d'abord.** Un tampon vide passerait la vérification
    // suivante sans rien prouver, et c'est arrivé — voir l'en-tête du module.
    assert!(
        logs.contains("dossiers découverts") && logs.contains("moisson terminée"),
        "les journaux de la moisson n'ont pas été capturés : la vérification ne porte sur \
         rien.\n{logs}"
    );
    assert!(
        !logs.contains(SENTINEL),
        "le mot de passe est dans les journaux, au niveau TRACE"
    );

    // L'identifiant, lui, **est** journalisé, et c'est voulu : savoir pour quel compte une
    // synchronisation a tourné est nécessaire au diagnostic. C'est la ligne qui sépare une
    // trace utile d'une fuite, et l'affirmer ici la rend visible.
    assert!(
        logs.contains("compte") || logs.contains("account"),
        "le compte n'est pas journalisé : le diagnostic serait impossible.\n{logs}"
    );

    let temp = leaks_in_system_temp();
    assert!(
        temp.is_empty(),
        "le mot de passe est dans un fichier temporaire du système : {temp:?}"
    );
}

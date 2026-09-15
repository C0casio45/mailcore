//! **Le critère 2 de `docs/PHASE-3.md`, mesuré et non raisonné** : zéro doublon et zéro perte
//! sur une coupure provoquée entre l'acceptation du serveur et l'écriture locale.
//!
//! ## Pourquoi un vrai processus, tué
//!
//! Tous les autres tests de ce crate simulent l'échec en rendant une erreur. Ça ne prouve rien
//! ici : ce que le critère met en cause n'est pas la gestion d'une erreur, c'est l'**ordre des
//! écritures sur le disque**. Un test qui rend `Err` a laissé tourner les destructeurs, vidé les
//! tampons, fermé la connexion SQLite — donc il valide un monde où rien de ce qui menace la
//! garantie ne se produit.
//!
//! Un processus enfant tué à un instant choisi, lui, laisse le store exactement dans l'état où
//! une panne le laisserait. Le parent le relit ensuite, et compte.
//!
//! ## Les deux instants, et pourquoi ce sont les seuls qui comptent
//!
//! | instant de la coupure | ce que le serveur a | ce que le store dit | ce qui doit suivre |
//! |---|---|---|---|
//! | après le `354`, avant la frontière | rien | `sending` | remise — sinon c'est une **perte** |
//! | après le `250` du serveur, avant l'écriture locale | le message | `committing` | rien — sinon c'est un **doublon** |
//!
//! Le second est le libellé exact du critère : « une coupure provoquée entre l'acceptation du
//! serveur et l'écriture locale ». C'est la seule fenêtre où le protocole ne permet pas de
//! savoir, et donc la seule où la réponse doit être « je ne sais pas », pas « je réessaie ».
//!
//! ## Comment l'enfant est lancé
//!
//! Le binaire de test se relance lui-même — `current_exe`, filtre sur un seul test, et trois
//! variables d'environnement. Pas de second binaire à déclarer dans le manifeste, et pas de
//! script : le test enfant est du Rust compilé avec le reste, donc il ne peut pas dériver de ce
//! qu'il teste.
//!
//! Le corps de l'enfant est [`child_body_never_run_directly`], qui ne fait **rien** quand la
//! variable d'environnement est absente : une exécution normale de `cargo test` le voit passer
//! en quelques microsecondes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use mailcore::{AccountKind, Outgoing, SendState, Store};
use mailsmtp::queue::{Outcome, Transport, deliver_one};
use mailsmtp::{Client, Error, Result};

/// La variable qui dit à l'enfant où couper. Absente, il ne fait rien.
const CUT: &str = "MAILCORE_TEST_CUT";
/// Le store que l'enfant doit ouvrir.
const STORE: &str = "MAILCORE_TEST_STORE";
/// Le port du serveur scripté, tenu par le parent.
const PORT: &str = "MAILCORE_TEST_PORT";

/// Le code de sortie de l'enfant coupé. Distinct de tout code que le harnais de test emploie,
/// pour qu'un enfant qui échoue *autrement* ne passe pas pour une coupure réussie.
const CUT_EXIT: i32 = 97;

// --------------------------------------------------------------------------------------------
// Le serveur scripté, tenu par le parent
// --------------------------------------------------------------------------------------------

/// Un serveur SMTP qui accepte tout et **compte les messages complets qu'il a acceptés**.
///
/// C'est le compteur du critère : « zéro doublon » veut dire que ce nombre vaut un à la fin,
/// jamais deux, et « zéro perte » qu'il ne vaut jamais zéro.
struct Counting {
    port: u16,
    accepted: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Counting {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&accepted);

        std::thread::spawn(move || {
            // Une connexion par tentative de remise, en série. Le fil vit jusqu'à la fin du
            // test ; il meurt avec le processus.
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || serve(stream, &sink));
            }
        });

        Self { port, accepted }
    }

    /// Les messages complets que le serveur a acceptés, dans l'ordre.
    fn accepted(&self) -> Vec<Vec<u8>> {
        self.accepted
            .lock()
            .map(|it| it.clone())
            .unwrap_or_default()
    }
}

/// Sert une connexion : accepte tout, et note le message quand le point final arrive.
///
/// ## Le message est noté **avant** le `250`
///
/// Et c'est le seul ordre honnête. Un vrai serveur écrit le message dans sa file avant de
/// l'accepter ; le noter après enverrait le `250` à un client qui pourrait mourir, ce qui
/// rendrait le compteur du test faux dans le sens qui l'arrange.
fn serve(stream: TcpStream, sink: &Arc<Mutex<Vec<Vec<u8>>>>) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut out = stream.try_clone().unwrap();
    let mut lines = BufReader::new(stream).lines();

    let _ = out.write_all(b"220 compteur pret\r\n");
    let _ = out.flush();

    while let Some(Ok(line)) = lines.next() {
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("EHLO") {
            let _ = out.write_all(b"250-compteur\r\n250 SIZE 0\r\n");
        } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
            let _ = out.write_all(b"250 ok\r\n");
        } else if upper == "DATA" {
            let _ = out.write_all(b"354 vas-y\r\n");
            let _ = out.flush();
            let mut body = Vec::new();
            for raw in lines.by_ref() {
                let Ok(raw) = raw else { return };
                if raw == "." {
                    break;
                }
                body.extend_from_slice(raw.as_bytes());
                body.push(b'\n');
            }
            if let Ok(mut held) = sink.lock() {
                held.push(body);
            }
            let _ = out.write_all(b"250 pris\r\n");
        } else if upper == "QUIT" {
            let _ = out.write_all(b"221 au revoir\r\n");
            let _ = out.flush();
            return;
        } else {
            let _ = out.write_all(b"250 ok\r\n");
        }
        let _ = out.flush();
    }
}

// --------------------------------------------------------------------------------------------
// Le transport, et l'instant où il meurt
// --------------------------------------------------------------------------------------------

/// Où l'enfant se tue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// Après le `354`, **avant** la frontière. Le serveur n'a rien.
    BeforeFrontier,
    /// Après le `250` du serveur, **avant** que la file n'écrive `sent`. Le serveur a tout.
    AfterAccept,
    /// Nulle part : la remise se termine normalement.
    None,
}

impl Cut {
    fn parse(label: &str) -> Self {
        match label {
            "before-frontier" => Self::BeforeFrontier,
            "after-accept" => Self::AfterAccept,
            _ => Self::None,
        }
    }
}

/// Le transport réel du test : un dialogue SMTP complet sur le bouclage, coupé où on veut.
struct Wired {
    port: u16,
    cut: Cut,
}

impl Transport for Wired {
    fn deliver(
        &mut self,
        job: &Outgoing,
        body: &mut dyn std::io::Read,
        frontier: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let stream =
            TcpStream::connect(("127.0.0.1", self.port)).map_err(|source| Error::Network {
                stage: mailsmtp::Stage::Greeting,
                source,
            })?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .ok();
        let mut client = Client::greet(stream)?;
        client.ehlo("exemple.fr")?;
        // La taille vient de la ligne de file, comme dans le vrai transport : un flux ne la
        // donne pas avant de l'avoir lu.
        client.mail_from(&job.sender, Some(job.size).filter(|it| *it > 0))?;
        for recipient in &job.recipients {
            client.rcpt_to(recipient)?;
        }
        client.open_data()?;

        if self.cut == Cut::BeforeFrontier {
            // Le serveur a une transaction **vide**. Il l'abandonnera à son propre délai, et le
            // store dit encore `sending` : la remise peut repartir sans risque de doublon.
            cut_now();
        }

        frontier()?;
        client.finish_data_from(body)?;

        if self.cut == Cut::AfterAccept {
            // **La fenêtre du critère.** Le serveur a répondu `250` : il a le message. La file
            // n'a pas encore écrit `sent`. C'est ici, et nulle part ailleurs, que le protocole
            // ne permet plus de savoir.
            cut_now();
        }

        client.quit();
        Ok(())
    }
}

/// Tue le processus, sans rien conclure.
///
/// `std::process::exit` et non un `panic!` : un `panic!` déroule la pile, ferme la connexion
/// SQLite et vide les tampons — c'est-à-dire tout ce qu'une panne ne fait pas.
///
/// Ce que `exit` ne simule pas, et qu'il faut dire : il rend la main au système, donc le cache
/// de pages garde les écritures non synchronisées. C'est exactement une coupure de **processus**
/// — celle du critère. Une coupure d'**alimentation** irait plus loin, et c'est pour elle que
/// `Store::commit_outgoing` monte `synchronous` à `FULL` : les transitions de la file sont
/// `fsync`ées, donc les deux coupures laissent le même état. Ce test mesure la première ; la
/// seconde repose sur `FULL` et sur un disque qui ne mente pas sur ses `fsync`.
fn cut_now() -> ! {
    std::process::exit(CUT_EXIT);
}

// --------------------------------------------------------------------------------------------
// L'enfant
// --------------------------------------------------------------------------------------------

/// Le corps de l'enfant. **Ne rien faire** quand l'environnement ne le désigne pas.
#[test]
fn child_body_never_run_directly() {
    let Ok(cut) = std::env::var(CUT) else { return };
    let root = std::env::var(STORE).expect("le parent doit dire où est le store");
    let port: u16 = std::env::var(PORT)
        .expect("le parent doit dire où est le serveur")
        .parse()
        .expect("port illisible");

    let store = Store::open(camino::Utf8Path::new(&root)).expect("store illisible");
    let job = store
        .deliverable(i64::MAX, 1)
        .expect("file illisible")
        .into_iter()
        .next()
        .expect("rien à remettre : le parent n'a pas mis de message dans la file");

    let mut transport = Wired {
        port,
        cut: Cut::parse(&cut),
    };
    // Si la coupure n'a pas lieu, ce résultat est celui d'une remise normale — c'est le contrôle
    // négatif, exécuté par `a_delivery_that_is_not_cut_sends_exactly_once`.
    let outcome = deliver_one(&store, &mut transport, &job, 5_000).expect("remise impossible");
    assert_eq!(outcome, Outcome::Sent);
}

// --------------------------------------------------------------------------------------------
// Le parent
// --------------------------------------------------------------------------------------------

/// Un store neuf avec un compte et un message en file.
fn queued(dir: &camino::Utf8Path) -> (Store, mailcore::OutboxId) {
    let store = Store::open(dir).unwrap();
    let account = {
        let writer = store.writer().unwrap();
        let id = writer
            .upsert_account(AccountKind::Imap.as_str(), "compte")
            .unwrap();
        writer.commit().unwrap();
        id
    };
    let raw =
        b"From: marie@exemple.fr\r\nTo: jean@ailleurs.fr\r\nSubject: unique\r\n\r\nune fois\r\n";
    let blob = store.blobs().put(raw).unwrap().hash;
    let id = store
        .enqueue(
            account,
            blob,
            "marie@exemple.fr",
            &["jean@ailleurs.fr".to_owned()],
            // La taille réelle du message : c'est elle qui part dans `SIZE=`.
            raw.len() as u64,
            1_000,
        )
        .unwrap();
    (store, id)
}

/// Lance l'enfant, et rend son code de sortie.
fn run_child(cut: &str, root: &camino::Utf8Path, port: u16) -> Option<i32> {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["child_body_never_run_directly", "--exact", "--nocapture"])
        .env(CUT, cut)
        .env(STORE, root.as_str())
        .env(PORT, port.to_string())
        // Le harnais du parent capture la sortie ; celle de l'enfant n'a pas à s'y mêler.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("l'enfant n'a pas pu être lancé");
    status.code()
}

/// Vide la file jusqu'à ce qu'il n'y ait plus rien à remettre, dans le processus courant.
///
/// C'est ce que fait le démon au redémarrage, et c'est le moment où un doublon apparaîtrait.
fn drain(store: &Store, port: u16) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for _ in 0..4 {
        let pending = store.deliverable(i64::MAX, 10).unwrap();
        if pending.is_empty() {
            break;
        }
        for job in pending {
            let mut transport = Wired {
                port,
                cut: Cut::None,
            };
            outcomes.push(deliver_one(store, &mut transport, &job, 6_000).unwrap());
        }
    }
    outcomes
}

#[test]
fn a_cut_before_the_frontier_loses_nothing_and_duplicates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let server = Counting::start();
    let (store, id) = queued(&root);
    // Le store de l'enfant est le même fichier : il faut lâcher la connexion, sinon les deux
    // processus se disputent le verrou d'écriture pendant tout le test.
    drop(store);

    assert_eq!(
        run_child("before-frontier", &root, server.port),
        Some(CUT_EXIT),
        "l'enfant n'est pas mort là où on l'attendait"
    );
    assert!(
        server.accepted().is_empty(),
        "le serveur a accepté un message alors que le corps n'était pas parti"
    );

    let store = Store::open(&root).unwrap();
    let after = store.outgoing(id).unwrap().unwrap();
    assert_eq!(
        after.state,
        SendState::Sending,
        "l'état après la coupure ne dit pas ce qui s'est passé"
    );
    assert!(
        after.state.is_deliverable(),
        "**une perte** : le message ne repartira jamais"
    );

    let outcomes = drain(&store, server.port);
    assert_eq!(outcomes, vec![Outcome::Sent]);
    assert_eq!(
        server.accepted().len(),
        1,
        "le serveur n'a pas reçu le message exactement une fois : {:?}",
        server.accepted().len()
    );
    assert_eq!(store.outgoing(id).unwrap().unwrap().state, SendState::Sent);
}

#[test]
fn a_cut_between_the_servers_acceptance_and_the_local_write_duplicates_nothing() {
    // **Le critère 2, mot pour mot.** Le serveur a le message ; l'écriture locale n'a pas eu
    // lieu. Zéro doublon veut dire que la reprise n'envoie rien de plus ; zéro perte veut dire
    // que le message est toujours là, visible, et que l'utilisateur peut trancher.
    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let server = Counting::start();
    let (store, id) = queued(&root);
    drop(store);

    assert_eq!(
        run_child("after-accept", &root, server.port),
        Some(CUT_EXIT),
        "l'enfant n'est pas mort là où on l'attendait"
    );
    assert_eq!(
        server.accepted().len(),
        1,
        "le serveur devait avoir le message : sans ça le test ne mesure pas la bonne fenêtre"
    );

    let store = Store::open(&root).unwrap();
    let after = store.outgoing(id).unwrap().unwrap();
    assert_eq!(
        after.state,
        SendState::Committing,
        "l'état après la coupure ne porte pas le doute"
    );
    assert!(after.state.is_doubtful());

    // La reprise. C'est ici qu'un doublon apparaîtrait.
    let outcomes = drain(&store, server.port);
    assert!(
        outcomes.is_empty(),
        "la reprise a tenté de remettre un message douteux : {outcomes:?}"
    );
    assert_eq!(
        server.accepted().len(),
        1,
        "**un doublon** : le destinataire a reçu le message deux fois"
    );

    // Et zéro perte : la ligne est toujours là, et l'interface a de quoi la montrer.
    let doubtful = store.doubtful().unwrap();
    assert_eq!(doubtful.len(), 1, "**une perte** : la ligne a disparu");
    assert_eq!(doubtful[0].id, id);
}

#[test]
fn a_delivery_that_is_not_cut_sends_exactly_once() {
    // Le contrôle positif. Sans lui, un transport qui n'enverrait **jamais rien** passerait les
    // deux tests ci-dessus : zéro doublon est trivial quand zéro message part.
    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let server = Counting::start();
    let (store, id) = queued(&root);
    drop(store);

    assert_eq!(
        run_child("none", &root, server.port),
        Some(0),
        "une remise non coupée doit réussir"
    );
    assert_eq!(server.accepted().len(), 1);

    let store = Store::open(&root).unwrap();
    assert_eq!(store.outgoing(id).unwrap().unwrap().state, SendState::Sent);
    assert!(store.deliverable(i64::MAX, 10).unwrap().is_empty());

    let body = &server.accepted()[0];
    let text = String::from_utf8_lossy(body);
    assert!(
        text.contains("Subject: unique"),
        "le serveur n'a pas reçu le message attendu : {text}"
    );
}

#[test]
fn the_message_the_server_received_is_the_one_that_was_queued() {
    // Le contrôle de contenu, séparé du comptage. Un envoi « exactement une fois » d'un message
    // tronqué ne vaut rien, et le doublage du point est la transformation qui peut le tronquer
    // sans rien signaler.
    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let server = Counting::start();
    let store = Store::open(&root).unwrap();
    let account = {
        let writer = store.writer().unwrap();
        let id = writer
            .upsert_account(AccountKind::Imap.as_str(), "compte")
            .unwrap();
        writer.commit().unwrap();
        id
    };
    // Une ligne qui commence par un point : elle terminerait le transfert si elle n'était pas
    // doublée, et le serveur ne verrait que la moitié du message.
    let raw = b"From: marie@exemple.fr\r\nSubject: piege\r\n\r\navant\r\n.cache\r\napres\r\n";
    let blob = store.blobs().put(raw).unwrap().hash;
    let id = store
        .enqueue(
            account,
            blob,
            "marie@exemple.fr",
            &["jean@ailleurs.fr".to_owned()],
            raw.len() as u64,
            1_000,
        )
        .unwrap();
    drop(store);

    assert_eq!(run_child("none", &root, server.port), Some(0));
    let seen = server.accepted();
    assert_eq!(seen.len(), 1);
    let text = String::from_utf8_lossy(&seen[0]);
    assert!(
        text.contains("apres"),
        "le transfert a été terminé par la ligne de point : {text}"
    );
    // Le serveur de test ne retire pas le point doublé — il rend ce qu'il a lu. Ce qui compte
    // est que la suite soit arrivée.
    assert!(
        text.contains("..cache"),
        "le point n'a pas été doublé : {text}"
    );

    let store = Store::open(&root).unwrap();
    assert_eq!(store.outgoing(id).unwrap().unwrap().state, SendState::Sent);
}

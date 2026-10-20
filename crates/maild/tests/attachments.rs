//! La lecture des PDF joints, de bout en bout : le parent décide et range, et **le vrai binaire
//! `maild`**, relancé en extracteur, lit les pièces.
//!
//! C'est ce qu'aucun test de `mailpdf` ni de `mailcore` ne peut dire : que l'enfant se lance sous
//! sa borne, que le protocole passe, et surtout **qu'une mort est attribuée à la bonne cause** —
//! jamais une pièce accusée parce qu'un job a tué l'enfant, jamais un message condamné à la
//! première mort. Les morts sont fabriquées par le crochet de test de l'enfant : une marque écrite
//! dans un PDF, qui n'agit que si le parent pose la variable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use lopdf::{Document, Object, Stream, StringFormat, dictionary};
use mailcore::attachtext::{BUDGET, PdfState};
use mailcore::model::{FolderKind, MessageFlags};
use mailcore::{MessageId, NewMessage, Store};
use maild::attachments::{Extractor, Failure, Limits, Passed, Watch, catch_up};
use maild::jobs::Activity;

fn program() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_BIN_EXE_maild"))
}

/// Un PDF d'une page dont le texte est `words`, en Helvetica WinAnsi — et `mark`, une marque du
/// crochet de test, écrite en commentaire **après** la fin du fichier : les décalages de la table
/// des références ne bougent pas, et sans crochet le PDF se lit comme un autre.
fn pdf(words: &str, mark: Option<&str>) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    doc.trailer.set(
        "ID",
        Object::Array(vec![
            Object::String(b"identifiant-1".to_vec(), StringFormat::Literal),
            Object::String(b"identifiant-2".to_vec(), StringFormat::Literal),
        ]),
    );
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => Object::Reference(font_id) },
    });
    let content = format!("BT /F1 12 Tf 72 720 Td ({words}) Tj ET");
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        "Resources" => Object::Reference(resources_id),
        "Contents" => Object::Reference(content_id),
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    if let Some(mark) = mark {
        out.extend_from_slice(format!("\n%mailcore-test:{mark}\n").as_bytes());
    }
    out
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    let mut wrapped = String::new();
    for (i, c) in out.chars().enumerate() {
        if i > 0 && i % 76 == 0 {
            wrapped.push_str("\r\n");
        }
        wrapped.push(c);
    }
    wrapped
}

/// Un message qui porte ces PDF, dans cet ordre.
fn message(subject: &str, pdfs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = format!(
        "From: a@exemple.invalid\r\nTo: b@exemple.invalid\r\nSubject: {subject}\r\n\
         Message-ID: <{subject}@exemple.invalid>\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"limite\"\r\n\r\n\
         --limite\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nVoir les pièces.\r\n"
    );
    for (rank, bytes) in pdfs.iter().enumerate() {
        out.push_str(&format!(
            "--limite\r\nContent-Type: application/pdf; name=\"piece-{rank}.pdf\"\r\n\
             Content-Disposition: attachment; filename=\"piece-{rank}.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            base64(bytes)
        ));
    }
    out.push_str("--limite--\r\n");
    out.into_bytes()
}

fn store() -> (tempfile::TempDir, Utf8PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
    let store = Store::open(&root).unwrap();
    (dir, root, store)
}

/// Range un message comme le ferait un import : à lire par l'extracteur.
fn insert(store: &Store, rfc822: &[u8]) -> MessageId {
    let hash = store.blobs().put(rfc822).unwrap().hash;
    let writer = store.writer().unwrap();
    let account = writer.upsert_account("imap", "compte").unwrap();
    let folder = writer
        .upsert_folder(account, "INBOX", FolderKind::Inbox)
        .unwrap();
    let (id, _) = writer
        .insert_message(&NewMessage {
            blob: hash,
            rfc822_id: None,
            date: 1_700_000_000,
            from_addr: "a@exemple.invalid",
            from_name: None,
            subject: "pièces",
            size: rfc822.len() as u64,
            has_attachments: true,
        })
        .unwrap();
    writer
        .insert_ref(id, folder, 1_700_000_000, MessageFlags::empty())
        .unwrap();
    writer.commit().unwrap();
    // Déjà indexé et vectorisé, comme tout le store du quotidien au premier passage : seule la
    // lecture d'un PDF doit le remettre.
    store.mark_indexed(&[(id, 0)]).unwrap();
    store.mark_vectorised(&[(id, 0)]).unwrap();
    id
}

fn texts(store: &Store, id: MessageId) -> Vec<String> {
    store.attachment_texts(id, BUDGET).unwrap().1
}

/// Un passage, dans un enfant au crochet posé, sans job ni arrêt.
fn pass(extractor: &mut Extractor, store: &Store) -> Passed {
    let stop = AtomicBool::new(false);
    let activity = Activity::default();
    let child = Mutex::new(None);
    extractor.pass(
        store,
        &Watch {
            stop: &stop,
            activity: &activity,
            child: &child,
        },
    )
}

fn hooked(dir: &Utf8Path) -> Extractor {
    let witnesses = dir.join("témoins");
    std::fs::create_dir_all(&witnesses).unwrap();
    Extractor::new(program()).with_hook(witnesses.as_std_path())
}

#[test]
fn the_text_of_a_pdf_is_read_ranged_and_handed_back_and_a_shared_one_is_not_read_twice() {
    let (_dir, root, store) = store();
    let invoice = pdf("facture du mois 1234,56", None);
    let first = insert(&store, &message("un", std::slice::from_ref(&invoice)));
    let reply = insert(&store, &message("deux", &[invoice]));
    let plain = insert(&store, &message("trois", &[]));

    let (marked, remarked) = catch_up(&root, program()).unwrap();
    assert_eq!((marked, remarked), (3, 2));
    for id in [first, reply] {
        let found = texts(&store, id);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("facture du mois"), "{found:?}");
    }
    assert!(texts(&store, plain).is_empty());
    assert_eq!(store.unscanned_total().unwrap(), 0);
    // Les deux messages ont été remis à l'index, le troisième non.
    assert_eq!(store.unindexed_total().unwrap(), 2);
}

/// Le contrôle inverse du crochet : sans la variable, une marque ne fait rien — le PDF se lit.
#[test]
fn a_marked_pdf_is_read_like_any_other_without_the_hook() {
    let (_dir, root, store) = store();
    let id = insert(
        &store,
        &message("marqué", &[pdf("bonjour tout le monde", Some("crash"))]),
    );
    catch_up(&root, program()).unwrap();
    assert_eq!(store.message_pdfs(id).unwrap(), [(0, PdfState::Text)]);
}

/// Une mort sans cause n'accuse qu'à la seconde : le message est relu seul, meurt encore, et
/// seule la pièce mortelle est condamnée — celles qui l'entourent sont lues.
#[test]
fn a_deadly_piece_is_condemned_at_its_second_death_and_its_neighbours_are_read() {
    let (_dir, root, store) = store();
    let id = insert(
        &store,
        &message(
            "mortel",
            &[
                pdf("avant la panne", None),
                pdf("jamais lu", Some("crash")),
                pdf("après la panne", None),
            ],
        ),
    );
    let mut extractor = hooked(&root);

    // Première mort : rien n'est condamné, ce qui précède est gardé.
    assert!(matches!(
        pass(&mut extractor, &store),
        Passed::Advanced { .. }
    ));
    assert_eq!(store.message_pdfs(id).unwrap(), [(0, PdfState::Text)]);
    assert_eq!(store.unscanned_total().unwrap(), 1);

    // Seconde mort, le message seul : la pièce est condamnée.
    assert!(matches!(
        pass(&mut extractor, &store),
        Passed::Advanced { .. }
    ));
    assert_eq!(
        store.message_pdfs(id).unwrap(),
        [(0, PdfState::Text), (1, PdfState::Crashed)]
    );
    assert_eq!(store.unscanned_total().unwrap(), 1);

    // Le passage suivant connaît les deux premières et lit la troisième.
    assert!(matches!(
        pass(&mut extractor, &store),
        Passed::Advanced { .. }
    ));
    assert_eq!(
        store.message_pdfs(id).unwrap(),
        [
            (0, PdfState::Text),
            (1, PdfState::Crashed),
            (2, PdfState::Text)
        ]
    );
    assert_eq!(store.unscanned_total().unwrap(), 0);
    let found = texts(&store, id);
    assert!(found[0].contains("avant la panne") && found[1].contains("après la panne"));
}

/// Le contrôle inverse : une mort qui ne se reproduit pas n'accuse personne.
#[test]
fn a_single_death_accuses_nothing() {
    let (_dir, root, store) = store();
    let id = insert(
        &store,
        &message(
            "innocent",
            &[pdf("bonjour tout le monde", Some("crash-once"))],
        ),
    );
    let mut extractor = hooked(&root);
    pass(&mut extractor, &store);
    pass(&mut extractor, &store);
    assert_eq!(store.message_pdfs(id).unwrap(), [(0, PdfState::Text)]);
    assert_eq!(store.unscanned_total().unwrap(), 0);
}

/// Une panique de la bibliothèque ne tue pas l'enfant : la pièce est illisible, la suivante lue,
/// le tout en un passage.
#[test]
fn a_panic_makes_a_piece_unreadable_and_the_child_goes_on() {
    let (_dir, root, store) = store();
    let id = insert(
        &store,
        &message(
            "panique",
            &[
                pdf("jamais lu", Some("panic")),
                pdf("bonjour tout le monde", None),
            ],
        ),
    );
    let mut extractor = hooked(&root);
    assert_eq!(
        pass(&mut extractor, &store),
        Passed::Advanced {
            marked: 1,
            remarked: 1
        }
    );
    assert_eq!(
        store.message_pdfs(id).unwrap(),
        [(0, PdfState::Invalid), (1, PdfState::Text)]
    );
}

/// Un enfant qui calcule sans fin épuise son budget processeur : la pièce passe « délai », du
/// premier coup — la cause est connue.
#[test]
fn a_piece_that_spins_runs_out_of_processor_time() {
    let (_dir, root, store) = store();
    let id = insert(
        &store,
        &message("boucle", &[pdf("jamais lu", Some("spin"))]),
    );
    let mut extractor = hooked(&root).with_limits(Limits {
        cpu: Duration::from_secs(1),
        ..Limits::default()
    });
    pass(&mut extractor, &store);
    assert_eq!(store.message_pdfs(id).unwrap(), [(0, PdfState::TimedOut)]);
    // Connue au passage suivant, elle n'est pas relue, et le message est fini.
    pass(&mut extractor, &store);
    assert_eq!(store.unscanned_total().unwrap(), 0);
}

/// Le contrôle inverse du budget : un enfant qui attend sans calculer n'est **pas** accusé — une
/// machine pleine fait attendre un enfant en priorité minimale. Le filet du silence le tue, et le
/// passage recule sans rien écrire.
#[test]
fn a_child_that_only_waits_is_never_accused() {
    let (_dir, root, store) = store();
    let id = insert(
        &store,
        &message("attente", &[pdf("jamais lu", Some("sleep"))]),
    );
    let mut extractor = hooked(&root).with_limits(Limits {
        cpu: Duration::from_secs(1),
        silence: Duration::from_secs(3),
        ..Limits::default()
    });
    assert_eq!(
        pass(&mut extractor, &store),
        Passed::Failed(Failure::Silent)
    );
    assert!(store.message_pdfs(id).unwrap().is_empty());
    assert_eq!(store.unscanned_total().unwrap(), 1);
}

/// Au-delà de sa borne, l'enfant avorte sur une allocation refusée, et la pièce passe
/// « mémoire » — l'assertion porte sur la borne : sans elle, ce test allouerait sans fin.
#[test]
fn a_piece_past_the_memory_bound_is_marked_out_of_memory() {
    let (_dir, root, store) = store();
    let id = insert(
        &store,
        &message("bombe", &[pdf("jamais lu", Some("allocate"))]),
    );
    let mut extractor = hooked(&root).with_limits(Limits {
        memory: 256 * 1024 * 1024,
        ..Limits::default()
    });
    pass(&mut extractor, &store);
    assert_eq!(
        store.message_pdfs(id).unwrap(),
        [(0, PdfState::OutOfMemory)]
    );
}

/// Un enfant tué parce que la recherche par le sens a démarré n'accuse rien, et le message reste
/// à lire.
#[test]
fn a_child_killed_for_a_search_accuses_nothing() {
    let (_dir, root, store) = store();
    let id = insert(&store, &message("tué", &[pdf("jamais lu", Some("sleep"))]));
    let mut extractor = hooked(&root);
    let stop = AtomicBool::new(false);
    let activity = Activity::default();
    let child = Mutex::new(None);
    let holder = activity.clone();
    let search = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        holder.hold()
    });
    let passed = extractor.pass(
        &store,
        &Watch {
            stop: &stop,
            activity: &activity,
            child: &child,
        },
    );
    let _held = search.join().unwrap();
    assert_eq!(passed, Passed::Stopped);
    assert!(store.message_pdfs(id).unwrap().is_empty());
    assert_eq!(store.unscanned_total().unwrap(), 1);
}

/// Deux extracteurs sur un même store : le second voit le verrou et ne lance rien.
#[test]
fn a_second_extractor_on_the_same_store_waits_its_turn() {
    let (_dir, _root, store) = store();
    insert(
        &store,
        &message("un", &[pdf("bonjour tout le monde", None)]),
    );
    let _held = mailcore::store::pdfs::ScanLock::try_acquire(&store)
        .unwrap()
        .unwrap();
    let mut extractor = Extractor::new(program());
    assert_eq!(pass(&mut extractor, &store), Passed::Nothing);
    assert_eq!(extractor.status(), maild::attachments::Status::Busy);
    assert_eq!(store.unscanned_total().unwrap(), 1);
}

/// De bout en bout, dans le fil de fond monté comme le monte le service : le PDF d'un message
/// déjà indexé est lu, une mise à jour de l'index est demandée aux jobs, et son texte devient
/// trouvable — sans que personne n'ait rien lancé.
#[test]
fn the_background_thread_reads_a_pdf_and_the_index_follows_by_itself() {
    let (_dir, root, store) = store();
    insert(
        &store,
        &message("facture", &[pdf("relevé électricité référence 2608", None)]),
    );
    mailcore::index::update(&store, &mailcore::Progress::new()).unwrap();
    assert!(
        mailcore::Mailbox::open(&root)
            .unwrap()
            .search("électricité", 10)
            .unwrap()
            .is_empty(),
        "le mot est déjà trouvable : ce test ne prouverait rien"
    );

    let activity = Activity::default();
    let mailbox = std::sync::Arc::new(Mutex::new(mailcore::Mailbox::open(&root).unwrap()));
    let jobs = maild::jobs::Jobs::start_with(
        std::sync::Arc::clone(&mailbox),
        root.clone(),
        maild::jobs::Sources::default(),
        activity.clone(),
    );
    let witnesses = root.join("témoins");
    std::fs::create_dir_all(&witnesses).unwrap();
    let vectoriser = maild::vectors::Vectoriser::start_with_hook(
        &root,
        activity,
        maild::vectors::Launch::program(program()),
        Duration::ZERO,
        witnesses.as_std_path(),
        Limits::default(),
    );
    let follow_jobs = jobs.clone();
    vectoriser.on_follow(move || {
        let _ = follow_jobs.follow();
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let found = mailbox
            .lock()
            .unwrap()
            .search("électricité", 10)
            .unwrap()
            .len();
        if found == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "jamais trouvé ; lecture {:?}, jobs {:?}",
            vectoriser.attachments_status(),
            jobs.list()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        jobs.list()
            .iter()
            .any(|job| job.kind == maild::jobs::Kind::Follow),
        "l'index a suivi sans mise à jour demandée"
    );
    vectoriser.stop();
}

/// Un message qui laisse l'enfant silencieux n'est pas accusé, **mais il ne barre plus la route** :
/// mis de côté jusqu'au prochain lancement, les messages plus anciens sont lus. Avant le
/// 2026-10-20, il passait en tête de chaque passage, et rien de plus ancien n'était jamais lu.
#[test]
fn a_silent_message_is_set_aside_and_the_older_ones_are_read() {
    let (_dir, root, store) = store();
    let older = insert(
        &store,
        &message("ancien", &[pdf("relevé de compte du mois", None)]),
    );
    let silent = insert(
        &store,
        &message("silencieux", &[pdf("jamais lu", Some("sleep"))]),
    );
    let mut extractor = hooked(&root).with_limits(Limits {
        silence: Duration::from_secs(2),
        backoff: Duration::from_millis(10),
        ..Limits::default()
    });
    assert_eq!(
        pass(&mut extractor, &store),
        Passed::Failed(Failure::Silent)
    );
    std::thread::sleep(Duration::from_millis(50));
    assert!(matches!(
        pass(&mut extractor, &store),
        Passed::Advanced { .. }
    ));
    assert_eq!(store.message_pdfs(older).unwrap(), [(0, PdfState::Text)]);
    // Et le silencieux n'est accusé de rien : il reste à lire.
    assert!(store.message_pdfs(silent).unwrap().is_empty());
    assert_eq!(store.unscanned_total().unwrap(), 1);
}

/// L'état est publié **avant** que l'enfant ne travaille, pas après sa sortie : pendant tout le
/// premier passage, l'écran disait encore « commence peu après le démarrage ».
#[test]
fn the_state_is_published_while_the_child_works() {
    let (_dir, root, store) = store();
    insert(
        &store,
        &message("attente", &[pdf("jamais lu", Some("sleep"))]),
    );
    let slot = std::sync::Arc::new(Mutex::new(maild::attachments::Status::Waiting));
    let mut extractor = hooked(&root)
        .with_limits(Limits {
            silence: Duration::from_secs(3),
            ..Limits::default()
        })
        .publishing_to(std::sync::Arc::clone(&slot));
    let watcher = {
        let slot = std::sync::Arc::clone(&slot);
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                if matches!(
                    *slot.lock().unwrap(),
                    maild::attachments::Status::Working { pending: 1 }
                ) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        })
    };
    pass(&mut extractor, &store);
    assert!(
        watcher.join().unwrap(),
        "« en cours » jamais publié pendant la lecture"
    );
    assert_eq!(
        *slot.lock().unwrap(),
        maild::attachments::Status::Failed(Failure::Silent)
    );
}

/// « En pause » seulement quand quelque chose attend : sans rien à lire, une recherche par le sens
/// laisse l'état à « tout est lu ».
#[test]
fn a_search_pauses_only_a_reader_that_has_something_to_read() {
    let (_dir, root, store) = store();
    let stop = AtomicBool::new(false);
    let activity = Activity::default();
    let child = Mutex::new(None);
    let _held = activity.hold();
    let watch = Watch {
        stop: &stop,
        activity: &activity,
        child: &child,
    };
    let mut extractor = Extractor::new(program());
    assert_eq!(extractor.pass(&store, &watch), Passed::Nothing);
    assert_eq!(extractor.status(), maild::attachments::Status::Idle);
    // Le contrôle inverse : un message à lire, et la même recherche met la lecture en pause.
    insert(
        &store,
        &message("un", &[pdf("bonjour tout le monde", None)]),
    );
    assert_eq!(extractor.pass(&store, &watch), Passed::Nothing);
    assert_eq!(extractor.status(), maild::attachments::Status::Paused);
    let _ = root;
}

/// Un passage interrompu par une recherche a déjà validé des remises : elles sont dues à l'index
/// quand même. Avant le 2026-10-20, seul un passage allé au bout les comptait, et le texte de ces
/// PDF n'entrait dans la recherche par mots qu'à la moisson suivante — jamais, sur un store sans
/// compte IMAP.
#[test]
fn the_remarks_of_an_interrupted_pass_are_still_owed_to_the_index() {
    let (_dir, root, store) = store();
    insert(
        &store,
        &message("bloquant", &[pdf("jamais lu", Some("sleep"))]),
    );
    let read = insert(
        &store,
        &message("lu", &[pdf("relevé de compte du mois", None)]),
    );
    let mut extractor = hooked(&root);
    let stop = AtomicBool::new(false);
    let activity = Activity::default();
    let child = Mutex::new(None);
    let holder = activity.clone();
    // Assez pour que le message lu soit validé — une validation toutes les deux secondes.
    let search = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(3_500));
        holder.hold()
    });
    let passed = extractor.pass(
        &store,
        &Watch {
            stop: &stop,
            activity: &activity,
            child: &child,
        },
    );
    let _held = search.join().unwrap();
    assert_eq!(passed, Passed::Stopped);
    assert_eq!(store.message_pdfs(read).unwrap(), [(0, PdfState::Text)]);
    assert_eq!(
        extractor.take_remarked(),
        1,
        "la remise validée n'est pas due"
    );
    assert_eq!(
        extractor.take_remarked(),
        0,
        "le compte n'a pas été remis à zéro"
    );
}

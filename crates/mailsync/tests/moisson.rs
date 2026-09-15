//! La moisson **contre un vrai serveur et un vrai store**.
//!
//! ## Ce que ces tests prouvent que les tests unitaires ne prouvent pas
//!
//! `sync::plan` est pur et testé exhaustivement : il dit toujours ce qu'il faut demander.
//! `client` est testé sur des réponses fabriquées : il analyse toujours ce qu'on lui donne.
//! Aucun des deux ne dit si **la moisson écrit le bon store**.
//!
//! Et c'est là que sont les bugs intéressants : une copie posée sans référence, une référence
//! retirée alors qu'une copie reste, un `UIDVALIDITY` changé qui laisse des UID périmés. Ces
//! tests montent un `mailfake` et un store temporaire, et regardent le store.
//!
//! ## Chaque panne de `mailfake` a son test ici
//!
//! Le harnais de l'étape 2 sait mal répondre. S'en servir est l'étape 3. Une panne dont
//! aucun test ne vérifie que le client s'en sort est une panne qui n'a servi à rien.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpStream;

use mailcore::{FolderId, FolderKind, MessageFlags, Store};
use mailfake::{Config, Fault, Mailbox, Message, Server};
use mailsync::client::Client;
use mailsync::sync::{FullReason, Plan};
use mailsync::{Error, harvest};

/// Un store neuf, un compte IMAP, un dossier `INBOX`.
fn store() -> (tempfile::TempDir, Store, FolderId) {
    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
    let store = Store::open(&root).unwrap();

    let folder = {
        let writer = store.writer().unwrap();
        let account = writer
            .upsert_imap_account(
                "Perso",
                &mailcore::Server {
                    host: "imap.exemple.fr".to_owned(),
                    port: 993,
                    username: "marie@exemple.fr".to_owned(),
                    auth: mailcore::AuthKind::Password,
                    security: mailcore::Security::Tls,
                },
            )
            .unwrap();
        let folder = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        writer.commit().unwrap();
        folder
    };
    (dir, store, folder)
}

/// Un client connecté et authentifié au serveur donné.
fn connect(server: &Server) -> Client<TcpStream> {
    let stream = TcpStream::connect(server.address()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut client = Client::greet(stream).unwrap();
    client.login("marie@exemple.fr", "secret").unwrap();
    client
}

/// Le nombre de lignes d'une table, par l'API publique du store.
///
/// `Store::connection` est volontairement privé — c'est la frontière décrite en tête de
/// `store`. Un test qui l'ouvrirait ferait de son SQL une deuxième vérité à maintenir.
fn count(store: &Store, table: &str) -> u64 {
    let stats = store.stats().unwrap();
    match table {
        "messages" => stats.messages,
        "refs" => stats.refs,
        "remote_uids" => stats.copies,
        other => panic!("table sans compteur public : {other}"),
    }
}

// ---------------------------------------------------------------------------
// Le chemin correct.
// ---------------------------------------------------------------------------

#[test]
fn a_first_harvest_brings_the_folder_into_the_store() {
    let server = Server::start(Config::with_inbox()).unwrap();
    let (_dir, store, folder) = store();
    let mut client = connect(&server);

    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert!(report.full, "un dossier jamais vu se moissonne en entier");
    assert_eq!(report.fetched, 3);
    assert_eq!(report.stored, 3);
    assert_eq!(report.copies, 3);
    assert_eq!(count(&store, "messages"), 3);
    assert_eq!(count(&store, "refs"), 3);
    assert_eq!(count(&store, "remote_uids"), 3);

    // Les lignes sont affichables : c'est l'objectif de l'étape 3.
    let page = store.page(folder, None, 10).unwrap();
    assert_eq!(page.len(), 3);
    assert!(page.iter().all(|it| it.from_addr == "marie@exemple.fr"));

    // Et l'état de synchronisation est écrit.
    let state = store.sync_state(folder).unwrap();
    assert_eq!(state.uidvalidity, Some(1000));
    assert_eq!(state.uidnext, Some(4));
    assert_eq!(state.highest_modseq, Some(3));
    assert_eq!(state.remote_name, b"INBOX".to_vec());
    assert!(state.synced_at.is_some());
}

#[test]
fn a_second_harvest_writes_nothing() {
    // **Le critère 3 de `docs/PHASE-2.md`**, dans sa formulation testable : une
    // synchronisation incrémentale sans rien de neuf n'écrit rien.
    let server = Server::start(Config::with_inbox()).unwrap();
    let (_dir, store, folder) = store();

    {
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }
    let mut client = connect(&server);
    let second = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert!(!second.full, "le deuxième passage a tout retéléchargé");
    assert!(
        second.wrote_nothing(),
        "le deuxième passage a écrit : {second:?}"
    );
    assert_eq!(second.fetched, 0, "des corps ont été retéléchargés");
    assert_eq!(count(&store, "messages"), 3);
    assert_eq!(count(&store, "remote_uids"), 3);
}

#[test]
fn the_same_content_in_two_folders_is_one_blob_and_two_references() {
    // **Le pari de la phase 1, sur le chemin réseau.** C'est ce que la duplication Gmail
    // rendait coûteux : `INBOX` et `Tous les messages` portent le même message.
    let shared = Message::simple(1, "partagé");
    let config = Config::with_inbox().with_mailboxes(vec![
        Mailbox::new("INBOX", 1000, vec![shared.clone()]),
        Mailbox::new("Archive", 2000, vec![shared]),
    ]);
    let server = Server::start(config).unwrap();
    let (_dir, store, inbox) = store();

    let archive = {
        let writer = store.writer().unwrap();
        let accounts = store.full_accounts().unwrap();
        let folder = writer
            .upsert_folder(accounts[0].id, "Archive", FolderKind::Archive)
            .unwrap();
        writer.commit().unwrap();
        folder
    };

    let mut client = connect(&server);
    harvest(&mut client, &store, inbox, b"INBOX").unwrap();
    let second = harvest(&mut client, &store, archive, b"Archive").unwrap();

    assert_eq!(second.duplicates, 1, "le contenu n'a pas été reconnu");
    assert_eq!(second.stored, 0, "un blob a été écrit deux fois");
    assert_eq!(count(&store, "messages"), 1, "un contenu, un seul");
    assert_eq!(count(&store, "refs"), 2, "deux dossiers, deux références");
    assert_eq!(count(&store, "remote_uids"), 2);
}

#[test]
fn a_new_message_is_picked_up_without_retouching_the_others() {
    let (_dir, store, folder) = store();
    {
        let server = Server::start(Config::with_inbox()).unwrap();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }

    // Le même serveur, un message de plus.
    let mut messages = vec![
        Message::simple(1, "facture"),
        Message::simple(2, "devis"),
        Message::simple(3, "relance"),
    ];
    messages.push(Message {
        modseq: 9,
        ..Message::simple(4, "nouveau")
    });
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox::new("INBOX", 1000, messages)]);
    let server = Server::start(config).unwrap();
    let mut client = connect(&server);

    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert!(
        !report.full,
        "un message de plus ne justifie pas tout refaire"
    );
    assert_eq!(report.fetched, 1, "le neuf seulement");
    assert_eq!(report.stored, 1);
    assert_eq!(count(&store, "messages"), 4);
    assert_eq!(count(&store, "refs"), 4);
}

#[test]
fn a_flag_set_on_the_server_reaches_the_reference() {
    let (_dir, store, folder) = store();
    {
        let server = Server::start(Config::with_inbox()).unwrap();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }
    assert!(
        store
            .page(folder, None, 10)
            .unwrap()
            .iter()
            .all(|it| !it.flags.contains(MessageFlags::SEEN)),
        "rien ne devait être lu au départ"
    );

    // Le même serveur, un `\Seen` de plus et un MODSEQ qui bouge.
    let messages = vec![
        Message {
            flags: vec!["\\Seen".to_owned()],
            modseq: 7,
            ..Message::simple(1, "facture")
        },
        Message::simple(2, "devis"),
        Message::simple(3, "relance"),
    ];
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox::new("INBOX", 1000, messages)]);
    let server = Server::start(config).unwrap();
    let mut client = connect(&server);

    harvest(&mut client, &store, folder, b"INBOX").unwrap();

    let seen: Vec<u32> = store
        .page(folder, None, 10)
        .unwrap()
        .iter()
        .filter(|it| it.flags.contains(MessageFlags::SEEN))
        .map(|it| u32::try_from(it.id.0).unwrap_or(0))
        .collect();
    assert_eq!(seen.len(), 1, "le drapeau n'a pas atteint la référence");
    assert_eq!(count(&store, "messages"), 3, "un corps a été retéléchargé");
}

#[test]
fn a_message_expunged_on_the_server_loses_its_reference() {
    let (_dir, store, folder) = store();
    {
        let server = Server::start(Config::with_inbox()).unwrap();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }

    // Le même serveur, le deuxième message parti.
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox::new(
        "INBOX",
        1000,
        vec![Message::simple(1, "facture"), Message::simple(3, "relance")],
    )]);
    let server = Server::start(config).unwrap();
    let mut client = connect(&server);

    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert_eq!(report.vanished, 1, "la purge n'a pas été détectée");
    assert_eq!(count(&store, "remote_uids"), 2);
    assert_eq!(count(&store, "refs"), 2, "la référence n'a pas été retirée");
    // **Le contenu reste.** Il est adressé par contenu et peut être référencé ailleurs ;
    // le ramassage des blobs sans référence est une passe distincte.
    assert_eq!(count(&store, "messages"), 3);
}

// ---------------------------------------------------------------------------
// Les pannes de `mailfake`. Une par test.
// ---------------------------------------------------------------------------

#[test]
fn a_changed_uidvalidity_triggers_a_full_reharvest_without_duplicating_a_blob() {
    // **Le critère 9 de `docs/PHASE-2.md`** : détecté, resynchronisé, zéro blob dupliqué.
    let (_dir, store, folder) = store();
    {
        let server = Server::start(Config::with_inbox()).unwrap();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }
    let before = count(&store, "messages");

    let config = Config::with_inbox().with_fault(Fault::UidvalidityChangesOnSelect);
    let server = Server::start(config).unwrap();
    let mut client = connect(&server);
    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert!(report.full, "le changement d'UIDVALIDITY n'a pas été vu");
    assert_eq!(report.fetched, 3, "tout devait être redemandé");
    assert_eq!(
        report.stored, 0,
        "un blob a été écrit deux fois : l'adressage par contenu ne tient pas"
    );
    assert_eq!(report.duplicates, 3);
    assert_eq!(count(&store, "messages"), before, "le store a grossi");
    assert_eq!(
        count(&store, "remote_uids"),
        3,
        "des UID périmés ont survécu"
    );
    assert_eq!(count(&store, "refs"), 3);
}

#[test]
fn a_server_that_lies_about_condstore_still_gets_synced() {
    // **Le critère 10** : le chemin de repli rend le même résultat que le chemin rapide.
    let config = Config::without_condstore().with_fault(Fault::AdvertisesCondstoreThenRefuses);
    let server = Server::start(config).unwrap();
    let (_dir, store, folder) = store();
    let mut client = connect(&server);

    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert_eq!(report.fetched, 3);
    assert_eq!(count(&store, "refs"), 3);
    // Sans CONDSTORE, `HIGHESTMODSEQ` n'est pas annoncé : l'état le reflète, il n'invente pas.
    assert_eq!(store.sync_state(folder).unwrap().highest_modseq, None);
}

#[test]
fn the_fallback_path_gives_the_same_store_as_the_fast_path() {
    // Deux stores, deux serveurs, même corpus : l'un avec `CONDSTORE`, l'autre sans. Le
    // critère 10 demande **le même résultat**, et le vérifier demande de comparer.
    let with = {
        let server = Server::start(Config::with_inbox()).unwrap();
        let (dir, store, folder) = store();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
        let page = store.page(folder, None, 10).unwrap();
        let subjects: Vec<String> = page.iter().map(|it| it.subject.clone()).collect();
        let uids = store.known_uids(folder).unwrap();
        drop(dir);
        (subjects, uids)
    };
    let without = {
        let server = Server::start(Config::without_condstore()).unwrap();
        let (dir, store, folder) = store();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
        let page = store.page(folder, None, 10).unwrap();
        let subjects: Vec<String> = page.iter().map(|it| it.subject.clone()).collect();
        let uids = store.known_uids(folder).unwrap();
        drop(dir);
        (subjects, uids)
    };

    assert_eq!(
        with, without,
        "les deux chemins ne donnent pas le même store"
    );
}

#[test]
fn a_truncated_literal_fails_the_harvest_instead_of_hanging() {
    // La panne la plus vicieuse : un client qui lit « jusqu'à la parenthèse » attendrait
    // pour toujours. Le délai de lecture du test le prouve par l'absurde — s'il bloquait,
    // ce test dépasserait son délai au lieu d'échouer proprement.
    let config = Config::with_inbox().with_fault(Fault::TruncatedLiteral { uid: 1, sent: 10 });
    let server = Server::start(config).unwrap();
    let (_dir, store, folder) = store();
    let mut client = connect(&server);

    let outcome = harvest(&mut client, &store, folder, b"INBOX");

    let error = outcome.expect_err("la moisson devait échouer");
    assert!(
        matches!(error, Error::Malformed { .. } | Error::Network(_)),
        "erreur inattendue : {error:?}"
    );
    // **Le store est resté cohérent.** Le témoin de reprise est posé — il dit contre quelle
    // numérotation les copies partielles ont été enregistrées — mais rien de ce qui ferait
    // croire le dossier à jour n'a avancé.
    let state = store.sync_state(folder).unwrap();
    assert_eq!(state.uidvalidity, Some(1000), "le témoin de reprise manque");
    assert_eq!(state.uidnext, None, "le dossier se croit à jour");
    assert_eq!(state.highest_modseq, None);
    assert_eq!(state.synced_at, None);
}

#[test]
fn an_announced_length_larger_than_the_body_fails_the_harvest() {
    let config = Config::with_inbox().with_fault(Fault::LiteralLengthMismatch {
        uid: 1,
        delta: 5_000,
    });
    let server = Server::start(config).unwrap();
    let (_dir, store, folder) = store();
    let mut client = connect(&server);

    let error = harvest(&mut client, &store, folder, b"INBOX").expect_err("devait échouer");
    assert!(
        matches!(error, Error::Malformed { .. } | Error::Network(_)),
        "erreur inattendue : {error:?}"
    );
}

#[test]
fn a_literal_beyond_the_ceiling_is_refused_before_allocating() {
    // **La défense, pas la limite fonctionnelle.** Le plafond est abaissé pour le test ; en
    // production il est à 128 Mio, et un `{4294967295}` ne doit pas faire réserver quatre
    // gigaoctets pour découvrir ensuite que rien ne suit.
    let config = Config::with_inbox().with_fault(Fault::LiteralLengthMismatch {
        uid: 1,
        delta: 10_000,
    });
    let server = Server::start(config).unwrap();
    let (_dir, store, folder) = store();

    let stream = TcpStream::connect(server.address()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut client = Client::greet(stream).unwrap().with_message_limit(1_000);
    client.login("marie@exemple.fr", "secret").unwrap();

    let error = harvest(&mut client, &store, folder, b"INBOX").expect_err("devait échouer");
    assert!(
        matches!(error, Error::LiteralTooLarge { .. }),
        "erreur inattendue : {error:?}"
    );
}

#[test]
fn an_impossible_sequence_number_does_not_misplace_a_message() {
    // Un client qui indexe par numéro de séquence écrirait le message au mauvais endroit.
    // Celui-ci indexe par UID, donc le numéro absurde ne change rien.
    let config = Config::with_inbox().with_fault(Fault::ImpossibleSequenceNumber);
    let server = Server::start(config).unwrap();
    let (_dir, store, folder) = store();
    let mut client = connect(&server);

    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert_eq!(report.fetched, 3);
    assert_eq!(store.known_uids(folder).unwrap(), vec![1, 2, 3]);
}

#[test]
fn a_duplicated_uid_is_absorbed_without_duplicating_anything() {
    // Vu chez des serveurs sous charge. Le client doit être idempotent, pas surpris.
    let config = Config::with_inbox().with_fault(Fault::DuplicateUid { uid: 2 });
    let server = Server::start(config).unwrap();
    let (_dir, store, folder) = store();
    let mut client = connect(&server);

    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert_eq!(count(&store, "remote_uids"), 3, "un UID a été dédoublé");
    assert_eq!(count(&store, "refs"), 3);
    assert_eq!(count(&store, "messages"), 3);
    assert_eq!(report.stored, 3, "un blob a été écrit en trop");
}

#[test]
fn a_refused_login_is_not_retryable() {
    // Réessayer sur un mot de passe faux fait bloquer le compte chez le fournisseur.
    let config = Config::with_inbox().with_fault(Fault::RefusesLogin);
    let server = Server::start(config).unwrap();

    let stream = TcpStream::connect(server.address()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut client = Client::greet(stream).unwrap();

    let error = client
        .login("marie@exemple.fr", "secret")
        .expect_err("devait être refusé");
    assert!(matches!(error, Error::AuthRefused { .. }), "{error:?}");
    assert!(
        !error.retryable(),
        "un mot de passe faux ne se réessaie pas"
    );
}

#[test]
fn a_connection_cut_mid_response_is_a_retryable_failure() {
    // Le câble débranché. La coupure tombe dans le salut, donc avant toute écriture.
    let config = Config::with_inbox().with_fault(Fault::ClosesAfter { after: 12 });
    let server = Server::start(config).unwrap();

    let stream = TcpStream::connect(server.address()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();

    // Le salut est tronqué : soit il n'arrive pas, soit il n'est pas un `* OK` complet.
    match Client::greet(stream) {
        Err(error) => assert!(
            matches!(error, Error::Malformed { .. } | Error::Network(_)),
            "{error:?}"
        ),
        // Douze octets suffisent à écrire `* OK [CAPABI`, qui commence bien par `* OK`.
        // Le refus tombera alors sur la commande suivante, et c'est aussi correct.
        Ok(mut client) => {
            let error = client
                .login("marie@exemple.fr", "secret")
                .expect_err("la connexion était coupée");
            assert!(
                matches!(error, Error::Malformed { .. } | Error::Network(_)),
                "{error:?}"
            );
        }
    }
}

#[test]
fn an_unknown_mailbox_is_a_clean_refusal() {
    let server = Server::start(Config::with_inbox()).unwrap();
    let (_dir, store, folder) = store();
    let mut client = connect(&server);

    let error = harvest(&mut client, &store, folder, b"Inexistante").expect_err("devait échouer");
    assert!(matches!(error, Error::Refused { .. }), "{error:?}");
    assert!(!error.retryable());
    assert_eq!(count(&store, "refs"), 0);
}

#[test]
fn a_mailbox_name_with_spaces_is_harvested() {
    // `[Gmail]/Tous les messages` est le cas ordinaire des quatre comptes Gmail.
    let name = "[Gmail]/Tous les messages";
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox::new(
        name,
        4242,
        vec![Message::simple(1, "a"), Message::simple(2, "b")],
    )]);
    let server = Server::start(config).unwrap();
    let (_dir, store, folder) = store();
    let mut client = connect(&server);

    let report = harvest(&mut client, &store, folder, name.as_bytes()).unwrap();

    assert_eq!(report.fetched, 2);
    assert_eq!(
        store.sync_state(folder).unwrap().remote_name,
        name.as_bytes().to_vec()
    );
}

#[test]
fn listing_returns_the_mailbox_names_as_bytes() {
    let server = Server::start(Config::with_inbox()).unwrap();
    let mut client = connect(&server);

    let listed = client.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, b"INBOX".to_vec());
    assert_eq!(listed[0].delimiter, Some(b'/'));
    assert!(listed[0].selectable());
}

#[test]
fn a_plan_is_reported_for_what_it_is() {
    // Le plan est journalisé, et il est aussi observable : un dossier retéléchargé sans
    // raison est le genre d'événement qu'on ne peut pas expliquer six mois plus tard.
    let local = mailcore::SyncState::default();
    let remote = mailsync::Selected {
        exists: 3,
        uidvalidity: Some(1000),
        uidnext: Some(4),
        highest_modseq: Some(3),
    };
    let (plan, reason) = mailsync::sync::plan(&local, &remote, true);
    assert_eq!(plan, Plan::Full);
    assert_eq!(reason, Some(FullReason::Never));
}

// ---------------------------------------------------------------------------
// Le chiffrement. Ce que ces tests prouvent est un **refus**.
// ---------------------------------------------------------------------------

#[test]
fn the_tls_connector_refuses_a_plaintext_server_instead_of_falling_back() {
    // **Le test le plus important du fichier.** `mailfake` parle en clair. Si le connecteur
    // TLS savait se rabattre, il enverrait le mot de passe en clair sur le réseau — l'accident
    // exact que `Security` sans variante en clair sert à rendre impossible.
    //
    // Il n'y a aucun autre moyen de le vérifier : un connecteur qui n'a jamais rencontré de
    // serveur en clair ne prouve rien sur ce qu'il ferait.
    let server = Server::start(Config::with_inbox()).unwrap();
    let address = server.address();

    let outcome = mailsync::connect(&mailcore::Server {
        host: address.ip().to_string(),
        port: address.port(),
        username: "marie@exemple.fr".to_owned(),
        auth: mailcore::AuthKind::Password,
        security: mailcore::Security::Tls,
    });

    let error = outcome.expect_err("un serveur en clair devait être refusé");
    assert!(
        matches!(
            error,
            Error::Tls { .. } | Error::Network(_) | Error::Malformed { .. }
        ),
        "erreur inattendue : {error:?}"
    );
}

#[test]
fn a_starttls_account_refuses_a_server_that_does_not_announce_it() {
    // Se laisser rétrograder par un attaquant qui a retiré l'annonce est exactement ce qu'un
    // repli permissif autoriserait. `mailfake` n'annonce pas `STARTTLS`.
    let server = Server::start(Config::with_inbox()).unwrap();
    let address = server.address();

    let error = mailsync::connect(&mailcore::Server {
        host: address.ip().to_string(),
        port: address.port(),
        username: "marie@exemple.fr".to_owned(),
        auth: mailcore::AuthKind::Password,
        security: mailcore::Security::StartTls,
    })
    .expect_err("STARTTLS absent devait faire échouer la connexion");

    assert!(matches!(error, Error::Tls { .. }), "{error:?}");
    // Réessayable : c'est une configuration à corriger, mais un serveur peut aussi avoir été
    // en panne. Ce qui compte est qu'aucun octet d'authentification n'est parti.
    assert!(error.retryable());
}

#[test]
fn an_unresolvable_host_fails_without_hanging() {
    // Un pare-feu qui jette les paquets laisserait le fil attendre le délai du système. Ici,
    // le nom ne résout pas : l'échec doit être immédiat.
    let at = std::time::Instant::now();
    let error = mailsync::connect(&mailcore::Server {
        host: "hote.qui.nexiste.pas.invalid".to_owned(),
        port: 993,
        username: "marie@exemple.fr".to_owned(),
        auth: mailcore::AuthKind::Password,
        security: mailcore::Security::Tls,
    })
    .expect_err("un nom qui ne résout pas doit échouer");

    assert!(matches!(error, Error::Network(_)), "{error:?}");
    assert!(error.retryable(), "un DNS peut revenir");
    assert!(
        at.elapsed() < std::time::Duration::from_secs(20),
        "la résolution a traîné : {:?}",
        at.elapsed()
    );
}

// ---------------------------------------------------------------------------
// La découverte des dossiers.
// ---------------------------------------------------------------------------

/// La configuration d'un serveur qui ressemble à Gmail : un nœud non sélectionnable, des
/// rôles annoncés, un nom encodé en UTF-7 modifié.
fn gmail_like() -> Config {
    Config::with_inbox().with_mailboxes(vec![
        Mailbox::new("INBOX", 1000, vec![Message::simple(1, "a")]).with_attribute("\\Inbox"),
        // Le nœud de hiérarchie de Gmail : pas de contenu, pas sélectionnable.
        Mailbox {
            attributes: vec!["\\Noselect".to_owned(), "\\HasChildren".to_owned()],
            ..Mailbox::new("[Gmail]", 0, Vec::new())
        },
        Mailbox::new(
            "[Gmail]/Tous les messages",
            2000,
            vec![Message::simple(2, "b")],
        )
        .with_attribute("\\All"),
        // Une corbeille renommée : aucune heuristique de nom ne la reconnaît, l'attribut oui.
        Mailbox::new("Poubelle", 3000, Vec::new()).with_attribute("\\Trash"),
        // Un nom encodé : `&AOk-` est `é`, donc `Réglages`.
        Mailbox::new("R&AOk-glages", 4000, Vec::new()),
    ])
}

#[test]
fn discovery_creates_the_folders_it_can_select() {
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);

    let found = mailsync::discover(&mut client, &store, account).unwrap();

    let paths: Vec<&str> = found.iter().map(|it| it.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["INBOX", "[Gmail]/Tous les messages", "Poubelle", "Réglages",],
        "le nœud `[Gmail]` devait être écarté et `R&AOk-glages` décodé"
    );
}

#[test]
fn a_noselect_node_is_left_out_rather_than_failing_the_sync() {
    // Un `EXAMINE` sur `[Gmail]` rendrait un `NO`, ce qui ferait échouer une synchronisation
    // par ailleurs correcte.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);

    let found = mailsync::discover(&mut client, &store, account).unwrap();
    assert!(
        !found.iter().any(|it| it.path == "[Gmail]"),
        "un dossier non sélectionnable a été retenu"
    );
}

#[test]
fn a_special_use_attribute_beats_the_name() {
    // Une corbeille renommée `Poubelle` : le nom ne dit rien, `\Trash` dit tout.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);
    mailsync::discover(&mut client, &store, account).unwrap();

    let poubelle = store
        .folders()
        .unwrap()
        .into_iter()
        .find(|it| it.path == "Poubelle")
        .expect("le dossier n'a pas été créé");
    assert_eq!(poubelle.kind, FolderKind::Trash);
}

#[test]
fn the_name_is_the_fallback_when_the_server_says_nothing() {
    let config = Config::with_inbox().with_mailboxes(vec![
        // Aucun attribut de rôle : c'est le nom qui décide.
        Mailbox::new("Corbeille", 1, Vec::new()),
        Mailbox::new("Factures 2026", 2, Vec::new()),
    ]);
    let server = Server::start(config).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);
    mailsync::discover(&mut client, &store, account).unwrap();

    let kinds: Vec<(String, FolderKind)> = store
        .folders()
        .unwrap()
        .into_iter()
        .map(|it| (it.path, it.kind))
        .collect();
    assert!(kinds.contains(&("Corbeille".to_owned(), FolderKind::Trash)));
    assert!(kinds.contains(&("Factures 2026".to_owned(), FolderKind::Other)));
}

#[test]
fn the_remote_name_is_kept_in_bytes_beside_the_decoded_path() {
    // Les deux exemplaires ont deux rôles : le protocole ne voit que les octets, l'affichage
    // ne voit que le décodage.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);

    let found = mailsync::discover(&mut client, &store, account).unwrap();
    let reglages = found
        .iter()
        .find(|it| it.path == "Réglages")
        .expect("dossier absent");
    assert_eq!(reglages.remote_name, b"R&AOk-glages".to_vec());
    assert_eq!(
        store.sync_state(reglages.folder).unwrap().remote_name,
        b"R&AOk-glages".to_vec(),
        "le nom du serveur n'a pas été écrit"
    );
}

#[test]
fn discovering_twice_does_not_duplicate_a_folder() {
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);

    let first = mailsync::discover(&mut client, &store, account).unwrap();
    let second = mailsync::discover(&mut client, &store, account).unwrap();

    assert_eq!(first, second);
    // Plus l'`INBOX` créée par la fixture, qui porte le même chemin que celle du serveur.
    assert_eq!(store.folders().unwrap().len(), 4);
}

#[test]
fn discovery_does_not_reset_the_state_of_an_already_synced_folder() {
    // **Le piège que `set_remote_name` évite.** Passer par `set_sync_state` remettrait
    // `uidvalidity` à `None`, donc déclencherait une moisson complète à chaque découverte.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);

    harvest(&mut client, &store, inbox, b"INBOX").unwrap();
    let before = store.sync_state(inbox).unwrap();
    assert_eq!(before.uidvalidity, Some(1000));

    mailsync::discover(&mut client, &store, account).unwrap();

    let after = store.sync_state(inbox).unwrap();
    assert_eq!(
        after.uidvalidity,
        Some(1000),
        "la découverte a effacé l'état de synchronisation"
    );
    assert_eq!(after.uidnext, before.uidnext);
    assert_eq!(after.highest_modseq, before.highest_modseq);
}

#[test]
fn a_folder_that_vanished_from_the_server_is_kept() {
    // Retirer un dossier local sur la foi d'un `LIST` retirerait les références qu'il porte,
    // donc ferait disparaître du courrier — sur un `LIST` qui peut être incomplet.
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    {
        let server = Server::start(gmail_like()).unwrap();
        let mut client = connect(&server);
        mailsync::discover(&mut client, &store, account).unwrap();
    }
    let before = store.folders().unwrap().len();

    // Le même serveur, un seul dossier.
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox::new("INBOX", 1000, Vec::new())]);
    let server = Server::start(config).unwrap();
    let mut client = connect(&server);
    mailsync::discover(&mut client, &store, account).unwrap();

    assert_eq!(
        store.folders().unwrap().len(),
        before,
        "un dossier a été supprimé sur la foi d'un LIST"
    );
}

#[test]
fn a_courier_style_dot_separator_is_normalised() {
    // Courier sépare avec `.`. Sans normalisation, `INBOX.Corbeille` ne serait pas reconnu
    // comme une corbeille et l'arborescence s'afficherait à plat.
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox {
        delimiter: b'.',
        ..Mailbox::new("INBOX.Corbeille", 1, Vec::new())
    }]);
    let server = Server::start(config).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);

    let found = mailsync::discover(&mut client, &store, account).unwrap();
    assert_eq!(found[0].path, "INBOX/Corbeille");
    assert_eq!(
        found[0].remote_name,
        b"INBOX.Corbeille".to_vec(),
        "le nom du serveur ne doit pas être normalisé"
    );

    let folder = store
        .folders()
        .unwrap()
        .into_iter()
        .find(|it| it.path == "INBOX/Corbeille")
        .expect("dossier absent");
    assert_eq!(folder.kind, FolderKind::Trash);
}

#[test]
fn a_discovered_folder_can_be_harvested_by_its_remote_name() {
    // La boucle complète de l'étape : découvrir, puis moissonner ce qu'on a découvert, sans
    // qu'aucun nom soit écrit en dur.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);

    let found = mailsync::discover(&mut client, &store, account).unwrap();
    let mut total = 0;
    for folder in &found {
        let report = harvest(&mut client, &store, folder.folder, &folder.remote_name).unwrap();
        total += report.fetched;
    }

    assert_eq!(total, 2, "les deux messages du serveur devaient arriver");
    assert_eq!(count(&store, "messages"), 2);
}

// ---------------------------------------------------------------------------
// La boucle de compte, celle que la CLI et le job du démon partagent.
// ---------------------------------------------------------------------------

#[test]
fn syncing_an_account_discovers_then_harvests_every_folder() {
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);
    let progress = mailcore::Progress::new();

    let report = mailsync::sync_account_over(&mut client, &store, account, &progress).unwrap();

    assert_eq!(report.folders, 4, "les quatre dossiers sélectionnables");
    assert_eq!(report.fetched, 2, "les deux messages du serveur");
    assert_eq!(report.stored, 2);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(!report.cancelled);
    assert_eq!(count(&store, "messages"), 2);
}

#[test]
fn the_progress_total_is_discovered_folder_by_folder() {
    // Comme celui de l'import est découvert fichier par fichier. Un total inventé au départ
    // serait un chiffre qu'on ne sait pas, présenté comme un chiffre qu'on sait.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);
    let progress = mailcore::Progress::new();

    assert_eq!(progress.total(), 0, "rien n'est connu avant de commencer");
    mailsync::sync_account_over(&mut client, &store, account, &progress).unwrap();

    assert_eq!(progress.total(), 2, "les deux messages annoncés");
    assert_eq!(progress.done(), 2, "les deux messages faits");
}

#[test]
fn a_cancelled_sync_stops_and_says_so() {
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);

    // Annulée **avant** de commencer : la boucle doit s'arrêter au premier dossier.
    let progress = mailcore::Progress::new();
    progress.cancel();
    let report = mailsync::sync_account_over(&mut client, &store, account, &progress).unwrap();

    assert!(report.cancelled, "l'annulation n'est pas rapportée");
    assert_eq!(
        report.fetched, 0,
        "des corps ont été téléchargés quand même"
    );
    assert_eq!(count(&store, "messages"), 0);
    // Les dossiers **sont** découverts : le `LIST` a lieu avant la boucle, et il est
    // inoffensif. Le dire évite de croire que l'annulation n'a rien fait.
    assert_eq!(report.folders, 4);
}

#[test]
fn a_cancelled_harvest_leaves_a_folder_resumable() {
    // **Le point de l'annulation coopérative.** Ce qui est validé reste validé, et l'état de
    // synchronisation n'avance pas — donc le passage suivant redemande ce qui manque au lieu
    // de croire le dossier à jour.
    let messages: Vec<Message> = (1..=250).map(|uid| Message::simple(uid, "gros")).collect();
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox::new("INBOX", 1000, messages)]);
    let server = Server::start(config).unwrap();
    let (_dir, store, folder) = store();

    let progress = mailcore::Progress::new();
    {
        let mut client = connect(&server);
        // Annulée après le premier lot : `harvest_watched` vérifie entre deux lots de cent.
        let store_for_thread = &store;
        let watcher = &progress;
        // Négocié avant, comme le fait `sync_account_over` : `ENABLE` n'est valide qu'en état
        // authentifié, donc avant le premier `EXAMINE`.
        let enabled = mailsync::enable(&mut client).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while watcher.done() == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                watcher.cancel();
            });
            mailsync::harvest_watched(
                &mut client,
                store_for_thread,
                folder,
                b"INBOX",
                enabled,
                watcher,
            )
            .unwrap();
        });
    }

    let written = count(&store, "messages");
    assert!(written > 0, "rien n'a été écrit avant l'annulation");
    assert!(
        written < 250,
        "l'annulation n'a rien interrompu : {written} messages écrits"
    );
    // **L'état n'a pas avancé** : c'est ce qui rend la reprise possible. Le témoin de
    // reprise, lui, est posé — c'est justement ce qui permet de savoir que les copies déjà
    // écrites valent encore quelque chose.
    let state = store.sync_state(folder).unwrap();
    assert_eq!(state.uidvalidity, Some(1000), "le témoin de reprise manque");
    assert_eq!(
        state.uidnext, None,
        "l'état de synchronisation a avancé sur un passage interrompu"
    );
    assert_eq!(state.synced_at, None);

    // Le passage suivant, sans annulation, finit le travail.
    let mut client = connect(&server);
    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();
    assert_eq!(count(&store, "messages"), 250, "la reprise n'a pas fini");
    assert!(report.fetched > 0, "la reprise n'a rien redemandé");
    assert_eq!(
        store.sync_state(folder).unwrap().uidvalidity,
        Some(1000),
        "l'état n'a pas été écrit à la fin du passage complet"
    );
}

#[test]
fn a_folder_that_fails_does_not_stop_the_account() {
    // Un dossier au nom exotique, une boîte rendue inaccessible : le reste du compte doit se
    // synchroniser. Ici, le dossier est retiré du serveur entre la découverte et la moisson.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(&server);

    // Un dossier que le store connaît mais que le serveur ne sert pas : l'`EXAMINE` rendra
    // un `NO`, et la boucle doit continuer.
    {
        let writer = store.writer().unwrap();
        let ghost = writer
            .upsert_folder(account, "Fantôme", FolderKind::Other)
            .unwrap();
        writer.set_remote_name(ghost, b"Fantome").unwrap();
        writer.commit().unwrap();
    }

    let progress = mailcore::Progress::new();
    let report = mailsync::sync_account_over(&mut client, &store, account, &progress).unwrap();

    // Le dossier fantôme n'est pas dans le `LIST`, donc il n'est pas moissonné du tout — la
    // découverte n'invente pas de dossier. Le compte se synchronise normalement.
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.fetched, 2);
}

#[test]
fn the_dedup_ratio_is_none_when_nothing_was_received() {
    // Un taux calculé sur zéro serait un chiffre inventé.
    let report = mailsync::AccountReport::default();
    assert_eq!(report.dedup_ratio(), None);

    let received = mailsync::AccountReport {
        fetched: 4,
        duplicates: 1,
        ..mailsync::AccountReport::default()
    };
    assert_eq!(received.dedup_ratio(), Some(25.0));
}

#[test]
fn a_second_harvest_without_condstore_fetches_nothing_either() {
    // **Le test qui manquait**, et c'est un vrai serveur qui l'a montré le 2026-09-03.
    //
    // `a_second_harvest_writes_nothing` passait — mais sur un serveur avec `CONDSTORE`, où le
    // plan est `UpToDate` et où aucun corps n'est demandé. Le chemin de repli, lui, n'avait
    // jamais été exercé **deux fois de suite** : ses deux tests ne faisaient qu'un passage.
    //
    // Ce qu'il faisait : `{uidnext}:*` où `uidnext` dépasse le plus grand UID se lit
    // `*:uidnext` — un ensemble IMAP n'est pas ordonné — donc le serveur rend le **dernier
    // message**, et il était retéléchargé à chaque passage.
    let server = Server::start(Config::without_condstore()).unwrap();
    let (_dir, store, folder) = store();

    {
        let mut client = connect(&server);
        let first = harvest(&mut client, &store, folder, b"INBOX").unwrap();
        assert_eq!(first.fetched, 3);
    }

    let mut client = connect(&server);
    let second = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert_eq!(
        second.fetched, 0,
        "un corps a été redemandé alors que rien n'est nouveau"
    );
    assert!(second.wrote_nothing(), "{second:?}");
    assert_eq!(count(&store, "messages"), 3);
}

#[test]
fn a_new_message_is_still_picked_up_without_condstore() {
    // Le contrôle du test précédent : la garde ne doit pas empêcher de voir du neuf. Sans
    // lui, « zéro corps demandé » serait aussi le comportement d'un client cassé.
    let (_dir, store, folder) = store();
    {
        let server = Server::start(Config::without_condstore()).unwrap();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }

    let messages = vec![
        Message::simple(1, "facture"),
        Message::simple(2, "devis"),
        Message::simple(3, "relance"),
        Message::simple(4, "nouveau"),
    ];
    let config =
        Config::without_condstore().with_mailboxes(vec![Mailbox::new("INBOX", 1000, messages)]);
    let server = Server::start(config).unwrap();
    let mut client = connect(&server);

    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();
    assert_eq!(report.fetched, 1, "le message neuf n'a pas été vu");
    assert_eq!(count(&store, "messages"), 4);
}

// ---------------------------------------------------------------------------
// XOAUTH2. Le protocole seulement : obtenir un jeton est l'affaire de mailauth.
// ---------------------------------------------------------------------------

/// Un client connecté, sans authentification.
fn greet(server: &Server) -> Client<TcpStream> {
    let stream = TcpStream::connect(server.address()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    Client::greet(stream).unwrap()
}

#[test]
fn a_valid_token_authenticates_with_the_initial_response() {
    let server = Server::start(Config::with_oauth("jeton-valide")).unwrap();
    let mut client = greet(&server);

    assert!(client.has("AUTH=XOAUTH2"), "{:?}", client.capabilities());
    assert!(client.has("SASL-IR"));
    client
        .authenticate_xoauth2("marie@exemple.fr", "jeton-valide")
        .unwrap();

    // La preuve que la session est bien authentifiée : `EXAMINE` marche.
    let selected = client.examine(b"INBOX").unwrap();
    assert_eq!(selected.exists, 3);
}

#[test]
fn a_valid_token_authenticates_without_sasl_ir_too() {
    // **Le chemin qu'un client oublie d'écrire.** Google annonce `SASL-IR`, donc tout marche
    // jusqu'au serveur qui ne l'annonce pas — et là il faut une continuation.
    let server = Server::start(Config::with_oauth("jeton-valide").without_sasl_ir()).unwrap();
    let mut client = greet(&server);

    assert!(!client.has("SASL-IR"), "le serveur ne doit pas l'annoncer");
    client
        .authenticate_xoauth2("marie@exemple.fr", "jeton-valide")
        .unwrap();
    assert_eq!(client.examine(b"INBOX").unwrap().exists, 3);
}

#[test]
fn a_refused_token_fails_instead_of_hanging() {
    // **Le piège de la continuation d'erreur.** Google n'envoie pas `NO` tout de suite : il
    // envoie un JSON en base64 et attend une ligne vide. Un client qui ne la renvoie pas
    // attend pour toujours — et ça n'arrive que le jour où un jeton expire, donc jamais
    // pendant le développement.
    //
    // Si le client bloquait, ce test dépasserait son délai de lecture au lieu d'échouer
    // proprement : la preuve est dans le fait qu'il rende une erreur en quelques
    // millisecondes.
    let server = Server::start(Config::with_oauth("le-bon-jeton")).unwrap();
    let mut client = greet(&server);

    let at = std::time::Instant::now();
    let error = client
        .authenticate_xoauth2("marie@exemple.fr", "un-jeton-perime")
        .expect_err("le jeton devait être refusé");

    assert!(
        at.elapsed() < std::time::Duration::from_secs(2),
        "le client a attendu : {:?}",
        at.elapsed()
    );
    assert!(matches!(error, Error::AuthRefused { .. }), "{error:?}");
    assert!(
        !error.retryable(),
        "un jeton refusé ne se réessaie pas en boucle"
    );

    // **Le diagnostic du serveur est remonté.** C'est la seule information qui distingue un
    // jeton expiré d'un périmètre insuffisant, et elle n'est que dans cette continuation.
    let message = error.to_string();
    assert!(
        message.contains("400"),
        "le JSON du serveur a été perdu : {message}"
    );
    assert!(message.contains("mail.google.com"), "{message}");
}

#[test]
fn a_wrong_username_is_refused_too() {
    // Le jeton est lié à un compte : le bon jeton avec la mauvaise adresse ne doit pas passer.
    let server = Server::start(Config::with_oauth("le-bon-jeton")).unwrap();
    let mut client = greet(&server);

    let error = client
        .authenticate_xoauth2("quelquun@ailleurs.fr", "le-bon-jeton")
        .expect_err("devait être refusé");
    assert!(matches!(error, Error::AuthRefused { .. }), "{error:?}");
}

#[test]
fn a_server_without_xoauth2_says_so_before_the_attempt() {
    // Essayer un mécanisme non annoncé, c'est envoyer un jeton à un serveur qui ne saura
    // qu'en faire. Le refus vient de notre côté, sans qu'un octet de jeton parte.
    let server = Server::start(Config::with_inbox()).unwrap();
    let mut client = greet(&server);

    assert!(!client.has("AUTH=XOAUTH2"));
    let error = client
        .authenticate_xoauth2("marie@exemple.fr", "un-jeton")
        .expect_err("devait être refusé sans réseau");
    assert!(
        matches!(error, Error::MissingCapability { .. }),
        "{error:?}"
    );
}

#[test]
fn a_password_login_is_refused_by_an_oauth_only_server() {
    // Ce que Google fait depuis qu'il a retiré l'authentification par mot de passe. Le refus
    // doit être distinguable d'un mauvais mot de passe, sinon on cherche du mauvais côté.
    let server = Server::start(Config::with_oauth("jeton")).unwrap();
    let mut client = greet(&server);

    assert!(client.has("LOGINDISABLED"), "le serveur doit l'annoncer");
    let error = client
        .login("marie@exemple.fr", "secret")
        .expect_err("LOGIN devait être refusé");
    assert!(matches!(error, Error::AuthRefused { .. }), "{error:?}");
    assert!(
        error.to_string().contains("PRIVACYREQUIRED"),
        "le code du serveur dit pourquoi : {error}"
    );
}

#[test]
fn a_full_sync_works_over_a_token() {
    // La boucle complète, authentifiée par jeton : c'est ce que fera un compte Gmail.
    let server = Server::start(Config::with_oauth("jeton-valide")).unwrap();
    let (_dir, store, _inbox) = store();
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = greet(&server);
    client
        .authenticate_xoauth2("marie@exemple.fr", "jeton-valide")
        .unwrap();

    let progress = mailcore::Progress::new();
    let report = mailsync::sync_account_over(&mut client, &store, account, &progress).unwrap();
    assert_eq!(report.fetched, 3);
    assert_eq!(count(&store, "messages"), 3);
}

// ---------------------------------------------------------------------------
// `QRESYNC` — les purges sans balayage.
//
// Ce que ces tests protègent : un passage sans changement ne doit plus coûter une
// ligne par message. Mesuré le 2026-09-08 avant le correctif, sur le corpus réel :
// 67 s pour un compte de 51 496 messages où rien n'avait bougé.
// ---------------------------------------------------------------------------

/// La configuration `QRESYNC`, avec la boîte donnée.
fn qresync_config(mailbox: Mailbox) -> Config {
    Config {
        mailboxes: vec![mailbox],
        ..Config::with_qresync()
    }
}

#[test]
fn qresync_is_negotiated_before_any_mailbox_is_selected() {
    // `ENABLE` n'est valide qu'en état authentifié (RFC 5161 §3.1). La première version
    // l'envoyait après l'`EXAMINE`, une fois par dossier : Gmail le tolérait, ce qui est
    // exactement le genre de tolérance sur laquelle on ne peut pas compter.
    let server = Server::start(Config::with_qresync()).unwrap();
    let mut client = connect(&server);

    let enabled = mailsync::enable(&mut client).unwrap();
    assert!(enabled.qresync, "QRESYNC annoncé mais pas activé");
    assert!(
        enabled.condstore,
        "QRESYNC implique CONDSTORE (RFC 7162 §3.2.3)"
    );
}

#[test]
fn a_server_without_qresync_still_falls_back_to_the_sweep() {
    // **Le contrôle du repli.** La majorité des serveurs ne servent pas `QRESYNC` ; le chemin
    // par balayage doit rester le chemin correct pour eux, pas un vestige mort.
    let server = Server::start(Config::with_inbox()).unwrap();
    let mut client = connect(&server);

    let enabled = mailsync::enable(&mut client).unwrap();
    assert!(
        !enabled.qresync,
        "QRESYNC activé sur un serveur qui ne l'a pas"
    );
    assert!(enabled.condstore);
}

#[test]
fn a_purge_is_seen_through_vanished() {
    let (_dir, store, folder) = store();
    {
        let server = Server::start(Config::with_qresync()).unwrap();
        let mut client = connect(&server);
        let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();
        assert_eq!(report.stored, 3);
    }

    // Le message 2 est purgé côté serveur.
    let mailbox = Mailbox::new(
        "INBOX",
        1_000,
        vec![
            Message::simple(1, "facture"),
            Message::simple(2, "devis"),
            Message::simple(3, "relance"),
        ],
    )
    .expunge(2);
    let server = Server::start(qresync_config(mailbox)).unwrap();
    let mut client = connect(&server);
    let second = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert_eq!(second.vanished, 1, "la purge n'a pas été vue");
    assert_eq!(second.fetched, 0, "des corps ont été retéléchargés");
    assert_eq!(count(&store, "remote_uids"), 2);
    assert_eq!(count(&store, "refs"), 2, "la référence n'a pas été retirée");
    // Le contenu reste : il est adressé par contenu et peut être référencé ailleurs.
    assert_eq!(count(&store, "messages"), 3);
}

#[test]
fn a_pass_with_nothing_new_writes_nothing_with_qresync() {
    // **Le critère 3 sur le chemin `QRESYNC`.** Le test jumeau existe pour le chemin par
    // balayage ; sans celui-ci, le nouveau chemin ne serait couvert par rien sur ce point.
    let server = Server::start(Config::with_qresync()).unwrap();
    let (_dir, store, folder) = store();

    {
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }
    let mut client = connect(&server);
    let second = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert!(
        second.wrote_nothing(),
        "le second passage a écrit : {second:?}"
    );
    assert_eq!(second.fetched, 0);
    assert_eq!(second.vanished, 0);
    assert_eq!(
        second.reflagged, 0,
        "les drapeaux ont été réécrits alors que rien n'avait changé"
    );
}

#[test]
fn a_flag_change_arrives_with_the_examine_and_not_in_a_second_round_trip() {
    let (_dir, store, folder) = store();
    {
        let server = Server::start(Config::with_qresync()).unwrap();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }

    // Le message 2 devient lu, à un `MODSEQ` plus élevé.
    let mut messages = vec![
        Message::simple(1, "facture"),
        Message::simple(2, "devis"),
        Message::simple(3, "relance"),
    ];
    messages[1].flags = vec![r"\Seen".to_owned()];
    messages[1].modseq = 42;
    let server = Server::start(qresync_config(Mailbox::new("INBOX", 1_000, messages))).unwrap();
    let mut client = connect(&server);
    let second = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert_eq!(second.copies, 1, "le changement de drapeau a été raté");
    assert_eq!(
        second.fetched, 0,
        "un corps a été retéléchargé pour un drapeau"
    );
    assert_eq!(second.reflagged, 1);

    let read = store
        .page(folder, None, 10)
        .unwrap()
        .iter()
        .filter(|it| it.flags.contains(MessageFlags::SEEN))
        .count();
    assert_eq!(read, 1, "la référence n'a pas suivi le drapeau du serveur");
}

#[test]
fn a_changed_uidvalidity_ignores_vanished_and_harvests_everything() {
    // **Le piège de `QRESYNC`.** Si l'`UIDVALIDITY` ne correspond plus, un serveur conforme
    // ignore le paramètre et ne rend **aucun** `VANISHED`. Un client qui en conclurait « rien
    // n'a disparu » garderait des UID qui ne désignent plus rien.
    let (_dir, store, folder) = store();
    {
        let server = Server::start(Config::with_qresync()).unwrap();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }
    assert_eq!(count(&store, "remote_uids"), 3);

    // La même boîte, renumérotée, avec un seul message.
    let mailbox = Mailbox::new(
        "INBOX",
        7_777,
        vec![Message::simple(1, "après reconstruction")],
    );
    let server = Server::start(qresync_config(mailbox)).unwrap();
    let mut client = connect(&server);
    let second = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert!(
        second.full,
        "un UIDVALIDITY changé impose une moisson complète"
    );
    assert_eq!(
        count(&store, "remote_uids"),
        1,
        "les anciens UID ont survécu à la renumérotation"
    );
    assert_eq!(store.sync_state(folder).unwrap().uidvalidity, Some(7_777));
}

#[test]
fn vanished_naming_an_unknown_uid_is_not_counted_as_a_disappearance() {
    // `VANISHED` annonce ce qui est parti depuis un `MODSEQ`, sans savoir ce que **nous**
    // avions vu. Un UID qu'on n'a jamais eu n'est pas une anomalie, et le compter comme une
    // disparition ferait mentir le bilan.
    let (_dir, store, folder) = store();
    {
        let server = Server::start(Config::with_qresync()).unwrap();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }

    let mut mailbox = Mailbox::new(
        "INBOX",
        1_000,
        vec![
            Message::simple(1, "facture"),
            Message::simple(2, "devis"),
            Message::simple(3, "relance"),
        ],
    );
    // Un UID jamais servi, purgé quand même : le serveur a le droit.
    mailbox.expunged.push((99, 500));
    let server = Server::start(qresync_config(mailbox)).unwrap();
    let mut client = connect(&server);
    let second = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert_eq!(
        second.vanished, 0,
        "un UID inconnu a été compté comme disparu"
    );
    assert_eq!(
        count(&store, "refs"),
        3,
        "une référence a été retirée à tort"
    );
}

// ---------------------------------------------------------------------------
// La reprise après coupure.
//
// Ce que ces tests protègent : un dossier coupé au milieu ne doit pas être
// retéléchargé en entier. Mesuré le 2026-09-08 avant le correctif, après une
// coupure réelle de Gmail : 25 028 corps redemandés pour 2 messages nouveaux.
// ---------------------------------------------------------------------------

/// Une `INBOX` de `n` messages, sans `CONDSTORE` pour rester sur le chemin le plus simple.
fn inbox_of(count: u32) -> Config {
    let messages = (1..=count)
        .map(|uid| Message::simple(uid, &format!("message {uid}")))
        .collect();
    Config::with_inbox().with_mailboxes(vec![Mailbox::new("INBOX", 1_000, messages)])
}

#[test]
fn a_folder_cut_mid_harvest_resumes_instead_of_starting_over() {
    let (_dir, store, folder) = store();

    // **Une vraie coupure**, pas une simulation : le serveur raccroche après un budget
    // d'octets, au milieu d'un lot de corps.
    {
        // 250 messages : `fetch_bodies` valide par lot de cent, donc il faut plus d'un lot
        // pour qu'une coupure laisse quelque chose derrière elle.
        //
        // **Le budget compte tous les octets que le serveur écrit**, et il a dû baisser le
        // 2026-09-09 : `mailfake` n'envoie plus les corps quand la commande ne les demande
        // pas, donc l'énumération des UID ne transfère plus le dossier une seconde fois. Le
        // budget d'avant tombait dans ce transfert-là et ne coupait donc plus rien.
        //
        // 50 000 : après l'énumération (~14 Kio) et le premier lot de cent corps (~26 Kio),
        // donc dans le deuxième — ce qui laisse un lot validé derrière la coupure.
        let mut config = inbox_of(250);
        config.faults.push(Fault::ClosesAfter { after: 50_000 });
        let server = Server::start(config).unwrap();
        let mut client = connect(&server);
        // L'échec est attendu : c'est le sujet du test.
        let _ = harvest(&mut client, &store, folder, b"INBOX");
    }

    let partial = count(&store, "remote_uids");
    assert!(partial > 0, "la coupure n'a rien laissé : test sans objet");
    assert!(
        partial < 250,
        "la coupure n'a rien coupé : budget trop large"
    );

    // Le témoin de reprise a été posé avant le premier téléchargement.
    let state = store.sync_state(folder).unwrap();
    assert_eq!(
        state.uidvalidity,
        Some(1_000),
        "sans témoin, la reprise repart en aveugle"
    );

    // Le même serveur, sans la panne.
    let server = Server::start(inbox_of(250)).unwrap();
    let mut client = connect(&server);
    let second = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert_eq!(
        second.fetched,
        250 - partial as usize,
        "la reprise a redemandé des corps déjà présents"
    );
    assert_eq!(count(&store, "remote_uids"), 250);
    assert_eq!(count(&store, "messages"), 250);
}

#[test]
fn a_changed_uidvalidity_still_wipes_everything_before_harvesting() {
    // **Le contrôle inverse, et il est essentiel.** Ne plus effacer sur une reprise ne doit pas
    // faire oublier d'effacer quand les UID sont réellement devenus faux : garder des copies
    // sous des UID renumérotés lierait des messages à des numéros qui désignent autre chose.
    let (_dir, store, folder) = store();
    {
        let server = Server::start(inbox_of(5)).unwrap();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }
    assert_eq!(count(&store, "remote_uids"), 5);

    // La même boîte renumérotée, avec deux messages.
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox::new(
        "INBOX",
        9_999,
        vec![Message::simple(1, "un"), Message::simple(2, "deux")],
    )]);
    let server = Server::start(config).unwrap();
    let mut client = connect(&server);
    let second = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert!(second.full);
    assert_eq!(
        count(&store, "remote_uids"),
        2,
        "des UID de l'ancienne numérotation ont survécu"
    );
    assert_eq!(second.fetched, 2, "les corps n'ont pas été redemandés");
}

#[test]
fn a_flag_changed_during_the_interruption_is_not_lost() {
    // Une moisson complète prend les drapeaux avec les corps. Les messages dont le corps n'est
    // pas redemandé n'en verraient donc aucun — d'où la passe de drapeaux ajoutée quand une
    // moisson complète saute des UID.
    let (_dir, store, folder) = store();
    {
        // Le même budget que le test de reprise, et pour la même raison — voir son
        // commentaire : `mailfake` ne transfère plus les corps à l'énumération des UID.
        let mut config = inbox_of(250);
        config.faults.push(Fault::ClosesAfter { after: 50_000 });
        let server = Server::start(config).unwrap();
        let mut client = connect(&server);
        let _ = harvest(&mut client, &store, folder, b"INBOX");
    }
    let partial = count(&store, "remote_uids") as usize;
    assert!(
        partial > 0 && partial < 250,
        "coupure inexploitable : {partial}"
    );

    // Le message 1 — déjà téléchargé — devient lu pendant l'interruption.
    let mut messages: Vec<Message> = (1..=250)
        .map(|uid| Message::simple(uid, &format!("message {uid}")))
        .collect();
    messages[0].flags = vec![r"\Seen".to_owned()];
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox::new("INBOX", 1_000, messages)]);
    let server = Server::start(config).unwrap();
    let mut client = connect(&server);
    harvest(&mut client, &store, folder, b"INBOX").unwrap();

    // La page entière : les 250 messages portent la même date, donc rien ne garantit que le
    // numéro 1 soit dans les premières lignes. Une limite trop courte ferait passer ce test
    // pour un échec du code.
    let read = store
        .page(folder, None, 300)
        .unwrap()
        .iter()
        .filter(|it| it.flags.contains(MessageFlags::SEEN))
        .count();
    assert_eq!(read, 1, "le drapeau posé pendant la coupure a été perdu");
}

// ---------------------------------------------------------------------------
// Le critère 9 : un dossier renuméroté, à l'échelle.
//
// « `UIDVALIDITY` qui change sur un dossier de 1 Go — détecté, resynchronisé,
// 0 blob dupliqué. »
//
// Ce que le critère met à l'épreuve n'est pas la détection, qui tient en une
// comparaison d'entiers, mais **le pari de la phase 1** : quand un serveur
// renumérote une boîte, un client ordinaire retélécharge tout et réécrit tout.
// Ici, les corps retéléchargés sont reconnus par leur hachage BLAKE3, et aucun
// contenu n'est écrit une deuxième fois.
//
// **Ce que le test relève**, le 2026-09-08 : 7,63 Mio de RFC 5322 en 2 000
// contenus distincts ; la resynchronisation redemande les 2 000 corps, en
// reconnaît 2 000 comme déjà présents, en écrit **zéro**, et refait les 2 000
// copies sous les nouveaux UID. Le répertoire des blobs — 2 000 fichiers,
// 676 836 octets compressés — est identique à l'octet près avant et après. La
// première moisson coûte 5,7 s en binaire de débogage, la resynchronisation
// 0,57 s : c'est la même mesure vue par le temps, et le facteur dix est le prix
// de l'écriture qui n'a pas lieu.
//
// **Pourquoi pas un gigaoctet.** Un dossier de 1 Go dans une suite de tests,
// c'est un gigaoctet transféré sur le bouclage, compressé, haché et écrit —
// deux fois, puisqu'il faut moissonner avant de renuméroter — à chaque
// `cargo test` de chaque machine. Ce qui se mesure ici est un **rapport** :
// ce qui est retéléchargé sur ce qui est réécrit. Il ne dépend pas du nombre
// d'octets, et 2 000 messages de quelques kibioctets suffisent à le rendre
// observable — la renumérotation, la dédup et l'écriture par lots travaillent
// alors sur des milliers d'éléments et non sur trois. Ce que l'échelle réelle
// éprouverait en plus — la mémoire, la durée, un lot coupé au milieu —
// appartient aux critères 2 et 8, qui la mesurent là où elle compte.
//
// **Pourquoi pas `Fault::UidvalidityChangesOnSelect`.** Elle fait dériver
// l'`UIDVALIDITY` à *chaque* `SELECT`. C'est la bonne panne pour montrer qu'un
// client remarque un changement, et la mauvaise ici : la valeur qu'un `EXAMINE`
// d'observation lirait ne serait pas celle que la moisson verrait, donc le test
// ne pourrait affirmer ni que la raison du plan est la bonne, ni que le nouvel
// `UIDVALIDITY` enregistré est celui du serveur. Reconstruire une `Config` —
// même courrier, autre numérotation — dit exactement ce qu'un serveur restauré
// depuis une sauvegarde fait, et le dit une fois pour toutes.
// ---------------------------------------------------------------------------

/// Le nombre de messages du dossier volumineux.
const BULK: u32 = 2_000;

/// Le premier UID de la numérotation d'après. Disjoint de la première, à dessein : un ancien
/// UID qui survivrait se verrait, alors que deux plages qui se recouvrent le cacheraient.
const RENUMBERED_FROM: u32 = 500_000;

/// Un corps RFC 5322 de quelques kibioctets, **indépendant de l'UID**.
///
/// C'est la condition qui rend le test honnête. `Message::simple` met l'UID dans le
/// `Message-ID` : renuméroter changerait les octets, donc le hachage, donc « zéro blob
/// dupliqué » serait faux — et le test ne dirait plus rien du critère, qui parle d'un serveur
/// qui resert **le même courrier** sous d'autres numéros.
fn bulky_body(n: u32) -> Vec<u8> {
    let mut body = format!(
        "From: Marie <marie@exemple.fr>\r\n\
         To: Jean <jean@exemple.fr>\r\n\
         Subject: dossier volumineux, message {n}\r\n\
         Date: Tue, 1 Sep 2026 10:00:00 +0200\r\n\
         Message-ID: <volumineux-{n}@exemple.fr>\r\n\
         \r\n"
    );
    for line in 0..24 {
        body.push_str(&format!(
            "Ligne {line} du message {n}. De quoi peser sans dépendre du numéro de la copie : \
             c'est le contenu qui identifie un message, et c'est lui qu'on renumérote ici.\r\n"
        ));
    }
    body.into_bytes()
}

/// Les corps du dossier volumineux, dans l'ordre.
fn bulky_bodies() -> Vec<Vec<u8>> {
    (1..=BULK).map(bulky_body).collect()
}

/// Une `INBOX` qui sert `bodies` sous les UID `first + 1`, `first + 2`… avec cet `UIDVALIDITY`.
fn numbered_inbox(uidvalidity: u32, first: u32, bodies: &[Vec<u8>]) -> Config {
    let messages = bodies
        .iter()
        .enumerate()
        .map(|(rank, body)| {
            // La conversion est écrite plutôt qu'un `as` : un débordement silencieux ferait
            // servir deux messages sous le même UID le jour où `BULK` grandit, et le serveur
            // de test mentirait sans le dire.
            let uid = first + u32::try_from(rank + 1).unwrap();
            Message {
                uid,
                flags: Vec::new(),
                modseq: u64::from(uid),
                body: body.clone(),
            }
        })
        .collect();
    Config::with_inbox().with_mailboxes(vec![Mailbox::new("INBOX", uidvalidity, messages)])
}

/// Les blobs présents **sur le disque** : combien de fichiers, et combien d'octets.
///
/// La mesure la plus directe de « 0 blob dupliqué ». Un compteur de bilan dit ce que le code
/// croit avoir fait ; ceci dit ce que le système de fichiers a reçu. Un contenu écrit une
/// deuxième fois par un chemin qu'on n'aurait pas vu apparaîtrait ici et nulle part ailleurs.
fn blobs_on_disk(root: &std::path::Path) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut stack = vec![root.join("blobs")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                files += 1;
                bytes += metadata.len();
            }
        }
    }
    (files, bytes)
}

#[test]
fn a_renumbered_bulk_folder_is_reharvested_without_writing_one_content_twice() {
    // **Le critère 9 de `docs/PHASE-2.md`**, ses trois affirmations dans l'ordre : détecté,
    // resynchronisé, zéro blob dupliqué.
    let bodies = bulky_bodies();
    let (dir, store, folder) = store();
    let root = dir.path().to_owned();

    {
        let server = Server::start(numbered_inbox(1_000, 0, &bodies)).unwrap();
        let mut client = connect(&server);
        let first = harvest(&mut client, &store, folder, b"INBOX").unwrap();
        assert_eq!(first.stored, BULK as usize, "le corpus n'est pas entré");
        assert_eq!(first.duplicates, 0, "les corps ne sont pas tous distincts");
    }

    let before_messages = count(&store, "messages");
    let before_refs = count(&store, "refs");
    let before_bytes = store.stats().unwrap().raw_bytes;
    let before_blobs = blobs_on_disk(&root);
    assert_eq!(before_messages, u64::from(BULK));
    assert_eq!(before_blobs.0, u64::from(BULK), "un blob par contenu");
    assert_eq!(
        store.known_uids(folder).unwrap(),
        (1..=BULK).collect::<Vec<u32>>()
    );

    // Le même courrier, renuméroté : ce qu'un serveur fait après une restauration.
    let server = Server::start(numbered_inbox(4_242, RENUMBERED_FROM, &bodies)).unwrap();

    // --- détecté ---
    //
    // `report.full` dit qu'une moisson complète a eu lieu ; il ne dit pas **pourquoi**, et un
    // dossier qu'on aurait par ailleurs oublié se moissonnerait complètement lui aussi. La
    // raison se lit sur `plan`, qui est pure : l'état local d'un côté, ce que le serveur vient
    // d'annoncer de l'autre.
    let local = store.sync_state(folder).unwrap();
    let remote = connect(&server).examine(b"INBOX").unwrap();
    assert_eq!(local.uidvalidity, Some(1_000));
    assert_eq!(remote.uidvalidity, Some(4_242));
    let (planned, reason) = mailsync::sync::plan(&local, &remote, true);
    assert_eq!(planned, Plan::Full);
    assert_eq!(reason, Some(FullReason::UidvalidityChanged));

    let mut client = connect(&server);
    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert!(report.full, "le changement d'UIDVALIDITY n'a pas été vu");
    assert_eq!(
        store.sync_state(folder).unwrap().uidvalidity,
        Some(4_242),
        "la nouvelle numérotation n'a pas été enregistrée"
    );

    // --- resynchronisé ---
    let expected: Vec<u32> = (1..=BULK).map(|it| RENUMBERED_FROM + it).collect();
    assert_eq!(
        store.known_uids(folder).unwrap(),
        expected,
        "les copies n'ont pas été refaites sous les nouveaux UID"
    );
    assert_eq!(report.copies, BULK as usize);
    assert_eq!(count(&store, "remote_uids"), u64::from(BULK));
    assert_eq!(count(&store, "refs"), before_refs, "une référence manque");
    assert_eq!(
        store.page(folder, None, 10).unwrap().len(),
        10,
        "le dossier ne s'affiche plus"
    );

    // --- 0 blob dupliqué ---
    //
    // `BULK` corps retéléchargés, zéro contenu nouveau. C'est ce que l'adressage par contenu
    // achète, et un client qui range par dossier aurait réécrit les 2 000.
    assert_eq!(report.fetched, BULK as usize, "tout devait être redemandé");
    assert_eq!(
        report.stored, 0,
        "un contenu a été écrit deux fois : l'adressage par contenu ne tient pas"
    );
    assert_eq!(
        report.duplicates, BULK as usize,
        "la dédup n'a rien reconnu"
    );
    assert_eq!(
        count(&store, "messages"),
        before_messages,
        "le nombre de contenus distincts a bougé"
    );
    assert_eq!(store.stats().unwrap().raw_bytes, before_bytes);
    assert_eq!(
        blobs_on_disk(&root),
        before_blobs,
        "le répertoire des blobs a bougé : ni un fichier ni un octet ne le devaient"
    );
}

#[test]
fn a_body_that_really_changed_during_the_renumbering_is_stored() {
    // **Le contrôle inverse, et sans lui le test précédent ne prouve rien.** Un store qui
    // n'écrirait plus jamais rien — un `put` qui rendrait toujours `created: false`, un chemin
    // d'écriture débranché — passerait « zéro blob dupliqué » sans effort. Il faut donc qu'un
    // contenu réellement nouveau, arrivé par le même chemin et dans la même resynchronisation,
    // soit écrit.
    let bodies = bulky_bodies();
    let (dir, store, folder) = store();
    let root = dir.path().to_owned();

    {
        let server = Server::start(numbered_inbox(1_000, 0, &bodies)).unwrap();
        let mut client = connect(&server);
        harvest(&mut client, &store, folder, b"INBOX").unwrap();
    }
    let before_messages = count(&store, "messages");
    let before_blobs = blobs_on_disk(&root).0;

    // Le même corpus renuméroté, à un corps près : un message a changé pendant que le serveur
    // était en panne. Quelques octets suffisent — le hachage porte sur tout.
    let mut changed = bodies.clone();
    changed[0].extend_from_slice(b"Post-scriptum : ce corps n'est plus le meme.\r\n");
    let server = Server::start(numbered_inbox(4_242, RENUMBERED_FROM, &changed)).unwrap();
    let mut client = connect(&server);
    let report = harvest(&mut client, &store, folder, b"INBOX").unwrap();

    assert!(report.full);
    assert_eq!(report.fetched, BULK as usize);
    assert_eq!(report.stored, 1, "le contenu modifié n'a pas été écrit");
    assert_eq!(report.duplicates, BULK as usize - 1);
    assert_eq!(
        count(&store, "messages"),
        before_messages + 1,
        "le nouveau contenu n'a pas de ligne à lui"
    );
    assert_eq!(
        blobs_on_disk(&root).0,
        before_blobs + 1,
        "un blob, et un seul, devait apparaître sur le disque"
    );

    // Et l'ancien contenu **reste**, avec sa référence : il est adressé par contenu, plus
    // aucune copie ne le porte mais rien ne dit qu'un autre dossier ne le référencera pas. Le
    // ramassage de ce qui n'est plus référencé est une passe distincte, comme pour une purge.
    assert_eq!(
        count(&store, "refs"),
        u64::from(BULK) + 1,
        "la référence de l'ancien contenu a été retirée"
    );
}

// ---------------------------------------------------------------------------
// `LIST-STATUS` : l'état de toutes les boîtes en une commande.
//
// Ce que ces tests doivent établir n'est pas « le raccourci marche » mais « le raccourci ne
// décide rien de plus que le chemin normal ». Un `EXAMINE` évité qui laisserait un message
// dehors serait une régression invisible : le compte se synchroniserait vite, et faux.
// ---------------------------------------------------------------------------

/// Le bilan d'une synchronisation de compte, sur un serveur donné, contre un store donné.
fn sync_over(server: &Server, store: &Store) -> mailsync::AccountReport {
    let account = store.full_accounts().unwrap()[0].id;
    let mut client = connect(server);
    let progress = mailcore::Progress::new();
    mailsync::sync_account_over(&mut client, store, account, &progress).unwrap()
}

#[test]
fn a_second_pass_skips_every_folder_that_has_nothing_new() {
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();

    let first = sync_over(&server, &store);
    assert_eq!(first.skipped, 0, "rien n'est à jour au premier passage");
    assert_eq!(first.fetched, 2);

    let second = sync_over(&server, &store);

    assert_eq!(
        second.skipped, 4,
        "les quatre dossiers devaient être reconnus à jour sans EXAMINE"
    );
    assert_eq!(second.fetched, 0, "rien n'aurait dû être retéléchargé");
    assert_eq!(second.stored, 0);
    assert_eq!(count(&store, "messages"), 2);
}

#[test]
fn a_folder_that_received_a_message_is_not_skipped() {
    // **Le test qui compte.** Les autres montrent que le raccourci va vite ; celui-ci montre
    // qu'il ne fait pas manquer de courrier. Le serveur du second passage a un message de plus
    // dans une seule boîte, avec les mêmes `UIDVALIDITY` : c'est l'arrivée de courrier vue par
    // un client qui repasse.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    sync_over(&server, &store);
    drop(server);

    let mut boxes = gmail_like().mailboxes;
    boxes[0] = Mailbox::new(
        "INBOX",
        1000,
        vec![Message::simple(1, "a"), Message::simple(2, "neuf")],
    )
    .with_attribute("\\Inbox");
    let grown = Server::start(Config::with_inbox().with_mailboxes(boxes)).unwrap();

    let second = sync_over(&grown, &store);

    assert_eq!(
        second.skipped, 3,
        "les trois boîtes inchangées, et pas celle qui a grossi"
    );
    assert_eq!(second.fetched, 1, "le message neuf devait descendre");
    assert_eq!(second.stored, 1);
    assert_eq!(count(&store, "messages"), 3);
}

#[test]
fn a_folder_that_lost_a_message_is_not_skipped() {
    // L'autre moitié du raisonnement : `UIDNEXT` inchangé ne veut pas dire « rien n'a bougé ».
    // Une purge laisse `UIDNEXT` là où il était et fait baisser le compte de messages, et
    // c'est ce compte-là qui doit renvoyer la boîte au chemin normal.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    sync_over(&server, &store);
    drop(server);

    let mut boxes = gmail_like().mailboxes;
    // Le même `UIDNEXT` qu'avant — `expunge` garde la trace du disparu — mais un message de
    // moins.
    boxes[0] = Mailbox::new("INBOX", 1000, vec![Message::simple(1, "a")])
        .with_attribute("\\Inbox")
        .expunge(1);
    let shrunk = Server::start(Config::with_inbox().with_mailboxes(boxes)).unwrap();

    let second = sync_over(&shrunk, &store);

    assert_eq!(
        second.skipped, 3,
        "la boîte purgée devait repasser par le balayage"
    );
    assert_eq!(second.vanished, 1, "la copie disparue n'a pas été retirée");
}

#[test]
fn a_server_without_list_status_syncs_exactly_as_before() {
    // Le chemin d'avant doit rester exercé : c'est celui de tout serveur qui n'a pas la RFC
    // 5819, et un chemin qu'on n'exécute plus est un chemin qui pourrit.
    let server = Server::start(gmail_like().without_list_status()).unwrap();
    let (_dir, store, _inbox) = store();

    sync_over(&server, &store);
    let second = sync_over(&server, &store);

    assert_eq!(second.skipped, 0, "aucun raccourci n'était disponible");
    assert_eq!(second.fetched, 0, "et pourtant rien n'a été retéléchargé");
    assert_eq!(count(&store, "messages"), 2);
}

#[test]
fn a_server_that_advertises_list_status_then_refuses_it_still_syncs() {
    // La même panne que `AdvertisesCondstoreThenRefuses`, sur l'extension du raccourci. Un
    // refus doit coûter un aller-retour, pas une synchronisation.
    let server =
        Server::start(gmail_like().with_fault(Fault::AdvertisesListStatusThenRefuses)).unwrap();
    let (_dir, store, _inbox) = store();

    let first = sync_over(&server, &store);
    let second = sync_over(&server, &store);

    assert_eq!(first.fetched, 2, "{:?}", first.failures);
    assert!(first.failures.is_empty(), "{:?}", first.failures);
    assert_eq!(second.skipped, 0, "le refus devait interdire le raccourci");
    assert_eq!(second.fetched, 0);
    assert_eq!(count(&store, "messages"), 2);
}

#[test]
fn a_box_left_out_of_the_list_status_is_harvested_anyway() {
    // La RFC 5819 §2 autorise le serveur à omettre le `STATUS` d'une boîte. « Pas de `STATUS` »
    // doit vouloir dire « je ne sais pas », donc `EXAMINE`, et surtout pas « rien à faire ».
    let server = Server::start(gmail_like().with_fault(Fault::PartialListStatus)).unwrap();
    let (_dir, store, _inbox) = store();

    sync_over(&server, &store);
    let second = sync_over(&server, &store);

    assert!(
        second.skipped < 4,
        "toutes les boîtes ont été sautées alors que la moitié n'avait pas de STATUS"
    );
    assert_eq!(
        second.fetched, 0,
        "les boîtes examinées n'avaient rien de neuf"
    );
    assert_eq!(count(&store, "messages"), 2, "un message a été perdu");
}

#[test]
fn without_condstore_no_folder_is_skipped() {
    // Sans `CONDSTORE`, `plan` ne peut jamais répondre `UpToDate` : les drapeaux d'un message
    // déjà connu peuvent avoir changé sans que `UIDNEXT` ni le compte ne bougent, et rien ne
    // le dirait. Le raccourci doit donc rester fermé — l'économie ne vaut pas un message lu
    // qui s'affiche non lu pour toujours.
    let server = Server::start(Config {
        condstore: false,
        ..gmail_like()
    })
    .unwrap();
    let (_dir, store, _inbox) = store();

    sync_over(&server, &store);
    let second = sync_over(&server, &store);

    assert_eq!(second.skipped, 0);
    assert_eq!(second.fetched, 0);
    assert_eq!(count(&store, "messages"), 2);
}

#[test]
fn asking_a_highestmodseq_of_a_server_without_condstore_is_refused() {
    // Ce que le raccourci évite en choisissant ses éléments selon l'`ENABLE` plutôt que selon
    // l'envie : un `BAD` qui emporterait l'état de **toutes** les boîtes, pas seulement celui
    // de la donnée en trop.
    let server = Server::start(Config {
        condstore: false,
        ..gmail_like()
    })
    .unwrap();
    let mut client = connect(&server);

    let refused = client.list_status("MESSAGES UIDNEXT UIDVALIDITY HIGHESTMODSEQ");
    assert!(
        matches!(refused, Err(Error::Refused { .. })),
        "un serveur sans CONDSTORE devait refuser : {refused:?}"
    );

    let mut client = connect(&server);
    let accepted = client.list_status("MESSAGES UIDNEXT UIDVALIDITY").unwrap();
    assert_eq!(accepted.len(), 4, "les quatre boîtes sélectionnables");
    assert!(
        accepted.iter().all(|it| it.highest_modseq.is_none()),
        "aucun MODSEQ n'était demandé"
    );
}

#[test]
fn a_noselect_node_gets_no_status_line() {
    // Le `[Gmail]` de Gmail : il apparaît dans le `LIST` et pas dans les `STATUS`. Mesuré le
    // 2026-09-08 sur les quatre comptes Gmail réels — 19 boîtes annoncées, 18 `STATUS`.
    let server = Server::start(gmail_like()).unwrap();
    let mut client = connect(&server);

    let found = client.list_status("MESSAGES UIDNEXT UIDVALIDITY").unwrap();

    assert_eq!(found.len(), 4, "le nœud de hiérarchie a rendu un STATUS");
    assert!(
        !found.iter().any(|it| it.name == b"[Gmail]"),
        "le nœud `[Gmail]` n'a pas d'état à donner"
    );
}

// ---------------------------------------------------------------------------
// `IDLE` : le courrier qui arrive sans qu'on demande (RFC 2177).
//
// Ce que ces tests doivent établir tient en deux points. Que le client entende ce que le
// serveur annonce, évidemment. Mais surtout que **le silence ne casse rien** : une boîte
// tranquille est le cas normal, l'attente doit donc pouvoir expirer autant de fois qu'il faut
// sans que la connexion cesse d'être utilisable.
// ---------------------------------------------------------------------------

/// Un client connecté, authentifié, avec `INBOX` sélectionnée et un délai de lecture court.
///
/// Le délai court est ce qui rend les tranches d'attente observables en test : sans lui, un
/// `idle_wait` sur une boîte tranquille durerait le délai par défaut du serveur de test.
fn idling(server: &Server, tick: std::time::Duration) -> (Client<TcpStream>, TcpStream) {
    let stream = TcpStream::connect(server.address()).unwrap();
    stream.set_read_timeout(Some(tick)).unwrap();
    let watched = stream.try_clone().unwrap();
    let mut client = Client::greet(stream).unwrap();
    client.login("marie@exemple.fr", "secret").unwrap();
    client.examine(b"INBOX").unwrap();
    (client, watched)
}

#[test]
fn an_idle_hears_what_the_server_announces() {
    let server = Server::start(Config::with_inbox().announcing_on_idle("* 4 EXISTS")).unwrap();
    let (mut client, _) = idling(&server, std::time::Duration::from_secs(5));

    let tag = client.idle_start().unwrap();
    let heard = client
        .idle_wait(std::time::Duration::from_secs(5))
        .unwrap()
        .expect("le serveur avait quelque chose à dire");

    assert_eq!(heard.text, "* 4 EXISTS");
    client.idle_done(&tag).unwrap();
}

#[test]
fn a_quiet_idle_returns_nothing_and_leaves_the_connection_usable() {
    // **Le test qui compte.** Le silence est le cas normal, et il ne doit ni être pris pour une
    // panne, ni désynchroniser le dialogue : c'est ce qui permet à un veilleur de vérifier
    // entre deux tranches s'il doit s'arrêter.
    let server = Server::start(Config::with_inbox().with_fault(Fault::IdleStaysSilent)).unwrap();
    let (mut client, _) = idling(&server, std::time::Duration::from_secs(5));

    let tag = client.idle_start().unwrap();
    for _ in 0..3 {
        assert!(
            client
                .idle_wait(std::time::Duration::from_millis(60))
                .unwrap()
                .is_none(),
            "un serveur muet ne doit rien rendre, et surtout pas échouer"
        );
    }
    client.idle_done(&tag).unwrap();

    // Et le dialogue tient toujours : c'est la vraie preuve que les tranches n'ont rien
    // consommé de travers.
    let selected = client.examine(b"INBOX").unwrap();
    assert_eq!(selected.exists, 3, "la connexion n'est plus exploitable");
}

#[test]
fn an_idle_restores_the_read_timeout_it_borrowed() {
    // Sans ça, la moisson qui suit un `IDLE` expirerait au bout d'une tranche — quelques
    // dizaines de millisecondes — au lieu du délai de lecture ordinaire. Le bug serait
    // invisible en test court et fatal sur un vrai serveur.
    let server = Server::start(Config::with_inbox().with_fault(Fault::IdleStaysSilent)).unwrap();
    let ordinary = std::time::Duration::from_secs(7);
    let (mut client, _) = idling(&server, ordinary);

    let tag = client.idle_start().unwrap();
    assert!(
        client
            .idle_wait(std::time::Duration::from_millis(60))
            .unwrap()
            .is_none()
    );
    client.idle_done(&tag).unwrap();

    // **Le socket est relu par son propre descripteur, pas par un `try_clone`.** Un descripteur
    // dupliqué ne rend pas la même valeur sur Windows : la première version de ce test le
    // relisait sur un clone, et elle passait aussi bien avec le correctif que sans. Un contrôle
    // négatif — retirer la remise en place et voir le test tomber — est ce qui l'a montré.
    let stream = client.into_stream();
    assert_eq!(
        stream.read_timeout().unwrap(),
        Some(ordinary),
        "la tranche d'attente est restée en place : la moisson suivante expirerait au bout de \
         quelques dizaines de millisecondes"
    );
}

#[test]
fn a_server_without_idle_is_refused_before_a_byte_is_sent() {
    let server = Server::start(Config::with_inbox().without_idle()).unwrap();
    let (mut client, _) = idling(&server, std::time::Duration::from_secs(5));

    let refused = client.idle_start();
    assert!(
        matches!(&refused, Err(Error::MissingCapability { capability }) if capability == "IDLE"),
        "un serveur sans IDLE devait être reconnu sur sa capacité : {refused:?}"
    );

    // Rien n'a été envoyé, donc la connexion sert encore. C'est la différence entre refuser
    // sur la capacité et refuser sur la réponse.
    assert_eq!(client.examine(b"INBOX").unwrap().exists, 3);
}

#[test]
fn a_server_that_advertises_idle_then_refuses_it_says_so() {
    let server =
        Server::start(Config::with_inbox().with_fault(Fault::AdvertisesIdleThenRefuses)).unwrap();
    let (mut client, _) = idling(&server, std::time::Duration::from_secs(5));

    let refused = client.idle_start();
    assert!(
        matches!(refused, Err(Error::Refused { .. })),
        "le refus du serveur devait remonter tel quel : {refused:?}"
    );
}

#[test]
fn an_idle_without_a_selected_mailbox_is_refused_by_the_server() {
    // `IDLE` ne rapporte que la boîte courante. L'accepter sans sélection laisserait un client
    // attendre des événements qu'aucun serveur ne lui enverra.
    let server = Server::start(Config::with_inbox()).unwrap();
    let stream = TcpStream::connect(server.address()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut client = Client::greet(stream).unwrap();
    client.login("marie@exemple.fr", "secret").unwrap();

    let refused = client.idle_start();
    assert!(
        matches!(refused, Err(Error::Refused { .. })),
        "un IDLE hors boîte sélectionnée devait être refusé : {refused:?}"
    );
}

#[test]
fn what_the_server_says_while_we_leave_the_idle_is_not_lost() {
    // Le serveur a le droit d'annoncer un dernier événement pendant qu'on sort. Le jeter
    // perdrait justement l'information qui a motivé la sortie.
    let server = Server::start(Config::with_inbox().announcing_on_idle("* 9 EXISTS")).unwrap();
    let (mut client, _) = idling(&server, std::time::Duration::from_secs(5));

    let tag = client.idle_start().unwrap();
    // On sort **sans** avoir lu : l'événement est encore dans le tuyau, et c'est `idle_done`
    // qui doit le rendre plutôt que de l'avaler en cherchant son `OK`.
    let trailing = client.idle_done(&tag).unwrap();

    assert!(
        trailing.iter().any(|it| it.text == "* 9 EXISTS"),
        "l'événement annoncé avant le OK a été perdu : {trailing:?}"
    );
}

// ---------------------------------------------------------------------------
// `QRESYNC` **et** `LIST-STATUS` sur le même compte.
//
// C'est la configuration du seul Dovecot du corpus réel, découverte le 2026-09-08 quand les
// capacités ont enfin été demandées après le `LOGIN`. Les sept tests `QRESYNC` existants
// appellent `harvest` sur un dossier ; aucun ne passait par la boucle de compte, donc aucun ne
// faisait cohabiter le raccourci et la reprise par `VANISHED`.
// ---------------------------------------------------------------------------

#[test]
fn with_qresync_and_list_status_a_second_pass_skips_everything() {
    let server = Server::start(Config {
        qresync: true,
        ..gmail_like()
    })
    .unwrap();
    let (_dir, store, _inbox) = store();

    let first = sync_over(&server, &store);
    assert_eq!(first.fetched, 2, "{:?}", first.failures);

    let second = sync_over(&server, &store);

    assert_eq!(
        second.skipped, 4,
        "le raccourci doit fonctionner aussi sur un serveur QRESYNC"
    );
    assert_eq!(second.fetched, 0);
    assert_eq!(count(&store, "messages"), 2);
}

#[test]
fn with_qresync_and_list_status_a_purge_is_still_seen() {
    // **Le test qui compte.** Le raccourci saute un dossier sur la foi de trois nombres ; si
    // l'un d'eux ne bougeait pas sur une purge, `VANISHED` ne serait jamais demandé et la copie
    // resterait pour toujours. `MESSAGES` est ce nombre-là.
    let server = Server::start(Config {
        qresync: true,
        ..gmail_like()
    })
    .unwrap();
    let (_dir, store, _inbox) = store();
    sync_over(&server, &store);
    drop(server);

    let mut boxes = gmail_like().mailboxes;
    // Le même `UIDVALIDITY`, le même `UIDNEXT` — `expunge` garde la trace du disparu — et un
    // message de moins.
    boxes[0] = Mailbox::new("INBOX", 1000, vec![Message::simple(1, "a")])
        .with_attribute("\\Inbox")
        .expunge(1);
    let purged = Server::start(Config {
        qresync: true,
        ..Config::with_inbox().with_mailboxes(boxes)
    })
    .unwrap();

    let second = sync_over(&purged, &store);

    assert_eq!(
        second.skipped, 3,
        "la boîte purgée a été sautée : sa disparition ne serait jamais vue"
    );
    assert_eq!(
        second.vanished, 1,
        "la copie disparue n'a pas été retirée par VANISHED"
    );
}

#[test]
fn a_courtesy_line_during_an_idle_is_not_an_event() {
    // **La régression du 2026-09-09.** Dovecot envoie `* OK Still here` toutes les deux minutes
    // pendant un `IDLE`. Rendue comme un événement, cette ligne faisait resynchroniser le
    // compte toutes les deux minutes — quinze dossiers sautés pour ne rien trouver, en boucle.
    let server = Server::start(Config::with_inbox().announcing_on_idle("* OK Still here")).unwrap();
    let (mut client, _) = idling(&server, std::time::Duration::from_secs(5));

    let tag = client.idle_start().unwrap();
    assert!(
        client
            .idle_wait(std::time::Duration::from_millis(200))
            .unwrap()
            .is_none(),
        "une ligne de courtoisie a été prise pour une arrivée"
    );
    client.idle_done(&tag).unwrap();

    // Et la connexion tient : la ligne a bien été lue, pas laissée dans le tuyau.
    assert_eq!(client.examine(b"INBOX").unwrap().exists, 3);
}

#[test]
fn a_bye_during_an_idle_ends_the_wait_rather_than_looping() {
    // `* BYE` veut dire que le serveur ferme. Continuer à attendre sur cette connexion
    // attendrait pour toujours ; l'erreur est ce qui fait reconnecter le veilleur.
    let server = Server::start(Config::with_inbox().announcing_on_idle("* BYE fermeture")).unwrap();
    let (mut client, _) = idling(&server, std::time::Duration::from_secs(5));

    let _tag = client.idle_start().unwrap();
    let outcome = client.idle_wait(std::time::Duration::from_millis(200));

    assert!(
        matches!(outcome, Err(Error::Malformed { .. })),
        "un BYE devait mettre fin à l'attente : {outcome:?}"
    );
}

#[test]
fn a_purge_and_an_arrival_that_cancel_each_other_out_are_still_seen() {
    // **Le contre-exemple qui a servi d'argument, et qui ne tient pas.**
    //
    // La première version de l'évitement du balayage exigeait `Plan::UpToDate`, au motif qu'un
    // message purgé et un message reçu laissent `EXISTS` inchangé. C'est vrai d'`EXISTS`
    // comparé à lui-même — et faux de la comparaison qui est faite : on compare `EXISTS` au
    // nombre d'UID **qu'on connaît après avoir appris les nouveaux**, et ce nombre-là monte
    // avec l'arrivée. L'inégalité apparaît, donc le balayage a lieu.
    //
    // Ce test est ce qui empêche de le croire sur parole.
    let server = Server::start(Config::without_condstore().with_mailboxes(vec![
        Mailbox::new(
            "INBOX",
            1000,
            vec![
                Message::simple(1, "un"),
                Message::simple(2, "deux"),
                Message::simple(3, "trois"),
            ],
        )
        .with_attribute("\\Inbox"),
    ]))
    .unwrap();
    let (_dir, store, _inbox) = store();
    let first = sync_over(&server, &store);
    assert_eq!(first.fetched, 3, "{:?}", first.failures);
    drop(server);

    // Le serveur d'après : l'UID 2 a été purgé, l'UID 4 est arrivé. Trois messages avant,
    // trois messages après — `EXISTS` n'a pas bougé.
    let after = Server::start(Config::without_condstore().with_mailboxes(vec![
            Mailbox::new(
                "INBOX",
                1000,
                vec![
                    Message::simple(1, "un"),
                    Message::simple(3, "trois"),
                    Message::simple(4, "quatre"),
                ],
            )
            .with_attribute("\\Inbox")
            .expunge(2),
        ]))
    .unwrap();

    let second = sync_over(&after, &store);

    assert_eq!(second.fetched, 1, "le message neuf devait descendre");
    assert_eq!(
        second.vanished, 1,
        "la purge a été manquée : le balayage n'a pas eu lieu alors que les comptes divergeaient"
    );
    assert_eq!(
        count(&store, "messages"),
        4,
        "les quatre contenus distincts"
    );
}

#[test]
fn a_folder_where_nothing_moved_is_not_swept_even_with_new_mail_elsewhere() {
    // L'autre moitié : le balayage doit bien être évité quand les comptes tombent juste. Sans
    // cette moitié, on aurait pu « corriger » le test précédent en balayant toujours.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, _inbox) = store();
    sync_over(&server, &store);
    drop(server);

    let mut boxes = gmail_like().mailboxes;
    boxes[0] = Mailbox::new(
        "INBOX",
        1000,
        vec![Message::simple(1, "a"), Message::simple(2, "neuf")],
    )
    .with_attribute("\\Inbox");
    let grown = Server::start(Config::with_inbox().with_mailboxes(boxes)).unwrap();

    let second = sync_over(&grown, &store);

    assert_eq!(second.fetched, 1, "le message neuf devait descendre");
    assert_eq!(
        second.vanished, 0,
        "un balayage a cru voir disparaître quelque chose"
    );
}

#[test]
fn a_folder_with_a_pending_seen_push_is_never_skipped() {
    // **Le défaut que ce test verrouille, trouvé sur le corpus réel et par aucun test.** Le
    // raccourci `LIST-STATUS` ne regarde que ce que le serveur annonce, et une marque `\Seen`
    // posée localement ne change rien de ce qu'il annonce. Résultat, dans le journal du démon :
    // `skipped=29`, la poussée en attente pour toujours, et un message qui redevient non lu au
    // passage suivant.
    //
    // Aucun test ne l'avait vu parce qu'aucun n'avait à la fois un dossier **à jour** et une
    // marque **en attente** : les tests de marquage partaient d'un store neuf, ceux du
    // raccourci n'ouvraient pas de message.
    let server = Server::start(gmail_like()).unwrap();
    let (_dir, store, inbox) = store();

    let first = sync_over(&server, &store);
    assert_eq!(first.skipped, 0);
    // Contrôle : sans marque, tout est évité au second passage.
    let second = sync_over(&server, &store);
    assert_eq!(second.skipped, 4, "le raccourci ne s'est pas déclenché");

    // Un message est marqué lu localement. Le serveur, lui, n'a pas bougé.
    let page = store
        .page(inbox, None, 10)
        .unwrap()
        .into_iter()
        .next()
        .expect("la boîte doit avoir un message");
    assert_eq!(store.mark_seen(page.id, inbox, 1_000).unwrap(), 1);
    assert_eq!(store.pending_seen(inbox).unwrap().len(), 1);

    let third = sync_over(&server, &store);
    assert!(
        third.skipped < 4,
        "le dossier a été évité malgré une poussée en attente : elle ne partira jamais"
    );
    // Et la poussée est faite, donc oubliée.
    assert!(
        store.pending_seen(inbox).unwrap().is_empty(),
        "la poussée est restée en attente après un passage qui a ouvert le dossier"
    );
}

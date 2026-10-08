//! **Critère 6 de `docs/PHASE-4.md`** : aucune requête réseau déclenchée par le contenu d'un
//! message, ni pendant une moisson, ni pendant une passe de vectorisation.
//!
//! ## Comment
//!
//! Un **canari** : un écouteur TCP sur le bouclage, que personne d'autre ne connaît. Les messages
//! servis par le faux serveur IMAP le désignent de toutes les façons qu'un courrier a de faire
//! sortir une requête — image, feuille de style, police, cadre, accusé de lecture, lien de
//! désabonnement, corps externe, pièce d'agenda — et passent par **le vrai chemin livré** : la
//! moisson de `mailsync`, les trois passes dérivées, puis la passe de fond des vecteurs dans
//! **l'exécutable `maild` relancé en enfant**, avec un vrai transformeur, minuscule. Une seule
//! connexion au canari fait tomber le test.
//!
//! ## Ce qu'il ne prouve pas, et il faut le lire d'abord
//!
//! - **Une résolution de nom** n'atteint pas le canari : un code qui ferait un `getaddrinfo` sur un
//!   nom tiré du courrier sans s'y connecter passerait. L'analyse de source de
//!   `xtask/tests/reseau.rs` couvre ce cas pour notre code — `to_socket_addrs` y est cherché ;
//! - **une adresse qui n'est pas celle du canari** ne le touche pas non plus. Le test vérifie que
//!   ce qui est *écrit dans le courrier* n'est pas contacté, pas que rien d'autre ne l'est — c'est
//!   l'autre moitié, celle de l'analyse de source.
//!
//! Les deux tests se complètent : celui-ci voit le comportement, l'autre voit la surface.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use mailcore::{FolderKind, Store};
use maild::jobs::Activity;
use maild::vectors::{Launch, Status, Vectoriser};
use mailfake::{Config, Mailbox, Message, Server};
use mailvec::testing::{TINY_PIECES, tiny_bert};

/// Un écouteur que seul le courrier hostile désigne.
struct Canary {
    listener: TcpListener,
}

impl Canary {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self { listener }
    }

    fn port(&self) -> u16 {
        self.listener.local_addr().unwrap().port()
    }

    /// Les connexions reçues depuis le dernier relevé. Le noyau les termine sans `accept` : elles
    /// attendent dans la file, et c'est là qu'on les compte.
    fn hits(&self) -> usize {
        // Un délai pour qu'une connexion partie juste avant le relevé ait le temps d'arriver.
        std::thread::sleep(Duration::from_millis(300));
        // Jusqu'à « rien en attente », et pas jusqu'à la première erreur : une connexion avortée
        // avant d'être acceptée rend `ECONNABORTED` sous Linux, et c'est **une connexion**. S'arrêter
        // dessus ferait taire le canari sur celles qui suivent.
        let mut count = 0;
        for _ in 0..10_000 {
            match self.listener.accept() {
                Ok(_) => count += 1,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => count += 1,
            }
        }
        count
    }
}

/// Tout ce qu'un courrier a pour faire sortir une requête, pointé sur le canari.
fn hostile(port: u16) -> Vec<Message> {
    let at = format!("127.0.0.1:{port}");
    let message = |uid: u32, raw: String| Message {
        uid,
        flags: Vec::new(),
        modseq: u64::from(uid),
        body: raw.into_bytes(),
    };
    vec![
        message(
            1,
            format!(
                "From: Lettre <lettre@exemple.fr>\r\n\
                 To: Marie <marie@exemple.fr>\r\n\
                 Subject: canari html\r\n\
                 Date: Tue, 1 Sep 2026 10:00:00 +0200\r\n\
                 Message-ID: <1@exemple.fr>\r\n\
                 Disposition-Notification-To: <http://{at}/accuse>\r\n\
                 Return-Receipt-To: http://{at}/recu\r\n\
                 X-Confirm-Reading-To: http://{at}/lu\r\n\
                 List-Unsubscribe: <http://{at}/desabonner>, <mailto:fin@exemple.fr>\r\n\
                 List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
                 Content-Base: http://{at}/base/\r\n\
                 Content-Location: http://{at}/emplacement\r\n\
                 MIME-Version: 1.0\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 \r\n\
                 <html><head>\
                 <meta http-equiv=\"refresh\" content=\"0;url=http://{at}/refresh\">\
                 <link rel=\"stylesheet\" href=\"http://{at}/style.css\">\
                 <link rel=\"prefetch\" href=\"http://{at}/prefetch\">\
                 <link rel=\"dns-prefetch\" href=\"//{at}\">\
                 <style>@import url(http://{at}/import.css);\
                 @font-face {{ font-family: x; src: url(http://{at}/police.woff2); }}\
                 body {{ background: url('http://{at}/fond.png'); }}</style>\
                 </head><body background=\"http://{at}/fond2.png\">\
                 <p>Bonjour, voici le canari.</p>\
                 <img src=\"http://{at}/pixel.png\" width=1 height=1>\
                 <img srcset=\"http://{at}/a.png 1x, https://{at}/b.png 2x\">\
                 <picture><source srcset=\"http://{at}/c.webp\"></picture>\
                 <iframe src=\"http://{at}/cadre\"></iframe>\
                 <video poster=\"http://{at}/affiche.png\" src=\"http://{at}/film.mp4\"></video>\
                 <audio src=\"http://{at}/son.mp3\"></audio>\
                 <object data=\"http://{at}/objet\"></object>\
                 <embed src=\"http://{at}/embarque\">\
                 <table background=\"http://{at}/table.png\"><tr><td>case</td></tr></table>\
                 <a href=\"http://{at}/lien\" ping=\"http://{at}/ping\">un lien</a>\
                 <svg><image href=\"http://{at}/svg.png\"/></svg>\
                 <form action=\"http://{at}/formulaire\"><input type=image src=\"http://{at}/bouton.png\"></form>\
                 </body></html>\r\n"
            ),
        ),
        message(
            2,
            format!(
                "From: Agenda <agenda@exemple.fr>\r\n\
                 To: Marie <marie@exemple.fr>\r\n\
                 Subject: canari agenda\r\n\
                 Date: Tue, 1 Sep 2026 11:00:00 +0200\r\n\
                 Message-ID: <2@exemple.fr>\r\n\
                 MIME-Version: 1.0\r\n\
                 Content-Type: multipart/mixed; boundary=\"limite\"\r\n\
                 \r\n\
                 --limite\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\
                 \r\n\
                 Une réunion, et un lien en clair : http://{at}/texte\r\n\
                 --limite\r\n\
                 Content-Type: text/calendar; charset=utf-8; method=REQUEST\r\n\
                 \r\n\
                 BEGIN:VCALENDAR\r\n\
                 VERSION:2.0\r\n\
                 METHOD:REQUEST\r\n\
                 BEGIN:VEVENT\r\n\
                 UID:canari@exemple.fr\r\n\
                 DTSTART:20261001T080000Z\r\n\
                 DTEND:20261001T090000Z\r\n\
                 SUMMARY:Réunion canari\r\n\
                 URL:http://{at}/evenement\r\n\
                 ATTACH;VALUE=URI:http://{at}/piece\r\n\
                 ATTACH;FMTTYPE=image/png:http://{at}/piece2.png\r\n\
                 LOCATION;ALTREP=\"http://{at}/lieu\":Salle\r\n\
                 ORGANIZER;SENT-BY=\"mailto:a@exemple.fr\";DIR=\"http://{at}/annuaire\":mailto:o@exemple.fr\r\n\
                 END:VEVENT\r\n\
                 END:VCALENDAR\r\n\
                 --limite\r\n\
                 Content-Type: message/external-body; access-type=URL; URL=\"http://{at}/externe\"\r\n\
                 \r\n\
                 Content-Type: text/plain\r\n\
                 \r\n\
                 --limite\r\n\
                 Content-Type: text/plain; name=\"notes.txt\"\r\n\
                 Content-Disposition: attachment; filename=\"notes.txt\"\r\n\
                 Content-Location: http://{at}/notes\r\n\
                 \r\n\
                 Des notes, et https://{at}/notes-lien\r\n\
                 --limite--\r\n"
            ),
        ),
        message(
            3,
            format!(
                "From: Fil <fil@exemple.fr>\r\n\
                 To: Marie <marie@exemple.fr>\r\n\
                 Subject: Re: canari agenda\r\n\
                 Date: Tue, 1 Sep 2026 12:00:00 +0200\r\n\
                 Message-ID: <3@exemple.fr>\r\n\
                 In-Reply-To: <2@exemple.fr>\r\n\
                 References: <http://{at}/reference> <2@exemple.fr>\r\n\
                 MIME-Version: 1.0\r\n\
                 Content-Type: multipart/related; boundary=\"r\"\r\n\
                 \r\n\
                 --r\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 \r\n\
                 <p>Une image liée <img src=\"cid:img1\"> et une autre <img src=\"http://{at}/related.png\"></p>\r\n\
                 --r\r\n\
                 Content-Type: image/png\r\n\
                 Content-ID: <img1>\r\n\
                 Content-Location: http://{at}/cid-location.png\r\n\
                 Content-Transfer-Encoding: base64\r\n\
                 \r\n\
                 iVBORw0KGgo=\r\n\
                 --r--\r\n"
            ),
        ),
    ]
}

/// Un store neuf, un compte IMAP, un dossier `INBOX`, et le transformeur minuscule à sa place.
fn store() -> (tempfile::TempDir, Store, Utf8PathBuf, mailcore::FolderId) {
    let dir = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
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
    tiny_bert(&mailvec::bert::default_dir(&root), TINY_PIECES);
    (dir, store, root, folder)
}

#[test]
fn a_harvest_and_a_vectorisation_pass_never_reach_what_the_mail_points_to() {
    let canary = Canary::start();
    let server = Server::start(Config::with_inbox().with_mailboxes(vec![Mailbox::new(
        "INBOX",
        1_000,
        hostile(canary.port()),
    )]))
    .unwrap();
    let (_dir, store, root, folder) = store();

    // La moisson, par le vrai client.
    let stream = TcpStream::connect(server.address()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut client = mailsync::client::Client::greet(stream).unwrap();
    client.login("marie@exemple.fr", "secret").unwrap();
    let report = mailsync::harvest(&mut client, &store, folder, b"INBOX").unwrap();
    assert_eq!(report.stored, 3, "la moisson n'a pas tout rangé");

    // Les trois passes que le démon fait suivre à une moisson.
    let progress = mailcore::Progress::new();
    mailcore::index::update(&store, &progress).unwrap();
    mailcore::contacts::update(&store, &progress).unwrap();
    mailcore::thread::update(&store, &progress).unwrap();
    assert_eq!(
        canary.hits(),
        0,
        "la moisson a contacté une adresse du courrier"
    );

    // La passe de fond des vecteurs, dans l'exécutable livré relancé en enfant.
    let vectoriser = Vectoriser::start_after(
        &root,
        Activity::default(),
        Launch::program(env!("CARGO_BIN_EXE_maild")),
        Duration::ZERO,
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while !(store.unvectorised_total().unwrap() == 0 && vectoriser.status() == Status::Idle) {
        assert!(
            Instant::now() < deadline,
            "la passe n'a pas fini : {:?}",
            vectoriser.status()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    vectoriser.stop();

    // **Le contrôle positif** : le contenu a bien été lu. Une passe qui n'aurait rien ouvert ne
    // contacterait rien non plus, et passerait ce test sans rien prouver.
    let summary = mailcore::vectors::summary_of(&store.vectors_path())
        .unwrap()
        .unwrap();
    assert_eq!(summary.records, 3, "les trois messages ont leur vecteur");
    let mailbox = mailcore::Mailbox::open(&root).unwrap();
    assert_eq!(
        mailbox.search("canari", 10).unwrap().len(),
        3,
        "l'index a lu les trois messages"
    );

    assert_eq!(
        canary.hits(),
        0,
        "la passe de vectorisation a contacté une adresse du courrier"
    );
}

#[test]
fn the_canary_hears_a_connection() {
    // **Le contrôle de l'instrument.** Un canari sourd — mauvaise adresse, file vidée trop tôt —
    // rendrait zéro à chaque fois, et le test précédent passerait en ne vérifiant rien.
    let canary = Canary::start();
    let _stream = TcpStream::connect(("127.0.0.1", canary.port())).unwrap();
    assert_eq!(canary.hits(), 1);
    assert_eq!(canary.hits(), 0, "une connexion se compte une fois");
}

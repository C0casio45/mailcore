//! Le dialogue SMTP **contre un serveur qu'on maîtrise**, y compris quand il répond mal.
//!
//! ## Pourquoi un serveur scripté et pas un vrai
//!
//! La même raison que `mailfake` pour l'IMAP, et une de plus. Un vrai serveur SMTP répond
//! correctement, et ce qu'il faut éprouver ici est ce qu'il fait **mal** : accepter le `DATA`
//! puis se taire, changer de code au milieu d'une réponse multi-ligne, refuser un destinataire
//! sur quatre.
//!
//! La raison de plus est que ce crate écrit **vers l'extérieur**. Un test contre un vrai serveur
//! enverrait un message à quelqu'un, et « le test envoie du courrier » n'est pas une propriété
//! qu'on veut d'une suite qui tourne à chaque `cargo test`.
//!
//! ## Le serveur est scripté, pas intelligent
//!
//! Il envoie ses réponses dans l'ordre, une par commande reçue. L'ordre des commandes du client
//! est déterminé par le protocole, donc une liste suffit — et elle rend chaque test lisible :
//! ce qu'on lui donne est exactement ce que le serveur dira.
//!
//! La seule chose qu'il comprend est `DATA`, parce qu'il faut bien consommer le message jusqu'au
//! point final avant de répondre. C'est aussi ce qui rend testable la seule étape qui compte
//! vraiment.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

use mailsmtp::client::Credential;
use mailsmtp::{Client, Error, Stage};

/// Ce que le serveur scripté fait après avoir envoyé sa dernière réponse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// Il attend, poliment, que le client raccroche.
    Wait,
    /// Il ferme la connexion **sans répondre** à la commande suivante.
    ///
    /// C'est la panne qui définit le critère 2 quand elle tombe après le point final : le
    /// serveur a le message, et son silence ne dit pas s'il l'a gardé.
    Cut,
}

/// Un serveur SMTP scripté, sur le bouclage.
struct Fake {
    port: u16,
    handle: Option<std::thread::JoinHandle<Vec<String>>>,
}

impl Fake {
    /// Démarre le serveur. `replies` est envoyé dans l'ordre, une entrée par commande reçue ;
    /// la première est le salut, avant toute commande.
    fn start(replies: &[&str], ending: Ending) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let script: Vec<String> = replies.iter().map(|it| (*it).to_owned()).collect();

        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve(stream, &script, ending)
        });

        Self {
            port,
            handle: Some(handle),
        }
    }

    /// Un client connecté et salué.
    fn greet(&self) -> Client<TcpStream> {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        Client::greet(stream).unwrap()
    }

    /// Un client connecté, sans lire le salut — pour tester un salut de refus.
    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        stream
    }

    /// Ce que le serveur a reçu, dans l'ordre. **Le contrôle du test** : sans lui, un client qui
    /// n'envoie rien du tout passerait.
    ///
    /// Le client est **consommé** : tant qu'il tient la connexion, le serveur attend une ligne
    /// de plus. Le prendre par valeur rend l'oubli impossible plutôt que lent.
    fn received<S>(&mut self, client: Client<S>) -> Vec<String> {
        drop(client);
        self.received_alone()
    }

    /// La même, quand il n'y a pas de client à fermer.
    ///
    /// Un seul cas : le salut de refus, où `Client::greet` a consommé le flux et échoué. Le
    /// serveur a déjà fermé de son côté, donc rien n'attend.
    fn received_alone(&mut self) -> Vec<String> {
        self.handle
            .take()
            .expect("déjà récupéré")
            .join()
            .unwrap_or_default()
    }
}

/// Sert une connexion selon le script, et rend ce qui a été reçu.
///
/// ## Le délai de lecture n'est pas une précaution, c'est ce qui évite une suite qui bloque
///
/// Le serveur attend la ligne suivante. Si le client n'en envoie plus — parce qu'il a fini, ou
/// parce que le test le tient encore ouvert en relevant ce qui a été reçu — cette attente est
/// sans fin, et `join` avec elle.
///
/// C'est arrivé au premier jet : la suite a tourné dix minutes avant d'être tuée. Un serveur de
/// test qui peut bloquer est une suite qui bloque, et le diagnostic coûte plus cher que les deux
/// secondes de ce délai.
fn serve(stream: TcpStream, script: &[String], ending: Ending) -> Vec<String> {
    let mut received = Vec::new();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut out = stream.try_clone().unwrap();
    let mut lines = BufReader::new(stream).lines();
    let mut sent = 0_usize;

    // **Une entrée de script est une réponse entière**, pas une ligne. Une réponse multi-ligne
    // part d'un bloc en réaction à **une** commande ; en envoyer une ligne par commande reçue
    // bloquait les deux côtés — le client attendait la fin de la réponse, le serveur attendait
    // la commande suivante. C'était le premier jet, et les deux s'attendaient jusqu'au délai.
    //
    // Les lignes d'une même réponse sont donc séparées par un saut de ligne dans le script.
    let say = |out: &mut TcpStream, sent: &mut usize| -> bool {
        match script.get(*sent) {
            Some(reply) => {
                for line in reply.split('\n') {
                    let _ = out.write_all(format!("{line}\r\n").as_bytes());
                }
                let _ = out.flush();
                *sent += 1;
                true
            }
            // Script épuisé : soit on attend, soit on coupe. C'est `Ending` qui décide.
            None => ending == Ending::Wait,
        }
    };

    // Le salut part avant toute commande.
    if !say(&mut out, &mut sent) {
        return received;
    }

    while let Some(Ok(line)) = lines.next() {
        received.push(line.clone());

        // **`DATA` est la seule commande que ce serveur comprenne.** Il faut consommer le
        // message jusqu'au point final avant de répondre, sinon la réponse suivante partirait au
        // milieu des données.
        if line.eq_ignore_ascii_case("DATA") {
            if !say(&mut out, &mut sent) {
                return received;
            }
            for body in lines.by_ref() {
                let Ok(body) = body else { return received };
                if body == "." {
                    break;
                }
                received.push(format!("DATA> {body}"));
            }
            if !say(&mut out, &mut sent) {
                return received;
            }
            continue;
        }

        if line.eq_ignore_ascii_case("QUIT") {
            let _ = out.write_all(b"221 au revoir\r\n");
            return received;
        }
        if !say(&mut out, &mut sent) {
            return received;
        }
    }
    received
}

/// Le script d'un serveur qui accepte tout, jusqu'au `DATA` inclus.
fn accepting() -> Vec<&'static str> {
    vec![
        "220 mail.exemple.fr prêt",
        "250-mail.exemple.fr
250-SIZE 35882577
250-STARTTLS
250 AUTH PLAIN XOAUTH2",
        "235 authentifié",
        "250 expéditeur accepté",
        "250 destinataire accepté",
        "354 allez-y",
        "250 2.0.0 message accepté",
    ]
}

// ---------------------------------------------------------------------------
// Le chemin correct.
// ---------------------------------------------------------------------------

#[test]
fn a_full_dialogue_sends_a_message_and_the_server_sees_it() {
    let mut fake = Fake::start(&accepting(), Ending::Wait);
    let mut client = fake.greet();

    client.ehlo("mailcore").unwrap();
    assert!(client.has("STARTTLS"));
    assert_eq!(client.size_limit(), Some(35_882_577));

    client
        .auth("marie@exemple.fr", Credential::Password("secret"))
        .unwrap();
    client.mail_from("marie@exemple.fr", Some(1024)).unwrap();
    client.rcpt_to("jean@ailleurs.fr").unwrap();
    let accepted = client.data(b"Subject: essai\r\n\r\nBonjour.\r\n").unwrap();
    client.quit();

    assert_eq!(accepted.code, 250);

    // **Le contrôle du test** : ce que le serveur a vraiment reçu. Sans lui, un client qui
    // n'enverrait rien passerait.
    let seen = fake.received(client);
    assert!(seen.iter().any(|it| it == "EHLO mailcore"), "{seen:?}");
    assert!(
        seen.iter().any(|it| it.starts_with("AUTH PLAIN ")),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|it| it == "MAIL FROM:<marie@exemple.fr> SIZE=1024"),
        "la taille n'a pas été annoncée : {seen:?}"
    );
    assert!(
        seen.iter().any(|it| it == "RCPT TO:<jean@ailleurs.fr>"),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|it| it == "DATA> Subject: essai"),
        "le corps n'est pas arrivé : {seen:?}"
    );
}

#[test]
fn the_secret_never_travels_in_clear() {
    // La ligne d'`AUTH` porte le secret en base64. Ce n'est pas du chiffrement — le critère 7
    // s'en occupera pour de bon — mais le secret ne doit pas être lisible tel quel sur le fil.
    let mut fake = Fake::start(&accepting(), Ending::Wait);
    let mut client = fake.greet();
    client.ehlo("mailcore").unwrap();
    client
        .auth("marie@exemple.fr", Credential::Password("MON-SECRET"))
        .unwrap();
    client.quit();

    let seen = fake.received(client).join("\n");
    assert!(
        !seen.contains("MON-SECRET"),
        "le secret est passé en clair sur le fil"
    );
}

#[test]
fn an_oauth2_account_authenticates_with_xoauth2() {
    let mut fake = Fake::start(&accepting(), Ending::Wait);
    let mut client = fake.greet();
    client.ehlo("mailcore").unwrap();
    client
        .auth("marie@exemple.fr", Credential::Bearer("ya29.jeton"))
        .unwrap();
    client.quit();

    let seen = fake.received(client);
    assert!(
        seen.iter().any(|it| it.starts_with("AUTH XOAUTH2 ")),
        "un compte OAuth2 doit s'authentifier en XOAUTH2 : {seen:?}"
    );
    assert!(
        !seen.iter().any(|it| it.starts_with("AUTH PLAIN")),
        "le jeton est parti dans un champ mot de passe : {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// Ce que le serveur fait mal.
// ---------------------------------------------------------------------------

#[test]
fn a_server_that_refuses_the_connection_is_seen_at_the_greeting() {
    let mut fake = Fake::start(&["554 pas de service ici"], Ending::Wait);
    let refused = Client::greet(fake.connect());

    match refused {
        Err(Error::Rejected { stage, code, .. }) => {
            assert_eq!(stage, Stage::Greeting);
            assert_eq!(code, 554);
        }
        other => panic!("attendu un refus au salut : {other:?}"),
    }
    // Rien n'a été envoyé : le dire tout de suite vaut mieux qu'un `EHLO` dans le vide.
    assert!(fake.received_alone().is_empty());
}

#[test]
fn a_transient_refusal_is_retryable_and_a_permanent_one_is_not() {
    // La distinction appartient au serveur, et c'est elle qui décide de réessayer.
    for (code, retryable) in [(452, true), (550, false)] {
        let script = vec![
            "220 prêt",
            "250 mail.exemple.fr",
            if code == 452 {
                "452 boîte pleine, reviens"
            } else {
                "550 destinataire inconnu"
            },
        ];
        let mut fake = Fake::start(&script, Ending::Wait);
        let mut client = fake.greet();
        client.ehlo("mailcore").unwrap();

        let refused = client.mail_from("marie@exemple.fr", None).unwrap_err();
        assert_eq!(refused.retryable(), retryable, "pour {code}");
        assert_eq!(refused.stage(), Some(Stage::Sender));
        // **Et surtout** : aucun de ces refus ne laisse de doute sur l'envoi.
        assert!(!refused.may_have_been_sent(), "pour {code}");
        drop(fake.received(client));
    }
}

#[test]
fn a_server_that_cuts_after_the_final_dot_leaves_a_doubt() {
    // **Le cas qui définit le critère 2 de `docs/PHASE-3.md`.**
    //
    // Le serveur a accepté le `DATA`, a reçu tout le message, et se tait. Rien dans le protocole
    // ne permet de lui demander s'il l'a gardé. Réessayer risque un doublon chez le
    // destinataire ; abandonner risque de perdre le message. Le client ne peut pas trancher —
    // il doit dire **où** il en était, et c'est tout.
    let script = vec![
        "220 prêt",
        "250 mail.exemple.fr",
        "250 expéditeur accepté",
        "250 destinataire accepté",
        "354 allez-y",
        // Rien après : `Ending::Cut` ferme au moment de répondre au point final.
    ];
    let mut fake = Fake::start(&script, Ending::Cut);
    let mut client = fake.greet();
    client.ehlo("mailcore").unwrap();
    client.mail_from("marie@exemple.fr", None).unwrap();
    client.rcpt_to("jean@ailleurs.fr").unwrap();

    let cut = client.data(b"Subject: essai\r\n\r\ncorps\r\n").unwrap_err();

    assert!(
        cut.may_have_been_sent(),
        "le doute n'est pas signalé : la file d'envoi renverrait le message. {cut:?}"
    );
    assert_eq!(cut.stage(), Some(Stage::Committing));

    // Le serveur avait bien tout reçu — c'est ce qui rend le doute réel et non théorique.
    let seen = fake.received(client);
    assert!(
        seen.iter().any(|it| it == "DATA> Subject: essai"),
        "le serveur n'a pas reçu le message : le test ne mesure pas le bon doute. {seen:?}"
    );
}

#[test]
fn a_cut_before_the_data_leaves_no_doubt() {
    // Le contrôle inverse, sans lequel le précédent ne prouverait rien : une coupure **avant**
    // le point final ne laisse aucun doute, et un client qui signalerait un doute partout ferait
    // abandonner des messages qui n'ont jamais été envoyés.
    let mut fake = Fake::start(&["220 prêt", "250 mail.exemple.fr"], Ending::Cut);
    let mut client = fake.greet();
    client.ehlo("mailcore").unwrap();

    let cut = client.mail_from("marie@exemple.fr", None).unwrap_err();
    assert!(!cut.may_have_been_sent(), "faux doute : {cut:?}");
    drop(fake.received(client));
}

#[test]
fn a_multi_line_reply_with_mismatched_codes_is_refused() {
    let mut fake = Fake::start(
        &[
            "220 prêt",
            "250-un
251 deux",
        ],
        Ending::Wait,
    );
    let mut client = fake.greet();

    let refused = client.ehlo("mailcore");
    assert!(
        matches!(refused, Err(Error::Malformed { .. })),
        "{refused:?}"
    );
    drop(fake.received(client));
}

#[test]
fn a_server_that_answers_data_with_a_two_fifty_is_refused_rather_than_trusted() {
    // Un `DATA` attend un `354`. Un `250` veut dire que le serveur n'a pas compris ce qu'on lui
    // demandait, et lui envoyer le message quand même le ferait interpréter comme des commandes.
    let script = vec![
        "220 prêt",
        "250 mail.exemple.fr",
        "250 expéditeur accepté",
        "250 destinataire accepté",
        "250 pas un 354",
    ];
    let mut fake = Fake::start(&script, Ending::Wait);
    let mut client = fake.greet();
    client.ehlo("mailcore").unwrap();
    client.mail_from("marie@exemple.fr", None).unwrap();
    client.rcpt_to("jean@ailleurs.fr").unwrap();

    let refused = client.data(b"corps\r\n");
    assert!(
        matches!(refused, Err(Error::Malformed { .. })),
        "{refused:?}"
    );

    let seen = fake.received(client);
    assert!(
        !seen.iter().any(|it| it.starts_with("DATA> ")),
        "le message a été envoyé malgré la réponse inattendue : {seen:?}"
    );
}

#[test]
fn a_message_bigger_than_the_announced_limit_is_refused_before_the_envelope() {
    // RFC 1870 : le serveur annonce sa limite à l'`EHLO`. La voir avant d'ouvrir l'enveloppe
    // évite de téléverser trente mégaoctets pour se faire refuser à la fin.
    let script = vec![
        "220 prêt",
        "250-mail.exemple.fr
250 SIZE 1000",
    ];
    let mut fake = Fake::start(&script, Ending::Wait);
    let mut client = fake.greet();
    client.ehlo("mailcore").unwrap();

    let refused = client.mail_from("marie@exemple.fr", Some(5000));
    match refused {
        Err(Error::TooLarge { size, limit }) => {
            assert_eq!(size, 5000);
            assert_eq!(limit, 1000);
        }
        other => panic!("attendu un refus de taille : {other:?}"),
    }

    let seen = fake.received(client);
    assert!(
        !seen.iter().any(|it| it.starts_with("MAIL FROM")),
        "l'enveloppe a été ouverte quand même : {seen:?}"
    );
}

#[test]
fn a_mechanism_the_server_does_not_announce_is_refused_before_sending_the_secret() {
    // **Le secret ne part pas au hasard.** Un serveur qui n'annonce pas `XOAUTH2` ne doit pas
    // recevoir un jeton d'accès dans un `AUTH PLAIN` : il le journaliserait avec l'identifiant.
    let script = vec![
        "220 prêt",
        "250-mail.exemple.fr
250 AUTH PLAIN",
    ];
    let mut fake = Fake::start(&script, Ending::Wait);
    let mut client = fake.greet();
    client.ehlo("mailcore").unwrap();

    let refused = client.auth("marie@exemple.fr", Credential::Bearer("ya29.jeton"));
    match refused {
        Err(Error::MissingCapability { capability }) => {
            assert_eq!(capability, "AUTH XOAUTH2");
        }
        other => panic!("attendu une capacité manquante : {other:?}"),
    }

    let seen = fake.received(client);
    assert!(
        !seen.iter().any(|it| it.starts_with("AUTH")),
        "le jeton est parti quand même : {seen:?}"
    );
}

#[test]
fn a_refused_authentication_says_so_without_the_secret() {
    let script = vec![
        "220 prêt",
        "250-mail.exemple.fr\n250 AUTH PLAIN",
        "535 5.7.8 identifiants refusés",
    ];
    let mut fake = Fake::start(&script, Ending::Wait);
    let mut client = fake.greet();
    client.ehlo("mailcore").unwrap();

    let refused = client.auth("marie@exemple.fr", Credential::Password("MON-SECRET"));
    match refused {
        Err(Error::AuthRefused { reason }) => {
            assert!(reason.contains("refusés"), "{reason}");
            assert!(
                !reason.contains("MON-SECRET"),
                "le secret est dans l'erreur"
            );
        }
        other => panic!("attendu un refus d'authentification : {other:?}"),
    }
    // Un refus d'authentification n'est **pas** réessayable : insister fait bloquer le compte.
    drop(fake.received(client));
}

#[test]
fn a_dot_only_line_in_the_message_reaches_the_server_intact() {
    // Le doublage du point, vu de bout en bout. Sans lui, le serveur n'aurait reçu que la
    // première ligne — et aurait pris le reste pour des commandes.
    let mut fake = Fake::start(&accepting(), Ending::Wait);
    let mut client = fake.greet();
    client.ehlo("mailcore").unwrap();
    client
        .auth("marie@exemple.fr", Credential::Password("secret"))
        .unwrap();
    client.mail_from("marie@exemple.fr", None).unwrap();
    client.rcpt_to("jean@ailleurs.fr").unwrap();
    client
        .data(b"Subject: essai\r\n\r\navant\r\n.\r\napres\r\n")
        .unwrap();
    client.quit();

    let seen = fake.received(client);
    // Le serveur de test ne retire pas le point doublé — il rend ce qu'il a lu. Ce qui compte
    // est que la ligne `.` **n'ait pas** terminé le transfert : `apres` est arrivé.
    assert!(
        seen.iter().any(|it| it == "DATA> apres"),
        "le transfert a été terminé par la ligne de point : {seen:?}"
    );
    assert!(
        seen.iter().any(|it| it == "DATA> .."),
        "le point n'a pas été doublé : {seen:?}"
    );
}

#[test]
fn a_well_behaved_starttls_go_ahead_hands_the_stream_over() {
    // Le contrôle négatif de `an_injection_after_the_starttls_go_ahead_is_refused`, qui est
    // dans `client.rs` : sans octets en trop, le flux passe. Sans ce test-ci, une
    // implémentation qui refuserait **toujours** passerait là-bas et casserait tout `STARTTLS`.
    //
    // Et il vérifie ce que le refus, lui, ne peut pas voir : que le dialogue **en clair** se
    // borne à `EHLO` et `STARTTLS`. C'est la seule assertion du crate sur ce qui part avant le
    // chiffrement, et donc sur ce qu'un observateur du réseau peut lire.
    let mut fake = Fake::start(
        &[
            "220 mail.exemple.fr prêt",
            "250-mail.exemple.fr
250 STARTTLS",
            "220 Ready to start TLS",
        ],
        Ending::Wait,
    );
    let mut client = fake.greet();
    client.ehlo("exemple.fr").unwrap();
    client.command(Stage::StartTls, "STARTTLS").unwrap();

    let stream = client.into_stream().expect("le flux devait être rendu");
    drop(stream);
    let seen = fake.received_alone();
    assert_eq!(
        seen,
        vec!["EHLO exemple.fr".to_owned(), "STARTTLS".to_owned()],
        "le dialogue en clair doit se borner à ces deux commandes : {seen:?}"
    );
}

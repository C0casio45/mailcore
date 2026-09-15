//! Le serveur mis à l'épreuve **par une vraie connexion TCP**.
//!
//! ## Pourquoi ces tests existent en plus des tests unitaires
//!
//! Les tests unitaires de `mailfake` couvrent des fonctions pures : découper une commande,
//! développer un ensemble d'UID. Ils ne disent rien du serveur, qui n'a jamais reçu un octet.
//!
//! Et pour un harnais, c'est la seule chose qui compte. **Une panne qui ne se déclenche pas
//! est pire que pas de panne** : elle ferait passer le client de l'étape 3 pour correct alors
//! que rien ne l'a éprouvé. Chaque [`Fault`] a donc ici un test qui vérifie qu'elle produit
//! bien le comportement fautif annoncé — et pas seulement qu'elle est acceptée en
//! configuration.
//!
//! ## Le client de test compte les octets
//!
//! Il lit les littéraux `{n}` en comptant n octets, jamais « jusqu'à la parenthèse ». C'est
//! une condition de validité : un client qui lirait par délimiteur ne verrait pas la
//! différence entre un littéral correct et un littéral tronqué, donc il ne pourrait pas
//! prouver que la panne fonctionne.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use mailfake::{Config, Fault, Mailbox, Message, Server};

/// Un client IMAP minimal, écrit pour ces tests seulement.
struct Client {
    reader: BufReader<TcpStream>,
    stream: TcpStream,
    tag: u32,
}

/// Un morceau de réponse : une ligne, ou les octets d'un littéral.
#[derive(Debug, PartialEq, Eq)]
enum Piece {
    Line(String),
    Literal(Vec<u8>),
}

impl Client {
    fn connect(server: &Server) -> Self {
        let stream = TcpStream::connect(server.address()).expect("connexion au serveur de test");
        // Court exprès : plusieurs pannes font que rien n'arrivera jamais, et un test qui
        // attend trente secondes pour le constater est un test qu'on désactive.
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("délai de lecture");
        Self {
            reader: BufReader::new(stream.try_clone().expect("dupliquer le socket")),
            stream,
            tag: 0,
        }
    }

    /// Lit une ligne, sans le `CRLF`. `None` si la connexion est fermée ou muette.
    fn line(&mut self) -> Option<String> {
        let mut raw = Vec::new();
        match self.reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        while raw.last().is_some_and(|it| *it == b'\n' || *it == b'\r') {
            raw.pop();
        }
        Some(String::from_utf8_lossy(&raw).into_owned())
    }

    /// Envoie une commande étiquetée et rend son étiquette.
    fn send(&mut self, command: &str) -> String {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        write!(self.stream, "{tag} {command}\r\n").expect("écriture de la commande");
        self.stream.flush().expect("vidage");
        tag
    }

    /// Lit jusqu'à la réponse étiquetée, en comptant les littéraux.
    ///
    /// Rend tous les morceaux, la ligne étiquetée comprise. S'arrête aussi quand la connexion
    /// se ferme — ce qui est le cas normal sous plusieurs pannes, et le test le vérifie alors
    /// à l'absence de ligne étiquetée.
    fn collect(&mut self, tag: &str) -> Vec<Piece> {
        let mut out = Vec::new();
        loop {
            let Some(line) = self.line() else { return out };
            let literal = literal_length(&line);
            let tagged = line.starts_with(&format!("{tag} "));
            out.push(Piece::Line(line));

            if let Some(length) = literal {
                let mut bytes = vec![0_u8; length];
                match self.reader.read_exact(&mut bytes) {
                    // **Compté, jamais délimité.** C'est ce qui rend la troncature visible.
                    Ok(()) => out.push(Piece::Literal(bytes)),
                    Err(_) => return out,
                }
            }
            if tagged {
                return out;
            }
        }
    }

    /// S'authentifie avec les identifiants de [`Config::with_inbox`].
    fn login(&mut self) -> Vec<Piece> {
        let tag = self.send("LOGIN marie@exemple.fr secret");
        self.collect(&tag)
    }
}

/// La longueur annoncée par un `{n}` en fin de ligne.
fn literal_length(line: &str) -> Option<usize> {
    let inner = line.rsplit_once('{')?.1.strip_suffix('}')?;
    inner.parse().ok()
}

/// Les lignes d'une réponse, à plat.
fn lines(pieces: &[Piece]) -> Vec<String> {
    pieces
        .iter()
        .filter_map(|it| match it {
            Piece::Line(line) => Some(line.clone()),
            Piece::Literal(_) => None,
        })
        .collect()
}

/// Les littéraux d'une réponse.
fn literals(pieces: &[Piece]) -> Vec<Vec<u8>> {
    pieces
        .iter()
        .filter_map(|it| match it {
            Piece::Literal(bytes) => Some(bytes.clone()),
            Piece::Line(_) => None,
        })
        .collect()
}

/// Vrai si une des lignes contient le fragment.
fn has(pieces: &[Piece], fragment: &str) -> bool {
    lines(pieces).iter().any(|it| it.contains(fragment))
}

/// Les en-têtes de réponse `FETCH`, et rien d'autre.
///
/// `contains("FETCH")` ne suffit pas, et c'est un piège dans lequel la première version de ce
/// fichier est tombée : la ligne étiquetée `a4 OK UID FETCH terminé` contient le mot aussi.
/// Elle n'a pas de littéral, donc en extraire une longueur paniquait — dans le test, pas dans
/// le serveur.
fn fetch_heads(pieces: &[Piece]) -> Vec<String> {
    lines(pieces)
        .into_iter()
        .filter(|it| it.starts_with("* ") && it.contains(" FETCH ("))
        .collect()
}

// ---------------------------------------------------------------------------
// Le chemin correct. Sans lui, aucun test de panne ne veut rien dire : un
// serveur qui échoue toujours « réussirait » toutes les pannes.
// ---------------------------------------------------------------------------

#[test]
fn the_greeting_announces_the_capabilities() {
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);

    let greeting = client.line().expect("pas de salut");
    assert!(greeting.starts_with("* OK [CAPABILITY "), "{greeting}");
    assert!(greeting.contains("IMAP4rev1"), "{greeting}");
    assert!(greeting.contains("CONDSTORE"), "{greeting}");
}

#[test]
fn login_succeeds_with_the_right_credentials() {
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();

    let reply = client.login();
    assert!(has(&reply, "OK LOGIN"), "{:?}", lines(&reply));
}

#[test]
fn login_fails_with_the_wrong_password() {
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();

    let tag = client.send("LOGIN marie@exemple.fr pas-le-bon");
    let reply = client.collect(&tag);
    assert!(has(&reply, "NO"), "{:?}", lines(&reply));
}

#[test]
fn selecting_before_authenticating_is_refused() {
    // Un serveur qui pardonne cache au client qu'il a émis ses commandes dans le mauvais
    // ordre — et le fournisseur, lui, ne pardonnera pas.
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();

    let tag = client.send("SELECT INBOX");
    let reply = client.collect(&tag);
    assert!(has(&reply, "BAD"), "{:?}", lines(&reply));
}

#[test]
fn list_returns_the_mailbox_with_its_delimiter() {
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();

    let tag = client.send("LIST \"\" *");
    let reply = client.collect(&tag);
    assert!(
        has(&reply, "* LIST (\\HasNoChildren) \"/\" \"INBOX\""),
        "{:?}",
        lines(&reply)
    );
    assert!(has(&reply, "OK LIST"), "{:?}", lines(&reply));
}

#[test]
fn select_announces_uidvalidity_uidnext_and_highest_modseq() {
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();

    let tag = client.send("SELECT INBOX");
    let reply = client.collect(&tag);
    assert!(has(&reply, "* 3 EXISTS"), "{:?}", lines(&reply));
    assert!(has(&reply, "[UIDVALIDITY 1000]"), "{:?}", lines(&reply));
    assert!(has(&reply, "[UIDNEXT 4]"), "{:?}", lines(&reply));
    assert!(has(&reply, "[HIGHESTMODSEQ 3]"), "{:?}", lines(&reply));
}

#[test]
fn selecting_an_unknown_mailbox_is_refused() {
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();

    let tag = client.send("SELECT \"Boîte inventée\"");
    let reply = client.collect(&tag);
    assert!(has(&reply, "NO"), "{:?}", lines(&reply));
}

#[test]
fn uid_fetch_returns_bodies_whose_length_matches_the_announcement() {
    // Le test de référence du littéral : la longueur annoncée et le nombre d'octets lus
    // doivent coïncider. Tout le reste du fichier en dépend.
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();
    let tag = client.send("SELECT INBOX");
    client.collect(&tag);

    let tag = client.send("UID FETCH 1:* (UID FLAGS BODY[])");
    let reply = client.collect(&tag);

    let bodies = literals(&reply);
    assert_eq!(bodies.len(), 3, "{:?}", lines(&reply));
    for (index, line) in fetch_heads(&reply).iter().enumerate() {
        let declared = literal_length(line).expect("longueur annoncée");
        assert_eq!(
            declared,
            bodies[index].len(),
            "longueur annoncée et octets lus divergent"
        );
    }
    assert!(bodies[0].starts_with(b"From: Marie"), "corps inattendu");
    assert!(has(&reply, "OK UID FETCH"), "{:?}", lines(&reply));
}

#[test]
fn uid_fetch_of_one_uid_returns_only_that_one() {
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();
    let tag = client.send("SELECT INBOX");
    client.collect(&tag);

    let tag = client.send("UID FETCH 2 (UID BODY[])");
    let reply = client.collect(&tag);
    assert_eq!(literals(&reply).len(), 1, "{:?}", lines(&reply));
    assert!(has(&reply, "UID 2"), "{:?}", lines(&reply));
}

#[test]
fn changed_since_filters_on_modseq() {
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();
    let tag = client.send("ENABLE CONDSTORE");
    let reply = client.collect(&tag);
    assert!(has(&reply, "* ENABLED CONDSTORE"), "{:?}", lines(&reply));
    let tag = client.send("SELECT INBOX");
    client.collect(&tag);

    // Les MODSEQ valent 1, 2, 3 : au-delà de 2, il ne reste que le troisième.
    let tag = client.send("UID FETCH 1:* (UID BODY[]) (CHANGEDSINCE 2)");
    let reply = client.collect(&tag);
    assert_eq!(literals(&reply).len(), 1, "{:?}", lines(&reply));
    assert!(has(&reply, "UID 3"), "{:?}", lines(&reply));
}

#[test]
fn fetch_by_sequence_number_is_refused() {
    // Le serveur refuse ce que la synchronisation ne doit jamais émettre.
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();
    let tag = client.send("SELECT INBOX");
    client.collect(&tag);

    let tag = client.send("FETCH 1 (BODY[])");
    let reply = client.collect(&tag);
    assert!(has(&reply, "BAD"), "{:?}", lines(&reply));
}

#[test]
fn a_mailbox_name_with_spaces_survives_the_round_trip() {
    // `[Gmail]/Tous les messages` est le cas ordinaire, pas la curiosité.
    let name = "[Gmail]/Tous les messages";
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox::new(
        name,
        7,
        vec![Message::simple(1, "a")],
    )]);
    let server = Server::start(config).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();

    let tag = client.send(&format!("SELECT \"{name}\""));
    let reply = client.collect(&tag);
    assert!(has(&reply, "[UIDVALIDITY 7]"), "{:?}", lines(&reply));
}

#[test]
fn an_unparsable_line_gets_an_answer_rather_than_silence() {
    // Ne rien répondre laisserait le client attendre pour toujours — c'est un blocage, pas
    // une erreur, et c'est bien plus dur à diagnostiquer.
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();

    write!(client.stream, "\r\n").expect("écriture");
    client.stream.flush().expect("vidage");
    assert_eq!(client.line().as_deref(), Some("* BAD ligne illisible"));
}

#[test]
fn logout_says_bye_and_closes() {
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();

    let tag = client.send("LOGOUT");
    let reply = client.collect(&tag);
    assert!(has(&reply, "* BYE"), "{:?}", lines(&reply));
    assert!(has(&reply, "OK LOGOUT"), "{:?}", lines(&reply));
    assert!(client.line().is_none(), "la connexion est restée ouverte");
}

// ---------------------------------------------------------------------------
// Les pannes. Une par test, et chacune vérifie le comportement **fautif**,
// pas seulement que la configuration a été acceptée.
// ---------------------------------------------------------------------------

#[test]
fn a_truncated_literal_sends_fewer_bytes_than_announced_then_hangs_up() {
    let config = Config::with_inbox().with_fault(Fault::TruncatedLiteral { uid: 1, sent: 10 });
    let server = Server::start(config).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();
    let tag = client.send("SELECT INBOX");
    client.collect(&tag);

    let tag = client.send("UID FETCH 1 (UID BODY[])");
    let reply = client.collect(&tag);

    let head = fetch_heads(&reply)
        .into_iter()
        .next()
        .expect("aucune réponse FETCH");
    let declared = literal_length(&head).expect("longueur annoncée");
    assert!(declared > 10, "la longueur annoncée devait rester entière");
    // `read_exact` échoue sur une connexion fermée avant la fin : le littéral n'est donc pas
    // rendu du tout, et c'est la preuve que la troncature est passée. Un client qui lirait
    // « jusqu'à la parenthèse » ne verrait aucune différence.
    assert!(
        literals(&reply).is_empty(),
        "le littéral complet est arrivé : la troncature n'a pas eu lieu"
    );
    assert!(
        !has(&reply, "OK UID FETCH"),
        "le serveur a terminé sa réponse alors qu'il devait raccrocher"
    );
}

#[test]
fn an_announced_length_may_exceed_the_bytes_that_follow() {
    // Le client attend des octets qui ne viendront jamais. C'est la panne qui bloque un
    // client naïf plutôt que de le faire échouer.
    let config =
        Config::with_inbox().with_fault(Fault::LiteralLengthMismatch { uid: 1, delta: 50 });
    let server = Server::start(config).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();
    let tag = client.send("SELECT INBOX");
    client.collect(&tag);

    let tag = client.send("UID FETCH 1 (UID BODY[])");
    let reply = client.collect(&tag);

    let head = fetch_heads(&reply)
        .into_iter()
        .next()
        .expect("aucune réponse FETCH");
    let declared = literal_length(&head).expect("longueur annoncée");
    let real = Message::simple(1, "facture").body.len();
    assert_eq!(declared, real + 50, "l'écart annoncé n'a pas été appliqué");
}

#[test]
fn an_announced_length_may_fall_short_of_the_bytes_that_follow() {
    // Le miroir du cas précédent : le client prend la fin du corps pour de la syntaxe IMAP.
    let config =
        Config::with_inbox().with_fault(Fault::LiteralLengthMismatch { uid: 1, delta: -20 });
    let server = Server::start(config).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();
    let tag = client.send("SELECT INBOX");
    client.collect(&tag);

    let tag = client.send("UID FETCH 1 (UID BODY[])");
    let reply = client.collect(&tag);

    let head = fetch_heads(&reply)
        .into_iter()
        .next()
        .expect("aucune réponse FETCH");
    let declared = literal_length(&head).expect("longueur annoncée");
    let real = Message::simple(1, "facture").body.len();
    assert_eq!(declared, real - 20);
    // Les octets en trop sont bien partis : ils polluent le flux après le littéral.
    let literal = literals(&reply).into_iter().next().expect("littéral");
    assert_eq!(
        literal.len(),
        declared,
        "le client a lu la longueur annoncée"
    );
}

#[test]
fn uidvalidity_changes_between_two_selects() {
    // Ce qu'un serveur fait après une restauration de sauvegarde : tous les UID connus
    // deviennent faux d'un coup.
    let config = Config::with_inbox().with_fault(Fault::UidvalidityChangesOnSelect);
    let server = Server::start(config).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();

    let first = {
        let tag = client.send("SELECT INBOX");
        uidvalidity(&client.collect(&tag))
    };
    let second = {
        let tag = client.send("SELECT INBOX");
        uidvalidity(&client.collect(&tag))
    };
    assert_ne!(first, second, "UIDVALIDITY n'a pas bougé");
}

#[test]
fn uidvalidity_is_stable_without_the_fault() {
    // Le contrôle du test précédent. Sans lui, un serveur qui changerait toujours
    // d'UIDVALIDITY passerait pour correct.
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();

    let first = {
        let tag = client.send("SELECT INBOX");
        uidvalidity(&client.collect(&tag))
    };
    let second = {
        let tag = client.send("SELECT INBOX");
        uidvalidity(&client.collect(&tag))
    };
    assert_eq!(first, second);
}

/// La valeur de `[UIDVALIDITY n]` dans une réponse de `SELECT`.
fn uidvalidity(pieces: &[Piece]) -> u32 {
    lines(pieces)
        .iter()
        .find_map(|line| {
            let inner = line.split_once("[UIDVALIDITY ")?.1;
            inner.split_once(']')?.0.parse().ok()
        })
        .expect("aucun UIDVALIDITY dans la réponse")
}

#[test]
fn a_server_may_announce_condstore_then_refuse_it() {
    // **Le test du chemin de repli.** Un repli qu'on n'exécute jamais n'existe pas.
    let config = Config::without_condstore().with_fault(Fault::AdvertisesCondstoreThenRefuses);
    let server = Server::start(config).expect("démarrage");
    let mut client = Client::connect(&server);

    let greeting = client.line().expect("salut");
    assert!(greeting.contains("CONDSTORE"), "l'annonce doit être là");
    client.login();

    let tag = client.send("ENABLE CONDSTORE");
    let reply = client.collect(&tag);
    assert!(has(&reply, "NO"), "{:?}", lines(&reply));

    let tag = client.send("SELECT INBOX");
    client.collect(&tag);
    let tag = client.send("UID FETCH 1:* (UID) (CHANGEDSINCE 1)");
    let reply = client.collect(&tag);
    assert!(has(&reply, "BAD"), "{:?}", lines(&reply));
}

#[test]
fn a_server_without_condstore_omits_highest_modseq() {
    // Le chemin de repli légitime : le serveur ne prétend rien, et le client doit s'en
    // sortir par plages d'UID.
    let server = Server::start(Config::without_condstore()).expect("démarrage");
    let mut client = Client::connect(&server);
    let greeting = client.line().expect("salut");
    assert!(!greeting.contains("CONDSTORE"));
    client.login();

    let tag = client.send("SELECT INBOX");
    let reply = client.collect(&tag);
    assert!(!has(&reply, "HIGHESTMODSEQ"), "{:?}", lines(&reply));
    assert!(has(&reply, "[UIDNEXT 4]"), "{:?}", lines(&reply));
}

#[test]
fn the_connection_can_be_cut_in_the_middle_of_a_response() {
    // Le câble débranché. La coupure tombe à l'intérieur du salut, donc pas à une frontière
    // propre : c'est ce que le budget d'octets achète.
    let config = Config::with_inbox().with_fault(Fault::ClosesAfter { after: 12 });
    let server = Server::start(config).expect("démarrage");
    let mut client = Client::connect(&server);

    let first = client.line();
    assert!(
        first.is_none_or(|it| it.len() <= 12),
        "le serveur a écrit plus que son budget"
    );
    assert!(client.line().is_none(), "la connexion devait être fermée");
}

#[test]
fn login_can_be_refused_outright() {
    let config = Config::with_inbox().with_fault(Fault::RefusesLogin);
    let server = Server::start(config).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();

    let reply = client.login();
    assert!(has(&reply, "NO"), "{:?}", lines(&reply));

    // Et l'état n'a pas changé : ce qui suit doit rester refusé.
    let tag = client.send("SELECT INBOX");
    let reply = client.collect(&tag);
    assert!(has(&reply, "BAD"), "{:?}", lines(&reply));
}

#[test]
fn a_sequence_number_may_point_outside_the_mailbox() {
    // Un client qui indexe par numéro de séquence écrira le message au mauvais endroit.
    // C'est la raison pour laquelle la synchronisation ne doit jamais s'y appuyer.
    let config = Config::with_inbox().with_fault(Fault::ImpossibleSequenceNumber);
    let server = Server::start(config).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();
    let tag = client.send("SELECT INBOX");
    client.collect(&tag);

    let tag = client.send("UID FETCH 1 (UID BODY[])");
    let reply = client.collect(&tag);
    let head = fetch_heads(&reply)
        .into_iter()
        .next()
        .expect("aucune réponse FETCH");
    let sequence: usize = head
        .split_whitespace()
        .nth(1)
        .and_then(|it| it.parse().ok())
        .expect("numéro de séquence");
    assert!(sequence > 3, "le numéro devait sortir de la boîte : {head}");
    // L'UID, lui, reste juste — c'est ce sur quoi le client doit s'appuyer.
    assert!(has(&reply, "UID 1"), "{:?}", lines(&reply));
}

#[test]
fn the_same_uid_may_arrive_twice_in_one_response() {
    // Vu chez des serveurs sous charge. Le client doit être idempotent, pas surpris.
    let config = Config::with_inbox().with_fault(Fault::DuplicateUid { uid: 2 });
    let server = Server::start(config).expect("démarrage");
    let mut client = Client::connect(&server);
    client.line();
    client.login();
    let tag = client.send("SELECT INBOX");
    client.collect(&tag);

    let tag = client.send("UID FETCH 1:* (UID BODY[])");
    let reply = client.collect(&tag);

    let twos = lines(&reply)
        .iter()
        .filter(|it| it.contains("UID 2 "))
        .count();
    assert_eq!(twos, 2, "{:?}", lines(&reply));
    assert_eq!(
        literals(&reply).len(),
        4,
        "quatre corps pour trois messages"
    );
}

#[test]
fn a_mailbox_name_that_is_not_valid_utf8_is_served_as_is() {
    // Le nom part en octets. Un serveur qui le ferait passer par une chaîne Rust le
    // remplacerait par des caractères de remplacement, ce qui désamorcerait le cas.
    let raw: Vec<u8> = vec![b'I', b'N', b'&', 0xFF, 0xFE, b'X'];
    let config = Config::with_inbox().with_mailboxes(vec![Mailbox {
        name: raw.clone(),
        ..Mailbox::new("x", 3, Vec::new())
    }]);
    let server = Server::start(config).expect("démarrage");

    // Lu en octets, sans passer par `Client` : tout l'intérêt est de ne pas convertir.
    let mut stream = TcpStream::connect(server.address()).expect("connexion");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("délai");
    let mut reader = BufReader::new(stream.try_clone().expect("dupliquer"));
    let mut greeting = Vec::new();
    reader.read_until(b'\n', &mut greeting).expect("salut");
    write!(stream, "a1 LOGIN marie@exemple.fr secret\r\n").expect("login");
    let mut ok = Vec::new();
    reader.read_until(b'\n', &mut ok).expect("réponse");
    write!(stream, "a2 LIST \"\" *\r\n").expect("list");
    let mut listing = Vec::new();
    reader.read_until(b'\n', &mut listing).expect("liste");

    let found = listing
        .windows(raw.len())
        .any(|window| window == raw.as_slice());
    assert!(
        found,
        "le nom n'est pas ressorti octet pour octet : {listing:?}"
    );
}

#[test]
fn two_clients_are_served_at_the_same_time() {
    // Servir en série est le défaut qui a bloqué le harnais du critère 8 pendant une
    // exécution entière : un client qui ouvre et attend retenait la boucle pour tout le
    // monde. Ici, le premier client ne finit jamais sa session.
    let server = Server::start(Config::with_inbox()).expect("démarrage");
    let mut idle = Client::connect(&server);
    idle.line();

    let mut busy = Client::connect(&server);
    busy.line();
    busy.login();
    let tag = busy.send("SELECT INBOX");
    let reply = busy.collect(&tag);
    assert!(has(&reply, "OK"), "le deuxième client a été bloqué");
}

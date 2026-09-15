//! Une connexion, du salut au `LOGOUT`.
//!
//! ## L'ordre des vérifications compte
//!
//! Une panne se provoque **au plus tard possible**. [`Fault::ClosesAfter`] est un budget
//! d'octets, donc elle peut tomber au milieu d'un littéral ; si on la testait avant d'écrire
//! une réponse, elle ne couperait jamais qu'aux frontières propres, c'est-à-dire là où un
//! client s'en sort trop facilement.
//!
//! ## Ce qui est refusé, et pourquoi c'est fidèle
//!
//! Une commande qui demande une boîte sélectionnée alors qu'aucune ne l'est rend `BAD`, pas un
//! résultat vide. Un serveur qui pardonne cache au client qu'il a émis ses commandes dans le
//! mauvais ordre, et c'est le genre de bug qui ne se manifeste que chez le fournisseur qui,
//! lui, ne pardonne pas.
//!
//! [`Fault::ClosesAfter`]: crate::Fault::ClosesAfter

use std::io;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use crate::wire::{Command, Done, Wire, parse, quote_bytes, unquote};
use crate::{Config, Fault, Mailbox, Message, State};

/// Le préfixe d'une demande de continuation, pour un test qui voudrait le reconnaître.
pub const CONTINUATION: &str = "+ ";

/// Ce que la session sait d'elle-même.
#[derive(Debug, Default)]
struct Session {
    authenticated: bool,
    /// Le rang de la boîte sélectionnée dans `Config::mailboxes`.
    selected: Option<usize>,
    /// Vrai après un `ENABLE CONDSTORE` accepté.
    condstore: bool,
    /// Vrai quand la boîte courante a été ouverte par `EXAMINE` et non `SELECT`.
    ///
    /// Un `UID STORE` y est refusé, et c'est ce qui rend le refus **observable** : un client
    /// qui pousserait un drapeau après un `EXAMINE` le découvre ici plutôt qu'en production.
    /// La phase 2 n'écrivait pas, la phase 3 pousse `Seen` — d'où ce champ.
    read_only: bool,
    /// Vrai après un `ENABLE QRESYNC` accepté.
    ///
    /// Sans lui, le paramètre `(QRESYNC …)` d'un `SELECT` doit être **refusé** : la RFC 7162
    /// §3.2.5 en fait une erreur de protocole, et un serveur qui l'accepterait quand même
    /// laisserait passer un client qui a oublié son `ENABLE` — celui-là échouerait chez un
    /// serveur conforme, en production.
    qresync: bool,
}

/// Sert une connexion jusqu'à sa fin.
///
/// # Errors
///
/// L'erreur d'`io` du socket. Un client qui raccroche au milieu est le comportement **attendu**
/// de plusieurs pannes : l'appelant trace, il ne signale pas.
pub(crate) fn serve(stream: TcpStream, state: &Arc<Mutex<State>>) -> io::Result<()> {
    let budget = {
        let guard = lock(state);
        guard
            .config
            .find(|it| matches!(it, Fault::ClosesAfter { .. }))
            .and_then(|it| match it {
                Fault::ClosesAfter { after } => Some(*after),
                _ => None,
            })
    };
    let mut wire = Wire::new(stream, budget)?;
    let mut session = Session::default();

    let greeting = {
        let guard = lock(state);
        format!("* OK [CAPABILITY {}] mailfake", capabilities(&guard.config))
    };
    if wire.line(&greeting)?.is_some() {
        return Ok(());
    }

    while let Some(line) = wire.read_line()? {
        let Some(command) = parse(&line) else {
            // Une ligne qu'on ne sait pas découper reçoit un `BAD` sans étiquette, comme un
            // vrai serveur. Ne rien répondre laisserait le client attendre pour toujours.
            if wire.line("* BAD ligne illisible")?.is_some() {
                return Ok(());
            }
            continue;
        };

        match dispatch(&command, &mut session, state, &mut wire)? {
            Flow::Continue => {}
            Flow::Stop => return Ok(()),
        }
    }
    Ok(())
}

/// La suite à donner après une commande.
enum Flow {
    Continue,
    Stop,
}

/// Prend le verrou de l'état partagé.
///
/// Un verrou empoisonné rend quand même l'état : un fil de session qui a paniqué ne doit pas
/// rendre le serveur de test inutilisable pour les tests suivants, il doit les laisser
/// échouer sur leur propre assertion.
fn lock(state: &Arc<Mutex<State>>) -> std::sync::MutexGuard<'_, State> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Les capacités annoncées.
///
/// `CONDSTORE` apparaît aussi quand [`Fault::AdvertisesCondstoreThenRefuses`] est demandée —
/// c'est tout l'intérêt de cette panne : l'annonce est vraie, la promesse ne l'est pas.
fn capabilities(config: &Config) -> String {
    let mut out = vec!["IMAP4rev1", "UIDPLUS"];
    if config.condstore || config.has(&Fault::AdvertisesCondstoreThenRefuses) {
        out.push("CONDSTORE");
        out.push("ENABLE");
    }
    // `QRESYNC` n'est jamais annoncé seul : la RFC 7162 §3.2 en fait une extension de
    // `CONDSTORE`, et un serveur qui l'annoncerait sans lui serait un serveur qu'aucun client
    // ne sait utiliser.
    if config.qresync && config.condstore {
        out.push("QRESYNC");
    }
    if config.access_token.is_some() {
        out.push("AUTH=XOAUTH2");
        // `LOGINDISABLED` comme Google : un serveur qui n'accepte que le jeton doit le dire,
        // sinon un client essaie `LOGIN` et reçoit un refus qui ressemble à un mauvais mot de
        // passe.
        out.push("LOGINDISABLED");
    }
    if config.sasl_ir {
        out.push("SASL-IR");
    }
    if config.idle || config.has(&Fault::AdvertisesIdleThenRefuses) {
        out.push("IDLE");
    }
    if config.list_status || config.has(&Fault::AdvertisesListStatusThenRefuses) {
        // `LIST-STATUS` est une option de `RETURN`, qui vient de `LIST-EXTENDED` : annoncer le
        // second sans le premier décrirait un serveur qui n'existe pas.
        out.push("LIST-EXTENDED");
        out.push("LIST-STATUS");
    }
    out.join(" ")
}

/// Exécute une commande.
fn dispatch(
    command: &Command,
    session: &mut Session,
    state: &Arc<Mutex<State>>,
    wire: &mut Wire,
) -> io::Result<Flow> {
    let tag = command.tag.as_str();

    match command.name.as_str() {
        "CAPABILITY" => {
            let list = {
                let guard = lock(state);
                capabilities(&guard.config)
            };
            emit(wire, &format!("* CAPABILITY {list}"))?;
            reply(wire, tag, "OK", "CAPABILITY terminé")
        }

        "NOOP" => reply(wire, tag, "OK", "NOOP terminé"),

        "LOGOUT" => {
            emit(wire, "* BYE au revoir")?;
            let _ = reply(wire, tag, "OK", "LOGOUT terminé")?;
            Ok(Flow::Stop)
        }

        "LOGIN" => login(command, session, state, wire),

        "AUTHENTICATE" => authenticate(command, session, state, wire),

        "ENABLE" => enable(command, session, state, wire),

        "IDLE" => idle(command, session, state, wire),

        "LIST" | "LSUB" => list(command, state, wire),

        "SELECT" | "EXAMINE" => select(command, session, state, wire),

        "FETCH" => reply(
            wire,
            tag,
            "BAD",
            "FETCH par numéro de séquence non servi : utiliser UID FETCH",
        ),

        "UID" => uid(command, session, state, wire),

        other => reply(wire, tag, "BAD", &format!("commande inconnue : {other}")),
    }
}

/// Écrit une ligne non étiquetée, et rend `Flow::Stop` si le budget est épuisé.
fn emit(wire: &mut Wire, line: &str) -> io::Result<Flow> {
    Ok(match wire.line(line)? {
        Some(Done::Cut) => Flow::Stop,
        None => Flow::Continue,
    })
}

/// Écrit une réponse étiquetée.
fn reply(wire: &mut Wire, tag: &str, status: &str, text: &str) -> io::Result<Flow> {
    emit(wire, &format!("{tag} {status} {text}"))
}

/// `LOGIN`.
///
/// Le mot de passe est comparé en entier, et l'échec ne dit pas lequel des deux champs est
/// faux — comme un vrai serveur. Un serveur de test plus bavard qu'un vrai apprendrait au
/// client à s'appuyer sur une information qu'il n'aura pas en production.
fn login(
    command: &Command,
    session: &mut Session,
    state: &Arc<Mutex<State>>,
    wire: &mut Wire,
) -> io::Result<Flow> {
    let tag = command.tag.as_str();
    let guard = lock(state);
    if guard.config.has(&Fault::RefusesLogin) {
        drop(guard);
        return reply(wire, tag, "NO", "authentification refusée");
    }
    // Un serveur qui n'accepte que le jeton annonce `LOGINDISABLED` et refuse `LOGIN` — c'est
    // ce que Google fait depuis qu'il a retiré l'authentification par mot de passe. Le refus
    // doit être distinguable d'un mauvais mot de passe, sinon un client cherche du mauvais
    // côté pendant une heure.
    if guard.config.access_token.is_some() {
        drop(guard);
        return reply(
            wire,
            tag,
            "NO",
            "[PRIVACYREQUIRED] LOGIN désactivé sur ce serveur",
        );
    }

    let [username, password] = command.args.as_slice() else {
        drop(guard);
        return reply(wire, tag, "BAD", "LOGIN attend deux arguments");
    };
    let ok =
        unquote(username) == guard.config.username && unquote(password) == guard.config.password;
    drop(guard);

    if ok {
        session.authenticated = true;
        reply(wire, tag, "OK", "LOGIN terminé")
    } else {
        reply(wire, tag, "NO", "identifiants refusés")
    }
}

/// `AUTHENTICATE XOAUTH2`, avec ou sans réponse initiale.
///
/// ## Le refus imite Google, et c'est le point
///
/// Un jeton refusé ne reçoit **pas** un `NO` tout de suite : le serveur envoie une
/// continuation `+ <base64>` portant un objet JSON, et il attend une **ligne vide** avant de
/// conclure. Un client qui ne renvoie pas cette ligne attend pour toujours.
///
/// C'est le genre de défaut qui ne se manifeste que le jour où un jeton expire — donc jamais
/// pendant le développement. Le reproduire ici est la seule façon de l'attraper avant.
fn authenticate(
    command: &Command,
    session: &mut Session,
    state: &Arc<Mutex<State>>,
    wire: &mut Wire,
) -> io::Result<Flow> {
    let tag = command.tag.as_str();
    let guard = lock(state);
    let expected = guard.config.access_token.clone();
    let username = guard.config.username.clone();
    let sasl_ir = guard.config.sasl_ir;
    drop(guard);

    let Some(expected) = expected else {
        return reply(
            wire,
            tag,
            "NO",
            "AUTHENTICATE non servi : ce serveur veut un LOGIN",
        );
    };
    let Some(mechanism) = command.args.first().map(|it| it.to_ascii_uppercase()) else {
        return reply(wire, tag, "BAD", "AUTHENTICATE attend un mécanisme");
    };
    if mechanism != "XOAUTH2" {
        return reply(wire, tag, "NO", &format!("mécanisme {mechanism} non servi"));
    }

    // La réponse initiale, ou une continuation pour la demander.
    let response = match command.args.get(1) {
        Some(response) if sasl_ir => response.clone(),
        Some(_) => {
            // Le client a envoyé une réponse initiale que le serveur n'a pas annoncé savoir
            // lire. Un vrai serveur répond `BAD` ; le simuler évite qu'un client s'appuie sur
            // une tolérance qu'il n'aura pas ailleurs.
            return reply(wire, tag, "BAD", "réponse initiale sans SASL-IR annoncé");
        }
        None => {
            // `+ ` seul : « envoyez la réponse ». Le vide après le `+` est ce que la RFC 3501
            // appelle un défi vide.
            if matches!(emit(wire, "+")?, Flow::Stop) {
                return Ok(Flow::Stop);
            }
            match wire.read_line()? {
                Some(line) => line,
                None => return Ok(Flow::Stop),
            }
        }
    };

    if credentials_match(&response, &username, &expected) {
        session.authenticated = true;
        return reply(wire, tag, "OK", "AUTHENTICATE terminé");
    }

    // **Le refus à la Google.** Un JSON en base64 dans une continuation, puis on attend une
    // ligne avant de conclure.
    let challenge =
        base64_encode(br#"{"status":"400","schemes":"Bearer","scope":"https://mail.google.com/"}"#);
    if matches!(emit(wire, &format!("+ {challenge}"))?, Flow::Stop) {
        return Ok(Flow::Stop);
    }
    if wire.read_line()?.is_none() {
        return Ok(Flow::Stop);
    }
    reply(wire, tag, "NO", "Invalid credentials (Failure)")
}

/// Vrai si la réponse SASL porte l'identifiant et le jeton attendus.
fn credentials_match(response: &str, username: &str, token: &str) -> bool {
    let Some(decoded) = base64_decode(response.trim()) else {
        return false;
    };
    let expected = format!("user={username}\u{1}auth=Bearer {token}\u{1}\u{1}");
    decoded == expected.into_bytes()
}

/// Base64 standard, avec bourrage.
///
/// Écrit ici plutôt que tiré d'un crate : le serveur de test n'a **aucune dépendance**, et
/// c'est ce qui garantit qu'il n'emprunte pas la même bibliothèque que le client. Un encodeur
/// partagé entre les deux côtés cacherait un bug commun — les deux se tromperaient pareil, et
/// le test passerait.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for index in 0..4 {
            #[allow(clippy::cast_possible_truncation)]
            let sextet = ((n >> (18 - index * 6)) & 0x3F) as usize;
            // Les positions au-delà des octets réels reçoivent le bourrage.
            if index <= chunk.len() {
                out.push(char::from(ALPHABET[sextet]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Base64 standard, décodé. `None` sur une entrée qui n'en est pas.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0_u32;
    let mut held = 0_u32;
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        held += 6;
        if held >= 8 {
            held -= 8;
            #[allow(clippy::cast_possible_truncation)]
            out.push(((bits >> held) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// `ENABLE CONDSTORE`.
fn enable(
    command: &Command,
    session: &mut Session,
    state: &Arc<Mutex<State>>,
    wire: &mut Wire,
) -> io::Result<Flow> {
    let tag = command.tag.as_str();
    if !session.authenticated {
        return reply(wire, tag, "BAD", "il faut s'authentifier d'abord");
    }

    let wants_condstore = command
        .args
        .iter()
        .any(|it| it.eq_ignore_ascii_case("CONDSTORE"));
    let wants_qresync = command
        .args
        .iter()
        .any(|it| it.eq_ignore_ascii_case("QRESYNC"));
    let guard = lock(state);
    let honours = guard.config.condstore;
    let qresync = guard.config.qresync && guard.config.condstore;
    let lies = guard.config.has(&Fault::AdvertisesCondstoreThenRefuses);
    drop(guard);

    if wants_qresync && qresync {
        // **`QRESYNC` active `CONDSTORE` avec lui** (RFC 7162 §3.2.3), et la réponse le dit :
        // un client qui ne verrait que `QRESYNC` dans la liste `ENABLED` pourrait croire qu'il
        // doit encore demander `CONDSTORE`, ce qui est hors protocole une fois une boîte
        // sélectionnée.
        session.condstore = true;
        session.qresync = true;
        emit(wire, "* ENABLED CONDSTORE QRESYNC")?;
        return reply(wire, tag, "OK", "ENABLE terminé");
    }
    if wants_condstore && lies {
        // **La panne du chemin de repli.** L'annonce était vraie, la promesse ne l'est pas.
        return reply(wire, tag, "NO", "CONDSTORE indisponible");
    }
    if wants_condstore && honours {
        session.condstore = true;
        emit(wire, "* ENABLED CONDSTORE")?;
        return reply(wire, tag, "OK", "ENABLE terminé");
    }
    // Une extension inconnue n'est pas une erreur : la RFC 5161 demande de rendre `OK` avec
    // une liste `ENABLED` vide.
    emit(wire, "* ENABLED")?;
    reply(wire, tag, "OK", "ENABLE terminé")
}

/// Le couple `(uidvalidity, modseq)` d'un paramètre `(QRESYNC (u m …))`.
///
/// ## L'analyse est volontairement grossière
///
/// Les arguments arrivent déjà découpés par espaces, et les parenthèses sont retirées à la
/// main. Un vrai serveur analyserait la liste ; ici, ce qui compte est que **le serveur de test
/// ne soit pas plus tolérant que le protocole** sur ce qui suit — un paramètre mal formé rend
/// `None`, donc un `EXAMINE` nu, donc pas de `VANISHED`. Un client qui envoie n'importe quoi ne
/// doit pas être récompensé.
///
/// Les deux paramètres facultatifs de la RFC 7162 — l'ensemble d'UID connus et la
/// correspondance de séquence — sont ignorés : le client ne les envoie pas, et un serveur de
/// test qui prétendrait les honorer mentirait.
fn qresync_param(args: &[String]) -> Option<(u32, u64)> {
    // Le découpage de `wire` garde un groupe parenthésé **en un seul argument** : tout
    // `(QRESYNC (1000 3))` arrive d'un bloc. La première version cherchait `QRESYNC` comme un
    // mot isolé et ne trouvait donc jamais rien — les tests de purge passaient à côté du
    // chemin qu'ils étaient censés éprouver, en constatant simplement qu'il ne s'était rien
    // passé.
    let group = args.iter().find(|it| {
        it.trim_start_matches('(')
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("QRESYNC"))
    })?;

    // Les deux premiers nombres du groupe sont l'`UIDVALIDITY` et le `MODSEQ`. Découper sur ce
    // qui n'est pas un chiffre évite de compter les parenthèses : le nom de l'extension n'en
    // contient aucun.
    let mut numbers = group
        .split(|c: char| !c.is_ascii_digit())
        .filter(|it| !it.is_empty());
    let uidvalidity = numbers.next()?.parse().ok()?;
    let modseq = numbers.next()?.parse().ok()?;
    Some((uidvalidity, modseq))
}

/// `LIST` et `LSUB`.
fn list(command: &Command, state: &Arc<Mutex<State>>, wire: &mut Wire) -> io::Result<Flow> {
    let tag = command.tag.as_str();
    let only_subscribed = command.name == "LSUB";

    let wanted = status_items(&command.args);
    let guard = lock(state);
    if wanted.is_some() {
        if !guard.config.list_status || guard.config.has(&Fault::AdvertisesListStatusThenRefuses) {
            drop(guard);
            return reply(wire, tag, "BAD", "RETURN (STATUS) inconnu");
        }
        // RFC 7162 §3.1.2 : `HIGHESTMODSEQ` n'existe que chez un serveur `CONDSTORE`. Le
        // refuser plutôt que de rendre zéro est ce qui apprend à un client à ne pas le
        // demander quand il n'a pas négocié l'extension.
        if !guard.config.condstore
            && wanted
                .as_ref()
                .is_some_and(|it| it.iter().any(|item| item == "HIGHESTMODSEQ"))
        {
            drop(guard);
            return reply(wire, tag, "BAD", "HIGHESTMODSEQ sans CONDSTORE");
        }
    }
    let partial = guard.config.has(&Fault::PartialListStatus);
    let condstore = guard.config.condstore;
    let rows: Vec<Row> = guard
        .config
        .mailboxes
        .iter()
        .filter(|it| !only_subscribed || it.subscribed)
        .map(Row::of)
        .collect();
    drop(guard);

    for (rank, row) in rows.into_iter().enumerate() {
        // Assemblé en octets : le nom peut ne pas être de l'UTF-8, et le faire passer par une
        // `String` remplacerait ses octets par des caractères de remplacement — ce qui
        // désamorcerait précisément le cas qu'on veut servir.
        let mut line = format!("* {} ({}) \"", command.name, row.attributes).into_bytes();
        line.push(row.delimiter);
        line.extend_from_slice(b"\" ");
        line.extend_from_slice(&quote_bytes(&row.name));
        line.extend_from_slice(b"\r\n");
        if wire.write(&line)?.is_some() {
            return Ok(Flow::Stop);
        }

        let Some(items) = wanted.as_ref() else {
            continue;
        };
        // Une boîte qu'on ne peut pas ouvrir n'a pas d'état : la RFC 5819 §2 dit de ne rien
        // rendre pour elle, et c'est ce que fait Gmail sur son nœud `[Gmail]`.
        if row.attributes.to_ascii_uppercase().contains("\\NOSELECT") {
            continue;
        }
        if partial && rank % 2 == 1 {
            continue;
        }

        let values: Vec<String> = items
            .iter()
            .filter_map(|item| match item.as_str() {
                "MESSAGES" => Some(format!("MESSAGES {}", row.count)),
                "UIDNEXT" => Some(format!("UIDNEXT {}", row.uidnext)),
                "UIDVALIDITY" => Some(format!("UIDVALIDITY {}", row.uidvalidity)),
                "HIGHESTMODSEQ" if condstore => Some(format!("HIGHESTMODSEQ {}", row.modseq)),
                _ => None,
            })
            .collect();
        let mut line = b"* STATUS ".to_vec();
        line.extend_from_slice(&quote_bytes(&row.name));
        line.extend_from_slice(format!(" ({})\r\n", values.join(" ")).as_bytes());
        if wire.write(&line)?.is_some() {
            return Ok(Flow::Stop);
        }
    }
    reply(wire, tag, "OK", &format!("{} terminé", command.name))
}

/// `IDLE` (RFC 2177).
///
/// ## Ce que ce serveur vérifie du client, et pourquoi
///
/// - **une boîte doit être sélectionnée.** `IDLE` ne rapporte que la boîte courante ;
///   l'accepter sans sélection laisserait passer un client qui a oublié son `EXAMINE` et qui
///   attendrait alors des événements qu'aucun serveur ne lui enverra jamais ;
/// - **la sortie est `DONE`, sans étiquette.** Un client qui l'étiquetterait recevrait ici un
///   `BAD` plutôt qu'un silence — le silence étant exactement ce qui rend ce bug-là
///   indébogable en production.
fn idle(
    command: &Command,
    session: &mut Session,
    state: &Arc<Mutex<State>>,
    wire: &mut Wire,
) -> io::Result<Flow> {
    let tag = command.tag.as_str();
    if !session.authenticated {
        return reply(wire, tag, "BAD", "il faut s'authentifier d'abord");
    }

    let (refuses, announces) = {
        let guard = lock(state);
        (
            !guard.config.idle || guard.config.has(&Fault::AdvertisesIdleThenRefuses),
            guard.config.idle_announces.clone(),
        )
    };
    if refuses {
        return reply(wire, tag, "BAD", "IDLE inconnu");
    }
    if session.selected.is_none() {
        return reply(wire, tag, "BAD", "IDLE demande une boîte sélectionnée");
    }

    if matches!(emit(wire, "+ idling")?, Flow::Stop) {
        return Ok(Flow::Stop);
    }
    // L'événement part tout de suite quand il est configuré. Le différer demanderait un fil de
    // plus ; ce que le client doit savoir faire — le lire, sortir, moissonner — n'en dépend pas.
    if let Some(line) = announces
        && matches!(emit(wire, &line)?, Flow::Stop)
    {
        return Ok(Flow::Stop);
    }

    // On attend `DONE`. Une autre ligne est une erreur de protocole du client, et le dire vaut
    // mieux que de l'ignorer : c'est un serveur de test, son travail est de faire échouer tôt.
    loop {
        let Some(line) = wire.read_line()? else {
            // Le client a raccroché en pleine attente. C'est le cas de la panne
            // `IdleStaysSilent` quand le test s'arrête : rien à signaler.
            return Ok(Flow::Stop);
        };
        if line.trim().eq_ignore_ascii_case("DONE") {
            return reply(wire, tag, "OK", "IDLE terminé");
        }
        if matches!(emit(wire, "* BAD seul DONE sort d'un IDLE")?, Flow::Stop) {
            return Ok(Flow::Stop);
        }
    }
}

/// Ce qu'il faut savoir d'une boîte pour l'annoncer, verrou relâché.
///
/// Le verrou est pris une fois et rendu avant d'écrire quoi que ce soit : écrire sur le socket
/// en le tenant bloquerait les autres connexions le temps d'un client lent, ce qu'un vrai
/// serveur ne fait pas.
struct Row {
    name: Vec<u8>,
    delimiter: u8,
    attributes: String,
    uidvalidity: u32,
    uidnext: u32,
    modseq: u64,
    count: usize,
}

impl Row {
    fn of(mailbox: &Mailbox) -> Self {
        Self {
            name: mailbox.name.clone(),
            delimiter: mailbox.delimiter,
            attributes: mailbox.attributes.join(" "),
            uidvalidity: mailbox.uidvalidity,
            uidnext: mailbox.uidnext(),
            modseq: mailbox.highest_modseq(),
            count: mailbox.messages.len(),
        }
    }
}

/// Les éléments d'un `RETURN (STATUS (…))`, ou `None` si la commande n'en demande pas.
///
/// Les arguments arrivent découpés par `wire::split`, qui garde les parenthèses groupées :
/// `RETURN` d'un côté, `(STATUS (MESSAGES UIDNEXT))` de l'autre.
fn status_items(args: &[String]) -> Option<Vec<String>> {
    let at = args
        .iter()
        .position(|it| it.eq_ignore_ascii_case("RETURN"))?;
    let group = args.get(at + 1)?.to_ascii_uppercase();
    let inner = group
        .strip_prefix('(')?
        .strip_suffix(')')?
        .trim()
        .to_owned();
    let inner = inner.strip_prefix("STATUS")?.trim_start();
    let inner = inner.strip_prefix('(')?.strip_suffix(')')?;
    Some(inner.split_whitespace().map(str::to_owned).collect())
}

/// `SELECT` et `EXAMINE`.
fn select(
    command: &Command,
    session: &mut Session,
    state: &Arc<Mutex<State>>,
    wire: &mut Wire,
) -> io::Result<Flow> {
    let tag = command.tag.as_str();
    if !session.authenticated {
        return reply(wire, tag, "BAD", "il faut s'authentifier d'abord");
    }
    let Some(wanted) = command.args.first().map(|it| unquote(it)) else {
        return reply(wire, tag, "BAD", "SELECT attend un nom de boîte");
    };

    let mut guard = lock(state);
    let found = guard
        .config
        .mailboxes
        .iter()
        .position(|it| it.name == wanted.as_bytes());
    let Some(index) = found else {
        drop(guard);
        return reply(wire, tag, "NO", "boîte inconnue");
    };

    // **La dérive d'UIDVALIDITY.** Elle est calculée à partir du nombre de `SELECT` servis,
    // donc deux `SELECT` de la même boîte dans la même session rendent deux valeurs — ce qui
    // est exactement ce qu'un client doit remarquer.
    guard.selects = guard.selects.saturating_add(1);
    let drift = if guard.config.has(&Fault::UidvalidityChangesOnSelect) {
        guard.selects
    } else {
        0
    };
    let mailbox = guard.config.mailboxes[index].clone();
    let condstore = guard.config.condstore;
    drop(guard);

    session.selected = Some(index);
    // Le verbe décide du droit d'écrire. `command.name` est déjà en majuscules.
    session.read_only = command.name.eq_ignore_ascii_case("EXAMINE");
    let uidvalidity = mailbox.uidvalidity.wrapping_add(drift);

    let mut lines = vec![
        format!("* {} EXISTS", mailbox.messages.len()),
        "* 0 RECENT".to_owned(),
        "* FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)".to_owned(),
        format!("* OK [UIDVALIDITY {uidvalidity}] valide"),
        format!("* OK [UIDNEXT {}] suivant", mailbox.uidnext()),
    ];
    if condstore {
        lines.push(format!(
            "* OK [HIGHESTMODSEQ {}] modseq",
            mailbox.highest_modseq()
        ));
    }

    // **Le paramètre `QRESYNC`.** `EXAMINE nom (QRESYNC (uidvalidity modseq))`.
    match qresync_param(&command.args) {
        Some(_) if !session.qresync => {
            // RFC 7162 §3.2.5 : sans `ENABLE QRESYNC`, le paramètre est une erreur. Le refuser
            // est ce qui empêche un client d'oublier son `ENABLE` et de ne le découvrir que
            // chez un serveur conforme, en production.
            return reply(wire, tag, "BAD", "QRESYNC demandé sans ENABLE");
        }
        // Un `UIDVALIDITY` qui ne correspond plus : le serveur **ignore** le paramètre et
        // répond comme à un `EXAMINE` nu. C'est ce que dit la RFC, et c'est le cas que le
        // client doit savoir reconnaître — sinon il conclurait « rien n'a disparu » sur une
        // boîte entièrement renumérotée. Cette branche l'exprime en refusant de filer.
        Some((asked_validity, since)) if asked_validity == uidvalidity => {
            let gone: Vec<String> = mailbox
                .expunged
                .iter()
                .filter(|&&(_, modseq)| modseq > since)
                .map(|&(uid, _)| uid.to_string())
                .collect();
            if !gone.is_empty() {
                lines.push(format!("* VANISHED (EARLIER) {}", gone.join(",")));
            }
            for (rank, message) in mailbox.messages.iter().enumerate() {
                if message.modseq > since {
                    lines.push(format!(
                        "* {} FETCH (UID {} FLAGS ({}) MODSEQ ({}))",
                        rank + 1,
                        message.uid,
                        message.flags.join(" "),
                        message.modseq
                    ));
                }
            }
        }
        Some(_) | None => {}
    }
    for line in lines {
        if matches!(emit(wire, &line)?, Flow::Stop) {
            return Ok(Flow::Stop);
        }
    }
    // `READ-ONLY` même sur `SELECT` : ce serveur n'écrit rien, et l'annoncer est plus honnête
    // que d'accepter un `STORE` qu'il ignorerait.
    reply(wire, tag, "OK", "[READ-ONLY] SELECT terminé")
}

/// `UID STORE` — la seule écriture que ce serveur serve.
///
/// ## Pourquoi il existe alors que la documentation du crate disait « ni `STORE` »
///
/// Elle le disait parce que la phase 2 n'écrivait pas côté serveur. La phase 3 pousse `\Seen`
/// quand l'utilisateur ouvre un message, et un serveur de test qui refuse la commande rend le
/// test de cette poussée impossible à écrire — donc la poussée non vérifiée.
///
/// **Toujours pas d'`APPEND` ni d'`EXPUNGE`** : rien dans le client ne les émet.
///
/// ## Il refuse après un `EXAMINE`, et c'est le point
///
/// Un vrai serveur refuse une écriture sur une boîte ouverte en lecture seule. Le reproduire
/// est ce qui rend le refus observable : un client qui pousserait un drapeau après un `EXAMINE`
/// le découvre ici, et non en production.
///
/// ## Il n'honore que `+FLAGS`, et ignore `FLAGS` et `-FLAGS`
///
/// Le client n'émet que `+FLAGS.SILENT (\Seen)`. Servir `FLAGS` — qui **remplace** la liste —
/// demanderait de choisir un comportement pour une commande que personne n'envoie, et le jour
/// où quelqu'un l'enverrait, un serveur de test qui l'accepte silencieusement serait pire qu'un
/// serveur qui la refuse.
fn uid_store(
    command: &Command,
    session: &Session,
    state: &Arc<Mutex<State>>,
    wire: &mut Wire,
) -> io::Result<Flow> {
    let tag = command.tag.as_str();
    let Some(index) = session.selected else {
        return reply(wire, tag, "BAD", "aucune boîte sélectionnée");
    };
    if session.read_only {
        return reply(
            wire,
            tag,
            "NO",
            "[READ-ONLY] boîte ouverte en lecture seule : utiliser SELECT",
        );
    }
    let Some(range) = command.args.get(1) else {
        return reply(wire, tag, "BAD", "UID STORE attend un ensemble d'UID");
    };
    let Some(item) = command.args.get(2).map(|it| it.to_ascii_uppercase()) else {
        return reply(wire, tag, "BAD", "UID STORE attend un élément");
    };
    if !item.starts_with("+FLAGS") {
        return reply(
            wire,
            tag,
            "BAD",
            &format!("UID STORE {item} non servi : seul +FLAGS l'est"),
        );
    }

    // Les drapeaux demandés : tout ce qui suit l'élément, parenthèses retirées. Le client
    // envoie `(\Seen)`, mais un ensemble de plusieurs doit passer aussi — sinon le test ne
    // dirait rien du jour où le client en poussera deux.
    let wanted: Vec<String> = command
        .args
        .iter()
        .skip(3)
        .map(|it| it.trim_matches(['(', ')']).to_owned())
        .filter(|it| !it.is_empty())
        .collect();

    let mut guard = lock(state);
    let mailbox = guard.config.mailboxes[index].clone();
    let uids = uid_set(range, &mailbox);
    let mut touched = 0_usize;
    for uid in uids {
        let Some(message) = guard.config.mailboxes[index]
            .messages
            .iter_mut()
            .find(|it| it.uid == uid)
        else {
            continue;
        };
        for flag in &wanted {
            // `+FLAGS` **ajoute** : un drapeau déjà là n'est pas dupliqué, et les autres sont
            // conservés. `FLAGS` remplacerait, ce qui effacerait `\Flagged` — la différence
            // est d'un caractère et elle détruit des données.
            if !message.flags.iter().any(|it| it.eq_ignore_ascii_case(flag)) {
                message.flags.push(flag.clone());
            }
        }
        touched += 1;
    }
    drop(guard);

    // `.SILENT` demandé : aucune réponse `FETCH` de confirmation. Sans `.SILENT`, un vrai
    // serveur en renvoie une par message — mais le client demande le silence, donc le lui
    // servir est ce qui reproduit son cas.
    reply(wire, tag, "OK", &format!("{touched} messages marqués"))
}

/// `UID FETCH`, et rien d'autre pour l'instant.
fn uid(
    command: &Command,
    session: &Session,
    state: &Arc<Mutex<State>>,
    wire: &mut Wire,
) -> io::Result<Flow> {
    let tag = command.tag.as_str();
    let Some(sub) = command.args.first().map(|it| it.to_ascii_uppercase()) else {
        return reply(wire, tag, "BAD", "UID attend une sous-commande");
    };
    if sub == "STORE" {
        return uid_store(command, session, state, wire);
    }
    if sub != "FETCH" {
        return reply(wire, tag, "BAD", &format!("UID {sub} non servi"));
    }
    let Some(index) = session.selected else {
        return reply(wire, tag, "BAD", "aucune boîte sélectionnée");
    };

    let guard = lock(state);
    let mailbox = guard.config.mailboxes[index].clone();
    let config = guard.config.clone();
    drop(guard);

    // `CHANGEDSINCE` sur un serveur qui a menti sur `CONDSTORE`.
    let changed_since = command.args.iter().find_map(|it| parse_changed_since(it));
    if changed_since.is_some() && config.has(&Fault::AdvertisesCondstoreThenRefuses) {
        return reply(wire, tag, "BAD", "CHANGEDSINCE non servi");
    }

    let Some(range) = command.args.get(1) else {
        return reply(wire, tag, "BAD", "UID FETCH attend un ensemble d'UID");
    };
    // **Les éléments demandés décident de la réponse.** La liste est le dernier argument
    // parenthésé qui n'est pas un modificateur — `(UID FLAGS BODY[])`, et non le
    // `(CHANGEDSINCE n)` qui peut le suivre.
    let wants_body = command
        .args
        .iter()
        .skip(2)
        .any(|it| it.to_ascii_uppercase().contains("BODY"));
    let wanted = uid_set(range, &mailbox);

    for uid in wanted {
        let Some(message) = mailbox.messages.iter().find(|it| it.uid == uid) else {
            continue;
        };
        if changed_since.is_some_and(|since| message.modseq <= since) {
            continue;
        }
        let sequence = position(&mailbox, uid, &config);
        if matches!(
            fetch_one(
                wire,
                sequence,
                message,
                &config,
                config.condstore,
                wants_body
            )?,
            Flow::Stop
        ) {
            return Ok(Flow::Stop);
        }
        if let Some(Fault::DuplicateUid { uid: doubled }) = config
            .find(|it| matches!(it, Fault::DuplicateUid { .. }))
            .filter(|it| matches!(it, Fault::DuplicateUid { uid: d } if *d == uid))
        {
            debug_assert_eq!(*doubled, uid);
            if matches!(
                fetch_one(
                    wire,
                    sequence,
                    message,
                    &config,
                    config.condstore,
                    wants_body
                )?,
                Flow::Stop
            ) {
                return Ok(Flow::Stop);
            }
        }
    }
    reply(wire, tag, "OK", "UID FETCH terminé")
}

/// Le numéro de séquence annoncé pour un UID.
///
/// Faux exprès sous [`Fault::ImpossibleSequenceNumber`] : un client qui indexe par numéro de
/// séquence plutôt que par UID écrira le message au mauvais endroit, et c'est précisément la
/// raison pour laquelle la synchronisation ne doit jamais s'y appuyer.
fn position(mailbox: &Mailbox, uid: u32, config: &Config) -> usize {
    if config.has(&Fault::ImpossibleSequenceNumber) {
        return mailbox.messages.len() + 9000;
    }
    mailbox
        .messages
        .iter()
        .position(|it| it.uid == uid)
        .map_or(1, |it| it + 1)
}

/// Écrit une réponse `FETCH` pour un message.
///
/// C'est ici que les deux pannes de littéral vivent, et elles sont volontairement écrites au
/// plus près de l'octet : la longueur annoncée et les octets envoyés sont deux valeurs
/// distinctes, ce qu'une bibliothèque correcte ne permettrait pas d'exprimer.
fn fetch_one(
    wire: &mut Wire,
    sequence: usize,
    message: &Message,
    config: &Config,
    with_modseq: bool,
    wants_body: bool,
) -> io::Result<Flow> {
    let truncate = config
        .find(|it| matches!(it, Fault::TruncatedLiteral { uid, .. } if *uid == message.uid))
        .and_then(|it| match it {
            Fault::TruncatedLiteral { sent, .. } => Some(*sent),
            _ => None,
        });
    let delta = config
        .find(|it| matches!(it, Fault::LiteralLengthMismatch { uid, .. } if *uid == message.uid))
        .and_then(|it| match it {
            Fault::LiteralLengthMismatch { delta, .. } => Some(*delta),
            _ => None,
        })
        .unwrap_or(0);

    let real = message.body.len();
    // La longueur **annoncée**. Saturée pour que `delta` ne puisse pas déborder : un test qui
    // demande un écart absurde doit produire une réponse absurde, pas une panique dans le
    // serveur.
    let declared = i64::try_from(real)
        .unwrap_or(i64::MAX)
        .saturating_add(delta)
        .max(0);

    let flags = message.flags.join(" ");
    let modseq = if with_modseq {
        format!(" MODSEQ ({})", message.modseq)
    } else {
        String::new()
    };
    // `RFC822.SIZE` porte la taille **réelle**, pas celle qu'un
    // [`Fault::LiteralLengthMismatch`] fait annoncer : cette panne-là porte sur la longueur du
    // littéral, et un serveur qui mentirait sur les deux à la fois serait une autre panne.
    //
    // Le client s'en sert pour borner un lot de corps en octets. Un serveur de test qui ne le
    // rendrait pas ferait passer tous ses tests par le chemin « taille inconnue », c'est-à-dire
    // par l'estimation — et le chemin réel, celui que tous les serveurs du corpus empruntent,
    // ne serait couvert nulle part.
    //
    // **Le corps ne part que s'il a été demandé.** La première version l'envoyait toujours, ce
    // qui rendait ce serveur plus généreux qu'un vrai : un client qui demanderait `(UID)` et
    // s'appuierait quand même sur le corps aurait passé tous ses tests ici et échoué en
    // production. Le `UID FETCH n:* (UID RFC822.SIZE)` de la découverte transférait aussi tous
    // les corps du dossier pour rien, ce qui rendait les tests plus lents et moins fidèles à la
    // fois.
    let head = if wants_body {
        format!(
            "* {sequence} FETCH (UID {} RFC822.SIZE {real} FLAGS ({flags}){modseq} \
             BODY[] {{{declared}}}\r\n",
            message.uid
        )
    } else {
        format!(
            "* {sequence} FETCH (UID {} RFC822.SIZE {real} FLAGS ({flags}){modseq})\r\n",
            message.uid
        )
    };
    if wire.write(head.as_bytes())?.is_some() {
        return Ok(Flow::Stop);
    }
    if !wants_body {
        return Ok(Flow::Continue);
    }

    match truncate {
        // Tronqué **puis raccroché** : c'est la panne complète. Envoyer moins d'octets sans
        // fermer laisserait le client attendre la suite, ce qui est un autre cas — celui de
        // `ClosesAfter`.
        Some(sent) => {
            let end = sent.min(real);
            if wire.write(&message.body[..end])?.is_some() {
                return Ok(Flow::Stop);
            }
            Ok(Flow::Stop)
        }
        None => {
            if wire.write(&message.body)?.is_some() {
                return Ok(Flow::Stop);
            }
            emit(wire, ")")
        }
    }
}

/// Lit `(CHANGEDSINCE n)`.
fn parse_changed_since(word: &str) -> Option<u64> {
    let inner = word.strip_prefix('(')?.strip_suffix(')')?;
    let mut parts = inner.split_whitespace();
    let name = parts.next()?;
    if !name.eq_ignore_ascii_case("CHANGEDSINCE") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// Développe un ensemble d'UID : `1`, `1:5`, `1:*`, `1,3,7`.
///
/// ## Le cas `*` n'est pas une commodité
///
/// `1:*` est la forme que tout client émet, et `*` veut dire « le plus grand UID de la
/// boîte », pas « `u32::MAX` ». Développer jusqu'à `u32::MAX` produirait quatre milliards
/// d'itérations dans un serveur de test — le genre de détail qui fait passer un test de
/// quelques millisecondes à jamais.
fn uid_set(range: &str, mailbox: &Mailbox) -> Vec<u32> {
    let highest = mailbox.messages.iter().map(|it| it.uid).max().unwrap_or(0);
    let mut out = Vec::new();
    for part in range.split(',') {
        match part.split_once(':') {
            Some((low, high)) => {
                let low = bound(low, highest);
                let high = bound(high, highest);
                let (low, high) = if low <= high {
                    (low, high)
                } else {
                    (high, low)
                };
                // Bornée aux UID **existants** plutôt qu'à la plage demandée : `1:*` sur une
                // boîte de trois messages ne doit pas parcourir un intervalle arbitraire.
                out.extend(
                    mailbox
                        .messages
                        .iter()
                        .map(|it| it.uid)
                        .filter(|it| *it >= low && *it <= high),
                );
            }
            None => {
                let single = bound(part, highest);
                if single != 0 {
                    out.push(single);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Une borne d'ensemble d'UID. `*` est le plus grand UID présent.
fn bound(word: &str, highest: u32) -> u32 {
    if word.trim() == "*" {
        return highest;
    }
    word.trim().parse().unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn inbox() -> Mailbox {
        Mailbox::new(
            "INBOX",
            1,
            vec![
                Message::simple(2, "a"),
                Message::simple(5, "b"),
                Message::simple(9, "c"),
            ],
        )
    }

    #[test]
    fn a_single_uid() {
        assert_eq!(uid_set("5", &inbox()), vec![5]);
    }

    #[test]
    fn a_range_keeps_only_existing_uids() {
        assert_eq!(uid_set("1:6", &inbox()), vec![2, 5]);
    }

    #[test]
    fn a_star_range_stops_at_the_highest_uid() {
        // Développer jusqu'à `u32::MAX` ferait boucler le serveur de test quatre milliards
        // de fois pour trois messages.
        assert_eq!(uid_set("1:*", &inbox()), vec![2, 5, 9]);
    }

    #[test]
    fn a_reversed_range_is_read_the_right_way_round() {
        // La RFC 3501 dit qu'un ensemble n'est pas ordonné : `9:1` vaut `1:9`.
        assert_eq!(uid_set("9:1", &inbox()), vec![2, 5, 9]);
    }

    #[test]
    fn a_comma_list_is_sorted_and_deduplicated() {
        assert_eq!(uid_set("9,2,9", &inbox()), vec![2, 9]);
    }

    #[test]
    fn a_malformed_set_yields_nothing_rather_than_panicking() {
        // L'entrée vient du client : elle est hostile par principe.
        assert!(uid_set("", &inbox()).is_empty());
        assert!(uid_set("abc", &inbox()).is_empty());
        assert!(uid_set(":", &inbox()).is_empty());
        assert!(uid_set("99999999999999999999", &inbox()).is_empty());
    }

    #[test]
    fn changed_since_is_read_case_insensitively() {
        assert_eq!(parse_changed_since("(CHANGEDSINCE 42)"), Some(42));
        assert_eq!(parse_changed_since("(changedsince 42)"), Some(42));
        assert_eq!(parse_changed_since("(FLAGS UID)"), None);
        assert_eq!(parse_changed_since("CHANGEDSINCE 42"), None);
        assert_eq!(parse_changed_since("(CHANGEDSINCE)"), None);
    }

    #[test]
    fn capabilities_announce_condstore_even_when_the_server_will_refuse_it() {
        // C'est la définition de la panne : l'annonce est vraie, la promesse ne l'est pas.
        let config = Config::without_condstore().with_fault(Fault::AdvertisesCondstoreThenRefuses);
        assert!(capabilities(&config).contains("CONDSTORE"));
    }

    #[test]
    fn a_server_without_condstore_does_not_announce_it() {
        assert!(!capabilities(&Config::without_condstore()).contains("CONDSTORE"));
    }

    #[test]
    fn an_impossible_sequence_number_is_out_of_the_mailbox() {
        let mailbox = inbox();
        let config = Config::with_inbox().with_fault(Fault::ImpossibleSequenceNumber);
        assert!(position(&mailbox, 5, &config) > mailbox.messages.len());
    }
}

#[cfg(test)]
mod qresync_param_tests {
    use super::qresync_param;

    /// Les arguments tels que `wire::split` les rend, pas tels qu'on les imagine.
    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|it| (*it).to_owned()).collect()
    }

    #[test]
    fn the_whole_group_arrives_as_one_argument() {
        // **Le cas réel, et celui qui a manqué au premier jet.** Le découpage garde un groupe
        // parenthésé d'un bloc ; chercher `QRESYNC` comme mot isolé ne trouvait rien, et les
        // tests de purge constataient sereinement qu'il ne se passait rien.
        assert_eq!(
            qresync_param(&args(&["\"INBOX\"", "(QRESYNC (1000 3))"])),
            Some((1000, 3))
        );
    }

    #[test]
    fn the_case_of_the_keyword_is_not_guaranteed() {
        assert_eq!(
            qresync_param(&args(&["\"INBOX\"", "(qresync (7 42))"])),
            Some((7, 42))
        );
    }

    #[test]
    fn an_examine_without_the_parameter_yields_nothing() {
        assert_eq!(qresync_param(&args(&["\"INBOX\""])), None);
        assert_eq!(qresync_param(&args(&[])), None);
        // Un autre paramètre parenthésé ne doit pas être pris pour celui-là.
        assert_eq!(qresync_param(&args(&["\"INBOX\"", "(CONDSTORE)"])), None);
    }

    #[test]
    fn an_incomplete_or_malformed_group_yields_nothing() {
        // Rendre `None` fait répondre comme à un `EXAMINE` nu : pas de `VANISHED`, donc pas de
        // purge inventée. C'est le sens sûr de l'erreur.
        assert_eq!(qresync_param(&args(&["(QRESYNC)"])), None);
        assert_eq!(qresync_param(&args(&["(QRESYNC (1000))"])), None);
        assert_eq!(qresync_param(&args(&["(QRESYNC (a b))"])), None);
        assert_eq!(qresync_param(&args(&["(QRESYNC ())"])), None);
    }

    #[test]
    fn the_optional_arguments_of_the_rfc_are_ignored_not_refused() {
        // La RFC 7162 autorise un ensemble d'UID connus et une correspondance de séquence
        // après le `MODSEQ`. Le client n'en envoie pas, mais les refuser rendrait ce serveur
        // plus strict que le protocole.
        assert_eq!(
            qresync_param(&args(&["(QRESYNC (1000 3 1:9))"])),
            Some((1000, 3))
        );
    }

    #[test]
    fn parsing_never_panics_whatever_arrives() {
        for candidate in [
            "(",
            ")",
            "(QRESYNC",
            "((((",
            "(QRESYNC (999999999999 1))",
            "()",
        ] {
            let _ = qresync_param(&args(&[candidate]));
        }
    }
}

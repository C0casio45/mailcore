//! **Le critère 8 de `docs/PHASE-3.md`, provoqué et non raisonné** : un envoi refusé, et
//! l'utilisateur voit *quoi faire*.
//!
//! ## Ce que ce fichier prouve, et ce qu'il ne prouve pas
//!
//! Le critère demande qu'un refus soit **actionnable** : « quota, destinataire refusé, message
//! trop gros, authentification. Pas un code numérique ». Les familles existaient déjà dans le
//! code, mais aucune n'avait jamais été **provoquée** : un test qui fabrique un `Err` à la main
//! valide la mise en forme d'une phrase, et rien du chemin qui y mène.
//!
//! Ici un serveur répond vraiment, à l'étape qui compte, avec le code qui compte. La chaîne
//! entière passe : le dialogue SMTP, la file d'envoi, l'état écrit dans le store, et la phrase
//! que l'utilisateur lira. Chaque famille du critère a son test, et le test dit **ce que
//! l'utilisateur doit pouvoir en tirer** — pas seulement que la phrase existe.
//!
//! Ce que ça ne prouve pas : qu'un **vrai** serveur — Gmail, Exchange, Postfix — rende ces codes
//! à ces étapes. Un serveur scripté dit ce qu'on lui dit de dire. Ce qui rend la classification
//! défendable malgré ça est qu'elle ne lit que le **code** et l'**étape**, tous deux normalisés
//! par la RFC 5321 ; le texte, écrit par chaque administrateur dans sa langue, n'entre pas dans
//! la décision. Le dernier mot du critère reste un refus obtenu d'un vrai serveur.
//!
//! ## Pourquoi un serveur scripté ici aussi
//!
//! Provoquer un vrai quota demande de remplir la boîte de quelqu'un ; un vrai « trop gros »
//! demande d'expédier 30 Mo à un tiers ; un vrai refus d'authentification demande de casser un
//! compte, ce qui le fait bloquer chez le fournisseur. Aucun des trois n'a sa place dans une
//! suite qui tourne à chaque `cargo test`, et deux d'entre eux dérangeraient quelqu'un.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

use mailcore::{AccountKind, Outgoing, SendState, Store};
use mailsmtp::client::Credential;
use mailsmtp::queue::{Outcome, Transport, deliver_one};
use mailsmtp::{Client, Error, Refusal, Result, Stage};

/// Où le serveur refuse, et avec quoi.
#[derive(Debug, Clone, Copy)]
struct Script {
    /// La commande après laquelle le refus tombe : `MAIL FROM`, `RCPT TO`, `DATA` ou `AUTH`.
    /// Vide pour un serveur qui n'oppose aucun refus.
    at: &'static str,
    /// La réponse complète, code et texte, sans le `\r\n`.
    reply: &'static str,
    /// La limite annoncée à l'`EHLO`. Zéro veut dire « pas de limite » — c'est aussi ce que
    /// répond un serveur qui gère `SIZE` sans plafond, et `Client::size_limit` l'écarte pour ça.
    size: u64,
}

impl Script {
    const fn refusing(at: &'static str, reply: &'static str) -> Self {
        Self { at, reply, size: 0 }
    }

    /// Un serveur qui ne refuse rien.
    const fn accepting() -> Self {
        Self {
            at: "",
            reply: "",
            size: 0,
        }
    }
}

// --------------------------------------------------------------------------------------------
// Le serveur scripté
// --------------------------------------------------------------------------------------------

/// Un serveur qui accepte tout, sauf ce que le script refuse.
struct Fake {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Fake {
    fn start(script: Script) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                serve(stream, script);
            }
        });
        Self {
            port,
            handle: Some(handle),
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        // Le serveur sort de lui-même quand le client raccroche ; on l'attend pour qu'aucun fil
        // ne survive au test qui l'a lancé.
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Le dialogue.
fn serve(stream: TcpStream, script: Script) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut out = stream.try_clone().unwrap();
    let mut lines = BufReader::new(stream).lines();

    let _ = out.write_all(b"220 refuseur pret\r\n");
    let _ = out.flush();

    while let Some(Ok(line)) = lines.next() {
        let upper = line.to_ascii_uppercase();
        // Le refus tombe après la commande que le script nomme, et un `at` vide ne nomme rien —
        // sans ce garde-fou, `starts_with("")` refuserait la première commande venue et le
        // contrôle positif deviendrait un test de plus qui passe pour la mauvaise raison.
        let refuse = !script.at.is_empty() && upper.starts_with(script.at);
        let reply = format!("{}\r\n", script.reply);

        if upper.starts_with("EHLO") {
            // `AUTH PLAIN` est annoncé pour que le client tente de s'authentifier : sans
            // l'annonce il refuse de lui-même, et on mesurerait alors sa prudence au lieu du
            // refus du serveur.
            let _ = out.write_all(b"250-refuseur\r\n250-AUTH PLAIN\r\n");
            let _ = out.write_all(format!("250 SIZE {}\r\n", script.size).as_bytes());
        } else if upper.starts_with("AUTH") {
            let _ = out.write_all(if refuse {
                reply.as_bytes()
            } else {
                b"235 ok\r\n"
            });
        } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
            let _ = out.write_all(if refuse {
                reply.as_bytes()
            } else {
                b"250 ok\r\n"
            });
        } else if upper == "DATA" {
            if refuse {
                // Un refus **avant** le `354` : le message n'a jamais commencé à partir. C'est
                // ce que fait un serveur qui refuse la taille annoncée par `SIZE=` sans l'avoir
                // publiée à l'`EHLO`, et c'est le meilleur cas pour l'utilisateur — le refus ne
                // coûte pas le téléversement.
                let _ = out.write_all(reply.as_bytes());
            } else {
                let _ = out.write_all(b"354 vas-y\r\n");
                let _ = out.flush();
                for raw in lines.by_ref() {
                    let Ok(raw) = raw else { return };
                    if raw == "." {
                        break;
                    }
                }
                let _ = out.write_all(b"250 recu\r\n");
            }
        } else if upper.starts_with("QUIT") {
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
// Le transport, et la file
// --------------------------------------------------------------------------------------------

/// Le vrai client, sur le port du serveur scripté.
///
/// Le dialogue est celui du transport de production, `AUTH` compris : c'est la seule façon de
/// faire tomber un refus d'authentification par le chemin qui le rendra en vrai.
struct Wired {
    port: u16,
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
                stage: Stage::Greeting,
                source,
            })?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .ok();
        let mut client = Client::greet(stream)?;
        client.ehlo("exemple.fr")?;
        client.auth("compte", Credential::Password("mot-de-passe"))?;
        client.mail_from(&job.sender, Some(job.size).filter(|it| *it > 0))?;
        for recipient in &job.recipients {
            client.rcpt_to(recipient)?;
        }
        client.open_data()?;
        frontier()?;
        client.finish_data_from(body)?;
        client.quit();
        Ok(())
    }
}

/// Un store neuf avec un message en file, prêt à être refusé.
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
    let raw = b"From: marie@exemple.fr\r\nTo: jean@ailleurs.fr\r\nSubject: essai\r\n\r\ncorps\r\n";
    let blob = store.blobs().put(raw).unwrap().hash;
    let id = store
        .enqueue(
            account,
            blob,
            "marie@exemple.fr",
            &["jean@ailleurs.fr".to_owned()],
            raw.len() as u64,
            1_000,
            None,
        )
        .unwrap();
    (store, id)
}

/// Ce qu'un refus laisse derrière lui.
struct Left {
    /// Le verdict de la file.
    outcome: Outcome,
    /// L'état écrit dans le store.
    state: SendState,
    /// **La phrase que l'utilisateur lit.**
    message: String,
    /// Le nombre de tentatives consignées : un refus doit en compter une.
    attempts: u32,
}

/// Provoque un refus de bout en bout, et rend ce qu'il a laissé.
fn provoke(script: Script) -> Left {
    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8Path::from_path(dir.path()).unwrap();
    let (store, id) = queued(root);
    let server = Fake::start(script);

    let pending = store.deliverable(i64::MAX, 10).unwrap();
    assert_eq!(pending.len(), 1, "le message devait être remettable");
    let outcome = deliver_one(&store, &mut Wired { port: server.port }, &pending[0], 2_000)
        .expect("la file elle-même n'avait pas à échouer");

    let line = store
        .outgoing(id)
        .unwrap()
        .expect("la ligne de file doit survivre au refus");
    Left {
        outcome,
        state: line.state,
        message: line.last_error.unwrap_or_default(),
        attempts: line.attempts,
    }
}

// --------------------------------------------------------------------------------------------
// Une famille, un test
// --------------------------------------------------------------------------------------------

#[test]
fn a_refused_recipient_says_to_check_the_address() {
    // `550` à l'étape d'un destinataire : l'adresse n'existe pas. C'est le refus le plus
    // fréquent, et le seul que l'utilisateur peut corriger tout de suite.
    let left = provoke(Script::refusing("RCPT TO", "550 5.1.1 No such user here"));

    assert_eq!(left.outcome, Outcome::Failed);
    assert_eq!(left.state, SendState::Failed);
    assert_eq!(left.attempts, 1, "un refus est une tentative");
    assert!(left.message.contains("destinataire"), "{}", left.message);
    assert!(
        left.message.contains("Vérifiez l'adresse"),
        "{}",
        left.message
    );
    // **Le code n'est pas ce que l'utilisateur lit en premier.** Il reste dans la phrase, à la
    // fin, pour qui enquête — mais la première chose dite est quoi faire. C'est le contrôle qui
    // attraperait un retour à `failure.to_string()`, dont la phrase ouvre sur « refus ».
    assert!(
        !left.message.starts_with("refus") && !left.message.starts_with("550"),
        "la phrase s'ouvre sur un code ou du jargon : {}",
        left.message
    );
    assert!(!Refusal::Recipient.worth_retrying());
}

#[test]
fn a_full_mailbox_says_that_nothing_needs_doing() {
    // `452` à l'étape d'un destinataire : sa boîte est pleine, ou le serveur est occupé. Le
    // message reste en file et repartira tout seul — **le dire est ce qui empêche un doublon**,
    // parce qu'un utilisateur qui croit l'envoi perdu le renvoie à la main.
    let left = provoke(Script::refusing("RCPT TO", "452 4.2.2 Mailbox full"));

    assert_eq!(left.outcome, Outcome::Deferred);
    assert_eq!(
        left.state,
        SendState::Queued,
        "un refus passager doit laisser la ligne en file"
    );
    assert!(
        left.message.contains("pleine") || left.message.contains("occupé"),
        "{}",
        left.message
    );
    assert!(left.message.contains("rien à faire"), "{}", left.message);
    assert!(Refusal::Quota.worth_retrying());
}

#[test]
fn a_message_too_large_says_to_remove_an_attachment() {
    // `552` à l'étape `DATA` : c'est la taille du message qui est refusée, pas la boîte du
    // destinataire. Le même code à l'étape d'un destinataire veut dire l'inverse.
    let left = provoke(Script::refusing(
        "DATA",
        "552 5.3.4 Message size exceeds fixed limit",
    ));

    assert_eq!(left.outcome, Outcome::Failed);
    assert_eq!(left.state, SendState::Failed);
    assert!(left.message.contains("trop gros"), "{}", left.message);
    assert!(left.message.contains("pièce jointe"), "{}", left.message);
}

#[test]
fn the_same_code_means_two_different_things_at_two_stages() {
    // **Le contrôle qui justifie que l'étape entre dans la décision.** Sans lui, quelqu'un
    // simplifierait `classify` en une table de codes, et le message trop gros dirait au
    // destinataire de vider sa boîte.
    assert_eq!(Refusal::classify(Stage::Data, 552), Refusal::TooLarge);
    assert_eq!(Refusal::classify(Stage::Recipient, 552), Refusal::Quota);
    assert_eq!(Refusal::classify(Stage::Recipient, 550), Refusal::Recipient);
    assert_eq!(Refusal::classify(Stage::Sender, 550), Refusal::Sender);
    // Et l'authentification ne dépend pas du code : quoi que le serveur rende à l'`AUTH`, il n'y
    // a qu'une chose à faire.
    assert_eq!(Refusal::classify(Stage::Auth, 535), Refusal::Authentication);
    assert_eq!(Refusal::classify(Stage::Auth, 454), Refusal::Authentication);
}

#[test]
fn a_size_refused_before_the_transfer_says_the_same_thing() {
    // Le serveur annonce sa limite à l'`EHLO`, et le client refuse **avant** d'ouvrir
    // l'enveloppe : RFC 1870. L'utilisateur doit lire la même chose que si le refus était venu
    // du serveur — la cause est identique, seul le moment change, et le moment ne le regarde
    // pas.
    let left = provoke(Script {
        at: "",
        reply: "",
        size: 10,
    });

    assert_eq!(left.outcome, Outcome::Failed);
    assert_eq!(left.state, SendState::Failed);
    assert!(left.message.contains("trop gros"), "{}", left.message);
    assert!(left.message.contains("pièce jointe"), "{}", left.message);
}

#[test]
fn a_refused_authentication_says_to_reconfigure_the_account() {
    // `535` à l'étape `AUTH` : le mot de passe ou le jeton n'est plus valable. C'est le refus
    // qui demande une action **hors** du message ; sans le dire, l'utilisateur cherche ce qui
    // cloche dans le message.
    let left = provoke(Script::refusing(
        "AUTH",
        "535 5.7.8 Authentication credentials invalid",
    ));

    assert_eq!(left.outcome, Outcome::Failed);
    assert_eq!(left.state, SendState::Failed);
    assert!(
        left.message.contains("authentification"),
        "{}",
        left.message
    );
    assert!(
        left.message.contains("mail account submission"),
        "la phrase ne dit pas où reconfigurer : {}",
        left.message
    );
    // **Et surtout : aucun secret dans ce que le store garde.** C'est le critère 7 sur le chemin
    // du critère 8, et c'est le seul refus qui passe à côté d'un secret.
    assert!(
        !left.message.contains("mot-de-passe"),
        "le secret est dans la phrase : {}",
        left.message
    );
    // Le base64 de `\0compte\0mot-de-passe`, tel qu'il part sur le fil. Le contrôle en clair ne
    // suffit pas : c'est encodé que le secret voyage, donc encodé qu'il pourrait fuir.
    assert!(
        !left.message.contains("bW90LWRlLXBhc3Nl"),
        "le secret encodé est dans la phrase : {}",
        left.message
    );
}

#[test]
fn a_refused_sender_does_not_blame_the_recipient() {
    // `550` à l'étape de l'expéditeur : le serveur refuse de relayer pour cette adresse. Dire
    // « vérifiez le destinataire » ferait chercher au mauvais endroit, et c'est précisément ce
    // qu'une classification par code seul aurait dit.
    let left = provoke(Script::refusing(
        "MAIL FROM",
        "550 5.7.1 Sender address rejected: not owned by user",
    ));

    assert_eq!(left.state, SendState::Failed);
    assert!(
        left.message.contains("expédier depuis cette adresse"),
        "{}",
        left.message
    );
    assert!(
        !left.message.contains("destinataire"),
        "le refus accuse le destinataire : {}",
        left.message
    );
}

#[test]
fn an_unknown_refusal_says_so_rather_than_inventing_advice() {
    // Un code hors des familles connues. **Inventer un conseil serait pire que dire qu'on ne
    // sait pas** : l'utilisateur suivrait une piste fausse et conclurait que le logiciel se
    // trompe. Le détail du serveur, lui, reste joignable.
    let left = provoke(Script::refusing(
        "RCPT TO",
        "571 5.7.1 Delivery not authorized, message refused",
    ));

    assert_eq!(left.state, SendState::Failed);
    assert!(
        left.message.contains("sans que la raison soit reconnue"),
        "{}",
        left.message
    );
    assert!(
        left.message.contains("571"),
        "le détail du serveur a été perdu : {}",
        left.message
    );
}

#[test]
fn every_family_of_the_criterion_has_its_own_sentence() {
    // Le critère nomme quatre familles : quota, destinataire refusé, message trop gros,
    // authentification. Chacune doit avoir **sa** phrase — deux familles qui se lisent pareil
    // laisseraient l'utilisateur devant le même message pour deux problèmes différents, ce qui
    // annule le bénéfice de les avoir distinguées.
    let families = [
        Refusal::Quota,
        Refusal::Recipient,
        Refusal::TooLarge,
        Refusal::Authentication,
        Refusal::Sender,
        Refusal::Other,
    ];
    let mut seen: Vec<String> = Vec::new();
    for family in families {
        let advice = family.advice(false);
        assert!(!advice.is_empty());
        // Pas de code numérique dans le conseil : c'est le libellé même du critère.
        assert!(
            !advice.chars().any(char::is_numeric),
            "un chiffre dans le conseil de {family:?} : {advice}"
        );
        assert!(
            !seen.contains(&advice),
            "deux familles disent la même chose : {family:?}"
        );
        // Chaque famille nomme un geste, et le geste n'est pas la cause : une phrase qui se
        // contenterait de décrire le refus dirait *quoi* sans dire *quoi faire*.
        assert_ne!(family.action(), family.cause());
        assert!(advice.contains(family.action()));
        seen.push(advice);
    }
}

#[test]
fn the_promise_of_an_automatic_retry_is_made_only_while_it_holds() {
    // La promesse est la même pour toutes les familles — c'est la file qui parle, pas le
    // serveur — et elle **remplace** le geste demandé à l'utilisateur, au lieu de s'y ajouter.
    // Les deux ensemble diraient « rien à faire » et « faites ceci » dans la même phrase.
    for family in [Refusal::Quota, Refusal::Recipient, Refusal::Authentication] {
        let waiting = family.advice(true);
        let over = family.advice(false);
        assert!(waiting.contains("réessayé automatiquement"), "{waiting}");
        assert!(!waiting.contains(family.action()), "{waiting}");
        assert!(!over.contains("réessayé automatiquement"), "{over}");
        // La cause, elle, ne dépend pas de la file : le serveur a refusé pour la même raison.
        assert!(waiting.starts_with(family.cause()));
        assert!(over.starts_with(family.cause()));
    }
}

#[test]
fn a_delivery_that_is_not_refused_still_goes_through() {
    // Le contrôle positif, et il n'est pas décoratif : un serveur scripté qui refuserait
    // *toujours* — un `at` mal lu, un `starts_with("")` — ferait passer tous les tests
    // ci-dessus sans que la classification soit jamais exercée.
    let left = provoke(Script::accepting());

    assert_eq!(left.outcome, Outcome::Sent);
    assert_eq!(left.state, SendState::Sent);
    assert!(
        left.message.is_empty(),
        "un envoi réussi ne laisse pas de message d'erreur : {}",
        left.message
    );
}

//! La file d'envoi : **exactement une fois**, ou un doute assumé.
//!
//! ## Ce que ce module garantit, et ce qu'aucun module ne peut garantir
//!
//! SMTP ne permet pas de demander à un serveur « as-tu déjà reçu ce message ? ». Il n'existe
//! donc pas de code qui envoie exactement une fois face à une coupure arbitraire. Ce qui
//! existe, et ce que ce module tient :
//!
//! - **zéro perte.** Le message est dans le store avant qu'un octet ne sorte. Une coupure à
//!   n'importe quel instant laisse une ligne, visible, avec un état ;
//! - **zéro doublon.** Aucun message n'est remis automatiquement si le serveur a **peut-être**
//!   déjà le sien. La reprise est un choix de l'utilisateur, jamais un effet de bord d'un
//!   redémarrage ;
//! - **le doute est borné.** Il n'existe que sur la fenêtre entre le point final et la réponse
//!   du serveur. Partout ailleurs, l'état du store dit sans ambiguïté ce qui s'est passé.
//!
//! ## L'ordre d'écriture est le mécanisme, et il est ici seulement
//!
//! ```text
//!   store: 'sending'   (durable)
//!   réseau: EHLO, AUTH, MAIL FROM, RCPT TO, DATA  → 354
//!   store: 'committing' (durable)   ← la frontière
//!   réseau: le corps, puis le point final
//!   réseau: la réponse du serveur
//!   store: 'sent' ou 'failed'
//! ```
//!
//! Tué **avant** la frontière : le serveur a une transaction vide qu'il abandonne à son délai,
//! et la ligne est en `sending` — remise sans risque. Tué **après** : la ligne est en
//! `committing`, et personne ne la remet.
//!
//! Le sens de l'erreur est choisi. La frontière peut créer un **faux doute** — écrite, puis le
//! processus meurt avant le premier octet du corps. Un faux doute coûte une décision à
//! l'utilisateur ; l'inverse coûte un doublon chez le destinataire, et personne ne peut le
//! retirer.
//!
//! ## Pourquoi ce module est dans `mailsmtp` et non dans `maild`
//!
//! Parce que la règle qu'il applique est une lecture de [`Stage`], et que les deux doivent
//! rester d'accord. Séparés d'un crate, `Stage::Committing` pourrait gagner une variante — ou
//! `may_have_been_sent` changer de sens — sans que la file le voie. Ici, le compilateur les
//! tient ensemble.
//!
//! `maild` fournit le transport et la boucle ; il ne décide de rien.

use std::io::Read;

use mailcore::{Outgoing, SendState, Store};

use crate::client::Stage;
use crate::error::{Error, Refusal, Result};

/// Le recul minimal entre deux tentatives, en secondes.
///
/// Une minute : un refus passager vient d'un quota ou d'une limitation de débit, et réessayer
/// dans la seconde ne fait qu'ajouter au débit qui a déclenché la limitation.
const BACKOFF: i64 = 60;

/// Le plafond du recul, en secondes.
///
/// Deux heures. Au-delà, la file cesserait d'être une file : un message écrit le matin partirait
/// le soir sans que rien ne l'explique à l'utilisateur.
const BACKOFF_CAP: i64 = 7_200;

/// Combien de tentatives avant de rendre le message à l'utilisateur.
///
/// Six, ce qui couvre environ une heure de reculs successifs. Un serveur qui refuse six fois de
/// suite ne refuse plus passagèrement, quoi que dise son code.
const MAX_ATTEMPTS: u32 = 6;

/// Ce que la file a besoin de savoir faire pour remettre un message.
///
/// ## Pourquoi un trait et pas la fonction de connexion
///
/// Trois raisons, dans l'ordre où elles comptent :
///
/// **Le test du critère 2 doit pouvoir mourir à un instant précis.** Un transport de test tue
/// le processus entre le `354` et le point final ; contre `tls::connect`, cet instant n'est pas
/// atteignable sans un vrai serveur.
///
/// **Les identifiants ne sont pas ici.** Ils viennent de `mailauth`, du trousseau du système, et
/// parfois d'un jeton à rafraîchir. Ce module n'a pas à connaître ce chemin.
///
/// **La frontière est un paramètre, pas une convention.** [`Transport::deliver`] reçoit le
/// rappel qui rend durable l'état `committing`, et son contrat est de l'appeler **entre** le
/// `354` et le point final. Une implémentation qui l'appellerait ailleurs casserait la garantie,
/// et le seul moyen de la lire est d'avoir le rappel dans la signature.
pub trait Transport {
    /// Remet un message, en appelant `frontier` entre l'acceptation du `DATA` et le point final.
    ///
    /// ## Le corps est un **flux**, et il ne se relit pas
    ///
    /// C'était une tranche, donc un message de 25 Mo vivait en mémoire du début à la fin de la
    /// remise. Le critère 3 de `docs/PHASE-3.md` borne la crête à 200 Mo et la règle 4 du
    /// `CLAUDE.md` interdit de charger un contenu entier : le corps passe donc du magasin de
    /// blobs au socket par un tampon de quelques kilooctets.
    ///
    /// **Conséquence pour l'implémenteur** : le flux se lit une fois. Un transport qui voudrait
    /// le relire — pour mesurer, pour réessayer — doit demander un nouveau flux à l'appelant,
    /// et il n'y a pas d'interface pour ça. C'est délibéré : réessayer **dans** une remise est
    /// exactement ce que la file existe pour empêcher.
    ///
    /// # Errors
    ///
    /// Ce que le serveur ou le réseau refuse. **L'étape portée par l'erreur décide de la
    /// suite** : voir [`Error::may_have_been_sent`].
    ///
    /// [`Error::may_have_been_sent`]: crate::Error::may_have_been_sent
    fn deliver(
        &mut self,
        job: &Outgoing,
        body: &mut dyn Read,
        frontier: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()>;
}

/// Ce qu'une tentative de remise a donné.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Le serveur a pris le message.
    Sent,
    /// Refus passager : la ligne repart en file, avec un recul.
    Deferred,
    /// Refus définitif, ou trop de tentatives : rendu à l'utilisateur.
    Failed,
    /// Le serveur a peut-être le message. **Rien d'automatique ne suivra.**
    Doubtful,
}

impl Outcome {
    /// Vrai si la ligne repassera par la boucle de remise.
    #[must_use]
    pub const fn will_be_retried(self) -> bool {
        matches!(self, Self::Deferred)
    }
}

/// Remet un message de la file, une fois.
///
/// ## L'ordre des écritures est la correction de cette fonction
///
/// C'est le seul endroit du projet où il est écrit, et c'est délibéré : le dupliquer, c'est
/// accepter qu'une copie dérive. Voir la documentation du module pour le déroulé.
///
/// ## Une erreur du store arrête l'envoi
///
/// Si `commit_outgoing` échoue avant la remise, rien n'est envoyé. Si le rappel `frontier`
/// échoue, `deliver` doit renoncer **avant** le point final — c'est son contrat, et
/// [`Transport::deliver`] le dit. Sans la frontière sur le disque, envoyer serait indéfendable :
/// une coupure juste après laisserait une ligne en `sending`, donc remise, donc un doublon.
///
/// # Errors
///
/// [`Error::Unsendable`] si la ligne n'est pas remettable — un état douteux, ou un corps absent
/// du magasin de blobs. Les échecs de remise, eux, ne sont **pas** des erreurs de cette
/// fonction : ils sont écrits dans le store et rendus comme [`Outcome`].
pub fn deliver_one(
    store: &Store,
    transport: &mut dyn Transport,
    job: &Outgoing,
    now: i64,
) -> Result<Outcome> {
    if !job.state.is_deliverable() {
        // Le refus est ici **en plus** du filtre SQL de `Store::deliverable`. Deux barrières
        // pour la même règle, parce qu'un appelant peut passer une ligne qu'il a lue lui-même.
        return Err(Error::Unsendable {
            reason: format!(
                "un message à l'état {} ne se remet pas automatiquement",
                job.state.as_str()
            ),
        });
    }

    // Le corps est lu **avant** de marquer la ligne : un blob absent est un défaut local, et
    // marquer `sending` pour rien poserait une tentative que rien n'a tentée.
    let present = store
        .blobs()
        .contains(job.blob)
        .map_err(|source| Error::Unsendable {
            reason: format!("magasin de blobs illisible : {source}"),
        })?;
    if !present {
        return Err(Error::Unsendable {
            reason: "corps absent du magasin de blobs".to_owned(),
        });
    }
    // **Un flux, pas une tranche.** `read` chargerait les 25 Mo d'un message avec pièce
    // jointe ; `open` décompresse au fil de la lecture.
    let mut body = store
        .blobs()
        .open(job.blob)
        .map_err(|source| Error::Unsendable {
            reason: format!("corps illisible dans le magasin de blobs : {source}"),
        })?;

    // **Avant tout octet réseau.** Une coupure ici laisse la ligne en `sending`, donc
    // remettable — ce qui est exact, puisque rien n'est parti.
    store
        .commit_outgoing(job.id, SendState::Sending)
        .map_err(|source| Error::Unsendable {
            reason: format!("la file n'a pas pu être marquée avant l'envoi : {source}"),
        })?;

    let mut frontier_error = None;
    let outcome = {
        let mut frontier = || -> Result<()> {
            match store.commit_outgoing(job.id, SendState::Committing) {
                Ok(()) => Ok(()),
                Err(source) => {
                    let reason = format!("la frontière du doute n'a pas pu être écrite : {source}");
                    frontier_error = Some(reason.clone());
                    Err(Error::Unsendable { reason })
                }
            }
        };
        transport.deliver(job, &mut body, &mut frontier)
    };

    match outcome {
        Ok(()) => {
            store
                .record_attempt(job.id, SendState::Sent, now, None, None, false)
                .map_err(|source| Error::Unsendable {
                    reason: format!("envoi réussi, écriture de l'état impossible : {source}"),
                })?;
            tracing::info!(job = job.id.0, "message remis");
            Ok(Outcome::Sent)
        }
        Err(failure) => Ok(settle(
            store,
            job,
            now,
            &failure,
            frontier_error.as_deref(),
        )?),
    }
}

/// Écrit ce qu'un échec veut dire, et rend le verdict.
///
/// ## Le doute est lu sur l'étape, pas sur le code
///
/// [`Error::may_have_been_sent`] est vrai pour [`Stage::Committing`] et pour rien d'autre. Un
/// `451` avant le `DATA` et un `451` après le point final portent le même code et n'ont pas le
/// même sens : le premier se réessaie, le second ne se réessaie jamais.
fn settle(
    store: &Store,
    job: &Outgoing,
    now: i64,
    failure: &Error,
    frontier_error: Option<&str>,
) -> Result<Outcome> {
    // **La phrase se compose après la décision, pas avant.** « L'envoi est réessayé
    // automatiquement » n'est vrai que si la file va vraiment réessayer, et elle ne le sait
    // qu'ici. Composer d'abord était le défaut trouvé par
    // `a_transient_refusal_that_gave_up_stops_promising_an_automatic_retry` : la promesse
    // survivait à l'épuisement des tentatives, sur une ligne `failed` que rien ne bougerait
    // plus.
    let attempts = job.attempts.saturating_add(1);
    let retrying = !failure.may_have_been_sent() && failure.retryable() && attempts < MAX_ATTEMPTS;
    let text = message_for_user(failure, frontier_error, retrying);
    // Le bit que l'interface consulte avant d'offrir « Renvoyer ». Il n'a de sens que sur une
    // ligne finie : tant que la file réessaie, il n'y a rien à renvoyer à la main.
    let resendable = !retrying
        && !failure.may_have_been_sent()
        && failure.refusal().is_some_and(Refusal::worth_retrying);

    if failure.may_have_been_sent() {
        // **La ligne du critère 2.** L'état reste `committing` : il y a été écrit avant le point
        // final, et `record_attempt` ne fait qu'ajouter le texte que l'utilisateur lira. Aucune
        // date de reprise n'est posée, parce qu'aucune reprise n'aura lieu sans lui.
        store
            // `false` : un envoi douteux ne se renvoie pas d'un clic. Sa sortie est
            // `Store::resolve_doubt`, qui demande d'avoir vérifié chez le fournisseur — c'est le
            // critère 2, et c'est la seule décision du projet qu'on ne raccourcit pas.
            .record_attempt(job.id, SendState::Committing, now, Some(&text), None, false)
            .map_err(|source| Error::Unsendable {
                reason: format!("doute non consigné : {source}"),
            })?;
        tracing::warn!(
            job = job.id.0,
            stage = %Stage::Committing,
            "envoi douteux : le serveur a peut-être le message, aucune reprise automatique"
        );
        return Ok(Outcome::Doubtful);
    }

    if retrying {
        let delay = backoff(attempts);
        store
            .record_attempt(
                job.id,
                SendState::Queued,
                now,
                Some(&text),
                Some(now.saturating_add(delay)),
                false,
            )
            .map_err(|source| Error::Unsendable {
                reason: format!("report non consigné : {source}"),
            })?;
        tracing::info!(job = job.id.0, attempts, delay, "envoi reporté");
        return Ok(Outcome::Deferred);
    }

    store
        .record_attempt(
            job.id,
            SendState::Failed,
            now,
            Some(&text),
            None,
            resendable,
        )
        .map_err(|source| Error::Unsendable {
            reason: format!("échec non consigné : {source}"),
        })?;
    tracing::warn!(job = job.id.0, attempts, "envoi abandonné");
    Ok(Outcome::Failed)
}

/// Le recul avant la tentative suivante.
///
/// Doublement à chaque fois, plafonné. `saturating_mul` plutôt qu'un décalage : une tentative
/// numéro trente déborderait, et un débordement rendrait un recul négatif — donc une reprise
/// immédiate, en boucle.
fn backoff(attempts: u32) -> i64 {
    let factor = 1_i64 << attempts.saturating_sub(1).min(20);
    BACKOFF.saturating_mul(factor).min(BACKOFF_CAP)
}

/// Le texte que l'utilisateur lira — critère 8.
///
/// ## Ce qu'il ne contient pas
///
/// Pas de secret : les seules erreurs qui en approchent sont les refus d'authentification, et
/// [`Error::AuthRefused`] ne porte que ce que le serveur a répondu. Pas de code nu non plus :
/// l'affichage de `Error` dit toujours ce qui s'est passé en français avant de donner le code.
///
/// `retrying` dit si la file va réessayer d'elle-même. C'est ce qui décide de la fin de la
/// phrase — voir [`Refusal::advice`].
fn message_for_user(failure: &Error, frontier_error: Option<&str>, retrying: bool) -> String {
    if let Some(reason) = frontier_error {
        // La frontière n'a pas pu être écrite, donc l'envoi a été abandonné **avant** le point
        // final. Dire l'échec du store, pas l'échec réseau qui n'en est que la conséquence.
        return reason.to_owned();
    }
    // **La phrase d'abord, le détail ensuite.** L'affichage de `Error` nomme l'étape et le code
    // — utile pour enquêter, insuffisant pour agir. Le critère 8 demande que l'utilisateur voie
    // *quoi faire* ; la famille du refus le dit, et le texte du serveur reste derrière pour qui
    // veut regarder.
    match failure.refusal() {
        Some(family) => format!("{} ({failure})", family.advice(retrying)),
        None => failure.to_string(),
    }
}

/// Assemble un brouillon et le met en file, **sans le tenir en mémoire**.
///
/// ## Un seul endroit assemble et met en file
///
/// `mail send` et `outbox.send` en avaient chacun leur copie, et elles ont divergé le jour où
/// l'assemblage est devenu streamé : la CLI aurait gardé le chemin en mémoire. Une copie de
/// plus, c'est une copie qui ne recevra pas la correction suivante.
///
/// ## L'ordre : le blob, puis la ligne
///
/// Une ligne de file qui pointe vers un blob absent serait un message perdu que rien ne
/// signale. Un blob que plus aucune ligne ne désigne est de la place — `mail doctor` le compte,
/// et c'est le sens de l'erreur qu'on veut.
///
/// ## Les pièces jointes restent dans le magasin après coup
///
/// Le message assemblé contient leur base64, qui n'est pas le même contenu que le fichier
/// brut : les deux blobs coexistent, et celui de la pièce devient orphelin dès la mise en file.
/// `Store::orphan_blobs` le compte et `mail doctor` le dit. Le supprimer ici demanderait de
/// savoir qu'aucun autre brouillon ne l'utilise, ce qui suppose des brouillons persistés — ils
/// n'existent pas encore.
///
/// # Errors
///
/// [`Error::Unsendable`] si le brouillon n'est pas envoyable ou si une pièce jointe est absente
/// du magasin, [`Error::Network`] — étape `Data` — sur une erreur d'écriture.
pub fn stage(
    store: &Store,
    account: mailcore::AccountId,
    draft: &crate::compose::Draft,
    now: i64,
) -> Result<(mailcore::OutboxId, u64)> {
    let recipients: Vec<String> = draft
        .envelope_recipients()
        .iter()
        .map(|it| it.addr().to_owned())
        .collect();

    let mut sink = store
        .blobs()
        .put_writer()
        .map_err(|source| Error::Unsendable {
            reason: format!("le magasin de blobs n'accepte pas d'écriture : {source}"),
        })?;
    // Le compteur enveloppe le puits : la taille du message n'est connue qu'en l'écrivant, et
    // `SIZE` en a besoin. La compter en relisant le blob demanderait de le décompresser en
    // entier — voir la migration v7 du store.
    let mut counting = Counting {
        sink: &mut sink,
        written: 0,
    };
    draft.write_to(&mut counting, now, &mut |hash| {
        store
            .blobs()
            .open(hash)
            .map_err(|source| Error::Unsendable {
                reason: format!("pièce jointe introuvable : {source}"),
            })
            .map(|it| Box::new(it) as Box<dyn std::io::Read>)
    })?;
    let size = counting.written;

    let blob = sink
        .finish()
        .map_err(|source| Error::Unsendable {
            reason: format!("le message n'a pas pu être rangé : {source}"),
        })?
        .hash;
    let id = store
        .enqueue(account, blob, draft.from.addr(), &recipients, size, now)
        .map_err(|source| Error::Unsendable {
            reason: format!("mise en file impossible : {source}"),
        })?;
    tracing::info!(job = id.0, size, "message mis en file");
    Ok((id, size))
}

/// Un puits qui compte ce qu'il laisse passer.
struct Counting<'a> {
    sink: &'a mut Box<dyn mailcore::store::blobs::BlobSink>,
    written: u64,
}

impl std::io::Write for Counting<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.sink.write_all(buffer)?;
        self.written = self.written.saturating_add(buffer.len() as u64);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sink.flush()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use mailcore::{AccountId, AccountKind, BlobHash, OutboxId};

    /// Un store neuf, avec un compte et un corps de message dans les blobs.
    fn fixture() -> (tempfile::TempDir, Store, AccountId, BlobHash) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let store = Store::open(&root).unwrap();
        let account = {
            let writer = store.writer().unwrap();
            let id = writer
                .upsert_account(AccountKind::Imap.as_str(), "compte")
                .unwrap();
            writer.commit().unwrap();
            id
        };
        let blob = store
            .blobs()
            .put(b"From: marie@exemple.fr\r\n\r\nbonjour\r\n")
            .unwrap()
            .hash;
        (dir, store, account, blob)
    }

    fn enqueue(store: &Store, account: AccountId, blob: BlobHash) -> Outgoing {
        let id = store
            .enqueue(
                account,
                blob,
                "marie@exemple.fr",
                &["jean@ailleurs.fr".to_owned()],
                // La taille : les tests de la file ne la lisent pas, mais `SIZE` en dépend.
                42,
                1_000,
            )
            .unwrap();
        store.outgoing(id).unwrap().unwrap()
    }

    /// Un transport scripté : il décide où il échoue, et il note ce qu'il a fait.
    struct Scripted {
        /// Ce que la remise rend. `None` pour un succès.
        failure: Option<Error>,
        /// Faux si le transport doit échouer **avant** d'appeler la frontière.
        cross_frontier: bool,
        /// Vrai après un appel à la frontière.
        crossed: bool,
        /// Les octets qu'on lui a donnés.
        body: Vec<u8>,
    }

    impl Scripted {
        fn succeeding() -> Self {
            Self {
                failure: None,
                cross_frontier: true,
                crossed: false,
                body: Vec::new(),
            }
        }

        fn failing(failure: Error, cross_frontier: bool) -> Self {
            Self {
                failure: Some(failure),
                cross_frontier,
                crossed: false,
                body: Vec::new(),
            }
        }
    }

    impl Transport for Scripted {
        fn deliver(
            &mut self,
            _job: &Outgoing,
            body: &mut dyn Read,
            frontier: &mut dyn FnMut() -> Result<()>,
        ) -> Result<()> {
            // Le flux est lu **en entier** pour que les tests puissent l'inspecter. Un vrai
            // transport ne le fait pas : c'est le point du critère 3.
            self.body.clear();
            body.read_to_end(&mut self.body)
                .map_err(|source| Error::Network {
                    stage: crate::Stage::Data,
                    source,
                })?;
            if self.cross_frontier {
                frontier()?;
                self.crossed = true;
            }
            match self.failure.take() {
                Some(failure) => Err(failure),
                None => Ok(()),
            }
        }
    }

    fn refusal(stage: Stage, code: u16) -> Error {
        Error::from_reply(
            stage,
            &crate::Reply {
                code,
                lines: vec!["ce que le serveur a dit".to_owned()],
            },
        )
    }

    #[test]
    fn a_successful_send_ends_in_sent_and_hands_over_the_body() {
        let (_dir, store, account, blob) = fixture();
        let job = enqueue(&store, account, blob);
        let mut transport = Scripted::succeeding();

        let outcome = deliver_one(&store, &mut transport, &job, 2_000).unwrap();
        assert_eq!(outcome, Outcome::Sent);
        assert!(transport.crossed, "la frontière n'a pas été franchie");
        assert!(
            transport.body.starts_with(b"From: marie@exemple.fr"),
            "le corps donné au transport n'est pas celui du blob"
        );

        let after = store.outgoing(job.id).unwrap().unwrap();
        assert_eq!(after.state, SendState::Sent);
        assert_eq!(after.attempts, 1);
        assert_eq!(after.tried_at, Some(2_000));
        assert!(after.retry_after.is_none());
    }

    #[test]
    fn a_failure_after_the_final_dot_leaves_the_line_doubtful_and_out_of_the_queue() {
        // **Le critère 2, sur le store.** L'erreur porte `Committing` : le serveur a peut-être
        // le message. La ligne reste douteuse et ne repasse par aucune boucle.
        let (_dir, store, account, blob) = fixture();
        let job = enqueue(&store, account, blob);
        let mut transport = Scripted::failing(refusal(Stage::Committing, 451), true);

        let outcome = deliver_one(&store, &mut transport, &job, 2_000).unwrap();
        assert_eq!(outcome, Outcome::Doubtful);
        assert!(!outcome.will_be_retried());

        let after = store.outgoing(job.id).unwrap().unwrap();
        assert_eq!(after.state, SendState::Committing);
        assert!(after.state.is_doubtful());
        assert!(
            after.retry_after.is_none(),
            "une date de reprise a été posée sur un envoi douteux"
        );
        assert!(
            store.deliverable(9_999_999, 10).unwrap().is_empty(),
            "un envoi douteux est remis automatiquement"
        );
        assert_eq!(store.doubtful().unwrap().len(), 1);
    }

    #[test]
    fn a_transient_failure_before_the_body_goes_back_to_the_queue_with_a_delay() {
        // Le contrôle négatif du test ci-dessus : un `451` **avant** le corps porte le même code
        // et n'a pas le même sens. Sans ce test, une implémentation qui rendrait tout douteux
        // passerait le précédent et ne remettrait plus jamais rien.
        let (_dir, store, account, blob) = fixture();
        let job = enqueue(&store, account, blob);
        let mut transport = Scripted::failing(refusal(Stage::Recipient, 451), false);

        let outcome = deliver_one(&store, &mut transport, &job, 2_000).unwrap();
        assert_eq!(outcome, Outcome::Deferred);
        assert!(outcome.will_be_retried());

        let after = store.outgoing(job.id).unwrap().unwrap();
        assert_eq!(after.state, SendState::Queued);
        assert_eq!(after.retry_after, Some(2_000 + BACKOFF));
        assert!(
            store.deliverable(2_000, 10).unwrap().is_empty(),
            "le recul n'est pas respecté"
        );
        assert_eq!(store.deliverable(2_000 + BACKOFF, 10).unwrap().len(), 1);
    }

    #[test]
    fn a_permanent_refusal_comes_back_to_the_user_without_a_retry() {
        let (_dir, store, account, blob) = fixture();
        let job = enqueue(&store, account, blob);
        let mut transport = Scripted::failing(refusal(Stage::Recipient, 550), false);

        assert_eq!(
            deliver_one(&store, &mut transport, &job, 2_000).unwrap(),
            Outcome::Failed
        );
        let after = store.outgoing(job.id).unwrap().unwrap();
        assert_eq!(after.state, SendState::Failed);
        assert!(after.last_error.is_some(), "l'utilisateur n'a rien à lire");
        assert!(store.deliverable(9_999_999, 10).unwrap().is_empty());
    }

    #[test]
    fn a_doubtful_line_is_refused_even_when_handed_over_directly() {
        // La deuxième barrière : le filtre SQL de `Store::deliverable` ne protège que les
        // appelants qui l'utilisent. Un appelant qui lit la ligne lui-même se heurte à celle-ci.
        let (_dir, store, account, blob) = fixture();
        let job = enqueue(&store, account, blob);
        store
            .commit_outgoing(job.id, SendState::Committing)
            .unwrap();
        let doubtful = store.outgoing(job.id).unwrap().unwrap();

        let mut transport = Scripted::succeeding();
        let outcome = deliver_one(&store, &mut transport, &doubtful, 2_000);
        assert!(matches!(outcome, Err(Error::Unsendable { .. })));
        assert!(
            transport.body.is_empty(),
            "le corps est parti au transport malgré le refus"
        );
    }

    #[test]
    fn a_line_marked_sending_is_taken_again() {
        // Le rattrapage d'un processus tué avant le corps. Ne pas le reprendre serait une perte.
        let (_dir, store, account, blob) = fixture();
        let job = enqueue(&store, account, blob);
        store.commit_outgoing(job.id, SendState::Sending).unwrap();
        let resumed = store.outgoing(job.id).unwrap().unwrap();

        let mut transport = Scripted::succeeding();
        assert_eq!(
            deliver_one(&store, &mut transport, &resumed, 2_000).unwrap(),
            Outcome::Sent
        );
    }

    #[test]
    fn a_missing_body_is_refused_before_anything_leaves() {
        let (_dir, store, account, _blob) = fixture();
        let job = enqueue(&store, account, BlobHash::from_bytes([9_u8; 32]));
        let mut transport = Scripted::succeeding();

        let outcome = deliver_one(&store, &mut transport, &job, 2_000);
        assert!(matches!(outcome, Err(Error::Unsendable { .. })));
        assert!(transport.body.is_empty());
        // La ligne n'a pas été marquée `sending` : rien n'a été tenté.
        assert_eq!(
            store.outgoing(job.id).unwrap().unwrap().state,
            SendState::Queued
        );
    }

    #[test]
    fn the_backoff_doubles_and_never_goes_backwards() {
        assert_eq!(backoff(1), BACKOFF);
        assert_eq!(backoff(2), BACKOFF * 2);
        assert_eq!(backoff(3), BACKOFF * 4);
        assert_eq!(backoff(MAX_ATTEMPTS), BACKOFF * 32);
        // Le plafond, et surtout : jamais négatif. Un recul négatif serait une reprise
        // immédiate en boucle, ce qui est comment on se fait bloquer par un fournisseur.
        for attempts in [0_u32, 1, 20, 40, 100, u32::MAX] {
            let delay = backoff(attempts);
            assert!(delay > 0, "recul nul ou négatif à {attempts} tentatives");
            assert!(delay <= BACKOFF_CAP, "recul au-dessus du plafond");
        }
    }

    #[test]
    fn too_many_attempts_stops_retrying_a_retryable_failure() {
        let (_dir, store, account, blob) = fixture();
        let job = enqueue(&store, account, blob);
        // La ligne a déjà épuisé son crédit.
        for _ in 0..MAX_ATTEMPTS {
            store
                .record_attempt(
                    job.id,
                    SendState::Queued,
                    1_500,
                    Some("occupé"),
                    None,
                    false,
                )
                .unwrap();
        }
        let worn = store.outgoing(job.id).unwrap().unwrap();
        assert_eq!(worn.attempts, MAX_ATTEMPTS);

        let mut transport = Scripted::failing(refusal(Stage::Sender, 451), false);
        let outcome = deliver_one(&store, &mut transport, &worn, 2_000).unwrap();
        assert_eq!(
            outcome,
            Outcome::Failed,
            "un refus passager est réessayé indéfiniment"
        );
    }

    #[test]
    fn a_transient_refusal_that_gave_up_stops_promising_an_automatic_retry() {
        // **Le trou du critère 8, et il ne se voyait pas depuis le verdict.** Un refus passager
        // dit « l'envoi est réessayé automatiquement ; rien à faire », ce qui est vrai tant que
        // la file réessaie. Les tentatives épuisées, l'état passe à `failed` et la phrase reste
        // — l'utilisateur lit qu'il n'a rien à faire sur un message qui ne repartira plus
        // jamais, alors qu'il est le seul qui puisse encore le faire partir.
        let (_dir, store, account, blob) = fixture();
        let job = enqueue(&store, account, blob);
        for _ in 0..MAX_ATTEMPTS {
            store
                .record_attempt(
                    job.id,
                    SendState::Queued,
                    1_500,
                    Some("occupé"),
                    None,
                    false,
                )
                .unwrap();
        }
        let worn = store.outgoing(job.id).unwrap().unwrap();

        let mut transport = Scripted::failing(refusal(Stage::Recipient, 452), false);
        assert_eq!(
            deliver_one(&store, &mut transport, &worn, 2_000).unwrap(),
            Outcome::Failed
        );

        let text = store
            .outgoing(job.id)
            .unwrap()
            .unwrap()
            .last_error
            .unwrap_or_default();
        assert!(
            !text.contains("rien à faire"),
            "la phrase promet une reprise qui n'aura pas lieu : {text}"
        );
        assert!(
            !text.contains("automatiquement"),
            "la phrase promet une reprise qui n'aura pas lieu : {text}"
        );
        // Et elle doit dire ce qui reste à faire : renvoyer soi-même.
        assert!(
            text.contains("Renvoyez"),
            "la phrase ne dit pas ce qu'il reste à faire : {text}"
        );
    }

    #[test]
    fn a_transient_refusal_with_credit_left_still_promises_the_retry() {
        // Le contrôle qui empêche de « corriger » le défaut ci-dessus en retirant la promesse
        // partout. Tant que la file réessaie, le dire est ce qui évite un renvoi à la main —
        // donc un doublon.
        let (_dir, store, account, blob) = fixture();
        let job = enqueue(&store, account, blob);

        let mut transport = Scripted::failing(refusal(Stage::Recipient, 452), false);
        assert_eq!(
            deliver_one(&store, &mut transport, &job, 2_000).unwrap(),
            Outcome::Deferred
        );

        let text = store
            .outgoing(job.id)
            .unwrap()
            .unwrap()
            .last_error
            .unwrap_or_default();
        assert!(text.contains("rien à faire"), "{text}");
    }

    #[test]
    fn an_id_that_does_not_exist_is_not_a_panic() {
        let (_dir, store, _account, _blob) = fixture();
        assert!(store.outgoing(OutboxId(4_242)).unwrap().is_none());
    }
}

//! La file d'envoi : le seul journal du projet dont la perte soit **irréparable**.
//!
//! ## Pourquoi ce module est à part de `write`
//!
//! [`crate::store::write::Writer`] est fait pour les gros lots : une transaction, des dizaines
//! de milliers de lignes, un `fsync` pour tout le monde. C'est ce qui rend un import
//! réalisable, et c'est exactement le mauvais compromis ici.
//!
//! Une transition de la file est une ligne, et sa **durabilité est le mécanisme de correction**
//! du critère 2 : `committing` doit être sur le disque avant que le point final ne parte. Une
//! transition regroupée avec la suivante, ou validée en `synchronous = NORMAL`, ne garantit
//! plus l'ordre — et l'ordre est tout ce qu'on a.
//!
//! D'où un module qui écrit une ligne à la fois, chacune dans sa transaction, et qui monte
//! `synchronous` à `FULL` pour la durée de l'écriture. Voir [`Store::commit_outgoing`].
//!
//! ## Ce que la durabilité couvre, et ce qu'elle ne couvre pas
//!
//! `FULL` en WAL demande à SQLite un `fsync` du journal à chaque validation. Ça couvre la
//! coupure d'alimentation et le redémarrage brutal, en plus du processus tué — que `NORMAL`
//! couvrait déjà, puisque le cache de pages du système survit à la mort d'un processus.
//!
//! Ce qu'aucun réglage ne couvre : un disque qui ment sur ses `fsync`. C'est hors de portée du
//! programme, et le dire vaut mieux que de l'ignorer.

use rusqlite::{OptionalExtension, params};

use crate::error::Result;
use crate::model::{AccountId, BlobHash, OutboxId, Outgoing, SendState};
use crate::store::Store;

impl Store {
    /// Met un message dans la file, à l'état [`SendState::Queued`].
    ///
    /// Les octets du message doivent déjà être dans le magasin de blobs : l'appelant les y met
    /// **avant**, parce qu'une ligne de file qui pointe vers un blob absent serait un message
    /// perdu que rien ne signale.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue.
    pub fn enqueue(
        &self,
        account: AccountId,
        blob: BlobHash,
        sender: &str,
        recipients: &[String],
        size: u64,
        now: i64,
    ) -> Result<OutboxId> {
        let joined = recipients.join("\n");
        // La taille est celle du message assemblé, connue par l'appelant qui vient de l'écrire.
        // La relire du blob demanderait de le décompresser en entier pour le compter — voir la
        // migration v7.
        let size = i64::try_from(size).unwrap_or(i64::MAX);
        self.durably(|conn| {
            conn.execute(
                "INSERT INTO outbox
                    (account_id, blob_hash, sender, recipients, state, queued_at, size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    account.0,
                    blob.as_bytes().as_slice(),
                    sender,
                    joined,
                    SendState::Queued.as_str(),
                    now,
                    size
                ],
            )?;
            Ok(OutboxId(conn.last_insert_rowid()))
        })
    }

    /// Écrit un état, **et le rend durable avant de rendre la main**.
    ///
    /// ## C'est la fonction dont l'ordre décide du critère 2
    ///
    /// Appelée avec [`SendState::Committing`], elle doit revenir seulement quand la transition
    /// est sur le disque : le point final part après. Appelée avec [`SendState::Sending`], elle
    /// pose la borne inverse — tant qu'elle n'est pas revenue, aucune enveloppe n'est ouverte.
    ///
    /// Un appelant qui inverserait l'ordre — écrire l'état après l'action — perdrait la
    /// garantie sans que rien ne le signale. C'est la raison de `mailsmtp::queue::deliver_one` :
    /// elle est le seul endroit du projet où cet ordre est écrit.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture ou la validation échoue. **Une erreur ici doit
    /// arrêter l'envoi** : sans la transition sur le disque, la suite serait indéfendable.
    pub fn commit_outgoing(&self, id: OutboxId, state: SendState) -> Result<()> {
        self.durably(|conn| {
            conn.execute(
                "UPDATE outbox SET state = ?2 WHERE id = ?1",
                params![id.0, state.as_str()],
            )?;
            Ok(())
        })
    }

    /// Note une tentative : l'horodatage, le compteur, et le recul avant la suivante.
    ///
    /// Le texte d'erreur est celui que l'utilisateur lira — critère 8. **L'appelant est
    /// responsable de n'y mettre aucun secret** ; ce module ne peut pas le vérifier, et le
    /// critère 7 le mesure sur le store entier.
    ///
    /// `resendable` accompagne ce texte : la phrase dit quoi faire, le bit dit si l'interface
    /// peut le faire d'un clic. Voir [`Outgoing::resendable`]. Il est écrit **dans la même
    /// transaction** que l'état et le texte, pour qu'aucun lecteur ne voie une phrase et un bit
    /// qui se contredisent.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue.
    pub fn record_attempt(
        &self,
        id: OutboxId,
        state: SendState,
        now: i64,
        error: Option<&str>,
        retry_after: Option<i64>,
        resendable: bool,
    ) -> Result<()> {
        self.durably(|conn| {
            conn.execute(
                "UPDATE outbox
                    SET state = ?2,
                        attempts = attempts + 1,
                        tried_at = ?3,
                        last_error = ?4,
                        retry_after = ?5,
                        resendable = ?6
                  WHERE id = ?1",
                params![id.0, state.as_str(), now, error, retry_after, resendable],
            )?;
            Ok(())
        })
    }

    /// Remet en file une ligne **échouée**, sur décision de l'utilisateur.
    ///
    /// ## Pourquoi ce n'est pas `resolve_doubt`, et pourquoi ce n'est pas dangereux
    ///
    /// [`Store::resolve_doubt`] existe pour l'état `committing`, où le serveur a **peut-être**
    /// le message : renvoyer y est un pari, et c'est pour ça qu'il demande d'avoir vérifié chez
    /// le fournisseur.
    ///
    /// `failed` est l'inverse : le serveur a refusé, donc il n'a rien pris, donc **un renvoi ne
    /// peut pas faire de doublon**. Ce qu'il peut faire, c'est échouer à nouveau — et c'est à
    /// quoi sert [`Outgoing::resendable`], que l'interface consulte avant de proposer le geste.
    /// Cette fonction ne le consulte pas elle-même : un utilisateur qui demande explicitement un
    /// renvoi après avoir corrigé son compte chez son fournisseur a raison, et le bit ne sait
    /// pas ce qu'il a corrigé.
    ///
    /// ## Le compteur de tentatives repart de zéro
    ///
    /// À l'inverse de `resolve_doubt`, qui le conserve comme historique. Ici il **doit** repartir
    /// : une ligne `failed` par épuisement porte six tentatives, et les garder ferait échouer le
    /// renvoi au premier refus passager. Un renvoi demandé par un humain est un envoi neuf, pas
    /// la septième tentative d'un ancien.
    ///
    /// Ce qui est perdu est le compte des tentatives passées. `tried_at` le rattrape en partie,
    /// et le journal `info` ci-dessous garde la trace de la décision.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue. Rend `false` — sans rien écrire — si la
    /// ligne n'existe pas ou n'est pas à l'état `failed`. Le refus des autres états n'est pas de
    /// la prudence : sans lui, cette fonction renverrait un message déjà `sent`.
    pub fn retry_outgoing(&self, id: OutboxId) -> Result<bool> {
        let Some(line) = self.outgoing(id)? else {
            return Ok(false);
        };
        if line.state != SendState::Failed {
            tracing::warn!(
                job = id.0,
                state = line.state.as_str(),
                "renvoi refusé : cette ligne n'a pas échoué"
            );
            return Ok(false);
        }
        self.durably(|conn| {
            conn.execute(
                "UPDATE outbox
                    SET state = ?2,
                        attempts = 0,
                        last_error = ?3,
                        retry_after = NULL,
                        resendable = 0
                  WHERE id = ?1",
                params![
                    id.0,
                    SendState::Queued.as_str(),
                    "remis en file sur décision de l'utilisateur"
                ],
            )?;
            Ok(())
        })?;
        tracing::info!(job = id.0, "ligne échouée remise en file");
        Ok(true)
    }

    /// Retire une ligne **finie** de la file.
    ///
    /// ## Elle refuse tout ce qui n'est pas fini
    ///
    /// `queued`, `sending` et surtout `committing` sont refusés. Retirer une ligne en cours
    /// perdrait un message que le facteur allait remettre ; retirer une ligne douteuse
    /// effacerait la trace d'un message peut-être parti, ce qui est exactement l'information
    /// que le critère 2 existe pour conserver.
    ///
    /// Seuls `sent` et `failed` partent, et ils partent sur décision de l'utilisateur : rien
    /// n'appelle cette fonction tout seul. L'historique d'envoi est un journal, et un journal
    /// qui se purge de lui-même ne sert à rien le jour où on le consulte.
    ///
    /// ## Le blob n'est pas supprimé ici
    ///
    /// Il devient orphelin, et `Store::orphan_blobs` le compte — `mail doctor` le dit. Le
    /// supprimer ici demanderait de savoir qu'aucune autre ligne ne le partage, ce qui arrive :
    /// deux envois du même contenu ont le même blob, par construction.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`]. Rend `false` — sans rien écrire — si la ligne n'existe pas ou
    /// n'est pas finie.
    pub fn forget_outgoing(&self, id: OutboxId) -> Result<bool> {
        let Some(line) = self.outgoing(id)? else {
            return Ok(false);
        };
        if !matches!(line.state, SendState::Sent | SendState::Failed) {
            tracing::warn!(
                job = id.0,
                state = line.state.as_str(),
                "retrait refusé : cette ligne n'est pas finie"
            );
            return Ok(false);
        }
        self.durably(|conn| {
            conn.execute("DELETE FROM outbox WHERE id = ?1", params![id.0])?;
            Ok(())
        })?;
        tracing::info!(job = id.0, "ligne de file retirée");
        Ok(true)
    }

    /// Les messages que la boucle de remise peut prendre, du plus ancien au plus récent.
    ///
    /// ## `committing` n'en fait jamais partie
    ///
    /// C'est la règle du critère 2, et elle est dans la clause `WHERE` plutôt que dans la
    /// boucle appelante : un état douteux ne doit pas pouvoir être remis par oubli d'un
    /// filtre. [`SendState::is_deliverable`] dit la même chose côté Rust, et les deux doivent
    /// rester d'accord — d'où le test `the_sql_filter_and_the_rust_predicate_agree`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si la lecture échoue, [`crate::Error::UnknownSendState`] si une
    /// ligne porte un état que ce binaire ne connaît pas.
    pub fn deliverable(&self, now: i64, limit: u32) -> Result<Vec<Outgoing>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT id, account_id, blob_hash, sender, recipients, state, attempts,
                    size,
                    queued_at, tried_at, last_error, retry_after, resendable
               FROM outbox
              WHERE state IN ('queued', 'sending')
                AND (retry_after IS NULL OR retry_after <= ?1)
              ORDER BY id
              LIMIT ?2",
        )?;
        let rows = statement.query_map(params![now, limit], row_to_outgoing)?;
        collect(rows)
    }

    /// Les messages dont personne ne sait s'ils sont partis.
    ///
    /// C'est ce que l'interface doit montrer, et ce que la boucle de remise doit ignorer. Deux
    /// lecteurs, une seule vérité.
    ///
    /// # Errors
    ///
    /// Comme [`Store::deliverable`].
    pub fn doubtful(&self) -> Result<Vec<Outgoing>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT id, account_id, blob_hash, sender, recipients, state, attempts,
                    size,
                    queued_at, tried_at, last_error, retry_after, resendable
               FROM outbox
              WHERE state = 'committing'
              ORDER BY id",
        )?;
        let rows = statement.query_map([], row_to_outgoing)?;
        collect(rows)
    }

    /// Une ligne de la file, par identifiant.
    ///
    /// # Errors
    ///
    /// Comme [`Store::deliverable`].
    pub fn outgoing(&self, id: OutboxId) -> Result<Option<Outgoing>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT id, account_id, blob_hash, sender, recipients, state, attempts,
                    size,
                    queued_at, tried_at, last_error, retry_after, resendable
               FROM outbox
              WHERE id = ?1",
        )?;
        statement
            .query_row(params![id.0], row_to_outgoing)
            .optional()?
            .transpose()
    }

    /// Toute la file, du plus ancien au plus récent. Pour l'interface et pour `mail doctor`.
    ///
    /// # Errors
    ///
    /// Comme [`Store::deliverable`].
    pub fn outbox(&self) -> Result<Vec<Outgoing>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT id, account_id, blob_hash, sender, recipients, state, attempts,
                    size,
                    queued_at, tried_at, last_error, retry_after, resendable
               FROM outbox
              ORDER BY id",
        )?;
        let rows = statement.query_map([], row_to_outgoing)?;
        collect(rows)
    }

    /// Exécute une écriture et la rend durable avant de rendre la main.
    ///
    /// `synchronous` est remonté à `FULL` le temps de la transaction, puis remis à `NORMAL`.
    /// Le pragma est **par connexion**, pas par transaction : il n'y a pas d'autre façon de
    /// demander la durabilité à SQLite pour une écriture donnée.
    ///
    /// Le retour à `NORMAL` a lieu même si l'écriture échoue. Le laisser à `FULL` ne serait pas
    /// faux, seulement lent : un import ultérieur sur la même connexion paierait un `fsync` par
    /// transaction.
    fn durably<T>(&self, write: impl FnOnce(&rusqlite::Connection) -> Result<T>) -> Result<T> {
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        let outcome = (|| {
            let tx = self.conn.unchecked_transaction()?;
            let value = write(&tx)?;
            tx.commit()?;
            Ok(value)
        })();
        // Le résultat de la remise en place est ignoré exprès : une erreur ici n'annule pas
        // l'écriture, et la masquer derrière le succès de l'écriture serait mentir sur ce qui
        // s'est passé. Le pire cas est une connexion restée en `FULL`, donc lente.
        let _ = self.conn.pragma_update(None, "synchronous", "NORMAL");
        outcome
    }
}

/// Ce que l'utilisateur décide d'un message douteux.
///
/// ## Pourquoi ces deux-là et pas une seule
///
/// Le protocole ne peut pas lever le doute ; l'utilisateur, lui, peut aller regarder ses
/// messages envoyés chez son fournisseur. Il revient alors avec l'une de deux réponses, et elles
/// demandent l'inverse l'une de l'autre :
///
/// - **il n'est pas arrivé** → le remettre en file. Sans cette issue, un message perdu le reste,
///   et l'utilisateur doit le réécrire ;
/// - **il est arrivé** → l'accepter comme envoyé. Sans cette issue, l'avertissement reste
///   affiché pour toujours, et un avertissement qu'on ne peut pas faire disparaître finit par
///   ne plus être lu — y compris le jour où il compte.
///
/// La troisième réponse — « je ne sais pas » — est l'état actuel : ne rien faire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Le destinataire ne l'a pas reçu : le remettre en file.
    Resend,
    /// Le destinataire l'a reçu : le marquer envoyé.
    Accept,
}

impl Decision {
    /// L'étiquette reçue d'un client.
    ///
    /// # Errors
    ///
    /// [`crate::Error::UnknownSendState`] sur une valeur inconnue. Pas de repli : un repli
    /// choisirait entre « renvoyer » et « accepter » à la place de l'utilisateur, et les deux
    /// sont irréversibles dans un sens différent.
    pub fn parse(label: &str) -> crate::Result<Self> {
        match label {
            "resend" => Ok(Self::Resend),
            "accept" => Ok(Self::Accept),
            other => Err(crate::Error::UnknownSendState {
                found: other.to_owned(),
            }),
        }
    }
}

impl Store {
    /// Tranche le doute sur un message, **sur décision de l'utilisateur**.
    ///
    /// ## C'est la seule sortie de `committing`, et elle ne s'automatise pas
    ///
    /// Rien dans le programme n'appelle cette fonction tout seul : ni le facteur, ni
    /// `mail send --flush`, ni un redémarrage. C'est le critère 2 de `docs/PHASE-3.md` — un
    /// message peut-être parti ne se remet pas sans qu'un humain l'ait demandé.
    ///
    /// ## Elle refuse tout état qui n'est pas `committing`
    ///
    /// Et le refus n'est pas de la prudence : sans lui, cette fonction serait un contournement
    /// général de la file. Un appelant qui passerait un identifiant quelconque avec
    /// [`Decision::Resend`] renverrait un message déjà envoyé — exactement le doublon que tout
    /// le reste du chemin existe pour empêcher.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'écriture échoue. Rend `false` — et n'écrit rien — si la
    /// ligne n'existe pas ou n'est pas douteuse.
    ///
    /// ## Pas d'horodatage de la décision
    ///
    /// `tried_at` reste celui de la tentative qui a échoué, et c'est exact : une décision n'est
    /// pas une tentative. Y écrire l'instant de la décision ferait croire à un envoi qui n'a pas
    /// eu lieu — sur un `Accept`, il n'y en a précisément pas eu.
    pub fn resolve_doubt(&self, id: OutboxId, decision: Decision) -> Result<bool> {
        let Some(line) = self.outgoing(id)? else {
            return Ok(false);
        };
        if !line.state.is_doubtful() {
            tracing::warn!(
                job = id.0,
                state = line.state.as_str(),
                "décision refusée : ce message n'est pas douteux"
            );
            return Ok(false);
        }

        let (state, note) = match decision {
            // `retry_after` est remis à NULL : la décision vient d'être prise, l'attente n'a
            // plus de sens. Le compteur de tentatives, lui, est conservé — c'est l'historique.
            Decision::Resend => (
                SendState::Queued,
                "remis en file sur décision de l'utilisateur",
            ),
            Decision::Accept => (
                SendState::Sent,
                "marqué envoyé sur décision de l'utilisateur",
            ),
        };
        self.durably(|conn| {
            conn.execute(
                "UPDATE outbox
                    SET state = ?2, last_error = ?3, retry_after = NULL
                  WHERE id = ?1",
                params![id.0, state.as_str(), note],
            )?;
            Ok(())
        })?;
        // Journalisé en `info` : c'est une décision humaine sur un message, et savoir qu'elle a
        // été prise vaut mieux que de la déduire d'un état.
        tracing::info!(job = id.0, decision = ?decision, "doute tranché");
        Ok(true)
    }
}

/// Une ligne SQL vers un [`Outgoing`].
///
/// L'état est relu par [`SendState::parse`], donc une valeur inconnue est une erreur et non un
/// repli. Le `Result` imbriqué sort du fait que `query_map` ne sait rendre qu'une
/// `rusqlite::Error` : l'erreur de domaine remonte dans la valeur.
fn row_to_outgoing(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Outgoing>> {
    let raw: Vec<u8> = row.get(2)?;
    let label: String = row.get(5)?;
    let recipients: String = row.get(4)?;
    let id = OutboxId(row.get(0)?);
    let account = AccountId(row.get(1)?);
    let sender: String = row.get(3)?;
    let attempts: i64 = row.get(6)?;
    // **Les rangs suivent l'ordre du `SELECT`, pas celui de la table.** La colonne `size` a été
    // insérée au milieu de la liste par la migration v7 : tout ce qui suit a décalé d'un rang.
    // Les quatre requêtes de ce module partagent la même liste de colonnes, mot pour mot, et
    // c'est ce qui rend cette fonction utilisable par les quatre.
    let size: i64 = row.get(7)?;
    let queued_at: i64 = row.get(8)?;
    let tried_at: Option<i64> = row.get(9)?;
    let last_error: Option<String> = row.get(10)?;
    let retry_after: Option<i64> = row.get(11)?;
    // Ajoutée **en fin de liste** par la migration v10, précisément pour ne décaler aucun des
    // rangs ci-dessus — la leçon de `size`, insérée au milieu en v7.
    let resendable: bool = row.get(12)?;

    Ok((|| {
        let bytes: [u8; 32] = raw
            .try_into()
            .map_err(|_| crate::Error::CorruptRow { what: "blob_hash" })?;
        Ok(Outgoing {
            id,
            account,
            blob: BlobHash::from_bytes(bytes),
            size: u64::try_from(size).unwrap_or(0),
            sender,
            // `split` et non `lines` : une chaîne vide donnerait une adresse vide avec `split`,
            // et aucune avec `lines` sur une chaîne vraiment vide — mais `filter` tranche pour
            // les deux. Une adresse ne peut pas être vide, et une file sans destinataire est
            // refusée à l'entrée par `compose`.
            recipients: recipients
                .split('\n')
                .filter(|it| !it.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
            state: SendState::parse(&label)?,
            attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
            queued_at,
            tried_at,
            last_error,
            retry_after,
            resendable,
        })
    })())
}

/// Aplatit les deux niveaux d'erreur de `query_map`.
fn collect(
    rows: impl Iterator<Item = rusqlite::Result<Result<Outgoing>>>,
) -> Result<Vec<Outgoing>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::{AccountKind, SendState};

    fn store() -> (tempfile::TempDir, Store, AccountId) {
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
        (dir, store, account)
    }

    fn queued(store: &Store, account: AccountId) -> OutboxId {
        store
            .enqueue(
                account,
                BlobHash::from_bytes([7_u8; 32]),
                "marie@exemple.fr",
                &[
                    "jean@ailleurs.fr".to_owned(),
                    "cache@ailleurs.fr".to_owned(),
                ],
                // La taille du message assemblé. Les tests de ce module ne la lisent pas ;
                // `SIZE` en dépend, et un zéro veut dire « inconnue ».
                42,
                1_000,
            )
            .unwrap()
    }

    #[test]
    fn a_queued_message_comes_back_whole() {
        let (_dir, store, account) = store();
        let id = queued(&store, account);

        let it = store.outgoing(id).unwrap().expect("la ligne doit exister");
        assert_eq!(it.state, SendState::Queued);
        assert_eq!(it.sender, "marie@exemple.fr");
        assert_eq!(it.recipients, vec!["jean@ailleurs.fr", "cache@ailleurs.fr"]);
        assert_eq!(it.blob.as_bytes(), &[7_u8; 32]);
        assert_eq!(it.attempts, 0);
        assert_eq!(it.queued_at, 1_000);
        assert!(it.tried_at.is_none());
    }

    #[test]
    fn a_doubtful_message_is_never_deliverable() {
        // **La règle du critère 2, au niveau du store.** Un message en `committing` est peut-être
        // parti ; le rendre à la boucle de remise serait un doublon chez le destinataire.
        let (_dir, store, account) = store();
        let id = queued(&store, account);
        store.commit_outgoing(id, SendState::Committing).unwrap();

        assert!(
            store.deliverable(9_999, 10).unwrap().is_empty(),
            "un message douteux a été rendu à la remise"
        );
        let doubtful = store.doubtful().unwrap();
        assert_eq!(doubtful.len(), 1);
        assert_eq!(doubtful[0].id, id);
    }

    #[test]
    fn a_message_interrupted_before_its_body_is_deliverable_again() {
        // Le contrôle négatif du test ci-dessus. `sending` veut dire « l'enveloppe est ouverte,
        // aucun octet du corps n'est parti » : le remettre est sans risque, et **ne pas** le
        // remettre serait une perte.
        let (_dir, store, account) = store();
        let id = queued(&store, account);
        store.commit_outgoing(id, SendState::Sending).unwrap();

        let pending = store.deliverable(9_999, 10).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "un envoi interrompu avant le corps est perdu"
        );
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].state, SendState::Sending);
    }

    #[test]
    fn the_sql_filter_and_the_rust_predicate_agree() {
        // Deux endroits disent quels états sont remettables : la clause `WHERE` de
        // `deliverable` et `SendState::is_deliverable`. S'ils divergent, l'un des deux devient
        // un mensonge — et selon le sens, c'est un doublon ou une perte.
        let (_dir, store, account) = store();
        for state in [
            SendState::Queued,
            SendState::Sending,
            SendState::Committing,
            SendState::Sent,
            SendState::Failed,
        ] {
            let id = queued(&store, account);
            store.commit_outgoing(id, state).unwrap();
            let seen = store
                .deliverable(9_999, 100)
                .unwrap()
                .iter()
                .any(|it| it.id == id);
            assert_eq!(
                seen,
                state.is_deliverable(),
                "{state:?} : le SQL et le prédicat Rust ne disent pas la même chose"
            );
        }
    }

    #[test]
    fn a_retry_deadline_in_the_future_holds_the_message_back() {
        let (_dir, store, account) = store();
        let id = queued(&store, account);
        store
            .record_attempt(
                id,
                SendState::Queued,
                1_100,
                Some("serveur occupé"),
                Some(1_400),
                false,
            )
            .unwrap();

        assert!(store.deliverable(1_399, 10).unwrap().is_empty());
        assert_eq!(store.deliverable(1_400, 10).unwrap().len(), 1);

        let it = store.outgoing(id).unwrap().unwrap();
        assert_eq!(it.attempts, 1);
        assert_eq!(it.tried_at, Some(1_100));
        assert_eq!(it.last_error.as_deref(), Some("serveur occupé"));
    }

    #[test]
    fn the_connection_goes_back_to_normal_after_a_durable_write() {
        // `FULL` laissé en place ne serait pas faux, seulement lent : un import sur la même
        // connexion paierait un `fsync` par transaction. Ce test le voit, la mesure non.
        let (_dir, store, account) = store();
        queued(&store, account);
        let level: i64 = store
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        // 1 = NORMAL, 2 = FULL.
        assert_eq!(level, 1, "la connexion est restée en synchronous FULL");
    }

    #[test]
    fn an_unknown_state_is_refused_rather_than_treated_as_queued() {
        // **Le refus le plus important du module.** Un état illisible traité comme « à
        // envoyer » renverrait un message peut-être déjà parti.
        let (_dir, store, account) = store();
        let id = queued(&store, account);
        store
            .conn
            .execute(
                "UPDATE outbox SET state = 'peut-etre' WHERE id = ?1",
                params![id.0],
            )
            .unwrap();

        let outcome = store.outgoing(id);
        assert!(
            matches!(outcome, Err(crate::Error::UnknownSendState { .. })),
            "un état inconnu n'a pas été refusé"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod decision_tests {
    use super::*;
    use crate::model::AccountKind;

    fn fixture() -> (tempfile::TempDir, Store, OutboxId) {
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
        let id = store
            .enqueue(
                account,
                BlobHash::from_bytes([7_u8; 32]),
                "marie@exemple.fr",
                &["jean@ailleurs.fr".to_owned()],
                // La taille : les tests de la file ne la lisent pas, mais `SIZE` en dépend.
                42,
                1_000,
            )
            .unwrap();
        (dir, store, id)
    }

    #[test]
    fn resending_a_doubtful_message_puts_it_back_in_the_queue() {
        // La sortie qui évite de perdre un message : l'utilisateur a vérifié, il n'est pas
        // arrivé. Sans cette issue, il faut le réécrire.
        let (_dir, store, id) = fixture();
        store.commit_outgoing(id, SendState::Committing).unwrap();

        assert!(store.resolve_doubt(id, Decision::Resend).unwrap());
        let after = store.outgoing(id).unwrap().unwrap();
        assert_eq!(after.state, SendState::Queued);
        assert!(after.retry_after.is_none(), "un recul survit à la décision");
        assert_eq!(store.deliverable(2_000, 10).unwrap().len(), 1);
        assert!(store.doubtful().unwrap().is_empty());
    }

    #[test]
    fn accepting_a_doubtful_message_closes_it_without_sending_anything() {
        // L'autre sortie : l'utilisateur a vérifié, il est arrivé. Sans elle, l'avertissement
        // reste affiché pour toujours — et un avertissement indélébile finit par ne plus être lu.
        let (_dir, store, id) = fixture();
        store.commit_outgoing(id, SendState::Committing).unwrap();

        assert!(store.resolve_doubt(id, Decision::Accept).unwrap());
        let after = store.outgoing(id).unwrap().unwrap();
        assert_eq!(after.state, SendState::Sent);
        assert!(
            store.deliverable(9_999_999, 10).unwrap().is_empty(),
            "un message accepté est reparti"
        );
        assert!(store.doubtful().unwrap().is_empty());
    }

    #[test]
    fn a_state_that_is_not_doubtful_is_refused_and_nothing_is_written() {
        // **Le refus qui empêche cette fonction d'être un contournement de la file.** Sans lui,
        // un `Resend` sur un identifiant quelconque renverrait un message déjà envoyé, ce que
        // tout le reste du chemin existe pour empêcher.
        let (_dir, store, id) = fixture();
        for state in [
            SendState::Queued,
            SendState::Sending,
            SendState::Sent,
            SendState::Failed,
        ] {
            store.commit_outgoing(id, state).unwrap();
            for decision in [Decision::Resend, Decision::Accept] {
                assert!(
                    !store.resolve_doubt(id, decision).unwrap(),
                    "{state:?} + {decision:?} a été accepté"
                );
                assert_eq!(
                    store.outgoing(id).unwrap().unwrap().state,
                    state,
                    "l'état a bougé alors que la décision était refusée"
                );
            }
        }
    }

    #[test]
    fn retrying_a_failed_message_puts_it_back_with_a_fresh_credit() {
        // Le geste que la phrase du critère 8 demande — « renvoyez le message plus tard » — et
        // qui n'existait pas : la seule autre sortie de `failed` était `forget_outgoing`, qui
        // jette le message.
        let (_dir, store, id) = fixture();
        store
            .record_attempt(id, SendState::Failed, 1_200, Some("occupé"), None, true)
            .unwrap();
        let dead = store.outgoing(id).unwrap().unwrap();
        assert_eq!(dead.attempts, 1);
        assert!(dead.resendable);

        assert!(store.retry_outgoing(id).unwrap());
        let back = store.outgoing(id).unwrap().unwrap();
        assert_eq!(back.state, SendState::Queued);
        // **Le crédit repart de zéro**, sinon une ligne épuisée échouerait au premier refus
        // passager du renvoi. Voir la documentation de `retry_outgoing`.
        assert_eq!(back.attempts, 0);
        assert!(back.retry_after.is_none(), "un recul survit au renvoi");
        // Et le bit s'éteint : il ne décrit plus rien, la ligne n'a pas encore échoué.
        assert!(!back.resendable);
        assert_eq!(store.deliverable(1_300, 10).unwrap().len(), 1);
    }

    #[test]
    fn only_a_failed_message_can_be_retried() {
        // Le même refus que `resolve_doubt`, pour la même raison : sans lui, cette fonction
        // renverrait un message déjà `sent`, ou remettrait en file un `committing` — dont le
        // serveur a peut-être la copie. Le doublon que le critère 2 existe pour empêcher.
        let (_dir, store, id) = fixture();
        for state in [
            SendState::Queued,
            SendState::Sending,
            SendState::Committing,
            SendState::Sent,
        ] {
            store.commit_outgoing(id, state).unwrap();
            assert!(
                !store.retry_outgoing(id).unwrap(),
                "{state:?} a été accepté au renvoi"
            );
            assert_eq!(
                store.outgoing(id).unwrap().unwrap().state,
                state,
                "l'état a bougé alors que le renvoi était refusé"
            );
        }
    }

    #[test]
    fn retrying_a_line_that_does_not_exist_is_not_a_panic() {
        let (_dir, store, _id) = fixture();
        assert!(!store.retry_outgoing(OutboxId(9_999)).unwrap());
    }

    #[test]
    fn a_queued_line_is_never_resendable() {
        // Le bit n'a de sens que sur une ligne finie. Une ligne neuve le porterait à faux, et
        // un client qui offrirait « Renvoyer » sur un message encore en file ferait renvoyer
        // ce qui n'est pas encore parti.
        let (_dir, store, id) = fixture();
        assert!(!store.outgoing(id).unwrap().unwrap().resendable);
    }

    #[test]
    fn an_unknown_id_is_refused_rather_than_created() {
        let (_dir, store, _id) = fixture();
        assert!(
            !store
                .resolve_doubt(OutboxId(4_242), Decision::Resend)
                .unwrap()
        );
    }

    #[test]
    fn the_decision_is_readable_by_the_user_afterwards() {
        // Le texte remplace l'erreur d'origine : ce qui compte désormais est que la décision a
        // été prise, pas la coupure qui l'a provoquée — critère 8.
        let (_dir, store, id) = fixture();
        store.commit_outgoing(id, SendState::Committing).unwrap();
        store.resolve_doubt(id, Decision::Accept).unwrap();

        let note = store.outgoing(id).unwrap().unwrap().last_error.unwrap();
        assert!(note.contains("utilisateur"), "{note}");
    }

    #[test]
    fn a_label_that_is_neither_is_refused_rather_than_guessed() {
        // Les deux décisions sont irréversibles dans un sens différent : un repli choisirait à
        // la place de l'utilisateur.
        assert_eq!(Decision::parse("resend").unwrap(), Decision::Resend);
        assert_eq!(Decision::parse("accept").unwrap(), Decision::Accept);
        assert!(Decision::parse("peut-etre").is_err());
        assert!(Decision::parse("").is_err());
        assert!(
            Decision::parse("RESEND").is_err(),
            "la casse n'est pas un repli"
        );
    }
}

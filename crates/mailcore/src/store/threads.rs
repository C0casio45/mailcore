//! Ce que la passe de threading lit et écrit dans le store.
//!
//! ## Trois faits dérivés, rangés pour ne pas relire les blobs
//!
//! `thread::rebuild` relit **tous** les blobs pour en tirer deux choses : le sujet normalisé et
//! les identifiants référencés par `In-Reply-To` et `References`. C'est ce qui coûte — 334 s à
//! froid sur le corpus, voir `docs/PHASE-1.md` — et c'était la seule raison pour laquelle les
//! fils ne pouvaient pas suivre la moisson.
//!
//! Depuis `SCHEMA_V13`, ces faits sont **rangés dans le store** au moment où un message est
//! rattaché : `messages.subject_norm`, la table `message_references`, et `messages.thread_link`
//! qui retient **comment** le message a été rattaché. Avec eux, « qui est concerné par l'arrivée
//! de ce message ? » se répond en SQL sur des colonnes indexées, et un blob n'est lu qu'une fois
//! dans la vie d'un message.
//!
//! ## Rien ici ne décide
//!
//! La règle de fil — qui va avec qui — vit dans [`crate::thread::partition`], à un seul endroit,
//! et les deux passes l'appellent. Ce module ne fait que lire des lignes et en écrire.

use rusqlite::params;

use crate::error::Result;
use crate::model::{MessageId, ThreadId};
use crate::store::Store;
use crate::store::read::{ThreadRow, blob_hash};
use crate::store::write::Writer;
use crate::thread::Node;

/// Comment un message a été rattaché à son fil.
///
/// Le repli par sujet n'est ouvert qu'aux messages qu'aucune référence n'a rattachés — c'est le
/// garde-fou écrit dans [`crate::thread`]. Sans ce fait rangé, retrouver un groupe de sujet
/// demanderait de recalculer la résolution de **toutes** les références du store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadLink {
    /// Au moins une référence résolue, vers lui ou depuis lui. Pas éligible au repli par sujet.
    ByReference,
    /// Aucune référence résolue : seul dans son fil, ou réuni à d'autres par le sujet.
    WithoutReference,
}

impl ThreadLink {
    /// La valeur rangée en base. Explicite plutôt qu'un booléen : un troisième mode de
    /// rattachement se lirait dans les données au lieu de les réinterpréter.
    #[must_use]
    pub const fn to_sql(self) -> i64 {
        match self {
            Self::ByReference => 1,
            Self::WithoutReference => 2,
        }
    }

    fn from_sql(raw: Option<i64>) -> Option<Self> {
        match raw {
            Some(1) => Some(Self::ByReference),
            Some(2) => Some(Self::WithoutReference),
            _ => None,
        }
    }
}

/// Un message tel que la règle de fil le voit, plus son état actuel dans le store.
#[derive(Debug, Clone)]
pub struct ThreadNode {
    /// Ce que la règle de fil lit. Le même type des deux côtés : la passe complète le
    /// construit depuis un blob, la passe locale depuis ces colonnes.
    pub node: Node,
    /// Le fil actuel, s'il en a un.
    pub thread: Option<ThreadId>,
    /// Comment il y a été rattaché, si le store le sait.
    pub link: Option<ThreadLink>,
}

/// Combien d'identifiants une clause `IN` reçoit à la fois.
///
/// SQLite plafonne les paramètres à 32 766 depuis la 3.32 ; rester très en dessous garde chaque
/// requête petite et son plan simple.
const CHUNK: usize = 400;

impl Store {
    /// Combien de messages n'ont pas encore de fil.
    ///
    /// La question la moins chère, et c'est pour ça qu'elle existe : un `COUNT` sur l'index
    /// partiel `messages_unthreaded`. La leçon du 2026-09-11 sur l'index plein texte — ouvrir ce
    /// qu'il faut pour travailler **après** avoir demandé s'il y a du travail.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn unthreaded_total(&self) -> Result<u64> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM messages WHERE thread_id IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count.unsigned_abs())
    }

    /// Les messages sans fil, du plus ancien au plus récent, au plus `limit`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn unthreaded(&self, limit: usize) -> Result<Vec<ThreadRow>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT id, blob_hash, message_id, date, subject
             FROM messages WHERE thread_id IS NULL ORDER BY date, id LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            let hash: Vec<u8> = row.get(1)?;
            Ok(ThreadRow {
                id: MessageId(row.get(0)?),
                blob: blob_hash(&hash),
                rfc822_id: row.get(2)?,
                date: row.get(3)?,
                subject: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Combien de messages ont un fil mais pas les faits qui permettent de le prolonger.
    ///
    /// Non nul sur un store rattaché **avant** `SCHEMA_V13` : ses fils sont justes, mais rien ne
    /// dit qui répond à quoi. La passe locale n'aurait alors aucun moyen de trouver le voisinage
    /// d'un message qui arrive — elle refait donc une passe complète, une fois, et le store est
    /// équipé pour toujours.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn threaded_without_facts(&self) -> Result<u64> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM messages WHERE thread_link IS NULL AND thread_id IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count.unsigned_abs())
    }

    /// Les messages d'un fil.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn thread_member_ids(&self, thread: ThreadId) -> Result<Vec<MessageId>> {
        self.message_ids(
            "SELECT id FROM messages WHERE thread_id = ?1",
            params![thread.0],
        )
    }

    /// Les messages **déjà rattachés** qui portent cet en-tête `Message-ID`.
    ///
    /// Plusieurs, parfois : l'identifiant vient du réseau, et des expéditeurs réutilisent le
    /// même. La règle de fil choisit le plus ancien ; il faut donc les rendre tous pour qu'elle
    /// ait le choix.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn threaded_carriers_of(&self, rfc822_id: &str) -> Result<Vec<MessageId>> {
        self.message_ids(
            "SELECT id FROM messages WHERE message_id = ?1 AND thread_id IS NOT NULL",
            params![rfc822_id],
        )
    }

    /// Les messages **déjà rattachés** qui référencent cet identifiant.
    ///
    /// C'est la question que les blobs ne permettent pas de poser sans tout relire : « qui répond
    /// à ce message ? » se lit dans les en-têtes des **autres**. La table `message_references`
    /// existe pour elle, et son index aussi.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn threaded_referencing(&self, rfc822_id: &str) -> Result<Vec<MessageId>> {
        self.message_ids(
            "SELECT r.message_id FROM message_references r
             JOIN messages m ON m.id = r.message_id
             WHERE r.rfc822_id = ?1 AND m.thread_id IS NOT NULL",
            params![rfc822_id],
        )
    }

    /// Les messages déjà rattachés **sans référence résolue** qui portent ce sujet normalisé,
    /// au plus `limit`.
    ///
    /// La borne est ce qui permet de répondre à « ce groupe dépasse-t-il le plafond ? » sans
    /// rapporter les cinq mille messages d'une newsletter.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn unlinked_with_subject(
        &self,
        subject_norm: &str,
        limit: usize,
    ) -> Result<Vec<MessageId>> {
        self.message_ids(
            "SELECT id FROM messages
             WHERE subject_norm = ?1 AND thread_link = ?2 AND thread_id IS NOT NULL
             LIMIT ?3",
            params![
                subject_norm,
                ThreadLink::WithoutReference.to_sql(),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
        )
    }

    /// Les nœuds de ces messages, faits dérivés compris, triés par date puis par identifiant.
    ///
    /// L'ordre est celui dont la règle de fil a besoin : la racine d'un fil est son message le
    /// plus ancien, et c'est aussi lui qui gagne quand deux messages portent le même
    /// `Message-ID`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn thread_nodes(&self, ids: &[MessageId]) -> Result<Vec<ThreadNode>> {
        let mut nodes: Vec<ThreadNode> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(CHUNK) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT id, message_id, date, subject_norm, thread_id, thread_link
                 FROM messages WHERE id IN ({placeholders})"
            );
            let mut statement = self.connection().prepare(&sql)?;
            let bound = rusqlite::params_from_iter(chunk.iter().map(|id| id.0));
            let rows = statement.query_map(bound, |row| {
                Ok(ThreadNode {
                    node: Node {
                        id: MessageId(row.get(0)?),
                        rfc822_id: row.get(1)?,
                        date: row.get(2)?,
                        // Un message rattaché avant `SCHEMA_V13` n'a pas de sujet rangé. La
                        // chaîne vide est le bon défaut : elle est plus courte que
                        // `MIN_SUBJECT_LEN`, donc elle ne rejoint aucun groupe — exactement ce
                        // que fait un sujet trop générique.
                        subject_norm: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        references: Vec::new(),
                    },
                    thread: row.get::<_, Option<i64>>(4)?.map(ThreadId),
                    link: ThreadLink::from_sql(row.get(5)?),
                })
            })?;
            nodes.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
        }

        let mut references = self.connection().prepare_cached(
            "SELECT rfc822_id FROM message_references WHERE message_id = ?1 ORDER BY rowid",
        )?;
        for entry in &mut nodes {
            let rows = references.query_map([entry.node.id.0], |row| row.get::<_, String>(0))?;
            entry.node.references = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        }

        nodes.sort_by_key(|entry| (entry.node.date, entry.node.id));
        Ok(nodes)
    }

    /// Le corps commun des requêtes qui ne rendent que des identifiants.
    fn message_ids(&self, sql: &str, bound: impl rusqlite::Params) -> Result<Vec<MessageId>> {
        let mut statement = self.connection().prepare_cached(sql)?;
        let rows = statement.query_map(bound, |row| Ok(MessageId(row.get(0)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

impl Writer<'_> {
    /// Range les faits dérivés d'un message : son sujet normalisé et ce qu'il référence.
    ///
    /// **Idempotente** : les références existantes sont retirées avant d'être réécrites, donc
    /// repasser sur un message ne double rien. C'est ce qui rend l'ordre des écritures sûr, et
    /// c'est le même raisonnement que `delete_term` dans l'index plein texte — une écriture
    /// qu'on peut refaire est une écriture dont la reprise après coupure ne se discute pas.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn record_thread_facts(
        &self,
        message: MessageId,
        subject_norm: &str,
        references: &[String],
    ) -> Result<()> {
        self.transaction()
            .prepare_cached("UPDATE messages SET subject_norm = ?2 WHERE id = ?1")?
            .execute(params![message.0, subject_norm])?;
        self.transaction()
            .prepare_cached("DELETE FROM message_references WHERE message_id = ?1")?
            .execute(params![message.0])?;
        let mut insert = self.transaction().prepare_cached(
            "INSERT INTO message_references (message_id, rfc822_id) VALUES (?1, ?2)",
        )?;
        for reference in references {
            insert.execute(params![message.0, reference])?;
        }
        Ok(())
    }

    /// Retient comment un message a été rattaché.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn set_thread_link(&self, message: MessageId, link: ThreadLink) -> Result<()> {
        self.transaction()
            .prepare_cached("UPDATE messages SET thread_link = ?2 WHERE id = ?1")?
            .execute(params![message.0, link.to_sql()])?;
        Ok(())
    }

    /// Met à jour l'en-tête d'un fil : sa racine, son sujet, sa date et son compte.
    ///
    /// Réécrire plutôt que recréer garde l'identifiant stable, et un identifiant de fil stable
    /// est ce qui permet à une interface de rester sur le fil qu'elle affichait quand une
    /// réponse arrive.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn update_thread(
        &self,
        thread: ThreadId,
        root: MessageId,
        subject_norm: &str,
        last_date: i64,
        message_count: u32,
    ) -> Result<()> {
        self.transaction()
            .prepare_cached(
                "UPDATE threads SET root_message_id = ?2, subject_norm = ?3,
                                    last_date = ?4, message_count = ?5
                 WHERE id = ?1",
            )?
            .execute(params![
                thread.0,
                root.0,
                subject_norm,
                last_date,
                message_count
            ])?;
        Ok(())
    }

    /// Supprime un fil dont plus aucun message ne se réclame.
    ///
    /// Appelée seulement après que tous ses messages ont été rattachés ailleurs. Un fil encore
    /// désigné laisserait des `thread_id` pendants : c'est un défaut de la passe, et le test
    /// `no_message_points_at_a_deleted_thread` le ferait tomber.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn delete_thread(&self, thread: ThreadId) -> Result<()> {
        self.transaction()
            .prepare_cached("DELETE FROM threads WHERE id = ?1")?
            .execute(params![thread.0])?;
        Ok(())
    }
}

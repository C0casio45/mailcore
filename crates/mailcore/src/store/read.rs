//! La lecture de l'index de métadonnées.
//!
//! Volontairement maigre : ce qu'il faut pour vérifier un import, alimenter `mail stats` et
//! `mail doctor`. L'API de requête complète — recherche, fils, historique d'un contact — vit
//! dans `query.rs` à l'étape 5.
//!
//! Rien ici ne rend un type de `rusqlite`. C'est la frontière : le jour où le moteur change,
//! ce module est réécrit et personne d'autre ne s'en aperçoit.

use rusqlite::{OptionalExtension, params};

use crate::error::Result;
use crate::model::{BlobHash, Folder, FolderId, FolderKind, MessageFlags, MessageId, ThreadId};
use crate::store::Store;

/// Une ligne de liste : ce qu'il faut pour afficher un message sans ouvrir son blob.
///
/// Assez petite pour qu'en paginer 100 000 reste indolore — critère 2.
#[derive(Debug, Clone)]
pub struct ListItem {
    /// Identifiant interne.
    pub id: MessageId,
    /// L'identité du contenu, pour aller lire le corps.
    pub blob: BlobHash,
    /// Date en secondes Unix.
    pub date: i64,
    /// Adresse de l'expéditeur, en minuscules.
    pub from_addr: String,
    /// Nom affiché de l'expéditeur.
    pub from_name: Option<String>,
    /// Sujet décodé.
    pub subject: String,
    /// Vrai si le message déclare des pièces jointes.
    pub has_attachments: bool,
    /// Les drapeaux **de cette référence**, pas du message.
    pub flags: MessageFlags,
}

/// Tout ce dont l'indexation a besoin pour un message, en une ligne.
///
/// Les dossiers sont pré-joints en une chaîne : l'index plein texte veut un champ texte, et
/// faire une requête par message pour ses dossiers multiplierait par deux le nombre d'allers
/// vers SQLite sur 73 000 messages.
#[derive(Debug, Clone)]
pub struct IndexRow {
    /// Identifiant interne.
    pub id: MessageId,
    /// L'identité du contenu, pour aller relire le corps.
    pub blob: BlobHash,
    /// Sujet décodé.
    pub subject: String,
    /// Adresse de l'expéditeur.
    pub from_addr: String,
    /// Nom affiché de l'expéditeur.
    pub from_name: Option<String>,
    /// Date en secondes Unix.
    pub date: i64,
    /// Les chemins des dossiers qui référencent ce message, séparés par une espace.
    pub folders: String,
}

/// Ce dont la passe de threading a besoin pour un message.
#[derive(Debug, Clone)]
pub struct ThreadRow {
    /// Identifiant interne.
    pub id: MessageId,
    /// L'identité du contenu, pour relire `References` et `In-Reply-To`.
    pub blob: BlobHash,
    /// L'en-tête `Message-ID`, s'il était présent.
    pub rfc822_id: Option<String>,
    /// Date en secondes Unix.
    pub date: i64,
    /// Sujet décodé, avant normalisation.
    pub subject: String,
}
/// Le curseur de pagination : la position du dernier élément rendu.
///
/// `(date, id)` et pas seulement `date` : plusieurs messages partagent la même seconde, et
/// paginer sur une clé non unique saute ou répète des lignes à la frontière des pages.
pub type Cursor = (i64, MessageId);

/// L'état du store, pour `mail stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Comptes connus.
    pub accounts: u64,
    /// Dossiers connus.
    pub folders: u64,
    /// Contenus distincts stockés.
    pub messages: u64,
    /// Références — toujours ≥ `messages` si la dédup fonctionne.
    pub refs: u64,
    /// Copies côté serveur : une par UID connu.
    ///
    /// Toujours ≥ `refs` restreint aux dossiers synchronisés, et l'écart est le nombre de
    /// contenus qu'un serveur détient en double dans un même dossier. Zéro sur un store qui
    /// ne vient que d'un import mbox.
    pub copies: u64,
    /// Messages que la passe de threading n'a pas encore traités.
    pub unthreaded: u64,
    /// Somme des tailles RFC 5322, avant compression.
    pub raw_bytes: u64,
}

impl StoreStats {
    /// Références par message. Au-dessus de 1, la dédup a servi à quelque chose.
    #[must_use]
    pub fn refs_per_message(&self) -> f64 {
        if self.messages == 0 {
            return 0.0;
        }
        self.refs as f64 / self.messages as f64
    }
}

impl Store {
    /// Tous les dossiers, triés par compte puis par chemin.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn folders(&self) -> Result<Vec<Folder>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT id, account_id, path, kind FROM folders ORDER BY account_id, path",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Folder {
                id: FolderId(row.get(0)?),
                account: crate::model::AccountId(row.get(1)?),
                path: row.get(2)?,
                kind: FolderKind::from_str_lossy(&row.get::<_, String>(3)?),
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Une page de messages d'un dossier, du plus récent au plus ancien.
    ///
    /// `after` est le curseur de la page précédente ; `None` pour la première page.
    ///
    /// **Pagination par clé, jamais `OFFSET`.** Un `OFFSET 50000` demande à SQLite de lire
    /// 50 000 lignes pour en jeter 49 950 ; la page 1000 coûterait alors mille fois la
    /// page 1. Ici toutes les pages coûtent la même chose, et c'est ce qui rend le critère 2
    /// atteignable.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn page(
        &self,
        folder: FolderId,
        after: Option<Cursor>,
        limit: u32,
    ) -> Result<Vec<ListItem>> {
        // Le curseur absent est représenté par la borne haute : `(date, id) < (MAX, MAX)`
        // est vrai pour toute ligne, donc une seule requête sert les deux cas et le plan
        // d'exécution reste identique d'une page à l'autre.
        let (date, id) = after.map_or((i64::MAX, i64::MAX), |(d, i)| (d, i.0));

        let mut statement = self.connection().prepare_cached(
            "SELECT m.id, m.blob_hash, r.date, m.from_addr, m.from_name, m.subject,
                    m.has_attachments, r.flags
             FROM refs r
             JOIN messages m ON m.id = r.message_id
             WHERE r.folder_id = ?1 AND (r.date, r.message_id) < (?2, ?3)
             ORDER BY r.date DESC, r.message_id DESC
             LIMIT ?4",
        )?;

        let rows = statement.query_map(params![folder.0, date, id, limit], |row| {
            let hash: Vec<u8> = row.get(1)?;
            Ok(ListItem {
                id: MessageId(row.get(0)?),
                blob: blob_hash(&hash),
                date: row.get(2)?,
                from_addr: row.get(3)?,
                from_name: row.get(4)?,
                subject: row.get(5)?,
                has_attachments: row.get::<_, i64>(6)? != 0,
                flags: MessageFlags::from_bits_truncate(row.get(7)?),
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Le curseur à passer pour obtenir la page suivante.
    #[must_use]
    pub fn next_cursor(page: &[ListItem]) -> Option<Cursor> {
        page.last().map(|item| (item.date, item.id))
    }

    /// Un message par son identifiant, sans ses drapeaux.
    ///
    /// Les drapeaux appartiennent à une référence, donc à un dossier : un message vu hors
    /// d'un dossier — un résultat de recherche, par exemple — n'en a pas un jeu unique.
    /// [`ListItem::flags`] est alors vide, et c'est honnête plutôt que d'en inventer.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn message(&self, id: MessageId) -> Result<Option<ListItem>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT id, blob_hash, date, from_addr, from_name, subject, has_attachments
             FROM messages WHERE id = ?1",
        )?;
        let found = statement
            .query_row([id.0], |row| {
                let hash: Vec<u8> = row.get(1)?;
                Ok(ListItem {
                    id: MessageId(row.get(0)?),
                    blob: blob_hash(&hash),
                    date: row.get(2)?,
                    from_addr: row.get(3)?,
                    from_name: row.get(4)?,
                    subject: row.get(5)?,
                    has_attachments: row.get::<_, i64>(6)? != 0,
                    flags: MessageFlags::empty(),
                })
            })
            .optional()?;
        Ok(found)
    }

    /// Les messages que l'index plein texte n'a pas encore vus, au plus `limit`.
    ///
    /// Même forme que `uncounted_for_contacts`, et même raison : la passe tourne après chaque
    /// moisson, donc elle ne doit pas charger le corpus pour trois messages.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn unindexed(&self, limit: usize) -> Result<Vec<IndexRow>> {
        // Les dossiers sont joints comme dans `all_for_indexing` : le champ `folder` de l'index
        // les contient, et une passe incrémentale qui les omettrait rendrait des documents
        // différents de ceux d'une reconstruction.
        let mut statement = self.connection().prepare_cached(
            "SELECT m.id, m.blob_hash, m.subject, m.from_addr, m.from_name, m.date,
                    COALESCE(GROUP_CONCAT(f.path, ' '), '')
             FROM messages m
             LEFT JOIN refs r ON r.message_id = m.id
             LEFT JOIN folders f ON f.id = r.folder_id
             WHERE m.indexed = 0
             GROUP BY m.id
             ORDER BY m.id
             LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], index_row)?;
        collect(rows)
    }

    /// Combien de messages restent à indexer.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn unindexed_total(&self) -> Result<u64> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM messages WHERE indexed = 0",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Combien de messages se disent indexés.
    ///
    /// Sert au seul contrôle de cohérence entre les deux magasins : un index vide alors que des
    /// messages se disent indexés veut dire que l'index a été recréé sous les pieds du store.
    /// Voir `SCHEMA_V12`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn indexed_count(&self) -> Result<u64> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM messages WHERE indexed = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Marque des messages comme indexés.
    ///
    /// **À n'appeler qu'après le `commit` de l'index.** L'ordre est celui qui rend une coupure
    /// sans dommage : un lot indexé puis non marqué sera réindexé — sans effet, l'écriture
    /// remplaçant le document par son identifiant — alors qu'un lot marqué puis non indexé
    /// serait introuvable pour toujours.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn mark_indexed(&self, ids: &[MessageId]) -> Result<()> {
        let tx = self.connection().unchecked_transaction()?;
        {
            let mut statement =
                tx.prepare_cached("UPDATE messages SET indexed = 1 WHERE id = ?1")?;
            for id in ids {
                statement.execute([id.0])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Remet tous les messages à « pas encore indexé ».
    ///
    /// Le premier pas d'une reconstruction, et le remède quand l'index a été recréé sans que le
    /// store le sache.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn reset_indexed(&self) -> Result<()> {
        self.connection()
            .execute("UPDATE messages SET indexed = 0", [])?;
        Ok(())
    }

    /// La taille des octets RFC 5322 d'un message, telle que l'import l'a relevée.
    ///
    /// Séparée de [`Self::message`] parce qu'un seul appelant en a besoin — la vue « source du
    /// message », qui doit dire de combien elle tronque — et que l'ajouter à [`ListItem`] la
    /// ferait lire par la pagination, qui la traverse cent mille fois sans jamais s'en servir.
    ///
    /// La relire du blob coûterait une décompression complète pour compter des octets que
    /// l'index connaît déjà.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn message_size(&self, id: MessageId) -> Result<Option<u64>> {
        let mut statement = self
            .connection()
            .prepare_cached("SELECT size FROM messages WHERE id = ?1")?;
        let found = statement
            .query_row([id.0], |row| row.get::<_, i64>(0))
            .optional()?;
        Ok(found.map(|size| u64::try_from(size).unwrap_or(0)))
    }

    /// Tout ce dont la passe de threading a besoin, dans l'ordre chronologique.
    ///
    /// Trié par date puis par identifiant : la racine d'un fil est son message le plus
    /// ancien, et lire dans l'ordre évite de trier 73 000 lignes en mémoire ensuite.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn all_for_threading(&self) -> Result<Vec<ThreadRow>> {
        let mut statement = self.connection().prepare(
            "SELECT id, blob_hash, message_id, date, subject
             FROM messages ORDER BY date, id",
        )?;
        let rows = statement.query_map([], |row| {
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
    /// Références pointant vers un message qui n'existe pas.
    ///
    /// Devrait toujours valoir zéro : la clé étrangère l'interdit. Le vérifier quand même,
    /// parce qu'un store ouvert par un binaire qui aurait oublié `PRAGMA foreign_keys` peut
    /// en avoir créé, et qu'une incohérence silencieuse est pire qu'une contrainte violée.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn dangling_refs(&self) -> Result<u64> {
        let value: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM refs r
             WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = r.message_id)",
            [],
            |row| row.get(0),
        )?;
        Ok(value.unsigned_abs())
    }

    /// Les blobs présents sur le disque que **plus aucun message ne désigne**.
    ///
    /// ## Le symétrique de [`Store::missing_blobs`], et il manquait
    ///
    /// `missing_blobs` répond à « un message dont le contenu a disparu ». Celui-ci répond à
    /// l'inverse : « un contenu que rien ne réclame ». Les deux sont possibles parce que les
    /// blobs vivent **hors** de SQLite, donc hors de ses transactions — un `ROLLBACK` n'efface
    /// pas un fichier.
    ///
    /// Trouvé manquant le 2026-09-09, en vérifiant une affirmation écrite dans `mailsync` :
    /// elle disait que `mail doctor` savait déjà les compter. Il ne savait compter que les
    /// *références* orphelines.
    ///
    /// ## Ce que ça ne fait pas
    ///
    /// Ça compte, ça n'efface rien. Un blob non référencé n'est pas une perte de courrier —
    /// c'est de la place — et le supprimer demande de décider qu'aucun import en cours ne va le
    /// réclamer dans la seconde. Le ramassage sera une action explicite.
    ///
    /// ## Le coût
    ///
    /// Un passage sur les noms de fichiers — aucun contenu lu — et un ensemble d'empreintes en
    /// mémoire : 32 octets par message, soit ~1,5 Mo pour les 48 532 du corpus réel. C'est le
    /// même ordre que le `Vec` de `all_for_indexing`, et c'est assumé pour la même raison :
    /// comparer deux ensembles demande d'en tenir un.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'index est illisible, [`crate::Error::Io`] si
    /// l'arborescence des blobs l'est.
    pub fn orphan_blobs(&self) -> Result<u64> {
        // **Les deux tables, et la seconde a été oubliée pendant une journée.**
        //
        // Cette fonction ne lisait que `messages`, ce qui était complet quand elle a été
        // écrite : rien d'autre ne désignait un blob. La file d'envoi en désigne aussi, et le
        // décalage s'est vu à la première purge sur le store réel — 6 orphelins comptés,
        // 4 supprimés, parce que `purge_orphan_blobs` regardait bien les deux.
        //
        // Le sens de l'erreur était le bon : compter en trop fait annoncer de la place qu'on
        // n'a pas, l'inverse aurait fait purger des messages en attente d'envoi. Mais deux
        // définitions d'« orphelin » dans le même module sont un mensonge en attente.
        let mut known: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();
        for table in ["messages", "outbox"] {
            let sql = format!("SELECT blob_hash FROM {table}");
            let mut statement = self.connection().prepare(&sql)?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let bytes: Vec<u8> = row.get(0)?;
                let mut buffer = [0_u8; 32];
                let len = bytes.len().min(32);
                buffer[..len].copy_from_slice(&bytes[..len]);
                known.insert(buffer);
            }
        }

        let mut orphans = 0_u64;
        self.blobs.for_each_hash(&mut |hash| {
            if !known.contains(hash.as_bytes()) {
                orphans += 1;
            }
            Ok(())
        })?;
        Ok(orphans)
    }

    /// Les messages dont le blob a disparu du système de fichiers.
    ///
    /// Aucune contrainte ne peut l'empêcher : les blobs vivent hors de SQLite. C'est
    /// exactement l'état que la position de durabilité de `store::blobs` accepte — un blob
    /// perdu sur coupure d'alimentation est détectable ici et réparable par réimport.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn missing_blobs(&self) -> Result<Vec<MessageId>> {
        let mut statement = self
            .connection()
            .prepare("SELECT id, blob_hash FROM messages ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            let hash: Vec<u8> = row.get(1)?;
            Ok((MessageId(row.get(0)?), blob_hash(&hash)))
        })?;

        let mut missing = Vec::new();
        for row in rows {
            let (id, hash) = row?;
            if !self.blobs().contains(hash)? {
                missing.push(id);
            }
        }
        Ok(missing)
    }

    /// Le fil auquel appartient un message, s'il a été threadé.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn thread_of(&self, message: MessageId) -> Result<Option<ThreadId>> {
        let found: Option<Option<i64>> = self
            .connection()
            .prepare_cached("SELECT thread_id FROM messages WHERE id = ?1")?
            .query_row([message.0], |row| row.get(0))
            .optional()?;
        Ok(found.flatten().map(ThreadId))
    }

    /// Les messages d'un fil, du plus ancien au plus récent.
    ///
    /// Chronologique et non par pertinence : un fil se lit dans l'ordre où il s'est écrit.
    /// Les drapeaux sont vides — ils appartiennent à une référence, donc à un dossier, et un
    /// fil traverse les dossiers.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn thread_messages(&self, thread: ThreadId) -> Result<Vec<ListItem>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT id, blob_hash, date, from_addr, from_name, subject, has_attachments
             FROM messages WHERE thread_id = ?1 ORDER BY date, id",
        )?;
        let rows = statement.query_map([thread.0], |row| {
            let hash: Vec<u8> = row.get(1)?;
            Ok(ListItem {
                id: MessageId(row.get(0)?),
                blob: blob_hash(&hash),
                date: row.get(2)?,
                from_addr: row.get(3)?,
                from_name: row.get(4)?,
                subject: row.get(5)?,
                has_attachments: row.get::<_, i64>(6)? != 0,
                flags: MessageFlags::empty(),
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Les dossiers qui référencent un message, avec les drapeaux propres à chacun.
    ///
    /// Un message présent dans `INBOX` et dans l'archive rend deux entrées — c'est
    /// exactement ce que la dédup rend visible, et un client a besoin de le savoir.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn folders_of(&self, message: MessageId) -> Result<Vec<(String, MessageFlags)>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT f.path, r.flags FROM refs r
             JOIN folders f ON f.id = r.folder_id
             WHERE r.message_id = ?1 ORDER BY f.path",
        )?;
        let rows = statement.query_map([message.0], |row| {
            Ok((
                row.get::<_, String>(0)?,
                MessageFlags::from_bits_truncate(row.get(1)?),
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Les comptes connus : identifiant, type, nom affiché.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn accounts(&self) -> Result<Vec<(crate::model::AccountId, String, String)>> {
        let mut statement = self
            .connection()
            .prepare_cached("SELECT id, kind, display_name FROM accounts ORDER BY display_name")?;
        let rows = statement.query_map([], |row| {
            Ok((
                crate::model::AccountId(row.get(0)?),
                row.get(1)?,
                row.get(2)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// La signature d'un compte, si elle est lisible.
    ///
    /// ## Une colonne illisible rend `None`, et n'est pas une erreur
    ///
    /// Trois cas donnent `None` et se ressemblent volontairement : le compte n'a pas de
    /// signature, le compte n'existe pas, la colonne porte quelque chose que ce binaire ne sait
    /// pas relire. Le troisième est le seul intéressant, et il est traité comme les deux autres
    /// pour une raison précise : **une signature abîmée ne doit pas empêcher d'écrire un
    /// message.** Refuser ici ferait échouer la fenêtre de rédaction sur un champ décoratif.
    ///
    /// Ce qui est relu est passé par `normalise` : le document peut venir d'octets que ce code
    /// n'a pas écrits, donc ses invariants — une nature par ligne, des intervalles triés et
    /// disjoints — sont rétablis plutôt que supposés.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si le store est illisible. Le contenu de la colonne, lui, ne
    /// produit jamais d'erreur.
    pub fn signature(
        &self,
        account: crate::model::AccountId,
    ) -> Result<Option<mailhtml::rich::Document>> {
        let rangee: Option<Option<String>> = self
            .connection()
            .prepare_cached("SELECT signature FROM accounts WHERE id = ?1")?
            .query_row(rusqlite::params![account.0], |row| row.get(0))
            .optional()?;
        let Some(Some(rangee)) = rangee else {
            return Ok(None);
        };
        let Ok(mut document) = serde_json::from_str::<mailhtml::rich::Document>(&rangee) else {
            tracing::warn!(
                account = account.0,
                "signature illisible dans le store, ignorée"
            );
            return Ok(None);
        };
        document.normalise();
        Ok((!document.is_empty()).then_some(document))
    }

    /// Les comptes, en entier, serveur compris — **jamais le secret**.
    ///
    /// ## La cohérence entre `kind` et les colonnes de serveur est vérifiée ici
    ///
    /// Le schéma laisse `host`, `port` et le reste nullables : il le faut, un compte mbox n'a
    /// pas de serveur. Rien n'empêche donc, au niveau de SQLite, un compte `imap` sans hôte —
    /// une insertion incomplète, une migration bricolée à la main.
    ///
    /// Cette fonction **refuse** un tel compte ([`crate::Error::InconsistentAccount`]) au lieu
    /// de rendre un `Server` avec un hôte vide. Un hôte vide finirait par être passé à un
    /// résolveur, et « échouer à la connexion » est un diagnostic beaucoup moins utile que
    /// « ce compte est incohérent en base ».
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`], [`crate::Error::UnknownAuth`],
    /// [`crate::Error::UnknownSecurity`], [`crate::Error::InconsistentAccount`].
    pub fn full_accounts(&self) -> Result<Vec<crate::model::Account>> {
        use crate::model::{Account, AccountId, AccountKind, AuthKind, Security, Server};

        let mut statement = self.connection().prepare_cached(
            "SELECT id, kind, display_name, host, port, username, auth, security, enabled,
                    smtp_host, smtp_port, smtp_username, smtp_auth, smtp_security
             FROM accounts ORDER BY display_name",
        )?;

        // Les colonnes sortent brutes dans une structure locale, et la validation se fait hors
        // de la fermeture : une erreur de `rusqlite` et une erreur de cohérence ne sont pas la
        // même chose, et `query_map` ne sait rendre que la première.
        //
        // Une structure et non un tuple : à quatorze colonnes, un tuple ne se relit plus, et
        // une inversion de deux `Option<String>` voisines passerait le compilateur.
        struct Raw {
            id: i64,
            kind: String,
            display_name: String,
            host: Option<String>,
            port: Option<i64>,
            username: Option<String>,
            auth: Option<String>,
            security: Option<String>,
            enabled: bool,
            smtp_host: Option<String>,
            smtp_port: Option<i64>,
            smtp_username: Option<String>,
            smtp_auth: Option<String>,
            smtp_security: Option<String>,
        }

        let rows: Vec<Raw> = statement
            .query_map([], |row| {
                Ok(Raw {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    display_name: row.get(2)?,
                    host: row.get(3)?,
                    port: row.get(4)?,
                    username: row.get(5)?,
                    auth: row.get(6)?,
                    security: row.get(7)?,
                    enabled: row.get(8)?,
                    smtp_host: row.get(9)?,
                    smtp_port: row.get(10)?,
                    smtp_username: row.get(11)?,
                    smtp_auth: row.get(12)?,
                    smtp_security: row.get(13)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut out = Vec::with_capacity(rows.len());
        for raw in rows {
            let id = raw.id;
            let kind = AccountKind::from_str_lossy(&raw.kind);
            let missing = |what: &str| crate::Error::InconsistentAccount {
                account: id,
                reason: format!("compte imap sans {what}"),
            };
            let server = match kind {
                AccountKind::Mbox => None,
                AccountKind::Imap => {
                    let host = raw
                        .host
                        .clone()
                        .filter(|it| !it.is_empty())
                        .ok_or_else(|| missing("hôte"))?;
                    let username = raw.username.clone().ok_or_else(|| missing("identifiant"))?;
                    let auth =
                        AuthKind::parse(&raw.auth.clone().ok_or_else(|| missing("mécanisme"))?)?;
                    let security = Security::parse(
                        &raw.security
                            .clone()
                            .ok_or_else(|| missing("mode de chiffrement"))?,
                    )?;
                    // Un port hors de 1..=65535 est une base abîmée, pas une configuration.
                    let port = raw
                        .port
                        .and_then(|it| u16::try_from(it).ok())
                        .filter(|it| *it != 0)
                        .ok_or_else(|| missing("port valide"))?;
                    Some(Server {
                        host,
                        port,
                        username,
                        auth,
                        security,
                    })
                }
            };

            // ## Le serveur de soumission est **tout ou rien**
            //
            // Un `smtp_host` sans `smtp_security` ne dit pas s'il faut du TLS direct ou un
            // `STARTTLS`, et deviner rétrograderait le chiffrement à l'insu de l'utilisateur.
            // Deux colonnes sont donc obligatoires ensemble ; les trois autres se replient sur
            // celles de la lecture, où le repli est sans danger — même identifiant, même
            // mécanisme, même secret.
            //
            // Une configuration à moitié écrite est une **incohérence**, pas une absence : la
            // taire ferait croire à l'utilisateur qu'il a configuré l'envoi.
            let submission = match (&raw.smtp_host, &raw.smtp_security) {
                (None, None) => None,
                (Some(host), Some(security)) if !host.is_empty() => {
                    let security = Security::parse(security)?;
                    let port = raw
                        .smtp_port
                        .and_then(|it| u16::try_from(it).ok())
                        .filter(|it| *it != 0)
                        .unwrap_or_else(|| security.submission_port());
                    let username = raw
                        .smtp_username
                        .clone()
                        .or_else(|| server.as_ref().map(|it| it.username.clone()))
                        .ok_or_else(|| crate::Error::InconsistentAccount {
                            account: id,
                            reason: "serveur de soumission sans identifiant".to_owned(),
                        })?;
                    let auth = match &raw.smtp_auth {
                        Some(label) => AuthKind::parse(label)?,
                        None => server.as_ref().map_or(AuthKind::Password, |it| it.auth),
                    };
                    Some(Server {
                        host: host.clone(),
                        port,
                        username,
                        auth,
                        security,
                    })
                }
                _ => {
                    return Err(crate::Error::InconsistentAccount {
                        account: id,
                        reason: "serveur de soumission à moitié configuré : il faut un hôte \
                                 **et** un mode de chiffrement"
                            .to_owned(),
                    });
                }
            };

            out.push(Account {
                id: AccountId(id),
                kind,
                display_name: raw.display_name,
                server,
                submission,
                enabled: raw.enabled,
            });
        }
        Ok(out)
    }

    /// L'état de synchronisation d'un dossier.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn sync_state(&self, folder: crate::model::FolderId) -> Result<crate::model::SyncState> {
        let mut statement = self.connection().prepare_cached(
            "SELECT remote_name, uidvalidity, uidnext, highest_modseq, synced_at, subscribed
             FROM folders WHERE id = ?1",
        )?;
        let state = statement.query_row([folder.0], |row| {
            Ok(crate::model::SyncState {
                remote_name: row.get::<_, Option<Vec<u8>>>(0)?.unwrap_or_default(),
                uidvalidity: row.get(1)?,
                uidnext: row.get(2)?,
                // Relu en signé — c'est ce que SQLite stocke — puis rendu non signé, comme la
                // RFC 7162 le décrit. Une valeur négative en base est une base abîmée : elle
                // devient `None`, donc « jamais synchronisé », donc une resynchronisation
                // complète. C'est le repli sûr : il refait du travail, il n'en rate pas.
                highest_modseq: row
                    .get::<_, Option<i64>>(3)?
                    .and_then(|it| u64::try_from(it).ok()),
                synced_at: row.get(4)?,
                subscribed: row.get(5)?,
            })
        })?;
        Ok(state)
    }

    /// Les drapeaux de chaque copie d'un contenu dans un dossier.
    ///
    /// Plusieurs valeurs quand le serveur a plusieurs copies du même contenu sous des UID
    /// différents. À passer à [`crate::model::MessageFlags::reduce`].
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn copy_flags(
        &self,
        folder: crate::model::FolderId,
        message: crate::model::MessageId,
    ) -> Result<Vec<crate::model::MessageFlags>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT flags FROM remote_uids WHERE folder_id = ?1 AND message_id = ?2 ORDER BY uid",
        )?;
        let rows = statement.query_map([folder.0, message.0], |row| {
            Ok(crate::model::MessageFlags::from_bits_truncate(row.get(0)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Le contenu qu'un UID désigne, s'il est connu.
    ///
    /// Sert à la moisson des drapeaux : une copie déjà connue reçoit ses nouveaux drapeaux
    /// sans qu'on retélécharge son corps.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn copy_message(
        &self,
        folder: crate::model::FolderId,
        uid: u32,
    ) -> Result<Option<MessageId>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT message_id FROM remote_uids WHERE folder_id = ?1 AND uid = ?2",
        )?;
        let found: Option<i64> = statement
            .query_row(params![folder.0, uid], |row| row.get(0))
            .optional()?;
        Ok(found.map(MessageId))
    }

    /// Les UID déjà connus d'un dossier, dans l'ordre croissant.
    ///
    /// **C'est la question de la moisson incrémentale sans `CONDSTORE`** : ce que le serveur
    /// annonce, moins ce qu'on a déjà. Rendus en entier plutôt que paginés parce qu'un UID
    /// fait quatre octets — un dossier de 100 000 messages tient dans 400 Ko, et la moisson a
    /// besoin de l'ensemble pour calculer une différence.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn known_uids(&self, folder: crate::model::FolderId) -> Result<Vec<u32>> {
        let mut statement = self
            .connection()
            .prepare_cached("SELECT uid FROM remote_uids WHERE folder_id = ?1 ORDER BY uid")?;
        let rows = statement.query_map([folder.0], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Le nombre de copies connues d'un dossier.
    ///
    /// ## Pourquoi un compte et pas [`Store::known_uids`]
    ///
    /// Cette valeur se compare à l'`EXISTS` d'un `EXAMINE` pour savoir si une purge a pu avoir
    /// lieu — voir le balayage dans `mailsync`. La question est « combien », pas « lesquels », et
    /// rapatrier 19 176 entiers pour en prendre la longueur est exactement le genre de coût
    /// qu'on cherche à supprimer ici.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn copies_in(&self, folder: crate::model::FolderId) -> Result<u64> {
        let mut statement = self
            .connection()
            .prepare_cached("SELECT COUNT(*) FROM remote_uids WHERE folder_id = ?1")?;
        let count: i64 = statement.query_row([folder.0], |row| row.get(0))?;
        Ok(count.unsigned_abs())
    }

    /// Le nombre de messages de chaque dossier, en une requête.
    ///
    /// En une requête et non une par dossier : un profil réel a 96 dossiers, et
    /// `list_folders` est appelé à chaque ouverture de client.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn folder_counts(&self) -> Result<std::collections::HashMap<FolderId, (u64, u64)>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT folder_id, COUNT(*), SUM(CASE WHEN flags & 1 = 0 THEN 1 ELSE 0 END)
             FROM refs GROUP BY folder_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                FolderId(row.get(0)?),
                (
                    row.get::<_, i64>(1)?.unsigned_abs(),
                    row.get::<_, i64>(2)?.unsigned_abs(),
                ),
            ))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
    /// Tous les messages, avec de quoi les indexer.
    ///
    /// Rend un `Vec` et non un itérateur paresseux : `rusqlite` tient un emprunt sur la
    /// connexion tant qu'une requête est ouverte, ce qui interdirait d'écrire pendant qu'on
    /// lit. À ~200 octets la ligne, 100 000 messages tiennent dans 20 Mo — négligeable
    /// devant le plafond du critère 3, et bien plus simple qu'un curseur paginé.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn all_for_indexing(&self) -> Result<Vec<IndexRow>> {
        let mut statement = self.connection().prepare(
            "SELECT m.id, m.blob_hash, m.subject, m.from_addr, m.from_name, m.date,
                    COALESCE(GROUP_CONCAT(f.path, ' '), '')
             FROM messages m
             LEFT JOIN refs r ON r.message_id = m.id
             LEFT JOIN folders f ON f.id = r.folder_id
             GROUP BY m.id
             ORDER BY m.id",
        )?;

        let rows = statement.query_map([], index_row)?;
        collect(rows)
    }

    /// Supprime les blobs que plus aucun message et plus aucune ligne de file ne désignent.
    ///
    /// ## Ce que « plus aucun » veut dire ici, et ce qu'il ne couvre pas
    ///
    /// Un blob est orphelin s'il n'est ni dans `messages.blob_hash`, ni dans `outbox.blob_hash`.
    /// C'est vrai à l'instant de la lecture, et **ce n'est pas suffisant en général** : un
    /// import ou une moisson en cours écrit ses blobs **avant** les lignes qui les désignent —
    /// c'est l'ordre qui évite qu'une ligne pointe vers un blob absent. Un blob fraîchement
    /// écrit y ressemble donc à un orphelin.
    ///
    /// D'où le contrat de cette fonction : **l'appelant garantit qu'aucune tâche d'écriture ne
    /// tourne.** `mail doctor --purge-orphans` le demande explicitement, et rien ne l'appelle
    /// automatiquement — un ramasse-miettes qui tourne pendant un import supprimerait du
    /// courrier en cours d'arrivée.
    ///
    /// ## Elle rend ce qu'elle a libéré
    ///
    /// Le nombre de blobs et les octets rendus au système de fichiers, mesurés avant
    /// suppression. C'est ce qu'un utilisateur veut savoir avant de décider si ça valait la
    /// peine.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si l'index est illisible, [`crate::Error::Io`] si le magasin
    /// l'est. Un blob qui refuse de disparaître est **compté** et n'interrompt pas la purge.
    pub fn purge_orphan_blobs(&self) -> Result<PurgeReport> {
        // Les deux ensembles de clés vivantes, lus **avant** de parcourir le disque : dans
        // l'autre ordre, un blob écrit entre les deux lectures serait vu orphelin.
        let mut alive = std::collections::HashSet::new();
        for table in ["messages", "outbox"] {
            let sql = format!("SELECT blob_hash FROM {table}");
            let mut statement = self.connection().prepare(&sql)?;
            let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            for row in rows {
                alive.insert(row?);
            }
        }

        // Le trait est amené dans la portée : `FsBlobStore` a des méthodes propres et des
        // méthodes de trait, et seules les secondes suppriment.
        use crate::store::blobs::BlobStore as _;

        let mut report = PurgeReport::default();
        let mut doomed = Vec::new();
        self.blobs.for_each_hash(&mut |hash| {
            if !alive.contains(hash.as_bytes().as_slice()) {
                doomed.push(hash);
            }
            Ok(())
        })?;

        for hash in doomed {
            // La taille est relevée **avant** la suppression : après, il n'y a plus rien à
            // mesurer, et un compteur maison serait une seconde vérité sur la même donnée.
            let freed = std::fs::metadata(self.blobs.path_of(hash).as_std_path())
                .map(|it| it.len())
                .unwrap_or(0);
            match self.blobs.delete(hash) {
                Ok(true) => {
                    report.removed += 1;
                    report.freed = report.freed.saturating_add(freed);
                }
                // Déjà parti entre le parcours et la suppression. Rien à signaler.
                Ok(false) => {}
                Err(source) => {
                    tracing::warn!(%hash, %source, "blob orphelin non supprimé");
                    report.failed += 1;
                }
            }
        }
        tracing::info!(
            removed = report.removed,
            freed = report.freed,
            failed = report.failed,
            "blobs orphelins purgés"
        );
        Ok(report)
    }

    /// Nombre de contenus distincts stockés.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn message_count(&self) -> Result<u64> {
        self.count("messages")
    }

    /// L'état du store.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn stats(&self) -> Result<StoreStats> {
        let raw_bytes: i64 = self.connection().query_row(
            "SELECT COALESCE(SUM(size), 0) FROM messages",
            [],
            |row| row.get(0),
        )?;
        let unthreaded: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM messages WHERE thread_id IS NULL",
            [],
            |row| row.get(0),
        )?;

        Ok(StoreStats {
            accounts: self.count("accounts")?,
            folders: self.count("folders")?,
            messages: self.count("messages")?,
            refs: self.count("refs")?,
            copies: self.count("remote_uids")?,
            unthreaded: unthreaded.unsigned_abs(),
            raw_bytes: raw_bytes.unsigned_abs(),
        })
    }

    /// Le compteur de version des données de SQLite.
    ///
    /// `PRAGMA data_version` change dès qu'**une autre connexion** valide une transaction
    /// sur cette base. C'est exactement la primitive dont un démon en lecture a besoin en
    /// phase 1 : l'import et l'indexation tournent dans un autre processus (`mail import`),
    /// donc un canal de notification interne au démon ne se déclencherait jamais. Ici, un
    /// `mail import` lancé à côté rend le compteur différent au prochain appel.
    ///
    /// La valeur de départ est arbitraire et n'a aucun sens absolu : elle se compare par
    /// égalité, jamais par ordre.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .connection()
            .query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    /// Le nombre de lignes que **cette connexion** a modifiées depuis son ouverture.
    ///
    /// ## Le complément indispensable de [`Store::data_version`]
    ///
    /// `PRAGMA data_version` ne bouge **pas** pour les écritures validées sur la connexion qui
    /// l'interroge. Un processus qui sert son propre store — la coquille en mode embarqué — ne
    /// voit donc jamais ses propres écritures, et c'est ce qui faisait qu'un message marqué lu
    /// restait affiché « non lu » jusqu'au redémarrage.
    ///
    /// Les deux compteurs sont complémentaires par construction : l'un voit les autres, l'autre
    /// voit soi. Aucun registre à tenir à la main, donc aucun chemin d'écriture à ne pas
    /// oublier de signaler — ce qui était l'autre solution, et celle qui se dégraderait en
    /// silence au prochain `UPDATE` ajouté ailleurs.
    ///
    /// Monotone, et sans signification absolue : un jeton à comparer, pas une quantité.
    #[must_use]
    pub fn local_changes(&self) -> u64 {
        self.connection().total_changes()
    }

    /// Compte les lignes d'une table. `table` n'est jamais une entrée utilisateur — les
    /// seuls appels viennent d'ici, avec des littéraux.
    fn count(&self, table: &str) -> Result<u64> {
        let value: i64 =
            self.connection()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
        Ok(value.unsigned_abs())
    }
}

/// Lit une ligne d'indexation.
///
/// Partagée par `all_for_indexing` et `unindexed` : les deux doivent produire **le même
/// document**, et deux mappeurs recopiés divergeraient au premier champ ajouté — un index
/// reconstruit ne rendrait alors pas les mêmes résultats qu'un index tenu à jour.
fn index_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexRow> {
    let hash: Vec<u8> = row.get(1)?;
    Ok(IndexRow {
        id: MessageId(row.get(0)?),
        blob: blob_hash(&hash),
        subject: row.get(2)?,
        from_addr: row.get(3)?,
        from_name: row.get(4)?,
        date: row.get(5)?,
        folders: row.get(6)?,
    })
}

/// Ramasse un itérateur de lignes, en propageant la première erreur.
fn collect<T>(rows: impl Iterator<Item = rusqlite::Result<T>>) -> Result<Vec<T>> {
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Relit un hash depuis la base.
///
/// Un blob de taille inattendue ne fait pas paniquer : la ligne est rendue avec un hash
/// nul, qui ne correspondra à aucun blob et sera signalé par `mail doctor`. Un index
/// corrompu doit se diagnostiquer, pas faire tomber le démon.
pub(crate) fn blob_hash(bytes: &[u8]) -> BlobHash {
    let mut buffer = [0u8; 32];
    let len = bytes.len().min(32);
    buffer[..len].copy_from_slice(&bytes[..len]);
    BlobHash::from_bytes(buffer)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::AccountId;
    use crate::store::write::NewMessage;
    use camino::Utf8Path;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let store = Store::open(&root).unwrap();
        (dir, store)
    }

    /// Remplit un dossier de `count` messages, un par seconde décroissante.
    fn fill(store: &Store, count: usize) -> (AccountId, FolderId) {
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();
        let folder = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();

        for index in 0..count {
            let blob = BlobHash::of(format!("message {index}").as_bytes());
            let date = 1_700_000_000 + index as i64;
            let (id, _) = writer
                .insert_message(&NewMessage {
                    blob,
                    rfc822_id: None,
                    date,
                    from_addr: "a@b.c",
                    from_name: None,
                    subject: &format!("sujet {index}"),
                    size: 100,
                    has_attachments: false,
                })
                .unwrap();
            writer
                .insert_ref(id, folder, date, MessageFlags::empty())
                .unwrap();
        }
        writer.commit().unwrap();
        (account, folder)
    }

    #[test]
    fn an_empty_store_reports_zeroes() {
        let (_dir, store) = store();
        assert_eq!(store.stats().unwrap(), StoreStats::default());
        assert!(store.folders().unwrap().is_empty());
    }

    #[test]
    fn folders_come_back_with_their_role() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();
        writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        writer
            .upsert_folder(account, "Corbeille", FolderKind::Trash)
            .unwrap();
        writer.commit().unwrap();

        let folders = store.folders().unwrap();
        assert_eq!(folders.len(), 2);
        // Triés par chemin : Corbeille avant INBOX.
        assert_eq!(folders[0].path, "Corbeille");
        assert_eq!(folders[0].kind, FolderKind::Trash);
        assert_eq!(folders[1].kind, FolderKind::Inbox);
    }

    #[test]
    fn a_page_comes_back_newest_first() {
        let (_dir, store) = store();
        let (_, folder) = fill(&store, 10);

        let page = store.page(folder, None, 3).unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(page[0].subject, "sujet 9");
        assert_eq!(page[1].subject, "sujet 8");
        assert_eq!(page[2].subject, "sujet 7");
    }

    #[test]
    fn paging_walks_the_whole_folder_exactly_once() {
        // Le test qui compte : ni saut ni répétition à la frontière des pages.
        let (_dir, store) = store();
        let (_, folder) = fill(&store, 250);

        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = store.page(folder, cursor, 40).unwrap();
            if page.is_empty() {
                break;
            }
            cursor = Store::next_cursor(&page);
            seen.extend(page.into_iter().map(|item| item.id.0));
        }

        assert_eq!(seen.len(), 250);
        let unique: std::collections::HashSet<i64> = seen.iter().copied().collect();
        assert_eq!(unique.len(), 250, "une page a répété des lignes");
    }

    #[test]
    fn paging_is_stable_when_many_messages_share_a_date() {
        // La raison pour laquelle le curseur porte `(date, id)` et pas seulement `date`.
        let (_dir, store) = store();
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();
        let folder = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        for index in 0..100 {
            let blob = BlobHash::of(format!("m{index}").as_bytes());
            let (id, _) = writer
                .insert_message(&NewMessage {
                    blob,
                    rfc822_id: None,
                    date: 42,
                    from_addr: "a@b.c",
                    from_name: None,
                    subject: "meme seconde",
                    size: 10,
                    has_attachments: false,
                })
                .unwrap();
            writer
                .insert_ref(id, folder, 42, MessageFlags::empty())
                .unwrap();
        }
        writer.commit().unwrap();

        let mut seen = std::collections::HashSet::new();
        let mut cursor = None;
        loop {
            let page = store.page(folder, cursor, 7).unwrap();
            if page.is_empty() {
                break;
            }
            cursor = Store::next_cursor(&page);
            for item in page {
                assert!(seen.insert(item.id.0), "ligne rendue deux fois");
            }
        }
        assert_eq!(seen.len(), 100);
    }

    #[test]
    fn an_empty_folder_yields_an_empty_page_and_no_cursor() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();
        let folder = writer
            .upsert_folder(account, "Vide", FolderKind::Other)
            .unwrap();
        writer.commit().unwrap();

        let page = store.page(folder, None, 50).unwrap();
        assert!(page.is_empty());
        assert!(Store::next_cursor(&page).is_none());
    }

    #[test]
    fn the_blob_hash_survives_the_round_trip() {
        let (_dir, store) = store();
        let (_, folder) = fill(&store, 1);

        let page = store.page(folder, None, 1).unwrap();
        assert_eq!(page[0].blob, BlobHash::of(b"message 0"));
    }

    #[test]
    fn stats_show_deduplication() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();
        let inbox = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let archive = writer
            .upsert_folder(account, "Archive", FolderKind::Archive)
            .unwrap();

        let (id, _) = writer
            .insert_message(&NewMessage {
                blob: BlobHash::of(b"un seul contenu"),
                rfc822_id: None,
                date: 1,
                from_addr: "a@b.c",
                from_name: None,
                subject: "s",
                size: 500,
                has_attachments: false,
            })
            .unwrap();
        writer
            .insert_ref(id, inbox, 1, MessageFlags::empty())
            .unwrap();
        writer
            .insert_ref(id, archive, 1, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.messages, 1);
        assert_eq!(stats.refs, 2);
        assert_eq!(stats.raw_bytes, 500);
        assert_eq!(stats.unthreaded, 1);
        assert!((stats.refs_per_message() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_truncated_hash_in_the_index_does_not_panic() {
        // Index corrompu : ça se diagnostique, ça ne fait pas tomber le démon.
        assert_eq!(blob_hash(&[]).as_bytes(), &[0u8; 32]);
        assert_eq!(blob_hash(&[1, 2, 3]).as_bytes()[0..3], [1, 2, 3]);
        assert_eq!(blob_hash(&[7u8; 64]).as_bytes(), &[7u8; 32]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod orphan_tests {
    use crate::Store;

    /// Un store neuf, jetable.
    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let store = Store::open(&root).unwrap();
        (dir, store)
    }

    #[test]
    fn a_store_without_blobs_has_no_orphan() {
        let (_dir, store) = store();
        assert_eq!(store.orphan_blobs().unwrap(), 0);
    }

    #[test]
    fn a_blob_that_no_message_names_is_an_orphan() {
        // **Le cas réel** : un lot de moisson coupé entre l'écriture du blob et celle des
        // lignes. Le blob est là, rien ne le désigne.
        let (_dir, store) = store();
        store
            .blobs()
            .put(b"un corps que personne ne reclame")
            .unwrap();
        assert_eq!(store.orphan_blobs().unwrap(), 1);
    }

    #[test]
    fn a_blob_a_message_names_is_not_an_orphan() {
        // Le contrôle inverse, sans lequel le compteur pourrait tout compter et personne ne le
        // verrait : un store sain doit rendre zéro.
        let (_dir, store) = store();
        let body = b"From: a@b\r\nSubject: s\r\n\r\ncorps\r\n";
        let outcome = store.blobs().put(body).unwrap();

        let writer = store.writer().unwrap();
        writer
            .insert_message(&crate::store::write::NewMessage {
                blob: outcome.hash,
                rfc822_id: None,
                date: 0,
                from_addr: "a@b",
                from_name: None,
                subject: "s",
                size: body.len() as u64,
                has_attachments: false,
            })
            .unwrap();
        writer.commit().unwrap();

        assert_eq!(store.orphan_blobs().unwrap(), 0);
    }

    #[test]
    fn a_file_that_is_not_a_hash_is_not_counted() {
        // Le répertoire des blobs peut recevoir n'importe quoi — un outil extérieur, une
        // sauvegarde d'éditeur. Refuser tout l'inventaire à cause d'un intrus rendrait
        // l'inventaire inutile le jour où il y en a un.
        let (dir, store) = store();
        let shard = dir.path().join("blobs").join("zz").join("zz");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join("pas-une-empreinte.txt"), b"bruit").unwrap();

        assert_eq!(store.orphan_blobs().unwrap(), 0);
    }
}

/// Ce qu'une purge de blobs orphelins a libéré.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// Combien de blobs ont été supprimés.
    pub removed: u64,
    /// Octets rendus au système de fichiers, tels que mesurés **avant** suppression.
    pub freed: u64,
    /// Combien ont refusé de disparaître. Comptés, jamais propagés.
    pub failed: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod purge_tests {
    use super::*;
    use crate::store::write::NewMessage;
    use camino::Utf8Path;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        (dir, Store::open(&root).unwrap())
    }

    #[test]
    fn a_blob_no_one_designates_is_purged() {
        let (_dir, store) = store();
        let orphan = store.blobs().put(b"personne ne me designe").unwrap().hash;
        assert_eq!(store.orphan_blobs().unwrap(), 1);

        let report = store.purge_orphan_blobs().unwrap();
        assert_eq!(report.removed, 1);
        assert!(report.freed > 0, "aucun octet rendu");
        assert_eq!(report.failed, 0);
        assert!(!store.blobs().contains(orphan).unwrap());
        assert_eq!(store.orphan_blobs().unwrap(), 0);
    }

    #[test]
    fn a_blob_a_message_designates_survives() {
        // Le contrôle négatif, et c'est celui qui compte : une purge qui supprimerait un blob
        // référencé effacerait du courrier.
        let (_dir, store) = store();
        let kept = store.blobs().put(b"un vrai message").unwrap().hash;
        let writer = store.writer().unwrap();
        writer
            .insert_message(&NewMessage {
                blob: kept,
                rfc822_id: None,
                date: 1,
                from_addr: "a@b.c",
                from_name: None,
                subject: "sujet",
                size: 15,
                has_attachments: false,
            })
            .unwrap();
        writer.commit().unwrap();

        let report = store.purge_orphan_blobs().unwrap();
        assert_eq!(report.removed, 0, "un blob référencé a été supprimé");
        assert!(store.blobs().contains(kept).unwrap());
    }

    #[test]
    fn a_blob_the_outbox_designates_survives_too() {
        // **La seconde table, et celle qu'on oublie.** Un message en file n'est pas dans
        // `messages` : le purger effacerait un message que le facteur allait envoyer.
        let (_dir, store) = store();
        let queued = store.blobs().put(b"en attente de depart").unwrap().hash;
        let account = {
            let writer = store.writer().unwrap();
            let id = writer.upsert_account("imap", "compte").unwrap();
            writer.commit().unwrap();
            id
        };
        store
            .enqueue(
                account,
                queued,
                "marie@exemple.fr",
                &["jean@ailleurs.fr".to_owned()],
                20,
                1_000,
                None,
            )
            .unwrap();

        let report = store.purge_orphan_blobs().unwrap();
        assert_eq!(report.removed, 0, "un message en file a été purgé");
        assert!(store.blobs().contains(queued).unwrap());
    }

    #[test]
    fn purging_an_empty_store_does_nothing_and_says_so() {
        let (_dir, store) = store();
        assert_eq!(store.purge_orphan_blobs().unwrap(), PurgeReport::default());
    }

    #[test]
    fn a_finished_line_can_be_forgotten_and_an_unfinished_one_cannot() {
        // Le refus qui protège la trace : retirer une ligne douteuse effacerait l'information
        // que le critère 2 existe pour conserver.
        let (_dir, store) = store();
        let account = {
            let writer = store.writer().unwrap();
            let id = writer.upsert_account("imap", "compte").unwrap();
            writer.commit().unwrap();
            id
        };
        let blob = store.blobs().put(b"un message").unwrap().hash;
        let id = store
            .enqueue(
                account,
                blob,
                "marie@exemple.fr",
                &["jean@ailleurs.fr".to_owned()],
                10,
                1_000,
                None,
            )
            .unwrap();

        for state in [
            crate::SendState::Queued,
            crate::SendState::Sending,
            crate::SendState::Committing,
        ] {
            store.commit_outgoing(id, state).unwrap();
            assert!(
                !store.forget_outgoing(id).unwrap(),
                "{state:?} a été retiré alors qu'il n'est pas fini"
            );
            assert!(store.outgoing(id).unwrap().is_some());
        }

        store.commit_outgoing(id, crate::SendState::Sent).unwrap();
        assert!(store.forget_outgoing(id).unwrap());
        assert!(store.outgoing(id).unwrap().is_none());
        // Le blob devient orphelin — c'est la purge qui s'en occupe, pas le retrait.
        assert_eq!(store.orphan_blobs().unwrap(), 1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod orphan_agreement_tests {
    use super::*;
    use camino::Utf8Path;

    #[test]
    fn the_count_and_the_purge_agree_on_what_an_orphan_is() {
        // **Le décalage trouvé sur le store réel** : `orphan_blobs` comptait 6, la purge en
        // supprimait 4, parce que la première ignorait la file d'envoi. Deux définitions
        // d'« orphelin » dans le même module sont un mensonge en attente — celle-ci était
        // du bon côté, la suivante ne le serait pas forcément.
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let store = Store::open(&root).unwrap();

        let account = {
            let writer = store.writer().unwrap();
            let id = writer.upsert_account("imap", "compte").unwrap();
            writer.commit().unwrap();
            id
        };
        // Un blob en file — pas un orphelin — et un vrai orphelin.
        let queued = store.blobs().put(b"en file").unwrap().hash;
        store
            .enqueue(
                account,
                queued,
                "marie@exemple.fr",
                &["jean@ailleurs.fr".to_owned()],
                7,
                1_000,
                None,
            )
            .unwrap();
        store.blobs().put(b"vraiment orphelin").unwrap();

        let counted = store.orphan_blobs().unwrap();
        let purged = store.purge_orphan_blobs().unwrap().removed;
        assert_eq!(
            counted, purged,
            "le compte et la purge ne s'accordent pas sur ce qu'est un orphelin"
        );
        assert_eq!(counted, 1, "le blob en file a été compté orphelin");
        assert!(store.blobs().contains(queued).unwrap());
    }
}

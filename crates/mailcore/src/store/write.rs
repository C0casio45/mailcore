//! L'écriture dans l'index de métadonnées.
//!
//! Une seule porte : [`Store::writer`], qui rend un [`Writer`] tenant une transaction. Tout
//! passe par lui, et le SQL ne sort jamais de ce module — c'est la frontière décrite en tête
//! de `store`.
//!
//! ## Pourquoi une transaction explicite plutôt qu'une méthode par insertion
//!
//! Un import écrit ~100 000 messages et ~130 000 références. En validant chaque insertion,
//! SQLite ferait un `fsync` par ligne : des heures. En les groupant par milliers, c'est des
//! secondes. La transaction n'est donc pas un détail d'API, c'est la différence entre un
//! import réalisable et un import qui ne finit pas.
//!
//! Les instructions sont préparées une fois et réutilisées via le cache de `rusqlite` : sur
//! 230 000 insertions, recompiler le SQL à chaque appel coûterait plus que l'écriture.

use rusqlite::{OptionalExtension, params};

use crate::error::Result;
use crate::model::{
    AccountId, BlobHash, FolderId, FolderKind, MessageFlags, MessageId, RemoteCopy, Server,
    SyncState, ThreadId,
};
use crate::store::Store;

/// Convertit un `MODSEQ` pour SQLite, qui ne stocke que des entiers signés de 64 bits.
///
/// La RFC 7162 borne un `MODSEQ` à 63 bits non signés : la conversion est donc toujours
/// possible sur un serveur conforme, et le cas d'échec dénonce un serveur qui ne l'est pas.
/// Le tronquer serait pire que refuser — la moisson incrémentale croirait avoir déjà vu ce
/// qui arrive après.
fn modseq_for_sql(modseq: Option<u64>) -> Result<Option<i64>> {
    modseq
        .map(|it| i64::try_from(it).map_err(|_| crate::Error::ModseqOutOfRange { found: it }))
        .transpose()
}

/// Un message à insérer, tel que l'import le connaît.
///
/// Pas de `thread_id` : au moment de l'import, on ne sait pas encore à quel fil le message
/// appartient. La passe de threading le remplira.
#[derive(Debug, Clone)]
pub struct NewMessage<'a> {
    /// L'identité du contenu.
    pub blob: BlobHash,
    /// L'en-tête `Message-ID`, s'il était lisible.
    pub rfc822_id: Option<&'a str>,
    /// Date en secondes Unix.
    pub date: i64,
    /// Adresse de l'expéditeur, en minuscules.
    pub from_addr: &'a str,
    /// Nom affiché de l'expéditeur.
    pub from_name: Option<&'a str>,
    /// Sujet décodé.
    pub subject: &'a str,
    /// Taille du message RFC 5322 stocké.
    pub size: u64,
    /// Vrai si le message déclare des pièces jointes.
    pub has_attachments: bool,
}

/// Une transaction d'écriture sur l'index.
///
/// Rien n'est visible pour les lecteurs tant que [`Writer::commit`] n'a pas été appelé. Un
/// `Writer` abandonné annule ses écritures — c'est le comportement voulu : un import
/// interrompu ne laisse pas un index à moitié rempli.
#[derive(Debug)]
pub struct Writer<'a> {
    tx: rusqlite::Transaction<'a>,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(tx: rusqlite::Transaction<'a>) -> Self {
        Self { tx }
    }

    /// La transaction en cours, pour les modules du store qui écrivent depuis un autre fichier —
    /// `store::threads`.
    ///
    /// `pub(crate)` et pas `pub` : la frontière décrite en tête de `store` tient, aucun crate
    /// extérieur ne peut écrire du SQL contre ce store.
    pub(crate) fn transaction(&self) -> &rusqlite::Connection {
        &self.tx
    }

    /// Valide les écritures.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si la validation échoue.
    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }

    /// Trouve ou crée un compte. Idempotent sur `(kind, display_name)`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn upsert_account(&self, kind: &str, display_name: &str) -> Result<AccountId> {
        let existing: Option<i64> = self
            .tx
            .prepare_cached("SELECT id FROM accounts WHERE kind = ?1 AND display_name = ?2")?
            .query_row(params![kind, display_name], |row| row.get(0))
            .optional()?;

        if let Some(id) = existing {
            return Ok(AccountId(id));
        }

        self.tx
            .prepare_cached("INSERT INTO accounts (kind, display_name) VALUES (?1, ?2)")?
            .execute(params![kind, display_name])?;
        Ok(AccountId(self.tx.last_insert_rowid()))
    }

    /// Trouve ou crée un dossier. Idempotent sur `(account, path)`.
    ///
    /// Si le dossier existe déjà avec un autre rôle, le rôle est mis à jour : la détection
    /// s'améliore avec le temps, et un dossier reconnu ne doit pas rester `other` parce
    /// qu'un import antérieur ne savait pas le lire.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn upsert_folder(
        &self,
        account: AccountId,
        path: &str,
        kind: FolderKind,
    ) -> Result<FolderId> {
        let existing: Option<i64> = self
            .tx
            .prepare_cached("SELECT id FROM folders WHERE account_id = ?1 AND path = ?2")?
            .query_row(params![account.0, path], |row| row.get(0))
            .optional()?;

        if let Some(id) = existing {
            self.tx
                .prepare_cached("UPDATE folders SET kind = ?2 WHERE id = ?1")?
                .execute(params![id, kind.as_str()])?;
            return Ok(FolderId(id));
        }

        self.tx
            .prepare_cached("INSERT INTO folders (account_id, path, kind) VALUES (?1, ?2, ?3)")?
            .execute(params![account.0, path, kind.as_str()])?;
        Ok(FolderId(self.tx.last_insert_rowid()))
    }

    /// Trouve ou crée un compte IMAP, et met son serveur à jour. Idempotent sur
    /// `(hôte, identifiant)`.
    ///
    /// ## Pourquoi la clé n'est pas le nom affiché
    ///
    /// [`Writer::upsert_account`] identifie un compte par `(kind, display_name)`, ce qui va
    /// pour un import mbox : le nom vient du répertoire, il ne change pas. Un compte IMAP est
    /// autre chose — l'utilisateur renomme « Perso » en « Gmail perso » et ne s'attend pas à
    /// se retrouver avec deux comptes et deux fois son courrier.
    ///
    /// Ce qui identifie un compte IMAP est **où il se connecte et sous quel nom**. Le port et
    /// le mode de chiffrement n'en font pas partie : passer un compte de `STARTTLS` à TLS est
    /// une correction de configuration, pas un déménagement.
    ///
    /// **Aucun secret n'est écrit.** Il n'y a pas de paramètre pour en passer un ; le
    /// trousseau du système est le seul endroit.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn upsert_imap_account(&self, display_name: &str, server: &Server) -> Result<AccountId> {
        let existing: Option<i64> = self
            .tx
            .prepare_cached(
                "SELECT id FROM accounts WHERE kind = 'imap' AND host = ?1 AND username = ?2",
            )?
            .query_row(params![server.host, server.username], |row| row.get(0))
            .optional()?;

        if let Some(id) = existing {
            self.tx
                .prepare_cached(
                    "UPDATE accounts SET display_name = ?2, port = ?3, auth = ?4, security = ?5
                     WHERE id = ?1",
                )?
                .execute(params![
                    id,
                    display_name,
                    server.port,
                    server.auth.as_str(),
                    server.security.as_str(),
                ])?;
            return Ok(AccountId(id));
        }

        self.tx
            .prepare_cached(
                "INSERT INTO accounts
                     (kind, display_name, host, port, username, auth, security)
                 VALUES ('imap', ?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                display_name,
                server.host,
                server.port,
                server.username,
                server.auth.as_str(),
                server.security.as_str(),
            ])?;
        Ok(AccountId(self.tx.last_insert_rowid()))
    }

    /// Écrit — ou efface — le serveur de soumission d'un compte.
    ///
    /// ## Les deux colonnes obligatoires partent ensemble
    ///
    /// `smtp_host` et `smtp_security` sont écrites ou effacées d'un bloc, parce qu'une
    /// configuration à moitié écrite fait refuser le compte à la lecture
    /// ([`crate::Error::InconsistentAccount`]) — voir `Store::full_accounts`. Passer `None`
    /// efface les cinq colonnes : le compte redevient un compte qui ne peut pas envoyer, ce qui
    /// est un état valide.
    ///
    /// L'identifiant et le mécanisme ne sont écrits que s'ils **diffèrent** de ceux de la
    /// lecture. Les recopier ferait deux vérités à tenir d'accord pour rien.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn set_submission(&self, account: AccountId, server: Option<&Server>) -> Result<()> {
        match server {
            None => {
                self.tx
                    .prepare_cached(
                        "UPDATE accounts
                            SET smtp_host = NULL, smtp_port = NULL, smtp_username = NULL,
                                smtp_auth = NULL, smtp_security = NULL
                          WHERE id = ?1",
                    )?
                    .execute(params![account.0])?;
            }
            Some(server) => {
                let reading: Option<(Option<String>, Option<String>)> = self
                    .tx
                    .prepare_cached("SELECT username, auth FROM accounts WHERE id = ?1")?
                    .query_row(params![account.0], |row| Ok((row.get(0)?, row.get(1)?)))
                    .optional()?;
                let (read_username, read_auth) = reading.unwrap_or((None, None));

                let username = (read_username.as_deref() != Some(server.username.as_str()))
                    .then(|| server.username.clone());
                let auth = (read_auth.as_deref() != Some(server.auth.as_str()))
                    .then(|| server.auth.as_str().to_owned());

                self.tx
                    .prepare_cached(
                        "UPDATE accounts
                            SET smtp_host = ?2, smtp_port = ?3, smtp_username = ?4,
                                smtp_auth = ?5, smtp_security = ?6
                          WHERE id = ?1",
                    )?
                    .execute(params![
                        account.0,
                        server.host,
                        server.port,
                        username,
                        auth,
                        server.security.as_str(),
                    ])?;
            }
        }
        Ok(())
    }

    /// Écrit la signature d'un compte, ou l'efface.
    ///
    /// Un document vide **efface** : une signature réduite à des blancs n'est pas une signature
    /// qu'il faut ajouter à chaque message, et garder la colonne remplie d'un document vide
    /// ferait sortir une ligne blanche en fin de courrier sans que personne ne l'ait demandée.
    ///
    /// La forme rangée est la sérialisation du document et non son HTML : voir la migration
    /// `SCHEMA_V8`, qui dit ce que le HTML perdrait.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`], et [`crate::Error::Unserialisable`] qui n'est pas atteignable
    /// pour ce type — la raison est écrite sur la variante.
    pub fn set_signature(
        &self,
        account: AccountId,
        signature: Option<&mailhtml::rich::Document>,
    ) -> Result<()> {
        let rangee = match signature.filter(|it| !it.is_empty()) {
            None => None,
            Some(document) => Some(
                serde_json::to_string(document)
                    .map_err(|_| crate::Error::Unserialisable { what: "signature" })?,
            ),
        };
        self.tx
            .prepare_cached("UPDATE accounts SET signature = ?2 WHERE id = ?1")?
            .execute(params![account.0, rangee])?;
        Ok(())
    }

    /// Met en pause, ou reprend, la synchronisation d'un compte.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn set_account_enabled(&self, account: AccountId, enabled: bool) -> Result<()> {
        self.tx
            .prepare_cached("UPDATE accounts SET enabled = ?2 WHERE id = ?1")?
            .execute(params![account.0, enabled])?;
        Ok(())
    }

    /// Écrit l'état de synchronisation d'un dossier.
    ///
    /// Écrit **tous** les champs, `None` compris : c'est ce qui permet de remettre un dossier
    /// à « jamais synchronisé » quand `UIDVALIDITY` a changé. Un `UPDATE` partiel laisserait
    /// un `uidnext` périmé à côté d'un `uidvalidity` neuf, et la moisson suivante croirait
    /// n'avoir rien à faire.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn set_sync_state(&self, folder: FolderId, state: &SyncState) -> Result<()> {
        self.tx
            .prepare_cached(
                "UPDATE folders SET
                     remote_name = ?2, uidvalidity = ?3, uidnext = ?4,
                     highest_modseq = ?5, synced_at = ?6, subscribed = ?7
                 WHERE id = ?1",
            )?
            .execute(params![
                folder.0,
                state.remote_name,
                state.uidvalidity,
                state.uidnext,
                modseq_for_sql(state.highest_modseq)?,
                state.synced_at,
                state.subscribed,
            ])?;
        Ok(())
    }

    /// Écrit le nom d'un dossier tel que le serveur l'écrit, **et rien d'autre**.
    ///
    /// ## Pourquoi ce n'est pas [`Writer::set_sync_state`]
    ///
    /// La découverte des dossiers apprend leur nom, pas leur état. Passer par
    /// `set_sync_state` écrirait aussi `uidvalidity` et `uidnext` — donc les remettrait à
    /// `None` sur un dossier déjà synchronisé, donc déclencherait une moisson **complète** à
    /// chaque découverte.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn set_remote_name(&self, folder: FolderId, name: &[u8]) -> Result<()> {
        self.tx
            .prepare_cached("UPDATE folders SET remote_name = ?2 WHERE id = ?1")?
            .execute(params![folder.0, name])?;
        Ok(())
    }

    /// Enregistre une copie côté serveur : un UID, ce qu'il désigne, ses drapeaux.
    ///
    /// Idempotent sur `(dossier, UID)` : un deuxième passage sur le même UID met à jour les
    /// drapeaux et le `MODSEQ`, ce qui est exactement ce qu'une moisson incrémentale demande.
    ///
    /// **Ne touche pas à `refs`.** Poser la référence et recalculer ses drapeaux sont deux
    /// opérations distinctes ([`Writer::insert_ref`], [`Writer::refresh_ref_flags`]), parce
    /// qu'une moisson enregistre des milliers de copies avant de savoir ce que les références
    /// doivent afficher.
    ///
    /// ## Rend `true` seulement si quelque chose a **changé**
    ///
    /// La clause `WHERE` sur le `DO UPDATE` est ce qui rend ce booléen utile. Sans elle,
    /// réenregistrer une copie identique compte comme une écriture : SQLite met la ligne à
    /// jour avec les mêmes valeurs, salit une page du WAL, et le bilan annonce du travail
    /// qui n'a rien produit.
    ///
    /// Ce n'est pas cosmétique. Une synchronisation sans `CONDSTORE` redemande **tous** les
    /// drapeaux à chaque passage : sur un compte réel de 2 840 messages, c'était 2 840 mises
    /// à jour identiques par passage, et le critère 3 de `docs/PHASE-2.md` — « 0 octet
    /// écrit » — était faux pour cette raison seule.
    ///
    /// `IS NOT` et non `<>` pour le `MODSEQ` : il est nullable, et `NULL <> NULL` vaut `NULL`,
    /// donc la comparaison naïve rendrait la clause fausse et n'écrirait jamais.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn record_copy(&self, copy: &RemoteCopy) -> Result<bool> {
        let changed = self
            .tx
            .prepare_cached(
                "INSERT INTO remote_uids (folder_id, uid, message_id, flags, modseq)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (folder_id, uid) DO UPDATE SET
                     message_id = excluded.message_id,
                     flags      = excluded.flags,
                     modseq     = excluded.modseq
                 WHERE remote_uids.message_id IS NOT excluded.message_id
                    OR remote_uids.flags      IS NOT excluded.flags
                    OR remote_uids.modseq     IS NOT excluded.modseq",
            )?
            .execute(params![
                copy.folder.0,
                copy.uid,
                copy.message.0,
                copy.flags.bits(),
                modseq_for_sql(copy.modseq)?,
            ])?;
        Ok(changed > 0)
    }

    /// Oublie une copie côté serveur.
    ///
    /// Rend `Some(message)` quand le contenu qu'elle désignait **n'a plus aucune copie** dans
    /// ce dossier — c'est-à-dire quand sa référence doit disparaître de la liste. Tant qu'une
    /// autre copie existe, le message est encore dans la boîte et la référence reste.
    ///
    /// ## Pourquoi le message est rendu, et pas un booléen
    ///
    /// C'est le bug que `remote_uids` existe pour éviter, et l'appelant ne doit pas avoir à le
    /// reconstituer : au moment où il apprend « c'était la dernière », la ligne qui portait
    /// l'identifiant du contenu est déjà supprimée. Un booléen l'obligerait à la lire avant, ce
    /// que la moitié des appelants oublieraient.
    ///
    /// `None` couvre deux cas indiscernables **et sans conséquence** : l'UID n'était pas
    /// connu, ou une autre copie subsiste. Dans les deux, il n'y a pas de référence à retirer.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn forget_copy(&self, folder: FolderId, uid: u32) -> Result<Option<MessageId>> {
        let message: Option<i64> = self
            .tx
            .prepare_cached("SELECT message_id FROM remote_uids WHERE folder_id = ?1 AND uid = ?2")?
            .query_row(params![folder.0, uid], |row| row.get(0))
            .optional()?;
        let Some(message) = message else {
            return Ok(None);
        };

        self.tx
            .prepare_cached("DELETE FROM remote_uids WHERE folder_id = ?1 AND uid = ?2")?
            .execute(params![folder.0, uid])?;

        let left: i64 = self
            .tx
            .prepare_cached(
                "SELECT COUNT(*) FROM remote_uids WHERE folder_id = ?1 AND message_id = ?2",
            )?
            .query_row(params![folder.0, message], |row| row.get(0))?;
        Ok((left == 0).then_some(MessageId(message)))
    }

    /// Oublie **toutes** les copies d'un dossier.
    ///
    /// Ce qu'un `UIDVALIDITY` changé impose : les UID ne désignent plus rien. Les contenus
    /// restent — ils sont adressés par contenu et partagés avec d'autres dossiers — et les
    /// références restent aussi, parce que la moisson complète qui suit va les reposer.
    ///
    /// Rend le nombre de copies oubliées.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn forget_copies(&self, folder: FolderId) -> Result<usize> {
        Ok(self
            .tx
            .prepare_cached("DELETE FROM remote_uids WHERE folder_id = ?1")?
            .execute(params![folder.0])?)
    }

    /// Retire la référence d'un message dans un dossier.
    ///
    /// **Le contenu n'est pas supprimé.** Il peut être référencé par d'autres dossiers, et
    /// c'est le principe même de l'adressage par contenu. Le ramassage des blobs sans
    /// référence est une passe distincte, qui n'existe pas encore.
    ///
    /// Rend `true` si une référence a été retirée.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn remove_ref(&self, message: MessageId, folder: FolderId) -> Result<bool> {
        let changed = self
            .tx
            .prepare_cached("DELETE FROM refs WHERE message_id = ?1 AND folder_id = ?2")?
            .execute(params![message.0, folder.0])?;
        Ok(changed > 0)
    }

    /// Recalcule les drapeaux des références d'un dossier depuis les copies du serveur.
    ///
    /// La règle est dans [`MessageFlags::reduce`] : un OU pour ce qu'une copie suffit à
    /// poser, un ET pour `\Draft` et `\Deleted`. Elle est appliquée **ici, en SQL**, et
    /// l'expression est construite depuis les mêmes constantes que la version Rust — les deux
    /// ne peuvent donc pas diverger sans que le test de parité tombe.
    ///
    /// Rend le nombre de références **réellement modifiées**.
    ///
    /// ## Une ligne identique n'est pas une écriture
    ///
    /// Sans la dernière clause, cette requête réécrivait **toutes** les références du dossier
    /// à chaque passage, même quand la valeur calculée était déjà celle en place. Mesuré le
    /// 2026-09-08 sur un compte réel : 11 900 lignes réécrites sur une synchronisation où rien
    /// n'avait changé — exactement le nombre de références du compte.
    ///
    /// Ça fait échouer la moitié « 0 octet écrit » du critère 3 de `docs/PHASE-2.md`, et ça
    /// fait grossir le journal WAL de SQLite pour rien à chaque passe.
    ///
    /// C'est **la même erreur** que celle corrigée le 2026-09-03 sur
    /// [`Writer::record_copy`] : là aussi, un `UPDATE` inconditionnel comptait une ligne
    /// inchangée comme une écriture. La leçon retenue trop étroitement la première fois est
    /// qu'il fallait aller relire les autres écritures de la même passe, pas seulement celle
    /// qui avait été signalée.
    ///
    /// `IS NOT` et non `<>` : `refs.flags` est déclaré non nul, mais la sous-requête peut
    /// rendre `NULL` si le dossier n'a aucune copie pour ce message — et `NULL <> x` vaut
    /// `NULL`, donc faux, ce qui masquerait la mise à jour au lieu de la faire.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn refresh_ref_flags(&self, folder: FolderId) -> Result<usize> {
        // `MAX(flags & bit)` vaut `bit` dès qu'une copie le porte : c'est le OU.
        // `MIN(flags & bit)` ne vaut `bit` que si toutes le portent : c'est le ET.
        // SQLite n'a pas d'agrégat bit à bit, donc on assemble par drapeau.
        let mut terms = Vec::with_capacity(MessageFlags::ANY_COPY.len() + 1);
        for flag in MessageFlags::ANY_COPY {
            terms.push(format!("MAX(u.flags & {})", flag.bits()));
        }
        for flag in MessageFlags::ALL_COPIES {
            terms.push(format!("MIN(u.flags & {})", flag.bits()));
        }
        let reduced = terms.join(" | ");

        let sql = format!(
            "UPDATE refs SET flags = (
                 SELECT {reduced} FROM remote_uids u
                 WHERE u.folder_id = refs.folder_id AND u.message_id = refs.message_id
             )
             WHERE refs.folder_id = ?1
               AND EXISTS (
                 SELECT 1 FROM remote_uids u
                 WHERE u.folder_id = refs.folder_id AND u.message_id = refs.message_id
               )
               AND refs.flags IS NOT (
                 SELECT {reduced} FROM remote_uids u
                 WHERE u.folder_id = refs.folder_id AND u.message_id = refs.message_id
               )"
        );
        Ok(self.tx.prepare_cached(&sql)?.execute(params![folder.0])?)
    }

    /// Insère un message, ou rend celui qui porte déjà ce contenu.
    ///
    /// Le booléen rendu est `true` si la ligne a été créée. **C'est la mesure de la dédup**
    /// (critère 6) : un `false` veut dire que ce contenu exact était déjà là, vu depuis un
    /// autre dossier.
    ///
    /// L'insertion tente d'abord d'écrire et se rabat sur la lecture en cas de conflit,
    /// plutôt que l'inverse : sur un corpus dont ~72 % des messages sont uniques, écrire
    /// d'abord évite un `SELECT` dans le cas majoritaire.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn insert_message(&self, message: &NewMessage<'_>) -> Result<(MessageId, bool)> {
        let inserted = self
            .tx
            .prepare_cached(
                "INSERT INTO messages
                     (blob_hash, message_id, date, from_addr, from_name, subject, size,
                      has_attachments)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (blob_hash) DO NOTHING
                 RETURNING id",
            )?
            .query_row(
                params![
                    message.blob.as_bytes().as_slice(),
                    message.rfc822_id,
                    message.date,
                    message.from_addr,
                    message.from_name,
                    message.subject,
                    // SQLite ne connaît que des entiers signés 64 bits. Saturer plutôt que
                    // refuser : un message de plus de 8 exaoctets n'existe pas, et perdre
                    // un message pour une taille aberrante serait pire que la stocker mal.
                    i64::try_from(message.size).unwrap_or(i64::MAX),
                    i64::from(message.has_attachments),
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;

        if let Some(id) = inserted {
            return Ok((MessageId(id), true));
        }

        let existing: i64 = self
            .tx
            .prepare_cached("SELECT id FROM messages WHERE blob_hash = ?1")?
            .query_row(params![message.blob.as_bytes().as_slice()], |row| {
                row.get(0)
            })?;
        Ok((MessageId(existing), false))
    }

    /// Efface tous les fils et détache les messages.
    ///
    /// Le threading est une passe reconstructible : elle se refait depuis les blobs, comme
    /// l'index plein texte. La refaire à blanc est plus simple et plus sûr qu'une mise à
    /// jour incrémentale qui devrait fusionner des fils existants.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn clear_threads(&self) -> Result<()> {
        self.tx.execute(
            "UPDATE messages SET thread_id = NULL, thread_link = NULL",
            [],
        )?;
        self.tx.execute("DELETE FROM threads", [])?;
        Ok(())
    }

    /// Crée un fil.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn insert_thread(
        &self,
        root: MessageId,
        subject_norm: &str,
        last_date: i64,
        message_count: u32,
    ) -> Result<ThreadId> {
        self.tx
            .prepare_cached(
                "INSERT INTO threads (root_message_id, subject_norm, last_date, message_count)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![root.0, subject_norm, last_date, message_count])?;
        Ok(ThreadId(self.tx.last_insert_rowid()))
    }

    /// Rattache un message à un fil.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn set_thread(&self, message: MessageId, thread: ThreadId) -> Result<()> {
        self.tx
            .prepare_cached("UPDATE messages SET thread_id = ?2 WHERE id = ?1")?
            .execute(params![message.0, thread.0])?;
        Ok(())
    }
    /// Crée une référence d'un message dans un dossier. Rend `false` si elle existait déjà.
    ///
    /// La date est recopiée depuis le message : c'est la dénormalisation qui permet à
    /// `refs_folder_date` de servir la pagination sans jointure. Sans risque de dérive,
    /// puisqu'elle vient d'un message immuable.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn insert_ref(
        &self,
        message: MessageId,
        folder: FolderId,
        date: i64,
        flags: MessageFlags,
    ) -> Result<bool> {
        let changed = self
            .tx
            .prepare_cached(
                "INSERT INTO refs (message_id, folder_id, date, flags)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (message_id, folder_id) DO NOTHING",
            )?
            .execute(params![message.0, folder.0, date, flags.bits()])?;
        Ok(changed > 0)
    }
}

impl Store {
    /// Ouvre une transaction d'écriture.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si la transaction ne peut pas être ouverte.
    pub fn writer(&self) -> Result<Writer<'_>> {
        Ok(Writer::new(self.connection().unchecked_transaction()?))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{AuthKind, Security};
    use camino::Utf8Path;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let store = Store::open(&root).unwrap();
        (dir, store)
    }

    fn message(blob: &[u8]) -> NewMessage<'static> {
        NewMessage {
            blob: BlobHash::of(blob),
            rfc822_id: Some("<un@exemple.fr>"),
            date: 1_700_000_000,
            from_addr: "plombier@exemple.fr",
            from_name: Some("Le plombier"),
            subject: "facture",
            size: 1234,
            has_attachments: false,
        }
    }

    fn count(store: &Store, table: &str) -> i64 {
        store
            .connection()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn accounts_and_folders_are_idempotent() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();

        let account = writer.upsert_account("imap", "imap.exemple.fr").unwrap();
        let again = writer.upsert_account("imap", "imap.exemple.fr").unwrap();
        assert_eq!(account, again);

        let folder = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let folder_again = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        assert_eq!(folder, folder_again);

        writer.commit().unwrap();
        assert_eq!(count(&store, "accounts"), 1);
        assert_eq!(count(&store, "folders"), 1);
    }

    #[test]
    fn a_rediscovered_folder_role_is_updated() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();

        let folder = writer
            .upsert_folder(account, "Corbeille", FolderKind::Other)
            .unwrap();
        writer
            .upsert_folder(account, "Corbeille", FolderKind::Trash)
            .unwrap();
        writer.commit().unwrap();

        let kind: String = store
            .connection()
            .query_row("SELECT kind FROM folders WHERE id = ?1", [folder.0], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kind, "trash");
    }

    #[test]
    fn the_same_account_name_under_two_kinds_is_two_accounts() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();

        let a = writer.upsert_account("imap", "compte").unwrap();
        let b = writer.upsert_account("local", "compte").unwrap();
        writer.commit().unwrap();

        assert_ne!(a, b);
        assert_eq!(count(&store, "accounts"), 2);
    }

    #[test]
    fn one_message_in_two_folders_is_one_row_and_two_refs() {
        // La thèse du projet, cette fois à travers l'API d'écriture.
        let (_dir, store) = store();
        let writer = store.writer().unwrap();

        let account = writer.upsert_account("imap", "gmail").unwrap();
        let inbox = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let all = writer
            .upsert_folder(account, "[Gmail]/Tous les messages", FolderKind::Archive)
            .unwrap();

        let msg = message(b"un contenu unique");
        let (id, created) = writer.insert_message(&msg).unwrap();
        assert!(created);
        let (same_id, created_again) = writer.insert_message(&msg).unwrap();
        assert_eq!(id, same_id);
        assert!(!created_again, "le doublon a créé une seconde ligne");

        assert!(
            writer
                .insert_ref(id, inbox, msg.date, MessageFlags::SEEN)
                .unwrap()
        );
        assert!(
            writer
                .insert_ref(id, all, msg.date, MessageFlags::empty())
                .unwrap()
        );

        writer.commit().unwrap();
        assert_eq!(count(&store, "messages"), 1);
        assert_eq!(count(&store, "refs"), 2);
    }

    #[test]
    fn the_same_reference_twice_is_reported_not_duplicated() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "gmail").unwrap();
        let inbox = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let (id, _) = writer.insert_message(&message(b"contenu")).unwrap();

        assert!(
            writer
                .insert_ref(id, inbox, 1, MessageFlags::empty())
                .unwrap()
        );
        assert!(
            !writer
                .insert_ref(id, inbox, 1, MessageFlags::empty())
                .unwrap(),
            "la seconde insertion aurait dû être signalée comme déjà présente"
        );

        writer.commit().unwrap();
        assert_eq!(count(&store, "refs"), 1);
    }

    #[test]
    fn flags_belong_to_the_reference_not_to_the_message() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "gmail").unwrap();
        let inbox = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let all = writer
            .upsert_folder(account, "Archive", FolderKind::Archive)
            .unwrap();

        let (id, _) = writer.insert_message(&message(b"contenu")).unwrap();
        writer.insert_ref(id, inbox, 1, MessageFlags::SEEN).unwrap();
        writer
            .insert_ref(id, all, 1, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();

        let mut statement = store
            .connection()
            .prepare("SELECT flags FROM refs WHERE message_id = ?1 ORDER BY folder_id")
            .unwrap();
        let flags: Vec<u32> = statement
            .query_map([id.0], |r| r.get(0))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();

        assert_eq!(flags, vec![MessageFlags::SEEN.bits(), 0]);
    }

    #[test]
    fn a_message_arrives_without_a_thread() {
        // Le threading est une passe séparée : à l'import, `thread_id` doit être NULL.
        let (_dir, store) = store();
        let writer = store.writer().unwrap();
        let (id, _) = writer.insert_message(&message(b"contenu")).unwrap();
        writer.commit().unwrap();

        let thread: Option<i64> = store
            .connection()
            .query_row(
                "SELECT thread_id FROM messages WHERE id = ?1",
                [id.0],
                |r| r.get(0),
            )
            .unwrap();
        assert!(thread.is_none());
    }

    #[test]
    fn an_abandoned_writer_writes_nothing() {
        // Un import interrompu ne doit pas laisser un index à moitié rempli.
        let (_dir, store) = store();
        {
            let writer = store.writer().unwrap();
            writer.upsert_account("imap", "gmail").unwrap();
            writer.insert_message(&message(b"contenu")).unwrap();
            // Pas de commit.
        }
        assert_eq!(count(&store, "accounts"), 0);
        assert_eq!(count(&store, "messages"), 0);
    }

    #[test]
    fn a_message_without_a_message_id_is_accepted() {
        // Des expéditeurs omettent l'en-tête. Ce n'est pas une raison de perdre le message.
        let (_dir, store) = store();
        let writer = store.writer().unwrap();

        let mut msg = message(b"sans identifiant");
        msg.rfc822_id = None;
        msg.from_name = None;
        let (_, created) = writer.insert_message(&msg).unwrap();
        writer.commit().unwrap();

        assert!(created);
        assert_eq!(count(&store, "messages"), 1);
    }

    #[test]
    fn two_messages_may_share_a_message_id() {
        // Le Message-ID vient du réseau : il n'est ni unique ni digne de confiance.
        let (_dir, store) = store();
        let writer = store.writer().unwrap();

        let first = message(b"premier contenu");
        let mut second = message(b"second contenu");
        second.rfc822_id = first.rfc822_id;

        writer.insert_message(&first).unwrap();
        writer.insert_message(&second).unwrap();
        writer.commit().unwrap();

        assert_eq!(count(&store, "messages"), 2);
    }

    #[test]
    fn writes_survive_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();

        {
            let store = Store::open(&root).unwrap();
            let writer = store.writer().unwrap();
            let account = writer.upsert_account("imap", "gmail").unwrap();
            let inbox = writer
                .upsert_folder(account, "INBOX", FolderKind::Inbox)
                .unwrap();
            let (id, _) = writer.insert_message(&message(b"contenu")).unwrap();
            writer
                .insert_ref(id, inbox, 1, MessageFlags::empty())
                .unwrap();
            writer.commit().unwrap();
        }

        let store = Store::open(&root).unwrap();
        assert_eq!(count(&store, "messages"), 1);
        assert_eq!(count(&store, "refs"), 1);
    }

    /// Un serveur de test, sans secret — il n'y a pas de champ pour en mettre.
    fn server() -> Server {
        Server {
            host: "imap.exemple.fr".to_owned(),
            port: 993,
            username: "marie@exemple.fr".to_owned(),
            auth: AuthKind::Password,
            security: Security::Tls,
        }
    }

    #[test]
    fn renaming_an_imap_account_does_not_duplicate_it() {
        // La clé est `(hôte, identifiant)`, pas le nom affiché : renommer « Perso » en
        // « Gmail perso » ne doit pas donner deux comptes et deux fois le courrier.
        let (_dir, store) = store();
        let writer = store.writer().unwrap();

        let first = writer.upsert_imap_account("Perso", &server()).unwrap();
        let second = writer
            .upsert_imap_account("Gmail perso", &server())
            .unwrap();
        writer.commit().unwrap();

        assert_eq!(first, second);
        assert_eq!(count(&store, "accounts"), 1);
        let accounts = store.full_accounts().unwrap();
        assert_eq!(accounts[0].display_name, "Gmail perso");
    }

    #[test]
    fn changing_the_security_mode_is_not_a_new_account() {
        // Passer de STARTTLS à TLS est une correction de configuration, pas un déménagement.
        let (_dir, store) = store();
        let writer = store.writer().unwrap();

        let first = writer.upsert_imap_account("Perso", &server()).unwrap();
        let moved = Server {
            port: 143,
            security: Security::StartTls,
            ..server()
        };
        let second = writer.upsert_imap_account("Perso", &moved).unwrap();
        writer.commit().unwrap();

        assert_eq!(first, second);
        let accounts = store.full_accounts().unwrap();
        assert_eq!(
            accounts[0].server.as_ref().unwrap().security,
            Security::StartTls
        );
        assert_eq!(accounts[0].server.as_ref().unwrap().port, 143);
    }

    #[test]
    fn a_different_mailbox_on_the_same_host_is_another_account() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();

        writer.upsert_imap_account("Marie", &server()).unwrap();
        writer
            .upsert_imap_account(
                "Jean",
                &Server {
                    username: "jean@exemple.fr".to_owned(),
                    ..server()
                },
            )
            .unwrap();
        writer.commit().unwrap();

        assert_eq!(count(&store, "accounts"), 2);
    }

    #[test]
    fn an_mbox_account_has_no_server() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();
        writer.upsert_account("mbox", "Thunderbird").unwrap();
        writer.commit().unwrap();

        let accounts = store.full_accounts().unwrap();
        assert_eq!(accounts[0].kind, crate::model::AccountKind::Mbox);
        assert!(accounts[0].server.is_none());
        assert!(accounts[0].enabled, "un compte est actif par défaut");
    }

    #[test]
    fn an_imap_account_without_a_host_is_refused_not_papered_over() {
        // Le schéma laisse les colonnes de serveur nullables — il le faut pour mbox. La
        // cohérence est donc tenue par le code, et ce test est ce qui l'empêche de l'être
        // en silence : un `Server` avec un hôte vide finirait dans un résolveur.
        let (_dir, store) = store();
        store
            .connection()
            .execute(
                "INSERT INTO accounts (id, kind, display_name) VALUES (7, 'imap', 'cassé')",
                [],
            )
            .unwrap();

        assert!(matches!(
            store.full_accounts(),
            Err(crate::Error::InconsistentAccount { account: 7, .. })
        ));
    }

    #[test]
    fn an_unknown_auth_mechanism_is_refused() {
        // Se tromper de mécanisme, c'est envoyer un secret dans un champ qui ne l'attend pas.
        let (_dir, store) = store();
        store
            .connection()
            .execute(
                "INSERT INTO accounts (id, kind, display_name, host, port, username, auth, security)
                 VALUES (7, 'imap', 'x', 'h', 993, 'u', 'ntlm', 'tls')",
                [],
            )
            .unwrap();

        assert!(matches!(
            store.full_accounts(),
            Err(crate::Error::UnknownAuth { .. })
        ));
    }

    #[test]
    fn disabling_an_account_survives_a_reopen() {
        let (_dir, store) = store();
        let writer = store.writer().unwrap();
        let account = writer.upsert_imap_account("Perso", &server()).unwrap();
        writer.set_account_enabled(account, false).unwrap();
        writer.commit().unwrap();

        assert!(!store.full_accounts().unwrap()[0].enabled);
    }

    /// Un compte IMAP, un dossier, et le message inséré. Rend `(dossier, message)`.
    fn imap_fixture(store: &Store) -> (FolderId, MessageId) {
        let writer = store.writer().unwrap();
        let account = writer.upsert_imap_account("Perso", &server()).unwrap();
        let folder = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let (message, _) = writer.insert_message(&message(b"contenu")).unwrap();
        writer
            .insert_ref(message, folder, 1_700_000_000, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();
        (folder, message)
    }

    fn copy(folder: FolderId, uid: u32, message: MessageId, flags: MessageFlags) -> RemoteCopy {
        RemoteCopy {
            folder,
            uid,
            message,
            flags,
            modseq: None,
        }
    }

    #[test]
    fn the_sync_state_round_trips_including_its_nones() {
        let (_dir, store) = store();
        let (folder, _) = imap_fixture(&store);

        // « Jamais synchronisé » doit rester distinguable de « le serveur a dit zéro ».
        assert_eq!(store.sync_state(folder).unwrap().uidvalidity, None);

        let state = SyncState {
            remote_name: b"INBOX".to_vec(),
            uidvalidity: Some(1_234_567_890),
            uidnext: Some(4242),
            highest_modseq: Some(9_007_199_254_740_993),
            synced_at: Some(1_700_000_000),
            subscribed: true,
        };
        let writer = store.writer().unwrap();
        writer.set_sync_state(folder, &state).unwrap();
        writer.commit().unwrap();

        assert_eq!(store.sync_state(folder).unwrap(), state);
    }

    #[test]
    fn resetting_the_sync_state_clears_every_counter() {
        // Ce qu'un changement d'UIDVALIDITY exige. Un UPDATE partiel laisserait un `uidnext`
        // périmé à côté d'un `uidvalidity` neuf, et la moisson croirait n'avoir rien à faire.
        let (_dir, store) = store();
        let (folder, _) = imap_fixture(&store);

        let writer = store.writer().unwrap();
        writer
            .set_sync_state(
                folder,
                &SyncState {
                    remote_name: b"INBOX".to_vec(),
                    uidvalidity: Some(1),
                    uidnext: Some(9000),
                    highest_modseq: Some(77),
                    synced_at: Some(5),
                    subscribed: true,
                },
            )
            .unwrap();
        writer
            .set_sync_state(
                folder,
                &SyncState {
                    remote_name: b"INBOX".to_vec(),
                    subscribed: true,
                    ..SyncState::default()
                },
            )
            .unwrap();
        writer.commit().unwrap();

        let state = store.sync_state(folder).unwrap();
        assert_eq!(state.uidnext, None, "un compteur périmé a survécu");
        assert_eq!(state.highest_modseq, None);
        assert_eq!(state.synced_at, None);
    }

    #[test]
    fn a_modseq_beyond_63_bits_is_refused_not_truncated() {
        // Tronquer ferait rater des changements pour toujours : la moisson croirait avoir
        // déjà vu ce qui arrive après.
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);
        let writer = store.writer().unwrap();

        let out_of_range = RemoteCopy {
            modseq: Some(u64::MAX),
            ..copy(folder, 1, message, MessageFlags::empty())
        };
        assert!(matches!(
            writer.record_copy(&out_of_range),
            Err(crate::Error::ModseqOutOfRange { found: u64::MAX })
        ));
    }

    #[test]
    fn recording_a_copy_twice_updates_its_flags() {
        // Ce qu'une moisson incrémentale fait à chaque passage.
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);
        let writer = store.writer().unwrap();

        writer
            .record_copy(&copy(folder, 5, message, MessageFlags::empty()))
            .unwrap();
        writer
            .record_copy(&copy(folder, 5, message, MessageFlags::SEEN))
            .unwrap();
        writer.commit().unwrap();

        assert_eq!(count(&store, "remote_uids"), 1);
        assert_eq!(
            store.copy_flags(folder, message).unwrap(),
            vec![MessageFlags::SEEN]
        );
    }

    #[test]
    fn forgetting_one_of_two_copies_does_not_orphan_the_reference() {
        // **Le bug que `remote_uids` existe pour éviter.** Deux UID, même contenu : retirer
        // l'un ne doit pas faire disparaître le message de la liste.
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);
        let writer = store.writer().unwrap();
        for uid in [5, 9] {
            writer
                .record_copy(&copy(folder, uid, message, MessageFlags::empty()))
                .unwrap();
        }

        assert_eq!(
            writer.forget_copy(folder, 5).unwrap(),
            None,
            "la référence serait retirée alors qu'une copie reste"
        );
        assert_eq!(
            writer.forget_copy(folder, 9).unwrap(),
            Some(message),
            "la dernière copie partie, la référence doit tomber"
        );
        writer.commit().unwrap();

        assert_eq!(count(&store, "remote_uids"), 0);
        assert_eq!(store.known_uids(folder).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn forgetting_a_uid_that_was_never_known_is_not_a_reference_to_drop() {
        let (_dir, store) = store();
        let (folder, _) = imap_fixture(&store);
        let writer = store.writer().unwrap();

        assert_eq!(writer.forget_copy(folder, 4242).unwrap(), None);
    }

    #[test]
    fn forgetting_every_copy_of_a_folder_keeps_the_contents() {
        // Ce qu'un `UIDVALIDITY` changé impose : les UID ne désignent plus rien. Les
        // contenus, eux, sont adressés par contenu et partagés avec d'autres dossiers.
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);
        let writer = store.writer().unwrap();
        for uid in [5, 9] {
            writer
                .record_copy(&copy(folder, uid, message, MessageFlags::empty()))
                .unwrap();
        }

        assert_eq!(writer.forget_copies(folder).unwrap(), 2);
        writer.commit().unwrap();

        assert_eq!(count(&store, "remote_uids"), 0);
        assert_eq!(count(&store, "messages"), 1, "un contenu a été supprimé");
        assert_eq!(count(&store, "refs"), 1, "une référence a été supprimée");
    }

    #[test]
    fn removing_a_reference_keeps_the_content() {
        // Le ramassage des blobs sans référence est une passe distincte, et elle n'existe
        // pas encore. Supprimer le contenu ici perdrait ce que d'autres dossiers voient.
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);
        let writer = store.writer().unwrap();

        assert!(writer.remove_ref(message, folder).unwrap());
        assert!(
            !writer.remove_ref(message, folder).unwrap(),
            "retirer deux fois la même référence doit être sans effet"
        );
        writer.commit().unwrap();

        assert_eq!(count(&store, "refs"), 0);
        assert_eq!(count(&store, "messages"), 1);
    }

    #[test]
    fn known_uids_come_back_sorted() {
        // La moisson calcule une différence avec ce que le serveur annonce : l'ordre est
        // une condition, pas une commodité.
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);
        let writer = store.writer().unwrap();
        for uid in [900, 3, 77, 1] {
            writer
                .record_copy(&copy(folder, uid, message, MessageFlags::empty()))
                .unwrap();
        }
        writer.commit().unwrap();

        assert_eq!(store.known_uids(folder).unwrap(), vec![1, 3, 77, 900]);
    }

    #[test]
    fn one_read_copy_makes_the_reference_read() {
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);
        let writer = store.writer().unwrap();
        writer
            .record_copy(&copy(folder, 5, message, MessageFlags::SEEN))
            .unwrap();
        writer
            .record_copy(&copy(folder, 9, message, MessageFlags::empty()))
            .unwrap();
        assert_eq!(writer.refresh_ref_flags(folder).unwrap(), 1);
        writer.commit().unwrap();

        let flags = store.page(folder, None, 10).unwrap()[0].flags;
        assert!(flags.contains(MessageFlags::SEEN), "le contenu a été lu");
    }

    #[test]
    fn recomputing_unchanged_flags_writes_nothing() {
        // **Le critère 3 de `docs/PHASE-2.md` : 0 octet écrit quand rien n'a changé.**
        //
        // Sans la dernière clause de la requête, le second appel réécrivait la ligne avec la
        // valeur qu'elle portait déjà. Mesuré le 2026-09-08 sur un compte réel : 11 900 lignes
        // réécrites sur une synchronisation sans le moindre changement.
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);
        let writer = store.writer().unwrap();
        writer
            .record_copy(&copy(folder, 5, message, MessageFlags::SEEN))
            .unwrap();

        // Le premier appel a du travail : la référence naît sans drapeau.
        assert_eq!(
            writer.refresh_ref_flags(folder).unwrap(),
            1,
            "le premier calcul doit poser le drapeau"
        );
        // Le second n'en a aucun. C'est tout le correctif.
        assert_eq!(
            writer.refresh_ref_flags(folder).unwrap(),
            0,
            "recalculer une valeur identique ne doit pas compter comme une écriture"
        );
        assert_eq!(writer.refresh_ref_flags(folder).unwrap(), 0);
        writer.commit().unwrap();
    }

    #[test]
    fn a_real_change_is_still_written_after_a_no_op_pass() {
        // **Le contrôle inverse.** Une clause trop gourmande ferait passer le test précédent
        // en n'écrivant plus jamais rien, et les drapeaux se figeraient pour toujours — un
        // message lu sur le serveur resterait non lu ici, sans que rien ne le signale.
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);
        let writer = store.writer().unwrap();
        writer
            .record_copy(&copy(folder, 5, message, MessageFlags::empty()))
            .unwrap();
        writer.refresh_ref_flags(folder).unwrap();
        assert_eq!(writer.refresh_ref_flags(folder).unwrap(), 0, "passe à vide");

        // La copie devient lue côté serveur.
        writer
            .record_copy(&copy(folder, 5, message, MessageFlags::SEEN))
            .unwrap();
        assert_eq!(
            writer.refresh_ref_flags(folder).unwrap(),
            1,
            "un vrai changement doit toujours être écrit"
        );
        writer.commit().unwrap();

        let flags = store.page(folder, None, 10).unwrap()[0].flags;
        assert!(flags.contains(MessageFlags::SEEN));
    }

    #[test]
    fn one_undeleted_copy_keeps_the_reference_undeleted() {
        // `\Deleted` veut dire « marqué pour la purge », pas « parti ». Un OU ferait
        // disparaître de la liste un message que le serveur a toujours.
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);
        let writer = store.writer().unwrap();
        writer
            .record_copy(&copy(folder, 5, message, MessageFlags::DELETED))
            .unwrap();
        writer
            .record_copy(&copy(folder, 9, message, MessageFlags::empty()))
            .unwrap();
        writer.refresh_ref_flags(folder).unwrap();
        writer.commit().unwrap();

        let flags = store.page(folder, None, 10).unwrap()[0].flags;
        assert!(
            !flags.contains(MessageFlags::DELETED),
            "une copie non marquée suffit à garder le message"
        );
    }

    #[test]
    fn the_sql_reduction_agrees_with_the_rust_one() {
        // **Le test qui empêche les deux implémentations de dériver.** `refresh_ref_flags`
        // réduit en SQL, `MessageFlags::reduce` en Rust ; les deux servent le même contrat, et
        // seul un test croisé garantit qu'elles disent la même chose.
        //
        // Toutes les paires de drapeaux connus, y compris les cas où les deux copies sont
        // identiques : 32 × 32 combinaisons.
        let (_dir, store) = store();
        let (folder, message) = imap_fixture(&store);

        for left in 0..32_u32 {
            for right in 0..32_u32 {
                let left = MessageFlags::from_bits_truncate(left);
                let right = MessageFlags::from_bits_truncate(right);

                let writer = store.writer().unwrap();
                writer.record_copy(&copy(folder, 5, message, left)).unwrap();
                writer
                    .record_copy(&copy(folder, 9, message, right))
                    .unwrap();
                writer.refresh_ref_flags(folder).unwrap();
                writer.commit().unwrap();

                let sql = store.page(folder, None, 10).unwrap()[0].flags;
                let rust = MessageFlags::reduce(&[left, right]);
                assert_eq!(sql, rust, "SQL et Rust divergent sur {left:?} + {right:?}");
            }
        }
    }

    #[test]
    fn a_single_copy_reduces_to_itself() {
        // Le cas ordinaire : un dossier sans doublon. Ni le OU ni le ET ne doivent rien
        // changer aux drapeaux du serveur.
        for bits in 0..32_u32 {
            let flags = MessageFlags::from_bits_truncate(bits);
            assert_eq!(MessageFlags::reduce(&[flags]), flags);
        }
    }

    #[test]
    fn no_copy_reduces_to_nothing() {
        assert_eq!(MessageFlags::reduce(&[]), MessageFlags::empty());
    }

    #[test]
    fn refreshing_flags_leaves_mbox_references_alone() {
        // Un dossier importé d'un mbox n'a pas de copie côté serveur. Ses drapeaux ne
        // doivent pas être remis à zéro par une passe de synchronisation.
        let (_dir, store) = store();
        let (folder, _) = imap_fixture(&store);
        {
            let writer = store.writer().unwrap();
            let (other, _) = writer.insert_message(&message(b"autre contenu")).unwrap();
            writer
                .insert_ref(other, folder, 1_600_000_000, MessageFlags::SEEN)
                .unwrap();
            writer.commit().unwrap();
        }

        let writer = store.writer().unwrap();
        // Aucune copie enregistrée : la mise à jour ne doit toucher aucune ligne.
        assert_eq!(writer.refresh_ref_flags(folder).unwrap(), 0);
        writer.commit().unwrap();

        let seen = store
            .page(folder, None, 10)
            .unwrap()
            .iter()
            .filter(|it| it.flags.contains(MessageFlags::SEEN))
            .count();
        assert_eq!(seen, 1, "un drapeau venu de l'import a été effacé");
    }

    /// Un compte IMAP complet, pour les tests de soumission.
    fn imap_account(store: &Store) -> AccountId {
        let writer = store.writer().unwrap();
        let id = writer
            .upsert_imap_account(
                "compte",
                &Server {
                    host: "imap.exemple.fr".to_owned(),
                    port: 993,
                    username: "marie@exemple.fr".to_owned(),
                    auth: crate::model::AuthKind::Password,
                    security: crate::model::Security::Tls,
                },
            )
            .unwrap();
        writer.commit().unwrap();
        id
    }

    #[test]
    fn a_submission_server_round_trips_and_falls_back_on_the_reading_identity() {
        let (_dir, store) = store();
        let account = imap_account(&store);
        let writer = store.writer().unwrap();
        writer
            .set_submission(
                account,
                Some(&Server {
                    host: "smtp.exemple.fr".to_owned(),
                    port: 587,
                    // Le même identifiant et le même mécanisme que la lecture : ils ne doivent
                    // donc pas être recopiés en base, et se relire quand même.
                    username: "marie@exemple.fr".to_owned(),
                    auth: crate::model::AuthKind::Password,
                    security: crate::model::Security::StartTls,
                }),
            )
            .unwrap();
        writer.commit().unwrap();

        let accounts = store.full_accounts().unwrap();
        let it = accounts.first().unwrap();
        assert!(it.can_send());
        let submission = it.submission.as_ref().unwrap();
        assert_eq!(submission.host, "smtp.exemple.fr");
        assert_eq!(submission.port, 587);
        assert_eq!(submission.username, "marie@exemple.fr");
        assert_eq!(submission.security, crate::model::Security::StartTls);

        // Et rien n'a été recopié : c'est le repli qui a rempli.
        let stored: Option<String> = store
            .connection()
            .query_row(
                "SELECT smtp_username FROM accounts WHERE id = ?1",
                params![account.0],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stored.is_none(),
            "l'identifiant a été recopié : deux vérités à tenir d'accord pour rien"
        );
    }

    #[test]
    fn a_submission_identity_that_differs_is_kept_as_it_is() {
        // Le contrôle négatif du repli. Certains fournisseurs demandent un identifiant de
        // soumission différent de celui de la lecture, et l'écraser par le repli ferait échouer
        // l'authentification sans dire pourquoi.
        let (_dir, store) = store();
        let account = imap_account(&store);
        let writer = store.writer().unwrap();
        writer
            .set_submission(
                account,
                Some(&Server {
                    host: "smtp.exemple.fr".to_owned(),
                    port: 465,
                    username: "envoi-marie".to_owned(),
                    auth: crate::model::AuthKind::OAuth2,
                    security: crate::model::Security::Tls,
                }),
            )
            .unwrap();
        writer.commit().unwrap();

        let accounts = store.full_accounts().unwrap();
        let submission = accounts[0].submission.as_ref().unwrap();
        assert_eq!(submission.username, "envoi-marie");
        assert_eq!(submission.auth, crate::model::AuthKind::OAuth2);
    }

    #[test]
    fn a_half_configured_submission_is_refused_rather_than_guessed() {
        // **La règle de correction de ce champ.** Un hôte sans mode de chiffrement ne dit pas
        // s'il faut du TLS direct ou un `STARTTLS`. Deviner rétrograderait le chiffrement à
        // l'insu de l'utilisateur — et un `SUBMIT` en clair transporte le mot de passe **et**
        // le message.
        let (_dir, store) = store();
        let account = imap_account(&store);
        store
            .connection()
            .execute(
                "UPDATE accounts SET smtp_host = 'smtp.exemple.fr' WHERE id = ?1",
                params![account.0],
            )
            .unwrap();

        assert!(
            matches!(
                store.full_accounts(),
                Err(crate::Error::InconsistentAccount { .. })
            ),
            "un serveur de soumission à moitié configuré a été accepté"
        );
    }

    #[test]
    fn an_absent_submission_port_falls_back_on_the_one_the_mode_implies() {
        let (_dir, store) = store();
        let account = imap_account(&store);
        store
            .connection()
            .execute(
                "UPDATE accounts SET smtp_host = 'smtp.exemple.fr', smtp_security = 'starttls'
                  WHERE id = ?1",
                params![account.0],
            )
            .unwrap();

        let accounts = store.full_accounts().unwrap();
        assert_eq!(accounts[0].submission.as_ref().unwrap().port, 587);
    }

    #[test]
    fn clearing_the_submission_makes_the_account_unable_to_send() {
        let (_dir, store) = store();
        let account = imap_account(&store);
        let writer = store.writer().unwrap();
        writer
            .set_submission(
                account,
                Some(&Server {
                    host: "smtp.exemple.fr".to_owned(),
                    port: 587,
                    username: "marie@exemple.fr".to_owned(),
                    auth: crate::model::AuthKind::Password,
                    security: crate::model::Security::StartTls,
                }),
            )
            .unwrap();
        writer.commit().unwrap();
        assert!(store.full_accounts().unwrap()[0].can_send());

        let writer = store.writer().unwrap();
        writer.set_submission(account, None).unwrap();
        writer.commit().unwrap();

        let accounts = store.full_accounts().unwrap();
        assert!(!accounts[0].can_send());
        assert!(accounts[0].submission.is_none());
    }

    /// Une signature riche, avec ce qu'un aller-retour peut abîmer : un accent, un gras, un
    /// lien, une puce, et **une ligne blanche** — celle qui a décidé de la forme rangée.
    fn signature() -> mailhtml::rich::Document {
        let mut it = mailhtml::rich::Document::plain(
            "Cordialement,\n\nÉloïse Durand\ndirectrice\nhttps://exemple.fr",
        );
        it.apply(
            15,
            28,
            &mailhtml::rich::Style {
                bold: true,
                ..mailhtml::rich::Style::default()
            },
        );
        it.apply(
            40,
            58,
            &mailhtml::rich::Style {
                link: Some("https://exemple.fr".to_owned()),
                ..mailhtml::rich::Style::default()
            },
        );
        it.toggle_bullet(3);
        it
    }

    #[test]
    fn a_signature_round_trips_through_the_store_with_its_blank_line() {
        // **La raison d'être de la colonne.** Ranger le HTML perdrait la ligne blanche après
        // « Cordialement, » à chaque ouverture de l'éditeur : voir `SCHEMA_V8`.
        let (_dir, store) = store();
        let account = imap_account(&store);
        let writer = store.writer().unwrap();
        writer.set_signature(account, Some(&signature())).unwrap();
        writer.commit().unwrap();

        let back = store.signature(account).unwrap().unwrap();
        assert_eq!(back, signature());
        assert!(back.text.contains("\n\n"), "la ligne blanche a disparu");
        assert_eq!(back.blocks[3], mailhtml::rich::Block::Bullet);
        assert_eq!(
            back.style_at(41).link.as_deref(),
            Some("https://exemple.fr")
        );
    }

    #[test]
    fn a_signature_survives_a_reopen() {
        // Le contrôle qui dit que la valeur est bien sur le disque et pas dans un cache : le
        // même dossier, un `Store` neuf.
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let account = {
            let store = Store::open(&root).unwrap();
            let account = imap_account(&store);
            let writer = store.writer().unwrap();
            writer.set_signature(account, Some(&signature())).unwrap();
            writer.commit().unwrap();
            account
        };
        let store = Store::open(&root).unwrap();
        assert_eq!(store.signature(account).unwrap(), Some(signature()));
    }

    #[test]
    fn an_empty_signature_clears_the_column_rather_than_storing_a_blank() {
        // Une signature réduite à des blancs n'est pas une signature : la garder ferait sortir
        // une ligne vide en fin de chaque message sans que personne ne l'ait demandée.
        let (_dir, store) = store();
        let account = imap_account(&store);
        let writer = store.writer().unwrap();
        writer.set_signature(account, Some(&signature())).unwrap();
        writer
            .set_signature(account, Some(&mailhtml::rich::Document::plain("   \n  ")))
            .unwrap();
        writer.commit().unwrap();
        assert_eq!(store.signature(account).unwrap(), None);

        let writer = store.writer().unwrap();
        writer.set_signature(account, Some(&signature())).unwrap();
        writer.set_signature(account, None).unwrap();
        writer.commit().unwrap();
        assert_eq!(
            store.signature(account).unwrap(),
            None,
            "None n'a pas effacé"
        );
    }

    #[test]
    fn a_signature_is_per_account() {
        // Le carnet, la file d'envoi et les drapeaux sont par compte ; la signature aussi. Une
        // signature qui déborderait sur un autre compte signerait un message professionnel
        // d'une adresse personnelle.
        let (_dir, store) = store();
        let un = imap_account(&store);
        let writer = store.writer().unwrap();
        let deux = writer.upsert_account("mbox", "autre").unwrap();
        writer.set_signature(un, Some(&signature())).unwrap();
        writer.commit().unwrap();
        assert!(store.signature(un).unwrap().is_some());
        assert_eq!(store.signature(deux).unwrap(), None);
    }

    #[test]
    fn an_unreadable_signature_column_reads_as_no_signature() {
        // **Le cas qui décide que la lecture ne rend pas d'erreur.** Une signature abîmée — une
        // migration bricolée, un binaire plus vieux — ne doit pas empêcher d'écrire un message.
        // Un refus ici ferait échouer la fenêtre de rédaction sur un champ décoratif.
        let (_dir, store) = store();
        let account = imap_account(&store);
        for hostile in [
            "pas du json",
            "{}",
            "null",
            "[]",
            r#"{"text":"ok","spans":"pas une liste"}"#,
            "",
        ] {
            store
                .connection()
                .execute(
                    "UPDATE accounts SET signature = ?2 WHERE id = ?1",
                    params![account.0, hostile],
                )
                .unwrap();
            assert_eq!(store.signature(account).unwrap(), None, "{hostile}");
        }
    }

    #[test]
    fn a_signature_whose_invariants_are_broken_is_put_back_in_order_on_the_way_out() {
        // Un document venu du store peut porter des intervalles dans le désordre, des natures
        // de ligne en trop, ou un intervalle vide. Ils sont **rétablis**, pas supposés : la
        // suite du code — la mise en page, la sortie HTML — parcourt les intervalles une fois
        // en supposant qu'ils sont triés.
        let (_dir, store) = store();
        let account = imap_account(&store);
        let broken = r#"{
            "text": "deux lignes\nici",
            "spans": [
                {"at": 12, "len": 3, "style": {"bold": true, "italic": false, "link": null}},
                {"at": 0,  "len": 4, "style": {"bold": true, "italic": false, "link": null}},
                {"at": 5,  "len": 0, "style": {"bold": true, "italic": false, "link": null}}
            ],
            "blocks": ["Paragraph", "Paragraph", "Bullet", "Bullet"]
        }"#;
        store
            .connection()
            .execute(
                "UPDATE accounts SET signature = ?2 WHERE id = ?1",
                params![account.0, broken],
            )
            .unwrap();

        let back = store.signature(account).unwrap().unwrap();
        assert_eq!(
            back.spans.iter().map(|it| it.at).collect::<Vec<_>>(),
            vec![0, 12],
            "les intervalles ne sont pas triés, ou le vide a survécu"
        );
        assert_eq!(back.blocks.len(), 2, "une nature par ligne, et deux lignes");
        // Et la sortie HTML est celle du document remis d'aplomb.
        assert_eq!(back.to_html(), "<p><b>deux</b> lignes</p><p><b>ici</b></p>");
    }
}

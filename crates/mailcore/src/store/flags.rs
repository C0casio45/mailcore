//! Marquer un message lu, et retenir ce qu'il reste à dire au serveur.
//!
//! ## L'ordre des trois écritures
//!
//! Marquer un message lu touche trois choses, et l'ordre n'est pas indifférent :
//!
//! 1. `remote_uids.flags` — la marque sur chaque copie du dossier. C'est la table dont
//!    `refs.flags` dérive, donc c'est là qu'il faut écrire pour que la liste change ;
//! 2. `refs.flags` — recalculé depuis les copies, pour que la liste n'attende pas une moisson ;
//! 3. `flag_pushes` — ce qu'il reste à pousser vers le serveur.
//!
//! Les trois dans **une seule transaction** : une marque locale sans sa poussée en attente
//! serait effacée au prochain passage, et une poussée sans marque locale afficherait « non lu »
//! sur un message que le serveur sait lu.
//!
//! ## Ce que ça ne fait pas
//!
//! Aucun réseau. La poussée est faite par la moisson, qui a la connexion — voir
//! `mailsync::sync`. C'est la règle 3 du `CLAUDE.md` : marquer un message lu est un clic, et un
//! clic n'attend pas un serveur.

use rusqlite::params;

use crate::error::Result;
use crate::model::{FolderId, MessageFlags, MessageId};
use crate::store::Store;

/// Une copie à laquelle il reste à dire `\Seen` sur le serveur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingSeen {
    /// Le dossier.
    pub folder: FolderId,
    /// L'UID de la copie dans ce dossier.
    pub uid: u32,
    /// Quand la marque a été posée localement, en secondes Unix.
    pub marked_at: i64,
}

impl Store {
    /// Marque un message lu dans un dossier, localement, et note la poussée à faire.
    ///
    /// ## Idempotent, et c'est ce qui permet de l'appeler à chaque ouverture
    ///
    /// La coquille appelle à l'ouverture d'un message, sans savoir s'il était déjà lu. Un
    /// message déjà lu ne produit donc **aucune écriture** : ni marque, ni poussée. Sans ça,
    /// relire trois fois le même message mettrait trois poussées en attente et déclencherait
    /// trois `UID STORE` pour rien.
    ///
    /// ## Elle ne touche que les copies du dossier demandé
    ///
    /// Le serveur ne connaît pas les messages, il connaît des UID dans des boîtes. Marquer le
    /// même contenu dans « Tous les messages » serait une décision de plus — et une écriture de
    /// plus sur le serveur — que l'utilisateur n'a pas demandée en ouvrant un message d'`INBOX`.
    /// Gmail lie les copies de lui-même ; les autres serveurs non, et c'est leur affaire.
    ///
    /// ## Un message sans copie côté serveur se marque quand même
    ///
    /// Un message importé d'un mbox n'a pas d'UID : il n'existe sur aucun serveur, donc il n'y
    /// a rien à pousser. La première version n'écrivait alors **rien du tout** — et sur un
    /// store dont l'essentiel vient d'un import Thunderbird, ouvrir un message ne le marquait
    /// jamais lu. C'est ce que l'utilisateur a vu.
    ///
    /// La marque va donc dans `refs` directement dans ce cas, et **ça ne contredit pas** la
    /// règle « `refs.flags` est dérivé de `remote_uids.flags` » : [`Writer::refresh_ref_flags`]
    /// ne recalcule que les références qui **ont** des copies (`AND EXISTS (…)`). Une marque
    /// locale sur un message qui n'en a pas ne peut donc pas être effacée par le recalcul —
    /// c'est vérifié par un test.
    ///
    /// [`Writer::refresh_ref_flags`]: crate::Writer::refresh_ref_flags
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si une écriture échoue. Rend le nombre de lignes marquées — les
    /// copies quand il y en a, la référence sinon — donc **zéro** quand il n'y avait rien à
    /// faire.
    pub fn mark_seen(&self, message: MessageId, folder: FolderId, now: i64) -> Result<usize> {
        let seen = MessageFlags::SEEN.bits();
        let tx = self.connection().unchecked_transaction()?;
        let marked = {
            // 1. Les copies. `& ~seen = 0` filtre celles qui l'ont déjà : sans ce filtre,
            //    l'`UPDATE` toucherait toutes les lignes et le compte rendu serait faux.
            let changed = tx.execute(
                "UPDATE remote_uids SET flags = flags | ?3
                  WHERE folder_id = ?2 AND message_id = ?1 AND (flags & ?3) = 0",
                params![message.0, folder.0, seen],
            )?;
            if changed == 0 {
                // Aucune copie n'a bougé. Deux causes très différentes, et une seule requête
                // les distingue : soit les copies portaient déjà le drapeau — rien à faire —
                // soit il n'y a pas de copie du tout, et la marque est purement locale.
                //
                // Le filtre `(flags & ?3) = 0` rend la marque locale idempotente, comme celle
                // des copies : relire trois fois n'écrit qu'une fois.
                tx.execute(
                    "UPDATE refs SET flags = flags | ?3
                      WHERE folder_id = ?2 AND message_id = ?1 AND (flags & ?3) = 0
                        AND NOT EXISTS (
                          SELECT 1 FROM remote_uids u
                           WHERE u.folder_id = refs.folder_id
                             AND u.message_id = refs.message_id
                        )",
                    params![message.0, folder.0, seen],
                )?
            } else {
                // 2. La référence, pour que la liste change tout de suite. Le même OU que
                //    `refresh_ref_flags` calcule, restreint à ce message.
                tx.execute(
                    "UPDATE refs SET flags = flags | ?3
                      WHERE folder_id = ?2 AND message_id = ?1",
                    params![message.0, folder.0, seen],
                )?;

                // 3. Ce qu'il reste à dire au serveur, une ligne par copie.
                //
                //    `INSERT OR IGNORE` : une poussée déjà en attente pour cette copie n'a pas
                //    à être réécrite, et son `marked_at` d'origine est plus utile que le
                //    nouveau — c'est lui qui dit depuis quand ça traîne.
                tx.execute(
                    "INSERT OR IGNORE INTO flag_pushes (folder_id, uid, marked_at)
                     SELECT folder_id, uid, ?3 FROM remote_uids
                      WHERE folder_id = ?2 AND message_id = ?1",
                    params![message.0, folder.0, now],
                )?;
                changed
            }
        };
        tx.commit()?;
        if marked > 0 {
            tracing::debug!(message = message.0, folder = folder.0, marked, "marqué lu");
        }
        Ok(marked)
    }

    /// Les poussées `\Seen` en attente pour un dossier, du plus ancien au plus récent.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn pending_seen(&self, folder: FolderId) -> Result<Vec<PendingSeen>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT uid, marked_at FROM flag_pushes WHERE folder_id = ?1 ORDER BY marked_at, uid",
        )?;
        let rows = statement.query_map(params![folder.0], |row| {
            let uid: i64 = row.get(0)?;
            Ok(PendingSeen {
                folder,
                uid: u32::try_from(uid).unwrap_or(u32::MAX),
                marked_at: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Toutes les poussées en attente, tous dossiers confondus. Pour `mail doctor`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn pending_seen_count(&self) -> Result<u64> {
        Ok(self
            .connection()
            .query_row("SELECT count(*) FROM flag_pushes", [], |row| {
                let count: i64 = row.get(0)?;
                Ok(u64::try_from(count).unwrap_or(0))
            })?)
    }

    /// Oublie des poussées, **après** que le serveur les a acceptées.
    ///
    /// ## Jamais avant
    ///
    /// Les retirer avant la réponse du serveur perdrait la marque : la moisson suivante relirait
    /// « non lu » du serveur, l'écrirait dans `remote_uids`, et plus rien ne saurait qu'il fallait
    /// pousser. Le message redeviendrait non lu tout seul — exactement le défaut que cette table
    /// existe pour empêcher.
    ///
    /// Le sens de l'erreur est choisi : une poussée oubliée trop tard est refaite, et un
    /// `UID STORE +FLAGS (\Seen)` sur un message déjà lu ne fait rien. Trop tôt, elle est perdue.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn forget_pending_seen(&self, folder: FolderId, uids: &[u32]) -> Result<usize> {
        if uids.is_empty() {
            return Ok(0);
        }
        let tx = self.connection().unchecked_transaction()?;
        let mut removed = 0;
        {
            let mut statement =
                tx.prepare_cached("DELETE FROM flag_pushes WHERE folder_id = ?1 AND uid = ?2")?;
            for uid in uids {
                removed += statement.execute(params![folder.0, i64::from(*uid)])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::{BlobHash, FolderKind, RemoteCopy};
    use crate::store::write::NewMessage;

    /// Un store avec un message dans `INBOX`, non lu, et une copie côté serveur à l'UID 5.
    fn fixture() -> (tempfile::TempDir, Store, MessageId, FolderId) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let store = Store::open(&root).unwrap();

        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();
        let folder = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let (message, _) = writer
            .insert_message(&NewMessage {
                blob: BlobHash::of(b"un message"),
                rfc822_id: None,
                date: 1_700_000_000,
                from_addr: "a@b.c",
                from_name: None,
                subject: "sujet",
                size: 10,
                has_attachments: false,
            })
            .unwrap();
        writer
            .insert_ref(message, folder, 1_700_000_000, MessageFlags::empty())
            .unwrap();
        writer
            .record_copy(&RemoteCopy {
                folder,
                uid: 5,
                message,
                flags: MessageFlags::empty(),
                modseq: None,
            })
            .unwrap();
        writer.commit().unwrap();
        (dir, store, message, folder)
    }

    fn ref_flags(store: &Store, message: MessageId, folder: FolderId) -> MessageFlags {
        let bits: i64 = store
            .connection()
            .query_row(
                "SELECT flags FROM refs WHERE message_id = ?1 AND folder_id = ?2",
                params![message.0, folder.0],
                |row| row.get(0),
            )
            .unwrap();
        MessageFlags::from_bits_truncate(u32::try_from(bits).unwrap_or(0))
    }

    #[test]
    fn marking_seen_changes_the_list_and_queues_the_push() {
        // **Les trois écritures.** Sans la première, la liste ne change pas ; sans la
        // troisième, elle rechange toute seule à la moisson suivante.
        let (_dir, store, message, folder) = fixture();
        assert!(!ref_flags(&store, message, folder).contains(MessageFlags::SEEN));

        assert_eq!(store.mark_seen(message, folder, 2_000).unwrap(), 1);
        assert!(
            ref_flags(&store, message, folder).contains(MessageFlags::SEEN),
            "la liste dit encore « non lu »"
        );

        let pending = store.pending_seen(folder).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].uid, 5);
        assert_eq!(pending[0].marked_at, 2_000);
    }

    #[test]
    fn marking_a_message_already_read_writes_nothing() {
        // La coquille appelle à chaque ouverture sans savoir. Sans ce court-circuit, relire
        // trois fois le même message mettrait trois poussées en attente.
        let (_dir, store, message, folder) = fixture();
        assert_eq!(store.mark_seen(message, folder, 2_000).unwrap(), 1);
        store.forget_pending_seen(folder, &[5]).unwrap();

        assert_eq!(
            store.mark_seen(message, folder, 3_000).unwrap(),
            0,
            "un message déjà lu a été remarqué"
        );
        assert!(
            store.pending_seen(folder).unwrap().is_empty(),
            "une poussée a été remise en attente pour rien"
        );
    }

    #[test]
    fn marking_twice_keeps_the_first_timestamp() {
        // `marked_at` dit **depuis quand ça traîne** : l'écraser à chaque ouverture ferait
        // paraître neuve une poussée qui attend depuis des jours, donc masquerait un compte
        // qui ne se synchronise plus.
        let (_dir, store, message, folder) = fixture();
        store.mark_seen(message, folder, 2_000).unwrap();
        // Le drapeau est retiré des copies à la main pour forcer un second marquage réel.
        store
            .connection()
            .execute("UPDATE remote_uids SET flags = 0", [])
            .unwrap();
        store.mark_seen(message, folder, 9_000).unwrap();

        assert_eq!(store.pending_seen(folder).unwrap()[0].marked_at, 2_000);
    }

    #[test]
    fn a_message_without_a_server_copy_is_marked_locally_and_promises_nothing() {
        // **Ce test disait l'inverse, et il avait tort sur une moitié.** Sa raison écrite était
        // « marquer promettrait au serveur une poussée sur une copie qui n'existe pas » — ce
        // qui reste vrai et reste vérifié ci-dessous. Mais il en tirait aussi qu'il ne faut
        // rien marquer **du tout**, et la conséquence était visible : sur un store venu d'un
        // import Thunderbird, où aucun message n'a d'UID, ouvrir un message ne le marquait
        // jamais lu. L'utilisateur l'a signalé.
        //
        // La marque va donc dans `refs`, et rien ne part vers le serveur.
        let (_dir, store, message, folder) = fixture();
        store
            .connection()
            .execute("DELETE FROM remote_uids", [])
            .unwrap();

        assert_eq!(store.mark_seen(message, folder, 2_000).unwrap(), 1);
        assert!(
            store.pending_seen(folder).unwrap().is_empty(),
            "une poussée a été promise sur une copie qui n'existe pas"
        );
        assert!(
            ref_flags(&store, message, folder).contains(MessageFlags::SEEN),
            "la liste continuerait d'afficher « non lu »"
        );

        // Idempotente, comme la marque des copies : relire trois fois n'écrit qu'une fois.
        assert_eq!(store.mark_seen(message, folder, 3_000).unwrap(), 0);
    }

    #[test]
    fn a_local_mark_is_not_erased_by_a_flag_refresh() {
        // **La question que la règle « `refs.flags` est dérivé » pose à ce chemin.** Le recalcul
        // ne touche que les références qui **ont** des copies (`AND EXISTS (…)`), donc une
        // marque locale sur un message qui n'en a pas ne peut pas être effacée. Si cette clause
        // disparaissait un jour, ce test tomberait — et c'est exactement ce qu'on veut, parce
        // que le drapeau se remettrait alors à zéro dans le dos de l'utilisateur.
        let (_dir, store, message, folder) = fixture();
        store
            .connection()
            .execute("DELETE FROM remote_uids", [])
            .unwrap();
        store.mark_seen(message, folder, 2_000).unwrap();

        let writer = store.writer().unwrap();
        writer.refresh_ref_flags(folder).unwrap();
        writer.commit().unwrap();

        assert!(
            ref_flags(&store, message, folder).contains(MessageFlags::SEEN),
            "le recalcul a effacé une marque locale"
        );
    }

    #[test]
    fn the_local_mark_survives_a_flag_refresh() {
        // **Le défaut que cette conception évite.** `refs.flags` est dérivé de
        // `remote_uids.flags` : écrire « lu » dans `refs` seul serait effacé au premier
        // recalcul. La marque va donc dans les copies, et le recalcul la retrouve.
        let (_dir, store, message, folder) = fixture();
        store.mark_seen(message, folder, 2_000).unwrap();

        let writer = store.writer().unwrap();
        writer.refresh_ref_flags(folder).unwrap();
        writer.commit().unwrap();

        assert!(
            ref_flags(&store, message, folder).contains(MessageFlags::SEEN),
            "le recalcul a effacé la marque locale"
        );
    }

    #[test]
    fn forgetting_a_push_that_does_not_exist_is_not_an_error() {
        let (_dir, store, _message, folder) = fixture();
        assert_eq!(store.forget_pending_seen(folder, &[]).unwrap(), 0);
        assert_eq!(store.forget_pending_seen(folder, &[404]).unwrap(), 0);
    }

    #[test]
    fn deleting_a_folder_takes_its_pending_pushes_with_it() {
        // Sans la cascade, une poussée resterait pour un dossier que plus rien ne moissonne :
        // elle ne partirait jamais et compterait pour toujours dans `mail doctor`.
        let (_dir, store, message, folder) = fixture();
        store.mark_seen(message, folder, 2_000).unwrap();
        store
            .connection()
            .execute("DELETE FROM folders WHERE id = ?1", params![folder.0])
            .unwrap();

        assert_eq!(store.pending_seen_count().unwrap(), 0);
    }
}

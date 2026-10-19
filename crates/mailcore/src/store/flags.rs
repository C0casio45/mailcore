//! Marquer un message lu ou non lu, et retenir ce qu'il reste à dire au serveur.
//!
//! ## L'ordre des trois écritures
//!
//! Marquer un message touche trois choses, et l'ordre n'est pas indifférent :
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
//! ## Les deux sens sont une seule fonction
//!
//! « Lu » et « non lu » sont le même bit écrit dans deux sens : [`Store::mark_seen`] et
//! [`Store::mark_unseen`] appellent toutes deux `mark`. Deux copies de ces trois écritures
//! finiraient par ne plus avoir le même ordre, et l'ordre est ce qui rend la marque durable.
//!
//! **Le dernier geste remplace le précédent.** Une copie n'a qu'une poussée en attente — la clé
//! de `flag_pushes` est `(folder_id, uid)` —, et elle porte le sens voulu. Marquer non lu un
//! message dont le « lu » n'est pas encore parti retourne la poussée au lieu d'en ajouter une
//! seconde : avec deux, l'ordre dans lequel la moisson les enverrait déciderait de l'état final.
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

/// Une copie dont il reste à dire au serveur si elle est lue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingSeen {
    /// Le dossier.
    pub folder: FolderId,
    /// L'UID de la copie dans ce dossier.
    pub uid: u32,
    /// Quand la marque a été posée localement, en secondes Unix. Un changement de sens la
    /// remet à l'heure du nouveau geste : c'est une autre demande.
    pub marked_at: i64,
    /// Le sens : `true` pour poser `\Seen`, `false` pour le retirer.
    pub seen: bool,
}

impl Store {
    /// Marque un message non lu dans un dossier, localement, et note la poussée à faire.
    ///
    /// La symétrique de [`Store::mark_seen`], avec les mêmes propriétés : idempotente, limitée
    /// aux copies du dossier demandé, et purement locale pour un message sans copie côté
    /// serveur. Elle retire `\Seen` et **seulement** lui : `\Flagged` et `\Answered` restent ce
    /// qu'ils étaient, côté store comme côté serveur (`-FLAGS`, jamais `FLAGS`).
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`] si une écriture échoue. Rend le nombre de lignes marquées, donc
    /// **zéro** quand le message était déjà non lu.
    pub fn mark_unseen(&self, message: MessageId, folder: FolderId, now: i64) -> Result<usize> {
        self.mark(message, folder, now, false)
    }

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
        self.mark(message, folder, now, true)
    }

    /// Les trois écritures, dans un sens ou dans l'autre. Voir le module.
    fn mark(&self, message: MessageId, folder: FolderId, now: i64, seen: bool) -> Result<usize> {
        let bit = MessageFlags::SEEN.bits();
        // La valeur que le bit doit prendre. Une ligne « à changer » est une ligne où il vaut
        // autre chose : `(flags & bit) <> want` est le filtre d'idempotence dans les deux sens.
        let want = if seen { bit } else { 0 };
        let tx = self.connection().unchecked_transaction()?;
        let marked = {
            // 1. Les copies. Le filtre écarte celles qui sont déjà dans le bon état : sans lui,
            //    l'`UPDATE` toucherait toutes les lignes et le compte rendu serait faux.
            let changed = tx.execute(
                "UPDATE remote_uids SET flags = (flags & ~?3) | ?4
                  WHERE folder_id = ?2 AND message_id = ?1 AND (flags & ?3) <> ?4",
                params![message.0, folder.0, bit, want],
            )?;
            if changed == 0 {
                // Aucune copie n'a bougé. Deux causes très différentes, et une seule requête
                // les distingue : soit les copies étaient déjà dans le bon état — rien à faire
                // — soit il n'y a pas de copie du tout, et la marque est purement locale.
                //
                // Le même filtre rend la marque locale idempotente, comme celle des copies :
                // relire trois fois n'écrit qu'une fois.
                tx.execute(
                    "UPDATE refs SET flags = (flags & ~?3) | ?4
                      WHERE folder_id = ?2 AND message_id = ?1 AND (flags & ?3) <> ?4
                        AND NOT EXISTS (
                          SELECT 1 FROM remote_uids u
                           WHERE u.folder_id = refs.folder_id
                             AND u.message_id = refs.message_id
                        )",
                    params![message.0, folder.0, bit, want],
                )?
            } else {
                // 2. La référence, pour que la liste change tout de suite. Toutes les copies du
                //    dossier ont maintenant le même bit, donc le OU que `refresh_ref_flags`
                //    calcule vaut ce bit : l'écrire directement donne le même résultat.
                tx.execute(
                    "UPDATE refs SET flags = (flags & ~?3) | ?4
                      WHERE folder_id = ?2 AND message_id = ?1",
                    params![message.0, folder.0, bit, want],
                )?;

                // 3. Ce qu'il reste à dire au serveur, une ligne par copie.
                //
                //    Une poussée déjà en attente **dans le même sens** n'est pas réécrite : son
                //    `marked_at` d'origine dit depuis quand ça traîne, et c'est le plus utile.
                //    Dans l'autre sens, elle est **remplacée** — voir le module. SQLite exige un
                //    `WHERE` dans un `INSERT … SELECT` suivi d'`ON CONFLICT`, sans quoi le `ON`
                //    se lit comme une jointure : celui-ci filtre réellement les copies.
                tx.execute(
                    "INSERT INTO flag_pushes (folder_id, uid, marked_at, seen)
                     SELECT folder_id, uid, ?3, ?4 FROM remote_uids
                      WHERE folder_id = ?2 AND message_id = ?1
                     ON CONFLICT (folder_id, uid) DO UPDATE
                        SET seen = excluded.seen, marked_at = excluded.marked_at
                      WHERE flag_pushes.seen <> excluded.seen",
                    params![message.0, folder.0, now, seen],
                )?;
                changed
            }
        };
        tx.commit()?;
        if marked > 0 {
            tracing::debug!(
                message = message.0,
                folder = folder.0,
                marked,
                seen,
                "marqué"
            );
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
            "SELECT uid, marked_at, seen FROM flag_pushes
              WHERE folder_id = ?1 ORDER BY marked_at, uid",
        )?;
        let rows = statement.query_map(params![folder.0], |row| {
            let uid: i64 = row.get(0)?;
            Ok(PendingSeen {
                folder,
                uid: u32::try_from(uid).unwrap_or(u32::MAX),
                marked_at: row.get(1)?,
                seen: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Les poussées en attente d'un dossier, **par message** et non par UID : le dernier geste de
    /// chaque message, lu ou non lu.
    ///
    /// Pour une renumérotation du dossier : les UID en attente ne valent plus rien, mais tant que
    /// `remote_uids` porte encore l'ancienne numérotation, on sait quel message chacun désignait.
    /// À lire **avant** `Writer::forget_copies`, qui efface les deux. Voir
    /// `mailsync::sync::harvest_watched`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn pending_seen_by_message(&self, folder: FolderId) -> Result<Vec<(MessageId, bool)>> {
        // Le geste le plus récent l'emporte : SQLite rend, pour une colonne nue d'un `GROUP BY`
        // avec `max()`, la valeur de la ligne qui porte le maximum.
        let mut statement = self.connection().prepare_cached(
            "SELECT u.message_id, p.seen, max(p.marked_at)
               FROM flag_pushes p
               JOIN remote_uids u ON u.folder_id = p.folder_id AND u.uid = p.uid
              WHERE p.folder_id = ?1
              GROUP BY u.message_id
              ORDER BY u.message_id",
        )?;
        let rows = statement.query_map(params![folder.0], |row| {
            Ok((MessageId(row.get(0)?), row.get(1)?))
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
    /// ## Seulement dans le sens qui a été poussé
    ///
    /// Elle prend les poussées **telles qu'elles ont été lues**, sens compris, et n'efface une
    /// ligne que si elle porte encore ce sens. Entre la lecture et la réponse du serveur, un
    /// clic peut retourner la marque : effacer par UID seul jetterait ce nouveau geste, et le
    /// serveur garderait l'état d'avant sans que rien ne sache qu'il fallait le changer.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn forget_pending_seen(&self, folder: FolderId, pushed: &[PendingSeen]) -> Result<usize> {
        if pushed.is_empty() {
            return Ok(0);
        }
        let tx = self.connection().unchecked_transaction()?;
        let mut removed = 0;
        {
            let mut statement = tx.prepare_cached(
                "DELETE FROM flag_pushes WHERE folder_id = ?1 AND uid = ?2 AND seen = ?3",
            )?;
            for push in pushed {
                removed += statement.execute(params![folder.0, i64::from(push.uid), push.seen])?;
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
        let pushed = store.pending_seen(folder).unwrap();
        store.forget_pending_seen(folder, &pushed).unwrap();

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
        let absent = PendingSeen {
            folder,
            uid: 404,
            marked_at: 0,
            seen: true,
        };
        assert_eq!(store.forget_pending_seen(folder, &[absent]).unwrap(), 0);
    }

    /// Le fixture, lu puis poussé : le serveur sait le message lu, rien n'est en attente.
    fn read_and_pushed() -> (tempfile::TempDir, Store, MessageId, FolderId) {
        let (dir, store, message, folder) = fixture();
        store.mark_seen(message, folder, 2_000).unwrap();
        let pushed = store.pending_seen(folder).unwrap();
        store.forget_pending_seen(folder, &pushed).unwrap();
        (dir, store, message, folder)
    }

    #[test]
    fn marking_unseen_changes_the_list_and_queues_a_removal() {
        let (_dir, store, message, folder) = read_and_pushed();

        assert_eq!(store.mark_unseen(message, folder, 5_000).unwrap(), 1);
        assert!(
            !ref_flags(&store, message, folder).contains(MessageFlags::SEEN),
            "la liste dit encore « lu »"
        );
        let pending = store.pending_seen(folder).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].uid, 5);
        assert!(!pending[0].seen, "la poussée irait dans le mauvais sens");

        // Et la marque survit au recalcul, comme celle de l'autre sens.
        let writer = store.writer().unwrap();
        writer.refresh_ref_flags(folder).unwrap();
        writer.commit().unwrap();
        assert!(!ref_flags(&store, message, folder).contains(MessageFlags::SEEN));
    }

    #[test]
    fn marking_unseen_a_message_already_unread_writes_nothing() {
        let (_dir, store, message, folder) = fixture();
        assert_eq!(store.mark_unseen(message, folder, 5_000).unwrap(), 0);
        assert!(
            store.pending_seen(folder).unwrap().is_empty(),
            "une poussée a été mise en attente pour un message déjà non lu"
        );
    }

    #[test]
    fn marking_unseen_takes_only_the_seen_flag() {
        // `\Flagged` est porté par la copie : le retirer avec `\Seen` serait détruire une marque
        // que l'utilisateur a posée ailleurs. Le contrôle inverse est le premier `assert` : sans
        // lui, un `& ~SEEN` qui ne toucherait rien passerait aussi.
        let (_dir, store, message, folder) = read_and_pushed();
        let both = MessageFlags::SEEN.union(MessageFlags::FLAGGED).bits();
        store
            .connection()
            .execute("UPDATE remote_uids SET flags = ?1", params![both])
            .unwrap();
        let writer = store.writer().unwrap();
        writer.refresh_ref_flags(folder).unwrap();
        writer.commit().unwrap();
        assert!(ref_flags(&store, message, folder).contains(MessageFlags::FLAGGED));

        store.mark_unseen(message, folder, 5_000).unwrap();

        let flags = ref_flags(&store, message, folder);
        assert!(!flags.contains(MessageFlags::SEEN));
        assert!(
            flags.contains(MessageFlags::FLAGGED),
            "l'étoile a été retirée avec le drapeau lu"
        );
    }

    #[test]
    fn the_last_gesture_replaces_a_push_that_has_not_left() {
        // Lu, puis non lu avant toute moisson. Deux lignes laisseraient l'ordre d'envoi décider
        // de l'état final ; une seule, retournée, dit ce que l'utilisateur a fait en dernier.
        let (_dir, store, message, folder) = fixture();
        store.mark_seen(message, folder, 2_000).unwrap();
        store.mark_unseen(message, folder, 7_000).unwrap();

        let pending = store.pending_seen(folder).unwrap();
        assert_eq!(pending.len(), 1, "deux poussées pour la même copie");
        assert!(!pending[0].seen, "le premier geste a gagné sur le dernier");
        // C'est une autre demande : elle date du geste qui l'a faite.
        assert_eq!(pending[0].marked_at, 7_000);

        // Et le retour à « lu » retourne encore, sans rien empiler.
        store.mark_seen(message, folder, 9_000).unwrap();
        let pending = store.pending_seen(folder).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].seen);
    }

    #[test]
    fn a_push_that_changed_direction_is_not_forgotten() {
        // **La course que le sens dans `forget_pending_seen` ferme.** La moisson lit « lu »,
        // l'envoie ; pendant ce temps l'utilisateur marque non lu ; puis la réponse du serveur
        // arrive et la moisson oublie ce qu'elle a poussé. Oublier par UID seul jetterait le
        // « non lu », et le serveur resterait à « lu » pour toujours.
        let (_dir, store, message, folder) = fixture();
        store.mark_seen(message, folder, 2_000).unwrap();
        let in_flight = store.pending_seen(folder).unwrap();

        store.mark_unseen(message, folder, 3_000).unwrap();
        assert_eq!(store.forget_pending_seen(folder, &in_flight).unwrap(), 0);

        let pending = store.pending_seen(folder).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "le dernier geste a été oublié avec l'ancien"
        );
        assert!(!pending[0].seen);
    }

    #[test]
    fn a_message_without_a_server_copy_is_marked_unread_locally() {
        let (_dir, store, message, folder) = fixture();
        store
            .connection()
            .execute("DELETE FROM remote_uids", [])
            .unwrap();
        store.mark_seen(message, folder, 2_000).unwrap();

        assert_eq!(store.mark_unseen(message, folder, 3_000).unwrap(), 1);
        assert!(!ref_flags(&store, message, folder).contains(MessageFlags::SEEN));
        assert!(
            store.pending_seen(folder).unwrap().is_empty(),
            "une poussée a été promise sur une copie qui n'existe pas"
        );
        assert_eq!(store.mark_unseen(message, folder, 4_000).unwrap(), 0);
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

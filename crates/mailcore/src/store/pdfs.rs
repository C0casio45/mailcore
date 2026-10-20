//! Le texte des PDF joints, rangé — `SCHEMA_V17`, `docs/PHASE-4.md`, entrée du 2026-10-20.
//!
//! ## Qui écrit ici
//!
//! Le parent de l'enfant extracteur, et lui seul : l'enfant n'ouvre pas SQLite. Il rend, pour
//! chaque message, ce qu'il a tiré de chacun de ses PDF ; le parent le range ici, en une
//! transaction par message achevé.
//!
//! ## Trois règles, écrites ici parce qu'ici seulement elles se tiennent
//!
//! - **La remise se décide sur ce que le store contient**, pas sur ce que l'enfant vient de rendre.
//!   Un message qui reçoit un PDF déjà lu dans un autre message — une réponse qui reporte la
//!   facture — n'apporte aucun texte neuf, et doit pourtant être réindexé : son document n'a pas
//!   ce texte. Un message achevé qui porte au moins un PDF en texte est remis, point.
//! - **Un poison ne remplace jamais un texte.** Deux extracteurs — la coquille ouverte deux fois —
//!   peuvent ranger la même pièce ; celui qui a été tué ne doit pas effacer celui qui l'a lue.
//! - **Un identifiant ne suffit pas** : `messages.id` est un `rowid` que SQLite peut redonner. Le
//!   blob relu par l'enfant est redonné avec l'identifiant, et une ligne qui ne lui correspond plus
//!   ne reçoit rien.

use rusqlite::{OptionalExtension, params};

use crate::attachtext::PdfState;
use crate::error::Result;
use crate::model::{BlobHash, MessageId};
use crate::store::Store;

/// Ce qu'un message lu rend d'une de ses pièces PDF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfRecord {
    /// L'enfant l'a lue.
    Read {
        /// Son rang dans le parcours du message — `attachtext::Piece::rank`.
        rank: u32,
        /// BLAKE3 de ses octets décodés.
        hash: [u8; 32],
        state: PdfState,
        /// La révision de la règle d'extraction qui l'a lue.
        revision: u32,
        /// Ses octets décodés.
        size: u64,
        /// Son texte, plafonné par l'enfant ; `None` hors [`PdfState::Text`].
        text: Option<String>,
    },
    /// L'enfant la connaissait déjà — une autre ligne porte cette empreinte — et ne l'a pas relue.
    Known { rank: u32, hash: [u8; 32] },
}

/// Ce que le rangement d'un message a fait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Rangé et marqué lu ; `remarked` dit s'il a été remis à indexer et à vectoriser.
    Done { remarked: bool },
    /// Rangé, mais **pas** marqué lu : l'enfant n'a pas rendu le message en entier — une de ses
    /// pièces l'a tué —, ou une pièce « connue » n'avait plus de ligne à recopier — le message qui
    /// la portait a disparu entre-temps. Le passage suivant le relira, et trouvera connues les
    /// pièces déjà rangées.
    Incomplete,
    /// La ligne ne correspond plus — un autre blob sous cet identifiant, ou un message déjà lu par
    /// un autre extracteur : rien n'a été écrit.
    Gone,
}

/// Un message que l'enfant a lu, en tout ou en partie, prêt à être rangé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub id: MessageId,
    /// Le blob que l'enfant a relu : une ligne qui en porte un autre ne reçoit rien.
    pub blob: BlobHash,
    pub pdfs: Vec<PdfRecord>,
    /// Vrai si l'enfant a rendu le message en entier. Faux pour les pièces lues avant celle qui
    /// l'a tué : elles sont rangées **sans** marquer le message, qui sera relu — sans quoi les
    /// pièces qui suivent la mortelle ne seraient jamais lues.
    pub finished: bool,
}

impl Store {
    /// Range les PDF d'un message que l'enfant a lu en entier — [`Self::record_scans`] pour un
    /// seul message.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn record_scan(
        &self,
        id: MessageId,
        blob: BlobHash,
        pdfs: &[PdfRecord],
    ) -> Result<Recorded> {
        let scan = Scan {
            id,
            blob,
            pdfs: pdfs.to_vec(),
            finished: true,
        };
        Ok(self
            .record_scans(std::slice::from_ref(&scan))?
            .pop()
            .unwrap_or(Recorded::Gone))
    }

    /// Range les PDF de plusieurs messages, marque lus ceux que l'enfant a rendus en entier, et
    /// remet à indexer et à vectoriser ceux qui portent un texte — **en une transaction**, qui rend
    /// un [`Recorded`] par message, dans l'ordre.
    ///
    /// Une transaction pour plusieurs messages, parce que chaque validation change
    /// `PRAGMA data_version`, donc réveille les clients qui attendent le store : une par message
    /// ferait relire sa liste à l'interface à chaque pièce lue, pendant tout un rattrapage.
    ///
    /// Immédiate : deux extracteurs sur un même store se suivent au lieu de se croiser, et le
    /// second trouve le message déjà lu.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn record_scans(&self, scans: &[Scan]) -> Result<Vec<Recorded>> {
        let tx = rusqlite::Transaction::new_unchecked(
            self.connection(),
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let mut recorded = Vec::with_capacity(scans.len());
        for scan in scans {
            recorded.push(record_one(&tx, scan)?);
        }
        tx.commit()?;
        Ok(recorded)
    }
}

/// Un message, dans la transaction de [`Store::record_scans`].
fn record_one(tx: &rusqlite::Transaction<'_>, scan: &Scan) -> Result<Recorded> {
    let id = scan.id;
    let pending = tx
        .query_row(
            "SELECT 1 FROM messages
              WHERE id = ?1 AND blob_hash = ?2 AND attachments_scanned = 0",
            params![id.0, scan.blob.as_bytes().as_slice()],
            |_| Ok(()),
        )
        .optional()?;
    if pending.is_none() {
        return Ok(Recorded::Gone);
    }

    let mut complete = scan.finished;
    for pdf in &scan.pdfs {
        match pdf {
            PdfRecord::Read {
                rank,
                hash,
                state,
                revision,
                size,
                text,
            } => {
                let packed = text
                    .as_deref()
                    .filter(|_| *state == PdfState::Text)
                    .map(crate::attachtext::pack);
                // **Un poison ne remplace jamais le texte de la même pièce** : il n'en monte que la
                // révision. C'est ce qui rend la pièce « connue » à la révision courante, donc ce
                // qui laisse le message finir — sans quoi, après une montée de `mailpdf::REVISION`,
                // une pièce qui tue l'enfant serait annoncée, tuerait, et serait réannoncée à
                // chaque passage, sans fin (relu le 2026-10-20). Une **autre** pièce au même rang —
                // une ligne périmée, d'une analyse qui rangeait autrement — est remplacée.
                tx.prepare_cached(
                    "INSERT INTO message_pdfs
                         (message_id, rank, hash, state, revision, size, text)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (message_id, rank) DO UPDATE SET
                         state = CASE WHEN message_pdfs.state = ?8 AND excluded.state <> ?8
                                        AND message_pdfs.hash = excluded.hash
                                      THEN message_pdfs.state ELSE excluded.state END,
                         text = CASE WHEN message_pdfs.state = ?8 AND excluded.state <> ?8
                                       AND message_pdfs.hash = excluded.hash
                                     THEN message_pdfs.text ELSE excluded.text END,
                         hash = excluded.hash, revision = excluded.revision,
                         size = excluded.size",
                )?
                .execute(params![
                    id.0,
                    rank,
                    hash.as_slice(),
                    state.code(),
                    revision,
                    i64::try_from(*size).unwrap_or(i64::MAX),
                    packed,
                    PdfState::Text.code(),
                ])?;
            }
            PdfRecord::Known { rank, hash } => {
                // La meilleure ligne de même empreinte : un texte d'abord, la révision la plus
                // haute ensuite. Elle peut être la ligne même de ce message, rangée par un passage
                // interrompu — alors rien ne change. Une ligne d'**une autre** empreinte au même
                // rang est périmée et remplacée : gardée, elle laisserait le message incomplet
                // pour toujours.
                let copied = tx
                    .prepare_cached(
                        "INSERT INTO message_pdfs
                             (message_id, rank, hash, state, revision, size, text)
                         SELECT ?1, ?2, hash, state, revision, size, text
                           FROM message_pdfs
                          WHERE hash = ?3
                          ORDER BY (state = ?4) DESC, revision DESC
                          LIMIT 1
                         ON CONFLICT (message_id, rank) DO UPDATE SET
                             hash = excluded.hash, state = excluded.state,
                             revision = excluded.revision, size = excluded.size,
                             text = excluded.text
                         WHERE message_pdfs.hash <> excluded.hash",
                    )?
                    .execute(params![id.0, rank, hash.as_slice(), PdfState::Text.code()])?;
                if copied == 0 {
                    let present = tx
                        .query_row(
                            "SELECT 1 FROM message_pdfs
                              WHERE message_id = ?1 AND rank = ?2 AND hash = ?3",
                            params![id.0, rank, hash.as_slice()],
                            |_| Ok(()),
                        )
                        .optional()?;
                    complete &= present.is_some();
                }
            }
        }
    }
    if !complete {
        return Ok(Recorded::Incomplete);
    }

    tx.prepare_cached(
        "UPDATE messages SET attachments_scanned = 1 WHERE id = ?1 AND attachments_scanned = 0",
    )?
    .execute(params![id.0])?;
    let remarked = tx
        .prepare_cached(
            "UPDATE messages SET attach_rev = attach_rev + 1, indexed = 0, vectorised = 0
              WHERE id = ?1
                AND EXISTS (SELECT 1 FROM message_pdfs
                             WHERE message_id = ?1 AND state = ?2 AND text IS NOT NULL)",
        )?
        .execute(params![id.0, PdfState::Text.code()])?
        > 0;
    Ok(Recorded::Done { remarked })
}

impl Store {
    /// Marque un message que l'enfant n'a pas pu lire : 2 si son analyse l'a tué deux fois, 3 s'il
    /// est trop gros pour être analysé. Rien n'est remis à indexer : il n'y a pas de texte neuf.
    ///
    /// Un correctif de l'analyse relira ces messages par une migration qui les remet à 0 — la
    /// seule façon de dire « la règle a changé » à un store déjà écrit.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn mark_unscannable(
        &self,
        id: MessageId,
        blob: BlobHash,
        state: Unscannable,
    ) -> Result<bool> {
        let changed = self.connection().execute(
            "UPDATE messages SET attachments_scanned = ?3
              WHERE id = ?1 AND blob_hash = ?2 AND attachments_scanned = 0",
            params![id.0, blob.as_bytes().as_slice(), state as i64],
        )?;
        Ok(changed > 0)
    }

    /// Les pièces PDF rangées d'un message, par rang : ce que chacune est devenue.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn message_pdfs(&self, id: MessageId) -> Result<Vec<(u32, PdfState)>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT rank, state FROM message_pdfs WHERE message_id = ?1 ORDER BY rank",
        )?;
        let rows = statement.query_map(params![id.0], |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (rank, code) = row?;
            if let Some(state) = PdfState::from_code(code) {
                out.push((rank, state));
            }
        }
        Ok(out)
    }

    /// Combien de pièces PDF sont dans chaque état, sur tout le store — pour qu'un écran ou
    /// `mail doctor` dise combien sont des scans, combien ont été abandonnées.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn pdf_counts(&self) -> Result<Vec<(PdfState, u64)>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT state, COUNT(*) FROM message_pdfs GROUP BY state ORDER BY state",
        )?;
        let rows =
            statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (code, count) = row?;
            if let Some(state) = PdfState::from_code(code) {
                out.push((state, u64::try_from(count).unwrap_or(0)));
            }
        }
        Ok(out)
    }

    /// Les empreintes que l'enfant n'a pas à relire : celles qu'une ligne porte déjà, à la
    /// révision `revision` ou au-delà — texte ou poison : un poison connu est ce qui laisse finir
    /// un message dont une pièce tue l'enfant.
    ///
    /// **Au plus `limit`, les plus récemment portées d'abord.** La liste part dans une requête de
    /// taille bornée ; sans plafond, un store de quelques centaines de milliers de PDF distincts
    /// l'aurait dépassée, et plus aucun PDF n'aurait été lu (relu le 2026-10-20). Une empreinte
    /// laissée dehors n'est pas une erreur : la pièce est relue, et rangée à l'identique.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Sqlite`].
    pub fn known_pdfs(&self, revision: u32, limit: usize) -> Result<Vec<[u8; 32]>> {
        let mut statement = self.connection().prepare_cached(
            "SELECT hash FROM message_pdfs WHERE revision >= ?1
              GROUP BY hash ORDER BY MAX(message_id) DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![revision, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(hash) = <[u8; 32]>::try_from(row?.as_slice()) {
                out.push(hash);
            }
        }
        Ok(out)
    }
}

/// Le verrou du passage d'extraction — `attachments.lock` à la racine du store.
///
/// Tenu par le **parent** de l'enfant extracteur, du choix des lignes à lire à la dernière
/// écriture de leurs résultats : deux parents sur un même store — la coquille ouverte deux fois,
/// la coquille et un démon — liraient sinon les mêmes messages en même temps, et chacun rangerait
/// pour des morts que l'autre ne voit pas. Rendu par le système à la mort du processus, et jamais
/// effacé, comme `vectors.lock` et `outbox.lock` : une colonne « réservé » resterait vraie après la
/// mort de qui l'a écrite.
#[derive(Debug)]
pub struct ScanLock {
    _file: std::fs::File,
}

impl ScanLock {
    /// Prend le verrou sans attendre ; `None` si un autre passage le tient.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] si le fichier de verrou n'est pas ouvrable.
    pub fn try_acquire(store: &Store) -> Result<Option<Self>> {
        let path = store.root().join("attachments.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.as_std_path())
            .map_err(|source| crate::Error::Io {
                path: path.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(source)) => Err(crate::Error::Io { path, source }),
        }
    }
}

/// Pourquoi un message n'a pas pu être lu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unscannable {
    /// Son analyse a tué l'enfant, deux fois, dans deux enfants.
    Deadly = 2,
    /// Au-delà de ce que l'enfant accepte d'analyser.
    TooBig = 3,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{FolderKind, MessageFlags};
    use crate::store::write::NewMessage;
    use camino::Utf8Path;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let store = Store::open(&root).unwrap();
        (dir, store)
    }

    /// Un message déjà indexé et vectorisé — l'état de tout le store du quotidien au premier
    /// passage de l'extracteur.
    fn message(store: &Store, name: &str) -> (MessageId, BlobHash) {
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();
        let folder = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let blob = BlobHash::of(name.as_bytes());
        let (id, _) = writer
            .insert_message(&NewMessage {
                blob,
                rfc822_id: None,
                date: 1_700_000_000,
                from_addr: "a@b.c",
                from_name: None,
                subject: name,
                size: 100,
                has_attachments: true,
            })
            .unwrap();
        writer
            .insert_ref(id, folder, 1_700_000_000, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();
        store.mark_indexed(&[(id, 0)]).unwrap();
        store.mark_vectorised(&[(id, 0)]).unwrap();
        (id, blob)
    }

    fn text(rank: u32, hash: u8, words: &str) -> PdfRecord {
        PdfRecord::Read {
            rank,
            hash: [hash; 32],
            state: PdfState::Text,
            revision: 1,
            size: 1_000,
            text: Some(words.to_owned()),
        }
    }

    fn without_text(rank: u32, hash: u8, state: PdfState) -> PdfRecord {
        PdfRecord::Read {
            rank,
            hash: [hash; 32],
            state,
            revision: 1,
            size: 10,
            text: None,
        }
    }

    fn flags(store: &Store, id: MessageId) -> (i64, i64, i64, i64) {
        store
            .connection()
            .query_row(
                "SELECT attachments_scanned, attach_rev, indexed, vectorised FROM messages
                  WHERE id = ?1",
                [id.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    fn texts(store: &Store, id: MessageId) -> Vec<String> {
        store
            .attachment_texts(id, crate::attachtext::BUDGET)
            .unwrap()
            .1
    }

    #[test]
    fn a_message_read_with_a_text_is_handed_back_to_both_indexes() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "facture");
        let done = store
            .record_scan(id, blob, &[text(0, 7, "électricité 1234,56")])
            .unwrap();
        assert_eq!(done, Recorded::Done { remarked: true });
        assert_eq!(flags(&store, id), (1, 1, 0, 0));
        let (revision, read) = store
            .attachment_texts(id, crate::attachtext::BUDGET)
            .unwrap();
        assert_eq!(revision, 1);
        assert_eq!(read, ["électricité 1234,56"]);
    }

    /// Le contrôle inverse : un message lu sans texte — un scan — est marqué lu, et **pas**
    /// remis : le réindexer ne changerait rien.
    #[test]
    fn a_message_read_without_a_text_is_not_handed_back() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "scan");
        assert_eq!(
            store
                .record_scan(id, blob, &[without_text(0, 9, PdfState::NoText)])
                .unwrap(),
            Recorded::Done { remarked: false }
        );
        assert_eq!(flags(&store, id), (1, 0, 1, 1));
    }

    /// La perte de mise à jour que le marquage conditionnel ferme : une passe lit le message,
    /// l'extracteur range son texte pendant qu'elle travaille, puis la passe marque.
    #[test]
    fn a_text_stored_during_a_pass_is_not_covered_by_its_mark() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "facture");
        store
            .connection()
            .execute("UPDATE messages SET indexed = 0, vectorised = 0", [])
            .unwrap();
        let row = store.unindexed(10).unwrap().remove(0);
        let vector_row = store.unvectorised(10).unwrap().remove(0);
        assert_eq!((row.attach_rev, vector_row.attach_rev), (0, 0));

        store
            .record_scan(id, blob, &[text(0, 7, "électricité")])
            .unwrap();

        // Les passes marquent avec ce qu'elles ont lu : rien n'est marqué, le message reste à
        // refaire des deux côtés.
        assert_eq!(store.mark_indexed(&[(row.id, row.attach_rev)]).unwrap(), 0);
        assert_eq!(
            store
                .mark_vectorised(&[(vector_row.id, vector_row.attach_rev)])
                .unwrap(),
            0
        );
        assert_eq!(store.unindexed_total().unwrap(), 1);
        assert_eq!(store.unvectorised_total().unwrap(), 1);
        // Le contrôle inverse : la passe suivante, qui a lu la révision nouvelle, marque.
        let row = store.unindexed(10).unwrap().remove(0);
        assert_eq!(store.mark_indexed(&[(row.id, row.attach_rev)]).unwrap(), 1);
    }

    /// Une réponse qui reporte la facture déjà lue dans un autre message : rien de neuf à lire,
    /// et pourtant son document n'a pas ce texte — il doit être remis.
    #[test]
    fn a_message_that_only_links_a_known_pdf_is_handed_back_too() {
        let (_dir, store) = store();
        let (first, first_blob) = message(&store, "facture");
        let (second, second_blob) = message(&store, "Re: facture");
        store
            .record_scan(first, first_blob, &[text(0, 7, "électricité")])
            .unwrap();
        let known = PdfRecord::Known {
            rank: 0,
            hash: [7; 32],
        };
        assert_eq!(
            store.record_scan(second, second_blob, &[known]).unwrap(),
            Recorded::Done { remarked: true }
        );
        assert_eq!(texts(&store, second), ["électricité"]);
    }

    /// Une pièce « connue » dont la seule ligne a disparu entre-temps : le message reste à lire,
    /// et le passage suivant la relira au lieu de la croire lue.
    #[test]
    fn a_known_pdf_without_a_line_to_copy_leaves_the_message_to_read() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "facture");
        let known = PdfRecord::Known {
            rank: 0,
            hash: [3; 32],
        };
        assert_eq!(
            store.record_scan(id, blob, &[known]).unwrap(),
            Recorded::Incomplete
        );
        assert_eq!(flags(&store, id).0, 0);
    }

    /// Un extracteur tué ne fait pas oublier ce qu'un autre a lu.
    #[test]
    fn a_poison_never_replaces_a_text() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "facture");
        // Ce qu'un premier passage, interrompu avant la fin du message, a déjà rangé.
        store
            .connection()
            .execute(
                "INSERT INTO message_pdfs (message_id, rank, hash, state, revision, size, text)
                 VALUES (?1, 0, ?2, 0, 1, 10, ?3)",
                params![
                    id.0,
                    [7u8; 32].as_slice(),
                    crate::attachtext::pack("électricité")
                ],
            )
            .unwrap();
        store
            .record_scan(id, blob, &[without_text(0, 7, PdfState::Crashed)])
            .unwrap();
        assert_eq!(
            texts(&store, id),
            ["électricité"],
            "un poison a effacé un texte"
        );
    }

    /// Un identifiant redonné à un autre message ne reçoit pas les pièces du premier.
    #[test]
    fn a_line_that_no_longer_matches_receives_nothing() {
        let (_dir, store) = store();
        let (id, _) = message(&store, "facture");
        let other = BlobHash::of(b"un autre message");
        assert_eq!(
            store
                .record_scan(id, other, &[text(0, 7, "électricité")])
                .unwrap(),
            Recorded::Gone
        );
        assert!(texts(&store, id).is_empty());
        assert!(
            !store
                .mark_unscannable(id, other, Unscannable::Deadly)
                .unwrap()
        );
        assert_eq!(flags(&store, id).0, 0);
    }

    /// Un message déjà lu par un extracteur ne l'est pas une seconde fois par un autre.
    #[test]
    fn a_message_already_read_is_not_written_twice() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "facture");
        store.record_scan(id, blob, &[text(0, 7, "un")]).unwrap();
        assert_eq!(
            store.record_scan(id, blob, &[text(0, 7, "deux")]).unwrap(),
            Recorded::Gone
        );
        assert_eq!(flags(&store, id).1, 1, "la révision est montée deux fois");
        assert_eq!(texts(&store, id), ["un"]);
    }

    /// Les textes reviennent dans l'ordre des pièces, sans les pièces sans texte, et la lecture
    /// s'arrête au plafond.
    #[test]
    fn texts_come_back_in_piece_order_and_stop_at_the_ceiling() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "dossier");
        store
            .record_scan(
                id,
                blob,
                &[
                    text(2, 8, "troisième"),
                    without_text(1, 9, PdfState::NoText),
                    text(0, 7, "première"),
                ],
            )
            .unwrap();
        assert_eq!(texts(&store, id), ["première", "troisième"]);
        let (_, capped) = store.attachment_texts(id, 3).unwrap();
        assert_eq!(capped, ["première"], "le plafond n'a pas arrêté la lecture");
    }

    #[test]
    fn the_messages_to_read_come_newest_first() {
        let (_dir, store) = store();
        let (old, _) = message(&store, "ancien");
        let (new, _) = message(&store, "nouveau");
        let rows = store.unscanned(10).unwrap();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), [new, old]);
        assert_eq!(store.unscanned_total().unwrap(), 2);
    }

    #[test]
    fn an_unreadable_message_is_marked_by_its_reason() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "trop gros");
        assert!(
            store
                .mark_unscannable(id, blob, Unscannable::TooBig)
                .unwrap()
        );
        assert_eq!(flags(&store, id), (3, 0, 1, 1));
        assert_eq!(store.unscanned_total().unwrap(), 0);
    }

    /// Une pièce tue l'enfant au milieu d'un message : ce qui précède est rangé, le message n'est
    /// **pas** marqué, et le passage suivant — qui connaît la pièce mortelle — l'achève.
    #[test]
    fn a_message_cut_by_a_deadly_piece_keeps_what_came_before_and_is_read_again() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "dossier");
        let cut = Scan {
            id,
            blob,
            pdfs: vec![
                text(0, 7, "première"),
                without_text(1, 8, PdfState::TimedOut),
            ],
            finished: false,
        };
        assert_eq!(store.record_scans(&[cut]).unwrap(), [Recorded::Incomplete]);
        assert_eq!(
            flags(&store, id),
            (0, 0, 1, 1),
            "un message coupé a été marqué"
        );
        assert_eq!(texts(&store, id), ["première"]);
        assert_eq!(store.unscanned_total().unwrap(), 1);

        // Le passage suivant : les deux premières pièces sont connues, la troisième est lue.
        let again = store
            .record_scan(
                id,
                blob,
                &[
                    PdfRecord::Known {
                        rank: 0,
                        hash: [7; 32],
                    },
                    PdfRecord::Known {
                        rank: 1,
                        hash: [8; 32],
                    },
                    text(2, 9, "troisième"),
                ],
            )
            .unwrap();
        assert_eq!(again, Recorded::Done { remarked: true });
        assert_eq!(texts(&store, id), ["première", "troisième"]);
    }

    /// Plusieurs messages en une transaction : chacun a son verdict, et une ligne qui ne
    /// correspond plus n'empêche pas les autres d'être rangées.
    #[test]
    fn a_batch_answers_for_each_message_and_one_gone_line_spoils_nothing() {
        let (_dir, store) = store();
        let (first, first_blob) = message(&store, "un");
        let (second, _) = message(&store, "deux");
        let (third, third_blob) = message(&store, "trois");
        let scans = [
            Scan {
                id: first,
                blob: first_blob,
                pdfs: vec![text(0, 7, "électricité")],
                finished: true,
            },
            Scan {
                id: second,
                blob: BlobHash::of(b"un autre"),
                pdfs: vec![text(0, 8, "perdu")],
                finished: true,
            },
            Scan {
                id: third,
                blob: third_blob,
                pdfs: vec![without_text(0, 9, PdfState::NoText)],
                finished: true,
            },
        ];
        assert_eq!(
            store.record_scans(&scans).unwrap(),
            [
                Recorded::Done { remarked: true },
                Recorded::Gone,
                Recorded::Done { remarked: false }
            ]
        );
        assert_eq!(flags(&store, first).0, 1);
        assert_eq!(flags(&store, second).0, 0);
        assert!(texts(&store, second).is_empty());
        assert_eq!(flags(&store, third).0, 1);
    }

    #[test]
    fn known_pdfs_are_those_of_the_revision_asked() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "facture");
        store.record_scan(id, blob, &[text(0, 7, "un")]).unwrap();
        assert_eq!(store.known_pdfs(1, 10).unwrap(), [[7u8; 32]]);
        assert!(store.known_pdfs(2, 10).unwrap().is_empty());
    }

    /// La liste est bornée, et ce qui reste dedans est ce qui a été porté le plus récemment — le
    /// courrier qui arrive ressemble à celui d'hier, pas à celui d'il y a dix ans.
    #[test]
    fn known_pdfs_stop_at_their_limit_newest_first() {
        let (_dir, store) = store();
        let (old, old_blob) = message(&store, "ancien");
        let (new, new_blob) = message(&store, "nouveau");
        store
            .record_scan(old, old_blob, &[text(0, 1, "un")])
            .unwrap();
        store
            .record_scan(new, new_blob, &[text(0, 2, "deux")])
            .unwrap();
        assert_eq!(store.known_pdfs(1, 1).unwrap(), [[2u8; 32]]);
        assert_eq!(store.known_pdfs(1, 10).unwrap().len(), 2);
    }

    /// Après une montée de révision, une pièce déjà lue en texte qui tue l'enfant à la relecture :
    /// son texte reste, **et** elle devient connue à la nouvelle révision. Sans la seconde moitié,
    /// elle serait réannoncée à chaque passage, sans fin.
    #[test]
    fn a_poison_over_the_same_piece_keeps_its_text_and_makes_it_known() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "facture");
        store
            .record_scan(id, blob, &[text(0, 7, "électricité")])
            .unwrap();
        store
            .connection()
            .execute(
                "UPDATE messages SET attachments_scanned = 0 WHERE id = ?1",
                [id.0],
            )
            .unwrap();
        let poison = PdfRecord::Read {
            rank: 0,
            hash: [7; 32],
            state: PdfState::TimedOut,
            revision: 2,
            size: 10,
            text: None,
        };
        store
            .record_scans(&[Scan {
                id,
                blob,
                pdfs: vec![poison],
                finished: false,
            }])
            .unwrap();
        assert_eq!(texts(&store, id), ["électricité"]);
        assert_eq!(store.known_pdfs(2, 10).unwrap(), [[7u8; 32]]);
    }

    /// Le contrôle inverse : une **autre** pièce au même rang n'est pas protégée — la ligne
    /// périmée est remplacée, sans quoi le message ne finirait jamais.
    #[test]
    fn another_piece_at_the_same_rank_replaces_a_stale_line() {
        let (_dir, store) = store();
        let (id, blob) = message(&store, "facture");
        let (other, other_blob) = message(&store, "autre");
        store
            .record_scan(other, other_blob, &[text(0, 9, "neuf")])
            .unwrap();
        // Une ligne périmée au rang 0 : une pièce d'empreinte 7, d'une analyse d'avant.
        store
            .record_scans(&[Scan {
                id,
                blob,
                pdfs: vec![text(0, 7, "périmé")],
                finished: false,
            }])
            .unwrap();
        // La relecture trouve au rang 0 la pièce d'empreinte 9, connue ailleurs.
        let again = store
            .record_scan(
                id,
                blob,
                &[PdfRecord::Known {
                    rank: 0,
                    hash: [9; 32],
                }],
            )
            .unwrap();
        assert_eq!(again, Recorded::Done { remarked: true });
        assert_eq!(texts(&store, id), ["neuf"]);
    }
}

//! Le répertoire de l'index tel que tantivy l'écrit, avec la patience que Windows demande.
//!
//! ## Ce qui échouait
//!
//! À chaque `commit`, tantivy remplace `meta.json` et `.managed.json` de la même façon : un
//! fichier temporaire écrit à côté, puis renommé par-dessus l'ancien (`atomic_write`). Sous
//! Windows, ce renommage échoue — « Accès refusé », erreur 5 — tant qu'un autre processus tient
//! l'ancien fichier ouvert sans partager sa suppression ; un antivirus qui inspecte le fichier
//! qu'on vient d'écrire le fait typiquement, quelques millisecondes. tantivy 0.26.1 ne réessaie
//! pas : le `commit` échoue, la passe d'index avec lui, et une fusion dont l'enregistrement
//! tombe là est jetée.
//!
//! Trouvé le 2026-10-18 : une fois sur quarante, la suite de `mailcore` tombait sur un `commit`
//! refusé, dans les deux tests qui valident huit et treize fois de suite. La correction des
//! fusions du même jour en multipliait les occasions dans le produit — une fusion réécrit
//! `meta.json` quelques millisecondes après le `commit` qui l'a lancée.
//!
//! ## Ce qui est réessayé, et ce qui ne l'est pas
//!
//! Les deux lectures-écritures de petits fichiers entiers, `atomic_write` et `atomic_read` — la
//! seconde parce qu'un `Searcher` remonté pendant un remplacement relit `meta.json`, et qu'un
//! échec y laisserait la boîte sans recherche jusqu'au rechargement suivant. Seulement pour les
//! deux erreurs d'un fichier tenu par un autre, accès refusé (5) et violation de partage (32),
//! et **seulement sous Windows** : ailleurs, une permission refusée est une vraie permission
//! refusée, et la réessayer ne ferait que retarder l'erreur.
//!
//! Réessayer ne change rien à ce qu'un lecteur voit : chaque essai repart d'un fichier temporaire
//! neuf, et l'ancien `meta.json` reste en place tant qu'un renommage n'a pas réussi. L'attente est
//! bornée — [`WAITS`], 635 ms en tout —, puis l'erreur remonte comme avant.
//!
//! Tout le reste est délégué tel quel à [`MmapDirectory`], **verrou compris** : le verrou par
//! défaut du trait n'est pas celui du répertoire mappé, et deux écrivains ne doivent jamais
//! s'en disputer deux sortes.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tantivy::directory::error::{
    DeleteError, LockError, OpenDirectoryError, OpenReadError, OpenWriteError,
};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, FileSlice, Lock, MmapDirectory, WatchCallback,
    WatchHandle, WritePtr,
};

/// Les attentes entre deux essais : chacune double la précédente.
const WAITS: [Duration; 7] = [
    Duration::from_millis(5),
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(40),
    Duration::from_millis(80),
    Duration::from_millis(160),
    Duration::from_millis(320),
];

/// Le répertoire de l'index : [`MmapDirectory`], qui attend qu'un fichier se libère.
#[derive(Debug, Clone)]
pub(crate) struct Patient {
    inner: MmapDirectory,
}

impl Patient {
    /// Ouvre le répertoire, qui doit exister.
    pub(crate) fn open(path: &Path) -> Result<Self, OpenDirectoryError> {
        Ok(Self {
            inner: MmapDirectory::open(path)?,
        })
    }
}

/// Un fichier tenu par un autre processus, sous Windows : l'échec qui passe en attendant.
fn held_elsewhere(error: &io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32))
}

/// La même chose, pour une lecture : tantivy y enveloppe l'erreur d'entrée-sortie.
fn read_held_elsewhere(error: &OpenReadError) -> bool {
    matches!(error, OpenReadError::IoError { io_error, .. } if held_elsewhere(io_error))
}

/// Essaie, et réessaie après chacune de `waits` tant que l'échec est passager.
fn patiently<T, E>(
    waits: &[Duration],
    mut attempt: impl FnMut() -> Result<T, E>,
    passing: impl Fn(&E) -> bool,
) -> Result<T, E>
where
    E: std::fmt::Display,
{
    for wait in waits {
        match attempt() {
            Err(error) if passing(&error) => {
                tracing::debug!(%error, ?wait, "fichier de l'index tenu ailleurs, nouvel essai");
                std::thread::sleep(*wait);
            }
            other => return other,
        }
    }
    attempt()
}

impl Directory for Patient {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.inner.get_file_handle(path)
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        self.inner.open_read(path)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        self.inner.delete(path)
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.inner.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        self.inner.open_write(path)
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        patiently(&WAITS, || self.inner.atomic_read(path), read_held_elsewhere)
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        patiently(
            &WAITS,
            || self.inner.atomic_write(path, data),
            held_elsewhere,
        )
    }

    fn sync_directory(&self) -> io::Result<()> {
        self.inner.sync_directory()
    }

    fn acquire_lock(&self, lock: &Lock) -> Result<DirectoryLock, LockError> {
        self.inner.acquire_lock(lock)
    }

    fn watch(&self, callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        self.inner.watch(callback)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::cell::Cell;
    use std::io;
    use std::time::Duration;

    use super::patiently;

    const NO_WAIT: [Duration; 3] = [Duration::ZERO; 3];

    #[test]
    fn a_passing_failure_is_tried_again_then_given_up() {
        let tries = Cell::new(0);
        let outcome: io::Result<()> = patiently(
            &NO_WAIT,
            || {
                tries.set(tries.get() + 1);
                Err(io::Error::other("tenu"))
            },
            |_| true,
        );
        assert!(
            outcome.is_err(),
            "l'erreur remonte une fois la patience épuisée"
        );
        assert_eq!(
            tries.get(),
            NO_WAIT.len() + 1,
            "un essai par attente, plus le dernier"
        );
    }

    #[test]
    fn a_failure_that_is_not_passing_is_not_tried_again() {
        // Le contrôle inverse : une erreur définitive ne fait pas attendre.
        let tries = Cell::new(0);
        let outcome: io::Result<()> = patiently(
            &NO_WAIT,
            || {
                tries.set(tries.get() + 1);
                Err(io::Error::from(io::ErrorKind::NotFound))
            },
            |_| false,
        );
        assert!(outcome.is_err());
        assert_eq!(tries.get(), 1);
    }

    #[test]
    fn a_success_after_a_passing_failure_is_kept() {
        let tries = Cell::new(0);
        let outcome = patiently(
            &NO_WAIT,
            || {
                tries.set(tries.get() + 1);
                if tries.get() < 3 {
                    Err(io::Error::other("tenu"))
                } else {
                    Ok(tries.get())
                }
            },
            |_| true,
        );
        assert_eq!(outcome.unwrap(), 3);
    }

    #[test]
    fn only_a_file_held_elsewhere_on_windows_is_passing() {
        let held = io::Error::from_raw_os_error(5);
        let shared = io::Error::from_raw_os_error(32);
        assert_eq!(super::held_elsewhere(&held), cfg!(windows));
        assert_eq!(super::held_elsewhere(&shared), cfg!(windows));
        assert!(!super::held_elsewhere(&io::Error::from(
            io::ErrorKind::NotFound
        )));
        // Et la lecture, où tantivy enveloppe l'erreur : un fichier absent n'est pas passager.
        let path = std::path::PathBuf::from("meta.json");
        let wrapped = tantivy::directory::error::OpenReadError::wrap_io_error(held, path.clone());
        assert_eq!(super::read_held_elsewhere(&wrapped), cfg!(windows));
        let absent = tantivy::directory::error::OpenReadError::FileDoesNotExist(path);
        assert!(!super::read_held_elsewhere(&absent));
    }

    /// Le scénario lui-même, sous Windows : un autre tient `meta.json` ouvert sans partager sa
    /// suppression, et le relâche un peu plus tard.
    #[cfg(windows)]
    #[test]
    fn a_replacement_waits_for_a_file_held_elsewhere() {
        use std::os::windows::fs::OpenOptionsExt;
        use tantivy::directory::Directory;

        /// `FILE_SHARE_READ` seul : ni écriture ni suppression partagées.
        const SHARE_READ: u32 = 0x1;

        let dir = tempfile::tempdir().unwrap();
        let patient = super::Patient::open(dir.path()).unwrap();
        let meta = std::path::Path::new("meta.json");
        patient.atomic_write(meta, b"un").unwrap();

        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(SHARE_READ)
            .open(dir.path().join(meta))
            .unwrap();

        // Le contrôle négatif d'abord : sans patience, le remplacement échoue, et par l'erreur
        // même que la suite de tests rencontrait. Sans lui, ce test pourrait passer pour une
        // autre raison que celle qu'il vérifie.
        let refused = tantivy::directory::MmapDirectory::open(dir.path())
            .unwrap()
            .atomic_write(meta, b"refus")
            .unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(5), "{refused}");

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(held);
        });
        patient.atomic_write(meta, b"deux").unwrap();
        releaser.join().unwrap();
        assert_eq!(patient.atomic_read(meta).unwrap(), b"deux");
    }

    /// La lecture : un autre qui ne partage rien du tout — pas même la lecture — fait échouer
    /// l'ouverture elle-même, par une violation de partage.
    #[cfg(windows)]
    #[test]
    fn a_read_waits_for_a_file_held_elsewhere() {
        use std::os::windows::fs::OpenOptionsExt;
        use tantivy::directory::Directory;

        let dir = tempfile::tempdir().unwrap();
        let patient = super::Patient::open(dir.path()).unwrap();
        let meta = std::path::Path::new("meta.json");
        patient.atomic_write(meta, b"un").unwrap();

        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.path().join(meta))
            .unwrap();
        let refused = tantivy::directory::MmapDirectory::open(dir.path())
            .unwrap()
            .atomic_read(meta)
            .unwrap_err();
        assert!(super::read_held_elsewhere(&refused), "{refused}");

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(held);
        });
        assert_eq!(patient.atomic_read(meta).unwrap(), b"un");
        releaser.join().unwrap();
    }
}

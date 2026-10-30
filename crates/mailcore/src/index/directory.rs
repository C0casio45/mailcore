//! Le répertoire de l'index tel que tantivy l'écrit, avec la patience que Windows demande.
//!
//! ## Ce qui échouait
//!
//! À chaque `commit`, tantivy remplace `meta.json` et `.managed.json` de la même façon : un
//! fichier temporaire écrit à côté, puis renommé par-dessus l'ancien (`atomic_write`). Sous
//! Windows, ce renommage échoue — « Accès refusé », erreur 5 — tant que l'ancien fichier est
//! ouvert **par qui que ce soit**, même par une ouverture ordinaire qui partage tout : mesuré le
//! 2026-10-18, un `std::fs::File::open` suffit. tantivy 0.26.1 ne réessaie pas : le `commit`
//! échoue, la passe d'index avec lui, et une fusion dont l'enregistrement tombe là est jetée.
//!
//! **Le premier porteur est tantivy lui-même**, dans notre processus. Un lecteur ouvert avec la
//! politique par défaut lance un fil qui rouvre `meta.json` toutes les 500 ms tant que l'index
//! vit, et un `Searcher` remonté le relit : la boîte de la coquille et du démon en tient un en
//! permanence, pendant que la passe d'index valide à côté. Un antivirus qui inspecte le fichier
//! qu'on vient d'écrire en est un autre. Lequel a fait tomber la suite n'est pas établi ; le
//! remède vaut pour tous.
//!
//! Trouvé le 2026-10-18 : une exécution de la suite de `mailcore` sur quarante tombait sur un
//! `commit` refusé, dans les deux tests qui valident huit et treize fois de suite — puis vingt
//! sur cent, le soir même, contre zéro avec la patience. La correction des fusions du même jour en
//! multipliait les occasions dans le produit : une fusion réécrit `meta.json` quelques
//! millisecondes après le `commit` qui l'a lancée.
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
//! bornée — [`WAITS`], huit essais et 635 ms d'attente en tout —, puis l'erreur remonte comme
//! avant.
//!
//! Tout le reste est délégué tel quel à [`MmapDirectory`], **verrou compris**, et ce n'est pas un
//! détail : le verrou de [`MmapDirectory`] laisse ses fichiers `.tantivy-writer.lock` et
//! `.tantivy-meta.lock` sur le disque, et le verrou par défaut du trait prend un fichier présent
//! pour un verrou tenu. Sans cette délégation, tout store dont l'index a déjà servi refuserait
//! chaque écrivain et ferait attendre chaque lecteur dix secondes — un test le vérifie.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tantivy::directory::error::{
    DeleteError, LockError, OpenDirectoryError, OpenReadError, OpenWriteError,
};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, FileSlice, Lock, META_LOCK, MmapDirectory, WatchCallback,
    WatchHandle, WritePtr,
};

/// Les attentes entre deux essais : chacune double la précédente, 635 ms en tout.
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

/// Le répertoire de l'index **d'un lecteur** : [`Patient`], qui refuse d'écrire et de créer.
///
/// Ce que tantivy fait d'un index qu'on ne fait que lire : il relit `meta.json` et `.managed.json`,
/// mappe les segments, et prend, le temps de les ouvrir, le verrou `.tantivy-meta.lock` — qui
/// empêche l'écrivain d'un autre processus d'en effacer un sous ses yeux. Tout ça se fait par
/// lecture, **sauf le verrou** : [`MmapDirectory`] ouvre son fichier **en écriture**, et le crée
/// s'il manque. Sur un store protégé en écriture — un attribut lecture seule, un montage `:ro` —
/// cette ouverture échouait, et la boîte répondait sans recherche, « aucun résultat » à tout,
/// devant un index intact.
///
/// D'où les trois règles :
///
/// - **écrire, effacer, remplacer est refusé**, par une erreur d'accès refusé, et le verrou de
///   l'écrivain aussi : un `IndexWriter` construit par erreur sur cet index échoue à sa
///   construction, au lieu d'écrire — ou de tenir ce verrou contre l'écrivain de l'application ;
/// - **le verrou des lecteurs est pris sur une ouverture en lecture**, par le même appel système
///   que tantivy — `LockFileEx` sur toute l'étendue sous Windows, `flock` sous Linux et macOS, qui
///   acceptent tous deux une poignée en lecture. C'est donc toujours le verrou de l'écrivain : un test
///   vérifie que l'un tient l'autre à la porte, dans les deux sens ;
/// - **ce verrou n'est pris que si son fichier existe déjà.** Il existe dès qu'un écrivain a validé
///   ou qu'une boîte a ouvert l'index une fois, et tantivy ne l'efface jamais. S'il manque, le
///   lecteur renonce à l'index — la boîte répond sans recherche — plutôt que de le créer, ou que de
///   lire sans le verrou des segments qu'un écrivain pourrait effacer.
#[derive(Debug, Clone)]
pub(crate) struct Readonly {
    inner: Patient,
    /// La racine de l'index, où le verrou des lecteurs est ouvert.
    root: PathBuf,
}

impl Readonly {
    /// Ouvre le répertoire, qui doit exister : il n'est pas créé.
    pub(crate) fn open(path: &Path) -> Result<Self, OpenDirectoryError> {
        Ok(Self {
            inner: Patient::open(path)?,
            root: path.to_owned(),
        })
    }
}

/// Le refus d'écrire d'un [`Readonly`], en erreur d'entrée-sortie.
fn refused(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "index ouvert en lecture : {} n'est pas écrit",
            path.display()
        ),
    )
}

impl Directory for Readonly {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.inner.get_file_handle(path)
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        self.inner.open_read(path)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(refused(path)),
            filepath: path.to_owned(),
        })
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.inner.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::wrap_io_error(
            refused(path),
            path.to_owned(),
        ))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.inner.atomic_read(path)
    }

    fn atomic_write(&self, path: &Path, _data: &[u8]) -> io::Result<()> {
        Err(refused(path))
    }

    fn sync_directory(&self) -> io::Result<()> {
        // Rien n'a été écrit, donc rien à rendre durable.
        Ok(())
    }

    fn acquire_lock(&self, lock: &Lock) -> Result<DirectoryLock, LockError> {
        // **Le verrou des lecteurs, et lui seul.** Celui de l'écrivain, pris par un `IndexWriter`
        // construit par erreur, tiendrait l'écrivain de l'application à la porte pendant toute la
        // vie de ce processus.
        if lock.filepath != META_LOCK.filepath {
            return Err(LockError::wrap_io_error(refused(&lock.filepath)));
        }
        // **En lecture**, là où `MmapDirectory::acquire_lock` ouvre en écriture même un fichier
        // présent : `File::open` ne crée rien, et passe sur un fichier qu'on ne peut pas écrire.
        let file = match std::fs::File::open(self.root.join(&lock.filepath)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(LockError::wrap_io_error(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "{} absent : un lecteur ne le crée pas",
                        lock.filepath.display()
                    ),
                )));
            }
            Err(error) => return Err(LockError::wrap_io_error(error)),
        };
        // Le verrou exclusif de tantivy (`fs4`), par les mêmes appels : `LockFileEx` sur toute
        // l'étendue, `0..!0`, sous Windows — sur une autre, il ne heurterait pas celui de
        // l'écrivain — et `flock` sous Linux et macOS.
        if lock.is_blocking {
            file.lock().map_err(LockError::wrap_io_error)?;
        } else if file.try_lock().is_err() {
            // Ce que fait tantivy : tout échec d'un essai sans attente est un verrou occupé.
            return Err(LockError::LockBusy);
        }
        // Lâcher la poignée relâche le verrou, comme chez tantivy.
        Ok(DirectoryLock::from(Box::new(file)))
    }

    fn watch(&self, callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        self.inner.watch(callback)
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
        // Le contrôle inverse : les autres erreurs du système, voisines comprises, ne font pas
        // attendre — fichier ou chemin absent, fichier déjà là, disque plein, fichier mappé.
        for code in [2, 3, 80, 112, 183, 1224] {
            assert!(
                !super::held_elsewhere(&io::Error::from_raw_os_error(code)),
                "{code}"
            );
        }
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

    /// Ouvre `path` comme tantivy l'ouvre pour le relire — une ouverture ordinaire, qui partage
    /// tout —, ou sans rien partager. L'ouverture peut elle-même tomber sur un porteur passager :
    /// elle a la même patience que ce qu'elle met à l'épreuve.
    #[cfg(windows)]
    fn hold(path: &std::path::Path, exclusive: bool) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;

        super::patiently(
            &super::WAITS,
            || {
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                if exclusive {
                    options.share_mode(0);
                }
                options.open(path)
            },
            super::held_elsewhere,
        )
        .unwrap()
    }

    /// Le refus et sa reprise, **sans course** : le fichier est tenu pendant tout le premier
    /// essai, et relâché seulement quand cet essai a été refusé. Si une ouverture ordinaire ne
    /// faisait pas refuser le remplacement, le premier essai passerait et le compte le dirait.
    #[cfg(windows)]
    #[test]
    fn a_refused_replacement_is_tried_again_once_the_file_is_released() {
        use tantivy::directory::Directory;

        let dir = tempfile::tempdir().unwrap();
        let inner = tantivy::directory::MmapDirectory::open(dir.path()).unwrap();
        let meta = std::path::Path::new("meta.json");
        super::patiently(
            &super::WAITS,
            || inner.atomic_write(meta, b"un"),
            super::held_elsewhere,
        )
        .unwrap();

        let held = std::cell::RefCell::new(Some(hold(&dir.path().join(meta), false)));
        let tries = Cell::new(0);
        super::patiently(
            &super::WAITS,
            || {
                tries.set(tries.get() + 1);
                let outcome = inner.atomic_write(meta, b"deux");
                if let Err(error) = &outcome {
                    if tries.get() == 1 {
                        // L'erreur même que la suite rencontrait, et pas une autre.
                        assert_eq!(error.raw_os_error(), Some(5), "{error}");
                    }
                    held.borrow_mut().take();
                }
                outcome
            },
            super::held_elsewhere,
        )
        .unwrap();
        assert!(
            tries.get() >= 2,
            "le premier essai devait être refusé : {} essai",
            tries.get()
        );
        assert_eq!(std::fs::read(dir.path().join(meta)).unwrap(), b"deux");
    }

    /// La lecture, de même : un porteur qui ne partage rien fait échouer l'ouverture elle-même,
    /// par une violation de partage, et la lecture reprend quand il relâche.
    #[cfg(windows)]
    #[test]
    fn a_refused_read_is_tried_again_once_the_file_is_released() {
        use tantivy::directory::Directory;

        let dir = tempfile::tempdir().unwrap();
        let inner = tantivy::directory::MmapDirectory::open(dir.path()).unwrap();
        let meta = std::path::Path::new("meta.json");
        super::patiently(
            &super::WAITS,
            || inner.atomic_write(meta, b"un"),
            super::held_elsewhere,
        )
        .unwrap();

        let held = std::cell::RefCell::new(Some(hold(&dir.path().join(meta), true)));
        let tries = Cell::new(0);
        let read = super::patiently(
            &super::WAITS,
            || {
                tries.set(tries.get() + 1);
                let outcome = inner.atomic_read(meta);
                if let Err(error) = &outcome {
                    if tries.get() == 1 {
                        assert!(super::read_held_elsewhere(error), "{error}");
                    }
                    held.borrow_mut().take();
                }
                outcome
            },
            super::read_held_elsewhere,
        )
        .unwrap();
        assert!(
            tries.get() >= 2,
            "le premier essai devait être refusé : {} essai",
            tries.get()
        );
        assert_eq!(read, b"un");
    }

    /// Le câblage : c'est bien `Patient` qui attend, en écriture. Un autre fil relâche le fichier
    /// au bout de 300 ms ; le test ne pourrait passer à tort que si le premier essai durait plus
    /// longtemps que ça — la reprise elle-même est établie sans course ci-dessus.
    #[cfg(windows)]
    #[test]
    fn the_index_directory_waits_to_replace_a_file_held_elsewhere() {
        use tantivy::directory::Directory;

        let dir = tempfile::tempdir().unwrap();
        let patient = super::Patient::open(dir.path()).unwrap();
        let meta = std::path::Path::new("meta.json");
        patient.atomic_write(meta, b"un").unwrap();

        let held = hold(&dir.path().join(meta), false);
        // Le contrôle négatif : le répertoire de tantivy, sans patience, refuse.
        let refused = tantivy::directory::MmapDirectory::open(dir.path())
            .unwrap()
            .atomic_write(meta, b"refus")
            .unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(5), "{refused}");

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        patient.atomic_write(meta, b"deux").unwrap();
        releaser.join().unwrap();
        assert_eq!(patient.atomic_read(meta).unwrap(), b"deux");
    }

    /// Le verrou des lecteurs que prend [`super::Readonly`], sur une ouverture en lecture, est
    /// **celui de tantivy** : tenu par l'un, l'autre est à la porte, dans les deux sens. Un verrou
    /// réimplémenté qui ne heurterait pas celui de l'écrivain laisserait celui-ci effacer un
    /// segment pendant qu'un lecteur l'ouvre.
    #[test]
    fn the_lock_of_a_reader_is_the_lock_of_tantivy() {
        use tantivy::directory::error::LockError;
        use tantivy::directory::{Directory, Lock, META_LOCK, MmapDirectory};

        let dir = tempfile::tempdir().unwrap();
        let tantivy = MmapDirectory::open(dir.path()).unwrap();
        let reader = super::Readonly::open(dir.path()).unwrap();
        // Sans attente, pour qu'un verrou tenu se lise en un refus au lieu de bloquer le test.
        let at_once = Lock {
            filepath: META_LOCK.filepath.clone(),
            is_blocking: false,
        };
        let busy = |outcome: Result<_, LockError>| matches!(outcome, Err(LockError::LockBusy));

        // tantivy le tient — et crée le fichier : le lecteur est refusé.
        let held = tantivy.acquire_lock(&META_LOCK).unwrap();
        assert!(busy(reader.acquire_lock(&at_once)), "le lecteur est passé");
        drop(held);

        // Le lecteur le tient : tantivy est refusé.
        let held = reader.acquire_lock(&META_LOCK).unwrap();
        assert!(busy(tantivy.acquire_lock(&at_once)), "tantivy est passé");
        drop(held);
        // Et relâché, il est libre.
        drop(tantivy.acquire_lock(&at_once).unwrap());
    }

    /// Le cas trouvé en relecture : un verrou qu'on ne peut pas écrire — store protégé en écriture.
    /// tantivy ne le prend pas, parce qu'il ouvre en écriture ; le lecteur, si.
    #[test]
    fn a_reader_takes_a_lock_file_it_cannot_write() {
        use tantivy::directory::{Directory, META_LOCK, MmapDirectory};

        let dir = tempfile::tempdir().unwrap();
        let tantivy = MmapDirectory::open(dir.path()).unwrap();
        drop(tantivy.acquire_lock(&META_LOCK).unwrap());
        let path = dir.path().join(&META_LOCK.filepath);
        let set_readonly = |readonly: bool| {
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(readonly);
            std::fs::set_permissions(&path, permissions).unwrap();
        };
        set_readonly(true);
        let writable = std::fs::OpenOptions::new().write(true).open(&path).is_ok();
        let by_tantivy = tantivy.acquire_lock(&META_LOCK).map(drop);
        let by_reader = super::Readonly::open(dir.path())
            .unwrap()
            .acquire_lock(&META_LOCK)
            .map(drop);
        // Rendu inscriptible avant toute assertion : le répertoire temporaire doit pouvoir s'effacer.
        set_readonly(false);

        assert!(
            !writable,
            "précondition : le verrou est encore inscriptible — compte privilégié ?"
        );
        assert!(
            by_tantivy.is_err(),
            "précondition : tantivy a pris un verrou en lecture seule"
        );
        by_reader.unwrap();
    }

    /// Le câblage, en lecture.
    #[cfg(windows)]
    #[test]
    fn the_index_directory_waits_to_read_a_file_held_elsewhere() {
        use tantivy::directory::Directory;

        let dir = tempfile::tempdir().unwrap();
        let patient = super::Patient::open(dir.path()).unwrap();
        let meta = std::path::Path::new("meta.json");
        patient.atomic_write(meta, b"un").unwrap();

        let held = hold(&dir.path().join(meta), true);
        let refused = tantivy::directory::MmapDirectory::open(dir.path())
            .unwrap()
            .atomic_read(meta)
            .unwrap_err();
        assert!(super::read_held_elsewhere(&refused), "{refused}");

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        assert_eq!(patient.atomic_read(meta).unwrap(), b"un");
        releaser.join().unwrap();
    }
}

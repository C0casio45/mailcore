//! Le store : blobs adressés par contenu + index de métadonnées SQLite.
//!
//! ```text
//! <root>/
//! ├── blobs/<aa>/<bb>/<hash>   message RFC 5322 brut, compressé zstd
//! ├── index.sqlite             métadonnées, références, fils, comptes
//! ├── search/                  index tantivy
//! ├── outbox.lock              verrou de remise — voir `outbox::DeliveryLock`
//! ├── outbox.sending           la ligne que le détenteur du verrou remet — voir
//! │                            `Store::in_flight`
//! └── tmp/                     temporaires d'écriture, même volume que blobs/
//! ```
//!
//! ## La frontière
//!
//! Rien hors de ce module ne sait qu'il y a du SQLite dessous. [`Store::connection`] est
//! `pub(crate)`, pas `pub` : `query.rs` rend des types de [`crate::model`], jamais un
//! `rusqlite::Row`. C'est ce qui rend le choix du moteur révisable — si une mesure condamne
//! SQLite, on remplace ce module et pas le projet.

pub mod blobs;
pub mod db;
pub mod drafts;
pub mod flags;
pub mod migrations;
pub mod outbox;
pub mod pdfs;
pub mod read;
pub mod threads;
pub mod write;

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, Result};
use blobs::{BlobStore, FsBlobStore};

/// Le store ouvert : blobs et index de métadonnées.
#[derive(Debug)]
pub struct Store {
    root: Utf8PathBuf,
    blobs: FsBlobStore,
    conn: rusqlite::Connection,
    /// Vrai pour un store ouvert par [`Store::open_reader`] : voir [`Store::is_reader`].
    reader: bool,
}

impl Store {
    /// Ouvre — et crée si besoin — un store sous `root`.
    ///
    /// Migre le schéma au passage. Appelable sur un store neuf comme sur un store existant.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] si l'arborescence ne peut pas être créée, [`Error::Sqlite`] ou
    /// [`Error::UnsupportedSchema`] si l'index n'est pas exploitable.
    pub fn open(root: impl AsRef<Utf8Path>) -> Result<Self> {
        let root = root.as_ref().to_owned();
        std::fs::create_dir_all(&root).map_err(|e| Error::Io {
            path: root.clone(),
            source: e,
        })?;

        let blobs = FsBlobStore::open(&root)?;
        let conn = db::open(&root.join("index.sqlite"))?;

        tracing::debug!(root = %root, "store ouvert");
        Ok(Self {
            root,
            blobs,
            conn,
            reader: false,
        })
    }

    /// Ouvre un store **existant** en lecture : rien n'est créé, migré ni écrit.
    ///
    /// ## Pourquoi un second chemin d'ouverture
    ///
    /// [`Self::open`] est celui d'un écrivain : il crée l'arborescence et migre le schéma, ce qui
    /// est irréversible. Le serveur MCP qu'un client lance en stdio passait par là, et par tout ce
    /// que le démon monte autour : le 2026-10-30, il a migré le corpus de mesure et lancé sur lui
    /// la moisson et les passes de fond. Pointé sur le store du quotidien, il aurait été un second
    /// écrivain complet à côté de la coquille. La règle « un lecteur n'écrit pas » est donc tenue
    /// **ici, au point d'ouverture**, et pas par la vigilance de chaque appelant :
    ///
    /// - pas de `create_dir_all` — ni de la racine, ni de `blobs/`, ni de `tmp/` ;
    /// - SQLite ouvert en lecture, sans création, sans migration — voir [`db::open_reader`] ;
    /// - les blobs et les fichiers de verrou refusent d'écrire ([`crate::Error::ReadOnly`]).
    ///
    /// Ce qui peut bouger sur le disque est ce que SQLite écrit pour coordonner un lecteur WAL —
    /// `index.sqlite-shm`, et `-wal` vide s'il manquait. [`db::open_reader`] le détaille.
    ///
    /// # Errors
    ///
    /// [`crate::Error::NoStore`] si `index.sqlite` est absent, [`crate::Error::SchemaBehind`] si
    /// le store attend une migration, et ce que rend [`db::open_reader`].
    pub fn open_reader(root: impl AsRef<Utf8Path>) -> Result<Self> {
        let root = root.as_ref().to_owned();
        let index = root.join("index.sqlite");
        match std::fs::metadata(&index) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Err(Error::NoStore { root }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NoStore { root });
            }
            Err(source) => {
                return Err(Error::Io {
                    path: index,
                    source,
                });
            }
        }

        let blobs = FsBlobStore::open_reader(&root);
        let conn = db::open_reader(&index)?;

        tracing::debug!(root = %root, "store ouvert en lecture");
        Ok(Self {
            root,
            blobs,
            conn,
            reader: true,
        })
    }

    /// Vrai si le store a été ouvert par [`Self::open_reader`].
    ///
    /// Ce qui écrit à côté de SQLite le demande avant de toucher au disque — voir
    /// [`Self::refuse_if_reader`] —, et `Mailbox` s'en sert pour ouvrir l'index plein texte sans
    /// jamais le créer.
    #[must_use]
    pub const fn is_reader(&self) -> bool {
        self.reader
    }

    /// [`crate::Error::ReadOnly`] pour `path` si le store a été ouvert en lecture.
    ///
    /// À appeler **avant** de créer un fichier sous la racine. SQLite n'en a pas besoin : sa
    /// connexion de lecteur refuse elle-même.
    pub(crate) fn refuse_if_reader(&self, path: &Utf8Path) -> Result<()> {
        if self.reader {
            return Err(Error::ReadOnly {
                path: path.to_owned(),
            });
        }
        Ok(())
    }

    /// La racine du store.
    #[must_use]
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// Le répertoire de l'index plein texte tantivy.
    #[must_use]
    pub fn search_dir(&self) -> Utf8PathBuf {
        self.root.join("search")
    }

    /// Le fichier du magasin de vecteurs.
    ///
    /// Un fichier et non un répertoire : le format est plat et en ajout seul, voir
    /// [`crate::vectors`]. Il vit à côté de l'index tantivy parce qu'il a la même nature — un
    /// dérivé reconstructible depuis les blobs, dont la perte coûte une passe et jamais une
    /// donnée.
    #[must_use]
    pub fn vectors_path(&self) -> Utf8PathBuf {
        Self::vectors_path_in(&self.root)
    }

    /// [`Self::vectors_path`] sans ouvrir le store : pour qui veut un `stat` et rien de plus.
    #[must_use]
    pub fn vectors_path_in(root: &Utf8Path) -> Utf8PathBuf {
        root.join("vectors.bin")
    }

    /// L'accès aux blobs, derrière le trait — les appelants n'ont pas à savoir que c'est le
    /// système de fichiers.
    #[must_use]
    pub fn blobs(&self) -> &dyn BlobStore {
        &self.blobs
    }

    /// La connexion à l'index.
    ///
    /// `pub(crate)` et pas `pub` : c'est la frontière décrite en tête de module. Aucun crate
    /// extérieur ne doit pouvoir écrire du SQL contre ce store.
    pub(crate) fn connection(&self) -> &rusqlite::Connection {
        &self.conn
    }
}

/// Le répertoire de données par défaut de mailcore.
///
/// # Errors
///
/// [`Error::Io`] si la plateforme ne sait pas dire où vivent les données utilisateur, ou si
/// le chemin n'est pas de l'UTF-8.
pub fn default_root() -> Result<Utf8PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "mailcore").ok_or_else(|| Error::Io {
        path: Utf8PathBuf::from("<répertoire de données>"),
        source: std::io::Error::other("répertoire de données introuvable sur cette plateforme"),
    })?;

    Utf8PathBuf::from_path_buf(dirs.data_dir().to_path_buf()).map_err(|p| Error::Io {
        path: Utf8PathBuf::from(p.to_string_lossy().into_owned()),
        source: std::io::Error::other("chemin de données non UTF-8"),
    })
}

/// Un relevé de fichiers : chemin relatif, et contenu — `None` pour un répertoire.
#[cfg(test)]
pub(crate) type Tree = std::collections::BTreeMap<String, Option<Vec<u8>>>;

/// Tout ce qu'il y a sous `root`, octet pour octet : un répertoire vaut `None`, un fichier son
/// contenu. Pour les tests qui vérifient qu'un lecteur n'a rien écrit — sur des stores de quelques
/// fichiers.
///
/// Les deux fichiers que SQLite écrit pour coordonner un lecteur WAL, `index.sqlite-wal` et
/// `index.sqlite-shm`, sont rendus à part : chaque test dit ce qu'il en attend, au lieu de les
/// ignorer en silence.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn snapshot(root: &Utf8Path) -> (Tree, [Option<Vec<u8>>; 2]) {
    fn walk(root: &Utf8Path, dir: &Utf8Path, out: &mut Tree) {
        for entry in dir.read_dir_utf8().unwrap() {
            let path = entry.unwrap().into_path();
            let name = path.strip_prefix(root).unwrap().as_str().replace('\\', "/");
            if path.is_dir() {
                out.insert(name, None);
                walk(root, &path, out);
            } else {
                out.insert(name, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut tree = std::collections::BTreeMap::new();
    if root.exists() {
        walk(root, root, &mut tree);
    }
    let wal = tree.remove("index.sqlite-wal").flatten();
    let shm = tree.remove("index.sqlite-shm").flatten();
    (tree, [wal, shm])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let store = Store::open(&root).unwrap();
        (dir, store)
    }

    #[test]
    fn lays_out_the_expected_tree() {
        let (_dir, s) = temp_store();
        assert!(s.root().join("blobs").is_dir());
        assert!(s.root().join("tmp").is_dir());
        assert!(s.root().join("index.sqlite").is_file());
    }

    #[test]
    fn creates_a_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path())
            .unwrap()
            .join("pas/encore/la");
        assert!(Store::open(&root).is_ok());
    }

    #[test]
    fn reopening_is_not_destructive() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();

        let hash = {
            let s = Store::open(&root).unwrap();
            s.blobs().put(b"un message").unwrap().hash
        };

        let reopened = Store::open(&root).unwrap();
        assert!(reopened.blobs().contains(hash).unwrap());
        assert_eq!(
            migrations::user_version(reopened.connection()).unwrap(),
            migrations::SCHEMA_VERSION
        );
    }

    #[test]
    fn two_stores_do_not_share_state() {
        let (_a, first) = temp_store();
        let (_b, second) = temp_store();

        let hash = first.blobs().put(b"un message").unwrap().hash;
        assert!(!second.blobs().contains(hash).unwrap());
    }

    // --- `open_reader` ---------------------------------------------------------------------

    #[test]
    fn a_reader_pointed_at_no_store_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let parent = Utf8Path::from_path(dir.path()).unwrap().to_owned();

        // Une racine absente : refusée, et toujours absente.
        let absent = parent.join("pas/encore/la");
        assert!(matches!(
            Store::open_reader(&absent),
            Err(Error::NoStore { root }) if root == absent
        ));
        assert!(!parent.join("pas").exists(), "la racine a été créée");

        // Une racine vide : refusée, et toujours vide — ni `blobs/`, ni `tmp/`, ni base.
        let empty = parent.join("vide");
        std::fs::create_dir(&empty).unwrap();
        let before = snapshot(&parent);
        assert!(matches!(
            Store::open_reader(&empty),
            Err(Error::NoStore { .. })
        ));
        assert_eq!(snapshot(&parent), before, "le lecteur a créé quelque chose");
    }

    #[test]
    fn a_reader_store_writes_no_file_beside_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let hash = {
            let store = Store::open(&root).unwrap();
            store.blobs().put(b"un message").unwrap().hash
        };
        let (files, journal) = snapshot(&root);
        assert_eq!(journal, [None, None], "précondition : un store fermé");

        let reader = Store::open_reader(&root).unwrap();
        assert!(reader.is_reader());
        // La lecture marche…
        assert_eq!(reader.blobs().read(hash).unwrap(), b"un message");
        // … et chaque écriture est refusée avant de toucher au disque.
        let refused = |outcome: Result<()>| matches!(outcome, Err(Error::ReadOnly { .. }));
        assert!(refused(reader.blobs().put(b"un autre").map(drop)));
        assert!(refused(
            reader.blobs().put_reader(&mut &b"un flux"[..]).map(drop)
        ));
        assert!(refused(reader.blobs().put_writer().map(drop)));
        assert!(refused(reader.blobs().delete(hash).map(drop)));
        assert!(refused(reader.lock_delivery().map(drop)));
        assert!(refused(pdfs::ScanLock::try_acquire(&reader).map(drop)));
        drop(reader);

        let (after, [wal, _shm]) = snapshot(&root);
        assert_eq!(after, files, "un fichier du store a changé");
        // Ce que SQLite a créé pour lire, et rien de plus : un journal vide — voir
        // `db::open_reader`.
        assert_eq!(wal.as_deref(), Some(&[][..]));
    }

    /// Une racine qui n'a que sa base — copiée, ou archivée par un outil qui laisse les répertoires
    /// vides en route : un lecteur l'ouvre, et n'y crée ni `blobs/` ni `tmp/`. Les autres tests
    /// ouvrent des stores que `Store::open` a déjà garnis, où un `create_dir_all` ne se verrait pas.
    #[test]
    fn a_reader_creates_no_blob_store_beside_a_lone_base() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap().to_owned();
        drop(db::open(&root.join("index.sqlite")).unwrap());
        let (files, journal) = snapshot(&root);
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["index.sqlite"],
            "précondition : la base seule"
        );
        assert_eq!(journal, [None, None], "précondition : une base fermée");

        let reader = Store::open_reader(&root).unwrap();
        // Un magasin absent se lit comme un magasin vide.
        let hash = crate::model::BlobHash::of(b"un message");
        assert!(matches!(
            reader.blobs().read(hash),
            Err(Error::BlobNotFound(missing)) if missing == hash
        ));
        assert!(!reader.blobs().contains(hash).unwrap());
        drop(reader);

        let (after, _) = snapshot(&root);
        assert_eq!(after, files, "le lecteur a créé quelque chose");
    }

    #[test]
    fn a_writer_store_is_not_a_reader() {
        // Le contrôle inverse du précédent : la barrière ne vise que le lecteur.
        let (_dir, store) = temp_store();
        assert!(!store.is_reader());
        assert!(store.blobs().put(b"un message").is_ok());
        assert!(store.lock_delivery().is_ok());
    }
}

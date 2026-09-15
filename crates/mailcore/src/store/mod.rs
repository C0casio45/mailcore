//! Le store : blobs adressés par contenu + index de métadonnées SQLite.
//!
//! ```text
//! <root>/
//! ├── blobs/<aa>/<bb>/<hash>   message RFC 5322 brut, compressé zstd
//! ├── index.sqlite             métadonnées, références, fils, comptes
//! ├── search/                  index tantivy
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
        Ok(Self { root, blobs, conn })
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
}

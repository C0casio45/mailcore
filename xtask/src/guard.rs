//! Les gardes des bancs : ce qu'un banc refuse avant d'ouvrir un store ou de mesurer.
//!
//! ## Ouvrir n'est pas lire
//!
//! `Store::open` applique les migrations, et `mailcore` n'a pas d'ouverture en lecture seule :
//! ouvrir le corpus `measurements/store` — au schéma 15 le 2026-10-20 — avec un binaire au schéma
//! 17 l'écrit **en place**, et aucun binaire plus ancien ne le rouvre ensuite. Les bancs qui
//! travaillent sur une réplique sous `target/` n'ont rien à craindre ; les autres passent par
//! [`open_store`] et [`open_mailbox`], qui refusent un store qu'ils migreraient hors de `target/`.
//!
//! ## Un relevé sur un état à moitié fait ment sans le dire
//!
//! Depuis le texte des PDF (`SCHEMA_V17`), lire un PDF remet son message à indexer et à vectoriser,
//! mais l'ancien document et l'ancien vecteur restent en place jusqu'à la passe suivante : un banc
//! qui mesure entre les deux mesure un mélange, et rien ne le signale. [`settled`] le refuse.

use anyhow::{Context, Result};
use camino::Utf8Path;
use mailcore::{Mailbox, Store};

/// La version de schéma d'une base, lue dans son en-tête — sans ouvrir SQLite, qui créerait un
/// journal à côté. `None` s'il n'y a pas de base.
pub(crate) fn schema_of(index: &Utf8Path) -> Result<Option<u32>> {
    use std::io::Read;
    if !index.is_file() {
        return Ok(None);
    }
    let mut header = [0u8; 64];
    std::fs::File::open(index.as_std_path())
        .and_then(|mut file| file.read_exact(&mut header))
        .with_context(|| format!("lecture de l'en-tête de {index}"))?;
    Ok(Some(u32::from_be_bytes([
        header[60], header[61], header[62], header[63],
    ])))
}

/// Refuse d'ouvrir `root` si l'ouverture le migrerait **hors de `target/`**.
///
/// # Errors
///
/// Si le store est à un schéma plus ancien que celui du binaire et qu'il n'est pas sous le
/// répertoire de travail de Cargo.
pub(crate) fn unmigrated(root: &Utf8Path) -> Result<()> {
    let Some(found) = schema_of(&root.join("index.sqlite"))? else {
        return Ok(());
    };
    let current = mailcore::store::migrations::SCHEMA_VERSION;
    if found >= current || under_target(root) {
        return Ok(());
    }
    anyhow::bail!(
        "ouvrir {root} le migrerait en place, du schéma {found} au schéma {current} — et aucun \
         binaire plus ancien ne le rouvrirait. Travaillez sur une réplique sous target/, ou \
         migrez-le exprès avec `mail doctor --store {root}`"
    )
}

fn under_target(root: &Utf8Path) -> bool {
    let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|workspace| workspace.join("target"));
    match (
        target.and_then(|t| std::fs::canonicalize(t).ok()),
        std::fs::canonicalize(root.as_std_path()).ok(),
    ) {
        (Some(target), Some(root)) => root.starts_with(target),
        _ => false,
    }
}

/// [`Store::open`], après [`unmigrated`].
///
/// # Errors
///
/// Comme [`unmigrated`], ou si le store ne s'ouvre pas.
pub(crate) fn open_store(root: impl AsRef<Utf8Path>) -> Result<Store> {
    let root = root.as_ref();
    unmigrated(root)?;
    Ok(Store::open(root)?)
}

/// [`Mailbox::open`], après [`unmigrated`].
///
/// # Errors
///
/// Comme [`unmigrated`], ou si la boîte ne s'ouvre pas.
pub(crate) fn open_mailbox(root: impl AsRef<Utf8Path>) -> Result<Mailbox> {
    let root = root.as_ref();
    unmigrated(root)?;
    Ok(Mailbox::open(root)?)
}

/// Refuse un store où des messages attendent l'index (`index`) ou leurs vecteurs (`vectors`), et
/// dit où en est la lecture des PDF — un état d'avant, d'après, ou à moitié lu.
///
/// # Errors
///
/// Si une passe manque.
pub(crate) fn settled(store: &Store, index: bool, vectors: bool) -> Result<()> {
    let unindexed = store.unindexed_total()?;
    let unvectorised = store.unvectorised_total()?;
    anyhow::ensure!(
        !index || unindexed == 0,
        "{unindexed} messages attendent l'index plein texte — un texte de PDF lu, un import — : le \
         relevé mesurerait d'anciens documents. Lancez la mise à jour de l'index d'abord"
    );
    anyhow::ensure!(
        !vectors || unvectorised == 0,
        "{unvectorised} messages attendent leur vecteur : le relevé mesurerait d'anciens vecteurs. \
         Lancez `mail vectors` d'abord"
    );
    let unscanned = store.unscanned_total()?;
    let messages = store.message_count()?;
    let read: u64 = store.pdf_counts()?.iter().map(|(_, count)| count).sum();
    println!(
        "État des PDF      {}",
        if unscanned == messages && read == 0 {
            "aucun lu — un état d'avant".to_owned()
        } else if unscanned == 0 {
            format!("tous lus — {read} pièces rangées")
        } else {
            format!("à moitié : {unscanned} messages sur {messages} restent à lire")
        }
    );
    Ok(())
}

/// Une durée assez longue pour qu'un démon lancé par un banc ne démarre jamais sa passe de fond —
/// ni les vecteurs, ni la lecture des PDF — pendant la mesure : un jour, en millisecondes.
pub(crate) const NO_BACKGROUND_MS: &str = "86400000";

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn store_at(dir: &std::path::Path, version: u32) -> Utf8PathBuf {
        let root = Utf8PathBuf::from_path_buf(dir.to_path_buf()).unwrap();
        let connection =
            rusqlite::Connection::open(root.join("index.sqlite").as_std_path()).unwrap();
        connection.execute_batch("CREATE TABLE t(x)").unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        root
    }

    /// Un store ancien hors de `target/` est refusé — le corpus, dans son état du 2026-10-20 —, et
    /// la lecture de son en-tête ne crée aucun fichier à côté.
    #[test]
    fn an_old_store_outside_target_is_refused_without_touching_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = store_at(dir.path(), 15);
        let before: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        let refused = unmigrated(&root).unwrap_err().to_string();
        assert!(refused.contains("du schéma 15"), "{refused}");
        assert!(open_store(&root).is_err());
        let after: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(before.len(), after.len(), "le refus a écrit quelque chose");
    }

    /// Les contrôles inverses : un store à jour, un store neuf, une réplique sous `target/`
    /// s'ouvrent.
    #[test]
    fn a_current_store_a_new_store_and_a_replica_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let current = store_at(dir.path(), mailcore::store::migrations::SCHEMA_VERSION);
        unmigrated(&current).unwrap();
        let empty = tempfile::tempdir().unwrap();
        unmigrated(Utf8Path::from_path(empty.path()).unwrap()).unwrap();
        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target")
            .join("guard-test");
        std::fs::create_dir_all(&target).unwrap();
        let replica = tempfile::tempdir_in(&target).unwrap();
        let old = store_at(replica.path(), 15);
        unmigrated(&old).unwrap();
    }
}

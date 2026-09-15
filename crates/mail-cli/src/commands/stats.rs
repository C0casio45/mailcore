//! `mail stats` — état du store : blobs, références, fils, taille, dédup.
//!
//! C'est le chiffre à confronter aux 10 Gio du diagnostic de `docs/VISION.md`.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;

use mailapi::human;

/// Point d'entrée de la sous-commande.
///
/// # Errors
///
/// Si le store est introuvable ou illisible.
pub fn run(store_root: Option<&Utf8PathBuf>) -> Result<()> {
    let root = crate::store_root(store_root)?;
    let store = Store::open(&root).with_context(|| format!("ouverture du store {root}"))?;
    let stats = store.stats().context("lecture des statistiques")?;

    println!("Store              {root}");
    println!("Comptes            {}", stats.accounts);
    println!("Dossiers           {}", stats.folders);
    println!("Contenus distincts {}", stats.messages);
    println!("Références         {}", stats.refs);
    println!(
        "Réf. par message   {:.2}  — au-dessus de 1, la dédup a servi",
        stats.refs_per_message()
    );
    println!("Octets RFC 5322    {}", human::bytes(stats.raw_bytes));
    if stats.unthreaded > 0 {
        println!(
            "Sans fil           {} — la passe de threading n'est pas passée",
            stats.unthreaded
        );
    }
    Ok(())
}

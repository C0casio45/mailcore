//! `mail index` — (re)construit l'index plein texte depuis le store.
//!
//! Ne lit jamais le profil Thunderbird : l'import a déjà tout copié, et les blobs sont la
//! source de vérité. L'index est entièrement reconstructible — le perdre coûte du temps,
//! jamais une donnée.

use mailcore::Progress;
use std::time::Instant;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;

use mailapi::human;

/// Point d'entrée de la sous-commande.
///
/// # Errors
///
/// Si le store est introuvable ou l'index inaccessible en écriture.
pub fn run(store_root: Option<&Utf8PathBuf>) -> Result<()> {
    let root = crate::store_root(store_root)?;
    let store = Store::open(&root).with_context(|| format!("ouverture du store {root}"))?;

    println!("Store   {root}");
    println!("indexation en cours...\n");

    // Jamais annulée ici : la CLI tourne au premier plan, l'utilisateur a Ctrl-C. Le drapeau
    // sert au démon, qui expose l'annulation à ses clients.
    let progress = Progress::new();
    let started = Instant::now();
    let stats = mailcore::index::rebuild(&store, &progress).context("indexation")?;
    let elapsed = started.elapsed();

    println!("Durée              {elapsed:.1?}");
    println!("Messages indexés   {}", stats.indexed);
    println!("Texte indexé       {}", human::bytes(stats.text_bytes));
    if stats.degraded > 0 {
        println!(
            "MIME illisible     {} — indexés sur leurs métadonnées",
            stats.degraded
        );
    }
    if stats.missing_blobs > 0 {
        println!(
            "Blobs absents      {} — voir `mail doctor`",
            stats.missing_blobs
        );
    }
    Ok(())
}

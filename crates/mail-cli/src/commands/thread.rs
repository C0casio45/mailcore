//! `mail thread` — reconstruit les fils depuis les blobs.
//!
//! Passe séparée de l'import, parce que `jwz` a besoin de voir des messages qui n'arrivent
//! qu'après celui qu'on traite. Séparée de l'indexation aussi, même si les deux relisent les
//! mêmes blobs : les entremêler rendrait chacune plus difficile à tester, et la seconde
//! lecture coûte une trentaine de secondes sur le corpus complet.

use mailcore::Progress;
use std::time::Instant;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;

/// Point d'entrée de la sous-commande.
///
/// # Errors
///
/// Si le store est introuvable ou illisible.
pub fn run(store_root: Option<&Utf8PathBuf>) -> Result<()> {
    let root = crate::store_root(store_root)?;
    let store = Store::open(&root).with_context(|| format!("ouverture du store {root}"))?;

    println!("Store   {root}");
    println!("threading en cours...\n");

    // Jamais annulée ici : la CLI tourne au premier plan, l'utilisateur a Ctrl-C. Le drapeau
    // sert au démon, qui expose l'annulation à ses clients.
    let progress = Progress::new();
    let started = Instant::now();
    let stats = mailcore::thread::rebuild(&store, &progress).context("threading")?;
    let elapsed = started.elapsed();

    println!("Durée              {elapsed:.1?}");
    println!("Fils               {}", stats.threads);
    println!("Messages rattachés {}", stats.messages);
    println!("Liens References   {}", stats.links_by_references);
    println!("Liens par sujet    {}", stats.links_by_subject);
    println!(
        "Avec Message-ID    {} ({:.1} %)",
        stats.with_message_id,
        percent(stats.with_message_id, stats.messages)
    );
    println!(
        "Avec References    {} ({:.1} %)",
        stats.with_references,
        percent(stats.with_references, stats.messages)
    );
    println!(
        "Réf. non résolues  {} — le message parent n'est pas dans le corpus",
        stats.references_unresolved
    );
    println!(
        "Fils d'un message  {} ({:.1} %)",
        stats.singletons,
        percent(stats.singletons, stats.threads)
    );
    if stats.subject_groups_rejected > 0 {
        println!(
            "Sujets écartés     {} groupes trop gros pour être une conversation",
            stats.subject_groups_rejected
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

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    (part as f64 / whole as f64) * 100.0
}

//! `mail import` — parcourt un profil Thunderbird et importe son contenu.
//!
//! Rend en fin de course les statistiques du critère 6 : blobs créés, références créées,
//! taux de dédup, messages ignorés, messages en échec.
//!
//! N'écrit jamais dans le profil source (critère 7). Le chemin du profil se passe en
//! argument ou par `MAILCORE_TB_PROFILE` — jamais en dur dans le code.

use std::time::Instant;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;
use mailimport::import::{ImportOptions, ImportStats, import_profile};

use mailapi::human;

/// Point d'entrée de la sous-commande.
///
/// # Errors
///
/// Si le profil est introuvable ou le store inutilisable. Une erreur sur un dossier ou un
/// message n'interrompt pas l'import : elle est comptée et journalisée.
pub fn run(
    profile: &Utf8PathBuf,
    store_root: Option<&Utf8PathBuf>,
    dry_run: bool,
    include_feeds: bool,
    include_orphans: bool,
) -> Result<()> {
    let root = crate::store_root(store_root)?;

    let store = Store::open(&root).with_context(|| format!("ouverture du store {root}"))?;
    let mut options = ImportOptions::new(profile.as_std_path());
    options.dry_run = dry_run;
    options.include_feeds = include_feeds;
    options.include_orphans = include_orphans;

    println!("Profil  {profile}");
    println!("Store   {root}");
    if dry_run {
        println!("Mode    simulation — rien ne sera écrit");
    }
    println!();

    let started = Instant::now();
    let stats = import_profile(&store, &options, &mailcore::Progress::new()).context("import")?;
    report(&stats, started.elapsed());

    Ok(())
}

/// Affiche le bilan. C'est cette sortie que le critère 6 demande à voir.
fn report(stats: &ImportStats, elapsed: std::time::Duration) {
    println!("Durée              {elapsed:.1?}");
    println!("Dossiers           {}", stats.folders);
    // Jamais silencieux : ce qui est écarté est dit, avec le moyen de le récupérer.
    if stats.feed_folders_skipped > 0 {
        println!(
            "Flux RSS écartés   {} dossier(s) — --include-feeds pour les importer",
            stats.feed_folders_skipped
        );
    }
    if stats.orphan_folders_skipped > 0 {
        println!(
            "Orphelins écartés  {} dossier(s) de comptes absents de prefs.js — \n             --include-orphans pour les importer",
            stats.orphan_folders_skipped
        );
    }
    println!("Messages lus       {}", stats.messages_read);
    println!("Contenus stockés   {}", stats.blobs_created);
    println!(
        "Doublons           {} — {:.1} % des messages",
        stats.duplicates,
        stats.dedup_ratio()
    );
    println!("Références créées  {}", stats.refs_created);
    if stats.refs_existing > 0 {
        println!("Réf. déjà là       {}", stats.refs_existing);
    }
    println!();
    println!("Octets RFC 5322    {}", human::bytes(stats.raw_bytes));
    println!("Écrits sur disque  {}", human::bytes(stats.stored_bytes));
    println!(
        "Évités par dédup   {}",
        human::bytes(stats.deduplicated_bytes)
    );

    let anomalies = stats.expunged
        + stats.degraded
        + stats.undated
        + stats.unreadable_files
        + stats.aborted_files;
    if anomalies == 0 {
        return;
    }

    // Toujours affiché, jamais enfoui dans un journal : un import qui a perdu quelque chose
    // doit le dire à l'endroit où l'utilisateur regarde.
    println!("\nÀ signaler :");
    if stats.expunged > 0 {
        println!("  {} supprimés non compactés, ignorés", stats.expunged);
    }
    if stats.degraded > 0 {
        println!(
            "  {} en-têtes illisibles — messages stockés quand même",
            stats.degraded
        );
    }
    if stats.undated > 0 {
        println!("  {} sans date exploitable", stats.undated);
    }
    if stats.unreadable_files > 0 {
        println!("  {} dossiers illisibles", stats.unreadable_files);
    }
    if stats.aborted_files > 0 {
        println!(
            "  {} dossiers abandonnés en cours de lecture",
            stats.aborted_files
        );
    }
}

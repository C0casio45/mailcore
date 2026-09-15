//! `mail contacts` — reconstruire le carnet, et l'interroger.
//!
//! Le carnet est **dérivé du corpus** : aucun protocole, aucune requête réseau. Voir
//! `mailcore::contacts` pour le classement, qui est la seule partie qui décide de quoi que ce
//! soit.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;

/// Reconstruit le carnet depuis tous les messages du store.
///
/// # Errors
///
/// Si le store est illisible.
pub fn rebuild(root: Option<&Utf8PathBuf>) -> Result<()> {
    let store = open(root)?;
    let progress = mailcore::Progress::default();
    let started = std::time::Instant::now();

    let stats = mailcore::contacts::rebuild(&store, &progress).context("reconstruction")?;
    let elapsed = started.elapsed();

    println!("Messages parcourus {}", stats.scanned);
    println!("Adresses retenues  {}", stats.addresses);
    // **Le contrôle du carnet.** Un zéro veut dire que le classement n'a plus qu'un signal sur
    // deux, et l'autocomplétion redevient « par ordre de réception ».
    println!("Messages envoyés   {}", stats.outgoing);
    if stats.missing > 0 {
        println!("Blobs absents      {}", stats.missing);
    }
    println!("Durée              {:.1}s", elapsed.as_secs_f64());

    if stats.outgoing == 0 && stats.scanned > 0 {
        println!();
        println!(
            "Aucun message envoyé reconnu : le classement n'a qu'un signal sur deux. Vérifier \
             que les dossiers d'envoi des comptes sont bien synchronisés."
        );
    }
    Ok(())
}

/// Interroge le carnet, comme un champ de destinataire le ferait.
///
/// # Errors
///
/// Si le store est illisible.
pub fn complete(root: Option<&Utf8PathBuf>, prefix: &str, limit: usize) -> Result<()> {
    let store = open(root)?;
    let started = std::time::Instant::now();
    let found = store.complete(prefix, limit).context("complétion")?;
    let elapsed = started.elapsed();

    println!(
        "{} propositions en {:.1}ms",
        found.len(),
        elapsed.as_secs_f64() * 1000.0
    );
    println!();
    for contact in &found {
        // Le score est affiché : c'est ce qui rend le classement critiquable au lieu d'être
        // subi. « Pourquoi celui-là d'abord ? » doit avoir une réponse lisible.
        println!(
            "  {:>5}  {:>4}↑ {:>4}↓  {}",
            contact.score(),
            contact.seen_to,
            contact.seen_from,
            contact.label(),
        );
    }
    Ok(())
}

/// Le carnet en quelques nombres.
///
/// # Errors
///
/// Si le store est illisible.
pub fn stats(root: Option<&Utf8PathBuf>) -> Result<()> {
    let store = open(root)?;
    let count = store.contact_count().context("carnet")?;
    println!("Adresses connues {count}");
    if count == 0 {
        println!("Carnet vide — `mail contacts rebuild` pour le construire depuis le corpus.");
        return Ok(());
    }
    println!();
    println!("Les mieux classées :");
    for contact in &store.top_contacts(10).context("carnet")? {
        println!(
            "  {:>5}  {:>4}↑ {:>4}↓  {}",
            contact.score(),
            contact.seen_to,
            contact.seen_from,
            contact.label(),
        );
    }
    Ok(())
}

/// Ouvre le store local.
fn open(root: Option<&Utf8PathBuf>) -> Result<Store> {
    let root = crate::store_root(root)?;
    Store::open(&root).with_context(|| format!("ouverture du store {root}"))
}

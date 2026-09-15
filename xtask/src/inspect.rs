//! Ce que le store contient, dossier par dossier.
//!
//! ## Pourquoi cet outil existe
//!
//! Le 2026-09-08, la coquille n'affichait que trois lignes sur un store de 39 000 messages, et
//! il n'y avait **aucun moyen de savoir si le dossier ouvert en contenait trois ou trois
//! cents**. `mail stats` rend des totaux, `mail doctor` rend de la cohérence ; ni l'un ni
//! l'autre ne dit ce qu'il y a dans un dossier donné.
//!
//! Diagnostiquer un affichage sans pouvoir lire la donnée qu'il affiche, c'est deviner. Cette
//! commande est là pour ne pas deviner.
//!
//! ## Elle lit, et rien d'autre
//!
//! Le store est ouvert normalement, mais aucune écriture n'est faite. Elle est utilisable
//! pendant qu'une synchronisation tourne : SQLite en WAL laisse lire pendant qu'on écrit, et
//! c'est même une des choses qu'on veut vérifier.

use anyhow::{Context, Result};
use camino::Utf8Path;

/// Affiche les dossiers du store avec leurs compteurs.
///
/// `min` masque les dossiers en dessous d'un seuil : un profil réel en a des dizaines de vides,
/// et les lister noie ce qu'on cherche.
///
/// # Errors
///
/// Si le store est illisible.
pub fn folders(root: &Utf8Path, min: u64) -> Result<()> {
    let mailbox =
        mailcore::Mailbox::open(root).with_context(|| format!("ouverture du store {root}"))?;
    let folders = mailbox.folders().context("liste des dossiers")?;

    let mut shown = 0_usize;
    let mut hidden = 0_usize;
    let mut total = 0_u64;
    let mut account = String::new();

    for summary in &folders {
        total += summary.total;
        if summary.total < min {
            hidden += 1;
            continue;
        }
        if summary.account_name != account {
            println!();
            println!("{}", summary.account_name);
            account.clone_from(&summary.account_name);
        }
        // Le rôle est affiché parce que c'est **lui** qui décide du dossier ouvert au
        // démarrage de la coquille : elle prend le premier `inbox` qu'elle trouve. Un rôle mal
        // deviné et l'utilisateur s'ouvre sur un dossier qui n'est pas sa boîte de réception.
        println!(
            "  {:>7}  {:>6} non lus  [{:8}] #{:<4} {}",
            summary.total,
            summary.unread,
            summary.folder.kind.as_str(),
            summary.folder.id.0,
            summary.folder.path,
        );
        shown += 1;
    }

    println!();
    println!(
        "{} dossiers ({shown} affichés, {hidden} sous le seuil)",
        folders.len()
    );
    println!("{total} références au total");

    // Le dossier que la coquille ouvrirait, calculé **de la même façon qu'elle** : premier
    // `inbox` dans l'ordre de la liste, à défaut le premier dossier tout court. C'est la
    // réponse à « pourquoi je ne vois que trois messages ».
    let opened = folders
        .iter()
        .find(|it| it.folder.kind == mailcore::FolderKind::Inbox)
        .or_else(|| folders.first());
    match opened {
        Some(summary) => println!(
            "La coquille ouvrirait #{} {} — {} message(s)",
            summary.folder.id.0, summary.folder.path, summary.total
        ),
        None => println!("Aucun dossier : la coquille ouvrirait son panneau d'import"),
    }
    Ok(())
}

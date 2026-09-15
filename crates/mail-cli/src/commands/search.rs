//! `mail search` — recherche plein texte dans le corpus indexé.
//!
//! Sert aussi de banc de mesure du critère 4 : p95 sous 50 ms.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;
use mailcore::index::{Searcher, open_or_create};

/// Point d'entrée de la sous-commande.
///
/// # Errors
///
/// Si le store ou l'index est introuvable, ou si la requête est mal formée.
pub fn run(store_root: Option<&Utf8PathBuf>, query: &str, limit: usize) -> Result<()> {
    let root = crate::store_root(store_root)?;
    let store = Store::open(&root).with_context(|| format!("ouverture du store {root}"))?;
    let (index, _) = open_or_create(&store).context("ouverture de l'index")?;
    let searcher = Searcher::open(&index).context("ouverture du chercheur")?;

    let started = std::time::Instant::now();
    let hits = searcher.search(query, limit).context("recherche")?;
    let elapsed = started.elapsed();

    if hits.is_empty() {
        println!("aucun résultat pour « {query} »   ({elapsed:.1?})");
        return Ok(());
    }

    println!("{} résultats en {elapsed:.1?}\n", hits.len());
    for hit in &hits {
        // Une requête par résultat, sur la clé primaire : c'est le prix de ne pas stocker
        // le contenu dans l'index. À 50 résultats, c'est du bruit dans le budget.
        let Some(item) = store.message(hit.id).context("relecture du message")? else {
            println!(
                "  [{}] introuvable dans l'index — voir `mail doctor`",
                hit.id.0
            );
            continue;
        };
        println!(
            "  {:>6.2}  {}  {:<32.32}  {}",
            hit.score,
            crate::human::date(item.date),
            item.from_name.as_deref().unwrap_or(&item.from_addr),
            item.subject
        );
    }
    Ok(())
}

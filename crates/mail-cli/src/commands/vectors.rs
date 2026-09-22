//! `mail vectors` — calcule les embeddings du corpus avec le modèle local.
//!
//! Le pendant de `mail index` pour la recherche sémantique. Comme lui, il ne lit jamais le profil
//! Thunderbird : les blobs sont la source de vérité, et le magasin de vecteurs est entièrement
//! reconstructible — le perdre coûte une passe, jamais une donnée.
//!
//! ## Pourquoi la passe incrémentale est le défaut, et la reconstruction une option
//!
//! Le sens inverse de `mail index`, qui reconstruit toujours. La raison est le coût : une passe
//! complète sur le corpus réel prend **339 s**, là où l'indexation plein texte se compte en
//! dizaines de secondes. Relancer la commande pour rattraper le courrier arrivé depuis doit être
//! bon marché, sans quoi personne ne la relancera.
//!
//! `--rebuild` reste nécessaire pour un cas précis : **changer de modèle ou de budget de texte**.
//! Les vecteurs d'un modèle ne se comparent pas à ceux d'un autre, et mélanger les deux donnerait
//! un classement dont chaque moitié est correcte et dont la comparaison ne veut rien dire.

use std::time::Instant;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Progress, Store};

/// Traduit une erreur de `mailvec` en phrase utilisable.
///
/// Le modèle absent est le cas nominal d'une installation qui n'a pas fait l'étape, pas un
/// incident. Il a droit à une phrase qui dit quoi faire — critère 8 de la phase 3 — et non à un
/// message d'erreur de bibliothèque.
fn explain(error: mailvec::Error) -> anyhow::Error {
    match error {
        mailvec::Error::Model { path, reason } => anyhow::anyhow!(
            "aucun modèle utilisable dans {path} ({reason}).\n\
             La recherche sémantique a besoin d'un modèle local ; le plein texte, lui, marche \
             sans.\n\
             Installez-en un dans ce répertoire, ou désignez-en un autre avec `--model`."
        ),
        other => anyhow::Error::new(other).context("vectorisation"),
    }
}

/// Point d'entrée de la sous-commande.
///
/// # Errors
///
/// Si le store est introuvable, si le modèle n'est pas installé, ou si le magasin de vecteurs
/// n'est pas accessible en écriture.
pub fn run(
    store_root: Option<&Utf8PathBuf>,
    model: Option<&Utf8PathBuf>,
    rebuild: bool,
    budget: Option<usize>,
    bert: bool,
) -> Result<()> {
    let root = crate::store_root(store_root)?;
    let store = Store::open(&root).with_context(|| format!("ouverture du store {root}"))?;
    let model = model
        .cloned()
        .unwrap_or_else(|| mailvec::model::default_dir(&root));

    // Le budget de texte : combien de caractères du corps entrent dans le vecteur. Changer sa
    // valeur change **tous** les vecteurs, donc il impose une reconstruction — c'est dit dans
    // l'aide de --budget, et --rebuild est là pour ça.
    let budget = budget.map_or_else(mailvec::text::Budget::default, mailvec::text::Budget::of);

    // **La question est posée à `mailvec`, pas refaite ici.** Cette commande comptait elle-même
    // les messages sans vecteur et retournait sur zéro — donc elle n'atteignait jamais le
    // contrôle de péremption ajouté dans la passe le 2026-09-23, et continuait d'annoncer « rien
    // à faire » sur un magasin écrit par une autre composition de texte. Deux endroits qui
    // répondent à la même question finissent par ne pas y répondre pareil.
    let pending = mailvec::pass::pending(&store, budget)
        .map_err(explain)
        .context("état du magasin de vecteurs")?;
    println!("Store              {root}");
    println!("Modèle             {model}");
    if rebuild {
        println!("Mode               reconstruction complète");
    } else {
        if pending.stale {
            println!(
                "Magasin périmé     écrit par une autre composition de texte ou un autre budget"
            );
            println!("                   — le corpus entier est revectorisé");
        } else {
            println!("À vectoriser       {} messages", pending.messages);
        }
        if pending.is_empty() {
            println!("\nRien à faire — tout le corpus a déjà son vecteur.");
            println!("Pour refaire après un changement de modèle : `mail vectors --rebuild`.");
            return Ok(());
        }
    }
    println!("vectorisation en cours...\n");

    // Jamais annulée ici : la CLI tourne au premier plan, l'utilisateur a Ctrl-C — et une passe
    // coupée repart où elle en était, ce qui est précisément ce que le critère 3 garantit.
    let progress = Progress::new();
    let started = Instant::now();
    // Le choix de la famille est explicite et non deviné du contenu du répertoire : deux
    // modèles y ressemblent de loin, et charger le mauvais rendrait des vecteurs de la bonne
    // taille et du mauvais sens.
    let stats = if bert {
        let encoder = match mailvec::bert::Bert::load(&model) {
            Ok(encoder) => encoder,
            Err(error) => return Err(explain(error)),
        };
        if rebuild {
            mailvec::pass::rebuild_with(&store, &encoder, budget, &progress)
        } else {
            mailvec::pass::update_with(&store, &encoder, budget, &progress)
        }
    } else if rebuild {
        mailvec::pass::rebuild(&store, &model, budget, &progress)
    } else {
        mailvec::pass::update(&store, &model, budget, &progress)
    };

    let stats = match stats {
        Ok(stats) => stats,
        Err(error) => return Err(explain(error)),
    };
    let elapsed = started.elapsed();

    #[allow(clippy::cast_precision_loss)]
    let rate = stats.vectorised as f64 / elapsed.as_secs_f64().max(f64::EPSILON);

    println!("Durée              {elapsed:.1?}");
    println!("Messages vectorisés {}", stats.vectorised);
    println!("Débit              {rate:.0} messages/s");
    if stats.empty > 0 {
        println!(
            "Sans texte         {} — vectorisés sur leurs métadonnées seules",
            stats.empty
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

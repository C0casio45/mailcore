//! `model-probe` — la question la moins chère à poser à un modèle d'embeddings.
//!
//! ## Pourquoi cette sonde existe
//!
//! Le 2026-09-21, le critère 1 a rendu 0 % sur le sous-ensemble dur et le relevé désignait le
//! modèle. « Désigner » n'est pas « démontrer » : un rappel nul peut venir du modèle, mais aussi
//! d'un budget de texte mal choisi, d'une requête encodée autrement que les documents, ou d'un
//! réglage de fusion. Chacune de ces hypothèses coûte une passe complète à écarter — six minutes
//! plus un banc.
//!
//! Celle-ci coûte une seconde, et elle les départage toutes : **on demande au modèle si deux mots
//! qu'un humain juge proches le sont pour lui.** Si « colis » et « paquet » sont aussi éloignés
//! que « colis » et « voiture », aucun réglage en aval ne rattrapera quoi que ce soit, et le
//! modèle est le coupable démontré plutôt que suspecté.
//!
//! C'est la même forme que les quatre cents octets d'en-tête `safetensors` lus le 2026-09-15
//! avant de télécharger 530 Mo : la mesure la plus rentable est celle qu'on n'a pas eu besoin de
//! faire en grand.
//!
//! ## Comment le lire
//!
//! Chaque ligne compare une paire **proche** à une paire **témoin** qui partage son premier mot.
//! Ce qui compte n'est pas la valeur absolue du cosinus — elle dépend du modèle — mais l'**écart**
//! entre les deux. Un modèle qui a du sens met la paire proche nettement au-dessus du témoin ; un
//! modèle qui n'en a pas les met à égalité.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;

/// Les paires de la sonde : (mot, proche, témoin).
///
/// Elles ne sont pas prises au hasard : les quatre premières sont **les cas que le critère 1 a
/// ratés** le 2026-09-21, et les suivantes sont des synonymies plus faciles et un cas
/// interlangue. Sonder sur les cas réels plutôt que sur des exemples de manuel est ce qui rend la
/// réponse utilisable.
const PAIRS: &[(&str, &str, &str)] = &[
    // Les ratés du sous-ensemble dur, en français.
    ("signalement", "dénoncé", "casserole"),
    ("pleine conscience", "méditation", "plomberie"),
    ("colis", "paquet", "voiture"),
    ("désactivé", "coupé", "bleu"),
    // Des synonymies franches, pour savoir si le modèle en attrape au moins une.
    ("facture", "note à payer", "bicyclette"),
    ("rendez-vous", "entrevue", "fromage"),
    ("acheter", "acquérir", "dormir"),
    // Le passage d'une langue à l'autre, qui est sept des dix ratés.
    ("commentaires", "comments", "betterave"),
    ("stockage en ligne", "cloud storage", "chaussure"),
    ("clé de jeu", "game key", "parapluie"),
];

/// Interroge le modèle sur des paires dont un humain connaît la réponse.
///
/// # Errors
///
/// Si le modèle n'est pas chargeable.
pub fn model(dir: &Utf8PathBuf, bert: bool) -> Result<()> {
    let mut texts = Vec::new();
    for (word, near, control) in PAIRS {
        texts.push((*word).to_owned());
        texts.push((*near).to_owned());
        texts.push((*control).to_owned());
    }

    // Les deux familles derrière la même sonde, pour que les chiffres se comparent ligne à ligne.
    // Le transformeur encode ici en rôle **passage** des deux côtés de chaque paire : la sonde
    // compare des textes entre eux, pas une requête à un document.
    let (dim, vectors) = if bert {
        let model = mailvec::bert::Bert::load(dir).context("chargement du transformeur")?;
        let vectors = model
            .encode_as(&texts, mailvec::bert::Role::Passage)
            .context("encodage de la sonde")?;
        (model.dim(), vectors)
    } else {
        let model = mailvec::model::Model::load(dir).context("chargement du modèle")?;
        let vectors = model.encode(&texts).context("encodage de la sonde")?;
        (model.dim(), vectors)
    };

    println!("Modèle             {dir}");
    println!(
        "Famille            {}",
        if bert {
            "transformeur"
        } else {
            "embedding statique"
        }
    );
    println!("Dimension          {dim}\n");

    println!(
        "{:<22} {:<20} {:>8}   {:<16} {:>8}   {:>8}",
        "mot", "proche", "cos", "témoin", "cos", "écart"
    );

    let mut gaps = Vec::with_capacity(PAIRS.len());
    for (index, (word, near, control)) in PAIRS.iter().enumerate() {
        let base = &vectors[index * 3];
        let close = cosine(base, &vectors[index * 3 + 1]);
        let far = cosine(base, &vectors[index * 3 + 2]);
        let gap = close - far;
        gaps.push(gap);
        println!("{word:<22} {near:<20} {close:>8.3}   {control:<16} {far:>8.3}   {gap:>+8.3}");
    }

    let ordered = gaps.iter().filter(|gap| **gap > 0.0).count();
    #[allow(clippy::cast_precision_loss)]
    let mean = gaps.iter().sum::<f32>() / gaps.len() as f32;

    println!("\nPaires bien ordonnées  {ordered} sur {}", gaps.len());
    println!("Écart moyen            {mean:+.3}   (voir ci-dessous)");
    println!(
        "\nLecture : **l'ordre d'abord, l'écart ensuite.** Un modèle est utile pour la recherche\n\
         s'il place la paire proche au-dessus du témoin ; c'est la seule chose qui se compare\n\
         d'un modèle à l'autre.\n\
         \n\
         L'écart, lui, **ne se compare pas entre familles**. Les modèles de type `e5` sont\n\
         anisotropes : tous leurs vecteurs tiennent dans un cône étroit, donc tous leurs cosinus\n\
         sont hauts et tous leurs écarts petits. Lire un écart de +0,05 comme « moins bon » qu'un\n\
         écart de +0,33 sur un autre modèle serait l'erreur qu'on a refusé de faire en fusionnant\n\
         un BM25 et un cosinus. Seul le classement final sur le corpus tranche."
    );
    Ok(())
}

/// Le cosinus de deux vecteurs, normalisation comprise.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

//! La taille du vocabulaire du transformeur, et sa vérification — le critère 5 de
//! `docs/PHASE-4.md`.
//!
//! La taille elle-même vit dans `mailvec::trim`, parce que l'installation du modèle l'appellera ;
//! ce module-ci la déclenche et, surtout, **vérifie ce qu'elle a produit sur le corpus réel**.
//!
//! ## Ce que la vérification prouve, et comment
//!
//! Deux étages, du moins cher au plus cher :
//!
//! 1. **les pièces, sur tout le corpus.** Deux modèles qui donnent la même suite de pièces à un
//!    texte, et dont la table porte pour chaque pièce la même ligne, donnent le même vecteur. La
//!    seconde moitié est prouvée par les tests de `mailvec::trim` ; la première se vérifie ici, sur
//!    chacun des messages, avec le texte exact que la passe compose — `pass::compose`, pas une
//!    recomposition. Tokeniser ne coûte presque rien, donc rien ne justifie un échantillon.
//! 2. **les vecteurs, sur un échantillon.** Encoder coûte cher, et cet étage ne sert qu'à
//!    vérifier le raisonnement du premier : des pièces égales doivent donner des vecteurs égaux
//!    **au bit près**. Un écart, même minuscule, dirait que quelque chose d'autre a bougé.
//!
//! Le tableau croise deux questions : le texte n'emploie-t-il que des caractères gardés, et ses
//! pièces sont-elles les mêmes ? Une case doit être **vide** — texte entièrement gardé, pièces
//! différentes —, parce que c'est la propriété que la taille affirme. Une autre dit ce qu'elle
//! coûte.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;
use mailvec::bert::{Bert, Role};
use mailvec::pass::Composed;

/// Taille un modèle transformeur.
///
/// # Errors
///
/// Si la source est illisible ou d'un format non géré, ou si la cible est occupée.
pub fn trim(model: &Utf8PathBuf, out: &Utf8PathBuf) -> Result<()> {
    let report = mailvec::trim::trim(model, out).context("taille du vocabulaire")?;
    #[allow(clippy::cast_precision_loss)]
    let share = report.kept as f64 * 100.0 / report.pieces.max(1) as f64;
    println!("Source            {model}");
    println!("Cible             {out}");
    println!(
        "Pièces            {} → {} ({share:.1} %)",
        report.pieces, report.kept
    );
    println!("Lignes de table   {} → {}", report.rows, report.kept);
    println!(
        "Par normalisation {} caractères hors des blocs, gardés — `μ` depuis `µ`",
        report.images
    );
    for name in ["model.safetensors", "tokenizer.json"] {
        let before = size(&model.join(name));
        let after = size(&out.join(name));
        println!(
            "{name:<18}{} → {}",
            crate::model::mib(before),
            crate::model::mib(after)
        );
    }
    println!("Empreinte         {}", report.digest);
    Ok(())
}

fn size(path: &Utf8PathBuf) -> usize {
    std::fs::metadata(path.as_std_path())
        .map_or(0, |m| usize::try_from(m.len()).unwrap_or(usize::MAX))
}

/// Les quatre cases du tableau.
#[derive(Debug, Default)]
struct Cross {
    /// Texte entièrement gardé, mêmes pièces : le cas attendu.
    kept_same: u64,
    /// Texte entièrement gardé, pièces différentes : **doit rester à zéro**.
    kept_differ: u64,
    /// Texte qui emploie une autre écriture, pièces différentes : ce que la taille coûte.
    other_differ: u64,
    /// Texte qui emploie une autre écriture, mêmes pièces : les caractères étrangers étaient au
    /// delà de la fenêtre de 256 pièces, ou la normalisation les a ramenés au latin — une lettre
    /// grecque de compatibilité, un exposant.
    other_same: u64,
}

/// Vérifie un modèle taillé contre celui dont il vient, sur le corpus.
///
/// # Errors
///
/// Si un des modèles n'est pas chargeable, si le store est illisible, ou si un encodage échoue.
pub fn check(
    store: &Utf8PathBuf,
    model: &Utf8PathBuf,
    trimmed: &Utf8PathBuf,
    sample: usize,
) -> Result<()> {
    let full = Bert::load(model).context("modèle d'origine")?;
    let small = Bert::load(trimmed).context("modèle taillé")?;
    let store = Store::open(store).context("ouverture du store")?;
    let rows = store.all_for_indexing().context("lecture des messages")?;
    let budget = mailvec::text::Budget::default();

    println!("Messages          {}", rows.len());
    let mut cross = Cross::default();
    let mut missing = 0u64;
    let mut unknown_pieces = 0u64;
    let mut pieces = 0u64;
    // Les textes gardés pour l'étage des vecteurs : les premiers de chaque sorte.
    let mut same_texts: Vec<String> = Vec::new();
    let mut same_ids: Vec<mailcore::MessageId> = Vec::new();
    let mut differ_texts: Vec<String> = Vec::new();
    // Les identifiants des messages qui violent la propriété, pour aller les regarder.
    let mut violations: Vec<i64> = Vec::new();
    // Les caractères que le modèle entier voit et que la taille retire, **après normalisation** :
    // par points de code, jamais par texte — c'est du courrier. Un pour les messages qui violent
    // la propriété, un pour tous.
    let mut culprits: std::collections::BTreeMap<u32, u64> = std::collections::BTreeMap::new();
    let mut lost: std::collections::BTreeMap<u32, u64> = std::collections::BTreeMap::new();

    for (done, row) in rows.iter().enumerate() {
        let row = mailcore::store::read::VectorRow {
            id: row.id,
            blob: row.blob,
            subject: row.subject.clone(),
            from_name: row.from_name.clone(),
        };
        let text = match mailvec::pass::compose(&store, &row, budget)? {
            Composed::Missing | Composed::Unreadable => {
                missing += 1;
                continue;
            }
            Composed::Text(text) => text,
        };
        let a = full.tokens(&text, Role::Passage)?;
        let b = small.tokens(&text, Role::Passage)?;
        pieces += b.len() as u64;
        unknown_pieces += b.iter().filter(|piece| *piece == "<unk>").count() as u64;
        let latin = text.chars().all(mailvec::trim::is_kept);
        let same = a == b;
        // Les caractères que le modèle entier voit **et que le taillé a perdus** : présents dans
        // ses pièces, absents de celles du taillé. Pas une seconde copie de la règle de la taille —
        // la première version comptait par `is_kept`, et nommait « retirés » les caractères que
        // la normalisation fait garder, `μ` compris.
        let kept_here: std::collections::HashSet<char> =
            b.iter().flat_map(|piece| piece.chars()).collect();
        let removed: std::collections::BTreeSet<u32> = a
            .iter()
            .flat_map(|piece| piece.chars())
            .filter(|c| !kept_here.contains(c))
            .map(u32::from)
            .collect();
        for code in removed {
            *lost.entry(code).or_default() += 1;
            if latin && !same {
                *culprits.entry(code).or_default() += 1;
            }
        }
        match (latin, same) {
            (true, true) => cross.kept_same += 1,
            (true, false) => {
                cross.kept_differ += 1;
                if violations.len() < 10 {
                    violations.push(row.id.0);
                }
            }
            (false, false) => cross.other_differ += 1,
            (false, true) => cross.other_same += 1,
        }
        if same && same_texts.len() < sample {
            same_texts.push(text);
            same_ids.push(row.id);
        } else if !same && differ_texts.len() < sample {
            differ_texts.push(text);
        }
        if (done + 1) % 10_000 == 0 {
            println!("  … {} messages", done + 1);
        }
    }

    println!("\n--- étage 1 : les pièces, sur tout le corpus ---");
    println!("Blobs absents                         {missing}");
    println!("Texte gardé,     mêmes pièces         {}", cross.kept_same);
    println!(
        "Texte gardé,     pièces différentes   {}   {}",
        cross.kept_differ,
        if cross.kept_differ == 0 {
            "— la propriété tient"
        } else {
            "— ANOMALIE : la propriété de la taille est fausse"
        }
    );
    println!(
        "Autre écriture,  pièces différentes   {}",
        cross.other_differ
    );
    println!("Autre écriture,  mêmes pièces         {}", cross.other_same);
    #[allow(clippy::cast_precision_loss)]
    let unknown_share = unknown_pieces as f64 * 100.0 / pieces.max(1) as f64;
    println!(
        "Pièces devenues <unk>                 {unknown_pieces} sur {pieces} ({unknown_share:.3} %)"
    );
    if !violations.is_empty() {
        println!("Messages à regarder                   {violations:?}");
        println!(
            "Caractères en cause, normalisés       {}",
            top(&culprits, 10)
        );
    }
    println!("Caractères retirés les plus vus       {}", top(&lost, 15));

    println!("\n--- étage 2 : les vecteurs, sur un échantillon ---");
    let exact = compare(&full, &small, &same_texts)?;
    println!(
        "Mêmes pièces      {} messages : {} identiques au bit près, écart maximal {:e}",
        same_texts.len(),
        exact.identical,
        exact.max_gap
    );
    if exact.identical != same_texts.len() {
        println!("ANOMALIE          des pièces égales ont donné des vecteurs différents");
    }
    if !differ_texts.is_empty() {
        let lost = compare(&full, &small, &differ_texts)?;
        println!(
            "Pièces changées   {} messages : cosinus minimal {:.4}, médian {:.4}",
            differ_texts.len(),
            lost.min_cosine,
            lost.median_cosine
        );
    }

    against_store(&store, &full, &same_ids, &same_texts)?;
    Ok(())
}

/// L'étage 3 : ce que le code d'aujourd'hui calcule, contre ce que le magasin du corpus porte.
///
/// Le magasin a été écrit par une passe antérieure — autre chargement des poids, autres lots —
/// avec le **même** modèle. Ne comparer que si sa provenance porte l'empreinte de ce modèle :
/// comparer deux modèles ne dirait rien du code. Et par la recherche plutôt que par un accès
/// direct : le vecteur rangé d'un message doit revenir en tête quand on cherche avec le vecteur
/// recalculé, et le score rendu est le cosinus des deux.
fn against_store(
    store: &Store,
    full: &Bert,
    ids: &[mailcore::MessageId],
    texts: &[String],
) -> Result<()> {
    use mailvec::pass::Encoder;

    println!("\n--- étage 3 : le code d'aujourd'hui contre le magasin du corpus ---");
    let path = store.vectors_path();
    let digest = full.identity().digest;
    let Some(summary) = mailcore::vectors::summary_of(&path)? else {
        println!("Pas de magasin — rien à comparer");
        return Ok(());
    };
    // Même modèle **et même composition du texte** : un magasin d'un autre budget ou d'une autre
    // révision porte d'autres textes, et ses écarts accuseraient le code à tort.
    let budget = mailvec::text::Budget::default();
    let Some(provenance) = summary.provenance.as_ref() else {
        println!("Magasin sans provenance — rien à comparer");
        return Ok(());
    };
    if provenance.digest != digest || summary.dim != full.dim() {
        println!(
            "Magasin écrit par un autre modèle ({}) — rien à comparer",
            provenance.digest
        );
        return Ok(());
    }
    if provenance.budget != budget.body || provenance.revision != mailvec::text::REVISION {
        println!(
            "Magasin d'une autre composition (budget {}, révision {}) — rien à comparer",
            provenance.budget, provenance.revision
        );
        return Ok(());
    }
    let stored = mailcore::vectors::Vectors::load(&path, full.dim())?;
    let mut cosines = Vec::with_capacity(ids.len());
    let mut absent = 0;
    for (chunk_ids, chunk) in ids.chunks(64).zip(texts.chunks(64)) {
        for (id, vector) in chunk_ids.iter().zip(full.encode_as(chunk, Role::Passage)?) {
            // Le vecteur rangé **de ce message**, et non le plus proche : une recherche rendait
            // un doublon exact à sa place, et un message absent se confondait avec un écart.
            match stored.get(*id) {
                Some(row) => cosines.push(cosine(&vector, row)),
                None => absent += 1,
            }
        }
    }
    cosines.sort_by(f32::total_cmp);
    let close = cosines.iter().filter(|c| **c >= 0.999_99).count();
    println!(
        "{} messages : {close} à cosinus ≥ 0,99999, minimal {:.6}, absents du magasin {absent}",
        ids.len(),
        cosines.first().copied().unwrap_or(f32::NAN)
    );
    if close != ids.len() {
        println!("ANOMALIE          le code d'aujourd'hui ne rend pas les vecteurs du magasin");
    }
    Ok(())
}

/// Les points de code les plus fréquents, en `U+XXXX ×n`.
fn top(counts: &std::collections::BTreeMap<u32, u64>, n: usize) -> String {
    let mut sorted: Vec<(&u32, &u64)> = counts.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    sorted
        .into_iter()
        .take(n)
        .map(|(code, count)| format!("U+{code:04X} ×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Ce que la comparaison de deux lots de vecteurs rend.
struct Comparison {
    identical: usize,
    max_gap: f32,
    min_cosine: f32,
    median_cosine: f32,
}

/// Encode les mêmes textes avec les deux modèles et compare, texte par texte.
fn compare(full: &Bert, small: &Bert, texts: &[String]) -> Result<Comparison> {
    let mut identical = 0;
    let mut max_gap = 0f32;
    let mut cosines = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(16) {
        let a = full.encode_as(chunk, Role::Passage)?;
        let b = small.encode_as(chunk, Role::Passage)?;
        for (x, y) in a.iter().zip(&b) {
            if x == y {
                identical += 1;
            }
            let gap = x
                .iter()
                .zip(y)
                .map(|(p, q)| (p - q).abs())
                .fold(0f32, f32::max);
            max_gap = max_gap.max(gap);
            cosines.push(cosine(x, y));
        }
    }
    cosines.sort_by(f32::total_cmp);
    Ok(Comparison {
        identical,
        max_gap,
        min_cosine: cosines.first().copied().unwrap_or(f32::NAN),
        median_cosine: cosines.get(cosines.len() / 2).copied().unwrap_or(f32::NAN),
    })
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm(a) * norm(b)).max(f32::MIN_POSITIVE)
}

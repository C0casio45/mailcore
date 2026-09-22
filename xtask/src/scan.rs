//! Le banc du critère 2 de `docs/PHASE-4.md` : la latence d'un balayage vectoriel.
//!
//! ## Pourquoi il mesure sur des vecteurs fabriqués
//!
//! L'étape 4 de la phase le dit : « la latence d'un balayage ne dépend pas de ce que les
//! nombres veulent dire ». Un produit scalaire coûte le même prix sur un vecteur qui décrit un
//! message et sur un vecteur tiré au sort — donc ce banc peut tourner **avant** la première
//! passe de vectorisation, ce qui est tout l'intérêt : on saura si un balayage complet tient le
//! budget avant d'avoir écrit le moteur qui le remplirait.
//!
//! Une chose dépend quand même des nombres, et elle est mesurée à part : le **tas borné**. Sa
//! porte d'entrée écarte un message dès que son score est sous le pire des retenus, et cette
//! porte se franchit d'autant plus souvent que les scores arrivent en ordre croissant. Le banc
//! fabrique donc aussi ce pire cas, plutôt que de rendre un chiffre optimiste sur du hasard.
//!
//! ## Aucun `--store`
//!
//! Le banc écrit, donc il crée son propre répertoire jetable — la règle de `measure-followup`,
//! écrite après que `measure-attachment` a laissé 533 Mo dans les données de production le
//! 2026-09-09. Un paramètre qui peut désigner le store réel le désignera.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::MessageId;
use mailcore::vectors::{VectorWriter, Vectors};

use crate::model::{current_rss, mib};

/// Mesure le coût d'un balayage complet : remplissage, relecture, et latence par requête.
///
/// # Errors
///
/// En profil de débogage, avant toute mesure — voir ci-dessous. Sinon, si le répertoire jetable
/// n'est pas créable, ou si le magasin n'est pas écrivable.
pub fn measure(messages: usize, dim: usize, limit: usize, queries: usize) -> Result<()> {
    // Le balayage vit dans `mailcore`, qui est un **membre de l'espace de travail** : le
    // `[profile.dev.package."*"]` du `Cargo.toml` racine optimise les dépendances et pas lui.
    // Le produit scalaire y est donc interprété boucle par boucle, et le p95 qui en sort n'a
    // aucun rapport avec celui du binaire livré.
    //
    // C'est le contraire des bancs voisins : `measure-search` chronomètre tantivy et
    // `model-bench` chronomètre `model2vec-rs`, tous deux des dépendances, donc tous deux
    // optimisés même en débogage. L'habitude prise sur eux est ici un piège, et un chiffre
    // faux publié dans un journal de phase ne se rattrape pas.
    //
    // Un refus plutôt qu'un avertissement : un relevé imprimé finit recopié.
    if cfg!(debug_assertions) {
        anyhow::bail!(
            "measure-scan chronomètre du code de `mailcore`, qui n'est pas optimisé en \
             profil de débogage — le relevé serait faux.\n\
             Lancez : cargo run --release -p xtask -- measure-scan"
        );
    }

    let dir = tempfile::tempdir().context("répertoire jetable")?;
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
        .map_err(|p| anyhow::anyhow!("chemin non UTF-8 : {}", p.display()))?;

    println!("Corpus fabriqué   {messages} messages × {dim} dimensions\n");

    let scattered = fill(&root.join("epars.bin"), messages, dim, Shape::Scattered)?;
    let rising = fill(&root.join("croissant.bin"), messages, dim, Shape::Rising)?;

    // La relecture, trois fois, première jetée : c'est la parade du `CLAUDE.md`, et elle vaut
    // ici parce que le premier chargement paie aussi le cache de fichiers du système.
    let mut loads = Vec::new();
    let mut vectors = Vectors::empty(dim);
    for _ in 0..3 {
        let at = Instant::now();
        vectors = Vectors::load(&scattered, dim).context("relecture du magasin")?;
        loads.push(at.elapsed());
    }
    let rss_loaded = current_rss();

    println!("Fichier           {}", mib(file_size(&scattered)));
    println!(
        "Relecture         {} / {} / {}   (la première est jetée)",
        fmt(loads[0]),
        fmt(loads[1]),
        fmt(loads[2])
    );
    println!("Vecteurs relus    {}", vectors.len());
    println!("RSS après         {}\n", mib(rss_loaded as usize));

    let epars = latencies(&vectors, dim, limit, queries, Shape::Scattered)?;
    let croissant = {
        let vectors = Vectors::load(&rising, dim).context("relecture du magasin croissant")?;
        latencies(&vectors, dim, limit, queries, Shape::Rising)?
    };

    report(limit, queries, &epars, &croissant);
    Ok(())
}

/// La forme du jeu de vecteurs, et de quoi elle est le cas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Des vecteurs sans rapport entre eux : le cas ordinaire, où la porte du tas écarte la
    /// grande majorité des messages sans les insérer.
    Scattered,
    /// Des vecteurs dont la proximité à la requête **croît** avec leur rang : chaque message
    /// bat le pire des retenus, donc chaque message est inséré. C'est le pire cas du tas borné,
    /// et il n'arrive jamais par hasard — d'où le fabriquer.
    Rising,
}

/// Remplit un magasin, et rend son chemin.
fn fill(path: &Utf8PathBuf, messages: usize, dim: usize, shape: Shape) -> Result<Utf8PathBuf> {
    // Le banc de latence fabrique ses vecteurs : ils ne viennent d'aucun modèle, et la
    // provenance le dit plutôt que d'emprunter le nom d'un vrai. Un magasin de mesure qu'on
    // retrouverait par erreur doit s'annoncer comme tel — c'est l'intérêt même du champ.
    let provenance = mailcore::vectors::Provenance {
        model: "measure-scan".to_owned(),
        family: "vecteurs fabriqués".to_owned(),
        digest: "aucun-modele___".to_owned(),
        budget: 0,
        revision: 0,
        written: "banc".to_owned(),
    };
    let mut writer = VectorWriter::open(path, dim, &provenance).context("ouverture du magasin")?;
    let mut noise = Noise::new(0x5EED_4EC7_0000_0001);
    let mut vector = vec![0f32; dim];

    for index in 0..messages {
        match shape {
            Shape::Scattered => {
                for slot in &mut vector {
                    *slot = noise.unit();
                }
            }
            Shape::Rising => {
                // Un arc du plan porté par les deux premiers axes : l'angle à la requête
                // `(1, 0, …)` décroît avec le rang, donc le cosinus croît strictement.
                vector.fill(0.0);
                #[allow(clippy::cast_precision_loss)]
                let angle = std::f32::consts::FRAC_PI_2 * (1.0 - index as f32 / messages as f32);
                vector[0] = angle.cos();
                if dim > 1 {
                    vector[1] = angle.sin();
                }
            }
        }
        writer
            .append(MessageId(index as i64 + 1), &vector)
            .context("écriture d'un vecteur")?;
    }
    writer.sync().context("synchronisation du magasin")?;
    Ok(path.clone())
}

/// Chronomètre `queries` requêtes, et rend les durées triées.
fn latencies(
    vectors: &Vectors,
    dim: usize,
    limit: usize,
    queries: usize,
    shape: Shape,
) -> Result<Vec<Duration>> {
    let mut noise = Noise::new(0x0BAD_C0DE_0000_0001);
    let mut measured = Vec::with_capacity(queries);
    let mut query = vec![0f32; dim];

    for _ in 0..queries {
        match shape {
            Shape::Scattered => {
                for slot in &mut query {
                    *slot = noise.unit();
                }
            }
            // La requête qui fait travailler le tas au maximum : celle dont le corpus s'approche
            // en montant.
            Shape::Rising => {
                query.fill(0.0);
                query[0] = 1.0;
            }
        }

        let at = Instant::now();
        let hits = vectors.search(&query, limit).context("balayage")?;
        measured.push(at.elapsed());

        // Empêcher le compilateur de décider que le balayage ne sert à rien. Sans ça, un banc
        // qui jette son résultat mesure une boucle vide.
        anyhow::ensure!(
            hits.len() == limit.min(vectors.len()),
            "le balayage n'a pas rendu {limit} résultats"
        );
    }

    measured.sort_unstable();
    Ok(measured)
}

/// Écrit le relevé.
fn report(limit: usize, queries: usize, scattered: &[Duration], rising: &[Duration]) {
    println!(
        "{:<14} {:>11} {:>11} {:>11} {:>11}",
        "corpus", "p50", "p95", "p99", "pire"
    );
    for (name, measured) in [("épars", scattered), ("croissant", rising)] {
        println!(
            "{name:<14} {:>11} {:>11} {:>11} {:>11}",
            fmt(quantile(measured, 0.50)),
            fmt(quantile(measured, 0.95)),
            fmt(quantile(measured, 0.99)),
            fmt(measured.last().copied().unwrap_or_default())
        );
    }

    println!();
    println!(
        "Lecture : {queries} requêtes de {limit} résultats par ligne. « épars » est le cas\n\
         ordinaire ; « croissant » est le pire cas du tas borné, fabriqué pour que chaque\n\
         message batte le précédent et force une insertion. Le critère 2 accorde 50 ms au\n\
         trajet complet depuis le client — ce banc n'en mesure que le balayage, auquel il\n\
         faudra ajouter l'encodage de la requête et l'aller-retour JSON-RPC."
    );
}

/// Le quantile d'une série **déjà triée**.
fn quantile(sorted: &[Duration], q: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rank = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// Une durée, à la précision qui se lit.
fn fmt(d: Duration) -> String {
    format!("{d:.2?}")
}

/// La taille d'un fichier, ou zéro s'il n'est pas interrogeable.
fn file_size(path: &Utf8PathBuf) -> usize {
    std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or_default()
        .try_into()
        .unwrap_or(usize::MAX)
}

/// Un générateur déterministe — `splitmix64`.
///
/// Déterministe et non aléatoire : un banc dont les nombres changent d'une exécution à l'autre
/// mélange sa dispersion propre à celle de la machine, et c'est la dispersion de la machine
/// qu'on vient lire.
struct Noise(u64);

impl Noise {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Un flottant dans `[-1, 1)`.
    fn unit(&mut self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        {
            (self.next() >> 40) as f32 / 8_388_608.0 - 1.0
        }
    }
}

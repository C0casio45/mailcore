//! L'effet du texte des PDF joints sur la recherche, mesuré sur une réplique du corpus —
//! `cargo xtask pdf-effect`, `docs/PHASE-4.md`, entrée du 2026-10-20.
//!
//! ## La question
//!
//! Le texte de 1 330 messages change : leurs PDF entrent dans l'index plein texte et dans leurs
//! vecteurs. Est-ce que la recherche **régresse** sur le jeu de requêtes du critère 1 — qui ne vise
//! aucune pièce jointe, donc qui ne peut pas montrer un gain mais peut montrer une perte —, et que
//! coûtent l'index, sa mise à jour, la revectorisation et la latence ?
//!
//! ## Un témoin, et pas seulement un « avant »
//!
//! Comparer « avant » et « après » mêlerait deux effets : celui du texte des PDF, et celui d'une
//! réindexation et d'une revectorisation des mêmes messages — un document repris change les
//! statistiques de l'index, un vecteur réécrit s'ajoute au magasin. Le banc fait donc **deux
//! répliques au même état** : l'une lit les PDF (« après »), l'autre — le **témoin** — reprend
//! exactement les mêmes messages, par le même code, **sans** le texte des PDF. La comparaison
//! primaire est témoin contre après : deux côtés passés par les mêmes étapes, qui ne diffèrent que
//! par le texte. « Avant contre témoin » sert de contrôle : il doit ne rien changer.
//!
//! Et l'« avant » est **reconstruit par le code d'aujourd'hui** : le premier relevé a montré, par
//! ce contrôle même, que l'index du corpus datait d'un autre code de composition des documents.
//!
//! ## La règle, déclarée avant le premier relevé
//!
//! Imprimée en tête de chaque exécution — [`RULE`] —, et écrite dans le journal de la phase avant
//! la première. Le comptage est celui de `recall-models` : `recall::compare_states` n'ajoute
//! aucune règle de comptage, il relâche seulement la condition « même rang plein texte ».
//!
//! ## Ce qui n'est jamais écrit
//!
//! Rien sous `measurements/` : les deux répliques sont sous `target/measure-pdf-effect/`, leurs
//! blobs sont liés et lus en place — rien n'y est déposé —, et les rangs gardés, qui portent les
//! textes des requêtes, restent sous `target/`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use mailcore::{Mailbox, MessageId, Progress, Store};

use crate::measure::{RssSampler, build_queries};
use crate::merge::{Compared, index_size, merge_to_one, open_searcher, segments};
use crate::model::current_rss;
use crate::recall::{StateComparison, check_coverage, compare_states, same_rows};
use crate::semantic::{Replica, Sources, query_texts, release_only};
use crate::vectors::{Linked, copy_index, copy_tree, link_blobs, work_dir_in};

/// Des octets en Mio, pour l'affichage.
fn mib(bytes: u64) -> String {
    crate::model::mib(usize::try_from(bytes).unwrap_or(usize::MAX))
}

/// La règle, telle qu'elle a été écrite avant le premier relevé.
pub const RULE: &str = "\
Règle déclarée le 2026-10-20, avant le premier relevé
  Comparaison primaire : jeu v5 (30 requêtes), bras hybride, tout le jeu ; base = témoin (mêmes
  réindexation et revectorisation, sans le texte des PDF), candidat = après.
  Le critère 1 n'est pas régressé si les deux tiennent :
    (P1) le témoin n'est pas significativement meilleur — test de signe exact bilatéral,
         p < 0,05, au moins neuf requêtes décidées ;
    (P2) l'après a au plus une requête de moins que le témoin dans les dix premiers du bras
         hybride.
  Un verdict indécis ne prouve pas l'équivalence : il se dit « pas montré pire ».
  Relevé invalide, et non verdict : v5 avant ≠ v5 avant-bis, ou deux témoins faits par le même
  chemin — témoin et témoin-bis — rendent un verdict décidé sur le bras hybride.
  (Amendé le 2026-10-20 après le deuxième relevé, avant le troisième, pour cette seule condition :
  la précédente comparait avant et témoin, et réindexer n'est pas neutre dans tantivy — le nombre
  de mots est estimé à la fusion d'un segment qui porte des retraits, et un document repris passe
  en fin d'index. Ces artefacts sont communs au témoin et à l'après ; avant → témoin reste
  imprimé, comme diagnostic. P1, P2 et la latence ne changent pas.)
  Latence, à fragmentation égale (un segment des deux côtés) : p95 après ÷ p95 témoin ≤ 1,25 sur
  chacun des trois jeux, et p95 après < 50 ms.
  Exploratoire, rien n'y est décidé : le bras plein texte, les sous-ensembles, v3 et v4,
  avant → après. La taille de l'index est rapportée sans seuil.";

/// Le seuil de latence du critère 2 de la phase 4.
const SEARCH_BUDGET: Duration = Duration::from_millis(50);

/// Le rapport de latence au-delà duquel le texte des PDF coûte trop.
const LATENCY_RATIO: f64 = 1.25;

/// Ce que le banc copie.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// Le corpus, lu et jamais écrit.
    pub store: Utf8PathBuf,
    /// Ses vecteurs.
    pub vectors: Utf8PathBuf,
    /// Le modèle qui les a écrits.
    pub model: Utf8PathBuf,
    /// Le jeu primaire, v5.
    pub primary: Utf8PathBuf,
    /// Les jeux exploratoires.
    pub others: Vec<Utf8PathBuf>,
    /// Les phrases de latence historiques.
    pub phrases: Utf8PathBuf,
    /// L'exécutable `maild` qui lit les PDF.
    pub maild: Option<Utf8PathBuf>,
}

/// Le témoin : une copie de la réplique « après », faite avant qu'elle ne lise ses PDF. Le lien
/// des blobs est déclaré **avant** le répertoire, donc retiré avant lui — voir [`Linked`].
struct Control {
    _linked: Linked,
    root: Utf8PathBuf,
    _work: tempfile::TempDir,
}

/// Point d'entrée de `pdf-effect`.
///
/// # Errors
///
/// Si une entrée manque, si `maild` est plus vieux que les sources, si un garde refuse l'état d'une
/// réplique, ou si une passe échoue.
#[allow(clippy::too_many_lines)]
pub fn run(inputs: &Inputs) -> Result<()> {
    release_only("pdf-effect")?;
    let workspace = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("racine du workspace")?
        .to_owned();
    let program = fresh_maild(&workspace, inputs.maild.as_ref())?;
    for path in std::iter::once(&inputs.primary)
        .chain(&inputs.others)
        .chain(std::iter::once(&inputs.phrases))
    {
        anyhow::ensure!(path.is_file(), "jeu de requêtes introuvable : {path}");
    }
    println!("{RULE}\n");

    let base = workspace.join("target").join("measure-pdf-effect");
    let sources = Sources {
        store: inputs.store.clone(),
        vectors: inputs.vectors.clone(),
        model: inputs.model.clone(),
        maild: Some(program.clone()),
    };

    // 1. La réplique « après », au schéma du corpus, migrée à l'ouverture.
    let after = Replica::build(&sources, base.join("apres").as_str(), true)?;
    let a_root = after.root.clone();
    inside(&a_root, &base)?;
    println!(
        "Réplique          {a_root}  (schéma {} copié)",
        crate::guard::schema_of(&a_root.join("index.sqlite"))?.unwrap_or(0)
    );
    let opened = Instant::now();
    let store_a = Store::open(&a_root).context("ouverture de la réplique")?;
    println!(
        "Migration         {:.0} ms — ce que paie la première ouverture après la mise à jour",
        opened.elapsed().as_secs_f64() * 1000.0
    );
    let messages = store_a.message_count()?;

    // 2. L'état d'avant : rien de lu, rien en attente, des vecteurs du modèle donné.
    let model_dir = mailvec::bert::default_dir(&a_root);
    let bert = mailvec::bert::Bert::load(&model_dir).context("chargement du modèle")?;
    settled_before(&store_a, &a_root, &bert, messages)?;
    println!("État d'avant      {messages} messages, aucun PDF lu, rien à indexer ni à vectoriser");

    // 3. **L'index reconstruit par le code d'aujourd'hui**, puis un segment. L'index du corpus date
    // du 2026-09-23 à 00 h 56, d'avant la règle des formats gratuits et sa correction du même jour :
    // un document repris par le code d'aujourd'hui n'était donc pas celui de l'index copié, et le
    // premier relevé — le 2026-10-20 — l'a vu au contrôle : vingt-trois rangs plein texte déplacés
    // par une simple réindexation, à fréquences de mots égales. Un index d'un autre code n'est pas
    // un « avant ».
    let rebuilt = Instant::now();
    let fresh = mailcore::index::rebuild(&store_a, &Progress::new())?;
    println!(
        "Index reconstruit {} messages en {:.0} s, par le code d'aujourd'hui",
        fresh.indexed,
        rebuilt.elapsed().as_secs_f64()
    );
    let found = segments(&store_a)?;
    let merged = Instant::now();
    merge_to_one(&store_a)?;
    println!(
        "Index             {} segments fusionnés en un, {:.1} s ; {} documents",
        found.count,
        merged.elapsed().as_secs_f64(),
        found.documents
    );
    let size_before = index_size(&store_a)?;
    let tokens_before = body_tokens(&store_a)?;
    drop(store_a);

    // 4. Deux témoins, copiés de la réplique à cet état — jamais du corpus. Deux, parce que le
    // second mesure le bruit : deux répliques passées par le même chemin, sans rien d'autre de
    // différent.
    let blobs = sources.store.join("blobs");
    let control = control_of(&a_root, &blobs, &base, "temoin")?;
    let twin = control_of(&a_root, &blobs, &base, "temoin-bis")?;
    let (t_root, u_root) = (control.root.clone(), twin.root.clone());
    let store_a = Store::open(&a_root)?;
    let store_t = Store::open(&t_root)?;
    let store_u = Store::open(&u_root)?;
    let sa = segments(&store_a)?;
    for (root, store) in [(&t_root, &store_t), (&u_root, &store_u)] {
        settled_before(store, root, &bert, messages)?;
        let found = segments(store)?;
        anyhow::ensure!(
            sa.count == found.count && sa.documents == found.documents,
            "{root} n'est pas au même état : {} segments et {} documents contre {} et {}",
            found.count,
            found.documents,
            sa.count,
            sa.documents
        );
    }
    println!("Témoins           {t_root}\n                  {u_root}  (copiés de la réplique)");

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| it.as_secs());
    let ranks = base.join("ranks").join(stamp.to_string());
    std::fs::create_dir_all(&ranks).with_context(|| format!("création de {ranks}"))?;
    let sets: Vec<(String, Utf8PathBuf)> = std::iter::once(&inputs.primary)
        .chain(&inputs.others)
        .map(|path| (set_name(path), path.clone()))
        .collect();

    // 5. Les rangs d'avant, du jeu primaire seulement — les autres jeux ne se comparent qu'entre
    // témoin et après —, deux fois : la preuve que le relevé se reproduit.
    calm()?;
    let primary = &sets[0].0;
    rank(
        &a_root,
        &inputs.primary,
        &model_dir,
        &ranks.join(format!("{primary}-avant.json")),
    )?;
    rank(
        &a_root,
        &inputs.primary,
        &model_dir,
        &ranks.join(format!("{primary}-avant-bis.json")),
    )?;
    let reproducible = same_rows(
        &ranks.join(format!("{primary}-avant.json")),
        &ranks.join(format!("{primary}-avant-bis.json")),
    )?;

    // 6. La lecture des PDF, sur la réplique « après » seulement.
    let sqlite_before = sqlite_size(&a_root);
    let peak = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, done) = (Arc::clone(&peak), Arc::clone(&done));
        std::thread::spawn(move || crate::pdf::children_peak(&peak, &done))
    };
    let read = Instant::now();
    let caught = maild::attachments::catch_up(&a_root, program.as_std_path());
    let read_wall = read.elapsed();
    done.store(true, Ordering::SeqCst);
    let _ = sampler.join();
    let (marked, remarked) = caught
        .map_err(|failure| anyhow::anyhow!("lecture des PDF en échec : {}", failure.code()))?;
    let pending_index = store_a.unindexed_total()?;
    let pending_vectors = store_a.unvectorised_total()?;
    anyhow::ensure!(
        store_a.unscanned_total()? == 0,
        "la lecture n'a pas tout lu : {} messages restent",
        store_a.unscanned_total()?
    );
    anyhow::ensure!(
        remarked > 0 && pending_index == remarked && pending_vectors == remarked,
        "remises incohérentes : {remarked} remis, {pending_index} à indexer, {pending_vectors} à \
         vectoriser"
    );
    println!(
        "Lecture des PDF   {marked} messages lus, {remarked} remis, {:.1} s, crête d'un enfant {}",
        read_wall.as_secs_f64(),
        mib(peak.load(Ordering::SeqCst))
    );
    for (state, count) in store_a.pdf_counts()? {
        println!("  {:<16} {count}", format!("{state:?}"));
    }
    println!(
        "  base SQLite      {} → {}",
        mib(sqlite_before),
        mib(sqlite_size(&a_root))
    );
    let remarked_ids: Vec<MessageId> = store_a
        .unindexed(usize::MAX)?
        .into_iter()
        .map(|row| row.id)
        .collect();

    // 7. Les deux témoins reprennent exactement les mêmes messages, sans texte de PDF.
    for (root, store) in [(&t_root, &store_t), (&u_root, &store_u)] {
        mark_again(root, &remarked_ids)?;
        anyhow::ensure!(
            store.unindexed_total()? == remarked
                && store.unvectorised_total()? == remarked
                && store.pdf_counts()?.is_empty()
                && store.unscanned_total()? == messages,
            "{root} n'est pas dans l'état voulu après sa remise"
        );
    }

    // Le contrôle positif de la revectorisation : le texte composé change bien côté « après ».
    let budget = mailvec::text::Budget::default();
    let rows_a = store_a.unvectorised(usize::MAX)?;
    let rows_t = store_t.unvectorised(usize::MAX)?;
    let mut longer = 0usize;
    for (row_a, row_t) in rows_a.iter().zip(&rows_t) {
        anyhow::ensure!(
            row_a.id == row_t.id,
            "les deux côtés ne remettent pas les mêmes messages"
        );
        if let (mailvec::pass::Composed::Text(with), mailvec::pass::Composed::Text(without)) = (
            mailvec::pass::compose(&store_a, row_a, budget)?,
            mailvec::pass::compose(&store_t, row_t, budget)?,
        ) && with.len() > without.len()
        {
            longer += 1;
        }
    }
    println!(
        "Texte vectorisé   plus long côté « après » pour {longer} des {} messages remis",
        rows_a.len()
    );
    anyhow::ensure!(
        longer > 0,
        "aucun texte vectorisé n'a changé : le PDF n'y entre pas"
    );

    let sides = [
        ("témoin", &store_t),
        ("témoin-bis", &store_u),
        ("après", &store_a),
    ];

    // 8. La mise à jour de l'index — `Kind::Follow` —, des trois côtés.
    let mut text_bytes = [0u64; 3];
    for (slot, (name, store)) in sides.into_iter().enumerate() {
        let rest = current_rss();
        let sampler = RssSampler::start();
        let at = Instant::now();
        let stats = mailcore::index::update(store, &Progress::new())?;
        let wall = at.elapsed();
        let peak = sampler.stop();
        anyhow::ensure!(
            stats.indexed + stats.missing_blobs == remarked && store.unindexed_total()? == 0,
            "{name} : {} indexés, {} blobs absents, pour {remarked} remis",
            stats.indexed,
            stats.missing_blobs
        );
        text_bytes[slot] = stats.text_bytes;
        let left = segments(store)?;
        println!(
            "Index, {name:<10}  {} messages en {:.2} s, +{} de mémoire ; {} segments, {} retirés ; \
             {} de texte",
            stats.indexed,
            wall.as_secs_f64(),
            mib(peak.saturating_sub(rest)),
            left.count,
            left.deleted,
            mib(stats.text_bytes)
        );
    }
    anyhow::ensure!(
        text_bytes[0] == text_bytes[1] && text_bytes[2] > text_bytes[0],
        "textes indexés incohérents : témoins {} et {}, après {}",
        text_bytes[0],
        text_bytes[1],
        text_bytes[2]
    );

    // 9. La revectorisation, des trois côtés, avec le modèle déjà chargé.
    for (name, store) in sides {
        let lock = mailcore::vectors::VectorLock::try_acquire(&store.vectors_path())?;
        let rest = current_rss();
        let sampler = RssSampler::start();
        let at = Instant::now();
        let followed = mailvec::pass::follow_locked(store, &bert, budget, &Progress::new(), &lock)?;
        let wall = at.elapsed();
        let peak = sampler.stop();
        let mailvec::pass::Followed::Done(stats) = followed else {
            anyhow::bail!("{name} : la passe des vecteurs refuse — {followed:?}");
        };
        // `vectorised` compte déjà les messages sans texte — ils sont encodés quand même —, donc
        // `empty` ne s'y ajoute pas : additionné, un message vide se comptait deux fois.
        let accounted = stats.vectorised + stats.unreadable + stats.missing_blobs;
        anyhow::ensure!(
            accounted == remarked && store.unvectorised_total()? == 0,
            "{name} : {accounted} messages traités pour {remarked} remis"
        );
        println!(
            "Vecteurs, {name:<10}  {} messages en {:.1} s ({:.0} messages/s), dont {} sans \
             texte, +{} de mémoire",
            stats.vectorised,
            wall.as_secs_f64(),
            stats.vectorised as f64 / wall.as_secs_f64().max(f64::EPSILON),
            stats.empty,
            mib(peak.saturating_sub(rest))
        );
    }
    drop(bert);

    // 10. Fragmentation égale, puis taille — et le nombre de mots que l'index croit avoir : une
    // fusion d'un segment qui porte des retraits l'**estime** sur des longueurs quantifiées, et
    // c'est lui qui règle la normalisation par la longueur de BM25.
    for (_, store) in sides {
        merge_to_one(store)?;
    }
    let (size_t, size_a) = (index_size(&store_t)?, index_size(&store_a)?);
    println!(
        "Taille de l'index avant {} ; témoin {} ; après {} (+{}, pour +{} de texte indexé)",
        mib(size_before),
        mib(size_t),
        mib(size_a),
        mib(size_a.saturating_sub(size_t)),
        mib(text_bytes[2].saturating_sub(text_bytes[0]))
    );
    println!(
        "Mots du corps     avant {tokens_before} (exact) ; témoin {} ; témoin-bis {} ; après {} \
         (estimés à la fusion)",
        body_tokens(&store_t)?,
        body_tokens(&store_u)?,
        body_tokens(&store_a)?
    );
    println!(
        "Base SQLite       témoin {} ; après {}",
        mib(sqlite_size(&t_root)),
        mib(sqlite_size(&a_root))
    );

    // 11. La latence, côte à côte, à un segment chacun.
    calm()?;
    let latency_ok = latency(&store_t, &store_a, &inputs.phrases, &inputs.primary)?;

    // 12. Les rangs des témoins et d'après.
    rank(
        &u_root,
        &inputs.primary,
        &model_dir,
        &ranks.join(format!("{primary}-temoin-bis.json")),
    )?;
    for (name, path) in &sets {
        rank(
            &t_root,
            path,
            &model_dir,
            &ranks.join(format!("{name}-temoin.json")),
        )?;
        rank(
            &a_root,
            path,
            &model_dir,
            &ranks.join(format!("{name}-apres.json")),
        )?;
    }

    // 13. Les comparaisons. Le bruit d'abord : s'il décide, le reste ne se lit pas.
    println!("\n##### BRUIT — témoin contre témoin-bis, {primary} #####");
    let noise = compare_states(
        &ranks.join(format!("{primary}-temoin.json")),
        &ranks.join(format!("{primary}-temoin-bis.json")),
        ("témoin", "témoin-bis"),
    )?;
    println!("\n##### PRIMAIRE — témoin contre après, {primary} #####");
    let decided = compare_states(
        &ranks.join(format!("{primary}-temoin.json")),
        &ranks.join(format!("{primary}-apres.json")),
        ("témoin", "après"),
    )?;
    // **Le verdict tout de suite**, avant le diagnostic et l'exploratoire : une comparaison qui
    // échouerait ne doit pas le cacher.
    println!("\n{}", verdict(reproducible, &noise, &decided, latency_ok));
    println!(
        "\n##### DIAGNOSTIC — avant contre témoin, {primary} : ce que coûte une réindexation #####"
    );
    compare_states(
        &ranks.join(format!("{primary}-avant.json")),
        &ranks.join(format!("{primary}-temoin.json")),
        ("avant", "témoin"),
    )?;
    for (name, _) in sets.iter().skip(1) {
        println!("\n##### EXPLORATOIRE — témoin contre après, {name} #####");
        compare_states(
            &ranks.join(format!("{name}-temoin.json")),
            &ranks.join(format!("{name}-apres.json")),
            ("témoin", "après"),
        )?;
    }
    println!("\n##### EXPLORATOIRE — avant contre après, {primary} #####");
    compare_states(
        &ranks.join(format!("{primary}-avant.json")),
        &ranks.join(format!("{primary}-apres.json")),
        ("avant", "après"),
    )?;

    println!(
        "\nRangs gardés      {ranks}  (ils portent les textes des requêtes : jamais dans docs/)"
    );
    drop(store_a);
    drop(store_t);
    drop(store_u);
    drop(twin);
    drop(control);
    drop(after);
    Ok(())
}

/// `pdf-effect --latency-only` : la latence seule, témoin contre après, **trois fois de suite**
/// dans le même processus, chercheurs rouverts à chaque fois. Le banc complet l'avait mesurée
/// ×1,41, ×1,39 puis ×1,00 sur les mots seuls, d'un relevé à l'autre : un relevé qui ne se
/// reproduit pas ne se lit pas, et celui-ci dit si l'écart est dans l'index ou dans la machine.
///
/// # Errors
///
/// Comme [`run`].
pub fn latency_only(inputs: &Inputs) -> Result<()> {
    release_only("pdf-effect")?;
    let workspace = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("racine du workspace")?
        .to_owned();
    let program = fresh_maild(&workspace, inputs.maild.as_ref())?;
    let base = workspace.join("target").join("measure-pdf-latency");
    let sources = Sources {
        store: inputs.store.clone(),
        vectors: inputs.vectors.clone(),
        model: inputs.model.clone(),
        maild: Some(program.clone()),
    };
    let after = Replica::build(&sources, base.join("apres").as_str(), true)?;
    let a_root = after.root.clone();
    inside(&a_root, &base)?;
    let store_a = Store::open(&a_root)?;
    mailcore::index::rebuild(&store_a, &Progress::new())?;
    merge_to_one(&store_a)?;
    drop(store_a);
    let control = control_of(&a_root, &sources.store.join("blobs"), &base, "temoin")?;
    let t_root = control.root.clone();
    let store_a = Store::open(&a_root)?;
    let store_t = Store::open(&t_root)?;

    let (_, remarked) = maild::attachments::catch_up(&a_root, program.as_std_path())
        .map_err(|failure| anyhow::anyhow!("lecture des PDF en échec : {}", failure.code()))?;
    let ids: Vec<MessageId> = store_a
        .unindexed(usize::MAX)?
        .into_iter()
        .map(|row| row.id)
        .collect();
    mark_again(&t_root, &ids)?;
    for store in [&store_t, &store_a] {
        mailcore::index::update(store, &Progress::new())?;
        anyhow::ensure!(store.unindexed_total()? == 0, "l'index n'a pas tout repris");
        merge_to_one(store)?;
    }
    println!("Répliques         {remarked} messages remis ; un segment de chaque côté");
    let mut held = Vec::new();
    for attempt in 1..=3 {
        println!("\n===== mesure {attempt}/3 =====");
        calm()?;
        held.push(latency(
            &store_t,
            &store_a,
            &inputs.phrases,
            &inputs.primary,
        )?);
    }
    println!(
        "\nLatence           {} sur trois mesures",
        held.iter()
            .map(|ok| if *ok { "tenue" } else { "NON TENUE" })
            .collect::<Vec<_>>()
            .join(", ")
    );
    drop(store_a);
    drop(store_t);
    drop(control);
    drop(after);
    Ok(())
}

/// La conclusion, **composée après les décisions**, dans les mots de la règle.
fn verdict(
    reproducible: bool,
    noise: &StateComparison,
    decided: &StateComparison,
    latency_ok: bool,
) -> String {
    let mut lines =
        vec!["Verdict, selon la règle déclarée (amendée avant le troisième relevé)".to_owned()];
    let noisy = noise.base_better || noise.candidate_better;
    if !reproducible || noisy {
        lines.push(format!(
            "  RELEVÉ INVALIDE — {}",
            if reproducible {
                "deux témoins faits par le même chemin se départagent : le bruit décide".to_owned()
            } else {
                "le relevé d'avant ne se reproduit pas".to_owned()
            }
        ));
    } else {
        lines.push(format!(
            "  Bruit entre deux témoins : {} rangs plein texte, {} sens, {} hybride déplacés, aucun \
             verdict",
            noise.changed[0], noise.changed[1], noise.changed[2]
        ));
        let p1 = !decided.base_better;
        let p2 = decided.top10.1 + 1 >= decided.top10.0;
        lines.push(format!(
            "  (P1) témoin significativement meilleur : {} ",
            if p1 {
                "non — tenue"
            } else {
                "OUI — non tenue"
            }
        ));
        lines.push(format!(
            "  (P2) dans les dix premiers : témoin {}, après {} — {}",
            decided.top10.0,
            decided.top10.1,
            if p2 { "tenue" } else { "non tenue" }
        ));
        lines.push(format!(
            "  Critère 1 : {}",
            match (p1 && p2, decided.candidate_better) {
                (true, true) => "pas régressé — et l'après est significativement meilleur",
                (true, false) => "pas montré pire",
                (false, _) => "RÉGRESSÉ",
            }
        ));
    }
    lines.push(format!(
        "  Latence : {}",
        if latency_ok { "tenue" } else { "NON TENUE" }
    ));
    lines.join("\n")
}

/// Refuse un `maild` plus vieux que les sources qu'il exécute : il mesurerait le code d'hier.
fn fresh_maild(workspace: &Utf8Path, given: Option<&Utf8PathBuf>) -> Result<Utf8PathBuf> {
    let program = given.cloned().unwrap_or_else(|| {
        workspace
            .join("target")
            .join("release")
            .join(if cfg!(windows) { "maild.exe" } else { "maild" })
    });
    let built = std::fs::metadata(program.as_std_path())
        .and_then(|meta| meta.modified())
        .with_context(|| format!("{program} introuvable : cargo build --release -p maild"))?;
    // Les sources de **tous** les crates du dépôt et leurs manifestes, plus le verrou : une montée de
    // pdf-inspector ne touche que `Cargo.toml` et `Cargo.lock`, et c'est le cas le plus probable
    // d'un enfant qui n'exécute plus ce que le parent attend.
    let modified_at = |path: &Utf8Path| {
        std::fs::metadata(path.as_std_path())
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    };
    let mut newest =
        modified_at(&workspace.join("Cargo.toml")).max(modified_at(&workspace.join("Cargo.lock")));
    let mut stack = Vec::new();
    for entry in std::fs::read_dir(workspace.join("crates").as_std_path())
        .context("lecture de crates/")?
        .flatten()
    {
        let Ok(krate) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        newest = newest.max(modified_at(&krate.join("Cargo.toml")));
        if krate.join("src").is_dir() {
            stack.push(krate.join("src"));
        }
    }
    {
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir.as_std_path())
                .with_context(|| format!("lecture de {dir}"))?
                .flatten()
            {
                let meta = entry.metadata()?;
                if meta.is_dir() {
                    if let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) {
                        stack.push(path);
                    }
                } else if let Ok(modified) = meta.modified() {
                    newest = newest.max(modified);
                }
            }
        }
    }
    anyhow::ensure!(
        built >= newest,
        "{program} est plus vieux que ses sources : cargo build --release -p maild, puis relancer"
    );
    println!("Exécutable        {program} (plus récent que ses sources)");
    Ok(program)
}

/// Refuse une racine hors du répertoire de travail du banc : tout ce qui écrit ici doit écrire
/// sous `target/`.
fn inside(root: &Utf8Path, base: &Utf8Path) -> Result<()> {
    let root = std::fs::canonicalize(root.as_std_path())?;
    let base = std::fs::canonicalize(base.as_std_path())?;
    anyhow::ensure!(
        root.starts_with(&base),
        "{} n'est pas sous {} : refusé",
        root.display(),
        base.display()
    );
    Ok(())
}

/// L'état d'avant, vérifié : aucun PDF lu, rien à indexer ni à vectoriser, un index qui couvre le
/// store, et des vecteurs **de ce modèle** — le seul contrôle qui voit des vecteurs d'un autre
/// modèle sous le bon nombre de dimensions.
fn settled_before(
    store: &Store,
    root: &Utf8PathBuf,
    bert: &mailvec::bert::Bert,
    messages: u64,
) -> Result<()> {
    use mailvec::pass::Encoder;
    anyhow::ensure!(
        store.unscanned_total()? == messages && store.pdf_counts()?.is_empty(),
        "des PDF sont déjà lus dans {root} : ce n'est pas un état d'avant"
    );
    anyhow::ensure!(
        store.unindexed_total()? == 0 && store.unvectorised_total()? == 0,
        "{root} a {} messages à indexer et {} à vectoriser : un relevé y mentirait",
        store.unindexed_total()?,
        store.unvectorised_total()?
    );
    {
        let mailbox = Mailbox::open(root)?;
        check_coverage(&mailbox, root)?;
    }
    let identity = bert.identity();
    let planned = mailvec::pass::plan(
        store,
        mailvec::text::Budget::default(),
        Some((&identity, bert.dim())),
    )?;
    anyhow::ensure!(
        planned == mailvec::pass::Plan::Nothing,
        "les vecteurs de {root} ne sont pas ceux de ce modèle, ou pas à jour : {planned:?}"
    );
    Ok(())
}

/// Copie la réplique dans un témoin, à son état présent.
/// Chaque témoin a **son** répertoire de base : `work_dir_in` balaie les exécutions laissées dans le
/// sien, et un second témoin dans le même effacerait le premier.
fn control_of(
    from: &Utf8PathBuf,
    blobs: &Utf8PathBuf,
    base: &Utf8Path,
    name: &str,
) -> Result<Control> {
    let work = work_dir_in(base.join(name).as_std_path())?;
    let root = Utf8PathBuf::from_path_buf(work.path().to_path_buf())
        .map_err(|p| anyhow::anyhow!("chemin non UTF-8 : {}", p.display()))?;
    copy_index(from, &root)?;
    // **Avant** `Store::open`, qui créerait un `blobs/` vide : voir `link_blobs`.
    let linked = link_blobs(blobs, &root.join("blobs"))?;
    copy_tree(&from.join("search"), &root.join("search"))?;
    std::fs::copy(
        Store::vectors_path_in(from).as_std_path(),
        Store::vectors_path_in(&root).as_std_path(),
    )
    .context("copie du magasin de vecteurs")?;
    inside(&root, base)?;
    Ok(Control {
        _linked: linked,
        root,
        _work: work,
    })
}

/// Le nombre de mots du champ « corps » que l'index croit porter — la somme que BM25 divise par le
/// nombre de documents pour sa longueur moyenne. Exact pour un segment écrit d'un coup ;
/// **estimé** sur des longueurs quantifiées quand une fusion a porté des documents retirés
/// (`tantivy::indexer::merger`, 0.26).
fn body_tokens(store: &Store) -> Result<u64> {
    let (index, fields) = mailcore::index::open_or_create(store)?;
    let reader: tantivy::IndexReader = index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::Manual)
        .try_into()?;
    let searcher = reader.searcher();
    let mut total = 0u64;
    for segment in searcher.segment_readers() {
        total += segment.inverted_index(fields.body)?.total_num_tokens();
    }
    Ok(total)
}

/// Remet à indexer et à vectoriser exactement ces messages, dans le témoin.
fn mark_again(root: &Utf8Path, ids: &[MessageId]) -> Result<()> {
    let mut connection = rusqlite::Connection::open(root.join("index.sqlite").as_std_path())?;
    let tx = connection.transaction()?;
    for id in ids {
        tx.execute(
            "UPDATE messages SET indexed = 0, vectorised = 0 WHERE id = ?1",
            [id.0],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Un relevé du critère 1, gardé.
fn rank(
    root: &Utf8PathBuf,
    queries: &Utf8PathBuf,
    model: &Utf8Path,
    out: &Utf8PathBuf,
) -> Result<()> {
    println!("\n----- {root} · {queries} -----");
    crate::recall::measure(
        root,
        queries,
        None,
        Some(&model.to_owned()),
        mailcore::fusion::K,
        true,
        Some(out),
    )
}

/// La latence, témoin contre après, sur les trois jeux. Rend vrai si la règle tient.
fn latency(
    store_t: &Store,
    store_a: &Store,
    phrases: &Utf8PathBuf,
    primary: &Utf8PathBuf,
) -> Result<bool> {
    let open_index = |store: &Store| -> Result<mailcore::index::Searcher> {
        let (index, _) = mailcore::index::open_or_create(store)?;
        Ok(mailcore::index::Searcher::open(&index)?)
    };
    let searchers = [open_index(store_t)?, open_index(store_a)?];
    let documents = usize::try_from(segments(store_a)?.documents)?;
    let compared = Compared {
        stores: [store_t, store_a],
        searchers: &searchers,
        repeats: 4,
        all: documents + 10,
        segments: [1, 1],
        names: ["témoin", "après"],
        expect_gains: true,
    };
    let mut ok = true;
    for (label, queries) in [
        ("mots tirés des sujets", build_queries(store_t)?),
        ("phrases historiques", query_texts(phrases)?),
        ("requêtes du jeu primaire", query_texts(primary)?),
    ] {
        let [t, a] = compared.run(label, &queries);
        let ratio = a.as_secs_f64() / t.as_secs_f64().max(f64::EPSILON);
        let held = ratio <= LATENCY_RATIO && a < SEARCH_BUDGET;
        println!(
            "  p95 après ÷ témoin ×{ratio:.2}, après {:.2} ms — {}",
            a.as_secs_f64() * 1000.0,
            if held { "tenue" } else { "NON TENUE" }
        );
        ok &= held;
    }
    drop(searchers);
    let mut opens: [Vec<Duration>; 2] = [Vec::new(), Vec::new()];
    for round in 0..21 {
        for side in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
            let store = [store_t, store_a][side];
            let took = open_searcher(store)?;
            if round > 0 {
                opens[side].push(took);
            }
        }
    }
    println!(
        "Ouverture         p50 témoin {:.2} ms, après {:.2} ms",
        crate::semantic::percentile(&opens[0], 0.5).as_secs_f64() * 1000.0,
        crate::semantic::percentile(&opens[1], 0.5).as_secs_f64() * 1000.0
    );
    Ok(ok)
}

/// Attend que la machine retombe — dix secondes d'affilée sous 25 % —, au plus deux minutes, et
/// dit si elle ne l'a pas fait : la machine du quotidien n'est jamais au repos.
fn calm() -> Result<()> {
    let mut system = sysinfo::System::new();
    system.refresh_cpu_usage();
    let started = Instant::now();
    let mut quiet = 0u32;
    while quiet < 10 {
        std::thread::sleep(Duration::from_secs(1));
        system.refresh_cpu_usage();
        if system.global_cpu_usage() < 25.0 {
            quiet += 1;
        } else {
            quiet = 0;
        }
        if started.elapsed() > Duration::from_secs(120) {
            println!(
                "Machine           pas retombée sous 25 % en deux minutes ({:.0} %) : relevé fait \
                 quand même, à lire avec ça",
                system.global_cpu_usage()
            );
            return Ok(());
        }
    }
    println!(
        "Machine           retombée sous 25 % pendant dix secondes, après {:.0} s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// La taille de `index.sqlite`, **après un point de contrôle qui vide son journal**. Le WAL garde la
/// longueur de sa plus haute marée, et ses trames recopient des pages déjà reportées dans la base :
/// compté tel quel, il gonflait la croissance mesurée de plusieurs Mio — c'est ce que faisait
/// `pdf-catch-up` le 2026-10-20.
pub(crate) fn sqlite_size(root: &Utf8Path) -> u64 {
    let base = root.join("index.sqlite");
    if let Ok(connection) = rusqlite::Connection::open(base.as_std_path()) {
        let _ = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    }
    std::fs::metadata(base.as_std_path()).map_or(0, |meta| meta.len())
}

/// Le nom court d'un jeu : `phase4-queries-v5.toml` → `v5`.
fn set_name(path: &Utf8Path) -> String {
    let stem = path.file_stem().unwrap_or("jeu");
    stem.rsplit('-').next().unwrap_or(stem).to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_set_is_named_by_its_version() {
        assert_eq!(
            set_name(Utf8Path::new("measurements/phase4-queries-v5.toml")),
            "v5"
        );
        assert_eq!(set_name(Utf8Path::new("phase4-queries-v3.toml")), "v3");
    }

    /// Le verdict suit la règle : chaque condition, à sa limite.
    #[test]
    fn the_verdict_follows_the_declared_rule() {
        let quiet = StateComparison {
            base_better: false,
            candidate_better: false,
            top10: (10, 10),
            changed: [0, 0, 0],
        };
        assert!(verdict(true, &quiet, &quiet, true).contains("pas montré pire"));
        let one_less = StateComparison {
            top10: (10, 9),
            ..quiet
        };
        assert!(verdict(true, &quiet, &one_less, true).contains("pas montré pire"));
        let two_less = StateComparison {
            top10: (10, 8),
            ..quiet
        };
        assert!(verdict(true, &quiet, &two_less, true).contains("RÉGRESSÉ"));
        let worse = StateComparison {
            base_better: true,
            ..quiet
        };
        assert!(verdict(true, &quiet, &worse, true).contains("RÉGRESSÉ"));
        let better = StateComparison {
            candidate_better: true,
            ..quiet
        };
        assert!(verdict(true, &quiet, &better, true).contains("significativement meilleur"));
        // Le bruit invalide tout quand il **décide**, dans un sens ou dans l'autre — pas quand il
        // déplace des rangs sans verdict : c'est ce qu'il mesure.
        let moving = StateComparison {
            changed: [23, 0, 14],
            ..quiet
        };
        assert!(!verdict(true, &moving, &quiet, true).contains("INVALIDE"));
        for noisy in [
            StateComparison {
                base_better: true,
                ..quiet
            },
            StateComparison {
                candidate_better: true,
                ..quiet
            },
        ] {
            assert!(verdict(true, &noisy, &quiet, true).contains("INVALIDE"));
        }
        assert!(verdict(false, &quiet, &quiet, true).contains("INVALIDE"));
        assert!(verdict(true, &quiet, &quiet, false).contains("NON TENUE"));
    }
}

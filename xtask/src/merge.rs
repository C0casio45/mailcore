//! Ce que coûte et rapporte la fusion des segments de l'index plein texte.
//!
//! ## La question
//!
//! Jusqu'au 2026-10-18, les segments de l'index ne fusionnaient jamais : l'écrivain était lâché
//! sitôt validé, et tantivy abandonne alors ses fusions. Chaque paquet indexé laissait jusqu'à
//! huit segments de plus — un par fil d'indexation —, et une recherche les parcourt tous : 149
//! dans le store du quotidien, d'une seule réindexation. La correction attend les fusions à la fin de chaque passe : elle a un **prix**,
//! payé dans un job de fond, et un **gain**, à chaque ouverture de l'index et dans chaque
//! recherche. Ce banc chiffre les deux sur une réplique d'un store réel :
//!
//! 1. **la première arrivée** après la correction, qui fusionne tout ce qui s'est accumulé — sa
//!    durée, et ce qu'elle ajoute à la mémoire du processus ;
//! 2. **les arrivées suivantes**, une par passe, comme une moisson les apporte ;
//! 3. **l'ouverture de l'index**, avant et après : `Mailbox::open` la paie à chaque démarrage, et
//!    `reload_search` après chaque passe d'index — un `Searcher` ouvre chacun des segments ;
//! 4. **la recherche plein texte**, sur l'index d'avant et sur celui d'après, requête par requête
//!    et en alternant l'ordre — avec, pour chaque requête, le même nombre de résultats des deux
//!    côtés : une fusion qui perdrait des documents se verrait là. Deux jeux : des mots tirés des
//!    sujets, le cas le moins cher, et les phrases du critère 1, ce qu'on tape vraiment.
//!
//! ## Deux répliques, jamais le store
//!
//! Le banc écrit — des drapeaux, un index —, donc il travaille sur deux répliques sous `target/` :
//! `index.sqlite` et l'index copiés, les blobs liés et lus en place. **Une arrivée n'écrit aucun
//! blob** : les blobs de la réplique **sont** ceux du store d'origine, par un lien, et y déposer
//! un message neuf écrirait dans le store réel. Une arrivée est donc un message existant dont le
//! drapeau « indexé » est remis à zéro dans la copie : la passe relit son blob, retire son document
//! et le remet — le travail d'un message neuf, plus un retrait.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Progress, Store};

use crate::measure::{RssSampler, build_queries, run_once};
use crate::model::{current_rss, mib};
use crate::semantic::{percentile, query_texts, release_only};
use crate::vectors::{copy_index, copy_tree, link_blobs, work_dir_in};

/// Ce qu'un index dit de ses segments : leur nombre, et combien sont petits.
pub(crate) struct Segments {
    pub(crate) count: usize,
    pub(crate) small: usize,
    pub(crate) documents: u64,
    /// Les documents retirés que les segments portent encore — une réindexation en laisse un par
    /// message repris, jusqu'à la fusion suivante.
    pub(crate) deleted: u64,
}

/// En dessous, un segment est « petit » : c'est la taille d'un paquet de l'indexation.
const SMALL: u32 = 2_000;

/// Combien d'ouvertures de l'index par tour et par côté.
const OPENS: usize = 20;

/// Copie `index.sqlite` et l'index plein texte de `store` dans `dir`, et rend sa racine. Les
/// blobs se lient ensuite, **avant** `Store::open` — voir `link_blobs`.
fn replica(store: &Utf8PathBuf, dir: &std::path::Path) -> Result<Utf8PathBuf> {
    let root = Utf8PathBuf::from_path_buf(dir.to_path_buf())
        .map_err(|path| anyhow::anyhow!("chemin non UTF-8 : {}", path.display()))?;
    copy_index(store, &root)?;
    copy_tree(&store.join("search"), &root.join("search"))?;
    Ok(root)
}

pub(crate) fn segments(store: &Store) -> Result<Segments> {
    let (index, _) = mailcore::index::open_or_create(store).context("ouverture de l'index")?;
    let metas = index
        .searchable_segment_metas()
        .context("segments de l'index")?;
    Ok(Segments {
        count: metas.len(),
        small: metas.iter().filter(|meta| meta.max_doc() < SMALL).count(),
        documents: metas.iter().map(|meta| u64::from(meta.num_docs())).sum(),
        deleted: metas
            .iter()
            .map(|meta| u64::from(meta.num_deleted_docs()))
            .sum(),
    })
}

/// Combien de fichiers porte le répertoire de l'index : chaque segment en a plusieurs, et chacun
/// s'ouvre quand un `Searcher` se monte.
fn files(root: &Utf8PathBuf) -> Result<usize> {
    Ok(std::fs::read_dir(root.join("search").as_std_path())
        .context("répertoire de l'index")?
        .count())
}

/// Ce que `Mailbox::open` et `Mailbox::reload_search` font de l'index, et rien d'autre.
pub(crate) fn open_searcher(store: &Store) -> Result<Duration> {
    let at = Instant::now();
    let (index, _) = mailcore::index::open_or_create(store).context("ouverture de l'index")?;
    let searcher = mailcore::index::Searcher::open(&index).context("ouverture du chercheur")?;
    let elapsed = at.elapsed();
    drop(searcher);
    Ok(elapsed)
}

/// Fusionne tout l'index en **un seul segment sans document retiré**, et attend que les fichiers
/// des anciens segments soient effacés : c'est l'état où deux index se comparent — à
/// fragmentation égale —, et ce que deux relevés d'un même index ne garantissent pas.
///
/// **Aucun lecteur ne doit être ouvert** sur cet index : sous Windows, un fichier projeté par un
/// `Searcher` vivant ne s'efface pas, et la taille mesurée compterait des segments morts.
pub(crate) fn merge_to_one(store: &Store) -> Result<()> {
    let (index, _) = mailcore::index::open_or_create(store).context("ouverture de l'index")?;
    let before = segments(store)?;
    if before.count > 1 || before.deleted > 0 {
        let ids = index
            .searchable_segment_ids()
            .context("segments de l'index")?;
        let mut writer: tantivy::IndexWriter = index
            .writer(128 * 1024 * 1024)
            .context("écrivain de l'index")?;
        writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
        writer.merge(&ids).wait().context("fusion des segments")?;
        writer
            .garbage_collect_files()
            .wait()
            .context("effacement des anciens segments")?;
        writer.wait_merging_threads().context("fin des fusions")?;
    }
    let after = segments(store)?;
    anyhow::ensure!(
        after.count == 1 && after.deleted == 0 && after.documents == before.documents,
        "fusion incomplète : {} segments, {} documents retirés, {} documents contre {} avant",
        after.count,
        after.deleted,
        after.documents,
        before.documents
    );
    Ok(())
}

/// La taille de l'index plein texte, comptée sur **les fichiers que ses segments vivants
/// déclarent** et sur `meta.json` — jamais en parcourant le répertoire, où un segment mort qui n'a
/// pas pu être effacé compterait.
pub(crate) fn index_size(store: &Store) -> Result<u64> {
    let (index, _) = mailcore::index::open_or_create(store).context("ouverture de l'index")?;
    let dir = store.root().join("search");
    let size = |name: &std::path::Path| {
        std::fs::metadata(dir.as_std_path().join(name)).map_or(0, |meta| meta.len())
    };
    let mut total = size(std::path::Path::new("meta.json"));
    for meta in index
        .searchable_segment_metas()
        .context("segments de l'index")?
    {
        for file in meta.list_files() {
            total += size(&file);
        }
    }
    Ok(total)
}

/// Point d'entrée de `measure-merge`.
///
/// # Errors
///
/// Si le store est introuvable, si une réplique ne peut pas être faite, si une passe échoue, ou si
/// le jeu de phrases est illisible.
pub fn measure(
    store: &Utf8PathBuf,
    arrivals: usize,
    repeats: usize,
    phrases: Option<&Utf8PathBuf>,
) -> Result<()> {
    release_only("measure-merge")?;
    // Tantivy dit ses fusions ratées par `log`, pas par une erreur rendue : sans abonné, une
    // fusion jetée ne laisse aucune trace. `RUST_LOG=tantivy=info` montre aussi celles qui
    // aboutissent.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
    anyhow::ensure!(
        arrivals >= 2,
        "au moins deux arrivées : la première fusionne l'arriéré"
    );
    anyhow::ensure!(repeats >= 2, "au moins deux tours : le premier est jeté");

    // Chaque réplique dans son propre répertoire jetable, le lien des blobs à sa racine : c'est
    // là que `work_dir_in` cherche, au départ du banc suivant, les liens qu'une exécution tuée
    // aurait laissés, pour les retirer avant d'effacer. Un lien plus profond lui échapperait, et
    // l'effacement passerait par le corpus — la première version les mettait un cran plus bas ;
    // trouvé par une relecture le 2026-10-18. Le répertoire d'abord, le lien ensuite : détruits
    // dans l'ordre inverse, voir `Linked`.
    let before_work = work_dir_in(&std::path::absolute("target/measure-merge/avant")?)?;
    let before_root = replica(store, before_work.path())?;
    let _before_link = link_blobs(&store.join("blobs"), &before_root.join("blobs"))?;
    let after_work = work_dir_in(&std::path::absolute("target/measure-merge/apres")?)?;
    let after_root = replica(store, after_work.path())?;
    let _after_link = link_blobs(&store.join("blobs"), &after_root.join("blobs"))?;

    // Les arrivées : les messages **les plus anciens**, déjà indexés. Remis à indexer, chacun est
    // d'abord retiré de l'index, ce qu'un message neuf n'est pas ; les plus récents vivent dans
    // les petits segments d'une moisson, et retirer le seul document d'un segment le fait
    // disparaître au `commit` — tantivy jette alors la fusion qui le comptait (« couldn't find
    // segment in SegmentManager ») et la refait à la passe suivante. Vu au premier relevé du store
    // du quotidien, le 2026-10-18 : la fusion de l'arriéré y était comptée deux fois. Les plus
    // anciens vivent dans les segments de la reconstruction — des centaines de documents chacun —,
    // qu'un retrait ne vide pas ; et passé la première arrivée, qui fusionne tout, les suivantes
    // retirent d'un segment fusionné.
    let ids: Vec<i64> = {
        let db = rusqlite::Connection::open(after_root.join("index.sqlite").as_std_path())
            .context("copie de index.sqlite")?;
        let mut statement =
            db.prepare("SELECT id FROM messages WHERE indexed = 1 ORDER BY id ASC LIMIT ?1")?;
        let rows = statement.query_map([i64::try_from(arrivals)?], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    anyhow::ensure!(ids.len() == arrivals, "pas assez de messages indexés");

    let before = Store::open(&before_root).context("réplique d'avant")?;
    let after = Store::open(&after_root).context("réplique d'après")?;
    let found = segments(&after)?;
    println!("Store             {store}");
    println!(
        "Segments trouvés  {} dont {} de moins de {SMALL} documents, {} documents",
        found.count, found.small, found.documents
    );

    let arrive = |id: i64| -> Result<Duration> {
        let db = rusqlite::Connection::open(after_root.join("index.sqlite").as_std_path())?;
        db.execute("UPDATE messages SET indexed = 0 WHERE id = ?1", [id])?;
        drop(db);
        let at = Instant::now();
        let stats = mailcore::index::update(&after, &Progress::new()).context("passe d'index")?;
        let elapsed = at.elapsed();
        anyhow::ensure!(
            stats.indexed == 1,
            "{} messages indexés, 1 attendu",
            stats.indexed
        );
        Ok(elapsed)
    };

    // --- 1. La première arrivée : elle fusionne l'arriéré. ---
    let resting = current_rss();
    let sampler = RssSampler::start();
    let first = arrive(ids[0])?;
    let crest = sampler.stop();
    let merged = segments(&after)?;
    println!(
        "\nPremière arrivée  {:.1} ms, fusion de l'arriéré comprise",
        ms(first)
    );
    println!(
        "  mémoire         {} avant, {} de crête : la fusion ajoute {}",
        mib(usize::try_from(resting)?),
        mib(usize::try_from(crest)?),
        mib(usize::try_from(crest.saturating_sub(resting))?)
    );
    println!(
        "  segments        {} → {} dont {} petits, {} documents",
        found.count, merged.count, merged.small, merged.documents
    );
    anyhow::ensure!(
        merged.documents == found.documents,
        "la fusion a changé le nombre de documents : {} → {}",
        found.documents,
        merged.documents
    );

    // --- 2. Les arrivées suivantes, chacune avec les segments qu'elle laisse : une arrivée lente
    // qui en laisse moins que la précédente est celle qui a fusionné. ---
    let mut following = Vec::with_capacity(arrivals - 1);
    let mut detail = Vec::with_capacity(arrivals - 1);
    for id in &ids[1..] {
        let elapsed = arrive(*id)?;
        following.push(elapsed);
        detail.push(format!(
            "{:.1} ms → {}",
            ms(elapsed),
            segments(&after)?.count
        ));
    }
    let settled = segments(&after)?;
    println!(
        "\nArrivées suivantes  {} : médiane {:.1} ms, max {:.1} ms ; segments ensuite : {}",
        following.len(),
        ms(percentile(&following, 0.5)),
        ms(percentile(&following, 1.0)),
        settled.count
    );
    println!("  une à une       {}", detail.join(", "));

    // --- 3. L'ouverture de l'index, avant et après, en alternant. ---
    let stores = [&before, &after];
    let mut opens: [Vec<Duration>; 2] = [Vec::new(), Vec::new()];
    for round in 0..repeats {
        for turn in 0..OPENS {
            let order = if (turn + round) % 2 == 0 {
                [0, 1]
            } else {
                [1, 0]
            };
            for side in order {
                let elapsed = open_searcher(stores[side])?;
                if round > 0 {
                    opens[side].push(elapsed);
                }
            }
        }
    }
    println!(
        "\nOuverture de l'index  {OPENS} × {repeats} tours, le premier jeté, l'ordre alterné \
         — ce que paient un démarrage et chaque fin de passe d'index"
    );
    println!(
        "                  avant ({} fichiers)   après ({})",
        files(&before_root)?,
        files(&after_root)?
    );
    for (label, q) in [("p50", 0.5), ("p95", 0.95), ("max", 1.0)] {
        println!(
            "  {label}             {:>8.2} ms          {:>8.2} ms",
            ms(percentile(&opens[0], q)),
            ms(percentile(&opens[1], q))
        );
    }

    // --- 4. La recherche, avant et après, requêtes alternées : des mots, puis des phrases. ---
    let (before_index, _) = mailcore::index::open_or_create(&before).context("index d'avant")?;
    let (after_index, _) = mailcore::index::open_or_create(&after).context("index d'après")?;
    let searchers = [
        mailcore::index::Searcher::open(&before_index).context("chercheur d'avant")?,
        mailcore::index::Searcher::open(&after_index).context("chercheur d'après")?,
    ];
    let compared = Compared {
        stores,
        searchers: &searchers,
        repeats,
        all: usize::try_from(found.documents)?.saturating_add(arrivals),
        segments: [found.count, settled.count],
        names: ["avant", "après"],
        expect_gains: false,
    };
    compared.run("mots tirés des sujets", &build_queries(&before)?);
    // Les mots seuls sont le cas le moins cher : un terme, une liste de postings. Ce qu'on tape
    // dans la barre depuis que le sens y répond, ce sont des phrases — huit mots, dont « le » et
    // « de », qui ont chacun une liste de dizaines de milliers de documents.
    if let Some(phrases) = phrases {
        compared.run("phrases du critère 1", &query_texts(phrases)?);
    }
    Ok(())
}

/// Deux index ouverts côte à côte, et de quoi comparer une recherche sur l'un et sur l'autre.
pub(crate) struct Compared<'a> {
    pub(crate) stores: [&'a Store; 2],
    pub(crate) searchers: &'a [mailcore::index::Searcher; 2],
    pub(crate) repeats: usize,
    /// Assez pour qu'aucune requête ne soit coupée.
    pub(crate) all: usize,
    pub(crate) segments: [usize; 2],
    /// Le nom de chaque côté.
    pub(crate) names: [&'a str; 2],
    /// Le second côté **doit** rendre au moins autant de résultats que le premier — du texte
    /// ajouté —, et seuls les comptes sont imprimés, jamais les requêtes. Faux : les deux côtés
    /// doivent rendre autant, et chaque écart est imprimé.
    pub(crate) expect_gains: bool,
}

impl Compared<'_> {
    pub(crate) fn run(&self, label: &str, queries: &[String]) -> [Duration; 2] {
        let Self {
            stores,
            searchers,
            repeats,
            all,
            segments,
            names,
            expect_gains,
        } = self;
        println!(
            "\nRecherche, {label}  {} requêtes × {repeats} tours, le premier jeté, l'ordre alterné",
            queries.len()
        );

        // Le même nombre de résultats des deux côtés, compté **sans plafond** : la page du banc en
        // rend cinquante, et une requête fréquente y serait égale des deux côtés quoi que la
        // fusion ait perdu. Hors chronomètre.
        let mut differing = Vec::new();
        let (mut gained, mut lost, mut negated) = (0usize, 0usize, 0usize);
        let mut refused = 0usize;
        for query in queries {
            let counts = searchers
                .each_ref()
                .map(|searcher| searcher.search(query, *all).map(|hits| hits.len()).ok());
            // Une exclusion — `facture -spam` — peut perdre des résultats quand du texte s'ajoute :
            // le mot exclu peut y être. Elle ne dit donc rien de la règle « rien ne se perd ».
            let excludes = query.starts_with('-') || query.contains(" -") || query.contains("NOT ");
            match counts {
                [None, None] => refused += 1,
                [Some(_), Some(_)] if *expect_gains && excludes => negated += 1,
                [Some(first), Some(second)] if *expect_gains && second > first => gained += 1,
                [Some(first), Some(second)] if *expect_gains && second < first => lost += 1,
                [first, second] if first != second => {
                    if *expect_gains {
                        lost += 1;
                    } else {
                        differing.push(format!(
                            "{query} : {first:?} {}, {second:?} {}",
                            names[0], names[1]
                        ));
                    }
                }
                _ => {}
            }
        }

        // Les latences **par tour**, pour lire la dispersion avant la médiane.
        let mut rounds: [Vec<Vec<Duration>>; 2] = [Vec::new(), Vec::new()];
        for round in 0..*repeats {
            let mut this: [Vec<Duration>; 2] = [Vec::new(), Vec::new()];
            for (index, query) in queries.iter().enumerate() {
                let order = if (index + round) % 2 == 0 {
                    [0, 1]
                } else {
                    [1, 0]
                };
                for side in order {
                    let at = Instant::now();
                    // Une requête refusée l'est des deux côtés — comptée plus haut — et n'entre
                    // pas dans les latences, comme dans `measure-search`.
                    let answered = run_once(stores[side], &searchers[side], query).is_ok();
                    if answered {
                        this[side].push(at.elapsed());
                    }
                }
            }
            if round > 0 {
                for side in 0..2 {
                    rounds[side].push(std::mem::take(&mut this[side]));
                }
            }
        }
        let latencies: [Vec<Duration>; 2] = rounds
            .each_ref()
            .map(|kept| kept.iter().flatten().copied().collect());
        let spread = |kept: &[Vec<Duration>]| {
            let p95s: Vec<f64> = kept
                .iter()
                .map(|round| ms(percentile(round, 0.95)))
                .collect();
            let (low, high) = p95s
                .iter()
                .fold((f64::MAX, 0.0_f64), |(l, h), x| (l.min(*x), h.max(*x)));
            if high > 0.0 { (high - low) / high } else { 0.0 }
        };
        println!(
            "                  {} ({} segments)   {} ({})",
            names[0], segments[0], names[1], segments[1]
        );
        println!(
            "  dispersion p95   {:>8.2}             {:>8.2}      (max − min) / max entre tours",
            spread(&rounds[0]),
            spread(&rounds[1])
        );
        for (label, q) in [("p50", 0.5), ("p95", 0.95), ("max", 1.0)] {
            println!(
                "  {label}             {:>8.2} ms          {:>8.2} ms",
                ms(percentile(&latencies[0], q)),
                ms(percentile(&latencies[1], q))
            );
        }
        if refused > 0 {
            println!("  {refused} requêtes refusées des deux côtés — leur syntaxe, pas l'index");
        }
        if *expect_gains {
            println!(
                "  {gained} requêtes rendent plus de résultats côté « {} », {lost} en rendent moins{}{}",
                names[1],
                if negated > 0 {
                    format!(" ; {negated} à exclusion, hors de ce compte")
                } else {
                    String::new()
                },
                if lost > 0 {
                    " — ANOMALIE : du texte ajouté ne retire aucun document"
                } else {
                    ""
                }
            );
        } else if differing.is_empty() {
            println!(
                "  mêmes nombres de résultats des deux côtés, sans plafond, pour chaque requête"
            );
        } else {
            println!(
                "  {} requêtes rendent un autre nombre de résultats :",
                differing.len()
            );
            for line in &differing {
                println!("    {line}");
            }
        }
        [
            percentile(&latencies[0], 0.95),
            percentile(&latencies[1], 0.95),
        ]
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

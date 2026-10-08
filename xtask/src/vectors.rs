//! Le banc des critères 3 et 5 de `docs/PHASE-4.md` : le coût de la passe de vectorisation.
//!
//! ## Il écrit, donc il ne prend pas de `--store` sans filet
//!
//! Il en prend un, parce qu'une passe ne veut rien dire sur un corpus fabriqué — c'est tout le
//! contraire de `measure-scan`, dont la latence ne dépend pas de ce que les nombres signifient.
//! Mais il **copie** le store avant d'y toucher : `measure-attachment` a été visée sur les
//! données de production le 2026-09-09 et y a laissé 533 Mo, et la parade retenue alors était
//! qu'un paramètre qui peut désigner le store réel le désignera.
//!
//! La copie ne porte que `index.sqlite` : les blobs sont lus, jamais écrits, donc ils restent où
//! ils sont et le banc les lit en place. Ce qui est copié est exactement ce que la passe modifie
//! — les drapeaux —, et le magasin de vecteurs est créé dans la copie.
//!
//! ## Ce qu'il mesure, et dans quel ordre
//!
//! Le critère 5 demande que la mesure dise **où** va la mémoire. Les paliers sont donc relevés
//! un par un, comme pour `model-bench` : au démarrage, store ouvert, modèle chargé, passe finie.
//! C'est ce relevé-là qui avait désigné le tokeniseur plutôt que la table, le 2026-09-15.
//!
//! ## Des crêtes échantillonnées, et plus seulement des paliers
//!
//! Jusqu'au 2026-09-25, la « crête » était le plus grand de **deux relevés ponctuels** — modèle
//! chargé, passe finie. Ce qui se passe entre les deux lui échappait : le chargement d'un
//! transformeur double brièvement ses poids, et un lot d'attention se libère avant la fin de la
//! passe. Aucun des deux ne se voit dans un relevé pris après coup. Le chargement et la passe
//! ont donc chacun leur échantillonneur, celui de `measure-import`, toutes les 20 ms.
//!
//! Et le chiffre de 1 304 Mio du 2026-09-22 ne sortait d'aucun instrument du dépôt : ce banc ne
//! savait charger que l'embedding statique. `--bert` le rend remesurable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Progress, Store};
use mailvec::pass::Encoder;

use crate::measure::{RSS_LIMIT, RssSampler};
use crate::model::{current_rss, mib};

/// Ce que le banc doit mesurer.
#[derive(Debug, Clone)]
pub struct Options {
    /// Le store d'origine, lu et jamais écrit.
    pub store: Utf8PathBuf,
    /// Le répertoire du modèle.
    pub model: Utf8PathBuf,
    /// Le transformeur plutôt que l'embedding statique.
    ///
    /// Un drapeau et non une déduction depuis le contenu du répertoire : la famille se choisit,
    /// elle ne se devine pas — la même règle que `mail vectors`.
    pub bert: bool,
    /// Arrêter la passe après ce nombre de messages.
    ///
    /// La passe transformeur complète dure de trois à quatre heures. Une passe bornée sert à
    /// comparer deux réglages, **pas à trancher le critère 5** : l'encodage d'un lot est borné
    /// par construction, mais la composition du texte dépend de la taille du message, et les
    /// premiers messages du store ne sont pas les plus gros. Le banc le dit à côté de la crête —
    /// il l'affirmait le contraire jusqu'à ce qu'une relecture le réfute, le 2026-09-25.
    pub limit: Option<u64>,
    /// Enchaîner le contrôle de reprise.
    pub resume: bool,
    /// Où garder le magasin de vecteurs produit, une fois la passe finie.
    ///
    /// Sans lui, le magasin disparaît avec la copie jetable, et mesurer le rappel du modèle mesuré
    /// demanderait une seconde passe de plusieurs heures. **Un fichier neuf** : refusé s'il existe,
    /// pour qu'un banc ne puisse pas écraser le magasin d'un store — la règle de
    /// `measure-attachment`, appliquée à la sortie.
    pub keep: Option<Utf8PathBuf>,
}

/// Les relevés pris avant que le modèle ne soit chargé.
#[derive(Debug, Clone, Copy)]
struct Before {
    start: u64,
    store: u64,
}

/// Mesure une passe : débit, durée, mémoire par palier et par crête, et reprise.
///
/// # Errors
///
/// Si le store n'est pas copiable ou lisible, si le modèle n'est pas chargeable, ou si la passe
/// échoue.
pub fn measure(options: &Options) -> Result<()> {
    // Le même refus que `measure-scan`, et la même raison : la passe passe son temps dans
    // `mailvec` et `mailcore`, deux membres de l'espace de travail que le profil de débogage
    // n'optimise pas. Un débit relevé là serait faux d'un facteur qu'on ne connaît pas.
    if cfg!(debug_assertions) {
        anyhow::bail!(
            "measure-vectors chronomètre du code de `mailvec` et `mailcore`, qui ne sont pas \
             optimisés en profil de débogage — le relevé serait faux.\n\
             Lancez : cargo run --release -p xtask -- measure-vectors"
        );
    }

    // `--keep` vérifié **avant** la passe : un chemin refusé au bout de quatre heures perdait la
    // passe avec la copie jetable qui la portait.
    if let Some(keep) = &options.keep {
        check_keep(keep, &options.store)?;
    }

    let rss_start = current_rss();
    let work = work_dir().context("répertoire de travail")?;
    let root = Utf8PathBuf::from_path_buf(work.path().to_path_buf())
        .map_err(|p| anyhow::anyhow!("chemin non UTF-8 : {}", p.display()))?;

    copy_index(&options.store, &root)?;

    // Les blobs du store d'origine, lus en place. La copie n'en a pas, et les recopier
    // coûterait plusieurs gigaoctets pour une passe qui ne fait que les lire. **Avant**
    // l'ouverture du store : voir `link_blobs`.
    let blobs = options.store.join("blobs");
    if !blobs.is_dir() {
        anyhow::bail!("le store n'a pas de répertoire `blobs` : {blobs}");
    }
    let _linked = link_blobs(&blobs, &root.join("blobs"))?;

    let store = Store::open(&root).context("ouverture de la copie")?;
    let rss_store = current_rss();

    // **Une passe depuis zéro, dans la copie.** Le corpus de mesure est déjà vectorisé : ses
    // drapeaux disent « fait », et le banc refusait jusqu'ici de tourner dessus — « rien à
    // vectoriser ». Ce qui se mesure ici est le coût d'une passe, donc ils sont remis à zéro,
    // **dans la copie** : l'original n'est pas ouvert, il n'y a rien à y remettre.
    store
        .reset_vectorised()
        .context("remise des drapeaux de la copie")?;
    let total = store.unvectorised_total().context("compte des messages")?;
    println!("Store             {}", options.store);
    println!("Modèle            {}", options.model);
    println!(
        "Famille           {}",
        if options.bert {
            "transformeur"
        } else {
            "embedding statique"
        }
    );
    match options.limit {
        Some(limit) => println!("À vectoriser      {total} messages, arrêt après {limit}\n"),
        None => println!("À vectoriser      {total} messages\n"),
    }
    anyhow::ensure!(total > 0, "rien à vectoriser : le store est-il vide ?");

    let before = Before {
        start: rss_start,
        store: rss_store,
    };
    if options.bert {
        run(options, &store, total, before, || {
            mailvec::bert::Bert::load(&options.model)
        })
    } else {
        run(options, &store, total, before, || {
            mailvec::model::Model::load(&options.model)
        })
    }
}

/// Charge le modèle, fait la passe, relève tout.
///
/// Générique plutôt qu'un `Box<dyn Encoder>` : la passe prend un `&impl Encoder`, et c'est
/// l'appel que le produit fait. Un banc qui passerait par une autre porte mesurerait autre chose.
fn run<E: Encoder>(
    options: &Options,
    store: &Store,
    total: u64,
    before: Before,
    load: impl FnOnce() -> mailvec::Result<E>,
) -> Result<()> {
    let sampler = RssSampler::start();
    let loading = Instant::now();
    let encoder = load().context("chargement du modèle")?;
    let load_time = loading.elapsed();
    let load_peak = sampler.stop();
    let rss_model = current_rss();

    let progress = Progress::new();
    let sampler = RssSampler::start();
    let at = Instant::now();
    let stats = bounded(options.limit, &progress, || {
        mailvec::pass::update_with(store, &encoder, mailvec::text::Budget::default(), &progress)
    })
    .context("passe")?;
    let elapsed = at.elapsed();
    let pass_peak = sampler.stop();
    let rss_done = current_rss();

    report(&Reading {
        stats,
        elapsed,
        load: load_time,
        dim: encoder.dim(),
        batch: encoder.batch(),
        total,
        limited: options.limit.is_some(),
        start: before.start,
        store: before.store,
        load_peak,
        model: rss_model,
        pass_peak,
        done: rss_done,
    });

    if let Some(keep) = &options.keep {
        keep_vectors(store, keep)?;
    }

    if options.resume {
        measure_resume(store, &encoder)?;
    }
    Ok(())
}

/// Où le banc travaille : un répertoire à lui, sous `target/measure-vectors/`, et non le
/// répertoire temporaire du système.
///
/// ## Pourquoi un endroit connu
///
/// Une exécution interrompue — `Ctrl-C`, un processus tué — ne détruit rien : ni le lien vers les
/// blobs du corpus, ni la copie de `index.sqlite`, qui porte les sujets et les adresses du courrier
/// réel. Deux liens de ce genre ont été retrouvés dans `%TEMP%` le 2026-09-25, laissés par des
/// mesures arrêtées ; un nettoyeur qui les aurait suivis aurait vidé le corpus. Dans un endroit
/// connu, chaque exécution **balaie celles d'avant** en commençant : les liens d'abord, par
/// [`unlink`], puis le reste.
fn work_dir() -> Result<tempfile::TempDir> {
    let base = std::path::absolute("target/measure-vectors").context("chemin de travail")?;
    work_dir_in(&base)
}

/// [`work_dir`] sous une base donnée — pour que le balayage se teste ailleurs que sur les vraies
/// exécutions.
pub(crate) fn work_dir_in(base: &std::path::Path) -> Result<tempfile::TempDir> {
    let base = base.to_path_buf();
    std::fs::create_dir_all(&base).with_context(|| format!("création de {}", base.display()))?;
    for entry in
        std::fs::read_dir(&base).with_context(|| format!("lecture de {}", base.display()))?
    {
        let entry = entry?;
        let Ok(dir) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        if !dir.file_name().is_some_and(|name| name.starts_with("run-")) {
            continue;
        }
        let blobs = dir.join("blobs");
        if std::fs::symlink_metadata(blobs.as_std_path())
            .is_ok_and(|meta| !meta.is_dir() || is_link(&blobs))
        {
            unlink(&blobs)
                .with_context(|| format!("lien laissé par une exécution interrompue : {blobs}"))?;
        }
        println!("Exécution interrompue balayée : {dir}");
        std::fs::remove_dir_all(dir.as_std_path()).with_context(|| format!("balayage de {dir}"))?;
    }
    tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(&base)
        .context("répertoire jetable")
}

/// Vrai si `path` est un lien de répertoire ou une jonction, et non un vrai répertoire.
fn is_link(path: &Utf8PathBuf) -> bool {
    std::fs::read_link(path.as_std_path()).is_ok()
}

/// Refuse un `--keep` qui existe, dont le parent n'existe pas, ou qui tomberait dans le store.
fn check_keep(keep: &Utf8PathBuf, store: &Utf8PathBuf) -> Result<()> {
    anyhow::ensure!(
        !keep.exists(),
        "{keep} existe déjà : le banc n'écrase pas un magasin"
    );
    let parent = keep
        .parent()
        .filter(|p| !p.as_str().is_empty())
        .map_or_else(|| Utf8PathBuf::from("."), Utf8PathBuf::from);
    anyhow::ensure!(
        parent.is_dir(),
        "{parent} n'est pas un répertoire : --keep ne saurait où écrire"
    );
    let inside = std::path::absolute(parent.as_std_path())?
        .starts_with(std::path::absolute(store.as_std_path())?);
    anyhow::ensure!(
        !inside,
        "{keep} est dans le store mesuré, que le banc ne doit jamais écrire"
    );
    Ok(())
}

/// Recopie le magasin de vecteurs de la copie jetable vers `keep`, qui ne doit pas exister.
fn keep_vectors(store: &Store, keep: &Utf8PathBuf) -> Result<()> {
    anyhow::ensure!(
        !keep.exists(),
        "{keep} existe déjà : le banc n'écrase pas un magasin"
    );
    std::fs::copy(store.vectors_path().as_std_path(), keep.as_std_path())
        .with_context(|| format!("copie du magasin vers {keep}"))?;
    println!("\nMagasin gardé     {keep}");
    Ok(())
}

/// Fait tourner `work` et l'annule une fois `limit` messages faits.
///
/// Par l'annulation coopérative que la passe consulte à chaque message, et non par un paramètre
/// de plus dans la passe : le banc mesure la passe telle qu'elle est livrée. Elle s'arrête donc à
/// la fin du lot en cours — un lot au plus de plus que demandé, et le relevé donne le nombre
/// réellement fait.
fn bounded<T>(limit: Option<u64>, progress: &Progress, work: impl FnOnce() -> T) -> T {
    let Some(limit) = limit else {
        return work();
    };
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                if progress.done() >= limit {
                    progress.cancel();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        // **Levé par un gardien**, pas après l'appel : une panique dans la passe sautait la ligne
        // qui libère le surveillant, et `thread::scope` attend chaque fil avant de relancer la
        // panique — le banc restait suspendu pour toujours, à zéro pour cent de processeur.
        let _release = Release(&finished);
        work()
    })
}

/// Lève le drapeau de fin en étant détruit, y compris pendant une panique.
struct Release<'a>(&'a AtomicBool);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Le contrôle du critère 3 : une passe coupée repart où elle en était.
///
/// Mesuré et pas seulement testé, parce que le test unitaire porte sur quatre messages et que ce
/// qui est en jeu ici est le **temps** : une reprise qui recalculerait tout aurait exactement le
/// même résultat et un coût multiplié par le nombre de coupures.
fn measure_resume(store: &Store, encoder: &impl Encoder) -> Result<()> {
    store.reset_vectorised().context("remise des drapeaux")?;

    let progress = Progress::new();
    let first =
        mailvec::pass::update_with(store, encoder, mailvec::text::Budget::default(), &progress)
            .context("passe complète")?;

    let at = Instant::now();
    let again =
        mailvec::pass::update_with(store, encoder, mailvec::text::Budget::default(), &progress)
            .context("reprise")?;
    let elapsed = at.elapsed();

    println!("\n--- reprise ---");
    println!("Première passe    {} vecteurs", first.vectorised);
    println!(
        "Seconde passe     {} vecteurs, {elapsed:.2?}",
        again.vectorised
    );
    if again.vectorised == 0 {
        println!("Verdict           rien à refaire — la reprise ne recalcule pas");
    } else {
        println!("Verdict           ANOMALIE : la seconde passe a refait du travail");
    }
    Ok(())
}

/// Tout ce que le relevé imprime.
struct Reading {
    stats: mailvec::pass::Stats,
    elapsed: Duration,
    load: Duration,
    dim: usize,
    batch: usize,
    total: u64,
    limited: bool,
    start: u64,
    store: u64,
    load_peak: u64,
    model: u64,
    pass_peak: u64,
    done: u64,
}

/// Écrit le relevé.
fn report(r: &Reading) {
    let seconds = r.elapsed.as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let rate = r.stats.vectorised as f64 / seconds.max(f64::EPSILON);

    println!("--- critère 3 : le coût de la passe ---");
    println!("Dimension         {}", r.dim);
    println!("Lot               {}", r.batch);
    println!("Chargement        {:.2?}", r.load);
    println!("Passe             {:.2?}", r.elapsed);
    println!("Débit             {rate:.2} messages/s");
    println!("Vectorisés        {}", r.stats.vectorised);
    println!("Blobs absents     {}", r.stats.missing_blobs);
    println!("Textes vides      {}", r.stats.empty);
    #[allow(clippy::cast_precision_loss)]
    let projected = r.total as f64 / rate.max(f64::EPSILON);
    println!(
        "Projection        {projected:.0} s pour {} messages{}",
        r.total,
        if r.limited {
            " — sur les premiers messages seulement, dont les longueurs ne sont pas celles du \
             corpus"
        } else {
            ""
        }
    );
    if !r.limited && r.total != r.stats.vectorised + r.stats.missing_blobs {
        println!(
            "ANOMALIE          {} attendus, {} traités",
            r.total, r.stats.vectorised
        );
    }

    println!("\n--- critère 5 : où va la mémoire ---");
    for (label, rss) in [
        ("au démarrage", r.start),
        ("store ouvert", r.store),
        ("crête chargement", r.load_peak),
        ("modèle chargé", r.model),
        ("crête passe", r.pass_peak),
        ("passe finie", r.done),
    ] {
        println!(
            "{label:<18}{}",
            mib(usize::try_from(rss).unwrap_or(usize::MAX))
        );
    }
    // Les deux crêtes échantillonnées, et les deux paliers au cas où l'échantillonneur aurait
    // manqué un instant : la crête est le plus grand de tout ce qui a été vu.
    let peak = [r.load_peak, r.model, r.pass_peak, r.done]
        .into_iter()
        .max()
        .unwrap_or(0);
    // Le seuil est **celui du critère 2 de la phase 2**, en octets et non en mébioctets : le
    // critère 5 le cite. Ce banc comparait jusqu'au 2026-09-25 à 500 Mio, soit 23 Mio plus
    // indulgent que le seuil qu'il prétendait appliquer.
    println!(
        "\nCrête             {}   {}",
        mib(usize::try_from(peak).unwrap_or(usize::MAX)),
        if peak < RSS_LIMIT {
            "sous les 500 Mo du critère"
        } else {
            "AU-DESSUS du seuil de 500 Mo"
        }
    );
    if r.limited {
        // Une passe bornée ne voit que les premiers messages du store, et la composition d'un
        // message coûte selon sa taille : sa crête compare deux réglages, elle ne tranche pas.
        println!(
            "                  — sur {} messages seulement : pas un verdict du critère 5",
            r.stats.vectorised + r.stats.missing_blobs
        );
    }
}

/// Copie `index.sqlite` du store vers la racine de travail.
pub(crate) fn copy_index(from: &Utf8PathBuf, to: &Utf8PathBuf) -> Result<()> {
    let source = from.join("index.sqlite");
    anyhow::ensure!(source.is_file(), "pas de store à {source}");
    std::fs::copy(&source, to.join("index.sqlite"))
        .with_context(|| format!("copie de {source}"))?;
    Ok(())
}

/// Le lien vers les blobs d'origine, retiré **avant** que le répertoire jetable ne le soit.
///
/// ## Pourquoi un gardien, et pourquoi il passe en premier
///
/// Le répertoire de travail est effacé à la fin du banc, et le lien est dedans. Un effacement
/// récursif qui **suivrait** le lien effacerait le corpus de mesure — soixante-treize mille
/// messages, des heures de réimport. La bibliothèque standard ne suit ni les liens symboliques ni
/// les jonctions, et c'est précisément le genre de propriété qu'on ne veut pas avoir à croire :
/// le lien est donc retiré par ce gardien, seul, par [`unlink`] — qui retire un lien de
/// répertoire sans jamais toucher sa cible. Il est déclaré après le répertoire jetable, donc détruit avant lui.
#[must_use]
pub(crate) struct Linked {
    path: Utf8PathBuf,
    /// Faux quand on a dû copier : une copie s'efface avec le reste, elle n'a rien d'original.
    link: bool,
}

impl Drop for Linked {
    fn drop(&mut self) {
        if self.link
            && let Err(error) = unlink(&self.path)
        {
            eprintln!("lien des blobs non retiré ({}) : {error}", self.path);
        }
    }
}

/// Rend les blobs du store d'origine visibles depuis la copie, sans les dupliquer.
///
/// Un lien symbolique quand le système le permet, une **jonction** de répertoire sinon — elle ne
/// demande aucun privilège sous Windows, là où le lien symbolique demande le mode développeur.
/// La copie ne vient qu'en dernier recours. Les blobs sont **lus** par la passe et par rien
/// d'autre : le lien ne peut pas servir à les modifier, parce que personne n'écrit sur ce chemin.
///
/// **À appeler avant `Store::open`**, qui crée un `blobs/` vide s'il n'en trouve pas : le lien
/// tombait alors sur un répertoire existant, échouait, et le banc recopiait tout le corpus à
/// chaque exécution — ce qu'il faisait jusqu'au 2026-09-25, en l'attribuant à un privilège.
pub(crate) fn link_blobs(from: &Utf8PathBuf, to: &Utf8PathBuf) -> Result<Linked> {
    // **Absolue**, la cible : un lien symbolique range un chemin relatif par rapport à *son
    // propre* répertoire, pas au répertoire courant. `measurements/store/blobs`, la valeur par
    // défaut, donnait donc un lien qui ne menait nulle part — et `Store::open`, trouvant un nom
    // occupé qui n'était pas un répertoire, refusait d'ouvrir. Trouvé au premier relevé du
    // 2026-09-25 ; le test, qui passait des chemins absolus, ne pouvait pas le voir.
    let from = std::path::absolute(from.as_std_path())
        .with_context(|| format!("chemin absolu de {from}"))?;
    let from = Utf8PathBuf::from_path_buf(from)
        .map_err(|p| anyhow::anyhow!("chemin non UTF-8 : {}", p.display()))?;

    #[cfg(windows)]
    let made = usable(
        std::os::windows::fs::symlink_dir(from.as_std_path(), to.as_std_path()).is_ok(),
        to,
    ) || usable(junction(&from, to), to);
    #[cfg(not(windows))]
    let made = usable(
        std::os::unix::fs::symlink(from.as_std_path(), to.as_std_path()).is_ok(),
        to,
    );

    if made {
        return Ok(Linked {
            path: to.clone(),
            link: true,
        });
    }

    // Ni lien ni jonction : on copie. C'est long et ça marche, et le dire vaut mieux que
    // d'échouer sur une permission qui n'a rien à voir avec ce qu'on mesure.
    println!("Lien impossible, copie des blobs…");
    copy_tree(&from, to)?;
    Ok(Linked {
        path: to.clone(),
        link: false,
    })
}

/// Vrai si un lien vient d'être créé **et** mène à un répertoire.
///
/// Un lien créé qui ne mène nulle part est retiré : le laisser occuperait le nom, et la copie de
/// repli échouerait dessus.
fn usable(made: bool, to: &Utf8PathBuf) -> bool {
    if !made {
        // Une commande qui échoue peut avoir créé quelque chose avant d'échouer : un lien laissé
        // occuperait le nom, et la copie de repli écrirait **à travers** lui.
        if is_link(to) {
            unlink(to).ok();
        }
        return false;
    }
    if to.is_dir() {
        return true;
    }
    unlink(to).ok();
    false
}

/// Retire un lien de répertoire, jamais sa cible.
///
/// `remove_dir` sous Windows, où un lien de répertoire et une jonction sont des répertoires pour
/// le système ; `remove_file` ailleurs, où un lien symbolique est un fichier. Aucun des deux ne
/// descend dans ce qu'il désigne, et `remove_dir` échoue sur un vrai répertoire non vide plutôt
/// que de le vider.
fn unlink(path: &Utf8PathBuf) -> std::io::Result<()> {
    #[cfg(windows)]
    return std::fs::remove_dir(path.as_std_path());
    #[cfg(not(windows))]
    return std::fs::remove_file(path.as_std_path());
}

/// Une jonction de répertoire, par `mklink /J`.
///
/// Un utilitaire du système et non un script : la bibliothèque standard ne sait pas créer de
/// jonction, et la seule autre voie est un appel Win32 que `#![forbid(unsafe_code)]` exclut.
///
/// **Pas de caractère que `cmd` interpréterait.** Rust ne met entre guillemets qu'un argument qui
/// porte une espace, et `cmd` lit `&`, `^`, `%` : un chemin `a&b\blobs` créait une jonction vers
/// `a`, un tout autre répertoire, puis échouait — et la copie de repli y écrivait le corpus. Un tel
/// chemin est refusé, et la copie prend le relais.
#[cfg(windows)]
fn junction(from: &Utf8PathBuf, to: &Utf8PathBuf) -> bool {
    const INTERPRETED: &[char] = &['&', '|', '^', '%', '<', '>', '(', ')', '"', '!'];
    if from.as_str().contains(INTERPRETED) || to.as_str().contains(INTERPRETED) {
        return false;
    }
    std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J", to.as_str(), from.as_str()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Copie récursive, pour le repli de [`link_blobs`].
///
/// `create_dir` et non `create_dir_all` : la copie refuse un répertoire qui existe déjà — un lien
/// resté là la ferait écrire ailleurs que dans la copie.
pub(crate) fn copy_tree(from: &Utf8PathBuf, to: &Utf8PathBuf) -> Result<()> {
    std::fs::create_dir(to.as_std_path()).with_context(|| format!("création de {to}"))?;
    for entry in
        std::fs::read_dir(from.as_std_path()).with_context(|| format!("lecture de {from}"))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let source = from.join(name);
        let target = to.join(name);
        if entry.file_type()?.is_dir() {
            copy_tree(&source, &target)?;
        } else {
            std::fs::copy(source.as_std_path(), target.as_std_path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::link_blobs;
    use camino::Utf8PathBuf;

    fn utf8(dir: &tempfile::TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()
    }

    #[test]
    fn the_originals_survive_the_cleanup_of_the_work_directory() {
        // Le risque que le gardien couvre : un effacement du répertoire jetable qui suivrait le
        // lien et viderait le corpus de mesure.
        let originals = tempfile::tempdir().unwrap();
        let original = utf8(&originals).join("blobs");
        std::fs::create_dir_all(original.join("ab")).unwrap();
        std::fs::write(original.join("ab").join("cdef"), b"un message").unwrap();

        let work = tempfile::tempdir().unwrap();
        let linked_path = utf8(&work).join("blobs");
        let linked = link_blobs(&original, &linked_path).unwrap();

        // Le contrôle positif : le lien donne bien à lire les originaux. Sans lui, un lien raté
        // qui ne mène nulle part passerait le reste du test.
        assert_eq!(
            std::fs::read(linked_path.join("ab").join("cdef")).unwrap(),
            b"un message"
        );

        drop(linked);
        drop(work);
        assert_eq!(
            std::fs::read(original.join("ab").join("cdef")).unwrap(),
            b"un message",
            "l'effacement du répertoire jetable a atteint les originaux"
        );
    }

    #[test]
    fn a_link_is_made_rather_than_a_copy() {
        // Le défaut du 2026-09-25 : le banc recopiait tout le corpus à chaque exécution. Sur la
        // machine de mesure, il doit maintenant lier — symlink ou jonction.
        let originals = tempfile::tempdir().unwrap();
        let original = utf8(&originals).join("blobs");
        std::fs::create_dir_all(&original).unwrap();

        let work = tempfile::tempdir().unwrap();
        let linked = link_blobs(&original, &utf8(&work).join("blobs")).unwrap();
        assert!(linked.link, "les blobs ont été copiés au lieu d'être liés");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod dangling {
    use super::usable;
    use camino::Utf8PathBuf;

    #[test]
    fn a_link_that_leads_nowhere_is_refused_and_its_name_freed() {
        // Le défaut du premier relevé du 2026-09-25 : un lien vers une cible relative, donc
        // pendant. Il occupait le nom, et tout ce qui suivait — l'ouverture du store, la copie de
        // repli — échouait dessus.
        let work = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(work.path().to_path_buf()).unwrap();
        let nowhere = root.join("absent");
        let to = root.join("blobs");

        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(nowhere.as_std_path(), to.as_std_path())
            .is_ok()
            || super::junction(&nowhere, &to);
        #[cfg(not(windows))]
        let made = std::os::unix::fs::symlink(nowhere.as_std_path(), to.as_std_path()).is_ok();

        // Le contrôle positif : un lien pendant existe bien. Sinon le test ne prouverait rien.
        assert!(
            made,
            "impossible de fabriquer un lien pendant sur cette machine"
        );
        assert!(std::fs::symlink_metadata(to.as_std_path()).is_ok());

        assert!(!usable(made, &to), "un lien pendant a été accepté");
        assert!(
            std::fs::symlink_metadata(to.as_std_path()).is_err(),
            "le lien pendant occupe encore le nom"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod sweep {
    use super::{link_blobs, work_dir_in};
    use camino::Utf8PathBuf;

    #[test]
    fn an_interrupted_run_is_swept_without_touching_what_it_linked() {
        // Ce qu'une exécution tuée laisse : son répertoire, une copie d'index, et un lien vers les
        // blobs du corpus. Le balayage doit retirer le tout, et laisser la cible du lien intacte.
        let originals = tempfile::tempdir().unwrap();
        let blobs = Utf8PathBuf::from_path_buf(originals.path().join("blobs")).unwrap();
        std::fs::create_dir_all(blobs.join("ab")).unwrap();
        std::fs::write(blobs.join("ab").join("cdef"), b"un message").unwrap();

        let base = tempfile::tempdir().unwrap();
        let killed = Utf8PathBuf::from_path_buf(base.path().join("run-tue")).unwrap();
        std::fs::create_dir_all(&killed).unwrap();
        std::fs::write(killed.join("index.sqlite"), b"copie").unwrap();
        let linked = link_blobs(&blobs, &killed.join("blobs")).unwrap();
        assert!(linked.link, "le test veut un vrai lien");
        // Tué : le gardien ne s'exécute pas.
        std::mem::forget(linked);

        let fresh = work_dir_in(base.path()).unwrap();
        assert!(
            !killed.exists(),
            "l'exécution interrompue n'a pas été balayée"
        );
        assert_eq!(
            std::fs::read(blobs.join("ab").join("cdef")).unwrap(),
            b"un message",
            "le balayage a atteint les originaux"
        );
        assert!(fresh.path().starts_with(base.path()));
    }
}

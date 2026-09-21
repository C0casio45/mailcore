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

use std::time::Instant;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Progress, Store};

use crate::model::{current_rss, mib};

/// Mesure une passe complète : débit, durée, mémoire par palier, et reprise.
///
/// # Errors
///
/// Si le store n'est pas copiable ou lisible, si le modèle n'est pas chargeable, ou si la passe
/// échoue.
pub fn measure(store_root: &Utf8PathBuf, model: &Utf8PathBuf, resume: bool) -> Result<()> {
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

    let rss_start = current_rss();
    let work = tempfile::tempdir().context("répertoire jetable")?;
    let root = Utf8PathBuf::from_path_buf(work.path().to_path_buf())
        .map_err(|p| anyhow::anyhow!("chemin non UTF-8 : {}", p.display()))?;

    copy_index(store_root, &root)?;
    let store = Store::open(&root).context("ouverture de la copie")?;
    let rss_store = current_rss();

    // Les blobs du store d'origine, lus en place. La copie n'en a pas, et les recopier
    // coûterait plusieurs gigaoctets pour une passe qui ne fait que les lire.
    let blobs = store_root.join("blobs");
    if !blobs.is_dir() {
        anyhow::bail!("le store n'a pas de répertoire `blobs` : {blobs}");
    }
    link_blobs(&blobs, &root.join("blobs"))?;

    let total = store.unvectorised_total().context("compte des messages")?;
    println!("Store             {store_root}");
    println!("Modèle            {model}");
    println!("À vectoriser      {total} messages\n");
    anyhow::ensure!(total > 0, "rien à vectoriser : le store est-il indexé ?");

    let loading = Instant::now();
    let encoder = mailvec::model::Model::load(model).context("chargement du modèle")?;
    let load = loading.elapsed();
    let rss_model = current_rss();
    let dim = encoder.dim();

    let progress = Progress::new();
    let at = Instant::now();
    let stats = mailvec::pass::update_with(
        &store,
        &encoder,
        mailvec::text::Budget::default(),
        &progress,
    )
    .context("passe")?;
    let elapsed = at.elapsed();
    let rss_done = current_rss();

    report(
        &stats, elapsed, load, dim, total, rss_start, rss_store, rss_model, rss_done,
    );

    if resume {
        measure_resume(&store, &encoder)?;
    }

    Ok(())
}

/// Le contrôle du critère 3 : une passe coupée repart où elle en était.
///
/// Mesuré et pas seulement testé, parce que le test unitaire porte sur quatre messages et que ce
/// qui est en jeu ici est le **temps** : une reprise qui recalculerait tout aurait exactement le
/// même résultat et un coût multiplié par le nombre de coupures.
fn measure_resume(store: &Store, encoder: &mailvec::model::Model) -> Result<()> {
    store.reset_vectorised().context("remise des drapeaux")?;

    // Une coupure au premier lot : `Progress` annulé dès le départ arrête la passe avant tout
    // travail, ce qui n'est pas la coupure intéressante. On laisse donc un lot passer, puis on
    // annule — ce que la boucle regarde message par message.
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

/// Écrit le relevé.
#[allow(clippy::too_many_arguments)]
fn report(
    stats: &mailvec::pass::Stats,
    elapsed: std::time::Duration,
    load: std::time::Duration,
    dim: usize,
    total: u64,
    rss_start: u64,
    rss_store: u64,
    rss_model: u64,
    rss_done: u64,
) {
    let seconds = elapsed.as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let rate = stats.vectorised as f64 / seconds.max(f64::EPSILON);

    println!("--- critère 3 : le coût de la passe ---");
    println!("Dimension         {dim}");
    println!("Chargement        {load:.2?}");
    println!("Passe             {elapsed:.2?}");
    println!("Débit             {rate:.0} messages/s");
    println!("Vectorisés        {}", stats.vectorised);
    println!("Blobs absents     {}", stats.missing_blobs);
    println!("Textes vides      {}", stats.empty);
    #[allow(clippy::cast_precision_loss)]
    let projected = 73_658.0 / rate.max(f64::EPSILON);
    println!("Projection 73 658 {projected:.0} s");
    if total != stats.vectorised + stats.missing_blobs {
        println!(
            "ANOMALIE          {total} attendus, {} traités",
            stats.vectorised
        );
    }

    println!("\n--- critère 5 : où va la mémoire ---");
    for (label, rss) in [
        ("au démarrage", rss_start),
        ("store ouvert", rss_store),
        ("modèle chargé", rss_model),
        ("passe finie", rss_done),
    ] {
        println!("{label:<18}{}", mib(rss as usize));
    }
    let peak = rss_done.max(rss_model);
    println!(
        "\nCrête             {}   {}",
        mib(peak as usize),
        if peak < 500 * 1024 * 1024 {
            "sous les 500 Mio du critère"
        } else {
            "AU-DESSUS du seuil"
        }
    );
}

/// Copie `index.sqlite` du store vers la racine de travail.
fn copy_index(from: &Utf8PathBuf, to: &Utf8PathBuf) -> Result<()> {
    let source = from.join("index.sqlite");
    anyhow::ensure!(source.is_file(), "pas de store à {source}");
    std::fs::copy(&source, to.join("index.sqlite"))
        .with_context(|| format!("copie de {source}"))?;
    Ok(())
}

/// Rend les blobs du store d'origine visibles depuis la copie, sans les dupliquer.
///
/// Un lien symbolique quand le système le permet, une jonction de répertoire sinon. Les blobs
/// sont **lus** par la passe et par rien d'autre : le lien ne peut pas servir à les modifier,
/// parce que personne n'écrit sur ce chemin.
fn link_blobs(from: &Utf8PathBuf, to: &Utf8PathBuf) -> Result<()> {
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(from.as_std_path(), to.as_std_path());
    #[cfg(not(windows))]
    let made = std::os::unix::fs::symlink(from.as_std_path(), to.as_std_path());

    if made.is_ok() {
        return Ok(());
    }

    // Sans le privilège de création de lien — le cas ordinaire sur Windows hors mode
    // développeur —, on copie. C'est long et ça marche, et le dire vaut mieux que d'échouer sur
    // une permission qui n'a rien à voir avec ce qu'on mesure.
    println!("Lien impossible, copie des blobs…");
    copy_tree(from, to)
}

/// Copie récursive, pour le repli de [`link_blobs`].
fn copy_tree(from: &Utf8PathBuf, to: &Utf8PathBuf) -> Result<()> {
    std::fs::create_dir_all(to.as_std_path()).with_context(|| format!("création de {to}"))?;
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

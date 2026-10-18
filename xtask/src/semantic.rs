//! Les bancs des critères 2 et 4 de `docs/PHASE-4.md` : chercher par le sens de bout en bout, et
//! ce que coûte un message qui arrive.
//!
//! ## Une réplique, jamais le store
//!
//! Les deux écrivent — des drapeaux, un enfant qui ajoute au magasin —, donc ils travaillent sur
//! une **réplique** sous `target/` : `index.sqlite`, l'index plein texte, le modèle et le magasin
//! de vecteurs sont copiés, les blobs sont liés et lus en place. C'est la règle de
//! `measure-vectors`, et elle a la même raison : un paramètre qui peut désigner le store réel le
//! désignera.
//!
//! ## Le chemin livré, pas un chemin de banc
//!
//! - **Critère 2** : la requête entre par `Api::handle_message`, la porte que la coquille livrée
//!   emprunte — elle sert son démon embarqué —, et le sens est servi par **l'exécutable `maild`
//!   relancé en enfant**, comme dans le produit. Le transport HTTP n'est pas dans ce relevé : la
//!   coquille n'en a pas.
//! - **Critère 4** : l'enfant de la passe de fond est lancé tel que le démon le lance — même
//!   drapeau, même priorité minimale —, sur une réplique où **un seul** message attend. Ce que le
//!   relevé chronomètre est ce que paie une arrivée : démarrer un processus, décider, charger le
//!   modèle, encoder, écrire.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{MessageId, Store};
use serde::Deserialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Des octets en Mio, pour l'affichage.
fn mib(bytes: u64) -> String {
    crate::model::mib(usize::try_from(bytes).unwrap_or(usize::MAX))
}
use crate::vectors::{Linked, copy_index, copy_tree, link_blobs, work_dir_in};

/// Le seuil du critère 2 : celui du critère 4 de la phase 1.
const SEARCH_BUDGET: Duration = Duration::from_millis(50);

/// Le seuil du critère 4.
const ARRIVAL_BUDGET: Duration = Duration::from_millis(200);

/// Une page d'écran, comme `measure-api`.
const PAGE: usize = 50;

/// Ce que les deux bancs répliquent.
#[derive(Debug, Clone)]
pub struct Sources {
    /// Le store d'origine, lu et jamais écrit.
    pub store: Utf8PathBuf,
    /// Le magasin de vecteurs à servir — celui qu'a gardé `measure-vectors --keep`.
    pub vectors: Utf8PathBuf,
    /// Le modèle qui l'a écrit.
    pub model: Utf8PathBuf,
    /// L'exécutable `maild` à relancer en enfant. Par défaut, celui de `target/release`.
    pub maild: Option<Utf8PathBuf>,
}

/// Une réplique jetable. Le lien des blobs est déclaré **avant** le répertoire, donc retiré avant
/// lui : voir [`Linked`].
struct Replica {
    _linked: Linked,
    root: Utf8PathBuf,
    _work: tempfile::TempDir,
}

impl Replica {
    fn build(sources: &Sources, base: &str, with_search: bool) -> Result<Self> {
        for (what, path) in [
            ("le store", sources.store.join("index.sqlite")),
            ("le magasin de vecteurs", sources.vectors.clone()),
            ("le modèle", sources.model.join("config.json")),
        ] {
            anyhow::ensure!(path.exists(), "{what} est introuvable : {path}");
        }
        let work = work_dir_in(&std::path::absolute(base).context("chemin de travail")?)?;
        let root = Utf8PathBuf::from_path_buf(work.path().to_path_buf())
            .map_err(|p| anyhow::anyhow!("chemin non UTF-8 : {}", p.display()))?;
        copy_index(&sources.store, &root)?;
        // **Avant** `Store::open`, qui créerait un `blobs/` vide : voir `link_blobs`.
        let linked = link_blobs(&sources.store.join("blobs"), &root.join("blobs"))?;
        copy_tree(&sources.model, &mailvec::bert::default_dir(&root))?;
        std::fs::copy(
            sources.vectors.as_std_path(),
            Store::vectors_path_in(&root).as_std_path(),
        )
        .context("copie du magasin de vecteurs")?;
        if with_search {
            copy_tree(&sources.store.join("search"), &root.join("search"))?;
        }
        Ok(Self {
            _linked: linked,
            root,
            _work: work,
        })
    }
}

/// Refuse le profil de débogage : `mailcore`, `mailvec` et `maild` n'y sont pas optimisés.
pub(crate) fn release_only(name: &str) -> Result<()> {
    anyhow::ensure!(
        !cfg!(debug_assertions),
        "{name} chronomètre du code qui n'est pas optimisé en profil de débogage — le relevé \
         serait faux.\nLancez : cargo run --release -p xtask -- {name}"
    );
    Ok(())
}

/// L'exécutable à relancer, et sa date : un binaire plus ancien que les sources mesurerait le
/// code d'hier.
fn program(sources: &Sources) -> Result<Utf8PathBuf> {
    let program = match &sources.maild {
        Some(path) => path.clone(),
        None => Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .context("racine du workspace")?
            .join("target")
            .join("release")
            .join(if cfg!(windows) { "maild.exe" } else { "maild" }),
    };
    let modified = std::fs::metadata(program.as_std_path())
        .and_then(|meta| meta.modified())
        .with_context(|| {
            format!("{program} introuvable : cargo build --release -p maild, puis relancer")
        })?;
    let age = modified.elapsed().unwrap_or_default();
    println!(
        "Exécutable        {program} (compilé il y a {} min)",
        age.as_secs() / 60
    );
    Ok(program)
}

/// La mémoire résidente d'un processus.
fn rss_of(system: &mut System, pid: Pid) -> u64 {
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    system.process(pid).map_or(0, sysinfo::Process::memory)
}

/// Les enfants `maild` de ce processus, et leur mémoire résidente.
fn children_rss() -> Vec<u64> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    let me = Pid::from_u32(std::process::id());
    system
        .processes()
        .values()
        .filter(|process| process.parent() == Some(me))
        .filter(|process| process.name().to_string_lossy().contains("maild"))
        .map(sysinfo::Process::memory)
        .collect()
}

/// Le percentile d'une série.
pub(crate) fn percentile(latencies: &[Duration], quantile: f64) -> Duration {
    if latencies.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let index = ((sorted.len() as f64 - 1.0) * quantile).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn ms(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
}

// ---------------------------------------------------------------------------------------------
// Critère 2
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Ground {
    query: Vec<Query>,
}

/// Le texte seul : ce banc ne lit pas les cibles, il chronomètre.
#[derive(Debug, Deserialize)]
struct Query {
    text: String,
}

/// Les textes d'un jeu de requêtes du critère 1, dans l'ordre du fichier — les cibles ne sont pas
/// lues : qui chronomètre n'a pas à les connaître.
///
/// # Errors
///
/// Si le fichier est illisible, mal formé, ou ne contient aucune requête.
pub(crate) fn query_texts(queries: &Utf8PathBuf) -> Result<Vec<String>> {
    let raw = std::fs::read_to_string(queries)
        .with_context(|| format!("lecture du jeu de requêtes {queries}"))?;
    let ground: Ground = toml::from_str(&raw).with_context(|| format!("analyse de {queries}"))?;
    anyhow::ensure!(
        !ground.query.is_empty(),
        "{queries} ne contient aucune requête"
    );
    Ok(ground.query.into_iter().map(|query| query.text).collect())
}

/// Chronomètre `search.query` de bout en bout : la requête écrite, servie, et la réponse relue.
async fn timed(
    api: &maild::api::jsonrpc::Api,
    query: &str,
) -> Result<(Duration, mailapi::dto::Results)> {
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "search.query",
        "params": {"query": query, "limit": PAGE},
    })
    .to_string();
    let started = Instant::now();
    let text = api
        .handle_message(&request)
        .await
        .context("aucune réponse")?;
    let response: serde_json::Value = serde_json::from_str(&text).context("réponse illisible")?;
    let elapsed = started.elapsed();
    if let Some(error) = response.get("error") {
        anyhow::bail!("requête refusée : {error}");
    }
    let results =
        serde_json::from_value(response["result"].clone()).context("résultat illisible")?;
    Ok((elapsed, results))
}

/// Mesure le critère 2 : la recherche hybride, de bout en bout, à froid puis chaude.
///
/// # Errors
///
/// Si la réplique ne se construit pas, si le jeu de requêtes est illisible, ou si la recherche
/// par le sens ne répond pas — un relevé des mots seuls ne mesurerait pas le critère.
pub fn measure_search(sources: &Sources, queries: &Utf8PathBuf, rounds: usize) -> Result<()> {
    release_only("measure-semantic-search")?;
    anyhow::ensure!(
        rounds >= 2,
        "au moins deux tours : le premier est jeté, toujours"
    );
    let texts = query_texts(queries)?;

    let program = program(sources)?;
    let replica = Replica::build(sources, "target/measure-semantic-search", true)?;
    let mailbox = mailcore::Mailbox::open(&replica.root).context("ouverture de la réplique")?;
    anyhow::ensure!(
        mailbox.search_available(),
        "l'index plein texte de la réplique est absent"
    );
    let shared = Arc::new(Mutex::new(mailbox));
    let jobs = maild::jobs::Jobs::start(
        Arc::clone(&shared),
        replica.root.clone(),
        maild::jobs::Sources::new(Vec::new()),
    );
    let engine = Arc::new(maild::semantic::Engine::start(
        &replica.root,
        &maild::vectors::Launch::program(program.as_std_path()),
        maild::jobs::Activity::default(),
    ));
    let hybrid = maild::api::jsonrpc::Api::new(Arc::clone(&shared), jobs.clone())
        .with_semantic(Arc::clone(&engine));
    // La même boîte sans le sens : la différence des deux est ce que le sens coûte.
    let words = maild::api::jsonrpc::Api::new(shared, jobs);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("runtime")?;

    println!("Store             {}", sources.store);
    println!("Vecteurs          {}", sources.vectors);
    println!(
        "Requêtes          {} × {rounds} tours, le premier jeté\n",
        texts.len()
    );

    // **À froid** : la première recherche lance l'enfant **à côté** et répond par les mots seuls ;
    // ce qui se mesure est ce qu'elle a coûté, puis le temps que l'enfant met à être prêt.
    let (cold, first) = runtime.block_on(timed(&hybrid, &texts[0]))?;
    let state = first.semantic.map(|it| it.state);
    anyhow::ensure!(
        state == Some(mailapi::dto::SemanticState::Starting),
        "la première recherche devait réveiller le sens, elle dit {state:?}"
    );
    let woken = Instant::now();
    while !engine.is_ready() {
        anyhow::ensure!(
            woken.elapsed() < Duration::from_secs(180),
            "le sens n'est pas prêt après trois minutes : {:?}",
            engine.standing()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let warm_up = woken.elapsed();
    let child = children_rss();

    let mut hybrid_rounds: Vec<Vec<Duration>> = Vec::with_capacity(rounds);
    let mut words_rounds: Vec<Vec<Duration>> = Vec::with_capacity(rounds);
    let mut by_meaning = 0usize;
    for _ in 0..rounds {
        let mut h = Vec::with_capacity(texts.len());
        let mut w = Vec::with_capacity(texts.len());
        for (index, query) in texts.iter().enumerate() {
            // L'ordre des deux bras alterne : le second trouve l'index plein texte chaud, et
            // toujours le même second ferait paraître un bras plus rapide qu'il n'est.
            if index % 2 == 1 {
                w.push(runtime.block_on(timed(&words, query))?.0);
            }
            let (elapsed, results) = runtime.block_on(timed(&hybrid, query))?;
            // **Chaque** réponse doit venir du sens : un enfant mort en cours de relevé ferait
            // entrer des réponses des mots seuls dans le p95, et le chiffre serait flatteur.
            let state = results.semantic.as_ref().map(|it| it.state);
            anyhow::ensure!(
                state == Some(mailapi::dto::SemanticState::Ready),
                "« {query} » n'a pas été servie par le sens : {state:?}"
            );
            h.push(elapsed);
            by_meaning += results
                .rows
                .iter()
                .filter(|row| row.found_by == Some(mailapi::dto::FoundBy::Meaning))
                .count();
            if index % 2 == 0 {
                w.push(runtime.block_on(timed(&words, query))?.0);
            }
        }
        hybrid_rounds.push(h);
        words_rounds.push(w);
    }

    println!(
        "À froid           {}  la recherche qui réveille le sens, servie par les mots",
        ms(cold)
    );
    println!(
        "Réveil            {}  l'enfant naît, charge le modèle et les vecteurs",
        ms(warm_up)
    );
    for (index, rss) in child.iter().enumerate() {
        println!("Enfant {index}          {} résidents, chaud", mib(*rss));
    }
    println!(
        "Trouvés par le sens seul  {by_meaning} lignes sur {} réponses — le sens a bien répondu",
        rounds * texts.len()
    );

    // **La dispersion avant la médiane** : un tour qui s'écarte des autres n'est pas du bruit.
    println!("\nPar tour           hybride p50 / p95        mots seuls p50 / p95");
    for (index, (h, w)) in hybrid_rounds.iter().zip(&words_rounds).enumerate() {
        println!(
            "  tour {}{}     {:>9} / {:<9}     {:>9} / {}",
            index + 1,
            if index == 0 { " (jeté)" } else { "        " },
            ms(percentile(h, 0.5)),
            ms(percentile(h, 0.95)),
            ms(percentile(w, 0.5)),
            ms(percentile(w, 0.95)),
        );
    }
    let kept_h: Vec<Duration> = hybrid_rounds.iter().skip(1).flatten().copied().collect();
    let kept_w: Vec<Duration> = words_rounds.iter().skip(1).flatten().copied().collect();
    println!("\nTours gardés       hybride                  mots seuls");
    for (name, quantile) in [("p50", 0.5), ("p95", 0.95), ("max", 1.0)] {
        println!(
            "  {name}             {:>9}                {}",
            ms(percentile(&kept_h, quantile)),
            ms(percentile(&kept_w, quantile)),
        );
    }
    let p95 = percentile(&kept_h, 0.95);
    println!(
        "\n{} critère 2 — recherche hybride p95, chaude, de bout en bout : {} (seuil < {})",
        if p95 < SEARCH_BUDGET {
            "OK   "
        } else {
            "ÉCHEC"
        },
        ms(p95),
        ms(SEARCH_BUDGET)
    );
    println!(
        "Ce que ça ne dit pas : la première recherche après cinq minutes sans requête n'a pas le \
         sens — elle le réveille, et la coquille la refait une fois l'enfant prêt."
    );
    drop(engine);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Critère 7
// ---------------------------------------------------------------------------------------------

/// Mesure le critère 7 : le démarrage de la coquille livrée, avec le modèle puis sans.
///
/// ## La coquille ne doit pouvoir parler à personne
///
/// Elle sert son démon embarqué, qui monte la veille `IDLE` de chaque compte IMAP et le facteur.
/// Sur une réplique d'un store qui aurait un compte IMAP ou un message en file, le banc se
/// connecterait au vrai serveur — ou enverrait un vrai message. Il le **refuse** avant de lancer
/// quoi que ce soit.
///
/// # Errors
///
/// Si la réplique ne se construit pas, si elle porte un compte IMAP ou une file non vide, ou si
/// la coquille ne rend pas son relevé.
pub fn measure_startup(sources: &Sources, runs: usize) -> Result<()> {
    let replica = Replica::build(sources, "target/measure-startup-model", true)?;
    {
        // **Les comptes à serveur sont désactivés, dans la réplique.** Le corpus de mesure en
        // porte neuf, et la coquille ouvrirait leurs veilles et leurs moissons — des connexions
        // réelles, avec les secrets du trousseau, pendant un chronométrage. Un compte désactivé
        // n'est ni veillé ni moissonné (`maild::watch`, `maild::jobs`). La réplique est jetable :
        // l'original n'est pas ouvert.
        //
        // **En SQL, et pas par `Store::full_accounts`**, qui refuse le corpus entier dès qu'un
        // compte y est incohérent — le corpus en a un, `imap` sans hôte, et le banc tombait
        // avant d'avoir rien désactivé. Le banc n'a pas à valider les comptes : il doit
        // seulement garantir qu'aucun ne parlera à un serveur, et un `UPDATE` sur la colonne
        // le garantit pour tous, cohérents ou non.
        {
            let db = rusqlite::Connection::open(replica.root.join("index.sqlite").as_std_path())
                .context("ouverture de la base de la réplique")?;
            db.execute("UPDATE accounts SET enabled = 0 WHERE kind <> 'mbox'", [])
                .context("désactivation des comptes de la réplique")?;
            // Relu, et non supposé : un `UPDATE` qui n'aurait rien touché laisserait la coquille
            // se connecter.
            let live: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM accounts WHERE kind <> 'mbox' AND enabled <> 0",
                    [],
                    |row| row.get(0),
                )
                .context("relecture des comptes de la réplique")?;
            anyhow::ensure!(
                live == 0,
                "{live} compte(s) à serveur encore actifs dans la réplique : la coquille s'y \
                 connecterait"
            );
        }
        let store = Store::open(&replica.root).context("ouverture de la réplique")?;
        anyhow::ensure!(
            store.outbox()?.is_empty(),
            "la réplique a des messages en file : la coquille les enverrait"
        );
    }
    crate::ui::set_skip_build(true);

    println!("=== Avec le modèle et les vecteurs ===\n");
    crate::ui::measure(&replica.root, crate::ui::Shell::Egui, "startup", runs, None)?;

    // **Le fichier de modèle supprimé**, comme le dit le critère — et les vecteurs avec, pour
    // que rien de la recherche par le sens ne reste à lire.
    std::fs::remove_dir_all(mailvec::bert::default_dir(&replica.root).as_std_path())
        .context("retrait du modèle de la réplique")?;
    std::fs::remove_file(Store::vectors_path_in(&replica.root).as_std_path())
        .context("retrait des vecteurs de la réplique")?;
    println!("\n=== Sans modèle ni vecteurs ===\n");
    crate::ui::measure(&replica.root, crate::ui::Shell::Egui, "startup", runs, None)
}

// ---------------------------------------------------------------------------------------------
// Critère 4
// ---------------------------------------------------------------------------------------------

/// Ce qu'un passage de l'enfant a coûté.
struct Round {
    line: String,
    to_line: Duration,
    to_exit: Duration,
    peak: u64,
}

/// Lance l'enfant de la passe de fond **comme le démon le lance**, et le chronomètre.
fn run_child(program: &Utf8PathBuf, root: &Utf8PathBuf) -> Result<Round> {
    let mut command = Command::new(program.as_std_path());
    command
        .arg(maild::vectors::CHILD_FLAG)
        .arg(root.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Les drapeaux de `maild::vectors` : priorité minimale, pas de console.
        const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(IDLE_PRIORITY_CLASS | CREATE_NO_WINDOW);
    }
    let started = Instant::now();
    let mut child = command.spawn().context("lancement de l'enfant")?;
    let pid = Pid::from_u32(child.id());

    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut system = System::new();
            let mut peak = 0;
            while !stop.load(Ordering::Relaxed) {
                peak = peak.max(rss_of(&mut system, pid));
                std::thread::sleep(Duration::from_millis(5));
            }
            peak
        })
    };

    let stdout = child.stdout.take().context("sortie de l'enfant")?;
    // Sur un fil à part, pour qu'un enfant pendu ne pende pas le banc.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = sender.send(read);
    });
    let line = match receiver.recv_timeout(Duration::from_secs(300)) {
        Ok(read) => read.context("lecture de la réponse de l'enfant")?,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            stop.store(true, Ordering::Relaxed);
            anyhow::bail!("l'enfant n'a rien répondu en cinq minutes");
        }
    };
    let to_line = started.elapsed();
    // L'entrée fermée, l'enfant sort : c'est ainsi qu'il apprend la mort de son parent.
    drop(child.stdin.take());
    child.wait().context("attente de l'enfant")?;
    let to_exit = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    let peak = sampler.join().unwrap_or(0);
    Ok(Round {
        line: line.trim().to_owned(),
        to_line,
        to_exit,
        peak,
    })
}

/// Mesure le critère 4 : ce que coûte un message qui arrive.
///
/// # Errors
///
/// Si la réplique ne se construit pas, ou si l'enfant ne fait pas ce que le protocole prévoit.
pub fn measure_arrival(sources: &Sources, arrivals: usize) -> Result<()> {
    release_only("measure-arrival")?;
    anyhow::ensure!(
        arrivals >= 2,
        "au moins deux arrivées : la première est jetée, toujours"
    );
    let program = program(sources)?;
    let replica = Replica::build(sources, "target/measure-arrival", false)?;
    let store = Store::open(&replica.root).context("ouverture de la réplique")?;

    store.reset_vectorised()?;
    let all: Vec<MessageId> = store
        .unvectorised(usize::MAX)?
        .into_iter()
        .map(|row| row.id)
        .collect();
    anyhow::ensure!(all.len() > arrivals, "la réplique a trop peu de messages");
    let summary = mailcore::vectors::summary_of(&store.vectors_path())?
        .context("le magasin de vecteurs est vide")?;
    println!(
        "Store             {} ({} messages)",
        sources.store,
        all.len()
    );
    println!(
        "Vecteurs          {} records{}",
        summary.records,
        summary
            .provenance
            .as_ref()
            .map_or_else(String::new, |p| format!(", {}", p.summary()))
    );
    // Les plus récents : c'est ce qui arrive. Ils ont déjà un record — la relecture garde le
    // dernier, donc un second ne change rien à ce qui se cherche, et rien à ce qui se mesure.
    let targets: Vec<MessageId> = all.iter().rev().take(arrivals).copied().collect();

    // Ce que le démon paie **dans son propre processus** avant de lancer quoi que ce soit.
    store.mark_vectorised(&all)?;
    let decided = Instant::now();
    let plan = mailvec::pass::plan(&store, mailvec::text::Budget::default(), None)?;
    let decision = decided.elapsed();
    anyhow::ensure!(
        matches!(plan, mailvec::pass::Plan::Nothing),
        "la réplique devrait n'avoir rien à faire, la passe dit {plan:?} — le magasin vient-il \
         d'une autre composition ?"
    );

    // Un passage sans rien à faire : le prix du processus et de la décision, sans modèle.
    let idle = run_child(&program, &replica.root)?;
    anyhow::ensure!(
        idle.line == "nothing",
        "l'enfant devait n'avoir rien à faire : « {} »",
        idle.line
    );

    let mut rounds = Vec::with_capacity(arrivals);
    for target in &targets {
        store.reset_vectorised()?;
        let others: Vec<MessageId> = all.iter().filter(|id| *id != target).copied().collect();
        store.mark_vectorised(&others)?;
        anyhow::ensure!(
            store.unvectorised_total()? == 1,
            "un seul message doit attendre"
        );
        let round = run_child(&program, &replica.root)?;
        anyhow::ensure!(
            round.line == "done 1 0",
            "l'enfant devait vectoriser un message : « {} »",
            round.line
        );
        rounds.push(round);
    }

    // Le chargement du modèle seul, dans ce processus, pour dire où va le temps.
    let loaded = Instant::now();
    let model = mailvec::bert::Bert::load(mailvec::bert::default_dir(&replica.root))?;
    let load = loaded.elapsed();
    drop(model);

    // **L'enfant de la recherche chaud**, le chemin du 2026-10-09 : le lot lui est confié, sans
    // processus ni chargement. Mesuré à part, parce que c'est un autre chemin et un autre coût.
    let warm = warm_rounds(&program, &replica.root, &store, &all, &targets)?;

    println!(
        "\nDécision du démon {}  (dans son processus, sans modèle)",
        ms(decision)
    );
    println!(
        "Passage à vide    {} jusqu'à la réponse, {} jusqu'à la sortie, {} de crête",
        ms(idle.to_line),
        ms(idle.to_exit),
        mib(idle.peak)
    );
    println!("\nUne arrivée       réponse      sortie       crête");
    for (index, round) in rounds.iter().enumerate() {
        println!(
            "  {}{}  {:>10}   {:>10}   {}",
            index + 1,
            if index == 0 { " (jetée)" } else { "        " },
            ms(round.to_line),
            ms(round.to_exit),
            mib(round.peak)
        );
    }
    let kept: Vec<Duration> = rounds.iter().skip(1).map(|round| round.to_exit).collect();
    let median = percentile(&kept, 0.5);
    println!("\nDont chargement du modèle, mesuré seul : {}", ms(load));
    let verdict = |value: Duration| {
        if value < ARRIVAL_BUDGET {
            "OK   "
        } else {
            "ÉCHEC"
        }
    };
    println!(
        "\n{} critère 4, enfant froid — un processus et le modèle par arrivée, médiane : {} \
         (seuil < {})",
        verdict(median),
        ms(median),
        ms(ARRIVAL_BUDGET)
    );

    println!("\nEnfant de la recherche chaud — le lot confié");
    for (index, took) in warm.iter().enumerate() {
        println!(
            "  {}{}  {:>10}",
            index + 1,
            if index == 0 { " (jeté)" } else { "       " },
            ms(*took)
        );
    }
    let warm_kept: Vec<Duration> = warm.iter().skip(1).copied().collect();
    let warm_median = percentile(&warm_kept, 0.5);
    println!(
        "\n{} critère 4, enfant chaud — dans les cinq minutes qui suivent une recherche, médiane : \
         {} (seuil < {})",
        verdict(warm_median),
        ms(warm_median),
        ms(ARRIVAL_BUDGET)
    );
    println!(
        "Ce que ça ne dit pas : plusieurs messages arrivés ensemble partagent un passage, donc un \
         chargement du modèle ; et l'enfant chaud tourne en priorité normale, la passe de fond en \
         priorité minimale."
    );
    Ok(())
}

/// Les arrivées servies par l'enfant de la recherche chaud : une par cible, chronométrée de la
/// demande du lot à sa réponse — rattrapage du magasin compris.
fn warm_rounds(
    program: &Utf8PathBuf,
    root: &Utf8PathBuf,
    store: &Store,
    all: &[MessageId],
    targets: &[MessageId],
) -> Result<Vec<Duration>> {
    let engine = maild::semantic::Engine::start(
        root,
        &maild::vectors::Launch::program(program.as_std_path()),
        maild::jobs::Activity::default(),
    );
    // L'enfant naît sur un magasin à jour, comme après une recherche ordinaire.
    store.mark_vectorised(all)?;
    let _ = engine.ranked("réveil", 10);
    let deadline = Instant::now() + Duration::from_secs(300);
    while !engine.is_ready() {
        match engine.standing() {
            Some(maild::semantic::Unavailable::Starting) | None => {}
            Some(why) => anyhow::bail!("l'enfant de la recherche a refusé : {}", why.code()),
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "l'enfant de la recherche n'a pas démarré en cinq minutes"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut out = Vec::with_capacity(targets.len());
    for target in targets {
        store.reset_vectorised()?;
        let others: Vec<MessageId> = all.iter().filter(|id| *id != target).copied().collect();
        store.mark_vectorised(&others)?;
        anyhow::ensure!(
            store.unvectorised_total()? == 1,
            "un seul message doit attendre"
        );
        let started = Instant::now();
        let outcome = engine
            .follow(8)
            .context("l'enfant chaud n'a pas fait le lot")?;
        let took = started.elapsed();
        anyhow::ensure!(
            outcome
                == maild::vectors::Outcome::Done {
                    vectorised: 1,
                    unreadable: 0
                },
            "l'enfant chaud devait vectoriser un message : {outcome:?}"
        );
        out.push(took);
    }
    Ok(out)
}

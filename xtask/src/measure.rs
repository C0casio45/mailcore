//! Mesure des critères chiffrés de `docs/PHASE-1.md`.
//!
//! - Critère 3 : RSS crête de l'import sur le corpus, plafond 500 Mo. C'est celui qui fait
//!   échouer les implémentations naïves — un `read_to_string` sur le plus gros dossier
//!   alloue 1,4 Gio d'un coup.
//! - Critère 6 : taux de dédup, mesuré et affiché.
//! - Critère 4 : latence de recherche, p95 sous 50 ms (étape 5).
//!
//! ## Comment le RSS est relevé
//!
//! Un fil dédié échantillonne la mémoire résidente du processus pendant que l'import
//! tourne, et garde le maximum. Un relevé unique en fin d'import ne mesurerait rien : la
//! crête arrive au milieu, sur le plus gros fichier, et l'allocateur a déjà rendu la
//! mémoire au moment où l'import se termine.
//!
//! Échantillonner plutôt que d'instrumenter l'allocateur : un allocateur compteur donnerait
//! la mémoire *demandée*, alors que le critère porte sur ce que le système voit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;
use mailimport::import::{ImportOptions, import_profile};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Intervalle d'échantillonnage du RSS.
///
/// 20 ms : assez fin pour attraper la crête sur un import de quelques minutes, assez large
/// pour que la mesure ne pèse pas sur ce qu'elle mesure.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(20);

/// Le plafond du critère 3.
const RSS_LIMIT: u64 = 500 * 1000 * 1000;

/// Mesure un import complet : durée, RSS crête, taux de dédup.
pub fn import(profile: &Utf8PathBuf, store_root: &Utf8PathBuf, dry_run: bool) -> Result<()> {
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let mut options = ImportOptions::new(profile.as_std_path());
    options.dry_run = dry_run;

    println!("Profil  {profile}");
    println!("Store   {store_root}");
    if dry_run {
        println!("Mode    simulation");
    }
    println!("\nimport en cours...\n");

    let sampler = RssSampler::start();
    let started = Instant::now();
    let stats = import_profile(&store, &options, &mailcore::Progress::new()).context("import")?;
    let elapsed = started.elapsed();
    let peak = sampler.stop();

    let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
    println!("Durée              {elapsed:.1?}");
    println!(
        "Débit              {}/s",
        human(((stats.raw_bytes as f64) / seconds) as u64)
    );
    println!("Messages lus       {}", stats.messages_read);
    println!("Contenus stockés   {}", stats.blobs_created);
    println!(
        "Doublons           {} — {:.1} %",
        stats.duplicates,
        stats.dedup_ratio()
    );
    println!("Références         {}", stats.refs_created);
    println!("Octets RFC 5322    {}", human(stats.raw_bytes));
    println!("Écrits sur disque  {}", human(stats.stored_bytes));
    println!("Évités par dédup   {}", human(stats.deduplicated_bytes));
    println!(
        "Dégradés / sans date  {} / {}",
        stats.degraded, stats.undated
    );

    println!("\n--- critères ---");
    verdict(
        3,
        "RSS crête de l'import",
        &human(peak),
        &format!("< {}", human(RSS_LIMIT)),
        peak < RSS_LIMIT,
    );
    verdict(
        6,
        "Taux de dédup",
        &format!("{:.1} %", stats.dedup_ratio()),
        "mesuré et affiché",
        true,
    );
    println!(
        "\nCritère 7 : à vérifier séparément — `xtask profile-snapshot` avant et après, puis\n\
         `xtask profile-diff`."
    );
    Ok(())
}

fn verdict(number: u8, label: &str, measured: &str, threshold: &str, passed: bool) {
    let mark = if passed { "OK   " } else { "ÉCHEC" };
    println!("{mark} critère {number} — {label} : {measured} (seuil {threshold})");
}

/// Échantillonne la mémoire résidente du processus dans un fil dédié.
pub(crate) struct RssSampler {
    peak: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RssSampler {
    pub(crate) fn start() -> Self {
        let peak = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_peak = Arc::clone(&peak);
        let thread_stop = Arc::clone(&stop);
        let pid = Pid::from_u32(std::process::id());

        let handle = std::thread::spawn(move || {
            let mut system = System::new();
            let refresh = ProcessRefreshKind::nothing().with_memory();
            while !thread_stop.load(Ordering::Relaxed) {
                system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh);
                if let Some(process) = system.process(pid) {
                    thread_peak.fetch_max(process.memory(), Ordering::Relaxed);
                }
                std::thread::sleep(SAMPLE_INTERVAL);
            }
        });

        Self {
            peak,
            stop,
            handle: Some(handle),
        }
    }

    pub(crate) fn stop(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // Un fil d'échantillonnage qui s'est arrêté tout seul ne doit pas faire échouer
            // la mesure qu'il vient de prendre.
            drop(handle.join());
        }
        self.peak.load(Ordering::Relaxed)
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["o", "Kio", "Mio", "Gio", "Tio"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} o")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Mesure la latence de recherche — critère 4, p95 sous 50 ms.
///
/// ## Le jeu de requêtes vient du corpus, pas de mon imagination
///
/// Des requêtes écrites à la main mesureraient surtout ma capacité à choisir des mots
/// commodes. Les termes sont donc tirés des sujets réellement présents dans le store, ce qui
/// garantit une distribution de fréquences réaliste — y compris les mots très courants, qui
/// sont les plus coûteux parce qu'ils touchent le plus de documents.
///
/// S'y ajoutent des requêtes structurées écrites à la main : phrase exacte, champ nommé,
/// booléen, négation. Elles n'ont pas la même forme de coût, et un p95 qui ne les
/// contiendrait pas mesurerait un seul type de requête.
///
/// ## Bout en bout, pas seulement l'index
///
/// Chaque itération fait ce qu'un client fait : analyser la requête, interroger tantivy,
/// **puis relire les métadonnées de chaque résultat dans SQLite**. C'est cette dernière
/// partie que le critère 4 inclut et qu'une mesure du seul index oublierait.
pub fn search(store_root: &Utf8PathBuf, repeats: usize) -> Result<()> {
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let (index, _) = mailcore::index::open_or_create(&store).context("ouverture de l'index")?;
    let searcher = mailcore::index::Searcher::open(&index).context("chercheur")?;

    let queries = build_queries(&store)?;
    println!("Store       {store_root}");
    println!("Documents   {}", searcher.document_count());
    println!("Requêtes    {} × {repeats} répétitions", queries.len());

    // Chauffe : la première requête paie l'ouverture des segments et le remplissage du
    // cache de pages. La mesurer ferait passer un coût de démarrage pour une latence.
    for query in queries.iter().take(20) {
        drop(run_once(&store, &searcher, query));
    }

    let mut latencies = Vec::with_capacity(queries.len() * repeats);
    let mut empty = 0usize;
    let mut failed = 0usize;

    for _ in 0..repeats {
        for query in &queries {
            let started = Instant::now();
            match run_once(&store, &searcher, query) {
                Ok(0) => {
                    empty += 1;
                    latencies.push(started.elapsed());
                }
                Ok(_) => latencies.push(started.elapsed()),
                Err(_) => failed += 1,
            }
        }
    }

    latencies.sort_unstable();
    let p = |q: f64| -> Duration {
        if latencies.is_empty() {
            return Duration::ZERO;
        }
        let index = ((latencies.len() as f64 - 1.0) * q).round() as usize;
        latencies[index.min(latencies.len() - 1)]
    };

    println!("\nMesures     {} requêtes exécutées", latencies.len());
    println!("Sans résultat  {empty}");
    if failed > 0 {
        println!("En échec       {failed}");
    }
    println!();
    println!("p50         {:?}", p(0.50));
    println!("p90         {:?}", p(0.90));
    println!("p95         {:?}", p(0.95));
    println!("p99         {:?}", p(0.99));
    println!("max         {:?}", p(1.00));

    let p95 = p(0.95);
    println!("\n--- critère ---");
    let mark = if p95 < SEARCH_BUDGET {
        "OK   "
    } else {
        "ÉCHEC"
    };
    println!("{mark} critère 4 — recherche p95 : {p95:?} (seuil < {SEARCH_BUDGET:?})");
    println!(
        "\nMesuré en processus, sur la machine du store. La part réseau du déploiement de\n\
         référence s'ajoute à l'étape 6 ; comparer les deux relevés dira ce qu'a coûté le\n\
         transport."
    );
    Ok(())
}

/// Le budget du critère 4 de `docs/PHASE-3.md` : une image à 60 Hz.
const COMPLETE_BUDGET: Duration = Duration::from_micros(16_700);

/// Combien de propositions un champ de destinataire affiche.
///
/// Huit : une liste plus longue ne se lit pas d'un coup d'œil, et le nombre demandé change ce
/// que la requête coûte — le mesurer sur cinquante ne dirait rien de l'usage.
const SUGGESTIONS: usize = 8;

/// Mesure la complétion d'un destinataire — le critère 4 de `docs/PHASE-3.md`.
///
/// ## Ce qui est simulé est une **frappe**, pas une requête
///
/// Un utilisateur ne tape pas « annelaure » d'un coup : il tape `a`, puis `an`, puis `ann`… et
/// chaque frappe déclenche une complétion. Le coût qui compte est donc celui de la **séquence**,
/// et surtout celui des premières lettres — un préfixe d'une lettre correspond à des centaines
/// d'adresses, et c'est le pire cas, pas le meilleur.
///
/// Mesurer sur des préfixes longs donnerait un p95 flatteur qui ne dit rien : ils ne
/// correspondent presque à rien, donc ils sont rapides.
///
/// ## Les préfixes viennent du carnet réel
///
/// Pas d'une liste écrite à la main : les adresses les mieux classées sont celles que
/// l'utilisateur va effectivement taper, et leur distribution de premières lettres est celle de
/// son corpus. Une liste inventée mesurerait un carnet imaginaire.
///
/// # Errors
///
/// Si le store est illisible, ou si le carnet est vide — auquel cas il n'y a rien à mesurer et
/// le dire vaut mieux qu'un p95 de zéro.
pub fn complete(store_root: &Utf8PathBuf, repeats: usize) -> Result<()> {
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let known = store.contact_count().context("carnet")?;
    if known == 0 {
        anyhow::bail!(
            "carnet vide : `mail contacts rebuild` d'abord. Un p95 sur zéro adresse ne \
             mesurerait rien."
        );
    }

    let prefixes = typing_sequences(&store)?;
    println!("Store        {store_root}");
    println!("Adresses     {known}");
    println!("Frappes      {} × {repeats} répétitions", prefixes.len());
    println!("Propositions {SUGGESTIONS} par frappe");

    // Chauffe : la première requête paie le remplissage du cache de pages de SQLite. La
    // mesurer ferait passer un coût de démarrage pour une latence de frappe.
    for prefix in prefixes.iter().take(30) {
        drop(store.complete(prefix, SUGGESTIONS));
    }

    let mut latencies = Vec::with_capacity(prefixes.len() * repeats);
    let mut empty = 0_usize;
    let mut widest = (0_usize, String::new());

    for _ in 0..repeats {
        for prefix in &prefixes {
            let started = Instant::now();
            let found = store.complete(prefix, SUGGESTIONS)?;
            latencies.push(started.elapsed());
            if found.is_empty() {
                empty += 1;
            }
        }
    }

    // Le contrôle : combien d'adresses le pire préfixe touche vraiment. Un p95 flatteur obtenu
    // sur des préfixes qui ne correspondent à rien serait indiscernable d'un vrai, et ce
    // nombre-là les distingue.
    for prefix in &prefixes {
        let touched = store.complete(prefix, usize::MAX)?.len();
        if touched > widest.0 {
            widest = (touched, prefix.clone());
        }
    }

    latencies.sort_unstable();
    let p = |q: f64| -> Duration {
        if latencies.is_empty() {
            return Duration::ZERO;
        }
        let index = ((latencies.len() as f64 - 1.0) * q).round() as usize;
        latencies[index.min(latencies.len() - 1)]
    };

    println!("\nMesures      {} frappes", latencies.len());
    println!("Sans résultat {empty}");
    println!(
        "Pire préfixe  « {} » touche {} adresses",
        widest.1, widest.0
    );
    println!();
    println!("p50          {:?}", p(0.50));
    println!("p90          {:?}", p(0.90));
    println!("p95          {:?}", p(0.95));
    println!("p99          {:?}", p(0.99));
    println!("max          {:?}", p(1.00));

    let p95 = p(0.95);
    println!("\n--- critère ---");
    let mark = if p95 < COMPLETE_BUDGET {
        "OK   "
    } else {
        "ÉCHEC"
    };
    println!("{mark} critère 4 — complétion p95 : {p95:?} (seuil < {COMPLETE_BUDGET:?})");
    if widest.0 < 50 {
        println!(
            "\nATTENTION : le pire préfixe ne touche que {} adresses. Le relevé ne mesure \
             probablement pas le cas difficile.",
            widest.0
        );
    }
    Ok(())
}

/// Les préfixes qu'un utilisateur taperait, dérivés du carnet.
///
/// Pour chacune des adresses les mieux classées, toutes les frappes de son nom puis de son
/// adresse : `a`, `an`, `ann`… C'est la séquence réelle, et elle met les préfixes courts — les
/// coûteux — en majorité, ce qui est le bon biais.
fn typing_sequences(store: &Store) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for contact in store.top_contacts(20).context("carnet")? {
        for source in [contact.name.clone(), Some(contact.address.clone())]
            .into_iter()
            .flatten()
        {
            let lowered = source.to_lowercase();
            // Jusqu'à six lettres : au-delà, un préfixe ne correspond plus qu'à une adresse et
            // ne mesure plus rien. Sur les **frontières de caractère**, sinon une lettre
            // accentuée coupée en deux paniquerait.
            let mut taken = 0;
            for (index, _) in lowered.char_indices().skip(1) {
                out.push(lowered[..index].to_owned());
                taken += 1;
                if taken >= 6 {
                    break;
                }
            }
            out.push(lowered);
        }
    }
    out.sort();
    out.dedup();
    anyhow::ensure!(!out.is_empty(), "aucune adresse classée : rien à mesurer");
    Ok(out)
}

/// Le budget du critère 4.
const SEARCH_BUDGET: Duration = Duration::from_millis(50);

/// Nombre de résultats demandés par requête — une page d'écran.
const PAGE: usize = 50;

/// Une recherche complète : index, puis métadonnées. Rend le nombre de résultats.
fn run_once(
    store: &Store,
    searcher: &mailcore::index::Searcher,
    query: &str,
) -> mailcore::Result<usize> {
    let hits = searcher.search(query, PAGE)?;
    for hit in &hits {
        drop(store.message(hit.id)?);
    }
    Ok(hits.len())
}

/// Construit le jeu de requêtes : termes tirés du corpus + formes structurées.
pub fn build_queries(store: &Store) -> Result<Vec<String>> {
    let mut terms: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for folder in store.folders().context("dossiers")? {
        let page = store.page(folder.id, None, 200).context("page")?;
        for item in page {
            for word in item.subject.split(|c: char| !c.is_alphanumeric()) {
                let word = word.to_lowercase();
                if word.chars().count() >= 4 && seen.insert(word.clone()) {
                    terms.push(word);
                }
            }
        }
    }
    terms.truncate(400);

    // Des formes que les termes seuls ne couvrent pas. Chacune a un profil de coût différent
    // — une phrase exacte lit des positions, un booléen intersecte deux listes de postings.
    let structured = [
        "facture",
        "\"mot de passe\"",
        "subject:facture",
        "from:noreply",
        "facture AND devis",
        "facture OR commande",
        "facture -spam",
        "fact*",
    ];
    terms.extend(structured.into_iter().map(str::to_owned));

    anyhow::ensure!(
        terms.len() > 50,
        "jeu de requêtes trop maigre ({}) : le store est-il indexé ?",
        terms.len()
    );
    Ok(terms)
}

/// Le seuil du critère 3 : une synchronisation incrémentale où rien n'a changé.
const INCREMENTAL_BUDGET: Duration = Duration::from_secs(5);

/// Mesure le critère 3 : une passe qui ne trouve rien à faire, compte par compte.
///
/// ## Pourquoi cinq passes et pas une
///
/// Parce que Gmail est bimodal. Mesuré le 2026-09-08 sur `compte-b` : 9 135, 2 335, 9 166,
/// 2 310, 2 520 ms d'affilée, sans que rien change de notre côté. Une passe unique aurait dit
/// « 9 s » ou « 2 s » selon le tirage, et les deux auraient été présentées comme la vérité.
///
/// La dispersion se lit **avant** la médiane, comme partout ailleurs dans ce projet : deux
/// exécutions identiques qui s'écartent d'un facteur quatre ne sont pas du bruit, elles sont
/// un symptôme — ici, celui du serveur d'en face.
///
/// ## Ce que la première passe mesure, et qu'il ne faut pas moyenner avec le reste
///
/// La première passe d'un compte porte l'établissement de la connexion TLS, le rafraîchissement
/// éventuel du jeton OAuth2, et le premier accès au store. Elle est affichée séparément et
/// n'entre pas dans la médiane : la mélanger reviendrait à mesurer un démarrage à froid en le
/// présentant comme un régime établi.
///
/// # Errors
///
/// Si le store est illisible, ou si le binaire `mail` ne peut pas être lancé.
pub fn sync(store_root: &Utf8PathBuf, passes: usize, release: bool) -> Result<()> {
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let accounts: Vec<(i64, String)> = store
        .full_accounts()?
        .into_iter()
        .filter(|it| it.kind == mailcore::AccountKind::Imap && it.enabled)
        .map(|it| (it.id.0, it.display_name))
        .collect();
    drop(store);

    if accounts.is_empty() {
        anyhow::bail!("aucun compte IMAP actif dans {store_root}");
    }
    if passes == 0 {
        anyhow::bail!("il faut au moins une passe");
    }

    let binary = binary(release)?;
    println!(
        "Critère 3 — synchronisation incrémentale, {passes} passes par compte, binaire {}",
        if release { "release" } else { "de débogage" }
    );
    println!("Store : {store_root}\n");

    let mut worst: Option<(String, Duration)> = None;
    for (id, name) in &accounts {
        let mut timings = Vec::with_capacity(passes);
        for _ in 0..passes {
            let started = Instant::now();
            let output = std::process::Command::new(&binary)
                .args(["--store", store_root.as_str(), "sync", "--account"])
                .arg(id.to_string())
                .output()
                .with_context(|| format!("lancement de {binary}"))?;
            let took = started.elapsed();
            if !output.status.success() {
                // Un compte qui refuse de se synchroniser n'est pas une mesure lente, c'est
                // une mesure absente. La confondre avec un dépassement de seuil ferait croire
                // à un problème de performance là où il y a un problème d'authentification.
                anyhow::bail!(
                    "compte #{id} en échec : {}",
                    String::from_utf8_lossy(&output.stdout).trim()
                );
            }
            timings.push(took);
        }

        // Le premier relevé porte la connexion et le jeton : affiché, jamais moyenné.
        let (first, established) = timings.split_first().unwrap_or((&timings[0], &[]));
        let mut sorted: Vec<Duration> = if established.is_empty() {
            vec![*first]
        } else {
            established.to_vec()
        };
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2];

        let all: Vec<String> = timings
            .iter()
            .map(|it| it.as_millis().to_string())
            .collect();
        let mark = if median <= INCREMENTAL_BUDGET {
            "OK   "
        } else {
            "ÉCHEC"
        };
        println!(
            "{mark} #{id} {name}\n      passes {} ms — première {} ms, médiane des suivantes \
             {} ms",
            all.join(" "),
            first.as_millis(),
            median.as_millis(),
        );
        if worst.as_ref().is_none_or(|(_, it)| median > *it) {
            worst = Some((name.clone(), median));
        }
    }

    match worst {
        Some((name, median)) if median <= INCREMENTAL_BUDGET => println!(
            "\nOK    critère 3 — le pire compte est {name} à {} ms (seuil {} ms)",
            median.as_millis(),
            INCREMENTAL_BUDGET.as_millis()
        ),
        Some((name, median)) => println!(
            "\nÉCHEC critère 3 — {name} tient {} ms (seuil {} ms)",
            median.as_millis(),
            INCREMENTAL_BUDGET.as_millis()
        ),
        None => {}
    }
    println!(
        "\nLa moitié « 0 octet écrit » du critère se lit dans la sortie de `mail sync` :\n\
         « Corps reçus 0 », et les dossiers annoncés déjà à jour."
    );
    Ok(())
}

/// Le binaire `mail` à lancer, construit ou non.
///
/// **Le harnais ne compile pas.** La règle du `CLAUDE.md` est explicite : une mesure prise sur
/// une machine qui vient de compiler a faussé les relevés du 2026-09-02 d'un facteur trois.
/// Le binaire doit exister avant, et l'absence est une erreur, pas une invitation à le
/// construire ici.
fn binary(release: bool) -> Result<Utf8PathBuf> {
    let profile = if release { "release" } else { "debug" };
    let name = if cfg!(windows) { "mail.exe" } else { "mail" };
    let path = Utf8PathBuf::from(format!("target/{profile}/{name}"));
    if !path.exists() {
        anyhow::bail!(
            "{path} absent — le construire d'abord, puis laisser la machine retomber au repos \
             avant de mesurer"
        );
    }
    Ok(path)
}

/// Mesure la **crête mémoire d'une moisson complète** sur un vrai compte — critère 2.
///
/// ## La réserve que ça lève
///
/// Le critère 2 est passé depuis le 2026-09-08, mais avec une réserve nommée : « ce qui n'est
/// pas mesuré, et il faut le dire : la crête d'une **moisson complète** sur un vrai compte. Il
/// faudrait retélécharger les 6,3 Gio, et le correctif de reprise du même jour rend ce
/// retéléchargement impossible à provoquer sans casser le store ». Le lot de cent messages
/// borne la mémoire par construction, mais c'était « un argument de conception, pas un relevé ».
///
/// Un **store jetable** lève la réserve sans rien casser, exactement comme pour le critère 4 :
/// le compte y est déclaré avec le même hôte et le même identifiant, donc le trousseau retrouve
/// son secret, et le serveur est lu comme d'habitude. Le store réel n'est ouvert que pour y lire
/// le compte.
///
/// ## `--messages` borne la dépense, pas la validité
///
/// La moisson s'arrête dès que `messages` contenus sont écrits. Ce n'est pas une concession sur
/// la mesure : ce que le critère interroge est **si la mémoire croît avec le corpus**, et un lot
/// de cent messages qui tient sa promesse la tient au dixième comme au dix-millième. Une crête
/// plate sur les premiers milliers de messages est la réponse ; une crête qui monte le serait
/// aussi, et bien plus vite.
///
/// Ce qui reste hors de portée est un compte entier téléchargé pour de bon — 6,3 Gio de bande
/// passante pour un chiffre qu'on lit déjà sur la pente.
///
/// # Errors
///
/// Si le store est illisible, si le compte n'existe pas, ou si la moisson échoue.
pub fn harvest_rss(store_root: &Utf8PathBuf, account: i64, messages: u64) -> Result<()> {
    anyhow::ensure!(messages > 0, "il faut au moins un message à moissonner");

    let target = {
        let store =
            Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
        store
            .full_accounts()?
            .into_iter()
            .find(|it| it.id.0 == account && it.server.is_some())
            .with_context(|| format!("aucun compte IMAP portant le numéro {account}"))?
    };
    let server = target
        .server
        .as_ref()
        .context("compte sans serveur")?
        .clone();

    let dir = tempfile::tempdir().context("répertoire de travail")?;
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
        .map_err(|it| anyhow::anyhow!("chemin non UTF-8 : {}", it.display()))?;
    {
        let store = Store::open(&root).context("ouverture du store jetable")?;
        let writer = store.writer()?;
        writer.upsert_imap_account(&target.display_name, &server)?;
        writer.commit()?;
    }

    println!("Critère 2 — crête mémoire d'une moisson complète, sur un vrai serveur");
    println!("Compte      #{} {}", target.id.0, target.display_name);
    println!("Store réel  {store_root} — lu pour trouver le compte, jamais écrit");
    println!("Cible       {messages} contenus, puis la moisson est annulée\n");

    let secret = mailauth::session::secret_for(
        &server.host,
        &server.username,
        server.auth.as_str(),
        now_seconds(),
    )
    .with_context(|| format!("secret du compte {}", server.username))?;

    // **L'échantillonneur démarre avant la connexion**, donc avant `rustls` et avant la première
    // allocation de la moisson. Démarrer après raterait précisément ce que la poignée de main
    // TLS et le premier lot demandent.
    let sampler = RssSampler::start();
    let at_rest = current_rss();

    let progress = std::sync::Arc::new(mailcore::Progress::new());
    let watcher_progress = std::sync::Arc::clone(&progress);
    let watcher_root = root.clone();
    // Un fil surveille le compteur du store et annule quand la cible est atteinte. Le faire
    // depuis la moisson demanderait de lui passer un plafond, c'est-à-dire d'ajouter au produit
    // un réglage qui n'existe que pour ce banc.
    let watcher = std::thread::spawn(move || {
        let deadline = Instant::now() + HARVEST_PATIENCE;
        loop {
            if Instant::now() > deadline {
                watcher_progress.cancel();
                return;
            }
            if let Ok(store) = Store::open(&watcher_root)
                && store.stats().is_ok_and(|it| it.messages >= messages)
            {
                watcher_progress.cancel();
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });

    let started = Instant::now();
    let report = {
        let store = Store::open(&root)?;
        let account = store
            .full_accounts()?
            .into_iter()
            .next()
            .context("le compte a disparu du store jetable")?;
        let credential = mailsync::Credential::for_auth(server.auth, &secret);
        mailsync::sync_account(&store, &account, credential, &progress).context("moisson")?
    };
    let elapsed = started.elapsed();
    progress.cancel();
    drop(watcher.join());
    let peak = sampler.stop();

    let written = Store::open(&root)?.stats()?;
    println!("Durée              {elapsed:.1?}");
    println!(
        "Dossiers           {} ({} évités)",
        report.folders, report.skipped
    );
    println!("Corps reçus        {}", report.fetched);
    println!("Contenus écrits    {}", written.messages);
    println!("Octets RFC 5322    {}", human(written.raw_bytes));
    println!("RSS au repos       {}", human(at_rest));
    println!();

    // **Le contrôle du banc.** Une crête relevée sur une moisson qui n'a rien téléchargé serait
    // celle d'un processus au repos, et elle passerait le seuil sans rien dire.
    anyhow::ensure!(
        report.fetched > 0,
        "aucun corps téléchargé : la crête relevée serait celle d'un processus au repos, et \
         elle ne mesurerait pas le critère 2"
    );

    verdict(
        2,
        "RSS crête d'une moisson complète",
        &human(peak),
        &format!("< {}", human(RSS_LIMIT)),
        peak < RSS_LIMIT,
    );
    println!(
        "\nCe que ça ne dit pas : la crête d'un compte **entier**. Ce qui est mesuré est la\n\
         pente — la mémoire d'un lot de cent messages ne dépend pas du nombre de lots."
    );
    Ok(())
}

/// Au-delà, la cible ne sera pas atteinte : on annule et on relève ce qu'on a.
///
/// Cinq minutes. Un compte sur une liaison lente peut ne pas écrire mille contenus dans ce
/// temps-là, et un banc qui ne rend jamais la main est un banc qu'on n'exécute pas.
const HARVEST_PATIENCE: Duration = Duration::from_secs(300);

/// L'horloge, en secondes Unix.
fn now_seconds() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |it| it.as_secs()),
    )
    .unwrap_or(i64::MAX)
}

/// La mémoire résidente du processus, maintenant.
///
/// Sert de point de comparaison : une crête ne veut rien dire sans savoir de quoi elle part.
fn current_rss() -> u64 {
    let mut system = System::new();
    let pid = Pid::from_u32(std::process::id());
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    system.process(pid).map_or(0, sysinfo::Process::memory)
}

/// Le budget du critère 3 de `docs/PHASE-3.md` : la crête de mémoire résidente.
const ATTACH_RSS_BUDGET: u64 = 200 * 1024 * 1024;

/// Mesure l'assemblage et la remise d'un message à grosses pièces jointes — critère 3.
///
/// ## Ce qui est mesuré, et pourquoi c'est le RSS et pas autre chose
///
/// Un tas alloué puis libéré ne se voit pas dans un compteur d'allocations cumulées, et un
/// compteur de crête d'allocations ne dit rien de ce que le système a réellement dû fournir.
/// La mémoire **résidente** est ce que la machine paie : c'est elle que le critère borne, et
/// c'est elle qu'un utilisateur voit dans son gestionnaire de tâches.
///
/// ## Le contrôle qui rend la mesure interprétable
///
/// Le relevé affiche la taille du message **à côté** de la crête. Une crête de 40 Mo sur un
/// message de 34 Mo dit que le message est en mémoire ; la même crête sur un message de 340 Mo
/// dit qu'il ne l'est pas. Sans la taille, un chiffre sous le seuil serait indiscernable d'un
/// chiffre obtenu sur un message minuscule.
///
/// La borne est donc **relative autant qu'absolue** : la mesure échoue bruyamment si la crête
/// dépasse le tiers de la taille du message, quel que soit le seuil absolu.
///
/// ## Ce qui n'est pas mesuré
///
/// Le réseau. La remise va dans un puits local — un fichier temporaire — parce que mesurer
/// contre un vrai serveur mesurerait la liaison montante, pas la mémoire. Le chemin traversé
/// est le même : `Draft::write_to`, le magasin de blobs, `Stuffing`.
///
/// # Errors
///
/// Si le store est illisible, ou si l'assemblage échoue.
pub fn attachment(megabytes: u64) -> Result<()> {
    use mailsmtp::compose::{Address, Attachment, Draft};

    // **Un store jetable, et pas celui de l'utilisateur.**
    //
    // La première version prenait un `--store` et j'y ai visé le store réel : elle a laissé
    // deux lignes de file et 533 Mo de blobs orphelins dans les données de production. Une
    // mesure qui écrit n'a rien à faire dans un store qu'on garde, et le seul moyen sûr de
    // s'en assurer est de ne pas lui laisser le choix — d'où l'absence de paramètre.
    //
    // Le temporaire est supprimé à la fin de la fonction, y compris sur erreur : c'est son
    // destructeur qui le fait.
    let dir = tempfile::tempdir().context("répertoire temporaire")?;
    let store_root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
        .map_err(|path| anyhow::anyhow!("chemin temporaire non UTF-8 : {}", path.display()))?;
    let store = Store::open(&store_root).with_context(|| format!("ouverture de {store_root}"))?;
    // Un compte, pour que la mise en file ait une clé étrangère valide.
    {
        let writer = store.writer().context("écriture")?;
        writer
            .upsert_account("imap", "mesure")
            .context("compte de mesure")?;
        writer.commit().context("validation")?;
    }
    let bytes = megabytes.saturating_mul(1024 * 1024);

    println!("Store        {store_root} (jetable)");
    println!("Pièce jointe {}", human(bytes));

    let before = current_rss();
    println!("RSS au repos {}", human(before));

    // La pièce jointe est fabriquée **en flux** elle aussi : l'écrire d'abord dans un `Vec`
    // ferait payer à la mesure ce qu'elle cherche à mesurer.
    let staged = Instant::now();
    let put = store
        .blobs()
        .put_reader(&mut Noise::new(bytes))
        .context("mise au magasin de la pièce jointe")?;
    let after_staging = current_rss();
    println!(
        "Rangement    {:.1}s, RSS {}",
        staged.elapsed().as_secs_f64(),
        human(after_staging)
    );

    let from = Address::parse("marie@exemple.fr", None).context("adresse")?;
    let to = Address::parse("jean@ailleurs.fr", None).context("adresse")?;
    let mut draft = Draft::new(from, vec![to], "Mesure du critère 3", "Le corps.\r\n");
    draft.attachments.push(Attachment {
        filename: "mesure.bin".to_owned(),
        mime: "application/octet-stream".to_owned(),
        blob: put.hash,
        size: bytes,
    });

    // 1. L'assemblage et la mise en file.
    let assembling = Instant::now();
    let (id, size) =
        mailsmtp::queue::stage(&store, first_account(&store)?, &draft, now_seconds(), 0)
            .context("mise en file")?;
    let after_stage = current_rss();
    println!(
        "Assemblage   {:.1}s, {} écrits, RSS {}",
        assembling.elapsed().as_secs_f64(),
        human(size),
        human(after_stage)
    );

    // 2. La remise, vers un puits local.
    let job = store
        .outgoing(id)?
        .context("la ligne de file vient de disparaître")?;
    let mut sink = Discarding::default();
    let delivering = Instant::now();
    mailsmtp::queue::deliver_one(&store, &mut sink, &job, now_seconds()).context("remise")?;
    let after_delivery = current_rss();
    println!(
        "Remise       {:.1}s, {} lus, RSS {}",
        delivering.elapsed().as_secs_f64(),
        human(sink.written),
        human(after_delivery)
    );

    let peak = after_staging.max(after_stage).max(after_delivery);
    let growth = peak.saturating_sub(before);
    println!();
    println!("Crête        {}", human(peak));
    println!("Croissance   {} depuis le repos", human(growth));

    // Le contrôle : ce que le transport a réellement lu doit être la taille du message. Un
    // relevé flatteur obtenu sur un message qui n'a pas été assemblé serait indiscernable.
    println!();
    println!("--- contrôles ---");
    println!(
        "{} le transport a lu {} pour un message de {}",
        if sink.written == size {
            "OK   "
        } else {
            "ÉCHEC"
        },
        human(sink.written),
        human(size)
    );
    let third = size / 3;
    println!(
        "{} la croissance ({}) est sous le tiers du message ({})",
        if growth < third { "OK   " } else { "ÉCHEC" },
        human(growth),
        human(third)
    );

    println!();
    println!("--- critère ---");
    let passed = peak < ATTACH_RSS_BUDGET && sink.written == size && growth < third;
    verdict(
        3,
        "pièces jointes streamées",
        &human(peak),
        &format!("< {}", human(ATTACH_RSS_BUDGET)),
        passed,
    );
    Ok(())
}

/// Le premier compte du store, pour avoir une clé étrangère valide.
fn first_account(store: &Store) -> Result<mailcore::AccountId> {
    store
        .accounts()?
        .first()
        .map(|(id, _, _)| *id)
        .context("aucun compte dans ce store : la mise en file a besoin d'un compte")
}

/// Un flux d'octets pseudo-aléatoires, de longueur bornée.
///
/// Pseudo-aléatoires et non constants : un contenu répétitif se compresse à presque rien, donc
/// le magasin de blobs n'écrirait rien et la mesure ne dirait rien du disque. Ceux-là ne se
/// compressent pas.
struct Noise {
    left: u64,
    seed: u32,
}

impl Noise {
    const fn new(bytes: u64) -> Self {
        Self {
            left: bytes,
            seed: 0x1234_5678,
        }
    }
}

impl std::io::Read for Noise {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let take = usize::try_from(self.left)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        for slot in &mut buffer[..take] {
            self.seed = self
                .seed
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            *slot = (self.seed >> 16) as u8;
        }
        self.left -= take as u64;
        Ok(take)
    }
}

/// Un transport qui compte ce qu'on lui donne et le jette.
///
/// Il traverse **tout** le chemin d'assemblage et de doublage, et s'arrête au socket. Mesurer
/// contre un vrai serveur mesurerait la liaison montante.
#[derive(Debug, Default)]
struct Discarding {
    written: u64,
}

impl mailsmtp::queue::Transport for Discarding {
    fn deliver(
        &mut self,
        _job: &mailcore::Outgoing,
        body: &mut dyn std::io::Read,
        frontier: &mut dyn FnMut() -> mailsmtp::Result<()>,
    ) -> mailsmtp::Result<()> {
        frontier()?;
        // Un tampon de 8 Kio, comme un socket : lire d'un coup ferait exactement ce que la
        // mesure cherche à interdire.
        let mut buffer = vec![0_u8; 8 * 1024];
        loop {
            let read = body
                .read(&mut buffer)
                .map_err(|source| mailsmtp::Error::Network {
                    stage: mailsmtp::Stage::Data,
                    source,
                })?;
            if read == 0 {
                break;
            }
            self.written = self.written.saturating_add(read as u64);
        }
        Ok(())
    }
}

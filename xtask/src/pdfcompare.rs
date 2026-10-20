//! Le banc A2 du PDF : trois extracteurs, les mêmes documents, le même harnais — conception du
//! 2026-10-20, §1 (`docs/PHASE-4.md`).
//!
//! ## Ce qu'il mesure, et contre quoi
//!
//! La métrique primaire est la part des termes de la vérité terrain — lus **à l'œil** dans chaque
//! document, gelés avant la première exécution — retrouvés dans le texte d'un candidat **comme
//! l'index les verrait** : une suite contiguë de jetons de l'analyseur du champ `body`, celui que
//! le schéma livré déclare, jamais une copie de ses règles. pdf-inspector est comparé à
//! pdf-extract document par document, et le test de signe dit si la différence dépasse le tirage.
//! lopdf brut sert de plancher : c'est l'extracteur qui colle les mots.
//!
//! Le texte jugé est celui que le lecteur rend, plafonné à `BUDGET` : aucune règle « pas de
//! texte » propre à un candidat n'est appliquée, faute d'en avoir une d'écrite avant le choix.
//!
//! Tout le reste — U+FFFD, mojibake, zone privée, jetons perdus par l'analyseur, lettres isolées,
//! accord entre candidats — est un **diagnostic** : il désigne des documents à regarder, il ne
//! décide rien. La règle de décision est gelée avec la vérité terrain, pas ici.
//!
//! ## Pourquoi il refuse avant de lire le corpus
//!
//! Le premier relevé d'un banc neuf accuse le banc. Avant le premier document réel, deux PDF
//! fabriqués passent par le chemin normal : le positif doit être retrouvé par les trois, et le
//! câblage doit montrer que lopdf colle là où pdf-inspector sépare — sinon les deux noms
//! désignent la même bibliothèque, et le comparatif opposerait un candidat à lui-même. Et chaque
//! réponse porte la longueur et l'empreinte de ce que l'enfant a reçu : un écart est une erreur
//! **du banc**, qui arrête tout, jamais une issue du candidat.
//!
//! ## Un enfant par document
//!
//! Le lecteur (`tools/pdf-runner`) est hors de l'espace de travail, pour que les perdants
//! n'entrent pas dans `Cargo.lock`. Chaque document est lu par un processus neuf, avec le même
//! environnement pour tous — un seul fil rayon, aucune des variables que pdf-inspector lit, pas le
//! jeton du démon —, en priorité normale. Le parent échantillonne la mémoire et le temps
//! processeur de l'enfant toutes les [`SAMPLE_EVERY`], et le tue à [`LIMITS`] : trente secondes
//! de **processeur** accusent le candidat, dix minutes d'**horloge** n'accusent personne — une
//! machine chargée ralentit l'horloge, pas le travail.
//!
//! ## Ce qui s'imprime
//!
//! Des comptes et des centiles, rien d'autre : ni texte extrait, ni nom de fichier, ni sujet. Un
//! document se désigne par son numéro dans le fichier de termes et les huit premiers caractères de
//! son empreinte. Rien n'est écrit ni dans le dépôt ni sous `measurements/`, et aucun document
//! décodé ne touche le disque : les octets vont du blob à l'entrée standard de l'enfant.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use mailcore::attachtext::{BUDGET, pieces};
use serde::Deserialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tantivy::tokenizer::{SimpleTokenizer, TextAnalyzer, TokenStream};

use crate::pdf::{Skeleton, survey, with_replica};

/// Ce qu'on demande au banc.
#[derive(Debug)]
pub struct Options {
    /// Le store où lire les PDF — sur une réplique.
    pub store: Utf8PathBuf,
    /// Le lecteur de candidats, construit dans `tools/pdf-runner`.
    pub runner: Utf8PathBuf,
    /// La vérité terrain gelée, au format du squelette de `pdf-sample`.
    pub terms: Utf8PathBuf,
    /// Combien d'exécutions complètes ; la première est jetée pour les temps.
    pub runs: usize,
    /// `decision` ou `reserve`.
    pub set: String,
}

/// Le lecteur tel que `cargo build --release` le dépose dans `tools/pdf-runner`.
pub const DEFAULT_RUNNER: &str = if cfg!(windows) {
    "tools/pdf-runner/target/release/pdf-runner.exe"
} else {
    "tools/pdf-runner/target/release/pdf-runner"
};

/// Les bornes d'un enfant.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Temps processeur de l'enfant : au-delà, le candidat est en cause.
    cpu_ms: u64,
    /// Temps d'horloge : au-delà, l'enfant est tué **sans** que rien soit accusé.
    wall: Duration,
    /// Mémoire comptée par [`charged`].
    memory: u64,
}

const LIMITS: Limits = Limits {
    cpu_ms: 30_000,
    wall: Duration::from_secs(600),
    memory: 2 * 1024 * 1024 * 1024,
};

/// Le pas d'échantillonnage de l'enfant. L'intervalle **réel** est imprimé : sous Windows, chaque
/// relevé de sysinfo photographie la liste des processus, et le pas tenu peut être plus long.
const SAMPLE_EVERY: Duration = Duration::from_millis(2);

/// La borne de lecture de la sortie de l'enfant : le texte plafonné à `BUDGET`, que JSON peut
/// sextupler — un caractère de contrôle s'écrit `\u00XX` —, plus de quoi porter les autres champs.
const LINE_CAP: usize = 6 * BUDGET + 64 * 1024;

/// La borne de lecture d'un PDF fabriqué : quelques centaines d'octets attendus.
const FIXTURE_CAP: usize = 1024 * 1024;

/// Deux candidats qui rendent des sorties identiques à l'octet sur plus de cette part des documents
/// sont probablement **le même** candidat sous deux noms.
const IDENTICAL_ALARM: f64 = 0.05;

/// Ce que porte la fixture positive, en clair.
const POSITIVE: &str = "Électricité facture 1234,56";

/// Sous cet accord de Jaccard entre pdf-inspector et pdf-extract, un document est désigné pour être
/// regardé. Un seuil de désignation, pas de décision.
const DISAGREEMENT: f64 = 0.5;

// --- Les candidats et les issues ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Candidate {
    Inspector,
    Extract,
    Lopdf,
}

const CANDIDATES: [Candidate; 3] = [Candidate::Inspector, Candidate::Extract, Candidate::Lopdf];

impl Candidate {
    /// Le nom que le lecteur attend en argument.
    const fn name(self) -> &'static str {
        match self {
            Self::Inspector => "inspector",
            Self::Extract => "extract",
            Self::Lopdf => "lopdf",
        }
    }
}

/// Pourquoi le parent a tué un enfant. La raison est posée **avant** le geste : un enfant tué sans
/// raison écrite serait compté comme une mort spontanée, donc accusé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kill {
    Cpu,
    Memory,
    Wall,
}

/// L'issue d'un document pour un candidat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Outcome {
    /// Le lecteur a rendu un texte, éventuellement vide.
    Text,
    /// Le document ne s'ouvre pas, ou aucune de ses pages ne se lit.
    Error,
    /// Une panique, rattrapée par le lecteur.
    Panic,
    /// Plus que ce que le lecteur accepte.
    TooBig,
    /// Tué au-delà du budget de **processeur**.
    Timeout,
    /// Tué au-delà de la borne de mémoire.
    Memory,
    /// Mort sans ligne de réponse valable.
    Crash,
    /// Tué au-delà du filet d'**horloge** : n'accuse pas le candidat.
    Wall,
}

impl Outcome {
    const ALL: [Self; 8] = [
        Self::Text,
        Self::Error,
        Self::Panic,
        Self::TooBig,
        Self::Timeout,
        Self::Memory,
        Self::Crash,
        Self::Wall,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Text => "texte",
            Self::Error => "erreur",
            Self::Panic => "panique",
            Self::TooBig => "trop gros",
            Self::Timeout => "délai CPU",
            Self::Memory => "mémoire",
            Self::Crash => "mort",
            Self::Wall => "horloge",
        }
    }

    /// Le candidat est-il en cause ? Tout sauf un texte et le filet d'horloge.
    const fn accuses(self) -> bool {
        !matches!(self, Self::Text | Self::Wall)
    }
}

/// L'issue telle que le lecteur l'écrit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Said {
    Text,
    Error,
    Panic,
    TooBig,
}

/// La ligne de réponse du lecteur — voir `tools/pdf-runner/src/main.rs`, dont c'est le miroir. Un
/// champ inconnu est refusé : un lecteur d'une autre version ne passerait pas les contrôles.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    candidate: String,
    received: u64,
    hash: String,
    outcome: Said,
    text: String,
    pages: u32,
    pages_failed: u32,
    wall_us: u64,
    cpu_ms: u64,
}

// --- La lecture de la sortie, le transport, le classement ----------------------------------------

/// Ce que l'enfant a écrit sur sa sortie, lu avec une borne.
#[derive(Debug)]
enum Written {
    Bytes(Vec<u8>),
    /// Au-delà de la borne : rien n'en est gardé.
    Overlong,
    /// La lecture a échoué.
    Unreadable,
}

/// Lit `reader` jusqu'au bout, en gardant au plus `cap` octets. Le reste est **lu et jeté** : un
/// enfant qui écrit trop ne doit pas rester bloqué sur un tube plein, il doit finir pour qu'on le
/// classe.
fn read_bounded(mut reader: impl Read, cap: usize) -> Written {
    let mut bytes = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    let read = (&mut reader).take(limit).read_to_end(&mut bytes);
    let _ = std::io::copy(&mut reader, &mut std::io::sink());
    match read {
        Err(_) => Written::Unreadable,
        Ok(_) if bytes.len() > cap => Written::Overlong,
        Ok(_) => Written::Bytes(bytes),
    }
}

/// La réponse, si la sortie est **exactement** une ligne JSON du protocole. Une sortie qui en porte
/// deux, ou du texte avant, n'est pas une réponse : le protocole dit « rien d'autre ».
fn answer_of(written: &Written) -> Option<Answer> {
    let Written::Bytes(bytes) = written else {
        return None;
    };
    let line = bytes.strip_suffix(b"\n")?;
    if line.contains(&b'\n') {
        return None;
    }
    serde_json::from_slice(line).ok()
}

/// Le contrôle de transport : l'enfant a-t-il reçu exactement ce qu'on lui a envoyé ?
///
/// # Errors
///
/// Si la longueur ou l'empreinte diffèrent — une erreur du banc, qui arrête tout.
fn transport(answer: &Answer, sent: &[u8]) -> Result<()> {
    let length = u64::try_from(sent.len()).unwrap_or(u64::MAX);
    anyhow::ensure!(
        answer.received == length,
        "transport : l'enfant a reçu {} octets, {length} envoyés — erreur du banc, pas du candidat",
        answer.received
    );
    let expected = blake3::hash(sent).to_hex();
    anyhow::ensure!(
        answer.hash == expected.as_str(),
        "transport : l'empreinte reçue par l'enfant n'est pas celle envoyée — erreur du banc, \
         pas du candidat"
    );
    Ok(())
}

/// Le classement d'une exécution d'enfant.
#[derive(Debug)]
struct Classified {
    outcome: Outcome,
    answer: Option<Answer>,
}

/// Classe une exécution. **Un seul endroit décide**, et dans cet ordre :
///
/// 1. une raison de tuer posée par le parent l'emporte sur tout, même sur une réponse écrite juste
///    avant le geste ;
/// 2. sans elle, une sortie qui n'est pas une ligne de réponse est une mort ;
/// 3. une réponse dont le transport, le candidat ou le plafond de texte ne sont pas ceux attendus
///    est une erreur **du banc**.
///
/// # Errors
///
/// Au point 3.
fn classify(
    candidate: Candidate,
    kill: Option<Kill>,
    written: &Written,
    sent: &[u8],
) -> Result<Classified> {
    if let Some(kill) = kill {
        let outcome = match kill {
            Kill::Cpu => Outcome::Timeout,
            Kill::Memory => Outcome::Memory,
            Kill::Wall => Outcome::Wall,
        };
        return Ok(Classified {
            outcome,
            answer: None,
        });
    }
    let Some(answer) = answer_of(written) else {
        return Ok(Classified {
            outcome: Outcome::Crash,
            answer: None,
        });
    };
    transport(&answer, sent)?;
    anyhow::ensure!(
        answer.candidate == candidate.name(),
        "le lecteur répond pour « {} » à une demande « {} » : erreur du banc",
        answer.candidate,
        candidate.name()
    );
    anyhow::ensure!(
        answer.text.len() <= BUDGET,
        "le lecteur rend {} octets de texte, plus que BUDGET ({BUDGET}) : il ne coupe pas comme \
         l'enfant livré coupera",
        answer.text.len()
    );
    anyhow::ensure!(
        answer.pages_failed <= answer.pages,
        "le lecteur annonce plus de pages en échec que de pages : erreur du banc"
    );
    let outcome = match answer.outcome {
        Said::Text => Outcome::Text,
        Said::Error => Outcome::Error,
        Said::Panic => Outcome::Panic,
        Said::TooBig => Outcome::TooBig,
    };
    Ok(Classified {
        outcome,
        answer: Some(answer),
    })
}

/// La mémoire qui compte contre la borne. Sous Windows, le plus haut de l'ensemble de travail et de
/// la mémoire privée engagée — ce que `virtual_memory` y rapporte. Ailleurs, l'ensemble résident
/// seul : `virtual_memory` y est l'espace d'adressage **réservé**, qui compte des piles et des
/// arènes jamais touchées, et tuerait à tort.
fn charged(memory: u64, private: u64) -> u64 {
    if cfg!(windows) {
        memory.max(private)
    } else {
        memory
    }
}

/// Faut-il tuer ? La mémoire d'abord — c'est elle qui fait tomber la machine —, le processeur
/// ensuite, l'horloge en dernier : un enfant à la fois hors budget de processeur et d'horloge est
/// en cause, l'horloge seule ne l'est pas.
fn over(limits: &Limits, cpu_ms: u64, elapsed: Duration, charged: u64) -> Option<Kill> {
    if charged > limits.memory {
        Some(Kill::Memory)
    } else if cpu_ms > limits.cpu_ms {
        Some(Kill::Cpu)
    } else if elapsed > limits.wall {
        Some(Kill::Wall)
    } else {
        None
    }
}

// --- L'enfant ------------------------------------------------------------------------------------

/// Ce que le parent a vu d'un enfant.
#[derive(Debug, Default)]
struct Watched {
    kill: Option<Kill>,
    peak_memory: u64,
    peak_private: u64,
    cpu_ms: u64,
    samples: u64,
    elapsed: Duration,
}

/// Une exécution d'enfant, classée.
#[derive(Debug)]
struct Observation {
    outcome: Outcome,
    text: String,
    pages: u32,
    pages_failed: u32,
    /// Relevés par l'enfant autour de l'appel seul ; absents si l'enfant n'a pas répondu.
    wall_us: Option<u64>,
    cpu_ms: Option<u64>,
    peak_memory: u64,
    peak_private: u64,
    /// L'intervalle moyen tenu entre deux relevés.
    interval: Option<Duration>,
}

/// Le lecteur de candidats.
#[derive(Debug)]
struct Runner {
    path: Utf8PathBuf,
}

impl Runner {
    fn new(path: &Utf8Path) -> Result<Self> {
        refuse_stale(path)?;
        Ok(Self {
            path: path.to_owned(),
        })
    }

    /// La commande de base, **la même pour tous** les candidats.
    fn command(&self) -> Command {
        let mut command = Command::new(self.path.as_std_path());
        command
            // Un seul fil pour rayon, que pdf-inspector et lopdf emploient : le temps mesuré est
            // celui d'un cœur, comme dans l'enfant livré.
            .env("RAYON_NUM_THREADS", "1")
            // Les variables que pdf-inspector lit : un dossier de CMaps qui ferait d'un nom venu du
            // PDF un chemin, et un réglage de sa mise en page.
            .env_remove("PDF_INSPECTOR_BCMAPS_DIR")
            .env_remove("PI_NO_MERGE")
            // Le jeton du démon n'est hérité par aucun enfant.
            .env_remove("MAILCORE_TOKEN")
            .env_remove("RUST_LOG");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // Priorité **normale**, écrite : sans drapeau, un enfant hérite d'une priorité basse
            // si le parent en a une, et les temps ne vaudraient plus que pour cette machine-là.
            const NORMAL_PRIORITY_CLASS: u32 = 0x0000_0020;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW);
        }
        command
    }

    /// Un PDF fabriqué par le lecteur.
    fn fixture(&self, name: &str) -> Result<Arc<[u8]>> {
        let mut child = self
            .command()
            .args(["fixture", name])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("lancement de {}", self.path))?;
        let stdout = child.stdout.take().context("sortie du lecteur")?;
        let written = read_bounded(stdout, FIXTURE_CAP);
        let status = child.wait().context("attente du lecteur")?;
        anyhow::ensure!(status.success(), "le lecteur ne fabrique pas « {name} »");
        let Written::Bytes(bytes) = written else {
            anyhow::bail!("le lecteur rend une fixture « {name} » illisible ou trop longue");
        };
        anyhow::ensure!(
            bytes.starts_with(b"%PDF-"),
            "la fixture « {name} » n'est pas un PDF"
        );
        Ok(Arc::from(bytes))
    }

    /// Lit `bytes` avec `candidate`, dans un enfant neuf, sous les bornes du banc.
    fn run(&self, candidate: Candidate, bytes: &Arc<[u8]>) -> Result<Observation> {
        self.run_within(candidate, bytes, &LIMITS)
    }

    /// [`Runner::run`] sous d'autres bornes : les tests y passent des bornes minuscules pour voir
    /// chaque mise à mort se classer, sur un vrai enfant.
    fn run_within(
        &self,
        candidate: Candidate,
        bytes: &Arc<[u8]>,
        limits: &Limits,
    ) -> Result<Observation> {
        let mut command = self.command();
        command
            .arg(candidate.name())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let started = Instant::now();
        let mut child = command
            .spawn()
            .with_context(|| format!("lancement de {}", self.path))?;
        let pid = Pid::from_u32(child.id());
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("les tubes de l'enfant ne sont pas là");
        };
        // L'écriture sur son fil : un enfant qui ne lit plus ne doit pas bloquer la surveillance.
        // Une écriture qui échoue est un enfant mort avant d'avoir tout lu, et c'est sa sortie qui
        // le dira. L'entrée est fermée à la fin du fil : l'enfant lit jusqu'à la fin de fichier.
        let to_write = Arc::clone(bytes);
        let writer = std::thread::spawn(move || {
            let mut stdin = stdin;
            let _ = stdin.write_all(&to_write);
        });
        let reader = std::thread::spawn(move || read_bounded(stdout, LINE_CAP));
        let watched = watch(&mut child, pid, started, limits);
        let _ = child.wait();
        let written = reader.join().unwrap_or(Written::Unreadable);
        let _ = writer.join();
        let watched = watched?;
        let classified = classify(candidate, watched.kill, &written, bytes)?;
        let interval =
            (watched.samples > 0).then(|| watched.elapsed.div_f64(watched.samples as f64));
        let (text, pages, pages_failed, wall_us, cpu_ms) = match classified.answer {
            Some(answer) => (
                answer.text,
                answer.pages,
                answer.pages_failed,
                Some(answer.wall_us),
                Some(answer.cpu_ms),
            ),
            None => (String::new(), 0, 0, None, None),
        };
        Ok(Observation {
            outcome: classified.outcome,
            text,
            pages,
            pages_failed,
            wall_us,
            cpu_ms,
            peak_memory: watched.peak_memory,
            peak_private: watched.peak_private,
            interval,
        })
    }
}

/// Surveille l'enfant jusqu'à sa sortie, ou le tue à la première borne franchie.
///
/// # Errors
///
/// Si l'état de l'enfant ne peut plus être lu : il est tué, et le banc s'arrête plutôt que de
/// classer une exécution qu'il n'a pas vue finir.
fn watch(child: &mut Child, pid: Pid, started: Instant, limits: &Limits) -> Result<Watched> {
    let mut system = System::new();
    let refresh = ProcessRefreshKind::nothing().with_memory().with_cpu();
    let mut watched = Watched::default();
    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh);
        if let Some(process) = system.process(pid) {
            watched.peak_memory = watched.peak_memory.max(process.memory());
            watched.peak_private = watched.peak_private.max(process.virtual_memory());
            watched.cpu_ms = watched.cpu_ms.max(process.accumulated_cpu_time());
            watched.samples += 1;
        }
        let charge = charged(watched.peak_memory, watched.peak_private);
        if let Some(reason) = over(limits, watched.cpu_ms, started.elapsed(), charge) {
            watched.kill = Some(reason);
            let _ = child.kill();
            break;
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(SAMPLE_EVERY),
            Err(error) => {
                let _ = child.kill();
                return Err(error).context("état de l'enfant");
            }
        }
    }
    watched.elapsed = started.elapsed();
    Ok(watched)
}

/// Refuse un lecteur absent, ou plus ancien que ses sources : il mesurerait le code d'hier.
fn refuse_stale(runner: &Utf8Path) -> Result<()> {
    let built = std::fs::metadata(runner.as_std_path())
        .and_then(|meta| meta.modified())
        .with_context(|| {
            format!("{runner} introuvable : cd tools/pdf-runner && cargo build --release")
        })?;
    let sources = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("racine du dépôt")?
        .join("tools")
        .join("pdf-runner");
    if let Some(newest) = newest_source(&sources) {
        anyhow::ensure!(
            built >= newest,
            "{runner} est plus ancien que ses sources : il mesurerait le code d'hier.\n\
             Reconstruire : cd tools/pdf-runner && cargo build --release"
        );
    }
    Ok(())
}

/// La date de la source la plus récente du lecteur : son manifeste, son verrou, `src/`.
fn newest_source(root: &Utf8Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut consider = |path: &std::path::Path| {
        if let Ok(modified) = std::fs::metadata(path).and_then(|meta| meta.modified()) {
            newest = Some(newest.map_or(modified, |seen| seen.max(modified)));
        }
    };
    consider(root.join("Cargo.toml").as_std_path());
    consider(root.join("Cargo.lock").as_std_path());
    let mut pending = vec![root.join("src").into_std_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                consider(&path);
            }
        }
    }
    newest
}

// --- L'analyseur ---------------------------------------------------------------------------------

/// L'analyseur du champ `body` **tel que le schéma livré le déclare** : le schéma de
/// `mailcore::index::schema`, un index en mémoire, et `tokenizer_for_field`. Une copie de ses
/// règles cesserait d'être vraie au premier changement du schéma, sans rien en dire.
struct Analyzer {
    shipped: TextAnalyzer,
    /// Le découpage **sans filtre** : le tokeniseur de base de l'analyseur `default` de tantivy,
    /// sans le filtre des mots longs ni la mise en minuscules. Il ne sert qu'au diagnostic des
    /// jetons perdus, par différence — pas à recopier un seuil.
    unfiltered: TextAnalyzer,
}

impl std::fmt::Debug for Analyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Analyzer").finish_non_exhaustive()
    }
}

impl Analyzer {
    fn shipped() -> Result<Self> {
        let (schema, fields) = mailcore::index::schema::build();
        let index = tantivy::Index::builder()
            .schema(schema)
            .create_in_ram()
            .context("index en mémoire")?;
        let shipped = index
            .tokenizer_for_field(fields.body)
            .context("l'analyseur du champ body")?;
        Ok(Self {
            shipped,
            unfiltered: TextAnalyzer::from(SimpleTokenizer::default()),
        })
    }

    fn tokens(&mut self, text: &str) -> Vec<String> {
        collect(&mut self.shipped, text)
    }

    fn unfiltered_count(&mut self, text: &str) -> usize {
        let mut stream = self.unfiltered.token_stream(text);
        let mut count = 0;
        while stream.advance() {
            count += 1;
        }
        count
    }
}

fn collect(analyzer: &mut TextAnalyzer, text: &str) -> Vec<String> {
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

/// `needle` figure-t-il dans `haystack` comme **suite contiguë** ? C'est ce qu'une recherche par
/// phrase trouverait. Des jetons présents mais séparés ne comptent pas. Un terme sans jeton — que
/// l'analyseur jette en entier — n'est jamais trouvé.
fn contains_run(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// À qui imputer un terme manqué.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Miss {
    /// Au moins un de ses jetons manque au texte : le candidat ne l'a pas extrait.
    Extractor,
    /// Tous ses jetons sont dans le texte, pas dans cet ordre ni d'un seul tenant — ou le terme
    /// n'a aucun jeton : l'analyseur, la frappe de la vérité terrain, ou l'ordre de lecture. Rangé
    /// à part, hors du périmètre de l'extracteur.
    AnalyzerOrTyping,
}

fn attribute(present: &HashSet<&str>, needle: &[String]) -> Miss {
    if needle.iter().all(|token| present.contains(token.as_str())) {
        Miss::AnalyzerOrTyping
    } else {
        Miss::Extractor
    }
}

// --- Les diagnostics -----------------------------------------------------------------------------

/// Les suites qu'un texte UTF-8 lu comme du Windows-1252 laisse derrière lui, pour les caractères
/// d'un corpus français : `é` devient `Ã©`, l'apostrophe typographique `â€™`. Aucune n'apparaît
/// dans un texte français propre.
const MOJIBAKE: &[&str] = &[
    "Ã©",
    "Ã¨",
    "Ã ",
    "Ã\u{a0}",
    "Ãª",
    "Ã«",
    "Ã§",
    "Ã¢",
    "Ã®",
    "Ã¯",
    "Ã´",
    "Ã»",
    "Ã¹",
    "Ã‰",
    "Ã€",
    "Ã‡",
    "Ãˆ",
    "â€™",
    "â€œ",
    "â€\u{9d}",
    "â€“",
    "â€”",
    "â€¦",
    "Â«",
    "Â»",
    "Â°",
    "Â\u{a0}",
];

fn replacement_count(text: &str) -> usize {
    text.chars().filter(|&c| c == '\u{fffd}').count()
}

fn mojibake_count(text: &str) -> usize {
    MOJIBAKE
        .iter()
        .map(|pattern| text.matches(pattern).count())
        .sum()
}

/// Les caractères des zones d'usage privé : ce que rend une police sans `ToUnicode` qui range ses
/// glyphes là.
fn private_use_count(text: &str) -> usize {
    text.chars()
        .filter(|&c| {
            matches!(u32::from(c), 0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD)
        })
        .count()
}

/// Ce que les diagnostics disent d'un texte.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Diagnostics {
    chars: usize,
    replacement: usize,
    mojibake: usize,
    private_use: usize,
    /// Jetons du découpage sans filtre que l'analyseur livré ne garde pas.
    lost: usize,
    unfiltered: usize,
    /// Jetons de l'analyseur faits de lettres seulement, et parmi eux ceux d'une seule lettre : un
    /// texte espacé lettre à lettre (« F a c t u r e ») les fait monter vers 1.
    alphabetic: usize,
    single_letters: usize,
}

impl Diagnostics {
    fn of(text: &str, tokens: &[String], analyzer: &mut Analyzer) -> Self {
        let unfiltered = analyzer.unfiltered_count(text);
        let alphabetic: Vec<&String> = tokens
            .iter()
            .filter(|token| token.chars().all(char::is_alphabetic))
            .collect();
        Self {
            chars: text.chars().count(),
            replacement: replacement_count(text),
            mojibake: mojibake_count(text),
            private_use: private_use_count(text),
            lost: unfiltered.saturating_sub(tokens.len()),
            unfiltered,
            alphabetic: alphabetic.len(),
            single_letters: alphabetic
                .iter()
                .filter(|token| token.chars().count() == 1)
                .count(),
        }
    }

    fn replacement_per_mille(&self) -> f64 {
        per_mille(self.replacement, self.chars)
    }

    fn lost_per_mille(&self) -> f64 {
        per_mille(self.lost, self.unfiltered)
    }

    fn single_letter_share(&self) -> f64 {
        if self.alphabetic == 0 {
            0.0
        } else {
            self.single_letters as f64 / self.alphabetic as f64
        }
    }
}

fn per_mille(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 1000.0 / whole as f64
    }
}

/// L'accord de Jaccard entre deux ensembles de jetons. Deux textes vides s'accordent.
fn jaccard(a: &HashSet<&str>, b: &HashSet<&str>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 1.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

// --- La vérité terrain ---------------------------------------------------------------------------

/// Une entrée du fichier de termes, réduite à ce que le banc lit : jamais le nom, le sujet ni la
/// date, qui n'ont rien à faire dans un relevé.
#[derive(Debug, Clone)]
struct Truth {
    id: usize,
    hash: blake3::Hash,
    scan: bool,
    terms: Vec<String>,
    /// Faux pour un PDF du corpus qui n'est pas dans la vérité terrain : il compte pour la
    /// robustesse, la mémoire et les diagnostics, jamais pour les termes.
    judged: bool,
}

/// Les entrées de l'ensemble `set`.
///
/// # Errors
///
/// Si le fichier n'a pas la forme du squelette, si une empreinte est illisible ou répétée, ou si
/// une entrée n'a ni termes ni `scan = true` : une vérité terrain inachevée ne se mesure pas.
fn parse_terms(text: &str, set: &str) -> Result<Vec<Truth>> {
    let skeleton: Skeleton = toml::from_str(text).context("fichier de termes illisible")?;
    let mut seen_hashes = HashSet::new();
    let mut seen_ids = HashSet::new();
    let mut out = Vec::new();
    for entry in skeleton.pdf.iter().filter(|entry| entry.set == set) {
        let hash = blake3::Hash::from_hex(&entry.hash)
            .with_context(|| format!("n°{} : empreinte illisible", entry.id))?;
        anyhow::ensure!(
            seen_hashes.insert(hash),
            "n°{} : empreinte en double",
            entry.id
        );
        anyhow::ensure!(
            seen_ids.insert(entry.id),
            "n°{} : numéro en double",
            entry.id
        );
        let terms: Vec<String> = entry
            .terms
            .iter()
            .map(|term| term.trim().to_owned())
            .filter(|term| !term.is_empty())
            .collect();
        anyhow::ensure!(
            entry.scan || !terms.is_empty(),
            "n°{} n'a ni termes ni `scan = true` : la vérité terrain n'est pas finie",
            entry.id
        );
        out.push(Truth {
            id: entry.id,
            hash,
            scan: entry.scan,
            terms,
            judged: true,
        });
    }
    anyhow::ensure!(!out.is_empty(), "aucune entrée de l'ensemble « {set} »");
    Ok(out)
}

/// Comment un document est désigné dans un relevé : son numéro dans le fichier de termes, ou
/// « hors vérité » pour un PDF du corpus qui n'y est pas — l'empreinte suffit alors à le retrouver.
fn designation(truth: &Truth) -> String {
    let hex = truth.hash.to_hex();
    if truth.judged {
        format!("n°{} ({})", truth.id, &hex.as_str()[..8])
    } else {
        format!("hors vérité ({})", &hex.as_str()[..8])
    }
}

/// Un document du banc : sa vérité terrain, et le message d'où relire ses octets.
///
/// Les octets ne sont **pas** gardés : sur le corpus, 1 338 PDF distincts pèsent 836 Mio, que le
/// banc tiendrait en mémoire pendant des heures. Ils sont relus depuis le blob au moment de passer,
/// une fois par document et par exécution, et lâchés aussitôt.
#[derive(Debug)]
struct Document {
    truth: Truth,
    carrier: mailcore::BlobHash,
}

/// Retrouve chaque PDF de la vérité terrain parmi ceux que le recensement retient, et vérifie une
/// fois qu'il se relit.
fn gather(store: &mailcore::Store, truths: &[Truth]) -> Result<Vec<Document>> {
    let found = survey(store)?;
    let parser = mail_parser::MessageParser::default();
    let mut documents = Vec::with_capacity(truths.len());
    for truth in truths {
        let label = designation(truth);
        let distinct = found.distinct.get(&truth.hash).with_context(|| {
            format!(
                "{label} n'est pas parmi les PDF retenus de ce store : le fichier de termes décrit \
                 un autre corpus"
            )
        })?;
        let carrier = distinct
            .carriers
            .iter()
            .map(|carrier| carrier.blob)
            .find(|&blob| reread(store, &parser, blob, &truth.hash).is_some())
            .with_context(|| format!("{label} ne se retrouve dans aucun de ses messages"))?;
        documents.push(Document {
            truth: truth.clone(),
            carrier,
        });
    }
    Ok(documents)
}

/// Tous les PDF distincts que le recensement retient, ceux de la vérité terrain avec leurs termes.
///
/// C'est l'ensemble sur lequel la règle gelée juge ses conditions dures — morts, délais, mémoire,
/// caractères illisibles — : quarante documents ne disent rien d'un PDF sur cent qui tue
/// l'enfant. L'ordre est celui des empreintes, donc le même d'une exécution à l'autre.
fn gather_corpus(store: &mailcore::Store, truths: &[Truth]) -> Result<Vec<Document>> {
    let found = survey(store)?;
    let parser = mail_parser::MessageParser::default();
    let by_hash: HashMap<blake3::Hash, &Truth> =
        truths.iter().map(|truth| (truth.hash, truth)).collect();
    let mut hashes: Vec<&blake3::Hash> = found.distinct.keys().collect();
    hashes.sort_by_key(|hash| *hash.as_bytes());
    for truth in truths {
        anyhow::ensure!(
            found.distinct.contains_key(&truth.hash),
            "{} n'est pas parmi les PDF retenus de ce store : le fichier de termes décrit un autre \
             corpus",
            designation(truth)
        );
    }
    let mut documents = Vec::with_capacity(hashes.len());
    for hash in hashes {
        let truth = by_hash.get(hash).map_or_else(
            || Truth {
                id: 0,
                hash: *hash,
                scan: false,
                terms: Vec::new(),
                judged: false,
            },
            |&truth| truth.clone(),
        );
        let carrier = found.distinct[hash]
            .carriers
            .iter()
            .map(|carrier| carrier.blob)
            .find(|&blob| reread(store, &parser, blob, hash).is_some())
            .with_context(|| {
                format!(
                    "{} ne se retrouve dans aucun de ses messages",
                    designation(&truth)
                )
            })?;
        documents.push(Document { truth, carrier });
    }
    Ok(documents)
}

/// Les octets d'un document, relus depuis son message.
fn load(
    store: &mailcore::Store,
    parser: &mail_parser::MessageParser,
    document: &Document,
) -> Result<Vec<u8>> {
    reread(store, parser, document.carrier, &document.truth.hash)
        .with_context(|| format!("{} ne se relit plus", designation(&document.truth)))
}

/// Les octets de la pièce d'empreinte `hash` dans ce blob, par la règle de détection livrée.
fn reread(
    store: &mailcore::Store,
    parser: &mail_parser::MessageParser,
    blob: mailcore::BlobHash,
    hash: &blake3::Hash,
) -> Option<Vec<u8>> {
    let raw = store.blobs().read(blob).ok()?;
    let parsed = parser.parse(&raw)?;
    pieces(&parsed)
        .into_iter()
        .filter(|piece| piece.verdict.accepted())
        .map(|piece| piece.part.contents())
        .find(|bytes| blake3::hash(bytes) == *hash)
        .map(<[u8]>::to_vec)
}

// --- Le relevé -----------------------------------------------------------------------------------

/// Les exécutions d'un candidat sur un document.
#[derive(Debug, Default)]
struct Cell {
    runs: Vec<Run>,
    /// La première exécution que le filet d'horloge n'a pas coupée : c'est son texte qui est jugé.
    kept: Option<Kept>,
}

/// Ce qu'on garde de chaque exécution — le texte une seule fois, dans [`Kept`].
#[derive(Debug)]
struct Run {
    outcome: Outcome,
    text_hash: blake3::Hash,
    wall_us: Option<u64>,
    cpu_ms: Option<u64>,
    peak_memory: u64,
    peak_private: u64,
    interval: Option<Duration>,
}

#[derive(Debug)]
struct Kept {
    outcome: Outcome,
    text: String,
    pages: u32,
    pages_failed: u32,
}

impl Cell {
    fn record(&mut self, observation: Observation) {
        self.runs.push(Run {
            outcome: observation.outcome,
            text_hash: blake3::hash(observation.text.as_bytes()),
            wall_us: observation.wall_us,
            cpu_ms: observation.cpu_ms,
            peak_memory: observation.peak_memory,
            peak_private: observation.peak_private,
            interval: observation.interval,
        });
        if self.kept.is_none() && observation.outcome != Outcome::Wall {
            self.kept = Some(Kept {
                outcome: observation.outcome,
                text: observation.text,
                pages: observation.pages,
                pages_failed: observation.pages_failed,
            });
        }
    }

    /// La même issue à chaque exécution que l'horloge n'a pas coupée.
    fn stable(&self) -> bool {
        let mut outcomes = self
            .runs
            .iter()
            .map(|run| run.outcome)
            .filter(|&outcome| outcome != Outcome::Wall);
        let first = outcomes.next();
        outcomes.all(|outcome| Some(outcome) == first)
    }

    /// Le même texte à chaque exécution qui a rendu un texte.
    fn deterministic(&self) -> bool {
        let mut hashes = self
            .runs
            .iter()
            .filter(|run| run.outcome == Outcome::Text)
            .map(|run| run.text_hash);
        let first = hashes.next();
        hashes.all(|hash| Some(hash) == first)
    }

    /// Les relevés d'un champ sur les exécutions **chronométrées** : toutes sauf la première.
    fn timed(&self, field: impl Fn(&Run) -> Option<u64>) -> Vec<u64> {
        self.runs.iter().skip(1).filter_map(field).collect()
    }
}

/// Ce qu'un candidat donne d'un document, une fois jugé.
#[derive(Debug)]
struct Judged {
    /// `None` : toutes les exécutions ont été coupées par l'horloge.
    outcome: Option<Outcome>,
    tokens: Vec<String>,
    chars: usize,
    pages: u32,
    pages_failed: u32,
    found: usize,
    total: usize,
    missed_extractor: usize,
    missed_analyzer: usize,
    diagnostics: Diagnostics,
    stable: bool,
    deterministic: bool,
    wall_us: Vec<u64>,
    cpu_ms: Vec<u64>,
    peak_memory: u64,
    peak_private: u64,
}

fn judge(cell: &Cell, truth: &Truth, analyzer: &mut Analyzer) -> Judged {
    let (outcome, text, pages, pages_failed) = match &cell.kept {
        Some(kept) => (
            Some(kept.outcome),
            kept.text.as_str(),
            kept.pages,
            kept.pages_failed,
        ),
        None => (None, "", 0, 0),
    };
    let tokens = analyzer.tokens(text);
    let diagnostics = Diagnostics::of(text, &tokens, analyzer);
    let present: HashSet<&str> = tokens.iter().map(String::as_str).collect();
    let (mut found, mut missed_extractor, mut missed_analyzer) = (0, 0, 0);
    if !truth.scan {
        for term in &truth.terms {
            let needle = analyzer.tokens(term);
            if contains_run(&tokens, &needle) {
                found += 1;
            } else {
                match attribute(&present, &needle) {
                    Miss::Extractor => missed_extractor += 1,
                    Miss::AnalyzerOrTyping => missed_analyzer += 1,
                }
            }
        }
    }
    Judged {
        outcome,
        chars: diagnostics.chars,
        tokens,
        pages,
        pages_failed,
        found,
        total: if truth.scan { 0 } else { truth.terms.len() },
        missed_extractor,
        missed_analyzer,
        diagnostics,
        stable: cell.stable(),
        deterministic: cell.deterministic(),
        wall_us: cell.timed(|run| run.wall_us),
        cpu_ms: cell.timed(|run| run.cpu_ms),
        peak_memory: cell
            .runs
            .iter()
            .map(|run| run.peak_memory)
            .max()
            .unwrap_or(0),
        peak_private: cell
            .runs
            .iter()
            .map(|run| run.peak_private)
            .max()
            .unwrap_or(0),
    }
}

// --- Le point d'entrée ---------------------------------------------------------------------------

/// Point d'entrée de `pdf-compare`.
///
/// # Errors
///
/// En profil de débogage ; si le lecteur est absent ou périmé ; si la vérité terrain est illisible
/// ou inachevée ; si un contrôle bloquant échoue ; si un contrôle de transport échoue ; et, une
/// fois le relevé imprimé, si l'alarme des sorties identiques sonne.
pub fn compare(options: &Options) -> Result<()> {
    crate::semantic::release_only("pdf-compare")?;
    anyhow::ensure!(
        options.runs >= 2,
        "{} exécution : la première est jetée pour les temps, il en faut au moins deux — trois \
         pour lire une dispersion",
        options.runs
    );
    anyhow::ensure!(
        matches!(options.set.as_str(), "decision" | "reserve" | "corpus"),
        "ensemble « {} » inconnu : decision, reserve ou corpus",
        options.set
    );
    let runner = Runner::new(&options.runner)?;
    let raw = std::fs::read(options.terms.as_std_path())
        .with_context(|| format!("lecture de {}", options.terms))?;
    let terms_hash = blake3::hash(&raw);
    // Le corpus juge les termes du jeu de décision, et rien d'autre : la réserve sert à mettre le
    // banc au point, elle ne décide pas.
    let judged_set = if options.set == "corpus" {
        "decision"
    } else {
        options.set.as_str()
    };
    let truths = parse_terms(
        std::str::from_utf8(&raw).context("fichier de termes non UTF-8")?,
        judged_set,
    )?;
    drop(raw);
    let mut analyzer = Analyzer::shipped()?;

    let scans = truths.iter().filter(|truth| truth.scan).count();
    let term_count: usize = truths
        .iter()
        .filter(|truth| !truth.scan)
        .map(|truth| truth.terms.len())
        .sum();
    println!("Lecteur            {}", options.runner);
    println!(
        "Termes             {}  BLAKE3 {}",
        options.terms,
        terms_hash.to_hex()
    );
    println!(
        "Ensemble           « {} » ; vérité terrain : {} PDF jugés, dont {scans} sans texte \
         lisible à l'œil ; {term_count} termes",
        options.set,
        truths.len()
    );
    println!(
        "Exécutions         {} — la première jetée pour les temps",
        options.runs
    );
    println!();

    blocking_controls(&runner, &mut analyzer)?;

    // La réplique reste ouverte le temps de la mesure : les octets se relisent document par
    // document. Chaque document passe chez les trois candidats d'affilée, dans le même ordre à
    // chaque exécution — ils le lisent tous avec le même état du cache disque.
    let corpus = options.set == "corpus";
    let (documents, cells) = with_replica(&options.store, |store| {
        let documents = if corpus {
            gather_corpus(store, &truths)?
        } else {
            gather(store, &truths)?
        };
        let parser = mail_parser::MessageParser::default();
        let mut cells: HashMap<(Candidate, usize), Cell> = HashMap::new();
        for run in 1..=options.runs {
            for (index, document) in documents.iter().enumerate() {
                let bytes: Arc<[u8]> = Arc::from(load(store, &parser, document)?);
                for candidate in CANDIDATES {
                    eprint!(
                        "\r  exécution {run}/{}  {}/{}  {:<9}   ",
                        options.runs,
                        index + 1,
                        documents.len(),
                        candidate.name(),
                    );
                    let observation = runner.run(candidate, &bytes)?;
                    cells
                        .entry((candidate, index))
                        .or_default()
                        .record(observation);
                }
            }
        }
        eprintln!();
        Ok((documents, cells))
    })?;

    let mut judged: BTreeMap<(Candidate, usize), Judged> = BTreeMap::new();
    for ((candidate, index), cell) in &cells {
        if let Some(document) = documents.get(*index) {
            judged.insert(
                (*candidate, *index),
                judge(cell, &document.truth, &mut analyzer),
            );
        }
    }
    let reading = Reading {
        documents: &documents,
        cells: &cells,
        judged: &judged,
    };
    if let Some(alarm) = report(&reading, corpus) {
        anyhow::bail!("{alarm}");
    }
    Ok(())
}

/// Les contrôles bloquants : rien du corpus n'est lu tant qu'ils ne passent pas.
fn blocking_controls(runner: &Runner, analyzer: &mut Analyzer) -> Result<()> {
    let positive = runner.fixture("positive")?;
    let wiring = runner.fixture("wiring")?;
    let expected = analyzer.tokens(POSITIVE);
    anyhow::ensure!(
        !expected.is_empty(),
        "l'analyseur ne rend aucun jeton de la phrase positive"
    );
    for candidate in CANDIDATES {
        let seen = runner.run(candidate, &positive)?;
        anyhow::ensure!(
            seen.outcome == Outcome::Text,
            "contrôle positif : {} rend « {} » sur un PDF que tout extracteur lit",
            candidate.name(),
            seen.outcome.label()
        );
        anyhow::ensure!(
            contains_run(&analyzer.tokens(&seen.text), &expected),
            "contrôle positif : {} ne retrouve pas « {POSITIVE} » — le banc refuse de lire le \
             corpus",
            candidate.name()
        );
    }
    let mut wired = BTreeMap::new();
    for candidate in CANDIDATES {
        let seen = runner.run(candidate, &wiring)?;
        anyhow::ensure!(
            seen.outcome == Outcome::Text,
            "contrôle de câblage : {} rend « {} »",
            candidate.name(),
            seen.outcome.label()
        );
        wired.insert(candidate, analyzer.tokens(&seen.text));
    }
    check_wiring(&wired)?;
    let extract = wired.get(&Candidate::Extract).map_or(0, Vec::len);
    println!("Contrôles bloquants");
    println!("  positif          retrouvé par les trois candidats");
    println!(
        "  câblage          lopdf 1 jeton, pdf-inspector 2, pdf-extract {extract} (pour mémoire)"
    );
    println!("  transport        longueur et BLAKE3 vérifiés sur chaque réponse");
    println!();
    Ok(())
}

/// Le contrôle de câblage, sur les jetons que chaque candidat tire de la fixture `wiring` : lopdf
/// brut doit coller les deux mots en un seul jeton, pdf-inspector doit en rendre deux.
///
/// # Errors
///
/// Si l'un ou l'autre ne fait pas ce que sa nature dit : les noms ne désignent pas les
/// bibliothèques qu'on croit mesurer.
fn check_wiring(wired: &BTreeMap<Candidate, Vec<String>>) -> Result<()> {
    let glued = ["bonjourmonde".to_owned()];
    let apart = ["bonjour".to_owned(), "monde".to_owned()];
    anyhow::ensure!(
        wired
            .get(&Candidate::Lopdf)
            .is_some_and(|tokens| tokens.as_slice() == glued),
        "contrôle de câblage : lopdf brut ne colle pas « Bonjour » et « monde » — le candidat qui \
         porte son nom n'est pas lui"
    );
    anyhow::ensure!(
        wired
            .get(&Candidate::Inspector)
            .is_some_and(|tokens| tokens.as_slice() == apart),
        "contrôle de câblage : pdf-inspector ne sépare pas « Bonjour » de « monde » — il \
         mesurerait lopdf une seconde fois"
    );
    Ok(())
}

// --- L'impression --------------------------------------------------------------------------------

/// Le relevé jugé, et de quoi le lire par candidat.
#[derive(Debug)]
struct Reading<'a> {
    documents: &'a [Document],
    cells: &'a HashMap<(Candidate, usize), Cell>,
    judged: &'a BTreeMap<(Candidate, usize), Judged>,
}

/// Quels documents une section lit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    All,
    /// Ceux qui ont un texte lisible à l'œil : la métrique primaire.
    Text,
    /// Ceux que la vérité terrain dit `scan`.
    Scan,
}

impl Reading<'_> {
    fn indices(&self, kind: Kind) -> Vec<usize> {
        self.documents
            .iter()
            .enumerate()
            .filter(|(_, document)| match kind {
                Kind::All => true,
                Kind::Text => document.truth.judged && !document.truth.scan,
                Kind::Scan => document.truth.judged && document.truth.scan,
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn judged(&self, candidate: Candidate, index: usize) -> Option<&Judged> {
        self.judged.get(&(candidate, index))
    }

    fn rows(&self, candidate: Candidate, kind: Kind) -> Vec<&Judged> {
        self.indices(kind)
            .into_iter()
            .filter_map(|index| self.judged(candidate, index))
            .collect()
    }

    /// Les documents désignés, par numéro et début d'empreinte.
    fn named(&self, list: &[usize]) -> String {
        if list.is_empty() {
            return "aucun".to_owned();
        }
        list.iter()
            .filter_map(|&index| self.documents.get(index))
            .map(|document| designation(&document.truth))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// La comparaison appariée de pdf-inspector à pdf-extract, par documents.
#[derive(Debug, Default)]
struct Paired {
    wins: Vec<usize>,
    losses: Vec<usize>,
    ties: usize,
    /// Faute d'une exécution que l'horloge n'ait pas coupée, d'un côté ou de l'autre.
    unmeasured: usize,
}

fn pair(reading: &Reading<'_>) -> Paired {
    let mut paired = Paired::default();
    for index in reading.indices(Kind::Text) {
        let (Some(inspector), Some(extract)) = (
            reading.judged(Candidate::Inspector, index),
            reading.judged(Candidate::Extract, index),
        ) else {
            continue;
        };
        if inspector.outcome.is_none() || extract.outcome.is_none() {
            paired.unmeasured += 1;
            continue;
        }
        match inspector.found.cmp(&extract.found) {
            std::cmp::Ordering::Greater => paired.wins.push(index),
            std::cmp::Ordering::Less => paired.losses.push(index),
            std::cmp::Ordering::Equal => paired.ties += 1,
        }
    }
    paired
}

/// Imprime le relevé, et rend l'alarme si elle sonne.
fn report(reading: &Reading<'_>, corpus: bool) -> Option<String> {
    print_outcomes(reading);
    let paired = pair(reading);
    print_terms(reading, &paired);
    print_scans(reading);
    print_times(reading);
    print_memory(reading);
    print_diagnostics(reading);
    let alarm = identical_alarm(reading);
    print_rule(reading, &paired, corpus);
    print_arbitration(reading, &paired);
    alarm
}

fn print_outcomes(reading: &Reading<'_>) {
    println!("Issues (exécution retenue de chaque document)");
    print!("  {:<10}", "");
    for outcome in Outcome::ALL {
        print!(" {:>9}", outcome.label());
    }
    println!();
    for candidate in CANDIDATES {
        let rows = reading.rows(candidate, Kind::All);
        print!("  {:<10}", candidate.name());
        for outcome in Outcome::ALL {
            // Toutes les exécutions coupées par l'horloge : rangé sous « horloge ».
            let count = rows
                .iter()
                .filter(|judged| judged.outcome.unwrap_or(Outcome::Wall) == outcome)
                .count();
            print!(" {count:>9}");
        }
        println!();
    }
    for candidate in CANDIDATES {
        let rows = reading.rows(candidate, Kind::All);
        let pages: u64 = rows.iter().map(|j| u64::from(j.pages)).sum();
        let failed: u64 = rows.iter().map(|j| u64::from(j.pages_failed)).sum();
        let with_failure = rows.iter().filter(|j| j.pages_failed > 0).count();
        let unstable = rows.iter().filter(|j| !j.stable).count();
        let drifting = rows.iter().filter(|j| !j.deterministic).count();
        println!(
            "  {:<10} pages {pages}, en échec {failed} (dans {with_failure} documents) ; issue \
             instable {unstable} ; texte non déterministe {drifting}",
            candidate.name()
        );
    }
    println!();
}

fn print_terms(reading: &Reading<'_>, paired: &Paired) {
    let texts = reading.indices(Kind::Text);
    let total: usize = texts
        .iter()
        .filter_map(|&index| reading.documents.get(index))
        .map(|document| document.truth.terms.len())
        .sum();
    println!(
        "Termes retrouvés en suite de jetons de l'analyseur — {} PDF à texte, {total} termes",
        texts.len()
    );
    println!(
        "  {:<10} {:>9} {:>8}  {:>8}    part par document : p10 / p25 / p50 / p90",
        "", "termes", "part", "moyenne"
    );
    for candidate in CANDIDATES {
        let rows = reading.rows(candidate, Kind::Text);
        let found: usize = rows.iter().map(|j| j.found).sum();
        let mut shares = per_document(reading, candidate);
        shares.sort_by(f64::total_cmp);
        // La **moyenne par document** est ce que la règle juge (P2) : un document à vingt termes
        // n'y pèse pas plus qu'un document à quatre. La part globale est donnée à côté.
        println!(
            "  {:<10} {:>9} {:>6.1} %  {:>8}    {} / {} / {} / {}",
            candidate.name(),
            format!("{found}/{total}"),
            percent(found, total),
            mean(&shares).map_or_else(|| "—".to_owned(), |m| format!("{m:.3}")),
            share(&shares, 10),
            share(&shares, 25),
            share(&shares, 50),
            share(&shares, 90),
        );
    }
    let decided = paired.wins.len() + paired.losses.len();
    println!(
        "  apparié pdf-inspector contre pdf-extract : {} victoires, {} défaites, {} égalités ; \
         test de signe p = {:.3}",
        paired.wins.len(),
        paired.losses.len(),
        paired.ties,
        crate::recall::sign_test(paired.wins.len(), decided)
    );
    if paired.unmeasured > 0 {
        println!(
            "  non appariés, faute d'une exécution que l'horloge n'ait pas coupée : {}",
            paired.unmeasured
        );
    }
    println!();

    println!("Termes manqués, attribués (comptes)");
    println!(
        "  {:<10} {:>11} {:>20}",
        "", "extracteur", "analyseur ou frappe"
    );
    for candidate in CANDIDATES {
        let rows = reading.rows(candidate, Kind::Text);
        println!(
            "  {:<10} {:>11} {:>20}",
            candidate.name(),
            rows.iter().map(|j| j.missed_extractor).sum::<usize>(),
            rows.iter().map(|j| j.missed_analyzer).sum::<usize>()
        );
    }
    println!();
}

fn print_scans(reading: &Reading<'_>) {
    let scans = reading.indices(Kind::Scan);
    if scans.is_empty() {
        return;
    }
    println!(
        "Sans texte lisible à l'œil — {} PDF : longueur de texte rendue, en caractères",
        scans.len()
    );
    for candidate in CANDIDATES {
        let rows = reading.rows(candidate, Kind::Scan);
        let mut lengths: Vec<u64> = rows.iter().map(|j| j.chars as u64).collect();
        lengths.sort_unstable();
        let with_tokens = rows.iter().filter(|j| !j.tokens.is_empty()).count();
        println!(
            "  {:<10} avec jetons {with_tokens}/{}   p50 {}  max {}",
            candidate.name(),
            rows.len(),
            integer(&lengths, 50),
            integer(&lengths, 100)
        );
    }
    println!();
}

/// Les temps : la dispersion **avant** la médiane — deux exécutions qui s'écartent de 40 % ne sont
/// pas du bruit, c'est un symptôme, et une médiane le cacherait.
fn print_times(reading: &Reading<'_>) {
    println!("Temps relevés par l'enfant autour de l'appel seul — exécutions 2 et suivantes");
    println!(
        "  dispersion par document, (max − min) / max entre exécutions — à lire avant la médiane"
    );
    for candidate in CANDIDATES {
        let mut spread: Vec<f64> = reading
            .rows(candidate, Kind::All)
            .iter()
            .filter_map(|j| dispersion(&j.wall_us))
            .collect();
        spread.sort_by(f64::total_cmp);
        println!(
            "  {:<10} mural   p50 {}  p90 {}  max {}   ({} documents)",
            candidate.name(),
            share(&spread, 50),
            share(&spread, 90),
            share(&spread, 100),
            spread.len()
        );
    }
    println!("  médiane par document");
    for candidate in CANDIDATES {
        let rows = reading.rows(candidate, Kind::All);
        let mut wall: Vec<f64> = rows.iter().filter_map(|j| median(&j.wall_us)).collect();
        wall.sort_by(f64::total_cmp);
        let mut cpu: Vec<f64> = rows.iter().filter_map(|j| median(&j.cpu_ms)).collect();
        cpu.sort_by(f64::total_cmp);
        println!(
            "  {:<10} mural   p50 {}  p90 {}  max {}   processeur p50 {}  p90 {}  max {}",
            candidate.name(),
            millis(&wall, 50, 1000.0),
            millis(&wall, 90, 1000.0),
            millis(&wall, 100, 1000.0),
            millis(&cpu, 50, 1.0),
            millis(&cpu, 90, 1.0),
            millis(&cpu, 100, 1.0),
        );
    }
    println!(
        "  (le temps processeur a la résolution du pas d'horloge du système, ~16 ms sous Windows)"
    );
    println!();
}

fn print_memory(reading: &Reading<'_>) {
    println!("Mémoire de l'enfant, crête par document sur toutes les exécutions");
    for candidate in CANDIDATES {
        let rows = reading.rows(candidate, Kind::All);
        let mut resident: Vec<u64> = rows.iter().map(|j| j.peak_memory).collect();
        resident.sort_unstable();
        let mut private: Vec<u64> = rows.iter().map(|j| j.peak_private).collect();
        private.sort_unstable();
        println!(
            "  {:<10} résidente p50 {}  p90 {}  max {}   {} p50 {}  max {}",
            candidate.name(),
            bytes_at(&resident, 50),
            bytes_at(&resident, 90),
            bytes_at(&resident, 100),
            if cfg!(windows) {
                "privée"
            } else {
                "virtuelle"
            },
            bytes_at(&private, 50),
            bytes_at(&private, 100),
        );
    }
    let mut intervals: Vec<f64> = reading
        .cells
        .values()
        .flat_map(|cell| cell.runs.iter())
        .filter_map(|run| run.interval)
        .map(|interval| interval.as_secs_f64() * 1000.0)
        .collect();
    intervals.sort_by(f64::total_cmp);
    println!(
        "  échantillonnage visé {} ms, tenu p50 {} ms  p99 {} ms",
        SAMPLE_EVERY.as_millis(),
        decimal(&intervals, 50),
        decimal(&intervals, 99)
    );
    println!();
}

fn print_diagnostics(reading: &Reading<'_>) {
    println!("Diagnostics — jamais des critères — sur les documents qui rendent un texte");
    for candidate in CANDIDATES {
        let rows: Vec<&Judged> = reading
            .rows(candidate, Kind::All)
            .into_iter()
            .filter(|j| j.diagnostics.chars > 0)
            .collect();
        let sorted = |value: &dyn Fn(&Diagnostics) -> f64| {
            let mut values: Vec<f64> = rows.iter().map(|j| value(&j.diagnostics)).collect();
            values.sort_by(f64::total_cmp);
            values
        };
        let replacement = sorted(&Diagnostics::replacement_per_mille);
        let lost = sorted(&Diagnostics::lost_per_mille);
        let single = sorted(&|d: &Diagnostics| d.single_letter_share() * 100.0);
        let mojibake_docs = rows.iter().filter(|j| j.diagnostics.mojibake > 0).count();
        let mojibake: usize = rows.iter().map(|j| j.diagnostics.mojibake).sum();
        let private_docs = rows
            .iter()
            .filter(|j| j.diagnostics.private_use > 0)
            .count();
        let private: usize = rows.iter().map(|j| j.diagnostics.private_use).sum();
        println!("  {:<10} {} documents", candidate.name(), rows.len());
        println!(
            "    U+FFFD ‰ p50 {}  p90 {}  max {}   mojibake {mojibake} dans {mojibake_docs} \
             documents   zone privée {private} dans {private_docs} documents",
            decimal(&replacement, 50),
            decimal(&replacement, 90),
            decimal(&replacement, 100)
        );
        println!(
            "    jetons perdus par l'analyseur ‰ p50 {}  max {}   jetons d'une lettre % p50 {}  \
             p90 {}",
            decimal(&lost, 50),
            decimal(&lost, 100),
            decimal(&single, 50),
            decimal(&single, 90)
        );
    }
    println!();
}

/// L'alarme des sorties identiques à l'octet : sur plus de [`IDENTICAL_ALARM`] des documents, deux
/// noms désignent probablement un même extracteur. Deux textes **vides** identiques ne comptent
/// pas : un scan sans texte se lit vide chez tout le monde.
fn identical_alarm(reading: &Reading<'_>) -> Option<String> {
    let mut alarm = None;
    let count = reading.documents.len();
    for (a, b) in [
        (Candidate::Inspector, Candidate::Extract),
        (Candidate::Inspector, Candidate::Lopdf),
        (Candidate::Extract, Candidate::Lopdf),
    ] {
        let text_of = |candidate: Candidate, index: usize| {
            reading
                .cells
                .get(&(candidate, index))
                .and_then(|cell| cell.kept.as_ref())
                .map(|kept| kept.text.as_str())
        };
        let same = (0..count)
            .filter(|&index| {
                matches!(
                    (text_of(a, index), text_of(b, index)),
                    (Some(x), Some(y)) if !x.is_empty() && x == y
                )
            })
            .count();
        let part = if count == 0 {
            0.0
        } else {
            same as f64 / count as f64
        };
        if part > IDENTICAL_ALARM {
            let message = format!(
                "ALARME : {} et {} rendent des sorties identiques à l'octet sur {same} documents \
                 sur {count} ({:.1} %) — deux noms pour un même candidat ? Le relevé ne vaut rien \
                 tant que ce n'est pas expliqué.",
                a.name(),
                b.name(),
                part * 100.0
            );
            println!("{message}");
            alarm = Some(message);
        }
    }
    alarm
}

/// Les documents à regarder, désignés par leur numéro : c'est là que l'arbitrage se fait, à l'œil.
fn print_arbitration(reading: &Reading<'_>, paired: &Paired) {
    let all = reading.indices(Kind::All);
    println!("À arbitrer — par numéro du fichier de termes");
    println!(
        "  défaites de pdf-inspector      {}",
        reading.named(&paired.losses)
    );
    println!(
        "  victoires de pdf-inspector     {}",
        reading.named(&paired.wins)
    );
    for candidate in CANDIDATES {
        let failing: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&index| {
                reading
                    .judged(candidate, index)
                    .is_some_and(|j| j.outcome.is_none_or(Outcome::accuses) || !j.stable)
            })
            .collect();
        println!(
            "  {:<10} sans texte ou instable  {}",
            candidate.name(),
            reading.named(&failing)
        );
    }
    let disagreeing: Vec<usize> = reading
        .indices(Kind::Text)
        .into_iter()
        .filter(|&index| {
            let (Some(a), Some(b)) = (
                reading.judged(Candidate::Inspector, index),
                reading.judged(Candidate::Extract, index),
            ) else {
                return false;
            };
            let a: HashSet<&str> = a.tokens.iter().map(String::as_str).collect();
            let b: HashSet<&str> = b.tokens.iter().map(String::as_str).collect();
            jaccard(&a, &b) < DISAGREEMENT
        })
        .collect();
    println!(
        "  désaccord pdf-inspector / pdf-extract (Jaccard < {DISAGREEMENT})  {}",
        reading.named(&disagreeing)
    );
    let scans = reading.indices(Kind::Scan);
    if !scans.is_empty() {
        let speaking: Vec<usize> = scans
            .into_iter()
            .filter(|&index| {
                CANDIDATES.iter().any(|&candidate| {
                    reading
                        .judged(candidate, index)
                        .is_some_and(|j| !j.tokens.is_empty())
                })
            })
            .collect();
        println!(
            "  « scan » dont un candidat tire des jetons  {}",
            reading.named(&speaking)
        );
    }
    let drifting: Vec<usize> = all
        .iter()
        .copied()
        .filter(|&index| {
            CANDIDATES.iter().any(|&candidate| {
                reading
                    .judged(candidate, index)
                    .is_some_and(|j| !j.deterministic)
            })
        })
        .collect();
    println!(
        "  texte non déterministe         {}",
        reading.named(&drifting)
    );
}

/// La part des termes retrouvés, document par document, pour un candidat : la mesure R de la
/// règle gelée.
fn per_document(reading: &Reading<'_>, candidate: Candidate) -> Vec<f64> {
    reading
        .rows(candidate, Kind::Text)
        .iter()
        .filter(|j| j.total > 0)
        .map(|j| j.found as f64 / j.total as f64)
        .collect()
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

/// Le seuil de P2.
const P2_MEAN: f64 = 0.80;
/// Le seuil de H3 : la part des PDF qui tuent l'enfant — chacun serait un poison en production.
const H3_DEATHS: f64 = 0.01;
/// Le seuil de H4 : la crête de l'enfant pour 99 % des PDF.
const H4_PEAK: u64 = 512 * 1024 * 1024;
/// Le seuil de H5 : les caractères illisibles de pdf-inspector rapportés à ceux de pdf-extract.
const H5_RATIO: f64 = 1.25;

/// La règle gelée, jugée : chaque condition, et les chiffres qui la décident.
#[derive(Debug)]
struct Rule {
    wins: usize,
    losses: usize,
    ties: usize,
    p: f64,
    average: f64,
    /// Ce qui suit ne vaut que sur le corpus entier.
    count: usize,
    deaths: usize,
    p99: u64,
    killed: usize,
    illegible_inspector: usize,
    illegible_extract: usize,
    read_by_both: usize,
}

impl Rule {
    /// P1 : pdf-extract n'est pas significativement meilleur.
    fn p1(&self) -> bool {
        !(self.losses > self.wins && self.p < 0.05)
    }

    fn p2(&self) -> bool {
        self.average >= P2_MEAN
    }

    fn h3(&self) -> bool {
        self.count > 0 && (self.deaths as f64) <= H3_DEATHS * self.count as f64
    }

    fn h4(&self) -> bool {
        self.p99 <= H4_PEAK && self.killed == 0
    }

    fn h5(&self) -> bool {
        (self.illegible_inspector as f64) <= H5_RATIO * self.illegible_extract as f64
    }

    /// Le verdict, tel que la règle l'écrit.
    fn verdict(&self) -> String {
        let hard = self.h3() && self.h4() && self.h5();
        if self.p1() && self.p2() && hard {
            "pdf-inspector RETENU".to_owned()
        } else if self.p1() && hard {
            format!(
                "seule P2 échoue ({:.3}) : le dire avec le chiffre — l'utilisateur décide si le \
                 gain justifie la dépendance",
                self.average
            )
        } else {
            "pdf-inspector NON RETENU, et pdf-extract pas davantage : évaluer hayro avec une mise \
             en mots écrite chez nous ; consulter l'utilisateur avant toute bibliothèque en C ou \
             C++"
            .to_owned()
        }
    }
}

/// Juge la **règle gelée** en tête du fichier de termes.
///
/// Le banc applique la règle ; il ne la choisit pas. Elle a été écrite et gelée avec la vérité
/// terrain avant la première exécution, et ses seuils sont recopiés ici tels qu'écrits : une
/// règle qui se lirait autrement dans le banc que dans le fichier serait une règle réécrite après
/// coup.
fn rule(reading: &Reading<'_>, paired: &Paired) -> Rule {
    let decided = paired.wins.len() + paired.losses.len();
    let rows = reading.rows(Candidate::Inspector, Kind::All);
    let deaths = rows
        .iter()
        .filter(|j| {
            matches!(
                j.outcome,
                Some(Outcome::Crash | Outcome::Timeout | Outcome::Memory)
            )
        })
        .count();
    let mut peaks: Vec<u64> = rows
        .iter()
        .map(|j| charged(j.peak_memory, j.peak_private))
        .collect();
    peaks.sort_unstable();
    let killed = rows
        .iter()
        .filter(|j| j.outcome == Some(Outcome::Memory))
        .count();
    let (mut illegible_inspector, mut illegible_extract, mut read_by_both) = (0, 0, 0);
    for index in reading.indices(Kind::All) {
        let (Some(a), Some(b)) = (
            reading.judged(Candidate::Inspector, index),
            reading.judged(Candidate::Extract, index),
        ) else {
            continue;
        };
        if a.outcome == Some(Outcome::Text)
            && b.outcome == Some(Outcome::Text)
            && a.chars > 0
            && b.chars > 0
        {
            read_by_both += 1;
            illegible_inspector += a.diagnostics.replacement + a.diagnostics.mojibake;
            illegible_extract += b.diagnostics.replacement + b.diagnostics.mojibake;
        }
    }
    Rule {
        wins: paired.wins.len(),
        losses: paired.losses.len(),
        ties: paired.ties,
        p: crate::recall::sign_test(paired.wins.len(), decided),
        average: mean(&per_document(reading, Candidate::Inspector)).unwrap_or(0.0),
        count: rows.len(),
        deaths,
        p99: centile(&peaks, 99).unwrap_or(0),
        killed,
        illegible_inspector,
        illegible_extract,
        read_by_both,
    }
}

/// Imprime la règle jugée. Les conditions dures portent sur le corpus entier : sur un jeu de
/// quarante documents, elles ne sont pas jugées, et aucun verdict n'est rendu.
fn print_rule(reading: &Reading<'_>, paired: &Paired, corpus: bool) {
    let held = |ok: bool| if ok { "tenue " } else { "ÉCHOUE" };
    let rule = rule(reading, paired);
    println!("Règle gelée — pdf-inspector, condition par condition");
    println!(
        "  P1 pdf-extract pas significativement meilleur  {}  {} victoires, {} défaites, {} \
         égalités, p = {:.3}",
        held(rule.p1()),
        rule.wins,
        rule.losses,
        rule.ties,
        rule.p
    );
    println!(
        "  P2 moyenne par document ≥ {P2_MEAN:.2}              {}  {:.3}",
        held(rule.p2()),
        rule.average
    );
    println!(
        "  H1 contrôles bloquants                         tenue   sans eux, rien n'aurait été lu"
    );
    println!(
        "  H2 toute issue classée                         tenue   par construction : une sortie \
         hors protocole est une mort, un écart de transport arrête le banc"
    );
    if !corpus {
        println!(
            "  H3 à H5                                        non jugées — elles portent sur le \
             corpus : --set corpus"
        );
        println!("  Verdict                                        aucun sur cet ensemble");
        println!();
        return;
    }
    println!(
        "  H3 morts, délais, mémoire ≤ 1 %                {}  {} sur {} ({:.2} %)",
        held(rule.h3()),
        rule.deaths,
        rule.count,
        percent(rule.deaths, rule.count)
    );
    println!(
        "  H4 crête ≤ 512 Mio pour 99 %, aucune > 2 Gio   {}  p99 {}, tués pour mémoire {}",
        held(rule.h4()),
        mailapi::human::bytes(rule.p99),
        rule.killed
    );
    println!(
        "  H5 illisibles ≤ 1,25 × pdf-extract             {}  {} contre {}, sur {} documents lus \
         par les deux",
        held(rule.h5()),
        rule.illegible_inspector,
        rule.illegible_extract,
        rule.read_by_both
    );
    println!(
        "  Verdict                                        {}",
        rule.verdict()
    );
    println!();
}

/// La dispersion d'une suite de relevés : (max − min) / max. Il en faut deux.
fn dispersion(values: &[u64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let max = values.iter().copied().max()?;
    let min = values.iter().copied().min()?;
    if max == 0 {
        return Some(0.0);
    }
    Some((max - min) as f64 / max as f64)
}

/// La médiane au sens ordinaire : la moyenne des deux du milieu quand le compte est pair.
fn median(values: &[u64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => sorted.get(middle).map(|&v| v as f64),
        _ => {
            let low = *sorted.get(middle - 1)?;
            let high = *sorted.get(middle)?;
            Some((low as f64 + high as f64) / 2.0)
        }
    }
}

/// Le centile `p` d'une suite triée, au plus proche rang.
fn centile<T: Copy>(sorted: &[T], p: usize) -> Option<T> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() * p).div_ceil(100).max(1) - 1;
    sorted.get(rank.min(sorted.len() - 1)).copied()
}

fn percent(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

fn share(sorted: &[f64], p: usize) -> String {
    centile(sorted, p).map_or_else(|| "—".to_owned(), |v| format!("{v:.2}"))
}

fn decimal(sorted: &[f64], p: usize) -> String {
    centile(sorted, p).map_or_else(|| "—".to_owned(), |v| format!("{v:.1}"))
}

fn integer(sorted: &[u64], p: usize) -> String {
    centile(sorted, p).map_or_else(|| "—".to_owned(), |v| v.to_string())
}

/// Un centile de durées, ramené en millisecondes par `per_milli`.
fn millis(sorted: &[f64], p: usize, per_milli: f64) -> String {
    centile(sorted, p).map_or_else(|| "—".to_owned(), |v| format!("{:.1} ms", v / per_milli))
}

fn bytes_at(sorted: &[u64], p: usize) -> String {
    centile(sorted, p).map_or_else(|| "—".to_owned(), mailapi::human::bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn analyzer() -> Analyzer {
        Analyzer::shipped().unwrap()
    }

    /// Une phrase française propre, accents, cédille et apostrophe compris.
    const CLEAN: &str = "L'été dernier, Hélène a payé sa facture d'électricité à Orléans. Où ? Ça \
                         coûte 12 €, « tout compris ».";

    // --- Les diagnostics, chacun avec son contrôle inverse ---

    #[test]
    fn a_clean_french_sentence_raises_no_diagnostic() {
        let mut analyzer = analyzer();
        let tokens = analyzer.tokens(CLEAN);
        let diagnostics = Diagnostics::of(CLEAN, &tokens, &mut analyzer);
        assert_eq!(diagnostics.replacement, 0);
        assert_eq!(
            diagnostics.mojibake, 0,
            "un « é » propre compté comme mojibake"
        );
        assert_eq!(diagnostics.private_use, 0);
        assert_eq!(diagnostics.lost, 0);
        assert!(diagnostics.chars > 0);
    }

    #[test]
    fn replacement_characters_are_counted_per_mille() {
        assert_eq!(replacement_count(CLEAN), 0);
        let broken = "fa\u{fffd}ture d\u{fffd}\u{fffd}lectricit\u{fffd}";
        assert_eq!(replacement_count(broken), 4);
        let mut analyzer = analyzer();
        let tokens = analyzer.tokens(broken);
        let diagnostics = Diagnostics::of(broken, &tokens, &mut analyzer);
        assert!((diagnostics.replacement_per_mille() - 4000.0 / 21.0).abs() < 1e-9);
    }

    #[test]
    fn mojibake_is_counted_and_clean_accents_are_not() {
        assert_eq!(mojibake_count(CLEAN), 0);
        // « été », « l’ » et « É », lus en Windows-1252 après un encodage UTF-8.
        assert_eq!(mojibake_count("Ã©tÃ© lâ€™Ã‰tat"), 4);
        assert_eq!(mojibake_count("Â« citation Â»"), 2);
    }

    #[test]
    fn private_use_characters_are_counted() {
        assert_eq!(private_use_count(CLEAN), 0);
        assert_eq!(private_use_count("\u{e000}a\u{f8ff}b\u{f0000}"), 3);
        // Le contrôle inverse aux bords : au-dessous de la zone (U+E000 suit les demi-paires, qui
        // ne sont pas des caractères) et juste au-dessus.
        assert_eq!(private_use_count("\u{d7ff}\u{f900}"), 0);
    }

    /// L'analyseur livré jette les mots de plus de 40 octets ; le découpage sans filtre les garde.
    #[test]
    fn tokens_dropped_by_the_shipped_analyzer_are_counted() {
        let mut analyzer = analyzer();
        let long = "a".repeat(41);
        let text = format!("facture {long} payée");
        let tokens = analyzer.tokens(&text);
        let diagnostics = Diagnostics::of(&text, &tokens, &mut analyzer);
        assert_eq!(diagnostics.lost, 1, "{tokens:?}");
        assert_eq!(diagnostics.unfiltered, 3);
    }

    #[test]
    fn letter_spaced_text_raises_the_single_letter_share() {
        let mut analyzer = analyzer();
        let spaced = "F a c t u r e";
        let tokens = analyzer.tokens(spaced);
        let diagnostics = Diagnostics::of(spaced, &tokens, &mut analyzer);
        assert!((diagnostics.single_letter_share() - 1.0).abs() < f64::EPSILON);

        // Le contrôle inverse : des mots entiers, sans élision.
        let words = "Votre facture du mois de juin";
        let tokens = analyzer.tokens(words);
        let diagnostics = Diagnostics::of(words, &tokens, &mut analyzer);
        assert!(diagnostics.single_letter_share().abs() < f64::EPSILON);

        // Et des chiffres isolés ne sont pas des lettres.
        let digits = "1 2 3";
        let tokens = analyzer.tokens(digits);
        let diagnostics = Diagnostics::of(digits, &tokens, &mut analyzer);
        assert_eq!(diagnostics.alphabetic, 0);
    }

    // --- La suite contiguë de jetons ---

    /// L'analyseur est bien celui du schéma : il met en minuscules et jette les mots trop longs.
    #[test]
    fn the_analyzer_is_the_shipped_one() {
        let mut analyzer = analyzer();
        assert_eq!(
            analyzer.tokens("ÉLECTRICITÉ Facture"),
            ["électricité", "facture"]
        );
        assert!(analyzer.tokens(&"x".repeat(41)).is_empty());
        // Le contrôle inverse : le découpage sans filtre garde le mot long.
        assert_eq!(analyzer.unfiltered_count(&"x".repeat(41)), 1);
    }

    #[test]
    fn a_term_is_found_only_as_a_contiguous_run_of_tokens() {
        let mut analyzer = analyzer();
        let text = analyzer.tokens("Votre facture d'électricité n° 1234,56 du mois de juin");
        let term = |t: &str, analyzer: &mut Analyzer| analyzer.tokens(t);
        assert!(contains_run(
            &text,
            &term("facture d'électricité", &mut analyzer)
        ));
        assert!(contains_run(&text, &term("1234,56", &mut analyzer)));
        assert!(contains_run(&text, &term("FACTURE", &mut analyzer)));

        // Le contrôle : les jetons sont tous là, mais pas d'un seul tenant, ou pas dans l'ordre.
        let apart = term("facture 1234", &mut analyzer);
        assert!(apart.iter().all(|token| text.contains(token)));
        assert!(!contains_run(&text, &apart));
        assert!(!contains_run(
            &text,
            &term("électricité facture", &mut analyzer)
        ));
        // Un terme que l'analyseur réduit à rien n'est jamais trouvé.
        assert!(!contains_run(&text, &term("— !", &mut analyzer)));
    }

    #[test]
    fn a_missed_term_is_put_on_the_extractor_only_if_a_token_is_absent() {
        let mut analyzer = analyzer();
        let text = analyzer.tokens("Votre facture d'électricité n° 1234,56");
        let present: HashSet<&str> = text.iter().map(String::as_str).collect();
        assert_eq!(
            attribute(&present, &analyzer.tokens("gaz naturel")),
            Miss::Extractor
        );
        assert_eq!(
            attribute(&present, &analyzer.tokens("facture gaz")),
            Miss::Extractor
        );
        assert_eq!(
            attribute(&present, &analyzer.tokens("facture 1234")),
            Miss::AnalyzerOrTyping
        );
        assert_eq!(attribute(&present, &[]), Miss::AnalyzerOrTyping);
    }

    #[test]
    fn jaccard_agrees_on_nothing_and_disagrees_on_disjoint_sets() {
        let empty = HashSet::new();
        assert!((jaccard(&empty, &empty) - 1.0).abs() < f64::EPSILON);
        let a: HashSet<&str> = ["facture", "juin"].into_iter().collect();
        let b: HashSet<&str> = ["facture", "mai"].into_iter().collect();
        assert!((jaccard(&a, &b) - 1.0 / 3.0).abs() < 1e-9);
        let c: HashSet<&str> = ["avoir"].into_iter().collect();
        assert!(jaccard(&a, &c).abs() < f64::EPSILON);
    }

    // --- Le transport, la lecture bornée, le classement ---

    fn answer_line(candidate: &str, sent: &[u8], text: &str, outcome: &str) -> Vec<u8> {
        let mut line = serde_json::to_vec(&serde_json::json!({
            "candidate": candidate,
            "received": sent.len(),
            "hash": blake3::hash(sent).to_hex().as_str(),
            "outcome": outcome,
            "text": text,
            "pages": 1,
            "pages_failed": 0,
            "wall_us": 1200,
            "cpu_ms": 16,
        }))
        .unwrap();
        line.push(b'\n');
        line
    }

    #[test]
    fn the_transport_check_compares_length_and_fingerprint() {
        let sent = b"%PDF-1.4 octets envoyes";
        let answer = answer_of(&Written::Bytes(answer_line("lopdf", sent, "", "text"))).unwrap();
        assert!(transport(&answer, sent).is_ok());
        // Un octet de moins : la longueur.
        assert!(transport(&answer, &sent[..sent.len() - 1]).is_err());
        // Même longueur, un octet changé : l'empreinte.
        let mut altered = sent.to_vec();
        altered[0] ^= 1;
        assert!(transport(&answer, &altered).is_err());
    }

    #[test]
    fn the_output_is_read_with_a_bound_and_drained() {
        let line = answer_line("lopdf", b"x", "bonjour", "text");
        let Written::Bytes(read) = read_bounded(line.as_slice(), line.len()) else {
            panic!("une ligne sous la borne est refusée");
        };
        assert_eq!(read, line);

        // Au-delà de la borne : rien n'est gardé, et tout est lu — l'enfant ne reste pas bloqué.
        let mut source = std::io::Cursor::new(vec![b'a'; 10_000]);
        assert!(matches!(read_bounded(&mut source, 100), Written::Overlong));
        assert_eq!(source.position(), 10_000, "la sortie n'a pas été vidée");
    }

    #[test]
    fn only_exactly_one_protocol_line_is_an_answer() {
        let line = answer_line("lopdf", b"x", "bonjour", "text");
        assert!(answer_of(&Written::Bytes(line.clone())).is_some());
        // Sans fin de ligne : une sortie coupée.
        assert!(answer_of(&Written::Bytes(line[..line.len() - 1].to_vec())).is_none());
        // Deux lignes, ou du bruit devant : hors protocole.
        assert!(answer_of(&Written::Bytes([line.as_slice(), line.as_slice()].concat())).is_none());
        assert!(
            answer_of(&Written::Bytes(
                [b"bruit\n".as_slice(), line.as_slice()].concat()
            ))
            .is_none()
        );
        // Un champ de plus : un lecteur d'une autre version.
        let mut value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        value["extra"] = serde_json::json!(1);
        let mut extra = serde_json::to_vec(&value).unwrap();
        extra.push(b'\n');
        assert!(answer_of(&Written::Bytes(extra)).is_none());
        // Une issue inconnue.
        assert!(answer_of(&Written::Bytes(answer_line("lopdf", b"x", "", "abandon"))).is_none());
        assert!(answer_of(&Written::Overlong).is_none());
        assert!(answer_of(&Written::Unreadable).is_none());
    }

    #[test]
    fn outcomes_are_classified_in_one_place_and_in_order() {
        let sent = b"%PDF-1.7 document";
        let good = Written::Bytes(answer_line("extract", sent, "texte", "text"));

        // Une raison de tuer l'emporte, même sur une réponse valable.
        for (kill, outcome) in [
            (Kill::Cpu, Outcome::Timeout),
            (Kill::Memory, Outcome::Memory),
            (Kill::Wall, Outcome::Wall),
        ] {
            let classified = classify(Candidate::Extract, Some(kill), &good, sent).unwrap();
            assert_eq!(classified.outcome, outcome);
            assert!(classified.answer.is_none());
        }
        // Sans raison, une réponse valable donne son issue.
        for (written, outcome) in [
            ("text", Outcome::Text),
            ("error", Outcome::Error),
            ("panic", Outcome::Panic),
            ("too-big", Outcome::TooBig),
        ] {
            let line = Written::Bytes(answer_line("extract", sent, "", written));
            assert_eq!(
                classify(Candidate::Extract, None, &line, sent)
                    .unwrap()
                    .outcome,
                outcome
            );
        }
        // Sans raison et sans ligne : une mort.
        for written in [
            Written::Bytes(Vec::new()),
            Written::Bytes(b"pas du json\n".to_vec()),
            Written::Overlong,
            Written::Unreadable,
        ] {
            assert_eq!(
                classify(Candidate::Extract, None, &written, sent)
                    .unwrap()
                    .outcome,
                Outcome::Crash
            );
        }
        // Les erreurs du banc : transport, candidat, plafond.
        assert!(classify(Candidate::Extract, None, &good, b"autre chose").is_err());
        assert!(classify(Candidate::Lopdf, None, &good, sent).is_err());
        let huge = Written::Bytes(answer_line(
            "extract",
            sent,
            &"a".repeat(BUDGET + 1),
            "text",
        ));
        assert!(classify(Candidate::Extract, None, &huge, sent).is_err());
    }

    #[test]
    fn the_wall_clock_net_accuses_no_one() {
        assert!(!Outcome::Wall.accuses());
        assert!(!Outcome::Text.accuses());
        for outcome in [
            Outcome::Error,
            Outcome::Panic,
            Outcome::TooBig,
            Outcome::Timeout,
            Outcome::Memory,
            Outcome::Crash,
        ] {
            assert!(outcome.accuses(), "{outcome:?}");
        }
    }

    #[test]
    fn limits_are_checked_memory_first_then_processor_then_clock() {
        let limits = Limits {
            cpu_ms: 100,
            wall: Duration::from_secs(10),
            memory: 1000,
        };
        let calm = Duration::from_secs(1);
        let late = Duration::from_secs(11);
        assert_eq!(over(&limits, 50, calm, 500), None);
        // À la borne exactement, rien.
        assert_eq!(over(&limits, 100, Duration::from_secs(10), 1000), None);
        assert_eq!(over(&limits, 101, calm, 500), Some(Kill::Cpu));
        assert_eq!(over(&limits, 50, late, 500), Some(Kill::Wall));
        assert_eq!(over(&limits, 50, calm, 1001), Some(Kill::Memory));
        // Hors budget de processeur **et** d'horloge : le candidat est en cause.
        assert_eq!(over(&limits, 101, late, 500), Some(Kill::Cpu));
        assert_eq!(over(&limits, 101, late, 1001), Some(Kill::Memory));
    }

    // --- La vérité terrain et le relevé ---

    fn entry(id: usize, set: &str, hash: &str, scan: bool, terms: &[&str]) -> String {
        format!(
            "[[pdf]]\nid = {id}\nset = \"{set}\"\nhash = \"{hash}\"\nfile = \"f.pdf\"\n\
             name = \"n\"\nsize = 1\nroute = \"Declared\"\ndepth = 0\nblobs = []\n\
             first_date = \"d\"\nfirst_subject = \"s\"\nscan = {scan}\nterms = {terms:?}\n\n"
        )
    }

    #[test]
    fn the_ground_truth_is_read_by_set_and_refused_unfinished() {
        let a = blake3::hash(b"a").to_hex();
        let b = blake3::hash(b"b").to_hex();
        let c = blake3::hash(b"c").to_hex();
        let text = [
            entry(1, "decision", a.as_str(), false, &["facture", "  ", "juin"]),
            entry(2, "reserve", b.as_str(), false, &["avoir"]),
            entry(3, "decision", c.as_str(), true, &[]),
        ]
        .concat();
        let decision = parse_terms(&text, "decision").unwrap();
        assert_eq!(decision.len(), 2);
        assert_eq!(
            decision[0].terms,
            ["facture", "juin"],
            "un terme blanc est resté"
        );
        assert!(decision[1].scan);
        assert_eq!(parse_terms(&text, "reserve").unwrap().len(), 1);
        assert!(parse_terms(&text, "autre").is_err());

        // Une entrée ni « scan » ni pourvue de termes : inachevée.
        let unfinished = entry(4, "decision", a.as_str(), false, &[]);
        assert!(parse_terms(&unfinished, "decision").is_err());
        // Une empreinte illisible, une empreinte en double.
        assert!(parse_terms(&entry(5, "decision", "zz", false, &["x"]), "decision").is_err());
        let twice = [
            entry(6, "decision", a.as_str(), false, &["x"]),
            entry(7, "decision", a.as_str(), false, &["y"]),
        ]
        .concat();
        assert!(parse_terms(&twice, "decision").is_err());
    }

    fn observation(outcome: Outcome, text: &str, wall_us: u64) -> Observation {
        Observation {
            outcome,
            text: text.to_owned(),
            pages: 1,
            pages_failed: 0,
            wall_us: Some(wall_us),
            cpu_ms: Some(0),
            peak_memory: 10,
            peak_private: 20,
            interval: Some(Duration::from_millis(2)),
        }
    }

    #[test]
    fn a_cell_keeps_the_first_text_the_clock_did_not_cut_and_times_the_rest() {
        let mut cell = Cell::default();
        cell.record(observation(Outcome::Wall, "", 0));
        cell.record(observation(Outcome::Text, "un", 100));
        cell.record(observation(Outcome::Text, "un", 300));
        assert_eq!(cell.kept.as_ref().map(|k| k.text.as_str()), Some("un"));
        assert!(cell.stable());
        assert!(cell.deterministic());
        assert_eq!(cell.timed(|run| run.wall_us), [100, 300]);

        // Le contrôle inverse : un texte qui change, une issue qui change.
        cell.record(observation(Outcome::Text, "deux", 200));
        assert!(!cell.deterministic());
        cell.record(observation(Outcome::Timeout, "", 0));
        assert!(!cell.stable());
    }

    #[test]
    fn dispersion_median_and_centile_say_what_they_mean() {
        assert_eq!(
            dispersion(&[100]),
            None,
            "une exécution n'a pas de dispersion"
        );
        assert!((dispersion(&[100, 50]).unwrap() - 0.5).abs() < f64::EPSILON);
        assert_eq!(median(&[]), None);
        assert!((median(&[3, 1, 2]).unwrap() - 2.0).abs() < f64::EPSILON);
        assert!((median(&[1, 4]).unwrap() - 2.5).abs() < f64::EPSILON);
        assert_eq!(centile::<u64>(&[], 50), None);
        assert_eq!(centile(&[1, 2, 3, 4], 50), Some(2));
        assert_eq!(centile(&[1, 2, 3, 4], 100), Some(4));
    }

    #[test]
    fn a_designation_names_no_file() {
        let truth = document(12, false).truth;
        assert_eq!(
            designation(&truth),
            format!("n°12 ({})", &truth.hash.to_hex().as_str()[..8])
        );
        // Un PDF du corpus hors de la vérité terrain n'a pas de numéro : son empreinte suffit.
        let outside = Truth {
            judged: false,
            ..truth
        };
        assert!(designation(&outside).starts_with("hors vérité ("));
    }

    fn document(id: usize, scan: bool) -> Document {
        Document {
            truth: Truth {
                id,
                hash: blake3::hash(&id.to_le_bytes()),
                scan,
                terms: vec!["terme".to_owned(); 4],
                judged: true,
            },
            carrier: mailcore::BlobHash::from_bytes([0; 32]),
        }
    }

    /// Un PDF du corpus hors de la vérité terrain n'a pas de termes : il ne compte ni comme une
    /// égalité ni dans la moyenne. Sans la garde, quatre cents documents du corpus noieraient les
    /// quarante qui décident sous des « égalités » à zéro terme.
    #[test]
    fn a_document_outside_the_truth_is_never_paired() {
        let mut documents: Vec<Document> = (1..=2).map(|id| document(id, false)).collect();
        documents[1].truth.judged = false;
        documents[1].truth.terms.clear();
        let text = Some(Outcome::Text);
        let mut judged = BTreeMap::new();
        for index in 0..2 {
            judged.insert((Candidate::Inspector, index), judged_with(text, 2));
            judged.insert((Candidate::Extract, index), judged_with(text, 2));
        }
        let cells = HashMap::new();
        let reading = Reading {
            documents: &documents,
            cells: &cells,
            judged: &judged,
        };
        let paired = pair(&reading);
        assert_eq!(paired.ties, 1, "seul le document jugé est apparié");
        assert_eq!(per_document(&reading, Candidate::Inspector).len(), 1);
    }

    fn rule_with(wins: usize, losses: usize, average: f64) -> Rule {
        Rule {
            wins,
            losses,
            ties: 0,
            p: crate::recall::sign_test(wins, wins + losses),
            average,
            count: 1_000,
            deaths: 10,
            p99: H4_PEAK,
            killed: 0,
            illegible_inspector: 125,
            illegible_extract: 100,
            read_by_both: 900,
        }
    }

    /// Les seuils de la règle gelée, chacun à sa limite et juste au-delà : un seuil qui se lirait
    /// « < » dans le banc et « ≤ » dans le fichier serait une règle réécrite.
    #[test]
    fn the_frozen_rule_holds_at_its_limits_and_fails_just_beyond() {
        let rule = rule_with(20, 10, 0.80);
        assert!(rule.p1() && rule.p2() && rule.h3() && rule.h4() && rule.h5());
        assert_eq!(rule.verdict(), "pdf-inspector RETENU");

        // P1 n'échoue que si pdf-extract est **significativement** meilleur.
        assert!(
            rule_with(10, 14, 0.9).p1(),
            "14 contre 10 n'est pas significatif"
        );
        assert!(!rule_with(2, 14, 0.9).p1());
        // Et jamais quand c'est pdf-inspector qui gagne nettement.
        assert!(rule_with(14, 2, 0.9).p1());

        assert!(!rule_with(20, 10, 0.799).p2());
        let mut over = rule_with(20, 10, 0.9);
        over.deaths = 11;
        assert!(!over.h3(), "11 sur 1 000 dépasse 1 %");
        let mut peak = rule_with(20, 10, 0.9);
        peak.p99 = H4_PEAK + 1;
        assert!(!peak.h4());
        let mut killed = rule_with(20, 10, 0.9);
        killed.killed = 1;
        assert!(!killed.h4(), "un seul PDF au-delà de 2 Gio suffit");
        let mut illegible = rule_with(20, 10, 0.9);
        illegible.illegible_inspector = 126;
        assert!(!illegible.h5());
    }

    #[test]
    fn the_verdict_follows_the_rule_as_written() {
        // Seule P2 échoue : la décision revient à l'utilisateur, chiffre à l'appui.
        assert!(
            rule_with(20, 10, 0.5)
                .verdict()
                .starts_with("seule P2 échoue")
        );
        // Une condition dure échoue : ni l'un ni l'autre.
        let mut hard = rule_with(20, 10, 0.9);
        hard.deaths = 50;
        assert!(hard.verdict().contains("NON RETENU"));
        // P1 échoue : ni l'un ni l'autre non plus — pdf-extract n'est pas adopté pour autant.
        assert!(
            rule_with(1, 15, 0.9)
                .verdict()
                .contains("pdf-extract pas davantage")
        );
    }

    fn judged_with(outcome: Option<Outcome>, found: usize) -> Judged {
        Judged {
            outcome,
            tokens: Vec::new(),
            chars: 0,
            pages: 1,
            pages_failed: 0,
            found,
            total: 4,
            missed_extractor: 4 - found,
            missed_analyzer: 0,
            diagnostics: Diagnostics::default(),
            stable: true,
            deterministic: true,
            wall_us: Vec::new(),
            cpu_ms: Vec::new(),
            peak_memory: 0,
            peak_private: 0,
        }
    }

    #[test]
    fn the_pairing_counts_wins_losses_ties_and_leaves_out_the_unmeasured() {
        let documents: Vec<Document> = (1..=5).map(|id| document(id, id == 5)).collect();
        let text = Some(Outcome::Text);
        let mut judged = BTreeMap::new();
        for (index, (inspector, extract)) in [
            ((text, 3), (text, 2)),
            ((text, 1), (text, 4)),
            ((text, 2), (text, 2)),
            ((None, 0), (text, 4)),
            // Un scan : hors de la métrique primaire, quoi qu'il rende.
            ((text, 4), (text, 0)),
        ]
        .into_iter()
        .enumerate()
        {
            judged.insert(
                (Candidate::Inspector, index),
                judged_with(inspector.0, inspector.1),
            );
            judged.insert(
                (Candidate::Extract, index),
                judged_with(extract.0, extract.1),
            );
        }
        let cells = HashMap::new();
        let reading = Reading {
            documents: &documents,
            cells: &cells,
            judged: &judged,
        };
        let paired = pair(&reading);
        assert_eq!(paired.wins, [0]);
        assert_eq!(paired.losses, [1]);
        assert_eq!(paired.ties, 1);
        assert_eq!(paired.unmeasured, 1);
    }

    #[test]
    fn identical_outputs_ring_the_alarm_and_identical_silence_does_not() {
        let documents: Vec<Document> = (1..=10).map(|id| document(id, false)).collect();
        let kept = |text: &str| Cell {
            runs: Vec::new(),
            kept: Some(Kept {
                outcome: Outcome::Text,
                text: text.to_owned(),
                pages: 1,
                pages_failed: 0,
            }),
        };
        let judged = BTreeMap::new();
        let mut cells = HashMap::new();
        for index in 0..10 {
            cells.insert((Candidate::Inspector, index), kept(&format!("a{index}")));
            cells.insert((Candidate::Extract, index), kept(&format!("b{index}")));
            cells.insert((Candidate::Lopdf, index), kept(&format!("c{index}")));
        }
        let reading = |cells: &HashMap<(Candidate, usize), Cell>| {
            identical_alarm(&Reading {
                documents: &documents,
                cells,
                judged: &judged,
            })
        };
        assert!(reading(&cells).is_none());

        // Une sortie sur dix identique à l'octet : 10 %, au-delà de 5 %.
        cells.insert((Candidate::Lopdf, 3), kept("a3"));
        let alarm = reading(&cells).unwrap();
        assert!(
            alarm.contains("inspector") && alarm.contains("lopdf"),
            "{alarm}"
        );

        // Le contrôle inverse : des textes vides identiques partout ne sonnent pas.
        for index in 0..10 {
            for candidate in CANDIDATES {
                cells.insert((candidate, index), kept(""));
            }
        }
        assert!(reading(&cells).is_none());
    }

    /// Le contrôle de câblage refuse dès qu'un candidat ne fait pas ce que sa nature dit — et
    /// d'abord le cas qu'il est là pour attraper : pdf-inspector qui collerait comme lopdf.
    #[test]
    fn the_wiring_check_refuses_two_names_for_one_extractor() {
        let tokens = |list: &[&str]| list.iter().map(|&t| t.to_owned()).collect::<Vec<_>>();
        let good = BTreeMap::from([
            (Candidate::Inspector, tokens(&["bonjour", "monde"])),
            (Candidate::Extract, tokens(&["bonjour", "monde"])),
            (Candidate::Lopdf, tokens(&["bonjourmonde"])),
        ]);
        assert!(check_wiring(&good).is_ok());

        let mut glued_twice = good.clone();
        glued_twice.insert(Candidate::Inspector, tokens(&["bonjourmonde"]));
        assert!(check_wiring(&glued_twice).is_err());
        let mut apart_twice = good.clone();
        apart_twice.insert(Candidate::Lopdf, tokens(&["bonjour", "monde"]));
        assert!(check_wiring(&apart_twice).is_err());
        let mut missing = good;
        missing.remove(&Candidate::Lopdf);
        assert!(check_wiring(&missing).is_err());
    }

    /// Le chemin réel, sur les **fixtures seulement** : contrôles bloquants, un enfant, son
    /// échantillonnage, le transport, et chaque mise à mort classée sur un vrai processus.
    ///
    /// Ignoré par défaut : il demande le lecteur construit, que la CI ne construit pas.
    /// `cd tools/pdf-runner && cargo build --release`, puis
    /// `cargo test -p xtask --bin xtask pdfcompare -- --ignored`.
    #[test]
    #[ignore = "demande tools/pdf-runner construit en release"]
    fn the_built_runner_passes_the_blocking_controls_and_dies_as_told() {
        let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let runner = Runner::new(&root.join(DEFAULT_RUNNER)).unwrap();
        let mut analyzer = analyzer();
        blocking_controls(&runner, &mut analyzer).unwrap();

        let positive = runner.fixture("positive").unwrap();
        let seen = runner.run(Candidate::Lopdf, &positive).unwrap();
        assert_eq!(seen.outcome, Outcome::Text);
        assert!(
            seen.peak_memory > 0,
            "aucun relevé de la mémoire de l'enfant"
        );
        assert!(seen.interval.is_some());
        assert!(seen.wall_us.is_some());

        // Du charabia : une erreur du candidat, pas une mort.
        let noise: Arc<[u8]> = Arc::from(b"pas un PDF".as_slice());
        assert_eq!(
            runner.run(Candidate::Extract, &noise).unwrap().outcome,
            Outcome::Error
        );

        // Chaque borne, franchie au premier relevé, se classe par sa raison.
        let roomy = Limits {
            cpu_ms: u64::MAX,
            wall: Duration::from_secs(3600),
            memory: u64::MAX,
        };
        for (limits, outcome) in [
            (Limits { memory: 1, ..roomy }, Outcome::Memory),
            (
                Limits {
                    wall: Duration::ZERO,
                    ..roomy
                },
                Outcome::Wall,
            ),
        ] {
            let seen = runner
                .run_within(Candidate::Inspector, &positive, &limits)
                .unwrap();
            assert_eq!(seen.outcome, outcome);
            assert!(seen.text.is_empty(), "un enfant tué a rendu un texte");
        }
    }
}

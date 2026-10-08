//! La passe de fond des vecteurs : les messages qu'une moisson apporte reçoivent leur vecteur
//! sans que personne ne lance `mail vectors` — étape 7 de `docs/PHASE-4.md`, critère 4.
//!
//! ## Un processus enfant, et pas un fil
//!
//! Une passe tient 343 Mio de modèle et monte à 456 Mio de crête dans un processus qui n'a rien
//! d'autre à porter. Dans le démon — ou dans la coquille, qui l'embarque —, elle s'ajouterait à
//! une moisson, à l'interface, à l'index : le plafond de 500 Mo de la phase 2 ne tiendrait pas.
//! L'enfant, lui, rend toute sa mémoire au système en sortant, et une panique dans `candle` ne
//! tue que lui.
//!
//! C'est **le même exécutable**, relancé avec [`CHILD_FLAG`] : le démon, la coquille native et la
//! coquille Tauri appellent [`Launch::dispatch`] en première ligne de leur `main`. Même
//! exécutable, donc même révision de la composition du texte : deux binaires de dates
//! différentes ne peuvent pas se disputer un magasin.
//!
//! Il tourne **en priorité minimale** — `IDLE_PRIORITY_CLASS` sous Windows, `nice -n 19`
//! ailleurs —, et on le tue sans scrupule : la passe écrit ses vecteurs avant de poser ses
//! drapeaux (critère 3), et le verrou du magasin meurt avec lui.
//!
//! ## Ce que la passe de fond ne fait jamais
//!
//! **Effacer ou remettre à zéro.** Un magasin périmé, divergent, d'un autre modèle, ou un arriéré
//! de plus de [`mailvec::pass::FOLLOW_LIMIT`] messages demandent une passe explicite —
//! `mail vectors` —, et l'état le dit : [`Status::NeedsExplicitPass`]. La règle vit dans
//! `mailvec::pass::plan`, que le démon et l'enfant appellent tous deux.
//!
//! ## Quand
//!
//! Jamais dans la première minute — le démon démarre derrière la première image de la coquille.
//! Puis à la fin de chaque job, et toutes les dix minutes pour le courrier rangé par un autre
//! processus. **Jamais pendant un job** : une moisson et une passe ensemble feraient la somme de
//! leurs crêtes. Un job qui démarre tue l'enfant en cours ; il reprendra à la fin du job.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use mailvec::pass::{Explicit, Plan};

use crate::jobs::Activity;

/// L'argument qui fait d'un exécutable la passe de fond, suivi de la racine du store.
pub const CHILD_FLAG: &str = "--mailcore-vectorise";

/// Combien attendre après le démarrage avant le premier passage.
const STARTUP_DELAY: Duration = Duration::from_secs(60);

/// La variable qui raccourcit [`STARTUP_DELAY`], en millisecondes — pour les tests et les bancs,
/// jamais pour l'utilisateur. Le même précédent que `MAILCORE_UI_POSITION`.
pub const DELAY_ENV: &str = "MAILCORE_VECTORS_DELAY_MS";

/// Le filet : le courrier rangé par un autre processus — `mail sync`, `mail import` — n'envoie
/// aucun réveil.
const TICK: Duration = Duration::from_secs(10 * 60);

/// Les reculs après un échec passager : de trente secondes à un quart d'heure, comme la veille.
const BACKOFF_MIN: Duration = Duration::from_secs(30);
const BACKOFF_MAX: Duration = Duration::from_secs(15 * 60);

/// À quelle fréquence regarder l'enfant pendant qu'il travaille : s'il a fini, et s'il faut le
/// tuer parce qu'un job a démarré.
const WATCH: Duration = Duration::from_millis(200);

/// Pourquoi une passe de fond a échoué. Des codes, jamais une phrase : rien de ce qui sort d'ici
/// ne porte de chemin — `docs/PRIVACY.md`, §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Le store ne s'ouvre pas.
    Store,
    /// Le modèle est là et ne se charge pas.
    Model,
    /// L'encodage ou l'écriture a échoué.
    Pass,
    /// L'enfant n'a pas pu être lancé.
    Spawn,
    /// L'enfant a répondu autre chose que ce que le protocole prévoit, ou rien.
    Protocol,
}

impl Failure {
    const fn code(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Model => "model",
            Self::Pass => "pass",
            Self::Spawn => "spawn",
            Self::Protocol => "protocol",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        [
            Self::Store,
            Self::Model,
            Self::Pass,
            Self::Spawn,
            Self::Protocol,
        ]
        .into_iter()
        .find(|failure| failure.code() == code)
    }
}

/// Où en est la passe de fond.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Pas de passe de fond : l'exécutable ne sait pas se relancer — un test, un outil.
    Disabled,
    /// Le délai de démarrage n'est pas écoulé.
    Waiting,
    /// Aucun modèle installé : la recherche plein texte marche, le sémantique attend son modèle.
    ModelAbsent,
    /// Un job est en cours : la passe attend qu'il finisse.
    Paused,
    /// Rien à faire.
    Idle,
    /// L'enfant travaille, sur au plus `pending` messages.
    Working {
        /// Combien attendaient au lancement.
        pending: u64,
    },
    /// Une autre passe — `mail vectors` — tient le magasin.
    Busy,
    /// La passe de fond refuse ; une passe explicite est nécessaire.
    NeedsExplicitPass {
        /// Pourquoi.
        reason: Explicit,
        /// Combien de messages attendent.
        pending: u64,
    },
    /// Le dernier passage a échoué.
    Failed(Failure),
}

/// Comment relancer l'exécutable en passe de fond.
#[derive(Debug, Clone)]
pub enum Launch {
    /// Pas de passe de fond.
    Disabled,
    /// Ce programme, avec [`CHILD_FLAG`] et la racine du store.
    Program(PathBuf),
}

impl Launch {
    /// **À appeler en première ligne de `main`**, avant toute analyse d'arguments.
    ///
    /// Si l'exécutable a été lancé en passe de fond, fait la passe et **termine le processus** ;
    /// sinon rend de quoi le relancer plus tard. Les trois exécutables qui ouvrent un
    /// [`crate::Service`] l'appellent : c'est ce qui garantit que l'enfant sait faire la passe.
    #[must_use]
    pub fn dispatch() -> Self {
        let mut args = std::env::args_os().skip(1);
        if args.next().as_deref() == Some(std::ffi::OsStr::new(CHILD_FLAG)) {
            let code = match args.next().map(OsString::into_string) {
                Some(Ok(root)) => child::run(Utf8Path::new(&root)),
                _ => 2,
            };
            std::process::exit(code);
        }
        std::env::current_exe().map_or(Self::Disabled, Self::Program)
    }

    /// Pas de passe de fond.
    #[must_use]
    pub const fn disabled() -> Self {
        Self::Disabled
    }

    /// Un programme donné — pour les tests, qui relancent le binaire `maild` compilé.
    #[must_use]
    pub fn program(path: impl Into<PathBuf>) -> Self {
        Self::Program(path.into())
    }
}

/// La ligne que l'enfant écrit en partant. Un protocole fermé : toute autre ligne est un échec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done { vectorised: u64, unreadable: u64 },
    Nothing,
    Explicit { reason: Explicit, pending: u64 },
    Busy,
    ModelAbsent,
    Failed(Failure),
}

impl Outcome {
    fn line(self) -> String {
        match self {
            Self::Done {
                vectorised,
                unreadable,
            } => format!("done {vectorised} {unreadable}"),
            Self::Nothing => "nothing".to_owned(),
            Self::Explicit { reason, pending } => format!("explicit {} {pending}", reason.code()),
            Self::Busy => "busy".to_owned(),
            Self::ModelAbsent => "model-absent".to_owned(),
            Self::Failed(failure) => format!("failed {}", failure.code()),
        }
    }

    fn parse(line: &str) -> Option<Self> {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["done", vectorised, unreadable] => Some(Self::Done {
                vectorised: vectorised.parse().ok()?,
                unreadable: unreadable.parse().ok()?,
            }),
            ["nothing"] => Some(Self::Nothing),
            ["explicit", reason, pending] => Some(Self::Explicit {
                reason: Explicit::from_code(reason)?,
                pending: pending.parse().ok()?,
            }),
            ["busy"] => Some(Self::Busy),
            ["model-absent"] => Some(Self::ModelAbsent),
            ["failed", code] => Some(Self::Failed(Failure::from_code(code)?)),
            _ => None,
        }
    }
}

/// L'enfant : un passage, une ligne, et il sort.
mod child {
    use super::{Failure, Outcome};
    use camino::Utf8Path;
    use mailcore::{Progress, Store};
    use mailvec::pass::{Followed, Plan};

    /// Fait un passage et rend le code de sortie du processus.
    pub(super) fn run(root: &Utf8Path) -> i32 {
        // Un journal sur la sortie d'erreur, que le parent recopie dans le sien.
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .try_init();

        // **Le parent mort, l'enfant sort.** Il tient l'entrée standard ouverte sans rien y
        // écrire : la fin de fichier ne vient que de sa disparition.
        let _ = std::thread::Builder::new()
            .name("mailcore-vecteurs-parent".to_owned())
            .spawn(|| {
                let mut sink = Vec::new();
                let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut sink);
                tracing::info!("parent disparu : passe de fond arrêtée");
                std::process::exit(3);
            });

        let outcome = round(root);
        println!("{}", outcome.line());
        0
    }

    /// Le passage : verrou, question, modèle, passe — **dans cet ordre**, du moins cher au plus
    /// cher. Une passe qui perd la course au verrou coûte un démarrage de processus, pas 343 Mio
    /// de modèle.
    fn round(root: &Utf8Path) -> Outcome {
        let Ok(store) = Store::open(root) else {
            return Outcome::Failed(Failure::Store);
        };
        let lock = match mailcore::vectors::VectorLock::try_acquire(&store.vectors_path()) {
            Ok(lock) => lock,
            Err(mailcore::Error::VectorsBusy { .. }) => return Outcome::Busy,
            Err(error) => {
                tracing::warn!(%error, "verrou des vecteurs");
                return Outcome::Failed(Failure::Store);
            }
        };
        let budget = mailvec::text::Budget::default();
        match mailvec::pass::plan(&store, budget, None) {
            Ok(Plan::Nothing) => return Outcome::Nothing,
            Ok(Plan::Explicit { reason, pending }) => return Outcome::Explicit { reason, pending },
            Ok(Plan::Follow { .. }) => {}
            Err(error) => {
                tracing::warn!(%error, "état des vecteurs");
                return Outcome::Failed(Failure::Store);
            }
        }
        let dir = mailvec::bert::default_dir(root);
        if !dir.join("config.json").is_file() {
            return Outcome::ModelAbsent;
        }
        let model = match mailvec::bert::Bert::load(&dir) {
            Ok(model) => model,
            Err(error) => {
                tracing::warn!(%error, "modèle illisible");
                return Outcome::Failed(Failure::Model);
            }
        };
        match mailvec::pass::follow_locked(&store, &model, budget, &Progress::new(), &lock) {
            Ok(Followed::Done(stats)) => Outcome::Done {
                vectorised: stats.vectorised,
                unreadable: stats.unreadable,
            },
            Ok(Followed::Explicit { reason, pending }) => Outcome::Explicit { reason, pending },
            Ok(Followed::Busy) => Outcome::Busy,
            Err(error) => {
                tracing::warn!(%error, "passe de fond en échec");
                Outcome::Failed(Failure::Pass)
            }
        }
    }
}

/// Ce que le fil et ses propriétaires partagent.
struct Shared {
    stop: AtomicBool,
    /// Le réveil demandé, et ce sur quoi le fil dort.
    wake: (Mutex<bool>, Condvar),
    status: Mutex<Status>,
    /// L'enfant en cours, pour que l'arrêt le tue sans attendre le fil.
    child: Mutex<Option<Child>>,
}

impl Shared {
    fn nudge(&self) {
        if let Ok(mut posted) = self.wake.0.lock() {
            *posted = true;
        }
        self.wake.1.notify_all();
    }

    fn set(&self, status: Status) {
        if let Ok(mut slot) = self.status.lock() {
            *slot = status;
        }
    }

    fn kill_child(&self) {
        if let Ok(mut slot) = self.child.lock()
            && let Some(child) = slot.as_mut()
        {
            let _ = child.kill();
        }
    }

    /// Dort jusqu'au réveil, à l'arrêt, ou au bout de `timeout`. Consomme le réveil.
    fn sleep(&self, timeout: Duration) {
        let Ok(mut posted) = self.wake.0.lock() else {
            return;
        };
        let deadline = Instant::now() + timeout;
        while !*posted && !self.stop.load(Ordering::SeqCst) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match self.wake.1.wait_timeout(posted, left) {
                Ok((guard, _)) => posted = guard,
                Err(_) => return,
            }
        }
        *posted = false;
    }
}

/// Le fil qui décide de la passe de fond et la lance.
pub struct Vectoriser {
    shared: Arc<Shared>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Vectoriser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vectoriser")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl Vectoriser {
    /// Démarre le fil. **N'ouvre rien sur le chemin appelant** : le store est ouvert dans le fil,
    /// après le délai de démarrage, comme le facteur et la veille.
    #[must_use]
    pub fn start(store: &Utf8Path, activity: Activity, launch: Launch) -> Self {
        let delay = std::env::var(DELAY_ENV)
            .ok()
            .and_then(|ms| ms.parse::<u64>().ok())
            .map_or(STARTUP_DELAY, Duration::from_millis);
        Self::start_after(store, activity, launch, delay)
    }

    /// [`Self::start`], avec un délai de démarrage donné — pour les tests, qui ne peuvent pas
    /// poser de variable d'environnement sans `unsafe`.
    #[must_use]
    pub fn start_after(
        store: &Utf8Path,
        activity: Activity,
        launch: Launch,
        delay: Duration,
    ) -> Self {
        let initial = if matches!(launch, Launch::Disabled) {
            Status::Disabled
        } else {
            Status::Waiting
        };
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            wake: (Mutex::new(false), Condvar::new()),
            status: Mutex::new(initial),
            child: Mutex::new(None),
        });
        let Launch::Program(program) = launch else {
            return Self {
                shared,
                handle: None,
            };
        };

        // Chaque fin de job réveille le fil. Une référence faible : le compteur vit dans les jobs,
        // qui peuvent survivre au fil.
        let weak = Arc::downgrade(&shared);
        activity.listen(move || {
            if let Some(shared) = weak.upgrade() {
                shared.nudge();
            }
        });

        let thread_shared = Arc::clone(&shared);
        let root = store.to_owned();
        let handle = std::thread::Builder::new()
            .name("mailcore-vecteurs".to_owned())
            .spawn(move || run(&root, &program, &activity, &thread_shared, delay))
            .map_err(|error| tracing::error!(%error, "passe de fond des vecteurs non démarrée"))
            .ok();
        Self { shared, handle }
    }

    /// Où en est la passe de fond.
    #[must_use]
    pub fn status(&self) -> Status {
        self.shared
            .status
            .lock()
            .map_or(Status::Disabled, |status| *status)
    }

    /// Réveille le fil tout de suite.
    pub fn nudge(&self) {
        self.shared.nudge();
    }

    /// Arrête le fil : tue l'enfant en cours, et attend le fil — borné, l'enfant étant mort.
    pub fn stop(mut self) {
        self.halt();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn halt(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.kill_child();
        self.shared.nudge();
    }
}

impl Drop for Vectoriser {
    /// Arrête sans attendre : une coquille qui se ferme ne patiente pas derrière un enfant.
    fn drop(&mut self) {
        self.halt();
    }
}

/// La boucle du fil.
fn run(
    root: &Utf8Path,
    program: &std::path::Path,
    activity: &Activity,
    shared: &Shared,
    delay: Duration,
) {
    let started = Instant::now();
    while started.elapsed() < delay && !shared.stop.load(Ordering::SeqCst) {
        shared.sleep(delay.saturating_sub(started.elapsed()));
    }

    let mut store: Option<mailcore::Store> = None;
    let mut backoff = BACKOFF_MIN;
    let mut retry_at: Option<Instant> = None;
    // L'empreinte de l'état qui a rendu un refus durable : tant qu'elle ne change pas, relancer un
    // enfant redonnerait le même refus.
    let mut refused: Option<Fingerprint> = None;

    while !shared.stop.load(Ordering::SeqCst) {
        let again = round(
            root,
            program,
            activity,
            shared,
            &mut store,
            &mut backoff,
            &mut retry_at,
            &mut refused,
        );
        if !again {
            shared.sleep(TICK);
        }
    }
    tracing::info!("passe de fond des vecteurs arrêtée");
}

/// Ce qui, en changeant, peut lever un refus durable : les fichiers du modèle et le magasin.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint(Vec<Option<(u64, std::time::SystemTime)>>);

impl Fingerprint {
    fn of(root: &Utf8Path) -> Self {
        let model = mailvec::bert::default_dir(root);
        let files: Vec<Utf8PathBuf> = vec![
            model.join("model.safetensors"),
            model.join("config.json"),
            model.join("tokenizer.json"),
            root.join("vectors.bin"),
        ];
        Self(
            files
                .iter()
                .map(|file| {
                    std::fs::metadata(file.as_std_path())
                        .ok()
                        .and_then(|meta| Some((meta.len(), meta.modified().ok()?)))
                })
                .collect(),
        )
    }
}

/// Un passage du fil. Rend vrai s'il faut repasser tout de suite.
#[allow(clippy::too_many_arguments)]
fn round(
    root: &Utf8Path,
    program: &std::path::Path,
    activity: &Activity,
    shared: &Shared,
    store: &mut Option<mailcore::Store>,
    backoff: &mut Duration,
    retry_at: &mut Option<Instant>,
    refused: &mut Option<Fingerprint>,
) -> bool {
    // Du moins cher au plus cher : un job actif, un recul en cours, un refus que rien n'a levé,
    // l'absence de modèle, puis seulement la question au store.
    if activity.active() > 0 {
        shared.set(Status::Paused);
        return false;
    }
    if retry_at.is_some_and(|at| Instant::now() < at) {
        return false;
    }
    let fingerprint = Fingerprint::of(root);
    if refused.as_ref() == Some(&fingerprint) {
        return false;
    }
    *refused = None;
    if !mailvec::bert::default_dir(root)
        .join("config.json")
        .is_file()
    {
        shared.set(Status::ModelAbsent);
        return false;
    }
    if store.is_none() {
        match mailcore::Store::open(root) {
            Ok(opened) => *store = Some(opened),
            Err(error) => {
                tracing::warn!(%error, "store illisible pour la passe de fond");
                fail(shared, Failure::Store, backoff, retry_at);
                return false;
            }
        }
    }
    let Some(opened) = store.as_ref() else {
        return false;
    };
    let pending = match mailvec::pass::plan(opened, mailvec::text::Budget::default(), None) {
        Ok(Plan::Nothing) => {
            shared.set(Status::Idle);
            return false;
        }
        Ok(Plan::Explicit { reason, pending }) => {
            shared.set(Status::NeedsExplicitPass { reason, pending });
            *refused = Some(fingerprint);
            return false;
        }
        Ok(Plan::Follow { pending }) => pending,
        Err(error) => {
            tracing::warn!(%error, "état des vecteurs illisible");
            fail(shared, Failure::Store, backoff, retry_at);
            return false;
        }
    };

    shared.set(Status::Working { pending });
    match spawn_and_wait(root, program, activity, shared) {
        Waited::Killed => {
            // Un job a démarré, ou le démon s'arrête : ce qui est écrit reste écrit.
            shared.set(Status::Paused);
            false
        }
        Waited::Outcome(outcome) => {
            settle(outcome, shared, backoff, retry_at, refused, fingerprint)
        }
    }
}

/// Range ce que l'enfant a rendu. Rend vrai s'il faut repasser tout de suite.
fn settle(
    outcome: Outcome,
    shared: &Shared,
    backoff: &mut Duration,
    retry_at: &mut Option<Instant>,
    refused: &mut Option<Fingerprint>,
    fingerprint: Fingerprint,
) -> bool {
    match outcome {
        Outcome::Done {
            vectorised,
            unreadable,
        } => {
            *backoff = BACKOFF_MIN;
            *retry_at = None;
            if unreadable > 0 {
                tracing::warn!(unreadable, "messages illisibles marqués sans vecteur");
            }
            tracing::info!(vectorised, "passe de fond des vecteurs");
            shared.set(Status::Idle);
            // Repasser tout de suite si elle a avancé : ce qui est arrivé pendant reste à faire.
            // Jamais après un passage sans progrès — c'est la boucle qui ne finit jamais.
            vectorised > 0
        }
        Outcome::Nothing => {
            shared.set(Status::Idle);
            false
        }
        Outcome::Explicit { reason, pending } => {
            shared.set(Status::NeedsExplicitPass { reason, pending });
            *refused = Some(fingerprint);
            false
        }
        Outcome::Busy => {
            shared.set(Status::Busy);
            false
        }
        Outcome::ModelAbsent => {
            shared.set(Status::ModelAbsent);
            false
        }
        Outcome::Failed(Failure::Model) => {
            // Un modèle qui ne se charge pas ne se chargera pas mieux dans trente secondes :
            // attendre qu'un de ses fichiers change.
            shared.set(Status::Failed(Failure::Model));
            *refused = Some(fingerprint);
            false
        }
        Outcome::Failed(failure) => {
            fail(shared, failure, backoff, retry_at);
            false
        }
    }
}

/// Un échec passager : le dire, et reculer.
fn fail(shared: &Shared, failure: Failure, backoff: &mut Duration, retry_at: &mut Option<Instant>) {
    shared.set(Status::Failed(failure));
    *retry_at = Some(Instant::now() + *backoff);
    *backoff = (*backoff * 2).min(BACKOFF_MAX);
}

enum Waited {
    Killed,
    Outcome(Outcome),
}

/// Lance l'enfant, le surveille, et le tue si un job démarre ou si le démon s'arrête.
fn spawn_and_wait(
    root: &Utf8Path,
    program: &std::path::Path,
    activity: &Activity,
    shared: &Shared,
) -> Waited {
    let mut command = command(program, root);
    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, "passe de fond non lancée");
            return Waited::Outcome(Outcome::Failed(Failure::Spawn));
        }
    };

    // Le journal de l'enfant, recopié dans celui du démon, ligne par ligne.
    if let Some(stderr) = child.stderr.take() {
        let _ = std::thread::Builder::new()
            .name("mailcore-vecteurs-journal".to_owned())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    tracing::debug!(enfant = %line, "passe de fond");
                }
            });
    }
    let stdout = child.stdout.take();
    // L'entrée standard reste ouverte, dans le fil : la fermer dirait à l'enfant que le parent est
    // mort.
    let _stdin = child.stdin.take();

    if let Ok(mut slot) = shared.child.lock() {
        *slot = Some(child);
    }
    let mut killed = false;
    loop {
        if shared.stop.load(Ordering::SeqCst) || activity.active() > 0 {
            shared.kill_child();
            killed = true;
        }
        let exited = shared
            .child
            .lock()
            .ok()
            .and_then(|mut slot| slot.as_mut().map(Child::try_wait));
        match exited {
            Some(Ok(None)) => std::thread::sleep(WATCH),
            _ => break,
        }
    }
    if let Ok(mut slot) = shared.child.lock() {
        *slot = None;
    }

    // Tué par nous : ce n'est pas une réponse, c'est un arrêt. Mort tout seul, ou sorti normalement,
    // c'est sa dernière ligne qui le dit — et sans ligne valable, c'est un échec de protocole.
    if killed {
        return Waited::Killed;
    }
    let mut output = String::new();
    if let Some(mut stdout) = stdout {
        let _ = stdout.read_to_string(&mut output);
    }
    Waited::Outcome(
        output
            .lines()
            .last()
            .and_then(Outcome::parse)
            .unwrap_or(Outcome::Failed(Failure::Protocol)),
    )
}

/// La commande qui relance l'exécutable en passe de fond, en priorité minimale.
fn command(program: &std::path::Path, root: &Utf8Path) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `IDLE_PRIORITY_CLASS` : le système ne lui donne le processeur que quand personne d'autre
        /// n'en veut.
        const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
        /// `CREATE_NO_WINDOW` : pas de console qui s'ouvre à côté de la coquille.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = Command::new(program);
        command
            .arg(CHILD_FLAG)
            .arg(root.as_str())
            .creation_flags(IDLE_PRIORITY_CLASS | CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    {
        // `nice` plutôt qu'un appel système : `#![forbid(unsafe_code)]`, et aucune dépendance de
        // plus. Sans `nice` sur la machine, l'enfant tourne en priorité normale — moins bien, pas
        // faux.
        let nice = ["/usr/bin/nice", "/bin/nice"]
            .into_iter()
            .find(|path| std::path::Path::new(path).is_file());
        let mut command = match nice {
            Some(nice) => {
                let mut command = Command::new(nice);
                command.args(["-n", "19"]).arg(program);
                command
            }
            None => Command::new(program),
        };
        command.arg(CHILD_FLAG).arg(root.as_str());
        command
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Explicit, Failure, Outcome};

    #[test]
    fn every_outcome_survives_its_line() {
        for outcome in [
            Outcome::Done {
                vectorised: 12,
                unreadable: 1,
            },
            Outcome::Nothing,
            Outcome::Explicit {
                reason: Explicit::Backlog,
                pending: 73_658,
            },
            Outcome::Busy,
            Outcome::ModelAbsent,
            Outcome::Failed(Failure::Model),
        ] {
            assert_eq!(Outcome::parse(&outcome.line()), Some(outcome));
        }
    }

    #[test]
    fn a_line_outside_the_protocol_is_refused() {
        for line in [
            "",
            "done",
            "done 1",
            "done x 0",
            "explicit inconnu 3",
            "failed rien",
            "bonjour",
            "nothing de plus",
        ] {
            assert_eq!(Outcome::parse(line), None, "{line:?}");
        }
    }
}

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
//! **Effacer ou remettre à zéro.** Un magasin périmé, divergent ou d'un autre modèle demande une
//! passe explicite — `mail vectors` —, et l'état le dit : [`Status::NeedsExplicitPass`]. La
//! règle vit dans `mailvec::pass::plan`, que le démon et l'enfant appellent tous deux.
//!
//! ## Un arriéré se rattrape tout seul, par passages bornés
//!
//! Depuis le 2026-10-19 — avant, au-delà de mille messages, la passe de fond refusait, et trois
//! semaines sans ouvrir l'application suffisaient. Un arriéré n'efface rien : il ne fait
//! qu'ajouter. Chaque enfant en fait au plus [`mailvec::pass::ROUND`] et sort, le fil en relance
//! un aussitôt tant qu'un passage avance : la mémoire est rendue entre deux passages, un job qui
//! démarre ne fait perdre qu'un passage, et l'état — [`Status::Working`], lisible par
//! `semantic.status` hors de toute recherche — dit combien il en reste.
//!
//! ## Quand
//!
//! Jamais dans la première minute — le démon démarre derrière la première image de la coquille.
//! Puis à la fin de chaque job, et toutes les dix minutes pour le courrier rangé par un autre
//! processus. **Jamais pendant un job** : une moisson et une passe ensemble feraient la somme de
//! leurs crêtes. Un job qui démarre tue l'enfant en cours ; il reprendra à la fin du job.
//!
//! ## Qui la fait, quand on vient de chercher
//!
//! L'enfant de la recherche par le sens garde le modèle cinq minutes après la dernière requête.
//! Pendant ce temps, la passe de fond ne lance pas le sien — deux copies du modèle — : elle **lui
//! confie** son travail, par lots courts ([`Vectoriser::delegate_to`]). Une arrivée de courrier
//! ne coûte alors que son encodage, au lieu d'un processus et d'un chargement du modèle — 0,24 s
//! des 0,59 s mesurées le 2026-10-09. Le reste du temps, rien ne change.

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

/// Combien de messages porte au plus un lot confié à l'enfant de la recherche.
///
/// Une recherche qui arrive pendant un lot attend ce lot : il doit rester court. Une arrivée de
/// courrier en compte un à quelques-uns ; un arriéré plus long se fait en plusieurs lots, entre
/// lesquels les recherches passent.
const DELEGATED_BATCH: u64 = 8;

/// Au-delà de combien de messages en attente le travail n'est **pas** confié à l'enfant de la
/// recherche, même chaud.
///
/// La délégation est faite pour une arrivée : quelques messages, encodés par un modèle déjà
/// chargé. Mais elle tourne en priorité normale, et chaque lot fait attendre les recherches. Un
/// arriéré de mille messages en ferait cent vingt-cinq lots, pendant qu'on cherche. Au-delà de
/// ce seuil, la passe attend que l'enfant de la recherche s'éteigne — cinq minutes après la
/// dernière recherche, sa mort réveille ce fil — et lance son propre enfant, en priorité
/// minimale. Soixante-quatre : huit lots, quelques secondes.
const DELEGATED_AT_MOST: u64 = 64;

/// La variable qui fixe la borne d'un passage de l'enfant, à la place de
/// [`mailvec::pass::ROUND`] — pour les tests, jamais pour l'utilisateur. Le parent la pose
/// toujours sur la commande de l'enfant : `Command::env` ne demande pas d'`unsafe`, et un test
/// passe sa borne à [`Vectoriser::start_with_round`].
pub const ROUND_ENV: &str = "MAILCORE_VECTORS_ROUND";

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
    /// Un arriéré attend que l'enfant de la recherche s'éteigne — cinq minutes après la dernière
    /// recherche — pour être fait en priorité minimale plutôt qu'à côté des recherches.
    Deferred {
        /// Combien de messages attendent.
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
        let flag = args.next();
        let serve: Option<fn(&Utf8Path) -> i32> = match flag.as_deref() {
            Some(f) if f == std::ffi::OsStr::new(CHILD_FLAG) => Some(child::run),
            Some(f) if f == std::ffi::OsStr::new(crate::semantic::CHILD_FLAG) => {
                Some(crate::semantic::child::run)
            }
            _ => None,
        };
        if let Some(serve) = serve {
            let code = match args.next().map(OsString::into_string) {
                Some(Ok(root)) => serve(Utf8Path::new(&root)),
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

/// Ce qu'un passage de la passe de fond a donné — la ligne que l'enfant écrit en partant, ou que
/// l'enfant de la recherche rend pour un lot. Un protocole fermé : toute autre ligne est un échec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Des messages ont reçu leur vecteur.
    Done {
        /// Combien.
        vectorised: u64,
        /// Combien étaient illisibles, et ont été marqués sans vecteur.
        unreadable: u64,
    },
    /// Rien à faire.
    Nothing,
    /// La passe de fond refuse ; une passe explicite est nécessaire.
    Explicit {
        /// Pourquoi.
        reason: Explicit,
        /// Combien de messages attendent.
        pending: u64,
    },
    /// Une autre passe tient le magasin.
    Busy,
    /// Aucun modèle installé.
    ModelAbsent,
    /// Le passage a échoué.
    Failed(Failure),
}

impl Outcome {
    pub(crate) fn line(self) -> String {
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

    pub(crate) fn parse(line: &str) -> Option<Self> {
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

/// Ce qu'une passe de fond rend, dans les mots du protocole — pour l'enfant de la passe et pour
/// l'enfant de la recherche, qui la fait aussi quand il est chaud.
pub(crate) fn outcome_of(followed: mailvec::Result<mailvec::pass::Followed>) -> Outcome {
    use mailvec::pass::Followed;
    match followed {
        // Un blob absent est marqué comme un illisible : pour le parent, les deux sont du
        // progrès sans vecteur, et un passage qui n'a fait que ça a quand même avancé.
        Ok(Followed::Done(stats)) => Outcome::Done {
            vectorised: stats.vectorised,
            unreadable: stats.unreadable + stats.missing_blobs,
        },
        Ok(Followed::Explicit { reason, pending }) => Outcome::Explicit { reason, pending },
        Ok(Followed::Busy) => Outcome::Busy,
        Err(error) => {
            tracing::warn!(%error, "passe de fond en échec");
            Outcome::Failed(Failure::Pass)
        }
    }
}

/// L'enfant : un passage, une ligne, et il sort.
mod child {
    use super::{Failure, Outcome};
    use camino::Utf8Path;
    use mailcore::{Progress, Store};
    use mailvec::pass::Plan;

    /// Fait un passage et rend le code de sortie du processus.
    pub(super) fn run(root: &Utf8Path) -> i32 {
        // Un journal sur la sortie d'erreur, que le parent recopie dans le sien — donc, depuis la
        // coquille, dans un fichier : voir `crate::semantic::child::journal`.
        crate::semantic::child::journal();

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
        // Un passage borné : le parent relance aussitôt tant qu'un passage avance.
        let at_most = std::env::var(super::ROUND_ENV)
            .ok()
            .and_then(|round| round.parse::<u64>().ok())
            .filter(|round| *round > 0)
            .unwrap_or(mailvec::pass::ROUND);
        super::outcome_of(mailvec::pass::follow_some(
            &store,
            &model,
            budget,
            &Progress::new(),
            &lock,
            at_most,
        ))
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
    /// L'enfant de la recherche par le sens, à qui confier le travail quand il est chaud. Faible :
    /// la passe de fond ne le garde pas en vie.
    delegate: Mutex<Option<std::sync::Weak<crate::semantic::Engine>>>,
    /// Combien de lots lui ont été confiés — pour les tests et le diagnostic.
    delegated: std::sync::atomic::AtomicU64,
    /// Combien d'enfants de la passe ont été lancés — pour les tests et le diagnostic.
    launched: std::sync::atomic::AtomicU64,
    /// La borne d'un passage de l'enfant : [`mailvec::pass::ROUND`], sauf dans un test.
    round: u64,
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
        Self::start_with_round(store, activity, launch, delay, mailvec::pass::ROUND)
    }

    /// [`Self::start_after`], avec une borne de passage donnée — pour qu'un test voie un arriéré
    /// rattrapé en plusieurs passages sans fabriquer mille messages.
    #[must_use]
    pub fn start_with_round(
        store: &Utf8Path,
        activity: Activity,
        launch: Launch,
        delay: Duration,
        round: u64,
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
            delegate: Mutex::new(None),
            delegated: std::sync::atomic::AtomicU64::new(0),
            launched: std::sync::atomic::AtomicU64::new(0),
            round: round.max(1),
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

    /// Confie la passe à l'enfant de la recherche par le sens **quand il est chaud**.
    ///
    /// Sans ça, une arrivée de courrier payait un enfant de plus et un chargement du modèle —
    /// 0,24 s sur les 0,59 s d'une arrivée mesurée le 2026-10-09 —, et attendait même que l'enfant
    /// de la recherche meure si l'on venait de chercher. Avec, une arrivée pendant qu'on cherche
    /// ne coûte que son encodage. Hors de ces moments, rien ne change : l'enfant de la recherche
    /// n'est jamais réveillé pour la passe.
    pub fn delegate_to(&self, engine: &Arc<crate::semantic::Engine>) {
        if let Ok(mut slot) = self.shared.delegate.lock() {
            *slot = Some(Arc::downgrade(engine));
        }
    }

    /// Combien de lots ont été confiés à l'enfant de la recherche depuis le démarrage.
    #[must_use]
    pub fn delegated(&self) -> u64 {
        self.shared.delegated.load(Ordering::SeqCst)
    }

    /// Combien d'enfants de la passe ont été lancés depuis le démarrage.
    #[must_use]
    pub fn launched(&self) -> u64 {
        self.shared.launched.load(Ordering::SeqCst)
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
        let files: Vec<Utf8PathBuf> = mailvec::bert::MODEL_FILES
            .iter()
            .map(|name| model.join(name))
            .chain([root.join("vectors.bin")])
            .collect();
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
    //
    // Un **job**, pas l'enfant de la recherche : lui a déjà le modèle, et c'est à lui que la passe
    // est confiée plus bas.
    //
    // **Le refus durable et le recul passent avant le job**, depuis le 2026-10-19 : dans l'ordre
    // inverse, un job écrivait `Paused` par-dessus l'état qui disait pourquoi la passe ne tourne
    // pas, et il y restait jusqu'à ce que l'empreinte change. Un modèle qui ne se charge pas
    // devenait « la passe s'en charge dès que le démon est libre » — une promesse fausse, et une
    // coquille qui relevait l'état toutes les 500 ms pour toujours.
    let fingerprint = Fingerprint::of(root);
    if refused.as_ref() == Some(&fingerprint) {
        return false;
    }
    if retry_at.is_some_and(|at| Instant::now() < at) {
        return false;
    }
    if activity.jobs() > 0 {
        shared.set(Status::Paused);
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

    // **Un arriéré attend la mort de l'enfant de la recherche**, pour ne pas tourner en priorité
    // normale pendant qu'on cherche — voir `DELEGATED_AT_MOST`. Et l'état le dit tel quel : rien
    // ne tourne, et `Working` aurait annoncé un rattrapage en cours.
    if activity.held() > 0 && pending > DELEGATED_AT_MOST {
        shared.set(Status::Deferred { pending });
        return false;
    }
    shared.set(Status::Working { pending });
    // **L'enfant de la recherche vit : c'est lui qui fait la passe**, un lot à la fois. Jamais un
    // second enfant à côté — deux copies du modèle. S'il ne peut pas — il démarre encore, ou il n'a
    // pas répondu —, on attend : sa mise en route ou sa mort réveillent ce fil.
    if activity.held() > 0 {
        let engine = shared
            .delegate
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(std::sync::Weak::upgrade));
        return match engine.and_then(|engine| engine.follow(DELEGATED_BATCH)) {
            Some(outcome) => {
                shared.delegated.fetch_add(1, Ordering::SeqCst);
                settle(outcome, shared, backoff, retry_at, refused, fingerprint)
            }
            None => {
                shared.set(Status::Paused);
                false
            }
        };
    }
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
                tracing::warn!(
                    unreadable,
                    "messages illisibles ou absents marqués sans vecteur"
                );
            }
            tracing::info!(vectorised, "passe de fond des vecteurs");
            // Repasser tout de suite si elle a avancé : ce qui est arrivé pendant reste à faire.
            // Jamais après un passage sans progrès — c'est la boucle qui ne finit jamais.
            // **Avancé veut dire marqué**, vecteur ou non : un passage de messages illisibles
            // a réduit l'attente autant qu'un autre, et s'arrêter là laissait le reste dix
            // minutes sous un état « rien à faire ».
            let advanced = vectorised + unreadable > 0;
            // Et l'état reste `Working` entre deux passages : `Idle` le temps d'un relevé
            // suffisait à arrêter celui de la coquille.
            if !advanced {
                shared.set(Status::Idle);
            }
            advanced
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
            tracing::warn!(
                "modèle illisible : la passe de fond attend qu'un de ses fichiers change"
            );
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
    // Le code, dans le journal du parent : l'enfant a dit pourquoi, mais il peut être mort
    // avant d'avoir écrit quoi que ce soit — un lancement refusé, une ligne illisible.
    tracing::warn!(
        echec = failure.code(),
        attente = backoff.as_secs(),
        "passe de fond en échec"
    );
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
    command.env(ROUND_ENV, shared.round.to_string());
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
    shared.launched.fetch_add(1, Ordering::SeqCst);

    // Le journal de l'enfant, recopié dans celui du démon, ligne par ligne — **en `info`**,
    // depuis le 2026-10-19. En `debug`, le fichier de la coquille ne le gardait pas, et un modèle
    // qui ne se charge pas faisait dire à l'écran « le journal du démon dit pourquoi » sur un
    // journal muet. L'enfant plafonne lui-même à `info` : voir `semantic::child::journal`.
    if let Some(stderr) = child.stderr.take() {
        let _ = std::thread::Builder::new()
            .name("mailcore-vecteurs-journal".to_owned())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    tracing::info!(enfant = %line, "passe de fond");
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
    use super::{Explicit, Failure, Fingerprint, Outcome};

    fn shared() -> super::Shared {
        super::Shared {
            stop: std::sync::atomic::AtomicBool::new(false),
            wake: (std::sync::Mutex::new(false), std::sync::Condvar::new()),
            status: std::sync::Mutex::new(super::Status::Waiting),
            child: std::sync::Mutex::new(None),
            delegate: std::sync::Mutex::new(None),
            delegated: std::sync::atomic::AtomicU64::new(0),
            launched: std::sync::atomic::AtomicU64::new(0),
            round: mailvec::pass::ROUND,
        }
    }

    #[test]
    fn a_job_does_not_write_paused_over_a_lasting_refusal() {
        // Dans l'ordre d'avant le 2026-10-19, un job écrivait `Paused` par-dessus un modèle qui ne
        // se charge pas, et l'écran promettait que la passe s'en chargerait.
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let shared = shared();
        let activity = crate::jobs::Activity::default();
        activity.begin();
        let program = std::path::Path::new("aucun-programme");
        let mut backoff = super::BACKOFF_MIN;

        shared.set(super::Status::Failed(Failure::Model));
        let mut refused = Some(Fingerprint::of(&root));
        let again = super::round(
            &root,
            program,
            &activity,
            &shared,
            &mut None,
            &mut backoff,
            &mut None,
            &mut refused,
        );
        assert!(!again);
        assert_eq!(
            *shared.status.lock().unwrap(),
            super::Status::Failed(Failure::Model)
        );

        // Le contrôle inverse : sans refus, le même job met bien la passe en pause.
        let again = super::round(
            &root,
            program,
            &activity,
            &shared,
            &mut None,
            &mut backoff,
            &mut None,
            &mut None,
        );
        assert!(!again);
        assert_eq!(*shared.status.lock().unwrap(), super::Status::Paused);
        activity.end();
    }

    #[test]
    fn a_settings_file_lifts_a_refusal() {
        // Ces fichiers font l'empreinte du modèle : en ajouter, en retirer ou en corriger un peut
        // lever un refus, donc le démon doit le voir. L'empreinte des quatre autres ne bougeait pas.
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let model = mailvec::bert::default_dir(&root);
        std::fs::create_dir_all(model.join("1_Pooling").as_std_path()).unwrap();
        for name in ["model.safetensors", "tokenizer.json", "config.json"] {
            std::fs::write(model.join(name).as_std_path(), "x").unwrap();
        }
        for name in mailvec::bert::SETTINGS {
            let before = Fingerprint::of(&root);
            std::fs::write(model.join(name).as_std_path(), "{}").unwrap();
            let added = Fingerprint::of(&root);
            assert_ne!(before, added, "{name} ajouté");
            std::fs::remove_file(model.join(name).as_std_path()).unwrap();
            assert_ne!(added, Fingerprint::of(&root), "{name} retiré");
        }
        // Le contrôle inverse : rien n'a bougé, rien ne change.
        assert_eq!(Fingerprint::of(&root), Fingerprint::of(&root));
    }

    #[test]
    fn every_outcome_survives_its_line() {
        for outcome in [
            Outcome::Done {
                vectorised: 12,
                unreadable: 1,
            },
            Outcome::Nothing,
            Outcome::Explicit {
                reason: Explicit::Stale,
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

//! La recherche par le sens, servie par un processus enfant.
//!
//! ## Pourquoi un enfant, ici aussi
//!
//! Chercher par le sens demande le modèle — 343 Mio — pour encoder la requête, et les vecteurs du
//! corpus — 108 Mio — pour la comparer. Dans le démon ou dans la coquille, c'est 450 Mio de plus,
//! au-dessus du plafond de 500 Mo de la phase 2 avant même la première fenêtre. L'enfant les
//! porte seul : il naît à la première requête, garde le modèle chaud tant qu'on cherche, et meurt
//! après [`IDLE`] sans requête — la mémoire revient au système.
//!
//! C'est le même exécutable que la passe de fond, relancé avec [`CHILD_FLAG`] ; voir
//! [`crate::vectors::Launch::dispatch`].
//!
//! ## Aucune recherche n'attend qu'il démarre
//!
//! Charger le modèle et les vecteurs prend une seconde au repos, beaucoup plus sur une machine
//! occupée. La première version l'attendait **dans la recherche** — jusqu'à une minute, verrou
//! du moteur tenu —, et la coquille sert ses requêtes une à une sur un seul fil : une recherche
//! froide figeait la liste des dossiers, l'ouverture d'un message, tout. La règle 3, que
//! `CLAUDE.md` étend au fil du service depuis le 2026-09-11. Une relecture l'a trouvé le
//! 2026-10-08.
//!
//! Le démarrage se fait donc **à côté** : la recherche qui le déclenche répond par les mots
//! seuls et le dit, et l'enfant sert les suivantes une fois prêt. Une réponse d'un enfant chaud
//! prend quelques dizaines de millisecondes ; au-delà de [`ANSWER_TIMEOUT`], il est malade, et la
//! recherche ne l'attend pas davantage.
//!
//! ## Un refus se retient
//!
//! Un enfant qui refuse — pas de vecteurs, vecteurs d'un autre modèle — a chargé 343 Mio pour le
//! dire. Le refus est donc retenu avec l'**empreinte** des fichiers qui l'ont décidé — taille et
//! date du modèle et du magasin — et redemandé seulement quand l'un d'eux change : c'est la règle
//! des refus durables de la passe de fond. Un échec passager, lui, se retente après [`RETRY`].
//!
//! ## L'enfant compte comme un travail lourd
//!
//! Il tient un [`crate::jobs::Held`] tant qu'il vit : la passe de fond des vecteurs attend qu'il
//! meure pour se lancer, et se fait tuer s'il naît. Sans ça, une recherche suivie d'une arrivée
//! de courrier mettait deux copies du modèle en mémoire.
//!
//! ## Le protocole
//!
//! Une ligne JSON par requête dans un sens, une ligne JSON par réponse dans l'autre : la requête
//! de l'utilisateur peut contenir n'importe quoi, et JSON l'échappe. L'enfant annonce d'abord
//! qu'il est prêt, ou pourquoi il ne l'est pas. Toute ligne hors protocole est un échec.
//!
//! ## Ce qu'il refuse
//!
//! **Des vecteurs écrits par un autre modèle** — autre famille, autres poids, autre dimension —,
//! le critère 9 du côté de la lecture : les comparer à une requête encodée par celui-ci rendrait
//! un classement de la bonne forme et sans aucun sens. Des vecteurs d'une **composition** plus
//! ancienne, eux, restent dans le même espace : ils se cherchent. La règle est
//! [`mailcore::vectors::Provenance::same_model`], la même que celle de la passe.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use camino::{Utf8Path, Utf8PathBuf};
use mailcore::MessageId;
use serde::{Deserialize, Serialize};

/// L'argument qui fait d'un exécutable le serveur de recherche, suivi de la racine du store.
pub const CHILD_FLAG: &str = "--mailcore-search";

/// Combien de temps l'enfant reste chaud sans requête.
const IDLE: Duration = Duration::from_secs(5 * 60);

/// Combien attendre que l'enfant soit prêt — charger le modèle et les vecteurs. Personne ne
/// l'attend : c'est la patience du fil de démarrage, pas celle d'une recherche.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// Combien une recherche attend la réponse d'un enfant **chaud**.
///
/// Encoder une requête et balayer 73 658 vecteurs prend quelques dizaines de millisecondes ;
/// relire le magasin quand une passe l'a fait grandir, quelques centaines. Trois secondes, c'est
/// un enfant malade — et c'est tout ce qu'une recherche lui accorde.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);

/// Après un échec passager, combien attendre avant de relancer un enfant.
const RETRY: Duration = Duration::from_secs(60);

/// Pourquoi la recherche par le sens ne répond pas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// Pas de recherche par le sens dans cet exécutable — un test, un outil.
    Disabled,
    /// Aucun modèle installé.
    ModelAbsent,
    /// Les vecteurs ont été écrits par un autre modèle que celui installé.
    Foreign,
    /// Pas encore de vecteurs.
    Empty,
    /// L'enfant démarre : cette recherche ne l'attend pas.
    Starting,
    /// L'enfant n'a pas démarré, pas répondu, ou répondu hors protocole.
    Failed,
}

#[derive(Debug, Serialize, Deserialize)]
struct Ask {
    q: String,
    depth: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Said {
    Ready { records: u64 },
    Ids(Vec<i64>),
    Error(String),
}

impl Unavailable {
    /// Un mot qui le désigne, pour un journal ou le protocole — jamais un chemin.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::ModelAbsent => "model-absent",
            Self::Foreign => "foreign",
            Self::Empty => "empty",
            Self::Starting => "starting",
            Self::Failed => "failed",
        }
    }

    fn from_code(code: &str) -> Self {
        match code {
            "model-absent" => Self::ModelAbsent,
            "foreign" => Self::Foreign,
            "empty" => Self::Empty,
            "disabled" => Self::Disabled,
            "starting" => Self::Starting,
            _ => Self::Failed,
        }
    }
}

/// Ce que le démon sait de la recherche par le sens à un instant donné.
#[derive(Debug, Clone, Copy)]
pub struct Facts {
    /// L'exécutable sait lancer l'enfant.
    pub enabled: bool,
    /// Un modèle est installé.
    pub model_installed: bool,
    /// Le magasin de vecteurs a au moins un record.
    pub vectors_present: bool,
    /// Où en est la passe de fond, si elle est montée.
    pub status: Option<crate::vectors::Status>,
    /// Pourquoi le sens n'a pas répondu — à **cette** requête, ou au dernier essai pour
    /// `server.hello`.
    pub failure: Option<Unavailable>,
    /// Le sens a été **demandé** pour cette réponse. Faux pour `server.hello`, qui décrit un état
    /// sans avoir rien cherché, et pour une requête à opérateurs.
    pub asked: bool,
    /// La requête emploie la grammaire du plein texte, donc le sens n'a pas été demandé.
    pub operators: bool,
}

/// La phrase et le geste, **composés après la décision** : l'état vient des faits, la phrase de
/// l'état — jamais l'inverse. Aucun chemin n'y entre (`docs/PRIVACY.md`, §8).
///
/// Une seule fonction pour `server.hello` et `search.query` : deux phrases écrites à deux endroits
/// pour le même état finiraient par se contredire. Et aucune phrase ne promet : elle dit ce qui
/// a eu lieu, ou ce qui est.
#[must_use]
pub fn describe(facts: &Facts) -> mailapi::dto::Semantic {
    use crate::vectors::{Failure, Status};
    use mailapi::dto::SemanticState as State;
    use mailvec::pass::Explicit;

    const WORDS_ONLY: &str = "Les résultats viennent des mots seuls.";
    const LOG: &str = "le journal du démon dit pourquoi.";
    let said = |state, sentence: String, gesture: Option<&str>| mailapi::dto::Semantic {
        state,
        sentence,
        gesture: gesture.map(str::to_owned),
    };

    if !facts.enabled || facts.failure == Some(Unavailable::Disabled) {
        return said(
            State::Disabled,
            format!("Ce démon ne propose pas la recherche par le sens. {WORDS_ONLY}"),
            None,
        );
    }
    if !facts.model_installed || facts.failure == Some(Unavailable::ModelAbsent) {
        return said(
            State::ModelAbsent,
            format!(
                "Aucun modèle n'est installé pour chercher par le sens. {WORDS_ONLY} Le modèle se \
                 télécharge une fois ; le courrier, lui, ne quitte jamais la machine."
            ),
            Some("mail model install"),
        );
    }
    // Avant tout état des vecteurs : une requête à opérateurs n'a pas demandé le sens, donc ni un
    // arriéré ni des vecteurs étrangers ne la concernent.
    if facts.operators {
        return said(
            State::Ready,
            "Requête à opérateurs : les résultats viennent des mots seuls, comme demandé."
                .to_owned(),
            None,
        );
    }
    if facts.failure == Some(Unavailable::Foreign) {
        return said(
            State::NeedsPass,
            format!(
                "Les vecteurs ont été calculés par un autre modèle que celui installé : ils sont à \
                 refaire avant de chercher par le sens. {WORDS_ONLY}"
            ),
            Some("mail vectors --bert --rebuild"),
        );
    }
    if facts.failure == Some(Unavailable::Starting) {
        return said(
            State::Starting,
            format!("La recherche par le sens démarre. {WORDS_ONLY}"),
            None,
        );
    }
    // Une passe explicite demandée : la recherche par le sens répond peut-être, mais sur une
    // partie du courrier seulement — ou pas du tout.
    if let Some(Status::NeedsExplicitPass { reason, pending }) = facts.status {
        let tail = if facts.asked && facts.failure.is_none() {
            "En attendant, le sens ne voit qu'une partie du courrier."
        } else if facts.vectors_present && facts.failure.is_none() {
            "Le sens ne voit qu'une partie du courrier."
        } else {
            WORDS_ONLY
        };
        let (sentence, gesture) = match reason {
            Explicit::Backlog => (
                format!(
                    "{} sans vecteur — trop pour la passe de fond, qui ne suit que le courrier \
                     qui arrive.",
                    messages_waiting(pending)
                ),
                Some("mail vectors --bert"),
            ),
            Explicit::Stale | Explicit::Diverged => (
                "Les vecteurs ne correspondent plus au courrier : une passe complète les refait."
                    .to_owned(),
                Some("mail vectors --bert"),
            ),
            Explicit::ModelChanged | Explicit::Legacy => (
                "Les vecteurs ont été calculés par un autre modèle, ou dans un format ancien : ils \
                 sont à refaire."
                    .to_owned(),
                Some("mail vectors --bert --rebuild"),
            ),
            Explicit::Newer => (
                "Les vecteurs ont été écrits par une version plus récente de mailcore : mettez \
                 l'application à jour."
                    .to_owned(),
                None,
            ),
        };
        return said(State::NeedsPass, format!("{sentence} {tail}"), gesture);
    }
    let working = match facts.status {
        Some(Status::Working { pending }) => Some(format!(
            "La recherche par le sens rattrape {} arrivé{}.",
            messages(pending),
            if pending > 1 { "s" } else { "" }
        )),
        Some(Status::Busy) => Some(
            "Une passe de vectorisation est en cours : le courrier le plus récent n'est pas encore \
             cherchable par le sens."
                .to_owned(),
        ),
        _ => None,
    };
    if facts.failure == Some(Unavailable::Empty) || !facts.vectors_present {
        return match working {
            Some(_) => said(
                State::Working,
                format!("Le courrier est en cours de vectorisation. {WORDS_ONLY}"),
                None,
            ),
            None => said(
                State::NeedsPass,
                format!("Le courrier n'a pas encore été vectorisé. {WORDS_ONLY}"),
                Some("mail vectors --bert"),
            ),
        };
    }
    if facts.failure == Some(Unavailable::Failed) {
        return said(
            State::Unavailable,
            format!("La recherche par le sens n'a pas répondu ; {LOG} {WORDS_ONLY}"),
            None,
        );
    }
    // Sans avoir rien cherché : un modèle que la passe de fond n'a pas pu charger ne chargera pas
    // mieux pour une recherche. Après une recherche qui a répondu, il s'est chargé.
    if !facts.asked && facts.status == Some(Status::Failed(Failure::Model)) {
        return said(
            State::Unavailable,
            format!("Le modèle installé ne se charge pas ; {LOG}"),
            None,
        );
    }
    if let Some(sentence) = working {
        return said(State::Working, sentence, None);
    }
    if facts.asked {
        said(
            State::Ready,
            "Les résultats mêlent les mots et le sens.".to_owned(),
            None,
        )
    } else {
        said(
            State::Ready,
            "Un modèle et des vecteurs sont installés.".to_owned(),
            None,
        )
    }
}

/// « 1 message », « 3 messages ».
fn messages(count: u64) -> String {
    if count > 1 {
        format!("{count} messages")
    } else {
        format!("{count} message")
    }
}

/// « 1 message attend », « 3 messages attendent ».
fn messages_waiting(count: u64) -> String {
    if count > 1 {
        format!("{count} messages attendent")
    } else {
        format!("{count} message attend")
    }
}

/// L'enfant : charge, annonce, répond, jusqu'à la fin de son entrée.
pub(crate) mod child {
    use super::{Ask, Said, Unavailable};
    use camino::Utf8Path;
    use mailvec::search::QueryEncoder;
    use std::io::{BufRead, Write};

    fn say(said: &Said) {
        if let Ok(line) = serde_json::to_string(said) {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }

    /// Refuse, et le dit dans le journal **avant** : le parent relaie ce journal, et la phrase
    /// montrée à l'utilisateur y renvoie.
    fn refuse(why: Unavailable, detail: &dyn std::fmt::Display) -> i32 {
        tracing::warn!(raison = why.code(), %detail, "recherche par le sens refusée");
        say(&Said::Error(why.code().to_owned()));
        0
    }

    /// Sert jusqu'à la fin de l'entrée standard, et rend le code de sortie.
    pub(crate) fn run(root: &Utf8Path) -> i32 {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .try_init();

        let store = match mailcore::Store::open(root) {
            Ok(store) => store,
            Err(error) => return refuse(Unavailable::Failed, &error),
        };
        let dir = mailvec::bert::default_dir(root);
        if !dir.join("config.json").is_file() {
            return refuse(Unavailable::ModelAbsent, &"pas de config.json");
        }
        let path = store.vectors_path();
        // **Les vecteurs avant le modèle** : un magasin vide se lit en un `stat`, et le dire après
        // avoir chargé 343 Mio, c'était faire payer le refus le plus fréquent — juste après
        // l'installation du modèle — au prix fort.
        match mailcore::vectors::summary_of(&path) {
            Ok(Some(found)) if found.records > 0 => {}
            Ok(_) => return refuse(Unavailable::Empty, &"aucun vecteur"),
            Err(error) => return refuse(Unavailable::Failed, &error),
        }
        let model = match mailvec::bert::Bert::load(&dir) {
            Ok(model) => model,
            Err(error) => return refuse(Unavailable::Failed, &error),
        };
        let identity = mailvec::pass::Encoder::identity(&model);
        let ours = mailcore::vectors::Provenance {
            model: identity.name.clone(),
            family: identity.family.clone(),
            digest: identity.digest.clone(),
            budget: 0,
            revision: 0,
            written: String::new(),
        };

        type Loaded = (
            mailcore::vectors::Vectors,
            Option<mailcore::vectors::Summary>,
        );
        let load = || -> Result<Loaded, (Unavailable, String)> {
            let summary = mailcore::vectors::summary_of(&path)
                .map_err(|error| (Unavailable::Failed, error.to_string()))?;
            let Some(found) = &summary else {
                return Err((Unavailable::Empty, "aucun vecteur".to_owned()));
            };
            let same = found.dim == model.dim()
                && found
                    .provenance
                    .as_ref()
                    .is_some_and(|theirs| theirs.same_model(&ours));
            if !same {
                return Err((
                    Unavailable::Foreign,
                    "écrits par un autre modèle".to_owned(),
                ));
            }
            let vectors = mailcore::vectors::Vectors::load(&path, model.dim())
                .map_err(|error| (Unavailable::Failed, error.to_string()))?;
            Ok((vectors, summary))
        };
        let (mut vectors, mut summary) = match load() {
            Ok(loaded) => loaded,
            Err((why, detail)) => return refuse(why, &detail),
        };
        say(&Said::Ready {
            records: summary.as_ref().map_or(0, |s| s.records),
        });

        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Ok(ask) = serde_json::from_str::<Ask>(&line) else {
                say(&Said::Error("protocol".to_owned()));
                continue;
            };
            // **Relire si le magasin a changé** — la passe de fond y ajoute pendant qu'on cherche.
            // Un `stat` et un en-tête par requête : la question la moins chère d'abord.
            let now = mailcore::vectors::summary_of(&path).ok().flatten();
            if now != summary {
                match load() {
                    Ok(loaded) => (vectors, summary) = loaded,
                    Err((why, detail)) => {
                        // Le parent tue l'enfant sur tout refus : le modèle chargé ici ne
                        // répondra plus jamais à ces vecteurs.
                        return refuse(why, &detail);
                    }
                }
            }
            let answer = model
                .encode_query(&ask.q)
                .map_err(|error| error.to_string())
                .and_then(|query| {
                    vectors
                        .search(&query, ask.depth)
                        .map_err(|error| error.to_string())
                });
            match answer {
                Ok(hits) => say(&Said::Ids(hits.into_iter().map(|hit| hit.id.0).collect())),
                Err(error) => {
                    tracing::warn!(%error, "requête non servie");
                    say(&Said::Error("failed".to_owned()));
                }
            }
        }
        0
    }
}

/// Un enfant en vie, et de quoi lui parler.
struct Running {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    /// Compté comme travail lourd tant qu'il vit — voir le module.
    _held: Option<crate::jobs::Held>,
}

impl Running {
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Taille et date des fichiers qui décident d'un refus durable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint(Vec<Option<(u64, SystemTime)>>);

fn fingerprint(root: &Utf8Path) -> Fingerprint {
    let model = mailvec::bert::default_dir(root);
    Fingerprint(
        [
            model.join("config.json"),
            model.join("model.safetensors"),
            model.join("tokenizer.json"),
            mailcore::Store::vectors_path_in(root),
        ]
        .iter()
        .map(|path| {
            std::fs::metadata(path.as_std_path())
                .and_then(|meta| Ok((meta.len(), meta.modified()?)))
                .ok()
        })
        .collect(),
    )
}

/// Où en est l'enfant.
enum Slot {
    /// Pas d'enfant.
    Idle,
    /// Un fil le lance ; personne ne l'attend.
    Starting,
    /// Chaud.
    Ready(Running),
    /// Il a refusé. Retenu tant que les fichiers n'ont pas changé — ou, pour un échec passager,
    /// pendant [`RETRY`].
    Refused {
        why: Unavailable,
        seen: Fingerprint,
        at: Instant,
    },
}

struct Inner {
    slot: Slot,
    last_used: Instant,
}

/// La recherche par le sens, du côté du démon.
pub struct Engine {
    program: Option<PathBuf>,
    root: Utf8PathBuf,
    activity: crate::jobs::Activity,
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("enabled", &self.program.is_some())
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Monte le moteur. **Ne lance rien** : l'enfant naît à la première requête.
    ///
    /// `activity` est le compteur que la passe de fond lit : l'enfant y compte tant qu'il vit.
    #[must_use]
    pub fn start(
        store: &Utf8Path,
        launch: &crate::vectors::Launch,
        activity: crate::jobs::Activity,
    ) -> Self {
        let program = match launch {
            crate::vectors::Launch::Program(path) => Some(path.clone()),
            crate::vectors::Launch::Disabled => None,
        };
        let inner = Arc::new(Mutex::new(Inner {
            slot: Slot::Idle,
            last_used: Instant::now(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        if program.is_some() {
            // Le faucheur : un enfant resté [`IDLE`] sans requête est tué, sa mémoire rendue.
            let weak = Arc::downgrade(&inner);
            let stopped = Arc::clone(&stop);
            let _ = std::thread::Builder::new()
                .name("mailcore-sens-faucheur".to_owned())
                .spawn(move || {
                    while !stopped.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_secs(5));
                        let Some(inner) = weak.upgrade() else { return };
                        let Ok(mut inner) = inner.lock() else { return };
                        if matches!(inner.slot, Slot::Ready(_))
                            && inner.last_used.elapsed() > IDLE
                            && let Slot::Ready(running) =
                                std::mem::replace(&mut inner.slot, Slot::Idle)
                        {
                            running.kill();
                            tracing::debug!("recherche par le sens endormie");
                        }
                    }
                });
        }
        Self {
            program,
            root: store.to_owned(),
            activity,
            inner,
            stop,
        }
    }

    /// Vrai si un modèle est installé — un `stat`, rien de plus.
    #[must_use]
    pub fn model_installed(&self) -> bool {
        mailvec::bert::default_dir(&self.root)
            .join("config.json")
            .is_file()
    }

    /// Vrai si l'exécutable sait lancer la recherche par le sens.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.program.is_some()
    }

    /// Vrai si le magasin de vecteurs a au moins un record — un `stat` et un en-tête.
    #[must_use]
    pub fn vectors_present(&self) -> bool {
        mailcore::vectors::summary_of(&mailcore::Store::vectors_path_in(&self.root))
            .ok()
            .flatten()
            .is_some_and(|summary| summary.records > 0)
    }

    /// Vrai si un enfant est chaud et répondra sans démarrer.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.inner
            .lock()
            .is_ok_and(|inner| matches!(inner.slot, Slot::Ready(_)))
    }

    /// Ce qu'on sait sans rien demander : l'enfant démarre, ou un refus encore valable.
    /// Pour `server.hello`, qui décrit l'état sans chercher.
    #[must_use]
    pub fn standing(&self) -> Option<Unavailable> {
        let inner = self.inner.lock().ok()?;
        match &inner.slot {
            Slot::Starting => Some(Unavailable::Starting),
            Slot::Refused { why, seen, at } if self.still_refused(*why, seen, *at) => Some(*why),
            _ => None,
        }
    }

    fn still_refused(&self, why: Unavailable, seen: &Fingerprint, at: Instant) -> bool {
        if why == Unavailable::Failed {
            at.elapsed() < RETRY
        } else {
            *seen == fingerprint(&self.root)
        }
    }

    /// Les messages les plus proches d'une requête, du plus proche au moins.
    ///
    /// **N'attend jamais un démarrage** : sans enfant chaud, il en lance un à côté et rend
    /// [`Unavailable::Starting`] aussitôt.
    ///
    /// # Errors
    ///
    /// [`Unavailable`] qui dit pourquoi la recherche par le sens ne répond pas — l'appelant
    /// répond alors par les mots seuls, et le dit.
    pub fn ranked(&self, query: &str, depth: usize) -> Result<Vec<MessageId>, Unavailable> {
        let Some(program) = &self.program else {
            return Err(Unavailable::Disabled);
        };
        if !self.model_installed() {
            return Err(Unavailable::ModelAbsent);
        }
        // Un magasin vide se lit en un `stat` : aucun processus pour l'apprendre.
        if !self.vectors_present() {
            return Err(Unavailable::Empty);
        }
        let mut inner = self.inner.lock().map_err(|_| Unavailable::Failed)?;
        inner.last_used = Instant::now();
        match &inner.slot {
            Slot::Ready(_) => {}
            Slot::Starting => return Err(Unavailable::Starting),
            Slot::Refused { why, seen, at } if self.still_refused(*why, seen, *at) => {
                return Err(*why);
            }
            Slot::Refused { .. } | Slot::Idle => {
                inner.slot = Slot::Starting;
                drop(inner);
                self.warm(program.clone());
                return Err(Unavailable::Starting);
            }
        }
        let Slot::Ready(running) = &mut inner.slot else {
            return Err(Unavailable::Failed);
        };
        let answer = ask(running, query, depth);
        if let Err(why) = answer {
            // **Tout refus tue l'enfant**, pas seulement un échec : un enfant qui a chargé un
            // modèle répond « autre modèle » à des vecteurs refaits pour le nouveau tant qu'il
            // vit — et la phrase redemandait le geste qu'on venait de faire.
            tracing::warn!(
                raison = why.code(),
                "la recherche par le sens n'a pas répondu"
            );
            let refused = Slot::Refused {
                why,
                seen: fingerprint(&self.root),
                at: Instant::now(),
            };
            if let Slot::Ready(running) = std::mem::replace(&mut inner.slot, refused) {
                running.kill();
            }
        }
        answer
    }

    /// Lance l'enfant sur un fil à lui ; le range quand il est prêt.
    fn warm(&self, program: PathBuf) {
        let inner = Arc::clone(&self.inner);
        let root = self.root.clone();
        let activity = self.activity.clone();
        let stop = Arc::clone(&self.stop);
        let spawned = std::thread::Builder::new()
            .name("mailcore-sens-reveil".to_owned())
            .spawn(move || {
                // Compté **avant** le chargement : c'est lui qui coûte la mémoire.
                let held = activity.hold();
                let seen = fingerprint(&root);
                let outcome = spawn(&program, &root, held);
                let Ok(mut inner) = inner.lock() else { return };
                if stop.load(Ordering::Relaxed) || !matches!(inner.slot, Slot::Starting) {
                    if let Ok(running) = outcome {
                        running.kill();
                    }
                    return;
                }
                inner.slot = match outcome {
                    Ok(running) => Slot::Ready(running),
                    Err(why) => {
                        tracing::warn!(raison = why.code(), "recherche par le sens non démarrée");
                        Slot::Refused {
                            why,
                            seen,
                            at: Instant::now(),
                        }
                    }
                };
            });
        if spawned.is_err()
            && let Ok(mut inner) = self.inner.lock()
        {
            inner.slot = Slot::Refused {
                why: Unavailable::Failed,
                seen: fingerprint(&self.root),
                at: Instant::now(),
            };
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut inner) = self.inner.lock()
            && let Slot::Ready(running) = std::mem::replace(&mut inner.slot, Slot::Idle)
        {
            running.kill();
        }
    }
}

/// Lance l'enfant et attend qu'il soit prêt. Appelé sur le fil de démarrage, jamais dans une
/// recherche.
fn spawn(
    program: &std::path::Path,
    root: &Utf8Path,
    held: crate::jobs::Held,
) -> Result<Running, Unavailable> {
    let mut command = Command::new(program);
    command
        .arg(CHILD_FLAG)
        .arg(root.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CREATE_NO_WINDOW` : pas de console à côté de la coquille. Priorité normale : quelqu'un
        /// attend la réponse.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|error| {
        tracing::warn!(%error, "recherche par le sens non lancée");
        Unavailable::Failed
    })?;
    let stdin = child.stdin.take().ok_or(Unavailable::Failed)?;
    let stdout = child.stdout.take().ok_or(Unavailable::Failed)?;
    if let Some(stderr) = child.stderr.take() {
        let _ = std::thread::Builder::new()
            .name("mailcore-sens-journal".to_owned())
            .spawn(move || {
                // Au niveau `info`, que le filtre par défaut laisse passer : c'est ce journal que
                // la phrase « le journal du démon dit pourquoi » désigne.
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    tracing::info!(enfant = %line, "recherche par le sens");
                }
            });
    }
    let (sender, lines) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("mailcore-sens-lecture".to_owned())
        .spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });

    let running = Running {
        child,
        stdin,
        lines,
        _held: Some(held),
    };
    match read(&running, READY_TIMEOUT) {
        Ok(Said::Ready { records }) => {
            tracing::info!(records, "recherche par le sens prête");
            Ok(running)
        }
        Ok(Said::Error(code)) => {
            let why = Unavailable::from_code(&code);
            running.kill();
            Err(why)
        }
        _ => {
            running.kill();
            Err(Unavailable::Failed)
        }
    }
}

fn read(running: &Running, timeout: Duration) -> Result<Said, Unavailable> {
    let line = running
        .lines
        .recv_timeout(timeout)
        .map_err(|_| Unavailable::Failed)?;
    serde_json::from_str(&line).map_err(|_| Unavailable::Failed)
}

fn ask(running: &mut Running, query: &str, depth: usize) -> Result<Vec<MessageId>, Unavailable> {
    let line = serde_json::to_string(&Ask {
        q: query.to_owned(),
        depth,
    })
    .map_err(|_| Unavailable::Failed)?;
    writeln!(running.stdin, "{line}").map_err(|_| Unavailable::Failed)?;
    running.stdin.flush().map_err(|_| Unavailable::Failed)?;
    match read(running, ANSWER_TIMEOUT)? {
        Said::Ids(ids) => Ok(ids.into_iter().map(MessageId).collect()),
        Said::Error(code) => Err(Unavailable::from_code(&code)),
        Said::Ready { .. } => Err(Unavailable::Failed),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Ask, Said, Unavailable};

    fn ready() -> super::Facts {
        super::Facts {
            enabled: true,
            model_installed: true,
            vectors_present: true,
            status: Some(crate::vectors::Status::Idle),
            failure: None,
            asked: true,
            operators: false,
        }
    }

    #[test]
    fn every_state_names_its_gesture_and_only_when_there_is_one() {
        use crate::vectors::{Failure, Status};
        use mailapi::dto::SemanticState as State;
        use mailvec::pass::Explicit;

        let cases = [
            (ready(), State::Ready, None),
            (
                super::Facts {
                    model_installed: false,
                    ..ready()
                },
                State::ModelAbsent,
                Some("mail model install"),
            ),
            (
                super::Facts {
                    failure: Some(Unavailable::Foreign),
                    ..ready()
                },
                State::NeedsPass,
                Some("mail vectors --bert --rebuild"),
            ),
            (
                super::Facts {
                    failure: Some(Unavailable::Starting),
                    ..ready()
                },
                State::Starting,
                None,
            ),
            (
                super::Facts {
                    status: Some(Status::NeedsExplicitPass {
                        reason: Explicit::Backlog,
                        pending: 4_000,
                    }),
                    ..ready()
                },
                State::NeedsPass,
                Some("mail vectors --bert"),
            ),
            (
                super::Facts {
                    status: Some(Status::NeedsExplicitPass {
                        reason: Explicit::Newer,
                        pending: 3,
                    }),
                    ..ready()
                },
                State::NeedsPass,
                None,
            ),
            (
                super::Facts {
                    status: Some(Status::Working { pending: 12 }),
                    ..ready()
                },
                State::Working,
                None,
            ),
            (
                super::Facts {
                    vectors_present: false,
                    status: Some(Status::Waiting),
                    ..ready()
                },
                State::NeedsPass,
                Some("mail vectors --bert"),
            ),
            (
                super::Facts {
                    vectors_present: false,
                    status: Some(Status::Working { pending: 12 }),
                    ..ready()
                },
                State::Working,
                None,
            ),
            (
                super::Facts {
                    failure: Some(Unavailable::Failed),
                    ..ready()
                },
                State::Unavailable,
                None,
            ),
            (
                super::Facts {
                    asked: false,
                    status: Some(Status::Failed(Failure::Model)),
                    ..ready()
                },
                State::Unavailable,
                None,
            ),
            (
                super::Facts {
                    enabled: false,
                    ..ready()
                },
                State::Disabled,
                None,
            ),
        ];
        for (facts, state, gesture) in cases {
            let said = super::describe(&facts);
            assert_eq!(said.state, state, "{facts:?}");
            assert_eq!(said.gesture.as_deref(), gesture, "{facts:?}");
            assert!(!said.sentence.is_empty());
        }
    }

    #[test]
    fn a_description_without_a_search_claims_no_result() {
        // `server.hello` n'a rien cherché : « les résultats mêlent… » y serait une affirmation sur
        // des résultats qui n'existent pas. Relevé par une relecture le 2026-10-08.
        let hello = super::describe(&super::Facts {
            asked: false,
            ..ready()
        });
        assert!(!hello.sentence.contains("résultats"), "{}", hello.sentence);
        // Le contrôle inverse : après une recherche qui a répondu, la phrase le dit.
        assert!(super::describe(&ready()).sentence.contains("le sens"));
    }

    #[test]
    fn an_operator_query_is_not_told_about_vectors_it_never_used() {
        use crate::vectors::Status;
        use mailvec::pass::Explicit;
        // La requête à opérateurs n'a pas demandé le sens : un arriéré ou des vecteurs étrangers
        // ne la concernent pas, et « le sens ne voit qu'une partie du courrier » y serait faux.
        for facts in [
            super::Facts {
                operators: true,
                asked: false,
                status: Some(Status::NeedsExplicitPass {
                    reason: Explicit::Backlog,
                    pending: 4_000,
                }),
                ..ready()
            },
            super::Facts {
                operators: true,
                asked: false,
                failure: Some(Unavailable::Foreign),
                ..ready()
            },
        ] {
            let said = super::describe(&facts);
            assert!(said.sentence.contains("opérateurs"), "{}", said.sentence);
            assert_eq!(said.gesture, None);
        }
        // Mais un modèle absent reste dit : c'est un état de l'installation, pas de la requête.
        let absent = super::describe(&super::Facts {
            operators: true,
            asked: false,
            model_installed: false,
            ..ready()
        });
        assert_eq!(absent.gesture.as_deref(), Some("mail model install"));
    }

    #[test]
    fn a_backlog_says_how_many_and_agrees_in_number() {
        use crate::vectors::Status;
        use mailvec::pass::Explicit;
        let backlog = |pending| {
            super::describe(&super::Facts {
                status: Some(Status::NeedsExplicitPass {
                    reason: Explicit::Backlog,
                    pending,
                }),
                ..ready()
            })
            .sentence
        };
        assert!(
            backlog(4_000).starts_with("4000 messages attendent"),
            "{}",
            backlog(4_000)
        );
        assert!(
            backlog(1).starts_with("1 message attend "),
            "{}",
            backlog(1)
        );
        // Des vecteurs existent : le sens répond, sur une partie. Sans, les mots seuls — la phrase
        // ne doit pas promettre une réponse qui n'a pas eu lieu.
        assert!(backlog(4_000).contains("une partie"));
        let refused = super::describe(&super::Facts {
            failure: Some(Unavailable::Empty),
            vectors_present: false,
            status: Some(Status::NeedsExplicitPass {
                reason: Explicit::Backlog,
                pending: 4_000,
            }),
            ..ready()
        });
        assert!(
            refused.sentence.contains("mots seuls"),
            "{}",
            refused.sentence
        );
        assert!(!refused.sentence.contains("une partie"));
    }

    #[test]
    fn a_query_carrying_anything_survives_its_line() {
        // Une requête est du texte libre : saut de ligne, guillemets, contrôle — elle doit tenir
        // sur une ligne et revenir identique.
        let q = "ligne\nsuivante \"guillemets\" \u{1b}[2J fin".to_owned();
        let line = serde_json::to_string(&Ask {
            q: q.clone(),
            depth: 50,
        })
        .unwrap();
        assert!(!line.contains('\n'));
        let back: Ask = serde_json::from_str(&line).unwrap();
        assert_eq!(back.q, q);
    }

    #[test]
    fn every_answer_survives_its_line_and_an_unknown_code_is_a_failure() {
        for said in [
            Said::Ready { records: 3 },
            Said::Ids(vec![3, 1, 2]),
            Said::Error("model-absent".to_owned()),
        ] {
            let line = serde_json::to_string(&said).unwrap();
            let back: Said = serde_json::from_str(&line).unwrap();
            assert_eq!(format!("{back:?}"), format!("{said:?}"));
        }
        for why in [
            Unavailable::Disabled,
            Unavailable::ModelAbsent,
            Unavailable::Foreign,
            Unavailable::Empty,
            Unavailable::Starting,
            Unavailable::Failed,
        ] {
            assert_eq!(Unavailable::from_code(why.code()), why);
        }
        assert_eq!(
            Unavailable::from_code("n'importe quoi"),
            Unavailable::Failed
        );
    }
}

//! Le texte des PDF joints : l'enfant qui les lit, et le passage qui le lance — étape 6 quater de
//! `docs/PHASE-4.md`, entrée du 2026-10-20.
//!
//! ## Un enfant, parce qu'un PDF est écrit par un inconnu
//!
//! Lire un PDF, c'est exécuter un analyseur sur des octets choisis par l'expéditeur : des flux
//! compressés, des polices embarquées, des tables de caractères. Rien de tout ça ne tourne dans le
//! démon ni dans la coquille. L'enfant — **le même exécutable**, relancé avec [`CHILD_FLAG`] —
//! n'ouvre pas SQLite : il lit des blobs, rend du texte sur sa sortie standard, et c'est le parent
//! qui range. Une panique, une boucle sans fin, une bombe de décompression ne coûtent qu'un enfant.
//!
//! Il tourne en priorité minimale, sous une **borne de mémoire posée par le système** —
//! [`mailbound`], avant que la requête ne lui soit écrite — et sous un budget de **temps
//! processeur** que le parent relève : un enfant que la machine fait attendre n'est pas accusé
//! d'avoir trop travaillé.
//!
//! ## Un seul endroit décide de ce qui est mortel
//!
//! Tout `kill` passe par [`Supervision::kill`], qui pose sa raison **avant** de tuer. Après la
//! sortie, le parent classe sur cette raison et sur rien d'autre :
//!
//! - **arrêt, job, recherche** — rien n'est accusé : le parent a tué un enfant sain ;
//! - **silence** de dix minutes — rien n'est accusé, le passage recule : une machine pleine fait
//!   attendre un enfant en priorité minimale, ce n'est pas la faute du PDF ;
//! - **temps processeur** épuisé — la pièce en cours passe [`PdfState::TimedOut`] ;
//! - **mémoire** — l'enfant a avorté sur une allocation refusée et l'a dit sur sa sortie
//!   d'erreur : la pièce en cours passe [`PdfState::OutOfMemory`] ;
//! - **mort sans raison** — le message devient *suspect* : le passage suivant le lit seul, dans un
//!   enfant neuf, et seule une **seconde** mort le condamne. Une mort unique peut venir de la
//!   machine ; deux de suite sur le même message, non.
//!
//! Une mort avant que l'enfant ne se dise prêt ou n'ait commencé un message n'accuse rien : c'est
//! un lancement raté.
//!
//! ## Ce que le parent range, et quand
//!
//! Un message rendu en entier est rangé et marqué lu ; un message coupé par une pièce mortelle
//! garde ce qui la précède, et la pièce mortelle elle-même : il sera relu, et ses pièces déjà
//! rangées seront « connues ». Les écritures sont groupées, **une validation toutes les deux
//! secondes au plus** — chacune réveille les clients qui attendent le store. La règle de remise —
//! qui remet un message à indexer et à vectoriser — vit dans `mailcore::store::pdfs`, au seul
//! endroit où elle se tient.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use camino::Utf8Path;
use mailcore::attachtext::{BUDGET, PdfState};
use mailcore::store::pdfs::{PdfRecord, Recorded, Scan, ScanLock, Unscannable};
use mailcore::{BlobHash, MessageId, Store};
use serde::{Deserialize, Serialize};

use crate::jobs::Activity;

/// L'argument qui fait d'un exécutable l'extracteur, suivi de la racine du store.
pub const CHILD_FLAG: &str = "--mailcore-attachments";

/// Combien de messages un enfant lit au plus avant de sortir — et de rendre sa mémoire. Le
/// passage des vecteurs vient entre deux : « les PDF d'abord » vaut un passage, pas tout un
/// arriéré.
pub const ROUND: usize = 200;

/// Au-delà, un message n'est pas analysé du tout. Le recensement du 2026-10-20 n'en trouve aucun
/// qui en approche ; analysé, le message tient en mémoire avec ses pièces décodées, ce que la borne
/// de [`MEMORY_BOUND`] doit couvrir.
pub const MESSAGE_LIMIT: u64 = 160 * 1024 * 1024;

/// La mémoire que le système accorde à l'enfant. Une allocation au-delà échoue, et l'enfant avorte.
pub const MEMORY_BOUND: usize = 1024 * 1024 * 1024;

/// Le temps processeur qu'une pièce — ou l'analyse d'un message — peut prendre, compté depuis la
/// dernière ligne de l'enfant. L'extraction se donne elle-même vingt secondes de temps mural
/// (`mailpdf::TIME_BUDGET`) et rend un texte partiel ; ce budget-ci est celui d'un enfant qui ne
/// rend plus la main.
const CPU_BUDGET: Duration = Duration::from_secs(30);

/// Le filet : un enfant qui n'a rien dit depuis tout ce temps est tué sans être accusé.
const WALL_SILENCE: Duration = Duration::from_secs(10 * 60);

/// Les bornes d'un enfant : [`CPU_BUDGET`], [`WALL_SILENCE`], [`MEMORY_BOUND`] par défaut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Le temps processeur depuis la dernière ligne.
    pub cpu: Duration,
    /// Le temps mural sans ligne.
    pub silence: Duration,
    /// La mémoire que le système accorde.
    pub memory: usize,
    /// Le premier recul après un échec ; il double à chaque échec suivant, jusqu'à un quart
    /// d'heure.
    pub backoff: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cpu: CPU_BUDGET,
            silence: WALL_SILENCE,
            memory: MEMORY_BOUND,
            backoff: BACKOFF_MIN,
        }
    }
}

/// Au plus une validation tous les combien.
const COMMIT_EVERY: Duration = Duration::from_secs(2);

/// À quelle fréquence regarder l'enfant : s'il faut le tuer, et s'il a fini.
const WATCH: Duration = Duration::from_millis(200);

/// Combien de pièces d'un message l'enfant lit au plus. Le recensement en trouve neuf au plus.
const PIECES_PER_MESSAGE: usize = 16;

/// La plus longue ligne que l'enfant peut écrire : un texte au plafond, chaque caractère échappé.
const LINE_CAP: usize = 6 * BUDGET + 64 * 1024;

/// La plus longue requête que l'enfant accepte : deux cents messages et les empreintes connues.
const REQUEST_CAP: usize = 16 * 1024 * 1024;

/// Une ligne du journal de l'enfant au plus, recopiée dans celui du parent.
const RELAY_LINE: usize = 2 * 1024;

/// Combien de lignes du journal de l'enfant sont recopiées par minute au plus. Un PDF qui fait
/// avertir la bibliothèque à chaque objet remplirait sinon le fichier de la coquille.
const RELAY_PER_MINUTE: u32 = 200;

/// La pile du fil d'extraction : la même sur tous les systèmes, au lieu de celle du fil principal,
/// qui en change.
const STACK: usize = 16 * 1024 * 1024;

/// Combien de suspects innocentés d'affilée font conclure que l'enfant meurt pour une autre raison
/// que ses messages.
const CLEARED_AT_MOST: u32 = 3;

/// Les reculs après un échec : de trente secondes à un quart d'heure, comme la passe des vecteurs.
const BACKOFF_MIN: Duration = Duration::from_secs(30);
const BACKOFF_MAX: Duration = Duration::from_secs(15 * 60);

/// La variable qui donne à l'enfant un crochet de test — un répertoire où il peut écrire ses
/// témoins. **Posée ou retirée par le parent, toujours** : une variable laissée dans
/// l'environnement de l'utilisateur ne doit pas atteindre l'enfant.
pub const HOOK_ENV: &str = "MAILCORE_ATTACHMENTS_TEST_HOOK";

/// Pourquoi un passage a échoué. Des codes, jamais une phrase : rien de ce qui sort d'ici ne porte
/// de chemin — `docs/PRIVACY.md`, §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Le store ne se lit pas, ou n'accepte pas l'écriture.
    Store,
    /// L'enfant n'a pas pu être lancé, ou il est mort avant de commencer.
    Spawn,
    /// Le système a refusé la borne de mémoire.
    Bound,
    /// L'enfant a écrit autre chose que ce que le protocole prévoit.
    Protocol,
    /// L'enfant n'a rien dit pendant [`WALL_SILENCE`].
    Silent,
    /// L'enfant meurt sans que ses messages en soient la cause.
    Unstable,
    /// Une variable d'extraction est posée dans l'environnement de l'enfant : il refuse de lire.
    Environment,
}

impl Failure {
    /// Un mot qui le désigne, pour un journal ou l'API — jamais un chemin.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Spawn => "spawn",
            Self::Bound => "bound",
            Self::Protocol => "protocol",
            Self::Silent => "silent",
            Self::Unstable => "unstable",
            Self::Environment => "environment",
        }
    }
}

/// Où en est la lecture des PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Pas d'extracteur : l'exécutable ne sait pas se relancer.
    Disabled,
    /// Le délai de démarrage n'est pas écoulé.
    Waiting,
    /// Rien à lire.
    Idle,
    /// Un job ou la recherche par le sens tient la machine : la lecture attend.
    Paused,
    /// L'enfant lit ; `pending` messages restaient à lire au lancement.
    Working {
        /// Combien.
        pending: u64,
    },
    /// Un autre processus lit les PDF de ce store.
    Busy,
    /// Ce système ne sait pas borner la mémoire d'un enfant : aucun PDF n'est lu.
    Unsupported,
    /// Le dernier passage a échoué.
    Failed(Failure),
}

/// La phrase sur la lecture des PDF, **composée à un seul endroit**, après la décision, sans
/// chemin : `server.hello` et `semantic.status` la portent tous deux. Aucune ne promet ce que le
/// démon ne fera pas : un échec dit qu'il réessaie parce que le recul est posé, et le refus d'un
/// système sans borne de mémoire ne dit pas « plus tard », parce qu'il n'y a pas de plus tard.
#[must_use]
pub fn describe(status: Status) -> mailapi::dto::Attachments {
    use mailapi::dto::AttachmentsState as State;
    let said = |state, pending, sentence: String| mailapi::dto::Attachments {
        state,
        pending,
        sentence,
    };
    match status {
        Status::Disabled => said(
            State::Disabled,
            0,
            "Ce service ne lit pas le texte des PDF joints.".to_owned(),
        ),
        Status::Waiting => said(
            State::Waiting,
            0,
            "La lecture des PDF joints commence peu après le démarrage.".to_owned(),
        ),
        Status::Idle => said(
            State::Idle,
            0,
            "Le texte des PDF joints est lu : il se cherche comme celui des messages.".to_owned(),
        ),
        Status::Working { pending } => said(
            State::Working,
            pending,
            format!(
                "Lecture des PDF joints : {} à lire. Leur texte entre dans la recherche au fil \
                 de la lecture.",
                crate::semantic::messages(pending)
            ),
        ),
        Status::Paused => said(
            State::Paused,
            0,
            "Lecture des PDF joints en pause : une tâche ou une recherche occupe la machine."
                .to_owned(),
        ),
        Status::Busy => said(
            State::Busy,
            0,
            "Un autre processus lit les PDF joints de ce courrier.".to_owned(),
        ),
        Status::Unsupported => said(
            State::Unavailable,
            0,
            "Ce système ne sait pas borner la mémoire du lecteur de PDF : leur texte n'est pas \
             lu."
            .to_owned(),
        ),
        Status::Failed(_) => said(
            State::Unavailable,
            0,
            "La lecture des PDF joints a échoué ; elle réessaie d'elle-même, et le journal du \
             démon dit pourquoi."
                .to_owned(),
        ),
    }
}

/// Pourquoi le parent tue l'enfant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kill {
    /// Le démon s'arrête.
    Stop,
    /// Un job a démarré.
    Job,
    /// L'enfant de la recherche par le sens est chaud.
    Search,
    /// Le budget de temps processeur est épuisé.
    Cpu,
    /// L'enfant n'a rien dit depuis [`WALL_SILENCE`].
    Wall,
    /// L'enfant a écrit hors du protocole.
    Protocol,
}

// ---------------------------------------------------------------------------------------------
// Le protocole.

/// Ce que le parent demande : une ligne, une seule.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    items: Vec<Item>,
    /// Les empreintes que le store connaît déjà à la révision courante : l'enfant ne les relit pas.
    known: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    message: i64,
    blob: String,
}

/// Ce que l'enfant rend, une ligne JSON par fait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "said", rename_all = "kebab-case")]
enum Said {
    Ready {
        revision: u32,
    },
    Begin {
        message: i64,
    },
    /// Une pièce va être lue : c'est elle qu'une mort accusera. Sa taille vient avec l'annonce, pour
    /// qu'une pièce mortelle soit rangée avec la sienne.
    Pdf {
        message: i64,
        rank: u32,
        hash: String,
        size: u64,
    },
    Known {
        message: i64,
        rank: u32,
        hash: String,
    },
    Text {
        hash: String,
        state: i64,
        text: String,
    },
    Scanned {
        message: i64,
        pdfs: u32,
    },
    Done,
    Failed {
        code: String,
    },
}

// ---------------------------------------------------------------------------------------------
// L'enfant.

pub(crate) mod child {
    use super::{Item, PIECES_PER_MESSAGE, REQUEST_CAP, Request, STACK, Said};
    use camino::Utf8Path;
    use mailcore::attachtext::{BUDGET, PdfState};
    use mailcore::{BlobHash, BlobStore};
    use std::collections::HashSet;
    use std::io::{BufRead, Read, Write};

    fn say(said: &Said) {
        if let Ok(line) = serde_json::to_string(said) {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }

    /// Lit tout, et rend le code de sortie du processus.
    pub(crate) fn run(root: &Utf8Path) -> i32 {
        crate::semantic::child::journal();

        // **La requête d'abord, en entier, sur ce fil.** Le garde de la mort du parent lit
        // l'entrée standard jusqu'à sa fin : lancé avant, il pourrait avaler la requête.
        let mut line = Vec::new();
        let read = std::io::stdin()
            .lock()
            .take(REQUEST_CAP as u64)
            .read_until(b'\n', &mut line);
        let Some(request) = read
            .ok()
            .and_then(|_| serde_json::from_slice::<Request>(&line).ok())
        else {
            say(&Said::Failed {
                code: "request".to_owned(),
            });
            return 2;
        };
        drop(line);

        // **Le parent mort, l'enfant sort.** Il tient l'entrée standard ouverte sans plus rien y
        // écrire : la fin de fichier ne vient que de sa disparition.
        let _ = std::thread::Builder::new()
            .name("mailcore-pdf-parent".to_owned())
            .spawn(|| {
                let mut sink = Vec::new();
                let _ = std::io::stdin().read_to_end(&mut sink);
                std::process::exit(3);
            });

        // La barrière, avant toute lecture : le parent a retiré ces variables, mais un enfant lancé
        // à la main ne passe pas par lui.
        if mailpdf::ENVIRONMENT
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            tracing::warn!("variable d'extraction posée : aucun PDF n'est lu");
            say(&Said::Failed {
                code: "environment".to_owned(),
            });
            return 0;
        }
        let blobs = match mailcore::store::blobs::FsBlobStore::open(root) {
            Ok(blobs) => blobs,
            Err(error) => {
                tracing::warn!(%error, "magasin de blobs illisible");
                say(&Said::Failed {
                    code: "store".to_owned(),
                });
                return 0;
            }
        };
        let hook = hook::Hook::from_env();
        let mut known: HashSet<BlobHash> = request
            .known
            .iter()
            .filter_map(|hex| BlobHash::from_hex(hex).ok())
            .collect();
        say(&Said::Ready {
            revision: mailpdf::REVISION,
        });

        for item in &request.items {
            scan(&blobs, item, &mut known, hook.as_ref());
        }
        say(&Said::Done);
        0
    }

    /// Un message : ses pièces, une à une.
    fn scan(
        blobs: &mailcore::store::blobs::FsBlobStore,
        item: &Item,
        known: &mut HashSet<BlobHash>,
        hook: Option<&hook::Hook>,
    ) {
        say(&Said::Begin {
            message: item.message,
        });
        let pieces = BlobHash::from_hex(&item.blob)
            .ok()
            .and_then(|blob| blobs.read(blob).ok())
            .map(|rfc822| pieces_of(&rfc822))
            .unwrap_or_default();

        let mut announced = 0_u32;
        let mut spent = 0_usize;
        for (rank, bytes) in pieces {
            // Au-delà du plafond, plus rien de ce message n'entrerait dans l'index : les pièces qui
            // restent ne sont pas lues.
            if spent >= BUDGET {
                break;
            }
            announced += 1;
            let hash = BlobHash::of(&bytes);
            if known.contains(&hash) {
                say(&Said::Known {
                    message: item.message,
                    rank,
                    hash: hash.to_hex(),
                });
                continue;
            }
            say(&Said::Pdf {
                message: item.message,
                rank,
                hash: hash.to_hex(),
                size: bytes.len() as u64,
            });
            let extract = extract_apart(bytes, hook.cloned());
            spent += extract.text.len();
            say(&Said::Text {
                hash: hash.to_hex(),
                state: extract.state.code(),
                text: extract.text,
            });
            known.insert(hash);
        }
        say(&Said::Scanned {
            message: item.message,
            pdfs: announced,
        });
    }

    /// Les pièces PDF retenues d'un message, **copiées hors de lui** : le message et son analyse
    /// sont lâchés avant la première extraction, et la mémoire que celle-ci trouve est celle de la
    /// pièce, pas du message entier.
    fn pieces_of(rfc822: &[u8]) -> Vec<(u32, Vec<u8>)> {
        let Some(parsed) = mail_parser::MessageParser::default().parse(rfc822) else {
            return Vec::new();
        };
        mailcore::attachtext::pieces(&parsed)
            .into_iter()
            .filter(|piece| piece.verdict.accepted())
            .take(PIECES_PER_MESSAGE)
            .map(|piece| (piece.rank, piece.part.contents().to_vec()))
            .collect()
    }

    /// L'extraction, sur un fil à pile fixe. Une panique — un défaut de la bibliothèque devant un
    /// PDF qu'elle ne prévoyait pas — ne tue pas l'enfant : la pièce est illisible, et la suivante
    /// est lue.
    ///
    /// Le crochet de test agit **sur ce fil**, là où la bibliothèque tournerait : une panique
    /// fabriquée y suit le chemin d'une vraie.
    fn extract_apart(bytes: Vec<u8>, hook: Option<hook::Hook>) -> mailpdf::Extract {
        let worker = std::thread::Builder::new()
            .name("mailcore-pdf-extraction".to_owned())
            .stack_size(STACK)
            .spawn(move || {
                if let Some(hook) = hook {
                    hook.act(&bytes);
                }
                mailpdf::extract(&bytes)
            });
        match worker.map(std::thread::JoinHandle::join) {
            Ok(Ok(Ok(extract))) => extract,
            // `Refused` est écarté par la barrière de `run` ; un fil qui ne démarre pas, ou qui
            // panique, rend la pièce illisible.
            _ => mailpdf::Extract {
                state: PdfState::Invalid,
                text: String::new(),
            },
        }
    }

    /// Le crochet de test : des marques écrites dans un PDF fabriqué font mourir l'enfant de la
    /// façon qu'un test veut voir. **Sans la variable, les marques ne font rien** — c'est son
    /// contrôle inverse.
    pub(super) mod hook {
        use std::path::PathBuf;

        #[derive(Debug, Clone)]
        pub(crate) struct Hook {
            dir: PathBuf,
        }

        impl Hook {
            pub(crate) fn from_env() -> Option<Self> {
                std::env::var_os(crate::attachments::HOOK_ENV).map(|dir| Self { dir: dir.into() })
            }

            pub(crate) fn act(&self, pdf: &[u8]) {
                let marked = |mark: &str| {
                    let mark = format!("%mailcore-test:{mark}\n");
                    pdf.windows(mark.len())
                        .any(|window| window == mark.as_bytes())
                };
                if marked("crash") {
                    std::process::abort();
                }
                if marked("crash-once") {
                    let witness = self.dir.join("crashed-once");
                    if !witness.exists() {
                        let _ = std::fs::write(&witness, b"");
                        std::process::abort();
                    }
                }
                if marked("spin") {
                    let mut x = 0_u64;
                    loop {
                        x = std::hint::black_box(x.wrapping_add(1));
                    }
                }
                if marked("sleep") {
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(60));
                    }
                }
                if marked("allocate") {
                    let mut blocks = Vec::new();
                    loop {
                        let mut block = vec![0_u8; 64 * 1024 * 1024];
                        for page in block.chunks_mut(4096) {
                            page[0] = 1;
                        }
                        blocks.push(block);
                    }
                }
                if marked("panic") {
                    panic!("panique fabriquée");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Le parent.

/// Combien d'empreintes connues partent au plus dans une requête : environ 6,7 Mo de JSON, sous
/// [`REQUEST_CAP`]. Au-delà, les plus anciennes sont relues — voir `Store::known_pdfs`.
const KNOWN_AT_MOST: usize = 100_000;

/// Un message dont l'analyse a tué un enfant une fois, sans raison connue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Suspect {
    id: MessageId,
    blob: BlobHash,
}

/// Ce qu'un passage a donné.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Passed {
    /// Rien à lire, ou rien de possible maintenant — un job, un recul, un autre extracteur.
    Nothing,
    /// Des messages ont avancé : il faut repasser tout de suite.
    Advanced {
        /// Combien ont été marqués — lus, trop gros, ou condamnés.
        marked: u64,
        /// Combien ont été remis à indexer et à vectoriser.
        remarked: u64,
    },
    /// L'enfant a été tué par un job, une recherche ou l'arrêt : ce qui était rangé reste rangé.
    Stopped,
    /// Le passage a échoué ; le recul est posé.
    Failed(Failure),
}

/// Ce que le passage regarde pour savoir s'il doit tuer l'enfant, et où il le range pour que
/// l'arrêt du démon le tue sans attendre.
#[derive(Debug)]
pub struct Watch<'a> {
    /// L'arrêt du démon.
    pub stop: &'a AtomicBool,
    /// Les jobs et l'enfant de la recherche.
    pub activity: &'a Activity,
    /// L'enfant en cours.
    pub child: &'a Mutex<Option<Child>>,
}

impl Watch<'_> {
    /// La raison de tuer l'enfant maintenant, s'il y en a une.
    fn interrupt(&self) -> Option<Kill> {
        if self.stop.load(Ordering::SeqCst) {
            Some(Kill::Stop)
        } else if self.activity.jobs() > 0 {
            Some(Kill::Job)
        } else if self.activity.held() > 0 {
            Some(Kill::Search)
        } else {
            None
        }
    }
}

/// Ce que le journal de l'enfant a encore le droit d'écrire dans la minute — **partagé entre
/// enfants** : ils se suivent à quelques secondes d'intervalle pendant un rattrapage, et un plafond
/// par enfant n'en serait pas un.
#[derive(Debug)]
struct Relayed {
    window: Instant,
    said: u32,
    dropped: u32,
}

/// La lecture des PDF : ce qu'elle retient d'un passage à l'autre.
#[derive(Debug)]
pub struct Extractor {
    program: PathBuf,
    hook: Option<PathBuf>,
    limits: Limits,
    suspect: Option<Suspect>,
    cleared: u32,
    /// Les messages dont la lecture a laissé l'enfant silencieux : mis de côté pour la vie du
    /// processus, sans être accusés. Sans ça, le plus récent d'entre eux passait en tête de chaque
    /// passage, et aucun message plus ancien n'était jamais lu.
    silent: HashSet<MessageId>,
    /// Les messages remis depuis le dernier [`Self::take_remarked`], **quelle que soit l'issue du
    /// passage** : un passage interrompu a déjà validé des remises, et l'index doit les suivre.
    owed: u64,
    backoff: Duration,
    retry_at: Option<Instant>,
    unsupported: bool,
    status: Status,
    /// Où l'état se publie **au moment où il change** — avant le lancement d'un enfant, pas après sa
    /// sortie.
    published: Option<Arc<Mutex<Status>>>,
    relayed: Arc<Mutex<Relayed>>,
}

impl Extractor {
    /// Un extracteur qui relance `program`.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            hook: None,
            limits: Limits::default(),
            suspect: None,
            cleared: 0,
            silent: HashSet::new(),
            owed: 0,
            backoff: BACKOFF_MIN,
            retry_at: None,
            unsupported: false,
            status: Status::Waiting,
            published: None,
            relayed: Arc::new(Mutex::new(Relayed {
                window: Instant::now(),
                said: 0,
                dropped: 0,
            })),
        }
    }

    /// Le même, avec un crochet de test : `dir` reçoit les témoins de l'enfant. **Pour les tests**,
    /// jamais pour l'utilisateur.
    #[must_use]
    pub fn with_hook(mut self, dir: impl Into<PathBuf>) -> Self {
        self.hook = Some(dir.into());
        self
    }

    /// Le même, avec d'autres bornes — pour qu'un test voie un délai épuisé sans attendre trente
    /// secondes. **Pour les tests**, jamais pour l'utilisateur.
    #[must_use]
    pub const fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self.backoff = limits.backoff;
        self
    }

    /// Le même, qui publie son état dans `slot` à chaque changement.
    #[must_use]
    pub fn publishing_to(mut self, slot: Arc<Mutex<Status>>) -> Self {
        self.published = Some(slot);
        self
    }

    /// Où en est la lecture.
    #[must_use]
    pub const fn status(&self) -> Status {
        self.status
    }

    /// Combien de messages ont été remis depuis le dernier appel — et remet le compte à zéro.
    pub fn take_remarked(&mut self) -> u64 {
        std::mem::take(&mut self.owed)
    }

    fn set(&mut self, status: Status) {
        self.status = status;
        if let Some(slot) = &self.published
            && let Ok(mut published) = slot.lock()
        {
            *published = status;
        }
    }

    /// Un passage : au plus [`ROUND`] messages, dans un enfant.
    pub fn pass(&mut self, store: &Store, watch: &Watch<'_>) -> Passed {
        // Du moins cher au plus cher : un refus durable, un recul, un job, puis le store.
        if self.unsupported {
            return Passed::Nothing;
        }
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return Passed::Nothing;
        }
        if watch.interrupt().is_some() {
            // « En pause » seulement s'il y a quelque chose qui attend : un compte sur l'index
            // partiel, quelques microsecondes. Sinon la page Activité disait du travail en attente
            // pendant chaque job et cinq minutes après chaque recherche par le sens.
            let waiting = !matches!(store.unscanned_total(), Ok(0));
            self.set(if waiting {
                Status::Paused
            } else {
                Status::Idle
            });
            return Passed::Nothing;
        }
        let lock = match ScanLock::try_acquire(store) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                self.set(Status::Busy);
                return Passed::Nothing;
            }
            Err(error) => {
                tracing::warn!(%error, "verrou de lecture des PDF");
                return self.fail(Failure::Store);
            }
        };
        let passed = self.pass_locked(store, watch);
        drop(lock);
        passed
    }

    fn pass_locked(&mut self, store: &Store, watch: &Watch<'_>) -> Passed {
        let mut too_big = 0_u64;
        let items: Vec<(MessageId, BlobHash)> = if let Some(suspect) = self.suspect {
            vec![(suspect.id, suspect.blob)]
        } else {
            let rows = match store.unscanned(ROUND + self.silent.len()) {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(%error, "messages à lire illisibles");
                    return self.fail(Failure::Store);
                }
            };
            let mut items = Vec::with_capacity(rows.len());
            for row in rows {
                if self.silent.contains(&row.id) || items.len() >= ROUND {
                    continue;
                }
                if row.size > MESSAGE_LIMIT {
                    if store
                        .mark_unscannable(row.id, row.blob, Unscannable::TooBig)
                        .unwrap_or(false)
                    {
                        too_big += 1;
                    }
                } else {
                    items.push((row.id, row.blob));
                }
            }
            items
        };
        if items.is_empty() {
            if too_big > 0 {
                return Passed::Advanced {
                    marked: too_big,
                    remarked: 0,
                };
            }
            self.set(Status::Idle);
            return Passed::Nothing;
        }
        let pending = store
            .unscanned_total()
            .unwrap_or(items.len() as u64)
            .saturating_sub(self.silent.len() as u64);
        let known = match store.known_pdfs(mailpdf::REVISION, KNOWN_AT_MOST) {
            Ok(known) => known,
            Err(error) => {
                tracing::warn!(%error, "pièces connues illisibles");
                return self.fail(Failure::Store);
            }
        };
        let request = Request {
            items: items
                .iter()
                .map(|(id, blob)| Item {
                    message: id.0,
                    blob: blob.to_hex(),
                })
                .collect(),
            known: known
                .into_iter()
                .map(|hash| BlobHash::from_bytes(hash).to_hex())
                .collect(),
        };
        self.set(Status::Working { pending });

        let ended = match Supervision::run(
            &Spawning {
                program: &self.program,
                hook: self.hook.as_deref(),
                limits: self.limits,
                relayed: &self.relayed,
            },
            store,
            &request,
            watch,
        ) {
            Ok(ended) => ended,
            Err(Launched::Unsupported) => {
                tracing::warn!(
                    "ce système ne borne pas la mémoire d'un enfant : aucun PDF n'est lu"
                );
                self.unsupported = true;
                self.set(Status::Unsupported);
                return Passed::Nothing;
            }
            Err(Launched::Failed(failure)) => return self.fail(failure),
        };
        self.settle(store, &items, ended, too_big)
    }

    /// Range la fin d'un enfant. **C'est ici, et seulement ici, qu'une mort est attribuée.**
    fn settle(
        &mut self,
        store: &Store,
        items: &[(MessageId, BlobHash)],
        ended: Ended,
        too_big: u64,
    ) -> Passed {
        let mut marked = too_big + ended.marked;
        let remarked = ended.remarked;
        // Compté d'abord : ces remises sont validées, quoi que dise la suite.
        self.owed += remarked;
        if ended.failed_store {
            return self.fail(Failure::Store);
        }
        if let Some(code) = ended.refused.as_deref() {
            tracing::warn!(raison = code, "l'extracteur a refusé de lire");
            return self.fail(match code {
                "store" => Failure::Store,
                "environment" => Failure::Environment,
                _ => Failure::Protocol,
            });
        }
        // Un suspect relu en entier est innocent. Trois innocents d'affilée : l'enfant meurt pour
        // une raison qui n'est pas dans les messages.
        if let Some(suspect) = self.suspect
            && ended.finished.contains(&suspect.id)
        {
            self.suspect = None;
            self.cleared += 1;
            if self.cleared >= CLEARED_AT_MOST {
                self.cleared = 0;
                tracing::warn!("l'extracteur meurt sans que ses messages en soient la cause");
                return self.fail(Failure::Unstable);
            }
        }
        if ended.done {
            self.backoff = self.limits.backoff;
            self.retry_at = None;
            if marked == 0 {
                self.set(Status::Idle);
                return Passed::Nothing;
            }
            tracing::info!(lus = marked, remis = remarked, "PDF joints lus");
            return Passed::Advanced { marked, remarked };
        }

        let death = match ended.kill {
            Some(Kill::Stop | Kill::Job | Kill::Search | Kill::Wall) => None,
            Some(Kill::Cpu) => Some(Death::Cpu),
            Some(Kill::Protocol) => Some(Death::Unexplained),
            None if ended.out_of_memory => Some(Death::Memory),
            None => Some(Death::Unexplained),
        };
        // Le message en cours **au moment où la mort a été décidée** : après un `kill`, plus
        // aucune ligne n'est donnée à l'automate — voir `Supervision::run`.
        let current = ended.current.and_then(|current| {
            items
                .iter()
                .find(|(id, _)| *id == current.id)
                .map(|&(_, blob)| (blob, current))
        });
        // Mort avant d'avoir commencé un message : un lancement raté, qui n'accuse personne.
        let Some((blob, current)) = current else {
            return match ended.kill {
                Some(Kill::Stop | Kill::Job | Kill::Search) => self.paused(),
                Some(Kill::Wall) => self.fail(Failure::Silent),
                _ => self.fail(Failure::Spawn),
            };
        };
        let id = current.id;
        let mut pdfs = current.records;

        let Some(death) = death else {
            // Le parent a tué un enfant sain : ce qu'il avait lu du message en cours est gardé, et
            // le message sera relu.
            if !pdfs.is_empty() && !self.keep(store, id, blob, pdfs, false) {
                return self.fail(Failure::Store);
            }
            if ended.kill == Some(Kill::Wall) {
                // Silencieux : rien n'est accusé — une machine pleine fait attendre un enfant en
                // priorité minimale —, mais le message est mis de côté jusqu'au prochain
                // lancement, pour ne pas barrer la route à tous les plus anciens.
                tracing::warn!(
                    message = id.0,
                    "extracteur silencieux : message mis de côté, sans accusation"
                );
                self.silent.insert(id);
                return self.fail(Failure::Silent);
            }
            return self.paused();
        };

        let suspected = self.suspect.is_some_and(|it| it.id == id);
        let begun = !pdfs.is_empty() || current.announced.is_some();
        let verdict = match (death, current.announced) {
            (Death::Cpu, Some(piece)) => Verdict::Piece(PdfState::TimedOut, piece),
            (Death::Memory, Some(piece)) => Verdict::Piece(PdfState::OutOfMemory, piece),
            (Death::Unexplained, Some(piece)) if suspected => {
                Verdict::Piece(PdfState::Crashed, piece)
            }
            // Avant toute pièce : c'est l'analyse du message lui-même. Une cause connue condamne
            // tout de suite, une mort sans cause à la seconde.
            (Death::Cpu | Death::Memory, None) if !begun => Verdict::Message,
            (Death::Unexplained, None) if suspected && !begun => Verdict::Message,
            // **Entre deux pièces**, rien d'hostile ne tourne : une seconde mort y finit le
            // message avec ce qu'il a rendu, plutôt que de le relire sans fin.
            (Death::Unexplained, None) if suspected => Verdict::Finish,
            _ => Verdict::Suspect,
        };
        match verdict {
            Verdict::Piece(state, (rank, hash, size)) => {
                // La pièce en cours est rangée avec son poison, ce qui la précède avec son texte,
                // et le message reste à relire : ses pièces suivantes seront lues.
                pdfs.push(PdfRecord::Read {
                    rank,
                    hash: *hash.as_bytes(),
                    state,
                    revision: mailpdf::REVISION,
                    size,
                    text: None,
                });
                tracing::warn!(
                    message = id.0,
                    rang = rank,
                    etat = state_code(state),
                    "pièce PDF abandonnée"
                );
                self.suspect = None;
                self.cleared = 0;
                if !self.keep(store, id, blob, pdfs, false) {
                    return self.fail(Failure::Store);
                }
            }
            Verdict::Message => {
                tracing::warn!(message = id.0, "message abandonné par l'extracteur");
                match store.mark_unscannable(id, blob, Unscannable::Deadly) {
                    Ok(changed) => marked += u64::from(changed),
                    Err(_) => return self.fail(Failure::Store),
                }
                self.suspect = None;
                self.cleared = 0;
            }
            Verdict::Finish => {
                tracing::warn!(
                    message = id.0,
                    "extracteur mort deux fois entre deux pièces : message fini en l'état"
                );
                if !self.keep(store, id, blob, pdfs, true) {
                    return self.fail(Failure::Store);
                }
                marked += 1;
                self.suspect = None;
                self.cleared = 0;
            }
            Verdict::Suspect => {
                // Une première mort : le message est relu seul, dans un enfant neuf. Ce qu'il avait
                // rendu est gardé.
                tracing::warn!(message = id.0, "extracteur mort : message relu seul");
                if !pdfs.is_empty() && !self.keep(store, id, blob, pdfs, false) {
                    return self.fail(Failure::Store);
                }
                self.suspect = Some(Suspect { id, blob });
            }
        }
        // Mort, mais pas pour rien : quelque chose a été écrit ou décidé — un poison, une
        // condamnation, un suspect —, et le passage suivant repart tout de suite.
        self.backoff = self.limits.backoff;
        self.retry_at = None;
        Passed::Advanced { marked, remarked }
    }

    /// Range les pièces d'un message ; faux si le store a refusé.
    fn keep(
        &mut self,
        store: &Store,
        id: MessageId,
        blob: BlobHash,
        pdfs: Vec<PdfRecord>,
        finished: bool,
    ) -> bool {
        match store.record_scans(&[Scan {
            id,
            blob,
            pdfs,
            finished,
        }]) {
            Ok(recorded) => {
                for outcome in recorded {
                    if let Recorded::Done { remarked: true } = outcome {
                        self.owed += 1;
                    }
                }
                true
            }
            Err(error) => {
                tracing::warn!(%error, "PDF lus non rangés");
                false
            }
        }
    }

    /// Un enfant tué par un job, une recherche ou l'arrêt : rien n'est accusé.
    fn paused(&mut self) -> Passed {
        self.set(Status::Paused);
        Passed::Stopped
    }

    fn fail(&mut self, failure: Failure) -> Passed {
        tracing::warn!(
            echec = failure.code(),
            attente = self.backoff.as_secs(),
            "lecture des PDF en échec"
        );
        self.set(Status::Failed(failure));
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        Passed::Failed(failure)
    }
}

/// Ce que `settle` décide d'une mort.
#[derive(Debug)]
enum Verdict {
    /// La pièce en cours prend ce poison ; le message sera relu.
    Piece(PdfState, (u32, BlobHash, u64)),
    /// Le message entier est condamné : son analyse même a tué l'enfant.
    Message,
    /// Le message est fini avec ce qu'il a rendu.
    Finish,
    /// Rien n'est accusé : le message est relu seul.
    Suspect,
}

const fn state_code(state: PdfState) -> &'static str {
    match state {
        PdfState::Text => "text",
        PdfState::NoText => "no-text",
        PdfState::Encrypted => "encrypted",
        PdfState::Invalid => "invalid",
        PdfState::TooBig => "too-big",
        PdfState::Crashed => "crashed",
        PdfState::TimedOut => "timed-out",
        PdfState::OutOfMemory => "out-of-memory",
    }
}

#[derive(Debug, Clone, Copy)]
enum Death {
    Cpu,
    Memory,
    Unexplained,
}

/// Le message que l'enfant lisait quand il est mort.
#[derive(Debug)]
struct Current {
    id: MessageId,
    records: Vec<PdfRecord>,
    /// La pièce annoncée et pas encore rendue — rang, empreinte, taille : c'est elle qu'une mort
    /// accuse.
    announced: Option<(u32, BlobHash, u64)>,
}

/// Ce qu'un enfant a laissé en sortant.
#[derive(Debug)]
struct Ended {
    done: bool,
    kill: Option<Kill>,
    out_of_memory: bool,
    /// Le code d'un refus que l'enfant a dit avant de commencer.
    refused: Option<String>,
    current: Option<Current>,
    finished: HashSet<MessageId>,
    marked: u64,
    remarked: u64,
    failed_store: bool,
}

enum Launched {
    Unsupported,
    Failed(Failure),
}

// ---------------------------------------------------------------------------------------------
// L'automate : ce que l'enfant a le droit de dire, dans quel ordre.

#[derive(Debug)]
enum Phase {
    /// Rien reçu.
    Start,
    /// Entre deux messages ; `next` est le rang du prochain dans la requête.
    Between { next: usize },
    /// Dans un message.
    In {
        next: usize,
        current: Current,
        last_rank: Option<u32>,
        pieces: u32,
    },
    /// `done` reçu, ou un refus : plus rien n'est admis.
    Finished,
}

/// Ce qu'une ligne a fait avancer.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Nothing,
    /// Une pièce est annoncée : le budget processeur repart.
    Announced,
    /// Un message est rendu en entier.
    Scanned(MessageId, Vec<PdfRecord>),
    Done,
    /// L'enfant refuse de lire, avant d'avoir commencé.
    Refused(String),
}

struct Machine<'a> {
    items: &'a [Item],
    phase: Phase,
}

impl<'a> Machine<'a> {
    fn new(items: &'a [Item]) -> Self {
        Self {
            items,
            phase: Phase::Start,
        }
    }

    fn current(self) -> Option<Current> {
        match self.phase {
            Phase::In { current, .. } => Some(current),
            _ => None,
        }
    }

    /// Une ligne de l'enfant. `Err` : hors protocole — l'enfant est tué. **Une ligne refusée ne
    /// change rien** : le message en cours reste, avec ce qu'il a rendu, pour que la mort qui
    /// suit soit attribuée comme les autres.
    fn feed(&mut self, said: Said) -> Result<Step, ()> {
        match (&mut self.phase, said) {
            (Phase::Start, Said::Ready { revision }) if revision == mailpdf::REVISION => {
                self.phase = Phase::Between { next: 0 };
                Ok(Step::Nothing)
            }
            (Phase::Start, Said::Failed { code }) => {
                self.phase = Phase::Finished;
                Ok(Step::Refused(code))
            }
            (Phase::Between { next }, Said::Begin { message })
                if self
                    .items
                    .get(*next)
                    .is_some_and(|item| item.message == message) =>
            {
                let next = *next + 1;
                self.phase = Phase::In {
                    next,
                    current: Current {
                        id: MessageId(message),
                        records: Vec::new(),
                        announced: None,
                    },
                    last_rank: None,
                    pieces: 0,
                };
                Ok(Step::Nothing)
            }
            (Phase::Between { next }, Said::Done) if *next == self.items.len() => {
                self.phase = Phase::Finished;
                Ok(Step::Done)
            }
            (
                Phase::In {
                    current,
                    last_rank,
                    pieces,
                    ..
                },
                Said::Pdf {
                    message,
                    rank,
                    hash,
                    size,
                },
            ) if current.announced.is_none() => {
                let hash = BlobHash::from_hex(&hash).map_err(|_| ())?;
                if message != current.id.0 || last_rank.is_some_and(|last| rank <= last) {
                    return Err(());
                }
                current.announced = Some((rank, hash, size));
                *last_rank = Some(rank);
                *pieces += 1;
                Ok(Step::Announced)
            }
            (
                Phase::In {
                    current,
                    last_rank,
                    pieces,
                    ..
                },
                Said::Known {
                    message,
                    rank,
                    hash,
                },
            ) if current.announced.is_none() => {
                let hash = BlobHash::from_hex(&hash).map_err(|_| ())?;
                if message != current.id.0 || last_rank.is_some_and(|last| rank <= last) {
                    return Err(());
                }
                current.records.push(PdfRecord::Known {
                    rank,
                    hash: *hash.as_bytes(),
                });
                *last_rank = Some(rank);
                *pieces += 1;
                Ok(Step::Nothing)
            }
            (Phase::In { current, .. }, Said::Text { hash, state, text }) => {
                let Some((rank, announced, size)) = current.announced else {
                    return Err(());
                };
                let state = PdfState::from_code(state).ok_or(())?;
                let hash = BlobHash::from_hex(&hash).map_err(|_| ())?;
                if hash != announced
                    || text.len() > BUDGET
                    || (state != PdfState::Text && !text.is_empty())
                {
                    return Err(());
                }
                current.announced = None;
                current.records.push(PdfRecord::Read {
                    rank,
                    hash: *hash.as_bytes(),
                    state,
                    revision: mailpdf::REVISION,
                    size,
                    text: (state == PdfState::Text).then_some(text),
                });
                Ok(Step::Nothing)
            }
            (
                Phase::In {
                    next,
                    current,
                    pieces,
                    ..
                },
                Said::Scanned { message, pdfs },
            ) if current.announced.is_none() && message == current.id.0 && pdfs == *pieces => {
                let next = *next;
                match std::mem::replace(&mut self.phase, Phase::Between { next }) {
                    Phase::In { current, .. } => Ok(Step::Scanned(current.id, current.records)),
                    // Le motif ci-dessus vient de lire `In` : l'autre branche n'existe pas.
                    other => {
                        self.phase = other;
                        Err(())
                    }
                }
            }
            _ => Err(()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// La surveillance d'un enfant.

enum Line {
    Json(Vec<u8>),
    TooLong,
}

/// Lit une ligne d'au plus `cap` octets. `None` à la fin du flux ; `Some(false)` pour une ligne
/// trop longue, dont le reste est lu et jeté.
fn bounded_line(reader: &mut impl BufRead, cap: usize, out: &mut Vec<u8>) -> Option<bool> {
    out.clear();
    let read = reader
        .by_ref()
        .take(cap as u64)
        .read_until(b'\n', out)
        .ok()?;
    if read == 0 {
        return None;
    }
    if out.ends_with(b"\n") || out.len() < cap {
        return Some(true);
    }
    // Trop longue : le reste de la ligne est jeté, par morceaux bornés eux aussi.
    let mut rest = Vec::new();
    loop {
        rest.clear();
        let read = reader
            .by_ref()
            .take(cap as u64)
            .read_until(b'\n', &mut rest)
            .ok()?;
        if read == 0 || rest.ends_with(b"\n") {
            return Some(false);
        }
    }
}

/// Comment lancer un enfant.
struct Spawning<'a> {
    program: &'a Path,
    hook: Option<&'a Path>,
    limits: Limits,
    relayed: &'a Arc<Mutex<Relayed>>,
}

struct Supervision<'a> {
    watch: &'a Watch<'a>,
    limits: Limits,
    kill: Option<Kill>,
    system: sysinfo::System,
    pid: sysinfo::Pid,
    /// Le temps processeur de l'enfant au premier relevé après sa dernière ligne, en
    /// millisecondes ; `None` juste après une ligne. Le budget compte depuis là.
    cpu_mark: Option<u64>,
    /// Le dernier relevé : sous Windows, un relevé parcourt la table des processus, et il ne se
    /// fait qu'une fois par [`WATCH`], pas à chaque ligne.
    probed: Instant,
}

impl<'a> Supervision<'a> {
    /// Lance l'enfant, le borne, lui écrit sa requête, le surveille jusqu'à sa sortie, et range ce
    /// qu'il rend au fil de l'eau.
    #[allow(clippy::too_many_lines)]
    fn run(
        launch: &Spawning<'_>,
        store: &Store,
        request: &Request,
        watch: &'a Watch<'a>,
    ) -> Result<Ended, Launched> {
        let line =
            serde_json::to_string(request).map_err(|_| Launched::Failed(Failure::Protocol))?;
        // Une requête que l'enfant tronquerait ne se lirait pas : le dire, au lieu de lancer un
        // enfant pour rien à chaque passage. Avec [`KNOWN_AT_MOST`], elle tient.
        if line.len() >= REQUEST_CAP {
            tracing::warn!(octets = line.len(), "requête de l'extracteur trop longue");
            return Err(Launched::Failed(Failure::Protocol));
        }
        let mut command = crate::vectors::command(launch.program, CHILD_FLAG, store.root());
        for name in mailpdf::ENVIRONMENT {
            command.env_remove(name);
        }
        match launch.hook {
            Some(dir) => command.env(HOOK_ENV, dir),
            None => command.env_remove(HOOK_ENV),
        };
        // Un fil de calcul, pas un par cœur : l'enfant tourne à côté de quelqu'un qui travaille.
        command.env("RAYON_NUM_THREADS", "1");
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                tracing::warn!(%error, "extracteur non lancé");
                Launched::Failed(Failure::Spawn)
            })?;

        // **La borne avant la requête** : l'enfant n'a encore rien lu d'hostile. Tenue jusqu'à la fin de
        // la fonction, donc au-delà de la sortie de l'enfant : sous Windows, la lâcher fermerait
        // l'objet de travail, ce qui le tuerait.
        let _bound = match mailbound::bound(&child, launch.limits.memory) {
            Ok(bound) => bound,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                if error.kind() == std::io::ErrorKind::Unsupported {
                    return Err(Launched::Unsupported);
                }
                tracing::warn!(%error, "borne de mémoire refusée");
                return Err(Launched::Failed(Failure::Bound));
            }
        };

        // Les deux lecteurs, **ou aucun enfant** : un enfant dont la sortie n'est pas lue n'est
        // pas surveillé — ni budget, ni silence, ni arrêt.
        let out_of_memory = Arc::new(AtomicBool::new(false));
        let journal = child.stderr.take().and_then(|stderr| {
            let seen = Arc::clone(&out_of_memory);
            let budget = Arc::clone(launch.relayed);
            std::thread::Builder::new()
                .name("mailcore-pdf-journal".to_owned())
                .spawn(move || relay(stderr, &seen, &budget))
                .ok()
        });
        let (sender, lines) = mpsc::channel::<Line>();
        let reader = child.stdout.take().and_then(|stdout| {
            std::thread::Builder::new()
                .name("mailcore-pdf-lecture".to_owned())
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    let mut line = Vec::new();
                    while let Some(complete) = bounded_line(&mut reader, LINE_CAP, &mut line) {
                        let sent = if complete {
                            sender.send(Line::Json(std::mem::take(&mut line)))
                        } else {
                            sender.send(Line::TooLong)
                        };
                        if sent.is_err() {
                            return;
                        }
                    }
                })
                .ok()
        });
        let (Some(journal), Some(reader)) = (journal, reader) else {
            let _ = child.kill();
            let _ = child.wait();
            tracing::warn!("lecteurs de l'extracteur non lancés");
            return Err(Launched::Failed(Failure::Spawn));
        };
        // L'entrée standard reste ouverte jusqu'à la fin : la fermer dirait à l'enfant que le
        // parent est mort.
        let mut stdin = child.stdin.take();
        if let Some(input) = stdin.as_mut() {
            // Un échec d'écriture veut dire un enfant déjà mort : la surveillance le verra.
            let _ = writeln!(input, "{line}").and_then(|()| input.flush());
        }
        drop(line);

        let pid = sysinfo::Pid::from_u32(child.id());
        if let Ok(mut slot) = watch.child.lock() {
            *slot = Some(child);
        }
        let mut supervision = Supervision {
            watch,
            limits: launch.limits,
            kill: None,
            system: sysinfo::System::new(),
            pid,
            cpu_mark: None,
            probed: Instant::now(),
        };

        let mut machine = Machine::new(&request.items);
        let mut ended = Ended {
            done: false,
            kill: None,
            out_of_memory: false,
            refused: None,
            current: None,
            finished: HashSet::new(),
            marked: 0,
            remarked: 0,
            failed_store: false,
        };
        let mut batch: Vec<Scan> = Vec::new();
        let mut flushed = Instant::now();
        let mut heard = Instant::now();
        let blob_of = |id: MessageId| {
            request
                .items
                .iter()
                .find(|item| item.message == id.0)
                .and_then(|item| BlobHash::from_hex(&item.blob).ok())
        };

        loop {
            let received = lines.recv_timeout(WATCH);
            // **Après un `kill`, plus rien n'entre dans l'automate.** La mort s'attribue à ce qui
            // était en cours quand elle a été décidée : une ligne écrite entre le relevé et le
            // `kill` — le texte d'une pièce qui finissait, le début du message suivant — ferait
            // accuser un message dont aucune pièce n'a été ouverte. Les lignes sont lues
            // jusqu'au bout, et jetées.
            let listening = supervision.kill.is_none();
            match received {
                Ok(Line::Json(bytes)) if listening => {
                    heard = Instant::now();
                    supervision.cpu_mark = None;
                    // Une ligne qui n'est pas du JSON vient d'ailleurs que du protocole — une
                    // bibliothèque qui écrit sur la sortie standard : ignorée. Une ligne JSON hors
                    // séquence, elle, tue l'enfant.
                    if let Ok(said) = serde_json::from_slice::<Said>(&bytes) {
                        match machine.feed(said) {
                            Ok(Step::Scanned(id, pdfs)) => {
                                if let Some(blob) = blob_of(id) {
                                    batch.push(Scan {
                                        id,
                                        blob,
                                        pdfs,
                                        finished: true,
                                    });
                                    ended.finished.insert(id);
                                }
                            }
                            Ok(Step::Done) => ended.done = true,
                            Ok(Step::Refused(code)) => ended.refused = Some(code),
                            Ok(Step::Nothing | Step::Announced) => {}
                            Err(()) => supervision.kill(Kill::Protocol),
                        }
                    }
                }
                Ok(Line::TooLong) if listening => supervision.kill(Kill::Protocol),
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                // Le lecteur a vu la fin du flux : l'enfant est sorti, ou va sortir.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if supervision.kill.is_none() {
                if let Some(reason) = supervision.watch.interrupt() {
                    supervision.kill(reason);
                } else if supervision.over_budget() {
                    supervision.kill(Kill::Cpu);
                } else if heard.elapsed() > supervision.limits.silence {
                    supervision.kill(Kill::Wall);
                }
            }
            if !batch.is_empty() && flushed.elapsed() >= COMMIT_EVERY {
                flush(store, &mut batch, &mut ended);
                flushed = Instant::now();
            }
        }

        // L'enfant a fermé sa sortie : on l'attend, puis les deux lecteurs, qui ont tout lu — le
        // journal **avant** de lire son drapeau : la ligne d'une allocation refusée y passe juste
        // avant la mort, et la lire après aurait pris une mort expliquée pour une mort sans cause.
        let child = watch.child.lock().ok().and_then(|mut slot| slot.take());
        if let Some(mut child) = child {
            let _ = child.wait();
        }
        let _ = reader.join();
        let _ = journal.join();
        drop(stdin);
        flush(store, &mut batch, &mut ended);

        // L'arrêt du démon tue l'enfant sans passer par ici : il se lit sur le drapeau.
        ended.kill = if watch.stop.load(Ordering::SeqCst) {
            Some(Kill::Stop)
        } else {
            supervision.kill
        };
        ended.out_of_memory = out_of_memory.load(Ordering::SeqCst);
        ended.current = machine.current();
        Ok(ended)
    }

    /// Tue l'enfant, **la raison posée d'abord**.
    fn kill(&mut self, reason: Kill) {
        if self.kill.is_none() {
            self.kill = Some(reason);
        }
        if let Ok(mut slot) = self.watch.child.lock()
            && let Some(child) = slot.as_mut()
        {
            let _ = child.kill();
        }
    }

    /// Vrai si l'enfant a dépensé plus que son budget de processeur depuis sa dernière ligne.
    /// Relevé au plus une fois par [`WATCH`] ; un relevé manqué ne compte pas.
    fn over_budget(&mut self) -> bool {
        if self.probed.elapsed() < WATCH {
            return false;
        }
        self.probed = Instant::now();
        // Un relevé que le système n'a pas pu faire — table des processus refusée — ne pose pas
        // de marque : une marque à zéro ferait compter toute la vie de l'enfant contre la pièce
        // en cours au relevé suivant.
        let Some(now) = self.cpu() else {
            return false;
        };
        let Some(mark) = self.cpu_mark else {
            self.cpu_mark = Some(now);
            return false;
        };
        now.saturating_sub(mark) > u64::try_from(self.limits.cpu.as_millis()).unwrap_or(u64::MAX)
    }

    /// Le temps processeur de l'enfant, en millisecondes, si le système a pu le dire.
    fn cpu(&mut self) -> Option<u64> {
        self.system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[self.pid]),
            true,
            sysinfo::ProcessRefreshKind::nothing().with_cpu(),
        );
        self.system
            .process(self.pid)
            .map(sysinfo::Process::accumulated_cpu_time)
    }
}

/// Range ce qui attend, en une transaction.
fn flush(store: &Store, batch: &mut Vec<Scan>, ended: &mut Ended) {
    if batch.is_empty() {
        return;
    }
    match store.record_scans(batch) {
        Ok(recorded) => {
            for outcome in recorded {
                if let Recorded::Done { remarked } = outcome {
                    ended.marked += 1;
                    ended.remarked += u64::from(remarked);
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, "PDF lus non rangés");
            ended.failed_store = true;
        }
    }
    batch.clear();
}

/// Recopie le journal de l'enfant dans celui du parent, **borné** : une ligne coupée à
/// [`RELAY_LINE`] octets, au plus [`RELAY_PER_MINUTE`] lignes par minute, tous enfants confondus.
/// Et c'est là que se lit une allocation refusée : la bibliothèque standard l'écrit avant
/// d'avorter.
fn relay(stderr: impl Read, out_of_memory: &AtomicBool, budget: &Mutex<Relayed>) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    while bounded_line(&mut reader, RELAY_LINE, &mut line).is_some() {
        if line.windows(20).any(|w| w == b"memory allocation of") {
            out_of_memory.store(true, Ordering::SeqCst);
        }
        let Ok(mut budget) = budget.lock() else {
            continue;
        };
        if budget.window.elapsed() >= Duration::from_secs(60) {
            if budget.dropped > 0 {
                tracing::info!(
                    lignes = budget.dropped,
                    "journal de l'extracteur : lignes non recopiées"
                );
            }
            budget.window = Instant::now();
            budget.said = 0;
            budget.dropped = 0;
        }
        if budget.said >= RELAY_PER_MINUTE {
            budget.dropped += 1;
            continue;
        }
        budget.said += 1;
        drop(budget);
        let text = String::from_utf8_lossy(&line);
        tracing::info!(enfant = %text.trim_end(), "extracteur PDF");
    }
}

/// Lit tous les PDF d'un store sans démon — pour les bancs et un store qu'aucune coquille n'ouvre.
/// Rend le nombre de messages marqués et remis, et s'arrête au premier passage qui n'avance pas.
///
/// # Errors
///
/// La [`Failure`] du passage qui a échoué.
pub fn catch_up(root: &Utf8Path, program: &Path) -> Result<(u64, u64), Failure> {
    let store = Store::open(root).map_err(|_| Failure::Store)?;
    let mut extractor = Extractor::new(program);
    let stop = AtomicBool::new(false);
    let activity = Activity::default();
    let child = Mutex::new(None);
    let watch = Watch {
        stop: &stop,
        activity: &activity,
        child: &child,
    };
    let mut marked = 0;
    loop {
        match extractor.pass(&store, &watch) {
            Passed::Advanced { marked: m, .. } => marked += m,
            Passed::Nothing | Passed::Stopped => return Ok((marked, extractor.take_remarked())),
            Passed::Failed(failure) => return Err(failure),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Item, Machine, Said, Step, bounded_line};
    use mailcore::BlobHash;
    use mailcore::attachtext::PdfState;

    fn items() -> Vec<Item> {
        vec![
            Item {
                message: 5,
                blob: BlobHash::of(b"cinq").to_hex(),
            },
            Item {
                message: 3,
                blob: BlobHash::of(b"trois").to_hex(),
            },
        ]
    }

    fn hash(byte: u8) -> String {
        BlobHash::from_bytes([byte; 32]).to_hex()
    }

    fn ready() -> Said {
        Said::Ready {
            revision: mailpdf::REVISION,
        }
    }

    /// Chaque état a sa phrase ; seul l'échec promet un nouvel essai — c'est le seul où un recul
    /// est posé —, et le refus d'un système sans borne n'en promet aucun.
    #[test]
    fn every_state_has_its_sentence_and_only_a_failure_promises_a_retry() {
        use super::{Failure, Status, describe};
        use mailapi::dto::AttachmentsState;
        let all = [
            Status::Disabled,
            Status::Waiting,
            Status::Idle,
            Status::Working { pending: 1_330 },
            Status::Paused,
            Status::Busy,
            Status::Unsupported,
            Status::Failed(Failure::Spawn),
        ];
        for status in all {
            let said = describe(status);
            assert!(!said.sentence.is_empty(), "{status:?}");
            assert_eq!(
                said.sentence.contains("réessaie"),
                matches!(status, Status::Failed(_)),
                "{status:?} : {}",
                said.sentence
            );
        }
        let working = describe(Status::Working { pending: 1_330 });
        assert_eq!(working.state, AttachmentsState::Working);
        assert_eq!(working.pending, 1_330);
        assert!(
            working.sentence.contains("1330 messages"),
            "{}",
            working.sentence
        );
        assert_eq!(
            describe(Status::Unsupported).state,
            AttachmentsState::Unavailable
        );
    }

    /// Une ligne refusée ne change rien à l'automate : le message en cours et sa pièce annoncée
    /// restent, pour que la mort qui suit — l'enfant est tué — soit attribuée comme les autres.
    /// Avant le 2026-10-20, l'automate se vidait, et une violation du protocole n'accusait jamais
    /// rien.
    #[test]
    fn a_refused_line_leaves_the_message_in_progress() {
        let items = items();
        let mut machine = Machine::new(&items);
        machine.feed(ready()).unwrap();
        machine.feed(Said::Begin { message: 5 }).unwrap();
        machine
            .feed(Said::Pdf {
                message: 5,
                rank: 0,
                hash: hash(7),
                size: 10,
            })
            .unwrap();
        assert!(machine.feed(Said::Done).is_err());
        let current = machine.current().expect("le message en cours a été perdu");
        assert_eq!(current.id.0, 5);
        assert_eq!(current.announced.map(|(rank, ..)| rank), Some(0));
    }

    /// Un refus que l'enfant dit avant de commencer est rendu tel quel, avec son code : ce n'est
    /// pas une violation du protocole.
    #[test]
    fn a_refusal_before_ready_is_reported_with_its_code() {
        let items = items();
        let mut machine = Machine::new(&items);
        assert_eq!(
            machine.feed(Said::Failed {
                code: "environment".to_owned()
            }),
            Ok(Step::Refused("environment".to_owned()))
        );
    }

    /// Le plafond du journal vaut pour tous les enfants ensemble : deux enfants qui se suivent dans
    /// la minute ne recopient pas chacun deux cents lignes.
    #[test]
    fn the_journal_cap_is_shared_between_children() {
        let budget = std::sync::Mutex::new(super::Relayed {
            window: std::time::Instant::now(),
            said: 0,
            dropped: 0,
        });
        let seen = std::sync::atomic::AtomicBool::new(false);
        let lines: String = (0..150).map(|n| format!("ligne {n}\n")).collect();
        super::relay(lines.as_bytes(), &seen, &budget);
        super::relay(lines.as_bytes(), &seen, &budget);
        let budget = budget.into_inner().unwrap();
        assert_eq!(
            (budget.said, budget.dropped),
            (super::RELAY_PER_MINUTE, 100)
        );
        assert!(!seen.load(std::sync::atomic::Ordering::SeqCst));
        // Le contrôle positif du drapeau : la ligne de la bibliothèque standard le lève.
        let budget = std::sync::Mutex::new(super::Relayed {
            window: std::time::Instant::now(),
            said: 0,
            dropped: 0,
        });
        super::relay(
            &b"memory allocation of 67108864 bytes failed\n"[..],
            &seen,
            &budget,
        );
        assert!(seen.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Une mort décidée **entre deux pièces** ne condamne jamais le message entier : rien
    /// d'hostile n'y tourne. C'est le cas qu'une course entre un relevé de processeur et une ligne
    /// déjà écrite fabriquait — le message était condamné, et le texte de sa pièce lue, jeté.
    #[test]
    fn a_death_between_two_pieces_never_condemns_the_message() {
        use mailcore::model::{FolderKind, MessageFlags};
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = mailcore::Store::open(root).unwrap();
        let blob = BlobHash::of(b"message");
        let writer = store.writer().unwrap();
        let account = writer.upsert_account("imap", "compte").unwrap();
        let folder = writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        let (id, _) = writer
            .insert_message(&mailcore::NewMessage {
                blob,
                rfc822_id: None,
                date: 1_700_000_000,
                from_addr: "a@exemple.invalid",
                from_name: None,
                subject: "pièces",
                size: 10,
                has_attachments: true,
            })
            .unwrap();
        writer
            .insert_ref(id, folder, 1_700_000_000, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();

        for kill in [super::Kill::Cpu, super::Kill::Protocol] {
            let mut extractor = super::Extractor::new("programme");
            let ended = super::Ended {
                done: false,
                kill: Some(kill),
                out_of_memory: false,
                refused: None,
                current: Some(super::Current {
                    id,
                    records: vec![mailcore::store::pdfs::PdfRecord::Read {
                        rank: 0,
                        hash: [7; 32],
                        state: PdfState::Text,
                        revision: mailpdf::REVISION,
                        size: 10,
                        text: Some("facture du mois".to_owned()),
                    }],
                    announced: None,
                }),
                finished: std::collections::HashSet::new(),
                marked: 0,
                remarked: 0,
                failed_store: false,
            };
            extractor.settle(&store, &[(id, blob)], ended, 0);
            assert_eq!(
                store.unscanned_total().unwrap(),
                1,
                "{kill:?} : le message a été condamné"
            );
            assert_eq!(
                store.message_pdfs(id).unwrap(),
                [(0, PdfState::Text)],
                "{kill:?} : la pièce lue a été jetée"
            );
        }
    }

    #[test]
    fn a_whole_conversation_is_admitted() {
        let items = items();
        let mut machine = Machine::new(&items);
        assert_eq!(machine.feed(ready()), Ok(Step::Nothing));
        assert_eq!(machine.feed(Said::Begin { message: 5 }), Ok(Step::Nothing));
        assert_eq!(
            machine.feed(Said::Pdf {
                message: 5,
                rank: 0,
                hash: hash(7),
                size: 10,
            }),
            Ok(Step::Announced)
        );
        assert_eq!(
            machine.feed(Said::Text {
                hash: hash(7),
                state: PdfState::Text.code(),
                text: "facture".to_owned(),
            }),
            Ok(Step::Nothing)
        );
        assert_eq!(
            machine.feed(Said::Known {
                message: 5,
                rank: 2,
                hash: hash(8)
            }),
            Ok(Step::Nothing)
        );
        let Ok(Step::Scanned(id, pdfs)) = machine.feed(Said::Scanned {
            message: 5,
            pdfs: 2,
        }) else {
            panic!("message non rendu");
        };
        assert_eq!((id.0, pdfs.len()), (5, 2));
        assert_eq!(machine.feed(Said::Begin { message: 3 }), Ok(Step::Nothing));
        assert!(matches!(
            machine.feed(Said::Scanned {
                message: 3,
                pdfs: 0
            }),
            Ok(Step::Scanned(..))
        ));
        assert_eq!(machine.feed(Said::Done), Ok(Step::Done));
        // Rien n'est admis après la fin.
        assert_eq!(machine.feed(Said::Done), Err(()));
    }

    /// Chaque écart au protocole tue l'enfant — dont le cas qui compte : un texte d'une pièce qui
    /// n'a pas été annoncée, ou un message rendu sans que toutes ses pièces le soient.
    #[test]
    fn every_line_out_of_sequence_is_refused() {
        let items = items();
        let text = |h: u8, state: PdfState, text: &str| Said::Text {
            hash: hash(h),
            state: state.code(),
            text: text.to_owned(),
        };
        let begun = |machine: &mut Machine<'_>| {
            machine.feed(ready()).unwrap();
            machine.feed(Said::Begin { message: 5 }).unwrap();
        };
        let cases: Vec<(&str, Vec<Said>)> = vec![
            ("avant prêt", vec![Said::Begin { message: 5 }]),
            (
                "mauvaise révision",
                vec![Said::Ready {
                    revision: mailpdf::REVISION + 1,
                }],
            ),
            ("hors de l'ordre", vec![ready(), Said::Begin { message: 3 }]),
            ("fin trop tôt", vec![ready(), Said::Done]),
        ];
        for (name, lines) in cases {
            let mut machine = Machine::new(&items);
            let refused = lines.into_iter().any(|said| machine.feed(said).is_err());
            assert!(refused, "{name}");
        }

        let in_message: Vec<(&str, Vec<Said>)> = vec![
            ("texte sans annonce", vec![text(7, PdfState::Text, "x")]),
            (
                "texte d'une autre pièce",
                vec![
                    Said::Pdf {
                        message: 5,
                        rank: 0,
                        hash: hash(7),
                        size: 10,
                    },
                    text(8, PdfState::Text, "x"),
                ],
            ),
            (
                "texte d'un poison",
                vec![
                    Said::Pdf {
                        message: 5,
                        rank: 0,
                        hash: hash(7),
                        size: 10,
                    },
                    text(7, PdfState::NoText, "x"),
                ],
            ),
            (
                "état inconnu",
                vec![
                    Said::Pdf {
                        message: 5,
                        rank: 0,
                        hash: hash(7),
                        size: 10,
                    },
                    Said::Text {
                        hash: hash(7),
                        state: 99,
                        text: String::new(),
                    },
                ],
            ),
            (
                "rang qui recule",
                vec![
                    Said::Known {
                        message: 5,
                        rank: 2,
                        hash: hash(7),
                    },
                    Said::Known {
                        message: 5,
                        rank: 1,
                        hash: hash(8),
                    },
                ],
            ),
            (
                "pièce d'un autre message",
                vec![Said::Known {
                    message: 3,
                    rank: 0,
                    hash: hash(7),
                }],
            ),
            (
                "rendu avec une pièce en cours",
                vec![
                    Said::Pdf {
                        message: 5,
                        rank: 0,
                        hash: hash(7),
                        size: 10,
                    },
                    Said::Scanned {
                        message: 5,
                        pdfs: 1,
                    },
                ],
            ),
            (
                "compte de pièces faux",
                vec![Said::Scanned {
                    message: 5,
                    pdfs: 1,
                }],
            ),
        ];
        for (name, lines) in in_message {
            let mut machine = Machine::new(&items);
            begun(&mut machine);
            let refused = lines.into_iter().any(|said| machine.feed(said).is_err());
            assert!(refused, "{name}");
        }
    }

    /// Un texte qui porte une ligne de protocole n'en fabrique pas une : il est échappé dans sa
    /// propre ligne JSON.
    #[test]
    fn a_text_carrying_a_protocol_line_stays_one_line() {
        let said = Said::Text {
            hash: hash(7),
            state: PdfState::Text.code(),
            text: "facture\n{\"said\":\"scanned\",\"message\":5,\"pdfs\":0}\n".to_owned(),
        };
        let line = serde_json::to_string(&said).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<Said>(&line).unwrap(), said);
    }

    #[test]
    fn a_line_past_its_cap_is_reported_and_the_next_one_is_read_whole() {
        let input = b"court\nbeaucoup trop long pour la borne\nsuivant\n";
        let mut reader = std::io::BufReader::new(&input[..]);
        let mut line = Vec::new();
        assert_eq!(bounded_line(&mut reader, 10, &mut line), Some(true));
        assert_eq!(line, b"court\n");
        assert_eq!(bounded_line(&mut reader, 10, &mut line), Some(false));
        assert_eq!(bounded_line(&mut reader, 10, &mut line), Some(true));
        assert_eq!(line, b"suivant\n");
        assert_eq!(bounded_line(&mut reader, 10, &mut line), None);
    }
}

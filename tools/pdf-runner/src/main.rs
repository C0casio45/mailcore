//! # pdf-runner
//!
//! Le lecteur de candidats du banc A2 du PDF — conception du 2026-10-20, `docs/PHASE-4.md`. Le
//! parent est `cargo xtask pdf-compare` ; ce binaire n'est lancé que par lui.
//!
//! ## Le protocole
//!
//! `pdf-runner <candidat>`, avec `candidat` parmi `inspector`, `extract`, `lopdf` : lit **tout**
//! l'entrée standard — les octets d'un PDF —, en calcule la longueur et l'empreinte BLAKE3 (le
//! parent les compare à ce qu'il a envoyé : c'est le contrôle de transport), extrait, et écrit
//! **une** ligne JSON sur la sortie standard, et rien d'autre :
//!
//! ```text
//! {"candidate","received","hash","outcome","text","pages","pages_failed","wall_us","cpu_ms"}
//! ```
//!
//! `outcome` vaut `text`, `error` (le document ne s'ouvre pas, ou aucune page ne se lit), `panic`
//! (rattrapée), ou `too-big` (plus de [`INPUT_CAP`] octets : l'entrée est quand même lue jusqu'au
//! bout, hachée sans être gardée, pour que le contrôle de transport vaille aussi pour elle).
//!
//! `pdf-runner fixture <nom>` écrit sur la sortie standard un PDF fabriqué — voir [`fixtures`].
//!
//! ## Ce qui est mesuré, et où
//!
//! Le temps mural et le temps processeur du **processus** sont relevés autour de l'appel du
//! candidat seul : ni le démarrage, ni la lecture de l'entrée, ni l'écriture de la réponse. Le
//! temps processeur vient de sysinfo, donc de `GetProcessTimes` sous Windows, dont la résolution
//! est le pas d'horloge du système — de l'ordre de 16 ms : un petit PDF y vaut 0 ou 16. La mémoire,
//! elle, est relevée par le parent, qui seul peut voir un enfant mourir d'une allocation.
//!
//! ## Ce qui n'en sort jamais
//!
//! Aucun message d'erreur de bibliothèque, aucun message de panique : l'un ou l'autre peut citer un
//! objet du document. Une panique est réduite à son emplacement, sur l'erreur standard, que le
//! parent ne lit pas.

#![forbid(unsafe_code)]

mod candidates;
mod fixtures;

use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::time::Instant;

use candidates::{Candidate, Extraction, Unreadable};
use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Ce que le lecteur garde au plus de son entrée : 128 Mio. Au-delà, la réponse est `too-big`. Le
/// recensement compte les pièces de plus de 64 Mio ; la borne est deux fois au-dessus, pour qu'elle
/// ne morde que sur ce qui ne devrait pas exister.
const INPUT_CAP: u64 = 128 * 1024 * 1024;

/// Le plafond du texte rendu : `mailcore::attachtext::BUDGET`, 256 Kio. **Recopié**, faute de
/// pouvoir l'importer — ce crate ne dépend pas de l'espace de travail. Le parent vérifie chaque
/// réponse contre le vrai `BUDGET` : un écart se voit au premier contrôle, pas à l'usage.
const TEXT_CAP: usize = 256 * 1024;

/// La pile du fil d'extraction, **explicite** : la pile par défaut d'un fil n'est pas la même selon
/// le système, et un analyseur récursif qui passe sous Linux déborderait sous Windows. Seize Mio,
/// comme l'enfant livré.
const STACK: usize = 16 * 1024 * 1024;

/// L'issue que le lecteur rapporte. Les morts — délai, mémoire, plantage — sont l'affaire du
/// parent : un processus mort ne rapporte rien.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Outcome {
    Text,
    Error,
    Panic,
    TooBig,
}

/// La ligne de réponse. Les noms des champs sont le protocole : le parent les lit sans tolérer de
/// champ inconnu.
#[derive(Debug, Serialize)]
pub(crate) struct Response {
    candidate: &'static str,
    received: u64,
    hash: String,
    pub(crate) outcome: Outcome,
    text: String,
    pages: u32,
    pages_failed: u32,
    wall_us: u64,
    cpu_ms: u64,
}

/// Ce qui est arrivé sur l'entrée standard.
#[derive(Debug)]
pub(crate) struct Input {
    /// `None` au-delà de la borne : l'entrée a été lue et hachée, pas gardée.
    bytes: Option<Vec<u8>>,
    received: u64,
    hash: blake3::Hash,
}

impl Input {
    /// Une entrée entière, pour les tests.
    #[cfg(test)]
    pub(crate) fn whole(bytes: Vec<u8>) -> Self {
        Self {
            received: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            hash: blake3::hash(&bytes),
            bytes: Some(bytes),
        }
    }
}

/// Lit `reader` jusqu'au bout. Garde au plus `cap` octets ; au-delà, continue de lire et de hacher
/// sans garder — l'enfant qui s'arrêterait de lire ferait échouer l'écriture du parent, et le
/// contrôle de transport n'aurait rien à comparer.
fn read_input(reader: &mut impl Read, cap: u64) -> std::io::Result<Input> {
    let mut hasher = blake3::Hasher::new();
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(cap.saturating_add(1))
        .read_to_end(&mut bytes)?;
    hasher.update(&bytes);
    let mut received = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if received <= cap {
        return Ok(Input {
            bytes: Some(bytes),
            received,
            hash: hasher.finalize(),
        });
    }
    drop(bytes);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let Some(slice) = chunk.get(..read) else {
            break;
        };
        hasher.update(slice);
        received = received.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok(Input {
        bytes: None,
        received,
        hash: hasher.finalize(),
    })
}

/// Le temps processeur accumulé par ce processus, en millisecondes.
#[derive(Debug)]
struct CpuClock {
    system: System,
    pid: Pid,
}

impl CpuClock {
    fn new() -> Self {
        Self {
            system: System::new(),
            pid: Pid::from_u32(std::process::id()),
        }
    }

    fn read(&mut self) -> u64 {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            false,
            ProcessRefreshKind::nothing().with_cpu(),
        );
        self.system
            .process(self.pid)
            .map_or(0, sysinfo::Process::accumulated_cpu_time)
    }
}

/// Un appel chronométré.
#[derive(Debug)]
struct Measured<T> {
    /// `Err` : l'appel a paniqué.
    result: Result<T, ()>,
    wall_us: u64,
    cpu_ms: u64,
}

/// Exécute `work` sur un fil à pile de [`STACK`] octets, sous `catch_unwind`, et relève autour de
/// l'appel **seul** le temps mural et le temps processeur du processus.
///
/// `AssertUnwindSafe` : après une panique, rien de ce que `work` a touché n'est relu — la réponse
/// est `panic`, sans texte.
fn measured<T: Send>(work: impl FnOnce() -> T + Send) -> Measured<T> {
    std::thread::scope(|scope| {
        let spawned = std::thread::Builder::new()
            .name("extraction".to_owned())
            .stack_size(STACK)
            .spawn_scoped(scope, move || {
                let mut clock = CpuClock::new();
                let cpu_before = clock.read();
                let started = Instant::now();
                let result = catch_unwind(AssertUnwindSafe(work)).map_err(|_| ());
                let wall = started.elapsed();
                let cpu_after = clock.read();
                Measured {
                    result,
                    wall_us: u64::try_from(wall.as_micros()).unwrap_or(u64::MAX),
                    cpu_ms: cpu_after.saturating_sub(cpu_before),
                }
            });
        match spawned {
            Ok(handle) => handle.join().unwrap_or(Measured {
                result: Err(()),
                wall_us: 0,
                cpu_ms: 0,
            }),
            // Un fil qui ne se lance pas — plus de mémoire pour sa pile — n'est pas une panique du
            // candidat, mais rien ne l'a lu non plus : l'issue la plus proche est l'échec.
            Err(_) => Measured {
                result: Err(()),
                wall_us: 0,
                cpu_ms: 0,
            },
        }
    })
}

/// Coupe au plus près de `budget` octets, **sur une frontière de caractère** : couper un `String`
/// au milieu d'un caractère paniquerait, et sur un corpus français un caractère multi-octet est la
/// règle.
fn cut(mut text: String, budget: usize) -> String {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

/// La réponse d'un candidat à une entrée.
pub(crate) fn respond(candidate: Candidate, input: Input) -> Response {
    let mut response = Response {
        candidate: candidate.name(),
        received: input.received,
        hash: input.hash.to_hex().to_string(),
        outcome: Outcome::TooBig,
        text: String::new(),
        pages: 0,
        pages_failed: 0,
        wall_us: 0,
        cpu_ms: 0,
    };
    let Some(bytes) = input.bytes else {
        return response;
    };
    let call = measured(|| candidate.extract(&bytes));
    drop(bytes);
    response.wall_us = call.wall_us;
    response.cpu_ms = call.cpu_ms;
    finish(&mut response, call.result);
    response
}

/// Range le résultat d'un appel dans la réponse.
fn finish(response: &mut Response, result: Result<Result<Extraction, Unreadable>, ()>) {
    match result {
        Err(()) => response.outcome = Outcome::Panic,
        Ok(Err(Unreadable)) => response.outcome = Outcome::Error,
        Ok(Ok(extraction)) => {
            // Un document ouvert dont **aucune** page ne se lit n'a rien rendu : c'est un échec,
            // et `pages_failed` dit lequel. Un document sans page, lui, n'a rien à rendre.
            response.outcome =
                if extraction.pages > 0 && extraction.pages_failed >= extraction.pages {
                    Outcome::Error
                } else {
                    Outcome::Text
                };
            response.pages = extraction.pages;
            response.pages_failed = extraction.pages_failed;
            response.text = cut(extraction.text, TEXT_CAP);
        }
    }
}

fn serve(candidate: Candidate) -> ExitCode {
    // L'emplacement seul : le message d'une panique peut citer un objet du document.
    std::panic::set_hook(Box::new(|info| match info.location() {
        Some(location) => eprintln!("panique à {}:{}", location.file(), location.line()),
        None => eprintln!("panique"),
    }));
    let input = match read_input(&mut std::io::stdin().lock(), INPUT_CAP) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("lecture de l'entrée : {}", error.kind());
            return ExitCode::FAILURE;
        }
    };
    let response = respond(candidate, input);
    let line = match serde_json::to_string(&response) {
        Ok(line) => line,
        Err(_) => {
            eprintln!("réponse non sérialisable");
            return ExitCode::FAILURE;
        }
    };
    let mut out = std::io::stdout().lock();
    let written = out
        .write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    if written.is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn fixture(name: &str) -> ExitCode {
    let Some(built) = fixtures::by_name(name) else {
        eprintln!("fixture inconnue : {name} (connues : {})", fixtures::NAMES);
        return ExitCode::from(2);
    };
    let bytes = match built {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("fabrication de {name} : {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = std::io::stdout().lock();
    if out.write_all(&bytes).and_then(|()| out.flush()).is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!(
        "usage : pdf-runner <inspector|extract|lopdf>   (un PDF sur l'entrée, une ligne JSON)\n\
         \x20       pdf-runner fixture <{}>",
        fixtures::NAMES
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [verb, name] if verb == "fixture" => fixture(name),
        [name] => match Candidate::parse(name) {
            Some(candidate) => serve(candidate),
            None => usage(),
        },
        _ => usage(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// La coupe ne tombe jamais au milieu d'un caractère, quelle que soit la largeur de celui-ci.
    #[test]
    fn the_cut_never_splits_a_character() {
        for sample in ["é", "€", "𝄞", "a"] {
            let text = sample.repeat(TEXT_CAP);
            for budget in [TEXT_CAP, TEXT_CAP - 1, TEXT_CAP - 2, TEXT_CAP - 3, 1, 0] {
                let kept = cut(text.clone(), budget);
                assert!(kept.len() <= budget, "{sample} à {budget}");
                assert!(
                    kept.chars().all(|c| c.to_string() == sample),
                    "{sample} coupé"
                );
                // Et au plus un caractère de perdu : la coupe recule, elle ne vide pas.
                assert!(budget - kept.len() < sample.len(), "{sample} à {budget}");
            }
        }
        // Le contrôle inverse : un texte sous le plafond passe intact.
        assert_eq!(cut("court".to_owned(), TEXT_CAP), "court");
    }

    /// Le plafond est appliqué à la réponse, pas seulement disponible.
    #[test]
    fn a_long_text_is_cut_in_the_response() {
        let mut response = respond(Candidate::Lopdf, Input::whole(Vec::new()));
        finish(
            &mut response,
            Ok(Ok(Extraction {
                text: "é".repeat(TEXT_CAP),
                pages: 1,
                pages_failed: 0,
            })),
        );
        assert_eq!(response.outcome, Outcome::Text);
        assert!(response.text.len() <= TEXT_CAP);
        assert!(!response.text.is_empty());
    }

    /// Entrée vide ou charabia, par le chemin complet : `error`, jamais `panic`.
    #[test]
    fn nothing_or_noise_answers_error_not_panic() {
        for bytes in [Vec::new(), b"%%%% ceci n'est pas un PDF".to_vec()] {
            for candidate in Candidate::ALL {
                let answer = respond(candidate, Input::whole(bytes.clone()));
                assert_eq!(answer.outcome, Outcome::Error, "{}", candidate.name());
                assert!(answer.text.is_empty());
            }
        }
    }

    /// Une panique du candidat est rattrapée et devient une issue.
    #[test]
    fn a_panic_is_caught_and_reported() {
        let call = measured(|| -> Result<Extraction, Unreadable> { panic!("candidat fautif") });
        assert!(call.result.is_err());
        let mut response = respond(Candidate::Lopdf, Input::whole(Vec::new()));
        finish(&mut response, call.result);
        assert_eq!(response.outcome, Outcome::Panic);
    }

    /// La pile est bien celle qu'on a demandée : une récursion qui déborderait la pile par défaut
    /// d'un fil — 2 Mio — passe sur la nôtre. Six Mio, pour garder une marge large sous seize : un
    /// débordement ne panique pas, il fait avorter tout le binaire de test.
    #[test]
    fn the_extraction_thread_has_the_stack_it_asked_for() {
        fn deep(level: u32) -> u64 {
            let frame = std::hint::black_box([level as u8; 4096]);
            if level == 0 {
                return u64::from(frame[0]);
            }
            deep(level - 1) + u64::from(std::hint::black_box(frame)[4095])
        }
        let call = measured(|| deep(1536));
        assert!(call.result.is_ok());
    }

    /// Le contrôle de transport côté lecteur : longueur et empreinte de ce qui est arrivé, y
    /// compris au-delà de la borne, où rien n'est gardé.
    #[test]
    fn the_input_is_measured_even_beyond_the_cap() {
        let sent: Vec<u8> = (0..10_000u32).map(|i| i.to_le_bytes()[0]).collect();
        let under = read_input(&mut sent.as_slice(), 20_000).unwrap();
        assert_eq!(under.bytes.as_deref(), Some(sent.as_slice()));
        assert_eq!(under.received, 10_000);
        assert_eq!(under.hash, blake3::hash(&sent));

        let over = read_input(&mut sent.as_slice(), 1_000).unwrap();
        assert!(over.bytes.is_none());
        assert_eq!(over.received, 10_000, "la lecture s'est arrêtée à la borne");
        assert_eq!(over.hash, blake3::hash(&sent));
        assert_eq!(respond(Candidate::Lopdf, over).outcome, Outcome::TooBig);

        // À la borne exactement, l'entrée est gardée.
        let exact = read_input(&mut sent.as_slice(), 10_000).unwrap();
        assert!(exact.bytes.is_some());
    }

    /// Les noms des champs et des issues sont le protocole : le parent refuse un champ inconnu.
    #[test]
    fn the_response_line_has_exactly_the_protocol_fields() {
        let response = respond(Candidate::Inspector, Input::whole(Vec::new()));
        let value: serde_json::Value = serde_json::to_value(&response).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "candidate",
                "cpu_ms",
                "hash",
                "outcome",
                "pages",
                "pages_failed",
                "received",
                "text",
                "wall_us"
            ]
        );
        assert_eq!(value["candidate"], "inspector");
        for (outcome, written) in [
            (Outcome::Text, "text"),
            (Outcome::Error, "error"),
            (Outcome::Panic, "panic"),
            (Outcome::TooBig, "too-big"),
        ] {
            assert_eq!(serde_json::to_value(outcome).unwrap(), written);
        }
        // Une ligne : aucune fin de ligne dans la sérialisation, même si le texte en porte.
        let mut multiline = respond(Candidate::Lopdf, Input::whole(Vec::new()));
        multiline.text = "un\ndeux\r\n".to_owned();
        assert!(!serde_json::to_string(&multiline).unwrap().contains('\n'));
    }
}

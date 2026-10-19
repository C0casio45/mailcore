//! La passe de fond des vecteurs, de bout en bout : le fil du démon décide, et **le vrai binaire
//! `maild`**, relancé en processus enfant, charge un vrai transformeur — minuscule — et écrit.
//!
//! C'est ce qu'aucun test de `mailvec` ne peut dire : que l'enfant se lance, qu'il rend une ligne
//! que le parent comprend, et que le parent ne le lance pas quand il ne doit pas.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use maild::jobs::Activity;
use maild::vectors::{Launch, Status, Vectoriser};
use mailvec::pass::Explicit;
use mailvec::testing::{TINY_PIECES, corpus_with, tiny_bert};

/// Le binaire `maild` compilé par Cargo pour ces tests.
fn program() -> Launch {
    Launch::program(env!("CARGO_BIN_EXE_maild"))
}

/// Un store de messages que le transformeur minuscule sait lire — ses soixante-quatre positions
/// ne tiennent pas un texte long.
fn store(count: usize, with_model: bool) -> (tempfile::TempDir, mailcore::Store, Utf8PathBuf) {
    let messages: Vec<(String, String)> = (0..count)
        .map(|index| ("bonjour".to_owned(), format!("bon été {index}")))
        .collect();
    let (dir, store) = corpus_with(&messages);
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    if with_model {
        tiny_bert(&mailvec::bert::default_dir(&root), TINY_PIECES);
    }
    (dir, store, root)
}

/// Attend que `check` soit vrai, ou échoue au bout de soixante secondes.
fn eventually(what: &str, vectoriser: &Vectoriser, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "{what} : jamais atteint, état {:?}",
            vectoriser.status()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn the_messages_a_harvest_brings_get_their_vector_by_themselves() {
    let (_dir, store, root) = store(5, true);
    let vectoriser = Vectoriser::start_after(&root, Activity::default(), program(), Duration::ZERO);
    eventually("le corpus vectorisé", &vectoriser, || {
        store.unvectorised_total().unwrap() == 0 && vectoriser.status() == Status::Idle
    });
    let summary = mailcore::vectors::summary_of(&store.vectors_path())
        .unwrap()
        .unwrap();
    assert_eq!(summary.records, 5);
    assert_eq!(summary.dim, 8);

    // Un message arrive — comme après une moisson —, et le réveil suffit.
    store.reset_vectorised().unwrap();
    vectoriser.nudge();
    eventually("le message suivi", &vectoriser, || {
        store.unvectorised_total().unwrap() == 0
    });
    vectoriser.stop();
}

#[test]
fn nothing_starts_while_a_job_is_running_and_it_resumes_after() {
    let (_dir, store, root) = store(3, true);
    let activity = Activity::default();
    let held = activity.hold();
    let vectoriser = Vectoriser::start_after(&root, activity.clone(), program(), Duration::ZERO);
    eventually("la passe en pause", &vectoriser, || {
        vectoriser.status() == Status::Paused
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        store.unvectorised_total().unwrap(),
        3,
        "une passe a tourné pendant un job"
    );

    // Le contrôle inverse : le job fini, la passe se fait — sans autre réveil que sa fin.
    drop(held);
    eventually("la passe après le job", &vectoriser, || {
        store.unvectorised_total().unwrap() == 0
    });
    vectoriser.stop();
}

#[test]
fn without_a_model_nothing_is_launched_and_the_state_says_why() {
    let (_dir, store, root) = store(3, false);
    let vectoriser = Vectoriser::start_after(&root, Activity::default(), program(), Duration::ZERO);
    eventually("l'absence de modèle", &vectoriser, || {
        vectoriser.status() == Status::ModelAbsent
    });
    assert_eq!(store.unvectorised_total().unwrap(), 3);
    assert!(!store.vectors_path().exists());
    vectoriser.stop();
}

#[test]
fn a_store_that_needs_an_explicit_pass_is_left_untouched() {
    let (_dir, store, root) = store(3, true);
    // Écrit par une autre composition : un budget différent.
    let model = mailvec::bert::Bert::load(mailvec::bert::default_dir(&root)).unwrap();
    mailvec::pass::update_with(
        &store,
        &model,
        mailvec::text::Budget::of(150),
        &mailcore::Progress::new(),
    )
    .unwrap();
    store.reset_vectorised().unwrap();
    let before = std::fs::read(store.vectors_path().as_std_path()).unwrap();

    let vectoriser = Vectoriser::start_after(&root, Activity::default(), program(), Duration::ZERO);
    eventually("le refus", &vectoriser, || {
        matches!(
            vectoriser.status(),
            Status::NeedsExplicitPass {
                reason: Explicit::Stale,
                ..
            }
        )
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        std::fs::read(store.vectors_path().as_std_path()).unwrap(),
        before,
        "la passe de fond a touché un magasin qu'elle devait laisser"
    );
    assert_eq!(store.unvectorised_total().unwrap(), 3);
    vectoriser.stop();
}

#[test]
fn a_pass_holding_the_store_is_waited_for_not_fought() {
    let (_dir, store, root) = store(3, true);
    let held = mailcore::vectors::VectorLock::try_acquire(&store.vectors_path()).unwrap();
    let vectoriser = Vectoriser::start_after(&root, Activity::default(), program(), Duration::ZERO);
    eventually("l'attente du verrou", &vectoriser, || {
        vectoriser.status() == Status::Busy
    });
    assert_eq!(store.unvectorised_total().unwrap(), 3);
    drop(held);
    vectoriser.nudge();
    eventually("la passe une fois le verrou rendu", &vectoriser, || {
        store.unvectorised_total().unwrap() == 0
    });
    vectoriser.stop();
}

#[test]
fn a_disabled_launch_starts_nothing() {
    let (_dir, _store, root) = store(1, true);
    let vectoriser = Vectoriser::start_after(
        &root,
        Activity::default(),
        Launch::disabled(),
        Duration::ZERO,
    );
    assert_eq!(vectoriser.status(), Status::Disabled);
}

#[test]
fn a_backlog_past_a_round_is_caught_up_by_several_children_without_a_command() {
    // Jusqu'au 2026-10-19, au-delà de mille messages, la passe de fond refusait et demandait
    // `mail vectors`. Elle rattrape maintenant, un enfant par passage borné, et relance tant
    // qu'un passage avance. La borne est de deux ici, pour voir plusieurs passages sur cinq
    // messages.
    let (_dir, store, root) = store(5, true);
    let vectoriser =
        Vectoriser::start_with_round(&root, Activity::default(), program(), Duration::ZERO, 2);
    eventually("l'arriéré rattrapé", &vectoriser, || {
        store.unvectorised_total().unwrap() == 0 && vectoriser.status() == Status::Idle
    });
    // Le contrôle qui compte : un seul enfant aurait tout fait si la borne n'arrivait pas
    // jusqu'à lui.
    assert!(
        vectoriser.launched() >= 3,
        "{} enfants pour cinq messages par deux",
        vectoriser.launched()
    );
    vectoriser.stop();
}

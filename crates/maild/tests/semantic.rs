//! La recherche par le sens, de bout en bout : `search.query` sur l'API du démon, **le vrai binaire
//! `maild`** relancé en enfant pour encoder la requête, un vrai transformeur — minuscule — et un
//! vrai magasin de vecteurs.
//!
//! Ce qu'aucun test de `mailvec` ne peut dire : que l'enfant naît, répond dans le protocole, que
//! la fusion arrive au client avec ce que chaque résultat doit à chaque moteur, et que l'absence
//! du modèle se **dit** au lieu de se taire — le critère 7.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use mailapi::dto::{FoundBy, Hello, Results, SemanticState};
use maild::api::jsonrpc::Api;
use maild::vectors::Launch;
use mailvec::testing::{TINY_PIECES, corpus_with, tiny_bert};
use serde_json::Value;

/// Trois messages qui portent le mot cherché, deux qui ne le portent pas.
fn store(with_model: bool, with_vectors: bool) -> (tempfile::TempDir, Utf8PathBuf) {
    let messages: Vec<(String, String)> = [
        ("bonjour", "bon"),
        ("bonjour encore", "bon été"),
        ("bonjour enfin", "été"),
        ("rien", "été"),
        ("autre", "bon"),
    ]
    .iter()
    .map(|(subject, body)| ((*subject).to_owned(), (*body).to_owned()))
    .collect();
    let (dir, store) = corpus_with(&messages);
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    mailcore::index::rebuild(&store, &mailcore::Progress::new()).unwrap();
    if with_model {
        let model_dir = mailvec::bert::default_dir(&root);
        tiny_bert(&model_dir, TINY_PIECES);
        if with_vectors {
            let model = mailvec::bert::Bert::load(&model_dir).unwrap();
            mailvec::pass::update_with(
                &store,
                &model,
                mailvec::text::Budget::default(),
                &mailcore::Progress::new(),
            )
            .unwrap();
        }
    }
    (dir, root)
}

/// L'API avec la recherche par le sens, **sans** veille ni facteur : le corpus a un compte IMAP,
/// et un test n'a rien à faire sur le réseau.
fn api(root: &Utf8PathBuf, launch: &Launch) -> Api {
    let mailbox = Arc::new(Mutex::new(mailcore::Mailbox::open(root).unwrap()));
    let jobs = maild::jobs::Jobs::start(
        Arc::clone(&mailbox),
        root.clone(),
        maild::jobs::Sources::new(Vec::new()),
    );
    Api::new(mailbox, jobs).with_semantic(Arc::new(maild::semantic::Engine::start(root, launch)))
}

fn program() -> Launch {
    Launch::program(env!("CARGO_BIN_EXE_maild"))
}

async fn call(api: &Api, method: &str, params: Value) -> Value {
    let request =
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let text = api.handle_message(&request.to_string()).await.unwrap();
    serde_json::from_str(&text).unwrap()
}

async fn search(api: &Api, query: &str) -> Results {
    let response = call(api, "search.query", serde_json::json!({"query": query})).await;
    serde_json::from_value(response["result"].clone())
        .unwrap_or_else(|_| panic!("pas un résultat : {response}"))
}

#[tokio::test]
async fn the_words_and_the_meaning_reach_the_client_fused_and_attributed() {
    let (_dir, root) = store(true, true);
    let api = api(&root, &program());
    let results = search(&api, "bonjour").await;

    let semantic = results
        .semantic
        .expect("l'état de la recherche par le sens");
    assert_eq!(
        semantic.state,
        SemanticState::Ready,
        "{}",
        semantic.sentence
    );
    assert_eq!(semantic.gesture, None);
    // Cinq messages vectorisés : le sens les rend tous, les mots en rendent trois.
    assert_eq!(results.count, 5);
    let by = |found: FoundBy| {
        results
            .rows
            .iter()
            .filter(|row| row.found_by == Some(found))
            .count()
    };
    assert_eq!(by(FoundBy::Both), 3);
    assert_eq!(by(FoundBy::Meaning), 2);
    assert_eq!(by(FoundBy::Words), 0);
    for row in &results.rows {
        let words = row.subject.contains("bonjour");
        assert_eq!(
            row.found_by == Some(FoundBy::Both),
            words,
            "« {} » : attribué à tort",
            row.subject
        );
    }
    // Un message que les deux moteurs rendent passe devant un message que le sens seul rend : la
    // somme des rangs réciproques l'impose dès que les rangs sont proches, et ici ils le sont.
    assert_eq!(results.rows[0].found_by, Some(FoundBy::Both));
}

#[tokio::test]
async fn a_query_with_operators_is_answered_by_the_words_alone() {
    let (_dir, root) = store(true, true);
    let api = api(&root, &program());
    let results = search(&api, "subject:bonjour").await;
    assert_eq!(results.count, 3);
    assert!(
        results
            .rows
            .iter()
            .all(|row| row.found_by == Some(FoundBy::Words))
    );
    let semantic = results.semantic.unwrap();
    assert!(
        semantic.sentence.contains("opérateurs"),
        "{}",
        semantic.sentence
    );
}

#[tokio::test]
async fn a_query_the_full_text_refuses_stays_an_error() {
    let (_dir, root) = store(true, true);
    let api = api(&root, &program());
    let response = call(
        &api,
        "search.query",
        serde_json::json!({"query": "subject:("}),
    )
    .await;
    assert_eq!(
        response["error"]["code"],
        mailapi::jsonrpc::INVALID_PARAMS,
        "{response}"
    );
}

#[tokio::test]
async fn without_a_model_the_words_answer_and_the_client_is_told_how_to_get_the_meaning() {
    let (_dir, root) = store(false, false);
    let api = api(&root, &program());
    let results = search(&api, "bonjour").await;
    assert_eq!(results.count, 3, "les mots répondent seuls");
    assert!(
        results
            .rows
            .iter()
            .all(|row| row.found_by == Some(FoundBy::Words))
    );
    let semantic = results.semantic.unwrap();
    assert_eq!(semantic.state, SemanticState::ModelAbsent);
    assert_eq!(semantic.gesture.as_deref(), Some("mail model install"));
    assert!(
        !semantic.sentence.contains(root.as_str()),
        "un chemin a fui"
    );

    // `server.hello` le dit aussi, avant toute recherche.
    let hello: Hello =
        serde_json::from_value(call(&api, "server.hello", Value::Null).await["result"].clone())
            .unwrap();
    assert_eq!(hello.semantic.unwrap().state, SemanticState::ModelAbsent);
}

#[tokio::test]
async fn a_model_without_vectors_says_a_pass_is_needed() {
    let (_dir, root) = store(true, false);
    let api = api(&root, &program());
    let results = search(&api, "bonjour").await;
    assert_eq!(results.count, 3);
    let semantic = results.semantic.unwrap();
    assert_eq!(
        semantic.state,
        SemanticState::NeedsPass,
        "{}",
        semantic.sentence
    );
    assert_eq!(semantic.gesture.as_deref(), Some("mail vectors --bert"));
}

#[tokio::test]
async fn a_daemon_that_cannot_launch_itself_says_so() {
    let (_dir, root) = store(true, true);
    let api = api(&root, &Launch::disabled());
    let results = search(&api, "bonjour").await;
    assert_eq!(results.count, 3);
    assert_eq!(results.semantic.unwrap().state, SemanticState::Disabled);
}

#[tokio::test]
async fn an_api_without_the_engine_keeps_the_old_contract() {
    // Le contrôle inverse : sans moteur monté, ni champ `semantic` ni attribution — un client qui
    // parle à ce démon voit exactement ce qu'il voyait avant l'étape 8.
    let (_dir, root) = store(true, true);
    let mailbox = Arc::new(Mutex::new(mailcore::Mailbox::open(&root).unwrap()));
    let jobs = maild::jobs::Jobs::start(
        Arc::clone(&mailbox),
        root.clone(),
        maild::jobs::Sources::new(Vec::new()),
    );
    let api = Api::new(mailbox, jobs);
    let results = search(&api, "bonjour").await;
    assert_eq!(results.count, 3);
    assert!(results.semantic.is_none());
    assert!(results.rows.iter().all(|row| row.found_by.is_none()));
}

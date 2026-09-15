//! # maild
//!
//! Le démon. Il stocke, indexe et expose. Tout le reste — l'UI, un LLM, la CLI — n'est qu'un
//! client. Voir `docs/ARCHITECTURE.md`.
//!
//! ## Pourquoi c'est une bibliothèque, et pas seulement un binaire
//!
//! Le déploiement de référence met le démon sur une machine dédiée, et les clients parlent
//! HTTP. Mais **quelqu'un qui ne veut aucun serveur doit pouvoir tout avoir dans une seule
//! application**, sans port ouvert et sans jeton à gérer.
//!
//! Ce mode-là n'est possible que si le service est importable. La coquille Tauri consomme
//! donc [`api::jsonrpc::Api`] directement et appelle
//! [`api::jsonrpc::Api::handle_message`] en fonction, là où le mode réseau passe par
//! `axum`. **C'est le même code** dans les deux cas : le service n'a jamais connu son
//! transport, et c'est ce qui rend l'embarquement gratuit plutôt que dangereux.
//!
//! Réimplémenter le dispatch côté UI aurait été l'erreur : deux surfaces qui divergent, dont
//! une moins relue. Il n'y en a qu'une.
//!
//! ## Ce que l'embarquement change à la sécurité
//!
//! Il l'améliore. En mode réseau, `maild` écoute sur le bouclage : tout processus local peut
//! frapper à la porte, y compris un onglet de navigateur exécutant du JavaScript hostile —
//! la menace même qui impose le jeton sur le bouclage (`docs/PHASE-1.md`, critère 10).
//!
//! Embarqué, **rien n'écoute**. Le seul canal est l'IPC du webview que l'application a
//! elle-même créé. Il n'y a pas de jeton parce qu'il n'y a pas de canal à authentifier.
//!
//! La contrepartie est que la frontière de confiance devient « notre propre page ne doit pas
//! être compromise ». Ce n'était déjà pas différent : le jeton d'un client vit dans un
//! stockage que sa page peut lire, donc il ne protégeait pas d'une XSS. Ce qui protège est
//! ailleurs — l'`<iframe>` à origine opaque pour le corps des messages, et la CSP de
//! l'application (`docs/PRIVACY.md`).
//!
//! ## Les deux transports du mode réseau
//!
//! - **stdio** — le client tourne à côté du démon. Pas d'authentification : qui peut lancer
//!   le processus peut déjà lire le store. C'est le transport à garder fonctionnel en
//!   permanence, parce que c'est lui qui permet d'isoler un bug du transport d'un bug du
//!   cœur.
//! - **HTTP** — le client est ailleurs sur le réseau. Jeton porteur obligatoire, TLS
//!   obligatoire hors bouclage, **refus de démarrer sinon** (critère 10).
//!
//! Un serveur MCP joignable seulement en stdio n'est utilisable que depuis la machine du
//! démon, ce qui exclut le déploiement de référence de `docs/PHASE-1.md`.

#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod jobs;
pub mod outbox;
pub mod watch;

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use camino::Utf8Path;
use mailcore::Mailbox;

/// Tout ce qu'il faut pour servir une boîte, sans avoir choisi de transport.
///
/// C'est le point d'entrée du mode embarqué : ouvrir, puis appeler
/// [`api::jsonrpc::Api::handle_message`]. Le binaire s'en sert aussi, en montant un transport
/// par-dessus.
#[derive(Debug)]
pub struct Service {
    /// La boîte, partagée avec le serveur MCP et le fil des tâches de fond.
    pub mailbox: Arc<Mutex<Mailbox>>,
    /// L'API JSON-RPC, indépendante du transport.
    pub api: api::jsonrpc::Api,
    /// Le registre des tâches de fond.
    pub jobs: jobs::Jobs,
    /// Les veilleurs `IDLE`, un par compte IMAP actif.
    ///
    /// Ils ne sont là que pour être tenus en vie : tout ce qu'ils font passe par `jobs`.
    /// Les détruire les arrête.
    pub watchers: watch::Watchers,
    /// Le facteur : le fil qui vide la file d'envoi.
    ///
    /// Partagé avec l'API, qui le réveille quand un client met un message en file. Le
    /// détruire l'arrête.
    pub postman: Arc<outbox::Postman>,
}

impl Service {
    /// Ouvre un store et monte le service autour.
    ///
    /// `profiles` sont les profils qu'un client aura le droit de faire importer. Vide veut dire
    /// aucun : c'est un fail closed, et c'est le défaut — voir [`jobs::Sources`].
    ///
    /// # Errors
    ///
    /// Si le store est illisible.
    pub fn open(store: &Utf8Path, profiles: Vec<camino::Utf8PathBuf>) -> Result<Self> {
        let mailbox =
            Mailbox::open(store).with_context(|| format!("ouverture du store {store}"))?;
        let stats = mailbox.stats().context("lecture du store")?;
        let search = mailbox.search_available();

        tracing::info!(
            store = %store,
            messages = stats.messages,
            recherche = search,
            "boîte ouverte"
        );
        if !search {
            tracing::warn!("index plein texte absent : la recherche rendra une liste vide");
        }

        // Une seule boîte pour toutes les surfaces : ouvrir le store deux fois donnerait deux
        // `Searcher` et deux connexions SQLite pour lire les mêmes octets.
        let shared = Arc::new(Mutex::new(mailbox));

        let jobs = jobs::Jobs::start(
            Arc::clone(&shared),
            store.to_owned(),
            jobs::Sources::new(profiles),
        );
        let api = api::jsonrpc::Api::new(Arc::clone(&shared), jobs.clone());

        // La veille est montée **après** les jobs, parce qu'elle en dépend : un veilleur ne
        // synchronise pas, il met un job en file.
        let watchers = watch::Watchers::start(store, jobs.clone());

        // Le facteur est monté avec le reste, et **partagé avec l'API** : `outbox.send` écrit
        // la ligne puis le réveille. Sans ce partage, un envoi attendrait le tic suivant.
        let postman = Arc::new(outbox::Postman::start(store));
        let api = api.with_postman(Arc::clone(&postman));

        Ok(Self {
            mailbox: shared,
            api,
            jobs,
            watchers,
            postman,
        })
    }
}

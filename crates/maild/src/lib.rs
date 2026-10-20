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
pub mod attachments;
pub mod config;
pub mod jobs;
pub mod outbox;
pub mod panics;
pub mod semantic;
pub mod vectors;
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
    pub watchers: Arc<watch::Watchers>,
    /// Le facteur : le fil qui vide la file d'envoi.
    ///
    /// Partagé avec l'API, qui le réveille quand un client met un message en file. Le
    /// détruire l'arrête.
    pub postman: Arc<outbox::Postman>,
    /// La passe de fond des vecteurs. Partagée avec l'API, qui la garde en vie — voir
    /// [`vectors`].
    pub vectoriser: Arc<vectors::Vectoriser>,
    /// La recherche par le sens, partagée avec l'API pour la même raison. L'enfant qui la sert
    /// ne naît qu'à la première requête.
    pub semantic: Arc<semantic::Engine>,
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
    pub fn open(
        store: &Utf8Path,
        profiles: Vec<camino::Utf8PathBuf>,
        launch: vectors::Launch,
    ) -> Result<Self> {
        let mailbox =
            Mailbox::open(store).with_context(|| format!("ouverture du store {store}"))?;
        let search = mailbox.search_available();

        // **Pas de compte de messages ici.** Il y en avait un, pour cette seule ligne de journal :
        // 20 ms sur 73 658 messages, payées **avant la fenêtre** de la coquille embarquée, donc
        // sur le seuil de démarrage de 225 ms — mesuré le 2026-10-09, il faisait passer la
        // coquille de 220 à 240 ms. `server.hello` rend le même compte, sur le fil du service,
        // une fois la fenêtre ouverte.
        tracing::info!(store = %store, recherche = search, "boîte ouverte");
        if !search {
            tracing::warn!("index plein texte absent : la recherche rendra une liste vide");
        }

        // Une seule boîte pour toutes les surfaces : ouvrir le store deux fois donnerait deux
        // `Searcher` et deux connexions SQLite pour lire les mêmes octets.
        let shared = Arc::new(Mutex::new(mailbox));

        // Le compteur d'activité est créé **avant** les jobs : la passe de fond des vecteurs s'y
        // abonne, et aucune fin de job ne doit lui échapper.
        let activity = jobs::Activity::default();
        let jobs = jobs::Jobs::start_with(
            Arc::clone(&shared),
            store.to_owned(),
            jobs::Sources::new(profiles),
            activity.clone(),
        );
        let api = api::jsonrpc::Api::new(Arc::clone(&shared), jobs.clone());

        // La veille est montée **après** les jobs, parce qu'elle en dépend : un veilleur ne
        // synchronise pas, il met un job en file.
        let watchers = Arc::new(watch::Watchers::start(store, jobs.clone()));

        // Le facteur est monté avec le reste, et **partagé avec l'API** : `outbox.send` écrit
        // la ligne puis le réveille. Sans ce partage, un envoi attendrait le tic suivant.
        let postman = Arc::new(outbox::Postman::start(store));
        // Les veilleurs aussi, depuis le 2026-10-08 : la coquille embarquée ne garde que l'API et
        // la boîte, et des veilleurs tenus seulement par `Service` mouraient à son démarrage —
        // le courrier n'y arrivait que sur une moisson demandée. Une relecture l'a trouvé.
        let api = api
            .with_postman(Arc::clone(&postman))
            .with_watchers(Arc::clone(&watchers));

        // La passe de fond des vecteurs, **accrochée à l'API** comme le facteur : la coquille
        // embarquée ne garde que l'API et la boîte, et un fil tenu ailleurs mourrait au démarrage
        // — c'est ce qui arrive à la veille `IDLE` dans la coquille.
        // Le même exécutable sert la recherche par le sens, dans un autre enfant : monté avant que
        // `launch` ne parte à la passe de fond.
        let semantic = Arc::new(semantic::Engine::start(store, &launch, activity.clone()));
        let vectoriser = Arc::new(vectors::Vectoriser::start(store, activity, launch));
        // Quand l'enfant de la recherche est chaud, c'est lui qui fait la passe : il a déjà le
        // modèle. Voir `Vectoriser::delegate_to`.
        vectoriser.delegate_to(&semantic);
        // Le texte des PDF lus doit entrer dans l'index plein texte, et seul un job y écrit : la
        // passe de fond en demande un, que les jobs coalescent — voir `jobs::Jobs::follow`.
        let follow_jobs = jobs.clone();
        vectoriser.on_follow(move || {
            let _ = follow_jobs.follow();
        });
        let api = api
            .with_vectoriser(Arc::clone(&vectoriser))
            .with_semantic(Arc::clone(&semantic));

        Ok(Self {
            mailbox: shared,
            api,
            jobs,
            watchers,
            postman,
            vectoriser,
            semantic,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::Service;

    #[test]
    fn what_an_embedding_shell_keeps_keeps_the_background_alive() {
        // La coquille embarquée ne garde que l'API et la boîte du service, et détruit le reste.
        // Les veilleurs `IDLE` mouraient ainsi au démarrage, et le courrier n'arrivait plus que sur
        // une moisson demandée. Le défaut, reproduit ici : ne garder que l'API.
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap();
        let service = Service::open(root, Vec::new(), crate::vectors::Launch::disabled()).unwrap();
        let api = service.api.clone();
        drop(service);
        assert!(
            api.watchers_running(),
            "les veilleurs sont morts avec le service"
        );
        assert!(
            api.vectors_status().is_some(),
            "la passe de fond est morte avec le service"
        );
    }
}

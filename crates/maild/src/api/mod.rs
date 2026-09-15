//! Les deux surfaces du démon, et les transports qui les portent.
//!
//! | Module | Ce qu'il sert |
//! |---|---|
//! | [`jsonrpc`] | l'API des clients non-MCP : UI, CLI. Contrat dans le crate `mailapi`. |
//! | [`stdio`] | un protocole sur l'entrée et la sortie standard, client sur cette machine |
//! | [`http`] | les **deux** protocoles, sur `/mcp` et `/api`, client sur le réseau |
//! | [`auth`] | le jeton porteur : ce que la règle dit, et pourquoi |
//! | [`transport`] | les choix de transport et ce qu'ils impliquent |
//!
//! Les outils MCP eux-mêmes vivent dans le crate `mailmcp`, qui ne connaît aucun transport.
//!
//! ## Un protocole, deux publics
//!
//! JSON-RPC 2.0 dans les deux cas — une seule couche de sérialisation à maintenir
//! (`docs/ARCHITECTURE.md`). Ce qui diffère est la forme des données, pas l'encadrement : un
//! modèle de langage veut des dates lisibles et pas de drapeaux, une interface veut des
//! secondes Unix, des curseurs de pagination et l'état lu/non-lu. Voir `mailapi::dto` pour
//! le détail de cet arbitrage.
//!
//! ## Une seule règle d'authentification
//!
//! Le démon ne sert du mail sans authentification que sur une interface qu'il est seul à
//! pouvoir atteindre.
//!
//! | Transport | Authentification | Chiffrement |
//! |---|---|---|
//! | stdio | permissions du système de fichiers | sans objet |
//! | TCP | jeton porteur, obligatoire | TLS, obligatoire hors bouclage |
//!
//! Elle s'applique aux deux protocoles par la **même** couche `axum` : deux contrôles
//! séparés finiraient par divergrer, et l'un des deux serait le trou.
//!
//! `keyring` reste retenu et épinglé dans le manifeste du workspace sans être tiré ici :
//! c'est un besoin du **client** — stocker le jeton dans le trousseau du poste de travail —
//! et il arrivera avec le client, à l'étape 8.

pub mod auth;
pub mod http;
pub mod jsonrpc;
pub mod stdio;
pub mod transport;

//! # mailapi
//!
//! Le contrat entre le démon et ses clients qui ne parlent pas MCP : l'UI, la CLI, et tout
//! ce qui viendra. **Ce crate est le contrat, pas le serveur** — il ne connaît aucun
//! transport et n'ouvre aucune socket. `maild` le monte sur stdio et sur HTTP
//! (`docs/ARCHITECTURE.md`).
//!
//! ## Pourquoi un crate à part de `mailmcp`
//!
//! Les deux exposent le même cœur, à deux publics qui ne veulent pas les mêmes données —
//! voir le tableau de [`dto`]. Les fusionner ferait payer à un client chaque changement
//! demandé par l'autre. Le protocole, en revanche, est le même : JSON-RPC 2.0, une seule
//! couche de sérialisation à maintenir.
//!
//! Ce crate est aussi la dépendance que les clients tirent : les mêmes types servent à
//! écrire les réponses côté démon et à les relire côté UI, ce qui fait qu'un champ renommé
//! casse à la compilation plutôt qu'à l'exécution.
//!
//! ## Lecture seule, sauf les tâches de fond
//!
//! **Aucune méthode ne touche au courrier.** Répondre, envoyer, supprimer : phase 3, et
//! toujours derrière une confirmation explicite de l'utilisateur.
//!
//! Une exception s'est ajoutée et il faut la nommer plutôt que de la laisser passer :
//! `jobs.start` **écrit**, parce qu'importer et indexer sont des écritures. Sans elle, la
//! première action de tout utilisateur — importer son profil — exigerait un shell sur la
//! machine du démon, ce qui rendrait le déploiement de référence inutilisable.
//!
//! La surface nouvelle est bornée par construction :
//!
//! - un client ne nomme **jamais** un chemin, il choisit un rang dans `jobs.sources` ;
//! - cette liste est déclarée par l'opérateur au démarrage du démon, et **vide par
//!   défaut** — sans `--profile`, aucun import n'est possible ;
//! - une tâche ne peut ni supprimer ni modifier un message : elle ajoute des contenus
//!   adressés par leur hash, reconstruit un index dérivé, ou reconstruit des fils dérivés.
//!
//! Sans le premier point, quiconque détient le jeton pourrait faire lire n'importe quel
//! fichier de la machine du démon, le ranger dans le store, puis le relire par
//! `search.query` — une lecture de fichiers arbitraires déguisée en fonctionnalité d'import.
//!
//! ## Le point d'entrée
//!
//! ```ignore
//! let response = match mailapi::jsonrpc::parse(line) {
//!     Ok(request) => match mailapi::call(&mailbox, &request.method, request.params) {
//!         Ok(result) => Response::success(id, result),
//!         Err(error) => Response::failure(id, error),
//!     },
//!     Err(error) => Response::failure(Value::Null, error),
//! };
//! ```

#![forbid(unsafe_code)]

pub mod client;
pub mod dispatch;
pub mod dto;
pub mod human;
pub mod jsonrpc;

#[cfg(feature = "token-store")]
pub mod token;

pub use dispatch::{call, method};

/// La version du contrat exposé par ce crate.
///
/// Rendue par `server.hello` pour qu'un client sache s'il peut parler à ce démon **avant**
/// d'interpréter des réponses. Elle augmente à chaque changement incompatible — un champ
/// retiré, un type modifié, une sémantique déplacée. Ajouter un champ optionnel ou une
/// méthode ne la change pas : un ancien client continue de fonctionner.
pub const PROTOCOL: u32 = 1;

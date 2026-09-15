//! Les sous-commandes.
//!
//! Seul endroit du projet où écrire sur la sortie standard est légitime : c'est une sortie
//! destinée à l'utilisateur. Partout ailleurs, `tracing`.

pub mod account;
pub mod contacts;
pub mod doctor;
pub mod import;
pub mod index;
pub mod probe;
pub mod remote;
pub mod search;
pub mod send;
pub mod source;
pub mod stats;
pub mod sync;
pub mod thread;

//! # mailmcp
//!
//! Serveur MCP exposant `mailcore`. **Le premier frontend, avant l'UI.**
//!
//! Dès qu'il répond, le corpus réel est interrogeable depuis Claude Code : recherche,
//! lecture de fil, historique d'un contact. Ça valide le store, l'index et le modèle de
//! données sans avoir écrit une ligne d'interface — et si le cœur est mauvais, on le sait à
//! ce moment-là plutôt qu'après trois semaines de front (`docs/PHASE-1.md`).
//!
//! **Lecture seule en phase 1.** L'écriture — répondre, envoyer — arrive en phase 3 et
//! devra passer par une confirmation explicite de l'utilisateur, jamais par une décision du
//! modèle.
//!
//! Ce crate ne connaît aucun transport : `maild` monte stdio ou HTTP autour de
//! [`MailServer`].

#![forbid(unsafe_code)]

pub mod dto;
pub mod server;

pub use server::MailServer;

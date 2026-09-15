//! # mailhtml
//!
//! Les barrières de `docs/PRIVACY.md`, réunies dans un seul crate.
//!
//! Elles sont ici et pas ailleurs parce qu'elles servent deux appelants qui ne se
//! connaissent pas : `mailcore` compte les traceurs **à l'indexation**, le front
//! assainit **avant le rendu**. Deux copies de la même liste blanche divergeraient, et
//! `docs/PRIVACY.md` demande précisément deux barrières *indépendantes*, pas deux copies
//! d'une même barrière approximative.
//!
//! Ordre des barrières, du plus fiable au moins fiable :
//!
//! 1. [`csp`] — appliquée par le moteur de rendu. Rien ne sort, même si le code
//!    applicatif est buggé.
//! 2. [`sanitize`] — appliquée par nous. Deuxième ceinture, parce qu'une CSP mal formée
//!    ne doit pas être un point de défaillance unique.

#![forbid(unsafe_code)]

pub mod blocks;
pub mod csp;
pub mod rich;
pub mod sanitize;
pub mod text;
pub mod trackers;

pub use csp::{MESSAGE_CSP, MESSAGE_SANDBOX};
pub use sanitize::{Cleaned, Policy, clean};
pub use trackers::{Report as TrackerReport, Tracker};

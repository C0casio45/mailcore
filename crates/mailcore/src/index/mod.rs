//! Index plein texte tantivy.
//!
//! Champs indexés : `subject`, `body_text`, `from`, `to`, `folder`, `date`. On stocke les
//! postings, pas le contenu — le corps se relit depuis le blob. Objectif : p95 sous
//! 50 ms (critère 4 de `docs/PHASE-1.md`).
//!
//! Phase 4 : un index vectoriel à côté, un embedding par message.

pub mod coverage;
mod directory;
pub mod schema;
pub mod searcher;
pub mod writer;

pub use coverage::Coverage;
pub use schema::Fields;
pub use searcher::{Hit, Searcher};
pub use writer::{IndexStats, open_or_create, open_or_create_in, rebuild, update};

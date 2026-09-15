//! # mailimport
//!
//! Import des mbox du profil Thunderbird vers le store de `mailcore`.
//!
//! ## La règle qui domine tout le reste
//!
//! **Le profil Thunderbird est en production.** L'utilisateur s'en sert tous les jours en
//! parallèle. On lit, on ne touche à rien : pas de `rename`, pas de troncature, pas de
//! fichier temporaire déposé dedans, pas de compactage. Chaque ouverture est explicitement
//! en lecture seule. Le critère 7 de `docs/PHASE-1.md` exige **exactement zéro écriture**,
//! vérifié par relevé des `mtime` avant et après un import complet
//! (`cargo xtask profile-snapshot`).
//!
//! ## La deuxième règle
//!
//! Rien ne charge un mbox entier en mémoire. Le plus gros fait 1,44 Go. Tout est streamé,
//! y compris à l'import et à l'indexation — critère 3 : 500 Mo de RSS maximum.

#![forbid(unsafe_code)]

pub mod error;
pub mod import;
pub mod mbox;
pub mod mozilla;
pub mod prefs;
pub mod tree;
pub mod utf7;

pub use error::{Error, Result};

//! # mailvec
//!
//! Le modèle d'embeddings **local** et la passe qui vectorise le corpus — étape 5 de
//! `docs/PHASE-4.md`.
//!
//! ## La ligne de partage avec `mailcore`
//!
//! `mailcore::vectors` **range** les vecteurs et les compare : un fichier plat, un balayage, et
//! aucune dépendance. Ce crate-ci **fabrique** les vecteurs, et pour ça il lui faut un modèle,
//! un tokeniseur et de quoi lire un corps de message.
//!
//! La coupure est là parce que la recherche doit pouvoir lire le magasin sans rien savoir du
//! modèle : `mail search` et la coquille ouvrent `vectors.bin`, pas `potion`. Et à l'inverse,
//! trois binaires sur quatre ne vectorisent rien — leur faire compiler `tokenizers` serait les
//! faire payer pour un travail qu'ils ne demandent pas.
//!
//! ## Ce que la vie privée exige de ce crate en particulier
//!
//! Il traite **l'intégralité** du courrier, sans qu'un humain valide message par message. C'est
//! la passe la plus exposée du projet, et deux choses en découlent, écrites plutôt que crues :
//!
//! - **aucun client HTTP n'est compilé ici.** `model2vec-rs` est pris en variante `local-only`,
//!   qui n'embarque pas de quoi parler à un dépôt de modèles. « Zéro requête réseau » y est vrai
//!   par construction, comme dans `mailcal` — voir le manifeste ;
//! - **le modèle se charge depuis un répertoire**, jamais depuis un identifiant à résoudre. Un
//!   modèle absent est une erreur qui se lit, pas un téléchargement qui se déclenche.

#![forbid(unsafe_code)]

pub mod bert;
pub mod model;
pub mod pass;
pub mod search;
pub mod text;

#[cfg(test)]
mod testing;

/// Ce qui peut échouer dans ce crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Le répertoire du modèle n'existe pas, ou ne contient pas ce qu'il faut.
    ///
    /// Une erreur et non un téléchargement : le critère 6 n'accorde qu'une requête réseau au
    /// projet, celle de l'installation, et elle part d'une commande tapée par un humain.
    #[error("modèle absent ou illisible ({path}) : {reason}")]
    Model {
        /// Le répertoire visé.
        path: camino::Utf8PathBuf,
        /// Ce qui manque, en clair.
        reason: String,
    },

    /// L'encodage d'un texte a échoué.
    #[error("encodage impossible : {0}")]
    Encode(String),

    /// Une erreur venue du cœur : store illisible, blob absent, magasin refusé.
    #[error(transparent)]
    Core(#[from] mailcore::Error),
}

/// Le résultat de ce crate.
pub type Result<T> = std::result::Result<T, Error>;

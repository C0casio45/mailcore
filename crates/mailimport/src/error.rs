//! Le type d'erreur de l'import.

use camino::Utf8PathBuf;

/// Alias de commodité.
pub type Result<T> = std::result::Result<T, Error>;

/// Toute erreur remontée par `mailimport`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Échec d'entrée/sortie.
    #[error("erreur d'entrée/sortie sur {path}")]
    Io {
        /// Le fichier concerné.
        path: Utf8PathBuf,
        /// La cause système.
        #[source]
        source: std::io::Error,
    },

    /// Échec d'entrée/sortie pendant la lecture d'un flux, sans chemin connu.
    #[error("erreur de lecture du flux mbox")]
    Stream(#[source] std::io::Error),

    /// Un « message » a dépassé la taille plafond.
    ///
    /// En pratique : un fichier sans séparateur, ou un séparateur manqué. Sans ce
    /// plafond, le tampon grossirait jusqu'à la taille du fichier — 1,44 Go pour le plus
    /// gros du corpus — et ferait tomber le critère 3.
    #[error("message de plus de {limit} octets à l'offset {offset} : séparateur manquant ?")]
    MessageTooLarge {
        /// Offset du séparateur qui a ouvert ce message.
        offset: u64,
        /// Le plafond franchi.
        limit: usize,
    },

    /// Échec côté store.
    #[error(transparent)]
    Core(#[from] mailcore::Error),
}

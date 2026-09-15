//! Les erreurs de la moisson.
//!
//! ## Pourquoi elles distinguent autant de cas
//!
//! Une synchronisation échoue de dix façons, et **la conduite à tenir n'est pas la même** :
//! un identifiant refusé demande une action de l'utilisateur, une coupure réseau demande de
//! réessayer plus tard, un serveur hors spécification demande un rapport de bug. Un seul
//! `SyncFailed(String)` obligerait l'appelant à relire un message pour décider, ce qui est la
//! définition d'une erreur mal typée.

use camino::Utf8PathBuf;

/// Ce qui peut mal se passer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Le réseau, ou le socket.
    ///
    /// **Réessayable.** C'est le câble débranché, le serveur qui raccroche, le délai dépassé.
    #[error("erreur réseau : {0}")]
    Network(#[from] std::io::Error),

    /// Le nom d'hôte du compte n'est pas un nom valide pour TLS.
    #[error("nom d'hôte invalide pour TLS : {host}")]
    InvalidHost {
        /// L'hôte refusé.
        host: String,
    },

    /// TLS n'a pas pu être établi.
    ///
    /// **Jamais contourné.** Un échec de vérification de certificat est un échec de
    /// synchronisation, pas un avertissement : accepter un certificat non vérifié rendrait
    /// tout le reste sans objet.
    #[error("échec TLS : {reason}")]
    Tls {
        /// Ce que la couche TLS a refusé.
        reason: String,
    },

    /// Le serveur a refusé l'authentification.
    ///
    /// **Pas réessayable sans intervention.** Un mot de passe applicatif révoqué, un
    /// consentement OAuth2 expiré. Le message du serveur est conservé parce qu'il est la seule
    /// indication utile — et il ne contient pas de secret, seulement un refus.
    #[error("authentification refusée : {reason}")]
    AuthRefused {
        /// Ce que le serveur a répondu.
        reason: String,
    },

    /// Le serveur a refusé une commande.
    #[error("le serveur a refusé {command} : {reason}")]
    Refused {
        /// La commande refusée.
        command: String,
        /// Le texte du refus.
        reason: String,
    },

    /// La réponse du serveur ne suit pas la RFC.
    ///
    /// Le cas que `mailfake` provoque exprès : un littéral tronqué, une longueur qui ne
    /// correspond pas, une réponse `FETCH` sans UID.
    #[error("réponse hors spécification : {reason}")]
    Malformed {
        /// Ce qui n'allait pas.
        reason: String,
    },

    /// Un littéral annoncé dépasse le plafond.
    ///
    /// **C'est une défense, pas une limite fonctionnelle.** `{4294967295}` d'un serveur
    /// compromis ne doit pas faire allouer quatre gigaoctets avant qu'on s'aperçoive que rien
    /// ne suit. Voir [`crate::MESSAGE_LIMIT`].
    #[error("littéral de {announced} octets annoncé, plafond {limit}")]
    LiteralTooLarge {
        /// La taille annoncée.
        announced: u64,
        /// Le plafond.
        limit: usize,
    },

    /// Le serveur n'a pas annoncé une capacité indispensable.
    #[error("capacité absente : {capability}")]
    MissingCapability {
        /// La capacité qu'on attendait.
        capability: String,
    },

    /// Le compte n'a pas de serveur, ou n'est pas un compte IMAP.
    #[error("le compte {account} n'est pas synchronisable : {reason}")]
    NotSyncable {
        /// L'identifiant du compte.
        account: i64,
        /// Pourquoi.
        reason: String,
    },

    /// Le store a refusé une écriture.
    #[error("le store a refusé : {0}")]
    Store(#[from] mailcore::Error),

    /// Une lecture ou une écriture de fichier.
    #[error("{path} : {source}")]
    Io {
        /// Le chemin fautif.
        path: Utf8PathBuf,
        /// La cause.
        source: std::io::Error,
    },
}

impl Error {
    /// Vrai si réessayer plus tard a une chance de marcher.
    ///
    /// ## À quoi ça sert d'y répondre
    ///
    /// Une synchronisation périodique doit distinguer « le réseau était coupé » de « le mot de
    /// passe est faux ». Le premier se réessaie en silence ; le second doit remonter à
    /// l'utilisateur **une fois** et ne plus être retenté, sinon on tape sur un serveur avec
    /// un mauvais mot de passe jusqu'à se faire bloquer le compte.
    ///
    /// ## Le store n'est pas d'un seul bloc
    ///
    /// Un `database is locked` est l'erreur réessayable par excellence : quelqu'un d'autre
    /// écrivait, et il a fini depuis. La ranger avec les refus définitifs faisait abandonner un
    /// compte pour une contention qui n'existait plus une seconde plus tard.
    ///
    /// Trouvé le 2026-09-09 par le banc du critère 4, avec cinq moissons concurrentes sur le
    /// même store : l'une échouait après les 5 s de `busy_timeout`, et son échec était classé
    /// « ne pas réessayer ». Un schéma cassé ou une contrainte violée, eux, restent définitifs.
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Network(_) | Self::Io { .. } | Self::Tls { .. } => true,
            Self::Store(source) => source.is_locked(),
            Self::AuthRefused { .. }
            | Self::Refused { .. }
            | Self::Malformed { .. }
            | Self::LiteralTooLarge { .. }
            | Self::MissingCapability { .. }
            | Self::NotSyncable { .. }
            | Self::InvalidHost { .. } => false,
        }
    }
}

/// Le résultat de la moisson.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_cable_is_retryable_but_a_bad_password_is_not() {
        // Réessayer sur un mauvais mot de passe fait bloquer le compte chez le fournisseur.
        let cut = Error::Network(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert!(cut.retryable());

        let refused = Error::AuthRefused {
            reason: "NO".to_owned(),
        };
        assert!(!refused.retryable());
    }

    #[test]
    fn a_malformed_response_is_not_retryable() {
        // Un serveur hors spécification le restera à la prochaine tentative : réessayer
        // n'est pas une correction, c'est une boucle.
        let error = Error::Malformed {
            reason: "littéral tronqué".to_owned(),
        };
        assert!(!error.retryable());
    }
}

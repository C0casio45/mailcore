//! Le secret d'un compte, dans le trousseau du système. **Jamais ailleurs.**
//!
//! `docs/PRIVACY.md` §7 et le critère 6 de `docs/PHASE-2.md` : zéro identifiant en clair dans
//! le store, dans les journaux, dans les fichiers temporaires. Le schéma de `mailcore` n'a
//! aucune colonne où en ranger un ; ce crate est l'endroit où il va à la place.
//!
//! ## Pourquoi pas une variable d'environnement
//!
//! La même raison que pour le jeton du démon, et elle vaut d'autant plus pour un mot de passe
//! de messagerie : une variable d'environnement se retrouve dans l'historique du shell, dans
//! la table des processus lisible par les autres utilisateurs de la machine, et recopiée dans
//! chaque processus fils.
//!
//! ## La clé est `(hôte, identifiant)`, pas l'identifiant du compte
//!
//! Un `AccountId` est un `rowid` SQLite. Il change si le store est reconstruit — et le store
//! **est** reconstructible, c'est même une propriété du projet : tout se réimporte depuis les
//! blobs. Un secret rangé sous un `rowid` deviendrait orphelin au premier store neuf, et
//! l'utilisateur devrait retaper ses dix mots de passe sans comprendre pourquoi.
//!
//! `(hôte, identifiant)` est ce qui identifie réellement un compte : c'est déjà la clé
//! d'idempotence de `Writer::upsert_imap_account`.
//!
//! ## Ce que ce crate ne fait pas
//!
//! Il ne **lit** aucun secret pour le journaliser, ne l'affiche jamais, et n'a pas de fonction
//! qui rende « tous les secrets ». Un inventaire des secrets est une fonctionnalité qui n'a
//! qu'un usage : les exfiltrer d'un coup.

#![forbid(unsafe_code)]

pub mod consent;
pub mod http;
pub mod oauth;
pub mod session;

use keyring::Entry;

/// Le nom sous lequel les entrées apparaissent dans le trousseau.
///
/// Le même que celui du jeton du démon : l'utilisateur qui ouvre son Credential Manager voit
/// tout ce que mailcore garde au même endroit, ce qui est une propriété de transparence.
pub(crate) const SERVICE: &str = "mailcore";

/// Le préfixe des entrées de compte.
///
/// Distinct de celui du jeton du démon, dont la clé est une adresse. Sans préfixe, un démon
/// qui s'appellerait `marie@exemple.fr` — improbable mais pas impossible — écraserait un
/// secret de compte.
const PREFIX: &str = "imap";

/// Ce qui peut mal se passer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Aucun secret enregistré pour ce compte.
    #[error("aucun secret enregistré pour {username} sur {host} — utiliser `mail account add`")]
    NotFound {
        /// L'hôte cherché.
        host: String,
        /// L'identifiant cherché.
        username: String,
    },

    /// Le trousseau du système est inutilisable.
    ///
    /// Arrive sur une session Linux sans Secret Service, ou dans un conteneur. Le message le
    /// dit plutôt que de laisser croire à un secret manquant : les deux se corrigent
    /// autrement.
    #[error("trousseau du système inutilisable : {0}")]
    Unavailable(String),

    /// Le secret proposé est vide.
    ///
    /// Refusé à l'écriture, parce qu'un secret vide **réussit** l'enregistrement et échoue à
    /// l'authentification. L'utilisateur croirait avoir configuré son compte, et le
    /// diagnostic arriverait une synchronisation plus tard, sous la forme d'un refus du
    /// serveur.
    #[error("secret vide refusé")]
    Empty,

    /// Le réseau, ou le socket.
    #[error("erreur réseau : {0}")]
    Network(#[source] std::io::Error),

    /// Le nom d'hôte du point de terminaison n'est pas valide pour TLS.
    #[error("nom d'hôte invalide pour TLS : {host}")]
    InvalidHost {
        /// L'hôte refusé.
        host: String,
    },

    /// TLS n'a pas pu être établi.
    ///
    /// **Jamais contourné**, pour la même raison que dans `mailsync` : un échec de
    /// vérification de certificat sur le chemin qui transporte un jeton de rafraîchissement
    /// est un échec, pas un avertissement.
    #[error("échec TLS : {reason}")]
    Tls {
        /// Ce que la couche TLS a refusé.
        reason: String,
    },

    /// La réponse du serveur ne suit pas HTTP/1.1, ou dépasse un plafond.
    #[error("réponse hors protocole : {reason}")]
    Protocol {
        /// Ce qui n'allait pas. **Jamais un fragment de corps** : voir `http`.
        reason: String,
    },

    /// Le système ne rend pas d'aléa.
    ///
    /// **Refusé, jamais remplacé par autre chose.** Un vérificateur PKCE prévisible désarme
    /// la protection sans que rien ne le signale, et un état anti-rejeu prévisible se devine.
    #[error("aléa indisponible : {reason}")]
    Entropy {
        /// Ce que le système a dit.
        reason: String,
    },

    /// Le consentement est mort : il faut que l'utilisateur le redonne.
    ///
    /// Une classe à part, et pas un raffinement : réessayer ne le ressuscite pas. C'est ce que
    /// rend `invalid_grant` — jeton révoqué, mot de passe changé, ou périmé par les 7 jours du
    /// mode « test » de Google.
    #[error("consentement à redonner : {reason}")]
    Consent {
        /// Ce que le fournisseur a dit. Aucun secret : c'est un message d'erreur.
        reason: String,
    },

    /// Mécanisme d'authentification inconnu.
    ///
    /// **Aucun repli**, pour la raison qui vaut déjà dans `mailcore::AuthKind::parse` : se
    /// tromper de mécanisme, c'est envoyer un secret dans un champ qui ne l'attend pas — un
    /// jeton porteur placé dans un `LOGIN` part dans le champ mot de passe, et un serveur
    /// journalise les échecs de `LOGIN`.
    #[error("mécanisme d'authentification inconnu : {found}")]
    UnknownMechanism {
        /// L'étiquette refusée.
        found: String,
    },

    /// Le fournisseur a refusé pour une autre raison.
    #[error("le fournisseur a refusé ({code}) : {description}")]
    Provider {
        /// Le code d'erreur OAuth2 : `invalid_client`, `invalid_scope`…
        code: String,
        /// La description du fournisseur.
        description: String,
    },
}

impl Error {
    /// Vrai si réessayer plus tard a une chance de marcher.
    ///
    /// ## À quoi ça sert d'y répondre
    ///
    /// Une synchronisation périodique doit distinguer « le réseau était coupé » de « le
    /// consentement est mort ». Le premier se réessaie en silence ; le second doit remonter à
    /// l'utilisateur **une fois** et ne plus être retenté — insister sur un jeton révoqué fait
    /// bloquer le client chez certains fournisseurs.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        match self {
            Self::Network(_) | Self::Unavailable(_) | Self::Tls { .. } => true,
            Self::NotFound { .. }
            | Self::Empty
            | Self::InvalidHost { .. }
            | Self::Protocol { .. }
            | Self::Entropy { .. }
            | Self::Consent { .. }
            | Self::Provider { .. }
            | Self::UnknownMechanism { .. } => false,
        }
    }
}

/// Alias de commodité.
pub type Result<T> = std::result::Result<T, Error>;

/// Le verrou qui sérialise les épreuves de trousseau. **Tests seulement.**
///
/// ## Pourquoi il existe, et ce qu'il ne prétend pas corriger
///
/// Le Credential Manager de Windows ne se comporte pas de façon fiable sous accès concurrent :
/// mesuré le 2026-09-04, une exécution des 100 tests de ce crate en parallèle laissait
/// systématiquement deux à six entrées jetables derrière elle, et la **même** exécution avec
/// `--test-threads=1` n'en laissait aucune. Une suppression émise pendant qu'un autre fil écrit
/// se perd.
///
/// Ce n'est pas un défaut du code testé : rien dans mailcore n'écrit vingt secrets à la fois.
/// C'est un défaut de la suite de tests, qui salissait la machine de son utilisateur — le même
/// symptôme que les 34 entrées oubliées du 2026-09-03, mais par une cause différente, et que
/// les gardes `Drop` ne pouvaient pas attraper puisqu'ils s'exécutaient bel et bien.
///
/// Les deux modules de test du crate partagent ce verrou : le sérialiser par module laisserait
/// `session` et `lib` se marcher dessus.
#[cfg(test)]
pub(crate) static KEYRING_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Prend le verrou, en ignorant l'empoisonnement.
///
/// Un test qui panique en le tenant l'empoisonne. Refuser de le prendre ensuite ferait échouer
/// tous les tests suivants pour une raison qui n'est pas la leur, et masquerait la panique
/// d'origine derrière quatre-vingt-dix-neuf échecs.
#[cfg(test)]
pub(crate) fn keyring_lock() -> std::sync::MutexGuard<'static, ()> {
    KEYRING_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Enregistre le secret d'un compte.
///
/// # Errors
///
/// [`Error::Empty`] si le secret est vide, [`Error::Unavailable`] si le trousseau ne répond
/// pas.
pub fn store(host: &str, username: &str, secret: &str) -> Result<()> {
    if secret.is_empty() {
        return Err(Error::Empty);
    }
    entry(host, username)?
        .set_password(secret)
        .map_err(|source| Error::Unavailable(source.to_string()))?;
    // L'hôte et l'identifiant sont journalisés, le secret jamais. Savoir *qu'un* secret a été
    // enregistré et pour qui est ce qu'il faut pour diagnostiquer ; sa valeur ne l'est pas.
    tracing::info!(host, username, "secret enregistré dans le trousseau");
    Ok(())
}

/// Relit le secret d'un compte.
///
/// # Errors
///
/// [`Error::NotFound`] si rien n'est enregistré, [`Error::Unavailable`] si le trousseau ne
/// répond pas.
pub fn load(host: &str, username: &str) -> Result<String> {
    match entry(host, username)?.get_password() {
        Ok(secret) => Ok(secret),
        Err(keyring::Error::NoEntry) => Err(Error::NotFound {
            host: host.to_owned(),
            username: username.to_owned(),
        }),
        Err(source) => Err(Error::Unavailable(source.to_string())),
    }
}

/// Vrai si un secret est enregistré.
///
/// Ne rend **pas** le secret : sert à afficher l'état d'un compte sans le lire. Un trousseau
/// indisponible rend `false` plutôt qu'une erreur — pour un affichage, « on ne sait pas » et
/// « il n'y en a pas » se présentent pareil, et l'appel qui compte échouera clairement.
#[must_use]
pub fn is_stored(host: &str, username: &str) -> bool {
    load(host, username).is_ok()
}

/// Oublie le secret d'un compte.
///
/// Oublier ce qui n'existe pas n'est pas une erreur : la commande est idempotente, et un
/// utilisateur qui veut être sûr de ne plus avoir de secret enregistré doit pouvoir la
/// relancer.
///
/// # Errors
///
/// [`Error::Unavailable`] si le trousseau ne répond pas.
pub fn forget(host: &str, username: &str) -> Result<()> {
    match entry(host, username)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            tracing::info!(host, username, "secret oublié");
            Ok(())
        }
        Err(source) => Err(Error::Unavailable(source.to_string())),
    }
}

/// L'entrée de trousseau d'un compte.
fn entry(host: &str, username: &str) -> Result<Entry> {
    let key = format!("{PREFIX}:{username}@{host}");
    Entry::new(SERVICE, &key).map_err(|source| Error::Unavailable(source.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Un hôte jetable, qui **se nettoie même si le test panique**.
    ///
    /// ## Pourquoi un garde et pas un `forget` en fin de test
    ///
    /// Ces tests écrivent dans le **vrai** trousseau de la machine de l'utilisateur : il n'y a
    /// pas de trousseau en mémoire à substituer, et en simuler un ne testerait plus le
    /// trousseau. Un `forget` en dernière ligne est donc sauté dès qu'une assertion tombe, et
    /// l'entrée reste.
    ///
    /// Ce n'est pas théorique : **34 entrées de test traînaient dans le Credential Manager le
    /// 2026-09-03**, accumulées par les exécutions de plusieurs jours, dont celles de
    /// `mailapi::token` qui a le même défaut. Une suite de tests qui salit la machine de son
    /// utilisateur est un bug de la suite de tests.
    ///
    /// `Drop` tourne en panique comme en succès. C'est la seule construction qui tienne.
    struct Scratch {
        host: String,
        /// Un **deuxième** hôte jetable, nettoyé pareil.
        ///
        /// Il est dans le même garde et pas dans un second `Scratch` parce que le verrou de
        /// trousseau n'est pas réentrant : deux gardes vivants en même temps dans un seul test
        /// se bloqueraient l'un l'autre pour toujours.
        other: String,
        usernames: Vec<String>,
        /// Voir [`crate::KEYRING_LOCK`] : le trousseau de Windows perd des suppressions sous
        /// accès concurrent. Déclaré en dernier, mais ça ne change rien à l'ordre — `Drop` du
        /// type tourne avant celui de ses champs, donc le nettoyage se fait verrou en main.
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    /// Un nom d'hôte jetable, unique.
    ///
    /// `.invalid` est réservé par la RFC 2606 : aucune de ces épreuves ne peut joindre quoi que
    /// ce soit, même si un jour une d'entre elles se mettait à sortir sur le réseau.
    fn disposable(salt: u32) -> String {
        format!(
            "test-jetable-{}-{}-{salt}.invalid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        )
    }

    impl Scratch {
        /// Un hôte unique : deux exécutions concurrentes ne doivent pas se marcher dessus.
        fn new() -> Self {
            Self {
                _lock: crate::keyring_lock(),
                host: disposable(0),
                other: disposable(1),
                usernames: Vec::new(),
            }
        }

        /// L'hôte, en enregistrant l'identifiant à nettoyer.
        fn host(&mut self, username: &str) -> &str {
            self.remember(username);
            &self.host
        }

        /// Le second hôte, en enregistrant l'identifiant à nettoyer.
        fn other(&mut self, username: &str) -> &str {
            self.remember(username);
            &self.other
        }

        fn remember(&mut self, username: &str) {
            let owned = username.to_owned();
            if !self.usernames.contains(&owned) {
                self.usernames.push(owned);
            }
        }

        /// Vrai si le trousseau répond. Un trousseau indisponible — session Linux sans
        /// Secret Service, conteneur — n'est pas un échec du code testé.
        fn available(&self) -> bool {
            if forget(&self.host, "sonde").is_err() {
                eprintln!("trousseau indisponible, test ignoré");
                return false;
            }
            true
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            for username in &self.usernames {
                // L'échec est ignoré : on est peut-être déjà en train de dérouler une
                // panique, et en ajouter une deuxième masquerait la première.
                let _ = forget(&self.host, username);
                let _ = forget(&self.other, username);
            }
        }
    }

    #[test]
    fn a_secret_survives_a_round_trip() {
        let mut scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        let host = scratch.host("marie").to_owned();

        store(&host, "marie", "mot-de-passe-applicatif").unwrap();
        assert_eq!(load(&host, "marie").unwrap(), "mot-de-passe-applicatif");
        forget(&host, "marie").unwrap();
        assert!(matches!(load(&host, "marie"), Err(Error::NotFound { .. })));
    }

    #[test]
    fn two_accounts_on_the_same_host_keep_their_own_secrets() {
        // Deux boîtes chez le même fournisseur est le cas ordinaire, pas une curiosité : le
        // corpus réel a plusieurs comptes Gmail.
        let mut scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        scratch.host("jean");
        let host = scratch.host("marie").to_owned();

        store(&host, "marie", "secret-de-marie").unwrap();
        store(&host, "jean", "secret-de-jean").unwrap();

        assert_eq!(load(&host, "marie").unwrap(), "secret-de-marie");
        assert_eq!(load(&host, "jean").unwrap(), "secret-de-jean");

        forget(&host, "marie").unwrap();
        assert!(
            load(&host, "jean").is_ok(),
            "oublier un compte a effacé l'autre"
        );
    }

    #[test]
    fn the_same_username_on_two_hosts_keeps_two_secrets() {
        // Un seul garde et ses deux hôtes : deux gardes vivants en même temps se bloqueraient
        // sur le verrou de trousseau, qui n'est pas réentrant.
        let mut scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        let one = scratch.host("marie").to_owned();
        let two = scratch.other("marie").to_owned();

        store(&one, "marie", "chez-le-premier").unwrap();
        store(&two, "marie", "chez-le-second").unwrap();

        assert_eq!(load(&one, "marie").unwrap(), "chez-le-premier");
        assert_eq!(load(&two, "marie").unwrap(), "chez-le-second");
    }

    #[test]
    fn storing_again_replaces_the_secret() {
        // Un mot de passe applicatif se renouvelle. Le remplacer doit marcher sans qu'on
        // pense à oublier l'ancien d'abord.
        let mut scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        let host = scratch.host("marie").to_owned();

        store(&host, "marie", "ancien").unwrap();
        store(&host, "marie", "nouveau").unwrap();
        assert_eq!(load(&host, "marie").unwrap(), "nouveau");
    }

    #[test]
    fn an_empty_secret_is_refused() {
        // Un secret vide **réussit** l'enregistrement et échoue à l'authentification :
        // l'utilisateur croirait avoir configuré son compte, et le diagnostic arriverait une
        // synchronisation plus tard.
        let scratch = Scratch::new();
        assert!(matches!(
            store(&scratch.host, "marie", ""),
            Err(Error::Empty)
        ));
    }

    #[test]
    fn forgetting_twice_is_not_an_error() {
        let scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        assert!(
            forget(&scratch.host, "marie").is_ok(),
            "l'oubli n'est pas idempotent"
        );
    }

    #[test]
    fn an_absent_secret_says_what_to_do() {
        let scratch = Scratch::new();
        match load(&scratch.host, "marie") {
            Err(Error::NotFound { .. }) => {
                let message = load(&scratch.host, "marie").unwrap_err().to_string();
                assert!(
                    message.contains("mail account add"),
                    "le message doit dire quoi faire : {message}"
                );
            }
            Err(Error::Unavailable(_)) => eprintln!("trousseau indisponible, test ignoré"),
            other => panic!("issue inattendue : {other:?}"),
        }
    }

    #[test]
    fn a_secret_is_never_in_the_error_message() {
        // Un message d'erreur finit dans un journal. `docs/PRIVACY.md` §8.
        let mut scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        let host = scratch.host("marie").to_owned();
        let secret = "ne-doit-pas-fuiter";
        store(&host, "marie", secret).unwrap();

        // Toutes les erreurs que ce crate sait produire, sur un compte qui a un secret.
        for message in [
            Error::NotFound {
                host: host.clone(),
                username: "marie".to_owned(),
            }
            .to_string(),
            Error::Unavailable("panne".to_owned()).to_string(),
            Error::Empty.to_string(),
        ] {
            assert!(
                !message.contains(secret),
                "un secret apparaît dans un message : {message}"
            );
        }
    }

    #[test]
    fn checking_presence_does_not_require_reading_the_secret_elsewhere() {
        let mut scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        let host = scratch.host("marie").to_owned();

        assert!(!is_stored(&host, "marie"));
        store(&host, "marie", "secret").unwrap();
        assert!(is_stored(&host, "marie"));
        forget(&host, "marie").unwrap();
        assert!(!is_stored(&host, "marie"));
    }

    #[test]
    fn the_guard_cleans_up_after_a_panic() {
        // **Le test du garde lui-même.** Sans lui, on ne saurait pas que le nettoyage marche
        // sur le chemin qui compte — celui où le test échoue.
        let scratch = Scratch::new();
        if !scratch.available() {
            return;
        }
        let host = scratch.host.clone();
        drop(scratch);

        let outcome = std::panic::catch_unwind(|| {
            let mut scratch = Scratch::new();
            // Le même hôte que ci-dessus, pour pouvoir vérifier après.
            scratch.host = host.clone();
            scratch.host("marie");
            store(&scratch.host, "marie", "secret").unwrap();
            panic!("panique volontaire");
        });

        assert!(outcome.is_err(), "la panique n'a pas eu lieu");
        assert!(
            !is_stored(&host, "marie"),
            "le garde n'a pas nettoyé après la panique"
        );
    }
}

//! Le jeton d'accès d'un client, rangé dans le trousseau du système.
//!
//! `docs/PRIVACY.md` §7 est explicite : les secrets d'un client vont dans le trousseau —
//! Credential Manager sur Windows, Keychain sur macOS, Secret Service sur Linux — **jamais
//! dans un fichier de configuration ni dans une variable d'environnement**.
//!
//! La règle n'est pas décorative. Une variable d'environnement se retrouve dans l'historique
//! du shell, dans la table des processus lisible par les autres utilisateurs de la machine,
//! et recopiée dans chaque processus fils. Un jeton d'API mailcore ouvre une boîte mail
//! entière : c'est un secret de la même classe qu'un mot de passe.
//!
//! ## Le démon, lui, lit bien une variable d'environnement
//!
//! Ce n'est pas une contradiction. `maild` est démarré par un gestionnaire de services ou un
//! `compose.yaml`, qui ont leurs propres mécanismes de secrets et ne passent pas par le shell
//! d'un utilisateur. Le client, lui, est lancé à la main. Les deux côtés n'ont pas la même
//! surface d'exposition, donc pas la même règle.
//!
//! ## Une entrée par démon
//!
//! La clé est l'adresse du démon : un poste qui parle à deux démons — celui du bureau et
//! celui de la maison — garde deux jetons distincts, et en révoquer un ne touche pas l'autre.

use keyring::Entry;

/// Le nom sous lequel les entrées apparaissent dans le trousseau du système.
///
/// Visible par l'utilisateur dans le Credential Manager ou le Keychain : il doit dire ce que
/// c'est sans qu'on ait à chercher.
const SERVICE: &str = "mailcore";

/// Ce qui peut mal se passer avec le trousseau.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Aucun jeton n'est enregistré pour ce démon.
    #[error("aucun jeton enregistré pour {0} — utiliser `mail daemon login`")]
    NotFound(String),

    /// Le trousseau du système est inutilisable.
    ///
    /// Arrive sur une session Linux sans Secret Service, ou dans un conteneur. Le message le
    /// dit plutôt que de laisser croire à un jeton manquant.
    #[error("trousseau du système inutilisable : {0}")]
    Unavailable(String),
}

/// Alias de commodité.
pub type Result<T> = std::result::Result<T, Error>;

/// Enregistre le jeton d'un démon.
///
/// # Errors
///
/// [`Error::Unavailable`] si le trousseau ne répond pas.
pub fn store(daemon: &str, token: &str) -> Result<()> {
    entry(daemon)?
        .set_password(token)
        .map_err(|source| Error::Unavailable(source.to_string()))
}

/// Relit le jeton d'un démon.
///
/// # Errors
///
/// [`Error::NotFound`] si rien n'est enregistré, [`Error::Unavailable`] si le trousseau ne
/// répond pas.
pub fn load(daemon: &str) -> Result<String> {
    match entry(daemon)?.get_password() {
        Ok(token) => Ok(token),
        Err(keyring::Error::NoEntry) => Err(Error::NotFound(daemon.to_owned())),
        Err(source) => Err(Error::Unavailable(source.to_string())),
    }
}

/// Oublie le jeton d'un démon.
///
/// Oublier ce qui n'existe pas n'est pas une erreur : la commande est idempotente, et un
/// utilisateur qui veut être sûr de ne plus avoir de jeton doit pouvoir la relancer.
///
/// # Errors
///
/// [`Error::Unavailable`] si le trousseau ne répond pas.
pub fn forget(daemon: &str) -> Result<()> {
    match entry(daemon)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(source) => Err(Error::Unavailable(source.to_string())),
    }
}

/// L'entrée de trousseau d'un démon.
fn entry(daemon: &str) -> Result<Entry> {
    Entry::new(SERVICE, daemon).map_err(|source| Error::Unavailable(source.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Un nom de démon jetable, qui **se nettoie même si le test panique**.
    ///
    /// ## Pourquoi un garde et pas un `forget` en fin de test
    ///
    /// Ces tests écrivent dans le **vrai** trousseau de la machine : il n'y a pas de trousseau
    /// en mémoire à substituer, et en simuler un ne testerait plus le trousseau. Un `forget`
    /// en dernière ligne est donc sauté dès qu'une assertion tombe, et l'entrée reste.
    ///
    /// Ce n'est pas théorique : **34 entrées de test traînaient dans le Credential Manager le
    /// 2026-09-03**, accumulées sur plusieurs jours d'exécutions, la plupart venues d'ici. Une
    /// suite de tests qui salit la machine de son utilisateur est un bug de la suite de tests.
    ///
    /// `Drop` tourne en panique comme en succès. C'est la seule construction qui tienne.
    ///
    /// ## Et un verrou, parce que `Drop` ne suffisait pas
    ///
    /// Le Credential Manager de Windows ne se comporte pas de façon fiable sous accès
    /// concurrent : mesuré le 2026-09-04, une exécution en parallèle laissait des entrées
    /// derrière elle **alors que les gardes tournaient**, et la même exécution avec
    /// `--test-threads=1` n'en laissait aucune. Une suppression émise pendant qu'un autre fil
    /// écrit se perd. C'est aussi ce qui a fait échouer `two_daemons_keep_separate_tokens` une
    /// fois sur une exécution complète du workspace, puis passer à la reprise.
    ///
    /// Ce n'est pas un défaut du code testé : rien dans mailcore n'écrit plusieurs secrets à la
    /// fois. C'est un défaut de la suite de tests.
    struct Scratch {
        names: Vec<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    /// Sérialise les épreuves de trousseau de ce module. Voir [`Scratch`].
    static KEYRING_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl Scratch {
        /// Un garde qui porte **deux** noms jetables.
        ///
        /// Les deux sont dans le même garde et pas dans deux gardes : le verrou n'est pas
        /// réentrant, et deux gardes vivants dans un seul test se bloqueraient pour toujours.
        fn new() -> Self {
            let stamp = |salt: u32| {
                format!(
                    "test-jetable-{}-{}-{salt}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_nanos())
                )
            };
            Self {
                names: vec![stamp(0), stamp(1)],
                // L'empoisonnement est ignoré : un test qui panique en tenant le verrou ferait
                // sinon échouer tous les suivants pour une raison qui n'est pas la leur.
                _lock: KEYRING_LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            }
        }

        fn name(&self) -> &str {
            &self.names[0]
        }

        /// Le second nom jetable, pour un test qui compare deux démons.
        fn other(&self) -> &str {
            &self.names[1]
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            for name in &self.names {
                // L'échec est ignoré : on est peut-être déjà en train de dérouler une panique,
                // et en ajouter une deuxième masquerait la première.
                let _ = forget(name);
            }
        }
    }

    #[test]
    fn a_token_survives_a_round_trip_through_the_keyring() {
        let scratch = Scratch::new();
        let daemon = scratch.name().to_owned();
        // Un trousseau indisponible — session Linux sans Secret Service, conteneur — n'est
        // pas un échec du code testé. On le dit et on s'arrête.
        if store(&daemon, "jeton-de-test").is_err() {
            eprintln!("trousseau indisponible, test ignoré");
            return;
        }

        assert_eq!(load(&daemon).unwrap(), "jeton-de-test");
        forget(&daemon).unwrap();
        assert!(matches!(load(&daemon), Err(Error::NotFound(_))));
    }

    #[test]
    fn forgetting_twice_is_not_an_error() {
        let scratch = Scratch::new();
        let daemon = scratch.name().to_owned();
        if forget(&daemon).is_err() {
            eprintln!("trousseau indisponible, test ignoré");
            return;
        }
        assert!(forget(&daemon).is_ok(), "l'oubli n'est pas idempotent");
    }

    #[test]
    fn an_absent_token_says_what_to_do() {
        let scratch = Scratch::new();
        let daemon = scratch.name().to_owned();
        match load(&daemon) {
            Err(Error::NotFound(message)) => {
                assert!(message.contains(&daemon));
            }
            Err(Error::Unavailable(_)) => eprintln!("trousseau indisponible, test ignoré"),
            Ok(_) => panic!("un jeton existe pour un nom jamais utilisé"),
        }
    }

    #[test]
    fn two_daemons_keep_separate_tokens() {
        // Un poste qui parle au démon du bureau et à celui de la maison ne doit pas
        // confondre les deux, ni en révoquer un en révoquant l'autre.
        // Un seul garde et ses deux noms : deux gardes vivants en même temps se bloqueraient
        // sur le verrou de trousseau, qui n'est pas réentrant.
        let scratch = Scratch::new();
        let bureau = scratch.name().to_owned();
        let maison = scratch.other().to_owned();
        if store(&bureau, "jeton-bureau").is_err() {
            eprintln!("trousseau indisponible, test ignoré");
            return;
        }
        store(&maison, "jeton-maison").unwrap();

        assert_eq!(load(&bureau).unwrap(), "jeton-bureau");
        assert_eq!(load(&maison).unwrap(), "jeton-maison");

        forget(&bureau).unwrap();
        assert_eq!(load(&maison).unwrap(), "jeton-maison");
        forget(&maison).unwrap();
    }
}

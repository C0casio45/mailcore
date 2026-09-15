//! Configuration du démon.
//!
//! Rien ici ne touche à des identifiants. Les mots de passe et jetons OAuth vont dans le
//! trousseau du système (Credential Manager sur Windows), jamais dans un fichier du profil
//! — `docs/PRIVACY.md`, section 7.
//!
//! Le **jeton d'API** suit la même règle côté client. Côté démon, il est lu depuis
//! l'environnement au démarrage et n'est jamais écrit sur le disque : un démon qui persiste
//! son propre secret crée un fichier de plus à protéger.

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use camino::Utf8PathBuf;

/// La variable d'environnement qui porte le jeton porteur.
pub const TOKEN_VAR: &str = "MAILCORE_TOKEN";

/// Longueur minimale acceptée pour un jeton.
///
/// 32 caractères : un secret plus court se devine. Le démon en génère un de cette taille
/// quand on le lui demande, mais rien n'empêche d'en fournir un autre — sauf s'il est trop
/// court, auquel cas le démon refuse plutôt que d'accepter une protection illusoire.
pub const MIN_TOKEN_LEN: usize = 32;

/// Où le démon écoute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    /// Socket locale : named pipe sur Windows, socket Unix ailleurs.
    ///
    /// Protégée par les permissions du système de fichiers. Pas de jeton, pas de TLS :
    /// quelqu'un qui peut ouvrir cette socket peut déjà lire le store.
    Local,
    /// TCP. Exige un jeton, et TLS hors interface de bouclage.
    Tcp(SocketAddr),
}

impl Listen {
    /// Vrai si l'adresse n'est joignable que depuis la machine.
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        match self {
            Self::Local => true,
            Self::Tcp(address) => address.ip().is_loopback(),
        }
    }
}

/// Configuration résolue du démon.
#[derive(Debug, Clone)]
pub struct Config {
    /// Racine du store.
    pub store: Utf8PathBuf,
    /// Où écouter.
    pub listen: Listen,
    /// Le jeton porteur, si un a été fourni.
    pub token: Option<String>,
    /// Certificat TLS, au format PEM.
    pub tls_cert: Option<Utf8PathBuf>,
    /// Clé privée TLS, au format PEM.
    pub tls_key: Option<Utf8PathBuf>,
    /// L'opérateur affirme qu'une barrière extérieure au démon assure le confinement.
    ///
    /// Existe pour un cas précis et réel : **dans un conteneur, `0.0.0.0` n'est pas
    /// « public »**. C'est la seule adresse que le processus puisse y atteindre, et ce qui
    /// décide de l'exposition est la publication de port de l'hôte — `127.0.0.1:7847:7847`
    /// — que le démon ne peut pas observer. Refuser ce cas rendrait le conteneur
    /// inutilisable sans apporter de sécurité ; l'accepter en silence rendrait la règle
    /// creuse.
    ///
    /// Le compromis est donc de l'exiger **explicite**. L'opérateur écrit le drapeau, il
    /// apparaît dans le `compose.yaml`, et le démon le rappelle à chaque démarrage. Le
    /// jeton, lui, reste obligatoire quoi qu'il arrive.
    pub allow_plaintext: bool,
    /// Les profils que l'opérateur autorise un client à importer.
    ///
    /// **Vide par défaut, et c'est un fail closed.** `jobs.start` est la première méthode de
    /// l'API qui écrit ; sans cette liste, aucun import ne peut être déclenché à distance.
    /// Un client désigne un profil par son rang ici et ne nomme jamais de chemin — sinon
    /// quiconque détient le jeton pourrait faire lire n'importe quel fichier de la machine
    /// du démon, le ranger dans le store, puis le relire par `search.query`.
    ///
    /// L'opérateur, lui, sait ce qui est légitime : il le déclare avec `--profile`.
    pub profiles: Vec<Utf8PathBuf>,
    /// Répertoire du front à servir, s'il y en a un.
    ///
    /// Servi **hors de la couche de jeton** : un navigateur ne peut pas authentifier sa
    /// première requête. Ce qui sort par là est notre propre JavaScript, jamais du courrier.
    pub ui_dir: Option<Utf8PathBuf>,
}

impl Config {
    /// Vérifie que la configuration est sûre, ou refuse de démarrer.
    ///
    /// **C'est le critère 10 de `docs/PHASE-1.md`, et c'est un fail closed.** Une mauvaise
    /// configuration doit empêcher le service de tourner, pas exposer une boîte mail en
    /// clair sur un réseau. Un avertissement dans un journal que personne ne lit ne compte
    /// pas comme une protection.
    ///
    /// # Errors
    ///
    /// Si l'écoute est non locale sans jeton, si le jeton est trop court, ou si TLS est
    /// incomplet.
    pub fn validate(&self) -> Result<()> {
        let Listen::Tcp(address) = &self.listen else {
            return Ok(());
        };

        if let Some(token) = &self.token {
            if token.len() < MIN_TOKEN_LEN {
                bail!(
                    "jeton trop court : {} caractères, minimum {MIN_TOKEN_LEN}. \
                     Un secret devinable ne protège rien.",
                    token.len()
                );
            }
        } else {
            bail!(
                "écoute TCP sur {address} sans jeton. Poser {TOKEN_VAR} dans \
                 l'environnement, ou écouter sur la socket locale."
            );
        }

        // Le bouclage reste joignable par tout processus local, y compris un onglet de
        // navigateur ou un logiciel malveillant : le jeton y est donc exigé. TLS n'y ajoute
        // rien, puisque le trafic ne quitte pas la machine.
        if self.listen.is_loopback() {
            return Ok(());
        }

        match (&self.tls_cert, &self.tls_key) {
            (Some(_), Some(_)) => Ok(()),
            _ if self.allow_plaintext => Ok(()),
            _ => bail!(
                "écoute sur {address}, hors bouclage, sans TLS. Trois issues : fournir \
                 --tls-cert et --tls-key ; écouter sur 127.0.0.1 derrière un tunnel déjà \
                 chiffré ; ou, si une barrière extérieure au démon assure le confinement — \
                 la publication de port d'un conteneur, par exemple — l'affirmer avec \
                 --insecure-no-tls."
            ),
        }
    }

    /// Vrai si le trafic circulera en clair sur une interface non locale.
    ///
    /// À rappeler bruyamment au démarrage : un drapeau posé une fois dans un fichier de
    /// déploiement se fait oublier, et l'oubli porte ici sur le contenu d'une boîte mail.
    #[must_use]
    pub fn serves_plaintext_off_loopback(&self) -> bool {
        !self.listen.is_loopback() && self.tls_cert.is_none()
    }

    /// Le jeton lu depuis l'environnement, s'il y en a un.
    #[must_use]
    pub fn token_from_env() -> Option<String> {
        std::env::var(TOKEN_VAR).ok().filter(|t| !t.is_empty())
    }
}

/// Génère un jeton porteur.
///
/// Le démon le génère, l'utilisateur ne le choisit pas : un secret que quelqu'un a tapé est
/// un secret devinable. 32 caractères en base 32 lisible, soit 160 bits d'entropie — assez
/// pour qu'une attaque par force brute soit hors de question, et recopiable à la main sans
/// confondre `0` et `O`.
///
/// # Errors
///
/// Si le système ne peut pas fournir d'aléa cryptographique. Rendre une erreur plutôt que de
/// paniquer, et surtout plutôt que de se rabattre sur une source prévisible : un jeton
/// devinable serait pire que pas de jeton du tout, parce qu'il donnerait l'illusion d'une
/// protection.
pub fn generate_token() -> Result<String> {
    // Alphabet Crockford, sans I, L, O ni U : pas de confusion visuelle à la recopie, et pas
    // de mot formé par hasard.
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    let mut bytes = [0u8; MIN_TOKEN_LEN];
    // Le générateur du système, et lui seul — `BCryptGenRandom` sur Windows, `getrandom(2)`
    // sur Linux. Pas de générateur ensemencé sur l'horloge.
    getrandom::fill(&mut bytes).context("le système ne fournit pas d'aléa cryptographique")?;

    Ok(bytes
        .iter()
        // 256 = 8 × 32 : le modulo ne biaise pas la distribution.
        .map(|byte| char::from(ALPHABET[(*byte % 32) as usize]))
        .collect())
}

/// Les profils Thunderbird plausibles sur cette machine.
///
/// Découverts, pas devinés : `<données de l'utilisateur>/Thunderbird/Profiles/*` sur Windows et
/// macOS, `~/.thunderbird/*` ailleurs. Rien n'est ouvert ici, seulement listé.
///
/// ## Pourquoi cette fonction vit dans `maild` et pas dans une coquille
///
/// C'est le démon qui décide ce qui est importable — `jobs::Sources` — et **un client ne nomme
/// jamais un chemin** : il choisit un rang dans la liste que `jobs.sources` lui rend. La
/// découverte appartient donc au côté service, quelle que soit la coquille qui le monte.
///
/// Elle a d'abord été écrite dans la coquille Tauri, puis la coquille native en a eu besoin.
/// Deux copies d'une règle de sécurité — « `prefs.js` marque un profil » — sont une copie de
/// trop.
#[must_use]
pub fn discover_profiles() -> Vec<Utf8PathBuf> {
    let Some(dirs) = directories::BaseDirs::new() else {
        return Vec::new();
    };
    // Sur Windows, Thunderbird vit dans `Roaming` ; ailleurs, dans le répertoire de données.
    let roots = [
        dirs.data_dir().join("Thunderbird").join("Profiles"),
        dirs.home_dir().join(".thunderbird"),
    ];

    let mut found = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Un profil contient `prefs.js`. C'est le marqueur le moins ambigu, et il évite de
            // proposer un répertoire de cache pour un profil.
            if path.join("prefs.js").is_file()
                && let Ok(utf8) = Utf8PathBuf::from_path_buf(path)
            {
                found.push(utf8);
            }
        }
    }
    found.sort();
    found
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn config(listen: Listen, token: Option<&str>) -> Config {
        Config {
            store: Utf8PathBuf::from("store"),
            listen,
            token: token.map(str::to_owned),
            tls_cert: None,
            tls_key: None,
            allow_plaintext: false,
            profiles: Vec::new(),
            ui_dir: None,
        }
    }

    fn tcp(ip: [u8; 4], port: u16) -> Listen {
        Listen::Tcp(std::net::SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3])),
            port,
        ))
    }

    const GOOD_TOKEN: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    // ------------------------------------------------------- critère 10 : fail closed

    #[test]
    fn refuses_a_non_loopback_listener_without_a_token() {
        // Le critère 10, dans sa forme la plus directe.
        let config = config(tcp([0, 0, 0, 0], 8080), None);
        assert!(config.validate().is_err());
    }

    #[test]
    fn refuses_loopback_tcp_without_a_token_too() {
        // Le bouclage est joignable par n'importe quel processus local, y compris un onglet
        // de navigateur. Le jeton y est exigé aussi.
        assert!(config(tcp([127, 0, 0, 1], 8080), None).validate().is_err());
    }

    #[test]
    fn refuses_a_token_that_is_too_short() {
        for short in ["", "abc", "0123456789ABCDEF"] {
            let config = config(tcp([127, 0, 0, 1], 8080), Some(short));
            assert!(config.validate().is_err(), "accepté : {short:?}");
        }
    }

    #[test]
    fn refuses_a_public_listener_without_tls() {
        let config = config(tcp([192, 168, 1, 10], 8080), Some(GOOD_TOKEN));
        assert!(config.validate().is_err());
    }

    #[test]
    fn refuses_tls_with_only_half_the_pair() {
        let mut config = config(tcp([192, 168, 1, 10], 8080), Some(GOOD_TOKEN));
        config.tls_cert = Some(Utf8PathBuf::from("cert.pem"));
        assert!(config.validate().is_err(), "certificat sans clé accepté");

        config.tls_cert = None;
        config.tls_key = Some(Utf8PathBuf::from("key.pem"));
        assert!(config.validate().is_err(), "clé sans certificat acceptée");
    }

    #[test]
    fn refuses_plaintext_off_loopback_unless_it_is_stated_explicitly() {
        // Le cas du conteneur : `0.0.0.0` y est la seule adresse atteignable, et ce qui
        // confine est la publication de port de l'hôte — invisible depuis le démon.
        let mut config = config(tcp([0, 0, 0, 0], 7847), Some(GOOD_TOKEN));
        assert!(
            config.validate().is_err(),
            "le clair hors bouclage doit être refusé par défaut"
        );

        config.allow_plaintext = true;
        assert!(
            config.validate().is_ok(),
            "l'affirmation explicite doit être acceptée"
        );
    }

    #[test]
    fn stating_it_explicitly_still_does_not_waive_the_token() {
        // Le drapeau lève l'exigence de TLS, jamais celle du jeton.
        let mut config = config(tcp([0, 0, 0, 0], 7847), None);
        config.allow_plaintext = true;
        assert!(config.validate().is_err());
    }

    #[test]
    fn plaintext_off_loopback_is_reported_so_it_can_be_warned_about() {
        let mut plaintext = config(tcp([0, 0, 0, 0], 7847), Some(GOOD_TOKEN));
        plaintext.allow_plaintext = true;
        assert!(plaintext.serves_plaintext_off_loopback());

        // Sur le bouclage, rien à signaler : le trafic ne quitte pas la machine.
        let local = config(tcp([127, 0, 0, 1], 7847), Some(GOOD_TOKEN));
        assert!(!local.serves_plaintext_off_loopback());

        // Avec TLS non plus.
        let mut secured = config(tcp([192, 168, 1, 10], 7847), Some(GOOD_TOKEN));
        secured.tls_cert = Some(Utf8PathBuf::from("cert.pem"));
        secured.tls_key = Some(Utf8PathBuf::from("key.pem"));
        assert!(!secured.serves_plaintext_off_loopback());
    }

    // ------------------------------------------------------- ce qui doit être accepté

    #[test]
    fn accepts_the_local_socket_without_anything() {
        // Les permissions du système de fichiers suffisent : qui peut ouvrir la socket peut
        // déjà lire le store.
        assert!(config(Listen::Local, None).validate().is_ok());
    }

    #[test]
    fn accepts_loopback_tcp_with_a_token() {
        assert!(
            config(tcp([127, 0, 0, 1], 8080), Some(GOOD_TOKEN))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn accepts_a_public_listener_with_a_token_and_tls() {
        let mut config = config(tcp([192, 168, 1, 10], 8080), Some(GOOD_TOKEN));
        config.tls_cert = Some(Utf8PathBuf::from("cert.pem"));
        config.tls_key = Some(Utf8PathBuf::from("key.pem"));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn loopback_detection_matches_the_address() {
        assert!(Listen::Local.is_loopback());
        assert!(tcp([127, 0, 0, 1], 1).is_loopback());
        assert!(!tcp([0, 0, 0, 0], 1).is_loopback());
        assert!(!tcp([192, 168, 1, 10], 1).is_loopback());
    }

    // ------------------------------------------------------- génération de jeton

    #[test]
    fn generated_tokens_are_long_enough_and_unpredictable() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();

        assert_eq!(first.len(), MIN_TOKEN_LEN);
        assert_ne!(first, second, "deux jetons identiques");
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn generated_tokens_avoid_visually_confusable_characters() {
        // Un jeton se recopie à la main : `0`/`O` et `1`/`I` sont exclus de l'alphabet.
        for _ in 0..50 {
            let token = generate_token().unwrap();
            for confusable in ['I', 'L', 'O', 'U'] {
                assert!(!token.contains(confusable), "{token} contient {confusable}");
            }
        }
    }

    #[test]
    fn a_generated_token_passes_validation() {
        let config = config(tcp([127, 0, 0, 1], 8080), Some(&generate_token().unwrap()));
        assert!(config.validate().is_ok());
    }
}

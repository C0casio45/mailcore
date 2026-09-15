//! Établir la connexion chiffrée, et **refuser tout le reste**.
//!
//! ## Il n'y a pas de chemin en clair
//!
//! `mailcore::Security` n'a que deux variantes, `Tls` et `StartTls` : aucune configuration
//! d'un compte ne peut demander une connexion non chiffrée. Ce module est la conséquence de
//! ce choix, et il ne le contourne pas.
//!
//! Trois choses qu'il ne fait pas, et qui sont toutes des refus :
//!
//! - **aucune option pour ignorer un certificat.** Un échec de vérification est un échec de
//!   synchronisation, pas un avertissement. Une option « accepter quand même » finit toujours
//!   par être activée « juste pour tester », et ne se désactive jamais ;
//! - **aucun repli en clair** si le chiffrement échoue. Un repli silencieux enverrait le mot
//!   de passe en clair sur le réseau, ce qui est exactement l'accident que `Security` sans
//!   variante en clair sert à rendre impossible ;
//! - **aucun `STARTTLS` optionnel.** Un serveur qui n'annonce pas `STARTTLS` alors que le
//!   compte le demande fait échouer la connexion. Continuer en clair, c'est se laisser
//!   rétrograder par un attaquant qui a retiré l'annonce.
//!
//! ## Ce que la vérification utilise
//!
//! `rustls-platform-verifier` : le magasin de confiance **du système**. C'est ce qui fait
//! qu'un certificat d'entreprise installé par l'administrateur est accepté, et qu'un
//! certificat révoqué au niveau du système l'est aussi ici. Embarquer notre propre magasin
//! nous ferait diverger de ce que l'utilisateur a configuré.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use mailcore::{Security, Server};

use crate::client::Client;
use crate::error::{Error, Result};

/// Délai d'établissement de la connexion.
///
/// Trente secondes : un serveur injoignable doit le dire avant qu'une synchronisation
/// périodique ne se superpose à la suivante. Sans délai, un pare-feu qui **jette** les
/// paquets — au lieu de les refuser — laisse le fil attendre le délai du système, qui se
/// compte en minutes.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Délai de lecture, une fois la connexion établie.
///
/// Plus généreux : un `UID FETCH` sur un gros dossier prend son temps, et couper une moisson
/// en cours coûte le lot.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Un flux chiffré vers un serveur IMAP.
///
/// Le type est opaque exprès : personne en dehors de ce module n'a à savoir s'il y a une
/// couche TLS, et surtout personne ne doit pouvoir en fabriquer un sans passer par
/// [`connect`].
#[derive(Debug)]
pub struct Encrypted {
    inner: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
}

impl Read for Encrypted {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buffer)
    }
}

impl crate::client::Timed for Encrypted {
    /// La borne est posée sur le socket **sous** le tunnel, et c'est le seul endroit possible :
    /// TLS ne sait pas attendre, il déchiffre ce qui arrive.
    ///
    /// Une expiration au milieu d'un enregistrement TLS n'est pas une perte : `rustls` garde
    /// l'enregistrement partiel et reprend à la lecture suivante — il est écrit pour les
    /// sockets non bloquants. Ce qui ne se reprend pas est une **ligne** partiellement livrée,
    /// et c'est [`Client::idle_wait`] qui le refuse.
    ///
    /// [`Client::idle_wait`]: crate::Client::idle_wait
    fn set_read_timeout(&self, after: Option<std::time::Duration>) -> std::io::Result<()> {
        self.inner.sock.set_read_timeout(after)
    }

    fn read_timeout(&self) -> std::io::Result<Option<std::time::Duration>> {
        self.inner.sock.read_timeout()
    }
}

impl Write for Encrypted {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Ouvre une connexion chiffrée et lit le salut du serveur.
///
/// # Errors
///
/// [`Error::InvalidHost`] si l'hôte n'est pas un nom valide pour TLS, [`Error::Network`] si la
/// connexion TCP échoue, [`Error::Tls`] si le chiffrement ne peut pas être établi ou vérifié.
///
/// **Aucun de ces échecs ne laisse une connexion en clair derrière lui.**
pub fn connect(server: &Server) -> Result<Client<Encrypted>> {
    let stream = dial(server)?;

    // **Deux chemins qui ne lisent pas la même chose ensuite.** En TLS direct, le salut arrive
    // dans le tunnel. Après un `STARTTLS`, il a déjà été lu en clair et le serveur n'en envoie
    // pas de second — voir [`Client::over`].
    let mut client = match server.security {
        Security::Tls => Client::greet(handshake(server, stream)?)?,
        Security::StartTls => {
            // `STARTTLS` demande de parler en clair d'abord, ce qui est le seul endroit du
            // crate où des octets partent non chiffrés. Ils sont bornés à deux commandes qui
            // ne portent aucun secret — `CAPABILITY` et `STARTTLS` — et l'authentification
            // n'a lieu qu'après le passage en chiffré.
            let mut plain = Client::greet(stream)?;
            if !plain.has("STARTTLS") {
                return Err(Error::Tls {
                    reason: format!(
                        "le compte demande STARTTLS, {} ne l'annonce pas",
                        server.host
                    ),
                });
            }
            plain.command("STARTTLS").map_err(|source| Error::Tls {
                reason: format!("STARTTLS refusé : {source}"),
            })?;
            Client::over(handshake(server, plain.into_stream())?)
        }
    };

    // Les capacités d'avant le chiffrement ne comptent pas : un attaquant en position de les
    // modifier aurait pu en retirer. La RFC 2595 demande de les redemander, et c'est aussi ce
    // qui fait découvrir `CONDSTORE` chez les serveurs qui ne l'annoncent qu'authentifiés.
    client.refresh_capabilities()?;
    // Journalisé parce que c'est la première chose qu'on veut savoir quand un serveur refuse
    // ce qu'il annonçait. Une liste de capacités ne contient aucun secret.
    tracing::info!(
        host = %server.host,
        capabilities = %client.capabilities().join(" "),
        "connexion chiffrée établie"
    );
    Ok(client)
}

/// Ouvre la connexion TCP, avec un délai.
fn dial(server: &Server) -> Result<TcpStream> {
    use std::net::ToSocketAddrs;

    let target = (server.host.as_str(), server.port);
    let mut last = None;
    for address in target.to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream.set_read_timeout(Some(READ_TIMEOUT))?;
                stream.set_write_timeout(Some(READ_TIMEOUT))?;
                // Nagle désactivé : le dialogue IMAP est une suite de petites commandes, et
                // attendre un remplissage de segment ajoute un aller-retour à chacune.
                stream.set_nodelay(true)?;
                return Ok(stream);
            }
            // Une machine à double pile rend une adresse v6 et une v4 ; la première peut
            // échouer là où la seconde marche. Essayer toutes les adresses avant de renoncer.
            Err(error) => last = Some(error),
        }
    }
    Err(Error::Network(last.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "aucune adresse pour cet hôte",
        )
    })))
}

/// Passe un flux en TLS.
fn handshake(server: &Server, stream: TcpStream) -> Result<Encrypted> {
    let name = rustls::pki_types::ServerName::try_from(server.host.clone()).map_err(|_| {
        Error::InvalidHost {
            host: server.host.clone(),
        }
    })?;

    let config = client_config()?;
    let connection =
        rustls::ClientConnection::new(Arc::new(config), name).map_err(|source| Error::Tls {
            reason: source.to_string(),
        })?;
    Ok(Encrypted {
        inner: rustls::StreamOwned::new(connection, stream),
    })
}

/// La configuration TLS : magasin du système, aucune dérogation.
///
/// Le fournisseur cryptographique est posé **explicitement** plutôt que laissé au défaut du
/// processus : `rustls` panique si aucun défaut n'a été installé, et une panique au premier
/// compte synchronisé serait une panne difficile à relier à sa cause.
fn client_config() -> Result<rustls::ClientConfig> {
    use rustls_platform_verifier::BuilderVerifierExt;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|source| Error::Tls {
            reason: source.to_string(),
        })?
        .with_platform_verifier()
        .map_err(|source| Error::Tls {
            reason: format!("magasin de confiance du système illisible : {source}"),
        })?
        .with_no_client_auth();
    Ok(config)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use mailcore::AuthKind;

    fn server(host: &str, security: Security) -> Server {
        Server {
            host: host.to_owned(),
            port: 993,
            username: "marie@exemple.fr".to_owned(),
            auth: AuthKind::Password,
            security,
        }
    }

    #[test]
    fn a_host_that_is_not_a_valid_tls_name_is_refused_without_a_connection() {
        // Le refus tombe avant tout socket : un nom invalide n'a pas à faire sortir un
        // paquet, et le diagnostic est meilleur.
        let outcome = handshake(
            &server("pas un nom d'hôte", Security::Tls),
            // Un socket qui n'est jamais utilisé : la vérification du nom passe avant.
            std::net::TcpListener::bind(("127.0.0.1", 0))
                .and_then(|listener| {
                    let address = listener.local_addr()?;
                    std::thread::spawn(move || drop(listener.accept()));
                    TcpStream::connect(address)
                })
                .unwrap(),
        );
        assert!(matches!(outcome, Err(Error::InvalidHost { .. })));
    }

    #[test]
    fn an_ip_address_is_a_valid_tls_name() {
        // Le déploiement de référence passe par un tunnel vers `127.0.0.1`. Refuser une
        // adresse IP casserait ce cas.
        assert!(
            rustls::pki_types::ServerName::try_from("127.0.0.1".to_owned()).is_ok(),
            "une adresse IP doit rester un nom TLS valide"
        );
    }

    #[test]
    fn the_system_trust_store_is_readable() {
        // Un magasin illisible rendrait toute synchronisation impossible, et le dire ici
        // vaut mieux que de le découvrir au premier compte.
        assert!(client_config().is_ok());
    }
}

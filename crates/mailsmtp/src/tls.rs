//! Ouvrir la connexion chiffrée vers le serveur de soumission, et **refuser tout le reste**.
//!
//! ## Ce que ce module a de différent de son homologue IMAP
//!
//! Le squelette est celui de `mailsync::tls` — même magasin de confiance, mêmes refus, même
//! absence de dérogation. Trois choses seulement changent, et chacune vient du protocole :
//!
//! **L'`EHLO` encadre le `STARTTLS` des deux côtés.** En IMAP, `CAPABILITY` avant le chiffrement
//! est facultatif. En SMTP, il faut un `EHLO` **avant** pour savoir si `STARTTLS` est annoncé, et
//! un second **après** parce que la RFC 3207 §4.2 demande d'oublier le premier. Réutiliser les
//! capacités d'avant, ce serait accepter la liste qu'un attaquant a laissée passer — c'est
//! ainsi qu'on se fait rétrograder d'`AUTH XOAUTH2` vers `AUTH PLAIN`.
//!
//! **Il n'y a pas de second salut après la poignée de main**, d'où [`Client::over`].
//!
//! **Les ports ne sont pas les mêmes**, et [`Security::submission_port`] existe pour que
//! personne ne lise 993 pour un envoi.
//!
//! ## Il n'y a pas de chemin en clair
//!
//! `mailcore::Security` n'a que `Tls` et `StartTls`. Un `SUBMIT` non chiffré transporte le mot
//! de passe **et** le message ; c'est pire qu'un IMAP en clair, qui ne transporte que le mot de
//! passe et ce qu'on lit. Aucune option n'ignore un certificat, aucun repli en clair ne rattrape
//! un échec, et un serveur qui n'annonce pas `STARTTLS` alors que le compte le demande fait
//! échouer la connexion.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use mailcore::{Security, Server};

use crate::client::{Client, Stage};
use crate::error::{Error, Result};

/// Délai d'établissement de la connexion.
///
/// Trente secondes, comme en IMAP : un pare-feu qui **jette** les paquets — au lieu de les
/// refuser — laisserait sinon le fil attendre le délai du système, qui se compte en minutes.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Délai de lecture, une fois la connexion établie.
///
/// Cinq minutes, et c'est la RFC 5321 §4.5.3.2.6 qui le demande : après le point final, un
/// serveur prend le temps d'écrire le message et de le passer aux filtres avant de répondre.
/// Couper trop tôt ici est le pire cas de tout le crate — la réponse serait perdue alors que le
/// serveur a le message, et l'envoi deviendrait douteux pour rien.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Un flux chiffré vers un serveur de soumission.
///
/// Opaque exprès : personne en dehors de ce module ne doit pouvoir en fabriquer un sans passer
/// par [`connect`].
#[derive(Debug)]
pub struct Encrypted {
    inner: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
}

impl Read for Encrypted {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buffer)
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

/// Ouvre une connexion chiffrée, dit `EHLO`, et rend un client prêt à s'authentifier.
///
/// Les capacités rendues par [`Client::capabilities`] sont celles d'**après** le chiffrement
/// dans les deux modes. C'est ce qui permet à [`Client::auth`] de refuser un mécanisme absent
/// sans avoir à se demander si la liste est digne de foi.
///
/// # Errors
///
/// [`Error::InvalidHost`] si l'hôte n'est pas un nom valide pour TLS, [`Error::Network`] si la
/// connexion échoue, [`Error::Tls`] si le chiffrement ne peut pas être établi ou vérifié — y
/// compris quand le compte demande `STARTTLS` et que le serveur ne l'annonce pas.
///
/// **Aucun de ces échecs ne laisse une connexion en clair derrière lui**, et aucun ne porte
/// [`Stage::Committing`] : à ce stade rien n'a pu partir.
pub fn connect(server: &Server) -> Result<Client<Encrypted>> {
    let stream = dial(server)?;
    let name = client_name(&server.username);

    let mut client = match server.security {
        Security::Tls => Client::greet(handshake(server, stream)?)?,
        Security::StartTls => {
            // Le seul endroit du crate où des octets partent en clair. Ils sont bornés à un
            // `EHLO` et un `STARTTLS`, qui ne portent aucun secret : l'authentification et le
            // message n'ont lieu qu'après le passage en chiffré.
            let mut plain = Client::greet(stream)?;
            plain.ehlo(&name)?;
            if !plain.has("STARTTLS") {
                return Err(Error::Tls {
                    reason: format!(
                        "le compte demande STARTTLS, {} ne l'annonce pas",
                        server.host
                    ),
                });
            }
            let ready = plain.command(Stage::StartTls, "STARTTLS")?;
            // `command` laisse aussi passer un `334`, qui est une demande de continuation et
            // non un feu vert. Ouvrir la poignée de main dessus parlerait TLS à un serveur qui
            // attend une ligne en clair, et le diagnostic serait un échec de handshake.
            if !ready.accepted() {
                return Err(Error::Tls {
                    reason: format!("STARTTLS a rendu {} : {}", ready.code, ready.text()),
                });
            }
            // `into_stream` refuse un tampon non vide : voir sa documentation, c'est là que
            // passerait une injection en clair.
            Client::over(handshake(server, plain.into_stream()?)?)
        }
    };

    // **Le second `EHLO`, ou le premier en TLS direct.** Dans les deux cas il a lieu dans le
    // tunnel, et c'est sa réponse qui devient la liste de capacités du client.
    client.ehlo(&name)?;

    // Une liste de capacités ne porte aucun secret, et c'est la première chose qu'on veut
    // savoir quand un serveur refuse le mécanisme qu'il annonçait.
    tracing::info!(
        host = %server.host,
        port = server.port,
        extensions = client.capabilities().len(),
        size_limit = ?client.size_limit(),
        "connexion de soumission établie"
    );
    Ok(client)
}

/// Le nom annoncé à l'`EHLO`.
///
/// ## Pourquoi ce n'est pas le nom de la machine
///
/// Il finit dans les en-têtes `Received` du destinataire. `DESKTOP-4F2K9A` y dirait le nom du
/// poste de travail à tous les correspondants, et `docs/PRIVACY.md` demande que rien ne
/// s'ajoute au message qui ne soit visible et voulu.
///
/// Le domaine de l'identifiant, lui, ne publie rien de neuf : le serveur le connaît déjà — c'est
/// le sien ou celui du compte — et le destinataire le lit dans l'en-tête `From`.
///
/// Le repli est `localhost`, qui ne désigne personne. Il sert dès que l'identifiant n'a pas de
/// domaine pleinement qualifié : un identifiant sans `@`, ce qui arrive chez les fournisseurs
/// qui séparent identifiant et adresse, et un domaine sans point, qui pourrait être le nom
/// d'une machine du réseau local.
///
/// ## Le domaine est validé, pas recopié
///
/// Et ce n'est pas de la prudence de principe : au premier jet, cette fonction rendait le
/// domaine tel quel, et `client_name("a@b.fr\nQUIT")` rendait `"b.fr\nQUIT"` — une **injection
/// de commande SMTP** dans l'`EHLO`. Trouvé par le test
/// `the_name_never_carries_a_line_break`, qui est écrit pour ça. Un identifiant vient de la
/// configuration du compte, donc de l'utilisateur ou d'un import, et ce n'est pas une raison de
/// lui faire confiance.
///
/// Seuls les caractères d'un nom d'hôte passent — lettres ASCII, chiffres, `-` et `.` — ce qui
/// exclut le retour à la ligne, l'espace et tout le reste par construction plutôt que par
/// énumération de ce qui est dangereux.
fn client_name(username: &str) -> String {
    username
        .rsplit_once('@')
        .map(|(_, domain)| domain)
        .filter(|domain| {
            domain.contains('.')
                && domain
                    .bytes()
                    .all(|it| it.is_ascii_alphanumeric() || it == b'-' || it == b'.')
        })
        .unwrap_or("localhost")
        .to_owned()
}

/// Ouvre la connexion TCP, avec un délai.
fn dial(server: &Server) -> Result<TcpStream> {
    use std::net::ToSocketAddrs;

    let mut last = None;
    let addresses = (server.host.as_str(), server.port)
        .to_socket_addrs()
        .map_err(network)?;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(READ_TIMEOUT))
                    .and_then(|()| stream.set_write_timeout(Some(READ_TIMEOUT)))
                    // Nagle désactivé : le dialogue SMTP est une suite de petites commandes
                    // dont chacune attend une réponse, et attendre un remplissage de segment
                    // ajouterait un aller-retour à chacune.
                    .and_then(|()| stream.set_nodelay(true))
                    .map_err(network)?;
                return Ok(stream);
            }
            // Une machine à double pile rend une adresse v6 et une v4 ; la première peut
            // échouer là où la seconde marche. Essayer toutes les adresses avant de renoncer.
            Err(error) => last = Some(error),
        }
    }
    Err(network(last.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "aucune adresse pour cet hôte",
        )
    })))
}

/// Une erreur réseau d'avant le dialogue.
///
/// L'étape est [`Stage::Greeting`] et le choix compte : c'est la seule étape qui ne soit pas
/// [`Stage::Committing`] et qui décrive honnêtement « rien n'est parti ».
fn network(source: std::io::Error) -> Error {
    Error::Network {
        stage: Stage::Greeting,
        source,
    }
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
/// envoi serait une panne difficile à relier à sa cause.
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
    use super::{client_config, client_name, handshake};
    use crate::error::Error;
    use mailcore::{AuthKind, Security, Server};

    fn server(host: &str) -> Server {
        Server {
            host: host.to_owned(),
            port: 465,
            username: "marie@exemple.fr".to_owned(),
            auth: AuthKind::Password,
            security: Security::Tls,
        }
    }

    #[test]
    fn a_host_that_is_not_a_valid_tls_name_is_refused_without_a_connection() {
        // Le refus tombe avant tout octet chiffré : un nom invalide n'a pas à faire sortir un
        // paquet, et le diagnostic est meilleur.
        let socket = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|listener| {
                let address = listener.local_addr()?;
                std::thread::spawn(move || drop(listener.accept()));
                std::net::TcpStream::connect(address)
            })
            .unwrap();
        let outcome = handshake(&server("pas un nom d'hôte"), socket);
        assert!(matches!(outcome, Err(Error::InvalidHost { .. })));
    }

    #[test]
    fn the_system_trust_store_is_readable() {
        // Un magasin illisible rendrait tout envoi impossible, et le dire ici vaut mieux que
        // de le découvrir au premier message.
        assert!(client_config().is_ok());
    }

    #[test]
    fn the_ehlo_name_is_the_domain_of_the_account() {
        assert_eq!(client_name("marie@exemple.fr"), "exemple.fr");
        assert_eq!(
            client_name("marie@mail.exemple.co.uk"),
            "mail.exemple.co.uk"
        );
    }

    #[test]
    fn an_identifier_without_a_qualified_domain_never_names_the_machine() {
        // **La règle de confidentialité de cette fonction.** Le repli doit ne désigner
        // personne, et surtout pas le poste de travail : le nom annoncé finit dans les
        // en-têtes `Received` que lit le destinataire.
        assert_eq!(client_name("marie"), "localhost");
        assert_eq!(client_name("marie@"), "localhost");
        assert_eq!(client_name(""), "localhost");
        // Un domaine sans point n'est pas pleinement qualifié : un serveur strict le refuse, et
        // ce pourrait être le nom d'une machine du réseau local.
        assert_eq!(client_name("marie@localdomain"), "localhost");
    }

    #[test]
    fn the_name_never_carries_a_line_break() {
        // Il part dans une commande `EHLO`. Un retour à la ligne y injecterait une seconde
        // commande — le même piège que `compose::Address::parse` referme sur les adresses.
        for hostile in ["marie@exemple.fr\r\nMAIL FROM:<x@y.fr>", "a@b.fr\nQUIT"] {
            let name = client_name(hostile);
            assert!(
                !name.contains('\r') && !name.contains('\n'),
                "{name:?} porte un retour à la ligne"
            );
        }
    }

    #[test]
    fn the_submission_ports_are_not_the_imap_ones() {
        // Lire 993 pour un envoi enverrait le message là où rien ne parle SMTP, et l'échec
        // ressemblerait à une panne réseau.
        assert_eq!(Security::Tls.submission_port(), 465);
        assert_eq!(Security::StartTls.submission_port(), 587);
        assert_ne!(
            Security::Tls.submission_port(),
            Security::Tls.default_port()
        );
        assert_ne!(
            Security::StartTls.submission_port(),
            Security::StartTls.default_port()
        );
    }
}

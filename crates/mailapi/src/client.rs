//! Un client JSON-RPC vers un démon, bloquant et sans dépendance.
//!
//! Il vit ici parce que `mailapi` est **le contrat**, et qu'un contrat qui fournit les types
//! sans fournir de quoi les transporter oblige chaque client à réécrire la même centaine de
//! lignes. Elle a déjà été écrite deux fois — dans l'outillage de mesure et dans un test
//! d'intégration — ce qui est le signe habituel qu'elle doit être écrite une fois.
//!
//! ## Bloquant, et sans arbre de dépendances
//!
//! `std::net::TcpStream` et rien d'autre. Pas de `reqwest` : un client généreux en pool, en
//! interception et en nouvelle tentative masquerait ce qui part réellement sur le fil, et
//! `mailapi` ne dépend d'aucun runtime — c'est ce qui lui permet d'être tiré par une CLI
//! synchrone comme par un front asynchrone qui l'appellera depuis un fil bloquant.
//!
//! ## Pas de TLS, et c'est une contrainte assumée
//!
//! Ce client parle en clair. Il **refuse** d'envoyer un jeton à un hôte non local, parce que
//! ce serait envoyer le secret d'une boîte mail en clair sur un réseau — la symétrie exacte
//! de la règle que le démon s'applique à lui-même (critère 10).
//!
//! Ça ne restreint rien dans le déploiement recommandé : `docs/ARCHITECTURE.md` conseille un
//! tunnel déjà chiffré et authentifié, qui ramène le cas distant au cas local. Le client
//! parle alors à `127.0.0.1` et le tunnel fait le chiffrement. Un client TLS natif viendra
//! avec le front, qui en a besoin pour son propre compte.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

use crate::jsonrpc;

/// Le port par défaut du démon.
pub const DEFAULT_PORT: u16 = 7847;

/// Délai de lecture d'une réponse.
///
/// Trois minutes : `store.wait` est un long-poll qui peut légitimement dormir deux minutes,
/// et couper avant lui transformerait un abonnement qui marche en erreur périodique.
const READ_TIMEOUT: Duration = Duration::from_secs(180);

/// Taille maximale d'une réponse acceptée.
///
/// 64 Mio : bien au-delà d'un corps de message assaini, et assez bas pour qu'un démon en
/// panne ne fasse pas gonfler le client sans fin.
const MAX_RESPONSE: usize = 64 * 1024 * 1024;

/// Ce qui peut mal se passer en parlant au démon.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// L'adresse ne peut pas être résolue.
    #[error("adresse du démon illisible : {0}")]
    Address(String),

    /// Le client refuse d'envoyer un jeton en clair hors de la machine.
    ///
    /// Voir le module : c'est la même règle que celle que le démon s'applique.
    #[error(
        "refus d'envoyer le jeton en clair à {0}, qui n'est pas une adresse locale. \
         Monter un tunnel chiffré et viser 127.0.0.1, ou attendre le client TLS."
    )]
    WouldSendTokenInClear(String),

    /// Le démon est injoignable.
    #[error("démon injoignable sur {address}")]
    Unreachable {
        /// L'adresse tentée.
        address: String,
        /// La cause système.
        #[source]
        source: std::io::Error,
    },

    /// Échec d'entrée/sortie une fois la connexion établie.
    #[error("erreur de transport")]
    Io(#[from] std::io::Error),

    /// Le démon a refusé la requête.
    #[error("le démon a répondu {0} — jeton absent ou invalide ?")]
    Status(u16),

    /// La réponse n'est pas exploitable.
    #[error("réponse illisible du démon")]
    Malformed,

    /// Le démon a rendu une erreur JSON-RPC.
    #[error("{}", .0.message)]
    Rpc(jsonrpc::Error),
}

/// Alias de commodité.
pub type Result<T> = std::result::Result<T, Error>;

/// Une connexion vers l'API d'un démon.
///
/// La connexion est **réutilisée** d'un appel à l'autre. Ouvrir un TCP par requête ajouterait
/// une poignée de main à chaque appel, ce qui se verrait sur une liste paginée.
#[derive(Debug)]
pub struct Client {
    stream: BufReader<TcpStream>,
    host: String,
    token: String,
    next_id: u64,
}

impl Client {
    /// Ouvre une connexion vers `host` — `hôte:port`, le port valant [`DEFAULT_PORT`] s'il
    /// est omis.
    ///
    /// # Errors
    ///
    /// [`Error::WouldSendTokenInClear`] si l'hôte n'est pas local, [`Error::Unreachable`] si
    /// le démon ne répond pas.
    pub fn connect(host: &str, token: &str) -> Result<Self> {
        let target = if host.contains(':') {
            host.to_owned()
        } else {
            format!("{host}:{DEFAULT_PORT}")
        };

        let address = target
            .to_socket_addrs()
            .map_err(|_| Error::Address(target.clone()))?
            .next()
            .ok_or_else(|| Error::Address(target.clone()))?;

        // La règle, avant d'ouvrir quoi que ce soit : on ne met pas un secret de boîte mail
        // sur le fil en clair. Vérifiée sur l'adresse **résolue**, pas sur le nom : un nom
        // qui pointe ailleurs que sur la machine n'est pas local, quoi qu'il ressemble.
        if !address.ip().is_loopback() {
            return Err(Error::WouldSendTokenInClear(address.to_string()));
        }

        let stream = TcpStream::connect(address).map_err(|source| Error::Unreachable {
            address: target.clone(),
            source,
        })?;
        // Sans ça, Nagle retient un petit écrit jusqu'à l'acquittement du précédent.
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;

        Ok(Self {
            stream: BufReader::new(stream),
            host: target,
            token: token.to_owned(),
            next_id: 1,
        })
    }

    /// Appelle une méthode et rend son résultat.
    ///
    /// # Errors
    ///
    /// [`Error::Rpc`] si le démon rend une erreur de protocole, les autres variantes si le
    /// transport échoue.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let body = serde_json::to_string(&serde_json::json!({
            "jsonrpc": jsonrpc::VERSION,
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|_| Error::Malformed)?;

        let request = format!(
            "POST /api HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}",
            host = self.host,
            token = self.token,
            len = body.len(),
        );

        self.stream.get_mut().write_all(request.as_bytes())?;
        self.stream.get_mut().flush()?;

        let (status, payload) = self.read_response()?;
        if status != 200 {
            return Err(Error::Status(status));
        }

        let value: Value = serde_json::from_str(&payload).map_err(|_| Error::Malformed)?;
        // Une réponse dépareillée veut dire que le flux est désynchronisé : tout ce qui
        // suivrait serait faux sans qu'on le voie.
        if value.get("id") != Some(&Value::from(id)) {
            return Err(Error::Malformed);
        }
        if let Some(error) = value.get("error") {
            return Err(Error::Rpc(
                serde_json::from_value(error.clone()).map_err(|_| Error::Malformed)?,
            ));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Appelle une méthode et désérialise son résultat.
    ///
    /// # Errors
    ///
    /// Comme [`Client::call`], plus [`Error::Malformed`] si le résultat ne correspond pas au
    /// type attendu — ce qui veut dire que le démon ne parle pas la même version du contrat.
    pub fn typed<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<T> {
        serde_json::from_value(self.call(method, params)?).map_err(|_| Error::Malformed)
    }

    /// Lit une réponse HTTP : statut et corps.
    ///
    /// Le corps se lit par sa `Content-Length`, exactement, pour que la connexion reste
    /// utilisable ensuite.
    fn read_response(&mut self) -> Result<(u16, String)> {
        let mut line = String::new();
        if self.stream.read_line(&mut line)? == 0 {
            return Err(Error::Malformed);
        }
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or(Error::Malformed)?;

        let mut length = None;
        loop {
            let mut header = String::new();
            let read = self.stream.read_line(&mut header)?;
            if read == 0 || header == "\r\n" || header == "\n" {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().ok();
            }
        }

        let body = match length {
            None | Some(0) => String::new(),
            Some(len) if len > MAX_RESPONSE => return Err(Error::Malformed),
            Some(len) => {
                let mut buffer = vec![0u8; len];
                self.stream.read_exact(&mut buffer)?;
                String::from_utf8(buffer).map_err(|_| Error::Malformed)?
            }
        };
        Ok((status, body))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_non_local_host_is_refused_before_anything_is_sent() {
        // La garantie : le refus arrive **avant** l'ouverture de la socket, donc le jeton ne
        // part jamais, même partiellement.
        let error = Client::connect("192.0.2.1:7847", "un-jeton-secret").unwrap_err();
        assert!(matches!(error, Error::WouldSendTokenInClear(_)));
        // Et le message ne recopie pas le jeton qu'il vient de refuser d'envoyer.
        assert!(!error.to_string().contains("un-jeton-secret"));
    }

    #[test]
    fn the_refusal_says_what_to_do_instead() {
        let error = Client::connect("203.0.113.7:7847", "jeton").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("tunnel"), "{message}");
        assert!(message.contains("127.0.0.1"), "{message}");
    }

    #[test]
    fn a_loopback_host_gets_past_the_check_and_fails_on_connection() {
        // Rien n'écoute : l'erreur doit être « injoignable », pas « refusé ». C'est ce qui
        // distingue une règle de sécurité d'un démon éteint, et un utilisateur a besoin de
        // savoir lequel des deux le concerne.
        let error = Client::connect("127.0.0.1:1", "jeton").unwrap_err();
        assert!(matches!(error, Error::Unreachable { .. }), "{error:?}");
    }

    #[test]
    fn the_default_port_is_used_when_none_is_given() {
        // Pas de port dans l'adresse : le défaut s'applique, et la vérification de localité
        // aussi.
        let error = Client::connect("localhost", "jeton").unwrap_err();
        assert!(
            matches!(error, Error::Unreachable { .. } | Error::Address(_)),
            "{error:?}"
        );
    }

    #[test]
    fn an_unresolvable_host_is_named_as_such() {
        let error = Client::connect("hote.invalide.qui.nexiste.pas:7847", "jeton").unwrap_err();
        assert!(matches!(error, Error::Address(_)), "{error:?}");
    }
}

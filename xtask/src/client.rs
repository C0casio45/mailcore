//! Un client JSON-RPC sur HTTP, minimal, pour mesurer le démon de l'extérieur.
//!
//! Écrit à la main sur une `TcpStream` plutôt qu'avec `reqwest`. Trois raisons :
//!
//! - **Rien n'est masqué.** Ce qui part sur le fil est visible dans ce fichier, ce qui
//!   compte quand on mesure une latence : un client généreux en interception, en pool ou en
//!   nouvelle tentative mesurerait sa propre bibliothèque autant que le démon.
//! - **La connexion est réutilisée**, comme le fera l'UI. Ouvrir un TCP par requête
//!   ajouterait une poignée de main à chaque mesure et gonflerait un p95 dont le réseau n'est
//!   pas responsable.
//! - Pas d'arbre de dépendances complet dans l'outillage pour poster du JSON à notre propre
//!   serveur.
//!
//! Il ne parle pas TLS. Les mesures se prennent en clair sur le bouclage, où il n'y en a pas
//! besoin ; le relevé bout en bout du déploiement de référence — machine à machine, à travers
//! un tunnel — est de toute façon à prendre par l'utilisateur sur ses deux machines.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Une connexion ouverte vers `/api`.
#[derive(Debug)]
pub struct Client {
    stream: BufReader<TcpStream>,
    address: SocketAddr,
    token: String,
    /// Compteur d'identifiants JSON-RPC. Vérifié dans la réponse : une réponse dépareillée
    /// voudrait dire que le flux est désynchronisé, et toutes les mesures suivantes seraient
    /// fausses sans qu'on le voie.
    next_id: u64,
}

impl Client {
    /// Ouvre une connexion et la garde.
    ///
    /// # Errors
    ///
    /// Si le démon n'accepte pas la connexion.
    pub fn connect(address: SocketAddr, token: &str) -> Result<Self> {
        let stream =
            TcpStream::connect(address).with_context(|| format!("connexion à {address}"))?;
        // Sans ça, Nagle retient un petit écrit jusqu'à l'acquittement du précédent et
        // ajoute des dizaines de millisecondes à une mesure de latence.
        stream.set_nodelay(true).context("TCP_NODELAY")?;
        stream
            .set_read_timeout(Some(Duration::from_secs(180)))
            .context("délai de lecture")?;
        Ok(Self {
            stream: BufReader::new(stream),
            address,
            token: token.to_owned(),
            next_id: 1,
        })
    }

    /// Appelle une méthode et rend son résultat.
    ///
    /// # Errors
    ///
    /// Si le transport échoue, si le statut n'est pas `200`, ou si la réponse porte une
    /// erreur JSON-RPC.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let body = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .context("encodage de la requête")?;

        let request = format!(
            "POST /api HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}",
            host = self.address,
            token = self.token,
            len = body.len(),
        );

        self.stream
            .get_mut()
            .write_all(request.as_bytes())
            .context("envoi de la requête")?;
        self.stream.get_mut().flush().context("vidage")?;

        let (status, payload) = self.read_response()?;
        if status != 200 {
            bail!("le démon a répondu {status} sur {method}");
        }

        let value: Value = serde_json::from_str(&payload)
            .with_context(|| format!("réponse illisible sur {method}"))?;

        if value["id"] != id {
            bail!(
                "réponse dépareillée sur {method} : identifiant {} pour la requête {id}",
                value["id"]
            );
        }
        if let Some(error) = value.get("error") {
            bail!(
                "{method} a échoué : {} ({})",
                error["message"].as_str().unwrap_or("sans message"),
                error["code"]
            );
        }
        Ok(value["result"].clone())
    }

    /// Lit une réponse HTTP : statut et corps.
    ///
    /// Le corps se lit par sa `Content-Length`, exactement, pour que la connexion reste
    /// utilisable pour la requête suivante. C'est le prix du *keep-alive*, et il est mérité :
    /// sans lui chaque mesure paierait une poignée de main TCP.
    fn read_response(&mut self) -> Result<(u16, String)> {
        let mut line = String::new();
        self.stream
            .read_line(&mut line)
            .context("lecture du statut")?;
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .with_context(|| format!("statut illisible : {line:?}"))?;

        let mut length = None;
        loop {
            let mut header = String::new();
            let read = self
                .stream
                .read_line(&mut header)
                .context("lecture d'un en-tête")?;
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
            Some(0) | None => String::new(),
            Some(len) => {
                let mut buffer = vec![0u8; len];
                self.stream
                    .read_exact(&mut buffer)
                    .context("lecture du corps")?;
                String::from_utf8(buffer).context("corps non UTF-8")?
            }
        };
        Ok((status, body))
    }
}

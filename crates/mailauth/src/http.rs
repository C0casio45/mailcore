//! Un `POST` HTTPS, et rien d'autre.
//!
//! ## Pourquoi ce n'est pas `reqwest`
//!
//! Trois raisons, et la première suffirait.
//!
//! **Le critère 5 de `docs/PHASE-2.md`** dit que l'application se connecte aux serveurs que
//! l'utilisateur a configurés et à rien d'autre, et `xtask/tests/reseau.rs` le verrouille :
//! aucun crate livré ne dépend d'un client HTTP. Ce test existe pour attraper exactement le
//! geste qu'on ferait ici — ajouter une bibliothèque généreuse et cesser de savoir ce qui
//! sort.
//!
//! **Un seul appel existe** : un `POST` de formulaire vers un point de terminaison de jeton,
//! qui rend du JSON. Un client généraliste apporterait des redirections, un pool de
//! connexions, des cookies, un décompresseur — autant de chemins que personne n'exercera et
//! qui font partie de la surface d'attaque quand même.
//!
//! **La même raison que `mailapi::client`**, qui est écrit à la main pour le même genre
//! d'appel. Deux clients HTTP dans le même dépôt, dont un seul écrit ici, serait le pire des
//! deux mondes.
//!
//! ## Ce qu'il ne fait pas, et c'est délibéré
//!
//! Ni redirection — un point de terminaison de jeton qui redirige est un point de terminaison
//! qu'on ne veut pas suivre, parce que la cible pourrait être ailleurs. Ni `keep-alive` : un
//! appel, une connexion, `Connection: close`. Ni corps découpé en morceaux à l'envoi ; le
//! nôtre fait quelques centaines d'octets.
//!
//! Il **lit** un corps découpé en morceaux, parce qu'un serveur a le droit d'en envoyer un et
//! que le refuser rendrait un échec incompréhensible.

use std::io::{BufRead, BufReader, Read, Write};

use crate::Error;

/// Plafond d'un corps de réponse.
///
/// 256 Kio. Une réponse de jeton fait quelques centaines d'octets ; ce plafond n'est pas une
/// limite fonctionnelle, c'est une défense. Sans lui, un serveur — compromis, ou simplement
/// mal configuré derrière un portail captif — ferait allouer sans borne.
const BODY_LIMIT: usize = 256 * 1024;

/// Plafond d'une ligne d'en-tête, et nombre maximum d'en-têtes.
///
/// Même raison : un serveur qui n'envoie jamais de fin de ligne, ou qui en envoie un million.
const HEADER_LINE_LIMIT: usize = 16 * 1024;
const HEADER_COUNT_LIMIT: usize = 100;

/// Ce qu'un serveur a répondu.
#[derive(Debug, Clone)]
pub struct Response {
    /// Le code d'état.
    pub status: u16,
    /// Le corps, tel quel.
    ///
    /// Une `String` et non des octets : le seul appelant attend du JSON, et un corps qui n'est
    /// pas de l'UTF-8 est une réponse qu'on ne saura pas lire de toute façon. Les octets
    /// invalides deviennent des caractères de remplacement plutôt que de faire échouer la
    /// lecture — un message d'erreur à moitié lisible vaut mieux que pas de message.
    pub body: String,
}

/// Encode un formulaire `application/x-www-form-urlencoded`.
///
/// ## L'encodage est fait ici, et il est strict
///
/// Un jeton de rafraîchissement de Google contient des `/` et des `+`. Les laisser passer
/// tels quels dans un corps de formulaire les fait interpréter comme des séparateurs, et
/// l'échange échoue avec un message qui parle de jeton invalide — pas d'encodage.
///
/// La liste des caractères **non** encodés est celle des « unreserved » de la RFC 3986. Tout
/// le reste part en `%XX`, y compris l'espace : `+` pour l'espace est une tolérance des
/// serveurs, pas une règle, et `%20` est accepté partout.
#[must_use]
pub fn form_encode(fields: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (key, value) in fields {
        if !out.is_empty() {
            out.push('&');
        }
        percent(&mut out, key);
        out.push('=');
        percent(&mut out, value);
    }
    out
}

/// Ajoute une valeur encodée en pourcent.
fn percent(out: &mut String, value: &str) {
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
}

/// Envoie un `POST` de formulaire sur un flux **déjà établi**, et lit la réponse.
///
/// Le flux est un paramètre pour la même raison que dans `mailsync` : c'est ce qui rend cette
/// fonction testable contre un serveur local en clair, alors que le chemin de production est
/// chiffré et refuse tout le reste.
///
/// # Errors
///
/// [`Error::Network`] sur le socket, [`Error::Protocol`] sur une réponse qui ne suit pas
/// HTTP/1.1 ou qui dépasse un plafond.
pub fn post_form<S: Read + Write>(
    stream: S,
    host: &str,
    path: &str,
    fields: &[(&str, &str)],
) -> crate::Result<Response> {
    let body = form_encode(fields);
    let mut io = BufReader::new(stream);

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\n\
         Accept: application/json\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    // Le corps porte le secret client et le jeton de rafraîchissement : **rien de tout ça
    // n'est journalisé**. La requête n'est pas tracée, et son contenu n'apparaît dans aucun
    // message d'erreur.
    io.get_mut()
        .write_all(request.as_bytes())
        .map_err(Error::Network)?;
    io.get_mut().flush().map_err(Error::Network)?;

    read_response(&mut io)
}

/// Envoie un `POST` de formulaire **en HTTPS**, et lit la réponse.
///
/// ## C'est le seul endroit de ce crate qui ouvre une connexion
///
/// Et il est déclaré comme tel dans `xtask/tests/reseau.rs`. Ce test a échoué en ajoutant
/// cette fonction, ce qui est exactement son travail : la surface réseau d'un crate livré ne
/// grandit pas sans que quelqu'un l'écrive noir sur blanc.
///
/// L'hôte n'est pas choisi ici : il vient de [`crate::oauth::Provider`], donc d'une constante
/// du code, pas d'une donnée. Un point de terminaison de jeton n'est pas configurable par
/// l'utilisateur, et il ne doit pas l'être — ce serait le moyen le plus simple de faire
/// envoyer un jeton de rafraîchissement ailleurs.
///
/// ## Aucune dérogation TLS
///
/// La même position que `mailsync::tls`, et pour un enjeu plus grand : ce canal transporte le
/// secret client et le jeton de rafraîchissement. Pas d'option pour ignorer un certificat, pas
/// de repli en clair.
///
/// # Errors
///
/// [`Error::InvalidHost`], [`Error::Network`], [`Error::Tls`], [`Error::Protocol`].
pub fn post_form_tls(
    host: &str,
    port: u16,
    path: &str,
    fields: &[(&str, &str)],
) -> crate::Result<Response> {
    use std::net::ToSocketAddrs as _;
    use std::sync::Arc;

    let name = rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|_| {
        Error::InvalidHost {
            host: host.to_owned(),
        }
    })?;

    // Une machine à double pile rend une adresse v6 et une v4 ; la première peut échouer là où
    // la seconde marche. Essayer toutes les adresses avant de renoncer.
    let mut last = None;
    let mut stream = None;
    for address in (host, port).to_socket_addrs().map_err(Error::Network)? {
        match std::net::TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(socket) => {
                socket
                    .set_read_timeout(Some(READ_TIMEOUT))
                    .map_err(Error::Network)?;
                socket
                    .set_write_timeout(Some(READ_TIMEOUT))
                    .map_err(Error::Network)?;
                stream = Some(socket);
                break;
            }
            Err(error) => last = Some(error),
        }
    }
    let stream = stream.ok_or_else(|| {
        Error::Network(last.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "aucune adresse pour cet hôte",
            )
        }))
    })?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = {
        use rustls_platform_verifier::BuilderVerifierExt as _;
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|source| Error::Tls {
                reason: source.to_string(),
            })?
            .with_platform_verifier()
            .map_err(|source| Error::Tls {
                reason: format!("magasin de confiance du système illisible : {source}"),
            })?
            .with_no_client_auth()
    };
    let connection =
        rustls::ClientConnection::new(Arc::new(config), name).map_err(|source| Error::Tls {
            reason: source.to_string(),
        })?;

    post_form(
        rustls::StreamOwned::new(connection, stream),
        host,
        path,
        fields,
    )
}

/// Délai d'établissement de la connexion.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Délai de lecture et d'écriture.
///
/// Court : une réponse de jeton fait quelques centaines d'octets, et un point de terminaison
/// qui met plus de trente secondes est en panne. Attendre plus longtemps ne ferait que retarder
/// le diagnostic.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Lit une réponse HTTP/1.1.
fn read_response<S: Read>(io: &mut BufReader<S>) -> crate::Result<Response> {
    let status_line = read_line(io)?;
    // `HTTP/1.1 200 OK` — on ne veut que le code.
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|it| it.parse().ok())
        .ok_or_else(|| Error::Protocol {
            reason: format!("ligne d'état illisible : {}", head(&status_line)),
        })?;

    let mut length: Option<usize> = None;
    let mut chunked = false;
    for _ in 0..HEADER_COUNT_LIMIT {
        let line = read_line(io)?;
        if line.is_empty() {
            return read_body(io, status, length, chunked);
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            length = value.parse().ok().filter(|it| *it <= BODY_LIMIT);
            if length.is_none() {
                return Err(Error::Protocol {
                    reason: format!("Content-Length inexploitable : {}", head(value)),
                });
            }
        }
        if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }
    Err(Error::Protocol {
        reason: format!("plus de {HEADER_COUNT_LIMIT} en-têtes"),
    })
}

/// Lit le corps, selon ce que les en-têtes ont annoncé.
fn read_body<S: Read>(
    io: &mut BufReader<S>,
    status: u16,
    length: Option<usize>,
    chunked: bool,
) -> crate::Result<Response> {
    let bytes = if chunked {
        read_chunked(io)?
    } else if let Some(length) = length {
        let mut buffer = vec![0_u8; length];
        io.read_exact(&mut buffer).map_err(Error::Network)?;
        buffer
    } else {
        // Ni longueur ni découpage : le corps va jusqu'à la fermeture. C'est légal avec
        // `Connection: close`, que la requête demande.
        let mut buffer = Vec::new();
        io.take(BODY_LIMIT as u64 + 1)
            .read_to_end(&mut buffer)
            .map_err(Error::Network)?;
        if buffer.len() > BODY_LIMIT {
            return Err(Error::Protocol {
                reason: format!("corps de plus de {BODY_LIMIT} octets"),
            });
        }
        buffer
    };

    Ok(Response {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// Lit un corps découpé en morceaux.
fn read_chunked<S: Read>(io: &mut BufReader<S>) -> crate::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line = read_line(io)?;
        // La taille est en hexadécimal, éventuellement suivie d'une extension après `;`.
        let size = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size, 16).map_err(|_| Error::Protocol {
            reason: format!("taille de morceau illisible : {}", head(size)),
        })?;
        if size == 0 {
            // Le morceau vide termine le corps ; ce qui suit est une remorque qu'on ignore.
            return Ok(out);
        }
        if out.len() + size > BODY_LIMIT {
            return Err(Error::Protocol {
                reason: format!("corps découpé de plus de {BODY_LIMIT} octets"),
            });
        }
        let mut chunk = vec![0_u8; size];
        io.read_exact(&mut chunk).map_err(Error::Network)?;
        out.extend_from_slice(&chunk);
        // Le `CRLF` qui suit chaque morceau.
        let _ = read_line(io)?;
    }
}

/// Lit une ligne, sans le `CRLF`, en refusant les lignes sans fin.
fn read_line<S: Read>(io: &mut BufReader<S>) -> crate::Result<String> {
    let mut raw = Vec::new();
    let read = io
        .take(HEADER_LINE_LIMIT as u64 + 1)
        .read_until(b'\n', &mut raw)
        .map_err(Error::Network)?;
    if read == 0 {
        return Err(Error::Protocol {
            reason: "le serveur a fermé la connexion".to_owned(),
        });
    }
    if read > HEADER_LINE_LIMIT {
        return Err(Error::Protocol {
            reason: format!("ligne de plus de {HEADER_LINE_LIMIT} octets"),
        });
    }
    while raw.last().is_some_and(|it| *it == b'\n' || *it == b'\r') {
        raw.pop();
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// Les premiers caractères d'un texte, pour un message d'erreur.
///
/// Borné, et sur une frontière de caractère : trancher à l'octet couperait un caractère
/// multi-octets en deux et paniquerait.
fn head(text: &str) -> String {
    let cut = text.char_indices().nth(60).map_or(text.len(), |(at, _)| at);
    text[..cut].to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Un flux qui rend une réponse écrite d'avance, et garde ce qu'on lui a envoyé.
    ///
    /// Plus précis qu'un vrai serveur pour ces tests : on veut pouvoir répondre **exactement**
    /// ce qu'on veut, y compris des choses qu'aucun serveur correct n'enverrait.
    struct Canned {
        reply: std::io::Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Canned {
        fn new(reply: &str) -> Self {
            Self {
                reply: std::io::Cursor::new(reply.as_bytes().to_vec()),
                sent: Vec::new(),
            }
        }
    }

    impl Read for Canned {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.reply.read(buffer)
        }
    }

    impl Write for Canned {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.sent.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_unreserved_characters_are_not_encoded() {
        assert_eq!(form_encode(&[("a-b_c.d~e", "f")]), "a-b_c.d~e=f");
    }

    #[test]
    fn a_refresh_token_with_slashes_and_pluses_is_encoded() {
        // **Le cas réel.** Un jeton de Google contient des `/` et des `+` : les laisser
        // passer les fait lire comme des séparateurs, et l'échange échoue en parlant de
        // jeton invalide plutôt que d'encodage.
        let encoded = form_encode(&[("refresh_token", "1//0abc+def/ghi=")]);
        assert_eq!(encoded, "refresh_token=1%2F%2F0abc%2Bdef%2Fghi%3D");
    }

    #[test]
    fn a_space_becomes_percent_twenty_not_a_plus() {
        // `+` pour l'espace est une tolérance des serveurs, pas une règle. `%20` passe
        // partout, et il n'y a aucune raison de parier.
        assert_eq!(form_encode(&[("scope", "a b")]), "scope=a%20b");
    }

    #[test]
    fn a_unicode_value_is_encoded_byte_by_byte() {
        // L'encodage en pourcent porte sur des octets UTF-8, pas sur des caractères.
        assert_eq!(form_encode(&[("x", "é")]), "x=%C3%A9");
    }

    #[test]
    fn fields_are_joined_by_an_ampersand() {
        assert_eq!(form_encode(&[("a", "1"), ("b", "2")]), "a=1&b=2");
        assert_eq!(form_encode(&[]), "");
    }

    #[test]
    fn a_plain_response_is_read() {
        let canned = Canned::new(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"ok\":true}\r\n",
        );
        let response = post_form(canned, "exemple.fr", "/token", &[("a", "1")]).unwrap();
        assert_eq!(response.status, 200);
        assert!(response.body.starts_with("{\"ok\":true}"));
    }

    #[test]
    fn the_request_carries_what_it_should_and_nothing_more() {
        let mut canned = Canned::new("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        let mut sent = Vec::new();
        {
            // On récupère ce qui a été écrit en réutilisant le flux après l'appel.
            let response =
                post_form(&mut canned, "oauth2.exemple.fr", "/token", &[("a", "1")]).unwrap();
            assert_eq!(response.status, 200);
            sent.extend_from_slice(&canned.sent);
        }
        let text = String::from_utf8_lossy(&sent);

        assert!(text.starts_with("POST /token HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("Host: oauth2.exemple.fr\r\n"), "{text}");
        assert!(text.contains("Content-Length: 3\r\n"), "{text}");
        // `Connection: close` n'est pas décoratif : sans lui, un corps sans longueur ne se
        // termine jamais.
        assert!(text.contains("Connection: close\r\n"), "{text}");
        assert!(text.ends_with("a=1"), "{text}");
        // Ni cookie, ni compression, ni redirection : rien qu'on n'exercerait pas.
        assert!(!text.to_lowercase().contains("cookie"), "{text}");
        assert!(!text.to_lowercase().contains("accept-encoding"), "{text}");
    }

    #[test]
    fn an_error_status_is_returned_not_hidden() {
        // Un `400` d'un point de terminaison de jeton **porte le diagnostic** dans son corps.
        // Le transformer en erreur de transport perdrait la seule information utile.
        // La longueur annoncée est celle du corps, comptée : 25 octets. Le premier jet en
        // déclarait 31 et le test échouait sur un `read_exact` inassouvi — le code avait
        // raison, la donnée de test était fausse.
        let body = r#"{"error":"invalid_grant"}"#;
        assert_eq!(body.len(), 25);
        let canned = Canned::new(&format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
        let response = post_form(canned, "x", "/token", &[]).unwrap();
        assert_eq!(response.status, 400);
        assert!(response.body.contains("invalid_grant"));
    }

    #[test]
    fn a_chunked_body_is_reassembled() {
        // Un serveur a le droit d'en envoyer un, et le refuser rendrait un échec
        // incompréhensible.
        let canned = Canned::new(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n4\r\n1}\r\n\r\n0\r\n\r\n",
        );
        let response = post_form(canned, "x", "/token", &[]).unwrap();
        assert_eq!(response.body, "{\"a\":1}\r\n");
    }

    #[test]
    fn a_body_without_a_length_reads_to_the_close() {
        // Légal avec `Connection: close`, que la requête demande toujours.
        let canned = Canned::new("HTTP/1.1 200 OK\r\n\r\n{\"sans\":\"longueur\"}");
        let response = post_form(canned, "x", "/token", &[]).unwrap();
        assert_eq!(response.body, "{\"sans\":\"longueur\"}");
    }

    // ------------------------------------------------------------------
    // Entrée hostile. Le corps vient du réseau.
    // ------------------------------------------------------------------

    #[test]
    fn an_unreadable_status_line_is_refused() {
        for reply in [
            "",
            "pas du http\r\n\r\n",
            "HTTP/1.1\r\n\r\n",
            "HTTP/1.1 abc\r\n\r\n",
        ] {
            let outcome = post_form(Canned::new(reply), "x", "/token", &[]);
            assert!(
                matches!(outcome, Err(Error::Protocol { .. })),
                "accepté : {reply:?}"
            );
        }
    }

    #[test]
    fn an_absurd_content_length_is_refused_before_allocating() {
        // **La défense, pas la limite.** `Content-Length: 999999999999` ne doit pas faire
        // réserver un téraoctet pour découvrir ensuite que rien ne suit.
        let canned = Canned::new("HTTP/1.1 200 OK\r\nContent-Length: 999999999999\r\n\r\n");
        assert!(matches!(
            post_form(canned, "x", "/token", &[]),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn a_negative_content_length_is_refused() {
        let canned = Canned::new("HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n");
        assert!(matches!(
            post_form(canned, "x", "/token", &[]),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn a_body_longer_than_the_ceiling_is_refused() {
        let big = "x".repeat(BODY_LIMIT + 10);
        let canned = Canned::new(&format!("HTTP/1.1 200 OK\r\n\r\n{big}"));
        assert!(matches!(
            post_form(canned, "x", "/token", &[]),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn a_line_without_an_end_is_refused() {
        let endless = "x".repeat(HEADER_LINE_LIMIT + 10);
        let canned = Canned::new(&format!("HTTP/1.1 200 OK\r\n{endless}"));
        assert!(matches!(
            post_form(canned, "x", "/token", &[]),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn a_flood_of_headers_is_refused() {
        let mut reply = String::from("HTTP/1.1 200 OK\r\n");
        for index in 0..(HEADER_COUNT_LIMIT + 10) {
            reply.push_str(&format!("X-Bidon-{index}: valeur\r\n"));
        }
        reply.push_str("\r\n");
        assert!(matches!(
            post_form(Canned::new(&reply), "x", "/token", &[]),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn an_unreadable_chunk_size_is_refused() {
        let canned =
            Canned::new("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\npas-un-nombre\r\n");
        assert!(matches!(
            post_form(canned, "x", "/token", &[]),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn a_chunked_body_beyond_the_ceiling_is_refused() {
        let canned = Canned::new(&format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            BODY_LIMIT + 10
        ));
        assert!(matches!(
            post_form(canned, "x", "/token", &[]),
            Err(Error::Protocol { .. })
        ));
    }

    #[test]
    fn a_body_that_is_not_utf8_still_yields_a_response() {
        // Un corps illisible est une réponse qu'on ne saura pas exploiter, mais le dire vaut
        // mieux que d'échouer sur l'encodage : le code d'état, lui, est utile.
        let mut reply = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 3\r\n\r\n".to_vec();
        reply.extend_from_slice(&[0xFF, 0xFE, 0x00]);
        let canned = Canned {
            reply: std::io::Cursor::new(reply),
            sent: Vec::new(),
        };
        let response = post_form(canned, "x", "/token", &[]).unwrap();
        assert_eq!(response.status, 500);
    }
}

//! Encadrement JSON-RPC 2.0 — le même protocole que MCP, délibérément.
//!
//! Une seule couche de sérialisation à maintenir pour les deux surfaces du démon
//! (`docs/ARCHITECTURE.md`). Ce module ne connaît aucune méthode de mailcore : il transporte
//! un nom et un objet de paramètres, et rend un résultat ou une erreur.
//!
//! ## Ce qui n'est pas là, et pourquoi
//!
//! **Pas de lots.** JSON-RPC autorise un tableau de requêtes dans un seul message. On ne
//! l'accepte pas : un lot contenant un `store.wait` retiendrait toutes les autres réponses
//! jusqu'à son expiration, et un client qui veut du parallélisme a déjà des connexions
//! concurrentes. Un tableau reçu est refusé proprement, pas ignoré.
//!
//! **Pas de requête côté serveur.** Le démon ne pose jamais de question à un client. La
//! seule chose qu'un client attend de lui est une réponse, ce qui rend l'appariement des
//! `id` trivial et supprime toute une classe d'interblocages.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// La version de protocole, seule valeur acceptée dans le champ `jsonrpc`.
pub const VERSION: &str = "2.0";

/// Erreur d'analyse : le message n'est pas du JSON.
pub const PARSE_ERROR: i32 = -32_700;
/// Le message est du JSON, mais pas une requête JSON-RPC exploitable.
pub const INVALID_REQUEST: i32 = -32_600;
/// La méthode demandée n'existe pas.
pub const METHOD_NOT_FOUND: i32 = -32_601;
/// Les paramètres ne correspondent pas à ce que la méthode attend.
pub const INVALID_PARAMS: i32 = -32_602;
/// La méthode existe, les paramètres sont bons, l'exécution a échoué.
pub const INTERNAL_ERROR: i32 = -32_603;

/// Une requête entrante.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    /// Doit valoir [`VERSION`]. Absent toléré à la lecture : un client qui l'oublie a un
    /// bug bénin, et refuser sa requête ne l'aiderait pas à le trouver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<String>,
    /// L'identifiant à recopier dans la réponse. Absent = notification : le client ne veut
    /// pas de réponse, et il n'en recevra pas, même en cas d'erreur.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// Le nom de la méthode. Voir [`crate::method`].
    pub method: String,
    /// Les paramètres. `null` quand la méthode n'en prend pas.
    #[serde(default)]
    pub params: Value,
}

/// Une réponse sortante.
///
/// `result` et `error` sont exclusifs — la spécification l'impose — d'où l'énumération
/// aplatie plutôt que deux `Option` qu'on pourrait remplir toutes les deux.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Toujours [`VERSION`].
    pub jsonrpc: String,
    /// L'identifiant de la requête, recopié tel quel.
    pub id: Value,
    /// Le résultat ou l'erreur.
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// Le corps d'une réponse : l'un ou l'autre, jamais les deux.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Outcome {
    /// L'appel a réussi.
    #[serde(rename = "result")]
    Result(Value),
    /// L'appel a échoué.
    #[serde(rename = "error")]
    Error(Error),
}

/// Une erreur JSON-RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Error {
    /// Un des codes déclarés dans ce module.
    pub code: i32,
    /// Une phrase pour un développeur.
    ///
    /// **Jamais de contenu de message, ni de chemin de fichier, ni de jeton** : une erreur
    /// traverse le réseau et finit dans un journal côté client (`docs/PRIVACY.md`,
    /// section 8).
    pub message: String,
}

impl Error {
    /// Une erreur avec un code et un message.
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// La méthode n'existe pas.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("méthode inconnue : {method}"))
    }

    /// Les paramètres sont invalides.
    #[must_use]
    pub fn invalid_params(detail: impl std::fmt::Display) -> Self {
        Self::new(INVALID_PARAMS, format!("paramètres invalides : {detail}"))
    }
}

impl Response {
    /// Une réponse en succès.
    #[must_use]
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: VERSION.to_owned(),
            id,
            outcome: Outcome::Result(result),
        }
    }

    /// Une réponse en erreur.
    #[must_use]
    pub fn failure(id: Value, error: Error) -> Self {
        Self {
            jsonrpc: VERSION.to_owned(),
            id,
            outcome: Outcome::Error(error),
        }
    }

    /// Vrai si la réponse porte une erreur.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self.outcome, Outcome::Error(_))
    }
}

/// Analyse un message entrant.
///
/// # Errors
///
/// Rend l'[`Error`] à renvoyer au client — avec un `id` nul, puisqu'une requête illisible
/// n'a pas d'identifiant exploitable.
pub fn parse(message: &str) -> std::result::Result<Request, Error> {
    let value: Value = serde_json::from_str(message)
        .map_err(|source| Error::new(PARSE_ERROR, format!("JSON illisible : {source}")))?;

    if value.is_array() {
        return Err(Error::new(
            INVALID_REQUEST,
            "les lots ne sont pas acceptés : une requête par message",
        ));
    }

    let request: Request = serde_json::from_value(value)
        .map_err(|source| Error::new(INVALID_REQUEST, format!("requête invalide : {source}")))?;

    match request.jsonrpc.as_deref() {
        None | Some(VERSION) => Ok(request),
        Some(other) => Err(Error::new(
            INVALID_REQUEST,
            format!("version de protocole non prise en charge : {other}"),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_request_parses() {
        let request = parse(r#"{"jsonrpc":"2.0","id":1,"method":"folders.list"}"#).unwrap();
        assert_eq!(request.method, "folders.list");
        assert_eq!(request.id, Some(Value::from(1)));
        assert!(request.params.is_null());
    }

    #[test]
    fn a_notification_has_no_id() {
        let request = parse(r#"{"jsonrpc":"2.0","method":"store.revision"}"#).unwrap();
        assert!(request.id.is_none());
    }

    #[test]
    fn broken_json_is_a_parse_error() {
        assert_eq!(parse("{not json").unwrap_err().code, PARSE_ERROR);
    }

    #[test]
    fn a_batch_is_refused_rather_than_half_served() {
        // Un lot contenant un long-poll retiendrait toutes les autres réponses.
        let error = parse(r#"[{"jsonrpc":"2.0","id":1,"method":"folders.list"}]"#).unwrap_err();
        assert_eq!(error.code, INVALID_REQUEST);
    }

    #[test]
    fn a_missing_method_is_an_invalid_request() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1}"#).unwrap_err().code,
            INVALID_REQUEST
        );
    }

    #[test]
    fn an_unknown_protocol_version_is_refused() {
        let error = parse(r#"{"jsonrpc":"1.0","id":1,"method":"folders.list"}"#).unwrap_err();
        assert_eq!(error.code, INVALID_REQUEST);
    }

    #[test]
    fn a_response_carries_result_or_error_but_never_both() {
        let ok = serde_json::to_value(Response::success(Value::from(1), Value::from("x"))).unwrap();
        assert!(ok.get("result").is_some() && ok.get("error").is_none());

        let bad = serde_json::to_value(Response::failure(
            Value::Null,
            Error::method_not_found("nope"),
        ))
        .unwrap();
        assert!(bad.get("error").is_some() && bad.get("result").is_none());
    }
}

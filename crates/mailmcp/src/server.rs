//! Le serveur `rmcp` et ses cinq outils.
//!
//! **Deux transports, tous les deux de phase 1.** Un serveur MCP joignable seulement en
//! stdio n'est utilisable que par un client tournant à côté du démon, ce qui exclut le
//! déploiement de référence de `docs/PHASE-1.md`. Le transport est monté par `maild` ; ce
//! module ne connaît que les outils.
//!
//! ## Pourquoi un `Mutex` autour de la boîte
//!
//! `rusqlite::Connection` est `Send` mais pas `Sync` : deux tâches ne peuvent pas s'en
//! servir en même temps. Un `Mutex` sérialise donc les requêtes. À 500 µs la requête
//! (critère 4 mesuré), un seul utilisateur ne verra jamais la contention ; le jour où
//! plusieurs clients travaillent en parallèle, la réponse est un pool de connexions, pas un
//! verrou plus fin.
//!
//! Chaque appel passe par `spawn_blocking` : `mailcore` est synchrone, et le faire tourner
//! sur le fil du runtime bloquerait toutes les autres tâches pendant une lecture de blob.
//!
//! ## Lecture seule
//!
//! Les cinq outils lisent. L'écriture — répondre, envoyer — arrive en phase 3 et devra
//! passer par une confirmation explicite de l'utilisateur, jamais par une décision du
//! modèle.

use std::sync::{Arc, Mutex};

use mailcore::{Mailbox, MessageId, SearchResult};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorData, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, schemars, tool, tool_router};
use serde::Deserialize;

use crate::dto;

/// Nombre de résultats rendus par défaut.
///
/// 20 : de quoi répondre à une question sans noyer le contexte du modèle. Un modèle qui en
/// veut plus le demande, et le plafond de `mailcore` l'empêche d'en demander trop.
const DEFAULT_LIMIT: usize = 20;

/// Le serveur MCP.
#[derive(Clone)]
pub struct MailServer {
    mailbox: Arc<Mutex<Mailbox>>,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for MailServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailServer").finish_non_exhaustive()
    }
}

/// Paramètres de `search_mail`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// La requête. Syntaxe : `facture` pour un mot, `"phrase exacte"` pour une expression,
    /// `from:plombier` ou `subject:facture` pour cibler un champ, `AND` / `OR` / `-mot` pour
    /// combiner. Les champs disponibles sont `subject`, `body`, `from`, `to`, `folder`.
    pub query: String,
    /// Nombre maximum de résultats. Par défaut 20, plafonné à 500.
    pub limit: Option<usize>,
}

/// Paramètres d'un outil qui prend un identifiant de message.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MessageParams {
    /// L'identifiant rendu par `search_mail` ou `list_folders`.
    pub message_id: i64,
}

/// Paramètres de `get_contact_history`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContactParams {
    /// L'adresse électronique du contact. Exemple : `plombier@exemple.fr`.
    pub address: String,
    /// Nombre maximum de messages. Par défaut 20, plafonné à 500.
    pub limit: Option<usize>,
}

#[tool_router]
impl MailServer {
    /// Construit un serveur autour d'une boîte déjà ouverte.
    #[must_use]
    pub fn new(mailbox: Mailbox) -> Self {
        Self::from_shared(Arc::new(Mutex::new(mailbox)))
    }

    /// Construit un serveur autour d'une boîte déjà partagée.
    ///
    /// Le démon sert deux surfaces sur la même boîte — les outils MCP et l'API JSON-RPC des
    /// clients non-MCP. Ouvrir le store deux fois donnerait deux `Searcher` et deux
    /// connexions SQLite pour rien ; c'est la même boîte, sous le même verrou.
    #[must_use]
    pub fn from_shared(mailbox: Arc<Mutex<Mailbox>>) -> Self {
        Self {
            mailbox,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "search_mail",
        description = "Recherche plein texte dans tout le courrier stocké. Rend les \
                       métadonnées des messages trouvés, pas leur contenu — utiliser \
                       get_message pour lire un message. Cherche dans le sujet, le corps, \
                       l'expéditeur et les destinataires."
    )]
    async fn search_mail(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<Json<dto::Results>, ErrorData> {
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        let query = params.query;
        let found = self
            .read(move |mailbox| mailbox.search(&query, limit))
            .await?;
        Ok(Json(results(found, limit)))
    }

    #[tool(
        name = "get_message",
        description = "Ouvre un message : en-têtes, corps en texte brut, pièces jointes \
                       listées, et les dossiers qui le contiennent. Le corps est aplati en \
                       texte — le balisage HTML, les URL et les scripts sont retirés."
    )]
    async fn get_message(
        &self,
        Parameters(params): Parameters<MessageParams>,
    ) -> Result<Json<Option<dto::Message>>, ErrorData> {
        let id = MessageId(params.message_id);
        let found = self.read(move |mailbox| mailbox.message(id)).await?;
        Ok(Json(found.map(message)))
    }

    #[tool(
        name = "get_thread",
        description = "Rend tous les messages du fil auquel appartient un message, du plus \
                       ancien au plus récent. Un message isolé rend un fil d'un seul \
                       élément. Rend les métadonnées, pas les corps."
    )]
    async fn get_thread(
        &self,
        Parameters(params): Parameters<MessageParams>,
    ) -> Result<Json<dto::Thread>, ErrorData> {
        let id = MessageId(params.message_id);
        let items = self.read(move |mailbox| mailbox.thread(id)).await?;
        let messages: Vec<dto::MessageSummary> = items.iter().map(|i| summary(i, None)).collect();
        Ok(Json(dto::Thread {
            count: messages.len(),
            messages,
        }))
    }

    #[tool(
        name = "list_folders",
        description = "Liste tous les dossiers de tous les comptes, avec le nombre de \
                       messages et de non-lus. Un même message peut apparaître dans \
                       plusieurs dossiers : c'est le même contenu référencé plusieurs fois, \
                       pas une duplication."
    )]
    async fn list_folders(&self) -> Result<Json<Vec<dto::Folder>>, ErrorData> {
        let folders = self.read(mailcore::Mailbox::folders).await?;
        Ok(Json(
            folders
                .into_iter()
                .map(|summary| dto::Folder {
                    id: summary.folder.id.0,
                    account: summary.account_name,
                    path: summary.folder.path,
                    kind: summary.folder.kind.as_str().to_owned(),
                    total: summary.total,
                    unread: summary.unread,
                })
                .collect(),
        ))
    }

    #[tool(
        name = "get_contact_history",
        description = "Rend les messages échangés avec une adresse électronique, dans les \
                       deux sens : ceux qu'elle a envoyés et ceux qui lui étaient adressés. \
                       Trié par pertinence. Utile pour retrouver le contexte d'une relation \
                       avant de rédiger."
    )]
    async fn get_contact_history(
        &self,
        Parameters(params): Parameters<ContactParams>,
    ) -> Result<Json<dto::Results>, ErrorData> {
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        let address = params.address;
        let found = self
            .read(move |mailbox| mailbox.contact_history(&address, limit))
            .await?;
        Ok(Json(results(found, limit)))
    }

    /// Exécute une lecture sur la boîte, hors du fil du runtime.
    async fn read<T, F>(&self, action: F) -> Result<T, ErrorData>
    where
        F: FnOnce(&Mailbox) -> mailcore::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let mailbox = Arc::clone(&self.mailbox);
        let outcome = tokio::task::spawn_blocking(move || {
            // Un verrou empoisonné vient d'une panique dans une autre lecture. La boîte
            // n'est jamais mutée ici, donc son état reste valide : reprendre est plus utile
            // que de refuser tout le reste de la session.
            let guard = mailbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            action(&guard)
        })
        .await;

        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(source)) => {
                // Le message d'erreur part vers un modèle : il dit ce qui a échoué, jamais
                // un chemin de fichier ni un fragment de message.
                tracing::warn!(%source, "outil MCP en échec");
                Err(ErrorData::internal_error(source.to_string(), None))
            }
            Err(source) => {
                tracing::error!(%source, "tâche d'outil interrompue");
                Err(ErrorData::internal_error(
                    "la lecture a été interrompue".to_owned(),
                    None,
                ))
            }
        }
    }
}

// `router = self.tool_router` plutôt que le défaut `Self::tool_router()` : sans ça, la
// table des outils serait reconstruite à chaque appel.
#[rmcp::tool_handler(router = self.tool_router)]
impl ServerHandler for MailServer {
    /// `ServerInfo` et `Implementation` sont `#[non_exhaustive]` : impossible de les
    /// construire par littéral depuis un autre crate, d'où l'affectation champ par champ et
    /// l'exception au lint qui s'en plaint.
    #[allow(clippy::field_reassign_with_default)]
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "Accès en lecture seule au courrier local. Commencer par search_mail ou \
             list_folders, puis get_message pour lire. Les identifiants de message rendus \
             par un outil se passent tels quels aux autres. Aucun outil n'écrit, n'envoie \
             ni ne supprime quoi que ce soit."
                .to_owned(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = {
            let mut implementation = Implementation::default();
            implementation.name = "mailcore".to_owned();
            implementation.version = env!("CARGO_PKG_VERSION").to_owned();
            implementation
        };
        info
    }
}

/// Convertit une liste de résultats.
fn results(found: Vec<SearchResult>, limit: usize) -> dto::Results {
    let messages: Vec<dto::MessageSummary> = found
        .iter()
        .map(|result| summary(&result.item, Some(result.score)))
        .collect();
    dto::Results {
        // `truncated` plutôt qu'un total exact : compter tous les résultats coûterait un
        // parcours complet de l'index, pour une information dont le modèle n'a pas besoin.
        truncated: messages.len() >= limit,
        count: messages.len(),
        messages,
    }
}

/// Convertit un message de liste.
fn summary(item: &mailcore::ListItem, score: Option<f32>) -> dto::MessageSummary {
    dto::MessageSummary {
        id: item.id.0,
        date: iso_date(item.date),
        from: item.from_addr.clone(),
        from_name: item.from_name.clone(),
        subject: item.subject.clone(),
        has_attachments: item.has_attachments,
        score,
    }
}

/// Convertit un message ouvert.
fn message(detail: mailcore::MessageDetail) -> dto::Message {
    dto::Message {
        summary: summary(&detail.item, None),
        message_id: detail.rfc822_id,
        to: detail.to,
        body: detail.body,
        body_truncated: detail.body_truncated,
        attachments: detail
            .attachments
            .into_iter()
            .map(|attachment| dto::Attachment {
                name: attachment.name,
                mime: attachment.mime,
                size: attachment.size as u64,
            })
            .collect(),
        folders: detail.folders.into_iter().map(|(path, _)| path).collect(),
    }
}

/// Une date Unix en `AAAA-MM-JJ`, ou `None` si elle est absente.
///
/// Une date en secondes Unix se fait mal interpréter par un modèle — il la lit comme un
/// nombre. Une date ISO se lit sans ambiguïté.
fn iso_date(unix_seconds: i64) -> Option<String> {
    if unix_seconds <= 0 {
        return None;
    }
    // Algorithme de Howard Hinnant, transcrit tel quel : les calculs de calendrier sont un
    // nid à erreurs d'un jour, et ça ne vaut pas une dépendance de plus.
    let days = unix_seconds.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates_the_way_a_model_can_read_them() {
        assert_eq!(iso_date(1_700_000_000).as_deref(), Some("2023-11-14"));
        assert_eq!(iso_date(951_782_400).as_deref(), Some("2000-02-29"));
        assert_eq!(iso_date(0), None);
        assert_eq!(iso_date(-1), None);
    }

    #[test]
    fn a_full_page_is_reported_as_truncated() {
        // Le modèle doit savoir qu'il n'a peut-être pas tout vu.
        assert!(results(Vec::new(), 0).truncated);
        assert!(!results(Vec::new(), 20).truncated);
    }
}

//! Transport HTTP : les clients sont ailleurs sur le réseau.
//!
//! ## Les deux surfaces, servies ensemble
//!
//! | Chemin | Protocole | Public |
//! |---|---|---|
//! | `/mcp` | MCP streamable, par `rmcp` | un modèle de langage |
//! | `/api` | JSON-RPC 2.0, par [`crate::api::jsonrpc`] | l'UI, la CLI |
//!
//! Servies en même temps, contrairement à stdio où il faut choisir : deux chemins d'URL
//! suffisent à les distinguer, et un seul port à ouvrir est un seul port à protéger. Les
//! deux passent par **la même** couche de jeton — une surface d'authentification unique,
//! parce que deux en divergeraient.
//!
//! ## Ce que ce module applique
//!
//! - **Jeton porteur obligatoire.** Vérifié avant que la requête atteigne le moindre code
//!   qui lit du courrier, par une couche `axum` placée devant les deux services.
//!   Comparaison en temps constant.
//! - **TLS hors bouclage.** Exigé par [`crate::config::Config::validate`], monté ici.
//! - **Aucun contenu dans les journaux.** Ni jeton, ni sujet, ni adresse — voir
//!   `docs/PRIVACY.md`, section 8.
//!
//! La validation qui décide de tout ça vit dans `config`, pas ici : un contrôle de sécurité
//! doit être vérifiable sans monter un serveur, et il l'est par des tests unitaires.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use mailmcp::MailServer;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use crate::api::jsonrpc::Api;
use crate::config::Config;

/// Le chemin sur lequel le service MCP répond.
const MCP_PATH: &str = "/mcp";

/// Le chemin sur lequel l'API des clients non-MCP répond.
const API_PATH: &str = "/api";

/// Taille maximale d'une requête JSON-RPC acceptée sur `/api`.
///
/// 64 Kio : la plus grosse requête légitime est une requête de recherche, qui tient dans une
/// ligne. Le plafond existe pour qu'un client — ou quelqu'un qui a le jeton — ne puisse pas
/// faire allouer un gigaoctet au démon avec un seul POST.
const MAX_BODY: usize = 64 * 1024;

/// Sert MCP et l'API JSON-RPC en HTTP, sur le même port et derrière le même jeton.
///
/// # Errors
///
/// Si la socket ne peut pas être ouverte, ou si le certificat TLS est illisible.
pub async fn serve(
    server: MailServer,
    api: Api,
    address: SocketAddr,
    config: &Config,
) -> Result<()> {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig::default(),
    );

    let token = config
        .token
        .clone()
        .context("jeton absent : la validation aurait dû refuser ce démarrage")?;

    let protected = Router::new()
        .nest_service(MCP_PATH, service)
        .route(API_PATH, post(handle_api))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .with_state(api)
        // Posée en dernier, donc exécutée en premier : une requête non authentifiée
        // n'atteint ni `/mcp` ni `/api`.
        .layer(middleware::from_fn_with_state(
            Arc::new(token),
            require_token,
        ));

    // Le front, s'il est fourni. **Hors de la couche de jeton, et c'est nécessaire** : un
    // navigateur qui ouvre un onglet ne peut pas poser d'en-tête `Authorization` sur la
    // requête initiale. Ce qui est servi ici est notre propre JavaScript et notre propre CSS,
    // aucun octet de courrier — les données restent derrière `/api`, qui exige le jeton.
    //
    // `ServeDir` en repli plutôt qu'une route : n'importe quel chemin inconnu rend
    // `index.html`, ce qui laisse le front router côté client sans que le démon connaisse ses
    // URL. Et l'implémentation vient de `tower-http`, pas de nous : la protection contre la
    // traversée de chemin n'est pas un exercice.
    let app = match &config.ui_dir {
        Some(dir) => {
            tracing::info!(ui = %dir, "front servi depuis le disque");
            let index = dir.join("index.html");
            if !index.exists() {
                tracing::warn!(
                    "aucun index.html dans {dir} : le front répondra 404 tant qu'il n'est pas \
                     construit (npm run build dans crates/mail-ui/web)"
                );
            }
            // Le service de fichiers est enveloppé dans son propre `Router` pour pouvoir y
            // poser la couche de cache : c'est le seul endroit du montage où une couche doit
            // s'appliquer au repli et pas aux routes protégées.
            let files = Router::new()
                .fallback_service(
                    tower_http::services::ServeDir::new(dir.as_std_path())
                        .fallback(tower_http::services::ServeFile::new(index.as_std_path())),
                )
                .layer(middleware::from_fn(cache_headers));
            protected.fallback_service(files)
        }
        None => protected,
    };

    tracing::info!(%address, tls = config.tls_cert.is_some(), "service HTTP");
    tracing::info!("points d'entrée : {MCP_PATH} (MCP), {API_PATH} (JSON-RPC)");
    if config.ui_dir.is_some() {
        tracing::info!(
            "front : http{}://{address}/",
            if config.tls_cert.is_some() { "s" } else { "" }
        );
    }

    match (&config.tls_cert, &config.tls_key) {
        (Some(cert), Some(key)) => {
            // Fournisseur cryptographique explicite : `rustls` refuse de démarrer si
            // plusieurs sont compilés et qu'aucun n'est désigné, et le message d'erreur
            // arrive alors au moment de la première connexion, pas au démarrage.
            rustls::crypto::ring::default_provider()
                .install_default()
                .ok();
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                cert.as_std_path(),
                key.as_std_path(),
            )
            .await
            .with_context(|| format!("lecture du certificat {cert} et de la clé {key}"))?;

            axum_server::bind_rustls(address, tls)
                .serve(app.into_make_service())
                .await
                .context("service HTTPS")
        }
        _ => axum_server::bind(address)
            .serve(app.into_make_service())
            .await
            .context("service HTTP"),
    }
}

/// Le préfixe des fichiers dont le nom porte une empreinte de contenu.
///
/// Vite les écrit sous `assets/index-<hash>.js`. Un nom qui change avec le contenu peut être
/// mis en cache indéfiniment, parce qu'un contenu différent porte un autre nom.
const HASHED_ASSETS: &str = "/assets/";

/// Pose la politique de cache du front.
///
/// **Deux régimes opposés, et c'est le point.**
///
/// - `index.html` en `no-cache` : le navigateur doit revalider avant de s'en servir. C'est
///   lui qui nomme les fichiers d'assets, donc un `index.html` périmé fait charger l'ancien
///   JavaScript — et une page rafraîchie continue d'afficher la version précédente.
/// - Les assets hachés en `immutable` pour un an : leur nom change avec leur contenu, donc
///   il n'y a jamais rien à revalider.
///
/// Écrit après avoir vu le problème en vrai : `ServeDir` ne pose aucun `Cache-Control`, ce qui
/// laisse le navigateur appliquer sa propre heuristique de fraîcheur. Sur un simple `Ctrl+R`,
/// il servait l'`index.html` du cache et donc l'ancien bundle. `no-cache` ne veut pas dire
/// « ne pas garder » mais « revalider », donc les `304` continuent de fonctionner et on ne
/// paie rien quand rien n'a changé.
async fn cache_headers(request: axum::extract::Request, next: Next) -> Response {
    let hashed = request.uri().path().starts_with(HASHED_ASSETS);
    let mut response = next.run(request).await;

    let value = if hashed {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    if let Ok(header) = axum::http::HeaderValue::from_str(value) {
        response
            .headers_mut()
            .insert(axum::http::header::CACHE_CONTROL, header);
    }
    response
}

/// Sert un message JSON-RPC posté sur `/api`.
///
/// Un message par requête, le corps est le message. Pas de session à établir et rien à
/// négocier : un `curl` avec le jeton et un objet JSON suffit à interroger le store, ce qui
/// rend la surface aussi facile à déboguer qu'à écrire un client pour.
///
/// **`204` pour une notification.** Un message sans `id` n'attend pas de réponse ; renvoyer
/// un corps vide avec un `200` laisserait un client croire qu'il a reçu une réponse
/// illisible.
async fn handle_api(State(api): State<Api>, body: String) -> Response {
    match api.handle_message(&body).await {
        Some(text) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Refuse toute requête qui ne porte pas le bon jeton.
///
/// Placée devant les deux services : une requête non authentifiée n'atteint jamais le code
/// qui lit du courrier.
async fn require_token(
    State(expected): State<Arc<String>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();

    if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        // Ni le jeton présenté, ni le corps de la requête, ni l'adresse du client : un
        // journal d'échecs ne doit pas devenir un journal de secrets.
        tracing::warn!("requête refusée : jeton absent ou invalide");
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

/// Compare deux secrets en temps constant.
///
/// Un `==` sur des chaînes s'arrête au premier octet différent, ce qui laisse mesurer la
/// longueur du préfixe commun et reconstruire le jeton octet par octet. La différence de
/// longueur reste observable — elle ne révèle rien ici, le jeton ayant une taille fixe.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_comparison_agrees_with_equality() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrez"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn a_common_prefix_does_not_shortcut_the_comparison() {
        // Le point du temps constant : ces deux cas doivent parcourir le même nombre
        // d'octets, et tous les deux rendre faux.
        assert!(!constant_time_eq(b"AAAAAAAA", b"BBBBBBBB"));
        assert!(!constant_time_eq(b"AAAAAAAA", b"AAAAAAAB"));
    }
}

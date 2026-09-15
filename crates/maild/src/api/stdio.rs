//! Transport stdio : le client tourne sur cette machine.
//!
//! Pas d'authentification, et c'est correct : le client est un processus lancé par
//! l'utilisateur, qui a déjà accès au store par le système de fichiers. Ajouter un jeton ici
//! protégerait contre un attaquant qui a déjà gagné.
//!
//! **La sortie standard porte le protocole.** Rien d'autre ne doit y écrire — les traces
//! partent sur la sortie d'erreur, réglé dans `main`. Un `println!` égaré ici corromprait
//! la session sans message d'erreur exploitable.
//!
//! À garder fonctionnel en permanence : c'est le transport qui permet de distinguer un bug
//! du transport d'un bug du cœur.
//!
//! ## Deux protocoles, une seule paire de flux
//!
//! [`serve`] sert MCP, [`serve_api`] sert l'API des clients non-MCP. Les deux ne peuvent pas
//! cohabiter sur la même entrée standard : il n'y a qu'un flux, et rien dans un message ne
//! dirait à quel protocole il appartient. Le choix se fait donc au démarrage, par
//! `maild stdio --protocol`. En HTTP la question ne se pose pas — deux chemins d'URL
//! suffisent, et le démon sert les deux en même temps.

use anyhow::{Context, Result};
use mailmcp::MailServer;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::api::jsonrpc::Api;

/// Sert le protocole MCP sur l'entrée et la sortie standard, jusqu'à la fin du flux.
///
/// # Errors
///
/// Si l'initialisation MCP échoue.
pub async fn serve(server: MailServer) -> Result<()> {
    tracing::info!("service MCP sur stdio");
    let running = server
        .serve(stdio())
        .await
        .context("initialisation MCP sur stdio")?;

    // Le client ferme le flux quand il s'arrête : c'est la fin normale, pas une erreur.
    let reason = running.waiting().await.context("session MCP")?;
    tracing::info!(?reason, "session stdio terminée");
    Ok(())
}

/// Sert l'API JSON-RPC des clients non-MCP, une requête par ligne.
///
/// Encadrement par saut de ligne, comme MCP sur stdio : pas d'en-tête de longueur, pas de
/// délimiteur à négocier, et une session se rejoue à la main avec un `echo` — ce qui est
/// précisément ce qu'on veut du transport qui sert à isoler un bug du cœur.
///
/// Les requêtes sont traitées **dans l'ordre**, une à la fois. Un client qui veut du
/// parallélisme utilise HTTP : sérialiser ici évite d'avoir à entrelacer des réponses sur un
/// flux unique, et un `store.wait` de trente secondes n'y bloquerait pas seulement une
/// requête, il bloquerait la lecture de la suivante.
///
/// # Errors
///
/// Si l'entrée ou la sortie standard devient inutilisable.
pub async fn serve_api(api: Api) -> Result<()> {
    tracing::info!("service API JSON-RPC sur stdio");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();

    while let Some(line) = lines.next_line().await.context("lecture de l'entrée")? {
        // Une ligne vide n'est pas une erreur de protocole : elle vient d'un terminal, d'un
        // fichier rejoué, d'un client qui termine ses messages par un saut de ligne de trop.
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = api.handle_message(&line).await {
            out.write_all(response.as_bytes())
                .await
                .context("écriture de la réponse")?;
            out.write_all(b"\n").await.context("fin de la réponse")?;
            // Vidé à chaque réponse : un client qui attend une ligne pour continuer se
            // bloquerait sur un tampon retenu, et le démon aurait l'air en panne.
            out.flush().await.context("vidage de la sortie")?;
        }
    }

    tracing::info!("session API sur stdio terminée");
    Ok(())
}

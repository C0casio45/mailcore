//! `mail source` — les octets d'un message, rendus lisibles.
//!
//! La commande qui permet de vérifier plutôt que de croire. `docs/PHASE-3.md` promet que rien
//! n'est ajouté au message en secret ; jusqu'ici la promesse reposait sur la lecture du code de
//! `mailsmtp::compose`, ce qui n'est pas à la portée de qui l'utilise.
//!
//! `--outbox` est la moitié qui compte : c'est le message **que nous composons**, et le blob
//! qu'elle lit est celui qui a été remis au `DATA`.
//!
//! ## Pourquoi c'est sûr de l'écrire sur un terminal
//!
//! Ça ne le serait pas sans `mailcore::source` : un message qui porte `ESC[2J` effacerait
//! l'écran sur lequel on l'affiche, donc les en-têtes qu'on venait vérifier. L'échappement est
//! fait dans `mailcore`, au point de rendu, et pas ici — cette commande n'a qu'à imprimer.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailapi::dto;
use mailcore::{Mailbox, MessageId, OutboxId};

/// Ce qu'on veut lire.
#[derive(Debug, Clone, Copy)]
pub enum Target {
    /// Un message reçu, par son identifiant d'index.
    Message(i64),
    /// Une ligne de la file d'envoi, par son identifiant de file.
    Outgoing(i64),
}

impl Target {
    /// Comment la nommer à l'écran, et par quelle méthode la demander.
    pub(crate) fn named(self) -> (String, &'static str, i64) {
        match self {
            Self::Message(id) => (
                format!("message #{id}"),
                mailapi::method::MESSAGES_SOURCE,
                id,
            ),
            Self::Outgoing(id) => (
                format!("file d'envoi #{id}"),
                mailapi::method::OUTBOX_SOURCE,
                id,
            ),
        }
    }
}

/// Point d'entrée de la sous-commande, contre le store local.
///
/// # Errors
///
/// Si le store est introuvable ou illisible, ou si l'identifiant ne désigne rien.
pub fn run(store_root: Option<&Utf8PathBuf>, target: Target, headers_only: bool) -> Result<()> {
    let root = crate::store_root(store_root)?;
    let mailbox = Mailbox::open(&root).with_context(|| format!("ouverture du store {root}"))?;

    let found = match target {
        Target::Message(id) => mailbox.source(MessageId(id)),
        Target::Outgoing(id) => mailbox.outgoing_source(OutboxId(id)),
    }
    .context("lecture de la source")?;

    let (what, _, _) = target.named();
    let Some(source) = found else {
        anyhow::bail!("{what} : introuvable, ou son contenu manque du magasin");
    };
    print(&what, &dto::MessageSource::new(source), headers_only);
    Ok(())
}

/// Écrit la source sur la sortie standard, en disant ce qui manque avant ce qui est là.
///
/// Partagée avec le chemin `--daemon` : les deux montrent la même chose, et une mise en forme
/// par chemin finirait par diverger sur ce qui compte — les réserves.
pub(crate) fn print(what: &str, source: &dto::MessageSource, headers_only: bool) {
    // Les réserves d'abord : une troncature annoncée après mille lignes n'est jamais lue, et
    // c'est celle-là qui change la valeur de ce qui suit.
    println!("{what} — {}", mailapi::human::bytes(source.total));
    if source.invalid_sequences > 0 {
        println!(
            "  {} séquence(s) d'octets non UTF-8, rendues par « \u{fffd} »",
            source.invalid_sequences
        );
    }
    if source.escaped_controls > 0 {
        println!(
            "  {} caractère(s) de contrôle rendus visibles sous la forme \\xNN",
            source.escaped_controls
        );
    }
    if source.headers_truncated {
        println!("  en-têtes tronqués");
    }
    println!();

    print!("{}", source.headers);

    if headers_only {
        return;
    }
    println!();
    print!("{}", source.body);
    if source.body_truncated {
        println!(
            "\n[corps tronqué — le message fait {}]",
            mailapi::human::bytes(source.total)
        );
    }
}

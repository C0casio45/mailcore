//! `mail attach` — liste les pièces jointes d'un message, et en enregistre une.
//!
//! ## Pourquoi cette commande existe
//!
//! Écrite le 2026-09-21, en cherchant une facture dans le corpus réel. Le nom du fichier était
//! dans le message, le montant était dans le PDF, et le client n'avait **aucun moyen** de
//! l'ouvrir : `mail source` rend les octets bruts et tronqués, ce qui montre un base64 et pas un
//! document.
//!
//! C'est le pendant CLI de ce que `mailcore::attachment_bytes` savait déjà faire depuis la phase
//! 1. La fonction existait, la porte manquait.
//!
//! ## Le nom du fichier vient du message, donc il est hostile
//!
//! Un nom de pièce jointe est écrit par l'expéditeur. `../../.ssh/authorized_keys` en est un
//! valide du point de vue du format, et l'enregistrer tel quel écrirait où l'expéditeur veut.
//! Le nom est donc **réduit à son dernier segment** et nettoyé avant de toucher le disque — la
//! même règle que celle qui a valu au projet de prendre `tower-http::ServeDir` plutôt que
//! d'écrire son propre serveur de fichiers.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Mailbox, MessageId};

use mailapi::human;

/// Point d'entrée de la sous-commande.
///
/// # Errors
///
/// Si le store est introuvable, si le message n'existe pas, ou si le fichier n'est pas
/// enregistrable.
pub fn run(
    store_root: Option<&Utf8PathBuf>,
    id: i64,
    save: Option<usize>,
    out: Option<&Utf8PathBuf>,
) -> Result<()> {
    let root = crate::store_root(store_root)?;
    let mailbox = Mailbox::open(&root).with_context(|| format!("ouverture du store {root}"))?;
    let target = MessageId(id);

    let detail = mailbox
        .message(target)
        .with_context(|| format!("relecture du message #{id}"))?
        .with_context(|| format!("aucun message #{id} dans {root}"))?;

    if detail.attachments.is_empty() {
        println!("Le message #{id} ne porte aucune pièce jointe.");
        return Ok(());
    }

    println!("Message  #{id}  {}", detail.item.subject);
    println!();
    for (rank, part) in detail.attachments.iter().enumerate() {
        println!(
            "  {:>2}  {:<48} {:<28} {}",
            rank + 1,
            part.name.as_deref().unwrap_or("(sans nom)"),
            part.mime,
            human::bytes(part.size as u64)
        );
    }

    let Some(rank) = save else {
        println!("\nPour en enregistrer une : `mail attach --id {id} --save <n>`.");
        return Ok(());
    };
    anyhow::ensure!(
        rank >= 1 && rank <= detail.attachments.len(),
        "il n'y a pas de pièce jointe n° {rank} — le message en porte {}",
        detail.attachments.len()
    );

    // Les octets viennent du blob, pas de `Mailbox::source` : celle-ci s'arrête à un plafond de
    // lecture, ce qui est le bon comportement pour *afficher* une source et le mauvais pour
    // reconstituer un fichier.
    let raw = mailbox
        .store()
        .blobs()
        .read(detail.item.blob)
        .with_context(|| format!("relecture des octets du message #{id}"))?;

    let (name, bytes) = mailcore::attachment_bytes(&raw, rank - 1)
        .with_context(|| format!("la pièce jointe n° {rank} n'est pas décodable"))?;

    let target_path = match out {
        Some(path) => path.clone(),
        None => Utf8PathBuf::from(safe_name(&name)),
    };
    std::fs::write(target_path.as_std_path(), &bytes)
        .with_context(|| format!("écriture de {target_path}"))?;

    println!(
        "\nEnregistré  {target_path}  ({})",
        human::bytes(bytes.len() as u64)
    );
    Ok(())
}

/// Réduit un nom venu d'un message à quelque chose qu'on peut poser sur le disque.
///
/// Le dernier segment seulement, sans séparateur ni caractère réservé. Un expéditeur qui écrit
/// `../../secret` obtient `secret`, et un qui écrit `C:\Windows\system32\x` obtient `x`.
fn safe_name(name: &str) -> String {
    let last = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .trim_matches('.');
    let cleaned: String = last
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.is_empty() {
        "piece-jointe".to_owned()
    } else {
        cleaned
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_traversing_name_is_reduced_to_its_last_segment() {
        assert_eq!(
            safe_name("../../.ssh/authorized_keys"),
            "ssh/authorized_keys".rsplit('/').next().unwrap()
        );
        assert_eq!(safe_name(r"C:\Windows\system32\evil.dll"), "evil.dll");
        assert_eq!(safe_name("/etc/passwd"), "passwd");
    }

    /// Le contrôle inverse : un nom ordinaire traverse intact, sinon la commande enregistrerait
    /// tout sous des noms méconnaissables.
    #[test]
    fn an_ordinary_name_survives() {
        assert_eq!(safe_name("FA2608-0092.pdf"), "FA2608-0092.pdf");
        assert_eq!(safe_name("Rapport été 2026.docx"), "Rapport été 2026.docx");
    }

    #[test]
    fn a_name_that_reduces_to_nothing_gets_one() {
        assert_eq!(safe_name("../.."), "piece-jointe");
        assert_eq!(safe_name("   "), "piece-jointe");
    }
}

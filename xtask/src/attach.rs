//! Le recensement des pièces jointes du corpus — l'étape 6 quater, avant d'écrire une ligne
//! d'extracteur.
//!
//! ## Pourquoi ce banc existe avant l'extraction
//!
//! « Le contenu des pièces jointes n'entre dans aucun index » est un manque réel, vérifié à la
//! main sur une facture. Ça ne dit ni **combien** de messages sont concernés, ni **quels formats**
//! il faudrait savoir lire, ni ce que ça rapporterait. Écrire un extracteur PDF avant de savoir si
//! le corpus porte cent PDF ou dix mille, c'est choisir la réponse avant la question — la faute
//! que la phase 4 a déjà payée trois fois.
//!
//! C'est la même méthode qu'à l'étape 1, où le premier tirage du corpus a montré que neuf messages
//! sur dix étaient des notifications : un chiffre trivial à obtenir, et qui a changé le plan.
//!
//! ## Il n'écrit rien
//!
//! Lecture seule, donc pas de copie du store — le filet de `measure-vectors` protège d'une passe
//! qui **écrit**, et `measure-attachment` avait laissé 533 Mo dans les données de production le
//! 2026-09-09 pour cette raison exacte. Ici, rien n'est ouvert en écriture.

use std::collections::HashMap;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mail_parser::MimeHeaders;
use mailcore::Store;

/// Ce qu'on retient d'un type de pièce jointe.
#[derive(Debug, Default, Clone)]
struct Tally {
    /// Combien de pièces de ce type.
    parts: u64,
    /// Combien de messages en portent au moins une.
    messages: u64,
    /// Le total de leurs octets décodés.
    bytes: u64,
}

/// Recense les pièces jointes du corpus : types, nombres, volumes.
///
/// # Errors
///
/// Si le store n'est pas lisible.
pub fn census(store_root: &Utf8PathBuf, top: usize) -> Result<()> {
    let store =
        Store::open(store_root).with_context(|| format!("ouverture du store {store_root}"))?;
    let rows = store
        .all_for_indexing()
        .context("lecture des messages du store")?;

    let parser = mail_parser::MessageParser::default();
    let mut by_type: HashMap<String, Tally> = HashMap::new();
    let mut with_attachments = 0u64;
    let mut unreadable = 0u64;
    let mut total_parts = 0u64;
    // Ce que l'extraction rapporte réellement, et non ce qu'elle pourrait rapporter. Le
    // recensement des types dit ce qui **existe** ; ces deux compteurs disent ce qui **entre dans
    // l'index**, ce qui est la seule question à laquelle l'étape 6 quater doit répondre.
    let mut gained_messages = 0u64;
    let mut gained_bytes = 0u64;
    let mut newly_findable = 0u64;

    let total = rows.len();
    for (seen, row) in rows.iter().enumerate() {
        if seen % 5_000 == 0 {
            eprint!("\r  {seen} / {total} messages lus…");
        }
        let Ok(raw) = store.blobs().read(row.blob) else {
            unreadable += 1;
            continue;
        };
        let Some(parsed) = parser.parse(&raw) else {
            unreadable += 1;
            continue;
        };

        let mut kinds_here: Vec<String> = Vec::new();
        for part in parsed.attachments() {
            let kind = mime_of(part);
            total_parts += 1;
            let entry = by_type.entry(kind.clone()).or_default();
            entry.parts += 1;
            entry.bytes += part.contents().len() as u64;
            if !kinds_here.contains(&kind) {
                kinds_here.push(kind);
            }
        }
        let mut gained = String::new();
        mailcore::attachtext::attachment_text(&parsed, &mut gained);
        if !gained.trim().is_empty() {
            gained_messages += 1;
            gained_bytes += gained.len() as u64;
            // **Le chiffre qui compte vraiment.** « Du texte en plus » ne veut pas dire « un
            // message de plus trouvable » : une invitation dont le titre reprend le sujet du
            // message n'apporte rien à l'index. Ce compteur ne retient que les messages où la
            // pièce jointe apporte **un mot que le reste du message n'a pas**.
            if brings_a_new_word(&parsed, &gained, row) {
                newly_findable += 1;
            }
        }

        if !kinds_here.is_empty() {
            with_attachments += 1;
            // Un message ne compte qu'une fois par type, même s'il porte douze PDF : la colonne
            // « messages » répond à « combien de messages deviendraient trouvables », qui est la
            // question de l'étape, et non à « combien de fichiers faudrait-il lire ».
            for kind in kinds_here {
                by_type.entry(kind).or_default().messages += 1;
            }
        }
    }
    eprintln!("\r  {total} / {total} messages lus.   ");

    let mut ranked: Vec<(String, Tally)> = by_type.into_iter().collect();
    // Par nombre de **messages** et non de pièces : ce qui décide de l'ordre de travail est
    // combien de messages un format rendrait trouvables.
    ranked.sort_by(|a, b| {
        b.1.messages
            .cmp(&a.1.messages)
            .then(b.1.bytes.cmp(&a.1.bytes))
    });

    println!("Store              {store_root}");
    println!("Messages           {total}");
    println!(
        "Avec pièce jointe  {with_attachments}  ({:.1} %)",
        share(with_attachments, total as u64)
    );
    println!("Pièces jointes     {total_parts}");
    if unreadable > 0 {
        println!("Illisibles         {unreadable}  (blob absent ou en-têtes cassés)");
    }
    println!(
        "Texte récupéré     {gained_messages} messages, {}  ({:.1} % de ceux à pièce jointe)",
        mailapi::human::bytes(gained_bytes),
        share(gained_messages, with_attachments)
    );
    println!(
        "Mot nouveau        {newly_findable} messages  ({:.1} % de ceux à pièce jointe) — la pièce
                   jointe y apporte au moins un mot que le reste du message n'a pas",
        share(newly_findable, with_attachments)
    );
    println!();

    println!(
        "{:<44} {:>9} {:>9} {:>11} {:>8}",
        "type MIME", "messages", "pièces", "octets", "part"
    );
    for (kind, tally) in ranked.iter().take(top) {
        println!(
            "{:<44} {:>9} {:>9} {:>11} {:>7.1} %",
            short(kind),
            tally.messages,
            tally.parts,
            mailapi::human::bytes(tally.bytes),
            share(tally.messages, with_attachments)
        );
    }
    if ranked.len() > top {
        let rest: u64 = ranked.iter().skip(top).map(|(_, t)| t.parts).sum();
        println!(
            "{:<44} {:>9} {:>9}",
            format!("… et {} autres types", ranked.len() - top),
            "",
            rest
        );
    }

    Ok(())
}

/// La pièce jointe apporte-t-elle un mot que le reste du message n'a pas ?
///
/// ## Pourquoi ce n'est pas la même question que « a-t-elle du texte »
///
/// Une invitation dont le titre reprend le sujet du message n'apporte rien à l'index : le message
/// était déjà trouvable par ces mots-là. Compter ces cas ferait passer l'étape pour plus rentable
/// qu'elle n'est, et c'est exactement le genre de chiffre flatteur que le projet refuse — même
/// forme que le rappel binaire qui confondait « manqué de peu » et « pas du tout ».
///
/// Les mots d'un caractère sont ignorés, et la comparaison est repliée en minuscules **Unicode** :
/// `to_ascii_lowercase` ne replie pas « Éloïse », et sur un corpus français c'est la plupart des
/// noms — la leçon du carnet d'adresses, le 2026-09-11.
fn brings_a_new_word(
    parsed: &mail_parser::Message<'_>,
    gained: &str,
    row: &mailcore::store::read::IndexRow,
) -> bool {
    let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
    for source in [row.subject.as_str(), row.from_name.as_deref().unwrap_or("")] {
        known.extend(words(source));
    }
    for index in 0..parsed.text_body_count() {
        if let Some(part) = parsed.body_text(index) {
            known.extend(words(&part));
        }
    }
    for index in 0..parsed.html_body_count() {
        if let Some(part) = parsed.body_html(index) {
            let mut text = String::new();
            mailhtml::text::html_to_text(&part, &mut text);
            known.extend(words(&text));
        }
    }
    words(gained).any(|word| !known.contains(&word))
}

/// Les mots d'un texte, repliés en minuscules Unicode.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() > 1)
        .map(str::to_lowercase)
}

/// Le pourcentage, sans division par zéro.
#[allow(clippy::cast_precision_loss)]
fn share(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    part as f64 * 100.0 / whole as f64
}

/// Le type MIME d'une partie, en minuscules et sans ses paramètres.
///
/// Sans les paramètres parce que `application/pdf; name="facture.pdf"` et `application/pdf` sont
/// le même format : les garder ferait autant de lignes que de noms de fichiers, et le recensement
/// ne compterait plus rien.
///
/// Une partie sans `Content-Type` rend `inconnu`. C'est fréquent et ce n'est pas une erreur —
/// certains expéditeurs n'en mettent pas — mais ça doit se voir plutôt que d'être rangé d'office
/// avec les octets bruts.
fn mime_of(part: &mail_parser::MessagePart<'_>) -> String {
    part.content_type().map_or_else(
        || "inconnu".to_owned(),
        |ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub).to_lowercase(),
            None => ct.ctype().to_lowercase(),
        },
    )
}

/// Un type MIME raccourci, pour que la colonne tienne.
fn short(kind: &str) -> String {
    if kind.chars().count() <= 43 {
        return kind.to_owned();
    }
    let kept: String = kind.chars().take(42).collect();
    format!("{kept}…")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_share_of_nothing_is_zero_rather_than_a_division_by_zero() {
        assert!((share(0, 0) - 0.0).abs() < f64::EPSILON);
        assert!((share(1, 2) - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_long_mime_type_is_cut_on_a_character_boundary() {
        let long = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        assert!(short(long).ends_with('…'));
        assert!(short(long).chars().count() <= 43);
        assert_eq!(short("application/pdf"), "application/pdf");
    }
}

//! Le texte que portent les pièces jointes — l'étape 6 quater de `docs/PHASE-4.md`.
//!
//! ## Pourquoi ce module existe
//!
//! Le contenu d'une pièce jointe n'entrait dans **aucun** des deux index. Une facture en PDF ne se
//! trouvait donc ni par son montant ni par sa référence, et une invitation ne se trouvait pas par
//! le nom du lieu — vérifié sur le corpus réel le 2026-09-22, en cherchant une facture avec
//! `mail attach`.
//!
//! ## Pourquoi il est ici, et pas dans chaque index
//!
//! Parce qu'il y en a deux. `mailcore::index::writer` compose le document tantivy,
//! `mailvec::text` compose le texte à vectoriser, et les deux doivent voir **le même** texte de
//! pièce jointe. Deux extractions divergeraient, et la divergence ne se verrait qu'à l'usage :
//! un message trouvable en plein texte et pas en sémantique, sans que rien ne l'explique. C'est
//! la règle du carnet d'adresses — deux chemins vers la même donnée partagent leur code, pas
//! leur intention.
//!
//! ## Ce qu'il lit, et pourquoi seulement ça
//!
//! Le recensement du 2026-09-22 (`cargo xtask attachment-census`) a compté ce que le corpus porte
//! réellement, sur 7 094 messages à pièce jointe :
//!
//! | format | messages | lisible avec |
//! |---|---:|---|
//! | `image/*` | 4 659 | rien — il faudrait de l'OCR, donc un second modèle |
//! | `application/pdf` | 1 222 | une dépendance nouvelle, décidée séparément |
//! | `text/calendar`, `application/ics` | 1 286 | [`mailcal`], déjà là depuis la phase 3 |
//! | `text/*` | 594 | [`mailhtml`], déjà là |
//!
//! Ce module fait les deux dernières lignes, soit **1 880 messages sans une ligne de dépendance
//! nouvelle**. Le PDF viendra après, avec un chiffre : lire un PDF est de l'analyse d'entrée
//! hostile dans un format qui en a beaucoup coûté à l'informatique, et cette décision-là se prend
//! avec une mesure du gain que les formats gratuits auront déjà rapportée.
//!
//! ## Et pourquoi un plafond
//!
//! Une pièce jointe est écrite par l'expéditeur. Le corpus porte 14,5 Mio de `text/plain` en
//! pièces jointes, et rien n'empêche un message d'en porter cent à lui seul. Le plafond n'est pas
//! une économie d'octets — c'est ce qui empêche un expéditeur de décider combien de mémoire une
//! moisson consomme.

use mail_parser::MimeHeaders;

/// Le plafond de texte extrait des pièces jointes d'**un** message.
///
/// 256 Kio. Le recensement du 2026-09-22 donne une moyenne de 35 Kio de `text/*` par message
/// concerné, donc le plafond ne mord jamais en régime ordinaire : il borne le cas pathologique et
/// rien d'autre. Une invitation, qui est le gros du gisement, en fait deux ou trois.
pub const BUDGET: usize = 256 * 1024;

/// Ajoute à `out` le texte que portent les pièces jointes de ce message.
///
/// Les parties sont prises dans l'ordre du message. Ce qui n'est pas lisible en texte est
/// **ignoré en silence** : une image ou un PDF n'est pas une anomalie, c'est le cas ordinaire, et
/// signaler chacun noierait ce qui mérite de l'être.
pub fn attachment_text(parsed: &mail_parser::Message<'_>, out: &mut String) {
    let ceiling = out.len().saturating_add(BUDGET);
    for part in parsed.attachments() {
        if out.len() >= ceiling {
            break;
        }
        let Some(text) = part_text(part) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.is_empty() && !out.ends_with(char::is_whitespace) {
            out.push(' ');
        }
        // La troncature est au **point d'écriture** et sur une frontière de caractère : couper un
        // `String` sur un octet quelconque paniquerait, et sur un corpus français un caractère
        // multi-octet est la règle et non l'exception.
        let room = ceiling.saturating_sub(out.len());
        if trimmed.len() <= room {
            out.push_str(trimmed);
        } else {
            out.push_str(cut(trimmed, room));
            break;
        }
    }
}

/// Le texte d'une seule pièce jointe, si on sait le lire.
fn part_text(part: &mail_parser::MessagePart<'_>) -> Option<String> {
    let mime = mime_of(part);
    let (kind, subtype) = mime.split_once('/').unwrap_or((mime.as_str(), ""));

    match (kind, subtype) {
        // Une invitation : ce qu'on en garde est ce qu'un humain cherche — le titre, le lieu, la
        // description. Pas les dates, qui sont déjà des colonnes et ne se cherchent pas en
        // toutes lettres ; pas les URL, que `docs/PRIVACY.md` interdit de suivre et que personne
        // ne tape dans une recherche.
        ("text", "calendar") | ("application", "ics") | ("text", "ics") => {
            let raw = decode(part)?;
            let invitation = mailcal::read(&raw);
            let mut text = String::new();
            for value in [
                invitation.summary.as_deref(),
                invitation.location.as_deref(),
                invitation.description.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(value);
            }
            // Les noms des participants, mais pas leurs adresses : une adresse est déjà indexée
            // par les en-têtes quand elle compte, et les empiler ici ferait remonter toute une
            // liste de diffusion sur le nom d'une seule personne.
            for person in invitation
                .organizer
                .iter()
                .chain(invitation.attendees.iter())
            {
                if let Some(name) = person.name.as_deref() {
                    text.push(' ');
                    text.push_str(name);
                }
            }
            (!text.is_empty()).then_some(text)
        }
        // Le HTML d'une pièce jointe passe par le même dégraissage que celui d'un corps.
        ("text", "html" | "x-amp-html" | "x-watch-html" | "xhtml+xml") => {
            let raw = decode(part)?;
            let mut text = String::new();
            mailhtml::text::html_to_text(&raw, &mut text);
            (!text.is_empty()).then_some(text)
        }
        // Tout autre `text/*` est pris tel quel : `plain`, `csv`, `rfc822-headers`…
        ("text", _) => decode(part),
        _ => None,
    }
}

/// Les octets d'une pièce en texte.
///
/// `from_utf8_lossy` et non un refus : une pièce jointe est écrite par l'expéditeur, et une
/// déclaration d'encodage fausse est fréquente. Perdre un caractère vaut mieux que perdre le
/// message — c'est la règle que l'indexation applique déjà aux en-têtes illisibles.
fn decode(part: &mail_parser::MessagePart<'_>) -> Option<String> {
    let bytes = part.contents();
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Le type MIME d'une partie, en minuscules et sans ses paramètres.
fn mime_of(part: &mail_parser::MessagePart<'_>) -> String {
    part.content_type().map_or_else(
        || "application/octet-stream".to_owned(),
        |ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub).to_lowercase(),
            None => ct.ctype().to_lowercase(),
        },
    )
}

/// Coupe au plus près de `budget` octets, sur une frontière de caractère.
fn cut(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use mail_parser::MessageParser;

    /// Un message avec une pièce jointe de type et de contenu donnés.
    fn message(mime: &str, filename: &str, body: &str) -> Vec<u8> {
        format!(
            "From: a@exemple.invalid\r\n\
             Subject: porteur\r\n\
             Content-Type: multipart/mixed; boundary=SEP\r\n\
             \r\n\
             --SEP\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Le corps ordinaire.\r\n\
             --SEP\r\n\
             Content-Type: {mime}; name=\"{filename}\"\r\n\
             Content-Disposition: attachment; filename=\"{filename}\"\r\n\
             \r\n\
             {body}\r\n\
             --SEP--\r\n"
        )
        .into_bytes()
    }

    fn extracted(raw: &[u8]) -> String {
        let parser = MessageParser::default();
        let parsed = parser.parse(raw).unwrap();
        let mut out = String::new();
        attachment_text(&parsed, &mut out);
        out
    }

    /// Le gisement qui vaut 1 286 messages : une invitation rend son titre et son lieu.
    #[test]
    fn an_invitation_gives_up_its_summary_and_location() {
        let ics = "BEGIN:VCALENDAR\r\n\
                   BEGIN:VEVENT\r\n\
                   SUMMARY:Signature chez le notaire\r\n\
                   LOCATION:14 rue des Lilas, Orléans\r\n\
                   DESCRIPTION:Apporter les pièces d'identité\r\n\
                   END:VEVENT\r\n\
                   END:VCALENDAR";
        let found = extracted(&message("text/calendar", "invite.ics", ics));
        assert!(found.contains("Signature chez le notaire"), "{found}");
        assert!(found.contains("rue des Lilas"), "{found}");
        assert!(found.contains("pièces d'identité"), "{found}");
    }

    /// `application/ics` est le même format sous un autre nom, et il vaut 364 messages à lui seul.
    #[test]
    fn the_other_calendar_mime_type_is_read_too() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Conseil de classe\r\nEND:VEVENT\r\nEND:VCALENDAR";
        let found = extracted(&message("application/ics", "x.ics", ics));
        assert!(found.contains("Conseil de classe"), "{found}");
    }

    /// Le contrôle négatif : ce qu'on ne sait pas lire ne doit rien produire, et surtout pas les
    /// octets bruts. Indexer un PDF en clair remplirait l'index de charabia binaire.
    #[test]
    fn what_cannot_be_read_yields_nothing_rather_than_raw_bytes() {
        let pdf = "%PDF-1.4\n%\u{e2}\u{e3}\u{cf}\u{d3}\nbinaire illisible";
        assert_eq!(extracted(&message("application/pdf", "f.pdf", pdf)), "");
        assert_eq!(extracted(&message("image/png", "p.png", "\u{89}PNG")), "");
        assert_eq!(
            extracted(&message("application/octet-stream", "x.bin", "quelconque")),
            ""
        );
    }

    #[test]
    fn an_html_attachment_is_stripped_like_a_body() {
        let html = "<p>Votre facture <b>FA-2608</b></p><script>var x = 1;</script>";
        let found = extracted(&message("text/html", "f.html", html));
        assert!(found.contains("FA-2608"), "{found}");
        assert!(!found.contains("script"), "le script est passé : {found}");
        assert!(!found.contains('<'), "du balisage a survécu : {found}");
    }

    #[test]
    fn a_plain_text_attachment_is_taken_as_it_is() {
        let found = extracted(&message("text/plain", "note.txt", "montant 1 234,56 EUR"));
        assert!(found.contains("1 234,56"), "{found}");
    }

    /// Un expéditeur ne décide pas combien de mémoire une moisson consomme.
    #[test]
    fn a_huge_attachment_is_capped() {
        let huge = "mot ".repeat(BUDGET);
        let found = extracted(&message("text/plain", "gros.txt", &huge));
        assert!(found.len() <= BUDGET, "{} octets", found.len());
        assert!(!found.is_empty(), "le plafond a tout mangé");
    }

    /// Le plafond coupe sur une frontière de caractère : sur un corpus français, couper sur un
    /// octet quelconque paniquerait plutôt que de tronquer.
    #[test]
    fn the_cap_never_splits_a_character() {
        let accented = "é".repeat(BUDGET);
        let found = extracted(&message("text/plain", "e.txt", &accented));
        assert!(found.len() <= BUDGET);
        assert!(found.chars().all(|c| c == 'é'), "un caractère a été coupé");
    }

    /// Une invitation illisible ne doit pas empêcher le message d'être indexé : `mailcal::read`
    /// ne peut pas échouer, et ce module doit rendre la main sans rien produire.
    #[test]
    fn a_broken_invitation_produces_nothing_and_does_not_panic() {
        assert_eq!(
            extracted(&message("text/calendar", "x.ics", "n'importe quoi")),
            ""
        );
        assert_eq!(extracted(&message("text/calendar", "x.ics", "")), "");
        assert_eq!(
            extracted(&message(
                "text/calendar",
                "x.ics",
                "BEGIN:VCALENDAR\r\nEND:VCALENDAR"
            )),
            ""
        );
    }

    /// Les octets qui ne sont pas de l'UTF-8 ne font pas perdre le message.
    #[test]
    fn invalid_utf8_degrades_rather_than_losing_the_attachment() {
        let mut raw = message("text/plain", "n.txt", "PLACEHOLDER");
        let at = raw
            .windows(11)
            .position(|w| w == b"PLACEHOLDER")
            .expect("le gabarit porte le marqueur");
        raw.splice(at..at + 11, [b'f', b'a', 0xFF, 0xFE, b'c', b't']);
        let found = extracted(&raw);
        assert!(found.contains("fa"), "{found}");
        assert!(found.contains("ct"), "{found}");
    }

    /// Le corps ordinaire n'est **pas** repris ici : c'est le travail de `body_text`, et le
    /// reprendre doublerait chaque message dans l'index.
    #[test]
    fn the_ordinary_body_is_not_part_of_what_this_extracts() {
        let found = extracted(&message("text/plain", "n.txt", "la pièce"));
        assert!(!found.contains("Le corps ordinaire"), "{found}");
    }
}

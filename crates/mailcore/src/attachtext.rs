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
/// Les formats gratuits d'abord, dans l'ordre du message, puis `stored` — le texte des PDF que
/// l'enfant extracteur a rangé, lu par [`crate::Store::attachment_texts`] —, le tout sous le même
/// plafond. Les PDF viennent du store et **pas** de `parsed` : la passe des vecteurs ne lit que le
/// début d'un très gros message, et un PDF placé au-delà manquerait au vecteur sans manquer à
/// l'index — la divergence que ce module existe pour empêcher.
///
/// Ce qui n'est pas lisible en texte est **ignoré en silence** : une image ou un PDF que l'enfant
/// n'a pas encore lu n'est pas une anomalie, c'est le cas ordinaire, et signaler chacun noierait ce
/// qui mérite de l'être.
pub fn attachment_text(parsed: &mail_parser::Message<'_>, stored: &[String], out: &mut String) {
    let ceiling = out.len().saturating_add(BUDGET);
    let free = parsed.attachments().filter_map(part_text);
    for text in free.chain(stored.iter().cloned()) {
        if out.len() >= ceiling {
            break;
        }
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

// ---------------------------------------------------------------------------------------------
// Les PDF : reconnus ici, jamais lus ici
// ---------------------------------------------------------------------------------------------
//
// Lire un PDF est de l'analyse d'entrée hostile, et elle n'aura lieu que dans un processus enfant
// (`docs/PHASE-4.md`, entrée du 2026-10-20). Ce module ne fait que **reconnaître** une pièce pour
// un PDF : c'est du MIME et dix octets, sans dépendance, et c'est ce que le recensement et l'enfant
// doivent faire de la même façon — un banc qui réimplémente la règle qu'il compte ne compte rien.

/// Où chercher la signature `%PDF-` : la norme l'autorise dans les 1 024 premiers octets, et des
/// producteurs mettent du bruit devant (ISO 32000-1, annexe H).
pub const SIGNATURE_WINDOW: usize = 1024;

/// Combien de `message/rfc822` joints on traverse : un message transféré « en pièce jointe », et
/// le transfert d'un transfert. Au-delà, rien — la borne est ce qui empêche un expéditeur de
/// décider de la profondeur du parcours.
pub const MAX_DEPTH: u8 = 2;

/// Par où une pièce se présente comme un PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Route {
    /// Son type le dit : `application/pdf`, ou un de ses alias.
    Declared,
    /// Un type générique — `application/octet-stream`, absent, `application/x-download`… — et un
    /// nom de fichier en `.pdf`. Le gisement que le recensement du 2026-09-22 ne voyait pas : il
    /// classait par type seul.
    NamedOctetStream,
    /// Ni l'un ni l'autre.
    Undeclared,
}

/// Ce que la détection dit d'une pièce, **axe par axe**.
///
/// Un verdict et non un booléen : le recensement compte chaque axe — combien de pièces ont la
/// signature sans le type, ou le type sans la signature —, et l'enfant n'en retient qu'une
/// combinaison, [`Verdict::accepted`]. La règle est écrite une fois, et les deux la lisent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Verdict {
    pub route: Route,
    /// `%PDF-` figure dans les [`SIGNATURE_WINDOW`] premiers octets décodés.
    pub signature: bool,
    /// mail-parser n'a pas su décoder la partie — un base64 coupé par la fin du message — et en a
    /// gardé les octets **bruts**. Ce ne sont plus ceux du PDF : la lire serait lire du base64.
    pub encoding_problem: bool,
    /// mail-parser a décodé la partie **comme du texte** — c'est ce qu'il fait d'une partie sans
    /// `Content-Type`, `text/plain` par défaut selon la RFC 2045 — : ses octets sont passés par
    /// `from_utf8_lossy` ou par un jeu de caractères, et chaque séquence invalide d'un fichier
    /// binaire y est devenue un U+FFFD. La signature, en ASCII, survit ; le PDF, non.
    pub text_decoded: bool,
    /// 0 au premier niveau, 1 dans un `message/rfc822` joint, [`MAX_DEPTH`] au plus.
    pub depth: u8,
}

impl Verdict {
    /// La règle : un type ou un nom qui annonce un PDF, **et** la signature, **et** les octets
    /// tels qu'ils ont été envoyés.
    ///
    /// Le type seul ne suffit pas — une pièce `application/pdf` vide ou tronquée n'en est pas
    /// un —, et la signature seule non plus, tant que le recensement n'a pas dit combien de
    /// pièces n'ont qu'elle : ce sont celles dont on ne sait rien d'autre.
    ///
    /// `encoding_problem` n'a aujourd'hui aucun cas à lui seul : mail-parser range une partie mal
    /// décodée **en texte**, que `text_decoded` écarte déjà, et un base64 coupé garde des octets
    /// base64 où la signature n'est pas. Il est gardé pour le jour où mail-parser ferait
    /// autrement — et aucun test ne le distingue, faute de cas pour le faire.
    #[must_use]
    pub fn accepted(&self) -> bool {
        self.route != Route::Undeclared
            && self.signature
            && !self.encoding_problem
            && !self.text_decoded
    }
}

/// Une pièce jointe feuille — pas un message joint, qu'on traverse —, avec son verdict.
#[derive(Debug, Clone, Copy)]
pub struct Piece<'a> {
    /// Son rang dans le parcours en profondeur des pièces du message, imbriquées comprises :
    /// stable pour un même blob, puisque l'analyse l'est. Ce n'est **pas** le rang de
    /// `parsed.attachments()` dès qu'un message joint précède la pièce.
    pub rank: u32,
    pub verdict: Verdict,
    pub part: &'a mail_parser::MessagePart<'a>,
}

/// Toutes les pièces feuilles d'un message, chacune avec ce que la détection en dit.
///
/// **Toutes**, et pas seulement les PDF : le recensement compte aussi celles que la règle écarte.
/// Les messages joints sont traversés jusqu'à [`MAX_DEPTH`], jamais rangés eux-mêmes.
#[must_use]
pub fn pieces<'a>(parsed: &'a mail_parser::Message<'a>) -> Vec<Piece<'a>> {
    let mut out = Vec::new();
    walk(parsed, 0, &mut out);
    out
}

fn walk<'a>(message: &'a mail_parser::Message<'a>, depth: u8, out: &mut Vec<Piece<'a>>) {
    for part in message.attachments() {
        if let Some(nested) = part.message() {
            if depth < MAX_DEPTH {
                walk(nested, depth + 1, out);
            }
            continue;
        }
        // Un rang au-delà de `u32` demanderait quatre milliards de pièces dans un message : il
        // est saturé plutôt que de faire paniquer une moisson.
        let rank = u32::try_from(out.len()).unwrap_or(u32::MAX);
        out.push(Piece {
            rank,
            verdict: verdict_of(part, depth),
            part,
        });
    }
}

/// Les types qui annoncent un PDF. `application/pdf` est le seul enregistré (RFC 8118) ; les
/// autres sont des alias que des logiciels de facturation et de vieux clients écrivent encore.
const PDF_TYPES: &[&str] = &[
    "application/pdf",
    "application/x-pdf",
    "application/acrobat",
    "application/vnd.pdf",
];

/// Les types qui ne disent rien : seul le nom de fichier parle alors.
const GENERIC_TYPES: &[&str] = &[
    "application/octet-stream",
    "application/x-download",
    "application/download",
    "application/force-download",
    "application/binary",
    "application/unknown",
    "binary/octet-stream",
];

fn verdict_of(part: &mail_parser::MessagePart<'_>, depth: u8) -> Verdict {
    // `mime_of` range une partie **sans** `Content-Type` sous `application/octet-stream` : c'est
    // le défaut de la RFC 2046 pour une pièce, et c'est ce qui fait entrer « nom en .pdf sans
    // type » dans la voie générique.
    let mime = mime_of(part);
    let route = if PDF_TYPES.contains(&mime.as_str()) {
        Route::Declared
    } else if GENERIC_TYPES.contains(&mime.as_str()) && named_pdf(part) {
        Route::NamedOctetStream
    } else {
        Route::Undeclared
    };
    let contents = part.contents();
    let head = &contents[..contents.len().min(SIGNATURE_WINDOW)];
    Verdict {
        route,
        signature: head.windows(5).any(|window| window == b"%PDF-"),
        encoding_problem: part.is_encoding_problem,
        text_decoded: !part.is_binary(),
        depth,
    }
}

/// Ce qu'on a tiré d'un PDF, ou pourquoi rien — rangé dans `message_pdfs.state`.
///
/// Un ensemble **fermé** : chaque code a un sens arrêté, et une ligne qui en porte un autre
/// — écrite par un binaire plus récent — est lue comme absente plutôt que devinée.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PdfState {
    /// Un texte, rangé.
    Text = 0,
    /// Lisible, sans texte : un scan, ou une police sans table de caractères.
    NoText = 1,
    /// Chiffré par un mot de passe d'ouverture.
    Encrypted = 2,
    /// Le lecteur l'a refusé : ce n'est pas un PDF qu'il sait lire.
    Invalid = 3,
    /// Au-delà de ce que l'enfant accepte d'analyser.
    TooBig = 4,
    /// A tué l'enfant, deux fois.
    Crashed = 5,
    /// A dépassé son budget de temps **de calcul**.
    TimedOut = 6,
    /// A dépassé la borne de mémoire de l'enfant.
    OutOfMemory = 7,
}

impl PdfState {
    /// Le code rangé en base.
    #[must_use]
    pub const fn code(self) -> i64 {
        self as i64
    }

    /// L'état d'un code rangé, s'il en est un.
    #[must_use]
    pub const fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => Self::Text,
            1 => Self::NoText,
            2 => Self::Encrypted,
            3 => Self::Invalid,
            4 => Self::TooBig,
            5 => Self::Crashed,
            6 => Self::TimedOut,
            7 => Self::OutOfMemory,
            _ => return None,
        })
    }
}

/// Compresse le texte d'un PDF pour le ranger.
///
/// zstd, au niveau des blobs : le texte d'une facture compresse d'un facteur trois à cinq, et c'est
/// ce qui tient le principe « rien de volumineux dans SQLite ».
#[must_use]
pub fn pack(text: &str) -> Vec<u8> {
    // Compresser une tranche en mémoire n'échoue pas en pratique ; le jour où ce serait le cas, le
    // texte en clair est rangé tel quel, et `unpack` le reconnaît à l'absence de l'en-tête zstd.
    zstd::bulk::compress(text.as_bytes(), 3).unwrap_or_else(|_| text.as_bytes().to_vec())
}

/// Relit un texte rangé par [`pack`].
///
/// **Borné** : un texte rangé ne dépasse jamais [`BUDGET`], et une décompression qui irait au-delà
/// — une ligne abîmée, ou écrite par quelqu'un d'autre que la passe — rend `None` au lieu d'allouer
/// ce qu'elle annonce.
#[must_use]
pub fn unpack(bytes: &[u8]) -> Option<String> {
    const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
    if !bytes.starts_with(&ZSTD_MAGIC) {
        return std::str::from_utf8(bytes)
            .ok()
            .filter(|text| text.len() <= BUDGET)
            .map(str::to_owned);
    }
    let raw = zstd::bulk::decompress(bytes, BUDGET).ok()?;
    String::from_utf8(raw).ok()
}

/// Le nom de la pièce finit-il par `.pdf`, casse ignorée ?
fn named_pdf(part: &mail_parser::MessagePart<'_>) -> bool {
    part.attachment_name().is_some_and(|name| {
        let name = name.trim().trim_matches('"').trim();
        name.len() > 4
            && name.is_char_boundary(name.len() - 4)
            && name[name.len() - 4..].eq_ignore_ascii_case(".pdf")
    })
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
        attachment_text(&parsed, &[], &mut out);
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

    // --- La détection des PDF ----------------------------------------------------------------

    /// Un encodeur base64 minimal : les fixtures portent des octets binaires, que seul un
    /// transfert encodé garde intacts dans un message.
    fn base64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        // Des lignes de 76 caractères, comme un client réel.
        out.as_bytes()
            .chunks(76)
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    /// Des octets de PDF qui ne sont **pas** de l'UTF-8 : c'est ce qui révèle un décodage texte.
    const PDF_BYTES: &[u8] =
        b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n1 0 obj\n<< >>\nendobj\n\x80\x81\xfe\xff";

    /// Une partie encodée en base64, avec les en-têtes donnés.
    fn part(headers: &str, bytes: &[u8]) -> String {
        format!(
            "{headers}\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            base64(bytes)
        )
    }

    /// Un message `multipart/mixed` : un corps texte, puis les parties données.
    fn multipart(parts: &[String]) -> Vec<u8> {
        let mut raw = String::from(
            "From: a@exemple.invalid\r\n\
             Subject: porteur\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=SEP\r\n\
             \r\n\
             --SEP\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Le corps ordinaire.\r\n",
        );
        for part in parts {
            raw.push_str("--SEP\r\n");
            raw.push_str(part);
        }
        raw.push_str("--SEP--\r\n");
        raw.into_bytes()
    }

    /// Les verdicts d'un message, avec les octets de chaque pièce.
    fn verdicts(raw: &[u8]) -> Vec<(u32, Verdict, Vec<u8>)> {
        let parsed = MessageParser::default().parse(raw).unwrap();
        pieces(&parsed)
            .into_iter()
            .map(|piece| (piece.rank, piece.verdict, piece.part.contents().to_vec()))
            .collect()
    }

    fn declared(name: &str) -> String {
        format!(
            "Content-Type: application/pdf; name=\"{name}\"\r\n\
             Content-Disposition: attachment; filename=\"{name}\""
        )
    }

    #[test]
    fn a_declared_pdf_with_its_signature_is_accepted_with_its_bytes_intact() {
        let found = verdicts(&multipart(&[part(&declared("f.pdf"), PDF_BYTES)]));
        assert_eq!(found.len(), 1, "{found:?}");
        let (rank, verdict, bytes) = &found[0];
        assert_eq!(*rank, 0);
        assert_eq!(verdict.route, Route::Declared);
        assert!(verdict.accepted(), "{verdict:?}");
        assert_eq!(
            bytes.as_slice(),
            PDF_BYTES,
            "les octets ont changé en route"
        );
    }

    /// Le gisement que le recensement par type ne voyait pas.
    #[test]
    fn a_generic_type_with_a_pdf_name_is_accepted_whatever_the_case() {
        let headers = "Content-Type: application/octet-stream; name=\"FACTURE-2608.PDF\"\r\n\
                       Content-Disposition: attachment; filename=\"FACTURE-2608.PDF\"";
        let found = verdicts(&multipart(&[part(headers, PDF_BYTES)]));
        assert_eq!(found[0].1.route, Route::NamedOctetStream);
        assert!(found[0].1.accepted(), "{:?}", found[0].1);

        // Le contrôle inverse : le même type sans le nom ne dit rien.
        let headers = "Content-Type: application/octet-stream; name=\"facture.bin\"\r\n\
                       Content-Disposition: attachment; filename=\"facture.bin\"";
        let found = verdicts(&multipart(&[part(headers, PDF_BYTES)]));
        assert_eq!(found[0].1.route, Route::Undeclared);
        assert!(!found[0].1.accepted());
    }

    /// Une partie sans `Content-Type` est du texte pour mail-parser : ses octets binaires y
    /// deviennent des U+FFFD. La signature survit, le fichier non — d'où le refus.
    #[test]
    fn a_part_without_a_type_is_decoded_as_text_and_refused() {
        let headers = "Content-Disposition: attachment; filename=\"x.pdf\"";
        let found = verdicts(&multipart(&[part(headers, PDF_BYTES)]));
        assert_eq!(found.len(), 1, "{found:?}");
        let (_, verdict, bytes) = &found[0];
        assert_eq!(verdict.route, Route::NamedOctetStream);
        assert!(verdict.signature, "la signature est en ASCII, elle survit");
        assert!(verdict.text_decoded, "{verdict:?}");
        assert!(!verdict.accepted(), "{verdict:?}");
        // La raison du refus, vérifiée et non supposée : les octets ne sont plus ceux envoyés.
        assert_ne!(bytes.as_slice(), PDF_BYTES);
    }

    #[test]
    fn a_pdf_type_without_the_signature_is_refused() {
        let found = verdicts(&multipart(&[part(
            &declared("f.pdf"),
            b"pas un pdf du tout",
        )]));
        assert!(!found[0].1.signature);
        assert!(!found[0].1.accepted());
        // Et vide.
        let found = verdicts(&multipart(&[part(&declared("f.pdf"), b"")]));
        assert!(
            found.iter().all(|(_, verdict, _)| !verdict.accepted()),
            "{found:?}"
        );
    }

    /// La norme tolère du bruit devant la signature, dans le premier kilo-octet et pas après.
    #[test]
    fn the_signature_is_looked_for_in_the_first_kilobyte_only() {
        let mut near = vec![b' '; SIGNATURE_WINDOW - 5];
        near.extend_from_slice(PDF_BYTES);
        assert!(
            verdicts(&multipart(&[part(&declared("f.pdf"), &near)]))[0]
                .1
                .accepted()
        );

        let mut far = vec![b' '; SIGNATURE_WINDOW];
        far.extend_from_slice(PDF_BYTES);
        let found = verdicts(&multipart(&[part(&declared("f.pdf"), &far)]));
        assert!(!found[0].1.signature, "trouvée au-delà de la fenêtre");
        assert!(!found[0].1.accepted());
    }

    /// Un base64 coupé par la fin du message : mail-parser garde les octets **bruts**, donc du
    /// base64 et non un PDF.
    #[test]
    fn a_pdf_cut_by_the_end_of_the_message_is_refused() {
        let mut raw = String::from_utf8(multipart(&[])).unwrap();
        raw.truncate(raw.rfind("--SEP--").unwrap());
        raw.push_str("--SEP\r\n");
        raw.push_str(&declared("f.pdf"));
        raw.push_str("\r\nContent-Transfer-Encoding: base64\r\n\r\n");
        // La moitié du base64, et plus rien : ni fin de ligne, ni frontière.
        let encoded = base64(&[PDF_BYTES, &[b'x'; 300]].concat());
        raw.push_str(&encoded[..encoded.len() / 2]);
        let found = verdicts(raw.as_bytes());
        assert!(
            found.iter().all(|(_, verdict, _)| !verdict.accepted()),
            "une pièce coupée a été acceptée : {found:?}"
        );
    }

    /// Une image qui porte un nom en `.pdf` et la signature n'est pas pour autant un PDF annoncé.
    #[test]
    fn an_image_named_pdf_is_undeclared() {
        let headers = "Content-Type: image/png; name=\"x.pdf\"\r\n\
                       Content-Disposition: attachment; filename=\"x.pdf\"";
        let found = verdicts(&multipart(&[part(headers, PDF_BYTES)]));
        assert_eq!(found[0].1.route, Route::Undeclared);
        assert!(found[0].1.signature);
        assert!(!found[0].1.accepted());
    }

    /// Un message transféré en pièce jointe : son PDF est trouvé, un niveau plus bas.
    fn forwarded(inner: &[u8]) -> String {
        format!(
            "Content-Type: message/rfc822\r\n\
             Content-Disposition: attachment; filename=\"transfert.eml\"\r\n\r\n{}\r\n",
            String::from_utf8_lossy(inner)
        )
    }

    /// Le même message, avec une autre frontière : deux niveaux imbriqués ne peuvent pas
    /// partager la leur.
    fn multipart_with(boundary: &str, parts: &[String]) -> Vec<u8> {
        String::from_utf8(multipart(parts))
            .unwrap()
            .replace("SEP", boundary)
            .into_bytes()
    }

    #[test]
    fn a_pdf_inside_a_forwarded_message_is_found_one_level_down() {
        let inner = multipart_with("INNER", &[part(&declared("dedans.pdf"), PDF_BYTES)]);
        let found = verdicts(&multipart(&[forwarded(&inner)]));
        assert_eq!(
            found.len(),
            1,
            "le message joint lui-même n'est pas une pièce : {found:?}"
        );
        assert_eq!(found[0].1.depth, 1);
        assert!(found[0].1.accepted(), "{:?}", found[0].1);
        assert_eq!(found[0].2.as_slice(), PDF_BYTES);
    }

    /// La borne de profondeur : un PDF au troisième niveau de transfert n'est pas parcouru.
    #[test]
    fn nesting_beyond_the_limit_is_not_walked() {
        let level3 = multipart_with("L3", &[part(&declared("trois.pdf"), PDF_BYTES)]);
        let level2 = multipart_with("L2", &[forwarded(&level3)]);
        let level1 = multipart_with("L1", &[forwarded(&level2)]);
        let found = verdicts(&multipart(&[forwarded(&level1)]));
        assert!(
            found.is_empty(),
            "parcouru au-delà de {MAX_DEPTH} : {found:?}"
        );

        // Le contrôle inverse : un niveau de moins, et il est trouvé, à la profondeur limite.
        let found = verdicts(&multipart(&[forwarded(&level2)]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].1.depth, MAX_DEPTH);
    }

    /// Le rang suit le parcours en profondeur : la pièce d'un message joint s'intercale.
    #[test]
    fn ranks_follow_the_message_order_including_nested_pieces() {
        let inner = multipart_with("INNER", &[part(&declared("b.pdf"), PDF_BYTES)]);
        let image = "Content-Type: image/png; name=\"c.png\"\r\n\
                     Content-Disposition: attachment; filename=\"c.png\"";
        let found = verdicts(&multipart(&[
            part(&declared("a.pdf"), PDF_BYTES),
            forwarded(&inner),
            part(image, b"\x89PNG\r\n"),
        ]));
        let ranks: Vec<(u32, u8, bool)> = found
            .iter()
            .map(|(rank, verdict, _)| (*rank, verdict.depth, verdict.accepted()))
            .collect();
        assert_eq!(ranks, vec![(0, 0, true), (1, 1, true), (2, 0, false)]);
    }

    /// Un nom encodé selon la RFC 2231 est lu une fois décodé.
    #[test]
    fn an_encoded_file_name_is_read_decoded() {
        let headers = "Content-Type: application/octet-stream\r\n\
                       Content-Disposition: attachment; filename*=UTF-8''facture%C3%A9t%C3%A9.pdf";
        let found = verdicts(&multipart(&[part(headers, PDF_BYTES)]));
        assert_eq!(
            found[0].1.route,
            Route::NamedOctetStream,
            "{:?}",
            found[0].1
        );
    }

    /// Entrée hostile : des messages cassés de toutes les façons ne font jamais paniquer, et ne
    /// rendent jamais une pièce acceptée qui n'a pas ses octets.
    #[test]
    fn broken_messages_never_panic() {
        let pdf_b64 = base64(PDF_BYTES);
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"\r\n\r\n".to_vec(),
            b"Content-Type: multipart/mixed; boundary=SEP\r\n\r\n".to_vec(),
            b"Content-Type: multipart/mixed\r\n\r\n--SEP\r\n".to_vec(),
            format!("Content-Type: application/pdf\r\nContent-Transfer-Encoding: base64\r\n\r\n{pdf_b64}")
                .into_bytes(),
            b"Content-Type: multipart/mixed; boundary=SEP\r\n\r\n--SEP\r\nContent-Type: application/pdf; name=\".pdf\"\r\n\r\n%PDF-".to_vec(),
            b"Content-Type: multipart/mixed; boundary=SEP\r\n\r\n--SEP\r\nContent-Type: \r\nContent-Disposition: attachment; filename=\"\"\r\n\r\n%PDF-1.4\r\n--SEP--".to_vec(),
            b"Content-Type: multipart/mixed; boundary=SEP\r\n\r\n--SEP\r\nContent-Type: message/rfc822\r\n\r\n--SEP--".to_vec(),
            format!("Content-Type: multipart/mixed; boundary=SEP\r\n\r\n--SEP\r\nContent-Type: application/pdf\r\nContent-Transfer-Encoding: base64\r\n\r\n%%%%{pdf_b64}====\r\n--SEP--")
                .into_bytes(),
            [b"Content-Type: multipart/mixed; boundary=SEP\r\n\r\n--SEP\r\nContent-Type: application/pdf\r\n\r\n".as_slice(), &[0xFF; 64], b"\r\n--SEP--"].concat(),
        ];
        for raw in &cases {
            let Some(parsed) = MessageParser::default().parse(raw) else {
                continue;
            };
            for piece in pieces(&parsed) {
                if piece.verdict.accepted() {
                    assert!(
                        piece.part.contents().windows(5).any(|w| w == b"%PDF-"),
                        "acceptée sans signature : {:?}",
                        piece.verdict
                    );
                }
            }
        }
    }

    /// Le contrôle négatif d'origine tient toujours : reconnaître un PDF n'est pas le lire, et
    /// `attachment_text` ne rend rien d'un PDF, même accepté.
    #[test]
    fn an_accepted_pdf_still_gives_no_text_here() {
        let raw = multipart(&[part(&declared("f.pdf"), PDF_BYTES)]);
        assert!(verdicts(&raw)[0].1.accepted());
        assert_eq!(extracted(&raw), "");
    }
}

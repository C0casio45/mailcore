//! Le texte d'un PDF joint — `docs/PHASE-4.md`, entrée du 2026-10-20.
//!
//! ## Le seul crate qui lit un PDF, et il ne s'exécute que dans un enfant
//!
//! Lire un PDF est de l'analyse d'entrée hostile, dans un format qui en a beaucoup coûté à
//! l'informatique. Ce crate n'est appelé que par l'enfant `--mailcore-attachments` du démon : une
//! panique, une boucle sans fin, une bombe de décompression ou un débordement de pile n'y tuent que
//! lui, et le parent — le démon, ou la coquille qui l'embarque — ne voit jamais que du texte. Il
//! est **lié** dans l'exécutable de la coquille, puisque l'enfant est le même exécutable relancé ;
//! il n'y est jamais **appelé**.
//!
//! ## Ce qu'il appelle, et pourquoi exactement ça
//!
//! `pdf_inspector::process_pdf_mem_with_options` sur le document entier — un seul chargement, une
//! seule mise en page —, et, s'il échoue, par tranches de [`SLICE`] pages, puis page par page dans
//! une tranche qui échoue : une page indécodable ne coûte pas les autres. C'est l'appel que le banc
//! A2 a mesuré (`tools/pdf-runner`) ; en appeler un autre ici livrerait autre chose que ce qui a
//! été choisi. Jamais `pdf_inspector::extract_text_mem`, qui n'est que l'extracteur brut de lopdf.
//!
//! ## Ce qu'il rend
//!
//! Un état fermé ([`mailcore::attachtext::PdfState`]) et, pour un texte, ce texte normalisé en NFC,
//! débarrassé des caractères de contrôle et du balisage que la mise en page ajoute, et plafonné à
//! [`mailcore::attachtext::BUDGET`] sur une frontière de caractère.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::time::{Duration, Instant};

use mailcore::attachtext::{BUDGET, PdfState};
use pdf_inspector::{PdfError, PdfOptions};
use unicode_normalization::UnicodeNormalization;

/// La révision de la règle d'extraction — rangée avec chaque pièce lue. À monter quand ce que ce
/// crate rend d'un même PDF change : bibliothèque, normalisation, règle « sans texte ».
///
/// | révision | ce qui a changé |
/// |---|---|
/// | 1 | pdf-inspector 1.25.2, document entier puis tranches de 32 pages, NFC (2026-10-20) |
pub const REVISION: u32 = 1;

/// Au-delà, un PDF n'est pas analysé : [`PdfState::TooBig`]. Le plus gros du corpus de mesure fait
/// 22 Mio ; la borne laisse de la marge, et un fichier plus gros est presque toujours un scan.
pub const INPUT_LIMIT: usize = 64 * 1024 * 1024;

/// Combien de pages lire d'un coup quand le document entier échoue : un seul chargement pour
/// presque toutes les factures, et une page indécodable n'en coûte que sa tranche.
pub const SLICE: u32 = 32;

/// Le temps que l'extraction se donne à elle-même, sous le budget de calcul que le parent impose :
/// au-delà, elle rend ce qu'elle a lu — un texte partiel vaut mieux qu'une pièce tuée.
pub const TIME_BUDGET: Duration = Duration::from_secs(20);

/// La variable que pdf-inspector lit pour charger des tables de caractères **depuis le disque** :
/// posée, un nom écrit dans le PDF deviendrait un chemin de fichier — chemin absolu, `..`, ou
/// chemin UNC, donc une connexion SMB vers un hôte que l'expéditeur choisit. Le parent la retire de
/// l'environnement de l'enfant — l'enfant ne peut pas retirer une variable du sien sans `unsafe` ;
/// [`extract`] refuse de tourner si elle est là quand même.
pub const BCMAPS_ENV: &str = "PDF_INSPECTOR_BCMAPS_DIR";

/// Toutes les variables que les bibliothèques d'extraction lisent — relu dans les sources de
/// pdf-inspector 1.25.2 et lopdf 0.45 —, que qui lance l'extraction retire de son environnement.
/// [`BCMAPS_ENV`] pour la lecture de fichiers ; `PI_NO_MERGE` change le texte rendu, donc un même
/// PDF ne donnerait pas le même texte sur deux machines ; les deux dernières ne servent qu'au module
/// de vision, que ce crate ne compile pas — retirées quand même, pour qu'une montée de version qui
/// l'activerait ne trouve pas de chemin posé.
pub const ENVIRONMENT: [&str; 4] = [
    BCMAPS_ENV,
    "PI_NO_MERGE",
    "PDF_INSPECTOR_MODEL_CACHE",
    "ORT_DYLIB_PATH",
];

/// Ce qu'on a tiré d'un PDF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extract {
    pub state: PdfState,
    /// Vide hors [`PdfState::Text`].
    pub text: String,
}

impl Extract {
    fn empty(state: PdfState) -> Self {
        Self {
            state,
            text: String::new(),
        }
    }
}

/// L'extraction refuse de tourner.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// [`BCMAPS_ENV`] est posée : un PDF pourrait faire lire un fichier de la machine.
    #[error("la variable {BCMAPS_ENV} est posée : un PDF pourrait faire lire un fichier")]
    BcmapsDir,
}

/// Le texte d'un PDF.
///
/// # Errors
///
/// [`Refused`] si l'environnement rend l'extraction dangereuse — jamais pour un PDF illisible, qui
/// rend un état.
pub fn extract(pdf: &[u8]) -> Result<Extract, Refused> {
    extract_with(pdf, |name| std::env::var_os(name))
}

/// [`extract`], l'environnement lu par `env` — pour qu'un test pose la variable dangereuse sans
/// toucher à celui du processus, que l'édition 2024 ne laisse modifier qu'en `unsafe`.
///
/// # Errors
///
/// Comme [`extract`].
pub fn extract_with(
    pdf: &[u8],
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<Extract, Refused> {
    if env(BCMAPS_ENV).is_some() {
        return Err(Refused::BcmapsDir);
    }
    if pdf.len() > INPUT_LIMIT {
        return Ok(Extract::empty(PdfState::TooBig));
    }
    let started = Instant::now();
    let raw = match pdf_inspector::process_pdf_mem_with_options(pdf, PdfOptions::new()) {
        // `markdown` vaut `None` pour un document que la détection dit numérisé : aucun texte, et
        // ce n'est pas un échec.
        Ok(result) => result.markdown.unwrap_or_default(),
        Err(PdfError::Encrypted) => return Ok(Extract::empty(PdfState::Encrypted)),
        Err(PdfError::NotAPdf(_)) => return Ok(Extract::empty(PdfState::Invalid)),
        Err(_) => match by_slices(pdf, started) {
            Some(text) => text,
            None => return Ok(Extract::empty(PdfState::Invalid)),
        },
    };
    let text = cut(clean(&raw), BUDGET);
    if readable(&text) {
        Ok(Extract {
            state: PdfState::Text,
            text,
        })
    } else {
        Ok(Extract::empty(PdfState::NoText))
    }
}

/// Le document par tranches de [`SLICE`] pages, puis page par page dans une tranche qui échoue.
/// `None` si lopdf ne sait même pas en compter les pages : il n'y a rien à essayer une à une.
fn by_slices(pdf: &[u8], started: Instant) -> Option<String> {
    // La borne de chargement des flux d'objets et de références croisées — la seule que lopdf
    // applique. Les autres flux se décompressent sans borne : c'est la borne de mémoire de l'enfant,
    // posée par le système, qui les arrête.
    let options = lopdf::LoadOptions {
        max_decompressed_size: Some(8 * 1024 * 1024),
        ..Default::default()
    };
    let document = lopdf::Document::load_mem_with_options(pdf, options).ok()?;
    let pages: Vec<u32> = document.get_pages().keys().copied().collect();
    drop(document);
    let mut out = String::new();
    for slice in pages.chunks(SLICE as usize) {
        if started.elapsed() > TIME_BUDGET || out.len() >= BUDGET {
            break;
        }
        match pdf_inspector::process_pdf_mem_with_options(
            pdf,
            PdfOptions::new().pages(slice.iter().copied()),
        ) {
            Ok(result) => push(&mut out, result.markdown.as_deref().unwrap_or("")),
            Err(_) => {
                for &page in slice {
                    if started.elapsed() > TIME_BUDGET || out.len() >= BUDGET {
                        break;
                    }
                    if let Ok(result) = pdf_inspector::process_pdf_mem_with_options(
                        pdf,
                        PdfOptions::new().pages([page]),
                    ) {
                        push(&mut out, result.markdown.as_deref().unwrap_or(""));
                    }
                }
            }
        }
    }
    Some(out)
}

fn push(out: &mut String, text: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(text);
}

/// Le texte tel qu'il sera rangé : NFC, sans caractère de contrôle ni de zone privée, sans le
/// balisage Markdown que la mise en page ajoute, les blancs resserrés.
///
/// **NFC** parce que l'analyseur de l'index ne normalise rien : « é » décomposé en `e` + accent
/// combinant y serait coupé au diacritique, et la frappe ordinaire, composée, ne le trouverait pas.
/// Le balisage — `#`, `|`, `**` — n'est que de la ponctuation pour l'index, mais du bruit pour le
/// modèle qui vectorise. Les sauts de ligne restent : ils séparent des mots, et le protocole de
/// l'enfant les échappe.
#[must_use]
pub fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.nfc().collect::<String>().lines() {
        let line = line
            .trim_start_matches('#')
            .replace("**", "")
            .replace('|', " ");
        let mut kept = String::with_capacity(line.len());
        for c in line.chars() {
            if c.is_control() || ('\u{E000}'..='\u{F8FF}').contains(&c) {
                kept.push(' ');
            } else {
                kept.push(c);
            }
        }
        let words: Vec<&str> = kept.split_whitespace().collect();
        // Une ligne de tableau ou un séparateur `---` ne porte aucun mot.
        if words
            .iter()
            .all(|word| word.chars().all(|c| matches!(c, '-' | ':' | '=')))
        {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&words.join(" "));
    }
    out
}

/// Y a-t-il là un texte, ou du charabia ?
///
/// La règle de la révision 1, simple et écrite : au moins trois mots de deux caractères
/// alphanumériques, et moins d'un quart de caractères de remplacement. Une police sans table de
/// caractères rend des U+FFFD ; un scan, rien. Ni l'un ni l'autre ne doit entrer dans un index.
#[must_use]
pub fn readable(text: &str) -> bool {
    let words = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 2)
        .take(3)
        .count();
    let visible = text.chars().filter(|c| !c.is_whitespace()).count();
    let replaced = text.chars().filter(|&c| c == '\u{FFFD}').count();
    words >= 3 && replaced * 4 < visible.max(1)
}

/// Coupe au plus près de `budget` octets, sur une frontière de caractère.
fn cut(mut text: String, budget: usize) -> String {
    if text.len() > budget {
        let mut end = budget;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use lopdf::{Document, Object, Stream, StringFormat, dictionary};

    /// Un PDF d'une page, Helvetica en WinAnsi, dont le flux de contenu est `content`.
    fn document(content: &[u8]) -> Document {
        let mut doc = Document::with_version("1.5");
        doc.trailer.set(
            "ID",
            Object::Array(vec![
                Object::String(b"identifiant-1".to_vec(), StringFormat::Literal),
                Object::String(b"identifiant-2".to_vec(), StringFormat::Literal),
            ]),
        );
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => Object::Reference(font_id) },
        });
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Resources" => Object::Reference(resources_id),
            "Contents" => Object::Reference(content_id),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    fn bytes(mut doc: Document) -> Vec<u8> {
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    /// « Électricité facture 1234,56 » en WinAnsi : É = 0xC9, é = 0xE9.
    const POSITIVE: &[u8] = b"BT /F1 12 Tf 72 720 Td (\\311lectricit\\351 facture 1234,56) Tj ET";

    /// L'extraction, sur un fil à pile explicite, comme l'enfant la fera.
    fn read(pdf: &[u8]) -> Extract {
        let pdf = pdf.to_vec();
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || extract_with(&pdf, |_| None).unwrap())
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn a_plain_pdf_gives_its_words_with_their_accents_composed() {
        let found = read(&bytes(document(POSITIVE)));
        assert_eq!(found.state, PdfState::Text, "{found:?}");
        assert!(found.text.contains("Électricité"), "{found:?}");
        assert!(found.text.contains("1234,56"), "{found:?}");
        // Composé, pas décomposé : un « é » en `e` + accent combinant ne se retrouverait pas.
        assert!(!found.text.contains('\u{301}'), "{found:?}");
    }

    /// Des mots placés par des déplacements, sans caractère espace entre eux : l'extracteur brut
    /// les colle, celui-ci doit les séparer — c'est la raison du choix.
    #[test]
    fn words_placed_by_moves_come_out_separated() {
        let found = read(&bytes(document(
            b"BT /F1 12 Tf 72 720 Td (Bonjour) Tj 60 0 Td (monde) Tj 60 0 Td (encore) Tj ET",
        )));
        assert!(found.text.contains("Bonjour monde"), "{found:?}");
    }

    #[test]
    fn a_pdf_locked_by_an_opening_password_is_encrypted_and_gives_nothing() {
        let mut doc = document(POSITIVE);
        let version = lopdf::EncryptionVersion::V2 {
            document: &doc,
            owner_password: "proprietaire",
            user_password: "ouverture",
            key_length: 128,
            permissions: lopdf::Permissions::all(),
        };
        let state = lopdf::EncryptionState::try_from(version).unwrap();
        doc.encrypt(&state).unwrap();
        let found = read(&bytes(doc));
        assert_eq!(found.state, PdfState::Encrypted, "{found:?}");
        assert!(found.text.is_empty());
    }

    /// Le contrôle inverse : chiffré par le seul mot de passe du propriétaire — le cas ordinaire
    /// d'une facture « protégée » —, il s'ouvre avec le mot de passe vide et se lit.
    #[test]
    fn a_pdf_locked_by_its_owner_only_is_read() {
        let mut doc = document(POSITIVE);
        let version = lopdf::EncryptionVersion::V2 {
            document: &doc,
            owner_password: "proprietaire",
            user_password: "",
            key_length: 128,
            permissions: lopdf::Permissions::all(),
        };
        let state = lopdf::EncryptionState::try_from(version).unwrap();
        doc.encrypt(&state).unwrap();
        let found = read(&bytes(doc));
        assert_eq!(found.state, PdfState::Text, "{found:?}");
        assert!(found.text.contains("facture"), "{found:?}");
    }

    /// Ce qui n'est pas un PDF rend un état, jamais ses octets.
    #[test]
    fn what_is_not_a_pdf_is_invalid_never_raw_bytes() {
        for raw in [
            b"".as_slice(),
            b"pas un pdf du tout".as_slice(),
            b"%PDF-1.7".as_slice(),
            b"%PDF-1.7\n%%EOF".as_slice(),
        ] {
            let found = read(raw);
            assert_ne!(found.state, PdfState::Text, "{raw:?} : {found:?}");
            assert!(found.text.is_empty());
        }
    }

    /// Entrée hostile : des PDF cassés de toutes les façons ne font jamais paniquer, rendent un
    /// état, et ne rendent un texte que s'il est lisible.
    #[test]
    fn broken_pdfs_never_panic() {
        let good = bytes(document(POSITIVE));
        let mut cases: Vec<Vec<u8>> = Vec::new();
        // Tronqué au quart, à la moitié, aux trois quarts.
        for cut in [good.len() / 4, good.len() / 2, good.len() * 3 / 4] {
            cases.push(good[..cut].to_vec());
        }
        // Du bruit après `%%EOF`, et devant l'en-tête.
        cases.push([good.as_slice(), b"\n\x00\x01bruit apres la fin".as_slice()].concat());
        cases.push([b"bruit devant".as_slice(), good.as_slice()].concat());
        // Des décalages de xref faux : chaque chiffre de la table remplacé.
        if let Some(at) = good.windows(4).position(|window| window == b"xref") {
            let mut broken = good.clone();
            for byte in &mut broken[at..] {
                if byte.is_ascii_digit() {
                    *byte = b'9';
                }
            }
            cases.push(broken);
        }
        // Une chaîne `/Prev` qui boucle sur elle-même, et un objet qui se désigne lui-même.
        cases.push(
            b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
              2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
              3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>\nendobj\n\
              4 0 obj\n4 0 R\nendobj\n\
              xref\n0 5\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n\
              0000000115 00000 n \n0000000180 00000 n \n\
              trailer\n<< /Size 5 /Root 1 0 R /Prev 205 >>\nstartxref\n205\n%%EOF\n"
                .to_vec(),
        );
        // Dix mille tableaux imbriqués.
        let mut deep = b"%PDF-1.4\n1 0 obj\n".to_vec();
        deep.extend(std::iter::repeat_n(b'[', 10_000));
        deep.extend(std::iter::repeat_n(b']', 10_000));
        deep.extend_from_slice(b"\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n");
        cases.push(deep);
        // Un `/Length` énorme.
        cases.push(
            String::from_utf8_lossy(&good)
                .replacen("/Length", "/Length 99999999999 /L", 1)
                .into_bytes(),
        );

        for (index, pdf) in cases.iter().enumerate() {
            let started = Instant::now();
            let found = read(pdf);
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "cas {index} : {:?}",
                started.elapsed()
            );
            if found.state == PdfState::Text {
                assert!(readable(&found.text), "cas {index} : {found:?}");
            } else {
                assert!(found.text.is_empty(), "cas {index} : {found:?}");
            }
        }
    }

    /// Une bombe de décompression dans un flux de contenu : la page est refusée par la borne de la
    /// bibliothèque, et rien d'autre ne sort. La mémoire, elle, est l'affaire de la borne que le
    /// système pose à l'enfant — un test unitaire ne la voit pas, et ne prétend pas la voir.
    #[test]
    fn a_decompression_bomb_gives_no_text() {
        let mut doc = document(b"");
        let content_id = doc
            .objects
            .iter()
            .find_map(|(id, object)| matches!(object, Object::Stream(_)).then_some(*id))
            .unwrap();
        let mut bomb = Stream::new(dictionary! {}, vec![0u8; 80 * 1024 * 1024]);
        bomb.compress().unwrap();
        assert!(
            bomb.content.len() < 1024 * 1024,
            "la bombe n'en est pas une"
        );
        doc.objects.insert(content_id, Object::Stream(bomb));
        let found = read(&bytes(doc));
        assert_ne!(found.state, PdfState::Text, "{:?}", found.state);
    }

    /// Le plafond coupe sur une frontière de caractère. Cent soixante pages de cinquante lignes —
    /// le texte doit rester **dans** la page : celui qui en sort est écarté par la bibliothèque, à
    /// raison, et la première version de ce test le croyait extrait.
    #[test]
    fn a_long_text_is_cut_at_the_budget_on_a_character_boundary() {
        let line = "(\\351t\\351 \\351t\\351 \\351t\\351 \\351t\\351 \\351t\\351 \\351t\\351) Tj 0 -14 Td ";
        let mut content = b"BT /F1 10 Tf 40 800 Td ".to_vec();
        for _ in 0..50 {
            content.extend_from_slice(line.as_bytes());
        }
        content.extend_from_slice(b"ET");
        let mut doc = document(&content);
        let pages_id = doc
            .catalog()
            .unwrap()
            .get(b"Pages")
            .unwrap()
            .as_reference()
            .unwrap();
        let first = doc.get_pages()[&1];
        let page = doc.get_object(first).unwrap().clone();
        let mut kids = vec![Object::Reference(first)];
        for _ in 1..160 {
            kids.push(Object::Reference(doc.add_object(page.clone())));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Count" => 160,
                "Kids" => kids,
            }),
        );
        let found = read(&bytes(doc));
        assert_eq!(found.state, PdfState::Text, "{:?}", found.state);
        assert!(found.text.len() <= BUDGET, "{} octets", found.text.len());
        assert!(found.text.len() > BUDGET / 2, "{} octets", found.text.len());
    }

    /// La barrière : posée, la variable fait lire un fichier dont le PDF choisit le nom.
    #[test]
    fn the_cmap_directory_variable_stops_everything() {
        let pdf = bytes(document(POSITIVE));
        assert_eq!(
            extract_with(&pdf, |name| (name == BCMAPS_ENV)
                .then(|| "C:\\cartes".into())),
            Err(Refused::BcmapsDir)
        );
        // Le contrôle inverse : sans elle, le même PDF se lit.
        assert_eq!(extract_with(&pdf, |_| None).unwrap().state, PdfState::Text);
    }

    #[test]
    fn cleaning_composes_and_strips_what_is_not_text() {
        let raw =
            "## Re\u{301}sume\u{301}\n| Montant | 12,30 |\n|---|---|\n**gras**\u{7}\u{E000}fin";
        let cleaned = clean(raw);
        assert!(cleaned.contains("Résumé"), "{cleaned:?}");
        assert!(cleaned.contains("Montant 12,30"), "{cleaned:?}");
        assert!(!cleaned.contains('|') && !cleaned.contains("**") && !cleaned.contains('#'));
        assert!(!cleaned.contains('\u{7}') && !cleaned.contains('\u{E000}'));
        assert!(!cleaned.contains("---"), "{cleaned:?}");
        // Le contrôle inverse : une phrase française propre ne bouge pas.
        assert_eq!(
            clean("Électricité : 1 234,56 € réglés."),
            "Électricité : 1 234,56 € réglés."
        );
    }

    #[test]
    fn readable_needs_words_and_rejects_replacement_characters() {
        assert!(readable("facture du mois de mars"));
        assert!(!readable(""));
        assert!(
            !readable("a b c"),
            "des lettres isolées ne sont pas des mots"
        );
        assert!(!readable(
            "mot \u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD} autre \u{FFFD}\u{FFFD} trois"
        ));
    }
}

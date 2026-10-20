//! Les PDF fabriqués des contrôles bloquants, par l'API d'écriture de lopdf 0.45.
//!
//! ## Pourquoi fabriqués, et pourquoi ici
//!
//! Un contrôle sur un PDF du corpus dirait ce que le corpus contient, pas ce que le banc mesure.
//! Ceux-ci sont écrits octet par octet — le flux de contenu est donné tel quel, sans compression —,
//! donc on sait exactement ce qu'un extracteur juste doit en tirer. Ils sont fabriqués par le
//! lecteur lui-même (`pdf-runner fixture <nom>`) : le parent n'a pas à dépendre de lopdf, et les
//! octets passent ensuite par le chemin normal, celui des documents réels.
//!
//! - `positive` : une page Helvetica en `WinAnsiEncoding` qui porte « Électricité facture
//!   1234,56 ». Les accents sont en **octal WinAnsi** dans la chaîne (`\311`, `\351`) : c'est ainsi
//!   qu'un producteur réel les écrit, et un extracteur qui lirait l'UTF-8 n'y verrait rien.
//! - `wiring` : `(Bonjour) Tj 60 0 Td (monde) Tj`, **sans** caractère espace. Les deux mots ne
//!   sont séparés que par leur position : un extracteur brut les colle, un extracteur qui fait de
//!   la mise en page les sépare. C'est ce qui prouve que le banc mesure deux choses différentes.
//! - `cid-sans-tounicode` : une police `Type0` en `Identity-H` sans `ToUnicode` ni programme de
//!   police — des numéros de glyphes que rien ne relie à des caractères. Aucun texte juste n'en
//!   sort ; il sert à vérifier qu'un candidat rend la main.

use lopdf::{Dictionary, Document, Object, Stream, dictionary};

/// Le PDF fabriqué de ce nom.
pub(crate) fn by_name(name: &str) -> Option<Result<Vec<u8>, lopdf::Error>> {
    match name {
        "positive" => Some(positive()),
        "wiring" => Some(wiring()),
        "cid-sans-tounicode" => Some(cid_without_tounicode()),
        _ => None,
    }
}

/// Les noms reconnus, pour le message d'usage.
pub(crate) const NAMES: &str = "positive, wiring, cid-sans-tounicode";

pub(crate) fn positive() -> Result<Vec<u8>, lopdf::Error> {
    one_page(
        b"BT /F1 12 Tf 72 720 Td (\\311lectricit\\351 facture 1234,56) Tj ET",
        |_| helvetica(),
    )
}

pub(crate) fn wiring() -> Result<Vec<u8>, lopdf::Error> {
    one_page(
        b"BT /F1 12 Tf 72 720 Td (Bonjour) Tj 60 0 Td (monde) Tj ET",
        |_| helvetica(),
    )
}

pub(crate) fn cid_without_tounicode() -> Result<Vec<u8>, lopdf::Error> {
    one_page(b"BT /F1 12 Tf 72 720 Td <0026004F0048> Tj ET", |document| {
        let descriptor = document.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "Absente",
            "Flags" => 4,
            "FontBBox" => vec![0.into(), (-200).into(), 1000.into(), 800.into()],
            "ItalicAngle" => 0,
            "Ascent" => 800,
            "Descent" => -200,
            "CapHeight" => 700,
            "StemV" => 80,
        });
        let descendant = document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "CIDFontType2",
            "BaseFont" => "Absente",
            "CIDSystemInfo" => dictionary! {
                "Registry" => Object::string_literal("Adobe"),
                "Ordering" => Object::string_literal("Identity"),
                "Supplement" => 0,
            },
            "FontDescriptor" => descriptor,
            "DW" => 1000,
        });
        dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => "Absente",
            "Encoding" => "Identity-H",
            "DescendantFonts" => vec![descendant.into()],
        }
    })
}

/// Une des quatorze polices standard : aucun programme de police à embarquer, et des métriques que
/// tout lecteur connaît.
fn helvetica() -> Dictionary {
    dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    }
}

/// Un document d'une page A4, une police `F1`, le flux de contenu donné **tel quel**.
fn one_page(
    content: &[u8],
    font: impl FnOnce(&mut Document) -> Dictionary,
) -> Result<Vec<u8>, lopdf::Error> {
    let mut document = Document::with_version("1.4");
    let pages = document.new_object_id();
    let font = font(&mut document);
    let font = document.add_object(font);
    let resources = document.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font },
    });
    let contents = document.add_object(Stream::new(dictionary! {}, content.to_vec()));
    let page = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages,
        "Contents" => contents,
        "Resources" => resources,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
    });
    document.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page.into()],
            "Count" => 1,
        }),
    );
    let catalog = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages,
    });
    document.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Les octets du flux sont ceux qu'on a écrits : l'octal WinAnsi est dans le fichier, pas une
    /// réécriture de lopdf en UTF-8 ou en hexadécimal.
    #[test]
    fn the_content_stream_is_written_as_given() {
        let pdf = positive().unwrap();
        assert!(pdf.starts_with(b"%PDF-1.4"));
        let wanted: &[u8] = b"(\\311lectricit\\351 facture 1234,56) Tj";
        assert!(pdf.windows(wanted.len()).any(|window| window == wanted));
        // Le contrôle de câblage n'a aucun espace entre ses deux chaînes.
        let wiring = wiring().unwrap();
        let wanted: &[u8] = b"(Bonjour) Tj 60 0 Td (monde) Tj";
        assert!(wiring.windows(wanted.len()).any(|window| window == wanted));
    }

    #[test]
    fn every_name_builds_and_an_unknown_one_is_none() {
        for name in NAMES.split(", ") {
            let pdf = by_name(name).unwrap().unwrap();
            assert!(lopdf::Document::load_mem(&pdf).is_ok(), "{name}");
        }
        assert!(by_name("inconnue").is_none());
    }

    /// Le PDF sans `ToUnicode` : chaque candidat rend la main, quelle que soit son issue.
    #[test]
    fn a_cid_font_without_tounicode_does_not_hang_anyone() {
        let pdf = cid_without_tounicode().unwrap();
        for candidate in crate::candidates::Candidate::ALL {
            let answer = crate::respond(candidate, crate::Input::whole(pdf.clone()));
            assert_ne!(
                answer.outcome,
                crate::Outcome::TooBig,
                "{}",
                candidate.name()
            );
        }
    }
}

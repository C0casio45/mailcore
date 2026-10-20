//! Les trois candidats, chacun par son API publique, page par page.
//!
//! ## Même harnais pour tous
//!
//! Chaque candidat reçoit les mêmes octets, charge le document **une** fois quand son API le
//! permet, et parcourt les pages en **continuant après une erreur** : une page illisible ne doit
//! pas coûter les autres, et le nombre de pages en échec est une mesure en soi. Le mot de passe
//! vide est essayé par les trois — pdf-inspector et lopdf 0.45 le font au chargement,
//! pdf-extract le fait dans `extract_text_from_mem`, et on le refait ici comme lui.
//!
//! ## Ce qu'on n'appelle jamais
//!
//! `pdf_inspector::extract_text_mem` : c'est l'extracteur **brut** de lopdf sous un autre nom, et
//! l'appeler ferait concourir lopdf deux fois en croyant mesurer pdf-inspector. Son texte est le
//! `markdown` de `process_pdf_mem_with_options`, celui que sa mise en page produit.

use pdf_inspector::PdfOptions;

/// Les candidats, par le nom que le parent passe en argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Candidate {
    /// `pdf-inspector` 1.25.2 : mise en page, puis Markdown.
    Inspector,
    /// `pdf-extract` 0.12.1, la référence de la comparaison appariée.
    Extract,
    /// `lopdf` 0.45 seul, `Document::extract_text` : l'extracteur brut, qui colle les mots.
    Lopdf,
}

/// Le document n'a pas pu être ouvert du tout. Sans message : celui d'une bibliothèque peut citer
/// un objet du document, et le protocole ne transporte que des comptes et le texte extrait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unreadable;

/// Ce qu'un candidat a tiré d'un document ouvert.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Extraction {
    pub(crate) text: String,
    pub(crate) pages: u32,
    pub(crate) pages_failed: u32,
}

impl Candidate {
    /// Les trois, dans l'ordre où le banc les présente.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 3] = [Self::Inspector, Self::Extract, Self::Lopdf];

    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "inspector" => Some(Self::Inspector),
            "extract" => Some(Self::Extract),
            "lopdf" => Some(Self::Lopdf),
            _ => None,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Inspector => "inspector",
            Self::Extract => "extract",
            Self::Lopdf => "lopdf",
        }
    }

    /// Extrait le texte de `bytes`.
    ///
    /// # Errors
    ///
    /// [`Unreadable`] si le document ne s'ouvre pas.
    pub(crate) fn extract(self, bytes: &[u8]) -> Result<Extraction, Unreadable> {
        match self {
            Self::Inspector => inspector(bytes),
            Self::Extract => extract(bytes),
            Self::Lopdf => lopdf(bytes),
        }
    }
}

/// pdf-inspector sur le document entier — un seul chargement, une seule mise en page —, et page par
/// page seulement s'il échoue : c'est ce que fera l'enfant livré, par tranches.
///
/// Le comptage de pages du repli passe par lopdf 0.45, la version que pdf-inspector lit : un
/// document que lopdf ne sait pas ouvrir n'a pas de pages à essayer une à une.
fn inspector(bytes: &[u8]) -> Result<Extraction, Unreadable> {
    if let Ok(result) = pdf_inspector::process_pdf_mem_with_options(bytes, PdfOptions::new()) {
        // `markdown` vaut `None` pour un document que la détection dit numérisé : aucun texte, et
        // ce n'est pas un échec.
        return Ok(Extraction {
            text: result.markdown.unwrap_or_default(),
            pages: result.page_count,
            pages_failed: 0,
        });
    }
    let pages = page_numbers(bytes)?;
    let mut out = Extraction {
        pages: count(pages.len()),
        ..Extraction::default()
    };
    for page in pages {
        match pdf_inspector::process_pdf_mem_with_options(bytes, PdfOptions::new().pages([page])) {
            Ok(result) => push_page(&mut out.text, result.markdown.as_deref().unwrap_or("")),
            Err(_) => out.pages_failed += 1,
        }
    }
    Ok(out)
}

/// Les numéros de page selon lopdf 0.45.
fn page_numbers(bytes: &[u8]) -> Result<Vec<u32>, Unreadable> {
    let document = lopdf::Document::load_mem(bytes).map_err(|_| Unreadable)?;
    Ok(document.get_pages().keys().copied().collect())
}

/// pdf-extract : un chargement, puis `output_doc_page` page après page, avec la sortie texte de la
/// bibliothèque. `extract_text_from_mem_by_pages` s'arrêterait à la première page en erreur.
fn extract(bytes: &[u8]) -> Result<Extraction, Unreadable> {
    let mut document = pdf_extract::Document::load_mem(bytes).map_err(|_| Unreadable)?;
    if document.is_encrypted() {
        document.decrypt("").map_err(|_| Unreadable)?;
    }
    let pages: Vec<u32> = document.get_pages().keys().copied().collect();
    let mut out = Extraction {
        pages: count(pages.len()),
        ..Extraction::default()
    };
    for page in pages {
        let mut text = String::new();
        let done = {
            let mut output = pdf_extract::PlainTextOutput::new(&mut text);
            pdf_extract::output_doc_page(&document, &mut output, page)
        };
        match done {
            Ok(()) => push_page(&mut out.text, &text),
            Err(_) => out.pages_failed += 1,
        }
    }
    Ok(out)
}

/// lopdf 0.45 : `Document::load_mem`, puis `extract_text(&[n])` page par page.
fn lopdf(bytes: &[u8]) -> Result<Extraction, Unreadable> {
    let document = lopdf::Document::load_mem(bytes).map_err(|_| Unreadable)?;
    let pages: Vec<u32> = document.get_pages().keys().copied().collect();
    let mut out = Extraction {
        pages: count(pages.len()),
        ..Extraction::default()
    };
    for page in pages {
        match document.extract_text(&[page]) {
            Ok(text) => push_page(&mut out.text, &text),
            Err(_) => out.pages_failed += 1,
        }
    }
    Ok(out)
}

/// Ajoute le texte d'une page, séparé du précédent par une fin de ligne : deux pages collées
/// fabriqueraient un mot qui n'existe dans aucune des deux.
fn push_page(text: &mut String, page: &str) {
    if page.is_empty() {
        return;
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(page);
}

/// Un nombre de pages, saturé : quatre milliards de pages ne tiennent pas dans 128 Mio.
fn count(pages: usize) -> u32 {
    u32::try_from(pages).unwrap_or(u32::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::fixtures;

    /// Les mots d'un texte, en minuscules : ce que le banc parent verra, à l'analyseur près.
    fn words(text: &str) -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect()
    }

    /// Le contrôle positif : les trois retrouvent la phrase, accents WinAnsi compris.
    #[test]
    fn every_candidate_reads_the_positive_fixture() {
        let pdf = fixtures::positive().unwrap();
        let expected = ["électricité", "facture", "1234", "56"];
        for candidate in Candidate::ALL {
            let extraction = candidate.extract(&pdf).unwrap();
            let found = words(&extraction.text);
            assert!(
                found
                    .windows(expected.len())
                    .any(|window| window == expected),
                "{} : {found:?}",
                candidate.name()
            );
            assert_eq!(extraction.pages, 1, "{}", candidate.name());
            assert_eq!(extraction.pages_failed, 0, "{}", candidate.name());
        }
    }

    /// Le contrôle de câblage : sans caractère espace dans le flux, lopdf brut colle les deux mots,
    /// pdf-inspector les sépare par leur position. Si pdf-inspector collait, le banc mesurerait
    /// lopdf deux fois — ce test doit alors tomber, et c'est son rôle.
    #[test]
    fn the_wiring_fixture_tells_the_raw_extractor_from_the_layout_one() {
        let pdf = fixtures::wiring().unwrap();
        let raw = Candidate::Lopdf.extract(&pdf).unwrap();
        assert_eq!(words(&raw.text), ["bonjourmonde"], "{:?}", raw.text);
        let laid_out = Candidate::Inspector.extract(&pdf).unwrap();
        assert_eq!(
            words(&laid_out.text),
            ["bonjour", "monde"],
            "{:?}",
            laid_out.text
        );
    }

    /// Entrée hostile : rien, du texte, du bruit — une erreur, jamais une panique. Appelé hors de
    /// `catch_unwind` : une panique fait tomber le test.
    #[test]
    fn nothing_or_noise_is_unreadable_and_never_panics() {
        let mut noise = Vec::with_capacity(4096);
        let mut state = 0x2545_f491_u32;
        for _ in 0..4096 {
            // Un générateur congruentiel : du bruit reproductible, sans dépendance.
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            noise.push(state.to_be_bytes()[0]);
        }
        let cases: [&[u8]; 3] = [b"", b"pas un pdf du tout, seulement du texte", &noise];
        for bytes in cases {
            for candidate in Candidate::ALL {
                assert_eq!(
                    candidate.extract(bytes),
                    Err(Unreadable),
                    "{} sur {} octets",
                    candidate.name(),
                    bytes.len()
                );
            }
        }
    }

    #[test]
    fn pages_are_separated_and_empty_pages_add_nothing() {
        let mut text = String::new();
        push_page(&mut text, "");
        assert_eq!(text, "");
        push_page(&mut text, "un");
        push_page(&mut text, "");
        push_page(&mut text, "deux");
        assert_eq!(text, "un\ndeux");
        // Une page qui finit déjà par une fin de ligne n'en reçoit pas une seconde.
        let mut text = String::from("un\n");
        push_page(&mut text, "deux");
        assert_eq!(text, "un\ndeux");
    }

    #[test]
    fn names_go_both_ways() {
        for candidate in Candidate::ALL {
            assert_eq!(Candidate::parse(candidate.name()), Some(candidate));
        }
        assert_eq!(Candidate::parse("pdfium"), None);
    }
}

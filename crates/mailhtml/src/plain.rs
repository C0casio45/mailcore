//! Conversion d'un corps `text/plain` en HTML, **citations comprises**.
//!
//! ## Pourquoi ce module existe alors que `mail-parser` sait déjà le faire
//!
//! `mail-parser::body_html` convertit les parties texte en HTML. Sa conversion remplace `\n` par
//! `<br/>` et échappe `<`. C'est tout — et ce « tout » a deux conséquences que le projet a
//! payées.
//!
//! **Une citation en texte brut n'en est plus une.** Un mail en texte brut cite en préfixant ses
//! lignes de `>`. Passées au `<br/>`, ces lignes deviennent du texte ordinaire : [`crate::blocks`]
//! n'y voit aucun [`crate::blocks::Kind::Quote`], donc la coquille les affiche comme le reste, et
//! la passe de vectorisation les avale — trouvé le 2026-09-21 par le test de `mailvec::text`, et
//! c'est la moitié du courrier réel qui est concernée.
//!
//! **`&` n'est pas échappé.** Un `&amp;` écrit en toutes lettres dans un mail en texte brut
//! ressort en `&` après un aller-retour par le rendu. C'est le symétrique du piège que
//! `Mailbox::render` avait relevé pour `<script>` — là, l'échappement sauvait le texte ; ici, son
//! absence le mange.
//!
//! ## Ce qu'il n'est pas
//!
//! Pas un détecteur de citation « intelligent ». Il ne reconnaît **que** le préfixe `>`, qui est
//! la convention de la RFC 3676 et ce que tous les clients produisent. Les en-têtes de citation
//! — « Le 12 mars, Untel a écrit : » — restent du texte : les reconnaître demanderait une liste
//! de tournures par langue, elle serait fausse pour quelqu'un, et se tromper ici efface du texte
//! légitime. Une citation non reconnue est un défaut mineur ; un paragraphe effacé à tort ne l'est
//! pas.
//!
//! Comme [`crate::blocks`] et [`crate::text`], **il ne peut pas échouer** : toute entrée produit
//! une sortie, éventuellement plus pauvre.

use std::fmt::Write as _;

/// Profondeur maximale de citation rendue.
///
/// La même que celle de [`crate::blocks`], et pour la même raison : au-delà, l'indentation ne
/// veut plus rien dire. C'est aussi une borne de sûreté — une ligne de dix mille `>` produirait
/// sans elle dix mille éléments imbriqués, ce qui est une entrée hostile bon marché.
const MAX_DEPTH: usize = 12;

/// Convertit un corps en texte brut vers du HTML.
///
/// Les lignes préfixées de `>` deviennent des `<blockquote>` imbriqués selon leur profondeur ;
/// les lignes consécutives de même profondeur sont réunies dans un `<p>`, séparées par `<br/>` ;
/// une ligne blanche ferme le paragraphe.
#[must_use]
pub fn to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4 + 16);
    let mut depth = 0usize;
    let mut in_paragraph = false;

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (found, content) = split_markers(line);

        if content.trim().is_empty() {
            // Une ligne blanche ferme le paragraphe mais **ne change pas la profondeur**. Une
            // citation contient des lignes blanches, et les traiter comme une sortie de citation
            // couperait le bloc en deux au premier paragraphe cité.
            close_paragraph(&mut out, &mut in_paragraph);
            continue;
        }

        if found != depth {
            close_paragraph(&mut out, &mut in_paragraph);
            while depth > found {
                out.push_str("</blockquote>");
                depth -= 1;
            }
            while depth < found {
                out.push_str("<blockquote>");
                depth += 1;
            }
        }

        if in_paragraph {
            out.push_str("<br/>");
        } else {
            out.push_str("<p>");
            in_paragraph = true;
        }
        escape(content, &mut out);
    }

    close_paragraph(&mut out, &mut in_paragraph);
    while depth > 0 {
        out.push_str("</blockquote>");
        depth -= 1;
    }
    out
}

/// Ferme le paragraphe courant, s'il y en a un.
fn close_paragraph(out: &mut String, in_paragraph: &mut bool) {
    if *in_paragraph {
        out.push_str("</p>");
        *in_paragraph = false;
    }
}

/// Sépare les marqueurs de citation du contenu, et rend la profondeur.
///
/// Les marqueurs peuvent être espacés — `> > texte` autant que `>> texte` —, les deux formes
/// existant dans le corpus selon le client qui a répondu.
fn split_markers(line: &str) -> (usize, &str) {
    let mut depth = 0usize;
    let mut rest = line.trim_start_matches(' ');
    while let Some(after) = rest.strip_prefix('>') {
        depth += 1;
        rest = after.trim_start_matches(' ');
    }
    // Au-delà du plafond, les marqueurs en trop ont déjà été retirés du contenu : la citation est
    // rendue moins profonde qu'elle n'est, ce qui est le bon échec — le texte reste, et il reste
    // dans une citation.
    (depth.min(MAX_DEPTH), rest)
}

/// Écrit du texte échappé.
///
/// `&` en premier, sinon les entités écrites par les remplacements suivants seraient elles-mêmes
/// échappées et ressortiraient en toutes lettres.
fn escape(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => {
                let _ = write!(out, "{ch}");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::blocks::{Kind, blocks};

    /// Ce que le module doit garantir, exprimé sur la sortie de `blocks` plutôt que sur le
    /// balisage : c'est `blocks` qui décide de ce qu'est une citation, et c'est lui l'appelant
    /// réel.
    fn kinds(text: &str) -> Vec<Kind> {
        blocks(&to_html(text)).into_iter().map(|b| b.kind).collect()
    }

    #[test]
    fn a_quoted_line_becomes_a_quote() {
        assert_eq!(
            kinds("Bien reçu.\n\n> Voici le devis."),
            vec![Kind::Paragraph, Kind::Quote(1)]
        );
    }

    /// Le contrôle négatif : sans le marqueur, la même ligne n'est pas une citation.
    #[test]
    fn the_same_line_without_the_marker_is_a_paragraph() {
        assert_eq!(kinds("Voici le devis."), vec![Kind::Paragraph]);
    }

    #[test]
    fn depth_is_counted_in_both_spellings() {
        assert_eq!(kinds(">> profond"), vec![Kind::Quote(2)]);
        assert_eq!(kinds("> > profond"), vec![Kind::Quote(2)]);
    }

    /// Une citation contient des lignes blanches. Si elles fermaient la citation, un fil réel
    /// serait découpé en une dizaine de blocs alternant cité et non cité.
    #[test]
    fn a_blank_line_inside_a_quote_does_not_close_it() {
        assert_eq!(
            kinds("> un\n>\n> deux"),
            vec![Kind::Quote(1), Kind::Quote(1)]
        );
    }

    #[test]
    fn coming_back_out_of_a_quote_works() {
        assert_eq!(
            kinds("> cité\nrépondu"),
            vec![Kind::Quote(1), Kind::Paragraph]
        );
    }

    #[test]
    fn an_absurd_depth_is_capped_and_stays_a_quote() {
        let line = ">".repeat(10_000) + " fond";
        let found = kinds(&line);
        assert_eq!(
            found,
            vec![Kind::Quote(u8::try_from(MAX_DEPTH).unwrap_or(12))]
        );
    }

    #[test]
    fn markup_written_in_plain_text_is_escaped_and_survives() {
        let html = to_html("<script>alert('x')</script> et R&amp;D");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(
            html.contains("R&amp;amp;D"),
            "le `&` n'a pas été échappé : {html}"
        );

        // Et au bout de la chaîne, le texte est rendu tel qu'il a été écrit.
        let text = blocks(&html)[0].text();
        assert!(text.contains("<script>"), "{text}");
        assert!(text.contains("R&amp;D"), "{text}");
    }

    /// Chaque ligne de texte donne un bloc, et une ligne blanche n'en donne aucun.
    ///
    /// Le `<p>`/`<br/>` produit ici ne survit pas jusqu'à [`blocks`], qui coupe sur les sauts de
    /// ligne : la distinction ne sert qu'aux consommateurs du HTML — la coquille Tauri et
    /// « ouvrir dans le navigateur » — où elle donne des paragraphes plutôt qu'un mur. Ce qui
    /// compte ici est qu'aucune ligne ne disparaisse et qu'aucune ligne blanche n'invente un
    /// bloc.
    #[test]
    fn every_line_gives_a_block_and_a_blank_line_gives_none() {
        assert_eq!(
            kinds("un\ndeux\n\ntrois"),
            vec![Kind::Paragraph, Kind::Paragraph, Kind::Paragraph]
        );
    }

    #[test]
    fn crlf_does_not_leave_a_carriage_return_at_the_end_of_lines() {
        let html = to_html("un\r\ndeux\r\n");
        assert!(!html.contains('\r'), "{html}");
    }

    #[test]
    fn an_empty_body_gives_an_empty_document() {
        assert!(to_html("").is_empty());
        assert!(to_html("\n\n\n").is_empty());
    }

    /// Une entrée hostile ne doit produire ni panique ni balisage déséquilibré.
    #[test]
    fn hostile_input_closes_everything_it_opens() {
        for case in ["> ".repeat(5_000), ">".repeat(100), "\u{0}\u{1}".to_owned()] {
            let html = to_html(&case);
            assert_eq!(
                html.matches("<blockquote>").count(),
                html.matches("</blockquote>").count(),
                "balisage déséquilibré pour {case:?}"
            );
            assert_eq!(html.matches("<p>").count(), html.matches("</p>").count());
        }
    }
}

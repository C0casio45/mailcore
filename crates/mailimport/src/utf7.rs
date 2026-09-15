//! Décodage de l'UTF-7 modifié des noms de dossiers IMAP (RFC 3501, §5.1.3).
//!
//! ## Trouvé en regardant l'écran, pas en lisant la spécification
//!
//! Un dossier du profil réel s'appelle `P&AOk-pite`. C'est `Pépite` : IMAP encode les noms de
//! boîtes en UTF-7 modifié, et Thunderbird nomme les fichiers du profil avec cet encodage tel
//! quel. `docs/PHASE-1.md` prévenait — « ne rien supposer sur l'encodage du nom de fichier » —
//! et on avait supposé.
//!
//! Un seul dossier sur 96 est touché, mais ce n'est pas un défaut d'affichage : le nom faux est
//! dans le store, donc dans l'index de recherche, donc dans tout ce qui en dérive.
//!
//! ## L'encodage
//!
//! - `&` ouvre une séquence décalée, `-` la referme.
//! - `&-` est un `&` littéral.
//! - Entre les deux : du base64 **modifié** — `,` au lieu de `/`, sans remplissage — décodant
//!   des unités UTF-16 gros-boutistes.
//!
//! ## Conservateur par construction
//!
//! Toute séquence qui ne décode pas proprement est **rendue telle quelle**. C'est ce qui rend
//! le décodage sûr à appliquer largement : un dossier local nommé « Trucs & Machins » contient
//! `& `, l'espace n'est pas dans l'alphabet base64, la séquence est donc invalide et le nom
//! ressort intact.
//!
//! L'appelant le réserve quand même aux comptes IMAP ([`crate::tree`]) : c'est là que
//! l'encodage a un sens, et ne pas toucher au reste vaut mieux que de compter sur la prudence
//! du décodeur.

use std::borrow::Cow;

/// Décode un nom de dossier en UTF-7 modifié.
///
/// Rend l'entrée inchangée — sans allouer — quand elle ne contient rien à décoder ou quand une
/// séquence est malformée.
#[must_use]
pub fn decode(name: &str) -> Cow<'_, str> {
    if !name.contains('&') {
        return Cow::Borrowed(name);
    }

    let bytes = name.as_bytes();
    let mut out = String::with_capacity(name.len());
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] != b'&' {
            // Recopier jusqu'au prochain `&`, en une fois plutôt que caractère par caractère.
            let start = index;
            while index < bytes.len() && bytes[index] != b'&' {
                index += 1;
            }
            // `name` est de l'UTF-8 valide et on n'a coupé que sur des `&` ASCII : la tranche
            // est donc sur une frontière de caractère.
            out.push_str(&name[start..index]);
            continue;
        }

        // Une séquence décalée. Trouver son `-` de clôture.
        let Some(offset) = bytes[index + 1..].iter().position(|byte| *byte == b'-') else {
            // Pas de clôture : ce n'est pas une séquence, c'est un `&` littéral suivi de texte.
            out.push_str(&name[index..]);
            break;
        };
        let payload = &name[index + 1..index + 1 + offset];
        let after = index + offset + 2;

        if payload.is_empty() {
            // `&-` : un `&` littéral.
            out.push('&');
            index = after;
            continue;
        }

        match decode_shifted(payload) {
            Some(decoded) => out.push_str(&decoded),
            // Séquence invalide : la rendre telle quelle, `&` et `-` compris. Un nom qu'on ne
            // sait pas décoder doit ressortir intact, pas amputé.
            None => out.push_str(&name[index..after]),
        }
        index = after;
    }

    Cow::Owned(out)
}

/// Décode le contenu d'une séquence décalée : base64 modifié vers UTF-16BE vers texte.
fn decode_shifted(payload: &str) -> Option<String> {
    let mut bits = 0u32;
    let mut width = 0u32;
    let mut units: Vec<u16> = Vec::with_capacity(payload.len() / 2 + 1);

    for byte in payload.bytes() {
        let value = base64_value(byte)?;
        bits = (bits << 6) | u32::from(value);
        width += 6;
        if width >= 16 {
            width -= 16;
            units.push(((bits >> width) & 0xFFFF) as u16);
        }
    }

    // Le remplissage restant doit être nul : des bits non nuls veulent dire une séquence
    // tronquée, donc un nom qu'on n'a pas le droit de deviner.
    if width >= 6 || (bits & ((1 << width) - 1)) != 0 {
        return None;
    }

    // `from_utf16` refuse les substituts orphelins, ce qui est exactement le comportement
    // voulu : un nom mal encodé ressort intact plutôt que criblé de caractères de remplacement.
    String::from_utf16(&units).ok()
}

/// La valeur d'un caractère du base64 **modifié** d'IMAP : `,` remplace `/`.
fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b',' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_case_found_on_the_real_profile() {
        assert_eq!(decode("P&AOk-pite"), "Pépite");
    }

    #[test]
    fn plain_names_are_returned_without_allocating() {
        // `Cow::Borrowed` dit que rien n'a été copié : c'est le cas de 95 dossiers sur 96.
        assert!(matches!(decode("INBOX"), Cow::Borrowed("INBOX")));
        assert!(matches!(decode("Tous les messages"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_literal_ampersand_survives() {
        assert_eq!(decode("&-"), "&");
        assert_eq!(decode("Trucs &- Machins"), "Trucs & Machins");
    }

    #[test]
    fn accented_names_round_trip_from_the_forms_thunderbird_writes() {
        // Formes relevées dans la spécification et chez d'autres clients.
        assert_eq!(decode("&AOk-"), "é");
        assert_eq!(decode("&AMk-"), "É");
        assert_eq!(decode("El&AOk-ments envoy&AOk-s"), "Eléments envoyés");
        assert_eq!(decode("&AOAA6QDo-"), "àéè");
    }

    #[test]
    fn a_comma_stands_in_for_a_slash() {
        // Le point de « base64 modifié ». `,` doit valoir 63.
        assert_eq!(base64_value(b','), Some(63));
        assert_eq!(base64_value(b'/'), None);
    }

    #[test]
    fn the_example_from_rfc_3501_decodes() {
        // Tiré de la spécification, §5.1.3 : `~peter/mail/&U,BTFw-/&ZeVnLIqe-`. Une valeur
        // attendue qui vient d'ailleurs que de notre propre code — sans quoi le test ne
        // vérifierait que la cohérence du décodeur avec lui-même.
        assert_eq!(decode("&U,BTFw-"), "台北");
        assert_eq!(decode("&ZeVnLIqe-"), "日本語");
        assert_eq!(
            decode("~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
            "~peter/mail/台北/日本語"
        );
    }

    #[test]
    fn a_surrogate_pair_decodes_to_one_character() {
        // U+1F4E7 (📧) s'encode en deux unités UTF-16 : la paire doit se recomposer.
        assert_eq!(decode("&2D3c5w-"), "📧");
    }

    // --- Entrée hostile : la règle du CLAUDE.md ---

    #[test]
    fn a_malformed_sequence_is_returned_untouched() {
        // Un nom qu'on ne sait pas décoder doit ressortir intact, pas amputé ni deviné.
        for hostile in [
            "Trucs & Machins", // espace : pas du base64
            "A&B",             // pas de clôture
            "&/AA-",           // `/` n'est pas dans l'alphabet modifié
            "&AOk",            // séquence non terminée
            "&AO-",            // remplissage non nul
            "&2D0-",           // substitut orphelin
            "&====-",          // `=` n'est pas dans l'alphabet
            "&&&-",
            "compte & co",
        ] {
            let decoded = decode(hostile);
            assert_eq!(decoded, hostile, "nom altéré : {hostile:?}");
        }
    }

    #[test]
    fn malformed_input_never_panics() {
        for broken in [
            "",
            "&",
            "-",
            "&-&-&-",
            "&&",
            "-&",
            "\u{feff}&AOk-",
            "é&AOk-é",
        ] {
            // Ce qui compte : ça termine et ça ne panique pas.
            let _ = decode(broken);
        }
    }

    #[test]
    fn a_pathological_name_terminates() {
        // Le décodeur n'avance que vers l'avant, donc il termine quelle que soit l'entrée.
        let _ = decode(&"&".repeat(100_000));
        let _ = decode(&"&AOk-".repeat(50_000));
        let _ = decode(&format!("&{}-", "A".repeat(100_000)));
    }

    #[test]
    fn the_decoded_name_never_contains_a_replacement_character() {
        // Un remplacement voudrait dire qu'on a rendu un nom à moitié faux en le présentant
        // comme décodé. On préfère rendre l'original.
        for input in ["&2D0-", "&AOk", "&/AA-"] {
            assert!(!decode(input).contains('\u{fffd}'), "{input:?}");
        }
    }
}

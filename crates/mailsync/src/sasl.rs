//! `XOAUTH2` : l'authentification par jeton, telle que Google et Microsoft l'attendent.
//!
//! ## Le format, et pourquoi il est étrange
//!
//! Ce n'est pas un mécanisme SASL standardisé. C'est une invention de Google, reprise par
//! Microsoft, et sa forme s'en ressent :
//!
//! ```text
//! base64("user=" <adresse> \x01 "auth=Bearer " <jeton> \x01 \x01)
//! ```
//!
//! Deux `\x01` à la fin, dont un qui termine la liste vide des paramètres suivants. L'écrire
//! de mémoire donne un `AUTHENTICATE` refusé sans explication, ce qui est la raison d'être de
//! ce module et de ses tests.
//!
//! ## Pourquoi ce n'est pas dans `client`
//!
//! `client` parle IMAP ; ceci est du SASL, et le même format servira à SMTP quand l'envoi
//! arrivera en phase 3. Le séparer maintenant évite de l'extraire plus tard d'un module qui
//! aura grossi.
//!
//! ## Ce qui n'est **pas** ici
//!
//! Le jeton. Ce module met en forme ce qu'on lui donne ; obtenir un jeton d'accès à partir
//! d'un jeton de rafraîchissement est le travail de `mailauth`, et le garder est celui du
//! trousseau du système.

use base64::Engine as _;

/// L'encodeur base64 standard, avec bourrage.
///
/// Avec bourrage, et ce n'est pas un détail : Google refuse une chaîne non bourrée. C'est
/// exactement le genre d'erreur qu'un base64 écrit à la main produit.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// La réponse initiale d'un `AUTHENTICATE XOAUTH2`.
///
/// ## Le jeton n'est jamais journalisé, et ne peut pas l'être par accident
///
/// La fonction rend la chaîne base64, pas une structure qui porterait le jeton en clair avec
/// un `Debug` dérivé. Un `tracing::debug!(?credentials)` quelque part suffirait à le mettre
/// dans un journal, et `docs/PRIVACY.md` §8 l'interdit.
///
/// Le base64 n'est pas un chiffrement — il est trivialement réversible — mais il n'apparaît
/// nulle part dans un journal : la commande `AUTHENTICATE` n'est pas tracée, contrairement au
/// nom des autres commandes.
#[must_use]
pub fn xoauth2(username: &str, access_token: &str) -> String {
    let raw = format!("user={username}\u{1}auth=Bearer {access_token}\u{1}\u{1}");
    B64.encode(raw)
}

/// Décode une réponse de continuation du serveur, pour le journal.
///
/// ## Pourquoi il faut la lire
///
/// Quand `XOAUTH2` échoue, Google **ne répond pas `NO` tout de suite**. Il envoie une
/// continuation `+ <base64>` dont le contenu est un objet JSON qui dit pourquoi :
///
/// ```text
/// {"status":"400","schemes":"Bearer","scope":"https://mail.google.com/"}
/// ```
///
/// Le client doit alors envoyer une **ligne vide** pour que le serveur conclue par un `NO`.
/// Un client qui ne le fait pas attend une réponse qui ne viendra jamais.
///
/// C'est la seule information utile quand un jeton est refusé — `status` distingue un jeton
/// expiré d'un périmètre insuffisant, et les deux se corrigent autrement. Rendue en clair
/// parce qu'elle ne contient aucun secret : c'est un message d'erreur du serveur.
///
/// Une chaîne indécodable est rendue telle quelle plutôt que jetée : un serveur qui met autre
/// chose que du base64 là dit quand même quelque chose.
#[must_use]
pub fn decode_challenge(encoded: &str) -> String {
    B64.decode(encoded.trim())
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_else(|| encoded.trim().to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_initial_response_matches_the_documented_shape() {
        // L'exemple de la documentation de Google, décodé pour être lisible :
        //   user=marie@exemple.fr^Aauth=Bearer jeton^A^A
        let encoded = xoauth2("marie@exemple.fr", "jeton");
        let decoded = String::from_utf8(B64.decode(&encoded).unwrap()).unwrap();
        assert_eq!(
            decoded,
            "user=marie@exemple.fr\u{1}auth=Bearer jeton\u{1}\u{1}"
        );
    }

    #[test]
    fn there_are_exactly_two_separators_at_the_end() {
        // **L'erreur classique.** Un seul `\x01` final donne un `AUTHENTICATE` refusé sans
        // explication, et rien dans le message du serveur ne dit que c'est ça.
        let encoded = xoauth2("a@b.c", "t");
        let decoded = B64.decode(&encoded).unwrap();
        assert_eq!(
            decoded.iter().filter(|it| **it == 1).count(),
            3,
            "trois séparateurs en tout : un après l'adresse, deux à la fin"
        );
        assert!(decoded.ends_with(&[1, 1]));
    }

    #[test]
    fn the_encoding_is_padded() {
        // Google refuse une chaîne non bourrée. La longueur d'un base64 bourré est un
        // multiple de quatre.
        for (user, token) in [
            ("a@b.c", "t"),
            ("marie@exemple.fr", "ya29.a0AfB_by"),
            ("x", ""),
        ] {
            let encoded = xoauth2(user, token);
            assert!(
                encoded.len().is_multiple_of(4),
                "base64 non bourré pour {user} : {encoded}"
            );
        }
    }

    #[test]
    fn an_empty_token_still_produces_a_well_formed_string() {
        // Un jeton vide est une erreur d'appelant, pas une raison de paniquer. Le serveur
        // refusera, et son message dira pourquoi.
        let decoded = String::from_utf8(B64.decode(xoauth2("a@b.c", "")).unwrap()).unwrap();
        assert_eq!(decoded, "user=a@b.c\u{1}auth=Bearer \u{1}\u{1}");
    }

    #[test]
    fn a_username_with_unicode_survives() {
        // Une adresse internationalisée. Le base64 porte des octets UTF-8, pas des
        // caractères : rien à convertir, mais il faut que ça ressorte identique.
        let encoded = xoauth2("marié@exemple.fr", "t");
        let decoded = String::from_utf8(B64.decode(&encoded).unwrap()).unwrap();
        assert!(decoded.starts_with("user=marié@exemple.fr\u{1}"));
    }

    #[test]
    fn the_json_challenge_is_decoded() {
        // Ce que Google envoie quand un jeton est refusé. C'est la seule information utile,
        // et elle arrive dans une continuation, pas dans le `NO`.
        let json = r#"{"status":"400","schemes":"Bearer","scope":"https://mail.google.com/"}"#;
        let encoded = B64.encode(json);
        assert_eq!(decode_challenge(&encoded), json);
    }

    #[test]
    fn a_challenge_with_surrounding_space_is_still_decoded() {
        // La ligne arrive sous la forme `+ <base64>` : l'appelant coupe le `+`, l'espace peut
        // rester.
        let encoded = format!("  {}  ", B64.encode("bonjour"));
        assert_eq!(decode_challenge(&encoded), "bonjour");
    }

    #[test]
    fn an_undecodable_challenge_comes_back_as_is() {
        // Un serveur qui met autre chose que du base64 dit quand même quelque chose. Le jeter
        // remplacerait un diagnostic médiocre par pas de diagnostic.
        assert_eq!(decode_challenge("pas du base64 !!"), "pas du base64 !!");
        assert_eq!(decode_challenge(""), "");
    }

    #[test]
    fn a_challenge_never_panics_whatever_it_contains() {
        // L'entrée vient du réseau.
        for candidate in ["=", "==", "a", "ab", "abc", "\u{1}\u{1}", "////", "++++"] {
            let _ = decode_challenge(candidate);
        }
    }
}

//! Le découpage en mots que les bancs partagent.
//!
//! ## Pourquoi un module pour dix lignes
//!
//! Parce qu'il y avait deux découpages, et qu'ils ne disaient pas la même chose. `recall` retirait
//! les mots-outils avant de chercher un mot commun entre une requête et sa cible ; `attach`, qui
//! compte les messages auxquels une pièce jointe apporte un mot nouveau, ne les retirait pas. Un
//! « vous » présent seulement dans une invitation y comptait donc comme un mot nouveau, et le
//! chiffre de l'étape 6 quater — 1 363 messages — en était gonflé d'une quantité inconnue.
//!
//! C'est la leçon du carnet d'adresses : deux chemins vers la même donnée partagent leur **code**,
//! pas seulement leur intention. Ici la donnée est « les mots qui portent du sens », et les deux
//! bancs qui en dépendent — le critère 1 et le recensement des pièces jointes — en ont maintenant
//! une seule définition.

/// Les mots qu'on ne compte pas comme partagés.
///
/// **Sans cette liste, il n'y a pas de sous-ensemble « aucun mot commun » :** « le », « de »,
/// « que » sont dans toutes les requêtes et dans tous les messages, donc tout partagerait un mot
/// avec tout, et le banc mesurerait le rappel global sous un autre nom.
///
/// Elle est volontairement courte et ne contient que des mots-outils français et anglais. Un mot
/// porteur de sens n'y entre pas, même fréquent : « compte » est dans des dizaines de messages,
/// et c'est précisément ce qui rend une requête qui le contient facile pour un moteur de mots.
pub const STOPWORDS: &[&str] = &[
    "a", "à", "au", "aux", "avec", "ce", "ces", "cet", "cette", "dans", "de", "des", "du", "elle",
    "en", "est", "et", "eu", "il", "je", "j", "l", "la", "le", "les", "leur", "lui", "ma", "mais",
    "me", "mes", "mon", "n", "ne", "nos", "notre", "nous", "on", "ou", "où", "par", "pas", "pour",
    "qu", "que", "qui", "sa", "se", "ses", "son", "sur", "ta", "te", "tes", "toi", "ton", "tu",
    "un", "une", "vos", "votre", "vous", "y", "s", "d", "c", "m", "t", "quand", "plus", "moins",
    "fait", "faire", "été", "être", "avoir", "the", "of", "to", "and", "a", "in", "is", "it",
    "for", "on", "your", "you", "my", "i", "that", "this", "with", "was", "are", "be", "have",
];

/// Découpe en mots porteurs : minuscules Unicode, sans ponctuation, sans mots-outils.
///
/// `to_lowercase` et non `to_ascii_lowercase` : sur un corpus français, replier « Éloïse » en
/// ASCII ne replie rien du tout — c'est la leçon du carnet d'adresses, le 2026-09-11.
///
/// La longueur se compte en **caractères**, pas en octets. La version de `recall` comptait les
/// octets, donc une lettre accentuée seule — « é », deux octets — passait le filtre des mots d'un
/// caractère que « a » ne passait pas. Les deux versions divergeaient là aussi.
pub fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() > 1)
        .map(str::to_lowercase)
        .filter(|word| !STOPWORDS.contains(&word.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_words_are_not_words() {
        let found: Vec<String> = words("Vous avez une facture pour le plombier").collect();
        assert_eq!(found, ["avez", "facture", "plombier"]);
    }

    /// Le défaut qui a gonflé l'étape 6 quater : sans ce filtre, « vous » serait un mot nouveau.
    #[test]
    fn a_function_word_found_only_in_an_attachment_is_not_news() {
        assert_eq!(words("vous the your").count(), 0);
    }

    #[test]
    fn folding_is_unicode_and_length_is_in_characters() {
        let found: Vec<String> = words("Éloïse é ÉTÉ").collect();
        assert_eq!(
            found,
            ["éloïse"],
            "« é » seul fait un caractère, « été » est un mot-outil"
        );
    }
}

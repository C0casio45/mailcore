//! Aplatissement d'un corps HTML en texte, pour l'indexation tantivy.
//!
//! Ce n'est pas du rendu : c'est de l'extraction pour la recherche. Le résultat n'est jamais
//! réaffiché à l'utilisateur, il alimente l'index plein texte. La barre est donc « ce qu'un
//! humain lirait à l'écran », pas « du HTML fidèle ».
//!
//! ## Ce qui disparaît, et pourquoi
//!
//! - **Le contenu de `<script>` et `<style>`.** Du code, pas du texte. Sans ça, chercher
//!   « function » remonterait la moitié des newsletters.
//! - **Tous les attributs, donc toutes les URL.** Un lien de désabonnement est présent dans
//!   presque chaque mail commercial ; indexer les URL ferait remonter n'importe quel mot
//!   courant qui traîne dans un chemin.
//! - **Les commentaires HTML**, y compris les conditionnels d'Outlook, qui ne sont pas du
//!   texte pour l'utilisateur.
//!
//! ## Robustesse
//!
//! L'entrée est du HTML arbitraire écrit par un inconnu, souvent cassé — voir
//! `docs/PRIVACY.md`. Cette fonction ne peut pas échouer : elle rend toujours du texte, même
//! sur une balise non fermée, un `<` isolé ou du balisage imbriqué de travers. Un mail
//! illisible ne doit pas interrompre une indexation de 73 000 messages.
//!
//! Ce n'est pas une barrière de sécurité. Retirer `<script>` améliore la qualité de
//! l'index, rien de plus : ce qui empêche un script de s'exécuter est la CSP, doublée de
//! [`crate::sanitize`]. Voir `docs/PRIVACY.md`.
//!
//! Écrit à la main plutôt que confié à un analyseur HTML complet : un analyseur construirait
//! un arbre DOM par message, alors qu'on ne veut qu'un passage linéaire sur les octets.

/// Aplatit du HTML en texte, en ajoutant à `out`.
///
/// `out` n'est pas vidé : l'appelant peut concaténer plusieurs parties MIME dans le même
/// tampon, et le réutiliser d'un message au suivant.
pub fn html_to_text(html: &str, out: &mut String) {
    let bytes = html.as_bytes();
    let mut index = 0usize;
    // Vrai si le dernier caractère écrit était une espace : évite d'en accumuler.
    let mut pending_space = !out.is_empty() && !out.ends_with(char::is_whitespace);

    while index < bytes.len() {
        match bytes[index] {
            b'<' => {
                let Some(tag) = Tag::at(html, index) else {
                    // Un `<` qui n'ouvre pas de balise est du texte. Fréquent dans les mails
                    // où quelqu'un écrit « 3 < 5 » sans échapper.
                    push_char(out, '<', &mut pending_space);
                    index += 1;
                    continue;
                };

                index = match tag.name {
                    // Le contenu de ces éléments n'est pas du texte : on saute jusqu'à leur
                    // fermeture, ou jusqu'à la fin si elle manque.
                    "script" | "style" | "head" | "title" if !tag.closing => {
                        skip_element(html, tag.end, tag.name)
                    }
                    _ => tag.end,
                };
                pending_space = true;
            }
            b'&' => {
                let (decoded, consumed) = entity(html, index);
                match decoded {
                    Some(character) => push_char(out, character, &mut pending_space),
                    // Une entité inconnue reste telle quelle : « AT&T » est du texte.
                    None => push_char(out, '&', &mut pending_space),
                }
                index += consumed;
            }
            byte if byte.is_ascii_whitespace() => {
                pending_space = true;
                index += 1;
            }
            _ => {
                // Avancer d'un caractère complet, pas d'un octet : couper un caractère UTF-8
                // en deux paniquerait au `push_str`.
                let width = char_width(bytes[index]);
                let end = (index + width).min(bytes.len());
                // Une frontière de caractère invalide fait sauter l'octet plutôt que paniquer.
                if let Some(slice) = html.get(index..end) {
                    if pending_space && !out.is_empty() {
                        out.push(' ');
                    }
                    pending_space = false;
                    out.push_str(slice);
                }
                index = end;
            }
        }
    }
}

/// Aplatit du HTML et rend le texte. Commodité pour les tests et les petits usages.
#[must_use]
pub fn to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    html_to_text(html, &mut out);
    out
}

/// Une balise reconnue dans le flux.
struct Tag<'a> {
    /// Le nom en minuscules, vide pour un commentaire ou une balise anonyme.
    name: &'a str,
    /// Vrai pour `</...>`.
    closing: bool,
    /// L'offset juste après le `>`.
    end: usize,
}

impl<'a> Tag<'a> {
    /// Reconnaît la balise qui commence au `<` en `start`.
    ///
    /// Rend `None` si ce `<` n'ouvre pas quelque chose qui ressemble à une balise — auquel
    /// cas c'est du texte.
    fn at(html: &'a str, start: usize) -> Option<Self> {
        let bytes = html.as_bytes();
        let after = start + 1;
        let next = *bytes.get(after)?;

        // Commentaire, doctype, section CDATA : rien de tout ça n'est du texte.
        if next == b'!' {
            let end = if html[after..].starts_with("!--") {
                find_after(html, after + 3, "-->")
            } else {
                find_after(html, after, ">")
            };
            return Some(Self {
                name: "",
                closing: false,
                end,
            });
        }

        let closing = next == b'/';
        let name_start = if closing { after + 1 } else { after };
        let first = *bytes.get(name_start)?;
        if !first.is_ascii_alphabetic() {
            if closing {
                // `</` suivi d'autre chose qu'une lettre : la spécification HTML appelle ça
                // un « bogus comment » et le jette. On fait pareil — c'est du balisage cassé,
                // pas du texte, et l'indexer polluerait la recherche.
                return Some(Self {
                    name: "",
                    closing: true,
                    end: find_after(html, name_start, ">"),
                });
            }
            // `<3`, `< 5`, `<=` : du texte, pas une balise.
            return None;
        }

        let name_end = bytes[name_start..]
            .iter()
            .position(|b| !b.is_ascii_alphanumeric())
            .map_or(bytes.len(), |offset| name_start + offset);

        Some(Self {
            name: NAMES
                .iter()
                .find(|known| html[name_start..name_end].eq_ignore_ascii_case(known))
                .copied()
                .unwrap_or(""),
            closing,
            end: find_after(html, name_end, ">"),
        })
    }
}

/// Les seuls noms de balises dont le contenu nous intéresse.
const NAMES: &[&str] = &["script", "style", "head", "title"];

/// Saute jusqu'après la balise fermante de `name`, ou jusqu'à la fin.
///
/// Une balise non fermée fait tout avaler jusqu'à la fin du document. C'est le bon
/// comportement : un `<script>` non fermé ne redevient pas du texte lisible plus loin.
fn skip_element(html: &str, from: usize, name: &str) -> usize {
    let mut index = from;
    let bytes = html.as_bytes();

    while index < bytes.len() {
        let Some(relative) = html[index..].find('<') else {
            return bytes.len();
        };
        let open = index + relative;
        // Comparaison sur les octets, jamais sur une tranche de `&str` : `&rest[2..N]`
        // panique si N tombe au milieu d'un caractère UTF-8, ce qui arrive dès qu'un
        // accent suit un `<`. Trouvé sur le corpus réel, pas en test synthétique.
        let rest = &bytes[open..];
        if rest.len() > name.len() + 2
            && rest[1] == b'/'
            && rest[2..2 + name.len()].eq_ignore_ascii_case(name.as_bytes())
        {
            return find_after(html, open, ">");
        }
        index = open + 1;
    }
    bytes.len()
}

/// L'offset juste après la prochaine occurrence de `needle`, ou la fin.
fn find_after(html: &str, from: usize, needle: &str) -> usize {
    html.get(from..)
        .and_then(|rest| rest.find(needle))
        .map_or(html.len(), |offset| from + offset + needle.len())
}

/// Décode une entité HTML commençant en `start`. Rend le caractère et les octets consommés.
///
/// `pub(crate)` : [`crate::blocks`] a le même besoin, et une deuxième table d.entités serait
/// une deuxième table à corriger.
pub(crate) fn entity(html: &str, start: usize) -> (Option<char>, usize) {
    // Une entité HTML dépasse rarement dix caractères ; au-delà, c'est une esperluette
    // isolée suivie de texte, et chercher plus loin ferait avaler des mots entiers.
    const MAX: usize = 12;

    let limit = (start + MAX).min(html.len());
    let Some(window) = html.get(start..limit) else {
        return (None, 1);
    };
    let Some(semicolon) = window.find(';') else {
        return (None, 1);
    };
    let body = &window[1..semicolon];
    let consumed = semicolon + 1;

    if let Some(digits) = body.strip_prefix('#') {
        let code = digits
            .strip_prefix(['x', 'X'])
            .map_or_else(
                || digits.parse::<u32>().ok(),
                |hex| u32::from_str_radix(hex, 16).ok(),
            )
            .and_then(char::from_u32);
        // Un point de code invalide ne consomme qu'un octet : `&#;` et `&#999999999;` sont
        // du texte, et les avaler ferait disparaître les caractères qui suivent.
        return match code {
            Some(character) => (Some(character), consumed),
            None => (None, 1),
        };
    }

    let decoded = match body {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        // Espace insécable : traité comme une espace, pas comme un caractère. Sinon les mots
        // qu'il sépare seraient collés dans l'index.
        "nbsp" => Some(' '),
        _ => None,
    };
    (decoded, if decoded.is_some() { consumed } else { 1 })
}

/// Ajoute un caractère en honorant l'espace en attente.
fn push_char(out: &mut String, character: char, pending_space: &mut bool) {
    if character.is_whitespace() {
        *pending_space = true;
        return;
    }
    if *pending_space && !out.is_empty() {
        out.push(' ');
    }
    *pending_space = false;
    out.push(character);
}

/// La largeur en octets d'un caractère UTF-8 d'après son premier octet.
const fn char_width(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        // Octet de continuation isolé : entrée invalide, on avance d'un.
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------- cas nominaux

    #[test]
    fn strips_tags_and_keeps_words_apart() {
        assert_eq!(to_text("<p>Bonjour</p><p>Monde</p>"), "Bonjour Monde");
        assert_eq!(to_text("<b>gras</b>uite"), "gras uite");
    }

    #[test]
    fn collapses_runs_of_whitespace() {
        assert_eq!(to_text("un   \n\t deux"), "un deux");
        assert_eq!(to_text("<div>  un  </div>  <div>  deux  </div>"), "un deux");
    }

    #[test]
    fn drops_attributes_and_therefore_urls() {
        // Le lien de désabonnement ne doit pas polluer l'index.
        let html = r#"<a href="https://exemple.fr/desabonnement?token=abc">ici</a>"#;
        let text = to_text(html);
        assert_eq!(text, "ici");
        assert!(!text.contains("exemple"));
        assert!(!text.contains("token"));
    }

    #[test]
    fn drops_script_and_style_contents() {
        let html = "<style>body{color:red}</style>texte<script>alert(1)</script>fin";
        let text = to_text(html);
        assert_eq!(text, "texte fin");
        assert!(!text.contains("color"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn drops_the_head_and_the_title() {
        let html = "<html><head><title>pas du corps</title></head><body>le corps</body></html>";
        assert_eq!(to_text(html), "le corps");
    }

    #[test]
    fn drops_comments_including_outlook_conditionals() {
        let html = "avant<!--[if mso]><table><tr><![endif]-->apres";
        assert_eq!(to_text(html), "avant apres");
    }

    #[test]
    fn drops_the_doctype() {
        assert_eq!(to_text("<!DOCTYPE html><p>texte</p>"), "texte");
    }

    #[test]
    fn decodes_common_entities() {
        assert_eq!(to_text("Dupont &amp; fils"), "Dupont & fils");
        assert_eq!(to_text("3 &lt; 5 &gt; 1"), "3 < 5 > 1");
        assert_eq!(to_text("&quot;cite&quot;"), "\"cite\"");
        assert_eq!(to_text("l&apos;heure"), "l'heure");
    }

    #[test]
    fn decodes_numeric_entities() {
        assert_eq!(to_text("&#233;t&#233;"), "été");
        assert_eq!(to_text("&#xe9;t&#xE9;"), "été");
    }

    #[test]
    fn a_non_breaking_space_separates_words() {
        // Le coller donnerait « unmot » dans l'index, introuvable.
        assert_eq!(to_text("un&nbsp;mot"), "un mot");
    }

    #[test]
    fn preserves_accented_and_multibyte_text() {
        assert_eq!(to_text("<p>éàü — 日本語 🙂</p>"), "éàü — 日本語 🙂");
    }

    #[test]
    fn appends_to_an_existing_buffer_with_a_separator() {
        let mut out = String::from("partie une");
        html_to_text("<p>partie deux</p>", &mut out);
        assert_eq!(out, "partie une partie deux");
    }

    // ---------------------------------------------------------------- entrée hostile

    #[test]
    fn a_lone_angle_bracket_is_text() {
        assert_eq!(to_text("3 < 5"), "3 < 5");
        assert_eq!(to_text("a <3 b"), "a <3 b");
        assert_eq!(to_text("x <= y"), "x <= y");
    }

    #[test]
    fn an_unclosed_tag_does_not_hang_or_panic() {
        assert_eq!(to_text("<p>texte"), "texte");
        assert_eq!(to_text("texte<p"), "texte");
        assert_eq!(to_text("<"), "<");
        assert_eq!(to_text("<<<<"), "<<<<");
    }

    #[test]
    fn an_unclosed_script_swallows_the_rest() {
        // Le bon comportement : du JavaScript non terminé ne redevient pas du texte.
        assert_eq!(to_text("avant<script>alert(1)"), "avant");
    }

    #[test]
    fn a_nested_script_tag_behaves_like_a_tokeniser() {
        // `<scr<script>ipt>` est le contournement classique des filtres qui retirent la
        // sous-chaîne `<script>`. Ici il n'y a rien à contourner : `<scr<script>` est **une**
        // balise nommée `scr` avec un attribut absurde, exactement comme un navigateur la
        // découperait, et ce qui reste — `ipt>alert(1)` — est du texte.
        //
        // C'est le bon résultat pour de l'extraction : ce texte part dans un index plein
        // texte, il n'est jamais exécuté ni réaffiché. La barrière contre le script exécuté
        // est la CSP, doublée de `sanitize` — voir docs/PRIVACY.md. Ce module n'en est pas
        // une, et ce test existe pour que personne ne croie le contraire.
        let text = to_text("<scr<script>ipt>alert(1)</script>fin");
        assert!(text.contains("alert(1)"), "{text:?}");
        assert!(!text.contains('<'), "une balise a survécu : {text:?}");
    }

    #[test]
    fn a_well_formed_script_never_leaks() {
        // Le cas qui compte vraiment pour la qualité de l'index.
        for html in [
            "<script>alert(1)</script>texte",
            "<SCRIPT TYPE=\"text/javascript\">alert(1)</SCRIPT>texte",
            "<script src=\"x.js\"></script>texte",
        ] {
            let text = to_text(html);
            assert!(!text.contains("alert"), "sur {html:?} : {text:?}");
        }
    }

    #[test]
    fn a_multibyte_character_after_an_angle_bracket_does_not_panic() {
        // Régression, trouvée par l'indexation du corpus réel et pas par un test écrit à
        // l'avance : découper une `&str` à `rest[2..2+len]` panique dès qu'un accent suit
        // un `<`. La comparaison se fait maintenant sur les octets.
        assert_eq!(to_text("<style>x</à>y</style>fin"), "fin");
        assert_eq!(to_text("<script>a<é>b</script>fin"), "fin");
        assert_eq!(to_text("<style>c</日本>d</style>fin"), "fin");
        // Et le cas limite : un `<` suivi d'un multi-octets, sans fermeture du tout.
        assert_eq!(to_text("<script>x</é"), "");
    }

    #[test]
    fn accented_closing_tags_in_ordinary_text_are_fine() {
        assert_eq!(to_text("<p>Où ça ?</p>"), "Où ça ?");
        assert_eq!(to_text("a</é>b"), "a b");
    }
    #[test]
    fn an_unterminated_comment_does_not_panic() {
        assert_eq!(to_text("avant<!-- jamais ferme"), "avant");
    }

    #[test]
    fn a_malformed_entity_stays_as_text() {
        assert_eq!(to_text("AT&T"), "AT&T");
        assert_eq!(to_text("&pasuneentite;"), "&pasuneentite;");
        assert_eq!(to_text("&"), "&");
        assert_eq!(to_text("&#;"), "&#;");
        assert_eq!(to_text("&#999999999;"), "&#999999999;");
    }

    #[test]
    fn a_very_long_pseudo_entity_is_not_swallowed() {
        // Sans plafond, un `&` isolé avalerait la phrase entière jusqu'au prochain `;`.
        let input = "&une phrase entiere qui contient un point virgule ; et la suite";
        let text = to_text(input);
        assert!(text.contains("phrase entiere"), "{text:?}");
    }

    #[test]
    fn an_empty_input_yields_an_empty_string() {
        assert_eq!(to_text(""), "");
        assert_eq!(to_text("   \n\t  "), "");
        assert_eq!(to_text("<p></p>"), "");
    }

    #[test]
    fn deeply_nested_markup_does_not_recurse() {
        // Pas de récursion dans l'implémentation : 50 000 niveaux ne débordent pas la pile.
        let html = "<div>".repeat(50_000) + "fond" + &"</div>".repeat(50_000);
        assert_eq!(to_text(&html), "fond");
    }

    #[test]
    fn a_tag_name_touching_the_end_of_input_does_not_panic() {
        assert_eq!(to_text("<script"), "");
        assert_eq!(to_text("</"), "</");
        assert_eq!(to_text("<!"), "");
        assert_eq!(to_text("<!--"), "");
    }
}

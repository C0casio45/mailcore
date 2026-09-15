//! Assainissement du HTML d'un message, en liste blanche, via `ammonia`.
//!
//! La **deuxième** barrière de `docs/PRIVACY.md`. La première est la CSP, appliquée par le
//! moteur de rendu ([`crate::csp`]) ; celle-ci est appliquée par nous. Les deux sont
//! redondantes à dessein : une CSP mal formée ne doit pas être un point de défaillance
//! unique, et un bug ici ne doit pas être un point de défaillance unique non plus.
//!
//! **Liste blanche, jamais liste noire.** Une balise inconnue est refusée par défaut, pas
//! autorisée par oubli. C'est ce qui fait qu'une balise inventée demain — ou une qu'on a
//! simplement oubliée — ne passe pas.
//!
//! ## Le déblocage des images a imposé une correction
//!
//! `docs/PRIVACY.md` demande deux choses qui semblent se contredire : « aucune URL distante
//! dans la sortie » (§5, et le test du critère 8) et « un bandeau *Afficher les images*,
//! valable pour ce message uniquement » (§2). Un assainisseur qui supprime définitivement
//! les `src` distants rend le deuxième impossible.
//!
//! La sortie retenue : [`clean`] prend une [`Policy`]. Par défaut elle bloque, et le
//! déblocage est un **nouvel appel** avec `allow_remote_images`, pas une modification du
//! document déjà rendu. Conséquences directes, toutes souhaitables :
//!
//! - la sortie bloquée ne contient aucune URL distante *en position chargeable*, donc le
//!   test du critère 8 dit exactement ce qu'il prétend dire ;
//! - le déblocage ne persiste rien, puisqu'il n'y a rien à persister — c'est un rendu, pas
//!   un état ;
//! - le nombre d'images bloquées est un sous-produit du rendu, donc il ne peut pas
//!   diverger de ce qui a réellement été retiré.
//!
//! Le pendant côté CSP — une politique qui autorise `img-src https:` quand l'utilisateur a
//! débloqué — appartient à l'UI et arrive avec elle.
//!
//! ## Les `<style>` sont supprimés, et c'est un compromis assumé
//!
//! `ammonia` n'assainit **pas** le contenu d'une feuille de style : autoriser la balise
//! laisserait passer `@import url(https://…)` intact. Écrire nous-mêmes un demi-analyseur
//! CSS produirait exactement la barrière approximative contre laquelle `docs/PRIVACY.md`
//! met en garde. Les blocs `<style>` partent donc entièrement, contenu compris.
//!
//! Ce qui reste : l'attribut `style`, que `ammonia` sait analyser pour de vrai, filtré par
//! **liste blanche de propriétés**. Aucune propriété porteuse d'`url()` n'y figure —
//! `background`, `background-image`, `list-style-image`, `content`, `cursor`, `filter` sont
//! absentes — donc aucune URL ne peut entrer par le CSS, sans avoir à inspecter les valeurs.
//! Couleurs, polices, espacements, bordures et alignements de tableau survivent, ce qui
//! couvre l'essentiel de la mise en forme d'un mail.
//!
//! Le coût est réel sur les mails très mis en page. Un véritable assainisseur CSS est
//! l'amélioration de phase 2 ; d'ici là, la version conservatrice est la bonne pour une
//! deuxième ceinture.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use ammonia::{Builder, UrlRelative};

/// Ce que l'appelant autorise pour ce rendu.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// Laisser les `src` distants sur les images.
    ///
    /// Faux par défaut, et le défaut est le seul mode utilisé tant que l'utilisateur n'a
    /// pas cliqué. Vrai correspond à « Afficher les images » sur **ce** message : le mode
    /// ne se mémorise pas, il se redemande.
    pub allow_remote_images: bool,
}

/// Le résultat d'un assainissement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleaned {
    /// Le HTML à rendre.
    pub html: String,
    /// Nombre de sources d'images distantes retirées.
    ///
    /// Zéro quand la politique les autorise : ce compteur dit ce qui a été **retiré**, pas
    /// ce qui existe. Pour savoir ce que le message contient, voir [`crate::trackers`].
    pub blocked_images: usize,
}

/// Assainit le corps HTML d'un message.
///
/// Ne rend jamais d'erreur : un HTML illisible, tronqué ou hostile produit une sortie plus
/// pauvre, jamais un échec. Un message qu'on n'arrive pas à assainir se lit en texte, il ne
/// fait pas tomber le lecteur.
#[must_use]
pub fn clean(html: &str, policy: Policy) -> Cleaned {
    // `ammonia` ne rend pas le nombre d'attributs retirés : on le compte dans le filtre.
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&blocked);
    let allow_remote = policy.allow_remote_images;

    let mut builder = Builder::default();
    configure(&mut builder);
    builder.attribute_filter(move |element, attribute, value| {
        filter_attribute(element, attribute, value, allow_remote, &counter)
    });

    Cleaned {
        html: builder.clean(html).to_string(),
        blocked_images: blocked.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// Applique la liste blanche au constructeur.
fn configure(builder: &mut Builder<'static>) {
    builder
        .tags(tags().clone())
        // Le contenu part avec la balise, pas seulement la balise. Sans ça, le corps d'un
        // `<script>` se retrouverait affiché en texte au milieu du message — inoffensif mais
        // absurde — et celui d'un `<style>` ferait fuiter des URL dans le texte indexé.
        .clean_content_tags(stripped_with_content().clone())
        .generic_attributes(generic_attributes().clone())
        .tag_attributes(tag_attributes().clone())
        .url_schemes(url_schemes().clone())
        .filter_style_properties(style_properties().clone())
        // Un mail n'a pas d'URL de base. Une URL relative se résoudrait contre l'origine de
        // l'`<iframe>` — et comme la CSP autorise `img-src 'self'`, un `<img src="/pixel">`
        // atteindrait *notre* démon. Refuser le relatif ferme ce chemin.
        .url_relative(UrlRelative::Deny)
        // Le `sandbox` empêche déjà l'accès à l'ouvreur ; le `rel` le rappelle au cas où le
        // lien serait ouvert hors de l'`<iframe>`.
        .link_rel(Some("noopener noreferrer nofollow"))
        .strip_comments(true);
}

/// Décide du sort d'un attribut autorisé.
///
/// Appelé après la liste blanche, sur ce qui a déjà survécu. Trois choses s'y jouent que la
/// liste blanche ne sait pas exprimer : les `data:` ne sont acceptables que sur une image,
/// les `src` distants dépendent de la politique, et un `javascript:` ne doit pas dépendre
/// d'un seul mécanisme pour être écarté.
fn filter_attribute<'v>(
    element: &str,
    attribute: &str,
    value: &'v str,
    allow_remote: bool,
    blocked: &std::sync::atomic::AtomicUsize,
) -> Option<Cow<'v, str>> {
    let scheme = scheme_of(value);

    // Ceinture : `ammonia` a déjà écarté ces schémas par `url_schemes`. On ne s'en remet pas
    // à un seul mécanisme pour la chose la plus dangereuse du lot.
    if matches!(
        scheme.as_deref(),
        Some("javascript" | "vbscript" | "livescript" | "file" | "about")
    ) {
        return None;
    }

    if attribute == "src" && element == "img" {
        return match scheme.as_deref() {
            // Une image embarquée : elle ne provoque aucune requête, et c'est le seul usage
            // de `data:` que `docs/PRIVACY.md` accepte.
            Some("data") if is_image_data_url(value) => Some(Cow::Borrowed(value)),
            Some("data") => None,
            // `cid:` désigne une pièce jointe du message lui-même. Elle n'est pas encore
            // servie — la phase 1 liste les pièces jointes sans les ouvrir — donc la source
            // part, et l'image se rendra vide plutôt que cassée à moitié.
            Some("cid") => None,
            Some(_) if allow_remote => Some(Cow::Borrowed(value)),
            Some(_) => {
                blocked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                None
            }
            None => None,
        };
    }

    // Hors image, `data:` est refusé : c'est un document arbitraire déguisé en lien.
    if scheme.as_deref() == Some("data") {
        return None;
    }

    Some(Cow::Borrowed(value))
}

/// Le schéma d'une URL, en minuscules, sans son `:`.
///
/// Écrit à la main plutôt qu'avec un analyseur d'URL : il faut reconnaître ce qu'un **moteur
/// de rendu** considérerait comme un schéma, y compris les formes tordues qu'une
/// bibliothèque stricte refuserait d'analyser et rendrait donc « sans schéma ». Les espaces
/// et caractères de contrôle intercalés sont retirés, parce que `java\tscript:` est du
/// JavaScript pour un navigateur.
pub(crate) fn scheme_of(value: &str) -> Option<String> {
    let mut scheme = String::new();
    for character in value.chars() {
        match character {
            ':' => {
                return if scheme.is_empty() {
                    None
                } else {
                    Some(scheme.to_lowercase())
                };
            }
            // Tabulations, sauts de ligne et caractères de contrôle sont ignorés par les
            // navigateurs au milieu d'un schéma. Les ignorer aussi.
            c if c.is_whitespace() || c.is_control() => {}
            c if c.is_alphanumeric() || matches!(c, '+' | '-' | '.') => scheme.push(c),
            // Tout autre caractère termine ce qui aurait pu être un schéma : ce qui précède
            // n'en était pas un.
            _ => return None,
        }
    }
    None
}

/// Vrai si une URL `data:` porte une image.
fn is_image_data_url(value: &str) -> bool {
    let rest = value
        .split_once(':')
        .map_or("", |(_, rest)| rest)
        .trim_start();
    let media_type = rest.split([';', ',']).next().unwrap_or_default();
    // `image/svg+xml` est exclu : un SVG est un document, il peut porter des scripts et des
    // références externes. Une image qui peut faire des requêtes n'est pas une image.
    media_type.to_lowercase().starts_with("image/")
        && !media_type.to_lowercase().starts_with("image/svg")
}

/// Les balises autorisées.
fn tags() -> &'static HashSet<&'static str> {
    static TAGS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    TAGS.get_or_init(|| {
        [
            // Structure et texte.
            "a",
            "abbr",
            "address",
            "b",
            "big",
            "blockquote",
            "br",
            "caption",
            "center",
            "cite",
            "code",
            "col",
            "colgroup",
            "dd",
            "del",
            "dfn",
            "div",
            "dl",
            "dt",
            "em",
            "figcaption",
            "figure",
            "font",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "hr",
            "i",
            "img",
            "ins",
            "kbd",
            "li",
            "mark",
            "ol",
            "p",
            "pre",
            "q",
            "s",
            "samp",
            "small",
            "span",
            "strike",
            "strong",
            "sub",
            "sup",
            "table",
            "tbody",
            "td",
            "tfoot",
            "th",
            "thead",
            "time",
            "tr",
            "tt",
            "u",
            "ul",
            "var",
            "wbr",
        ]
        .into_iter()
        .collect()
    })
}

/// Les balises dont le contenu part avec elles.
///
/// `svg` et `math` y figurent : ce sont des espaces de noms étrangers, historiquement la
/// source des contournements par re-analyse (*mXSS*), et un `<svg>` peut porter des
/// références externes. Les laisser en liste noire de contenu plutôt qu'en simple omission
/// évite que leur intérieur remonte en texte.
fn stripped_with_content() -> &'static HashSet<&'static str> {
    static STRIPPED: OnceLock<HashSet<&'static str>> = OnceLock::new();
    STRIPPED.get_or_init(|| {
        [
            "script",
            "style",
            "title",
            "head",
            "noscript",
            "template",
            "svg",
            "math",
            "object",
            "embed",
            "iframe",
            "frame",
            "frameset",
            "applet",
            "form",
            "input",
            "button",
            "select",
            "option",
            "textarea",
            "base",
            "link",
            "meta",
            "canvas",
            "audio",
            "video",
            "source",
            "track",
            "portal",
            "xmp",
            "plaintext",
            "listing",
        ]
        .into_iter()
        .collect()
    })
}

/// Les attributs acceptés sur n'importe quelle balise autorisée.
fn generic_attributes() -> &'static HashSet<&'static str> {
    static GENERIC: OnceLock<HashSet<&'static str>> = OnceLock::new();
    GENERIC.get_or_init(|| {
        ["title", "dir", "lang", "align", "style"]
            .into_iter()
            .collect()
    })
}

/// Les attributs acceptés balise par balise.
///
/// Ni `id`, ni `class`, ni `name` : ils ne servent à rien sans CSS externe ni script, et un
/// `id` recopié dans le document de l'UI serait une collision avec les nôtres.
fn tag_attributes() -> &'static HashMap<&'static str, HashSet<&'static str>> {
    static ATTRIBUTES: OnceLock<HashMap<&'static str, HashSet<&'static str>>> = OnceLock::new();
    ATTRIBUTES.get_or_init(|| {
        let cell: HashSet<&str> = [
            "colspan", "rowspan", "align", "valign", "width", "height", "bgcolor", "nowrap",
        ]
        .into_iter()
        .collect();
        let table: HashSet<&str> = [
            "align",
            "width",
            "border",
            "cellpadding",
            "cellspacing",
            "bgcolor",
        ]
        .into_iter()
        .collect();

        HashMap::from([
            ("a", ["href"].into_iter().collect()),
            (
                "img",
                ["src", "alt", "width", "height"].into_iter().collect(),
            ),
            ("td", cell.clone()),
            ("th", cell),
            ("table", table),
            ("tr", ["align", "valign", "bgcolor"].into_iter().collect()),
            ("col", ["span", "width", "align"].into_iter().collect()),
            ("colgroup", ["span", "width", "align"].into_iter().collect()),
            ("font", ["color", "face", "size"].into_iter().collect()),
            ("ol", ["start", "type"].into_iter().collect()),
            ("time", ["datetime"].into_iter().collect()),
        ])
    })
}

/// Les schémas d'URL acceptés.
///
/// `http` et `https` restent : un lien n'est pas chargé, il est **navigué au clic**, et
/// `docs/PRIVACY.md` §6 demande que sa cible réelle soit visible avant. Ce qui déciderait de
/// charger quelque chose — un `src` d'image — est traité à part dans [`filter_attribute`].
fn url_schemes() -> &'static HashSet<&'static str> {
    static SCHEMES: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SCHEMES.get_or_init(|| {
        ["http", "https", "mailto", "tel", "data"]
            .into_iter()
            .collect()
    })
}

/// Les propriétés CSS acceptées dans un attribut `style`.
///
/// **Aucune ne peut porter une `url()`.** `background`, `background-image`,
/// `list-style-image`, `border-image`, `content`, `cursor`, `filter`, `src` et `mask` sont
/// absentes, ce qui écarte les URL du CSS par le seul nom de la propriété — sans avoir à en
/// inspecter les valeurs, donc sans possibilité de se faire contourner par un encodage.
///
/// `position`, `top`, `left`, `right`, `bottom`, `z-index` et `transform` sont absentes
/// aussi : elles permettent de superposer du contenu pour faire lire autre chose que ce qui
/// est affiché.
fn style_properties() -> &'static HashSet<&'static str> {
    static PROPERTIES: OnceLock<HashSet<&'static str>> = OnceLock::new();
    PROPERTIES.get_or_init(|| {
        [
            "color",
            "background-color",
            "font",
            "font-family",
            "font-size",
            "font-style",
            "font-weight",
            "font-variant",
            "line-height",
            "letter-spacing",
            "word-spacing",
            "text-align",
            "text-decoration",
            "text-indent",
            "text-transform",
            "vertical-align",
            "white-space",
            "word-break",
            "overflow-wrap",
            "direction",
            "margin",
            "margin-top",
            "margin-right",
            "margin-bottom",
            "margin-left",
            "padding",
            "padding-top",
            "padding-right",
            "padding-bottom",
            "padding-left",
            "border",
            "border-top",
            "border-right",
            "border-bottom",
            "border-left",
            "border-color",
            "border-style",
            "border-width",
            "border-radius",
            "border-collapse",
            "border-spacing",
            "width",
            "height",
            "min-width",
            "max-width",
            "min-height",
            "max-height",
            "display",
            "float",
            "clear",
            "list-style-type",
            "table-layout",
            "caption-side",
            "empty-cells",
            "opacity",
        ]
        .into_iter()
        .collect()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Assainit avec la politique par défaut — celle qui s'applique tant que l'utilisateur
    /// n'a rien cliqué, donc celle qui compte.
    fn blocked(html: &str) -> String {
        clean(html, Policy::default()).html
    }

    // --- Cas propres : ce qui doit survivre ---

    #[test]
    fn ordinary_formatting_survives() {
        let out = blocked(
            "<p>Bonjour <strong>Marie</strong>, voici la <em>facture</em>.</p>\
             <table><tr><td>Total</td><td>42 €</td></tr></table>",
        );
        assert!(out.contains("<strong>Marie</strong>"));
        assert!(out.contains("<em>facture</em>"));
        assert!(out.contains("<td>42 €</td>"));
    }

    #[test]
    fn a_link_keeps_its_target_and_gains_a_rel() {
        // La cible doit rester visible : `docs/PRIVACY.md` §6 demande qu'on puisse la lire
        // avant de cliquer, ce qui suppose qu'elle soit encore là.
        let out = blocked(r#"<a href="https://exemple.fr/facture">la facture</a>"#);
        assert!(out.contains("https://exemple.fr/facture"));
        assert!(out.contains("noopener"));
    }

    #[test]
    fn inline_styles_keep_the_formatting_that_cannot_fetch_anything() {
        let out = blocked(
            r#"<p style="color: #333; font-size: 14px; background-image: url(https://pisteur.fr/p.gif)">x</p>"#,
        );
        assert!(out.contains("color"));
        assert!(!out.contains("pisteur.fr"), "sortie : {out}");
        assert!(!out.contains("background-image"), "sortie : {out}");
    }

    #[test]
    fn an_embedded_image_is_kept_because_it_fetches_nothing() {
        let out = blocked(r#"<img src="data:image/png;base64,iVBORw0KGgo=" alt="logo">"#);
        assert!(out.contains("data:image/png"), "sortie : {out}");
    }

    // --- Ce qui doit disparaître ---

    #[test]
    fn scripts_go_with_their_content() {
        let out = blocked("<p>avant</p><script>alert(document.cookie)</script><p>après</p>");
        assert!(!out.contains("script"));
        assert!(
            !out.contains("alert"),
            "le corps du script est resté : {out}"
        );
        assert!(out.contains("avant") && out.contains("après"));
    }

    #[test]
    fn the_forbidden_container_tags_go_with_their_content() {
        for tag in [
            "script", "object", "iframe", "form", "style", "svg", "math", "button", "textarea",
            "video", "audio", "canvas", "applet", "noscript", "select",
        ] {
            let out = blocked(&format!("<{tag}>contenu</{tag}>"));
            assert!(
                !out.contains(&format!("<{tag}")),
                "balise {tag} conservée : {out}"
            );
            assert!(
                !out.contains("contenu"),
                "contenu de {tag} conservé : {out}"
            );
        }
    }

    #[test]
    fn the_forbidden_void_tags_are_removed_and_have_no_content_to_hide_in() {
        // `embed`, `input`, `link`, `meta`, `base`, `source`, `track` et `frame` sont des
        // éléments vides : l'analyseur HTML ne leur donne aucun contenu, donc le texte qui
        // suit un `<embed>` n'est pas *dedans*, il est à côté. Rien ne peut s'y cacher, et
        // c'est la balise seule qu'il faut voir partir.
        for tag in [
            "embed", "input", "link", "meta", "base", "source", "track", "frame",
        ] {
            let out = blocked(&format!(
                r#"<p>avant</p><{tag} src="https://pisteur.fr/x">"#
            ));
            assert!(
                !out.contains(&format!("<{tag}")),
                "balise {tag} conservée : {out}"
            );
            assert!(
                !out.contains("pisteur.fr"),
                "URL de {tag} conservée : {out}"
            );
            assert!(out.contains("avant"));
        }
    }

    #[test]
    fn every_event_handler_attribute_is_removed() {
        for handler in [
            "onclick",
            "onload",
            "onerror",
            "onmouseover",
            "onfocus",
            "onanimationstart",
            "ontoggle",
        ] {
            let out = blocked(&format!(r#"<p {handler}="alert(1)">x</p>"#));
            assert!(!out.contains(handler), "{handler} conservé : {out}");
            assert!(!out.contains("alert"), "charge conservée : {out}");
        }
    }

    #[test]
    fn a_style_block_never_survives_because_its_content_is_not_sanitised() {
        let out = blocked(
            "<style>@import url('https://pisteur.fr/vol.css'); \
             body { background: url(https://pisteur.fr/p.gif) }</style><p>corps</p>",
        );
        assert!(!out.contains("pisteur.fr"), "sortie : {out}");
        assert!(!out.contains("@import"), "sortie : {out}");
        assert!(out.contains("corps"));
    }

    #[test]
    fn remote_image_sources_are_removed_and_counted() {
        let outcome = clean(
            r#"<img src="https://pisteur.fr/pixel.gif" width="1" height="1">
               <img src="http://autre.fr/logo.png">"#,
            Policy::default(),
        );
        assert_eq!(outcome.blocked_images, 2);
        assert!(!outcome.html.contains("pisteur.fr"));
        assert!(!outcome.html.contains("autre.fr"));
        // La balise reste : c'est ce qui permet à l'UI de dire « 2 images bloquées » à
        // l'endroit où elles étaient.
        assert!(outcome.html.contains("<img"));
    }

    #[test]
    fn unblocking_is_a_new_render_and_restores_the_sources() {
        let html = r#"<img src="https://exemple.fr/photo.jpg">"#;
        let allowed = clean(
            html,
            Policy {
                allow_remote_images: true,
            },
        );
        assert!(allowed.html.contains("https://exemple.fr/photo.jpg"));
        assert_eq!(allowed.blocked_images, 0);

        // Et le défaut n'a pas bougé : rien n'est mémorisé entre deux appels.
        assert_eq!(clean(html, Policy::default()).blocked_images, 1);
    }

    #[test]
    fn relative_urls_are_denied_so_nothing_can_reach_our_own_origin() {
        // La CSP autorise `img-src 'self'` : un chemin relatif atteindrait le démon.
        let out = blocked(r#"<img src="/pixel.gif"><a href="../secret">x</a>"#);
        assert!(!out.contains("pixel.gif"), "sortie : {out}");
        assert!(!out.contains("secret"), "sortie : {out}");
    }

    // --- Entrée hostile : la règle du CLAUDE.md ---

    #[test]
    fn javascript_urls_are_refused_however_they_are_spelled() {
        for url in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "  javascript:alert(1)",
            "java\tscript:alert(1)",
            "java\nscript:alert(1)",
            "java\rscript:alert(1)",
            "java\0script:alert(1)",
            "vbscript:msgbox(1)",
            "livescript:x",
        ] {
            let out = blocked(&format!(r#"<a href="{url}">clic</a>"#));
            assert!(
                !out.to_lowercase().contains("script:") && !out.contains("alert"),
                "URL acceptée : {url:?} → {out}"
            );
        }
    }

    #[test]
    fn a_data_url_is_only_acceptable_as_an_image() {
        // Un document arbitraire déguisé en lien.
        let out = blocked(r#"<a href="data:text/html,<script>alert(1)</script>">clic</a>"#);
        assert!(!out.contains("data:text/html"), "sortie : {out}");

        // Un SVG est un document, pas une image : il peut porter des scripts et des
        // références externes.
        let out = blocked(r#"<img src="data:image/svg+xml,<svg onload=alert(1)>">"#);
        assert!(!out.contains("svg"), "sortie : {out}");
    }

    #[test]
    fn nested_tag_smuggling_does_not_reconstruct_a_script() {
        // Le classique : un assainisseur qui retire « <script> » par remplacement de chaîne
        // laisse « <script> » derrière lui. Une vraie analyse ne s'y laisse pas prendre.
        for hostile in [
            "<scr<script>ipt>alert(1)</script>",
            "<scr<!---->ipt>alert(1)",
            "<<script>script>alert(1)</script>",
            "<img src=x onerror=alert(1)>",
            "<svg><script>alert(1)</script></svg>",
        ] {
            let out = blocked(hostile);
            assert!(
                !out.contains("<script") && !out.contains("onerror"),
                "reconstruit : {hostile:?} → {out}"
            );
        }
    }

    #[test]
    fn malformed_input_produces_output_rather_than_a_panic() {
        for broken in [
            "",
            "<",
            "<p",
            "<p>pas fermé",
            "</p></div></html>",
            "<p class=sans-guillemets>x</p>",
            "<p title=\"guillemet non fermé>x",
            "<a href=>x</a>",
            "<!-- commentaire non fermé",
            "<![CDATA[<script>alert(1)</script>]]>",
            "&lt;script&gt;alert(1)&lt;/script&gt;",
            "&amp;lt;script&amp;gt;",
            "<p>\u{0}\u{1}\u{feff}texte</p>",
            // Ce qu'un octet UTF-8 invalide devient en amont : `mail-parser` décode en
            // remplaçant, donc c'est cette forme-là qui nous arrive, pas la séquence brute.
            "<p>\u{fffd}\u{fffd}texte</p>",
            "<a href=\"https://x.fr/\u{fffd}\">clic</a>",
        ] {
            let out = clean(broken, Policy::default());
            assert!(
                !out.html.contains("<script"),
                "script reconstruit depuis {broken:?} → {}",
                out.html
            );
        }
    }

    #[test]
    fn deep_nesting_does_not_blow_the_stack() {
        // Un message hostile peut être profond de dix mille niveaux ; l'assainisseur doit
        // rendre quelque chose, pas faire tomber le démon.
        let deep = "<div>".repeat(5_000) + "texte" + &"</div>".repeat(5_000);
        let out = clean(&deep, Policy::default());
        assert!(out.html.contains("texte"));
    }

    #[test]
    fn a_huge_document_is_handled_without_special_casing() {
        let big = "<p>ligne</p>".repeat(50_000);
        let out = clean(&big, Policy::default());
        assert!(out.html.len() > 100_000);
    }

    // --- Le détail des schémas, testé à part ---

    #[test]
    fn scheme_detection_matches_what_a_browser_would_do() {
        assert_eq!(scheme_of("https://x").as_deref(), Some("https"));
        assert_eq!(scheme_of("JAVASCRIPT:x").as_deref(), Some("javascript"));
        assert_eq!(scheme_of("java\tscript:x").as_deref(), Some("javascript"));
        assert_eq!(scheme_of("/chemin/relatif").as_deref(), None);
        assert_eq!(scheme_of("sans-schema").as_deref(), None);
        assert_eq!(scheme_of(":vide").as_deref(), None);
        // Un `?` avant le `:` veut dire qu'on était déjà dans une requête, pas un schéma.
        assert_eq!(scheme_of("page?x=1:2").as_deref(), None);
    }

    #[test]
    fn only_real_image_media_types_pass_as_data_urls() {
        assert!(is_image_data_url("data:image/png;base64,AAAA"));
        assert!(is_image_data_url("data:image/gif,AAAA"));
        assert!(!is_image_data_url("data:image/svg+xml,<svg/>"));
        assert!(!is_image_data_url("data:text/html,<b>x</b>"));
        assert!(!is_image_data_url("data:,rien"));
    }

    #[test]
    fn no_allowed_style_property_can_carry_a_url() {
        // Le verrou de la règle : la liste blanche des propriétés est ce qui écarte les URL
        // du CSS. Si quelqu'un y ajoute `background`, ce test tombe.
        for porteuse in [
            "background",
            "background-image",
            "list-style-image",
            "list-style",
            "border-image",
            "border-image-source",
            "content",
            "cursor",
            "filter",
            "mask",
            "mask-image",
            "src",
            "behavior",
            "-moz-binding",
        ] {
            assert!(
                !style_properties().contains(porteuse),
                "propriété porteuse d'url() autorisée : {porteuse}"
            );
        }
    }

    #[test]
    fn overlay_properties_are_not_allowed_either() {
        // Superposer du contenu permet de faire lire autre chose que ce qui est affiché.
        for overlay in [
            "position",
            "top",
            "left",
            "right",
            "bottom",
            "z-index",
            "transform",
        ] {
            assert!(
                !style_properties().contains(overlay),
                "propriété de superposition autorisée : {overlay}"
            );
        }
    }

    #[test]
    fn the_tag_whitelist_and_the_content_stripping_list_do_not_overlap() {
        // Une balise à la fois autorisée et vidée de son contenu serait une contradiction
        // qui se résoudrait silencieusement dans un sens ou dans l'autre.
        for tag in tags() {
            assert!(
                !stripped_with_content().contains(tag),
                "{tag} est à la fois autorisée et supprimée avec son contenu"
            );
        }
    }
}

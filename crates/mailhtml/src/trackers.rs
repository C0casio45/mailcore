//! Détection et comptage des traceurs d'un message.
//!
//! On ne se contente pas de bloquer silencieusement : montrer qui tente de pister est une
//! fonctionnalité, pas un journal de débogage (`docs/PRIVACY.md`, §3). Le bandeau de l'UI
//! — « Contenu distant bloqué — 3 traceurs détectés » — se remplit ici.
//!
//! ## Ce module n'est pas une barrière, et c'est ce qui autorise son approximation
//!
//! Les barrières sont la CSP ([`crate::csp`]) et l'assainisseur ([`crate::sanitize`]). Elles
//! doivent être exactes, et elles s'appuient sur un vrai analyseur HTML. Ici, un faux négatif
//! est un **compteur trop bas**, pas une fuite : la ressource reste bloquée par les deux
//! barrières, qu'on l'ait comptée ou non.
//!
//! C'est ce qui rend acceptable un lecteur de balises léger plutôt qu'un arbre DOM complet.
//! Le compromis est explicite : on gagne de pouvoir lire ensemble le `src`, le `width` et le
//! `height` d'une même image — ce qu'un filtre d'attributs, qui les voit un par un, ne
//! permet pas — et on perd sur les formes tordues, qui ne coûtent qu'un chiffre.
//!
//! Ce module ne doit **jamais** devenir le fondement d'une décision de sécurité. Le jour où
//! quelque chose dépend de sa sortie pour décider de charger ou non, il faudra le réécrire
//! sur l'analyseur.
//!
//! ## Ce qui n'est pas conservé
//!
//! Les hôtes, oui. Les URL complètes, **non**. Une URL de traceur porte précisément
//! l'identifiant corrélé au destinataire ; la recopier dans un rapport qui finira dans un
//! journal ou dans une interface reviendrait à conserver l'information qu'on dénonce.
//! L'hôte suffit à dire qui piste.

use std::collections::HashSet;

/// Ce qui rend une ressource suspecte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Image de 1×1 ou de dimensions déclarées nulles. Le pixel espion classique.
    Pixel,
    /// Hôte figurant dans la liste embarquée de traceurs connus.
    KnownDomain,
    /// URL portant ce qui ressemble à un identifiant corrélé au destinataire.
    CorrelatedId,
}

/// Un traceur relevé.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tracker {
    /// Pourquoi il est signalé.
    pub kind: Kind,
    /// L'hôte, en minuscules. Jamais l'URL complète — voir le module.
    pub host: String,
}

/// Ce qu'un message contient comme contenu distant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Les traceurs relevés, sans doublon.
    pub trackers: Vec<Tracker>,
    /// Nombre de ressources distantes référencées, traceurs compris.
    ///
    /// Distinct du nombre de traceurs : une image distante ordinaire — le logo d'une
    /// facture — n'est pas un traceur, mais c'est bien une requête qui aurait eu lieu.
    pub remote_resources: usize,
}

impl Report {
    /// Vrai si le message ne référence rien de distant.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.remote_resources == 0 && self.trackers.is_empty()
    }
}

/// Relève les ressources distantes et les traceurs d'un corps HTML.
///
/// À appeler sur le HTML **brut**, avant assainissement : après, les URL distantes ont déjà
/// été retirées, et il ne resterait rien à compter.
#[must_use]
pub fn scan(html: &str) -> Report {
    let mut report = Report::default();
    let mut seen: HashSet<Tracker> = HashSet::new();

    for element in elements(html) {
        // Les attributs porteurs d'une ressource *chargée*. `href` n'y est pas : un lien est
        // navigué au clic, pas récupéré, donc il ne fuite rien tant qu'on ne clique pas.
        for attribute in ["src", "background", "poster", "srcset", "data-src"] {
            let Some(value) = element.attribute(attribute) else {
                continue;
            };
            for url in split_srcset(value, attribute) {
                classify(&url, &element, &mut report, &mut seen);
            }
        }

        // Le CSS embarqué, attribut comme bloc : `url()` et `@import` y chargent aussi.
        if let Some(style) = element.attribute("style") {
            for url in css_urls(style) {
                classify(&url, &element, &mut report, &mut seen);
            }
        }
    }

    // Les blocs `<style>`, dont le contenu n'est pas un attribut.
    for block in style_blocks(html) {
        for url in css_urls(block) {
            classify(&url, &Element::anonymous(), &mut report, &mut seen);
        }
    }

    report.trackers = seen.into_iter().collect();
    // Un ordre stable : un compteur qui change d'ordre à chaque appel rendrait tout test
    // instable et ferait clignoter un bandeau.
    report.trackers.sort_by(|a, b| {
        a.host
            .cmp(&b.host)
            .then_with(|| format!("{:?}", a.kind).cmp(&format!("{:?}", b.kind)))
    });
    report
}

/// Classe une URL et met le rapport à jour.
fn classify(url: &str, element: &Element<'_>, report: &mut Report, seen: &mut HashSet<Tracker>) {
    let Some(host) = remote_host(url) else {
        // `data:`, `cid:`, relatif : rien ne part sur le réseau.
        return;
    };
    report.remote_resources += 1;

    let mut flag = |kind: Kind| {
        seen.insert(Tracker {
            kind,
            host: host.clone(),
        });
    };

    if element.is_pixel() {
        flag(Kind::Pixel);
    }
    if is_known_tracker(&host) {
        flag(Kind::KnownDomain);
    }
    if carries_correlated_id(url) {
        flag(Kind::CorrelatedId);
    }
}

/// L'hôte d'une URL, si elle désigne quelque chose de distant.
///
/// Rend `None` pour tout ce qui ne provoque pas de requête réseau : `data:`, `cid:`, et les
/// URL relatives — l'assainisseur les refuse de toute façon.
fn remote_host(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url.strip_prefix("//").map_or_else(
        || {
            let (scheme, rest) = url.split_once("://")?;
            matches!(scheme.to_lowercase().as_str(), "http" | "https").then_some(rest)
        },
        // Une URL sans schéma, qui hérite de celui de la page. Elle charge bien.
        Some,
    )?;

    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        // Les identifiants avant l'hôte : `https://leurre@vrai-hote.fr/`.
        .rsplit('@')
        .next()
        .unwrap_or_default()
        // Le port.
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_lowercase();

    (!host.is_empty() && host.contains('.')).then_some(host)
}

/// Vrai si l'hôte est un traceur connu.
///
/// Liste embarquée, mise à jour **hors ligne** : aller la chercher sur le réseau serait une
/// fuite pour prévenir une fuite (`docs/PRIVACY.md`, §3). Elle est volontairement courte et
/// sûre — un faux positif accuserait à tort, et le compteur perdrait sa crédibilité.
///
/// La comparaison porte sur le domaine **et ses sous-domaines** : les traceurs servent
/// presque toujours depuis un sous-domaine dédié par client.
fn is_known_tracker(host: &str) -> bool {
    const KNOWN: &[&str] = &[
        // Suivi d'ouverture d'emailing.
        "mailchimp.com",
        "list-manage.com",
        "sendgrid.net",
        "sparkpostmail.com",
        "mailgun.org",
        "sendinblue.com",
        "brevo.com",
        "mailjet.com",
        "constantcontact.com",
        "rs6.net",
        "hubspot.com",
        "hs-sites.com",
        "exacttarget.com",
        "salesforce-communities.com",
        "klaviyo.com",
        "klaviyomail.com",
        "braze.com",
        "iterable.com",
        "customer.io",
        "intercom-mail.com",
        "mixmax.com",
        "yesware.com",
        "streak.com",
        "mailtrack.io",
        "bananatag.com",
        "getnotify.com",
        "spymail.com",
        // Analytique généraliste, souvent embarquée dans un mail.
        "google-analytics.com",
        "googletagmanager.com",
        "doubleclick.net",
        "scorecardresearch.com",
        "omtrdc.net",
        "2o7.net",
        "adobedtm.com",
        "branch.io",
        "appsflyer.com",
        "adjust.com",
    ];

    KNOWN
        .iter()
        .any(|known| host == *known || host.ends_with(&format!(".{known}")))
}

/// Vrai si l'URL porte ce qui ressemble à un identifiant corrélé au destinataire.
///
/// Heuristique, et assumée comme telle. Deux signaux :
///
/// - une adresse électronique, en clair ou encodée, dans l'URL ;
/// - un paramètre dont le nom désigne le destinataire, ou un segment opaque assez long pour
///   être un identifiant unique plutôt qu'un nom de fichier.
///
/// Le seuil de 24 caractères écarte les noms de fichiers hachés courts et les identifiants de
/// campagne partagés par tous les destinataires — qui ne corrèlent rien.
fn carries_correlated_id(url: &str) -> bool {
    let lower = url.to_lowercase();

    if lower.contains("%40") || lower.split('?').nth(1).is_some_and(|q| q.contains('@')) {
        return true;
    }

    const NAMES: &[&str] = &[
        "email",
        "e_mail",
        "recipient",
        "subscriber",
        "contact",
        "rcpt",
        "uid",
        "eid",
        "sid",
        "pid",
        "cuid",
        "userid",
        "user_id",
    ];
    if let Some(query) = lower.split('?').nth(1) {
        for pair in query.split(['&', ';']) {
            let name = pair.split('=').next().unwrap_or_default().trim();
            if NAMES.contains(&name) {
                return true;
            }
        }
    }

    // Un segment opaque long, dans le chemin comme dans une valeur de paramètre.
    lower
        .split(['/', '?', '&', '=', '#', ';'])
        .any(looks_opaque)
}

/// Vrai si un segment ressemble à un jeton opaque plutôt qu'à un nom.
fn looks_opaque(segment: &str) -> bool {
    // Une extension de fichier veut dire que c'est un nom de fichier, aussi long soit-il.
    let segment = segment.trim_end_matches(|c: char| c == '.' || c.is_ascii_alphabetic());
    let candidate = segment.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if candidate.len() < 24 {
        return false;
    }
    let alphanumeric = candidate
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '%'));
    if !alphanumeric {
        return false;
    }
    // Un jeton mélange les casses ou les chiffres ; un mot en toutes lettres, non.
    let digits = candidate.chars().filter(char::is_ascii_digit).count();
    let uppercase = candidate.chars().filter(|c| c.is_ascii_uppercase()).count();
    digits > 0 || uppercase > 2
}

/// Découpe un `srcset`, laisse les autres attributs entiers.
fn split_srcset(value: &str, attribute: &str) -> Vec<String> {
    if attribute != "srcset" {
        return vec![value.trim().to_owned()];
    }
    value
        .split(',')
        .filter_map(|candidate| candidate.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

/// Les URL référencées par du CSS : `url(...)` et `@import "..."`.
fn css_urls(css: &str) -> Vec<String> {
    let mut found = Vec::new();
    let lower = css.to_lowercase();

    let mut cursor = 0usize;
    while let Some(offset) = lower[cursor..].find("url(") {
        let start = cursor + offset + 4;
        let Some(end) = css[start..].find(')') else {
            break;
        };
        found.push(
            css[start..start + end]
                .trim()
                .trim_matches(['"', '\''])
                .to_owned(),
        );
        cursor = start + end;
    }

    let mut cursor = 0usize;
    while let Some(offset) = lower[cursor..].find("@import") {
        let start = cursor + offset + 7;
        // La règle s'arrête au `;` ou à l'accolade : sans cette borne, on irait chercher un
        // guillemet dans la règle suivante et on compterait une URL qui n'est pas là.
        let rule_end = css[start..]
            .find([';', '{'])
            .map_or(css.len(), |end| start + end);
        let rest = css[start..rule_end].trim_start();

        // `@import url(...)` a déjà été compté par le passage sur `url(` ; le recompter ici
        // doublerait le chiffre. Seule la forme `@import "…"` reste à traiter.
        if !rest.to_lowercase().starts_with("url(")
            && let Some(quote_start) = rest.find(['"', '\''])
        {
            let quote = rest.as_bytes()[quote_start] as char;
            if let Some(quote_end) = rest[quote_start + 1..].find(quote) {
                found.push(rest[quote_start + 1..quote_start + 1 + quote_end].to_owned());
            }
        }
        cursor = start;
    }

    found
}

/// Une balise ouvrante et ses attributs.
#[derive(Debug, Default)]
struct Element<'a> {
    name: String,
    attributes: Vec<(String, &'a str)>,
}

impl<'a> Element<'a> {
    /// Un élément sans identité, pour les URL qui ne viennent pas d'une balise.
    fn anonymous() -> Self {
        Self::default()
    }

    /// La valeur d'un attribut, insensible à la casse du nom.
    fn attribute(&self, name: &str) -> Option<&'a str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| *value)
    }

    /// Vrai si l'élément est une image d'un pixel, ou invisible.
    fn is_pixel(&self) -> bool {
        if self.name != "img" {
            return false;
        }
        let tiny = |name: &str| {
            self.attribute(name)
                .and_then(|value| value.trim().trim_end_matches("px").parse::<f32>().ok())
                .is_some_and(|size| size <= 1.0)
        };
        if tiny("width") && tiny("height") {
            return true;
        }
        // Une image qu'on cache est une image qu'on ne veut pas voir mais qu'on veut charger.
        self.attribute("style").is_some_and(|style| {
            let style = style.replace(' ', "").to_lowercase();
            style.contains("display:none") || style.contains("visibility:hidden")
        })
    }
}

/// Parcourt les balises ouvrantes d'un document.
///
/// Lecteur délibérément simple : voir le module pour pourquoi c'est acceptable ici et
/// nulle part ailleurs. Il saute les commentaires, tolère les guillemets manquants, et ne
/// remonte jamais en arrière — donc il termine toujours, quelle que soit l'entrée.
fn elements(html: &str) -> Vec<Element<'_>> {
    let bytes = html.as_bytes();
    let mut found = Vec::new();
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] != b'<' {
            index += 1;
            continue;
        }
        if html[index..].starts_with("<!--") {
            index = html[index..]
                .find("-->")
                .map_or(bytes.len(), |end| index + end + 3);
            continue;
        }
        index += 1;

        let name_start = index;
        while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'-')
        {
            index += 1;
        }
        if index == name_start {
            continue;
        }
        let name = html[name_start..index].to_lowercase();

        let mut element = Element {
            name,
            attributes: Vec::new(),
        };
        index = parse_attributes(html, index, &mut element);
        found.push(element);
    }

    found
}

/// Lit les attributs jusqu'au `>`, et rend la position juste après.
fn parse_attributes<'a>(html: &'a str, mut index: usize, element: &mut Element<'a>) -> usize {
    let bytes = html.as_bytes();

    loop {
        while index < bytes.len() && (bytes[index] as char).is_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            return index;
        }
        if bytes[index] == b'>' {
            return index + 1;
        }
        if bytes[index] == b'/' {
            index += 1;
            continue;
        }

        let name_start = index;
        while index < bytes.len()
            && !matches!(bytes[index], b'=' | b'>' | b'/')
            && !(bytes[index] as char).is_whitespace()
        {
            index += 1;
        }
        if index == name_start {
            // Aucun progrès possible sur ce caractère : l'avaler plutôt que boucler.
            index += 1;
            continue;
        }
        let name = html[name_start..index].to_lowercase();

        while index < bytes.len() && (bytes[index] as char).is_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            // Attribut sans valeur.
            element.attributes.push((name, ""));
            continue;
        }
        index += 1;
        while index < bytes.len() && (bytes[index] as char).is_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            return index;
        }

        let value_start;
        let value_end;
        match bytes[index] {
            quote @ (b'"' | b'\'') => {
                index += 1;
                value_start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                value_end = index;
                index = (index + 1).min(bytes.len());
            }
            _ => {
                value_start = index;
                while index < bytes.len()
                    && bytes[index] != b'>'
                    && !(bytes[index] as char).is_whitespace()
                {
                    index += 1;
                }
                value_end = index;
            }
        }
        element
            .attributes
            .push((name, &html[value_start..value_end]));
    }
}

/// Le contenu de chaque bloc `<style>`.
fn style_blocks(html: &str) -> Vec<&str> {
    let lower = html.to_lowercase();
    let mut blocks = Vec::new();
    let mut cursor = 0usize;

    while let Some(offset) = lower[cursor..].find("<style") {
        let open = cursor + offset;
        let Some(content_start) = lower[open..].find('>').map(|end| open + end + 1) else {
            break;
        };
        let end = lower[content_start..]
            .find("</style")
            .map_or(html.len(), |end| content_start + end);
        blocks.push(&html[content_start..end]);
        cursor = end.max(content_start);
        if cursor >= html.len() {
            break;
        }
    }

    blocks
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn hosts(report: &Report, kind: Kind) -> Vec<String> {
        report
            .trackers
            .iter()
            .filter(|tracker| tracker.kind == kind)
            .map(|tracker| tracker.host.clone())
            .collect()
    }

    #[test]
    fn a_message_without_remote_content_is_clean() {
        let report = scan("<p>Bonjour <b>Marie</b></p><img src=\"data:image/png;base64,AA==\">");
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn a_one_by_one_image_is_a_pixel() {
        let report = scan(r#"<img src="https://pisteur.fr/o.gif" width="1" height="1">"#);
        assert_eq!(hosts(&report, Kind::Pixel), ["pisteur.fr"]);
        assert_eq!(report.remote_resources, 1);
    }

    #[test]
    fn zero_dimensions_count_as_a_pixel_too() {
        let report = scan(r#"<img src="https://pisteur.fr/o.gif" width="0" height="0">"#);
        assert_eq!(hosts(&report, Kind::Pixel), ["pisteur.fr"]);
    }

    #[test]
    fn a_hidden_image_is_a_pixel_whatever_its_declared_size() {
        // Cacher une image qu'on charge quand même est le même geste, mieux déguisé.
        let report =
            scan(r#"<img src="https://pisteur.fr/o.gif" width="600" style="display: none">"#);
        assert_eq!(hosts(&report, Kind::Pixel), ["pisteur.fr"]);
    }

    #[test]
    fn an_ordinary_remote_image_is_counted_without_being_accused() {
        // Le logo d'une facture n'est pas un traceur. C'est quand même une requête.
        let report = scan(r#"<img src="https://exemple.fr/logo.png" width="200" height="60">"#);
        assert_eq!(report.remote_resources, 1);
        assert!(report.trackers.is_empty(), "{report:?}");
    }

    #[test]
    fn a_known_tracker_domain_is_named() {
        let report = scan(r#"<img src="https://click.list-manage.com/track/open.php">"#);
        assert_eq!(hosts(&report, Kind::KnownDomain), ["click.list-manage.com"]);
    }

    #[test]
    fn a_correlated_identifier_is_recognised_in_its_usual_forms() {
        for url in [
            "https://x.fr/o.gif?email=marie%40exemple.fr",
            "https://x.fr/o.gif?recipient=marie@exemple.fr",
            "https://x.fr/open/7f3a9b2c4d5e6f708192a3b4c5d6e7f8",
            "https://x.fr/p?uid=abcdef",
        ] {
            let report = scan(&format!(r#"<img src="{url}">"#));
            assert_eq!(
                hosts(&report, Kind::CorrelatedId),
                ["x.fr"],
                "non détecté : {url}"
            );
        }
    }

    #[test]
    fn an_ordinary_filename_is_not_mistaken_for_an_identifier() {
        for url in [
            "https://exemple.fr/images/entete-de-la-newsletter.png",
            "https://exemple.fr/logo.png",
            "https://exemple.fr/assets/style-principal.css",
            "https://exemple.fr/c/campagne-de-noel",
        ] {
            let report = scan(&format!(r#"<img src="{url}">"#));
            assert!(
                hosts(&report, Kind::CorrelatedId).is_empty(),
                "faux positif sur {url} : {report:?}"
            );
        }
    }

    #[test]
    fn css_urls_are_seen_in_attributes_and_in_blocks() {
        let report = scan(
            "<style>@import url('https://a.fr/vol.css'); \
             body{background:url(\"https://b.fr/fond.png\")}</style>\
             <div style=\"background-image: url(https://c.fr/x.png)\">x</div>",
        );
        // Trois URL, comptées une fois chacune : l'`@import url(…)` ne doit pas être compté
        // deux fois par les deux passages du lecteur CSS.
        assert_eq!(report.remote_resources, 3, "{report:?}");
    }

    #[test]
    fn an_at_import_with_a_bare_string_is_seen() {
        let report = scan("<style>@import \"https://a.fr/vol.css\";</style>");
        assert_eq!(report.remote_resources, 1, "{report:?}");
    }

    #[test]
    fn a_link_is_not_a_remote_resource_because_nothing_is_fetched() {
        // `docs/PRIVACY.md` §6 : aucune navigation automatique, aucun préchargement. Un lien
        // ne fuite rien tant que personne ne clique.
        let report = scan(r#"<a href="https://pisteur.fr/clic?email=marie%40x.fr">clic</a>"#);
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn the_host_is_kept_but_never_the_url() {
        // Une URL de traceur *est* l'identifiant corrélé : la recopier reviendrait à
        // conserver ce qu'on dénonce.
        let report =
            scan(r#"<img src="https://x.fr/o.gif?email=marie%40exemple.fr" width=1 height=1>"#);
        let serialised = format!("{report:?}");
        assert!(
            !serialised.contains("marie"),
            "l'URL a fuité : {serialised}"
        );
        assert!(
            !serialised.contains("o.gif"),
            "l'URL a fuité : {serialised}"
        );
        assert!(serialised.contains("x.fr"));
    }

    #[test]
    fn credentials_before_the_host_do_not_fool_the_extraction() {
        // `https://exemple.fr@pisteur.fr/` est servi par pisteur.fr, pas par exemple.fr.
        let report = scan(r#"<img src="https://exemple.fr@click.list-manage.com/o.gif">"#);
        assert_eq!(hosts(&report, Kind::KnownDomain), ["click.list-manage.com"]);
    }

    #[test]
    fn a_scheme_relative_url_still_loads_and_still_counts() {
        let report = scan(r#"<img src="//pisteur.fr/o.gif" width=1 height=1>"#);
        assert_eq!(hosts(&report, Kind::Pixel), ["pisteur.fr"]);
    }

    #[test]
    fn duplicates_are_reported_once_per_host_and_reason() {
        let report = scan(&r#"<img src="https://pisteur.fr/o.gif" width=1 height=1>"#.repeat(5));
        assert_eq!(report.trackers.len(), 1);
        // Le compteur de ressources, lui, compte bien les cinq requêtes évitées.
        assert_eq!(report.remote_resources, 5);
    }

    #[test]
    fn the_order_is_stable_across_runs() {
        let html = r#"<img src="https://zeta.fr/o.gif" width=1 height=1>
                      <img src="https://alpha.fr/o.gif" width=1 height=1>"#;
        let first = scan(html);
        for _ in 0..20 {
            assert_eq!(scan(html), first, "l'ordre du rapport a changé");
        }
        assert_eq!(hosts(&first, Kind::Pixel), ["alpha.fr", "zeta.fr"]);
    }

    // --- Entrée hostile ---

    #[test]
    fn malformed_markup_never_hangs_or_panics() {
        for broken in [
            "",
            "<",
            "<img",
            "<img src",
            "<img src=",
            "<img src=\"",
            "<img src='non fermé",
            "<img src=sans-guillemets width=1 height=1>",
            "<<<<>>>>",
            "<!-- commentaire non fermé <img src=\"https://x.fr/o.gif\">",
            "<style>",
            "<style>@import",
            "<style>url(",
            "<img src=\"https://x.fr/o.gif\" width=\"abc\" height=\"\">",
            "<img/src=\"https://x.fr/o.gif\">",
            "<IMG SRC=\"https://X.FR/O.GIF\" WIDTH=1 HEIGHT=1>",
        ] {
            let report = scan(broken);
            // Ce qui compte : ça termine et ça ne panique pas. Le chiffre importe peu.
            let _ = report.remote_resources;
        }
    }

    #[test]
    fn uppercase_markup_is_handled_like_any_other() {
        let report = scan("<IMG SRC=\"https://PISTEUR.FR/O.GIF\" WIDTH=1 HEIGHT=1>");
        assert_eq!(hosts(&report, Kind::Pixel), ["pisteur.fr"]);
    }

    #[test]
    fn a_pathological_document_terminates() {
        // Un lecteur qui reculerait boucherait ici. Celui-ci n'avance que vers l'avant.
        let hostile = "<img src=\"<img src=\"".repeat(10_000);
        let _ = scan(&hostile);
        let unclosed = "<style>".repeat(10_000);
        let _ = scan(&unclosed);
        let nested = "<!--".repeat(10_000);
        let _ = scan(&nested);
    }
}

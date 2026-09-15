//! Découpage d'un corps HTML **assaini** en blocs affichables par une interface native.
//!
//! ## Pourquoi ce module existe
//!
//! [`crate::text`] aplatit le HTML pour l'index : il écrase tous les blancs et jette les URL.
//! C'est ce qu'il faut pour tantivy, et c'est inutilisable pour lire — un message y devient un
//! seul mur de texte sans paragraphes ni citations.
//!
//! La coquille native, elle, n'a pas de moteur de rendu à qui donner du HTML. Elle a besoin
//! d'une **liste de blocs** : des paragraphes, des titres, des puces, des citations, des liens.
//! Ce module la produit.
//!
//! ## Ce qu'il n'est pas
//!
//! **Ce n'est pas un moteur de rendu HTML et ça n'essaie pas de l'être.** Pas de tables mises
//! en page, pas de flottants, pas de CSS. Une table devient une suite de lignes, une cellule un
//! fragment de texte. Pour le mail humain et la plupart des listes de diffusion, c'est fidèle ;
//! pour une newsletter à tables imbriquées, c'est une mise en page « mode lecture », et
//! l'utilisateur garde l'échappatoire « ouvrir dans le navigateur ».
//!
//! Ce n'est **pas non plus une barrière de sécurité**. L'entrée attendue est la sortie de
//! [`crate::sanitize`], donc du HTML dont les balises et les attributs ont déjà été filtrés par
//! `ammonia`. Ce module ne réintroduit rien : il ne rend que du texte, un niveau de style et
//! des URL, et **il ne va jamais chercher une ressource**. Une image n'est pas chargée, elle est
//! annoncée — ce qui est précisément la garantie de `docs/PRIVACY.md`, obtenue ici par
//! construction plutôt que par configuration d'un moteur.
//!
//! ## Robustesse
//!
//! Comme [`crate::text`], cette fonction **ne peut pas échouer** : balise non fermée, `<`
//! isolé, imbrication de travers, entité inconnue, tout produit une sortie plus pauvre et
//! jamais une erreur. Le parcours est linéaire et sans récursion, donc une imbrication de dix
//! mille `<div>` coûte de la mémoire de pile constante — un corps de message est une entrée
//! hostile, et un débordement de pile serait un déni de service.

use std::fmt::Write as _;

/// Le style d'un fragment de texte. Un seul niveau : gras, italique, code, barré, lien.
///
/// Volontairement pauvre. Le mail réel utilise des dizaines de variantes typographiques dont
/// aucune ne change le sens ; en garder cinq suffit à lire, et évite de réinventer un moteur de
/// styles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    /// La cible du lien, telle que le sanitizer l'a laissée. Jamais suivie par ce module.
    pub link: Option<String>,
}

/// Un fragment de texte et son style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub style: Style,
}

/// Ce qu'un bloc représente.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Un paragraphe ordinaire.
    Paragraph,
    /// Un titre, de 1 à 6.
    Heading(u8),
    /// Un élément de liste, avec sa profondeur et sa numérotation.
    Item { depth: u8, ordered: bool },
    /// Une citation, avec sa profondeur — les fils de discussion en empilent.
    Quote(u8),
    /// Un séparateur horizontal.
    Rule,
    /// Du texte préformaté : les blancs y sont conservés.
    Pre,
    /// Une image **annoncée, jamais chargée**.
    Image {
        /// Le texte alternatif, s'il y en a un.
        alt: String,
        /// Ce que le sanitizer a laissé de la source, pour que l'interface puisse dire à
        /// l'utilisateur ce qui est en jeu. Absente quand elle a été retirée.
        source: Option<Source>,
    },
}

/// La nature d'une source d'image survivante.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Une image embarquée dans le message : `data:` ou `cid:`. Aucune requête réseau.
    Embedded,
    /// Une adresse distante. Présente seulement si l'utilisateur a autorisé les images.
    Remote,
}

/// Un bloc affichable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: Kind,
    /// Les fragments du bloc. Vide pour [`Kind::Rule`] et [`Kind::Image`].
    pub runs: Vec<Run>,
}

impl Block {
    /// Le texte du bloc, styles ignorés. Pour les tests et pour un repli en texte brut.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for run in &self.runs {
            let _ = write!(out, "{}", run.text);
        }
        out
    }
}

/// Nombre maximal de blocs rendus.
///
/// Une borne, pas une élégance : un message peut contenir des centaines de milliers de balises,
/// et une interface qui en construirait autant de blocs se figerait. Le corps est déjà tronqué
/// en amont par le service ; ceci est la ceinture.
const MAX_BLOCKS: usize = 20_000;

/// Profondeur maximale prise en compte pour les citations et les listes.
///
/// Au-delà, l'indentation ne veut plus rien dire et un `u8` déborderait.
const MAX_DEPTH: u8 = 12;

/// Découpe du HTML assaini en blocs affichables.
#[must_use]
pub fn blocks(html: &str) -> Vec<Block> {
    let mut state = Scanner::default();
    let bytes = html.as_bytes();
    let mut index = 0usize;

    while index < bytes.len() && state.out.len() < MAX_BLOCKS {
        match bytes[index] {
            b'<' => match Tag::at(html, index) {
                Some(tag) => {
                    let next = tag.end;
                    state.tag(&tag, html);
                    // Une balise dont la fin n'avance pas ferait une boucle infinie sur un
                    // document tronqué. `Tag::at` garantit `end > start`, mais la borne est
                    // écrite ici parce que c'est ici qu'elle protège.
                    index = next.max(index + 1);
                }
                // Un `<` qui n'ouvre pas de balise est du texte : « 3 < 5 » non échappé est
                // fréquent dans le courrier réel.
                None => {
                    state.push_char('<');
                    index += 1;
                }
            },
            b'&' => {
                let (decoded, consumed) = crate::text::entity(html, index);
                state.push_char(decoded.unwrap_or('&'));
                index += consumed.max(1);
            }
            byte if byte.is_ascii_whitespace() => {
                state.push_space(byte);
                index += 1;
            }
            _ => {
                let width = char_width(bytes[index]);
                let end = (index + width).min(bytes.len());
                if let Some(slice) = html.get(index..end) {
                    state.push_str(slice);
                }
                index = end;
            }
        }
    }

    state.flush();
    state.out
}

/// L'état du parcours.
#[derive(Debug, Default)]
struct Scanner {
    out: Vec<Block>,
    /// Les fragments du bloc en cours.
    runs: Vec<Run>,
    /// Le texte du fragment en cours, sous le style courant.
    pending: String,
    /// Vrai si un blanc est en attente d'être écrit — sert à les fondre en un seul.
    space: bool,
    /// Compteurs d'imbrication : `<b><b>x</b></b>` doit rester gras après le premier `</b>`.
    bold: u16,
    italic: u16,
    code: u16,
    strike: u16,
    /// La pile des liens ouverts. Le plus récent gagne.
    links: Vec<String>,
    quote: u8,
    /// La pile des listes : `true` pour une liste numérotée.
    lists: Vec<bool>,
    /// Vrai entre `<li>` et sa fermeture.
    item: bool,
    heading: Option<u8>,
    pre: u16,
}

impl Scanner {
    /// Le style courant.
    fn style(&self) -> Style {
        Style {
            bold: self.bold > 0,
            italic: self.italic > 0,
            code: self.code > 0 || self.pre > 0,
            strike: self.strike > 0,
            link: self.links.last().cloned(),
        }
    }

    /// Le genre du bloc en cours, déduit de l'état.
    fn kind(&self) -> Kind {
        if self.pre > 0 {
            return Kind::Pre;
        }
        if self.item {
            let depth = u8::try_from(self.lists.len()).unwrap_or(MAX_DEPTH).max(1);
            return Kind::Item {
                depth: depth.min(MAX_DEPTH),
                ordered: self.lists.last().copied().unwrap_or(false),
            };
        }
        if let Some(level) = self.heading {
            return Kind::Heading(level);
        }
        if self.quote > 0 {
            return Kind::Quote(self.quote);
        }
        Kind::Paragraph
    }

    /// Termine le fragment en cours.
    fn seal(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending);
        let style = self.style();
        // Fusionner avec le fragment précédent quand le style est le même : sans ça, une
        // phrase coupée par un `<span>` inutile produirait dix fragments pour rien.
        match self.runs.last_mut() {
            Some(last) if last.style == style => last.text.push_str(&text),
            _ => self.runs.push(Run { text, style }),
        }
    }

    /// Termine le bloc en cours et l'ajoute, s'il a du contenu.
    fn flush(&mut self) {
        self.seal();
        self.space = false;
        if self.runs.is_empty() {
            return;
        }
        let runs = std::mem::take(&mut self.runs);
        // Un bloc qui ne contient que des blancs n'apporte rien à l'écran. Fréquent : les
        // générateurs de mail empilent les `<div>` vides.
        if runs.iter().all(|run| run.text.trim().is_empty()) {
            return;
        }
        let kind = self.kind();
        self.out.push(Block { kind, runs });
    }

    /// Ajoute un bloc sans texte — un filet, une image.
    fn standalone(&mut self, kind: Kind) {
        self.flush();
        if self.out.len() < MAX_BLOCKS {
            self.out.push(Block {
                kind,
                runs: Vec::new(),
            });
        }
    }

    /// Vrai si un blanc en attente doit être écrit.
    ///
    /// Jamais en tête de bloc : `<td>` et `<th>` posent un blanc de séparation, et sans ce
    /// garde-fou la première cellule d.une ligne commencerait par une espace.
    fn wants_space(&self) -> bool {
        self.space && !(self.pending.is_empty() && self.runs.is_empty())
    }

    fn push_char(&mut self, character: char) {
        if self.wants_space() {
            self.pending.push(' ');
        }
        self.space = false;
        self.pending.push(character);
    }

    fn push_str(&mut self, slice: &str) {
        if self.wants_space() {
            self.pending.push(' ');
        }
        self.space = false;
        self.pending.push_str(slice);
    }

    /// Un blanc. Dans un `<pre>`, il compte tel quel ; ailleurs, il fond avec ses voisins.
    fn push_space(&mut self, byte: u8) {
        if self.pre > 0 {
            if byte == b'\n' {
                // Une nouvelle ligne dans du préformaté est une nouvelle ligne à l'écran.
                self.flush();
            } else {
                self.pending.push(char::from(byte));
            }
            return;
        }
        if !self.pending.is_empty() || !self.runs.is_empty() {
            self.space = true;
        }
    }

    /// Traite une balise.
    fn tag(&mut self, tag: &Tag<'_>, html: &str) {
        let name = tag.name;
        if tag.closing {
            self.close(name);
            return;
        }
        self.open(name, tag.attributes(html));
    }

    /// Une balise ouvrante.
    fn open(&mut self, name: &str, attributes: &str) {
        match name {
            // Coupures de bloc.
            "p" | "div" | "table" | "tr" | "dl" | "dt" | "dd" | "figure" | "figcaption"
            | "address" | "center" | "caption" | "br" => self.flush(),
            "td" | "th" => {
                // Une cellule ne mérite pas un bloc à elle seule : la ligne est le bloc, et
                // les cellules y sont séparées par un blanc. Une table de mise en page — le
                // cas courant dans le mail — se lit alors comme du texte.
                self.space = true;
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                // `as u8` sur un chiffre ASCII connu : le nom vient d'être reconnu.
                self.heading = name.as_bytes().get(1).map(|byte| byte - b'0');
            }
            "blockquote" => {
                self.flush();
                self.quote = self.quote.saturating_add(1).min(MAX_DEPTH);
            }
            "ul" | "ol" => {
                self.flush();
                if self.lists.len() < usize::from(MAX_DEPTH) {
                    self.lists.push(name == "ol");
                }
            }
            "li" => {
                self.flush();
                self.item = true;
            }
            "pre" => {
                self.flush();
                self.pre = self.pre.saturating_add(1);
            }
            "hr" => self.standalone(Kind::Rule),
            "img" => {
                let alt = attribute(attributes, "alt").unwrap_or_default();
                let source = attribute(attributes, "src").map(|src| {
                    let src = src.trim_start().to_ascii_lowercase();
                    if src.starts_with("data:") || src.starts_with("cid:") {
                        Source::Embedded
                    } else {
                        Source::Remote
                    }
                });
                self.standalone(Kind::Image { alt, source });
            }
            "a" => {
                self.seal();
                // Un `<a>` sans `href` est une ancre : du texte, pas un lien.
                if let Some(href) = attribute(attributes, "href") {
                    self.links.push(href);
                }
            }
            "b" | "strong" => {
                self.seal();
                self.bold = self.bold.saturating_add(1);
            }
            "i" | "em" | "cite" | "dfn" | "var" => {
                self.seal();
                self.italic = self.italic.saturating_add(1);
            }
            "code" | "kbd" | "samp" | "tt" => {
                self.seal();
                self.code = self.code.saturating_add(1);
            }
            "del" | "s" | "strike" => {
                self.seal();
                self.strike = self.strike.saturating_add(1);
            }
            // Tout le reste — `span`, `font`, `small`, `sub`, `abbr`… — ne change ni le bloc
            // ni le style : le texte continue.
            _ => {}
        }
    }

    /// Une balise fermante.
    fn close(&mut self, name: &str) {
        match name {
            "p" | "div" | "table" | "tr" | "dl" | "dt" | "dd" | "figure" | "figcaption"
            | "address" | "center" | "caption" => self.flush(),
            "td" | "th" => self.space = true,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                self.heading = None;
            }
            "blockquote" => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            "ul" | "ol" => {
                self.flush();
                self.item = false;
                self.lists.pop();
            }
            "li" => {
                self.flush();
                self.item = false;
            }
            "pre" => {
                self.flush();
                self.pre = self.pre.saturating_sub(1);
            }
            "a" => {
                self.seal();
                self.links.pop();
            }
            "b" | "strong" => {
                self.seal();
                self.bold = self.bold.saturating_sub(1);
            }
            "i" | "em" | "cite" | "dfn" | "var" => {
                self.seal();
                self.italic = self.italic.saturating_sub(1);
            }
            "code" | "kbd" | "samp" | "tt" => {
                self.seal();
                self.code = self.code.saturating_sub(1);
            }
            "del" | "s" | "strike" => {
                self.seal();
                self.strike = self.strike.saturating_sub(1);
            }
            _ => {}
        }
    }
}

/// Une balise, son nom et l'étendue de ses attributs.
///
/// Distincte de celle de [`crate::text`], qui ne reconnaît que quatre noms et jette les
/// attributs : ici il faut le nom exact, un `href` et un `alt`.
struct Tag<'a> {
    name: &'a str,
    closing: bool,
    /// Offset du premier octet après le nom.
    attributes_start: usize,
    /// Offset juste après le `>`.
    end: usize,
}

impl<'a> Tag<'a> {
    /// Reconnaît la balise qui commence au `<` en `start`, ou rend `None` si c'est du texte.
    fn at(html: &'a str, start: usize) -> Option<Self> {
        let bytes = html.as_bytes();
        let after = start + 1;
        let next = *bytes.get(after)?;

        // Commentaire, doctype, CDATA : rien de tout ça n'est du texte ni un bloc.
        if next == b'!' {
            let end = if html[after..].starts_with("!--") {
                find_after(html, after + 3, "-->")
            } else {
                find_after(html, after, ">")
            };
            return Some(Self {
                name: "",
                closing: false,
                attributes_start: end,
                end,
            });
        }

        let closing = next == b'/';
        let name_start = if closing { after + 1 } else { after };
        let first = *bytes.get(name_start)?;
        if !first.is_ascii_alphabetic() {
            if closing {
                // `</` suivi d'autre chose qu'une lettre : la spécification appelle ça un
                // « bogus comment » et le jette. Ici aussi.
                let end = find_after(html, name_start, ">");
                return Some(Self {
                    name: "",
                    closing: true,
                    attributes_start: end,
                    end,
                });
            }
            // `<3`, `< 5`, `<=` : du texte.
            return None;
        }

        let name_end = bytes[name_start..]
            .iter()
            .position(|byte| !byte.is_ascii_alphanumeric())
            .map_or(bytes.len(), |offset| name_start + offset);

        Some(Self {
            name: html.get(name_start..name_end).unwrap_or(""),
            closing,
            attributes_start: name_end,
            end: tag_end(html, name_end),
        })
    }

    /// La tranche brute des attributs, sans le `>`.
    fn attributes(&self, html: &'a str) -> &'a str {
        let end = self.end.saturating_sub(1).max(self.attributes_start);
        html.get(self.attributes_start..end).unwrap_or("")
    }
}

/// L'offset juste après la prochaine occurrence de `needle`, ou la fin.
///
/// Sert aux formes qui n'ont pas d'attributs — commentaires, doctype, balisage cassé — où il n'y
/// a pas de valeur entre guillemets à sauter.
fn find_after(html: &str, from: usize, needle: &str) -> usize {
    html.get(from..)
        .and_then(|rest| rest.find(needle))
        .map_or(html.len(), |offset| from + offset + needle.len())
}

/// L'offset juste après le `>` qui ferme la balise, **en sautant les valeurs entre guillemets**.
///
/// ## Pourquoi la conscience des guillemets est nécessaire
///
/// Le sérialiseur d'`html5ever` n'échappe **pas** le `>` dans une valeur d'attribut : un
/// `alt="a > b"` ressort tel quel, et c'est courant dans le courrier réel. Une recherche naïve du
/// premier `>` coupait alors la balise en son milieu — l'attribut suivant devenait du texte, et
/// un `href` pouvait disparaître.
///
/// Trouvé en relecture le 2026-09-03, avec deux reproductions : `<img alt="a > b" src="cid:x">`
/// rendait une image sans source, suivie du texte `b" src="cid:x"> suite`.
fn tag_end(html: &str, from: usize) -> usize {
    let bytes = html.as_bytes();
    let mut index = from;
    let mut quote: Option<u8> = None;

    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                // Le `>` de fermeture, hors de toute valeur.
                b'>' => return index + 1,
                _ => {}
            },
        }
        index += 1;
    }
    // Balise non fermée : elle avale la fin du document. C'est le bon comportement — un fragment
    // tronqué ne redevient pas du texte lisible plus loin.
    bytes.len()
}

/// Lit la valeur d'un attribut dans la tranche brute d'une balise.
///
/// ## Un tokeniseur, et non une recherche de sous-chaîne
///
/// La première version cherchait le nom avec `find` après avoir abaissé la casse du reste **à
/// chaque tour de boucle**. Deux défauts, tous deux mesurés en relecture le 2026-09-03 :
///
/// - **quadratique.** Un `alt` de 200 Ko contenant le mot cherché faisait 204 ms, et le coût
///   quadruplait à chaque doublement — soit une vingtaine de secondes au plafond d'un corps de
///   message. Ce découpage tourne sur une entrée hostile : c'était un déni de service à un mail.
/// - **faux positifs dans les valeurs.** La garde de frontière acceptait un guillemet comme
///   caractère précédent, donc `title="href=&quot;http://mechant/&quot;"` fabriquait un lien vers
///   `http://mechant/` en ignorant le vrai `href`. Inoffensif tant qu'un lien n'est pas
///   cliquable ; une URL choisie par l'expéditeur le jour où il le devient.
///
/// Ici : **un seul passage**, les noms comparés sans allocation, les valeurs sautées comme des
/// valeurs. Les trois formes d'attribut sont reconnues — sans valeur, valeur nue, valeur entre
/// guillemets simples ou doubles. `ammonia` n'en produit qu'une, mais ce module ne doit pas se
/// casser le jour où on lui donne autre chose.
fn attribute(raw: &str, name: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut index = 0usize;

    while index < bytes.len() {
        // Sauter les blancs et le `/` d'une balise auto-fermante.
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b'/') {
            index += 1;
        }
        if index >= bytes.len() {
            return None;
        }

        // Le nom : jusqu'à un `=`, un blanc, un `/`, ou la fin.
        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'='
            && bytes[index] != b'/'
        {
            index += 1;
        }
        let found = raw.get(name_start..index).unwrap_or("");
        // Un nom vide voudrait dire qu'on n'avance pas : sortir plutôt que boucler.
        if found.is_empty() {
            return None;
        }

        // L'éventuel `=`, après les blancs.
        let mut equals = index;
        while equals < bytes.len() && bytes[equals].is_ascii_whitespace() {
            equals += 1;
        }
        if bytes.get(equals) != Some(&b'=') {
            // Attribut sans valeur. Il compte quand même comme présent.
            if found.eq_ignore_ascii_case(name) {
                return Some(String::new());
            }
            continue;
        }

        let mut at = equals + 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let (value, next) = match bytes.get(at) {
            Some(&quote @ (b'"' | b'\'')) => {
                let inner = at + 1;
                let end = raw
                    .get(inner..)
                    .and_then(|rest| rest.find(char::from(quote)))
                    .map_or(bytes.len(), |offset| inner + offset);
                (
                    raw.get(inner..end).unwrap_or(""),
                    (end + 1).min(bytes.len()),
                )
            }
            Some(_) => {
                let end = raw
                    .get(at..)
                    .and_then(|rest| rest.find(char::is_whitespace))
                    .map_or(bytes.len(), |offset| at + offset);
                (raw.get(at..end).unwrap_or(""), end)
            }
            None => ("", bytes.len()),
        };
        index = next;

        if found.eq_ignore_ascii_case(name) {
            // Les entités sont décodées : `&amp;` dans une URL est un `&`.
            return Some(crate::text::to_text(value));
        }
    }
    None
}
/// Le nombre d'octets d'un caractère UTF-8 d'après son premier octet.
const fn char_width(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn kinds(html: &str) -> Vec<Kind> {
        blocks(html).into_iter().map(|it| it.kind).collect()
    }

    fn texts(html: &str) -> Vec<String> {
        blocks(html).iter().map(Block::text).collect()
    }

    #[test]
    fn paragraphs_are_separated() {
        assert_eq!(
            texts("<p>premier</p><p>second</p>"),
            vec!["premier".to_owned(), "second".to_owned()]
        );
    }

    #[test]
    fn a_line_break_starts_a_block() {
        assert_eq!(
            texts("une<br>deux<br />trois"),
            vec!["une".to_owned(), "deux".to_owned(), "trois".to_owned()]
        );
    }

    #[test]
    fn quotes_carry_their_depth() {
        let found = kinds("<blockquote>un<blockquote>deux</blockquote></blockquote>");
        assert_eq!(found, vec![Kind::Quote(1), Kind::Quote(2)]);
    }

    #[test]
    fn list_items_carry_depth_and_numbering() {
        let found = kinds("<ul><li>a</li><ol><li>b</li></ol></ul>");
        assert_eq!(
            found,
            vec![
                Kind::Item {
                    depth: 1,
                    ordered: false
                },
                Kind::Item {
                    depth: 2,
                    ordered: true
                }
            ]
        );
    }

    #[test]
    fn headings_keep_their_level() {
        assert_eq!(kinds("<h3>titre</h3>"), vec![Kind::Heading(3)]);
    }

    #[test]
    fn styles_nest_and_unwind() {
        let found = blocks("<b>gras <i>et italique</i> encore gras</b>");
        let styles: Vec<(bool, bool)> = found[0]
            .runs
            .iter()
            .map(|run| (run.style.bold, run.style.italic))
            .collect();
        assert_eq!(styles, vec![(true, false), (true, true), (true, false)]);
    }

    #[test]
    fn a_link_keeps_its_target_and_never_follows_it() {
        let found = blocks(r#"<a href="https://exemple.fr/x?a=1&amp;b=2">ici</a>"#);
        let run = &found[0].runs[0];
        assert_eq!(run.text, "ici");
        assert_eq!(
            run.style.link.as_deref(),
            Some("https://exemple.fr/x?a=1&b=2")
        );
    }

    #[test]
    fn an_anchor_without_href_is_plain_text() {
        let found = blocks("<a name=\"haut\">texte</a>");
        assert!(found[0].runs[0].style.link.is_none());
    }

    #[test]
    fn images_are_announced_by_nature_never_loaded() {
        let remote = blocks(r#"<img src="https://pisteur.example/p.gif" alt="pixel">"#);
        assert_eq!(
            remote[0].kind,
            Kind::Image {
                alt: "pixel".to_owned(),
                source: Some(Source::Remote)
            }
        );

        let embedded = blocks(r#"<img src="cid:logo" alt="logo">"#);
        assert_eq!(
            embedded[0].kind,
            Kind::Image {
                alt: "logo".to_owned(),
                source: Some(Source::Embedded)
            }
        );

        // Source retirée par le sanitizer : l'image est annoncée sans cible.
        let blocked = blocks(r#"<img alt="bloquée">"#);
        assert_eq!(
            blocked[0].kind,
            Kind::Image {
                alt: "bloquée".to_owned(),
                source: None
            }
        );
    }

    #[test]
    fn a_layout_table_reads_as_lines() {
        let found = texts("<table><tr><td>Nom</td><td>Valeur</td></tr><tr><td>a</td></tr></table>");
        assert_eq!(found, vec!["Nom Valeur".to_owned(), "a".to_owned()]);
    }

    #[test]
    fn preformatted_text_keeps_its_lines() {
        let found = blocks("<pre>un\n  deux</pre>");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].kind, Kind::Pre);
        assert_eq!(found[1].text(), "  deux");
    }

    #[test]
    fn whitespace_collapses_outside_pre() {
        assert_eq!(texts("<p>a   \n\t b</p>"), vec!["a b".to_owned()]);
    }

    #[test]
    fn empty_containers_produce_nothing() {
        assert!(blocks("<div></div><p>  </p><span> </span>").is_empty());
    }

    // --- Entrées malformées. `CLAUDE.md` : le parsing d'entrée hostile se teste sur les cas
    // cassés, pas seulement sur les cas propres.

    #[test]
    fn an_unclosed_tag_does_not_hang() {
        assert_eq!(texts("<p>texte"), vec!["texte".to_owned()]);
        assert!(blocks("<div").is_empty());
        // Un `<` seul est du texte, pas une balise.
        assert_eq!(texts("<"), vec!["<".to_owned()]);
        assert_eq!(texts("texte <"), vec!["texte <".to_owned()]);
    }

    #[test]
    fn a_stray_angle_bracket_is_text() {
        assert_eq!(
            texts("<p>3 < 5 et 5 > 3</p>"),
            vec!["3 < 5 et 5 > 3".to_owned()]
        );
    }

    #[test]
    fn unbalanced_closings_do_not_underflow() {
        // Trois fermetures pour aucune ouverture : les compteurs saturent à zéro.
        let found = blocks("</b></b></b>gras ?");
        assert!(!found[0].runs[0].style.bold);
    }

    #[test]
    fn deep_nesting_costs_constant_stack() {
        let deep = "<div>".repeat(50_000) + "fond" + &"</div>".repeat(50_000);
        // Le parcours est itératif : pas de récursion, donc pas de débordement de pile.
        assert_eq!(texts(&deep), vec!["fond".to_owned()]);
    }

    #[test]
    fn deep_quotes_are_capped() {
        let deep = "<blockquote>".repeat(100) + "fond";
        let found = blocks(&deep);
        assert_eq!(found[0].kind, Kind::Quote(MAX_DEPTH));
    }

    #[test]
    fn a_truncated_entity_is_text() {
        // Sans point-virgule, une entité reste littérale : « AT&T » est du texte, et non une
        // entité cassée à deviner.
        assert_eq!(
            texts("<p>a &amp b &#x</p>"),
            vec!["a &amp b &#x".to_owned()]
        );
    }

    #[test]
    fn a_multibyte_character_after_a_tag_does_not_panic() {
        // Le bogue trouvé sur le corpus réel dans `text.rs` : découper une tranche au milieu
        // d'un caractère UTF-8.
        assert_eq!(texts("<p>é</p>"), vec!["é".to_owned()]);
        assert_eq!(
            texts("<p>日本語のテキスト</p>"),
            vec!["日本語のテキスト".to_owned()]
        );
    }

    // --- Les trois défauts du premier tokeniseur, trouvés en relecture le 2026-09-03.

    #[test]
    fn an_angle_bracket_in_an_attribute_does_not_cut_the_tag() {
        // Le sérialiseur d'html5ever n'échappe pas `>` dans une valeur. La balise doit rester
        // entière, et l'attribut suivant doit encore être lu.
        let found = blocks(r#"<p><img alt="a > b" src="cid:x"> suite</p>"#);
        assert_eq!(
            found[0].kind,
            Kind::Image {
                alt: "a > b".to_owned(),
                source: Some(Source::Embedded)
            }
        );
        assert_eq!(found[1].text().trim(), "suite");
    }

    #[test]
    fn an_angle_bracket_in_a_link_keeps_the_link() {
        let found =
            blocks(r#"<p>Un <a title="a > b" href="https://bon.example/">lien</a> fin</p>"#);
        let link = found[0].runs.iter().find_map(|run| run.style.link.clone());
        assert_eq!(link.as_deref(), Some("https://bon.example/"));
    }

    #[test]
    fn a_value_that_looks_like_an_attribute_forges_nothing() {
        // `title` contient `href="…"`. La cible retenue doit être le **vrai** `href`, pas celle
        // que l'expéditeur a écrite dans une autre valeur.
        let found = blocks(
            r#"<a title="href=&quot;http://mechant.example/&quot;" href="https://bon.example/">lien</a>"#,
        );
        let link = found[0].runs.iter().find_map(|run| run.style.link.clone());
        assert_eq!(link.as_deref(), Some("https://bon.example/"));

        // Et une source forgée dans un `alt` ne doit pas faire annoncer une image comme
        // distante alors que l'assainisseur l'avait retirée.
        let image = blocks(r#"<img alt="src=&quot;http://mechant.example/x.png&quot;">"#);
        assert!(matches!(image[0].kind, Kind::Image { source: None, .. }));
    }

    #[test]
    fn a_huge_attribute_value_stays_linear() {
        // Le premier tokeniseur était quadratique : 200 Ko de `alt` contenant le nom cherché
        // prenaient 204 ms, et le coût quadruplait à chaque doublement. Ici, deux tailles et un
        // rapport qui doit rester proche de deux — la borne est large pour ne pas dépendre de la
        // machine, mais un retour au comportement quadratique la dépasserait de loin.
        let modest = format!(r#"<img alt="{}" src="cid:x">"#, "srcsrc".repeat(8_000));
        let double = format!(r#"<img alt="{}" src="cid:x">"#, "srcsrc".repeat(16_000));

        let start = std::time::Instant::now();
        let _ = blocks(&modest);
        let first = start.elapsed();
        let start = std::time::Instant::now();
        let _ = blocks(&double);
        let second = start.elapsed();

        // Le doublement de la taille ne doit pas multiplier le temps par plus de six.
        assert!(
            second < first.saturating_mul(6) + std::time::Duration::from_millis(5),
            "temps non linéaire : {first:?} puis {second:?}"
        );
    }

    #[test]
    fn attribute_forms_are_all_recognised() {
        // `ammonia` n'écrit qu'une forme, mais un jour on donnera peut-être autre chose à ce
        // module — et il ne doit pas se casser en silence.
        for html in [
            r#"<img src="cid:x">"#,
            r#"<img src='cid:x'>"#,
            r#"<img src=cid:x>"#,
            r#"<img  src = "cid:x" >"#,
        ] {
            let found = blocks(html);
            assert!(
                matches!(
                    found[0].kind,
                    Kind::Image {
                        source: Some(Source::Embedded),
                        ..
                    }
                ),
                "forme non reconnue : {html}"
            );
        }
    }

    #[test]
    fn an_attribute_lookalike_is_not_matched() {
        // `data-src` ne doit pas passer pour `src`.
        let found = blocks(r#"<img data-src="https://ailleurs.example/x.png" alt="a">"#);
        assert_eq!(
            found[0].kind,
            Kind::Image {
                alt: "a".to_owned(),
                source: None
            }
        );
    }

    #[test]
    fn the_block_count_is_bounded() {
        let many = "<p>x</p>".repeat(MAX_BLOCKS + 500);
        assert!(blocks(&many).len() <= MAX_BLOCKS);
    }
}

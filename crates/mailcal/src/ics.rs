//! Le format de ligne d'un fichier iCalendar : dépliage et propriétés.
//!
//! ## Une couche à part, parce qu'elle est purement syntaxique
//!
//! Un `.ics` est une suite de lignes `NOM;PARAM=VALEUR:valeur`, pliées à 75 octets. Ce module
//! ne connaît **aucune** propriété par son nom : il rend des lignes dépliées et des propriétés
//! découpées, et c'est [`crate::invitation`] qui sait ce que `DTSTART` veut dire. La séparation
//! n'est pas cosmétique — elle est ce qui permet au relevé de corpus de compter des noms de
//! propriétés sans rien comprendre au calendrier, et de découvrir ce qui existe avant qu'on
//! écrive comment le lire.
//!
//! ## Comme tout analyseur d'entrée hostile ici, il ne peut pas échouer
//!
//! Une ligne de travers, un `:` manquant, un paramètre non fermé, un octet nul : la sortie est
//! plus pauvre, jamais une erreur. C'est la règle de `mailhtml::blocks` et de `mailhtml::text`,
//! pour la même raison — ces octets viennent du réseau.

/// Nombre maximal de lignes dépliées d'un fichier.
///
/// Une borne, pas une élégance. Une invitation ordinaire fait quelques dizaines de lignes ;
/// celle d'un agenda entier exporté en fait des centaines de milliers, et un lecteur qui les
/// déplierait toutes mangerait la mémoire pour afficher un rendez-vous. Le corpus réel a été
/// mesuré avant de choisir ce chiffre — voir le journal de `docs/PHASE-3.md`.
pub const MAX_LINES: usize = 20_000;

/// Déplie les lignes d'un fichier iCalendar.
///
/// ## Le dépliage est la première chose à faire, et il est facile à faire de travers
///
/// RFC 5545 §3.1 : une ligne longue est coupée, et la suite commence par **une espace ou une
/// tabulation** qu'il faut retirer. Sans dépliage, une `SUMMARY` de plus de 75 octets — donc la
/// moitié des invitations réelles — arrive coupée en deux, et la seconde moitié ressemble à une
/// propriété inconnue.
///
/// Les fins de ligne acceptées sont `\r\n`, `\n` et `\r` seuls : la RFC impose `\r\n`, et le
/// corpus contient les trois. Refuser les deux autres reviendrait à ne pas lire un fichier
/// parfaitement lisible.
///
/// Les lignes vides sont jetées : elles n'ont pas de sens dans ce format, et certains
/// producteurs en laissent à la fin.
#[must_use]
pub fn unfold(text: &str) -> Vec<String> {
    // Les fins de ligne sont ramenées à `\n` d'abord : le reste de la fonction n'a alors qu'un
    // cas à traiter. Le premier jet découpait sur `\n` puis sur `\r`, ce qui demandait de
    // savoir si une continuation venait d'un pli ou d'une fin de ligne à l'ancienne — deux
    // questions là où il n'en faut qu'une.
    let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<String> = Vec::new();
    for raw in normalised.split('\n') {
        if out.len() >= MAX_LINES {
            break;
        }
        // **Un seul caractère est retiré, et c'est la règle exacte de la RFC.** Retirer tous
        // les blancs de tête mangerait une espace qui appartient à la valeur : un producteur
        // qui plie « du projet avec » juste avant l'espace écrit « \r\n  avec », dont le
        // premier blanc est le pli et le second le mot. Le premier jet trimait tout, et le
        // test a rendu « projetavec ».
        match raw.strip_prefix([' ', '\t']) {
            // Une continuation prolonge la ligne précédente, sans rien ajouter : le pli est
            // **dans** la valeur.
            Some(rest) if !out.is_empty() => {
                if let Some(last) = out.last_mut() {
                    last.push_str(rest);
                }
            }
            // Une continuation sans rien à prolonger — un fichier qui commence par un pli — est
            // jetée : elle ne peut appartenir à aucune propriété.
            Some(_) => {}
            None => {
                if !raw.trim().is_empty() {
                    out.push(raw.to_owned());
                }
            }
        }
    }
    out
}

/// Une propriété découpée : son nom, ses paramètres, sa valeur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// Le nom, tel qu'écrit. La comparaison se fait sans tenir compte de la casse.
    pub name: String,
    /// Les paramètres, dans l'ordre, sous la forme `(nom, valeur)`.
    pub params: Vec<(String, String)>,
    /// La valeur brute, **sans déséchappement** : voir [`Property::text`].
    pub value: String,
}

impl Property {
    /// Découpe une ligne dépliée.
    ///
    /// Rend `None` pour une ligne sans `:` — donc sans valeur — parce qu'elle ne dit rien.
    ///
    /// ## Le `:` cherché n'est pas le premier
    ///
    /// `ORGANIZER;CN="Durand, Éloïse":mailto:eloise@exemple.fr` a trois `:`. Celui qui sépare
    /// la valeur est le premier **hors guillemets**, et les guillemets existent précisément
    /// parce qu'un paramètre peut contenir `:` ou `,`. Prendre le premier `:` couperait au
    /// milieu d'un nom et rendrait un organisateur nommé « Durand » sans adresse.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let mut quoted = false;
        let mut split = None;
        for (at, character) in line.char_indices() {
            match character {
                '"' => quoted = !quoted,
                ':' if !quoted => {
                    split = Some(at);
                    break;
                }
                _ => {}
            }
        }
        let at = split?;
        let (head, value) = (&line[..at], &line[at + 1..]);

        let mut pieces = split_unquoted(head, ';');
        let name = pieces.next().unwrap_or_default().trim().to_owned();
        if name.is_empty() {
            return None;
        }
        let params = pieces
            .filter_map(|piece| {
                let (key, raw) = piece.split_once('=')?;
                let key = key.trim().to_owned();
                if key.is_empty() {
                    return None;
                }
                Some((key, unquote(raw.trim()).to_owned()))
            })
            .collect();

        Some(Self {
            name,
            params,
            value: value.to_owned(),
        })
    }

    /// Vrai si la propriété porte ce nom, casse ignorée.
    #[must_use]
    pub fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }

    /// La valeur d'un paramètre, casse du nom ignorée.
    ///
    /// Le **premier** quand il est répété : la RFC ne l'autorise pas pour ceux qui nous
    /// intéressent, et un fichier qui le fait quand même doit rendre une réponse et pas deux.
    #[must_use]
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// La valeur déséchappée d'une propriété de type TEXT.
    ///
    /// RFC 5545 §3.3.11 : `\\n` est un saut de ligne, `\\,` une virgule, `\\;` un point-virgule,
    /// `\\\\` une contre-oblique. Sans ce déséchappement, une adresse écrite sur deux lignes
    /// s'affiche avec des `\n` littéraux au milieu — et le corpus en est plein, parce que
    /// c'est ainsi qu'Outlook écrit un lieu.
    ///
    /// Une séquence inconnue rend le caractère qui suit, tel quel : c'est ce que fait un
    /// lecteur tolérant, et inventer autre chose ne servirait personne.
    #[must_use]
    pub fn text(&self) -> String {
        unescape(&self.value)
    }
}

/// Déséchappe une valeur TEXT.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n' | 'N') => out.push('\n'),
            Some('t') => out.push('\t'),
            // Une contre-oblique en fin de valeur : gardée telle quelle plutôt que jetée.
            None => out.push('\\'),
            Some(other) => out.push(other),
        }
    }
    out
}

/// Retire les guillemets d'une valeur de paramètre, s'il y en a.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|it| it.strip_suffix('"'))
        .unwrap_or(value)
}

/// Découpe sur un séparateur, en ignorant ceux qui sont entre guillemets.
fn split_unquoted(text: &str, separator: char) -> impl Iterator<Item = &str> {
    let mut pieces = Vec::new();
    let mut quoted = false;
    let mut start = 0usize;
    for (at, character) in text.char_indices() {
        match character {
            '"' => quoted = !quoted,
            it if it == separator && !quoted => {
                pieces.push(&text[start..at]);
                start = at + it.len_utf8();
            }
            _ => {}
        }
    }
    pieces.push(&text[start..]);
    pieces.into_iter()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{MAX_LINES, Property, unfold};

    #[test]
    fn a_folded_line_comes_back_in_one_piece() {
        // **Le premier piège du format.** Sans dépliage, une valeur longue arrive coupée et sa
        // seconde moitié ressemble à une propriété inconnue.
        let text = "SUMMARY:Réunion de suivi du projet\r\n  avec les équipes\r\nLOCATION:Paris\r\n";
        let lines = unfold(text);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[0],
            "SUMMARY:Réunion de suivi du projet avec les équipes"
        );
        assert_eq!(lines[1], "LOCATION:Paris");
    }

    #[test]
    fn the_fold_does_not_insert_a_space_that_nobody_wrote() {
        // Le pli est **dans** la valeur : le producteur coupe où il veut, y compris au milieu
        // d'un mot. Ajouter une espace au recollage inventerait un caractère.
        let lines = unfold("SUMMARY:Rendez-vous impor\r\n tant\r\n");
        assert_eq!(lines[0], "SUMMARY:Rendez-vous important");
    }

    #[test]
    fn the_fold_removes_exactly_one_blank_and_not_the_value_s_own() {
        // **Le bogue que le premier jet avait.** Un pli placé juste avant une espace de la
        // valeur s'écrit « \r\n  mot » : le premier blanc est le pli, le second appartient au
        // texte. Retirer tous les blancs de tête rendait « projetavec ».
        assert_eq!(unfold("A:un\r\n  deux")[0], "A:un deux");
        assert_eq!(
            unfold("A:un\r\n\tdeux")[0],
            "A:undeux",
            "une tabulation est un pli"
        );
        assert_eq!(unfold("A:un\r\n \tdeux")[0], "A:un\tdeux");
        // Une continuation en tête de fichier ne prolonge rien : jetée plutôt que promue.
        assert_eq!(unfold(" orpheline\r\nA:b"), vec!["A:b".to_owned()]);
    }

    #[test]
    fn every_line_ending_of_the_real_world_is_accepted() {
        // La RFC impose `\r\n`. Le corpus contient les trois formes, et refuser les deux autres
        // serait refuser de lire un fichier lisible.
        for text in [
            "BEGIN:VEVENT\r\nSUMMARY:x\r\nEND:VEVENT",
            "BEGIN:VEVENT\nSUMMARY:x\nEND:VEVENT",
            "BEGIN:VEVENT\rSUMMARY:x\rEND:VEVENT",
        ] {
            let lines = unfold(text);
            assert_eq!(lines.len(), 3, "{text:?} → {lines:?}");
            assert_eq!(lines[1], "SUMMARY:x");
        }
    }

    #[test]
    fn the_line_count_is_bounded() {
        // Un agenda entier exporté fait des centaines de milliers de lignes. Un lecteur qui les
        // déplierait toutes mangerait la mémoire pour afficher un rendez-vous.
        let huge = "X-A:1\r\n".repeat(MAX_LINES * 2);
        assert_eq!(unfold(&huge).len(), MAX_LINES);
    }

    #[test]
    fn the_value_separator_is_the_first_colon_outside_quotes() {
        // **Le piège qui coûte un organisateur.** Un nom entre guillemets peut contenir `:`,
        // et prendre le premier `:` couperait au milieu.
        let it =
            Property::parse(r#"ORGANIZER;CN="Durand: Éloïse":mailto:eloise@exemple.fr"#).unwrap();
        assert_eq!(it.name, "ORGANIZER");
        assert_eq!(it.parameter("CN"), Some("Durand: Éloïse"));
        assert_eq!(it.value, "mailto:eloise@exemple.fr");
    }

    #[test]
    fn a_semicolon_inside_quotes_does_not_split_a_parameter() {
        let it =
            Property::parse(r#"ATTENDEE;CN="Durand; Éloïse";ROLE=REQ-PARTICIPANT:mailto:e@x.fr"#)
                .unwrap();
        assert_eq!(it.parameter("CN"), Some("Durand; Éloïse"));
        assert_eq!(it.parameter("ROLE"), Some("REQ-PARTICIPANT"));
        assert_eq!(it.value, "mailto:e@x.fr");
    }

    #[test]
    fn parameter_names_are_case_insensitive_and_so_are_property_names() {
        let it = Property::parse("dtstart;tzid=Europe/Paris:20260910T140000").unwrap();
        assert!(it.is("DTSTART"));
        assert_eq!(it.parameter("TZID"), Some("Europe/Paris"));
    }

    #[test]
    fn escaped_text_comes_back_readable() {
        // Outlook écrit un lieu multi-ligne ainsi. Sans déséchappement, l'utilisateur lit des
        // `\n` littéraux au milieu d'une adresse.
        let it = Property::parse(r"LOCATION:12 rue des Lilas\n75000 Paris\, France").unwrap();
        assert_eq!(it.text(), "12 rue des Lilas\n75000 Paris, France");

        // Une séquence inconnue rend le caractère qui suit, et une contre-oblique finale reste.
        let it = Property::parse(r"SUMMARY:a\qb\").unwrap();
        assert_eq!(it.text(), "aqb\\");
    }

    #[test]
    fn a_line_without_a_value_is_not_a_property() {
        assert_eq!(Property::parse("SUMMARY"), None);
        assert_eq!(Property::parse(""), None);
        assert_eq!(Property::parse(":sans nom"), None);
    }

    #[test]
    fn malformed_lines_never_panic_and_lose_only_what_they_must() {
        // La règle de tous les analyseurs d'entrée hostile du dépôt : une sortie plus pauvre,
        // jamais une erreur, jamais une panique.
        for hostile in [
            "SUMMARY;=vide:x",
            "SUMMARY;CN=\"non fermé:x",
            "SUMMARY;;;:x",
            "\u{0}\u{1}:x",
            "SUMMARY:",
            "É:accentué",
            "SUMMARY;CN=é\u{fffd}:x",
        ] {
            // Le contrat est de ne pas paniquer ; ce qui sort peut être pauvre.
            let parsed = Property::parse(hostile);
            if let Some(it) = parsed {
                let _ = it.text();
                let _ = it.parameter("CN");
                assert!(!it.name.is_empty(), "{hostile:?}");
            }
        }
    }

    #[test]
    fn an_empty_value_is_kept_because_it_says_something() {
        // `SUMMARY:` avec une valeur vide est un titre vide, ce qui n'est pas la même chose
        // qu'un titre absent : l'un s'affiche « sans titre », l'autre dit qu'il manque.
        let it = Property::parse("SUMMARY:").unwrap();
        assert!(it.is("SUMMARY"));
        assert_eq!(it.value, "");
    }
}

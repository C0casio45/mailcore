//! La source d'un message, telle qu'un humain doit pouvoir la lire.
//!
//! `docs/PHASE-3.md` prend un engagement que rien, jusqu'ici, ne permettait de vérifier :
//! *« Rien de ce qu'on ajoute au message n'est invisible à l'utilisateur. […] Ce que le message
//! porte doit être lisible dans une fenêtre "source du message" qu'on écrira. »* La promesse
//! tenait sur un argument — `mailsmtp::compose` n'écrit qu'une liste courte et fixe d'en-têtes,
//! et `outbox.send` n'a pas de champ `headers` — mais un argument n'est pas une vérification.
//! Ce module rend les octets lisibles ; les deux appels qui s'en servent sont
//! [`crate::Mailbox::source`] pour un message reçu et [`crate::Mailbox::outgoing_source`] pour
//! une ligne de la file d'envoi, qui est la seule à montrer ce que *nous* avons composé.
//!
//! ## La source est du texte, jamais du balisage
//!
//! Rien ici ne passe par `mailhtml`, ne suit une URL, ne décode un `=?UTF-8?B?…?=` et ne
//! déplie un en-tête replié. C'est le sens même d'une source : ce qui est montré est ce qui est
//! sur le fil. Le sujet décodé se lit déjà dans `messages.get` ; s'il fallait choisir entre les
//! deux, c'est la forme brute qui a sa place ici, parce que c'est elle qu'on ne peut lire
//! nulle part ailleurs.
//!
//! ## Un afficheur d'octets hostiles est lui-même du parsing d'entrée hostile
//!
//! Un message est écrit par quelqu'un d'autre. Une source affichée telle quelle dans un
//! terminal laisserait un `ESC[2J` effacer ce qui est au-dessus de lui, et un `\r` en milieu de
//! ligne réécrire par-dessus : une vue dont la raison d'être est « vous voyez tout » se ferait
//! cacher une ligne par un octet. Les caractères de contrôle sont donc rendus **visibles**
//! avant de sortir d'ici, et c'est fait **au point de rendu** — pas chez chacun des trois
//! clients, qui seraient trois occasions d'oublier.
//!
//! Seuls trois caractères de contrôle survivent, parce que chacun porte une structure :
//! le `\n` qui sépare les lignes, la tabulation qui replie légitimement un en-tête, et le `\r`
//! d'un `\r\n` de fin de ligne — qui est la structure elle-même, et qu'afficher mettrait un
//! `\r` au bout de chaque ligne d'un message parfaitement normal. Un `\r` **seul** n'a pas
//! cette excuse : il est échappé, et c'est le contrôle négatif du test.

/// Taille maximale du bloc d'en-têtes rendu.
///
/// Les en-têtes sont ce que la promesse vise, donc le plafond est haut : le plus gros bloc du
/// corpus réel tient en quelques kilooctets. Il existe quand même, parce qu'un message hostile
/// peut n'être **que** des en-têtes.
pub const MAX_HEADERS: usize = 256 * 1024;

/// Taille maximale du corps rendu.
///
/// Le corps d'un message peut faire 48 Mio — mesuré sur le corpus réel — et le pousser dans une
/// réponse JSON pour en montrer les cinquante premières lignes serait la règle 4 du `CLAUDE.md`
/// prise à l'envers. Au-delà, [`Source::body_truncated`] le dit et [`Source::total`] donne la
/// taille réelle.
pub const MAX_BODY: usize = 1024 * 1024;

/// Ce qui est lu du blob avant même de chercher la frontière des en-têtes.
///
/// La borne de lecture est la somme des deux plafonds : c'est le pire cas d'un message dont les
/// en-têtes occupent tout [`MAX_HEADERS`] et dont le corps doit encore être montré jusqu'à
/// [`MAX_BODY`]. Au-delà, les octets ne sont jamais lus — pas lus puis jetés.
pub const MAX_READ: usize = MAX_HEADERS + MAX_BODY;

/// La source d'un message, prête à afficher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Le bloc d'en-têtes, verbatim, replis compris.
    pub headers: String,
    /// Le corps, verbatim, encodages de transfert compris.
    pub body: String,
    /// La taille réelle des octets RFC 5322, avant toute troncature.
    pub total: u64,
    /// Vrai si [`Self::headers`] s'arrête avant la fin du bloc d'en-têtes.
    pub headers_truncated: bool,
    /// Vrai si [`Self::body`] s'arrête avant la fin du corps.
    pub body_truncated: bool,
    /// Nombre de séquences d'octets qui n'étaient pas de l'UTF-8 valide.
    ///
    /// Chacune est rendue par un `U+FFFD`. Un en-tête en ISO-8859-1 non encodé en fait monter
    /// le compte, et c'est une information : la source n'est alors pas montrée à l'octet près.
    pub invalid_sequences: usize,
    /// Nombre de caractères de contrôle rendus visibles.
    ///
    /// Zéro sur l'immense majorité des messages. Non nul, c'est le signe d'un message qui
    /// essayait d'écrire ailleurs que là où on l'affiche.
    pub escaped_controls: usize,
}

/// Rend lisibles les octets d'un message.
///
/// `raw` est ce qui a été lu du magasin — au plus [`MAX_READ`] octets — et `total` la taille
/// réelle du message, qui vient de l'index et non du comptage de `raw` : c'est justement quand
/// `raw` est tronqué qu'on a besoin de savoir de combien.
#[must_use]
pub fn render(raw: &[u8], total: u64) -> Source {
    let (head, rest) = split(raw);

    // La frontière peut manquer pour deux raisons opposées : un message qui n'a pas de corps —
    // légitime, un accusé de réception vide en est un — et un message dont le bloc d'en-têtes
    // dépasse ce qu'on a lu. Seule la seconde est une troncature, et elle se reconnaît à ce que
    // la lecture a buté sur son plafond.
    let unterminated = rest.is_none() && raw.len() >= MAX_READ;
    let body = rest.unwrap_or(b"");

    let read = u64::try_from(raw.len()).unwrap_or(u64::MAX);
    let headers_truncated = unterminated || head.len() > MAX_HEADERS;
    let body_truncated = body.len() > MAX_BODY || read < total;

    let mut invalid = 0;
    let mut escaped = 0;
    let headers = visible(
        &head[..head.len().min(MAX_HEADERS)],
        &mut invalid,
        &mut escaped,
    );
    let body = visible(
        &body[..body.len().min(MAX_BODY)],
        &mut invalid,
        &mut escaped,
    );

    Source {
        headers,
        body,
        total,
        headers_truncated,
        body_truncated,
        invalid_sequences: invalid,
        escaped_controls: escaped,
    }
}

/// Sépare le bloc d'en-têtes du corps sur la première ligne vide.
///
/// Rend `None` pour le corps quand il n'y a pas de ligne vide : l'appelant décide si c'est un
/// message sans corps ou une lecture qui s'est arrêtée trop tôt.
fn split(raw: &[u8]) -> (&[u8], Option<&[u8]>) {
    // Les deux formes sont acceptées : `\r\n\r\n` est la seule conforme, mais un mbox local
    // stocke des fins de ligne nues et le corpus en contient. Refuser la seconde afficherait
    // un message entier comme un bloc d'en-têtes.
    for at in 0..raw.len() {
        if raw[at..].starts_with(b"\r\n\r\n") {
            return (&raw[..at + 2], Some(&raw[at + 4..]));
        }
        if raw[at..].starts_with(b"\n\n") {
            return (&raw[..at + 1], Some(&raw[at + 2..]));
        }
    }
    (raw, None)
}

/// Convertit des octets en texte affichable, en comptant ce qui a dû être remplacé.
fn visible(bytes: &[u8], invalid: &mut usize, escaped: &mut usize) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;

    // La conversion est faite à la main plutôt qu'avec `String::from_utf8_lossy` pour une seule
    // raison : compter. `from_utf8_lossy` remplace sans dire combien de fois, et un message qui
    // contient un vrai `U+FFFD` rendrait faux tout comptage fait après coup sur le résultat.
    loop {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                push(text, &mut out, escaped);
                break;
            }
            Err(error) => {
                let good = error.valid_up_to();
                // `valid_up_to` borne par définition une portion valide : le `unwrap_or_default`
                // est un invariant, pas un repli attendu.
                push(
                    std::str::from_utf8(&rest[..good]).unwrap_or_default(),
                    &mut out,
                    escaped,
                );
                out.push('\u{fffd}');
                *invalid += 1;
                match error.error_len() {
                    Some(bad) => rest = &rest[good + bad..],
                    // Fin d'entrée au milieu d'une séquence : il n'y a plus rien à lire, et
                    // c'est le cas normal d'une coupure à [`MAX_BODY`].
                    None => break,
                }
            }
        }
    }
    out
}

/// Recopie du texte en rendant visibles les caractères de contrôle.
fn push(text: &str, out: &mut String, escaped: &mut usize) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => out.push(c),
            // Le `\r` d'un `\r\n` est la fin de ligne elle-même : le montrer mettrait un `\r`
            // au bout de chaque ligne de tout message conforme. Un `\r` seul est un déplacement
            // de curseur, et il est échappé — c'est le contrôle négatif du test.
            '\r' if chars.peek() == Some(&'\n') => {}
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
                *escaped += 1;
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(raw: &[u8]) -> Source {
        render(raw, raw.len() as u64)
    }

    #[test]
    fn headers_and_body_split_on_the_first_empty_line() {
        let rendered = source(b"From: a@b\r\nSubject: x\r\n\r\ncorps\r\n\r\nsuite\r\n");
        assert_eq!(rendered.headers, "From: a@b\nSubject: x\n");
        assert_eq!(rendered.body, "corps\n\nsuite\n");
        assert!(!rendered.headers_truncated);
        assert!(!rendered.body_truncated);
    }

    #[test]
    fn bare_newlines_split_too_because_a_local_mbox_has_them() {
        let rendered = source(b"From: a@b\nSubject: x\n\ncorps\n");
        assert_eq!(rendered.headers, "From: a@b\nSubject: x\n");
        assert_eq!(rendered.body, "corps\n");
    }

    #[test]
    fn a_folded_header_keeps_its_fold_and_its_tab() {
        let rendered = source(b"Subject: un sujet\r\n\tqui continue\r\n\r\n");
        assert_eq!(rendered.headers, "Subject: un sujet\n\tqui continue\n");
        assert_eq!(rendered.escaped_controls, 0);
    }

    #[test]
    fn an_encoded_word_is_shown_as_written_and_not_decoded() {
        // Une source décodée ne serait plus une source : le sujet lisible est déjà dans
        // `messages.get`, celui-ci est le seul endroit où voir ce qui part vraiment.
        let rendered = source(b"Subject: =?UTF-8?B?w6l0w6k=?=\r\n\r\n");
        assert!(rendered.headers.contains("=?UTF-8?B?w6l0w6k=?="));
        assert!(!rendered.headers.contains("été"));
    }

    #[test]
    fn a_message_without_an_empty_line_is_all_headers_and_is_not_called_truncated() {
        let rendered = source(b"From: a@b\r\n");
        assert_eq!(rendered.headers, "From: a@b\n");
        assert_eq!(rendered.body, "");
        assert!(!rendered.headers_truncated);
    }

    #[test]
    fn an_escape_sequence_cannot_reach_the_terminal() {
        // La panne que cette vue doit à tout prix éviter : un message qui efface l'écran sur
        // lequel on l'affiche, donc qui cache les en-têtes qu'on venait vérifier.
        let rendered = source(b"X-Evil: \x1b[2J\x1b[H\r\n\r\n");
        assert!(!rendered.headers.contains('\u{1b}'));
        assert!(rendered.headers.contains("\\x1b[2J"));
        assert_eq!(rendered.escaped_controls, 2);
    }

    #[test]
    fn a_line_ending_carriage_return_is_not_shown() {
        let rendered = source(b"From: a@b\r\nTo: c@d\r\n\r\n");
        assert!(!rendered.headers.contains("\\x0d"));
        assert_eq!(rendered.escaped_controls, 0);
    }

    #[test]
    fn a_lone_carriage_return_is_shown() {
        // Le contrôle négatif du test précédent. Un `\r` sans `\n` réécrit la ligne courante :
        // c'est la façon la plus discrète de cacher un en-tête dans un affichage naïf.
        let rendered = source(b"X-Evil: visible\rcache\r\n\r\n");
        assert!(rendered.headers.contains("visible\\x0dcache"));
        assert_eq!(rendered.escaped_controls, 1);
    }

    #[test]
    fn a_null_byte_and_a_delete_are_shown() {
        let rendered = source(b"X-Evil: \x00\x7f\r\n\r\n");
        assert!(rendered.headers.contains("\\x00\\x7f"));
        assert_eq!(rendered.escaped_controls, 2);
    }

    #[test]
    fn invalid_utf8_is_replaced_and_counted() {
        // Un en-tête en ISO-8859-1 non encodé : le corpus en contient, et la source doit le
        // dire plutôt que de prétendre montrer les octets tels quels.
        let rendered = source(b"Subject: caf\xe9 et cr\xe8me\r\n\r\n");
        assert_eq!(rendered.invalid_sequences, 2);
        assert!(rendered.headers.contains('\u{fffd}'));
    }

    #[test]
    fn a_real_replacement_character_is_not_counted_as_invalid() {
        let rendered = source("Subject: \u{fffd}\r\n\r\n".as_bytes());
        assert_eq!(rendered.invalid_sequences, 0);
    }

    #[test]
    fn a_body_longer_than_the_cap_is_cut_and_says_so() {
        let mut raw = b"From: a@b\r\n\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'x', MAX_BODY + 10));
        let rendered = source(&raw);
        assert!(rendered.body_truncated);
        assert_eq!(rendered.body.len(), MAX_BODY);
        assert!(!rendered.headers_truncated);
    }

    #[test]
    fn a_body_cut_in_the_middle_of_a_character_does_not_lose_the_rest() {
        let mut raw = b"From: a@b\r\n\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'x', MAX_BODY - 1));
        raw.extend("é".as_bytes());
        let rendered = source(&raw);
        // La coupure tombe au milieu du « é » : un `U+FFFD` compté, et surtout pas une panique.
        assert_eq!(rendered.invalid_sequences, 1);
        assert!(rendered.body_truncated);
    }

    #[test]
    fn a_header_block_that_never_ends_is_a_truncation_and_not_a_message_without_a_body() {
        // Le cas hostile : pas de ligne vide, et plus d'octets que la lecture n'en prend. Sans
        // la distinction, la vue dirait « ce message n'a pas de corps » d'un message qui en a un.
        let raw = vec![b'x'; MAX_READ];
        let rendered = render(&raw, MAX_READ as u64 + 1);
        assert!(rendered.headers_truncated);
        assert_eq!(rendered.headers.len(), MAX_HEADERS);
    }

    #[test]
    fn a_truncated_read_says_the_body_is_truncated_even_when_it_fits_the_cap() {
        // Ce que l'appelant a lu s'arrête à la borne, mais le message fait le double : le corps
        // rendu tient dans son plafond et n'en est pas moins incomplet.
        let mut raw = b"From: a@b\r\n\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'x', 1000));
        let rendered = render(&raw, 10_000);
        assert!(rendered.body_truncated);
        assert_eq!(rendered.total, 10_000);
    }

    #[test]
    fn an_empty_message_renders_empty_and_does_not_panic() {
        let rendered = source(b"");
        assert_eq!(rendered.headers, "");
        assert_eq!(rendered.body, "");
        assert!(!rendered.headers_truncated);
        assert!(!rendered.body_truncated);
    }
}

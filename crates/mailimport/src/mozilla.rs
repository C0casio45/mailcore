//! En-têtes propriétaires Thunderbird.
//!
//! Trois en-têtes, écrits par le client dans le fichier mbox lui-même :
//!
//! - `X-Mozilla-Status: XXXX` — quatre chiffres hexadécimaux de drapeaux.
//! - `X-Mozilla-Status2: XXXXXXXX` — huit chiffres, la suite des drapeaux.
//! - `X-Mozilla-Keys: ...` — étiquettes utilisateur, sur une ligne rembourrée d'espaces
//!   pour pouvoir être réécrite sur place sans décaler le fichier.
//!
//! ## Deux raisons de s'en occuper, et elles vont dans des directions opposées
//!
//! **On les lit**, parce que `X-Mozilla-Status` bit `0x0008` marque un message supprimé
//! mais pas encore compacté. C'est exactement l'espace mort qui gonfle les 11 Go décrits
//! dans `docs/VISION.md`. On l'ignore à l'import — mais on le compte, pour pouvoir dire
//! combien de vide a été laissé derrière.
//!
//! **On les retire avant de hacher**, parce qu'ils sont locaux à Thunderbird et varient
//! d'un dossier à l'autre pour un contenu identique : le même message est « lu » dans
//! `INBOX` et « non lu » dans `[Gmail]/Tous les messages`. Les garder ferait deux blobs
//! distincts d'un seul contenu, et la dédup ne servirait plus à rien.
//!
//! Ce n'est donc pas une perte d'information : ces octets ne sont pas du message, ce sont
//! des métadonnées du client qui l'a stocké. Elles remontent dans `refs.flags`, à leur
//! place — attachées à la référence, pas au contenu.

/// Les drapeaux de `X-Mozilla-Status`.
///
/// Valeurs issues de `nsMsgMessageFlags` dans le code de Mozilla. Seules celles qui nous
/// servent sont nommées ; les autres sont conservées telles quelles dans
/// [`MozillaHeaders::status`] plutôt que perdues.
pub mod status {
    /// Lu.
    pub const READ: u16 = 0x0001;
    /// Répondu.
    pub const REPLIED: u16 = 0x0002;
    /// Marqué (étoile).
    pub const MARKED: u16 = 0x0004;
    /// **Supprimé, pas encore compacté.** L'espace mort. Ignoré à l'import.
    pub const EXPUNGED: u16 = 0x0008;
    /// Le sujet commençait par `Re:` — cache de threading de Thunderbird.
    pub const HAS_RE: u16 = 0x0010;
    /// Fil replié dans l'affichage.
    pub const ELIDED: u16 = 0x0020;
    /// Corps disponible hors ligne.
    pub const OFFLINE: u16 = 0x0080;
    /// Fil surveillé.
    pub const WATCHED: u16 = 0x0100;
    /// Message partiellement téléchargé — l'en-tête sans le corps.
    pub const PARTIAL: u16 = 0x0400;
    /// Transféré.
    pub const FORWARDED: u16 = 0x1000;
}

/// Ce qu'on a trouvé — et retiré — dans les en-têtes d'un message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MozillaHeaders {
    /// La valeur de `X-Mozilla-Status`, ou zéro si l'en-tête était absent ou illisible.
    pub status: u16,
    /// La valeur de `X-Mozilla-Status2`.
    pub status2: u32,
    /// Les étiquettes de `X-Mozilla-Keys`, espaces de remplissage retirés.
    pub keys: Vec<String>,
    /// Nombre de lignes retirées du message.
    pub stripped_lines: usize,
    /// Nombre d'octets retirés du message.
    pub stripped_bytes: usize,
}

impl MozillaHeaders {
    /// Vrai si le message est marqué supprimé mais pas encore compacté.
    #[must_use]
    pub const fn is_expunged(&self) -> bool {
        self.status & status::EXPUNGED != 0
    }

    /// Vrai si le message est marqué lu.
    #[must_use]
    pub const fn is_read(&self) -> bool {
        self.status & status::READ != 0
    }

    /// Les drapeaux traduits vers le modèle de `mailcore`.
    ///
    /// `EXPUNGED` n'est pas traduit : un message supprimé n'est pas importé du tout, donc
    /// il n'a pas de référence à porter le drapeau.
    #[must_use]
    pub fn to_flags(&self) -> mailcore::MessageFlags {
        let mut flags = mailcore::MessageFlags::empty();
        if self.status & status::READ != 0 {
            flags = flags.union(mailcore::MessageFlags::SEEN);
        }
        if self.status & status::REPLIED != 0 {
            flags = flags.union(mailcore::MessageFlags::ANSWERED);
        }
        if self.status & status::MARKED != 0 {
            flags = flags.union(mailcore::MessageFlags::FLAGGED);
        }
        flags
    }
}

/// Retire les en-têtes `X-Mozilla-*` de `message`, en place, et rend ce qu'ils disaient.
///
/// En place et non par copie : à raison d'un message par itération sur un corpus de
/// plusieurs centaines de milliers, une allocation supplémentaire par message serait un
/// coût pur. Le tampon appartient déjà à l'appelant et se réutilise.
///
/// Ne touche qu'au bloc d'en-têtes — la première ligne vide arrête le traitement. Un
/// `X-Mozilla-Status:` en début de ligne dans un corps de message, ou dans une partie MIME
/// imbriquée, est du contenu et reste intact.
#[must_use]
pub fn strip_in_place(message: &mut Vec<u8>) -> MozillaHeaders {
    let mut found = MozillaHeaders::default();
    let header_end = header_block_end(message);

    // Écriture compactante : `write` avance moins vite que `read` dès qu'une ligne est
    // retirée. Un seul passage, aucune allocation.
    let mut write = 0usize;
    let mut read = 0usize;

    while read < header_end {
        let line_end = line_end_at(message, read);
        let line_len = line_end - read;

        // Classer d'abord, agir ensuite : la reconnaissance emprunte le tampon en lecture,
        // le compactage l'emprunte en écriture. Les deux ne peuvent pas cohabiter.
        let kind = classify(&message[read..line_end]);

        match kind {
            Line::Keep => {
                // `copy_within` et non une copie de tampon à tampon : on décale vers la
                // gauche dans le même tampon, les plages ne se chevauchent pas vers l'avant.
                if write != read {
                    message.copy_within(read..line_end, write);
                }
                write += line_len;
            }
            stripped => {
                match stripped {
                    Line::Status(value) => found.status = value,
                    Line::Status2(value) => found.status2 = value,
                    Line::Keys(keys) => found.keys = keys,
                    Line::Keep => unreachable!("traité par le bras précédent"),
                }
                found.stripped_lines += 1;
                found.stripped_bytes += line_len;
            }
        }
        read = line_end;
    }

    if write != read {
        // Le reste du message — le corps — glisse d'un bloc.
        let tail = message.len() - read;
        message.copy_within(read.., write);
        message.truncate(write + tail);
    }

    found
}

/// Ce qu'une ligne du bloc d'en-têtes s'avère être.
#[derive(Debug)]
enum Line {
    /// `X-Mozilla-Status`, décodé.
    Status(u16),
    /// `X-Mozilla-Status2`, décodé.
    Status2(u32),
    /// `X-Mozilla-Keys`, découpé.
    Keys(Vec<String>),
    /// Tout le reste : appartient au message.
    Keep,
}

/// Reconnaît une ligne d'en-tête.
///
/// `Status2` est testé **avant** `Status` : son nom contient celui de l'autre en préfixe,
/// donc l'ordre inverse classerait `X-Mozilla-Status2` comme un `X-Mozilla-Status` dont la
/// valeur commencerait par `2`.
fn classify(line: &[u8]) -> Line {
    if let Some(value) = header_value(line, b"x-mozilla-status2:") {
        Line::Status2(parse_hex_u32(value))
    } else if let Some(value) = header_value(line, b"x-mozilla-status:") {
        Line::Status(parse_hex_u16(value))
    } else if let Some(value) = header_value(line, b"x-mozilla-keys:") {
        Line::Keys(parse_keys(value))
    } else {
        Line::Keep
    }
}

/// L'offset de la fin du bloc d'en-têtes : juste après la première ligne vide.
///
/// Si le message n'a pas de ligne vide — message tronqué, ou en-têtes seuls — tout le
/// message est considéré comme du bloc d'en-têtes. C'est le comportement sûr : on ne veut
/// pas retirer un `X-Mozilla-Status` qui serait en réalité dans un corps.
fn header_block_end(message: &[u8]) -> usize {
    let mut pos = 0usize;
    while pos < message.len() {
        let end = line_end_at(message, pos);
        if is_blank_line(&message[pos..end]) {
            return pos;
        }
        pos = end;
    }
    message.len()
}

/// L'offset juste après le terminateur de la ligne qui commence à `start`.
fn line_end_at(message: &[u8], start: usize) -> usize {
    match message[start..].iter().position(|&b| b == b'\n') {
        Some(rel) => start + rel + 1,
        None => message.len(),
    }
}

fn is_blank_line(line: &[u8]) -> bool {
    matches!(line, b"\n" | b"\r\n" | b"")
}

/// La valeur d'un en-tête si la ligne porte ce nom, comparaison insensible à la casse.
///
/// Les noms d'en-têtes sont insensibles à la casse (RFC 5322) et Thunderbird n'est pas seul
/// à écrire dans ces fichiers : un `x-mozilla-status:` en minuscules doit être reconnu.
fn header_value<'a>(line: &'a [u8], name_lower: &[u8]) -> Option<&'a [u8]> {
    if line.len() < name_lower.len() {
        return None;
    }
    let (name, value) = line.split_at(name_lower.len());
    if name.eq_ignore_ascii_case(name_lower) {
        Some(value)
    } else {
        None
    }
}

/// Décode une valeur hexadécimale, en s'arrêtant au premier caractère non hexadécimal.
///
/// Tolérante par conception : un en-tête tronqué, rembourré d'espaces ou vide rend zéro
/// plutôt que d'interrompre l'import. Un drapeau perdu coûte un message marqué non lu ;
/// un import interrompu coûte le corpus.
fn parse_hex_u16(value: &[u8]) -> u16 {
    u16::try_from(parse_hex(value, 4)).unwrap_or(0)
}

fn parse_hex_u32(value: &[u8]) -> u32 {
    parse_hex(value, 8)
}

fn parse_hex(value: &[u8], max_digits: usize) -> u32 {
    let mut out: u32 = 0;
    let mut digits = 0usize;
    for &byte in value {
        let Some(digit) = (byte as char).to_digit(16) else {
            if byte == b' ' || byte == b'\t' {
                if digits == 0 {
                    continue;
                }
                break;
            }
            break;
        };
        out = out.saturating_mul(16).saturating_add(digit);
        digits += 1;
        if digits == max_digits {
            break;
        }
    }
    out
}

/// Découpe `X-Mozilla-Keys` en étiquettes, en jetant le remplissage.
fn parse_keys(value: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(value)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn strip(input: &[u8]) -> (MozillaHeaders, String) {
        let mut buf = input.to_vec();
        let found = strip_in_place(&mut buf);
        (found, String::from_utf8_lossy(&buf).into_owned())
    }

    // ---------------------------------------------------------------- cas nominaux

    #[test]
    fn reads_and_removes_status() {
        let (found, rest) = strip(b"X-Mozilla-Status: 0001\r\nSubject: un\r\n\r\ncorps\r\n");
        assert_eq!(found.status, 1);
        assert!(found.is_read());
        assert_eq!(rest, "Subject: un\r\n\r\ncorps\r\n");
    }

    #[test]
    fn reads_status2_without_confusing_it_with_status() {
        // `X-Mozilla-Status2` commence par le nom de `X-Mozilla-Status` : l'ordre des tests
        // compte, et ce test est là pour qu'il ne se réinverse pas.
        let (found, rest) = strip(b"X-Mozilla-Status2: 00800000\nX-Mozilla-Status: 0009\nA: b\n");
        assert_eq!(found.status, 0x0009);
        assert_eq!(found.status2, 0x0080_0000);
        assert_eq!(rest, "A: b\n");
    }

    #[test]
    fn detects_the_expunged_bit() {
        let (found, _) = strip(b"X-Mozilla-Status: 0009\nSubject: un\n\n");
        assert!(found.is_expunged(), "le bit 0x0008 n'a pas été vu");
        assert!(found.is_read(), "les autres bits ont été perdus");
    }

    #[test]
    fn a_message_without_the_expunged_bit_is_kept() {
        let (found, _) = strip(b"X-Mozilla-Status: 0001\nSubject: un\n\n");
        assert!(!found.is_expunged());
    }

    #[test]
    fn parses_keys_dropping_the_padding() {
        let (found, rest) = strip(b"X-Mozilla-Keys: important travail          \nA: b\n\n");
        assert_eq!(found.keys, vec!["important", "travail"]);
        assert_eq!(rest, "A: b\n\n");
    }

    #[test]
    fn removes_all_three_headers_at_once() {
        let input = b"X-Mozilla-Status: 0001\n\
                      X-Mozilla-Status2: 00000000\n\
                      X-Mozilla-Keys:      \n\
                      From: a@b\n\
                      Subject: un\n\
                      \n\
                      corps\n";
        let (found, rest) = strip(input);
        assert_eq!(found.stripped_lines, 3);
        assert_eq!(rest, "From: a@b\nSubject: un\n\ncorps\n");
        assert!(found.keys.is_empty());
    }

    #[test]
    fn reports_the_bytes_it_removed() {
        let (found, rest) = strip(b"X-Mozilla-Status: 0001\nA: b\n\n");
        assert_eq!(found.stripped_bytes, "X-Mozilla-Status: 0001\n".len());
        assert_eq!(rest.len(), "A: b\n\n".len());
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let (found, rest) = strip(b"x-MOZILLA-status: 0004\nA: b\n\n");
        assert_eq!(found.status, status::MARKED);
        assert_eq!(rest, "A: b\n\n");
    }

    // ---------------------------------------------- ce qu'il ne faut surtout pas toucher

    #[test]
    fn leaves_the_body_alone() {
        // Le cas qui compte : la même chaîne, mais dans le corps. C'est du contenu.
        let input = b"Subject: un\n\nX-Mozilla-Status: 0009\nvoila du corps\n";
        let (found, rest) = strip(input);

        assert_eq!(found.status, 0, "un en-tête du corps a été interprété");
        assert_eq!(found.stripped_lines, 0);
        assert_eq!(
            rest,
            "Subject: un\n\nX-Mozilla-Status: 0009\nvoila du corps\n"
        );
    }

    #[test]
    fn leaves_a_nested_mime_part_alone() {
        let input = b"Subject: un\n\
                      \n\
                      --limite\n\
                      Content-Type: message/rfc822\n\
                      \n\
                      X-Mozilla-Status: 0001\n\
                      Subject: message joint\n";
        let (found, rest) = strip(input);
        assert_eq!(found.stripped_lines, 0);
        assert!(rest.contains("X-Mozilla-Status: 0001"));
    }

    #[test]
    fn a_message_with_no_mozilla_headers_is_returned_untouched() {
        let input = b"From: a@b\nSubject: un\n\ncorps\n";
        let (found, rest) = strip(input);
        assert_eq!(found, MozillaHeaders::default());
        assert_eq!(rest, String::from_utf8_lossy(input));
    }

    #[test]
    fn a_similar_but_different_header_is_kept() {
        let input = b"X-Mozilla-Status-Extra: 0001\nX-Mozilla: rien\nA: b\n\n";
        let (found, rest) = strip(input);
        assert_eq!(found.stripped_lines, 0);
        assert_eq!(rest, String::from_utf8_lossy(input));
    }

    // ---------------------------------------------------------------- entrée hostile

    #[test]
    fn an_empty_message_does_not_panic() {
        let (found, rest) = strip(b"");
        assert_eq!(found, MozillaHeaders::default());
        assert!(rest.is_empty());
    }

    #[test]
    fn a_headers_only_message_does_not_panic() {
        let (found, rest) = strip(b"X-Mozilla-Status: 0001\n");
        assert_eq!(found.status, 1);
        assert!(rest.is_empty());
    }

    #[test]
    fn a_truncated_header_line_does_not_panic() {
        // Fichier coupé au milieu de l'en-tête, sans terminateur.
        let (found, rest) = strip(b"X-Mozilla-Status: 00");
        assert_eq!(found.status, 0);
        assert!(rest.is_empty());
    }

    #[test]
    fn a_malformed_hex_value_yields_zero_instead_of_failing() {
        for bad in [
            &b"X-Mozilla-Status: zzzz\nA: b\n\n"[..],
            &b"X-Mozilla-Status:\nA: b\n\n"[..],
            &b"X-Mozilla-Status:      \nA: b\n\n"[..],
            &b"X-Mozilla-Status: ----\nA: b\n\n"[..],
        ] {
            let (found, rest) = strip(bad);
            assert_eq!(found.status, 0, "sur {:?}", String::from_utf8_lossy(bad));
            assert_eq!(rest, "A: b\n\n");
        }
    }

    #[test]
    fn an_oversized_hex_value_is_truncated_not_overflowed() {
        // Plus de quatre chiffres : on prend les quatre premiers, sans déborder.
        let (found, _) = strip(b"X-Mozilla-Status: FFFFFFFFFFFF\nA: b\n\n");
        assert_eq!(found.status, 0xFFFF);
    }

    #[test]
    fn a_non_utf8_keys_line_does_not_panic() {
        let mut input = b"X-Mozilla-Keys: ".to_vec();
        input.extend_from_slice(&[0xFF, 0xFE, b' ', b'o', b'k']);
        input.extend_from_slice(b"\nA: b\n\n");

        let mut buf = input;
        let found = strip_in_place(&mut buf);
        assert!(found.keys.iter().any(|k| k == "ok"));
    }

    #[test]
    fn a_status_header_with_no_space_after_the_colon_is_read() {
        let (found, _) = strip(b"X-Mozilla-Status:0001\nA: b\n\n");
        assert_eq!(found.status, 1);
    }

    // ---------------------------------------------------- l'effet sur la déduplication

    #[test]
    fn the_same_message_stored_twice_with_different_flags_hashes_identically() {
        // La raison d'être de tout ce module. Le même mail dans INBOX (lu) et dans
        // [Gmail]/Tous les messages (non lu, autre étiquette) doit donner un seul blob.
        let mut inbox = b"X-Mozilla-Status: 0001\n\
                          X-Mozilla-Status2: 00000000\n\
                          X-Mozilla-Keys: important   \n\
                          From: plombier@exemple.fr\n\
                          Subject: facture\n\
                          \n\
                          Voici la facture.\n"
            .to_vec();
        let mut archive = b"X-Mozilla-Status: 0000\n\
                            X-Mozilla-Status2: 00800000\n\
                            X-Mozilla-Keys:             \n\
                            From: plombier@exemple.fr\n\
                            Subject: facture\n\
                            \n\
                            Voici la facture.\n"
            .to_vec();

        let flags_inbox = strip_in_place(&mut inbox);
        let flags_archive = strip_in_place(&mut archive);

        assert_eq!(inbox, archive, "les octets stockés diffèrent");
        assert_eq!(
            mailcore::BlobHash::of(&inbox),
            mailcore::BlobHash::of(&archive),
            "un seul contenu donnerait deux blobs"
        );

        // Et les drapeaux, eux, restent distincts : ils appartiennent à la référence.
        assert!(flags_inbox.is_read());
        assert!(!flags_archive.is_read());
        assert_ne!(flags_inbox.keys, flags_archive.keys);
    }

    #[test]
    fn translates_flags_to_the_core_model() {
        let (found, _) = strip(b"X-Mozilla-Status: 0007\nA: b\n\n");
        let flags = found.to_flags();

        assert!(flags.contains(mailcore::MessageFlags::SEEN));
        assert!(flags.contains(mailcore::MessageFlags::ANSWERED));
        assert!(flags.contains(mailcore::MessageFlags::FLAGGED));
        assert!(!flags.contains(mailcore::MessageFlags::DRAFT));
    }

    #[test]
    fn the_expunged_bit_is_not_translated_to_a_reference_flag() {
        // Un message supprimé n'est pas importé, donc il n'a pas de référence à porter le
        // drapeau. Le traduire quand même serait un contresens.
        let (found, _) = strip(b"X-Mozilla-Status: 0008\nA: b\n\n");
        assert!(found.is_expunged());
        assert_eq!(found.to_flags(), mailcore::MessageFlags::empty());
    }
}

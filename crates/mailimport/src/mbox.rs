//! Lecteur mbox streamé.
//!
//! Rend les messages un par un sans jamais matérialiser le fichier. Le tampon de sortie
//! appartient à l'appelant et se réutilise d'un message au suivant : c'est ce qui fait
//! tenir le critère 3 (moins de 500 Mo de RSS sur 11 Go de corpus).
//!
//! ## Ce qu'est un séparateur, exactement
//!
//! Une ligne qui commence par les cinq octets `From `. Rien de plus.
//!
//! J'avais d'abord exigé qu'elle soit **précédée d'une ligne vide**, pour qu'une ligne de
//! corps commençant par `From ` ne coupe pas un message en deux. Le corpus réel a réfuté
//! l'heuristique : Thunderbird écrit parfois le séparateur juste après la frontière de
//! clôture d'un message MIME, sans ligne vide.
//!
//! ```text
//! ------=_NextPart_000_1271_01D9AA72.1A64FD40--\r\n
//! From - Tue Oct 17 17:34:18 2023\r\n
//! ```
//!
//! Le coût de l'erreur n'était pas symétrique : au lieu de couper un message de trop, la
//! condition en fusionnait 234 Mio en un seul, jusqu'à faire sauter le plafond de taille.
//!
//! Ce qui protège réellement des faux positifs n'est pas la ligne vide, c'est le
//! *From-mangling* : l'écrivain échappe les lignes de corps qui commencent par `From `, donc
//! celles qui restent sont des séparateurs. C'est pour ça que tous les lecteurs mbox sérieux
//! s'en tiennent à `From `, et c'est ce qu'on fait ici.
//!
//! Le risque résiduel est un fichier écrit par un outil qui n'échappe pas. Il se détecte à
//! la sonde (`cargo xtask profile-probe`) avant l'import, pas après.
//!
//! ## From-mangling
//!
//! Convention `mboxrd`, la seule réversible : une ligne de corps `>+From ` perd un `>`.
//! Donc `>From ` redevient `From `, et `>>From ` redevient `>From `.
//!
//! Le piège : si le fichier a été écrit en `mboxo` (qui n'échappe que `From `, pas
//! `>From `), dés-échapper en `mboxrd` transforme un `>From ` d'origine en `From `, ce qui
//! altère le contenu. Les deux conventions sont indistinguables sur un fichier qui ne
//! contient jamais `>>From `. La présence d'au moins un `>>From ` prouve en revanche que
//! l'écrivain est `mboxrd`, puisque `mboxo` ne peut pas en produire — c'est vérifiable sur
//! le corpus réel, en lecture seule, avant l'import. [`Mangling`] permet de choisir.

use std::io::BufRead;

use crate::error::{Error, Result};

/// Plafond par défaut de la taille d'un message.
///
/// 128 Mio : très au-delà du plus gros mail plausible (les serveurs plafonnent la pièce
/// jointe autour de 25 Mo, soit ~35 Mo encodés en base64), et très en dessous du critère 3.
/// Franchir ce plafond ne veut pas dire « gros message », ça veut dire « séparateur
/// manqué ».
pub const DEFAULT_MESSAGE_LIMIT: usize = 128 * 1024 * 1024;

/// La convention de dés-échappement à appliquer aux lignes de corps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mangling {
    /// `>+From ` perd un `>`. Réversible. Le défaut.
    #[default]
    MboxRd,
    /// Seul `>From ` redevient `From `. `>>From ` est laissé tel quel.
    MboxO,
    /// Aucun dés-échappement. Pour un fichier dont on sait que l'écrivain n'échappait pas.
    None,
}

/// Ce qu'on sait d'un message une fois lu.
#[derive(Debug, Clone)]
pub struct MessageMeta {
    /// Offset, en octets, de la ligne `From ` qui a ouvert ce message.
    ///
    /// Sert à retrouver un message dans le fichier source pour le déboguer, sans avoir à
    /// relire depuis le début.
    pub offset: u64,
    /// La ligne `From ` elle-même, terminateur retiré, décodée en UTF-8 avec remplacement.
    ///
    /// Décodée avec pertes sciemment : cette ligne ne sert qu'au diagnostic, elle ne fait
    /// pas partie du message et n'entre pas dans le hash.
    pub envelope: String,
    /// Taille du message écrit dans le tampon de sortie.
    pub len: usize,
}

/// Ce que la lecture a rencontré en chemin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaderStats {
    /// Messages rendus.
    pub messages: u64,
    /// Octets ignorés avant le premier séparateur.
    ///
    /// Non nul veut dire que le fichier ne commence pas par un `From ` : soit ce n'est pas
    /// un mbox, soit il a été tronqué par la tête. À journaliser, pas à ignorer.
    pub leading_garbage: u64,
    /// Lignes de corps dés-échappées.
    pub unmangled_lines: u64,
}

/// Lecteur mbox sur n'importe quel flux tamponné.
#[derive(Debug)]
pub struct MboxReader<R> {
    input: R,
    /// Tampon de ligne, réutilisé. Une allocation par ligne sur 11 Go serait absurde.
    line: Vec<u8>,
    /// Le séparateur déjà lu qui ouvre le prochain message.
    pending: Option<Separator>,
    /// Position de lecture dans le flux.
    pos: u64,
    /// Faux tant que le premier séparateur n'a pas été cherché.
    primed: bool,
    /// Vrai quand le flux est épuisé.
    exhausted: bool,
    limit: usize,
    mangling: Mangling,
    stats: ReaderStats,
}

#[derive(Debug)]
struct Separator {
    offset: u64,
    line: Vec<u8>,
}

impl<R: BufRead> MboxReader<R> {
    /// Un lecteur avec les réglages par défaut : `mboxrd`, plafond à
    /// [`DEFAULT_MESSAGE_LIMIT`].
    pub fn new(input: R) -> Self {
        Self {
            input,
            line: Vec::with_capacity(4096),
            pending: None,
            pos: 0,
            primed: false,
            exhausted: false,
            limit: DEFAULT_MESSAGE_LIMIT,
            mangling: Mangling::default(),
            stats: ReaderStats::default(),
        }
    }

    /// Change la convention de dés-échappement.
    #[must_use]
    pub fn with_mangling(mut self, mangling: Mangling) -> Self {
        self.mangling = mangling;
        self
    }

    /// Change le plafond de taille d'un message.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Ce que la lecture a rencontré jusqu'ici.
    #[must_use]
    pub fn stats(&self) -> ReaderStats {
        self.stats
    }

    /// Lit le prochain message dans `out`.
    ///
    /// `out` est vidé puis rempli. Le réutiliser d'un appel au suivant est le mode d'emploi
    /// normal — c'est ce qui évite une allocation par message.
    ///
    /// Rend `None` quand le flux est épuisé.
    ///
    /// # Errors
    ///
    /// [`Error::Stream`] sur échec de lecture, [`Error::MessageTooLarge`] si un message
    /// dépasse le plafond — ce qui, en pratique, signale un séparateur manqué.
    pub fn read_message_into(&mut self, out: &mut Vec<u8>) -> Result<Option<MessageMeta>> {
        out.clear();
        if self.exhausted {
            return Ok(None);
        }

        if !self.primed {
            self.prime()?;
        }

        let Some(separator) = self.pending.take() else {
            self.exhausted = true;
            return Ok(None);
        };

        let offset = separator.offset;
        let envelope = trim_terminator(&separator.line);
        let envelope = String::from_utf8_lossy(envelope).into_owned();

        while let Some(line_offset) = self.read_line()? {
            if is_separator(&self.line) {
                self.pending = Some(Separator {
                    offset: line_offset,
                    line: self.line.clone(),
                });
                break;
            }
            if unmangle_into(out, &self.line, self.mangling) {
                self.stats.unmangled_lines += 1;
            }

            if out.len() > self.limit {
                return Err(Error::MessageTooLarge {
                    offset,
                    limit: self.limit,
                });
            }
        }

        if self.pending.is_none() {
            self.exhausted = true;
        }

        strip_trailing_blank_line(out);
        self.stats.messages += 1;

        Ok(Some(MessageMeta {
            offset,
            envelope,
            len: out.len(),
        }))
    }

    /// Cherche le premier séparateur, en comptant ce qui le précède.
    fn prime(&mut self) -> Result<()> {
        self.primed = true;

        while let Some(line_offset) = self.read_line()? {
            if is_separator(&self.line) {
                self.pending = Some(Separator {
                    offset: line_offset,
                    line: self.line.clone(),
                });
                return Ok(());
            }
            self.stats.leading_garbage += self.line.len() as u64;
        }
        Ok(())
    }

    /// Lit une ligne dans [`Self::line`], terminateur inclus. Rend l'offset de son début.
    fn read_line(&mut self) -> Result<Option<u64>> {
        self.line.clear();
        let start = self.pos;
        let read = self
            .input
            .read_until(b'\n', &mut self.line)
            .map_err(Error::Stream)?;
        if read == 0 {
            return Ok(None);
        }
        self.pos += read as u64;
        Ok(Some(start))
    }
}

/// Vrai si la ligne ouvre un message.
///
/// Volontairement littérale : voir la discussion en tête de module sur pourquoi la
/// condition « précédée d'une ligne vide » a été retirée après mesure sur le corpus réel.
fn is_separator(line: &[u8]) -> bool {
    line.starts_with(b"From ")
}

/// La ligne sans son terminateur, quel qu'il soit.
fn trim_terminator(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Ajoute la ligne à `out` en dés-échappant si besoin. Rend vrai si un `>` a été retiré.
fn unmangle_into(out: &mut Vec<u8>, line: &[u8], mangling: Mangling) -> bool {
    let strip = match mangling {
        Mangling::None => false,
        Mangling::MboxO => line.starts_with(b">From "),
        Mangling::MboxRd => {
            let quotes = line.iter().take_while(|&&b| b == b'>').count();
            quotes > 0 && line[quotes..].starts_with(b"From ")
        }
    };

    if strip {
        out.extend_from_slice(&line[1..]);
    } else {
        out.extend_from_slice(line);
    }
    strip
}

/// Retire la ligne vide qui précède le séparateur suivant.
///
/// Elle appartient au format, pas au message. La garder ajouterait un blanc parasite à
/// chaque message stocké — et, comme le hash porte sur ce qui est stocké, changerait
/// l'identité de tous les messages du corpus.
fn strip_trailing_blank_line(out: &mut Vec<u8>) {
    if out.ends_with(b"\r\n\r\n") {
        out.truncate(out.len() - 2);
    } else if out.ends_with(b"\n\n") {
        out.truncate(out.len() - 1);
    } else if matches!(out.as_slice(), b"\n" | b"\r\n") {
        out.clear();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Lit tout un mbox synthétique et rend les messages sous forme d'octets.
    fn read_all(input: &[u8]) -> Vec<Vec<u8>> {
        read_all_with(input, Mangling::default())
    }

    fn read_all_with(input: &[u8], mangling: Mangling) -> Vec<Vec<u8>> {
        let mut reader = MboxReader::new(input).with_mangling(mangling);
        let mut out = Vec::new();
        let mut buf = Vec::new();
        while reader.read_message_into(&mut buf).unwrap().is_some() {
            out.push(buf.clone());
        }
        out
    }

    fn read_all_meta(input: &[u8]) -> Vec<(MessageMeta, Vec<u8>)> {
        let mut reader = MboxReader::new(input);
        let mut out = Vec::new();
        let mut buf = Vec::new();
        while let Some(meta) = reader.read_message_into(&mut buf).unwrap() {
            out.push((meta, buf.clone()));
        }
        out
    }

    fn as_str(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    // ---------------------------------------------------------------- cas nominaux

    #[test]
    fn reads_a_single_message() {
        let mbox = b"From a@b Mon Jan  1 00:00:00 2024\r\nSubject: un\r\n\r\ncorps\r\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 1);
        assert_eq!(as_str(&msgs[0]), "Subject: un\r\n\r\ncorps\r\n");
    }

    #[test]
    fn reads_two_messages() {
        let mbox = b"From a@b date\r\nSubject: un\r\n\r\npremier\r\n\r\n\
                     From c@d date\r\nSubject: deux\r\n\r\nsecond\r\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 2);
        assert_eq!(as_str(&msgs[0]), "Subject: un\r\n\r\npremier\r\n");
        assert_eq!(as_str(&msgs[1]), "Subject: deux\r\n\r\nsecond\r\n");
    }

    #[test]
    fn handles_lf_only_line_endings() {
        let mbox = b"From a@b date\nSubject: un\n\npremier\n\nFrom c@d date\nSubject: deux\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 2);
        assert_eq!(as_str(&msgs[0]), "Subject: un\n\npremier\n");
        assert_eq!(as_str(&msgs[1]), "Subject: deux\n");
    }

    #[test]
    fn handles_line_endings_mixed_within_one_file() {
        // Vu dans la nature : un dossier écrit par deux versions différentes du client.
        let mbox = b"From a@b date\nSubject: un\r\n\r\ncorps mixte\n\nFrom c@d date\r\nb\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 2);
        assert_eq!(as_str(&msgs[0]), "Subject: un\r\n\r\ncorps mixte\n");
    }

    #[test]
    fn reports_offset_and_envelope() {
        let mbox = b"From a@b lundi\nun\n\nFrom c@d mardi\ndeux\n";
        let read = read_all_meta(mbox);

        assert_eq!(read[0].0.offset, 0);
        assert_eq!(read[0].0.envelope, "From a@b lundi");
        assert_eq!(read[1].0.offset, u64::try_from(mbox.len() - 20).unwrap());
        assert_eq!(read[1].0.envelope, "From c@d mardi");
    }

    #[test]
    fn reported_length_matches_the_buffer() {
        let mbox = b"From a@b date\nSubject: un\n\ncorps\n";
        let read = read_all_meta(mbox);
        assert_eq!(read[0].0.len, read[0].1.len());
    }

    // ---------------------------------------------------------------- From-mangling

    #[test]
    fn unescapes_one_level_of_from_mangling() {
        let mbox = b"From a@b date\n\n>From le facteur\n";
        let msgs = read_all(mbox);
        assert_eq!(as_str(&msgs[0]), "\nFrom le facteur\n");
    }

    #[test]
    fn unescapes_deeper_levels_in_mboxrd() {
        let mbox = b"From a@b date\n\n>>From cite deux fois\n>>>From trois fois\n";
        let msgs = read_all(mbox);
        assert_eq!(
            as_str(&msgs[0]),
            "\n>From cite deux fois\n>>From trois fois\n"
        );
    }

    #[test]
    fn mboxo_only_unescapes_the_first_level() {
        let mbox = b"From a@b date\n\n>From un\n>>From deux\n";
        let msgs = read_all_with(mbox, Mangling::MboxO);
        assert_eq!(as_str(&msgs[0]), "\nFrom un\n>>From deux\n");
    }

    #[test]
    fn mangling_none_leaves_the_body_untouched() {
        let mbox = b"From a@b date\n\n>From un\n";
        let msgs = read_all_with(mbox, Mangling::None);
        assert_eq!(as_str(&msgs[0]), "\n>From un\n");
    }

    #[test]
    fn a_quoted_line_that_is_not_from_is_untouched() {
        // `>Fromage` ne doit pas perdre son chevron.
        let mbox = b"From a@b date\n\n>Fromage\n>From age\n> From espace\n";
        let msgs = read_all(mbox);
        assert_eq!(as_str(&msgs[0]), "\n>Fromage\nFrom age\n> From espace\n");
    }

    #[test]
    fn counts_unmangled_lines() {
        let mbox = b"From a@b date\n\n>From un\ntexte\n>From deux\n";
        let mut reader = MboxReader::new(&mbox[..]);
        let mut buf = Vec::new();
        while reader.read_message_into(&mut buf).unwrap().is_some() {}
        assert_eq!(reader.stats().unmangled_lines, 2);
    }

    // ------------------------------------------------- séparateurs et faux positifs

    #[test]
    fn a_separator_right_after_a_mime_boundary_is_detected() {
        // Régression, tirée telle quelle du corpus réel. Thunderbird colle le séparateur à
        // la frontière de clôture MIME, sans ligne vide. Exiger la ligne vide fusionnait
        // ici 234 Mio de messages en un seul jusqu'à faire sauter le plafond de taille.
        let mbox = b"From a@b lundi\r\n\
                     Content-Type: multipart/mixed; boundary=\"limite\"\r\n\
                     \r\n\
                     ------=_NextPart_000_1271_01D9AA72.1A64FD40--\r\n\
                     From - Tue Oct 17 17:34:18 2023\r\n\
                     X-Mozilla-Status: 0001\r\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 2, "séparateur sans ligne vide devant, manqué");
        assert!(as_str(&msgs[1]).contains("X-Mozilla-Status"));
    }

    #[test]
    fn an_unescaped_body_line_starting_with_from_does_split_the_message() {
        // La contrepartie assumée de la règle littérale, et la raison pour laquelle le
        // From-mangling n'est pas un détail : c'est lui qui empêche ce cas d'arriver sur un
        // fichier écrit correctement. Le test existe pour que le compromis reste explicite.
        let mbox = b"From a@b date\nSubject: un\nFrom cette phrase commence par From\nfin\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn a_properly_escaped_body_line_does_not_split_the_message() {
        // Le même contenu, échappé comme l'écrivain est censé le faire : un seul message,
        // et la ligne d'origine restituée intacte.
        let mbox = b"From a@b date\nSubject: un\n>From cette phrase commence par From\nfin\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 1);
        assert!(as_str(&msgs[0]).contains("From cette phrase commence par From"));
    }

    #[test]
    fn fromage_at_line_start_is_not_a_separator() {
        // `From` sans espace : pas un séparateur, quoi qu'il arrive.
        let mbox = b"From a@b date\nun\n\nFromage a discretion\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 1);
        assert!(as_str(&msgs[0]).contains("Fromage"));
    }
    // ---------------------------------------------------------------- cas dégénérés

    #[test]
    fn empty_input_yields_nothing() {
        assert!(read_all(b"").is_empty());
    }

    #[test]
    fn input_without_any_separator_yields_nothing() {
        let msgs = read_all(b"Subject: pas un mbox\n\ndu texte\n");
        assert!(msgs.is_empty());
    }

    #[test]
    fn counts_garbage_before_the_first_separator() {
        let mbox = b"des detritus\nen tete de fichier\n\nFrom a@b date\ncorps\n";
        let mut reader = MboxReader::new(&mbox[..]);
        let mut buf = Vec::new();

        assert!(reader.read_message_into(&mut buf).unwrap().is_some());
        assert_eq!(as_str(&buf), "corps\n");
        assert!(
            reader.stats().leading_garbage > 0,
            "les détritus n'ont pas été comptés"
        );
    }

    #[test]
    fn a_separator_alone_yields_one_empty_message() {
        let msgs = read_all(b"From a@b date\n");
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].is_empty());
    }

    #[test]
    fn an_empty_message_between_separators_is_still_a_message() {
        // Vu dans la nature, et ça ne doit pas décaler la suite.
        let mbox = b"From a@b date\n\nFrom c@d date\ndeuxieme\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].is_empty());
        assert_eq!(as_str(&msgs[1]), "deuxieme\n");
    }

    #[test]
    fn a_truncated_last_message_is_kept() {
        // Fichier coupé en cours d'écriture : on garde ce qu'on a, sans terminateur final.
        let mbox = b"From a@b date\nSubject: un\n\ncorps sans fin de ligne";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 1);
        assert_eq!(as_str(&msgs[0]), "Subject: un\n\ncorps sans fin de ligne");
    }

    #[test]
    fn a_message_without_trailing_blank_line_keeps_its_last_newline() {
        // Deux messages collés, sans ligne vide entre eux : le premier garde son seul
        // terminateur, on ne lui en retire pas un qui n'existe pas.
        let mbox = b"From a@b date\ncorps\nFrom c@d date\ndeuxieme\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 2);
        assert_eq!(as_str(&msgs[0]), "corps\n");
        assert_eq!(as_str(&msgs[1]), "deuxieme\n");
    }

    #[test]
    fn only_one_trailing_blank_line_is_removed() {
        // Deux lignes vides avant le séparateur : la première appartient au message.
        let mbox = b"From a@b date\ncorps\n\n\nFrom c@d date\nb\n";
        let msgs = read_all(mbox);
        assert_eq!(msgs.len(), 2);
        assert_eq!(as_str(&msgs[0]), "corps\n\n");
    }

    #[test]
    fn preserves_binary_bodies_byte_for_byte() {
        // Une pièce jointe binaire mal encodée : aucun octet ne doit être réinterprété.
        let mut mbox = b"From a@b date\nContent-Type: application/octet-stream\n\n".to_vec();
        let payload: Vec<u8> = (0u8..=255).filter(|b| *b != b'\n').collect();
        mbox.extend_from_slice(&payload);
        mbox.push(b'\n');

        let mut expected = b"Content-Type: application/octet-stream\n\n".to_vec();
        expected.extend_from_slice(&payload);
        expected.push(b'\n');

        let msgs = read_all(&mbox);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], expected, "le corps binaire a été altéré");
    }

    #[test]
    fn handles_a_very_long_single_line() {
        // Une pièce jointe base64 sur une seule ligne, ça existe.
        let mut mbox = b"From a@b date\n\n".to_vec();
        mbox.extend(std::iter::repeat_n(b'A', 2 * 1024 * 1024));
        mbox.push(b'\n');

        let msgs = read_all(&mbox);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].len(), 2 * 1024 * 1024 + 2);
    }

    // ---------------------------------------------------------------- garde-fous

    #[test]
    fn a_message_over_the_limit_is_an_error_not_an_allocation() {
        let mut mbox = b"From a@b date\n".to_vec();
        for _ in 0..1000 {
            mbox.extend_from_slice(b"une ligne de corps parfaitement anodine\n");
        }

        let mut reader = MboxReader::new(&mbox[..]).with_limit(1024);
        let mut buf = Vec::new();

        assert!(matches!(
            reader.read_message_into(&mut buf),
            Err(Error::MessageTooLarge { limit: 1024, .. })
        ));
    }

    #[test]
    fn the_limit_error_carries_the_offset_of_the_guilty_message() {
        let mut mbox = b"From a@b date\ncourt\n\n".to_vec();
        let second_offset = mbox.len() as u64;
        mbox.extend_from_slice(b"From c@d date\n");
        mbox.extend(std::iter::repeat_n(b'x', 5000));

        let mut reader = MboxReader::new(&mbox[..]).with_limit(1024);
        let mut buf = Vec::new();

        reader.read_message_into(&mut buf).unwrap();
        match reader.read_message_into(&mut buf) {
            Err(Error::MessageTooLarge { offset, .. }) => assert_eq!(offset, second_offset),
            other => panic!("attendu MessageTooLarge, obtenu {other:?}"),
        }
    }

    #[test]
    fn reusing_the_buffer_does_not_leak_the_previous_message() {
        let mbox = b"From a@b date\nun message assez long pour laisser des traces\n\n\
                     From c@d date\ncourt\n";
        let mut reader = MboxReader::new(&mbox[..]);
        let mut buf = Vec::new();

        reader.read_message_into(&mut buf).unwrap();
        reader.read_message_into(&mut buf).unwrap();

        assert_eq!(as_str(&buf), "court\n");
        assert!(!as_str(&buf).contains("traces"));
    }

    #[test]
    fn reading_past_the_end_keeps_returning_none() {
        let mut reader = MboxReader::new(&b"From a@b date\ncorps\n"[..]);
        let mut buf = Vec::new();

        assert!(reader.read_message_into(&mut buf).unwrap().is_some());
        assert!(reader.read_message_into(&mut buf).unwrap().is_none());
        assert!(reader.read_message_into(&mut buf).unwrap().is_none());
        assert!(buf.is_empty(), "le tampon n'a pas été vidé");
    }

    #[test]
    fn counts_the_messages_it_produced() {
        let mbox = b"From a@b d\nun\n\nFrom c@d d\ndeux\n\nFrom e@f d\ntrois\n";
        let mut reader = MboxReader::new(&mbox[..]);
        let mut buf = Vec::new();
        while reader.read_message_into(&mut buf).unwrap().is_some() {}
        assert_eq!(reader.stats().messages, 3);
    }

    #[test]
    fn identical_messages_in_two_files_hash_identically() {
        // Le lien entre ce lecteur et la dédup : la même entrée, séparateurs et
        // échappements différents, doit produire les mêmes octets stockés.
        let inbox = b"From a@b lundi\nSubject: facture\n\n>From le plombier\n";
        let archive =
            b"From a@b mardi tout autre enveloppe\nSubject: facture\n\n>From le plombier\n";

        let un = read_all(inbox);
        let deux = read_all(archive);

        assert_eq!(un[0], deux[0]);
        assert_eq!(
            mailcore::BlobHash::of(&un[0]),
            mailcore::BlobHash::of(&deux[0]),
            "la ligne d'enveloppe a fui dans le contenu haché"
        );
    }
}

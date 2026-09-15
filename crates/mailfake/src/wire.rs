//! Le fil : découper une commande, écrire une réponse, compter les octets.
//!
//! ## Le compteur d'octets n'est pas un détail
//!
//! [`Fault::ClosesAfter`] coupe la connexion après un nombre d'octets **écrits**, pas après
//! une réponse. C'est ce qui reproduit un câble débranché : la coupure tombe au milieu d'un
//! littéral, d'un nom de boîte, d'un `{`. Sans compteur, on ne saurait couper qu'aux
//! frontières propres — c'est-à-dire là où c'est facile.
//!
//! [`Fault::ClosesAfter`]: crate::Fault::ClosesAfter

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;

/// Une écriture qui compte ce qui est parti, et qui peut s'arrêter net.
#[derive(Debug)]
pub(crate) struct Wire {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    written: usize,
    /// La limite d'écriture, s'il y en a une. Voir le module.
    budget: Option<usize>,
}

/// Ce qui a mis fin à une écriture avant son terme.
///
/// Une seule variante, et un type nommé plutôt qu'un booléen : `Some(Done::Cut)` se lit sur
/// place, `Some(true)` obligerait à retourner voir la signature.
#[derive(Debug)]
pub(crate) enum Done {
    /// Le budget d'écriture est épuisé : la panne a fait son travail.
    Cut,
}

impl Wire {
    /// Enveloppe une connexion acceptée.
    ///
    /// # Errors
    ///
    /// L'erreur d'`io` si le socket ne peut pas être dupliqué pour la lecture.
    pub(crate) fn new(stream: TcpStream, budget: Option<usize>) -> io::Result<Self> {
        // Un délai de lecture, et il est là pour une raison précise : un client qui a
        // provoqué une panne peut ne plus rien envoyer. Sans délai, le fil de session
        // resterait pour toute la durée du processus de test.
        stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            stream,
            reader,
            written: 0,
            budget,
        })
    }

    /// Lit une ligne de commande, sans le `CRLF`.
    ///
    /// Rend `None` quand le client a raccroché.
    ///
    /// # Errors
    ///
    /// L'erreur d'`io` de la lecture, délai d'attente compris.
    pub(crate) fn read_line(&mut self) -> io::Result<Option<String>> {
        let mut raw = Vec::new();
        // Sur les octets et non sur les caractères : une commande peut porter un nom de
        // boîte qui n'est pas de l'UTF-8, et lire en `String` échouerait là où IMAP est
        // parfaitement légal.
        let read = self.reader.read_until(b'\n', &mut raw)?;
        if read == 0 {
            return Ok(None);
        }
        while raw.last().is_some_and(|it| *it == b'\n' || *it == b'\r') {
            raw.pop();
        }
        // Les octets non UTF-8 deviennent des caractères de remplacement : ce module ne juge
        // pas les commandes, il les découpe. Un nom de boîte illisible produira un `NO`, pas
        // une panique.
        Ok(Some(String::from_utf8_lossy(&raw).into_owned()))
    }

    /// Écrit des octets bruts, en respectant le budget.
    ///
    /// # Errors
    ///
    /// L'erreur d'`io` de l'écriture.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<Option<Done>> {
        let slice = match self.budget {
            // Le budget peut tomber **au milieu** de ce bloc : on écrit ce qui reste, puis on
            // signale la coupure. C'est ce qui met la coupure ailleurs qu'à une frontière.
            Some(budget) if self.written + bytes.len() > budget => {
                &bytes[..budget.saturating_sub(self.written).min(bytes.len())]
            }
            _ => bytes,
        };
        self.stream.write_all(slice)?;
        self.stream.flush()?;
        self.written += slice.len();

        if self.budget.is_some_and(|it| self.written >= it) {
            return Ok(Some(Done::Cut));
        }
        Ok(None)
    }

    /// Écrit une ligne de protocole, `CRLF` compris.
    ///
    /// # Errors
    ///
    /// L'erreur d'`io` de l'écriture.
    pub(crate) fn line(&mut self, text: &str) -> io::Result<Option<Done>> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.extend_from_slice(b"\r\n");
        self.write(&bytes)
    }
}

/// Une commande découpée : son étiquette, son nom en majuscules, ses arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command {
    pub(crate) tag: String,
    pub(crate) name: String,
    pub(crate) args: Vec<String>,
}

/// Découpe une ligne de commande.
///
/// ## Ce que ce découpage fait et ne fait pas
///
/// Il respecte les chaînes entre guillemets et le déguisement par `\` à l'intérieur, parce
/// qu'un nom de boîte contient des espaces — `"[Gmail]/Tous les messages"` est le cas
/// ordinaire, pas l'exception.
///
/// Il ne gère **pas** les littéraux en entrée (`{12}` suivi d'octets). C'est délibéré : les
/// commandes qu'une synchronisation en lecture émet n'en contiennent pas, et un analyseur de
/// littéraux dont aucun test ne se sert serait du code non couvert qui prétend l'être. Le jour
/// où `APPEND` arrive, il faudra l'écrire — et il aura ses tests.
///
/// Rend `None` sur une ligne vide ou sans nom de commande.
pub(crate) fn parse(line: &str) -> Option<Command> {
    let mut words = split(line).into_iter();
    let tag = words.next()?;
    let name = words.next()?.to_ascii_uppercase();
    if tag.is_empty() || name.is_empty() {
        return None;
    }
    Some(Command {
        tag,
        name,
        args: words.collect(),
    })
}

/// Découpe sur les espaces, en respectant les guillemets et les parenthèses.
///
/// Les parenthèses sont conservées dans le mot rendu : `(CHANGEDSINCE 42)` sort en un
/// morceau, parce que ce qu'il y a dedans est l'affaire de la commande et pas du découpage.
fn split(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut depth = 0_usize;

    for character in line.chars() {
        match character {
            // Le déguisement ne vaut qu'à l'intérieur d'une chaîne, comme dans la RFC 3501.
            '\\' if quoted && !escaped => {
                escaped = true;
                current.push(character);
            }
            '"' if !escaped => {
                quoted = !quoted;
                current.push(character);
                escaped = false;
            }
            '(' if !quoted => {
                depth += 1;
                current.push(character);
                escaped = false;
            }
            ')' if !quoted => {
                depth = depth.saturating_sub(1);
                current.push(character);
                escaped = false;
            }
            ' ' if !quoted && depth == 0 => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                escaped = false;
            }
            _ => {
                current.push(character);
                escaped = false;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Retire les guillemets d'un mot, et défait le déguisement.
pub(crate) fn unquote(word: &str) -> String {
    let trimmed = word
        .strip_prefix('"')
        .and_then(|it| it.strip_suffix('"'))
        .unwrap_or(word);
    let mut out = String::with_capacity(trimmed.len());
    let mut escaped = false;
    for character in trimmed.chars() {
        match character {
            '\\' if !escaped => escaped = true,
            _ => {
                out.push(character);
                escaped = false;
            }
        }
    }
    out
}

/// Met un nom de boîte entre guillemets pour une réponse `LIST`.
///
/// Sur des octets, et non sur une chaîne : un nom peut ne pas être de l'UTF-8, et le but du
/// serveur est justement de pouvoir en servir un.
pub(crate) fn quote_bytes(name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 2);
    out.push(b'"');
    for byte in name {
        if *byte == b'"' || *byte == b'\\' {
            out.push(b'\\');
        }
        out.push(*byte);
    }
    out.push(b'"');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_plain_command() {
        let command = parse("a1 LOGIN marie secret").unwrap();
        assert_eq!(command.tag, "a1");
        assert_eq!(command.name, "LOGIN");
        assert_eq!(command.args, vec!["marie", "secret"]);
    }

    #[test]
    fn the_command_name_is_case_insensitive() {
        assert_eq!(parse("a1 select INBOX").unwrap().name, "SELECT");
    }

    #[test]
    fn a_quoted_mailbox_name_keeps_its_spaces() {
        // Le cas ordinaire chez Gmail, pas une curiosité.
        let command = parse("a1 SELECT \"[Gmail]/Tous les messages\"").unwrap();
        assert_eq!(command.args.len(), 1);
        assert_eq!(unquote(&command.args[0]), "[Gmail]/Tous les messages");
    }

    #[test]
    fn a_parenthesised_argument_stays_in_one_piece() {
        let command = parse("a1 UID FETCH 1:* (FLAGS UID) (CHANGEDSINCE 42)").unwrap();
        assert_eq!(command.args[3], "(CHANGEDSINCE 42)");
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        let command = parse("a1 SELECT \"un\\\"nom\"").unwrap();
        assert_eq!(command.args.len(), 1, "la chaîne a été coupée");
        assert_eq!(unquote(&command.args[0]), "un\"nom");
    }

    #[test]
    fn an_escaped_backslash_at_the_end_does_not_swallow_the_quote() {
        // `"a\\"` est une chaîne qui contient un antislash, et son guillemet final ferme
        // bien la chaîne. Un analyseur qui traite `\\` comme un déguisement du guillemet
        // avalerait la fin de la ligne.
        let command = parse("a1 SELECT \"a\\\\\" reste").unwrap();
        assert_eq!(command.args.len(), 2, "la fin de ligne a été avalée");
        assert_eq!(unquote(&command.args[0]), "a\\");
        assert_eq!(command.args[1], "reste");
    }

    #[test]
    fn an_empty_line_is_not_a_command() {
        assert!(parse("").is_none());
        assert!(parse("   ").is_none());
    }

    #[test]
    fn a_tag_without_a_command_is_not_a_command() {
        assert!(parse("a1").is_none());
        assert!(parse("a1 ").is_none());
    }

    #[test]
    fn quoting_escapes_what_needs_it() {
        assert_eq!(quote_bytes(b"INBOX"), b"\"INBOX\"".to_vec());
        assert_eq!(quote_bytes(b"un\"nom"), b"\"un\\\"nom\"".to_vec());
        assert_eq!(quote_bytes(b"a\\b"), b"\"a\\\\b\"".to_vec());
    }

    #[test]
    fn quoting_leaves_invalid_utf8_alone() {
        // Le serveur doit pouvoir servir un nom illisible : c'est une de ses raisons d'être.
        let quoted = quote_bytes(&[0x49, 0x26, 0xFF, 0xFE]);
        assert_eq!(quoted, vec![b'"', 0x49, 0x26, 0xFF, 0xFE, b'"']);
    }
}

//! La réponse d'un serveur SMTP. **Analyseur d'entrée hostile.**
//!
//! ## La forme, et les deux façons de la lire de travers
//!
//! Une réponse est un code à trois chiffres suivi d'une espace et d'un texte. Une réponse
//! **multi-ligne** répète le code sur chaque ligne, avec un tiret au lieu de l'espace sur
//! toutes sauf la dernière :
//!
//! ```text
//! 250-mail.exemple.fr
//! 250-STARTTLS
//! 250 AUTH PLAIN XOAUTH2
//! ```
//!
//! Deux erreurs classiques, et les deux ont leur test :
//!
//! - **lire la première ligne et s'arrêter.** Un client qui fait ça prend `250-mail.exemple.fr`
//!   pour la réponse entière, puis lit `250-STARTTLS` comme la réponse à la commande
//!   *suivante*. Le dialogue est décalé d'un cran pour toujours, et le symptôme apparaît
//!   ailleurs ;
//! - **tester `line[3] == '-'` sans vérifier la longueur.** Une ligne de trois caractères
//!   fait paniquer, et un serveur qui envoie `250` tout court existe.
//!
//! ## Le code est un `u16`, pas une chaîne
//!
//! Parce que les familles se lisent par division : `2xx` accepté, `3xx` continue, `4xx`
//! passager, `5xx` définitif. Comparer des chaînes ferait écrire `starts_with("4")`, qui est
//! vrai pour `421` comme pour `4` — et `4` n'est pas un code.

use crate::error::{Error, Result};

/// Plafond d'une ligne de réponse.
///
/// La RFC 5321 §4.5.3.1.5 fixe 512 octets, tiret et `CRLF` compris. On accepte quatre fois
/// plus — des serveurs dépassent sur les listes de capacités — et on refuse au-delà : sans
/// plafond, un serveur qui n'envoie jamais de fin de ligne fait grossir un tampon jusqu'à la
/// mémoire disponible.
pub const LINE_LIMIT: usize = 2048;

/// Nombre de lignes acceptées dans une réponse multi-ligne.
///
/// Cinquante. La plus longue réponse réelle est l'`EHLO` d'un gros serveur, qui tient en une
/// vingtaine de lignes. Sans borne, un serveur qui envoie `250-` sans fin tiendrait la boucle
/// pour toujours — ce qui est un blocage, pas une erreur, donc le pire des deux.
pub const MAX_LINES: usize = 50;

/// Ce qu'un serveur a répondu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Le code à trois chiffres.
    pub code: u16,
    /// Les textes, une entrée par ligne, dans l'ordre. Sans le code ni le séparateur.
    pub lines: Vec<String>,
}

impl Reply {
    /// Vrai si le serveur a accepté — `2xx`.
    #[must_use]
    pub const fn accepted(&self) -> bool {
        self.code >= 200 && self.code < 300
    }

    /// Vrai si le serveur attend la suite — `3xx`.
    ///
    /// Deux endroits l'utilisent : le `354` d'un `DATA` et le `334` d'un `AUTH` qui demande sa
    /// réponse SASL.
    #[must_use]
    pub const fn wants_more(&self) -> bool {
        self.code >= 300 && self.code < 400
    }

    /// Vrai si l'échec est **passager** — `4xx`.
    ///
    /// C'est la distinction qui décide de réessayer ou non, et elle appartient au serveur : un
    /// `421` veut dire « reviens plus tard », un `550` veut dire « ne reviens pas ». Les
    /// confondre fait soit abandonner un message qui serait passé, soit marteler un serveur
    /// avec un message qu'il ne prendra jamais.
    #[must_use]
    pub const fn transient(&self) -> bool {
        self.code >= 400 && self.code < 500
    }

    /// Le texte entier, lignes jointes par une espace.
    ///
    /// Pour un message d'erreur destiné à un humain. **Jamais pour décider** : une décision se
    /// prend sur le code, qui est normalisé, pas sur un texte que chaque serveur écrit à sa
    /// façon.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join(" ")
    }

    /// Vrai si l'une des lignes annonce ce mot-clé d'extension, en réponse à un `EHLO`.
    ///
    /// Comparaison sur le **premier mot** de la ligne et insensible à la casse : `AUTH PLAIN
    /// LOGIN` annonce `AUTH`, et `SIZE 35882577` annonce `SIZE`. Chercher le mot n'importe où
    /// dans la ligne ferait croire qu'un serveur annonçant `AUTH PLAIN` annonce `PLAIN` comme
    /// une extension.
    #[must_use]
    pub fn announces(&self, keyword: &str) -> bool {
        self.lines.iter().any(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|first| first.eq_ignore_ascii_case(keyword))
        })
    }

    /// Les arguments d'une extension annoncée — `AUTH PLAIN XOAUTH2` rend `["PLAIN",
    /// "XOAUTH2"]`.
    #[must_use]
    pub fn arguments(&self, keyword: &str) -> Vec<String> {
        self.lines
            .iter()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let first = words.next()?;
                first
                    .eq_ignore_ascii_case(keyword)
                    .then(|| words.map(str::to_owned).collect::<Vec<_>>())
            })
            .next()
            .unwrap_or_default()
    }
}

/// Lit une réponse complète, lignes de continuation comprises.
///
/// `next_line` rend la ligne suivante **sans son `CRLF`**, ou `None` si le flux est fini.
/// Passer un lecteur plutôt qu'un flux garde cette fonction pure de toute entrée/sortie, donc
/// testable sur des réponses fabriquées — y compris celles qu'aucun serveur correct n'enverrait.
///
/// # Errors
///
/// [`Error::Malformed`] si une ligne n'a pas la forme de la RFC, si les codes d'une réponse
/// multi-ligne ne concordent pas, si le serveur ferme au milieu, ou si les bornes sont
/// franchies.
pub fn read(mut next_line: impl FnMut() -> Result<Option<String>>) -> Result<Reply> {
    let mut lines = Vec::new();
    let mut code = None;

    for _ in 0..MAX_LINES {
        let Some(line) = next_line()? else {
            return Err(Error::Malformed {
                stage: None,
                reason: if lines.is_empty() {
                    "le serveur a fermé sans répondre".to_owned()
                } else {
                    "le serveur a fermé au milieu d'une réponse multi-ligne".to_owned()
                },
            });
        };
        if line.len() > LINE_LIMIT {
            return Err(Error::Malformed {
                stage: None,
                reason: format!("ligne de réponse de plus de {LINE_LIMIT} octets"),
            });
        }

        let (this, more, text) = split(&line)?;
        // **Les codes d'une réponse multi-ligne doivent concorder.** La RFC 5321 §4.2.1
        // l'exige, et un serveur qui change de code en route ne dit plus rien d'exploitable :
        // garder le premier serait un pari, garder le dernier en serait un autre.
        match code {
            None => code = Some(this),
            Some(first) if first != this => {
                return Err(Error::Malformed {
                    stage: None,
                    reason: format!("réponse multi-ligne incohérente : {first} puis {this}"),
                });
            }
            Some(_) => {}
        }
        lines.push(text.to_owned());

        if !more {
            return Ok(Reply {
                // `code` vient d'être posé au premier tour : la boucle ne peut pas sortir ici
                // sans y être passée.
                code: code.unwrap_or(this),
                lines,
            });
        }
    }

    Err(Error::Malformed {
        stage: None,
        reason: format!("réponse de plus de {MAX_LINES} lignes"),
    })
}

/// Découpe une ligne : son code, s'il y a une suite, et son texte.
fn split(line: &str) -> Result<(u16, bool, &str)> {
    let bytes = line.as_bytes();
    // **La longueur est vérifiée avant l'indexation.** Un serveur qui répond `25` ou une ligne
    // vide ne doit pas faire paniquer un client qui écrit vers l'extérieur.
    if bytes.len() < 3 {
        return Err(Error::Malformed {
            stage: None,
            reason: format!("réponse trop courte : {}", head(line)),
        });
    }
    let code: u16 = line[..3].parse().map_err(|_| Error::Malformed {
        stage: None,
        reason: format!("réponse sans code à trois chiffres : {}", head(line)),
    })?;
    // La RFC ne définit rien en dehors de 2xx–5xx. Un `100` ou un `600` est une réponse qu'on
    // ne sait pas classer, donc sur laquelle on ne sait pas décider.
    if !(200..600).contains(&code) {
        return Err(Error::Malformed {
            stage: None,
            reason: format!("code de réponse hors des familles connues : {code}"),
        });
    }

    match bytes.get(3) {
        // Dernière ligne.
        Some(b' ') => Ok((code, false, line[4..].trim_end())),
        // Ligne de continuation.
        Some(b'-') => Ok((code, true, line[4..].trim_end())),
        // Un code seul, sans séparateur : accepté comme dernière ligne sans texte. Plusieurs
        // serveurs le font sur un `250` de `NOOP`, et le refuser casserait le dialogue pour
        // une réponse qui ne dit rien de plus que son code.
        None => Ok((code, false, "")),
        Some(other) => Err(Error::Malformed {
            stage: None,
            reason: format!(
                "séparateur de réponse inattendu `{}` : {}",
                char::from(*other),
                head(line)
            ),
        }),
    }
}

/// Le début d'une ligne, pour un message d'erreur.
///
/// Tronqué, et **sur une frontière de caractère** : couper au milieu d'un caractère UTF-8
/// paniquerait sur un serveur qui répond en accentué.
fn head(line: &str) -> String {
    const KEEP: usize = 80;
    if line.len() <= KEEP {
        return line.to_owned();
    }
    let mut at = KEEP;
    while at > 0 && !line.is_char_boundary(at) {
        at -= 1;
    }
    format!("{}…", &line[..at])
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{MAX_LINES, Reply, read};
    use crate::error::Error;

    /// Un lecteur qui rend les lignes données, puis la fin du flux.
    fn feed(lines: &[&str]) -> impl FnMut() -> crate::Result<Option<String>> {
        let mut queue: Vec<String> = lines.iter().rev().map(|it| (*it).to_owned()).collect();
        move || Ok(queue.pop())
    }

    #[test]
    fn a_single_line_reply_is_read() {
        let reply = read(feed(&["250 OK"])).unwrap();
        assert_eq!(reply.code, 250);
        assert_eq!(reply.lines, vec!["OK"]);
        assert!(reply.accepted());
    }

    #[test]
    fn a_multi_line_reply_is_read_to_its_end() {
        // **Le cas qui décale tout le dialogue quand on le lit mal.**
        let reply = read(feed(&[
            "250-mail.exemple.fr",
            "250-STARTTLS",
            "250 AUTH PLAIN XOAUTH2",
        ]))
        .unwrap();

        assert_eq!(reply.code, 250);
        assert_eq!(reply.lines.len(), 3);
        assert!(reply.announces("STARTTLS"));
        assert!(reply.announces("AUTH"));
        assert_eq!(reply.arguments("AUTH"), vec!["PLAIN", "XOAUTH2"]);
    }

    #[test]
    fn an_extension_is_matched_on_the_first_word_only() {
        // `AUTH PLAIN` annonce `AUTH`, pas `PLAIN` : chercher le mot n'importe où dans la ligne
        // ferait croire à des extensions qui n'existent pas.
        let reply = read(feed(&["250-AUTH PLAIN LOGIN", "250 SIZE 35882577"])).unwrap();
        assert!(reply.announces("AUTH"));
        assert!(reply.announces("SIZE"));
        assert!(!reply.announces("PLAIN"), "`PLAIN` n'est pas une extension");
        assert!(!reply.announces("LOGIN"));
        assert_eq!(reply.arguments("SIZE"), vec!["35882577"]);
    }

    #[test]
    fn the_families_are_read_by_division_not_by_prefix() {
        assert!(read(feed(&["250 OK"])).unwrap().accepted());
        assert!(read(feed(&["354 go ahead"])).unwrap().wants_more());
        assert!(read(feed(&["421 reviens plus tard"])).unwrap().transient());
        let hard = read(feed(&["550 inconnu"])).unwrap();
        assert!(!hard.transient() && !hard.accepted());
    }

    #[test]
    fn a_code_alone_is_a_reply_without_text() {
        // Plusieurs serveurs répondent `250` tout court à un `NOOP`. Le refuser casserait le
        // dialogue pour une réponse qui ne dit rien de plus que son code.
        let reply = read(feed(&["250"])).unwrap();
        assert_eq!(reply.code, 250);
        assert_eq!(reply.lines, vec![""]);
    }

    #[test]
    fn a_short_line_is_refused_rather_than_indexed() {
        // **Le piège qui fait paniquer** : tester `line[3]` sans vérifier la longueur.
        for line in ["", "2", "25"] {
            let refused = read(feed(&[line]));
            assert!(
                matches!(refused, Err(Error::Malformed { .. })),
                "pour {line:?} : {refused:?}"
            );
        }
    }

    #[test]
    fn a_line_without_a_numeric_code_is_refused() {
        for line in ["abc OK", "2x0 OK", "-250 OK", "   OK"] {
            assert!(
                matches!(read(feed(&[line])), Err(Error::Malformed { .. })),
                "pour {line:?}"
            );
        }
    }

    #[test]
    fn a_code_outside_the_known_families_is_refused() {
        // Un `100` ou un `600` est une réponse qu'on ne sait pas classer, donc sur laquelle on
        // ne sait pas décider — et décider au hasard, sur un envoi, est le pire choix.
        for line in ["100 bizarre", "600 bizarre", "999 bizarre"] {
            assert!(
                matches!(read(feed(&[line])), Err(Error::Malformed { .. })),
                "pour {line:?}"
            );
        }
    }

    #[test]
    fn an_unexpected_separator_is_refused() {
        assert!(matches!(
            read(feed(&["250:OK"])),
            Err(Error::Malformed { .. })
        ));
    }

    #[test]
    fn a_multi_line_reply_with_mismatched_codes_is_refused() {
        // La RFC 5321 §4.2.1 l'exige, et garder l'un des deux codes serait un pari.
        assert!(matches!(
            read(feed(&["250-un", "251 deux"])),
            Err(Error::Malformed { .. })
        ));
    }

    #[test]
    fn a_stream_that_ends_mid_reply_is_refused_and_says_so() {
        let refused = read(feed(&["250-un", "250-deux"]));
        match refused {
            Err(Error::Malformed { reason, .. }) => {
                assert!(reason.contains("multi-ligne"), "{reason}");
            }
            other => panic!("attendu un refus : {other:?}"),
        }
    }

    #[test]
    fn a_stream_that_ends_before_anything_is_refused_and_says_so() {
        match read(feed(&[])) {
            Err(Error::Malformed { reason, .. }) => {
                assert!(reason.contains("sans répondre"), "{reason}");
            }
            other => panic!("attendu un refus : {other:?}"),
        }
    }

    #[test]
    fn a_reply_that_never_ends_is_refused_rather_than_looped() {
        // Un serveur qui envoie `250-` sans fin tiendrait la boucle pour toujours, ce qui est
        // un blocage — pire qu'une erreur, parce que rien ne le signale.
        let mut count = 0_usize;
        let refused = read(|| {
            count += 1;
            Ok(Some("250-encore".to_owned()))
        });
        assert!(matches!(refused, Err(Error::Malformed { .. })));
        assert!(count <= MAX_LINES + 1, "la boucle n'est pas bornée");
    }

    #[test]
    fn a_very_long_line_is_refused() {
        let long = format!("250 {}", "x".repeat(super::LINE_LIMIT));
        assert!(matches!(read(feed(&[&long])), Err(Error::Malformed { .. })));
    }

    #[test]
    fn an_error_message_never_splits_a_character() {
        // Un serveur qui répond en accentué ne doit pas faire paniquer la construction du
        // message d'erreur.
        let line = format!("abc {}", "é".repeat(200));
        let refused = read(feed(&[&line]));
        assert!(matches!(refused, Err(Error::Malformed { .. })));
    }

    #[test]
    fn the_text_of_a_reply_joins_its_lines_for_a_human() {
        let reply = Reply {
            code: 550,
            lines: vec![
                "destinataire inconnu".to_owned(),
                "essayez autrement".to_owned(),
            ],
        };
        assert_eq!(reply.text(), "destinataire inconnu essayez autrement");
    }
}

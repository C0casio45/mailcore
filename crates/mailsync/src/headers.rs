//! Extraire les métadonnées d'un message RFC 5322 reçu du réseau.
//!
//! ## Pourquoi ce module existe au lieu d'appeler `mailimport`
//!
//! `mailimport` fait la même extraction, mais son entrée est un mbox : il connaît une position
//! dans un fichier, une convention de dés-échappement, une date de réception venue du
//! séparateur `From `. Rien de tout ça n'a de sens ici.
//!
//! Ce qui est commun est **`mail-parser` et le choix des champs**, et ce choix est copié
//! délibérément : deux chemins d'entrée qui rangeraient un message différemment feraient de la
//! dédup un mensonge — même contenu, même blob, mais un sujet différent selon la porte
//! d'entrée.
//!
//! ## Tout est optionnel, et c'est le sujet
//!
//! Un message venu du réseau n'a aucune garantie. Pas de `Date`, pas de `From`, un `Subject`
//! qui ne décode pas, des en-têtes en double. Le corpus réel de la phase 1 en contient : **102
//! messages sans date exploitable et un en-tête illisible** sur 102 760. Aucun de ces cas ne
//! doit faire échouer une moisson — un message qu'on ne sait pas décrire vaut mieux qu'un
//! dossier qu'on ne sait pas synchroniser.

use mailcore::NewMessage;

use crate::error::Result;

/// La date de repli quand le message n'en porte pas d'exploitable.
///
/// Zéro, soit le 1er janvier 1970. **Pas l'instant présent**, et c'est important : la date est
/// la clé de tri de la liste et elle est recopiée dans `refs`. Mettre « maintenant » ferait
/// remonter en tête un message sans date à chaque synchronisation, donc changerait l'ordre de
/// la liste selon le moment du passage. Une date fausse mais stable est moins nuisible qu'une
/// date fausse et mouvante.
pub const NO_DATE: i64 = 0;

/// Décrit un message pour le store.
///
/// # Errors
///
/// Aucune, en pratique : la signature rend un `Result` pour rester compatible avec un
/// analyseur qui échouerait un jour, mais un message indéchiffrable rend une description
/// pauvre plutôt qu'une erreur. **C'est le comportement voulu** — perdre un message parce que
/// son `Subject` ne décode pas serait perdre du courrier.
pub fn parse(rfc822: &[u8]) -> Result<NewMessage<'static>> {
    let parsed = mail_parser::MessageParser::default().parse(rfc822);

    let (from_addr, from_name) = parsed
        .as_ref()
        .and_then(|it| it.from())
        .and_then(|it| it.first().cloned())
        .map_or((String::new(), None), |address| {
            let addr = address
                .address()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            let name = address
                .name()
                .map(|it| it.trim().to_owned())
                .filter(|it| !it.is_empty());
            // **Une valeur sans arobase n'est pas une adresse**, et la ranger comme telle
            // afficherait une fausse adresse et produirait un « Répondre » que tout serveur
            // refuse. Elle devient le nom ; l'adresse reste vide, ce qui est la vérité. Même
            // règle que `mailimport::sender`, et pour la même raison — voir le banc du
            // critère 1, qui a trouvé le cas sur 15 881 messages du corpus.
            if addr.contains('@') {
                (addr, name)
            } else {
                (
                    String::new(),
                    name.or(Some(addr).filter(|it| !it.is_empty())),
                )
            }
        });

    let subject = parsed
        .as_ref()
        .and_then(|it| it.subject())
        .unwrap_or_default()
        .to_owned();

    let rfc822_id = parsed
        .as_ref()
        .and_then(|it| it.message_id())
        .map(str::to_owned);

    let has_attachments = parsed.as_ref().is_some_and(|it| it.attachment_count() > 0);

    Ok(NewMessage {
        blob: mailcore::BlobHash::of(rfc822),
        // Les champs textuels sont fuités volontairement : `NewMessage` emprunte, et la
        // moisson les garde le temps d'une transaction. Un `String` par message, libéré au
        // `commit` — mais `NewMessage<'a>` ne sait pas emprunter à une valeur locale.
        rfc822_id: rfc822_id.map(leak),
        date: date(rfc822),
        from_addr: leak(from_addr),
        from_name: from_name.map(leak),
        subject: leak(subject),
        size: rfc822.len() as u64,
        has_attachments,
    })
}

/// La date d'un message, en secondes Unix, ou [`NO_DATE`].
#[must_use]
pub fn date(rfc822: &[u8]) -> i64 {
    mail_parser::MessageParser::default()
        .parse(rfc822)
        .as_ref()
        .and_then(|it| it.date())
        .map_or(NO_DATE, |it| it.to_timestamp())
}

/// Prolonge la vie d'une chaîne pour la durée du processus.
///
/// ## Pourquoi c'est acceptable ici, et où est la limite
///
/// `NewMessage<'a>` emprunte ses champs, ce qui est le bon choix pour l'import mbox : les
/// tranches pointent dans un tampon de lecture qui vit plus longtemps que l'insertion. La
/// moisson IMAP, elle, **fabrique** ces chaînes en décodant des en-têtes, et elles n'ont
/// nulle part à vivre.
///
/// Trois options : changer `NewMessage` en type possédant — et faire payer une allocation par
/// champ aux 100 000 messages de l'import mbox, qui n'en a pas besoin ; le rendre génrique sur
/// la propriété — et compliquer un type public pour un seul appelant ; ou fuiter.
///
/// **Ce qui rend la fuite bornée** : quatre chaînes courtes par message moissonné, et un
/// message n'est moissonné qu'une fois — la deuxième synchronisation le trouve déjà connu par
/// son UID et ne repasse pas ici. Sur les 102 760 messages du corpus, c'est de l'ordre de
/// quelques mégaoctets, une seule fois.
///
/// **Où est la limite** : si un jour la moisson repassait ici à chaque cycle, cette fuite
/// deviendrait une fuite au sens propre. Le test `un_message_deja_connu_ne_repasse_pas` de
/// `tests/moisson.rs` est ce qui garde cette hypothèse honnête.
fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const CLEAN: &[u8] = b"From: Marie <marie@exemple.fr>\r\n\
                           Subject: facture\r\n\
                           Date: Tue, 1 Sep 2026 10:00:00 +0200\r\n\
                           Message-ID: <abc@exemple.fr>\r\n\
                           \r\n\
                           Bonjour.\r\n";

    #[test]
    fn a_clean_message_is_described_fully() {
        let described = parse(CLEAN).unwrap();
        assert_eq!(described.from_addr, "marie@exemple.fr");
        assert_eq!(described.from_name, Some("Marie"));
        assert_eq!(described.subject, "facture");
        assert_eq!(described.rfc822_id, Some("abc@exemple.fr"));
        assert!(described.date > 0);
        assert!(!described.has_attachments);
    }

    #[test]
    fn an_address_is_lowercased() {
        // La dédup et l'historique d'un contact comparent des adresses : une casse variable
        // ferait deux contacts d'un seul.
        let message = b"From: MARIE@EXEMPLE.FR\r\nSubject: x\r\n\r\ncorps\r\n";
        assert_eq!(parse(message).unwrap().from_addr, "marie@exemple.fr");
    }

    #[test]
    fn a_message_without_a_date_gets_the_epoch_not_now() {
        // « Maintenant » ferait remonter le message en tête à chaque synchronisation, donc
        // changerait l'ordre de la liste selon le moment du passage.
        let message = b"From: a@b.c\r\nSubject: x\r\n\r\ncorps\r\n";
        assert_eq!(parse(message).unwrap().date, NO_DATE);
    }

    #[test]
    fn an_unparsable_date_gets_the_epoch() {
        let message = b"From: a@b.c\r\nDate: pas une date du tout\r\n\r\ncorps\r\n";
        assert_eq!(parse(message).unwrap().date, NO_DATE);
    }

    #[test]
    fn a_message_without_a_from_is_still_described() {
        // 102 messages du corpus réel n'ont pas de date exploitable, un en-tête est
        // illisible. Perdre un message parce qu'il est mal formé serait perdre du courrier.
        let message = b"Subject: sans expediteur\r\n\r\ncorps\r\n";
        let described = parse(message).unwrap();
        assert_eq!(described.from_addr, "");
        assert_eq!(described.subject, "sans expediteur");
    }

    #[test]
    fn a_message_with_no_headers_at_all_is_still_described() {
        let described = parse(b"pas d'en-tetes, juste du texte").unwrap();
        assert_eq!(described.subject, "");
        assert_eq!(described.from_addr, "");
        assert_eq!(described.date, NO_DATE);
    }

    #[test]
    fn an_empty_message_does_not_panic() {
        let described = parse(b"").unwrap();
        assert_eq!(described.size, 0);
        assert_eq!(described.date, NO_DATE);
    }

    #[test]
    fn a_message_that_is_only_a_header_separator_does_not_panic() {
        assert!(parse(b"\r\n").is_ok());
        assert!(parse(b"\r\n\r\n").is_ok());
    }

    #[test]
    fn an_encoded_subject_is_decoded() {
        let message = b"Subject: =?utf-8?q?d=C3=A9j=C3=A0_vu?=\r\n\r\ncorps\r\n";
        assert_eq!(parse(message).unwrap().subject, "déjà vu");
    }

    #[test]
    fn a_subject_with_an_invalid_encoding_does_not_lose_the_message() {
        // Un mot encodé avec une charset inconnue, ou un base64 tronqué. Le sujet peut
        // ressortir de travers ; le message doit ressortir.
        let message = b"Subject: =?charset-inexistant?b?%%%%?=\r\nFrom: a@b.c\r\n\r\ncorps\r\n";
        let described = parse(message).unwrap();
        assert_eq!(described.from_addr, "a@b.c");
    }

    #[test]
    fn a_header_line_of_a_megabyte_does_not_hang() {
        // Le déni de service qu'un `alt` de deux mégaoctets avait provoqué dans
        // `mailhtml::blocks` en phase 1. Le même piège existe sur un en-tête.
        let mut message = b"Subject: ".to_vec();
        message.extend(std::iter::repeat_n(b'x', 1024 * 1024));
        message.extend_from_slice(b"\r\nFrom: a@b.c\r\n\r\ncorps\r\n");

        let at = std::time::Instant::now();
        let described = parse(&message).unwrap();
        assert!(
            at.elapsed() < std::time::Duration::from_secs(2),
            "analyse quadratique : {:?}",
            at.elapsed()
        );
        assert_eq!(described.from_addr, "a@b.c");
    }

    #[test]
    fn a_message_with_many_duplicate_headers_does_not_hang() {
        // Des en-têtes en double sont légaux, et un expéditeur hostile peut en mettre
        // beaucoup. `mail-parser` doit en sortir, et vite.
        let mut message = Vec::new();
        for index in 0..20_000 {
            message.extend_from_slice(format!("X-Bidon-{index}: valeur\r\n").as_bytes());
        }
        message.extend_from_slice(b"From: a@b.c\r\n\r\ncorps\r\n");

        let at = std::time::Instant::now();
        assert_eq!(parse(&message).unwrap().from_addr, "a@b.c");
        assert!(at.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn the_blob_hash_is_that_of_the_raw_bytes() {
        // La dédup en dépend : le hash doit porter sur les octets reçus, avant toute
        // normalisation d'en-tête.
        assert_eq!(parse(CLEAN).unwrap().blob, mailcore::BlobHash::of(CLEAN));
    }

    #[test]
    fn a_from_without_an_at_sign_becomes_a_name_and_not_a_false_address() {
        // **Trouvé par le banc du critère 1 sur le corpus réel** : 15 881 messages dont le
        // `From:` est une phrase sans adresse, écrits par un seul expéditeur automatique. La
        // phrase était rangée comme adresse, donc la liste affichait une fausse adresse et
        // « Répondre » produisait un destinataire que tout serveur refuse.
        //
        // RFC 5322 : un `addr-spec` s'écrit `local@domaine`. Sans arobase, ce n'est pas une
        // adresse — et dire « je ne sais pas qui a envoyé ça » est la vérité.
        let message = b"From: Service de notifications\r\n\
                        Subject: sujet\r\n\
                        \r\n\
                        corps\r\n";
        let described = parse(message).unwrap();
        assert_eq!(described.from_addr, "");
        assert_eq!(described.from_name, Some("Service de notifications"));

        // Une vraie adresse n'est pas touchée, nom compris.
        let message = b"From: Marie <marie@exemple.fr>\r\n\
                        Subject: sujet\r\n\
                        \r\n\
                        corps\r\n";
        let described = parse(message).unwrap();
        assert_eq!(described.from_addr, "marie@exemple.fr");
        assert_eq!(described.from_name, Some("Marie"));
    }
}

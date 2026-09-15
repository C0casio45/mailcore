//! Décoder un nom de boîte IMAP pour l'afficher.
//!
//! ## Ce que c'est
//!
//! La RFC 3501 §5.1.3 définit un encodage propre à IMAP, l'**UTF-7 modifié** : de l'ASCII
//! imprimable tel quel, et tout le reste en Base64 d'UTF-16BE, entre `&` et `-`. Le Base64 y
//! utilise `,` au lieu de `/`, parce que `/` est un séparateur de hiérarchie chez beaucoup de
//! serveurs.
//!
//! `&bHp7lw-` est `決算`. `Th&AOk-orie` est `Théorie`. `&-` est un `&` littéral.
//!
//! ## Il n'y a pas d'encodeur ici, et c'est délibéré
//!
//! On n'envoie **jamais** un nom qu'on n'a pas reçu. `folders.remote_name` garde les octets
//! tels que le serveur les a écrits, et c'est cette suite-là qui repart dans un `EXAMINE`.
//! Écrire un encodeur créerait un deuxième chemin vers le nom d'une boîte, donc une deuxième
//! occasion de se tromper — et le seul cas qui en aurait besoin, créer un dossier côté
//! serveur, est hors du périmètre de la phase 2.
//!
//! ## Le décodage ne peut pas échouer
//!
//! Il rend toujours une chaîne, et c'est un choix : `path` est un **libellé d'affichage**, pas
//! une clé. Un nom mal encodé doit produire quelque chose de lisible, pas faire échouer la
//! découverte des dossiers. Un dossier qu'on affiche de travers reste ouvrable ; un dossier
//! qu'on refuse de lister est du courrier perdu de vue.
//!
//! Ce qui protège la correction, c'est que le décodage **ne sert à rien d'autre** que
//! l'affichage. Le protocole, lui, ne voit que `remote_name`.
//!
//! ## Ce qui arrive vraiment dans la nature
//!
//! Trois écarts, tous rencontrés chez de vrais serveurs, et tous traités :
//!
//! - des noms en **UTF-8 brut**, sans encodage, malgré la RFC. Un octet non ASCII hors d'une
//!   séquence `&…-` est donc lu comme de l'UTF-8, ce qui rend le bon résultat sur ces
//!   serveurs et ne casse rien sur les autres — l'UTF-7 modifié n'utilise que de l'ASCII ;
//! - des séquences **non terminées** en fin de nom ;
//! - du Base64 **tronqué**, dont la longueur ne fait pas un nombre entier d'unités UTF-16.

/// Décode un nom de boîte pour l'affichage.
///
/// Total : toute entrée rend une chaîne. Voir le module pour pourquoi ce n'est pas un
/// `Result`.
#[must_use]
pub fn decode(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut plain: Vec<u8> = Vec::new();
    let mut index = 0;

    while index < raw.len() {
        if raw[index] != b'&' {
            plain.push(raw[index]);
            index += 1;
            continue;
        }

        // Les octets ASCII accumulés partent avant la séquence encodée. Ils sont lus comme de
        // l'UTF-8 : voir le module, des serveurs envoient de l'UTF-8 brut.
        flush(&mut plain, &mut out);
        index += 1;

        // `&-` est un `&` littéral.
        if raw.get(index) == Some(&b'-') {
            out.push('&');
            index += 1;
            continue;
        }

        let start = index;
        while index < raw.len() && raw[index] != b'-' {
            index += 1;
        }
        let encoded = &raw[start..index];
        // Le `-` de fin. Absent en fin de nom : une séquence non terminée, qu'on décode
        // quand même plutôt que de jeter le reste du nom.
        if index < raw.len() {
            index += 1;
        }

        match decode_run(encoded) {
            Some(text) => out.push_str(&text),
            // Indécodable : on rend la séquence telle qu'elle est arrivée, `&` compris. C'est
            // moche et c'est honnête — l'utilisateur voit qu'il y a quelque chose là, au lieu
            // d'un trou.
            None => {
                out.push('&');
                out.push_str(&String::from_utf8_lossy(encoded));
            }
        }
    }

    flush(&mut plain, &mut out);
    out
}

/// Verse les octets ASCII accumulés dans la sortie.
fn flush(plain: &mut Vec<u8>, out: &mut String) {
    if plain.is_empty() {
        return;
    }
    out.push_str(&String::from_utf8_lossy(plain));
    plain.clear();
}

/// Décode une séquence Base64 modifiée en texte.
///
/// `None` quand la séquence n'est pas décodable — caractère hors alphabet, longueur qui ne
/// fait pas un nombre entier d'unités UTF-16, ou substitut non apparié.
fn decode_run(encoded: &[u8]) -> Option<String> {
    if encoded.is_empty() {
        return None;
    }

    let mut bits: u32 = 0;
    let mut held = 0_u32;
    let mut bytes: Vec<u8> = Vec::with_capacity(encoded.len() * 3 / 4 + 1);

    for byte in encoded {
        let value = sextet(*byte)?;
        bits = (bits << 6) | u32::from(value);
        held += 6;
        if held >= 8 {
            held -= 8;
            // Le décalage tient dans 32 bits : `held` reste sous 8 après cette soustraction.
            #[allow(clippy::cast_possible_truncation)]
            bytes.push(((bits >> held) & 0xFF) as u8);
        }
    }

    // **Les bits restants doivent être nuls.** Du bourrage non nul veut dire que la séquence a
    // été tronquée au milieu d'un caractère, et décoder ce qui précède donnerait un nom
    // silencieusement faux plutôt qu'un nom visiblement cassé.
    if held > 0 && (bits & ((1 << held) - 1)) != 0 {
        return None;
    }

    // De l'UTF-16BE : il faut un nombre pair d'octets.
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();

    // `decode_utf16` signale les substituts non appariés. Les remplacer produirait un nom
    // avec un caractère de remplacement au milieu ; refuser rend la séquence brute, ce qui
    // montre qu'il y a un problème d'encodage et non un caractère exotique.
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
}

/// La valeur d'un caractère de l'alphabet Base64 **modifié**.
///
/// `,` remplace `/`, parce que `/` est un séparateur de hiérarchie chez la plupart des
/// serveurs. Un `/` reçu ici n'est donc **pas** accepté : ce serait du Base64 standard, donc
/// une séquence qu'on ne sait pas lire, et la deviner risquerait de rendre un nom faux.
const fn sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b',' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn plain_ascii_passes_through() {
        assert_eq!(decode(b"INBOX"), "INBOX");
        assert_eq!(
            decode(b"[Gmail]/Tous les messages"),
            "[Gmail]/Tous les messages"
        );
    }

    #[test]
    fn an_empty_name_decodes_to_nothing() {
        assert_eq!(decode(b""), "");
    }

    #[test]
    fn an_encoded_accent_is_decoded() {
        // `Th&AOk-orie` est l'exemple canonique : `é` est U+00E9, soit `00E9` en UTF-16BE,
        // soit `AOk` en Base64 modifié.
        assert_eq!(decode(b"Th&AOk-orie"), "Théorie");
    }

    #[test]
    fn a_lone_ampersand_is_written_as_a_terminated_empty_sequence() {
        // La RFC : `&-` est le `&` littéral.
        assert_eq!(decode(b"R&-D"), "R&D");
        assert_eq!(decode(b"&-"), "&");
    }

    #[test]
    fn a_name_outside_the_basic_plane_is_decoded() {
        // Un émoji : deux unités UTF-16, un substitut haut et un bas. C'est le cas que
        // `decode_utf16` doit recoller, et qu'un décodage unité par unité casserait.
        // U+1F600 → substituts D83D DE00 → `2D3+AA-` en Base64 modifié.
        let decoded = decode(b"&2D3eAA-");
        assert_eq!(decoded, "\u{1F600}");
    }

    #[test]
    fn several_sequences_in_one_name_are_all_decoded() {
        assert_eq!(decode(b"&AOk-t&AOk-"), "été");
    }

    #[test]
    fn a_sequence_mixes_with_ascii_around_it() {
        assert_eq!(decode(b"Dossier/&AOk-t&AOk-/2026"), "Dossier/été/2026");
    }

    #[test]
    fn a_slash_is_not_part_of_the_modified_alphabet() {
        // Le Base64 **modifié** utilise `,` et non `/`. Accepter `/` reviendrait à deviner
        // que le serveur parle du Base64 standard, et rendre un nom faux sans le dire.
        let decoded = decode(b"&AO/-");
        assert!(
            decoded.starts_with('&'),
            "une séquence en Base64 standard a été décodée : {decoded}"
        );
    }

    #[test]
    fn the_comma_of_the_modified_alphabet_is_accepted() {
        // `,` vaut 63, donc il n'apparaît que quand six bits consécutifs sont à un. U+03F0
        // tombe pile dessus : `03F0` en UTF-16BE fait `000000 111111 0000…`, soit `A,A`.
        //
        // L'encodage a été calculé à la main plutôt que recopié : la valeur que j'avais en
        // tête pour l'exemple de la RFC était fausse, et le test l'a montré. Un exemple
        // recopié de mémoire est un test qui vérifie une mémoire, pas un décodeur.
        assert_eq!(decode(b"&A,A-"), "\u{03F0}");

        // Et sans le `,`, les mêmes bits ne décrivent plus le même caractère.
        assert_ne!(decode(b"&A+A-"), "\u{03F0}");
    }

    #[test]
    fn two_cjk_characters_decode_from_one_sequence() {
        // 決 est U+6C7A, 算 est U+7B97 : quatre octets UTF-16BE, six sextets.
        assert_eq!(decode(b"&bHp7lw-"), "決算");
    }

    // ------------------------------------------------------------------
    // Entrée hostile. Rien ne doit paniquer, rien ne doit boucler.
    // ------------------------------------------------------------------

    #[test]
    fn an_unterminated_sequence_at_the_end_is_still_decoded() {
        // Un nom coupé. On décode ce qu'on peut plutôt que de jeter le nom entier.
        assert_eq!(decode(b"Th&AOk"), "Thé");
    }

    #[test]
    fn a_sequence_with_a_character_outside_the_alphabet_comes_back_raw() {
        let decoded = decode(b"&AO!k-suite");
        assert!(decoded.starts_with('&'), "{decoded}");
        assert!(decoded.contains("suite"), "le reste du nom a été perdu");
    }

    #[test]
    fn a_truncated_sequence_with_non_zero_padding_is_refused() {
        // La longueur ne fait pas un nombre entier d'unités UTF-16 : décoder ce qui précède
        // donnerait un nom silencieusement faux.
        let decoded = decode(b"&AOkA-");
        assert!(decoded.starts_with('&'), "{decoded}");
    }

    #[test]
    fn an_unpaired_surrogate_is_refused() {
        // `D83D` seul est un substitut haut sans son bas. Le remplacer mettrait un caractère
        // de remplacement au milieu d'un nom ; le refuser montre qu'il y a un problème
        // d'encodage.
        let decoded = decode(b"&2D0-");
        assert!(decoded.starts_with('&'), "{decoded}");
    }

    #[test]
    fn an_empty_sequence_without_its_terminator_is_not_a_crash() {
        assert_eq!(decode(b"&"), "&");
    }

    #[test]
    fn consecutive_ampersands_do_not_loop() {
        // Le cas qui ferait boucler un décodeur qui n'avance pas sur une séquence vide.
        let at = std::time::Instant::now();
        let decoded = decode(b"&&&&&&&&&&");
        assert!(at.elapsed() < std::time::Duration::from_secs(1));
        assert!(!decoded.is_empty());
    }

    #[test]
    fn raw_utf8_is_read_as_utf8() {
        // Hors spécification, mais des serveurs le font. Le lire comme de l'UTF-8 rend le bon
        // résultat chez eux et ne casse rien ailleurs : l'UTF-7 modifié n'est que de l'ASCII.
        assert_eq!(decode("Éléments envoyés".as_bytes()), "Éléments envoyés");
    }

    #[test]
    fn invalid_utf8_outside_a_sequence_does_not_panic() {
        let decoded = decode(&[b'I', b'N', 0xFF, 0xFE, b'X']);
        assert!(decoded.contains("IN"));
        assert!(decoded.contains('X'));
    }

    #[test]
    fn a_very_long_name_decodes_in_linear_time() {
        // Le piège du `alt` de deux mégaoctets de la phase 1, sur un autre analyseur.
        let mut raw = Vec::new();
        for _ in 0..50_000 {
            raw.extend_from_slice(b"&AOk-");
        }
        let at = std::time::Instant::now();
        let decoded = decode(&raw);
        assert!(
            at.elapsed() < std::time::Duration::from_secs(2),
            "décodage quadratique : {:?}",
            at.elapsed()
        );
        assert_eq!(decoded.chars().count(), 50_000);
    }

    #[test]
    fn every_byte_value_is_survivable() {
        // Balayage exhaustif : aucune valeur d'octet ne doit faire paniquer le décodeur.
        for byte in 0..=255_u8 {
            let _ = decode(&[byte]);
            let _ = decode(&[b'&', byte]);
            let _ = decode(&[b'&', byte, b'-']);
            let _ = decode(&[b'&', b'A', byte, b'-']);
        }
    }
}

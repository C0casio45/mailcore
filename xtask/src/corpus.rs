//! Ce que le corpus réel exige d'un moteur de texte.
//!
//! ## Pourquoi cette mesure existe
//!
//! Le choix du toolkit d'une éventuelle coquille native se joue en grande partie sur le
//! **rendu du texte**. Un client mail, c'est 95 % de texte, et les toolkits Rust ne se valent
//! pas là-dessus :
//!
//! - certains ne font **aucun façonnage** (*shaping*) — pas de ligatures obligatoires, pas de
//!   réordonnancement, pas de formes contextuelles. Un sujet en arabe s'affiche alors en
//!   lettres isolées et dans le mauvais sens ; en devanagari, les conjointes ne se forment
//!   pas. Ce n'est pas « moins joli », c'est **faux** ;
//! - certains n'ont pas de repli de police, et un sujet en japonais devient une suite de
//!   rectangles ;
//! - le bidirectionnel — hébreu, arabe — demande en plus un algorithme d'ordre visuel.
//!
//! La question n'est donc pas « est-ce que ce toolkit fait du beau texte » dans l'abstrait,
//! mais **est-ce que ce corpus-là contient les écritures que ce toolkit rendrait faux**. Ça
//! se compte, et c'est ce que fait cette commande.
//!
//! Elle lit les sujets et les noms d'expéditeurs — jamais un corps de message — et classe
//! chaque caractère par besoin de rendu. Rien n'est écrit, ni dans le store ni ailleurs.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;

/// Ce qu'une écriture demande au moteur de texte, du plus simple au plus exigeant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Need {
    /// Latin de base et ponctuation : n'importe quel rasteriseur suffit.
    Basic,
    /// Latin étendu, grec, cyrillique : des glyphes de plus dans la police, rien d'autre.
    ExtendedFont,
    /// CJK, hangul, hiragana, katakana : une police de repli, pas de façonnage.
    Fallback,
    /// Arabe, hébreu, syriaque, thaana : façonnage contextuel **et** bidirectionnel.
    ShapingBidi,
    /// Devanagari, bengali, tamoul, thaï, khmer, birman… : façonnage complexe.
    ShapingComplex,
}

impl Need {
    /// Ce qu'un point de code exige.
    ///
    /// Par intervalles, et non par table Unicode complète : la question posée est grossière —
    /// « y a-t-il de l'arabe dans ce corpus » — et une dépendance de plus pour y répondre
    /// serait disproportionnée. Les intervalles retenus sont les blocs principaux de chaque
    /// écriture ; un caractère rare mal classé ne changerait pas un pourcentage.
    fn of(c: char) -> Self {
        match u32::from(c) {
            // ASCII, Latin-1 courant, ponctuation générale, symboles usuels.
            0x0000..=0x00FF | 0x2000..=0x206F | 0x20A0..=0x20CF | 0x2100..=0x214F => Self::Basic,
            // Latin étendu, grec, cyrillique, arménien.
            0x0100..=0x058F => Self::ExtendedFont,
            // Hébreu, arabe, syriaque, thaana, n'ko, et leurs formes de présentation.
            //
            // **Les bornes hautes sont étroites, et c'est le fruit d'une erreur.** Le premier
            // jet prenait `0xFB1D..=0xFEFF` d'un bloc, ce qui avale les sélecteurs de variante
            // (U+FE00–FE0F) : le rapport annonçait alors 589 messages « en arabe ou en hébreu »
            // qui étaient du marketing français avec des émoji. C'est l'échantillon de sujets
            // imprimé en fin de rapport qui l'a montré — raison pour laquelle il est imprimé.
            0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF => Self::ShapingBidi,
            // Écritures indiennes, thaï, lao, tibétain, birman, khmer.
            0x0900..=0x109F | 0x1780..=0x17FF => Self::ShapingComplex,
            // Kana, CJK, hangul, formes pleine largeur.
            0x1100..=0x11FF
            | 0x3000..=0x30FF
            | 0x3130..=0x318F
            | 0x3400..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFF00..=0xFFEF => Self::Fallback,
            // Émoji, symboles, et les sélecteurs de variante qui les accompagnent : une police
            // de repli, souvent en couleur. Aucun façonnage.
            0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0xFE00..=0xFE0F => Self::Fallback,
            _ => Self::ExtendedFont,
        }
    }

    /// Le libellé du rapport.
    const fn label(self) -> &'static str {
        match self {
            Self::Basic => "latin de base",
            Self::ExtendedFont => "latin étendu, grec, cyrillique",
            Self::Fallback => "CJK, hangul, émoji — police de repli",
            Self::ShapingBidi => "arabe, hébreu — façonnage + bidi",
            Self::ShapingComplex => "indien, thaï, khmer — façonnage complexe",
        }
    }
}

/// Compte ce que le corpus exige d'un moteur de texte.
pub fn scripts(store_root: &Utf8PathBuf) -> Result<()> {
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;

    // La même lecture que celle de l'indexation : sujets, expéditeurs, jamais de corps.
    let rows = store.all_for_indexing()?;
    let total = rows.len();
    anyhow::ensure!(total > 0, "store vide");

    // Le besoin **maximal** de chaque message : un sujet qui contient un seul mot en arabe
    // exige le façonnage pour ce message entier. C'est la bonne granularité, parce que c'est
    // la ligne de liste qui sera fausse ou juste.
    let mut per_message = std::collections::BTreeMap::<Need, usize>::new();
    let mut with_shaping = Vec::new();

    for row in &rows {
        let name = row.from_name.as_deref().unwrap_or_default();
        let worst = row
            .subject
            .chars()
            .chain(name.chars())
            .map(Need::of)
            .max()
            .unwrap_or(Need::Basic);
        *per_message.entry(worst).or_default() += 1;

        // Quelques exemples, pour qu'un chiffre abstrait devienne vérifiable à l'œil.
        if worst >= Need::ShapingBidi && with_shaping.len() < 12 {
            with_shaping.push(row.subject.clone());
        }
    }

    println!("Store               {store_root}");
    println!("Messages            {total}");
    println!();
    println!("Exigence maximale par message — sujet et nom d'expéditeur :");
    for (need, count) in &per_message {
        #[allow(clippy::cast_precision_loss)]
        let share = *count as f64 * 100.0 / total as f64;
        println!("  {:<44} {count:>7}   {share:>6.2} %", need.label());
    }

    let shaping: usize = per_message
        .iter()
        .filter(|(need, _)| **need >= Need::ShapingBidi)
        .map(|(_, count)| *count)
        .sum();
    let fallback = per_message
        .get(&Need::Fallback)
        .copied()
        .unwrap_or_default();

    println!();
    #[allow(clippy::cast_precision_loss)]
    {
        println!(
            "Façonnage obligatoire pour {shaping} messages ({:.2} %) : sans lui, ces lignes de \
             liste sont fausses, pas seulement moins jolies.",
            shaping as f64 * 100.0 / total as f64
        );
        println!(
            "Police de repli nécessaire pour {fallback} messages ({:.2} %) : sans elle, des \
             rectangles.",
            fallback as f64 * 100.0 / total as f64
        );
    }

    if !with_shaping.is_empty() {
        println!();
        println!("Échantillon de sujets exigeant le façonnage :");
        for subject in with_shaping.iter().take(12) {
            // Tronqué : un sujet entier n'apporte rien et le rapport doit rester lisible.
            let short: String = subject.chars().take(60).collect();
            println!("  {short}");
        }
    }

    Ok(())
}

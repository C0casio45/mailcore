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

/// Combien de caractères de corps montrer par message, par défaut.
///
/// Assez pour reconnaître de quoi parle le message, pas assez pour le lire. Un échantillon
/// dont chaque bloc fait vingt lignes n'est plus un échantillon : on ne le parcourt plus.
const EXCERPT: usize = 240;

/// Sort un échantillon de messages réels, étalé sur toute la période du corpus.
///
/// ## Pourquoi cette commande existe
///
/// L'étape 1 de `docs/PHASE-4.md` est la seule de la phase qu'un programme ne peut pas faire :
/// écrire des requêtes en français **avec la réponse qu'elles doivent trouver**. Un humain ne
/// peut le faire que sur des messages qu'il a sous les yeux, et ouvrir cinq mille messages un
/// par un n'est pas une méthode.
///
/// ## Étalé, et pas les N premiers
///
/// Les N premiers messages d'un store sont ceux d'un dossier et d'une période. Un jeu de
/// requêtes écrit dessus mesurerait la recherche sur trois semaines de courrier, et le relevé
/// serait bon sans vouloir dire quoi que ce soit. Le pas est donc calculé sur le corpus trié
/// par date : l'échantillon couvre la même étendue que le corpus.
///
/// ## Elle ne fait que lire
///
/// Aucune écriture, donc elle peut viser le store de production — contrairement aux bancs qui
/// écrivent, qui se font leur propre store jetable. C'est même son intérêt : le jeu de
/// requêtes ne vaut que s'il porte sur le vrai courrier de quelqu'un.
///
/// # Errors
///
/// Si le store est illisible, ou si aucun message n'y est indexé.
pub fn sample(
    store_root: &Utf8PathBuf,
    count: usize,
    excerpt: Option<usize>,
    everything: bool,
    max_from: usize,
) -> Result<()> {
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let mailbox = mailcore::Mailbox::open(store_root)
        .with_context(|| format!("ouverture de la boîte {store_root}"))?;

    let mut rows = store.all_for_indexing()?;
    if rows.is_empty() {
        anyhow::bail!(
            "aucun message dans {store_root} — `mail sync` d'abord, ou viser un autre store"
        );
    }
    let corpus = rows.len();

    // **Le filtre qui rend l'échantillon utilisable**, et il vient d'un relevé : un tirage
    // uniforme sur le corpus réel a donné 90 % de notifications — Facebook, Dribbble, YouTube,
    // Twitch. Personne ne cherche la notification Facebook de mars 2016, donc un jeu de
    // requêtes écrit dessus mesurerait la recherche sur du courrier que personne ne relit.
    //
    // Le signal n'est pas le rang du carnet mais **`seen_to > 0`** : une adresse à laquelle
    // l'utilisateur a écrit au moins une fois. Un expéditeur automatique ne reçoit jamais de
    // réponse, et aucune liste de domaines à bannir n'est à tenir à jour.
    // Deux façons d'être retenu, et la seconde est celle qui a manqué au premier jet. Filtrer
    // sur les seuls correspondants gardait 32 messages sur 5 063 — et surtout il excluait le
    // cas canonique de la phase : « la facture du plombier de l'an dernier » vient d'un
    // expéditeur automatique, à qui personne ne répond jamais.
    //
    // Ce qui distingue une facture d'une notification n'est donc pas l'humain derrière, c'est
    // la **rareté** : une facture arrive une fois, Facebook écrit quatre cents fois. Un
    // expéditeur au-dessus du seuil est une source récurrente, et ce qu'on cherche dans une
    // source récurrente n'est pas un message mais un fil de vie.
    let kept = if everything {
        None
    } else {
        let contacts = store.top_contacts(usize::MAX)?;
        let corresponded: std::collections::HashSet<String> = contacts
            .into_iter()
            .filter(|it| it.seen_to > 0)
            .map(|it| it.address)
            .collect();

        let mut volume: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for row in &rows {
            *volume.entry(row.from_addr.to_lowercase()).or_default() += 1;
        }

        rows.retain(|row| {
            let from = row.from_addr.to_lowercase();
            corresponded.contains(&from) || volume.get(&from).is_some_and(|it| *it <= max_from)
        });
        Some((corresponded.len(), max_from))
    };

    if rows.is_empty() {
        anyhow::bail!(
            "aucun message d'un correspondant à qui l'utilisateur a écrit — \
             `--everything` pour tirer sans filtre"
        );
    }
    rows.sort_by_key(|row| row.date);

    let excerpt = excerpt.unwrap_or(EXCERPT);
    let total = rows.len();
    let count = count.min(total);
    // Le pas en virgule flottante puis arrondi : un pas entier sur 5 063 messages et 30
    // demandés donnerait 168, donc le dernier échantillon tomberait au message 5 040 et les
    // vingt-trois derniers ne seraient jamais tirés.
    #[allow(clippy::cast_precision_loss)]
    let step = total as f64 / count as f64;

    println!("# Échantillon du corpus — {store_root}");
    println!("#");
    match kept {
        Some((addresses, cap)) => println!(
            "# {total} messages retenus sur {corpus} : ceux d'une des {addresses} adresses à \
             qui l'utilisateur a écrit, plus ceux d'un expéditeur vu au plus {cap} fois"
        ),
        None => println!("# {total} messages, sans filtre (`--everything`)"),
    }
    println!("# {count} tirés, un tous les {step:.1}");
    println!("# Du plus ancien au plus récent. L'identifiant est celui de `mail source --id`.");
    println!();

    for rank in 0..count {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let at = ((rank as f64 * step) as usize).min(total - 1);
        let row = &rows[at];

        let who = row.from_name.as_deref().unwrap_or(&row.from_addr);
        println!(
            "#{:<6} {}  {} <{}>",
            row.id.0,
            mailapi::human::date(row.date),
            who,
            row.from_addr
        );
        println!(
            "       {}",
            if row.subject.is_empty() {
                "(sans sujet)"
            } else {
                &row.subject
            }
        );

        // Le corps passe par `Mailbox::message`, donc par le même aplatissement que
        // l'indexation. Le réécrire ici donnerait un échantillon qui ne ressemble pas à ce que
        // la recherche voit — et c'est exactement ce que le jeu de requêtes doit viser.
        match mailbox.message(row.id) {
            Ok(Some(detail)) => println!("       {}", squeeze(&detail.body, excerpt)),
            Ok(None) => println!("       (corps introuvable — voir `mail doctor`)"),
            Err(source) => println!("       (corps illisible : {source})"),
        }
        println!();
    }

    Ok(())
}

/// Réduit un corps à une ligne lisible : espaces repliés, coupé sur une frontière de
/// caractère.
///
/// Couper sur un index d'octet planterait au milieu d'un caractère accentué, ce qui sur un
/// corpus français veut dire « presque toujours ».
fn squeeze(body: &str, limit: usize) -> String {
    let mut out = String::with_capacity(limit + 1);
    let mut space = false;
    for ch in body.chars() {
        if out.chars().count() >= limit {
            out.push('…');
            break;
        }
        if ch.is_whitespace() {
            // Un seul blanc pour toute suite de blancs : un corps de mail est plein de sauts
            // de ligne et d'indentations de citation.
            if !out.is_empty() {
                space = true;
            }
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(ch);
    }
    if out.is_empty() {
        "(corps vide)".to_owned()
    } else {
        out
    }
}

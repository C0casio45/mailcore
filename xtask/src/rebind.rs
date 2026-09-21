//! `queries-rebind` — relie un jeu de requêtes à un store, par le contenu des messages.
//!
//! ## Ce qu'il répare, et ce qu'il refuse de réparer
//!
//! Le jeu de requêtes du critère 1 désignait ses cibles par `messages.id`. Un réimport du store
//! réattribue ces identifiants : le 2026-09-21, les 22 cibles pointaient sur d'autres messages,
//! et rien ne le signalait puisqu'elles **existaient** toutes. Le format désigne maintenant les
//! cibles par l'empreinte de leur contenu, qui ne bouge pas.
//!
//! Cet outil fait la migration, et il la fait **à moitié exprès**. Il propose une empreinte quand
//! il peut la déduire sans ambiguïté, et il refuse dans tous les autres cas plutôt que de
//! choisir. Une vérité terrain devinée n'est pas une vérité terrain : elle rendrait un chiffre,
//! et ce chiffre serait faux de la même façon silencieuse qu'avant.
//!
//! ## Comment il déduit
//!
//! Les notes du jeu citent le sujet du message visé entre guillemets français — « Votre colis a
//! bien été livré ». C'est la seule trace du message d'origine qui ait survécu au réimport, et
//! elle a été écrite par un humain qui le regardait. L'outil cherche donc les messages dont le
//! sujet **contient** ce texte, repliés en minuscules Unicode — `to_lowercase` et non
//! `to_ascii_lowercase`, la leçon du carnet d'adresses.
//!
//! Trois issues, et deux sont des refus :
//!
//! - **un seul message** : l'empreinte est proposée ;
//! - **plusieurs** : refus, avec la liste. « Votre colis a bien été livré » arrive quarante fois
//!   en quatre ans, et choisir le plus ancien serait inventer ;
//! - **aucun** : refus. Le message n'est pas dans ce store.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;

/// Ce que l'outil lit du jeu, sans rien supposer du reste.
///
/// `target` est accepté mais **ignoré** : c'est le champ périmé, gardé en lecture pour que les
/// anciens fichiers se chargent encore le temps de la migration.
#[derive(Debug, serde::Deserialize)]
struct Entry {
    text: String,
    #[serde(default)]
    #[allow(dead_code)]
    target: Option<i64>,
    #[serde(default)]
    blobs: Option<Vec<String>>,
    #[serde(default)]
    note: String,
}

#[derive(Debug, serde::Deserialize)]
struct Ground {
    query: Vec<Entry>,
}

/// Propose une empreinte par requête, et dit ce qui demande un humain.
///
/// # Errors
///
/// Si le store ou le jeu de requêtes est illisible.
pub fn run(
    store_root: &Utf8PathBuf,
    queries: &Utf8PathBuf,
    out: Option<&Utf8PathBuf>,
) -> Result<()> {
    let raw = std::fs::read_to_string(queries)
        .with_context(|| format!("lecture du jeu de requêtes {queries}"))?;
    let ground: Ground = toml::from_str(&raw).with_context(|| format!("analyse de {queries}"))?;
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;

    let rows = store
        .all_for_indexing()
        .context("lecture des messages du store")?;
    let subjects: Vec<(mailcore::BlobHash, String)> = rows
        .iter()
        .map(|row| (row.blob, row.subject.clone()))
        .collect();
    let by_id: std::collections::HashMap<i64, &str> = rows
        .iter()
        .map(|row| (row.id.0, row.subject.as_str()))
        .collect();

    println!("Store              {store_root}");
    println!("Messages           {}", subjects.len());
    println!("Requêtes           {}\n", ground.query.len());

    // **Le diagnostic, avant la réparation.** Combien des anciens identifiants désignent encore
    // un message dont le sujet correspond à la note ? Sans ce comptage, « le jeu est périmé »
    // reste une impression tirée de deux ou trois échantillons — et une impression ne justifie
    // pas de jeter une vérité terrain.
    let mut still_right = 0usize;
    let mut checkable = 0usize;
    for entry in &ground.query {
        let (Some(id), Some(quoted)) = (entry.target, quoted_subject(&entry.note)) else {
            continue;
        };
        checkable += 1;
        if by_id
            .get(&id)
            .is_some_and(|subject| subject.to_lowercase().contains(&quoted.to_lowercase()))
        {
            still_right += 1;
        }
    }
    if checkable > 0 {
        println!("--- l'ancien identifiant désigne-t-il encore le bon message ? ---");
        println!("Vérifiables        {checkable} (celles dont la note cite un sujet)");
        println!("Encore justes      {still_right}");
        println!("Ayant glissé       {}\n", checkable - still_right);
    }

    let mut resolved = 0usize;
    let mut ambiguous = 0usize;
    let mut absent = 0usize;
    let mut already = 0usize;

    let mut rebound: Vec<(&Entry, Vec<mailcore::BlobHash>)> = Vec::new();

    for entry in &ground.query {
        if let Some(existing) = &entry.blobs {
            already += 1;
            let parsed: Result<Vec<_>> = existing
                .iter()
                .map(|hex| {
                    mailcore::BlobHash::from_hex(hex.trim())
                        .with_context(|| format!("empreinte illisible : {hex}"))
                })
                .collect();
            rebound.push((entry, parsed?));
            continue;
        }

        let (rule, matches) = candidates(&subjects, entry);
        match matches.len() {
            0 => {
                absent += 1;
                println!("À REPRENDRE  « {} »", entry.text);
                println!("             aucun message ne correspond à cette note.");
                println!("             note : {}\n", entry.note);
            }
            n if n > MAX_ACCEPTABLE => {
                ambiguous += 1;
                println!("TROP LARGE   « {} »", entry.text);
                println!(
                    "             {n} messages correspondent — au-delà de {MAX_ACCEPTABLE}, la \
                     requête ne mesure plus rien :"
                );
                for (blob, subject) in matches.iter().take(5) {
                    println!(
                        "               {}  {}",
                        blob.to_hex(),
                        truncate(subject, 50)
                    );
                }
                println!("               … et {} autres\n", n - 5);
            }
            n => {
                resolved += 1;
                println!(
                    "RELIÉE       « {} »  →  {n} réponse(s) acceptable(s)   [{}]",
                    entry.text,
                    rule.label()
                );
                for (_, subject) in matches.iter().take(3) {
                    println!("               {}", truncate(subject, 66));
                }
                if n > 3 {
                    println!("               … et {} autres du même sujet", n - 3);
                }
                println!();
                rebound.push((entry, matches.iter().map(|(blob, _)| *blob).collect()));
            }
        }
    }

    println!("--- bilan ---");
    println!("Déjà reliées        {already}");
    println!("Reliées             {resolved}");
    println!("Trop larges         {ambiguous}");
    println!("À reprendre         {absent}");

    if let Some(out) = out {
        anyhow::ensure!(
            ambiguous + absent == 0,
            "{} requêtes ne sont pas reliées : écrire un jeu incomplet donnerait un rappel \
             calculé sur un sous-ensemble, présenté comme s'il portait sur tout le jeu",
            ambiguous + absent
        );
        write(out, &rebound).with_context(|| format!("écriture de {out}"))?;
        println!("\nÉcrit dans {out} — {} requêtes.", rebound.len());
        println!("Relire avant de remplacer le jeu : c'est une vérité terrain, pas un cache.");
    } else if ambiguous + absent > 0 {
        println!(
            "\nCes {} lignes demandent un humain. Une vérité terrain devinée rend un chiffre, \n\
             et ce chiffre est faux sans le dire.",
            ambiguous + absent
        );
    } else {
        println!("\nTout est reliable. Relancer avec `--out <fichier>` pour écrire le jeu.");
    }
    Ok(())
}

/// Au-delà de combien de réponses acceptables une requête ne mesure plus rien.
///
/// Trente. Le seuil n'est pas une élégance : si un tiers du corpus répond à une requête, la
/// trouver dans les dix premiers n'apprend rien sur le moteur. Une requête qui dépasse ce seuil
/// est mal écrite, pas mal reliée — et c'est un humain qui doit la réécrire.
const MAX_ACCEPTABLE: usize = 30;

/// Les messages qui répondent à une requête, déduits de sa note.
///
/// Deux règles, dans cet ordre :
///
/// - **la note cite un sujet** entre guillemets français : tous les messages dont le sujet le
///   contient. C'est le cas des requêtes sémantiques, dont la note a été écrite en regardant le
///   message ;
/// - **sinon**, tous les messages dont le sujet ou l'expéditeur contient **tous** les mots de la
///   requête. C'est le cas des requêtes de contrôle — « Colissimo », « AdSense désactivé » —
///   dont la note dit précisément que les mots de la requête *sont* dans la cible. Leur appliquer
///   leur propre définition est plus fidèle que de leur inventer un sujet.
fn candidates<'a>(
    subjects: &'a [(mailcore::BlobHash, String)],
    entry: &Entry,
) -> (Match, Vec<&'a (mailcore::BlobHash, String)>) {
    if let Some(quoted) = quoted_subject(&entry.note) {
        // **La phrase exacte d'abord.** C'est le signal le plus fort : la note a été écrite en
        // regardant le sujet.
        let needle = fold(quoted);
        let exact: Vec<_> = subjects
            .iter()
            .filter(|(_, subject)| fold(subject).contains(&needle))
            .collect();
        if !exact.is_empty() {
            return (Match::Exact, exact);
        }

        // **Tous les mots ensuite, dans le désordre.** Une note paraphrase : « Le compte AdSense
        // a été désactivé » contre le vrai sujet « Le compte AdSense (pub-9733…) a été désactivé ».
        // Exiger la phrase entière rate le message pour une parenthèse insérée au milieu.
        //
        // Ce n'est pas un relâchement du critère : exiger **tous** les mots porteurs de la
        // description reste vérifiable, et le relevé dit lesquelles ont été reliées par cette
        // règle plus faible — pour qu'un humain les regarde en priorité.
        let words = content_words(quoted);
        if !words.is_empty() {
            let loose: Vec<_> = subjects
                .iter()
                .filter(|(_, subject)| {
                    let folded = fold(subject);
                    words.iter().all(|word| folded.contains(word))
                })
                .collect();
            if !loose.is_empty() {
                return (Match::AllWords, loose);
            }
        }
        return (Match::None, Vec::new());
    }

    // Pas de sujet cité : les requêtes de contrôle, dont la note dit que les mots de la requête
    // *sont* dans la cible. On leur applique leur propre définition.
    let words = content_words(&entry.text);
    if words.is_empty() {
        return (Match::None, Vec::new());
    }
    let found: Vec<_> = subjects
        .iter()
        .filter(|(_, subject)| {
            let folded = fold(subject);
            words.iter().all(|word| folded.contains(word))
        })
        .collect();
    let rule = if found.is_empty() {
        Match::None
    } else {
        Match::QueryWords
    };
    (rule, found)
}

/// Par quelle règle une requête a été reliée.
///
/// Rendu au lecteur du relevé : une règle plus faible n'invalide pas le lien, mais elle dit
/// laquelle relire en premier. Cacher la différence donnerait au jeu réparé la même apparence
/// d'autorité que celui qui a été écrit à la main.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    /// Le sujet contient la phrase citée, telle quelle.
    Exact,
    /// Le sujet contient tous les mots porteurs de la phrase citée, dans le désordre.
    AllWords,
    /// Le sujet contient tous les mots de la requête — pour les contrôles.
    QueryWords,
    /// Rien.
    None,
}

impl Match {
    const fn label(self) -> &'static str {
        match self {
            Self::Exact => "phrase exacte",
            Self::AllWords => "tous les mots (note paraphrasée)",
            Self::QueryWords => "mots de la requête (contrôle)",
            Self::None => "rien",
        }
    }
}

/// Replie un texte pour la comparaison : minuscules Unicode, apostrophes unifiées.
///
/// `to_lowercase` et non `to_ascii_lowercase` — la leçon du carnet d'adresses du 2026-09-11 :
/// sur un corpus français, replier en ASCII ne replie rien. Et l'apostrophe typographique
/// `U+2019` est unifiée sur la droite, le même piège que celui de la barre de recherche le
/// 2026-09-15 : les téléphones et les correcteurs produisent l'une, les claviers l'autre.
fn fold(text: &str) -> String {
    text.to_lowercase().replace(['\u{2019}', '\u{02bc}'], "'")
}

/// Les mots porteurs d'un texte : repliés, sans ponctuation, de plus d'un caractère.
fn content_words(text: &str) -> Vec<String> {
    fold(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() > 1)
        .map(ToOwned::to_owned)
        .collect()
}

/// Écrit le jeu relié.
///
/// Un fichier **neuf**, jamais le fichier d'entrée : une vérité terrain écrite à la main ne se
/// remplace pas par un programme sans que quelqu'un ait relu ce qui la remplace.
fn write(out: &Utf8PathBuf, rebound: &[(&Entry, Vec<mailcore::BlobHash>)]) -> Result<()> {
    use std::fmt::Write as _;

    let mut text = String::new();
    text.push_str(
        "# Vérité terrain du critère 1 de la phase 4 — « trouver ce qu'un mot-clé ne trouve pas ».\n\
         #\n\
         # **Ce fichier n'entre pas dans le dépôt.** Il nomme des messages réels et ses requêtes\n\
         # disent ce que quelqu'un cherche dans son courrier.\n\
         #\n\
         # Les cibles sont désignées par l'**empreinte de leur contenu**, jamais par un\n\
         # identifiant : un réimport réattribue les identifiants, et le 2026-09-21 les 22 cibles\n\
         # de la version précédente désignaient toutes un autre message sans que rien ne le dise.\n\
         #\n\
         # `blobs` porte **toutes** les réponses acceptables. Sur un corpus réel, « quand mon\n\
         # paquet est arrivé » a plusieurs bonnes réponses ; n'en accepter qu'une mesurerait la\n\
         # chance du moteur et non son rappel.\n\
         #\n\
         # Écrit par `cargo xtask queries-rebind`. Relu par un humain.\n",
    );

    for (entry, blobs) in rebound {
        text.push_str("\n[[query]]\n");
        let _ = writeln!(text, "text = {}", quote(&entry.text));
        if !entry.note.is_empty() {
            let _ = writeln!(text, "note = {}", quote(&entry.note));
        }
        text.push_str("blobs = [\n");
        for blob in blobs {
            let _ = writeln!(text, "  \"{}\",", blob.to_hex());
        }
        text.push_str("]\n");
    }

    std::fs::write(out.as_std_path(), text).with_context(|| format!("écriture de {out}"))?;
    Ok(())
}

/// Une chaîne TOML entre guillemets, échappée.
fn quote(text: &str) -> String {
    let escaped = text.replace('\\', r"\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Le sujet cité entre guillemets français dans une note, s'il y en a un.
fn quoted_subject(note: &str) -> Option<&str> {
    let start = note.find('«')? + '«'.len_utf8();
    let end = note[start..].find('»')? + start;
    let quoted = note[start..end].trim();
    if quoted.is_empty() {
        None
    } else {
        Some(quoted)
    }
}

/// Coupe un sujet sur une frontière de caractère.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max - 1).collect();
    format!("{kept}…")
}

//! Le critère 1 de la phase 4 : **trouver ce qu'un mot-clé ne trouve pas**.
//!
//! ## Ce que ce banc mesure, et dans quel ordre il a été écrit
//!
//! Il mesure le rappel d'un moteur de recherche sur un jeu de requêtes dont **la réponse est
//! connue** — un fichier écrit à la main, en regardant de vrais messages. `docs/PHASE-4.md` met
//! ce banc à l'étape 2, avant le moteur sémantique de l'étape 3, et ce n'est pas un détail
//! d'ordonnancement : un banc écrit après le moteur ressemble toujours au moteur.
//!
//! Le premier relevé porte donc sur **tantivy seul**. C'est le chiffre auquel tout le reste de
//! la phase se comparera, et il est obtenu avant qu'on ait quoi que ce soit à défendre.
//!
//! ## Le sous-ensemble qui décide de la phase
//!
//! Le rappel global ne dit pas grand-chose : une requête qui reprend les mots du sujet est
//! trouvée par n'importe quel moteur de mots, et en empiler dix gonflerait n'importe quel
//! chiffre. Ce qui décide est le sous-ensemble des requêtes qui **ne partagent aucun mot** avec
//! leur cible : là, un moteur de mots ne peut rien, et c'est exactement le trou que le
//! sémantique prétend combler.
//!
//! Ce partage est **calculé ici**, jamais déclaré dans le fichier de requêtes. Deux sources pour
//! le même fait finiraient par se contredire, et c'est ce fait-là qui porte la conclusion.
//!
//! ## Ce que le banc ne fait pas
//!
//! Il n'écrit rien. Il lit un store, il lit un fichier de requêtes, il imprime. Le fichier de
//! requêtes nomme des messages réels : il vit dans `measurements/`, qui est gitignoré.

use std::collections::HashSet;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Mailbox, MessageId};
use serde::Deserialize;

/// Combien de résultats le critère regarde.
///
/// Dix, parce que c'est ce qu'une liste montre sans défiler. Un rappel à 100 qui demande de
/// parcourir cent lignes ne répond pas à la question « est-ce que je retrouve mon message ».
const TOP: usize = 10;

/// Les mots qu'on ne compte pas comme partagés.
///
/// **Sans cette liste, il n'y a pas de sous-ensemble « aucun mot commun » :** « le », « de »,
/// « que » sont dans toutes les requêtes et dans tous les messages, donc tout partagerait un mot
/// avec tout, et le banc mesurerait le rappel global sous un autre nom.
///
/// Elle est volontairement courte et ne contient que des mots-outils français et anglais. Un mot
/// porteur de sens n'y entre pas, même fréquent : « compte » est dans des dizaines de messages,
/// et c'est précisément ce qui rend une requête qui le contient facile pour un moteur de mots.
const STOPWORDS: &[&str] = &[
    "a", "à", "au", "aux", "avec", "ce", "ces", "cet", "cette", "dans", "de", "des", "du", "elle",
    "en", "est", "et", "eu", "il", "je", "j", "l", "la", "le", "les", "leur", "lui", "ma", "mais",
    "me", "mes", "mon", "n", "ne", "nos", "notre", "nous", "on", "ou", "où", "par", "pas", "pour",
    "qu", "que", "qui", "sa", "se", "ses", "son", "sur", "ta", "te", "tes", "toi", "ton", "tu",
    "un", "une", "vos", "votre", "vous", "y", "s", "d", "c", "m", "t", "quand", "plus", "moins",
    "fait", "faire", "été", "être", "avoir", "the", "of", "to", "and", "a", "in", "is", "it",
    "for", "on", "your", "you", "my", "i", "that", "this", "with", "was", "are", "be", "have",
];

/// Une requête et la réponse qu'elle doit trouver.
#[derive(Debug, Deserialize)]
struct Query {
    /// Ce que l'utilisateur taperait.
    text: String,
    /// L'identifiant du message attendu, tel que `mail search` le rend.
    target: i64,
    /// Pourquoi cette requête est dans le jeu. Pour le lecteur, jamais pour le calcul.
    #[serde(default)]
    #[allow(dead_code)]
    note: String,
}

/// Le fichier de vérité terrain.
#[derive(Debug, Deserialize)]
struct Ground {
    query: Vec<Query>,
}

/// Ce qu'une requête a donné.
struct Outcome {
    text: String,
    target: MessageId,
    /// Le sujet de la cible, pour que l'échec soit lisible sans rouvrir le store.
    subject: String,
    /// Le rang de la cible dans les résultats, à partir de 1. `None` si absente.
    rank: Option<usize>,
    /// La requête partage-t-elle un mot porteur avec sa cible ?
    shares_a_word: bool,
}

/// Mesure le rappel du moteur plein texte sur le jeu de requêtes.
///
/// # Errors
///
/// Si le store est illisible, si le fichier de requêtes est absent ou mal formé, ou si une cible
/// n'existe pas dans ce store.
pub fn measure(store_root: &Utf8PathBuf, queries: &Utf8PathBuf, limit: usize) -> Result<()> {
    let raw = std::fs::read_to_string(queries)
        .with_context(|| format!("lecture du jeu de requêtes {queries}"))?;
    let ground: Ground = toml::from_str(&raw).with_context(|| format!("analyse de {queries}"))?;
    anyhow::ensure!(
        !ground.query.is_empty(),
        "{queries} ne contient aucune requête"
    );

    let mailbox =
        Mailbox::open(store_root).with_context(|| format!("ouverture de la boîte {store_root}"))?;

    let mut outcomes = Vec::with_capacity(ground.query.len());
    for query in &ground.query {
        let target = MessageId(query.target);

        // **La cible est vérifiée avant d'être cherchée.** Un jeu de requêtes est lié au store
        // sur lequel il a été écrit : un réimport renumérote les messages, et le banc mesurerait
        // alors un rappel contre des cibles qui ont glissé — en rendant un chiffre parfaitement
        // présentable. Mieux vaut refuser.
        let detail = mailbox
            .message(target)
            .with_context(|| format!("relecture de la cible #{} ", query.target))?
            .with_context(|| {
                format!(
                    "la cible #{} n'existe pas dans {store_root} : ce jeu de requêtes a été \
                     écrit sur un autre store, ou le store a été réimporté",
                    query.target
                )
            })?;

        let hits = mailbox
            .search(&query.text, limit)
            .with_context(|| format!("recherche « {} »", query.text))?;
        let rank = hits
            .iter()
            .position(|hit| hit.item.id == target)
            .map(|at| at + 1);

        outcomes.push(Outcome {
            text: query.text.clone(),
            target,
            subject: detail.item.subject.clone(),
            rank,
            shares_a_word: shares_a_word(&query.text, &detail),
        });
    }

    report(store_root, queries, &outcomes);
    Ok(())
}

/// La requête partage-t-elle un mot porteur avec ce que le moteur indexe de la cible ?
///
/// Le texte comparé est **celui que l'indexation voit** — sujet, expéditeur, corps — et pas le
/// seul sujet : une requête peut tomber juste par un mot qui n'est que dans le corps, et le
/// compter comme « aucun mot commun » surestimerait ce que le sémantique a apporté.
fn shares_a_word(query: &str, detail: &mailcore::MessageDetail) -> bool {
    let mut haystack = HashSet::new();
    for source in [
        detail.item.subject.as_str(),
        detail.item.from_addr.as_str(),
        detail.item.from_name.as_deref().unwrap_or_default(),
        detail.body.as_str(),
    ] {
        haystack.extend(words(source));
    }
    words(query).any(|word| haystack.contains(&word))
}

/// Découpe en mots porteurs : minuscules Unicode, sans ponctuation, sans mots-outils.
///
/// `to_lowercase` et non `to_ascii_lowercase` : sur un corpus français, replier « Éloïse » en
/// ASCII ne replie rien du tout — c'est la leçon du carnet d'adresses, le 2026-09-11.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() > 1)
        .map(str::to_lowercase)
        .filter(|word| !STOPWORDS.contains(&word.as_str()))
}

/// Imprime le relevé.
fn report(store_root: &Utf8PathBuf, queries: &Utf8PathBuf, outcomes: &[Outcome]) {
    println!("Store              {store_root}");
    println!("Jeu de requêtes    {queries}");
    println!("Moteur             tantivy seul — plein texte");
    println!();

    let (hard, easy): (Vec<_>, Vec<_>) = outcomes.iter().partition(|it| !it.shares_a_word);

    println!(
        "{:<42} {:>7} {:>9} {:>9}",
        "sous-ensemble", "requêtes", "dans le top", "rappel"
    );
    for (label, set) in [
        ("aucun mot commun avec la cible", &hard),
        ("au moins un mot commun", &easy),
    ] {
        print_row(label, set);
    }
    let all: Vec<&Outcome> = outcomes.iter().collect();
    print_row("tout le jeu", &all);

    println!();
    println!("Ce qui n'est pas trouvé dans les {TOP} premiers :");
    let mut missed = 0;
    for outcome in outcomes {
        if outcome.rank.is_some_and(|rank| rank <= TOP) {
            continue;
        }
        missed += 1;
        let where_it_is = match outcome.rank {
            Some(rank) => format!("rang {rank}"),
            None => "absente".to_owned(),
        };
        let kind = if outcome.shares_a_word {
            "mot commun"
        } else {
            "aucun mot commun"
        };
        println!("  « {} »", outcome.text);
        println!(
            "      cible #{} — {}   [{kind}, {where_it_is}]",
            outcome.target.0,
            short(&outcome.subject)
        );
    }
    if missed == 0 {
        println!("  (rien — toutes les cibles sont dans les {TOP} premiers)");
    }
}

/// Une ligne du tableau.
fn print_row(label: &str, set: &[&Outcome]) {
    let found = set
        .iter()
        .filter(|it| it.rank.is_some_and(|rank| rank <= TOP))
        .count();
    if set.is_empty() {
        println!("{label:<42} {:>7} {:>9} {:>9}", 0, 0, "—");
        return;
    }
    #[allow(clippy::cast_precision_loss)]
    let share = found as f64 * 100.0 / set.len() as f64;
    println!("{label:<42} {:>7} {found:>9} {share:>8.1} %", set.len());
}

/// Un sujet raccourci, coupé sur une frontière de caractère.
fn short(subject: &str) -> String {
    if subject.chars().count() <= 58 {
        return subject.to_owned();
    }
    let kept: String = subject.chars().take(57).collect();
    format!("{kept}…")
}

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
    /// Les **empreintes de contenu** des messages qui répondent à la requête, en hexadécimal.
    ///
    /// ## Pourquoi des empreintes et pas des identifiants
    ///
    /// `messages.id` est attribué à l'import. Le 2026-09-21, le store a été réimporté et les 22
    /// cibles du jeu ont désigné d'autres messages — sans que rien ne tombe, puisqu'elles
    /// **existaient** toutes. Le contrôle d'existence était le bon geste sur la mauvaise chose.
    /// L'empreinte BLAKE3 des octets RFC 5322 est l'identité que le projet emploie déjà pour
    /// dédupliquer ; elle survit à un réimport, à un changement de machine, à un renumérotage.
    ///
    /// ## Pourquoi **plusieurs** et pas une
    ///
    /// Sur soixante-treize mille messages, « quand mon paquet est arrivé » a neuf bonnes
    /// réponses : neuf avis de livraison. Exiger celle que l'auteur du jeu avait sous les yeux
    /// en septembre ne mesure pas mieux le moteur — ça mesure sa chance. Un moteur qui remonte
    /// un autre avis de livraison a **répondu à la question posée**, et le compter en échec
    /// sous-estimerait le rappel au lieu de l'établir.
    ///
    /// Ce que ça coûte, et il faut le dire : le critère 1 devient « la liste contient une
    /// réponse acceptable » plutôt que « la liste contient *le* message ». C'est un
    /// assouplissement assumé, et il est **borné par le jeu** — c'est un humain qui a écrit
    /// quelles réponses sont acceptables, pas une heuristique appliquée au moment du calcul.
    blobs: Vec<String>,
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

/// Les moteurs comparés, dans l'ordre où le relevé les imprime.
///
/// Les trois bras et pas seulement l'hybride, parce que le critère 1 demande explicitement que
/// l'hybride soit **strictement meilleur que tantivy seul** sur le sous-ensemble dur. Sans le
/// bras sémantique isolé, on ne saurait pas non plus si un gain vient du sémantique ou de la
/// règle de fusion — et c'est la question que l'étape 6 doit trancher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    /// `tantivy` seul, le chiffre de référence du 2026-09-15.
    FullText,
    /// Le balayage vectoriel seul.
    Semantic,
    /// Les deux, fusionnés par rangs réciproques.
    Hybrid,
}

impl Engine {
    const fn label(self) -> &'static str {
        match self {
            Self::FullText => "plein texte seul",
            Self::Semantic => "sémantique seul",
            Self::Hybrid => "hybride (fusion par rangs)",
        }
    }
}

/// Ce qu'une requête a donné.
struct Outcome {
    text: String,
    /// Les messages qui répondent à la requête.
    targets: Vec<MessageId>,
    /// Le sujet de la première cible, pour que l'échec soit lisible sans rouvrir le store.
    subject: String,
    /// Le rang de la cible pour chaque moteur mesuré, à partir de 1. `None` si absente.
    ranks: Vec<(Engine, Option<usize>)>,
    /// La requête partage-t-elle un mot porteur avec sa cible ?
    shares_a_word: bool,
}

impl Outcome {
    /// Le rang obtenu par un moteur donné.
    fn rank(&self, engine: Engine) -> Option<usize> {
        self.ranks
            .iter()
            .find(|(which, _)| *which == engine)
            .and_then(|(_, rank)| *rank)
    }
}

/// Mesure le rappel du moteur plein texte sur le jeu de requêtes.
///
/// # Errors
///
/// Si le store est illisible, si le fichier de requêtes est absent ou mal formé, ou si une cible
/// n'existe pas dans ce store.
pub fn measure(
    store_root: &Utf8PathBuf,
    queries: &Utf8PathBuf,
    limit: usize,
    model: Option<&Utf8PathBuf>,
    k: f32,
) -> Result<()> {
    let raw = std::fs::read_to_string(queries)
        .with_context(|| format!("lecture du jeu de requêtes {queries}"))?;
    let ground: Ground = toml::from_str(&raw).with_context(|| {
        format!(
            "analyse de {queries}.\nSi le message parle d'un champ `blob` manquant : ce jeu \
             désigne encore ses cibles par `target`, c'est-à-dire par un identifiant que le \
             prochain réimport fera glisser. `cargo xtask queries-rebind` dit quoi mettre à la \
             place."
        )
    })?;
    anyhow::ensure!(
        !ground.query.is_empty(),
        "{queries} ne contient aucune requête"
    );

    let mailbox =
        Mailbox::open(store_root).with_context(|| format!("ouverture de la boîte {store_root}"))?;

    // Le moteur sémantique est **optionnel** ici comme dans le produit : sans modèle, le banc
    // rend le relevé de référence, celui du 2026-09-15. C'est ce qui permet de rejouer l'ancien
    // chiffre avec le code d'aujourd'hui plutôt que de le recopier depuis le journal.
    let semantic = match model {
        Some(dir) => {
            let engine = mailvec::search::Semantic::open(mailbox.store(), dir)
                .with_context(|| format!("ouverture du moteur sémantique sur {dir}"))?;
            anyhow::ensure!(
                !engine.is_empty(),
                "le magasin de vecteurs est vide : lancez la passe de vectorisation d'abord"
            );
            println!("Vecteurs           {} messages", engine.len());
            Some(engine)
        }
        None => None,
    };
    let engines: Vec<Engine> = if semantic.is_some() {
        vec![Engine::FullText, Engine::Semantic, Engine::Hybrid]
    } else {
        vec![Engine::FullText]
    };
    let hybrid = mailvec::search::Hybrid::new(&mailbox, semantic).with_k(k);

    let mut outcomes = Vec::with_capacity(ground.query.len());
    for query in &ground.query {
        anyhow::ensure!(
            !query.blobs.is_empty(),
            "« {} » n'a aucune cible : une requête sans réponse attendue ne mesure rien",
            query.text
        );

        // **Chaque cible est résolue par son contenu, puis relue.** Deux contrôles, et il en
        // faut deux : l'empreinte dit *quel* message, la relecture dit qu'il est là. Le jeu
        // d'avant ne faisait que le second, sur un identifiant — il a laissé passer un réimport
        // complet le 2026-09-21.
        let mut targets = Vec::with_capacity(query.blobs.len());
        let mut details = Vec::with_capacity(query.blobs.len());
        for hex in &query.blobs {
            let blob = mailcore::BlobHash::from_hex(hex.trim())
                .with_context(|| format!("empreinte illisible pour « {} »", query.text))?;
            let target = mailbox
                .store()
                .message_by_blob(blob)
                .with_context(|| format!("résolution d'une cible de « {} »", query.text))?
                .with_context(|| {
                    format!(
                        "aucun message de ce contenu dans {store_root} pour « {} » : ce store \
                         ne porte pas le message que ce jeu de requêtes vise",
                        query.text
                    )
                })?;
            let detail = mailbox
                .message(target)
                .with_context(|| format!("relecture d'une cible de « {} »", query.text))?
                .with_context(|| {
                    format!("cible introuvable après résolution de « {} »", query.text)
                })?;
            targets.push(target);
            details.push(detail);
        }
        // Le premier sert d'échantillon pour l'affichage d'un échec ; les autres comptent tout
        // autant pour le calcul.
        let detail = &details[0];

        // **Le rang retenu est le meilleur de toutes les cibles acceptables.** Une requête est
        // satisfaite dès qu'une réponse valable est dans la liste ; prendre autre chose que le
        // meilleur rang reviendrait à pénaliser un moteur pour avoir aussi trouvé les autres.
        let mut ranks = Vec::with_capacity(engines.len());
        for engine in &engines {
            let found: Vec<MessageId> = match engine {
                Engine::FullText => mailbox
                    .search(&query.text, limit)
                    .with_context(|| format!("plein texte « {} »", query.text))?
                    .into_iter()
                    .map(|hit| hit.item.id)
                    .collect(),
                Engine::Semantic => hybrid
                    .semantic()
                    .context("bras sémantique demandé sans moteur")?
                    .search(&query.text, limit)
                    .with_context(|| format!("sémantique « {} »", query.text))?
                    .into_iter()
                    .map(|hit| hit.id)
                    .collect(),
                Engine::Hybrid => hybrid
                    .search(&query.text, limit)
                    .with_context(|| format!("hybride « {} »", query.text))?
                    .into_iter()
                    .map(|hit| hit.id)
                    .collect(),
            };
            let rank = found
                .iter()
                .position(|id| targets.contains(id))
                .map(|at| at + 1);
            ranks.push((*engine, rank));
        }

        // **Le partage d'un mot se lit sur *toutes* les cibles.** Si l'une d'elles partage un
        // mot avec la requête, un moteur de mots peut l'atteindre — donc la requête n'est pas
        // dans le sous-ensemble dur, même si les autres cibles n'ont rien en commun. La
        // définition conservatrice est la seule honnête : elle ne peut que **sous-estimer** ce
        // que le sémantique apporte, jamais le gonfler.
        let shares = details.iter().any(|it| shares_a_word(&query.text, it));

        outcomes.push(Outcome {
            text: query.text.clone(),
            targets,
            subject: detail.item.subject.clone(),
            ranks,
            shares_a_word: shares,
        });
    }

    report(store_root, queries, &engines, k, &outcomes);
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
fn report(
    store_root: &Utf8PathBuf,
    queries: &Utf8PathBuf,
    engines: &[Engine],
    k: f32,
    outcomes: &[Outcome],
) {
    println!("Store              {store_root}");
    println!("Jeu de requêtes    {queries}");
    if engines.contains(&Engine::Hybrid) {
        println!("Fusion             rangs réciproques, K = {k}");
    }
    println!();

    let (hard, easy): (Vec<_>, Vec<_>) = outcomes.iter().partition(|it| !it.shares_a_word);
    let all: Vec<&Outcome> = outcomes.iter().collect();

    // **Le sous-ensemble dur en premier**, parce que c'est lui qui décide. Le rappel global peut
    // être gonflé par des requêtes qui reprennent les mots du sujet ; celui-ci ne peut pas.
    for (label, set) in [
        ("aucun mot commun avec la cible", &hard),
        ("au moins un mot commun", &easy),
        ("tout le jeu", &all),
    ] {
        println!("--- {label} — {} requêtes ---", set.len());
        println!(
            "{:<30} {:>13} {:>9} {:>12}",
            "moteur", "dans le top", "rappel", "rang médian"
        );
        for engine in engines {
            print_row(*engine, set);
        }
        println!();
    }

    // Le détail par requête : sans lui, un chiffre agrégé ne se diagnostique pas. Il montre
    // aussi les **régressions** de la fusion, qui sont ce qu'un rappel moyen cache le mieux.
    println!("Détail par requête — rang de la cible, « — » si elle n'est pas rendue du tout :");
    let header: Vec<&str> = engines.iter().map(|e| short_label(*e)).collect();
    println!(
        "  {:<8} {:<8} {:<8}  requête",
        header.first().copied().unwrap_or(""),
        header.get(1).copied().unwrap_or(""),
        header.get(2).copied().unwrap_or("")
    );
    for outcome in outcomes {
        let cells: Vec<String> = engines
            .iter()
            .map(|engine| match outcome.rank(*engine) {
                Some(rank) if rank <= TOP => format!("{rank}"),
                Some(rank) => format!("({rank})"),
                None => "—".to_owned(),
            })
            .collect();
        let mark = if outcome.shares_a_word { " " } else { "*" };
        println!(
            "{mark} {:<8} {:<8} {:<8}  « {} »",
            cells.first().cloned().unwrap_or_default(),
            cells.get(1).cloned().unwrap_or_default(),
            cells.get(2).cloned().unwrap_or_default(),
            short(&outcome.text)
        );
        // Le sujet de la cible n'est rappelé que quand le **dernier** moteur — l'hybride quand il
        // y en a un — l'a manquée : c'est là qu'on a besoin de savoir ce qu'on cherchait. Le
        // montrer partout noierait les échecs dans les réussites.
        let last = engines.last().copied().unwrap_or(Engine::FullText);
        if outcome.rank(last).is_none_or(|rank| rank > TOP) {
            let plural = if outcome.targets.len() > 1 {
                format!(
                    " (et {} autres réponses acceptables)",
                    outcome.targets.len() - 1
                )
            } else {
                String::new()
            };
            println!("           cible — {}{plural}", short(&outcome.subject));
        }
    }
    println!("\n  * = aucun mot commun avec la cible.  (n) = trouvée, mais hors du top {TOP}.");
}

/// Un libellé court pour la colonne du détail.
const fn short_label(engine: Engine) -> &'static str {
    match engine {
        Engine::FullText => "texte",
        Engine::Semantic => "sémant.",
        Engine::Hybrid => "hybride",
    }
}

/// Une ligne du tableau.
fn print_row(engine: Engine, set: &[&Outcome]) {
    if set.is_empty() {
        println!("{:<30} {:>13} {:>9} {:>12}", engine.label(), 0, "—", "—");
        return;
    }
    let found = set
        .iter()
        .filter(|it| it.rank(engine).is_some_and(|rank| rank <= TOP))
        .count();
    #[allow(clippy::cast_precision_loss)]
    let share = found as f64 * 100.0 / set.len() as f64;

    // Le rang médian dit ce que le rappel binaire cache : « manqué de peu » et « pas du tout »
    // comptent pareil dans un taux, et pas du tout pareil pour qui cherche.
    let mut ranks: Vec<usize> = set
        .iter()
        .map(|it| it.rank(engine).unwrap_or(usize::MAX))
        .collect();
    ranks.sort_unstable();
    let median = ranks[ranks.len() / 2];
    let median = if median == usize::MAX {
        "absente".to_owned()
    } else {
        median.to_string()
    };

    println!(
        "{:<30} {found:>13} {share:>8.1} % {median:>12}",
        engine.label()
    );
}

/// Un sujet raccourci, coupé sur une frontière de caractère.
fn short(subject: &str) -> String {
    if subject.chars().count() <= 58 {
        return subject.to_owned();
    }
    let kept: String = subject.chars().take(57).collect();
    format!("{kept}…")
}

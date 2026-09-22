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
    /// La langue des cibles, qui dit si les atteindre franchit une frontière.
    ///
    /// **La plus favorable de toutes les cibles acceptables**, de la même façon que
    /// `shares_a_word` prend le `any` : si l'une des bonnes réponses est en français, la requête
    /// peut être satisfaite sans franchir de frontière, donc elle ne mesure pas l'interlangue.
    lang: Lang,
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
    bert: bool,
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
            let engine = if bert {
                mailvec::search::Semantic::open_bert(mailbox.store(), dir)
            } else {
                mailvec::search::Semantic::open(mailbox.store(), dir)
            }
            .with_context(|| format!("ouverture du moteur sémantique sur {dir}"))?;
            anyhow::ensure!(
                !engine.is_empty(),
                "le magasin de vecteurs est vide : lancez la passe de vectorisation d'abord"
            );
            println!("Vecteurs           {} messages", engine.len());
            // **Ce que le relevé du 2026-09-21 ne disait pas, et qui l'a rendu indéfendable.**
            // Un rappel se lit sur des vecteurs, et des vecteurs viennent d'un modèle et d'un
            // budget. Sans cette ligne, deux relevés du même jour sur deux magasins différents
            // se ressemblent — c'est exactement ce qui est arrivé.
            match engine.provenance() {
                Some(provenance) => println!("Provenance         {}", provenance.summary()),
                None => println!(
                    "Provenance         INCONNUE — magasin au format de version 1.\n\
                     \x20                  Les chiffres ci-dessous ne disent pas quel modèle ni \
                     quel budget les a produits,\n\x20                  donc ils ne se comparent à \
                     rien. Relancer `mail vectors --rebuild` pour les dater."
                ),
            }
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

        // Le français l'emporte dès qu'une cible acceptable est française — voir le champ.
        let langs: Vec<Lang> = details.iter().map(language_of).collect();
        let lang = if langs.contains(&Lang::French) {
            Lang::French
        } else if langs.contains(&Lang::English) {
            Lang::English
        } else {
            Lang::Undetermined
        };

        outcomes.push(Outcome {
            text: query.text.clone(),
            targets,
            subject: detail.item.subject.clone(),
            ranks,
            shares_a_word: shares,
            lang,
        });
    }

    report(store_root, queries, &engines, k, &outcomes);
    Ok(())
}

/// Les mots-outils qui ne se disent que dans une langue.
///
/// Ils servent à répondre à une seule question : **la requête doit-elle franchir une frontière de
/// langue pour atteindre sa cible ?** Le corpus est français et les requêtes le sont toutes ; ce
/// qui varie est la langue du message visé.
///
/// Le choix est conservateur, et il faut l'être : un mot qui existe dans les deux langues avec
/// des sens différents — « on », « a », « son », « car », « but », « or », « en » — est **exclu
/// des deux listes**, parce qu'un marqueur ambigu ne déplace pas une requête d'un sous-ensemble à
/// l'autre, il la déplace au hasard.
const FRENCH_MARKERS: &[&str] = &[
    "le",
    "la",
    "les",
    "des",
    "une",
    "vous",
    "votre",
    "vos",
    "nous",
    "notre",
    "nos",
    "pour",
    "avec",
    "dans",
    "est",
    "sont",
    "cette",
    "ces",
    "aux",
    "du",
    "au",
    "qui",
    "que",
    "sur",
    "ses",
    "être",
    "avez",
    "leur",
    "sera",
    "ont",
    "été",
    "plus",
    "mais",
    "tout",
    "tous",
    "toute",
    "déjà",
    "vers",
    "chez",
    "sans",
    "sous",
    "entre",
    "depuis",
    "merci",
    "bonjour",
    "cordialement",
];

/// Le pendant anglais. Voir [`FRENCH_MARKERS`] pour la règle d'exclusion.
const ENGLISH_MARKERS: &[&str] = &[
    "the", "your", "you", "this", "that", "with", "from", "have", "will", "been", "are", "and",
    "for", "not", "can", "has", "was", "were", "their", "our", "its", "all", "new", "now", "here",
    "please", "thanks", "hello", "click", "account", "free", "get", "more", "about", "would",
];

/// La langue d'un texte, telle qu'un comptage de mots-outils la voit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    /// La langue des requêtes : atteindre cette cible ne franchit aucune frontière.
    French,
    /// Une autre langue : l'atteindre demande au modèle de relier deux langues.
    English,
    /// Trop peu de marqueurs pour trancher. **Elle n'est pas rangée d'office avec l'une des
    /// deux** : un sous-ensemble qui absorbe ses cas douteux mesure ses doutes.
    Undetermined,
}

impl Lang {
    /// Ce qu'on affiche dans le détail par requête, pour qu'un humain puisse contredire.
    const fn mark(self) -> &'static str {
        match self {
            Self::French => "fr",
            Self::English => "en",
            Self::Undetermined => "??",
        }
    }
}

/// La langue du texte **que l'indexation voit** — sujet, expéditeur, corps.
///
/// Le sujet seul aurait laissé une cible du sous-ensemble dur sans le moindre mot-outil :
/// « Entry confirmed: … Giveaway » n'en porte aucun. Le texte complet n'a pas ce problème, et il
/// est **le bon texte** de toute façon — c'est celui que le modèle encode, donc celui dont la
/// langue décide si le modèle doit franchir une frontière.
fn language_of(detail: &mailcore::MessageDetail) -> Lang {
    let mut french = 0usize;
    let mut english = 0usize;
    for source in [
        detail.item.subject.as_str(),
        detail.item.from_name.as_deref().unwrap_or_default(),
        detail.body.as_str(),
    ] {
        for word in source
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
        {
            if FRENCH_MARKERS.contains(&word.as_str()) {
                french += 1;
            } else if ENGLISH_MARKERS.contains(&word.as_str()) {
                english += 1;
            }
        }
    }

    // Une marge, et pas une simple majorité. Un courriel français cite souvent un mot anglais, et
    // l'inverse aussi ; départager douze contre onze par le signe reviendrait à tirer à pile ou
    // face sur les cas mixtes, qui sont exactement ceux où la question est intéressante.
    let (high, low) = (french.max(english), french.min(english));
    if high < 3 || high < low * 2 {
        return Lang::Undetermined;
    }
    if french > english {
        Lang::French
    } else {
        Lang::English
    }
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

    // **Le sous-ensemble dur se scinde**, et c'est l'étape 6 ter.
    //
    // « Aucun mot commun » est un critère **syntaxique** : il retient à la fois la synonymie —
    // paquet/colis — et le passage d'une langue à l'autre, parce qu'une requête française ne
    // partage par construction aucun mot avec un sujet anglais. Les deux problèmes n'ont ni la
    // même difficulté ni la même réponse, et le 2026-09-22 a mesuré qu'un modèle peut être trois
    // fois meilleur sur le premier et indistinct sur le second. Une médiane qui les agrège ne
    // décrit aucun des deux.
    let synonymy: Vec<&Outcome> = hard
        .iter()
        .copied()
        .filter(|it| it.lang == Lang::French)
        .collect();
    let crosslingual: Vec<&Outcome> = hard
        .iter()
        .copied()
        .filter(|it| it.lang == Lang::English)
        .collect();
    let undetermined: Vec<&Outcome> = hard
        .iter()
        .copied()
        .filter(|it| it.lang == Lang::Undetermined)
        .collect();

    // **Le sous-ensemble dur en premier**, parce que c'est lui qui décide. Le rappel global peut
    // être gonflé par des requêtes qui reprennent les mots du sujet ; celui-ci ne peut pas.
    let mut sets: Vec<(String, &Vec<&Outcome>)> = vec![
        (
            "aucun mot commun — SYNONYMIE (cible française)".to_owned(),
            &synonymy,
        ),
        (
            "aucun mot commun — INTERLANGUE (cible anglaise)".to_owned(),
            &crosslingual,
        ),
    ];
    // Le lot indéterminé n'est imprimé que s'il existe. L'afficher vide à chaque relevé le ferait
    // devenir du décor, et c'est précisément la ligne qu'il faut remarquer quand elle apparaît.
    if !undetermined.is_empty() {
        sets.push((
            "aucun mot commun — langue INDÉTERMINÉE, à trancher à la main".to_owned(),
            &undetermined,
        ));
    }
    sets.push(("aucun mot commun — les deux ensemble".to_owned(), &hard));
    sets.push(("au moins un mot commun".to_owned(), &easy));
    sets.push(("tout le jeu".to_owned(), &all));

    for (label, set) in sets {
        let label = label.as_str();
        println!("--- {label} — {} requêtes ---", set.len());
        println!(
            "{:<30} {:>13} {:>9} {:>12}   rangs triés",
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
        // Deux marques et non une : l'étoile dit « aucun mot commun », la langue dit **lequel des
        // deux problèmes** cette requête pose. La seconde est calculée par un comptage de
        // mots-outils, donc elle peut se tromper — elle est imprimée pour qu'un humain puisse la
        // contredire, comme `queries-rebind` imprime la règle qui a relié chaque cible.
        let mark = if outcome.shares_a_word { " " } else { "*" };
        let lang = outcome.lang.mark();
        println!(
            "{mark}{lang} {:<8} {:<8} {:<8}  « {} »",
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
    println!(
        "  fr / en / ?? = langue du texte visé, comptée sur ses mots-outils. Une requête est\n  \
         française : « fr » pose un problème de synonymie, « en » y ajoute le passage d'une\n  \
         langue à l'autre. Les deux ne se comparent pas et ne se moyennent pas."
    );
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
        println!(
            "{:<30} {:>13} {:>9} {:>12}   []",
            engine.label(),
            0,
            "—",
            "—"
        );
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

    println!(
        "{:<30} {found:>13} {share:>8.1} % {:>12}   {}",
        engine.label(),
        median(&ranks),
        sorted_ranks(&ranks),
    );
}

/// Le rang médian, au sens ordinaire du terme.
///
/// ## Pourquoi ce n'est plus `ranks[len / 2]`
///
/// Ça l'était jusqu'au 2026-09-22, et c'est le **plus grand** des deux rangs du milieu quand le
/// lot est pair. Tant que les lots faisaient vingt-deux et dix requêtes, le biais passait
/// inaperçu ; la scission du sous-ensemble dur a fait des lots de trois et six, et sur six rangs
/// valant 1 277, 6 023, 7 039, 27 623, 34 252 et 35 428, l'ancienne règle annonçait **27 623**
/// comme médiane. La moitié des valeurs sont sous 7 039 : le chiffre décrivait le contraire de ce
/// qu'il prétendait.
///
/// La leçon est celle du 2026-09-21, encore d'un cran plus loin : **un résumé statistique est un
/// instrument, et un instrument se vérifie sur les lots qu'on va vraiment lui donner.** Celui-ci
/// avait été écrit pour n = 22.
///
/// Une cible absente vaut `usize::MAX`, et on ne la moyenne pas : si le rang supérieur du milieu
/// est absent, la médiane l'est aussi, et c'est ce que la ligne dit.
fn median(sorted: &[usize]) -> String {
    if sorted.is_empty() {
        return "—".to_owned();
    }
    let middle = sorted.len() / 2;
    if sorted[middle] == usize::MAX {
        return "absente".to_owned();
    }
    if sorted.len() % 2 == 1 {
        return sorted[middle].to_string();
    }
    // Les deux du milieu sont présents : la moyenne est la médiane ordinaire.
    ((sorted[middle - 1] + sorted[middle]) / 2).to_string()
}

/// Les rangs eux-mêmes, triés.
///
/// Sur des lots de trois à six requêtes, une médiane résume trop peu de chose pour qu'on la
/// croie sur parole. Les imprimer à côté **rend le résumé vérifiable** : c'est la même exigence
/// que le détail par requête, appliquée à la ligne qui prétend le condenser.
fn sorted_ranks(sorted: &[usize]) -> String {
    let shown: Vec<String> = sorted
        .iter()
        .take(12)
        .map(|rank| {
            if *rank == usize::MAX {
                "—".to_owned()
            } else {
                rank.to_string()
            }
        })
        .collect();
    let suite = if sorted.len() > 12 { " …" } else { "" };
    format!("[{}{suite}]", shown.join(" "))
}

/// Un sujet raccourci, coupé sur une frontière de caractère.
fn short(subject: &str) -> String {
    if subject.chars().count() <= 58 {
        return subject.to_owned();
    }
    let kept: String = subject.chars().take(57).collect();
    format!("{kept}…")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Le défaut trouvé le 2026-09-22, sur les lots que la scission a créés.
    ///
    /// Les six rangs sont ceux du sous-ensemble interlangue. L'ancienne règle — `sorted[len / 2]`
    /// — rendait 27 623 alors que la moitié des valeurs sont sous 7 039.
    #[test]
    fn the_median_of_an_even_set_is_between_the_two_middle_ranks() {
        let ranks = [1_277, 6_023, 7_039, 27_623, 34_252, 35_428];
        assert_eq!(median(&ranks), "17331");
        assert_ne!(median(&ranks), "27623", "l'ancienne règle est revenue");
    }

    #[test]
    fn the_median_of_an_odd_set_is_the_middle_rank() {
        assert_eq!(median(&[78, 1_304, 1_834]), "1304");
        assert_eq!(median(&[5]), "5");
    }

    /// Une cible absente ne se moyenne pas.
    ///
    /// Si le rang supérieur du milieu manque, la moitié du lot est introuvable : la médiane l'est
    /// aussi. La moyenner avec un rang présent fabriquerait un nombre à partir d'une absence.
    #[test]
    fn an_absent_target_makes_the_median_absent_rather_than_averaged() {
        assert_eq!(median(&[10, 20, usize::MAX, usize::MAX]), "absente");
        assert_eq!(median(&[usize::MAX; 3]), "absente");
        // Mais une seule absence sur quatre laisse les deux du milieu présents.
        assert_eq!(median(&[10, 20, 30, usize::MAX]), "25");
    }

    #[test]
    fn an_empty_set_has_no_median() {
        assert_eq!(median(&[]), "—");
    }

    /// Les rangs triés sont ce qui rend la médiane vérifiable, donc ils doivent être lisibles.
    #[test]
    fn the_sorted_ranks_show_absences_and_say_when_they_are_cut() {
        assert_eq!(sorted_ranks(&[1, 2, usize::MAX]), "[1 2 —]");
        let many: Vec<usize> = (1..=15).collect();
        let shown = sorted_ranks(&many);
        assert!(shown.ends_with("…]"), "une liste coupée doit le dire");
        assert!(shown.contains("12"));
        assert!(!shown.contains("13"));
    }
}

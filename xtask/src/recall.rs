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
//! chiffre. Ce qui décide est le sous-ensemble des requêtes qui ne partagent **aucun mot porteur**
//! avec leur cible, tel que l'index le voit : là, un moteur de mots n'atteint la cible que par
//! les mots-outils et les mots d'une lettre, c'est-à-dire par du bruit, et c'est exactement le trou
//! que le sémantique prétend combler.
//!
//! Ce partage est **calculé ici**, jamais déclaré dans le fichier de requêtes. Deux sources pour
//! le même fait finiraient par se contredire, et c'est ce fait-là qui porte la conclusion.
//!
//! ## Ce que le banc ne fait pas
//!
//! Il n'écrit rien. Il lit un store, il lit un fichier de requêtes, il imprime. Le fichier de
//! requêtes nomme des messages réels : il vit dans `measurements/`, qui est gitignoré.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Mailbox, MessageId};
use serde::Deserialize;

use crate::words::words;

/// Combien de résultats le critère regarde.
///
/// Dix, parce que c'est ce qu'une liste montre sans défiler. Un rappel à 100 qui demande de
/// parcourir cent lignes ne répond pas à la question « est-ce que je retrouve mon message ».
const TOP: usize = 10;

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
    /// Ce qu'un moteur de mots peut espérer, vu la rareté du mot commun le plus rare.
    reach: Reach,
    /// La langue des cibles, qui dit si les atteindre franchit une frontière.
    ///
    /// **La plus favorable de toutes les cibles acceptables**, de la même façon que la
    /// recherche du mot commun regarde toutes les cibles : si l'une des bonnes réponses est en français, la requête
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
/// Si le store est illisible, si le fichier de requêtes est absent ou mal formé, si une cible
/// n'existe pas dans ce store, ou si l'index plein texte ne couvre pas exactement les messages
/// indexables — auquel cas les fréquences de mots, donc la partition, seraient fausses.
pub fn measure(
    store_root: &Utf8PathBuf,
    queries: &Utf8PathBuf,
    limit: Option<usize>,
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

    // L'index doit couvrir le store, sinon ce banc ment sans le dire — voir `check_coverage`.
    check_coverage(&mailbox, store_root)?;

    // **Toute la profondeur, sauf demande contraire.** Un rang coupé n'est pas une absence, et le
    // relevé les confondait : la valeur par défaut de 50 rendait « absente » chez les trois
    // moteurs une cible que le plein texte classait 3 095ᵉ — le 2026-10-08, sur le relevé qui
    // devait refaire celui du 2026-09-23. La leçon de ce jour-là, « ce qui tronque doit le
    // dire », valait pour le code et pas pour la valeur par défaut qui le réglait.
    let messages = mailbox
        .store()
        .message_count()
        .context("compte des messages")?;
    let corpus = usize::try_from(messages).unwrap_or(usize::MAX);
    let limit = limit.unwrap_or(corpus).max(1);
    let truncated = limit < corpus;
    println!(
        "Profondeur         {limit} résultats par moteur — {}",
        if truncated {
            "COUPÉE : « — » veut dire classée au-delà, pas absente"
        } else {
            "tout le corpus, chaque rang est le vrai"
        }
    );

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
                    .search_deep(&query.text, limit)
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

        // **Le partage d'un mot se lit sur *toutes* les cibles**, telles que l'index les voit. Si
        // l'une d'elles partage un mot avec la requête, un moteur de mots peut l'atteindre — donc la
        // requête n'est pas dans le sous-ensemble dur, même si les autres cibles n'ont rien en
        // commun. Cette définition conservatrice ne peut que **sous-estimer** ce que le sémantique
        // apporte, jamais le gonfler.
        let reach = reach_of(rarest_shared_word(&mailbox, &query.text, &targets)?);

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
            reach,
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

/// La langue de la cible, lue sur son sujet, son expéditeur et son corps rendu.
///
/// Ce n'est **pas** tout le texte que l'index voit — ni les destinataires ni les pièces jointes n'y
/// sont —, et ce n'est pas grave ici : il s'agit de compter des mots-outils pour reconnaître une
/// langue, pas de décider si un mot est partagé. Cette seconde question, qui décide de la
/// partition, est posée à l'index lui-même — voir [`rarest_shared_word`].
///
/// Le sujet seul aurait laissé une cible du sous-ensemble dur sans le moindre mot-outil :
/// un sujet de concours en anglais n'en portait aucun. Le corps n'a pas ce problème, et c'est la
/// plus grosse part de ce que le modèle encode — sans en être la totalité, puisque le texte des
/// pièces jointes s'y ajoute depuis le 2026-09-23.
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

/// L'index plein texte couvre-t-il le store ? Sinon le banc refuse, et dit quoi faire.
///
/// Rend le nombre de messages sans blob — hors index et hors mesure — pour que le relevé le dise.
///
/// ## Pourquoi ce banc ne peut pas s'en passer
///
/// La partition repose sur la fréquence documentaire des mots, et un index vide ou incomplet rend
/// une fréquence de zéro pour tout : chaque requête serait rangée « sans mot commun », et le bras
/// plein texte n'aurait jamais de rang. Rien ne l'aurait signalé — un store fraîchement importé
/// porte un index **vide**, pas une absence d'index.
///
/// ## Et pourquoi la règle n'est pas écrite ici
///
/// Elle l'a été, deux fois, et fausse les deux : d'abord en comparant au nombre de messages — ce
/// qu'un seul blob manquant rendait faux pour toujours —, puis corrigée ici seulement, alors que
/// `mail doctor`, vers qui ce banc renvoie, faisait la même erreur. Elle vit maintenant dans
/// [`mailcore::index::Coverage`], et les deux l'appliquent.
///
/// # Errors
///
/// Si l'index est illisible, ou s'il ne porte pas exactement un document par message indexable.
fn check_coverage(mailbox: &Mailbox, store_root: &Utf8PathBuf) -> Result<u64> {
    let coverage = mailbox
        .index_coverage()
        .context("état de l'index plein texte")?
        .context(
            "l'index plein texte est illisible : ni les fréquences de mots ni les rangs plein \
             texte ne peuvent être mesurés. Lancez `mail doctor`.",
        )?;
    anyhow::ensure!(
        coverage.is_complete(),
        "l'index plein texte porte {} documents, attendu entre {} et {} : il ne couvre pas le \
         store, donc les fréquences de mots et les rangs plein texte seraient faux.\n\
         Lancez `mail index --store {store_root}` avant de mesurer.",
        coverage.documents,
        coverage.indexable(),
        coverage.messages
    );
    if coverage.missing_blobs > 0 {
        println!(
            "Blobs absents      {} messages sans blob, donc hors index et hors mesure — \
             `mail doctor` les décrit",
            coverage.missing_blobs
        );
    }
    Ok(coverage.missing_blobs)
}

/// Le mot commun le plus **rare** entre la requête et sa cible, mesuré par sa fréquence
/// documentaire dans le corpus.
///
/// ## Ce que ça remplace, et pourquoi
///
/// Jusqu'au 2026-09-23, le banc partitionnait sur `shares_a_word` : la requête partage-t-elle un
/// mot porteur avec sa cible, oui ou non. Quatre requêtes dictées par l'utilisateur l'ont réfuté
/// en une fois — « les sorties en deux-roues avec le club » partage le mot « club » avec sa
/// cible, et le plein texte la classe **au-delà de trente mille**. Le critère testait
/// l'**existence** d'un mot commun ; ce qui compte est son **pouvoir discriminant**.
///
/// La fréquence documentaire du mot commun le plus rare y répond exactement, et sans regarder le
/// classement d'aucun moteur — ce qui serait circulaire dès qu'on compare ces moteurs entre eux.
/// Sa lecture est littérale : une requête réduite à ce seul mot a **au plus `df` candidats**,
/// donc le mot suffit à mettre la cible dans les dix premiers si et seulement si `df ≤ TOP`.
///
/// Le seuil n'est donc pas inventé : c'est [`TOP`], qui est déjà motivé ailleurs. C'est la
/// première fois de la phase qu'un partage de sous-ensembles n'a pas de constante à défendre.
///
/// ## « Partagé » se demande à l'index, pas au message
///
/// La première version cherchait les mots partagés dans un texte recomposé à la main — sujet,
/// expéditeur, corps rendu — et une relecture contradictoire a trouvé qu'il manquait **quatre**
/// sources que l'index, lui, interroge : les destinataires, les parties HTML, le texte des pièces
/// jointes et la fin des corps longs. Une requête qui partageait un mot par l'une de ces voies
/// était rangée « sans mot commun », c'est-à-dire dans le sous-ensemble de la comparaison primaire.
/// Le biais allait dans le sens du plein texte, donc il ne gonflait pas le résultat, mais le
/// sous-ensemble ne contenait pas ce que son nom dit.
///
/// La question est donc posée à l'index — [`Mailbox::contains`] —, qui répond sur le même texte et
/// les mêmes champs que la recherche elle-même. Le découpage des mots de la requête, lui, reste
/// celui de [`crate::words`] : il coïncide avec celui de tantivy sur du texte ordinaire, et
/// `Searcher::contains` dit les cas limites où ce n'est pas vrai.
///
/// Rend `None` quand la requête n'a **aucun** mot commun avec ses cibles — le cas que l'ancien
/// critère appelait « dur », qui reste le plus dur de tous et se distingue d'un mot commun trop
/// fréquent.
///
/// # Errors
///
/// Si l'index est illisible. Le banc a vérifié plus haut qu'il couvre le store, donc c'est une
/// panne et non un état ordinaire : la taire rangerait des requêtes au hasard.
fn rarest_shared_word(
    mailbox: &Mailbox,
    query: &str,
    targets: &[MessageId],
) -> Result<Option<u64>> {
    let mut rarest: Option<u64> = None;
    for word in words(query) {
        let mut shared = false;
        for target in targets {
            shared |= mailbox
                .contains(*target, &word)
                .context("index plein texte")?
                .context("l'index plein texte est devenu illisible pendant la mesure")?;
        }
        if !shared {
            continue;
        }
        // Le mot est dans une cible, donc dans au moins un document : sa fréquence vaut au moins
        // un. Un zéro ici serait un index incohérent, pas un mot rare — d'où le refus.
        let freq = mailbox
            .doc_freq(&word)
            .context("fréquence documentaire")?
            .context("l'index plein texte est devenu illisible pendant la mesure")?;
        anyhow::ensure!(
            freq > 0,
            "« {word} » est dans une cible selon l'index, et dans aucun document selon le même \
             index : l'index est incohérent"
        );
        rarest = Some(rarest.map_or(freq, |least| least.min(freq)));
    }
    Ok(rarest)
}

/// Range une requête d'après la fréquence de son mot commun le plus rare.
///
/// Le seuil est [`TOP`] et sa lecture est littérale : un mot porté par au plus dix documents met
/// la cible dans les dix premiers à lui seul. Isolé ici pour se tester, parce que c'est lui qui
/// décide dans quel sous-ensemble tombe chaque requête — donc sur quoi porte la comparaison
/// primaire.
fn reach_of(rarest: Option<u64>) -> Reach {
    match rarest {
        None => Reach::None,
        Some(freq) if freq <= TOP as u64 => Reach::Word(freq),
        Some(freq) => Reach::Group(freq),
    }
}

/// Comment une requête se range, une fois sa cible et le corpus regardés.
///
/// Trois états et non deux, parce que le 2026-09-23 a montré qu'il en manquait un : entre « aucun
/// mot commun » et « trouvable par un mot », il y a le cas majoritaire — un mot commun qui range
/// la cible dans un **groupe** trop grand pour la liste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Aucun mot **porteur** commun avec aucune cible, tel que l'index le voit.
    ///
    /// Un moteur de mots peut encore y rendre la cible, et il le fait — le plein texte en classe
    /// une au rang 3 095 —, mais seulement par les **mots-outils** que tantivy indexe et que ce
    /// banc écarte : « le », « de », « que ». Ce n'est pas un accès, c'est du bruit, et c'est
    /// pourquoi ces rangs se comptent en milliers.
    None,
    /// Un mot commun, mais sa fréquence dépasse [`TOP`] : il situe sans désigner.
    Group(u64),
    /// Un mot commun assez rare pour porter la cible dans les dix premiers à lui seul.
    Word(u64),
}

impl Reach {
    /// Ce qu'on affiche dans le détail par requête.
    fn mark(self) -> String {
        match self {
            Self::None => "  —".to_owned(),
            Self::Group(freq) if freq >= 10_000 => format!("{:>3}k", freq / 1_000),
            Self::Group(freq) | Self::Word(freq) => format!("{freq:>4}"),
        }
    }
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

    let all: Vec<&Outcome> = outcomes.iter().collect();

    // **Les sous-ensembles décrivent, ils ne décident plus.**
    //
    // Le 2026-09-23 a montré qu'une cellule de trois requêtes ne départage rien : le test de
    // signe apparié le plus permissif ne descend pas sous p = 0,25 à n = 3, et aucune médiane
    // n'y change quelque chose. Ce qui conclut est la **comparaison primaire** que le critère 1
    // déclare — voir [`compare`] et [`PRIMARY`] — ; ces lignes-ci sont là pour lire.
    //
    // C'est pour ça que sous [`MIN_FOR_STATS`] requêtes, le banc n'imprime ni rappel ni
    // médiane : c'est le plus petit effectif où une décision survit à une voix contraire.
    // La constante porte sa justification ; ce commentaire en portait une copie, qui disait dix
    // quand la constante disait neuf — une relecture l'a trouvé, d'où le renvoi plutôt que la
    // copie.
    let hard: Vec<&Outcome> = outcomes
        .iter()
        .filter(|it| it.reach == Reach::None)
        .collect();
    let grouped: Vec<&Outcome> = outcomes
        .iter()
        .filter(|it| matches!(it.reach, Reach::Group(_)))
        .collect();
    let reachable: Vec<&Outcome> = outcomes
        .iter()
        .filter(|it| matches!(it.reach, Reach::Word(_)))
        .collect();

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
    sets.push((
        "un mot commun, mais trop fréquent pour désigner (df > 10)".to_owned(),
        &grouped,
    ));
    sets.push((
        "un mot commun assez rare pour suffire (df ≤ 10)".to_owned(),
        &reachable,
    ));
    sets.push(("tout le jeu".to_owned(), &all));

    for (label, set) in sets {
        let label = label.as_str();
        println!("--- {label} — {} requêtes ---", set.len());
        if set.len() < MIN_FOR_STATS {
            // Sous le seuil, on n'imprime que les rangs : un pourcentage sur trois requêtes
            // varie de 33 points quand une seule bouge, et une médiane de trois nombres est ce
            // nombre du milieu et rien d'autre. Les afficher inviterait à les lire.
            println!(
                "{:<30} {:>12}   rangs triés   (moins de {MIN_FOR_STATS} requêtes : \
                 ni rappel ni médiane)",
                "moteur", ""
            );
            for engine in engines {
                let mut ranks: Vec<usize> = set
                    .iter()
                    .map(|it| it.rank(*engine).unwrap_or(usize::MAX))
                    .collect();
                ranks.sort_unstable();
                println!(
                    "{:<30} {:>12}   {}",
                    engine.label(),
                    "",
                    sorted_ranks(&ranks)
                );
            }
            println!();
            continue;
        }
        println!(
            "{:<30} {:>11} {:>7} {:>13} {:>12}   rangs triés",
            "moteur", "dans le top", "rappel", "IC 95 %", "rang médian"
        );
        for engine in engines {
            print_row(*engine, set);
        }
        println!();
    }

    compare(engines, &all, &hard);

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
        // Deux marques et non une : la fréquence du mot commun le plus rare dit **si** un moteur de
        // mots peut atteindre la cible — « — » s'il n'y a aucun mot porteur commun —, la langue dit
        // **lequel des deux problèmes** cette requête pose. La seconde est calculée par un comptage de
        // mots-outils, donc elle peut se tromper — elle est imprimée pour qu'un humain puisse la
        // contredire, comme `queries-rebind` imprime la règle qui a relié chaque cible.
        let mark = outcome.reach.mark();
        let lang = outcome.lang.mark();
        println!(
            "{mark} {lang} {:<8} {:<8} {:<8}  « {} »",
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
    println!(
        "\n  Première colonne : fréquence documentaire du mot porteur commun le plus rare, « — » \
         s'il n'y en a\n  aucun. Colonnes de rang : « — » = cible non rendue, (n) = trouvée hors du \
         top {TOP}."
    );
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
            "{:<30} {:>11} {:>7} {:>13} {:>12}   []",
            engine.label(),
            0,
            "—",
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

    // L'intervalle de Wilson à côté du taux : le critère 1 est un seuil, et un taux sur une
    // dizaine de requêtes sans son intervalle se lit comme une certitude qu'il n'est pas.
    let (low, high) = wilson(found, set.len());
    println!(
        "{:<30} {found:>11} {share:>5.1} % {:>13} {:>12}   {}",
        engine.label(),
        format!("[{low:.0} – {high:.0} %]"),
        median(&ranks),
        sorted_ranks(&ranks),
    );
}

/// L'effectif minimal sous lequel le banc n'imprime ni rappel ni médiane.
///
/// Neuf, et le nombre est motivé plutôt que choisi : c'est le **plus petit** effectif où une
/// décision à 5 % bilatéral survit à une voix contraire — 8 victoires sur 9 donnent p = 0,039.
/// À huit, il faut l'unanimité (7 sur 8 donne p = 0,070) ; à cinq, aucune répartition ne conclut.
///
/// La première version disait dix, sur la foi d'un exemple — 9 sur 10, p = 0,021 — qui est juste
/// mais pas minimal. Le test l'a attrapé. C'est exactement ce qu'on demande à un seuil : que sa
/// raison écrite soit vérifiable, et vérifiée.
///
/// En dessous, un pourcentage et une médiane ne sont pas imprécis : ils tiennent à une requête.
const MIN_FOR_STATS: usize = 9;

/// La comparaison que le critère 1 déclare, et la seule qui ait le droit de conclure.
///
/// ## D'où elle vient, et ce qui en a été choisi après coup
///
/// Le critère 1 de `docs/PHASE-4.md` demande, depuis le 2026-09-15, que la recherche de la phase
/// soit « strictement mieux que tantivy seul sur le sous-ensemble des requêtes qui ne partagent
/// aucun mot avec le message visé ». Les **deux bras** et le **sous-ensemble** sont donc déclarés
/// d'avance, avant qu'aucun moteur sémantique n'existe : c'est ce qui en fait une comparaison
/// primaire et non une piste trouvée en fouillant les chiffres.
///
/// Ce qui ne l'était pas, et qui doit être dit : **la grandeur mesurée et le test**. Le critère
/// parlait de rappel dans les dix premiers ; ce qui est comparé ici est le **rang** de la cible, et
/// le test de signe apparié qui le compare a été choisi le 2026-09-23, **après** les premiers
/// relevés — le résultat sur ce sous-ensemble avait même été compté à la main avant d'être écrit
/// ici. Sur la grandeur d'origine, le rappel, les trois moteurs sont à zéro et rien ne se départage. Une statistique choisie en connaissant le résultat n'est pas sans biais, et le
/// relevé l'imprime pour que personne ne le lise comme une prédiction confirmée.
const PRIMARY: (Engine, Engine) = (Engine::FullText, Engine::Hybrid);

/// Ce que la comparaison de deux moteurs a donné, requête par requête.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Paired {
    /// Requêtes où le premier moteur met la cible plus haut que le second.
    wins: usize,
    /// Requêtes où c'est l'inverse.
    losses: usize,
    /// Même rang des deux côtés : sortis du test de signe.
    ties: usize,
    /// Cible absente des **deux** listes : sortie du test de signe **et** du rapport des rangs.
    ///
    /// Séparée des ex æquo depuis le 2026-09-23 : les confondre faisait imprimer que ces requêtes
    /// « ne comptent que dans le signe », alors qu'elles ne comptent nulle part. Une relecture
    /// contradictoire l'a trouvé — trois sceptiques sur trois.
    neither: usize,
    /// Requêtes où **les deux** rendent la cible, seules à entrer dans le rapport des rangs.
    both: usize,
    /// Le rapport géométrique `rang du second / rang du premier`, sur les `both` requêtes.
    ///
    /// Inférieur à 1 veut dire que le second met la cible plus haut. `None` si aucune requête
    /// n'est rendue par les deux.
    ratio: Option<f64>,
    /// La p-value bilatérale du test de signe.
    p: f64,
}

/// Compare deux moteurs, requête par requête.
///
/// ## Ce que le rapport des rangs ne voit pas
///
/// Il ne se calcule que sur les requêtes où **les deux** moteurs rendent la cible : on ne prend
/// pas le logarithme d'une absence. C'est un biais, et son sens est connu dans le cas qui compte :
/// quand un moteur trouve nettement plus souvent que l'autre, les requêtes écartées sont ses
/// victoires les plus larges, donc le rapport **sous-estime** son avantage. Ce n'est pas une loi —
/// si les trouvailles unilatérales se partageaient, le biais pourrait aller dans l'autre sens. La première version l'imprimait sans
/// le dire ; il est maintenant accompagné du nombre de requêtes qui l'ont produit.
///
/// Le test de signe, lui, compte ces requêtes — un moteur qui trouve contre un moteur qui ne
/// trouve pas, c'est une victoire sans ambiguïté.
fn paired(left: Engine, right: Engine, set: &[&Outcome]) -> Paired {
    let mut wins = 0usize;
    let mut losses = 0usize;
    let mut ties = 0usize;
    let mut neither = 0usize;
    let mut logs: Vec<f64> = Vec::new();

    for outcome in set {
        match (outcome.rank(left), outcome.rank(right)) {
            (None, None) => neither += 1,
            (Some(_), None) => wins += 1,
            (None, Some(_)) => losses += 1,
            (Some(a), Some(b)) => {
                #[allow(clippy::cast_precision_loss)]
                logs.push((b as f64).log2() - (a as f64).log2());
                match a.cmp(&b) {
                    std::cmp::Ordering::Less => wins += 1,
                    std::cmp::Ordering::Greater => losses += 1,
                    std::cmp::Ordering::Equal => ties += 1,
                }
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let ratio = (!logs.is_empty()).then(|| 2f64.powf(logs.iter().sum::<f64>() / logs.len() as f64));
    Paired {
        wins,
        losses,
        ties,
        neither,
        both: logs.len(),
        ratio,
        p: sign_test(wins, wins + losses),
    }
}

/// Ce que la comparaison a le droit de dire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Le premier moteur est meilleur.
    Left,
    /// Le second est meilleur.
    Right,
    /// Le test ne passe pas à 5 %.
    Undecided,
    /// Trop peu de requêtes **décidées** — gagnées par l'un ou l'autre — pour qu'une conclusion
    /// survive à une voix contraire. Voir [`MIN_FOR_STATS`].
    TooFewDecided,
}

/// Le verdict, en fonction pure pour qu'il se teste. **Le test de signe décide, et lui seul.**
///
/// ## Deux règles, et l'histoire d'une troisième qui a été retirée
///
/// **Le test de signe décide**, à 5 % bilatéral.
///
/// **Il faut assez de requêtes décidées, pas seulement assez de requêtes.** Le seuil de
/// [`MIN_FOR_STATS`] s'appliquait d'abord à la taille du sous-ensemble, alors que le test ne compte
/// que les victoires et les défaites : dix requêtes dont quatre que personne ne trouve donnaient six
/// décisions, et un verdict que le seuil prétendait interdire.
///
/// **Le rapport des rangs ne décide pas**, et ce n'est pas faute d'avoir essayé. Trois relectures
/// contradictoires, le 2026-09-23, y ont chacune trouvé quelque chose :
///
/// 1. le commentaire promettait que « A meilleur que B » exigeait un rapport allant dans le sens du
///    signe, et le code ne le vérifiait pas ;
/// 2. la règle ajoutée pour tenir la promesse était absurde : un rapport calculé sur **une seule**
///    requête rendue par les deux annulait un 9 contre 1 ;
/// 3. la restreindre aux rapports calculés sur neuf requêtes au moins ne réglait rien : trente
///    trouvailles d'un seul côté et neuf requêtes partagées, réparties cinq contre quatre, donnaient
///    encore « ni l'un ni l'autre n'est meilleur » pour un moteur qui l'était nettement.
///
/// La cause était la même les trois fois, et c'est elle qu'il fallait voir plutôt que de régler un
/// seuil : le rapport ne se calcule que sur les requêtes rendues **par les deux**, donc il ignore par
/// construction les victoires les plus nettes — trouver ce que l'autre ne trouve pas. Un arbitre qui
/// ne voit pas les preuves les plus fortes ne peut pas arbitrer. Il faudrait, pour qu'il le puisse,
/// donner un rang aux absences — elles sont **censurées**, pas inconnues — et c'est un modèle à
/// écrire et à justifier, pas un seuil à retoucher.
///
/// En attendant, le rapport est **imprimé** comme taille d'effet, avec le nombre de requêtes sur
/// lequel il repose, et [`ratio_disagrees`] signale quand il va dans l'autre sens : c'est une
/// information à lire, parce qu'un moteur peut gagner souvent de peu, et ce n'est pas un veto.
fn verdict(result: &Paired) -> Verdict {
    if result.wins + result.losses < MIN_FOR_STATS {
        return Verdict::TooFewDecided;
    }
    if result.p >= 0.05 {
        return Verdict::Undecided;
    }
    // Des victoires et des défaites à égalité donnent p = 1 : le test ne passe pas, donc ici l'un
    // des deux l'emporte strictement.
    if result.wins > result.losses {
        Verdict::Left
    } else {
        Verdict::Right
    }
}

/// Le rapport des rangs va-t-il dans l'autre sens que le signe, sur assez de requêtes pour qu'on le
/// signale ?
///
/// Un avertissement et non un verdict — voir [`verdict`]. Sous [`MIN_FOR_STATS`] requêtes rendues
/// par les deux, le rapport tient à trop peu de chose pour qu'un désaccord veuille dire quoi que ce
/// soit, et le signaler inviterait à le lire.
fn ratio_disagrees(result: &Paired) -> bool {
    let Some(ratio) = result.ratio else {
        return false;
    };
    if result.both < MIN_FOR_STATS {
        return false;
    }
    // Un rapport inférieur à 1 veut dire que le second moteur met la cible plus haut.
    (result.wins > result.losses && ratio < 1.0) || (result.losses > result.wins && ratio > 1.0)
}

/// Imprime une comparaison et son verdict.
fn report_pair(left: Engine, right: Engine, set: &[&Outcome]) {
    let result = paired(left, right, set);
    let decided = result.wins + result.losses;
    let (l, r) = (short_label(left), short_label(right));

    println!("{l} contre {r}");
    println!(
        "  {} victoires, {} défaites, {} ex æquo, {} trouvées par aucun des deux   p = {:.3}",
        result.wins, result.losses, result.ties, result.neither, result.p
    );
    // Chaque requête a une place précise, et la première version les confondait. Rendue par les
    // deux : dans le rapport, et dans le signe sauf ex æquo. Rendue d'un seul côté : dans le
    // signe seulement. Rendue par aucun : nulle part.
    let one_sided = set.len() - result.both - result.neither;
    match result.ratio {
        Some(ratio) => println!(
            "  rang de « {r} » ÷ rang de « {l} » = ×{ratio:.2} en moyenne géométrique, sur les {} \
             requêtes où les deux rendent la cible ; les {one_sided} trouvées d'un seul côté ne \
             comptent que dans le signe, les {} trouvées par aucun ne comptent nulle part",
            result.both, result.neither
        ),
        None => println!("  aucune requête rendue par les deux : pas de rapport des rangs"),
    }
    if result.ratio.is_some() && result.both < MIN_FOR_STATS {
        println!(
            "  (moins de {MIN_FOR_STATS} requêtes rendues par les deux : le rapport tient à trop \
             peu de chose pour qu'on lise quoi que ce soit dans son sens)"
        );
    }

    match verdict(&result) {
        Verdict::Left => println!("  → « {l} » est meilleur que « {r} » (p = {:.3})", result.p),
        Verdict::Right => println!("  → « {r} » est meilleur que « {l} » (p = {:.3})", result.p),
        Verdict::TooFewDecided => println!(
            "  → indécidable : {decided} requêtes décidées seulement, moins de {MIN_FOR_STATS} — \
             les ex æquo et les requêtes trouvées par aucun des deux n'en font pas partie"
        ),
        // Au moins neuf décisions, donc `victories_needed` a toujours une réponse : de neuf à
        // n, la victoire unanime conclut. Le repli sur l'effectif lui-même n'est jamais atteint,
        // et il dirait la vérité s'il l'était.
        Verdict::Undecided => println!(
            "  → indécidable à n = {decided} : il en aurait fallu {} sur {decided}",
            victories_needed(decided).unwrap_or(decided)
        ),
    }
    // Un avertissement, pas un verdict : voir `verdict` pour pourquoi le rapport ne décide pas.
    if ratio_disagrees(&result) {
        println!(
            "  ! le rapport des rangs, sur les {} requêtes rendues par les deux, va dans l'autre \
             sens : les victoires sont peut-être nombreuses et petites. Le signe décide ; ceci \
             est à lire.",
            result.both
        );
    }
    println!();
}

/// Ce que la comparaison primaire peut faire, vu les moteurs mesurés et l'effectif.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Primary {
    /// L'un des deux bras n'a pas été mesuré — un relevé sans modèle, par exemple.
    Unavailable,
    /// Les deux bras sont là, mais trop peu de requêtes pour qu'une décision survive à une voix
    /// contraire : elle est annoncée, et rien n'est conclu.
    TooFew,
    /// Elle est faite.
    Run,
}

/// Ce que [`compare`] imprimera, décidé avant d'imprimer.
///
/// ## Pourquoi une fonction à part
///
/// Une relecture contradictoire a jugé **tautologique** le premier test de la comparaison
/// primaire : il vérifiait la valeur d'une constante, pas que `compare` la fasse sur le bon
/// sous-ensemble ni qu'il la retire des pistes. Ces deux propriétés vivaient au milieu de code
/// qui imprime, donc aucun test ne pouvait les voir. Les sortir ici les rend testables.
struct Plan<'a> {
    /// La comparaison que le critère 1 déclare.
    primary: Primary,
    /// L'ensemble sur lequel elle porte. Porté par le plan, et non repris par `compare`, pour
    /// qu'un test puisse vérifier que c'est bien le sous-ensemble sans mot commun.
    primary_set: &'a [&'a Outcome],
    /// Les pistes, dans l'ordre d'impression : ensemble, puis bras gauche et droit.
    exploratory: Vec<(&'static str, &'a [&'a Outcome], Engine, Engine)>,
}

/// Cette paire est-elle la comparaison primaire, dans un sens ou dans l'autre ?
fn is_primary(left: Engine, right: Engine) -> bool {
    (left, right) == PRIMARY || (right, left) == PRIMARY
}

/// Décide ce qui est comparé, et sur quoi.
///
/// La primaire porte sur les requêtes **sans mot commun** et elle en est retirée des pistes, pour
/// qu'une même comparaison ne soit pas lue deux fois — une fois comme preuve, une fois comme
/// piste. Un ensemble sous [`MIN_FOR_STATS`] n'entre pas dans les pistes : le compte annoncé en
/// tête doit être celui de ce qui est réellement comparé.
fn plan<'a>(engines: &[Engine], all: &'a [&'a Outcome], hard: &'a [&'a Outcome]) -> Plan<'a> {
    let (first, second) = PRIMARY;
    let primary = if !(engines.contains(&first) && engines.contains(&second)) {
        Primary::Unavailable
    } else if hard.len() < MIN_FOR_STATS {
        Primary::TooFew
    } else {
        Primary::Run
    };

    let mut exploratory = Vec::new();
    for (label, set, is_hard) in [("tout le jeu", all, false), ("sans mot commun", hard, true)] {
        if set.len() < MIN_FOR_STATS {
            continue;
        }
        for (index, left) in engines.iter().enumerate() {
            for right in engines.iter().skip(index + 1) {
                // Dans les deux ordres : la paire est la même quel que soit l'ordre des moteurs mesurés,
                // et une exclusion qui en dépendrait recompterait la primaire comme piste le jour où
                // l'ordre changerait.
                if is_hard && is_primary(*left, *right) {
                    continue;
                }
                exploratory.push((label, set, *left, *right));
            }
        }
    }
    Plan {
        primary,
        primary_set: hard,
        exploratory,
    }
}

/// Les comparaisons appariées : la primaire d'abord, les pistes ensuite.
///
/// ## Pourquoi une seule a le droit de conclure
///
/// Trois moteurs font trois paires ; sur deux ensembles de requêtes — tout le jeu, et celles
/// sans mot commun — cela fait six comparaisons, dont une primaire et cinq exploratoires. Le
/// risque qu'au moins une piste sorte « significative » par hasard est imprimé en tête, calculé
/// sur ce qui est réellement comparé.
/// La primaire est celle que le critère 1 a déclarée d'avance — voir [`PRIMARY`] ; toutes les
/// autres sont imprimées comme **exploratoires**, et une piste ne se lit pas comme une preuve.
fn compare(engines: &[Engine], all: &[&Outcome], hard: &[&Outcome]) {
    if engines.len() < 2 {
        return;
    }
    let (first, second) = PRIMARY;
    let plan = plan(engines, all, hard);

    if plan.primary != Primary::Unavailable {
        println!(
            "=== COMPARAISON PRIMAIRE — déclarée par le critère 1 : « {} » contre « {} », sur les \
             {} requêtes sans mot commun ===",
            short_label(first),
            short_label(second),
            plan.primary_set.len()
        );
        println!(
            "Les bras et le sous-ensemble sont déclarés depuis le 2026-09-15. La grandeur comparée — le\n\
             rang, et non plus la présence dans les dix premiers — et le test de signe ont été\n\
             choisis le 2026-09-23, après les premiers relevés : ce n'est pas une prédiction confirmée.\n"
        );
        if plan.primary == Primary::TooFew {
            println!(
                "  moins de {MIN_FOR_STATS} requêtes : une décision ne survivrait pas à une voix \
                 contraire, rien n'est conclu\n"
            );
        } else {
            report_pair(first, second, plan.primary_set);
        }
    }

    if plan.exploratory.is_empty() {
        return;
    }
    println!(
        "=== COMPARAISONS EXPLORATOIRES — des pistes, pas des preuves : {} comparaisons à 5 %, \
         donc jusqu'à {} % de chances qu'au moins une sorte « significative » par hasard ===",
        plan.exploratory.len(),
        // La borne de Bonferroni, et non 1 − 0,95^m : celle-ci suppose des tests indépendants et
        // de niveau exactement 5 %, ce qu'ils ne sont pas — les comparaisons partagent leurs
        // requêtes, et un test de signe exact est conservateur. La première version imprimait la
        // formule indépendante comme un fait ; la borne, elle, tient quelle que soit la
        // dépendance.
        (plan.exploratory.len() * 5).min(100)
    );
    println!(
        "Test de signe exact bilatéral. Une cible absente chez un seul des deux compte comme une\n\
         défaite pour lui ; absente des deux, la requête ne compte nulle part.\n"
    );
    let mut current = "";
    for (label, set, left, right) in plan.exploratory {
        if label != current {
            println!("--- sur {label} — {} requêtes ---", set.len());
            current = label;
        }
        report_pair(left, right, set);
    }
}

/// L'intervalle de confiance de Wilson à 95 % pour une proportion, en pourcents.
///
/// La **première moitié** du critère 1 est un seuil — 80 % des requêtes dans les dix premiers — et
/// un seuil ne se juge pas comme une comparaison : la seconde moitié, « mieux que tantivy », est
/// celle de [`PRIMARY`]. Un effectif petit ne l'invalide pas, il élargit l'intervalle, et il faut le
/// montrer : 0 sur 10 n'est pas « zéro », c'est « entre 0 et 28 % ». Wilson plutôt que
/// l'intervalle normal, qui donne des bornes négatives ou supérieures à 100 % sur de petits
/// effectifs et s'effondre sur une proportion de zéro — exactement le cas du sous-ensemble dur.
fn wilson(found: usize, total: usize) -> (f64, f64) {
    if total == 0 {
        return (0.0, 100.0);
    }
    const Z: f64 = 1.959_963_984_540_054;
    #[allow(clippy::cast_precision_loss)]
    let (k, n) = (found as f64, total as f64);
    let share = k / n;
    let z2 = Z * Z;
    let denominator = 1.0 + z2 / n;
    let center = (share + z2 / (2.0 * n)) / denominator;
    let half = Z * (share * (1.0 - share) / n + z2 / (4.0 * n * n)).sqrt() / denominator;
    (
        ((center - half) * 100.0).max(0.0),
        ((center + half) * 100.0).min(100.0),
    )
}

/// La p-value bilatérale d'un test de signe exact.
///
/// Calculée par somme de binomiales à p = 1/2, sans fonction gamma : les effectifs sont de
/// quelques dizaines, donc un produit itératif suffit et reste exact. Un test qui se trompe sur
/// ses propres bornes dirait n'importe quoi avec assurance.
fn sign_test(wins: usize, decided: usize) -> f64 {
    if decided == 0 {
        return 1.0;
    }
    let extreme = wins.max(decided - wins);
    // P(X ≥ extreme) + P(X ≤ decided − extreme), symétrique donc deux fois la queue.
    let mut tail = 0f64;
    for k in extreme..=decided {
        tail += binomial(decided, k);
    }
    (2.0 * tail / 2f64.powi(i32::try_from(decided).unwrap_or(i32::MAX))).min(1.0)
}

/// Le coefficient binomial, en flottant pour éviter le débordement.
fn binomial(n: usize, k: usize) -> f64 {
    let k = k.min(n - k);
    let mut value = 1f64;
    for step in 0..k {
        #[allow(clippy::cast_precision_loss)]
        {
            value = value * (n - step) as f64 / (step + 1) as f64;
        }
    }
    value
}

/// Combien de victoires il faudrait, sur cet effectif, pour conclure à 5 % bilatéral.
///
/// Imprimé quand le test ne conclut pas : « indécidable » sans cette borne laisse croire qu'on
/// est passé près. Sous six décisions, aucune répartition ne conclut ; de six à huit, seule
/// l'unanimité conclut — six sur six donne p = 0,031.
fn victories_needed(decided: usize) -> Option<usize> {
    (decided.div_ceil(2)..=decided).find(|&wins| sign_test(wins, decided) < 0.05)
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
    /// Le test de signe, sur les bornes que le banc imprime.
    ///
    /// Elles sont vérifiées contre la table binomiale : ce sont elles qui décident si une phrase
    /// du journal a le droit de dire « meilleur », et un test qui se trompe sur ses propres
    /// bornes dirait n'importe quoi avec assurance.
    #[test]
    fn the_sign_test_matches_the_binomial_table() {
        // Pile ou face parfait : rien à conclure.
        assert!((sign_test(5, 10) - 1.0).abs() < 1e-9);
        // 9 sur 10 → p = 2 × 11 / 1024.
        assert!((sign_test(9, 10) - 0.021_484_375).abs() < 1e-9);
        // 10 sur 10 → p = 2 / 1024.
        assert!((sign_test(10, 10) - 0.001_953_125).abs() < 1e-9);
        // Symétrique : perdre 9 fois sur 10 est aussi improbable que gagner 9 fois.
        assert!((sign_test(1, 10) - sign_test(9, 10)).abs() < 1e-12);
        // Aucun résultat décidé : pas de conclusion, et surtout pas de division par zéro.
        assert!((sign_test(0, 0) - 1.0).abs() < 1e-9);
    }

    /// Une requête fabriquée, dont seuls les rangs comptent.
    fn outcome(text: &str, full_text: Option<usize>, hybrid: Option<usize>) -> Outcome {
        Outcome {
            text: text.to_owned(),
            targets: Vec::new(),
            subject: String::new(),
            ranks: vec![(Engine::FullText, full_text), (Engine::Hybrid, hybrid)],
            reach: Reach::None,
            lang: Lang::French,
        }
    }

    /// Ce que `paired` compte, et surtout ce qu'il ne mélange pas — sur les **six** issues possibles.
    ///
    /// Une absence d'un seul côté est une victoire nette pour l'autre ; une absence des deux côtés
    /// ne compte nulle part, et n'est **pas** un ex æquo ; le rapport des rangs ne voit que les
    /// requêtes rendues par les deux.
    ///
    /// ## Pourquoi le rapport attendu n'est pas 1
    ///
    /// La première version de ce test avait un rapport géométrique de **exactement 1** : un rapport
    /// calculé à l'envers aurait passé. Une relecture contradictoire l'a relevé. Ici il vaut
    /// 2^−0,5 ≈ 0,707, et son inverse ≈ 1,414 ferait tomber le test.
    #[test]
    fn paired_counts_every_branch_and_the_ratio_points_the_right_way() {
        let set = [
            outcome("les deux, l'hybride devant", Some(400), Some(100)),
            outcome("les deux, le texte devant", Some(10), Some(40)),
            outcome("les deux, l'hybride devant encore", Some(8), Some(2)),
            outcome("seul l'hybride trouve", None, Some(7)),
            outcome("seul le texte trouve", Some(5), None),
            outcome("personne ne trouve", None, None),
            outcome("ex æquo", Some(30), Some(30)),
        ];
        let refs: Vec<&Outcome> = set.iter().collect();
        let result = paired(Engine::FullText, Engine::Hybrid, &refs);

        assert_eq!(
            result.wins, 2,
            "le texte gagne la deuxième et celle qu'il est seul à voir"
        );
        assert_eq!(
            result.losses, 3,
            "l'hybride gagne la première, la troisième et la sienne"
        );
        assert_eq!(result.ties, 1, "le vrai ex æquo, et lui seul");
        assert_eq!(
            result.neither, 1,
            "trouvée par personne : ni victoire, ni ex æquo"
        );
        assert_eq!(
            result.both, 4,
            "le rapport voit les quatre requêtes rendues des deux côtés"
        );
        // log₂ des rapports : −2, +2, −2, 0 → moyenne −0,5 → ×0,707. Inférieur à 1 : c'est le
        // second, l'hybride, qui met la cible plus haut.
        let ratio = result
            .ratio
            .expect("quatre requêtes rendues des deux côtés");
        assert!((ratio - 2f64.powf(-0.5)).abs() < 1e-9, "rapport {ratio}");
        assert!(ratio < 1.0, "le rapport a été calculé à l'envers");
        assert!((result.p - sign_test(2, 5)).abs() < 1e-12);
    }

    /// Un résultat apparié, construit à la main, avec la p-value qui va avec ses victoires.
    fn result(
        wins: usize,
        losses: usize,
        neither: usize,
        both: usize,
        ratio: Option<f64>,
    ) -> Paired {
        Paired {
            wins,
            losses,
            ties: 0,
            neither,
            both,
            ratio,
            p: sign_test(wins, wins + losses),
        }
    }

    /// Le signe décide, et lui seul — y compris dans les trois cas qui ont fait retirer le veto.
    #[test]
    fn a_verdict_is_decided_by_the_sign_alone() {
        assert_eq!(verdict(&result(9, 1, 0, 10, Some(2.0))), Verdict::Left);
        assert_eq!(verdict(&result(1, 9, 0, 10, Some(0.5))), Verdict::Right);
        assert_eq!(verdict(&result(6, 4, 0, 10, Some(0.5))), Verdict::Undecided);
        // Aucune requête rendue par les deux : le signe parle seul, comme toujours.
        assert_eq!(verdict(&result(1, 9, 0, 0, None)), Verdict::Right);

        // Premier cas du veto : un rapport sur une seule requête annulait un 9 contre 1.
        assert_eq!(verdict(&result(1, 9, 0, 1, Some(2.0))), Verdict::Right);
        // Troisième : trente trouvailles de l'hybride seul, neuf requêtes partagées à cinq contre
        // quatre. Le rapport penche vers le texte, qui ne trouve rien dans trente cas sur
        // trente-neuf ; la version à veto répondait « ni l'un ni l'autre ».
        assert_eq!(
            verdict(&result(5, 34, 0, 9, Some(2f64.powf(1.0 / 9.0)))),
            Verdict::Right
        );
    }

    /// Le désaccord du rapport est **signalé**, et seulement quand il repose sur assez de requêtes.
    #[test]
    fn a_ratio_that_points_the_other_way_is_flagged_but_not_obeyed() {
        // Le second met la cible deux fois plus haut, et perd 9 fois sur 10 : désaccord, signalé.
        assert!(ratio_disagrees(&result(9, 1, 0, 10, Some(0.5))));
        assert!(ratio_disagrees(&result(1, 9, 0, 10, Some(2.0))));
        // D'accord avec le signe : rien à signaler.
        assert!(!ratio_disagrees(&result(9, 1, 0, 10, Some(2.0))));
        // Un rapport sur trop peu de requêtes ne se signale pas : il ne dit rien.
        assert!(!ratio_disagrees(&result(1, 9, 0, 1, Some(2.0))));
        assert!(!ratio_disagrees(&result(1, 9, 0, 0, None)));
    }

    /// La règle des requêtes **décidées**, et sa frontière exacte.
    ///
    /// Dix requêtes dont quatre que personne ne trouve et six trouvées par l'hybride seul : six
    /// décisions. La première version, qui comptait la taille du sous-ensemble, concluait
    /// « hybride meilleur ».
    #[test]
    fn a_verdict_needs_enough_decided_queries_and_the_boundary_is_exact() {
        let six = result(0, 6, 4, 0, None);
        assert!(
            six.p < 0.05,
            "le cas n'a de sens que si le test, seul, conclurait"
        );
        assert_eq!(verdict(&six), Verdict::TooFewDecided);

        // La frontière : huit décisions ne suffisent pas, neuf suffisent. Un `<` changé en `<=`
        // ferait tomber l'une des deux lignes.
        assert_eq!(verdict(&result(8, 0, 0, 0, None)), Verdict::TooFewDecided);
        assert_eq!(verdict(&result(8, 1, 0, 0, None)), Verdict::Left);
        assert_eq!(
            MIN_FOR_STATS, 9,
            "la frontière ci-dessus suppose le seuil de neuf"
        );
    }

    /// Les bornes de Wilson, contre des valeurs calculées à la main.
    #[test]
    fn the_wilson_interval_matches_hand_computed_bounds() {
        let (low, high) = wilson(7, 16);
        assert!(
            (low - 23.1).abs() < 0.1 && (high - 66.8).abs() < 0.1,
            "{low} – {high}"
        );
        // Zéro sur dix n'est pas zéro : c'est entre 0 et 28 %, et c'est tout l'intérêt.
        let (low, high) = wilson(0, 10);
        assert!(
            low.abs() < 1e-9 && (high - 27.8).abs() < 0.1,
            "{low} – {high}"
        );
        let (low, high) = wilson(10, 10);
        assert!(
            (low - 72.2).abs() < 0.1 && (high - 100.0).abs() < 1e-9,
            "{low} – {high}"
        );
        assert_eq!(wilson(0, 0), (0.0, 100.0), "sans requête, on ne sait rien");
    }

    /// `n` requêtes fabriquées.
    fn outcomes(n: usize) -> Vec<Outcome> {
        (0..n)
            .map(|index| outcome(&format!("q{index}"), Some(1), Some(1)))
            .collect()
    }

    /// La primaire est celle que le critère 1 déclare, **faite sur le sous-ensemble sans mot
    /// commun**, et **retirée des pistes**.
    ///
    /// La première version de ce test ne vérifiait que la valeur de la constante — une relecture
    /// l'a jugée tautologique, à raison : rien ne voyait si `compare` la faisait au bon endroit.
    #[test]
    fn the_primary_runs_on_the_hard_subset_and_is_not_counted_again_as_a_lead() {
        assert_eq!(PRIMARY, (Engine::FullText, Engine::Hybrid));
        let (all, hard) = (outcomes(26), outcomes(10));
        let all: Vec<&Outcome> = all.iter().collect();
        let hard: Vec<&Outcome> = hard.iter().collect();
        let engines = [Engine::FullText, Engine::Semantic, Engine::Hybrid];

        let plan = plan(&engines, &all, &hard);
        assert_eq!(plan.primary, Primary::Run);
        assert!(
            std::ptr::eq(plan.primary_set.as_ptr(), hard.as_ptr())
                && plan.primary_set.len() == hard.len(),
            "la primaire ne porte pas sur le sous-ensemble sans mot commun"
        );
        assert_eq!(
            plan.exploratory.len(),
            5,
            "trois paires sur tout le jeu, deux sur le dur"
        );
        assert!(
            !plan
                .exploratory
                .iter()
                .any(|(label, _, l, r)| *label == "sans mot commun" && (*l, *r) == PRIMARY),
            "la primaire a été recomptée comme piste"
        );
        assert!(
            plan.exploratory
                .iter()
                .any(|(label, _, l, r)| *label == "tout le jeu" && (*l, *r) == PRIMARY),
            "sur tout le jeu, la même paire reste une piste"
        );
    }

    /// L'exclusion de la primaire ne dépend pas de l'ordre des moteurs mesurés.
    #[test]
    fn the_primary_is_excluded_from_the_leads_whatever_the_engine_order() {
        let (all, hard) = (outcomes(26), outcomes(10));
        let all: Vec<&Outcome> = all.iter().collect();
        let hard: Vec<&Outcome> = hard.iter().collect();
        let reversed = [Engine::Hybrid, Engine::Semantic, Engine::FullText];

        let plan = plan(&reversed, &all, &hard);
        assert_eq!(plan.exploratory.len(), 5);
        assert!(
            !plan
                .exploratory
                .iter()
                .any(|(label, _, l, r)| *label == "sans mot commun" && is_primary(*l, *r)),
            "dans l'autre ordre, la primaire a été recomptée comme piste"
        );
    }

    /// Sous le seuil, la primaire est annoncée sans conclure, et l'ensemble trop petit ne compte
    /// pas dans le nombre de pistes imprimé en tête.
    #[test]
    fn a_hard_subset_below_the_threshold_concludes_nothing_and_is_not_counted() {
        let (all, hard) = (outcomes(26), outcomes(MIN_FOR_STATS - 1));
        let all: Vec<&Outcome> = all.iter().collect();
        let hard: Vec<&Outcome> = hard.iter().collect();
        let engines = [Engine::FullText, Engine::Semantic, Engine::Hybrid];

        let plan = plan(&engines, &all, &hard);
        assert_eq!(plan.primary, Primary::TooFew);
        assert_eq!(
            plan.exploratory.len(),
            3,
            "seules les paires sur tout le jeu"
        );
    }

    /// Sans l'un des deux bras déclarés — un relevé sans modèle, par exemple —, pas de primaire.
    #[test]
    fn without_both_declared_arms_there_is_no_primary() {
        let (all, hard) = (outcomes(26), outcomes(10));
        let all: Vec<&Outcome> = all.iter().collect();
        let hard: Vec<&Outcome> = hard.iter().collect();

        let two = plan(&[Engine::FullText, Engine::Semantic], &all, &hard);
        assert_eq!(two.primary, Primary::Unavailable);
        assert_eq!(
            two.exploratory.len(),
            2,
            "texte contre sémantique, sur chacun des deux ensembles"
        );

        let one = plan(&[Engine::FullText], &all, &hard);
        assert_eq!(one.primary, Primary::Unavailable);
        assert!(one.exploratory.is_empty());
    }

    /// Un store indexé portant ces messages : (sujet, destinataire, corps).
    ///
    /// Une vraie boîte, avec de vrais blobs et un vrai index : ce sont précisément la partition et
    /// le garde-fou, qui interrogent l'index, qu'une relecture a trouvés sans aucun test.
    fn indexed(messages: &[(&str, &str, &str)]) -> (tempfile::TempDir, Mailbox, Vec<MessageId>) {
        use mailcore::model::{FolderKind, MessageFlags};

        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut ids = Vec::new();
        {
            let store = mailcore::Store::open(&root).unwrap();
            let writer = store.writer().unwrap();
            let account = writer.upsert_account("imap", "compte").unwrap();
            let folder = writer
                .upsert_folder(account, "INBOX", FolderKind::Inbox)
                .unwrap();
            for (index, (subject, to, body)) in messages.iter().enumerate() {
                let raw = format!(
                    "From: Expediteur <e@exemple.invalid>\r\nTo: {to}\r\nSubject: {subject}\r\n\r\n{body}\r\n"
                );
                let blob = store.blobs().put(raw.as_bytes()).unwrap().hash;
                let (id, _) = writer
                    .insert_message(&mailcore::NewMessage {
                        blob,
                        rfc822_id: None,
                        date: 1_700_000_000 + index as i64,
                        from_addr: "e@exemple.invalid",
                        from_name: Some("Expediteur"),
                        subject,
                        size: raw.len() as u64,
                        has_attachments: false,
                    })
                    .unwrap();
                writer
                    .insert_ref(id, folder, 1_700_000_000, MessageFlags::empty())
                    .unwrap();
                ids.push(id);
            }
            writer.commit().unwrap();
            mailcore::index::rebuild(&store, &mailcore::Progress::new()).unwrap();
        }
        let mailbox = Mailbox::open(&root).unwrap();
        (dir, mailbox, ids)
    }

    /// La partition, sur un vrai index — y compris le cas que l'ancien banc ratait.
    #[test]
    fn the_rarest_shared_word_is_asked_of_the_index_and_ignores_function_words() {
        let (_dir, mailbox, ids) = indexed(&[
            (
                "Reunion trimestrielle",
                "Bertrand Dupuis <b@exemple.invalid>",
                "ordre du jour",
            ),
            ("autre chose", "c@exemple.invalid", "rien a signaler"),
        ]);
        let (with_recipient, other) = (ids[0], ids[1]);

        // Le mot n'est que dans le destinataire : l'ancien banc, qui relisait sujet, expéditeur et
        // corps, l'aurait rangée « sans mot commun ». L'index le voit.
        assert_eq!(
            rarest_shared_word(&mailbox, "bertrand", &[with_recipient]).unwrap(),
            Some(1)
        );
        // Un mot-outil que la cible porte bien — « du », dans « ordre du jour » — ne compte pas :
        // tantivy l'indexe, le banc l'écarte. La première version testait des mots-outils absents de
        // la cible, et le contrôle ne pouvait pas échouer.
        assert_eq!(mailbox.contains(with_recipient, "du").unwrap(), Some(true));
        assert_eq!(
            rarest_shared_word(&mailbox, "du", &[with_recipient]).unwrap(),
            None
        );
        // Le partage se lit sur **toutes** les cibles, y compris la dernière.
        assert_eq!(
            rarest_shared_word(&mailbox, "trimestrielle", &[other, with_recipient]).unwrap(),
            Some(1)
        );
        // Le contrôle inverse : un mot d'un autre message n'est pas partagé avec celui-ci.
        assert_eq!(
            rarest_shared_word(&mailbox, "signaler", &[with_recipient]).unwrap(),
            None
        );
    }

    /// C'est le mot partagé le **plus rare** qui compte, pas le premier ni le plus fréquent.
    #[test]
    fn the_rarest_of_the_shared_words_is_the_one_that_counts() {
        let (_dir, mailbox, ids) = indexed(&[
            ("commun unique", "a@exemple.invalid", "texte"),
            ("commun", "b@exemple.invalid", "texte"),
            ("commun", "c@exemple.invalid", "texte"),
        ]);
        // « commun » est dans trois documents, « unique » dans un : la requête qui porte les deux
        // se range par le second. Dans les deux ordres, pour que ce ne soit pas le premier mot.
        assert_eq!(
            rarest_shared_word(&mailbox, "commun unique", &[ids[0]]).unwrap(),
            Some(1)
        );
        assert_eq!(
            rarest_shared_word(&mailbox, "unique commun", &[ids[0]]).unwrap(),
            Some(1)
        );
        assert_eq!(
            rarest_shared_word(&mailbox, "commun", &[ids[0]]).unwrap(),
            Some(3)
        );
    }

    /// Le seuil de classement est `TOP`, lu littéralement.
    #[test]
    fn a_shared_word_suffices_up_to_ten_documents_and_only_situates_beyond() {
        assert_eq!(reach_of(None), Reach::None);
        assert_eq!(reach_of(Some(1)), Reach::Word(1));
        assert_eq!(reach_of(Some(TOP as u64)), Reach::Word(TOP as u64));
        assert_eq!(reach_of(Some(TOP as u64 + 1)), Reach::Group(TOP as u64 + 1));
    }

    /// Le garde-fou, dans ses trois états : un store non indexé est refusé, un store indexé passe,
    /// et un store qui a perdu un blob passe **aussi** — c'était le défaut qui le bloquait pour
    /// toujours.
    #[test]
    fn the_coverage_guard_refuses_an_unindexed_store_and_tolerates_a_missing_blob() {
        use mailcore::model::{FolderKind, MessageFlags};

        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let mut blobs = Vec::new();
        {
            let store = mailcore::Store::open(&root).unwrap();
            let writer = store.writer().unwrap();
            let account = writer.upsert_account("imap", "compte").unwrap();
            let folder = writer
                .upsert_folder(account, "INBOX", FolderKind::Inbox)
                .unwrap();
            for index in 0..3 {
                let raw = format!("From: a@b.c\r\nSubject: s{index}\r\n\r\ncorps {index}\r\n");
                let blob = store.blobs().put(raw.as_bytes()).unwrap().hash;
                let (id, _) = writer
                    .insert_message(&mailcore::NewMessage {
                        blob,
                        rfc822_id: None,
                        date: 1_700_000_000,
                        from_addr: "a@b.c",
                        from_name: None,
                        subject: &format!("s{index}"),
                        size: raw.len() as u64,
                        has_attachments: false,
                    })
                    .unwrap();
                writer
                    .insert_ref(id, folder, 1_700_000_000, MessageFlags::empty())
                    .unwrap();
                blobs.push(blob);
            }
            writer.commit().unwrap();
        }

        // Importé, pas indexé : l'index est vide, et le banc doit refuser.
        let unindexed = Mailbox::open(&root).unwrap();
        assert!(
            check_coverage(&unindexed, &root).is_err(),
            "un store non indexé a été mesuré"
        );
        drop(unindexed);

        // Indexé : il passe, sans blob manquant.
        {
            let store = mailcore::Store::open(&root).unwrap();
            mailcore::index::rebuild(&store, &mailcore::Progress::new()).unwrap();
        }
        let indexed = Mailbox::open(&root).unwrap();
        assert_eq!(check_coverage(&indexed, &root).unwrap(), 0);
        drop(indexed);

        // Un blob perdu, puis réindexé : l'index a un document de moins que de messages, et c'est
        // normal. La première version refusait ici, et `mail index` n'y pouvait rien.
        {
            let store = mailcore::Store::open(&root).unwrap();
            assert!(store.blobs().delete(blobs[1]).unwrap());
            mailcore::index::rebuild(&store, &mailcore::Progress::new()).unwrap();
        }
        let partial = Mailbox::open(&root).unwrap();
        assert_eq!(
            check_coverage(&partial, &root).unwrap(),
            1,
            "un blob manquant est un problème de store, pas un index incomplet"
        );
    }

    /// Les seuils que Fable a donnés, et qui sont imprimés à l'utilisateur quand le test échoue.
    #[test]
    fn the_victory_thresholds_are_the_ones_the_bench_announces() {
        assert_eq!(victories_needed(10), Some(9));
        assert_eq!(victories_needed(20), Some(15));
        assert_eq!(victories_needed(26), Some(19));
        assert_eq!(victories_needed(30), Some(21));
    }

    /// **Le contrôle qui justifie le seuil de neuf**, et il a corrigé la première version de sa
    /// propre justification.
    ///
    /// Le commentaire disait d'abord « sous dix, aucune répartition ne conclut ». C'est faux : à
    /// six requêtes, six victoires sur six donnent p = 0,031, donc une décision est possible —
    /// **à condition d'être unanime**. La propriété qui tient vraiment, et qui est celle que le
    /// seuil doit défendre, est qu'en dessous de neuf une décision **ne survit pas à une seule
    /// voix contraire**.
    ///
    /// C'est une nuance et elle change le sens : un seuil qui interdit de conclure protège d'une
    /// fausse certitude, mais il faut dire de quoi exactement. Ici, de la fragilité — pas de
    /// l'impossibilité.
    #[test]
    fn below_the_threshold_a_decision_cannot_survive_a_single_dissent() {
        for decided in 1..MIN_FOR_STATS {
            if let Some(needed) = victories_needed(decided) {
                assert_eq!(
                    needed, decided,
                    "à {decided} requêtes, {needed} victoires suffiraient : une dissidence serait \
                     tolérée, ce que le seuil prétend impossible"
                );
            }
        }
        assert_eq!(
            victories_needed(MIN_FOR_STATS),
            Some(MIN_FOR_STATS - 1),
            "neuf est censé être le premier effectif qui tolère une dissidence"
        );
    }

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

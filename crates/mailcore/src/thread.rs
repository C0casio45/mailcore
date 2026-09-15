//! Reconstruction des fils, et leur prolongement après une moisson.
//!
//! ## Ce qu'on implémente, et ce qu'on n'implémente pas
//!
//! `jwz` construit un **arbre** de conteneurs : qui répond à qui, à quelle profondeur. Notre
//! schéma ne stocke pas cet arbre — `threads` porte une racine, un sujet normalisé, une date
//! et un compteur. Ce dont on a besoin est donc le **partitionnement** des messages en fils,
//! pas la hiérarchie interne.
//!
//! Pour un partitionnement, une union-find dit exactement la même chose que l'arbre de `jwz`,
//! en bien moins de code : deux messages sont dans le même fil si une chaîne de `References`
//! les relie. C'est la même relation d'équivalence, calculée autrement. L'arbre pourra se
//! reconstruire à l'affichage, à partir des `References` d'un fil de quelques dizaines de
//! messages — pas de 73 000.
//!
//! ## Les deux liens, dans l'ordre
//!
//! 1. **`References` et `In-Reply-To`.** Fiables quand ils sont là. Un message est uni à
//!    chaque message référencé qu'on possède.
//! 2. **Le sujet normalisé**, en dernier recours. Beaucoup de clients omettent `References`,
//!    et sans ce repli une conversation entière se présenterait comme des messages isolés.
//!
//! Le repli par sujet est le morceau dangereux : « Re: Bonjour » réunirait des dizaines de
//! conversations sans rapport. Trois garde-fous, et chacun est compté séparément pour qu'on
//! puisse juger sur pièces plutôt que sur l'intention :
//!
//! - le sujet normalisé doit faire au moins [`MIN_SUBJECT_LEN`] caractères ;
//! - un groupe de sujet de plus de [`MAX_SUBJECT_GROUP`] messages n'est pas fusionné — c'est
//!   une liste de diffusion ou un objet générique, pas une conversation ;
//! - seuls les messages qu'aucune `References` n'a rattachés y sont éligibles.
//!
//! ## Deux passes, une seule règle
//!
//! [`rebuild`] repart de zéro et relit tous les blobs. [`update`] prolonge les fils après une
//! moisson, sans relire quoi que ce soit hors du voisinage touché. **Les deux appellent
//! [`partition`]**, qui est le seul endroit où la règle de fil est écrite — c'est la leçon du
//! carnet d'adresses du 2026-09-11 : deux chemins vers la même donnée partagent leur code, pas
//! leur intention, sans quoi ils divergent et la différence ne se voit qu'à l'usage.
//!
//! ## Pourquoi « incrémental » ne veut pas dire la même chose ici
//!
//! Le carnet et l'index plein texte se contentent d'un drapeau par ligne : « ce message a été
//! compté ». Un fil ne s'y prête pas, et c'est écrit depuis la phase 1 — **un fil se calcule à
//! partir de messages qui arrivent après celui qu'on traite.** Un message rattaché aujourd'hui
//! peut devoir l'être autrement demain, quand son parent arrive.
//!
//! Ce que fait [`update`] n'est donc pas « ne regarder que le nouveau » mais « recalculer le
//! **voisinage** touché » : les messages reliés au nouveau par une référence, ceux qui sont déjà
//! dans les fils concernés, et ceux de son groupe de sujet. Sur ce voisinage — et sur lui seul —
//! la règle complète est rejouée. Le voisinage est **fermé** par construction : aucune arête du
//! graphe global n'en sort, donc y rejouer la règle donne le même découpage qu'une
//! reconstruction, et c'est ce que vérifie
//! `an_incremental_pass_gives_exactly_what_a_rebuild_gives`.
//!
//! Il est aussi **borné** ([`MAX_NEIGHBOURHOOD`]) : au-delà, la passe renonce et refait une
//! reconstruction complète. Une passe de fond qui explore sans fin serait pire qu'un résultat
//! faux, parce que rien ne la signalerait.

use std::collections::{HashMap, HashSet};

use mail_parser::MessageParser;

use crate::error::Result;
use crate::model::{MessageId, ThreadId};
use crate::progress::Progress;
use crate::store::Store;
use crate::store::read::ThreadRow;
use crate::store::threads::{ThreadLink, ThreadNode};

/// Longueur minimale d'un sujet normalisé pour autoriser le repli.
///
/// En dessous, ce sont des sujets comme « ok », « hi », « re » — trop génériques pour porter
/// une conversation.
pub const MIN_SUBJECT_LEN: usize = 8;

/// Au-delà de ce nombre de messages, un groupe de même sujet n'est pas fusionné.
///
/// « Votre facture », « Notification » ou le nom d'une liste de diffusion réunissent des
/// centaines de messages sans rapport entre eux. Les coller dans un fil unique produirait un
/// objet inutilisable, et masquerait les vraies conversations.
pub const MAX_SUBJECT_GROUP: usize = 25;

/// Au-delà de ce nombre de messages dans le voisinage, la passe locale renonce.
///
/// Le voisinage est fermé, donc il peut en principe atteindre la taille du plus gros fil du
/// store. Recalculer un fil de dix mille messages **à chaque arrivée de courrier** coûterait plus
/// cher que la reconstruction qu'on remplace, et sans borne rien ne le dirait : le job
/// s'afficherait « en cours ».
///
/// Le chiffre est haut par rapport à ce qu'on observe — le plus gros fil du corpus de la phase 1
/// tient largement en dessous — et la sortie n'est pas une erreur : c'est une reconstruction
/// complète, annoncée dans [`ThreadStats::full_pass`].
pub const MAX_NEIGHBOURHOOD: usize = 5_000;

/// Combien de messages neufs une passe locale traite d'un coup.
///
/// Ils sont traités **ensemble** parce qu'ils peuvent se référencer entre eux : une moisson qui
/// rapporte une conversation entière apporte le parent et sa réponse dans le même paquet.
const BATCH: usize = 200;

/// Un message tel que la règle de fil le voit.
///
/// Les deux passes le construisent de sources différentes — [`rebuild`] depuis le blob,
/// [`update`] depuis les colonnes rangées par `SCHEMA_V13` — et c'est tout l'intérêt d'avoir un
/// seul type : la règle ne sait pas d'où il vient.
#[derive(Debug, Clone)]
pub struct Node {
    /// Identifiant interne.
    pub id: MessageId,
    /// Date en secondes Unix. Avec `id`, l'ordre qui désigne la racine d'un fil.
    pub date: i64,
    /// L'en-tête `Message-ID`, s'il était présent.
    pub rfc822_id: Option<String>,
    /// Le sujet normalisé, tel que [`normalise_subject`] le rend.
    pub subject_norm: String,
    /// Les identifiants référencés, `In-Reply-To` puis `References`.
    pub references: Vec<String>,
}

/// Ce que la passe de threading a fait.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadStats {
    /// Fils créés ou mis à jour.
    pub threads: u64,
    /// Messages rattachés.
    pub messages: u64,
    /// Liens établis par `References` ou `In-Reply-To`.
    pub links_by_references: u64,
    /// Liens établis par le repli sur le sujet.
    pub links_by_subject: u64,
    /// Groupes de sujet écartés parce que trop gros.
    pub subject_groups_rejected: u64,
    /// Messages restés seuls dans leur fil.
    pub singletons: u64,
    /// Messages dont le blob était introuvable : non liés, mais rattachés à un fil à eux.
    pub missing_blobs: u64,
    /// Messages portant un en-tête `Message-ID`. Sans lui, un message ne peut jamais être
    /// la cible d'une référence.
    pub with_message_id: u64,
    /// Messages portant au moins un `References` ou `In-Reply-To`.
    pub with_references: u64,
    /// Références pointant vers un message absent du store.
    ///
    /// Mesure directement à quel point le graphe de conversation est troué : une réponse
    /// dont on n'a jamais reçu le message parent ne peut pas être rattachée.
    pub references_unresolved: u64,
    /// Fils dissous parce que leurs messages ont été rattachés ailleurs.
    ///
    /// Toujours nul sur une reconstruction, qui part d'une table vide. Non nul quand l'arrivée
    /// d'un parent réunit deux fils qui existaient séparément — exactement ce que la passe
    /// locale doit savoir faire.
    pub threads_dissolved: u64,
    /// Le plus grand voisinage qu'une passe locale a eu à recalculer.
    ///
    /// C'est la mesure qui dit si [`MAX_NEIGHBOURHOOD`] est au bon endroit.
    pub widest_neighbourhood: u64,
    /// Vrai si le travail a été fait par une reconstruction complète.
    ///
    /// Deux raisons : un store rattaché avant `SCHEMA_V13`, ou un voisinage trop large. Dans
    /// les deux cas la passe locale a renoncé **en le disant**.
    pub full_pass: bool,
}

/// Le découpage d'un ensemble de messages en fils.
#[derive(Debug, Clone, Default)]
pub struct Partition {
    /// Les composantes, chacune listant des indices dans les nœuds donnés, dans l'ordre.
    pub groups: Vec<Vec<usize>>,
    /// Pour chaque nœud, vrai si une référence l'a rattaché — donc s'il est hors du repli par
    /// sujet.
    pub linked: Vec<bool>,
    /// Liens établis par `References` ou `In-Reply-To`.
    pub links_by_references: u64,
    /// Liens établis par le repli sur le sujet.
    pub links_by_subject: u64,
    /// Groupes de sujet écartés parce que trop gros.
    pub subject_groups_rejected: u64,
    /// Références vers un message absent de l'ensemble donné.
    pub references_unresolved: u64,
    /// Nœuds portant un `Message-ID`.
    pub with_message_id: u64,
}

/// **La règle de fil, et le seul endroit où elle est écrite.**
///
/// Les nœuds doivent être triés par `(date, id)` : c'est cet ordre qui désigne la racine d'un
/// fil et qui départage deux messages portant le même `Message-ID`.
///
/// `oversized` nomme les sujets dont le groupe **réel** dépasse [`MAX_SUBJECT_GROUP`], pour le
/// cas où les nœuds donnés n'en sont qu'une partie. Une reconstruction, qui voit tout, passe un
/// ensemble vide et laisse la fonction compter elle-même.
#[must_use]
pub fn partition(nodes: &[Node], oversized: &HashSet<String>) -> Partition {
    let mut resolved = references_pass(nodes);
    subject_pass(nodes, oversized, &mut resolved);
    group(nodes.len(), &mut resolved);
    resolved.partition
}

/// L'état des trois temps : la partition en construction, et l'union-find qui la porte.
///
/// L'union-find ne sort pas d'ici : ce que les appelants lisent est un découpage, pas une
/// structure de données.
#[derive(Debug)]
struct Resolved {
    union: UnionFind,
    partition: Partition,
}

/// Premier temps : les liens que portent les en-têtes.
fn references_pass(nodes: &[Node]) -> Resolved {
    // `rfc822_id` → indice. Première occurrence gagnante : le `Message-ID` vient du réseau,
    // plusieurs messages peuvent porter le même, et il faut bien en choisir un. Les nœuds étant
    // triés, c'est le plus ancien.
    let mut by_rfc822: HashMap<&str, usize> = HashMap::with_capacity(nodes.len());
    let mut with_message_id = 0;
    for (index, node) in nodes.iter().enumerate() {
        if let Some(id) = node.rfc822_id.as_deref() {
            by_rfc822.entry(id).or_insert(index);
            with_message_id += 1;
        }
    }

    let mut resolved = Resolved {
        union: UnionFind::new(nodes.len()),
        partition: Partition {
            linked: vec![false; nodes.len()],
            with_message_id,
            ..Partition::default()
        },
    };

    for (index, node) in nodes.iter().enumerate() {
        for parent in &node.references {
            let Some(&other) = by_rfc822.get(parent.as_str()) else {
                resolved.partition.references_unresolved += 1;
                continue;
            };
            if other == index {
                continue;
            }
            if resolved.union.join(index, other) {
                resolved.partition.links_by_references += 1;
            }
            resolved.partition.linked[index] = true;
            resolved.partition.linked[other] = true;
        }
    }
    resolved
}

/// Second temps : le repli par sujet, sur ce que les références ont laissé isolé.
fn subject_pass(nodes: &[Node], oversized: &HashSet<String>, resolved: &mut Resolved) {
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    // Les sujets que l'appelant déclare trop gros et dont l'ensemble donné porte au moins un
    // membre éligible. Ce sont des groupes écartés, au même titre que ceux que la boucle
    // suivante refuse : sans ce compte, une passe locale dirait zéro là où une reconstruction
    // dirait un, et le relevé des deux ne serait plus comparable.
    let mut refused: HashSet<&str> = HashSet::new();
    for (index, node) in nodes.iter().enumerate() {
        if resolved.partition.linked[index] || node.subject_norm.chars().count() < MIN_SUBJECT_LEN {
            continue;
        }
        if oversized.contains(&node.subject_norm) {
            refused.insert(node.subject_norm.as_str());
            continue;
        }
        groups.entry(&node.subject_norm).or_default().push(index);
    }
    resolved.partition.subject_groups_rejected += refused.len() as u64;

    for members in groups.values() {
        if members.len() < 2 {
            continue;
        }
        if members.len() > MAX_SUBJECT_GROUP {
            resolved.partition.subject_groups_rejected += 1;
            continue;
        }
        for pair in members.windows(2) {
            if resolved.union.join(pair[0], pair[1]) {
                resolved.partition.links_by_subject += 1;
            }
        }
    }
}

/// Troisième temps : rassembler les composantes, dans un ordre déterminé.
fn group(len: usize, resolved: &mut Resolved) {
    // Indexé par la racine d'union-find, mais rendu dans l'ordre des nœuds : deux exécutions
    // rendent la même chose, ce qu'un parcours de `HashMap` ne garantirait pas.
    let mut position: HashMap<usize, usize> = HashMap::new();
    for index in 0..len {
        let root = resolved.union.find(index);
        match position.get(&root) {
            Some(&at) => resolved.partition.groups[at].push(index),
            None => {
                position.insert(root, resolved.partition.groups.len());
                resolved.partition.groups.push(vec![index]);
            }
        }
    }
}

/// Reconstruit tous les fils depuis les blobs.
///
/// Passe complète : elle vide la table et repart de zéro. C'est ce que demande `mail thread`, et
/// c'est ce à quoi [`update`] revient quand elle ne peut pas travailler localement.
///
/// Elle **range les faits dérivés** au passage — sujet normalisé, références, mode de
/// rattachement — et c'est ce qui rend les passes locales suivantes possibles sans relire un
/// seul blob.
///
/// # Errors
///
/// [`crate::Error::Sqlite`] si le store est illisible. Un message individuel qui échoue est
/// compté, pas propagé.
pub fn rebuild(store: &Store, progress: &Progress) -> Result<ThreadStats> {
    let rows = store.all_for_threading()?;
    let mut stats = ThreadStats {
        full_pass: true,
        ..ThreadStats::default()
    };

    progress.set_total(rows.len() as u64);
    let parser = MessageParser::default();
    let mut nodes = Vec::with_capacity(rows.len());
    for row in &rows {
        if progress.is_cancelled() {
            tracing::info!(processed = nodes.len(), "threading interrompu");
            return Ok(stats);
        }
        progress.advance(1);
        nodes.push(read_facts(store, &parser, row, &mut stats)?);
    }

    let partition = partition(&nodes, &HashSet::new());
    fold(&mut stats, &partition);

    let writer = store.writer()?;
    writer.clear_threads()?;
    for members in &partition.groups {
        let thread = writer.insert_thread(
            nodes[members[0]].id,
            &nodes[members[0]].subject_norm,
            last_date(&nodes, members),
            count_of(members),
        )?;
        attach(&writer, &nodes, members, &partition, thread, &mut stats)?;
    }
    for node in &nodes {
        writer.record_thread_facts(node.id, &node.subject_norm, &node.references)?;
    }
    writer.commit()?;

    tracing::info!(
        threads = stats.threads,
        messages = stats.messages,
        by_references = stats.links_by_references,
        by_subject = stats.links_by_subject,
        "fils reconstruits"
    );
    Ok(stats)
}

/// Prolonge les fils avec ce qu'une moisson vient d'apporter.
///
/// Ne relit aucun blob hors des messages neufs, et ne touche que le **voisinage** de ce qui
/// arrive : les fils des messages qu'il référence, ceux qui le référencent, et son groupe de
/// sujet. Le reste du store n'est ni lu ni écrit.
///
/// Elle revient à [`rebuild`] dans deux cas, et le dit dans [`ThreadStats::full_pass`] :
///
/// - le store porte des fils rattachés **avant** `SCHEMA_V13`, donc dépourvus des faits dérivés
///   dont le voisinage a besoin. Ça n'arrive qu'une fois par store ;
/// - le voisinage dépasse [`MAX_NEIGHBOURHOOD`]. Recalculer un fil énorme à chaque arrivée
///   coûterait plus que la reconstruction qu'on remplace.
///
/// # Errors
///
/// Les mêmes que [`rebuild`].
pub fn update(store: &Store, progress: &Progress) -> Result<ThreadStats> {
    // **La question la moins chère en premier**, la leçon du banc du 2026-09-11 : un `COUNT` sur
    // un index partiel, avant d'ouvrir quoi que ce soit. C'est le cas de très loin le plus
    // fréquent, puisque `IDLE` met une moisson en file à chaque arrivée de courrier.
    let pending = store.unthreaded_total()?;
    if pending == 0 {
        return Ok(ThreadStats::default());
    }

    if store.threaded_without_facts()? > 0 {
        tracing::info!("fils sans faits dérivés : une reconstruction, une fois");
        return rebuild(store, progress);
    }

    // **Bornée par ce qui était en attente au début.** Sans borne, la boucle ne s'arrête que
    // parce que les lignes traitées quittent la requête — et le jour où ce ne serait plus vrai,
    // le job tournerait pour toujours en consommant un cœur, affiché « en cours ». C'est le
    // contrôle négatif du carnet, le 2026-09-11 : les tests ne tombent pas, ils **pendent**.
    progress.set_total(pending);
    let mut remaining = pending;
    let mut stats = ThreadStats::default();
    let parser = MessageParser::default();

    while remaining > 0 {
        let rows = store.unthreaded(BATCH.min(remaining as usize))?;
        if rows.is_empty() {
            break;
        }
        remaining = remaining.saturating_sub(rows.len() as u64);
        if progress.is_cancelled() {
            tracing::info!(messages = stats.messages, "threading interrompu");
            return Ok(stats);
        }
        progress.advance(rows.len() as u64);

        if !advance(store, &parser, &rows, &mut stats)? {
            // Le voisinage a débordé. La reconstruction rend son propre bilan : le reprendre
            // tel quel plutôt que d'additionner deux relevés qui ne comptent pas la même chose.
            tracing::info!(
                limit = MAX_NEIGHBOURHOOD,
                "voisinage trop large : reconstruction complète"
            );
            // La progression repart de zéro : ce qui est déjà compté ne vaut plus rien, et le
            // garder montrerait une barre pleine pendant toute la reconstruction.
            progress.restart(0);
            return rebuild(store, progress);
        }
    }

    tracing::info!(
        threads = stats.threads,
        messages = stats.messages,
        dissolved = stats.threads_dissolved,
        widest = stats.widest_neighbourhood,
        "fils prolongés"
    );
    Ok(stats)
}

/// Traite un paquet de messages neufs. Rend faux si le voisinage a débordé.
fn advance(
    store: &Store,
    parser: &MessageParser,
    rows: &[ThreadRow],
    stats: &mut ThreadStats,
) -> Result<bool> {
    // Les faits d'abord, et validés : le voisinage se lit ensuite **en SQL**, y compris pour les
    // messages neufs. Une seule source de vérité pendant tout le reste de la passe.
    let writer = store.writer()?;
    let mut seeds = Vec::with_capacity(rows.len());
    for row in rows {
        let node = read_facts(store, parser, row, stats)?;
        writer.record_thread_facts(node.id, &node.subject_norm, &node.references)?;
        seeds.push(node.id);
    }
    writer.commit()?;

    let Some((entries, oversized)) = neighbourhood(store, &seeds)? else {
        return Ok(false);
    };
    stats.widest_neighbourhood = stats.widest_neighbourhood.max(entries.len() as u64);

    let nodes: Vec<Node> = entries.iter().map(|entry| entry.node.clone()).collect();
    let partition = partition(&nodes, &oversized);
    fold(stats, &partition);

    // Les fils que le voisinage occupait. Ceux qu'aucune composante ne réutilise sont dissous :
    // c'est ce qui arrive quand l'arrivée d'un parent réunit deux fils qui existaient à part.
    let mut freed: HashSet<ThreadId> = entries.iter().filter_map(|entry| entry.thread).collect();
    // **Un fil ne sert qu'une composante.** Un fil qui se sépare en deux — son message central
    // part rejoindre une conversation, le reste tient par le sujet — a deux composantes dont les
    // membres désignent tous l'ancien fil. Sans ce registre, les deux le réutiliseraient, et la
    // séparation se solderait par une fusion.
    let mut claimed: HashSet<ThreadId> = HashSet::new();

    let writer = store.writer()?;
    for members in &partition.groups {
        let root = &nodes[members[0]];
        // **Réutiliser un fil plutôt qu'en créer un.** Un identifiant de fil stable est ce qui
        // permet à une interface de rester sur le fil qu'elle affichait quand une réponse
        // arrive. Le plus petit identifiant, donc le plus ancien, pour que deux fils réunis
        // gardent celui qui existait en premier.
        let existing = members
            .iter()
            .filter_map(|&index| entries[index].thread)
            .filter(|thread| !claimed.contains(thread))
            .min();
        if let Some(thread) = existing {
            claimed.insert(thread);
        }
        let thread = match existing {
            Some(thread) => {
                freed.remove(&thread);
                writer.update_thread(
                    thread,
                    root.id,
                    &root.subject_norm,
                    last_date(&nodes, members),
                    count_of(members),
                )?;
                thread
            }
            None => writer.insert_thread(
                root.id,
                &root.subject_norm,
                last_date(&nodes, members),
                count_of(members),
            )?,
        };
        attach(&writer, &nodes, members, &partition, thread, stats)?;
    }
    for thread in freed {
        writer.delete_thread(thread)?;
        stats.threads_dissolved += 1;
    }
    writer.commit()?;
    Ok(true)
}

/// Le voisinage fermé de ces messages, et les sujets dont le groupe réel est trop gros.
///
/// Rend `None` si l'ensemble dépasse [`MAX_NEIGHBOURHOOD`].
///
/// ## Fermé veut dire : aucune arête n'en sort
///
/// C'est la propriété dont dépend l'équivalence avec une reconstruction. Trois façons pour une
/// arête de sortir, donc trois fermetures :
///
/// 1. **une référence** — les porteurs des identifiants qu'un membre cite, et les messages qui
///    citent l'identifiant d'un membre ;
/// 2. **un fil existant** — si un membre est déjà rattaché, tout son fil entre, sinon on
///    couperait un fil en deux sans le savoir ;
/// 3. **un groupe de sujet** — le groupe entier entre, ou aucun de ses membres n'est fusionné.
///
/// La troisième dépend de la première : un message rangé « sans référence » cesse de l'être dès
/// qu'un nouveau message le cite, et quitte alors son groupe. Les sujets sont donc réexaminés
/// **à chaque tour**, sur la connaissance du tour, et non mémorisés d'un tour à l'autre.
fn neighbourhood(
    store: &Store,
    seeds: &[MessageId],
) -> Result<Option<(Vec<ThreadNode>, HashSet<String>)>> {
    let mut known: HashMap<MessageId, ThreadNode> = HashMap::new();
    let mut pending: Vec<MessageId> = seeds.to_vec();
    let mut oversized: HashSet<String> = HashSet::new();

    loop {
        // --- Références et fils, jusqu'au point fixe. ---
        while !pending.is_empty() {
            let mut fetch: Vec<MessageId> = Vec::new();
            let mut seen: HashSet<MessageId> = HashSet::new();
            for id in pending.drain(..) {
                if !known.contains_key(&id) && seen.insert(id) {
                    fetch.push(id);
                }
            }
            if fetch.is_empty() {
                break;
            }
            if known.len() + fetch.len() > MAX_NEIGHBOURHOOD {
                return Ok(None);
            }
            for entry in store.thread_nodes(&fetch)? {
                for reference in &entry.node.references {
                    pending.extend(store.threaded_carriers_of(reference)?);
                }
                if let Some(id) = entry.node.rfc822_id.as_deref() {
                    pending.extend(store.threaded_referencing(id)?);
                }
                if let Some(thread) = entry.thread {
                    pending.extend(store.thread_member_ids(thread)?);
                }
                known.insert(entry.node.id, entry);
            }
        }

        // --- Les sujets, sur la connaissance de ce tour. ---
        let ordered = sorted(&known);
        let nodes: Vec<Node> = ordered.iter().map(|entry| entry.node.clone()).collect();
        let linked = references_pass(&nodes).partition.linked;

        oversized.clear();
        let mut done: HashSet<&str> = HashSet::new();
        let mut added = false;
        for (index, entry) in ordered.iter().enumerate() {
            let subject = entry.node.subject_norm.as_str();
            if subject.chars().count() < MIN_SUBJECT_LEN {
                continue;
            }
            // Deux raisons de s'intéresser à ce sujet : ce message rejoindrait le groupe, ou il
            // en faisait partie et le quitte — auquel cas le groupe doit être recalculé sans lui.
            let concerned = !linked[index] || entry.link == Some(ThreadLink::WithoutReference);
            if !concerned || !done.insert(subject) {
                continue;
            }

            let group = subject_group(store, subject, &ordered, &linked)?;
            let refused = group.members.len() > MAX_SUBJECT_GROUP;
            if refused {
                oversized.insert(subject.to_owned());
            }

            // **Un groupe refusé doit quand même entrer s'il était réuni hier.** Le plafond se
            // franchit message par message : à 25 les membres sont dans un fil commun, à 26 ce
            // fil doit être **dissous**. Les faire entrer est la seule façon de le dissoudre,
            // puisque seule une composante recalculée réécrit un rattachement.
            //
            // Au-delà, c'est inutile et ce serait ruineux : un sujet de newsletter réunit des
            // milliers de messages, et ils sont déjà seuls chacun dans son fil — un groupe
            // au-dessus du plafond n'a jamais été fusionné. Le relevé du store, avant filtrage,
            // est ce qui distingue les deux cas.
            let joins = !refused && group.members.len() >= 2;
            let splits = refused && group.stored <= MAX_SUBJECT_GROUP;
            if !joins && !splits {
                continue;
            }

            for id in group.members {
                if !known.contains_key(&id) {
                    pending.push(id);
                    added = true;
                }
            }
        }

        if !added {
            return Ok(Some((ordered, oversized)));
        }
    }
}

/// Un groupe de sujet tel que la passe locale a besoin de le connaître.
#[derive(Debug)]
struct SubjectGroup {
    /// Les membres réels : les messages sans référence résolue qui portent ce sujet.
    members: Vec<MessageId>,
    /// Combien le store en avait rangés comme tels, avant de tenir compte de ce qui arrive.
    ///
    /// Au-dessus du plafond, ce nombre dit que le groupe était **déjà** refusé hier : ses
    /// membres sont donc chacun dans leur fil, et il n'y a rien à dissoudre.
    stored: usize,
}

/// Le groupe de sujet réel : les messages sans référence résolue qui portent ce sujet.
///
/// Ceux du voisinage sont jugés sur la connaissance du tour — un message que le nouveau vient de
/// citer n'y est plus. Ceux d'ailleurs sont pris tels que le store les a rangés : rien de neuf ne
/// les touche, puisqu'une arête de référence les aurait fait entrer dans le voisinage.
fn subject_group(
    store: &Store,
    subject: &str,
    ordered: &[ThreadNode],
    linked: &[bool],
) -> Result<SubjectGroup> {
    // La borne doit couvrir les membres du voisinage qui ne sont **plus** éligibles : ils
    // occupent une place dans le relevé du store sans compter dans le groupe réel. Sans cette
    // marge, un groupe juste sous le plafond pourrait être lu comme au-dessus.
    let inside = ordered
        .iter()
        .filter(|entry| entry.node.subject_norm == subject)
        .count();
    let stored = store.unlinked_with_subject(subject, MAX_SUBJECT_GROUP + 1 + inside)?;
    let stored_count = stored.len();

    let mut members: Vec<MessageId> = Vec::with_capacity(stored.len());
    for id in stored {
        match ordered.iter().position(|entry| entry.node.id == id) {
            Some(index) if linked[index] => {}
            _ => members.push(id),
        }
    }
    for (index, entry) in ordered.iter().enumerate() {
        if entry.node.subject_norm == subject && !linked[index] && !members.contains(&entry.node.id)
        {
            members.push(entry.node.id);
        }
    }
    Ok(SubjectGroup {
        members,
        stored: stored_count,
    })
}

/// Les nœuds connus, triés comme la règle de fil les attend.
fn sorted(known: &HashMap<MessageId, ThreadNode>) -> Vec<ThreadNode> {
    let mut ordered: Vec<ThreadNode> = known.values().cloned().collect();
    ordered.sort_by_key(|entry| (entry.node.date, entry.node.id));
    ordered
}

/// Lit dans le blob ce que la règle de fil a besoin de savoir d'un message.
///
/// Un blob absent est **compté**, pas propagé : le message garde son sujet — donc son repli — et
/// n'a simplement aucune référence. C'est le même choix que dans le carnet d'adresses : un
/// résultat incomplet vaut mieux qu'une passe qui s'arrête.
fn read_facts(
    store: &Store,
    parser: &MessageParser,
    row: &ThreadRow,
    stats: &mut ThreadStats,
) -> Result<Node> {
    let mut node = Node {
        id: row.id,
        date: row.date,
        rfc822_id: row.rfc822_id.clone(),
        subject_norm: normalise_subject(&row.subject),
        references: Vec::new(),
    };

    let raw = match store.blobs().read(row.blob) {
        Ok(bytes) => bytes,
        Err(crate::Error::BlobNotFound(hash)) => {
            tracing::warn!(%hash, id = row.id.0, "blob absent, message non lié");
            stats.missing_blobs += 1;
            return Ok(node);
        }
        Err(other) => return Err(other),
    };
    let Some(parsed) = parser.parse_headers(&raw) else {
        return Ok(node);
    };

    node.references = referenced(&parsed);
    if !node.references.is_empty() {
        stats.with_references += 1;
    }
    Ok(node)
}

/// Verse dans le bilan ce que la règle de fil a compté.
fn fold(stats: &mut ThreadStats, partition: &Partition) {
    stats.links_by_references += partition.links_by_references;
    stats.links_by_subject += partition.links_by_subject;
    stats.subject_groups_rejected += partition.subject_groups_rejected;
    stats.references_unresolved += partition.references_unresolved;
    stats.with_message_id += partition.with_message_id;
}

/// Rattache les membres d'une composante à leur fil, et retient comment.
fn attach(
    writer: &crate::store::write::Writer<'_>,
    nodes: &[Node],
    members: &[usize],
    partition: &Partition,
    thread: ThreadId,
    stats: &mut ThreadStats,
) -> Result<()> {
    for &index in members {
        writer.set_thread(nodes[index].id, thread)?;
        writer.set_thread_link(
            nodes[index].id,
            if partition.linked[index] {
                ThreadLink::ByReference
            } else {
                ThreadLink::WithoutReference
            },
        )?;
    }
    stats.threads += 1;
    stats.messages += members.len() as u64;
    if members.len() == 1 {
        stats.singletons += 1;
    }
    Ok(())
}

/// La date du message le plus récent d'une composante.
fn last_date(nodes: &[Node], members: &[usize]) -> i64 {
    members
        .iter()
        .map(|&index| nodes[index].date)
        .max()
        .unwrap_or(0)
}

/// Le nombre de messages d'une composante, borné au format de la colonne.
fn count_of(members: &[usize]) -> u32 {
    u32::try_from(members.len()).unwrap_or(u32::MAX)
}

/// Les `Message-ID` référencés par un message, `In-Reply-To` d'abord, **sans doublon**.
///
/// `In-Reply-To` désigne le parent direct, `References` toute la chaîne. Les deux se
/// recoupent presque toujours ; les lire tous les deux coûte peu et rattrape les clients qui
/// n'écrivent que l'un des deux.
///
/// Le recoupement est justement la raison du dédoublonnage : le parent direct est presque
/// toujours cité **deux fois**, une par en-tête. Le garder deux fois range deux lignes
/// identiques dans `message_references`, donc fait rendre deux fois le même message à
/// `Store::threaded_referencing` — et compte deux fois une référence non résolue, ce qui
/// gonflerait d'autant la mesure du graphe troué.
fn referenced(parsed: &mail_parser::Message<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for header in ["In-Reply-To", "References"] {
        match parsed.header(header) {
            Some(mail_parser::HeaderValue::Text(value)) => out.push(value.to_string()),
            Some(mail_parser::HeaderValue::TextList(values)) => {
                out.extend(values.iter().map(ToString::to_string));
            }
            _ => {}
        }
    }
    let mut seen = HashSet::with_capacity(out.len());
    out.retain(|id| seen.insert(id.clone()));
    out
}

/// Normalise un sujet pour la comparaison : préfixes de réponse retirés, casse et espaces
/// aplaties.
///
/// Les préfixes couvrent le français, l'anglais et les variantes numérotées qu'écrivent
/// certains clients (`Re[2]:`). Un profil réel n'a pas que de l'anglais — voir
/// `docs/PHASE-1.md`.
#[must_use]
pub fn normalise_subject(subject: &str) -> String {
    const PREFIXES: &[&str] = &[
        "re", "ré", "rép", "rep", "aw", "sv", "vs", "antw", "fw", "fwd", "tr", "wg", "réf", "ref",
    ];

    let mut current = subject.trim();
    'stripping: loop {
        // `[liste]` en tête : marqueur de liste de diffusion, pas du sujet.
        if let Some((_, after)) = current.strip_prefix('[').and_then(|r| r.split_once(']')) {
            current = after.trim_start();
            continue;
        }
        for prefix in PREFIXES {
            let Some(rest) = strip_prefix_ci(current, prefix) else {
                continue;
            };
            // `Re:` mais aussi `Re[2]:` et `Re :` — la forme varie d'un client à l'autre.
            let rest = rest.trim_start();
            let rest = match rest.strip_prefix('[') {
                Some(bracketed) => match bracketed.split_once(']') {
                    Some((digits, after)) if digits.chars().all(|c| c.is_ascii_digit()) => after,
                    _ => rest,
                },
                None => rest,
            };
            if let Some(after) = rest.trim_start().strip_prefix(':') {
                current = after.trim_start();
                continue 'stripping;
            }
        }
        break;
    }

    current
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Retire un préfixe sans tenir compte de la casse.
fn strip_prefix_ci<'a>(haystack: &'a str, prefix: &str) -> Option<&'a str> {
    let end = prefix.len();
    if haystack.len() >= end
        && haystack.is_char_boundary(end)
        && haystack[..end].eq_ignore_ascii_case(prefix)
    {
        Some(&haystack[end..])
    } else {
        None
    }
}

/// Union-find avec compression de chemin et union par rang.
///
/// Quasi linéaire : 73 000 messages et leurs liens se partitionnent en quelques
/// millisecondes, ce qui laisse tout le budget à la lecture des blobs.
#[derive(Debug, Clone, Default)]
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(size: usize) -> Self {
        Self {
            parent: (0..size).collect(),
            rank: vec![0; size],
        }
    }

    /// La racine du groupe de `index`, en compressant le chemin au passage.
    ///
    /// Itératif et non récursif : une chaîne de réponses de 10 000 messages déborderait la
    /// pile avec une version récursive.
    fn find(&mut self, index: usize) -> usize {
        let mut root = index;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut current = index;
        while self.parent[current] != root {
            let next = self.parent[current];
            self.parent[current] = root;
            current = next;
        }
        root
    }

    /// Réunit deux groupes. Rend faux s'ils étaient déjà le même.
    fn join(&mut self, left: usize, right: usize) -> bool {
        let (mut a, mut b) = (self.find(left), self.find(right));
        if a == b {
            return false;
        }
        if self.rank[a] < self.rank[b] {
            std::mem::swap(&mut a, &mut b);
        }
        self.parent[b] = a;
        if self.rank[a] == self.rank[b] {
            self.rank[a] += 1;
        }
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------- normalisation

    #[test]
    fn strips_reply_prefixes_in_several_languages() {
        assert_eq!(normalise_subject("Re: Facture"), "facture");
        assert_eq!(normalise_subject("RE: Facture"), "facture");
        assert_eq!(normalise_subject("Ré: Facture"), "facture");
        assert_eq!(normalise_subject("TR: Facture"), "facture");
        assert_eq!(normalise_subject("Fwd: Facture"), "facture");
        assert_eq!(normalise_subject("AW: Facture"), "facture");
    }

    #[test]
    fn strips_stacked_and_numbered_prefixes() {
        assert_eq!(normalise_subject("Re: Re: Fwd: Facture"), "facture");
        assert_eq!(normalise_subject("Re[2]: Facture"), "facture");
        assert_eq!(normalise_subject("RE : Facture"), "facture");
    }

    #[test]
    fn strips_a_mailing_list_marker() {
        assert_eq!(normalise_subject("[rust-fr] Re: Question"), "question");
        assert_eq!(normalise_subject("Re: [rust-fr] Question"), "question");
    }

    #[test]
    fn collapses_case_and_whitespace() {
        assert_eq!(
            normalise_subject("  Facture   DU   Mois \n"),
            "facture du mois"
        );
    }

    #[test]
    fn does_not_strip_a_word_that_merely_starts_like_a_prefix() {
        // « Reçu » commence par « re » mais n'est pas un préfixe de réponse.
        assert_eq!(normalise_subject("Reçu de paiement"), "reçu de paiement");
        assert_eq!(normalise_subject("Refacturation"), "refacturation");
        assert_eq!(normalise_subject("Tresorerie"), "tresorerie");
    }

    #[test]
    fn an_empty_or_prefix_only_subject_normalises_to_nothing_usable() {
        assert_eq!(normalise_subject(""), "");
        assert_eq!(normalise_subject("Re:"), "");
        assert_eq!(normalise_subject("   "), "");
    }

    #[test]
    fn normalisation_never_panics_on_multibyte_input() {
        for subject in ["日本語の件名", "Ré: 日本語", "🙂 Re: 🙂", "é", "["] {
            drop(normalise_subject(subject));
        }
    }

    // ---------------------------------------------------------------- union-find

    #[test]
    fn union_find_partitions_correctly() {
        let mut union = UnionFind::new(6);
        assert!(union.join(0, 1));
        assert!(union.join(1, 2));
        assert!(!union.join(0, 2), "déjà réunis");
        assert!(union.join(3, 4));

        assert_eq!(union.find(0), union.find(2));
        assert_ne!(union.find(0), union.find(3));
        assert_ne!(union.find(5), union.find(0));
    }

    #[test]
    fn union_find_handles_a_very_long_chain_without_overflowing_the_stack() {
        // Une chaîne de réponses de 100 000 messages : la version récursive déborderait.
        let size = 100_000;
        let mut union = UnionFind::new(size);
        for index in 1..size {
            union.join(index - 1, index);
        }
        assert_eq!(union.find(0), union.find(size - 1));
    }

    #[test]
    fn union_find_is_stable_under_repeated_lookups() {
        let mut union = UnionFind::new(10);
        union.join(0, 5);
        union.join(5, 9);
        let root = union.find(0);
        for _ in 0..100 {
            assert_eq!(union.find(9), root);
        }
    }

    // ---------------------------------------------------------------- la règle, seule

    /// Un nœud, écrit court : les tests de la règle n'ont besoin ni de store ni de blob.
    fn node(id: i64, date: i64, rfc822: Option<&str>, subject: &str, refs: &[&str]) -> Node {
        Node {
            id: MessageId(id),
            date,
            rfc822_id: rfc822.map(ToOwned::to_owned),
            subject_norm: normalise_subject(subject),
            references: refs.iter().map(|it| (*it).to_owned()).collect(),
        }
    }

    /// Les composantes, en identifiants, triées — comparables d'une exécution à l'autre.
    fn components(nodes: &[Node], partition: &Partition) -> Vec<Vec<i64>> {
        let mut all: Vec<Vec<i64>> = partition
            .groups
            .iter()
            .map(|members| {
                let mut ids: Vec<i64> = members.iter().map(|&i| nodes[i].id.0).collect();
                ids.sort_unstable();
                ids
            })
            .collect();
        all.sort();
        all
    }

    #[test]
    fn a_reference_joins_two_messages() {
        let nodes = [
            node(1, 100, Some("a@x"), "Facture du mois", &[]),
            node(2, 200, Some("b@x"), "Re: Facture du mois", &["a@x"]),
        ];
        let partition = partition(&nodes, &HashSet::new());
        assert_eq!(components(&nodes, &partition), vec![vec![1, 2]]);
        assert_eq!(partition.links_by_references, 1);
        assert_eq!(partition.links_by_subject, 0, "la référence a suffi");
    }

    #[test]
    fn a_subject_joins_what_no_reference_did() {
        let nodes = [
            node(1, 100, Some("a@x"), "Facture du mois", &[]),
            node(2, 200, Some("b@x"), "Re: Facture du mois", &[]),
        ];
        let partition = partition(&nodes, &HashSet::new());
        assert_eq!(components(&nodes, &partition), vec![vec![1, 2]]);
        assert_eq!(partition.links_by_subject, 1);
    }

    #[test]
    fn a_message_linked_by_reference_leaves_its_subject_group() {
        // Le garde-fou qui compte : 1 et 2 sont liés par une référence, donc 3 — qui porte le
        // même sujet sans référence — reste seul. Sans cette règle, tout « Re: … » du corpus
        // finirait dans le même fil.
        let nodes = [
            node(1, 100, Some("a@x"), "Facture du mois", &[]),
            node(2, 200, Some("b@x"), "Re: Facture du mois", &["a@x"]),
            node(3, 300, Some("c@x"), "Facture du mois", &[]),
        ];
        let partition = partition(&nodes, &HashSet::new());
        assert_eq!(components(&nodes, &partition), vec![vec![1, 2], vec![3]]);
    }

    #[test]
    fn a_subject_group_beyond_the_cap_is_refused_whole() {
        let nodes: Vec<Node> = (0..=MAX_SUBJECT_GROUP as i64)
            .map(|index| node(index + 1, 100 + index, None, "Votre notification", &[]))
            .collect();
        let partition = partition(&nodes, &HashSet::new());
        assert_eq!(
            partition.groups.len(),
            MAX_SUBJECT_GROUP + 1,
            "un groupe trop gros ne fusionne rien du tout"
        );
        assert_eq!(partition.subject_groups_rejected, 1);
    }

    #[test]
    fn a_subject_declared_oversized_by_the_caller_is_refused_too() {
        // Le cas d'une passe locale : elle ne voit que deux membres du groupe, mais elle sait
        // que le groupe réel en compte trente. Sans ce paramètre, elle fusionnerait les deux et
        // rendrait un découpage qu'une reconstruction ne donnerait jamais.
        let nodes = [
            node(1, 100, None, "Votre notification", &[]),
            node(2, 200, None, "Votre notification", &[]),
        ];
        let mut oversized = HashSet::new();
        oversized.insert("votre notification".to_owned());

        let partition = partition(&nodes, &oversized);
        assert_eq!(components(&nodes, &partition), vec![vec![1], vec![2]]);
        assert_eq!(partition.subject_groups_rejected, 1);
    }

    #[test]
    fn a_short_subject_never_merges() {
        let nodes = [
            node(1, 100, None, "ok", &[]),
            node(2, 200, None, "Re: ok", &[]),
        ];
        let partition = partition(&nodes, &HashSet::new());
        assert_eq!(components(&nodes, &partition), vec![vec![1], vec![2]]);
    }

    #[test]
    fn an_unresolved_reference_is_counted_not_linked() {
        let nodes = [node(1, 100, Some("b@x"), "Facture du mois", &["absent@x"])];
        let partition = partition(&nodes, &HashSet::new());
        assert_eq!(partition.references_unresolved, 1);
        assert_eq!(components(&nodes, &partition), vec![vec![1]]);
    }

    #[test]
    fn a_message_that_references_itself_is_not_linked_to_itself() {
        // Vu dans le corpus : des clients écrivent leur propre identifiant dans `References`.
        let nodes = [node(1, 100, Some("a@x"), "Facture du mois", &["a@x"])];
        let partition = partition(&nodes, &HashSet::new());
        assert!(!partition.linked[0], "se citer soi-même n'est pas un lien");
        assert_eq!(partition.links_by_references, 0);
    }

    // ---------------------------------------------------------------- les deux passes

    /// Un store vide, avec un compte et un dossier.
    fn store() -> (tempfile::TempDir, Store) {
        use crate::model::{AuthKind, FolderKind, Security, Server};

        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = Store::open(root).unwrap();

        let writer = store.writer().unwrap();
        let account = writer
            .upsert_imap_account(
                "moi@exemple.fr",
                &Server {
                    host: "imap.exemple.fr".to_owned(),
                    port: 993,
                    username: "moi@exemple.fr".to_owned(),
                    auth: AuthKind::Password,
                    security: Security::Tls,
                },
            )
            .unwrap();
        writer
            .upsert_folder(account, "INBOX", FolderKind::Inbox)
            .unwrap();
        writer.commit().unwrap();
        (dir, store)
    }

    /// Ce qu'il faut pour décrire un message dans un test : son identifiant, son sujet, ce qu'il
    /// référence, et quand il est arrivé.
    struct Incoming<'a> {
        rfc822_id: &'a str,
        subject: &'a str,
        references: &'a [&'a str],
        date: i64,
    }

    /// Ajoute un message au store, comme une moisson le ferait : blob, ligne, référence.
    fn add(store: &Store, message: &Incoming<'_>) {
        use crate::model::MessageFlags;
        use crate::store::write::NewMessage;

        let references = if message.references.is_empty() {
            String::new()
        } else {
            let list = message
                .references
                .iter()
                .map(|it| format!("<{it}>"))
                .collect::<Vec<_>>()
                .join(" ");
            format!("In-Reply-To: <{}>\r\nReferences: {list}\r\n", {
                // `In-Reply-To` désigne le parent direct : le dernier de la chaîne.
                message.references.last().copied().unwrap_or_default()
            })
        };
        let raw = format!(
            "From: Quelqu'un <gens@exemple.fr>\r\n\
             To: moi@exemple.fr\r\n\
             Subject: {}\r\n\
             Message-ID: <{}>\r\n\
             {references}\
             \r\n\
             corps\r\n",
            message.subject, message.rfc822_id
        );

        let folder = store.folders().unwrap()[0].id;
        let blob = store.blobs().put(raw.as_bytes()).unwrap().hash;
        let writer = store.writer().unwrap();
        let (id, _) = writer
            .insert_message(&NewMessage {
                blob,
                rfc822_id: Some(message.rfc822_id),
                date: message.date,
                from_addr: "gens@exemple.fr",
                from_name: None,
                subject: message.subject,
                size: raw.len() as u64,
                has_attachments: false,
            })
            .unwrap();
        writer
            .insert_ref(id, folder, message.date, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();
    }

    /// Le découpage tel qu'il est rangé : un fil par ligne, ses messages triés.
    ///
    /// Compare des **ensembles de messages**, jamais des identifiants de fil : deux chemins
    /// peuvent rendre le même découpage sans numéroter les fils pareil, et c'est le découpage
    /// qui est la propriété.
    fn threading(store: &Store) -> Vec<Vec<i64>> {
        let mut statement = store
            .connection()
            .prepare("SELECT thread_id, id FROM messages ORDER BY thread_id, id")
            .unwrap();
        let rows: Vec<(Option<i64>, i64)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();

        let mut by_thread: HashMap<Option<i64>, Vec<i64>> = HashMap::new();
        for (thread, message) in rows {
            by_thread.entry(thread).or_default().push(message);
        }
        let mut all: Vec<Vec<i64>> = by_thread.into_values().collect();
        for group in &mut all {
            group.sort_unstable();
        }
        all.sort();
        all
    }

    /// Le corpus des tests d'équivalence : chaque mécanisme de la règle y est représenté.
    fn corpus() -> Vec<Incoming<'static>> {
        let mut all = vec![
            // Une conversation par références, en trois messages.
            Incoming {
                rfc822_id: "devis-1@x",
                subject: "Devis pour la toiture",
                references: &[],
                date: 1_000,
            },
            Incoming {
                rfc822_id: "devis-2@x",
                subject: "Re: Devis pour la toiture",
                references: &["devis-1@x"],
                date: 1_100,
            },
            Incoming {
                rfc822_id: "devis-3@x",
                subject: "Re: Devis pour la toiture",
                references: &["devis-1@x", "devis-2@x"],
                date: 1_200,
            },
            // Une conversation sans aucune référence : c'est le sujet qui la tient. Sur le
            // corpus réel, c'est le mécanisme **principal** — 11,4 % des messages seulement
            // portent une référence, voir `docs/PHASE-1.md`.
            Incoming {
                rfc822_id: "reunion-1@x",
                subject: "Reunion de chantier",
                references: &[],
                date: 2_000,
            },
            Incoming {
                rfc822_id: "reunion-2@x",
                subject: "Re: Reunion de chantier",
                references: &[],
                date: 2_100,
            },
            // Un message qui cite un parent qu'on n'a pas : il reste seul.
            Incoming {
                rfc822_id: "orphelin@x",
                subject: "Suite de notre echange",
                references: &["jamais-recu@x"],
                date: 3_000,
            },
            // Un sujet trop court pour porter un repli.
            Incoming {
                rfc822_id: "court-1@x",
                subject: "ok",
                references: &[],
                date: 4_000,
            },
            Incoming {
                rfc822_id: "court-2@x",
                subject: "Re: ok",
                references: &[],
                date: 4_100,
            },
        ];
        // Un groupe de sujet **juste au-dessus** du plafond : rien ne doit fusionner. C'est la
        // frontière, donc c'est là que les deux chemins ont le plus de chances de diverger.
        for index in 0..=MAX_SUBJECT_GROUP {
            all.push(Incoming {
                rfc822_id: Box::leak(format!("notif-{index}@x").into_boxed_str()),
                subject: "Votre notification hebdomadaire",
                references: &[],
                date: 5_000 + index as i64,
            });
        }
        all
    }

    #[test]
    fn an_incremental_pass_gives_exactly_what_a_rebuild_gives() {
        // **La propriété qui justifie toute la mécanique du voisinage.** Deux chemins mènent
        // maintenant aux fils, et s'ils divergeaient la différence ne se verrait qu'à l'usage :
        // un fil coupé en deux, sans rien pour le signaler. Les deux appellent `partition`, et
        // ce test fige que le voisinage suffit à lui donner le même contexte.
        let messages = corpus();

        let (_dir_full, full) = store();
        for message in &messages {
            add(&full, message);
        }
        rebuild(&full, &Progress::new()).unwrap();

        let (_dir_step, step) = store();
        for message in &messages {
            add(&step, message);
            update(&step, &Progress::new()).unwrap();
        }

        assert_eq!(threading(&full), threading(&step));
        assert!(
            threading(&full).len() > 1,
            "un corpus qui ne donne qu'un fil ne prouve rien"
        );
    }

    #[test]
    fn the_order_of_arrival_does_not_change_the_threading() {
        // Le même corpus, apporté à l'envers. C'est le cas que le drapeau par ligne ne savait
        // pas traiter : le parent arrive **après** sa réponse, donc le fil doit être recalculé
        // en arrière.
        let messages = corpus();

        let (_dir_a, forward) = store();
        for message in &messages {
            add(&forward, message);
            update(&forward, &Progress::new()).unwrap();
        }

        let (_dir_b, backward) = store();
        for message in messages.iter().rev() {
            add(&backward, message);
            update(&backward, &Progress::new()).unwrap();
        }

        // Les identifiants internes diffèrent — l'ordre d'insertion n'est pas le même — donc on
        // compare les fils par leurs sujets.
        let names = |store: &Store| {
            let mut all: Vec<Vec<String>> = threading(store)
                .into_iter()
                .map(|group| {
                    let mut subjects: Vec<String> = group
                        .iter()
                        .map(|id| {
                            store
                                .connection()
                                .query_row(
                                    "SELECT message_id FROM messages WHERE id = ?1",
                                    [id],
                                    |row| row.get::<_, String>(0),
                                )
                                .unwrap()
                        })
                        .collect();
                    subjects.sort();
                    subjects
                })
                .collect();
            all.sort();
            all
        };
        assert_eq!(names(&forward), names(&backward));
    }

    #[test]
    fn a_reply_arriving_later_joins_the_thread_of_its_parent() {
        // Le défaut que cette passe existe pour corriger : avant, une réponse arrivée par la
        // moisson restait hors de tout fil jusqu'à un `mail thread` lancé à la main.
        let (_dir, store) = store();
        add(
            &store,
            &Incoming {
                rfc822_id: "parent@x",
                subject: "Devis pour la toiture",
                references: &[],
                date: 1_000,
            },
        );
        update(&store, &Progress::new()).unwrap();

        add(
            &store,
            &Incoming {
                rfc822_id: "enfant@x",
                subject: "Re: Devis pour la toiture",
                references: &["parent@x"],
                date: 1_100,
            },
        );
        let stats = update(&store, &Progress::new()).unwrap();

        assert!(!stats.full_pass, "une réponse ne justifie pas tout refaire");
        assert_eq!(threading(&store), vec![vec![1, 2]]);
    }

    #[test]
    fn a_parent_arriving_later_merges_two_threads_into_one() {
        // L'autre sens, et le plus dur : deux messages existent déjà, chacun dans son fil,
        // et le message qui arrive les relie. Un fil doit être **dissous**.
        let (_dir, store) = store();
        add(
            &store,
            &Incoming {
                rfc822_id: "gauche@x",
                subject: "Un premier sujet bien assez long",
                references: &[],
                date: 1_000,
            },
        );
        add(
            &store,
            &Incoming {
                rfc822_id: "droite@x",
                subject: "Un autre sujet tout aussi long",
                references: &[],
                date: 1_100,
            },
        );
        update(&store, &Progress::new()).unwrap();
        assert_eq!(threading(&store), vec![vec![1], vec![2]], "deux fils");

        add(
            &store,
            &Incoming {
                rfc822_id: "pont@x",
                subject: "Re: Un premier sujet bien assez long",
                references: &["gauche@x", "droite@x"],
                date: 1_200,
            },
        );
        let stats = update(&store, &Progress::new()).unwrap();

        assert_eq!(threading(&store), vec![vec![1, 2, 3]]);
        assert_eq!(
            stats.threads_dissolved, 1,
            "un fils de trop doit disparaître"
        );
        let remaining: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "le fil dissous est bien supprimé");
    }

    #[test]
    fn a_message_pulled_out_of_its_subject_group_leaves_the_others_together() {
        // Le cas que seul le voisinage fermé attrape. Trois messages partagent un sujet et sont
        // réunis par lui. Puis une réponse arrive, qui cite le premier : il **quitte** le groupe
        // de sujet, et les deux autres doivent rester ensemble sans lui.
        let (_dir, store) = store();
        for (index, id) in ["a@x", "b@x", "c@x"].iter().enumerate() {
            add(
                &store,
                &Incoming {
                    rfc822_id: id,
                    subject: "Commande du mois de mars",
                    references: &[],
                    date: 1_000 + index as i64,
                },
            );
        }
        update(&store, &Progress::new()).unwrap();
        assert_eq!(threading(&store), vec![vec![1, 2, 3]]);

        add(
            &store,
            &Incoming {
                rfc822_id: "reponse@x",
                subject: "Un sujet completement different",
                references: &["a@x"],
                date: 2_000,
            },
        );
        update(&store, &Progress::new()).unwrap();

        // Le même corpus reconstruit d'un bloc, pour que la vérification porte sur la règle et
        // non sur ce que le test croit qu'elle dit.
        let expected = {
            let (_dir, fresh) = store_with_same_messages();
            rebuild(&fresh, &Progress::new()).unwrap();
            threading(&fresh)
        };
        assert_eq!(threading(&store), expected);
    }

    /// Le corpus de `a_message_pulled_out_of_its_subject_group_leaves_the_others_together`,
    /// monté d'un coup.
    fn store_with_same_messages() -> (tempfile::TempDir, Store) {
        let (dir, store) = store();
        for (index, id) in ["a@x", "b@x", "c@x"].iter().enumerate() {
            add(
                &store,
                &Incoming {
                    rfc822_id: id,
                    subject: "Commande du mois de mars",
                    references: &[],
                    date: 1_000 + index as i64,
                },
            );
        }
        add(
            &store,
            &Incoming {
                rfc822_id: "reponse@x",
                subject: "Un sujet completement different",
                references: &["a@x"],
                date: 2_000,
            },
        );
        (dir, store)
    }

    #[test]
    fn no_message_points_at_a_deleted_thread() {
        // L'invariant que la clé étrangère protège, vérifié pour lui-même : une passe locale
        // supprime des fils, et un `thread_id` pendant ferait disparaître un message de toute
        // liste sans qu'aucune erreur ne le dise.
        let messages = corpus();
        let (_dir, store) = store();
        for message in &messages {
            add(&store, message);
            update(&store, &Progress::new()).unwrap();
        }

        let dangling: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM messages m
                 WHERE m.thread_id IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM threads t WHERE t.id = m.thread_id)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dangling, 0);

        // Et l'inverse : aucun fil vide ne survit à une séparation.
        let empty: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM threads t
                 WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = t.id)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(empty, 0, "un fil sans message reste dans la liste des fils");
    }

    #[test]
    fn the_message_count_of_a_thread_matches_what_it_holds() {
        // `threads.message_count` est dénormalisé : c'est lui que les listes affichent, et il
        // dérive après une séparation si la composante réécrite n'est pas la seule à compter.
        let messages = corpus();
        let (_dir, store) = store();
        for message in &messages {
            add(&store, message);
            update(&store, &Progress::new()).unwrap();
        }

        let wrong: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM threads t
                 WHERE t.message_count <> (SELECT COUNT(*) FROM messages m WHERE m.thread_id = t.id)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(wrong, 0);
    }

    #[test]
    fn a_pass_with_nothing_to_do_touches_nothing() {
        // Le cas de très loin le plus fréquent : `IDLE` met une moisson en file à chaque arrivée
        // de courrier, et la plupart n'apportent rien. Elle doit ne rien parcourir.
        let (_dir, store) = store();
        add(
            &store,
            &Incoming {
                rfc822_id: "seul@x",
                subject: "Un sujet bien assez long pour compter",
                references: &[],
                date: 1_000,
            },
        );
        update(&store, &Progress::new()).unwrap();

        let stats = update(&store, &Progress::new()).unwrap();
        assert_eq!(
            stats,
            ThreadStats::default(),
            "une passe à vide ne fait rien"
        );
    }

    #[test]
    fn a_pass_always_finishes_and_leaves_nothing_unthreaded() {
        // La borne de la boucle. Le contrôle négatif du carnet, le 2026-09-11 : en retirant le
        // marquage, les tests ne tombent pas, ils **pendent**. Ici le marquage est le
        // rattachement lui-même, et ce test dit qu'il a bien lieu pour tout le monde.
        let (_dir, store) = store();
        for index in 0..(BATCH * 2 + 7) {
            add(
                &store,
                &Incoming {
                    rfc822_id: Box::leak(format!("m{index}@x").into_boxed_str()),
                    subject: Box::leak(format!("Sujet numero {}", index % 9).into_boxed_str()),
                    references: &[],
                    date: 1_000 + index as i64,
                },
            );
        }
        update(&store, &Progress::new()).unwrap();
        assert_eq!(store.unthreaded_total().unwrap(), 0);
    }

    #[test]
    fn a_store_threaded_before_the_derived_facts_is_rebuilt_once() {
        // Un store rattaché avant `SCHEMA_V13` : ses fils sont justes, mais rien ne dit qui
        // répond à quoi. La passe locale n'a pas de quoi travailler, et elle le **dit** plutôt
        // que de rendre un découpage faux.
        let (_dir, store) = store();
        add(
            &store,
            &Incoming {
                rfc822_id: "ancien@x",
                subject: "Un sujet bien assez long pour compter",
                references: &[],
                date: 1_000,
            },
        );
        rebuild(&store, &Progress::new()).unwrap();
        // Ce que la migration laisse : un fil, aucun fait.
        store
            .connection()
            .execute(
                "UPDATE messages SET thread_link = NULL, subject_norm = NULL",
                [],
            )
            .unwrap();
        store
            .connection()
            .execute("DELETE FROM message_references", [])
            .unwrap();

        add(
            &store,
            &Incoming {
                rfc822_id: "neuf@x",
                subject: "Re: Un sujet bien assez long pour compter",
                references: &["ancien@x"],
                date: 1_100,
            },
        );
        let stats = update(&store, &Progress::new()).unwrap();

        assert!(
            stats.full_pass,
            "la passe locale doit renoncer en le disant"
        );
        assert_eq!(threading(&store), vec![vec![1, 2]]);
        assert_eq!(
            store.threaded_without_facts().unwrap(),
            0,
            "équipé pour de bon"
        );
    }

    #[test]
    fn a_rebuild_records_the_facts_the_local_pass_needs() {
        // Ce qui rend la passe locale possible : un blob n'est lu qu'une fois dans la vie d'un
        // message. Sans ces trois faits rangés, il faudrait relire les 73 000 blobs à chaque
        // arrivée de courrier — 334 s à froid, voir `docs/PHASE-1.md`.
        let (_dir, store) = store();
        add(
            &store,
            &Incoming {
                rfc822_id: "parent@x",
                subject: "Devis pour la toiture",
                references: &[],
                date: 1_000,
            },
        );
        add(
            &store,
            &Incoming {
                rfc822_id: "enfant@x",
                subject: "Re: Devis pour la toiture",
                references: &["parent@x"],
                date: 1_100,
            },
        );
        rebuild(&store, &Progress::new()).unwrap();

        assert_eq!(store.threaded_without_facts().unwrap(), 0);
        assert_eq!(
            store.threaded_referencing("parent@x").unwrap(),
            vec![MessageId(2)],
            "la question que les blobs ne savent pas poser"
        );
        let subject: String = store
            .connection()
            .query_row(
                "SELECT subject_norm FROM messages WHERE id = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(subject, "devis pour la toiture", "le préfixe est retiré");
    }

    #[test]
    fn a_message_whose_blob_vanished_still_gets_a_thread() {
        // Un blob absent est un état diagnosticable, pas une panne. Le message garde son sujet,
        // donc son repli, et il est rattaché comme les autres.
        let (_dir, store) = store();
        add(
            &store,
            &Incoming {
                rfc822_id: "perdu@x",
                subject: "Un sujet bien assez long pour compter",
                references: &[],
                date: 1_000,
            },
        );
        std::fs::remove_dir_all(store.root().join("blobs")).unwrap();

        let stats = update(&store, &Progress::new()).unwrap();
        assert_eq!(stats.missing_blobs, 1);
        assert_eq!(store.unthreaded_total().unwrap(), 0);
    }

    #[test]
    fn the_partition_is_the_same_from_one_run_to_the_next() {
        // Les composantes viennent d'une `HashMap` : sans l'ordre imposé par `group`, deux
        // exécutions rendraient les mêmes fils dans un ordre différent, et la racine d'un fil
        // dépendrait du hasard.
        let nodes: Vec<Node> = (0..40)
            .map(|index| {
                node(
                    index + 1,
                    100 + index,
                    Some(&format!("m{index}@x")),
                    &format!("Sujet numéro {}", index % 7),
                    &[],
                )
            })
            .collect();
        let first = partition(&nodes, &HashSet::new());
        for _ in 0..5 {
            let again = partition(&nodes, &HashSet::new());
            assert_eq!(first.groups, again.groups);
        }
    }
}

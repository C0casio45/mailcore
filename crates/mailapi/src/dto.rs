//! Ce que l'API rend aux clients : UI, CLI, et tout ce qui n'est pas un modèle.
//!
//! Des types à part de ceux de `mailmcp`, et ce n'est pas de la duplication. Un modèle de
//! langage et une interface ne veulent pas les mêmes données :
//!
//! | | `mailmcp` | ici |
//! |---|---|---|
//! | Date | `AAAA-MM-JJ`, pour qu'un modèle la lise | secondes Unix, pour que le client la formate selon la locale |
//! | Drapeaux | absents, sans intérêt pour un modèle | `unread` / `flagged`, c'est la moitié d'une liste de mail |
//! | Pagination | une limite, et « tronqué » | un curseur opaque, parce qu'il faut défiler 100 000 messages |
//! | Dossiers d'un message | des chemins | chemins **et** identifiants, pour pouvoir naviguer |
//!
//! Fusionner les deux forcerait chaque changement d'un client sur l'autre.
//!
//! ## Sérialisation dans les deux sens
//!
//! Tout dérive `Serialize` **et** `Deserialize`. Le démon écrit, les clients relisent, et
//! c'est le même code de type qui garantit qu'ils s'accordent : un champ renommé casse à la
//! compilation côté client au lieu de rendre `null` à l'exécution.
//!
//! ## Ce qui ne sort pas d'ici
//!
//! Aucun `BlobHash`, aucun chemin de fichier, aucun HTML brut. Le hash révélerait la
//! structure du stockage sans rien apporter ; le HTML fidèle est l'affaire du front, qui le
//! rend dans une `<iframe>` sous CSP (`docs/PRIVACY.md`) — l'API sert du texte aplati
//! jusqu'à ce que ce moteur de rendu existe.

use mailcore::{FolderSummary, ListItem, MessageFlags, StoreStats};
use serde::{Deserialize, Serialize};

/// Ce que le démon dit de lui-même, en un aller-retour.
///
/// Appelé en premier par un client : il y trouve de quoi décider s'il peut parler à ce
/// démon, s'il peut chercher, et si son cache de lecture est encore à jour — sans avoir
/// enchaîné trois requêtes (critère 1 de `docs/PHASE-1.md`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    /// Toujours `mailcore`.
    pub server: String,
    /// La version du démon.
    pub version: String,
    /// La version du contrat exposé par ce module. Voir [`crate::PROTOCOL`].
    pub protocol: u32,
    /// Faux quand l'index plein texte est absent : `search.query` rendra une liste vide,
    /// et un client honnête le dit à l'utilisateur au lieu d'afficher « aucun résultat ».
    pub search_available: bool,
    /// La révision du store. Voir [`Revision`].
    pub revision: String,
    /// Nombre de messages, pour afficher quelque chose immédiatement.
    pub messages: u64,
}

/// Un dossier et ses compteurs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    /// Identifiant à passer à `messages.page`.
    pub id: i64,
    /// Le compte auquel il appartient, tel que l'utilisateur le nomme.
    pub account: String,
    /// Chemin complet, séparé par `/`.
    pub path: String,
    /// Rôle deviné : `inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`, `other`.
    pub kind: String,
    /// Nombre de messages référencés.
    pub total: u64,
    /// Nombre de messages non lus.
    pub unread: u64,
}

impl From<FolderSummary> for Folder {
    fn from(summary: FolderSummary) -> Self {
        Self {
            id: summary.folder.id.0,
            account: summary.account_name,
            path: summary.folder.path,
            kind: summary.folder.kind.as_str().to_owned(),
            total: summary.total,
            unread: summary.unread,
        }
    }
}

/// Une ligne de liste : ce qu'il faut pour dessiner une ligne, et rien de plus.
///
/// Ce type est celui que le client met en cache par milliers (critère 9 : la liste reste
/// défilable démon injoignable). Il reste donc plat et petit — pas de `Vec`, pas de champ
/// dont la taille dépende du message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    /// Identifiant à passer à `messages.get` ou `messages.thread`.
    pub id: i64,
    /// Date d'envoi, en secondes Unix. `0` quand l'en-tête `Date` était absent ou illisible.
    ///
    /// Des secondes et pas une chaîne : le client formate selon la locale et le fuseau de
    /// l'utilisateur, et trie sans reparser.
    pub date: i64,
    /// Adresse de l'expéditeur, en minuscules.
    pub from: String,
    /// Nom affiché de l'expéditeur, s'il y en avait un.
    pub from_name: Option<String>,
    /// Sujet, décodé.
    pub subject: String,
    /// Vrai si le message déclare des pièces jointes.
    pub has_attachments: bool,
    /// Vrai si le message n'a pas été lu.
    ///
    /// Exposé en positif — « non lu » — et non le `SEEN` de la RFC : c'est ce que l'interface
    /// affiche, et une double négation dans une condition de rendu est un bug qui attend.
    pub unread: bool,
    /// Vrai si le message est marqué.
    pub flagged: bool,
    /// Pertinence, pour un résultat de recherche. Comparable au sein d'une même requête
    /// seulement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

impl Row {
    /// Construit une ligne depuis le cœur.
    #[must_use]
    pub fn new(item: &ListItem, score: Option<f32>) -> Self {
        Self {
            id: item.id.0,
            date: item.date,
            from: item.from_addr.clone(),
            from_name: item.from_name.clone(),
            subject: item.subject.clone(),
            has_attachments: item.has_attachments,
            // Hors d'un dossier — un résultat de recherche — les drapeaux sont vides, donc
            // `unread` vaut vrai. C'est le choix le moins mauvais : un message affiché à
            // tort comme non lu se corrige en l'ouvrant, l'inverse le cache.
            unread: !item.flags.contains(MessageFlags::SEEN),
            flagged: item.flags.contains(MessageFlags::FLAGGED),
            score,
        }
    }
}

/// Une page de liste, et de quoi demander la suivante.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    /// Les lignes, du plus récent au plus ancien.
    pub rows: Vec<Row>,
    /// Le curseur à repasser dans `after` pour la page suivante. `None` = fin de la liste.
    ///
    /// **Opaque.** Un client le stocke et le renvoie ; il ne l'analyse pas. Ça laisse
    /// changer la clé de pagination sans casser un client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// La révision du store au moment de la lecture. Un client qui la voit changer sait que
    /// sa page est peut-être périmée.
    pub revision: String,
}

/// Une pièce jointe : décrite, jamais rendue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// Le nom de fichier déclaré, s'il y en a un.
    pub name: Option<String>,
    /// Le type MIME déclaré. Une déclaration, pas une vérité.
    pub mime: String,
    /// La taille en octets, après décodage.
    pub size: u64,
}

/// La source d'un message : les octets tels qu'ils sont, rendus lisibles.
///
/// ## Pourquoi ce n'est pas un champ de plus dans [`Message`]
///
/// [`Message`] est ce qu'on lit d'un message — sujet décodé, corps aplati, pièces listées — et
/// il est demandé à chaque ouverture. La source est le contraire : rien n'y est décodé, elle
/// pèse la taille du message, et elle n'est demandée que quand quelqu'un veut vérifier. La
/// joindre à [`Message`] ferait payer à toutes les ouvertures le prix d'une vérification rare.
///
/// ## Les compteurs font partie de la réponse, pas d'une note de bas de page
///
/// `invalid_sequences` et `escaped_controls` disent en quoi ce qui est montré diffère des
/// octets. Une vue qui prétend montrer la vérité doit dire où elle a dû intervenir, sinon
/// « vous voyez tout » devient faux sans que personne ne puisse s'en apercevoir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSource {
    /// Le bloc d'en-têtes, verbatim : ni déplié, ni décodé.
    pub headers: String,
    /// Le corps, verbatim : l'encodage de transfert est montré tel quel.
    pub body: String,
    /// La taille réelle du message en octets, avant toute troncature.
    pub total: u64,
    /// Vrai si `headers` s'arrête avant la fin du bloc.
    pub headers_truncated: bool,
    /// Vrai si `body` s'arrête avant la fin du corps.
    pub body_truncated: bool,
    /// Nombre de séquences d'octets qui n'étaient pas de l'UTF-8 valide.
    pub invalid_sequences: usize,
    /// Nombre de caractères de contrôle rendus sous la forme `\xNN`.
    pub escaped_controls: usize,
}

impl MessageSource {
    /// Traduit ce que `mailcore` a rendu.
    #[must_use]
    pub fn new(source: mailcore::Source) -> Self {
        Self {
            headers: source.headers,
            body: source.body,
            total: source.total,
            headers_truncated: source.headers_truncated,
            body_truncated: source.body_truncated,
            invalid_sequences: source.invalid_sequences,
            escaped_controls: source.escaped_controls,
        }
    }
}

/// Un dossier qui référence un message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    /// Le chemin du dossier.
    pub path: String,
    /// Vrai si le message est non lu **dans ce dossier**.
    ///
    /// Les drapeaux appartiennent à la référence, pas au contenu : le même message peut être
    /// lu dans `INBOX` et non lu dans `[Gmail]/Tous les messages`.
    pub unread: bool,
}

/// Pourquoi une ressource distante est signalée comme traceur.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrackerKind {
    /// Image de 1×1, de dimensions nulles, ou cachée. Le pixel espion.
    Pixel,
    /// Hôte figurant dans la liste embarquée de traceurs connus.
    KnownDomain,
    /// URL portant ce qui ressemble à un identifiant corrélé au destinataire.
    CorrelatedId,
}

/// Un traceur relevé dans un message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tracker {
    /// Pourquoi il est signalé.
    pub kind: TrackerKind,
    /// L'hôte. **Jamais l'URL complète** : elle porte l'identifiant corrélé au destinataire,
    /// et la faire traverser le réseau pour l'afficher reviendrait à conserver ce qu'on
    /// dénonce (`docs/PRIVACY.md`, §3).
    pub host: String,
}

/// Le corps HTML d'un message, assaini, avec de quoi le confiner.
///
/// **La CSP voyage avec le HTML qu'elle protège.** Elle n'est pas recopiée dans le front :
/// une deuxième copie, même identique le jour où elle est écrite, est exactement le point de
/// défaillance unique que `docs/PRIVACY.md` cherche à écarter. Le client pose ce qu'il reçoit
/// sur l'`<iframe>`, sans en connaître le contenu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Html {
    /// Le HTML assaini. Vide quand le message est en texte seul.
    pub html: String,
    /// Vrai si le HTML a été tronqué.
    pub truncated: bool,
    /// La valeur à poser en `Content-Security-Policy` sur l'`<iframe>`.
    pub csp: String,
    /// La valeur à poser en `sandbox` sur l'`<iframe>`.
    pub sandbox: String,
    /// Nombre d'images distantes retirées par la politique en vigueur.
    pub blocked_images: usize,
    /// Nombre de ressources distantes que le message référençait.
    pub remote_resources: usize,
    /// Les traceurs relevés, sans doublon.
    pub trackers: Vec<Tracker>,
}

/// Un message ouvert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// La ligne de liste, mêmes champs qu'ailleurs.
    pub row: Row,
    /// L'en-tête `Message-ID`, s'il était présent.
    pub message_id: Option<String>,
    /// La chaîne `References`, du plus ancien au plus récent.
    ///
    /// Sert à **répondre** : la RFC 5322 §3.6.4 demande que le `References` d'une réponse soit
    /// celui du parent, prolongé de son `Message-ID`. Un client qui la reconstruirait depuis le
    /// fil local produirait une chaîne différente de celle des autres clients, et un lecteur
    /// qui regroupe sur `References` verrait deux fils au lieu d'un.
    #[serde(default)]
    pub references: Vec<String>,
    /// Les destinataires (`To` et `Cc`), décodés.
    pub to: Vec<String>,
    /// Le corps, **aplati en texte**. Toujours présent, quel que soit le format demandé :
    /// c'est le repli quand le rendu HTML n'est pas disponible ou pas voulu.
    pub body: String,
    /// Vrai si le corps texte a été tronqué parce qu'il dépassait la taille maximale.
    pub body_truncated: bool,
    /// Le corps HTML assaini, quand le client l'a demandé (`body: "html"`).
    ///
    /// Absent par défaut : le rendu HTML coûte un assainissement complet, et un client qui ne
    /// sait pas confiner du balisage n'a rien à en faire. Voir [`Html`] pour ce qui
    /// l'accompagne — la CSP et le `sandbox` en font partie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<Html>,
    /// Les pièces jointes, listées et non ouvertes.
    pub attachments: Vec<Attachment>,
    /// Les dossiers qui contiennent ce message.
    pub folders: Vec<Location>,
    /// Le fil, si la passe de threading est passée.
    pub thread: Option<i64>,
    /// Le rendez-vous que le message porte, quand il porte une pièce `text/calendar`.
    ///
    /// Absent pour la très grande majorité des messages : 930 pièces sur 73 825 messages du
    /// corpus réel. Le champ est donc omis du JSON quand il n'y en a pas, plutôt que rendu
    /// `null` — un client qui ne connaît pas les invitations ne voit aucune différence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation: Option<Invitation>,
}

/// Un fil de discussion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    /// Les messages, du plus ancien au plus récent.
    pub rows: Vec<Row>,
    /// Nombre de messages dans le fil.
    pub count: usize,
}

/// Un résultat de recherche.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Results {
    /// Les messages trouvés, par pertinence décroissante.
    pub rows: Vec<Row>,
    /// Nombre de résultats rendus.
    pub count: usize,
    /// Vrai si la limite demandée a été atteinte : il y a probablement d'autres résultats.
    pub truncated: bool,
    /// Faux quand l'index plein texte est absent. Une liste vide et une recherche
    /// impossible ne doivent pas se ressembler côté client.
    pub search_available: bool,
}

/// L'état du store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    /// Nombre de comptes.
    pub accounts: u64,
    /// Nombre de dossiers.
    pub folders: u64,
    /// Nombre de contenus uniques stockés.
    pub messages: u64,
    /// Nombre de références — un message référencé dans trois dossiers en compte trois.
    pub refs: u64,
    /// Nombre de messages qu'aucune passe de threading n'a encore rattachés.
    pub unthreaded: u64,
    /// Somme des tailles RFC 5322 brutes, en octets.
    pub raw_bytes: u64,
    /// Vrai si la recherche plein texte est disponible.
    pub search_available: bool,
    /// La révision du store.
    pub revision: String,
}

impl Stats {
    /// Construit l'état depuis le cœur.
    #[must_use]
    pub fn new(stats: &StoreStats, search_available: bool, revision: String) -> Self {
        Self {
            accounts: stats.accounts,
            folders: stats.folders,
            messages: stats.messages,
            refs: stats.refs,
            unthreaded: stats.unthreaded,
            raw_bytes: stats.raw_bytes,
            search_available,
            revision,
        }
    }
}

/// La révision du store.
///
/// **Opaque, et sans ordre.** Deux révisions se comparent par égalité seulement : «
/// différente » veut dire « relis », pas « plus récente ». Voir
/// `mailcore::Mailbox::revision` pour ce qu'elle observe réellement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    /// Le jeton.
    pub revision: String,
}

/// Un profil que l'opérateur autorise à importer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// Le rang à passer à `jobs.start`. **Un client ne donne jamais de chemin** — voir la
    /// documentation de `jobs.start`.
    pub id: usize,
    /// Le chemin, pour que l'utilisateur sache lequel il importe.
    ///
    /// L'opérateur l'a écrit lui-même sur la ligne de commande du démon : il n'y a rien à
    /// cacher ici, et une liste de rangs sans libellé serait inutilisable.
    pub path: String,
}

/// Une tâche de fond.
///
/// L'unité de [`Job::done`] et [`Job::total`] appartient à la tâche — des octets lus pour un
/// import, des messages pour une indexation. Un client qui affiche une fraction n'a pas
/// besoin de la connaître ; un client qui voudrait afficher « 3 Go sur 11 » doit regarder le
/// `kind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// L'identifiant.
    pub id: u64,
    /// `import`, `index` ou `thread`.
    pub kind: String,
    /// `queued`, `running`, `done`, `cancelled` ou `failed`.
    pub state: String,
    /// Unités faites.
    pub done: u64,
    /// Unités prévues. `0` tant que la tâche ne le sait pas.
    pub total: u64,
    /// La fraction accomplie, entre `0` et `1`.
    ///
    /// Absente quand le total est inconnu : une barre indéterminée est honnête, une barre à
    /// zéro qui ne bouge pas ressemble à une panne.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f32>,
    /// Le bilan en fin de course, ou la raison de l'échec.
    ///
    /// **Jamais un chemin de fichier ni un fragment de message.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Mise en file, en secondes Unix.
    pub queued_at: i64,
    /// Fin, en secondes Unix. Absente tant que la tâche n'est pas terminée.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
}

/// La réponse à `store.wait` : le mécanisme d'abonnement aux changements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    /// La révision courante.
    pub revision: String,
    /// Vrai si elle diffère de celle que le client a présentée. Faux = le délai a expiré
    /// sans que rien ne bouge, et le client peut rappeler immédiatement.
    pub changed: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use mailcore::{BlobHash, MessageId};

    fn item(flags: MessageFlags) -> ListItem {
        ListItem {
            id: MessageId(7),
            blob: BlobHash::of(b"x"),
            date: 1_700_000_000,
            from_addr: "a@b.c".to_owned(),
            from_name: None,
            subject: "sujet".to_owned(),
            has_attachments: false,
            flags,
        }
    }

    #[test]
    fn unread_is_the_absence_of_seen() {
        assert!(Row::new(&item(MessageFlags::empty()), None).unread);
        assert!(!Row::new(&item(MessageFlags::SEEN), None).unread);
    }

    #[test]
    fn flagged_survives_alongside_seen() {
        let row = Row::new(&item(MessageFlags::SEEN.union(MessageFlags::FLAGGED)), None);
        assert!(!row.unread);
        assert!(row.flagged);
    }

    #[test]
    fn a_row_never_carries_the_blob_hash() {
        // Le hash révélerait la structure du stockage sans rien apporter à un client.
        let json = serde_json::to_value(Row::new(&item(MessageFlags::empty()), None)).unwrap();
        assert!(json.get("blob").is_none());
        // Et un score absent ne sort pas du tout, plutôt que de sortir en `null`.
        assert!(json.get("score").is_none());
    }

    #[test]
    fn the_contract_survives_a_round_trip() {
        // Ce que le démon écrit, un client doit le relire : c'est ce qui garantit que les
        // deux côtés parlent du même type.
        let row = Row::new(&item(MessageFlags::SEEN), Some(1.5));
        let text = serde_json::to_string(&row).unwrap();
        let back: Row = serde_json::from_str(&text).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.score, Some(1.5));
        assert!(!back.unread);
    }
}

/// Un compte, tel qu'un client a besoin de le connaître pour envoyer.
///
/// ## Ce que ce DTO ne porte pas
///
/// Ni hôte, ni port, ni mécanisme d'authentification, ni bien sûr de secret. Un client qui
/// rédige un message a besoin de trois choses : quel compte, sous quelle adresse, et
/// **est-ce que celui-là peut envoyer**. La configuration du serveur est l'affaire de
/// l'opérateur du démon, et l'exposer donnerait à quiconque détient le jeton une carte de
/// l'infrastructure de messagerie de l'utilisateur.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// L'identifiant, celui que portent aussi les dossiers.
    pub id: i64,
    /// Le nom que l'utilisateur a choisi.
    pub name: String,
    /// L'adresse sous laquelle ce compte envoie. Absente pour un compte importé d'un mbox.
    pub address: Option<String>,
    /// Vrai si un serveur de soumission est configuré.
    ///
    /// **Ce que l'interface doit lire avant de proposer un bouton « envoyer »** — critère 8 de
    /// `docs/PHASE-3.md` : mieux vaut dire « ce compte n'a pas de serveur d'envoi » que laisser
    /// écrire un message pour le refuser au dernier moment.
    pub can_send: bool,
    /// Faux quand la synchronisation est en pause.
    pub enabled: bool,
}

/// Une ligne de la file d'envoi.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outgoing {
    /// L'identifiant de la ligne.
    pub id: i64,
    /// Le compte qui envoie.
    pub account: i64,
    /// L'expéditeur de l'enveloppe.
    pub sender: String,
    /// Les destinataires de l'**enveloppe**, copies cachées comprises.
    ///
    /// Un client qui affiche cette liste affiche donc les copies cachées. C'est correct : la
    /// file est celle de l'utilisateur, et ce qu'il a caché aux destinataires n'a pas à lui
    /// être caché à lui.
    pub recipients: Vec<String>,
    /// `queued`, `sending`, `committing`, `sent` ou `failed`.
    pub state: String,
    /// Vrai pour `committing` et pour rien d'autre.
    ///
    /// **Le champ que l'interface doit traiter à part.** Un envoi douteux ne repartira pas tout
    /// seul, et l'utilisateur doit décider. Dérivé plutôt que déduit du libellé, pour qu'un
    /// client n'ait pas à connaître la liste des états pour reconnaître celui qui compte.
    pub doubtful: bool,
    /// Combien de tentatives ont eu lieu.
    pub attempts: u32,
    /// Secondes Unix de la mise en file.
    pub queued_at: i64,
    /// Ce qu'il faut montrer à l'utilisateur. **Jamais un secret.**
    pub last_error: Option<String>,
    /// Vrai si `outbox.retry` a une chance de marcher sur cette ligne — critère 8.
    ///
    /// **Le champ qui décide d'un bouton.** [`Self::last_error`] dit à l'utilisateur quoi faire ;
    /// celui-ci dit si le client peut le faire pour lui. Un « Renvoyer » sur une adresse qui
    /// n'existe pas renverrait à la même adresse, et un bouton qui échoue à tous les coups est ce
    /// que le critère 8 interdit autant qu'un code numérique.
    ///
    /// Dérivé côté service, comme [`Self::doubtful`] : seule la couche qui a parlé au serveur
    /// sait pourquoi il a refusé, et un client n'a pas à deviner en lisant une phrase française.
    pub resendable: bool,
}

impl Outgoing {
    /// Convertit une ligne du store.
    #[must_use]
    pub fn new(it: &mailcore::Outgoing) -> Self {
        Self {
            id: it.id.0,
            account: it.account.0,
            sender: it.sender.clone(),
            recipients: it.recipients.clone(),
            state: it.state.as_str().to_owned(),
            doubtful: it.state.is_doubtful(),
            attempts: it.attempts,
            queued_at: it.queued_at,
            last_error: it.last_error.clone(),
            resendable: it.resendable,
        }
    }
}

/// Ce que `outbox.send` rend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Queued {
    /// La ligne créée, à l'état `queued`.
    pub id: i64,
    /// La taille du message assemblé, en octets.
    pub size: u64,
    /// Les destinataires de l'enveloppe, tels que le démon les a retenus.
    ///
    /// Rendus pour que le client puisse **vérifier** ce qu'il a demandé : une adresse mal
    /// analysée est refusée, mais une adresse tombée d'une liste ne se verrait pas autrement.
    pub recipients: Vec<String>,
    /// Combien de secondes la ligne attend avant que le facteur puisse la prendre.
    ///
    /// C'est la fenêtre pendant laquelle `outbox.cancel` marchera encore. Elle est **rendue**
    /// plutôt que connue du client : un client qui écrirait « 10 secondes » dans sa phrase
    /// mentirait le jour où le délai change, et un client qui ne sait pas combien de temps il
    /// lui reste ne peut pas proposer l'annulation honnêtement.
    ///
    /// `0` veut dire que la ligne est remettable tout de suite.
    #[serde(default)]
    pub hold: i64,
}

/// Une proposition de complétion d'un destinataire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    /// L'adresse, en minuscules.
    pub address: String,
    /// Le nom affiché, s'il y en a un.
    pub name: Option<String>,
    /// Ce qu'un champ de destinataire doit insérer : `Nom <adresse>`, ou l'adresse seule.
    ///
    /// Calculé par le service et non par le client, parce que la forme doit rester relisible par
    /// `mailsmtp::compose::Address` — et c'est le service qui l'écrira dans l'en-tête.
    pub label: String,
    /// Combien de fois l'utilisateur a écrit à cette adresse.
    pub seen_to: u32,
    /// Combien de fois elle lui a écrit.
    pub seen_from: u32,
    /// Le rang. Rendu pour que l'interface puisse **montrer pourquoi** une proposition est
    /// première, plutôt que de faire subir un ordre.
    pub score: i64,
}

impl Suggestion {
    /// Convertit une entrée du carnet.
    #[must_use]
    pub fn new(it: &mailcore::contacts::Contact) -> Self {
        Self {
            address: it.address.clone(),
            name: it.name.clone(),
            label: it.label(),
            seen_to: it.seen_to,
            seen_from: it.seen_from,
            score: it.score(),
        }
    }
}

/// Une pièce jointe, désignée par son contenu.
///
/// ## Le hachage, jamais un chemin
///
/// Un chemin dans une demande de client donnerait, à quiconque détient le jeton du démon, la
/// lecture de n'importe quel fichier de la machine — rangé dans un message, puis envoyé où il
/// veut. C'est la même raison qui fait que `jobs.start` prend un rang et jamais un chemin.
///
/// Avec un hachage, un client ne peut référencer que du contenu **déjà** dans le magasin de
/// blobs. Y mettre un fichier est une opération locale, faite par celui qui a le fichier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attached {
    /// Le nom montré au destinataire.
    pub filename: String,
    /// Le contenu, en hexadécimal — 64 caractères.
    pub blob: String,
    /// La taille du contenu brut, en octets. Le base64 en fera un tiers de plus.
    pub size: u64,
}

/// Un rendez-vous lu dans une pièce `text/calendar`.
///
/// ## Pourquoi c'est une projection et non `mailcal::Invitation` tel quel
///
/// `mailcal` n'a **aucune dépendance**, et c'est une propriété du crate : il lit des octets
/// écrits par un inconnu et n'a rien pour parler à qui que ce soit. Lui ajouter `serde` pour
/// qu'il traverse le réseau échangerait cette propriété contre une commodité. Le contrat de
/// fil est donc écrit ici, comme celui des pièces jointes et des lignes de liste — c'est le
/// rôle de ce module.
///
/// ## Les dates sortent en deux formes, et il faut les deux
///
/// `*_wall` est l'heure **telle que l'organisateur l'a écrite**, avec `zone` pour dire d'où elle
/// tient son sens ; `*_unix` est l'instant, quand il est déterminé. Un client qui n'aurait que
/// l'instant ne pourrait pas afficher « 14:00 » pour une réunion écrite à 14:00 ; un client qui
/// n'aurait que l'heure murale ne pourrait ni trier ni poser un rappel.
///
/// `*_unix` absent avec un `*_wall` présent n'est pas une panne : voir `caveats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invitation {
    /// Ce que le fichier demande : `invitation`, `annulation`, `réponse`, `contre-proposition`.
    pub kind: String,
    /// Le titre, déséchappé.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Le début, en heure murale : `AAAA-MM-JJ HH:MM` — ou `AAAA-MM-JJ` pour une journée
    /// entière.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_wall: Option<String>,
    /// L'instant du début, en secondes Unix, quand il est déterminé.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_unix: Option<i64>,
    /// La fin, en heure murale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_wall: Option<String>,
    /// L'instant de la fin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_unix: Option<i64>,
    /// D'où l'heure murale tient son sens : un nom de fuseau, `UTC`, `journée entière`, ou
    /// `heure locale` pour une heure flottante.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// Vrai pour un rendez-vous sur la journée entière.
    pub all_day: bool,
    /// Le lieu, tel qu'écrit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// L'organisateur.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organizer: Option<Participant>,
    /// Les participants, dans l'ordre du fichier.
    #[serde(default)]
    pub attendees: Vec<Participant>,
    /// L'état déclaré de l'événement : `CONFIRMED`, `TENTATIVE`, `CANCELLED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// La règle de répétition **telle qu'écrite**, jamais développée en occurrences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<String>,
    /// Les URL que l'invitation porte — visioconférence, agenda.
    ///
    /// **Montrées, jamais suivies.** Rien ne part tant que l'utilisateur n'a pas cliqué :
    /// `docs/PRIVACY.md`, règle 5. Seuls `http` et `https` sortent ici.
    #[serde(default)]
    pub urls: Vec<String>,
    /// Ce que l'invitation ne dit pas, en français et prêt à afficher.
    ///
    /// ## C'est la moitié du critère 6, et pas une liste d'erreurs
    ///
    /// « L'invitation ne dit pas dans quel fuseau elle est » est une phrase qu'une interface
    /// doit montrer **à côté** de l'heure, pas à la place. Un client qui ignore ce champ
    /// affichera une heure sans sa réserve, ce qui est exactement le mode de panne que le
    /// critère cherche à éviter.
    #[serde(default)]
    pub caveats: Vec<String>,
    /// Vrai quand rien ne peut être affiché : pas de rendez-vous, pas de début lisible.
    ///
    /// Les `caveats` disent alors pourquoi.
    pub refused: bool,
    /// Le nombre d'événements au-delà du premier, quand la pièce en porte plusieurs.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub extra_events: usize,
}

/// Un organisateur ou un participant d'une invitation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    /// Le nom affiché, s'il y en a un.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// L'adresse de courrier, quand la valeur en était une.
    ///
    /// Absente pour une salle ou une ressource, que le format écrit en `urn:` : écrire à
    /// `urn:x-resource:salle-12` n'irait nulle part, et l'afficher comme une adresse
    /// laisserait croire le contraire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Où en est sa réponse : `accepte`, `refuse`, `peut-être`, `sans réponse`, `autre`.
    pub answer: String,
    /// Vrai si sa présence est demandée et non optionnelle.
    pub required: bool,
}

/// Vrai pour zéro. Sert à omettre un compteur vide du JSON.
fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// Un brouillon, tel que le formulaire l'avait à l'écran.
///
/// ## Les adresses sont des chaînes, pas des listes
///
/// Contrairement à `outbox.send`, qui prend des listes validées : un brouillon garde ce qui
/// était **tapé**, virgules et fragments compris. Rouvrir doit montrer « jean@, mar » au milieu
/// d'une saisie, et découper à l'enregistrement pour recoller à la relecture perdrait le
/// fragment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    /// L'identifiant, absent pour un brouillon jamais enregistré.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Le compte qui enverra.
    pub account: i64,
    /// Le champ « À », tel que tapé.
    #[serde(default)]
    pub to: String,
    /// Le champ « Copie », tel que tapé.
    #[serde(default)]
    pub cc: String,
    /// Le champ « Copie cachée », tel que tapé.
    #[serde(default)]
    pub bcc: String,
    /// Le sujet.
    #[serde(default)]
    pub subject: String,
    /// Le corps en texte.
    #[serde(default)]
    pub body: String,
    /// Le `Message-ID` auquel ce brouillon répond.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// La chaîne `References` du fil, du plus ancien au plus récent.
    #[serde(default)]
    pub references: Vec<String>,
    /// Vrai si la signature du compte doit être ajoutée à l'envoi.
    #[serde(default)]
    pub sign: bool,
    /// Les pièces jointes, désignées par leur contenu — jamais par un chemin.
    #[serde(default)]
    pub attachments: Vec<Attached>,
    /// Quand il a été touché pour la dernière fois, en secondes Unix.
    #[serde(default)]
    pub updated_at: i64,
    /// De quoi se repérer dans une liste : sujet, à défaut destinataire, à défaut début du
    /// corps.
    ///
    /// Calculé par le service et non par le client : un brouillon sans sujet est le cas
    /// ordinaire — c'est souvent la dernière chose qu'on écrit — et trois clients qui
    /// choisiraient chacun leur repli afficheraient trois listes différentes.
    #[serde(default)]
    pub label: String,
}

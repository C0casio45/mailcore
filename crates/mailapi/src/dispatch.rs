//! Le répartiteur : un nom de méthode et des paramètres JSON, une valeur JSON en retour.
//!
//! **Synchrone, comme `mailcore`.** Aucun runtime async ici : c'est l'appelant — `maild` —
//! qui décide sur quel fil ça tourne, et il le fait dans un `spawn_blocking`. Un répartiteur
//! synchrone se teste sans monter ni socket ni exécuteur, ce qui est exactement ce qu'on
//! veut d'un morceau de code qui décide quoi montrer d'une boîte mail.
//!
//! ## `store.wait` n'est pas ici
//!
//! Une méthode manque à l'appel : [`method::STORE_WAIT`]. Elle attend qu'une révision
//! change, donc elle dort, donc elle appartient au transport et pas à un répartiteur
//! synchrone qui tiendrait le verrou de la boîte pendant toute l'attente. `maild`
//! l'intercepte avant d'arriver ici et la sert par une boucle de `store.revision`. Le nom
//! reste déclaré dans [`method`] pour que [`method::ALL`] soit la liste complète.

use mailcore::{FolderId, Mailbox, MessageId};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::dto;
use crate::jsonrpc::{self, Error};

/// Les noms de méthodes.
///
/// En dur et regroupés : une faute de frappe dans un nom de méthode devient une erreur de
/// compilation au lieu d'un `-32601` à l'exécution, et la liste sert de contrat lisible.
pub mod method {
    /// Ce que le démon dit de lui-même.
    pub const SERVER_HELLO: &str = "server.hello";
    /// La liste des méthodes servies.
    pub const SERVER_METHODS: &str = "server.methods";
    /// Les dossiers de tous les comptes.
    pub const FOLDERS_LIST: &str = "folders.list";
    /// Une page de messages d'un dossier.
    pub const MESSAGES_PAGE: &str = "messages.page";
    /// Un message ouvert.
    pub const MESSAGES_GET: &str = "messages.get";
    /// Le fil auquel appartient un message.
    pub const MESSAGES_THREAD: &str = "messages.thread";
    /// Marque un message lu dans un dossier. Servie par le transport.
    pub const MESSAGES_MARK_READ: &str = "messages.mark_read";
    /// Range une pièce jointe d'un message reçu dans le magasin, pour la joindre à un autre
    /// message. Servie par le transport.
    pub const MESSAGES_STAGE_PART: &str = "messages.stage_part";
    /// Les octets RFC 5322 d'un message reçu, rendus lisibles.
    ///
    /// Voir [`MESSAGES_SOURCE`] et [`OUTBOX_SOURCE`] pour ce que les deux servent : c'est la
    /// seconde qui tient la promesse de `docs/PHASE-3.md`.
    pub const MESSAGES_SOURCE: &str = "messages.source";
    /// Recherche plein texte.
    pub const SEARCH_QUERY: &str = "search.query";
    /// L'état du store.
    pub const STORE_STATS: &str = "store.stats";
    /// La révision courante du store.
    pub const STORE_REVISION: &str = "store.revision";
    /// Attend qu'une révision change. Servie par le transport, voir le module.
    pub const STORE_WAIT: &str = "store.wait";
    /// Les profils importables déclarés par l'opérateur. Servie par le transport.
    pub const JOBS_SOURCES: &str = "jobs.sources";
    /// Met une tâche de fond en file. Servie par le transport.
    pub const JOBS_START: &str = "jobs.start";
    /// Les tâches de fond connues. Servie par le transport.
    pub const JOBS_LIST: &str = "jobs.list";
    /// Une tâche de fond par son identifiant. Servie par le transport.
    pub const JOBS_GET: &str = "jobs.get";
    /// Demande l'arrêt d'une tâche de fond. Servie par le transport.
    pub const JOBS_CANCEL: &str = "jobs.cancel";

    /// Met un message dans la file d'envoi. Servie par le transport.
    pub const OUTBOX_SEND: &str = "outbox.send";
    /// La file d'envoi, états compris. Servie par le transport.
    pub const OUTBOX_LIST: &str = "outbox.list";
    /// Tranche le doute sur un message. **Décision de l'utilisateur**, servie par le transport.
    pub const OUTBOX_DECIDE: &str = "outbox.decide";
    /// Remet en file un envoi **échoué**. **Décision de l'utilisateur**, servie par le transport.
    ///
    /// ## Pourquoi ce n'est pas une troisième valeur de `outbox.decide`
    ///
    /// Les deux méthodes sortent d'états différents, et la différence est celle qui porte le
    /// risque du projet. `outbox.decide` sort de `committing`, où le serveur a **peut-être** le
    /// message : c'est un pari, et il demande d'avoir vérifié chez le fournisseur.
    /// `outbox.retry` sort de `failed`, où le serveur a refusé, donc n'a rien pris, donc un
    /// renvoi ne peut pas faire de doublon.
    ///
    /// Les réunir sous un seul nom demanderait à chaque client de savoir laquelle des deux
    /// situations il regarde pour choisir l'étiquette — et un client qui se tromperait
    /// enverrait deux fois. Deux méthodes, et le store refuse l'état qui n'est pas le sien.
    pub const OUTBOX_RETRY: &str = "outbox.retry";
    /// Les octets RFC 5322 **que nous avons composés**, rendus lisibles.
    ///
    /// ## Pourquoi elle est servie par le répartiteur, contrairement à ses voisines
    ///
    /// Les autres `outbox.*` ont besoin du trousseau et du facteur. Celle-ci ne fait que lire
    /// un blob que la boîte mail contient déjà, et la règle de [`SERVED_BY_TRANSPORT`] est
    /// écrite en termes de ce qu'une boîte mail ne contient pas — pas en termes de préfixe.
    ///
    /// ## Pourquoi elle existe, alors que [`MESSAGES_SOURCE`] existe
    ///
    /// Un message envoyé finit par revenir du dossier « Envoyés », mais après une moisson, et
    /// jamais s'il a été refusé. Or c'est justement le message sortant — le seul que nous
    /// composons — sur lequel `docs/PHASE-3.md` promet que rien n'est ajouté en secret. Cette
    /// méthode lit le blob remis au `DATA`, donc les octets eux-mêmes et non une
    /// reconstruction : une vue qui recomposerait ne prouverait rien.
    pub const OUTBOX_SOURCE: &str = "outbox.source";
    /// Les comptes, et lesquels peuvent envoyer. Servie par le transport.
    pub const ACCOUNTS_LIST: &str = "accounts.list";
    /// La signature d'un compte. Servie par le transport.
    pub const ACCOUNTS_SIGNATURE: &str = "accounts.signature";
    /// Écrit la signature d'un compte. **Décision de l'utilisateur**, servie par le transport.
    pub const ACCOUNTS_SET_SIGNATURE: &str = "accounts.set_signature";
    /// Complète un début de destinataire, depuis le carnet dérivé du corpus.
    pub const CONTACTS_COMPLETE: &str = "contacts.complete";
    /// Les brouillons, du plus récemment touché au plus ancien. Servie par le transport.
    pub const DRAFTS_LIST: &str = "drafts.list";
    /// Enregistre un brouillon, ou met à jour celui dont l'identifiant est donné. Servie par le
    /// transport.
    pub const DRAFTS_SAVE: &str = "drafts.save";
    /// Jette un brouillon. **Décision de l'utilisateur**, servie par le transport.
    pub const DRAFTS_DELETE: &str = "drafts.delete";

    /// Toutes les méthodes, dans l'ordre où un client les découvre.
    pub const ALL: &[&str] = &[
        SERVER_HELLO,
        SERVER_METHODS,
        FOLDERS_LIST,
        MESSAGES_PAGE,
        MESSAGES_GET,
        MESSAGES_THREAD,
        MESSAGES_MARK_READ,
        MESSAGES_STAGE_PART,
        MESSAGES_SOURCE,
        SEARCH_QUERY,
        STORE_STATS,
        STORE_REVISION,
        STORE_WAIT,
        JOBS_SOURCES,
        JOBS_START,
        JOBS_LIST,
        JOBS_GET,
        JOBS_CANCEL,
        ACCOUNTS_LIST,
        ACCOUNTS_SIGNATURE,
        ACCOUNTS_SET_SIGNATURE,
        CONTACTS_COMPLETE,
        OUTBOX_SEND,
        OUTBOX_LIST,
        OUTBOX_DECIDE,
        OUTBOX_RETRY,
        OUTBOX_SOURCE,
        DRAFTS_LIST,
        DRAFTS_SAVE,
        DRAFTS_DELETE,
    ];

    /// Les méthodes que le répartiteur ne sert pas lui-même.
    ///
    /// Elles ont toutes besoin de quelque chose qu'une boîte mail ne contient pas : le temps
    /// qui passe pour `store.wait`, le registre des tâches de fond pour les autres. `maild`
    /// les intercepte avant [`super::call`].
    pub const SERVED_BY_TRANSPORT: &[&str] = &[
        STORE_WAIT,
        JOBS_SOURCES,
        JOBS_START,
        JOBS_LIST,
        JOBS_GET,
        JOBS_CANCEL,
        // L'envoi a besoin du trousseau du système et du facteur, dont une boîte mail ne sait
        // rien. `accounts.list` avec, parce qu'elle sert à savoir **qui peut envoyer** : le
        // répartiteur ne lit que la vue de lecture des comptes.
        ACCOUNTS_LIST,
        OUTBOX_SEND,
        OUTBOX_LIST,
        OUTBOX_DECIDE,
        OUTBOX_RETRY,
        // Marquer lu écrit dans le store **et** met une poussée en attente pour le serveur :
        // ce n'est pas une lecture, donc pas le répartiteur.
        MESSAGES_MARK_READ,
        MESSAGES_STAGE_PART,
        // La signature se lit et s'écrit sur `accounts`, et l'écriture est un `UPDATE` : le
        // répartiteur ne sert que des lectures. Les deux sont ensemble parce qu'un client qui
        // sait lire une signature doit savoir l'enregistrer, sinon l'éditeur n'a nulle part
        // où rendre la main.
        ACCOUNTS_SIGNATURE,
        ACCOUNTS_SET_SIGNATURE,
        // Les brouillons s'écrivent et se suppriment : le répartiteur ne sert que des
        // lectures. Les trois sont ensemble parce qu'un client qui liste des brouillons doit
        // pouvoir en enregistrer et en jeter — un brouillon qu'on ne peut pas jeter reste dans
        // la liste pour toujours.
        DRAFTS_LIST,
        DRAFTS_SAVE,
        DRAFTS_DELETE,
    ];
}

/// Nombre de lignes rendues par défaut par `messages.page`.
///
/// 100 : une page plus haute qu'un écran, pour que le défilement ait de l'avance sans que
/// la première réponse traîne. Le plafond de `mailcore` fait le reste.
pub const DEFAULT_PAGE: u32 = 100;

/// Nombre de résultats de recherche rendus par défaut.
pub const DEFAULT_RESULTS: usize = 50;

/// Nombre de propositions de complétion rendues par défaut.
///
/// Huit : une liste plus longue ne se lit pas d'un coup d'œil sous un champ de saisie, et c'est
/// aussi le nombre sur lequel le critère 4 est mesuré.
pub const DEFAULT_SUGGESTIONS: usize = 8;

/// Plafond des propositions.
///
/// Un client qui en demande mille ne veut pas les lire ; il veut le carnet. Le carnet entier est
/// une donnée personnelle dense — qui l'utilisateur connaît, et combien il leur écrit — et la
/// servir en un appel n'a aucun usage d'interface.
pub const MAX_SUGGESTIONS: usize = 50;

/// Paramètres de `messages.page`.
#[derive(Debug, Deserialize)]
pub struct PageParams {
    /// L'identifiant du dossier, tel que rendu par `folders.list`.
    pub folder: i64,
    /// Le curseur rendu par la page précédente. Absent pour la première page.
    #[serde(default)]
    pub after: Option<String>,
    /// Nombre de lignes voulues.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Paramètres d'une méthode qui prend un identifiant de message.
#[derive(Debug, Deserialize)]
pub struct MessageParams {
    /// L'identifiant.
    pub id: i64,
}

/// La forme de corps qu'un client veut.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    /// Texte aplati seulement. Le défaut, et le moins cher.
    #[default]
    Text,
    /// Texte **et** HTML assaini. Pour un client capable de confiner du balisage.
    Html,
}

/// Paramètres de `messages.get`.
#[derive(Debug, Deserialize)]
pub struct GetParams {
    /// L'identifiant rendu par `messages.page` ou `search.query`.
    pub id: i64,
    /// La forme de corps voulue. `text` par défaut.
    #[serde(default)]
    pub body: BodyFormat,
    /// Laisser les images distantes dans le HTML.
    ///
    /// Faux par défaut. Vrai correspond à « Afficher les images » sur **ce** message : le
    /// démon ne le mémorise pas, le client le redemande à chaque rendu qu'il veut débloqué
    /// (`docs/PRIVACY.md`, §2 — le déblocage ne persiste pas).
    #[serde(default)]
    pub remote_images: bool,
}

/// Paramètres de `search.query`.
#[derive(Debug, Deserialize)]
pub struct SearchParams {
    /// La requête. Même syntaxe que côté MCP : mot, `"phrase exacte"`, `from:`, `subject:`,
    /// `AND` / `OR` / `-mot`.
    pub query: String,
    /// Nombre maximum de résultats.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Paramètres de `store.wait`, servis par le transport.
#[derive(Debug, Deserialize)]
pub struct WaitParams {
    /// La révision que le client a en main.
    pub revision: String,
    /// Délai maximum d'attente, en millisecondes.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Paramètres de `contacts.complete`.
#[derive(Debug, Deserialize)]
pub struct CompleteParams {
    /// Le début de saisie. Vide pour les mieux classées.
    #[serde(default)]
    pub prefix: String,
    /// Nombre maximum de propositions.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Paramètres de `messages.mark_read`, servis par le transport.
///
/// ## Le dossier fait partie de la demande
///
/// Un même contenu vit dans plusieurs dossiers, avec des drapeaux **par dossier** — c'est la
/// raison d'être de `refs`. Marquer « le message » sans dire lequel des dossiers demanderait de
/// choisir à la place de l'utilisateur, qui a ouvert le message dans un dossier précis.
#[derive(Debug, Deserialize)]
pub struct MarkReadParams {
    /// L'identifiant rendu par `messages.page`.
    pub id: i64,
    /// Le dossier dans lequel le message a été ouvert.
    pub folder: i64,
}

/// Paramètres de `outbox.decide`, servis par le transport.
#[derive(Debug, Deserialize)]
pub struct DecideParams {
    /// La ligne de file, telle que `outbox.list` la rend.
    pub id: i64,
    /// `resend` — le destinataire ne l'a pas reçu. `accept` — il l'a reçu.
    ///
    /// Pas de défaut : les deux sont irréversibles dans un sens différent, et un repli
    /// choisirait à la place de l'utilisateur.
    pub decision: String,
}

/// Paramètres de `outbox.retry`, servis par le transport.
#[derive(Debug, Deserialize)]
pub struct RetryParams {
    /// La ligne de file **échouée**, telle que `outbox.list` la rend.
    ///
    /// Il n'y a pas de champ « quoi renvoyer » : c'est le même message, les mêmes octets, le
    /// même blob. Un renvoi qui changerait le message serait une nouvelle rédaction, et elle
    /// passe par `outbox.send`.
    pub id: i64,
}

/// Paramètres de `outbox.send`, servis par le transport.
///
/// ## Il n'y a pas de champ `from`
///
/// L'expéditeur est l'identifiant du compte, et il n'est pas négociable : envoyer sous une
/// autre adresse se fait refuser par tous les serveurs de soumission du corpus, et un champ
/// ferait croire le contraire. C'est aussi ce qui empêche un client qui détient le jeton
/// d'usurper une adresse.
///
/// ## Il n'y a pas de champ `headers`
///
/// Un client ne choisit pas les en-têtes. La liste que le démon écrit est courte et fixe — voir
/// `mailsmtp::compose::Draft` — et c'est ce qui rend vraie la promesse de `docs/PHASE-3.md` :
/// tout ce que le message porte est lisible par l'utilisateur. Un `headers` libre serait un
/// `X-Mailer`, puis un identifiant de suivi, puis un pixel.
#[derive(Debug, Deserialize)]
pub struct SendParams {
    /// Le compte qui envoie, tel que rendu par `accounts.list`.
    pub account: i64,
    /// Les destinataires visibles.
    #[serde(default)]
    pub to: Vec<String>,
    /// En copie, visibles.
    #[serde(default)]
    pub cc: Vec<String>,
    /// En copie **cachée** : dans l'enveloppe SMTP, dans aucun en-tête.
    #[serde(default)]
    pub bcc: Vec<String>,
    /// Le sujet, tel que l'utilisateur l'a tapé. Encodé par le démon s'il n'est pas ASCII.
    #[serde(default)]
    pub subject: String,
    /// Le corps en texte brut. **Toujours envoyé**, même quand il y a du HTML.
    pub text: String,
    /// Le corps en HTML, s'il y en a un. Le démon en fait un `multipart/alternative`.
    #[serde(default)]
    pub html: Option<String>,
    /// Le `Message-ID` auquel ce message répond.
    #[serde(default)]
    pub in_reply_to: Option<String>,
    /// La chaîne des `Message-ID` du fil, du plus ancien au plus récent.
    #[serde(default)]
    pub references: Vec<String>,
    /// Les pièces jointes, désignées par leur **contenu**.
    ///
    /// Un client ne peut référencer que du contenu déjà dans le magasin de blobs : voir
    /// [`dto::Attached`]. Il n'y a pas de champ pour un chemin, et c'est ce qui empêche
    /// quiconque détient le jeton de faire lire un fichier au démon.
    #[serde(default)]
    pub attachments: Vec<crate::dto::Attached>,
    /// Ajouter la signature du compte au corps.
    ///
    /// ## Un drapeau, et pas un ajout d'office
    ///
    /// Le démon sait où est la signature, donc il pourrait toujours l'ajouter. Il ne le fait pas,
    /// pour une raison de correction : un client qui l'aurait déjà mise dans `text` la verrait
    /// **doublée**, et le destinataire aussi. Le drapeau dit qui compose.
    ///
    /// Faux par défaut, ce qui est le choix conservateur : un client écrit avant ce champ
    /// continue d'envoyer exactement ce qu'il a composé.
    ///
    /// L'assemblage lui-même est `mailsmtp::compose::Draft::sign_with`, la même fonction que
    /// `mail send --signature` : deux implémentations de « comment une signature rejoint un
    /// message » finiraient par en doubler une.
    #[serde(default)]
    pub signature: bool,
}

/// Paramètres de `messages.stage_part`, servis par le transport.
///
/// ## Un rang dans une liste, jamais un chemin
///
/// Le client nomme un message qu'il peut **déjà lire** — il tient son identifiant de
/// `messages.page` ou de `search.query` — et le rang d'une pièce dans la liste que
/// `messages.get` lui a rendue. Il n'ouvre donc aucune capacité nouvelle : ce qui sort du
/// magasin est ce qui y était déjà.
///
/// C'est le même principe que le rang de profil de `jobs.start`, et pour la même raison : un
/// champ de chemin donnerait la lecture de n'importe quel fichier à quiconque détient le jeton.
#[derive(Debug, Deserialize)]
pub struct StagePartParams {
    /// Le message, tel que rendu par `messages.page` ou `search.query`.
    pub id: i64,
    /// Le rang de la pièce jointe **dans la liste rendue par `messages.get`**.
    pub part: usize,
}

/// Paramètres de `drafts.delete`, servis par le transport.
#[derive(Debug, Deserialize)]
pub struct DraftParams {
    /// L'identifiant rendu par `drafts.list` ou `drafts.save`.
    pub id: i64,
}

/// Paramètres de `accounts.signature`, servis par le transport.
#[derive(Debug, Deserialize)]
pub struct SignatureParams {
    /// Le compte, tel que rendu par `accounts.list`.
    pub account: i64,
}

/// Paramètres de `accounts.set_signature`, servis par le transport.
///
/// ## Le document passe tel quel, et il n'a pas de DTO à lui
///
/// `mailhtml::rich::Document` est déjà la forme rangée dans le store — voir la migration
/// `SCHEMA_V8`. Un DTO parallèle serait une deuxième définition du même format, à faire
/// coïncider avec la première ; et c'est celle qui traverse le réseau qui divergerait en
/// silence.
///
/// Ce qui arrive par là n'est pas cru pour autant : les invariants du document sont rétablis à
/// la relecture, et la sortie HTML écarte ce qui trancherait un caractère ou porterait un
/// `javascript:`. Un client qui détient le jeton ne peut donc pas faire écrire à l'utilisateur
/// une signature piégée.
#[derive(Debug, Deserialize)]
pub struct SetSignatureParams {
    /// Le compte visé.
    pub account: i64,
    /// La signature. Absente ou vide, elle **efface** celle du compte.
    #[serde(default)]
    pub signature: Option<mailhtml::rich::Document>,
}

/// Paramètres de `jobs.start`, servis par le transport.
#[derive(Debug, Deserialize)]
pub struct StartJobParams {
    /// `import`, `index` ou `thread`.
    pub kind: String,
    /// Le rang du profil à importer dans la liste rendue par `jobs.sources`.
    ///
    /// **Un rang, jamais un chemin.** Les profils importables sont déclarés par l'opérateur
    /// au démarrage du démon ; laisser un client nommer un chemin donnerait à quiconque
    /// détient le jeton la lecture de n'importe quel fichier de la machine du démon.
    #[serde(default)]
    pub source: Option<usize>,
    /// Tout lire et tout compter sans rien écrire. Pour un import seulement.
    #[serde(default)]
    pub dry_run: bool,
    /// Importer aussi les comptes de flux RSS. Pour un import seulement.
    ///
    /// Faux par défaut : les articles d'un agrégateur ne sont pas du courrier, et sur un
    /// profil réel ils peuvent représenter une part importante du volume. L'exclusion est
    /// comptée dans le bilan de la tâche, jamais silencieuse.
    #[serde(default)]
    pub include_feeds: bool,
    /// Importer aussi les répertoires de comptes que `prefs.js` ne déclare pas.
    ///
    /// Faux par défaut : Thunderbird ne supprime pas les mbox d'un compte retiré de sa
    /// configuration, et ces restes ne sont pas du courrier vivant. L'exclusion est comptée
    /// dans le bilan de la tâche.
    #[serde(default)]
    pub include_orphans: bool,
    /// Ne synchroniser que ce compte. Pour une tâche `sync` seulement.
    ///
    /// **Un identifiant, et il est déjà connu du client** : `folders.list` le porte pour
    /// chaque dossier. Ce paramètre n'ouvre donc aucune capacité nouvelle, contrairement au
    /// chemin d'un profil que l'import refuse au profit d'un rang.
    ///
    /// Absent, tous les comptes actifs sont synchronisés.
    #[serde(default)]
    pub account: Option<i64>,
}

/// Paramètres des méthodes qui désignent une tâche de fond.
#[derive(Debug, Deserialize)]
pub struct JobParams {
    /// L'identifiant rendu par `jobs.start`.
    pub id: u64,
}

/// Exécute une méthode.
///
/// # Errors
///
/// Une [`jsonrpc::Error`] prête à partir : méthode inconnue, paramètres invalides, ou échec
/// interne — jamais le message d'erreur brut de `mailcore`, voir [`opaque`].
pub fn call(mailbox: &Mailbox, method: &str, params: Value) -> Result<Value, Error> {
    match method {
        method::SERVER_HELLO => {
            let stats = mailbox.stats().map_err(opaque)?;
            value(&dto::Hello {
                server: "mailcore".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                protocol: crate::PROTOCOL,
                search_available: mailbox.search_available(),
                revision: mailbox.revision().map_err(opaque)?,
                messages: stats.messages,
            })
        }

        method::SERVER_METHODS => Ok(json!(method::ALL)),

        method::FOLDERS_LIST => {
            let folders = mailbox.folders().map_err(opaque)?;
            let folders: Vec<dto::Folder> = folders.into_iter().map(Into::into).collect();
            value(&folders)
        }

        method::MESSAGES_PAGE => {
            let params: PageParams = decode(params)?;
            let after = params.after.as_deref().map(decode_cursor).transpose()?;
            let limit = params.limit.unwrap_or(DEFAULT_PAGE);
            let items = mailbox
                .page(FolderId(params.folder), after, limit)
                .map_err(opaque)?;

            // Le curseur ne sort que si la page est pleine. Une page incomplète est la
            // dernière, et rendre un curseur ferait faire au client un aller-retour de plus
            // pour apprendre qu'il n'y a rien après.
            let next = (items.len() as u32 >= limit)
                .then(|| items.last().map(|item| encode_cursor(item.date, item.id.0)))
                .flatten();

            value(&dto::Page {
                rows: items.iter().map(|item| dto::Row::new(item, None)).collect(),
                next,
                revision: mailbox.revision().map_err(opaque)?,
            })
        }

        method::MESSAGES_GET => {
            let params: GetParams = decode(params)?;
            let id = MessageId(params.id);
            let Some(detail) = mailbox.message(id).map_err(opaque)? else {
                return value(&Option::<dto::Message>::None);
            };

            let mut converted = message(detail);
            if params.body == BodyFormat::Html {
                let policy = mailhtml::Policy {
                    allow_remote_images: params.remote_images,
                };
                converted.html = mailbox.render(id, policy).map_err(opaque)?.map(html);
            }
            value(&Some(converted))
        }

        method::MESSAGES_SOURCE => {
            let params: MessageParams = decode(params)?;
            let found = mailbox.source(MessageId(params.id)).map_err(opaque)?;
            value(&found.map(dto::MessageSource::new))
        }

        method::OUTBOX_SOURCE => {
            let params: MessageParams = decode(params)?;
            let found = mailbox
                .outgoing_source(mailcore::OutboxId(params.id))
                .map_err(opaque)?;
            value(&found.map(dto::MessageSource::new))
        }

        method::MESSAGES_THREAD => {
            let params: MessageParams = decode(params)?;
            let items = mailbox.thread(MessageId(params.id)).map_err(opaque)?;
            let rows: Vec<dto::Row> = items.iter().map(|item| dto::Row::new(item, None)).collect();
            value(&dto::Thread {
                count: rows.len(),
                rows,
            })
        }

        method::SEARCH_QUERY => {
            let params: SearchParams = decode(params)?;
            let limit = params.limit.unwrap_or(DEFAULT_RESULTS);
            // Une requête mal formée est une erreur de l'utilisateur, pas une panne : elle
            // sort en `-32602` avec de quoi corriger, et pas en `-32603`.
            let found = match mailbox.search(&params.query, limit) {
                Ok(found) => found,
                Err(mailcore::Error::Query(source)) => {
                    return Err(Error::invalid_params(source));
                }
                Err(source) => return Err(opaque(source)),
            };
            let rows: Vec<dto::Row> = found
                .iter()
                .map(|result| dto::Row::new(&result.item, Some(result.score)))
                .collect();
            value(&dto::Results {
                truncated: rows.len() >= limit,
                count: rows.len(),
                search_available: mailbox.search_available(),
                rows,
            })
        }

        method::STORE_STATS => {
            let stats = mailbox.stats().map_err(opaque)?;
            value(&dto::Stats::new(
                &stats,
                mailbox.search_available(),
                mailbox.revision().map_err(opaque)?,
            ))
        }

        method::STORE_REVISION => value(&dto::Revision {
            revision: mailbox.revision().map_err(opaque)?,
        }),

        // Servie **ici** et non par le transport : c'est une lecture du store, et `Mailbox`
        // donne accès à celui-ci. La servir ici la rend disponible à `mailmcp` aussi, ce qui
        // compte : un modèle qui rédige un message a le même besoin qu'un humain.
        method::CONTACTS_COMPLETE => {
            let params: CompleteParams = decode(params)?;
            let limit = params
                .limit
                .unwrap_or(DEFAULT_SUGGESTIONS)
                .min(MAX_SUGGESTIONS);
            let found = mailbox
                .store()
                .complete(&params.prefix, limit)
                .map_err(opaque)?;
            let out: Vec<dto::Suggestion> = found.iter().map(dto::Suggestion::new).collect();
            value(&out)
        }

        // Servies par le transport, qui les intercepte avant d'arriver ici. Y répondre par
        // « méthode inconnue » serait un mensonge, et un client les croirait absentes.
        other if method::SERVED_BY_TRANSPORT.contains(&other) => Err(Error::new(
            jsonrpc::INTERNAL_ERROR,
            format!("{other} doit être servie par le transport"),
        )),

        unknown => Err(Error::method_not_found(unknown)),
    }
}

/// Sérialise une réponse.
fn value<T: serde::Serialize>(payload: &T) -> Result<Value, Error> {
    serde_json::to_value(payload).map_err(|source| {
        // Un DTO qui ne se sérialise pas est un bug chez nous, pas chez le client.
        tracing::error!(%source, "sérialisation d'une réponse impossible");
        Error::new(jsonrpc::INTERNAL_ERROR, "réponse non sérialisable")
    })
}

/// Décode des paramètres.
///
/// `null` est accepté et traité comme un objet vide : une méthode dont tous les champs ont
/// une valeur par défaut doit pouvoir être appelée sans `params`.
fn decode<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, Error> {
    let params = if params.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params
    };
    serde_json::from_value(params).map_err(Error::invalid_params)
}

/// Traduit une erreur du cœur en erreur de protocole, **sans recopier son message**.
///
/// `mailcore::Error::Io` affiche un chemin de fichier et `BlobNotFound` un hash de contenu.
/// Ni l'un ni l'autre n'a à traverser le réseau ni à finir dans le journal d'un client
/// (`docs/PRIVACY.md`, section 8). La cause réelle est journalisée ici, côté démon, où elle
/// sert au débogage sans être exposée.
fn opaque(source: mailcore::Error) -> Error {
    tracing::warn!(%source, "méthode de l'API en échec");
    let message = match source {
        mailcore::Error::Query(_) => "requête de recherche invalide",
        mailcore::Error::Sqlite(_) | mailcore::Error::Io { .. } => "le store est illisible",
        mailcore::Error::Tantivy(_) => "l'index plein texte est illisible",
        mailcore::Error::BlobNotFound(_) => "le contenu du message est introuvable",
        mailcore::Error::InvalidHash(_) | mailcore::Error::UnsupportedSchema { .. } => {
            "le store n'est pas exploitable par cette version"
        }
        // La configuration d'un compte est cassée, ou un serveur répond hors spécification.
        // Le message reste vague pour la même raison que les autres : il porterait un nom
        // d'hôte et un identifiant, et ni l'un ni l'autre n'a à traverser le réseau.
        mailcore::Error::UnknownAuth { .. }
        | mailcore::Error::UnknownSecurity { .. }
        | mailcore::Error::InconsistentAccount { .. } => {
            "la configuration d'un compte est invalide"
        }
        mailcore::Error::ModseqOutOfRange { .. } => "un serveur a répondu hors spécification",
        // Une ligne du store porte une valeur d'une forme impossible. Le message reste vague
        // pour la même raison, et la cause est déjà journalisée juste au-dessus — c'est là
        // qu'on la lira, pas chez le client.
        mailcore::Error::CorruptRow { .. } | mailcore::Error::UnknownSendState { .. } => {
            "une ligne du store n'est pas exploitable"
        }
        // Une valeur refusée par la mise en forme rangée, à l'écriture. Le sens inverse est
        // toléré par le store, donc ce chemin ne se rencontre pas en lecture.
        mailcore::Error::Unserialisable { .. } => "une valeur n'a pas pu être enregistrée",
    };
    Error::new(jsonrpc::INTERNAL_ERROR, message)
}

/// Encode un curseur de pagination.
///
/// La forme — `date:id` — est un détail d'implémentation que le client ne doit pas lire. Elle
/// est laissée en clair et non chiffrée : ce n'est pas un secret, juste une clé de tri, et un
/// curseur illisible se déboguerait mal.
fn encode_cursor(date: i64, id: i64) -> String {
    format!("{date}:{id}")
}

/// Décode un curseur, ou refuse.
///
/// Un curseur inventé par un client hostile ne fait que déplacer sa propre fenêtre de
/// lecture — la clause `WHERE` du cœur reste bornée au dossier demandé. Il est quand même
/// validé, parce qu'un `unwrap` sur un `parse` ici serait une panique déclenchée à distance.
fn decode_cursor(raw: &str) -> Result<(i64, MessageId), Error> {
    let (date, id) = raw
        .split_once(':')
        .ok_or_else(|| Error::invalid_params("curseur mal formé"))?;
    let date = date
        .parse::<i64>()
        .map_err(|_| Error::invalid_params("curseur mal formé"))?;
    let id = id
        .parse::<i64>()
        .map_err(|_| Error::invalid_params("curseur mal formé"))?;
    Ok((date, MessageId(id)))
}

/// Convertit un corps HTML rendu.
fn html(rendered: mailcore::Rendered) -> dto::Html {
    dto::Html {
        html: rendered.html,
        truncated: rendered.truncated,
        // La CSP et le `sandbox` viennent de `mailhtml`, source unique. Le front pose ce
        // qu'il reçoit ; il n'en garde pas de copie qui pourrait diverger.
        csp: mailhtml::MESSAGE_CSP.to_owned(),
        sandbox: mailhtml::MESSAGE_SANDBOX.to_owned(),
        blocked_images: rendered.blocked_images,
        remote_resources: rendered.remote_resources,
        trackers: rendered
            .trackers
            .into_iter()
            .map(|tracker| dto::Tracker {
                kind: match tracker.kind {
                    mailhtml::trackers::Kind::Pixel => dto::TrackerKind::Pixel,
                    mailhtml::trackers::Kind::KnownDomain => dto::TrackerKind::KnownDomain,
                    mailhtml::trackers::Kind::CorrelatedId => dto::TrackerKind::CorrelatedId,
                },
                host: tracker.host,
            })
            .collect(),
    }
}

/// Projette une invitation lue vers le contrat de fil.
///
/// ## Les heures sortent formatées, et les instants bruts
///
/// L'heure murale est mise en forme ici — `AAAA-MM-JJ HH:MM` — parce que c'est la seule forme
/// qui garde le sens de ce que l'organisateur a écrit : « 14:00 » dans son fuseau. L'instant,
/// lui, sort en secondes Unix, brut, pour que le client le formate selon sa locale. Les deux
/// sortent, et l'en-tête de [`dto::Invitation`] dit pourquoi il faut les deux.
fn invitation(read: mailcal::Invitation) -> dto::Invitation {
    let zone = read.start.as_ref().map(|start| match &start.zone {
        mailcal::Zone::Utc => "UTC".to_owned(),
        mailcal::Zone::AllDay => "journée entière".to_owned(),
        mailcal::Zone::Floating => "heure locale".to_owned(),
        mailcal::Zone::Named { id, .. } => id.clone(),
    });
    let all_day = read.start.as_ref().is_some_and(mailcal::Moment::is_all_day);
    let refused = read.gaps.iter().any(mailcal::Gap::is_refusal);

    dto::Invitation {
        kind: read.method.label().to_owned(),
        summary: read.summary,
        start_wall: read.start.as_ref().map(|it| wall(it, all_day)),
        start_unix: read.start.as_ref().and_then(|it| it.unix),
        end_wall: read.end.as_ref().map(|it| wall(it, all_day)),
        end_unix: read.end.as_ref().and_then(|it| it.unix),
        zone,
        all_day,
        location: read.location,
        organizer: read.organizer.as_ref().map(participant),
        attendees: read.attendees.iter().map(participant).collect(),
        status: read.status,
        recurrence: read.recurrence,
        urls: read.urls,
        // Les phrases, pas les variantes : c'est ce qu'une interface affiche, et un client qui
        // aurait à traduire un `enum` en français réimplémenterait `Gap::Display`.
        caveats: read.gaps.iter().map(ToString::to_string).collect(),
        refused,
        extra_events: read.extra_events,
    }
}

/// L'heure murale d'un moment, telle que l'organisateur l'a écrite.
///
/// Une journée entière n'a pas d'heure : lui en afficher une — « 00:00 » — laisserait croire à
/// un rendez-vous qui commence à minuit.
fn wall(moment: &mailcal::Moment, all_day: bool) -> String {
    let it = &moment.wall;
    if all_day || moment.is_all_day() {
        format!("{:04}-{:02}-{:02}", it.year, it.month, it.day)
    } else {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            it.year, it.month, it.day, it.hour, it.minute
        )
    }
}

/// Projette un organisateur ou un participant.
fn participant(person: &mailcal::Person) -> dto::Participant {
    dto::Participant {
        name: person.name.clone(),
        address: person.address.clone(),
        answer: person.answer.label().to_owned(),
        required: person.required,
    }
}
/// Convertit un message ouvert.
fn message(detail: mailcore::MessageDetail) -> dto::Message {
    dto::Message {
        row: dto::Row::new(&detail.item, None),
        message_id: detail.rfc822_id,
        references: detail.references,
        to: detail.to,
        body: detail.body,
        body_truncated: detail.body_truncated,
        // Rempli plus haut, seulement si le client a demandé le HTML.
        html: None,
        invitation: detail.invitation.map(invitation),
        attachments: detail
            .attachments
            .into_iter()
            .map(|attachment| dto::Attachment {
                name: attachment.name,
                mime: attachment.mime,
                size: attachment.size as u64,
            })
            .collect(),
        folders: detail
            .folders
            .into_iter()
            .map(|(path, flags)| dto::Location {
                path,
                unread: !flags.contains(mailcore::MessageFlags::SEEN),
            })
            .collect(),
        thread: detail.thread.map(|thread| thread.0),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use mailcore::{FolderKind, MessageFlags, NewMessage, Store};

    /// Un store avec un dossier et `count` messages, un par seconde décroissante.
    fn mailbox(count: usize) -> (tempfile::TempDir, Mailbox, i64) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();

        let folder = {
            let store = Store::open(&root).unwrap();
            let writer = store.writer().unwrap();
            let account = writer.upsert_account("imap", "compte").unwrap();
            let folder = writer
                .upsert_folder(account, "INBOX", FolderKind::Inbox)
                .unwrap();
            for index in 0..count {
                let date = 1_700_000_000 + index as i64;
                let (id, _) = writer
                    .insert_message(&NewMessage {
                        blob: mailcore::BlobHash::of(format!("message {index}").as_bytes()),
                        rfc822_id: None,
                        date,
                        from_addr: "a@b.c",
                        from_name: None,
                        subject: &format!("sujet {index}"),
                        size: 100,
                        has_attachments: false,
                    })
                    .unwrap();
                writer
                    .insert_ref(id, folder, date, MessageFlags::empty())
                    .unwrap();
            }
            writer.commit().unwrap();
            folder.0
        };

        let mailbox = Mailbox::open(&root).unwrap();
        (dir, mailbox, folder)
    }

    fn ok(mailbox: &Mailbox, method: &str, params: Value) -> Value {
        call(mailbox, method, params).unwrap()
    }

    #[test]
    fn hello_says_enough_to_open_a_client_in_one_round_trip() {
        let (_dir, mailbox, _) = mailbox(3);
        let hello: dto::Hello =
            serde_json::from_value(ok(&mailbox, method::SERVER_HELLO, Value::Null)).unwrap();
        assert_eq!(hello.server, "mailcore");
        assert_eq!(hello.protocol, crate::PROTOCOL);
        assert_eq!(hello.messages, 3);
        assert!(!hello.revision.is_empty());
    }

    #[test]
    fn every_declared_method_is_reachable() {
        // La liste et le `match` ne doivent pas divergrer : une méthode annoncée et non
        // servie est un piège pour un client qui lit `server.methods`.
        let (_dir, mailbox, folder) = mailbox(1);
        let announced: Vec<String> =
            serde_json::from_value(ok(&mailbox, method::SERVER_METHODS, Value::Null)).unwrap();
        assert_eq!(announced.len(), method::ALL.len());

        for name in method::ALL {
            let params = match *name {
                method::MESSAGES_PAGE => json!({"folder": folder}),
                method::MESSAGES_GET
                | method::MESSAGES_THREAD
                | method::MESSAGES_SOURCE
                | method::OUTBOX_SOURCE => json!({"id": 1}),
                method::SEARCH_QUERY => json!({"query": "sujet"}),
                _ => Value::Null,
            };
            let outcome = call(&mailbox, name, params);

            if method::SERVED_BY_TRANSPORT.contains(name) {
                // Ces méthodes-là existent, mais pas ici. Elles doivent dire « pas à cet
                // endroit » et surtout **pas** « méthode inconnue » : un client qui lirait
                // ça les croirait absentes.
                let error = outcome.unwrap_err();
                assert_eq!(error.code, jsonrpc::INTERNAL_ERROR, "{name}");
                assert!(error.message.contains("transport"), "{name}");
            } else {
                assert!(outcome.is_ok(), "{name} a échoué");
            }
        }
    }

    /// Un store avec **un vrai blob** derrière son message : les autres tests n'en ont pas
    /// besoin, la source est la seule méthode dont c'est tout le sujet.
    fn mailbox_with(raw: &[u8]) -> (tempfile::TempDir, Mailbox) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        {
            let store = Store::open(&root).unwrap();
            let blob = store.blobs().put(raw).unwrap();
            let writer = store.writer().unwrap();
            let account = writer.upsert_account("imap", "compte").unwrap();
            let folder = writer
                .upsert_folder(account, "INBOX", FolderKind::Inbox)
                .unwrap();
            let (id, _) = writer
                .insert_message(&NewMessage {
                    blob: blob.hash,
                    rfc822_id: None,
                    date: 1_700_000_000,
                    from_addr: "a@b.c",
                    from_name: None,
                    subject: "sujet",
                    size: raw.len() as u64,
                    has_attachments: false,
                })
                .unwrap();
            writer
                .insert_ref(id, folder, 1_700_000_000, MessageFlags::empty())
                .unwrap();
            writer.commit().unwrap();
        }
        (dir, Mailbox::open(&root).unwrap())
    }

    #[test]
    fn the_source_of_a_message_is_the_bytes_and_not_the_reading_of_them() {
        // Ce que `messages.get` rend est décodé et aplati ; la source est l'autre chose, et
        // c'est la seule qui permette de vérifier ce que le message porte vraiment.
        let raw = b"From: a@b.c\r\nSubject: =?UTF-8?B?w6l0w6k=?=\r\n\r\ncorps\r\n";
        let (_dir, mailbox) = mailbox_with(raw);

        let source: dto::MessageSource =
            serde_json::from_value(ok(&mailbox, method::MESSAGES_SOURCE, json!({"id": 1})))
                .unwrap();
        assert!(source.headers.contains("=?UTF-8?B?w6l0w6k=?="));
        assert!(!source.headers.contains("été"));
        assert_eq!(source.body, "corps\n");
        assert_eq!(source.total, raw.len() as u64);
        assert!(!source.headers_truncated);
        assert!(!source.body_truncated);

        // Le contraste : la même méthode d'ouverture rend un corps sans son bloc d'en-têtes.
        // Les deux vues ne se remplacent pas, et c'est pour ça qu'il en faut deux.
        let read: dto::Message =
            serde_json::from_value(ok(&mailbox, method::MESSAGES_GET, json!({"id": 1}))).unwrap();
        assert!(!read.body.contains("From:"));
    }

    #[test]
    fn the_source_says_how_big_the_message_really_is_when_it_cuts() {
        // Le champ qui rend la troncature exploitable : sans lui, « tronqué » ne dit pas si on
        // voit tout sauf trois lignes ou tout sauf quarante mégaoctets.
        let mut raw = b"From: a@b.c\r\n\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'x', mailcore::source::MAX_BODY + 1));
        let (_dir, mailbox) = mailbox_with(&raw);

        let source: dto::MessageSource =
            serde_json::from_value(ok(&mailbox, method::MESSAGES_SOURCE, json!({"id": 1})))
                .unwrap();
        assert!(source.body_truncated);
        assert_eq!(source.body.len(), mailcore::source::MAX_BODY);
        assert_eq!(source.total, raw.len() as u64);
    }

    #[test]
    fn a_message_bigger_than_the_read_bound_still_knows_its_real_size() {
        // Le message dépasse ce que la lecture prend, donc `total` ne peut pas venir du
        // comptage des octets lus : il vient de l'index. Sans ça, « tronqué » ne dirait pas si
        // on voit tout sauf trois lignes ou tout sauf quarante mégaoctets.
        let mut raw = b"From: a@b.c\r\n\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'x', mailcore::source::MAX_READ + 1000));
        let (_dir, mailbox) = mailbox_with(&raw);

        let source: dto::MessageSource =
            serde_json::from_value(ok(&mailbox, method::MESSAGES_SOURCE, json!({"id": 1})))
                .unwrap();
        assert_eq!(source.total, raw.len() as u64);
        assert!(source.body_truncated);
        assert_eq!(source.body.len(), mailcore::source::MAX_BODY);
        // Les en-têtes, eux, tiennent : la borne de lecture est la somme des deux plafonds,
        // donc un bloc d'en-têtes ordinaire n'est jamais la victime d'un corps énorme.
        assert!(!source.headers_truncated);
        assert_eq!(source.headers, "From: a@b.c\n");
    }

    #[test]
    fn a_message_that_is_not_there_is_null_and_not_an_error() {
        // Même contrat que `messages.get` : un identifiant périmé est une situation normale
        // pour un client dont la page a vieilli, pas une panne.
        let (_dir, mailbox) = mailbox_with(b"From: a@b.c\r\n\r\n");
        assert!(ok(&mailbox, method::MESSAGES_SOURCE, json!({"id": 999})).is_null());
        assert!(ok(&mailbox, method::OUTBOX_SOURCE, json!({"id": 999})).is_null());
    }

    #[test]
    fn a_source_never_carries_a_control_character_to_its_client() {
        // La barrière est dans `mailcore`, et ce test vérifie qu'elle est bien **sur le
        // chemin** de l'API : un échappement fait par chaque client serait trois occasions
        // d'oublier, et le JSON transporterait l'octet jusqu'au terminal.
        let (_dir, mailbox) = mailbox_with(b"X-Evil: \x1b[2J\r\n\r\ncorps\r\n");
        let source: dto::MessageSource =
            serde_json::from_value(ok(&mailbox, method::MESSAGES_SOURCE, json!({"id": 1})))
                .unwrap();
        assert!(!source.headers.contains('\u{1b}'));
        assert_eq!(source.escaped_controls, 1);
    }

    #[test]
    fn no_method_is_both_dispatched_and_delegated() {
        // Une méthode dans les deux listes serait servie ou refusée selon l'ordre du `match`,
        // ce qui est exactement le genre d'ambiguïté qu'on ne veut pas dans un contrat.
        for name in method::SERVED_BY_TRANSPORT {
            assert!(
                method::ALL.contains(name),
                "{name} est déléguée au transport mais pas annoncée"
            );
        }
    }

    #[test]
    fn an_unknown_method_is_not_found() {
        let (_dir, mailbox, _) = mailbox(1);
        assert_eq!(
            call(&mailbox, "folders.delete", Value::Null)
                .unwrap_err()
                .code,
            jsonrpc::METHOD_NOT_FOUND
        );
    }

    #[test]
    fn paging_walks_the_whole_folder_without_repeating_a_row() {
        let (_dir, mailbox, folder) = mailbox(25);
        let mut seen = Vec::new();
        let mut after: Option<String> = None;

        loop {
            let mut params = json!({"folder": folder, "limit": 10});
            if let Some(cursor) = &after {
                params["after"] = json!(cursor);
            }
            let page: dto::Page =
                serde_json::from_value(ok(&mailbox, method::MESSAGES_PAGE, params)).unwrap();
            seen.extend(page.rows.iter().map(|row| row.id));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }

        assert_eq!(seen.len(), 25);
        let unique: std::collections::HashSet<i64> = seen.iter().copied().collect();
        assert_eq!(unique.len(), 25, "une ligne est sortie deux fois");
    }

    #[test]
    fn the_last_page_carries_no_cursor() {
        let (_dir, mailbox, folder) = mailbox(5);
        let page: dto::Page = serde_json::from_value(ok(
            &mailbox,
            method::MESSAGES_PAGE,
            json!({"folder": folder, "limit": 10}),
        ))
        .unwrap();
        assert_eq!(page.rows.len(), 5);
        assert!(page.next.is_none(), "un aller-retour pour rien");
    }

    #[test]
    fn a_forged_cursor_is_refused_and_never_panics() {
        let (_dir, mailbox, folder) = mailbox(3);
        for forged in ["", "abc", "1:", ":1", "9999999999999999999999:1", "1:2:3"] {
            let outcome = call(
                &mailbox,
                method::MESSAGES_PAGE,
                json!({"folder": folder, "after": forged}),
            );
            assert_eq!(
                outcome.unwrap_err().code,
                jsonrpc::INVALID_PARAMS,
                "curseur {forged:?} accepté"
            );
        }
    }

    #[test]
    fn missing_params_are_a_client_error_not_a_crash() {
        let (_dir, mailbox, _) = mailbox(1);
        assert_eq!(
            call(&mailbox, method::MESSAGES_PAGE, Value::Null)
                .unwrap_err()
                .code,
            jsonrpc::INVALID_PARAMS
        );
        assert_eq!(
            call(&mailbox, method::MESSAGES_GET, json!({"id": "sept"}))
                .unwrap_err()
                .code,
            jsonrpc::INVALID_PARAMS
        );
    }

    /// Le message HTML piégé de référence : un script, un pixel de traceur connu portant
    /// l'adresse du destinataire, et une image distante ordinaire.
    const TRAPPED: &[u8] = b"From: pisteur@exemple.fr\r\n\
        Subject: piege\r\n\
        Content-Type: text/html; charset=utf-8\r\n\
        \r\n\
        <p>Bonjour</p>\
        <script>alert(1)</script>\
        <img src=\"https://click.list-manage.com/o.gif?email=marie%40x.fr\" width=1 height=1>\
        <img src=\"https://exemple.fr/logo.png\" width=200 height=60>\r\n";

    /// Le même message, en texte seul.
    const TEXT_ONLY: &[u8] = b"From: ami@exemple.fr\r\n\
        Subject: sans balisage\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        \r\n\
        Juste du texte.\r\n";

    /// Un store contenant un seul message, blob compris.
    fn store_with(raw: &[u8]) -> (tempfile::TempDir, Mailbox, i64) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();

        let id = {
            let store = Store::open(&root).unwrap();
            let blob = store.blobs().put(raw).unwrap().hash;
            let writer = store.writer().unwrap();
            let account = writer.upsert_account("imap", "compte").unwrap();
            let folder = writer
                .upsert_folder(account, "INBOX", FolderKind::Inbox)
                .unwrap();
            let (id, _) = writer
                .insert_message(&NewMessage {
                    blob,
                    rfc822_id: None,
                    date: 1_700_000_000,
                    from_addr: "pisteur@exemple.fr",
                    from_name: None,
                    subject: "piege",
                    size: raw.len() as u64,
                    has_attachments: false,
                })
                .unwrap();
            writer
                .insert_ref(id, folder, 1_700_000_000, MessageFlags::empty())
                .unwrap();
            writer.commit().unwrap();
            id.0
        };

        (dir, Mailbox::open(&root).unwrap(), id)
    }

    #[test]
    fn the_html_body_is_absent_unless_it_is_asked_for() {
        // Il coûte un assainissement complet, et un client qui ne sait pas confiner du
        // balisage n'a rien à en faire.
        let (_dir, mailbox, id) = store_with(TRAPPED);
        let message: dto::Message =
            serde_json::from_value(ok(&mailbox, method::MESSAGES_GET, json!({"id": id}))).unwrap();
        assert!(message.html.is_none());
        assert!(message.body.contains("Bonjour"));
    }

    #[test]
    fn the_html_body_arrives_sanitised_with_what_confines_it() {
        let (_dir, mailbox, id) = store_with(TRAPPED);
        let message: dto::Message = serde_json::from_value(ok(
            &mailbox,
            method::MESSAGES_GET,
            json!({"id": id, "body": "html"}),
        ))
        .unwrap();

        let html = message.html.expect("le corps HTML a été demandé");
        assert!(html.html.contains("Bonjour"));
        assert!(!html.html.contains("<script"), "{}", html.html);
        assert!(!html.html.contains("list-manage.com"), "{}", html.html);
        assert!(!html.html.contains("logo.png"), "{}", html.html);

        // La CSP voyage avec le HTML qu'elle protège : le front n'en garde pas de copie.
        assert!(html.csp.contains("connect-src 'none'"));
        assert!(!html.sandbox.contains("allow-scripts"));

        // De quoi remplir le bandeau de `docs/PRIVACY.md` §2.
        assert_eq!(html.blocked_images, 2);
        assert_eq!(html.remote_resources, 2);
        assert!(
            html.trackers
                .iter()
                .any(|t| t.kind == dto::TrackerKind::Pixel),
            "{:?}",
            html.trackers
        );
        // L'adresse du destinataire, que le traceur transportait, ne ressort pas.
        assert!(!format!("{:?}", html.trackers).contains("marie"));
    }

    #[test]
    fn unblocking_images_is_asked_for_each_time_and_never_remembered() {
        let (_dir, mailbox, id) = store_with(TRAPPED);
        let unblocked: dto::Message = serde_json::from_value(ok(
            &mailbox,
            method::MESSAGES_GET,
            json!({"id": id, "body": "html", "remote_images": true}),
        ))
        .unwrap();
        let html = unblocked.html.unwrap();
        assert!(html.html.contains("logo.png"));
        assert_eq!(html.blocked_images, 0);
        // Le message contenait bien deux ressources distantes : le compteur du bandeau ne
        // ment pas parce qu'on a débloqué.
        assert_eq!(html.remote_resources, 2);

        // L'appel suivant, sans le drapeau, rebloque : rien n'a été mémorisé.
        let again: dto::Message = serde_json::from_value(ok(
            &mailbox,
            method::MESSAGES_GET,
            json!({"id": id, "body": "html"}),
        ))
        .unwrap();
        assert_eq!(again.html.unwrap().blocked_images, 2);
    }

    #[test]
    fn a_text_only_message_still_renders_so_the_front_has_one_path() {
        // `mail-parser` convertit une partie `text/plain` en HTML échappé. On s'appuie
        // dessus : le front a un seul chemin de rendu au lieu de deux, et il n'y a pas de
        // deuxième façon d'afficher un corps qui pourrait diverger de la première.
        let (_dir, mailbox, id) = store_with(TEXT_ONLY);
        let message: dto::Message = serde_json::from_value(ok(
            &mailbox,
            method::MESSAGES_GET,
            json!({"id": id, "body": "html"}),
        ))
        .unwrap();

        let html = message.html.expect("le corps HTML a été demandé");
        assert!(html.html.contains("Juste du texte"), "{}", html.html);
        assert_eq!(html.blocked_images, 0);
        assert!(html.trackers.is_empty());
        assert!(message.body.contains("Juste du texte"));
    }

    #[test]
    fn markup_written_as_text_survives_instead_of_being_eaten() {
        // Le risque de s'appuyer sur la conversion de `mail-parser` : si elle n'échappait
        // pas, un `<script>` écrit en toutes lettres dans un mail en texte deviendrait du
        // balisage, puis serait supprimé par l'assainisseur — et l'utilisateur perdrait du
        // texte qu'on lui a envoyé. Mesuré : elle échappe.
        const LITTERAL: &[u8] = b"From: dev@exemple.fr\r\n\
            Subject: extrait de code\r\n\
            Content-Type: text/plain; charset=utf-8\r\n\
            \r\n\
            Ajoute <script>alert(1)</script> dans la page, et un < tout seul.\r\n";

        let (_dir, mailbox, id) = store_with(LITTERAL);
        let message: dto::Message = serde_json::from_value(ok(
            &mailbox,
            method::MESSAGES_GET,
            json!({"id": id, "body": "html"}),
        ))
        .unwrap();

        let html = message.html.unwrap().html;
        assert!(html.contains("&lt;script&gt;"), "texte perdu : {html}");
        assert!(html.contains("alert(1)"), "texte perdu : {html}");
        // Et il reste inerte : c'est du texte échappé, pas une balise.
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn an_absent_message_is_null_and_not_an_error() {
        // Un identifiant périmé — le client avait une page en cache — ne doit pas ressembler
        // à une panne du store.
        let (_dir, mailbox, _) = mailbox(1);
        let found = ok(&mailbox, method::MESSAGES_GET, json!({"id": 99_999}));
        assert!(found.is_null());
    }

    #[test]
    fn a_malformed_query_blames_the_client() {
        let (_dir, mailbox, _) = mailbox(1);
        // Sans index, la recherche rend une liste vide plutôt qu'une erreur : c'est le
        // contrat de `mailcore`, et le drapeau dit au client de ne pas mentir à l'utilisateur.
        let results: dto::Results = serde_json::from_value(ok(
            &mailbox,
            method::SEARCH_QUERY,
            json!({"query": "sujet"}),
        ))
        .unwrap();
        assert!(results.rows.is_empty() || results.search_available);
    }

    #[test]
    fn an_error_never_carries_a_path_or_a_hash() {
        let error = opaque(mailcore::Error::Io {
            path: camino::Utf8PathBuf::from("F:/dev/mailcore/secret/index.sqlite"),
            source: std::io::Error::other("nope"),
        });
        assert!(!error.message.contains("secret"));
        assert!(!error.message.contains('/'));

        let error = opaque(mailcore::Error::BlobNotFound(mailcore::BlobHash::of(b"x")));
        assert_eq!(error.message, "le contenu du message est introuvable");
    }

    #[test]
    fn the_revision_changes_when_another_connection_writes() {
        // C'est tout le mécanisme d'abonnement : si ça ne bouge pas, `store.wait` ne
        // réveille jamais personne.
        let (dir, mailbox, folder) = mailbox(1);
        let before: dto::Revision =
            serde_json::from_value(ok(&mailbox, method::STORE_REVISION, Value::Null)).unwrap();

        let root = camino::Utf8Path::from_path(dir.path()).unwrap();
        let writer_store = Store::open(root).unwrap();
        let writer = writer_store.writer().unwrap();
        let (id, _) = writer
            .insert_message(&NewMessage {
                blob: mailcore::BlobHash::of(b"ajoute"),
                rfc822_id: None,
                date: 1_800_000_000,
                from_addr: "z@b.c",
                from_name: None,
                subject: "ajoute",
                size: 10,
                has_attachments: false,
            })
            .unwrap();
        writer
            .insert_ref(id, FolderId(folder), 1_800_000_000, MessageFlags::empty())
            .unwrap();
        writer.commit().unwrap();

        let after: dto::Revision =
            serde_json::from_value(ok(&mailbox, method::STORE_REVISION, Value::Null)).unwrap();
        assert_ne!(before.revision, after.revision);
    }
}

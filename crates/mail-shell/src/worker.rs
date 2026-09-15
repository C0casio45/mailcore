//! Le pont entre l'interface et le service : **des messages typés, jamais d'attente**.
//!
//! ## La règle qu'il fait respecter
//!
//! `CLAUDE.md`, règle 3 : *l'UI ne bloque jamais sur le réseau ou sur un import*. En mode
//! embarqué il n'y a pas de réseau, mais il y a un verrou sur la boîte mail, des lectures de
//! blobs, une décompression zstd et un parse MIME — 4,31 ms pour ouvrir un message HTML,
//! mesuré. À 60 fps une image dure 16,7 ms : une lecture inline passerait la plupart du temps,
//! et ferait tomber une image de temps en temps. « De temps en temps » n'est pas un régime
//! acceptable pour un critère qui dit *constant*. En mode distant, il y a un réseau, et la
//! question ne se pose même pas.
//!
//! Donc : l'interface **demande** et continue de dessiner ce qu'elle a. Un fil répond quand il
//! a fini, et réveille l'interface.
//!
//! ## Aucune logique n'est réimplémentée
//!
//! Les deux dos parlent le **même contrat** : `mailapi`. En embarqué, le fil appelle
//! [`maild::api::jsonrpc::Api::handle_message`] — la fonction que le démon sert sur HTTP et sur
//! stdio, celle que la page de la coquille Tauri appelle par IPC. En distant, il appelle
//! `mailapi::client`, comme la CLI. Pagination par curseur, politique d'images distantes,
//! bornes de recherche, registre des tâches de fond : tout est déjà écrit et déjà testé.
//!
//! Le prix, en embarqué, est un aller-retour de sérialisation en mémoire — quelques dizaines de
//! microsecondes pour une page de cent lignes, trois ordres de grandeur sous la lecture SQLite
//! qu'elle transporte. En échange, les types de `mailapi::dto` sont relus par le même code Rust
//! qui les écrit : un champ renommé casse à la compilation.
//!
//! ## Deux dos, un seul contrat
//!
//! | Mode | Transport | Jeton |
//! |---|---|---|
//! | **embarqué** (défaut) | appel en fonction, dans le processus | aucun |
//! | **distant** (`--daemon`) | HTTP vers un démon déjà en place | trousseau du système |
//!
//! Le premier est le mode d'une application de bureau : rien n'écoute, donc il n'y a pas de
//! canal à authentifier. Le second est le **déploiement de référence** de
//! `docs/ARCHITECTURE.md` — démon sur une machine dédiée, client ailleurs — et c'est le seul
//! dans lequel le critère 9 a un sens, puisqu'il faut un service à couper.
//!
//! Le reste de la coquille ignore lequel est actif : [`Link::call`] est le seul endroit qui
//! connaisse la différence. C'est ce qui rend le choix réversible.
//!
//! Le jeton vient de `mailapi::token`, donc du trousseau du système (`docs/PRIVACY.md` §7), et
//! `mailapi::client` refuse de l'envoyer à une adresse non locale. Pour un démon au bout du
//! réseau, il faut un tunnel chiffré et viser `127.0.0.1` — c'est le déploiement que
//! `docs/ARCHITECTURE.md` recommande de toute façon.
//!
//! ## Deux fils, et c'est nécessaire
//!
//! `store.wait` ne répond pas avant que la révision du store change — c'est l'abonnement aux
//! changements. Le démon **sonde** le store toutes les 250 ms pour le savoir : un
//! `PRAGMA data_version` et un `stat`, donc peu, mais pas rien. Ce qui est vrai côté client,
//! c'est qu'un client au repos n'envoie rien. Sur un fil unique, cet appel bloquerait tout le
//! reste : trente secondes sans pouvoir ouvrir un message.
//!
//! L'abonnement a donc son propre fil, et il est **autonome** : l'interface ne le demande pas,
//! elle reçoit un changement quand quelque chose a bougé. C'est aussi lui qui découvre qu'un
//! démon distant est tombé, sans que l'utilisateur ait à cliquer.
//!
//! ## Le réveil de l'interface
//!
//! egui ne redessine que sur événement. Une réponse qui arrive dans un canal n'est pas un
//! événement : sans `request_repaint`, elle attendrait le prochain mouvement de souris. Les
//! fils gardent donc une poignée sur le contexte, posée une fois l'interface créée.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, OnceLock};

use mailapi::dto;
use serde_json::{Value, json};

/// Ce que l'interface sait demander.
///
/// Un cas par méthode utilisée, et pas de variante fourre-tout : le compilateur doit pouvoir
/// dire qu'une réponse est traitée.
#[derive(Debug, Clone)]
pub enum Request {
    /// Le premier écran, en **un seul aller-retour** : les dossiers, et la première page du
    /// dossier à ouvrir.
    ///
    /// ## Pourquoi cette demande existe
    ///
    /// En deux demandes séparées, la page ne peut partir qu'une fois la liste des dossiers
    /// absorbée par l'interface — donc à l'image suivante, et la page arrive à celle d'après.
    /// Mesuré : **130 ms entre la première image et la première ligne**, sur un écran à 32 Hz,
    /// pour 22 ms de travail réel. Quatre images d'attente pour rien.
    ///
    /// Le choix du dossier — la boîte de réception, à défaut le premier — est une politique de
    /// présentation, et elle est ici assumée : c'est le prix d'un premier écran qui arrive avec
    /// son contenu. C'est le même arbitrage que `server.hello`, qui répond en une fois à tout
    /// ce qu'un client doit savoir.
    Bootstrap { limit: u32 },
    /// Se présenter et vérifier la version du contrat.
    Hello,
    /// La liste des dossiers et leurs compteurs.
    Folders,
    /// Une page de lignes. `after` est le curseur opaque rendu par la page précédente.
    Page {
        folder: i64,
        after: Option<String>,
        limit: u32,
    },
    /// Ouvrir un message. `remote_images` n'est vrai que si l'utilisateur l'a demandé.
    Open { id: i64, remote_images: bool },
    /// Le fil de discussion d'un message.
    Thread { id: i64 },
    /// Une recherche plein texte.
    Search { query: String, limit: u32 },
    /// Les profils que l'opérateur autorise à importer.
    Sources,
    /// Mettre un import en file. Un rang dans `Sources`, jamais un chemin.
    StartImport { source: usize },
    /// Reconstruire l'index plein texte.
    StartIndex,
    /// Moissonner un compte. Une tâche de fond du service, comme l'import.
    StartSync { account: i64 },
    /// Les comptes, et lesquels peuvent envoyer.
    Accounts,
    /// Marquer un message lu dans un dossier.
    ///
    /// Séparé de `Open` exprès : **lire n'est pas marquer**. Le service sert aussi des
    /// modèles, et un modèle qui parcourt une boîte marquerait tout comme lu au passage.
    MarkRead { id: i64, folder: i64 },
    /// Trancher le doute sur un message de la file. `resend` ou `accept`.
    Decide { id: i64, decision: &'static str },
    /// Renvoyer un envoi **échoué**, tel quel.
    ///
    /// Distinct de `Decide` : un envoi échoué a été refusé, donc le serveur n'a rien pris,
    /// donc le renvoi ne peut pas faire de doublon. Voir `outbox.retry`.
    Retry { id: i64 },
    /// Ranger un fichier dans le magasin, pour le joindre.
    ///
    /// Le chemin ne quitte pas ce processus : voir `Link::stage`.
    Attach { path: camino::Utf8PathBuf },
    /// Compléter un début de destinataire.
    ///
    /// Une demande **par frappe**, ce qui n'est pas un abus : le critère 4 la mesure à moins
    /// de 16,7 ms, et le fil du service est sériel — une frappe en retard est jetée par le
    /// jeton, pas mise en file.
    Complete { prefix: String, token: u64 },
    /// La file d'envoi, états compris.
    Outbox,
    /// Mettre un message dans la file d'envoi.
    ///
    /// **Ne l'envoie pas** : le démon écrit la ligne et réveille son facteur. L'interface ne
    /// bloque donc jamais sur une poignée de main TLS — règle 3 du `CLAUDE.md` — et le message
    /// survit à la fermeture de la fenêtre.
    Send(Box<Compose>),
    /// La signature d'un compte.
    Signature { account: i64 },
    /// Enregistrer, ou effacer, la signature d'un compte.
    ///
    /// **Une écriture demandée par un clic**, comme `Send` : le document part tel qu'il est à
    /// l'écran, et le démon rend ce qu'il a rangé.
    SetSignature {
        account: i64,
        signature: Option<mailhtml::rich::Document>,
    },
    /// Ranger une pièce jointe d'un message reçu dans le magasin, pour la joindre ailleurs.
    ///
    /// **Un rang dans la liste que le service a rendue, jamais un chemin.** C'est ce qui
    /// permet de transférer un message avec ses pièces sans donner à personne la lecture d'un
    /// fichier arbitraire.
    StagePart { id: i64, part: usize },
    /// Les brouillons connus.
    Drafts,
    /// Enregistrer un brouillon, ou mettre à jour celui qu'il porte.
    ///
    /// **Rien n'est validé** : c'est ce qui était à l'écran, adresse à moitié tapée comprise.
    /// Un brouillon vide n'est pas enregistré, et efface celui qu'il remplaçait.
    SaveDraft(Box<Compose>),
    /// Jeter un brouillon. **Décision de l'utilisateur.**
    DeleteDraft { id: i64 },
    /// Les tâches de fond en cours et récentes.
    Jobs,
    /// La source d'un message : les octets tels qu'ils sont.
    ///
    /// Demandée **à l'ouverture de la fenêtre**, jamais avec le message : elle pèse la taille du
    /// message, et l'immense majorité des ouvertures ne la regarde pas.
    Source { of: SourceOf },
    /// Les comptes **avec leur serveur et leur trousseau**, pour la page de paramètres.
    ///
    /// Distincte de [`Request::Accounts`], qui passe par `accounts.list` et ne rend ni hôte ni
    /// port — critère 7 de `docs/PHASE-3.md`. Celle-ci lit le store directement, et n'existe
    /// qu'en mode embarqué. Voir `crate::settings`.
    AccountsDetail,
    /// Déclarer, ou redéclarer, un compte. **Décision de l'utilisateur**, et elle écrit un
    /// secret : servie localement, jamais par l'API.
    DeclareAccount(Box<crate::settings::AccountForm>),
    /// Écrire le serveur de soumission d'un compte. **Décision de l'utilisateur.**
    SetSubmission(Box<crate::settings::SubmissionForm>),
    /// Oublier le secret d'un compte et mettre sa synchronisation en pause.
    ForgetAccount { id: i64 },
    /// Dérouler un consentement OAuth2 et ranger les jetons.
    ///
    /// **La seule demande qui ne répond pas sur le fil du service.** Elle attend qu'un humain
    /// revienne de son navigateur — jusqu'à cinq minutes, `mailauth::consent::CONSENT_TIMEOUT`
    /// — et la servir en ligne mettrait toute la boîte mail en file d'attente derrière : plus
    /// de page, plus de message ouvert, plus rien, pendant ce temps. Elle part donc sur son
    /// propre fil. Voir `spawn_consent`.
    Consent(Box<crate::settings::ConsentForm>),
}

/// Ce dont on veut la source.
///
/// Deux espaces d'identifiants distincts : un message d'index et une ligne de file ne se
/// confondent pas, et un seul champ `id` ferait lire le mauvais message à qui se tromperait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOf {
    /// Un message reçu.
    Message(i64),
    /// Une ligne de la file d'envoi — **ce que nous avons composé**.
    Outgoing(i64),
}

/// Ce que la fenêtre de rédaction a rassemblé.
///
/// ## Les adresses sont des chaînes, séparées ici
///
/// L'utilisateur tape « jean@x.fr, marie@y.fr » dans un champ. Le découpage a lieu dans
/// [`Compose::recipients`], et **la validation a lieu chez le démon** : c'est lui qui refuse une
/// adresse qui porte un retour à la ligne, parce que c'est lui qui écrit la commande SMTP. Une
/// validation côté interface serait une deuxième copie de la règle, donc une divergence
/// possible — et du mauvais côté, puisque le démon sert aussi d'autres clients.
///
/// Ce que l'interface fait, elle, c'est **montrer** le refus. Voir `Shell::compose_panel`.
#[derive(Debug, Clone)]
pub struct Compose {
    /// Le compte qui envoie.
    pub account: i64,
    /// Les destinataires visibles, tels que tapés : séparés par des virgules ou des
    /// points-virgules.
    pub to: String,
    /// En copie, visibles.
    pub cc: String,
    /// En copie **cachée**.
    pub bcc: String,
    /// Le sujet.
    pub subject: String,
    /// Le corps, en texte brut.
    pub text: String,
    /// Le `Message-ID` auquel on répond, pour une réponse.
    pub in_reply_to: Option<String>,
    /// La chaîne des `Message-ID` du fil.
    pub references: Vec<String>,
    /// Les pièces jointes déjà rangées dans le magasin de blobs.
    ///
    /// Elles y sont mises par `Request::Attach` **avant** d'arriver ici : le brouillon ne
    /// porte que des hachages, jamais des chemins.
    pub attachments: Vec<dto::Attached>,
    /// Joindre la signature du compte à ce message.
    ///
    /// ## L'interface demande, elle ne compose pas
    ///
    /// C'est le démon qui ajoute la signature au corps, par `Draft::sign_with` — la même
    /// fonction que `mail send --signature`. La coquille pourrait la coller elle-même dans
    /// `text` : ce serait une deuxième implémentation de « comment une signature rejoint un
    /// message », et c'est exactement le genre de doublon qui envoie à quelqu'un une signature
    /// en double.
    ///
    /// Ce que la coquille fait, elle, c'est la **montrer** avant l'envoi : voir
    /// `Shell::compose_form`. `docs/PHASE-3.md` demande que rien de ce qu'on ajoute au message
    /// ne soit invisible, et la case à cocher est ce qui rend l'ajout refusable.
    pub sign: bool,
    /// L'identifiant du brouillon dont ce formulaire vient, ou qu'il a déjà produit.
    ///
    /// ## Pourquoi le formulaire porte cet identifiant
    ///
    /// Sans lui, chaque enregistrement créerait une ligne : fermer une fenêtre après trois
    /// enregistrements laisserait trois brouillons du même message. Il est posé par la réponse
    /// du service au premier enregistrement, et renvoyé aux suivants.
    ///
    /// `None` pour une fenêtre qui n'a jamais été enregistrée.
    pub draft: Option<i64>,
}

impl Default for Compose {
    /// Tout est vide, **sauf** la signature qui est jointe.
    ///
    /// `Default` est écrit à la main pour ce seul champ : une signature qu'on a pris la peine de
    /// composer sert à être envoyée, et un défaut à faux la ferait oublier à chaque message
    /// jusqu'à ce que quelqu'un remarque son absence. La case reste décochable, message par
    /// message.
    fn default() -> Self {
        Self {
            account: 0,
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: String::new(),
            text: String::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            sign: true,
            draft: None,
        }
    }
}

impl Compose {
    /// Découpe un champ d'adresses.
    ///
    /// Sur la virgule **et** le point-virgule : Outlook sépare au point-virgule, et un
    /// utilisateur qui copie une liste depuis un autre client la collera telle quelle. Les
    /// vides sont jetés — une virgule finale est une frappe ordinaire, pas une erreur à
    /// signaler.
    #[must_use]
    pub fn recipients(field: &str) -> Vec<String> {
        field
            .split([',', ';'])
            .map(str::trim)
            .filter(|it| !it.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    /// Vrai s'il y a de quoi tenter un envoi.
    ///
    /// **Le sujet et le corps peuvent être vides** : un message sans sujet est un message
    /// valide, et l'interdire serait une politesse imposée. Un destinataire, en revanche, est
    /// ce sans quoi il n'y a rien à faire — et le refuser ici évite un aller-retour dont on
    /// connaît déjà l'issue.
    #[must_use]
    pub fn is_sendable(&self) -> bool {
        !Self::recipients(&self.to).is_empty()
            || !Self::recipients(&self.cc).is_empty()
            || !Self::recipients(&self.bcc).is_empty()
    }
}

/// Ce qu'un fil rend.
#[derive(Debug)]
pub enum Reply {
    /// Le premier écran : les dossiers, le dossier ouvert, et sa première page.
    Bootstrap {
        folders: Vec<dto::Folder>,
        folder: i64,
        page: Box<dto::Page>,
    },
    Hello(dto::Hello),
    Folders(Vec<dto::Folder>),
    /// La page, avec le dossier **et le curseur** de la demande.
    ///
    /// ## Pourquoi le curseur revient avec la réponse
    ///
    /// Le dossier seul ne suffit pas, et le cas a été trouvé en relecture le 2026-09-03 : une
    /// page `after=c1` en vol, un `Changed` qui recharge **le même** dossier, et la page en
    /// retard arrive avec le bon numéro de dossier. Elle est alors concaténée à une liste
    /// repartie de zéro : la liste commence à la centième ligne, les quatre-vingt-dix-neuf
    /// premières manquent, et le curseur enchaîne à partir de là.
    ///
    /// Ce n'est pas théorique : un import émet un `Changed` à chaque lot de 5 000 messages, soit
    /// une quinzaine de fois sur le corpus réel.
    ///
    /// Avec le curseur, l'interface reconnaît sa demande : une page dont le curseur ne
    /// correspond pas à celui qu'elle attend est jetée.
    Page {
        folder: i64,
        after: Option<String>,
        page: Box<dto::Page>,
    },
    Open(Box<Option<Opened>>),
    Thread(Box<dto::Thread>),
    Search(Box<dto::Results>),
    Sources(Vec<dto::Source>),
    Jobs(Vec<dto::Job>),
    Accounts(Vec<dto::Account>),
    /// Un fichier est rangé dans le magasin, prêt à être joint.
    Attached(Box<dto::Attached>),
    /// Les propositions, **avec le jeton de la demande**.
    ///
    /// Le jeton est ce qui évite d'afficher les propositions d'une frappe dépassée : le fil
    /// est sériel, donc une réponse peut arriver après que l'utilisateur a tapé une lettre de
    /// plus. Sans lui, la liste clignoterait entre deux états. C'est la même leçon que le
    /// curseur de `Reply::Page`, apprise à la phase 1.
    Complete {
        token: u64,
        found: Vec<dto::Suggestion>,
    },
    Outbox(Vec<dto::Outgoing>),
    /// La source d'un message, **avec ce dont elle est la source**.
    ///
    /// L'origine revient avec la réponse pour la même raison que le curseur de [`Reply::Page`] :
    /// la fenêtre peut avoir changé de cible pendant l'appel, et afficher les octets d'un autre
    /// message sous un titre qui en nomme un est exactement ce qu'une vue de vérification ne
    /// peut pas se permettre.
    Source {
        of: SourceOf,
        found: Box<Option<dto::MessageSource>>,
    },
    /// Les comptes avec leur serveur, pour la page de paramètres.
    AccountsDetail(Vec<crate::settings::AccountDetail>),
    /// L'URL de consentement, dès qu'elle est connue.
    ///
    /// Envoyée **avant** l'attente, et c'est le point : l'ouverture du navigateur peut échouer,
    /// et l'adresse est alors le seul moyen de finir. La même raison qui la fait imprimer par
    /// `mail account add`.
    ConsentUrl(String),
    /// Une écriture de compte a abouti, avec la phrase à montrer.
    ///
    /// Une phrase et non un accusé muet : ce que l'écriture a fait n'est pas toujours ce que
    /// l'utilisateur croit avoir demandé — réadopter un jeton du trousseau, par exemple, n'a
    /// rien écrit dans le trousseau, et il doit pouvoir le lire.
    AccountWritten(String),
    /// Les brouillons, du plus récemment touché au plus ancien.
    Drafts(Vec<dto::Draft>),
    /// Un brouillon a été enregistré — ou effacé parce qu'il était vide.
    ///
    /// L'identifiant rendu est celui à renvoyer au prochain enregistrement : sans lui, chaque
    /// enregistrement créerait une nouvelle ligne. `None` quand le brouillon était vide.
    Saved(Option<i64>),
    /// La signature d'un compte, telle que le store la rend. `None` : ce compte n'en a pas.
    Signature {
        account: i64,
        found: Option<mailhtml::rich::Document>,
    },
    /// Un message est en file. L'interface le verra dans le prochain `outbox.list`.
    Queued(Box<dto::Queued>),
    /// Une écriture a été acceptée. L'interface en verra l'effet au prochain `Changed`.
    Acknowledged,
    /// Une tâche de fond a été mise en file. L'interface la verra dans le prochain `jobs.list`.
    Started,
    /// Le store a bougé. Vient du fil d'abonnement, sans que l'interface l'ait demandé.
    Changed,
    /// Le transport est revenu. Émis une fois, après un échec.
    Online,
    /// Un appel refusé ou en échec. `what` nomme la demande, pour que l'interface sache quel
    /// état marquer en erreur.
    Failed {
        what: &'static str,
        message: String,
        /// Vrai si c'est le **transport** qui a échoué, et non le service qui a refusé.
        ///
        /// La distinction est le critère 9 : « service injoignable » met l'interface en mode
        /// dégradé visible, alors qu'un `search.query` mal formé est une erreur de
        /// l'utilisateur. Les confondre ferait afficher « hors ligne » sur une faute de frappe.
        transport: bool,
    },
}

/// Un message ouvert, **corps déjà découpé**.
///
/// ## Pourquoi le découpage se fait ici et pas dans l'interface
///
/// `mailhtml::blocks` analyse une entrée hostile. Sur un corps au plafond de deux mégaoctets, il
/// prend ~6 ms — mesuré — et c'était vingt secondes avant que son tokeniseur d'attributs soit
/// réécrit. Six millisecondes, c'est déjà un tiers d'une image à 60 fps ; vingt secondes,
/// c'était l'interface figée par un mail.
///
/// Le faire dans le fil, une fois par message, met les deux hors du chemin du dessin : quel que
/// soit le coût futur de l'analyse, il ne peut plus faire tomber une image.
#[derive(Debug)]
pub struct Opened {
    pub message: dto::Message,
    /// Le corps en blocs affichables. Vide quand le message n'a pas de partie HTML.
    pub body: Vec<mailhtml::blocks::Block>,
    /// Ce que le service a coûté, mesuré **sur ce fil** : appel, transport, décodage,
    /// découpage du corps.
    ///
    /// ## Pourquoi ce chiffre voyage avec la réponse
    ///
    /// Le délai que l'interface peut mesurer est arrondi à la cadence du compositeur : une
    /// réponse arrivée entre deux images n'est vue qu'à la suivante. Un relevé de 32 ms sur un
    /// écran à 60 Hz est donc compatible avec 3 ms de travail et une image d'attente — et
    /// confondre les deux est exactement l'erreur qui avait déjà été commise sur le critère 2.
    ///
    /// Le coût du service, lui, ne dépend pas de la cadence. Les deux chiffres se rapportent
    /// ensemble : l'un est ce que l'utilisateur attend, l'autre est ce dont on répond.
    pub served_ms: f64,
}

/// Ce que la coquille monte derrière elle.
#[derive(Debug)]
pub enum Backend {
    /// Le service dans le processus. Rien n'écoute, pas de jeton.
    Embedded(maild::Service),
    /// Un démon déjà en place. Le jeton vient du trousseau du système.
    Remote { host: String, token: String },
}

/// La poignée que l'interface garde.
#[derive(Debug)]
pub struct Worker {
    requests: Sender<Request>,
    replies: Receiver<Reply>,
    /// Posée une fois l'interface créée : c'est par là que les fils la réveillent.
    context: Arc<OnceLock<egui::Context>>,
    /// Vrai quand le dos est un démon distant. Ne change jamais.
    remote: bool,
}

impl Worker {
    /// Démarre les fils autour d'un dos, embarqué ou distant.
    ///
    /// Le dos est **déplacé** dans les fils : personne d'autre ne le touche, donc il n'y a pas
    /// de verrou de plus à prendre côté interface.
    #[must_use]
    pub fn start(backend: Backend) -> Self {
        // Relevé avant que le dos ne parte dans les fils : la page de paramètres doit savoir
        // si elle peut écrire, et le demander au fil ferait un aller-retour pour une valeur
        // qui ne change jamais de la vie du processus.
        let remote = matches!(backend, Backend::Remote { .. });
        let (requests, inbox) = channel::<Request>();
        let (outbox, replies) = channel::<Reply>();
        let context: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());

        // Deux liens indépendants, un par fil. En embarqué c'est la même boîte derrière le même
        // verrou ; en distant ce sont deux connexions TCP, ce qui est exactement ce qu'il faut :
        // l'abonnement dort trente secondes, il ne doit pas retenir la connexion des lectures.
        let (first, second) = Link::pair(backend);

        spawn_named("mailcore-shell-api", {
            let context = Arc::clone(&context);
            let outbox = outbox.clone();
            move || interactive(first, &inbox, &outbox, &context)
        });
        spawn_named("mailcore-shell-abonnement", {
            let context = Arc::clone(&context);
            move || subscribe(second, &outbox, &context)
        });

        Self {
            requests,
            replies,
            context,
            remote,
        }
    }

    /// Vrai quand le service est au bout du réseau.
    ///
    /// Ce que la page de paramètres lit pour savoir si elle peut écrire : les comptes se
    /// déclarent sur la machine du démon, parce que c'est son trousseau qui porte le secret.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.remote
    }

    /// Donne aux fils de quoi réveiller l'interface. À appeler une fois, à la création.
    pub fn attach(&self, context: &egui::Context) {
        // `set` échoue si le contexte est déjà posé : c'est un appel en trop, pas une erreur.
        let _ = self.context.set(context.clone());
    }

    /// Envoie une demande. Silencieuse si le fil est mort : l'interface reste utilisable.
    pub fn ask(&self, request: Request) {
        if self.requests.send(request).is_err() {
            tracing::warn!("le fil d'API ne répond plus");
        }
    }

    /// Récupère les réponses arrivées depuis la dernière image.
    ///
    /// Sans attendre, et **toutes** d'un coup : traiter une seule réponse par image ferait
    /// prendre 41 images à un chargement de 41 pages.
    pub fn drain(&self) -> Vec<Reply> {
        let mut out = Vec::new();
        loop {
            match self.replies.try_recv() {
                Ok(reply) => out.push(reply),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }
}

/// Démarre un fil nommé, et se contente de le dire si le système refuse.
///
/// Sans ces fils l'application ne lit rien, mais elle s'ouvre quand même sur un état vide et un
/// message — ce qui vaut mieux qu'une fenêtre qui n'apparaît jamais.
fn spawn_named(name: &str, body: impl FnOnce() + Send + 'static) {
    if let Err(source) = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
    {
        tracing::error!(%source, fil = name, "fil non démarré");
    }
}

/// Le résultat d'un appel, en trois cas et pas deux.
///
/// **La distinction entre un refus et une panne est le critère 9.** Un `search.query` mal formé
/// est une erreur de l'utilisateur, et l'interface doit l'afficher comme telle ; un service
/// injoignable est un mode dégradé, et l'interface doit le dire et garder ce qu'elle a.
#[derive(Debug)]
enum Outcome {
    /// Le service a répondu.
    Done(Value),
    /// Le service a refusé : méthode inconnue, paramètres invalides, message absent.
    Refused(String),
    /// Le transport a échoué : rien n'a été servi, et rien ne le sera tant que ça dure.
    Down(String),
}

/// Le lien d'un fil vers le service. **Le seul endroit qui sait par où passe un appel.**
enum Link {
    /// Le service dans le processus.
    Embedded {
        api: maild::api::jsonrpc::Api,
        /// La boîte, pour ce que l'API ne sert pas.
        ///
        /// Un seul usage : ranger une pièce jointe dans le magasin de blobs. `outbox.send`
        /// désigne les pièces par leur **contenu** et jamais par un chemin — voir
        /// `mailsmtp::compose::Attachment` — donc quelqu'un doit mettre le fichier dans le
        /// magasin avant, et ce quelqu'un doit être **du côté du fichier**.
        ///
        /// En embarqué, c'est ici : le chemin ne traverse aucune frontière de processus, et
        /// aucune capacité nouvelle n'est donnée à personne. En distant, il n'y a pas de
        /// chemin — c'est la limite documentée des pièces jointes à distance.
        mailbox: std::sync::Arc<std::sync::Mutex<mailcore::Mailbox>>,
        /// Créé à la première utilisation, sur le fil qui s'en sert : `handle_message` est
        /// `async` et `store.wait` a besoin d'une horloge.
        runtime: Option<tokio::runtime::Runtime>,
    },
    /// Un démon joint en HTTP.
    Remote {
        host: String,
        token: String,
        /// La connexion, reconstruite après une panne. `mailapi::client` la réutilise d'un appel
        /// à l'autre : ouvrir un TCP par requête ajouterait une poignée de main à chaque page.
        client: Option<mailapi::client::Client>,
    },
}

impl Link {
    /// Les deux liens dont les deux fils ont besoin.
    fn pair(backend: Backend) -> (Self, Self) {
        match backend {
            Backend::Embedded(service) => {
                // La poignée d'API est clonable et partage la même boîte derrière le même
                // verrou : deux liens, un seul store, aucun état dupliqué.
                let second = service.api.clone();
                let mailbox = std::sync::Arc::clone(&service.mailbox);
                (
                    Self::Embedded {
                        api: service.api,
                        mailbox: service.mailbox,
                        runtime: None,
                    },
                    Self::Embedded {
                        api: second,
                        mailbox,
                        runtime: None,
                    },
                )
            }
            Backend::Remote { host, token } => (
                Self::Remote {
                    host: host.clone(),
                    token: token.clone(),
                    client: None,
                },
                Self::Remote {
                    host,
                    token,
                    client: None,
                },
            ),
        }
    }

    /// Range un fichier dans le magasin de blobs, et rend de quoi le joindre.
    ///
    /// ## Pourquoi ça ne passe pas par l'API
    ///
    /// `outbox.send` désigne les pièces jointes par leur **contenu** et jamais par un chemin :
    /// un chemin dans une demande de client donnerait, à quiconque détient le jeton du démon, la
    /// lecture de n'importe quel fichier de la machine. Quelqu'un doit donc mettre le fichier
    /// dans le magasin avant, et ce quelqu'un doit être **du côté du fichier**.
    ///
    /// En embarqué, c'est ce fil : le chemin ne traverse aucune frontière de processus, et
    /// aucune capacité nouvelle n'est donnée à personne.
    ///
    /// ## En distant, c'est un refus
    ///
    /// Et un refus qui dit pourquoi. Téléverser le fichier demanderait un protocole qui n'existe
    /// pas encore ; le faire passer en base64 dans le JSON-RPC ferait 34 Mo de corps de requête
    /// pour une pièce de 25 Mo. Les deux sont du travail, et prétendre le contraire ferait
    /// échouer l'envoi au dernier moment plutôt qu'au moment de joindre.
    fn stage(&mut self, path: &camino::Utf8Path) -> Result<dto::Attached, String> {
        let Self::Embedded { mailbox, .. } = self else {
            return Err(
                "joindre un fichier demande un service local : un démon distant n'a pas accès \
                 au fichier, et le téléverser n'est pas encore écrit."
                    .to_owned(),
            );
        };

        let mut file = std::fs::File::open(path.as_std_path())
            .map_err(|source| format!("{path} illisible : {source}"))?;
        let size = file
            .metadata()
            .map_err(|source| format!("taille de {path} : {source}"))?
            .len();

        let guard = mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `put_reader` hache et compresse au passage : une pièce de 25 Mo se range sans être
        // chargée. C'est le critère 3, et la règle 4 du `CLAUDE.md`.
        let put = guard
            .store()
            .blobs()
            .put_reader(&mut file)
            .map_err(|source| format!("mise au magasin de {path} : {source}"))?;
        drop(guard);

        Ok(dto::Attached {
            filename: path.file_name().unwrap_or("piece-jointe").to_owned(),
            blob: put.hash.to_hex(),
            size,
        })
    }

    /// Ce que la page de paramètres refuse de faire à distance, et pourquoi.
    ///
    /// La même règle que `mail account …` en mode `--daemon` : le trousseau qui porte le secret
    /// est celui de la machine du démon, et un secret ne traverse pas le réseau pour aller s'y
    /// ranger. Un texte partagé, parce que quatre refus rédigés séparément finiraient par dire
    /// quatre choses.
    const REMOTE_REFUSAL: &'static str = "les comptes se déclarent sur la machine du démon : c'est son trousseau qui porte le \
         secret. Lancer la coquille sans --daemon, ou `mail account …` là-bas.";

    /// Le store local, ou le refus qui explique pourquoi il n'y en a pas.
    fn local(&mut self) -> Result<&std::sync::Arc<std::sync::Mutex<mailcore::Mailbox>>, String> {
        match self {
            Self::Embedded { mailbox, .. } => Ok(mailbox),
            Self::Remote { .. } => Err(Self::REMOTE_REFUSAL.to_owned()),
        }
    }

    /// Les comptes, serveur et état du trousseau compris.
    ///
    /// ## Pourquoi ça ne passe pas par `accounts.list`
    ///
    /// Parce que `accounts.list` ne rend **ni hôte ni port**, délibérément — critère 7 de
    /// `docs/PHASE-3.md`. Les y ajouter pour faire marcher une page de paramètres donnerait
    /// l'infrastructure de lecture de quelqu'un à tout client qui détient le jeton, pour le
    /// confort d'un écran qui n'existe qu'en local. La page lit donc le store directement.
    fn accounts_detail(&mut self) -> Result<Vec<crate::settings::AccountDetail>, String> {
        let mailbox = self.local()?;
        let guard = mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = guard
            .store()
            .full_accounts()
            .map_err(|source| format!("comptes illisibles : {source}"))?;
        drop(guard);

        Ok(accounts
            .iter()
            .map(|account| {
                let server = account.server.as_ref();
                crate::settings::AccountDetail {
                    id: account.id.0,
                    name: account.display_name.clone(),
                    server: server.map_or_else(
                        || "aucun serveur (compte importé)".to_owned(),
                        |it| format!("{}:{}", it.host, it.port),
                    ),
                    username: server.map_or_else(String::new, |it| it.username.clone()),
                    security: server.map_or_else(String::new, |it| it.security.as_str().to_owned()),
                    auth: server.map_or_else(String::new, |it| it.auth.as_str().to_owned()),
                    // La **présence**, jamais la valeur : `has_secret` ne lit pas le secret.
                    secret: server.is_some_and(|it| {
                        mailauth::session::has_secret(&it.host, &it.username, it.auth.as_str())
                    }),
                    submission: account
                        .submission
                        .as_ref()
                        .map(|it| format!("{}:{} {}", it.host, it.port, it.security.as_str())),
                    enabled: account.enabled,
                }
            })
            .collect())
    }

    /// Déclare, ou redéclare, un compte.
    ///
    /// ## L'ordre : le trousseau d'abord, le store ensuite
    ///
    /// Repris de `mail account add`, et c'est le bon ordre. Si le secret n'arrive pas à
    /// s'enregistrer — session sans Secret Service, trousseau verrouillé — on ne veut pas d'un
    /// compte inscrit dans le store dont la synchronisation échouera sans dire pourquoi.
    ///
    /// ## Le secret n'est demandé que s'il manque
    ///
    /// Le trousseau survit au store : le 2026-09-11, les cinq entrées étaient intactes alors que
    /// le store était vide. Redemander un secret déjà rangé, c'est au mieux faire retaper un mot
    /// de passe, au pire refaire un consentement OAuth2 complet pour aboutir au jeton qu'on
    /// avait déjà.
    fn declare_account(&mut self, form: &crate::settings::AccountForm) -> Result<String, String> {
        use mailcore::{AuthKind, Security, Server};

        let host = form.host.trim();
        let username = form.username.trim();
        if host.is_empty() || username.is_empty() {
            return Err("un serveur et un identifiant, au minimum".to_owned());
        }
        let security = Security::parse(&form.security)
            .map_err(|_| format!("chiffrement `{}` : attendu tls ou starttls", form.security))?;
        let auth = AuthKind::parse(&form.auth)
            .map_err(|_| format!("mécanisme `{}` : attendu password ou oauth2", form.auth))?;
        let port = match form.port.trim() {
            "" => security.default_port(),
            given => given
                .parse()
                .map_err(|_| format!("port `{given}` : attendu un nombre"))?,
        };

        let server = Server {
            host: host.to_owned(),
            port,
            username: username.to_owned(),
            auth,
            security,
        };

        let already = mailauth::session::has_secret(host, username, auth.as_str());
        let secret = form.secret.trim();
        let note = match (auth, already, secret.is_empty()) {
            // Un secret tapé l'emporte sur celui du trousseau : c'est le chemin du mot de passe
            // qui vient d'être changé chez le fournisseur.
            (AuthKind::Password, _, false) => {
                mailauth::store(host, username, secret)
                    .map_err(|source| format!("trousseau : {source}"))?;
                "secret enregistré dans le trousseau"
            }
            (AuthKind::Password, true, true) => "secret déjà dans le trousseau, réutilisé",
            (AuthKind::Password, false, true) => {
                return Err(
                    "aucun secret dans le trousseau pour ce compte : il en faut un".to_owned(),
                );
            }
            (AuthKind::OAuth2, true, _) => "jeton OAuth2 déjà dans le trousseau, réadopté",
            (AuthKind::OAuth2, false, _) => {
                // Refusé plutôt que tenté : un consentement demande un navigateur, un écouteur
                // de bouclage et un identifiant client. Prétendre ici que ça a marché ferait
                // échouer la première moisson sans dire pourquoi.
                return Err(
                    "aucun jeton OAuth2 dans le trousseau pour ce compte. Le consentement \
                     n'est pas encore dans cette page : `mail account add --auth oauth2 \
                     --client-id …` l'obtient, puis ce formulaire le réadoptera."
                        .to_owned(),
                );
            }
        };

        let mailbox = self.local()?;
        let guard = mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let writer = guard
            .store()
            .writer()
            .map_err(|source| format!("store non inscriptible : {source}"))?;
        let account = writer
            .upsert_imap_account(username, &server)
            .map_err(|source| format!("écriture du compte : {source}"))?;
        // Redéclarer un compte le réactive : y reposer un secret veut dire qu'on veut qu'il
        // remarche. La même règle que `mail account add`.
        writer
            .set_account_enabled(account, true)
            .map_err(|source| format!("réactivation : {source}"))?;
        writer
            .commit()
            .map_err(|source| format!("validation : {source}"))?;
        drop(guard);

        Ok(format!(
            "Compte #{} enregistré — {host}:{port} {} {} · {note}.",
            account.0,
            security.as_str(),
            auth.as_str(),
        ))
    }

    /// Écrit le serveur de soumission d'un compte.
    ///
    /// Le mode de chiffrement vient du formulaire et n'est **jamais** déduit : le deviner
    /// rétrograderait le chiffrement à l'insu de l'utilisateur. C'est la règle du `CLAUDE.md`,
    /// et la raison pour laquelle la page suggère un hôte dans un bouton plutôt que de le
    /// préremplir.
    fn set_submission(&mut self, form: &crate::settings::SubmissionForm) -> Result<String, String> {
        use mailcore::{Security, Server};

        let host = form.host.trim();
        if host.is_empty() {
            return Err("un serveur d'envoi, au minimum".to_owned());
        }
        let security = Security::parse(&form.security)
            .map_err(|_| format!("chiffrement `{}` : attendu tls ou starttls", form.security))?;
        let port = match form.port.trim() {
            "" => security.submission_port(),
            given => given
                .parse()
                .map_err(|_| format!("port `{given}` : attendu un nombre"))?,
        };

        let mailbox = self.local()?;
        let guard = mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = guard
            .store()
            .full_accounts()
            .map_err(|source| format!("comptes illisibles : {source}"))?;
        let account = accounts
            .iter()
            .find(|it| it.id.0 == form.account)
            .ok_or_else(|| format!("aucun compte #{}", form.account))?;
        let reading = account
            .server
            .as_ref()
            .ok_or_else(|| format!("le compte #{} n'a pas de serveur", form.account))?;

        // L'identifiant et le mécanisme se replient sur ceux de la lecture — c'est le cas de
        // tous les fournisseurs du corpus. Le chiffrement, jamais : voir plus haut.
        let submission = Server {
            host: host.to_owned(),
            port,
            username: reading.username.clone(),
            auth: reading.auth,
            security,
        };

        let writer = guard
            .store()
            .writer()
            .map_err(|source| format!("store non inscriptible : {source}"))?;
        writer
            .set_submission(account.id, Some(&submission))
            .map_err(|source| format!("écriture du serveur d'envoi : {source}"))?;
        writer
            .commit()
            .map_err(|source| format!("validation : {source}"))?;
        drop(guard);

        Ok(format!(
            "Compte #{} : envoi par {host}:{port} {}. Rien n'a été envoyé.",
            form.account,
            security.as_str(),
        ))
    }

    /// Oublie le secret d'un compte et met sa synchronisation en pause.
    fn forget_account(&mut self, id: i64) -> Result<String, String> {
        let mailbox = self.local()?;
        let guard = mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = guard
            .store()
            .full_accounts()
            .map_err(|source| format!("comptes illisibles : {source}"))?;
        let account = accounts
            .iter()
            .find(|it| it.id.0 == id)
            .ok_or_else(|| format!("aucun compte #{id}"))?;
        let server = account
            .server
            .as_ref()
            .ok_or_else(|| format!("le compte #{id} n'a pas de serveur"))?;

        // **Les deux entrées, quel que soit le mécanisme déclaré.** Un compte a pu passer du
        // mot de passe applicatif à OAuth2 ; n'effacer que celle du jour laisserait l'autre au
        // trousseau, où personne ne pensera à la chercher après avoir lu « secret oublié ».
        mailauth::forget(&server.host, &server.username)
            .map_err(|source| format!("trousseau : {source}"))?;
        mailauth::session::forget_oauth2(&server.host, &server.username)
            .map_err(|source| format!("trousseau : {source}"))?;

        let oauth = server.auth == mailcore::AuthKind::OAuth2;
        let writer = guard
            .store()
            .writer()
            .map_err(|source| format!("store non inscriptible : {source}"))?;
        writer
            .set_account_enabled(account.id, false)
            .map_err(|source| format!("mise en pause : {source}"))?;
        writer
            .commit()
            .map_err(|source| format!("validation : {source}"))?;
        drop(guard);

        let mut said = format!(
            "Compte #{id} : secret oublié, synchronisation en pause. Le courrier déjà \
             téléchargé reste."
        );
        if oauth {
            // Le dire, parce que l'inverse se croit facilement : « j'ai retiré le compte, donc
            // l'application n'a plus accès ». Le jeton reste valide côté fournisseur.
            said.push_str(
                " Le jeton reste valide chez le fournisseur : le révoquer se fait dans son \
                 écran de sécurité.",
            );
        }
        Ok(said)
    }

    /// Appelle une méthode et rend son résultat.
    fn call(&mut self, method: &str, params: Value) -> Outcome {
        match self {
            Self::Embedded { api, runtime, .. } => {
                let runtime = match runtime {
                    Some(runtime) => runtime,
                    None => {
                        // Un runtime en fil courant : les lectures partent sur le pool bloquant
                        // et les tâches de fond de `maild` ont leur propre fil `std`.
                        match tokio::runtime::Builder::new_current_thread()
                            .enable_time()
                            .build()
                        {
                            Ok(built) => runtime.insert(built),
                            Err(source) => {
                                return Outcome::Down(format!("runtime non créé : {source}"));
                            }
                        }
                    }
                };
                runtime.block_on(embedded_call(api, method, params))
            }
            Self::Remote {
                host,
                token,
                client,
            } => {
                if client.is_none() {
                    match mailapi::client::Client::connect(host, token) {
                        Ok(opened) => *client = Some(opened),
                        // Un refus d'envoyer le jeton en clair n'est pas une panne passagère,
                        // c'est une configuration à corriger. Il sort quand même en `Down` :
                        // du point de vue de l'interface, le service est inatteignable.
                        Err(source) => return Outcome::Down(source.to_string()),
                    }
                }
                let Some(open) = client.as_mut() else {
                    return Outcome::Down("connexion absente".to_owned());
                };

                match open.call(method, params) {
                    Ok(result) => Outcome::Done(result),
                    // Le service a répondu, et il a dit non.
                    Err(mailapi::client::Error::Rpc(error)) => Outcome::Refused(error.message),
                    Err(source) => {
                        // Panne de transport : la connexion est jetée pour que l'appel suivant
                        // en rouvre une. Sans ça, un démon redémarré resterait « injoignable »
                        // jusqu'à la fermeture de la coquille.
                        *client = None;
                        Outcome::Down(source.to_string())
                    }
                }
            }
        }
    }
}

/// Sert un appel au service embarqué, par la fonction que le démon sert sur HTTP.
async fn embedded_call(api: &maild::api::jsonrpc::Api, method: &str, params: Value) -> Outcome {
    /// Un identifiant par appel. La valeur n'a pas d'importance — un seul appel est en vol à la
    /// fois sur un fil donné — mais l'omettre ferait de la requête une notification, à laquelle
    /// le service ne répond rien du tout.
    fn call_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    let message = json!({
        "jsonrpc": "2.0",
        "id": call_id(),
        "method": method,
        "params": params,
    });
    let text = match serde_json::to_string(&message) {
        Ok(text) => text,
        Err(source) => return Outcome::Refused(format!("requête non sérialisable : {source}")),
    };

    // `None` veut dire « pas de réponse » : impossible ici, puisque la requête porte un `id`.
    // Le traiter en panne plutôt qu'en refus est le bon choix — c'est le service qui n'a pas
    // fonctionné, pas la demande qui était mauvaise.
    let Some(response) = api.handle_message(&text).await else {
        return Outcome::Down("le service n'a rien répondu".to_owned());
    };
    extract(&response)
}

/// La boucle du fil interactif : une demande, une réponse, un réveil.
fn interactive(
    mut link: Link,
    inbox: &Receiver<Request>,
    outbox: &Sender<Reply>,
    context: &Arc<OnceLock<egui::Context>>,
) {
    // L'état du transport, tel que ce fil le connaît. Il ne sert qu'à n'annoncer un retour
    // qu'une fois : l'interface n'a pas besoin d'un `Online` à chaque appel réussi.
    let mut down = false;

    // Le canal se ferme quand l'interface disparaît : c'est la condition d'arrêt du fil.
    while let Ok(request) = inbox.recv() {
        // **Le service est chronométré ici, et nulle part ailleurs.** Ce que l'interface peut
        // mesurer, elle le mesure par image : une réponse arrivée entre deux images n'est vue
        // qu'à la suivante, donc tout délai relevé là-bas est arrondi à la cadence du
        // compositeur. Le coût du service, lui, ne dépend pas d'elle. Voir le critère 5 dans
        // `docs/PHASE-1.md` : les deux chiffres disent deux choses, et confondre les deux est
        // l'erreur qui avait déjà été faite sur le critère 2.
        // **Le consentement part sur son propre fil, et ne revient pas par ici.** Il attend un
        // humain qui revient de son navigateur : le servir en ligne mettrait toute la boîte mail
        // en file derrière lui pendant cinq minutes. C'est la règle 3 du `CLAUDE.md` appliquée
        // au fil du service — l'interface ne bloquerait pas, mais elle n'aurait plus rien à
        // afficher, ce qui revient au même pour qui regarde.
        if let Request::Consent(form) = request {
            spawn_consent(*form, outbox.clone(), Arc::clone(context));
            continue;
        }

        let at = std::time::Instant::now();
        let mut reply = serve(&mut link, request);
        if let Reply::Open(opened) = &mut reply
            && let Some(opened) = opened.as_mut()
        {
            opened.served_ms = at.elapsed().as_secs_f64() * 1000.0;
        }
        let broken = matches!(
            reply,
            Reply::Failed {
                transport: true,
                ..
            }
        );

        if broken {
            down = true;
        } else if down {
            down = false;
            if outbox.send(Reply::Online).is_err() {
                return;
            }
        }

        if outbox.send(reply).is_err() {
            return;
        }
        wake(context);
    }
}

/// La boucle d'abonnement : un appel long qui réveille l'interface quand le store bouge.
///
/// Aucune minuterie **de ce côté** : le client n'envoie rien tant qu'il n'a pas été réveillé,
/// et il apprend un import déclenché ailleurs — par la CLI, par un autre client — sans
/// interroger toutes les secondes. Le sondage existe, mais il est chez le démon, une fois pour
/// tous les clients.
///
/// **C'est aussi ce fil qui découvre qu'un démon distant est tombé**, sans que l'utilisateur ait
/// à cliquer : son `store.wait` échoue, et l'interface passe en mode dégradé visible.
fn subscribe(mut link: Link, outbox: &Sender<Reply>, context: &Arc<OnceLock<egui::Context>>) {
    /// Le délai d'un appel `store.wait`. Au-delà, le service rend `changed: false` et on
    /// rappelle : c'est ce qui permet au fil de constater que l'interface a disparu.
    const WAIT_MS: u64 = 30_000;
    /// Après une panne, ne pas marteler.
    const BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

    let mut known: Option<String> = None;
    let mut down = false;

    /// Signale une panne de transport, une seule fois par épisode.
    fn report(
        outbox: &Sender<Reply>,
        context: &Arc<OnceLock<egui::Context>>,
        down: &mut bool,
        message: String,
    ) -> bool {
        if *down {
            return true;
        }
        *down = true;
        let reply = Reply::Failed {
            what: "abonnement",
            message,
            transport: true,
        };
        if outbox.send(reply).is_err() {
            return false;
        }
        wake(context);
        true
    }

    loop {
        let revision = match &known {
            Some(revision) => revision.clone(),
            None => match link.call(mailapi::method::STORE_REVISION, Value::Null) {
                Outcome::Done(result) => match serde_json::from_value::<dto::Revision>(result) {
                    Ok(it) => {
                        known = Some(it.revision.clone());
                        it.revision
                    }
                    Err(_) => {
                        std::thread::sleep(BACKOFF);
                        continue;
                    }
                },
                Outcome::Refused(_) => {
                    std::thread::sleep(BACKOFF);
                    continue;
                }
                Outcome::Down(message) => {
                    if !report(outbox, context, &mut down, message) {
                        return;
                    }
                    std::thread::sleep(BACKOFF);
                    continue;
                }
            },
        };

        let params = json!({"revision": revision, "timeout_ms": WAIT_MS});
        match link.call(mailapi::method::STORE_WAIT, params) {
            Outcome::Done(result) => {
                if down {
                    down = false;
                    if outbox.send(Reply::Online).is_err() {
                        return;
                    }
                    wake(context);
                }
                match serde_json::from_value::<dto::Change>(result) {
                    Ok(change) if change.changed => {
                        known = Some(change.revision);
                        if outbox.send(Reply::Changed).is_err() {
                            return;
                        }
                        wake(context);
                    }
                    // Délai expiré sans changement : rappeler immédiatement, c'est le régime
                    // normal d'un client au repos.
                    Ok(_) => {}
                    Err(_) => {
                        known = None;
                        std::thread::sleep(BACKOFF);
                    }
                }
            }
            Outcome::Refused(_) => {
                known = None;
                std::thread::sleep(BACKOFF);
            }
            Outcome::Down(message) => {
                known = None;
                if !report(outbox, context, &mut down, message) {
                    return;
                }
                std::thread::sleep(BACKOFF);
            }
        }
    }
}

/// Réveille l'interface, si elle est déjà là.
fn wake(context: &Arc<OnceLock<egui::Context>>) {
    if let Some(context) = context.get() {
        context.request_repaint();
    }
}

/// Déroule un consentement OAuth2 sur un fil à lui, et rend compte par le canal des réponses.
///
/// ## Pourquoi un fil de plus, et pourquoi il n'est pas joint
///
/// L'attente est celle d'un humain : il doit voir un écran de fournisseur, lire, cliquer. Cinq
/// minutes au plafond. Un fil détaché plutôt qu'une poignée gardée, parce qu'il n'y a rien à
/// attendre de lui — il parle par le canal, comme les deux autres, et si l'interface disparaît
/// avant lui son envoi échoue et il s'arrête.
///
/// ## Le trousseau, et rien d'autre
///
/// Ce fil n'a **pas** de `Link`, donc pas d'accès au store. Il écrit dans le trousseau et c'est
/// tout ; la déclaration du compte reste un geste séparé, que l'utilisateur fait ensuite avec un
/// secret désormais présent. Deux étapes plutôt qu'une : un consentement réussi suivi d'une
/// écriture de store ratée laisserait un jeton orphelin, et surtout la page dirait « compte
/// enregistré » d'un compte qui ne l'est pas.
fn spawn_consent(
    form: crate::settings::ConsentForm,
    outbox: Sender<Reply>,
    context: Arc<OnceLock<egui::Context>>,
) {
    spawn_named("mailcore-shell-consentement", move || {
        let host = form.host.trim().to_owned();
        let username = form.username.trim().to_owned();

        let failed = |message: String| Reply::Failed {
            what: "compte",
            message,
            // Pas un échec de transport : le service n'a rien à voir là-dedans, et marquer
            // l'interface « hors ligne » parce qu'un consentement a échoué serait faux.
            transport: false,
        };

        let Some(provider) = mailauth::oauth::Provider::for_host(&host) else {
            // Le point de terminaison de jeton n'est **pas** configurable, et ce refus est la
            // contrepartie visible de ce choix : le rendre configurable serait le moyen le plus
            // simple de faire envoyer un jeton de rafraîchissement ailleurs.
            let _ = outbox.send(failed(format!(
                "aucun fournisseur OAuth2 connu pour {host}. Utiliser un mot de passe \
                 applicatif avec le mécanisme `password`."
            )));
            wake(&context);
            return;
        };

        let credentials = mailauth::oauth::ClientCredentials {
            client_id: form.client_id.trim().to_owned(),
            client_secret: Some(form.client_secret.trim().to_owned()).filter(|it| !it.is_empty()),
        };
        let redirect_port = form.redirect_port.trim().parse().ok();

        // L'URL part vers l'interface **au moment où elle est connue**, avant l'attente : c'est
        // le seul instant où elle sert à quelque chose.
        let announce = |url: &str| {
            let _ = outbox.send(Reply::ConsentUrl(url.to_owned()));
            wake(&context);
        };

        let reply = match mailauth::session::authorize(
            &provider,
            &credentials,
            &username,
            &announce,
            redirect_port,
        ) {
            Ok(tokens) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |it| it.as_secs() as i64);
                match mailauth::session::store_tokens(&host, &username, &credentials, &tokens, now)
                {
                    Ok(()) => Reply::AccountWritten(format!(
                        "Jeton obtenu pour {username} et rangé dans le trousseau. \
                         Déclarez le compte : le formulaire le réadoptera sans rien redemander."
                    )),
                    Err(source) => failed(format!("trousseau : {source}")),
                }
            }
            Err(source) => failed(source.to_string()),
        };

        let _ = outbox.send(reply);
        wake(&context);
    });
}

/// Sert les demandes de la page de paramètres, qui ne passent pas par l'API.
///
/// Rend `None` pour tout le reste, que [`serve`] traite normalement. Un `match` qui rend une
/// option plutôt que quatre `if let` dans `serve` : le compilateur vérifie alors qu'aucune
/// demande de compte n'a été oubliée ici et ne part par erreur vers `describe`, où elle
/// n'aurait pas de méthode.
fn local_account_work(link: &mut Link, request: &Request) -> Option<Reply> {
    /// Le même traitement pour les trois écritures : une phrase, ou un refus qui n'est pas une
    /// panne de transport — rien n'a été demandé au service.
    fn wrote(outcome: Result<String, String>) -> Reply {
        match outcome {
            Ok(said) => Reply::AccountWritten(said),
            Err(message) => Reply::Failed {
                what: "compte",
                message,
                transport: false,
            },
        }
    }

    Some(match request {
        Request::AccountsDetail => match link.accounts_detail() {
            Ok(found) => Reply::AccountsDetail(found),
            Err(message) => Reply::Failed {
                what: "comptes",
                message,
                transport: false,
            },
        },
        Request::DeclareAccount(form) => wrote(link.declare_account(form)),
        Request::SetSubmission(form) => wrote(link.set_submission(form)),
        Request::ForgetAccount { id } => wrote(link.forget_account(*id)),
        _ => return None,
    })
}

/// Traduit une demande en appel, et la réponse en `Reply`.
///
/// La durée de chaque appel est journalisée en `debug` : c'est ce qui a permis de voir que les
/// 110 ms entre la première image et la première ligne étaient dans `folders.list`, et non dans
/// l'interface. Le **nom** de la méthode se journalise, ses paramètres non — un identifiant de
/// message ou une requête de recherche décrit ce que l'utilisateur lit (`docs/PRIVACY.md`, §8).
fn serve(link: &mut Link, request: Request) -> Reply {
    if let Request::Bootstrap { limit } = request {
        return bootstrap(link, limit);
    }
    // **Ranger une pièce jointe n'est pas un appel d'API**, et c'est le point : le chemin ne
    // traverse aucune frontière de processus. Voir `Link::stage`.
    if let Request::Attach { path } = &request {
        return match link.stage(path) {
            Ok(attached) => Reply::Attached(Box::new(attached)),
            Err(message) => Reply::Failed {
                what: "pièce jointe",
                message,
                // Pas un échec de transport : rien n'a été demandé au service. Le marquer
                // ainsi mettrait l'interface en mode dégradé pour un fichier illisible.
                transport: false,
            },
        };
    }

    // **La page de paramètres n'appelle pas l'API non plus**, et pour une raison plus forte que
    // celle des pièces jointes : elle écrit un secret. Une méthode `accounts.add` voudrait dire
    // qu'un client qui détient le jeton écrit dans le trousseau de la machine du démon, et que
    // le secret traverse le JSON-RPC pour y arriver. Voir `crate::settings`.
    if let Some(reply) = local_account_work(link, &request) {
        return reply;
    }

    let (what, method, params) = describe(&request);
    let started = std::time::Instant::now();
    let outcome = link.call(method, params);
    tracing::debug!(
        methode = method,
        ms = started.elapsed().as_secs_f64() * 1000.0,
        "appel servi"
    );

    match outcome {
        Outcome::Done(result) => decode(request, result).unwrap_or_else(|message| Reply::Failed {
            what,
            message,
            transport: false,
        }),
        Outcome::Refused(message) => Reply::Failed {
            what,
            message,
            transport: false,
        },
        Outcome::Down(message) => Reply::Failed {
            what,
            message,
            transport: true,
        },
    }
}

/// Sert le premier écran : les dossiers puis la première page, sans repasser par l'interface.
fn bootstrap(link: &mut Link, limit: u32) -> Reply {
    const WHAT: &str = "premier écran";

    /// Traduit un `Outcome` qui n'est pas un succès en échec pour l'interface.
    fn failed(outcome: Outcome, note: &str) -> Reply {
        match outcome {
            Outcome::Down(message) => Reply::Failed {
                what: WHAT,
                message,
                transport: true,
            },
            Outcome::Refused(message) => Reply::Failed {
                what: WHAT,
                message,
                transport: false,
            },
            Outcome::Done(_) => Reply::Failed {
                what: WHAT,
                message: note.to_owned(),
                transport: false,
            },
        }
    }

    let folders: Vec<dto::Folder> = match link.call(mailapi::method::FOLDERS_LIST, Value::Null) {
        Outcome::Done(result) => match serde_json::from_value(result) {
            Ok(folders) => folders,
            Err(source) => {
                return Reply::Failed {
                    what: WHAT,
                    message: format!("liste de dossiers inattendue : {source}"),
                    transport: false,
                };
            }
        },
        other => return failed(other, "dossiers illisibles"),
    };

    // La boîte de réception, à défaut le premier dossier. Un store vide n'en a aucun : la
    // coquille s'ouvre alors sur son panneau d'import, ce qui est exactement ce qu'il faut.
    let Some(folder) = folders
        .iter()
        .find(|it| it.kind == "inbox")
        .or_else(|| folders.first())
        .map(|it| it.id)
    else {
        return Reply::Bootstrap {
            folders,
            folder: 0,
            page: Box::new(dto::Page {
                rows: Vec::new(),
                next: None,
                revision: String::new(),
            }),
        };
    };

    let params = json!({"folder": folder, "limit": limit});
    match link.call(mailapi::method::MESSAGES_PAGE, params) {
        Outcome::Done(result) => match serde_json::from_value(result) {
            Ok(page) => Reply::Bootstrap {
                folders,
                folder,
                page: Box::new(page),
            },
            Err(source) => Reply::Failed {
                what: WHAT,
                message: format!("page inattendue : {source}"),
                transport: false,
            },
        },
        other => failed(other, "page illisible"),
    }
}

/// Le nom, la méthode et les paramètres d'une demande.
fn describe(request: &Request) -> (&'static str, &'static str, Value) {
    use mailapi::method as m;
    match request {
        // Servie par `bootstrap`, qui enchaîne deux méthodes : jamais décrite comme un appel
        // unique.
        // Toutes deux servies avant d'arriver ici : `bootstrap` enchaîne deux méthodes,
        // `Attach` n'en appelle aucune. Les décrire serait mentir sur ce qu'elles font.
        Request::Bootstrap { .. } => ("premier écran", m::FOLDERS_LIST, Value::Null),
        Request::Attach { .. } => ("pièce jointe", m::SERVER_HELLO, Value::Null),
        // Les quatre demandes de la page de paramètres sont servies par `local_account_work`,
        // avant d'arriver ici : aucune n'a de méthode, et il n'y en aura pas. Leur en donner une
        // voudrait dire faire traverser un secret par l'API — voir `crate::settings`.
        Request::AccountsDetail
        | Request::DeclareAccount(_)
        | Request::SetSubmission(_)
        | Request::ForgetAccount { .. }
        | Request::Consent(_) => ("compte", m::SERVER_HELLO, Value::Null),
        Request::Hello => ("hello", m::SERVER_HELLO, Value::Null),
        Request::Folders => ("dossiers", m::FOLDERS_LIST, Value::Null),
        Request::Page {
            folder,
            after,
            limit,
        } => (
            "page",
            m::MESSAGES_PAGE,
            match after {
                Some(after) => json!({"folder": folder, "after": after, "limit": limit}),
                None => json!({"folder": folder, "limit": limit}),
            },
        ),
        Request::Open { id, remote_images } => (
            "message",
            m::MESSAGES_GET,
            json!({"id": id, "body": "html", "remote_images": remote_images}),
        ),
        Request::Thread { id } => ("fil", m::MESSAGES_THREAD, json!({"id": id})),
        Request::Source { of } => match of {
            SourceOf::Message(id) => ("source", m::MESSAGES_SOURCE, json!({"id": id})),
            SourceOf::Outgoing(id) => ("source", m::OUTBOX_SOURCE, json!({"id": id})),
        },
        Request::Search { query, limit } => (
            "recherche",
            m::SEARCH_QUERY,
            json!({"query": query, "limit": limit}),
        ),
        Request::Sources => ("sources", m::JOBS_SOURCES, Value::Null),
        Request::StartImport { source } => (
            "import",
            m::JOBS_START,
            json!({"kind": "import", "source": source}),
        ),
        Request::StartIndex => ("index", m::JOBS_START, json!({"kind": "index"})),
        // Le compte est désigné par son identifiant, que le client connaît déjà — `folders.list`
        // le porte. Ce n'est donc pas une capacité nouvelle.
        Request::StartSync { account } => (
            "synchronisation",
            m::JOBS_START,
            json!({"kind": "sync", "account": account}),
        ),
        Request::Jobs => ("tâches", m::JOBS_LIST, Value::Null),
        Request::Accounts => ("comptes", m::ACCOUNTS_LIST, Value::Null),
        Request::MarkRead { id, folder } => (
            "marquage lu",
            m::MESSAGES_MARK_READ,
            json!({"id": id, "folder": folder}),
        ),
        Request::Decide { id, decision } => (
            "décision",
            m::OUTBOX_DECIDE,
            json!({"id": id, "decision": decision}),
        ),
        Request::Retry { id } => ("renvoi", m::OUTBOX_RETRY, json!({"id": id})),
        Request::Complete { prefix, .. } => (
            "complétion",
            m::CONTACTS_COMPLETE,
            json!({"prefix": prefix}),
        ),
        Request::Outbox => ("file d'envoi", m::OUTBOX_LIST, Value::Null),
        Request::StagePart { id, part } => (
            "reprise d'une pièce jointe",
            m::MESSAGES_STAGE_PART,
            json!({"id": id, "part": part}),
        ),
        Request::Drafts => ("brouillons", m::DRAFTS_LIST, Value::Null),
        Request::SaveDraft(it) => (
            "enregistrement du brouillon",
            m::DRAFTS_SAVE,
            json!({
                "id": it.draft,
                "account": it.account,
                // **Les champs tels que tapés**, pas découpés : rouvrir doit montrer ce qui
                // était à l'écran, fragment d'adresse compris.
                "to": it.to,
                "cc": it.cc,
                "bcc": it.bcc,
                "subject": it.subject,
                "body": it.text,
                "in_reply_to": it.in_reply_to,
                "references": it.references,
                "sign": it.sign,
                "attachments": it.attachments,
            }),
        ),
        Request::DeleteDraft { id } => (
            "suppression du brouillon",
            m::DRAFTS_DELETE,
            json!({"id": id}),
        ),
        Request::Send(it) => (
            "envoi",
            m::OUTBOX_SEND,
            json!({
                "account": it.account,
                "to": Compose::recipients(&it.to),
                "cc": Compose::recipients(&it.cc),
                "bcc": Compose::recipients(&it.bcc),
                "subject": it.subject,
                "text": it.text,
                // Le démon ajoute la signature du compte : voir `Compose::sign`.
                "signature": it.sign,
                "in_reply_to": it.in_reply_to,
                "references": it.references,
                "attachments": it.attachments,
            }),
        ),
        Request::Signature { account } => (
            "signature",
            m::ACCOUNTS_SIGNATURE,
            json!({"account": account}),
        ),
        Request::SetSignature { account, signature } => (
            "enregistrement de la signature",
            m::ACCOUNTS_SET_SIGNATURE,
            json!({"account": account, "signature": signature}),
        ),
    }
}

/// Sort le `result` d'une réponse JSON-RPC, ou le refus qu'elle porte.
///
/// Une réponse illisible sort en `Down` et non en `Refused` : si le service parle un dialecte
/// qu'on ne comprend pas, ce n'est pas la demande qui était mauvaise.
fn extract(response: &str) -> Outcome {
    let mut parsed: Value = match serde_json::from_str(response) {
        Ok(parsed) => parsed,
        Err(source) => return Outcome::Down(format!("réponse illisible : {source}")),
    };

    if let Some(error) = parsed.get_mut("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("erreur sans message");
        return Outcome::Refused(message.to_owned());
    }
    Outcome::Done(
        parsed
            .get_mut("result")
            .map(std::mem::take)
            .unwrap_or(Value::Null),
    )
}

/// Convertit le `result` dans le type attendu par la demande.
fn decode(request: Request, result: Value) -> Result<Reply, String> {
    fn from<T: serde::de::DeserializeOwned>(result: Value) -> Result<T, String> {
        serde_json::from_value(result).map_err(|source| format!("réponse inattendue : {source}"))
    }

    Ok(match request {
        // Servie par `bootstrap` : elle ne passe jamais par ici.
        Request::Bootstrap { .. } => {
            return Err("le premier écran ne se décode pas ici".to_owned());
        }
        // Servies par `local_account_work`, qui rend déjà une `Reply` : elles ne passent
        // jamais par ici non plus.
        Request::AccountsDetail
        | Request::DeclareAccount(_)
        | Request::SetSubmission(_)
        | Request::ForgetAccount { .. }
        | Request::Consent(_) => {
            return Err("une demande de compte ne se décode pas ici".to_owned());
        }
        Request::Hello => Reply::Hello(from(result)?),
        Request::Folders => Reply::Folders(from(result)?),
        Request::Page { folder, after, .. } => Reply::Page {
            folder,
            after,
            page: Box::new(from(result)?),
        },
        Request::Open { .. } => {
            // Le découpage du corps se fait **ici**, sur ce fil : voir `Opened`.
            let message: Option<dto::Message> = from(result)?;
            Reply::Open(Box::new(message.map(|message| {
                let body = message
                    .html
                    .as_ref()
                    .map_or_else(Vec::new, |html| mailhtml::blocks::blocks(&html.html));
                Opened {
                    message,
                    body,
                    // Rempli par `interactive`, qui seul voit le début de l'appel.
                    served_ms: 0.0,
                }
            })))
        }
        Request::Thread { .. } => Reply::Thread(Box::new(from(result)?)),
        Request::Source { of } => Reply::Source {
            of,
            found: Box::new(from(result)?),
        },
        Request::Search { .. } => Reply::Search(Box::new(from(result)?)),
        Request::Sources => Reply::Sources(from(result)?),
        // **Un accusé de réception, et non une liste vide.** Rendre `Reply::Jobs(vec![])`
        // faisait écraser la liste des tâches par un vide au moment précis où on venait d'en
        // lancer une : le panneau se vidait, et si l'utilisateur le refermait dans l'intervalle,
        // la relève cessait faute de tâche active visible.
        Request::StartImport { .. } | Request::StartIndex | Request::StartSync { .. } => {
            Reply::Started
        }
        Request::Jobs => Reply::Jobs(from(result)?),
        Request::Accounts => Reply::Accounts(from(result)?),
        // Servie par `Link::stage`, jamais par un appel : elle ne passe pas ici.
        Request::Attach { .. } => {
            return Err("une pièce jointe ne se décode pas ici".to_owned());
        }
        // **Un accusé, pas une valeur.** Le nombre de copies marquées n'intéresse pas
        // l'interface : ce qui l'intéresse est que la liste a changé, et c'est `Changed` qui le
        // dira. Même choix que `Reply::Started` pour les tâches de fond.
        Request::MarkRead { .. } | Request::Decide { .. } | Request::Retry { .. } => {
            Reply::Acknowledged
        }
        Request::Complete { token, .. } => Reply::Complete {
            token,
            found: from(result)?,
        },
        Request::Outbox => Reply::Outbox(from(result)?),
        // La pièce rangée revient comme celle d'un fichier déposé : même type, même chemin
        // d'ajout au brouillon. L'interface n'a donc qu'une façon de joindre.
        Request::StagePart { .. } => Reply::Attached(Box::new(from(result)?)),
        Request::Drafts => Reply::Drafts(from(result)?),
        Request::SaveDraft(_) => {
            // `null` veut dire « il n'y avait rien à garder » : le service a effacé, ou n'a
            // rien créé. Le distinguer d'un identifiant est ce qui évite d'enregistrer en
            // boucle une fenêtre vide.
            let saved: Option<dto::Draft> = from(result)?;
            Reply::Saved(saved.and_then(|it| it.id))
        }
        // Un accusé, pas une valeur : ce qui intéresse l'interface est que la liste a changé,
        // et le `Changed` de l'abonnement le dira.
        Request::DeleteDraft { .. } => Reply::Acknowledged,
        Request::Send(_) => Reply::Queued(Box::new(from(result)?)),
        // Les deux rendent la signature **telle que le store la rend**, et pas celle qui a été
        // envoyée : c'est ce qui fait qu'un document vide revient à `None` sans que l'interface
        // ait à connaître cette règle.
        Request::Signature { account } | Request::SetSignature { account, .. } => {
            Reply::Signature {
                account,
                found: from(result)?,
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Compose, Link};
    use crate::settings::{AccountForm, SubmissionForm};

    /// Un lien vers un démon qui n'existe pas : aucune de ces demandes n'est censée
    /// l'atteindre, et c'est justement ce qu'on vérifie.
    fn remote() -> Link {
        Link::Remote {
            host: "127.0.0.1:1".to_owned(),
            token: "pas-un-vrai-jeton".to_owned(),
            client: None,
        }
    }

    #[test]
    fn a_remote_service_gets_no_account_written_and_is_told_why() {
        // **La propriété qui compte de toute cette page.** Une méthode `accounts.add` ferait
        // écrire dans le trousseau de la machine du démon, et y transporterait le secret. Le
        // refus a lieu avant tout : le `Link` n'a pas de client ouvert, donc si l'une de ces
        // fonctions tentait un appel, le test échouerait par le délai de connexion plutôt que
        // par l'assertion.
        let form = AccountForm {
            host: "imap.exemple.fr".to_owned(),
            username: "marie@exemple.fr".to_owned(),
            secret: "jamais-transmis".to_owned(),
            ..AccountForm::new()
        };

        for refusal in [
            remote().declare_account(&form).unwrap_err(),
            remote()
                .set_submission(&SubmissionForm {
                    account: 1,
                    host: "smtp.exemple.fr".to_owned(),
                    port: String::new(),
                    security: "starttls".to_owned(),
                })
                .unwrap_err(),
            remote().forget_account(1).unwrap_err(),
            remote().accounts_detail().unwrap_err(),
        ] {
            // Le refus nomme le trousseau, donc la raison. « Non disponible » ferait chercher
            // une panne là où il y a une règle.
            assert!(refusal.contains("trousseau"), "refus muet : {refusal}");
            // Et il ne répète jamais ce qu'on lui a donné à écrire.
            assert!(!refusal.contains("jamais-transmis"));
        }
    }

    #[test]
    fn a_form_without_a_server_is_refused_before_anything_is_written() {
        // Le premier refus est celui de la forme, avant le trousseau et avant le store : un
        // compte à moitié déclaré est ce que l'ordre « trousseau d'abord » cherche à éviter.
        let refusal = remote().declare_account(&AccountForm::new()).unwrap_err();
        assert!(refusal.contains("serveur"), "{refusal}");
    }

    #[test]
    fn an_unknown_encryption_mode_is_refused_rather_than_downgraded() {
        // Aucun repli : se tromper de mode rétrograderait le chiffrement à l'insu de
        // l'utilisateur, ce qui est la règle du `CLAUDE.md` sur le serveur de soumission.
        let form = AccountForm {
            host: "imap.exemple.fr".to_owned(),
            username: "marie@exemple.fr".to_owned(),
            security: "aucun".to_owned(),
            ..AccountForm::new()
        };
        let refusal = remote().declare_account(&form).unwrap_err();
        assert!(refusal.contains("tls"), "{refusal}");
    }

    #[test]
    fn a_field_of_addresses_splits_on_commas_and_semicolons() {
        // Le point-virgule parce qu'Outlook sépare ainsi : un utilisateur qui colle une liste
        // venue d'un autre client la collerait telle quelle, et un champ non découpé partirait
        // comme **une seule** adresse — refusée par le démon, sans que la cause soit visible.
        assert_eq!(
            Compose::recipients("jean@x.fr, marie@y.fr; paul@z.fr"),
            vec!["jean@x.fr", "marie@y.fr", "paul@z.fr"]
        );
    }

    #[test]
    fn a_trailing_separator_is_not_an_empty_recipient() {
        // Une virgule finale est une frappe ordinaire — on vient d'ajouter une adresse et on
        // s'apprête à en taper une autre. La garder donnerait une adresse vide, donc un refus
        // du démon sur un formulaire qui a l'air correct.
        assert_eq!(Compose::recipients("jean@x.fr,"), vec!["jean@x.fr"]);
        assert_eq!(Compose::recipients("  ,  ; "), Vec::<String>::new());
        assert_eq!(Compose::recipients(""), Vec::<String>::new());
    }

    #[test]
    fn a_recipient_is_trimmed_but_never_otherwise_touched() {
        // **Le découpage ne valide pas.** La validation est chez le démon, qui écrit la commande
        // SMTP ; en faire une deuxième ici serait deux règles à tenir d'accord. Ce test fige la
        // limite : une adresse hostile passe le découpage **intacte**, et c'est le démon qui la
        // refuse — vérifié par `an_address_that_could_inject_a_command_is_refused…`.
        let hostile = "jean@x.fr\r\nRCPT TO:<ailleurs@y.fr>";
        assert_eq!(
            Compose::recipients(&format!("  {hostile}  ")),
            vec![hostile],
            "le découpage a modifié une adresse au lieu de la transmettre telle quelle"
        );
    }

    #[test]
    fn a_draft_with_only_a_blind_copy_is_sendable() {
        // Un message qui n'a **que** des copies cachées est un cas réel — une annonce à une
        // liste dont les destinataires ne doivent pas se voir. Le refuser serait une politesse
        // imposée.
        let it = Compose {
            bcc: "discret@x.fr".to_owned(),
            ..Compose::default()
        };
        assert!(it.is_sendable());
    }

    #[test]
    fn a_draft_without_any_recipient_is_not_sendable() {
        // Le contrôle négatif. Et le sujet et le corps n'y changent rien : un message sans
        // sujet est valide, un message sans destinataire n'a personne à qui aller.
        let it = Compose {
            subject: "un sujet".to_owned(),
            text: "un corps".to_owned(),
            ..Compose::default()
        };
        assert!(!it.is_sendable());

        let whitespace = Compose {
            to: "   ".to_owned(),
            cc: " , ".to_owned(),
            ..Compose::default()
        };
        assert!(!whitespace.is_sendable());
    }
}

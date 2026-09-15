//! L'assemblage : trois panneaux, un modèle, et rien qui bloque.
//!
//! ## Le modèle est plat, et volontairement
//!
//! Un `Vec<dto::Row>` dense pour la liste, un `Option<dto::Message>` pour le message ouvert,
//! des drapeaux pour ce qui est en vol. Pas de machine à états, pas de graphe réactif : egui
//! reconstruit l'interface à chaque image depuis ce modèle, et une image coûte 1,47 ms de p95
//! sur 20 544 lignes (mesuré). Ce qui rend ça possible est la virtualisation — seules les
//! quarante lignes visibles sont construites, quel que soit le total.
//!
//! ## Ce qui est en vol n'est jamais attendu
//!
//! Toute demande partie vers le service revient par [`crate::worker::Reply`], à l'image
//! suivante ou dans dix images. L'interface dessine ce qu'elle a et marque ce qui manque : une
//! ligne pas encore chargée est **vide**, pas « chargement… » — un mot qui défile est plus
//! agité qu'un blanc, et le blanc dit la même chose.
//!
//! ## La liste est dense, et c'est la pagination du service qui l'impose
//!
//! Les pages s'obtiennent **en séquence** (`docs/ARCHITECTURE.md` : jamais d'`OFFSET`), donc
//! les lignes arrivent dans l'ordre et un `Vec` qu'on étend suffit. La contrepartie est connue
//! et assumée : tirer la barre de défilement au milieu d'un dossier de 20 000 messages montre
//! des lignes vides le temps que les pages intermédiaires arrivent.

use std::time::Instant;

use mailapi::dto;
use mailhtml::blocks::{Block, Kind, Source, Style};

use crate::theme::{self, ROW_HEIGHT};
use crate::worker::{Compose, Reply, Request, Worker};
use crate::{Bench, MARK};
use mailapi::human;

/// Lignes demandées par page. Le défaut du service.
pub const PAGE: u32 = 100;

/// Résultats de recherche demandés.
const RESULTS: u32 = 200;

/// Intervalle de relève des tâches de fond, tant qu'il y en a une active.
const JOBS_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// Au-delà, le banc de démarrage renonce à attendre une ligne : le store est vide.
const STARTUP_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

/// Blocs de corps affichés d'emblée, et par clic sur « afficher la suite ».
///
/// 400 : de quoi lire un mail humain en entier, y compris une longue conversation citée, sans
/// construire les vingt mille blocs qu'un mail pathologique peut produire. Le volet de lecture
/// n'est pas virtualisé — un bloc n'a pas de hauteur fixe — donc c'est ici que la borne se pose.
const BODY_STEP: usize = 400;

/// Images du banc de défilement, au repos puis en défilant.
const IDLE_FRAMES: usize = 120;
const SCROLL_FRAMES: usize = 600;

/// Au-delà, le banc de défilement se contente de ce qu'il a chargé.
///
/// Existe pour le critère 4 : un dossier qu'on est en train de remplir ne se charge jamais en
/// entier, parce que chaque `Changed` relance la pagination. Sur un store au repos — le cas du
/// critère 2 — la pagination des 20 544 lignes du corpus réel finit en quelques secondes, et
/// cette borne ne se déclenche pas.
const LOAD_PATIENCE: std::time::Duration = std::time::Duration::from_secs(25);

/// En dessous, défiler ne mesurerait rien : il faut de quoi remplir plusieurs écrans.
const MIN_SCROLL_ROWS: usize = 500;

/// Le nombre de messages que le banc du critère 5 ouvre.
///
/// Trente, et **trente distincts** : ouvrir deux fois le même mesurerait un cache de blob
/// chaud, donc le meilleur cas plutôt que le cas ordinaire. Trente suffit à un p95 lisible et
/// tient dans quelques secondes.
const OPEN_SAMPLES: usize = 30;

/// Combien de lignes le document du banc de signature porte. Le critère en demande 200.
const SIGNATURE_LINES: usize = 200;

/// Combien d'images le banc de signature relève en frappe.
///
/// 240 : quatre secondes à 60 Hz, assez pour qu'un p95 porte sur des dizaines d'images et pas
/// sur trois. C'est le nombre de la sonde, gardé pour que les deux relevés se comparent.
const SIGNATURE_FRAMES: usize = 240;

/// Lequel des trois champs de destinataires a la main.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    /// « À ».
    To,
    /// « Copie ».
    Cc,
    /// « Copie cachée ».
    Bcc,
}

/// Ce que le lecteur propose d'écrire à partir du message ouvert.
///
/// Trois gestes qui produisent trois messages différents, et les distinguer par un booléen —
/// « répondre à tous, oui ou non » — laissait le transfert sans place. Un `enum` force à traiter
/// les trois là où la décision se prend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Répondre à l'expéditeur seul.
    Sender,
    /// Répondre à l'expéditeur, les autres destinataires en copie.
    Everyone,
    /// Transférer à quelqu'un d'autre, hors du fil.
    Forward,
    /// Ouvrir la source du message : les octets tels qu'ils sont.
    Source,
}

/// Ce que le formulaire de rédaction relève pendant le dessin.
///
/// ## Pourquoi ces cinq champs voyagent ensemble
///
/// Le formulaire reçoit `&self` — il dessine, il ne décide pas — et chacune de ces actions
/// demande `&mut self` pour être exécutée : demander une complétion, ranger un fichier, ouvrir
/// l'éditeur de signature, mettre un message en file. Elles sont donc **relevées** pendant le
/// dessin et consommées après, quand le `Ui` a rendu la main.
///
/// Un seul objet plutôt que cinq paramètres : à huit arguments, l'ordre d'appel devient une
/// chose à vérifier, et deux `&mut bool` voisins s'échangent sans que le compilateur bronche.
#[derive(Default)]
struct Pending {
    /// Le champ qui a le focus, et son contenu — pour la complétion.
    active: Option<(Field, String)>,
    /// Une proposition cliquée, à insérer dans son champ.
    insert: Option<(Field, String)>,
    /// Le rang d'une pièce jointe à retirer.
    detach: Option<usize>,
    /// L'utilisateur a cliqué « Envoyer ».
    send: bool,
    /// L'utilisateur veut modifier sa signature.
    edit_signature: bool,
}

/// L'état de l'application.
pub struct Shell {
    worker: Worker,
    started: Instant,
    bench: Bench,
    announced: bool,

    /// Ce que le service dit de lui-même.
    hello: Option<dto::Hello>,
    /// Un désaccord de version du contrat : irrattrapable, l'interface s'arrête là.
    fatal: Option<String>,

    folders: Vec<dto::Folder>,
    current: Option<i64>,
    /// Les lignes du dossier courant, dans l'ordre, telles qu'elles sont arrivées.
    rows: Vec<dto::Row>,
    cursor: Option<String>,
    exhausted: bool,
    loading_page: bool,
    /// Une page de rafraîchissement de tête est en vol. Voir `merge_head`.
    refreshing_head: bool,

    selected: Option<i64>,
    message: Option<Box<dto::Message>>,
    /// Le corps découpé en blocs, calculé une fois par message et non à chaque image.
    body: Vec<Block>,
    /// Combien de blocs du corps sont affichés. Voir [`BODY_STEP`].
    body_shown: usize,
    opening: bool,
    open_error: Option<String>,
    images_shown: bool,
    /// Le fil du message ouvert, quand il en a un. Vide sinon.
    thread: Vec<dto::Row>,

    query: String,
    /// Les résultats de recherche, ou `None` quand on regarde un dossier.
    results: Option<Vec<dto::Row>>,
    searching: bool,
    focus_search: bool,

    sources: Vec<dto::Source>,
    jobs: Vec<dto::Job>,
    jobs_polled: Option<Instant>,
    show_import: bool,

    /// Le dernier message à montrer à l'utilisateur — erreur d'appel, bilan de tâche.
    status: Option<String>,

    /// La ligne de file que la bannière propose encore d'annuler.
    ///
    /// ## Pourquoi le bouton est **dans** la bannière
    ///
    /// C'est le seul endroit où l'utilisateur regarde à cet instant : il vient de cliquer
    /// « Envoyer », la confirmation apparaît, et c'est en la lisant qu'il réalise qu'il s'est
    /// trompé de destinataire. Un bouton rangé dans le panneau de la file demanderait de
    /// chercher, et la fenêtre de rétractation se compte en secondes.
    ///
    /// Vidé en même temps que `status` : une bannière fermée ne laisse pas un bouton orphelin,
    /// et le panneau de la file reste le chemin de celui qui a fermé trop vite.
    cancellable: Option<i64>,

    /// La panne de transport en cours, s'il y en a une.
    ///
    /// **Critère 9** : ce qui est déjà chargé reste consultable, et l'état dégradé est visible
    /// plutôt que deviné. Rien n'est effacé — une liste qui se vide parce que le réseau a
    /// hoqueté est exactement ce que le critère interdit.
    offline: Option<String>,

    /// L'hôte du démon, en mode distant : la clé du cache de lecture. `None` en embarqué.
    cache_host: Option<String>,
    /// Dernière écriture du cache. Sert à ne pas réécrire à chaque page.
    cache_saved: Option<Instant>,
    /// Vrai après une purge : **plus rien n'est écrit jusqu'au prochain lancement**.
    ///
    /// Sans ce drapeau, le bouton de purge était un aller-retour à vide : le fichier était
    /// effacé, puis réécrit quelques dizaines de millisecondes plus tard par la réponse
    /// suivante du démon. Un bouton qui n'efface que jusqu'à la prochaine page n'efface pas.
    cache_disabled: bool,

    /// Vrai dès que la première ligne a été dessinée : le jalon ne se pose qu'une fois.
    rows_announced: bool,

    /// L'état du banc de mesure, quand l'outillage en demande un.
    bench_state: BenchState,

    /// Les comptes, et lesquels peuvent envoyer.
    ///
    /// Demandés au démarrage. Vides tant que la réponse n'est pas là : le bouton « écrire »
    /// est alors désactivé, ce qui vaut mieux qu'ouvrir une fenêtre sans savoir sous quelle
    /// adresse elle enverrait.
    accounts: Vec<dto::Account>,
    /// La file d'envoi, telle que le démon la voit.
    outbox: Vec<dto::Outgoing>,
    /// Le brouillon en cours, quand la fenêtre de rédaction est ouverte.
    ///
    /// ## Pourquoi un `Option` et pas un `bool` plus un brouillon permanent
    ///
    /// Fermer la fenêtre jette le brouillon, et c'est assumé pour l'instant : les brouillons
    /// persistés sont une fonction en soi — un dossier `Drafts` côté serveur, une
    /// synchronisation, une reprise. Un `Option` dit la vérité sur ce que le programme fait ;
    /// un brouillon gardé en mémoire ferait croire à une persistance qui n'existe pas.
    compose: Option<Compose>,
    /// Un envoi est en vol. Le bouton est désactivé pendant ce temps.
    ///
    /// Sans lui, deux clics rapides mettent **deux** messages en file — et le destinataire en
    /// reçoit deux. C'est le seul doublon de tout le chemin que la file d'envoi ne peut pas
    /// empêcher : elle garantit qu'un message part une fois, pas que l'utilisateur n'en a
    /// demandé qu'un.
    sending: bool,
    /// Les propositions de complétion affichées, et pour quel champ.
    ///
    /// ## Pourquoi le champ fait partie de l'état
    ///
    /// Trois champs de destinataires — À, Copie, Copie cachée — et un seul jeu de propositions.
    /// Sans savoir lequel a la main, la liste s'afficherait sous les trois, et un clic
    /// insérerait dans le mauvais.
    suggestions: Vec<dto::Suggestion>,
    /// Le champ qui a demandé les propositions affichées.
    suggesting: Option<Field>,
    /// Le jeton de la dernière demande de complétion.
    ///
    /// Une réponse qui ne porte pas ce jeton est **jetée** : le fil est sériel, donc une
    /// réponse peut arriver après que l'utilisateur a tapé une lettre de plus, et l'afficher
    /// ferait clignoter la liste entre deux états. Même leçon que le curseur de `Reply::Page`.
    suggest_token: u64,
    /// Le texte pour lequel les propositions affichées ont été demandées.
    ///
    /// Sert à ne pas redemander à chaque image : `egui` redessine soixante fois par seconde, et
    /// une demande par image serait soixante appels par seconde pour un texte qui n'a pas bougé.
    suggest_for: String,
    /// Vrai quand le panneau de la file d'envoi est ouvert.
    show_outbox: bool,

    /// Les signatures connues, par compte. `None` dit « ce compte n'en a pas », et l'absence
    /// de clé dit « on ne sait pas encore » — deux états qu'un seul `Option` confondrait, et
    /// la confusion redemanderait la signature à chaque image pour un compte qui n'en a pas.
    signatures: std::collections::HashMap<i64, Option<mailhtml::rich::Document>>,
    /// Les brouillons connus, du plus récemment touché au plus ancien.
    ///
    /// Relus au démarrage et après chaque écriture : un brouillon enregistré par un autre
    /// client — la CLI, un modèle — doit apparaître sans qu'on clique.
    drafts: Vec<dto::Draft>,
    /// Vrai quand le panneau des brouillons est ouvert.
    show_drafts: bool,
    /// L'éditeur de signature, quand sa fenêtre est ouverte.
    editor: Option<crate::signature::Editor>,
    /// La fenêtre « source du message », quand elle est ouverte.
    source: Option<SourceView>,
    /// La page de paramètres, quand sa fenêtre est ouverte.
    settings: Option<crate::settings::Page>,

    /// Le relevé des deux actions tentées hors ligne — banc du critère 9.
    probe: Probe,
}

/// La fenêtre « source du message ».
///
/// ## Elle garde ce qu'elle a demandé
///
/// `of` est ce qui a été demandé, et la réponse porte la même valeur : une fenêtre rouverte sur
/// une autre cible pendant l'appel jetterait la réponse en retard plutôt que d'afficher les
/// octets d'un message sous le titre d'un autre. C'est la leçon du curseur de `Reply::Page`,
/// et elle compte davantage ici : une vue dont le rôle est de vérifier ne peut pas se tromper
/// de sujet.
#[derive(Debug)]
struct SourceView {
    of: crate::worker::SourceOf,
    /// Comment la nommer, calculé à l'ouverture — le message peut être refermé entre-temps.
    title: String,
    /// La source, quand elle est arrivée.
    found: Option<dto::MessageSource>,
    /// Le refus du service, le cas échéant.
    error: Option<String>,
}

/// Ce que le banc de défilement accumule.
#[derive(Default)]
struct BenchState {
    phase: Phase,
    offset: f32,
    last: Option<Instant>,
    idle: Vec<(f64, f64)>,
    scrolling: Vec<(f64, f64)>,
    /// Changements du store reçus **pendant** la phase de défilement.
    ///
    /// Le contrôle du critère 4 : un zéro veut dire que rien n'écrivait, donc que le relevé
    /// ne dit rien de ce qu'il prétend dire.
    changed_while_scrolling: usize,
    /// Banc du critère 5 : les délais relevés, en millisecondes, et ce qu'on attend.
    opens: Vec<f64>,
    open_at: Option<Instant>,
    open_target: Option<i64>,
    open_index: usize,
    /// Les messages sans corps affichable. Comptés, jamais mélangés aux relevés : rien n'a été
    /// dessiné, donc il n'y a pas de délai d'affichage à mesurer.
    open_empty: usize,
    /// Le nombre de blocs dessinés, cumulé. C'est ce qui empêche un p95 flatteur de venir de
    /// trente messages vides.
    open_blocks: usize,
    /// Ce que le service a coûté, par ouverture. Voir `worker::Opened::served_ms` : ce chiffre
    /// ne dépend pas de la cadence du compositeur, contrairement à `opens`.
    open_served: Vec<f64>,
    /// Le nombre d'images écoulées entre la demande et le dessin.
    ///
    /// C'est ce qui rend le relevé interprétable au lieu de plausible : si le délai vaut deux
    /// images, il suit la cadence de l'écran et non notre travail, et le dire demande de les
    /// avoir comptées.
    open_frames: Vec<f64>,
    /// Les images écoulées depuis la demande en cours.
    open_waited: usize,
    /// Banc du critère 5 de la phase 3 : les images qui suivent le collage.
    after_paste: Vec<(f64, f64)>,
    /// Ce que la conversion du collage a coûté, en millisecondes.
    paste_ms: Option<f64>,
    /// La taille du HTML collé, pour que le chiffre ci-dessus soit interprétable.
    paste_bytes: usize,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    #[default]
    Waiting,
    Idle,
    Scrolling,
    /// Banc du critère 9 : le service est encore là, on attend qu'il tombe.
    WaitingDown,
    /// Banc du critère 9 : les deux actions de sonde sont en vol.
    Probing,
    /// Banc du critère 5 : les ouvertures se succèdent.
    Opening,
    /// Banc du critère 5 de la phase 3 : un caractère par image dans l'éditeur.
    Typing,
    /// Le même banc : l'image du collage de 50 Ko.
    Pasting,
    /// Le même banc : les images qui suivent le collage.
    AfterPaste,
    Done,
}

/// Ce que le banc du critère 9 a constaté des deux actions tentées hors ligne.
///
/// Le critère demande qu'elles « échouent proprement avec un état visible ». Les trois issues
/// se distinguent, et une seule est la bonne :
///
/// - `echec_propre` — le transport a échoué, l'interface l'a dit, rien n'a gelé ;
/// - `refus` — le service a répondu et a dit non : il n'est donc pas tombé, le banc est
///   invalide ;
/// - `servi` — l'action a réussi : le service est toujours là, le banc est invalide.
#[derive(Default)]
struct Probe {
    open: Option<&'static str>,
    search: Option<&'static str>,
}

impl Probe {
    /// Classe un échec selon sa cause.
    const fn verdict(transport: bool) -> &'static str {
        if transport { "echec_propre" } else { "refus" }
    }

    /// Vrai quand les deux sondes ont répondu.
    const fn complete(&self) -> bool {
        self.open.is_some() && self.search.is_some()
    }
}

/// Ce dont l'application a besoin pour démarrer.
///
/// Une structure et non cinq arguments : le compilateur nomme alors ce qu'on oublie, et l'appel
/// se lit sans compter les positions.
pub struct Setup {
    pub worker: Worker,
    pub started: Instant,
    pub bench: Bench,
    /// L'hôte du démon, en mode distant. `None` en embarqué : pas de cache à tenir.
    pub cache_host: Option<String>,
    /// Ce que le cache de lecture avait gardé, s'il y en avait.
    pub seed: Option<crate::cache::Snapshot>,
}

impl Shell {
    /// Monte l'application autour d'un fil déjà démarré.
    pub fn new(setup: Setup) -> Self {
        let Setup {
            worker,
            started,
            bench,
            cache_host,
            seed,
        } = setup;

        // **Le premier écran vient du cache, avant tout réseau.** La réponse du démon écrasera
        // tout ça sans arbitrage ni fusion : le cache n'est jamais une source de vérité, il est
        // ce qu'on affiche en attendant celle-ci.
        let (folders, current, rows) = match seed {
            Some(seed) => (seed.folders, seed.folder, seed.rows),
            None => (Vec::new(), None, Vec::new()),
        };

        Self {
            worker,
            started,
            bench,
            cache_host,
            cache_saved: None,
            cache_disabled: false,
            announced: false,
            hello: None,
            fatal: None,
            folders,
            current,
            rows,
            cursor: None,
            // Le cache ne garde pas de curseur : ses lignes sont un instantané, pas une
            // position dans une pagination. La première page du démon les remplace, et c'est
            // elle qui rétablit le curseur.
            exhausted: false,
            loading_page: false,
            refreshing_head: false,
            selected: None,
            message: None,
            body: Vec::new(),
            body_shown: BODY_STEP,
            opening: false,
            open_error: None,
            images_shown: false,
            thread: Vec::new(),
            query: String::new(),
            results: None,
            searching: false,
            focus_search: false,
            sources: Vec::new(),
            jobs: Vec::new(),
            jobs_polled: None,
            show_import: false,
            status: None,
            cancellable: None,
            offline: None,
            rows_announced: false,
            bench_state: BenchState::default(),
            probe: Probe::default(),
            accounts: Vec::new(),
            outbox: Vec::new(),
            compose: None,
            sending: false,
            suggestions: Vec::new(),
            suggesting: None,
            suggest_token: 0,
            suggest_for: String::new(),
            show_outbox: false,
            signatures: std::collections::HashMap::new(),
            drafts: Vec::new(),
            show_drafts: false,
            editor: None,
            source: None,
            settings: None,
        }
    }

    /// Le dossier courant, s'il est connu.
    fn folder(&self) -> Option<&dto::Folder> {
        let id = self.current?;
        self.folders.iter().find(|it| it.id == id)
    }

    /// Les lignes affichées : les résultats de recherche, ou le dossier.
    fn visible(&self) -> &[dto::Row] {
        self.results.as_deref().unwrap_or(&self.rows)
    }

    /// Le nombre de lignes que la liste doit prévoir.
    ///
    /// Le total du dossier, **connu sans avoir chargé une seule ligne** : `folders.list` rend
    /// déjà les compteurs, donc la barre de défilement est juste dès la première image.
    fn total(&self) -> usize {
        if let Some(results) = &self.results {
            return results.len();
        }
        self.folder()
            .map_or(0, |it| usize::try_from(it.total).unwrap_or(usize::MAX))
    }

    /// Ouvre un dossier : vide la liste et demande la première page.
    fn open_folder(&mut self, id: i64) {
        self.current = Some(id);
        self.results = None;
        self.rows.clear();
        self.cursor = None;
        self.exhausted = false;
        self.selected = None;
        self.message = None;
        self.body.clear();
        self.thread.clear();
        self.open_error = None;
        self.request_page();
    }

    /// Relit le dossier courant **sans rien fermer**.
    ///
    /// La différence avec [`Self::open_folder`] est tout le sujet : ici, l'utilisateur n'a rien
    /// demandé. C'est le store qui a bougé sous lui — un import en cours, un autre client — et
    /// la seule chose à refaire est la liste. Le message ouvert, la sélection et le fil restent
    /// où ils sont, parce que rien de ce que l'utilisateur regarde n'a changé.
    fn reload_folder(&mut self, id: i64) {
        // **Un dossier différent : on repart de zéro.** Rien à conserver, les lignes
        // appartenaient à autre chose.
        if self.current != Some(id) {
            self.current = Some(id);
            self.rows.clear();
            self.cursor = None;
            self.exhausted = false;
            self.request_page();
            return;
        }

        // Le **même** dossier, qui a bougé. Voir `merge_head` : la liste garde ce qu'elle a
        // chargé, et la première page est fusionnée par-dessus.
        self.refreshing_head = true;
        self.worker.ask(Request::Page {
            folder: id,
            after: None,
            limit: PAGE,
        });
    }

    /// Fusionne la première page par-dessus les lignes déjà chargées.
    ///
    /// ## Ce que la remise à zéro coûtait
    ///
    /// La version précédente vidait la liste et repaginait. C'est correct — les lignes
    /// reviennent — mais **la liste s'effondre sous l'utilisateur** : il était à la
    /// cinq-millième ligne, il se retrouve avec cent lignes et une position de défilement qui
    /// pointe au-delà de la fin.
    ///
    /// Ce n'est pas théorique. Le banc du critère 4 l'a rendu visible : pendant une
    /// synchronisation qui émet une soixantaine de changements en vingt secondes, la liste
    /// n'accumule **jamais** plus de quelques pages. Le relevé de défilement portait sur
    /// 200 lignes au lieu de 20 000, et c'est en cherchant pourquoi que le défaut est sorti.
    ///
    /// C'est le frère du bug corrigé le 2026-09-03 — `Changed` fermait le message ouvert. La
    /// relecture avait trouvé celui-là et pas celui-ci.
    ///
    /// ## Pourquoi une fusion en tête suffit
    ///
    /// La liste est triée par `(date DESC, id DESC)`. Un message qui arrive est plus récent
    /// que ceux qu'on a, donc il se place **en tête** : recharger la première page et la
    /// fusionner par-dessus attrape tout le nouveau courrier sans toucher au reste.
    ///
    /// Ce que ça ne rattrape pas : un message **supprimé** au milieu de ce qui est déjà
    /// chargé, et un message ancien qui arriverait après coup — un import qui remplit le
    /// passé. Les deux se corrigent au prochain vrai rechargement, et aucun des deux ne
    /// justifie de faire s'effondrer la liste à chaque lot de synchronisation.
    fn merge_head(&mut self, rows: Vec<dto::Row>, next: Option<String>) {
        if rows.is_empty() {
            return;
        }
        // Les identifiants de la page fraîche : ce qu'on retire d'en dessous pour ne pas
        // afficher deux fois la même ligne.
        let fresh: std::collections::HashSet<i64> = rows.iter().map(|it| it.id).collect();
        let kept: Vec<dto::Row> = std::mem::take(&mut self.rows)
            .into_iter()
            .filter(|it| !fresh.contains(&it.id))
            .collect();

        let had_rows = !kept.is_empty();
        self.rows = rows;
        self.rows.extend(kept);

        // Le curseur ne bouge que si la liste était vide : sinon il pointe déjà plus loin que
        // la page qu'on vient de fusionner, et le remplacer ferait repaginer depuis le haut.
        if !had_rows {
            self.cursor = next.clone();
            self.exhausted = next.is_none();
        }
        self.save_cache();
    }

    /// Demande la page suivante, si elle a un sens.
    fn request_page(&mut self) {
        let Some(folder) = self.current else {
            return;
        };
        if self.loading_page || self.exhausted || self.results.is_some() {
            return;
        }
        self.loading_page = true;
        self.worker.ask(Request::Page {
            folder,
            after: self.cursor.clone(),
            limit: PAGE,
        });
    }

    /// Ouvre un message.
    fn open_message(&mut self, id: i64, with_images: bool) {
        self.selected = Some(id);
        self.opening = true;
        self.open_error = None;
        self.images_shown = with_images;
        self.worker.ask(Request::Open {
            id,
            remote_images: with_images,
        });

        // **Marquer lu, dans le dossier ouvert.** Les drapeaux appartiennent à la référence,
        // pas au message : le même contenu peut être non lu ailleurs, et c'est celui qu'on
        // regarde qui devient lu.
        //
        // Une recherche n'a pas de dossier courant, et alors rien n'est marqué. C'est le bon
        // choix par défaut : un résultat de recherche est survolé, pas forcément lu — et
        // deviner le dossier parmi ceux qui contiennent le message serait choisir à la place
        // de l'utilisateur.
        //
        // L'appel est **inconditionnel** : la coquille ne sait pas si le message était déjà lu,
        // et `Store::mark_seen` est idempotent — un message déjà lu ne produit aucune écriture.
        // Filtrer ici sur la ligne de liste demanderait de la retrouver, et la ligne peut ne
        // pas être chargée.
        if let Some(folder) = self.current {
            self.worker.ask(Request::MarkRead { id, folder });
        }
    }

    /// Lance la recherche saisie.
    fn run_search(&mut self) {
        let query = self.query.trim().to_owned();
        if query.is_empty() {
            self.results = None;
            return;
        }
        self.searching = true;
        self.worker.ask(Request::Search {
            query,
            limit: RESULTS,
        });
    }

    /// Traite les réponses arrivées depuis la dernière image.
    fn absorb(&mut self) {
        for reply in self.worker.drain() {
            match reply {
                Reply::Bootstrap {
                    folders,
                    folder,
                    page,
                } => {
                    // Le premier écran arrive d'un bloc : dossiers, dossier ouvert, première
                    // page. Rien à enchaîner, donc rien à attendre une image de plus.
                    self.folders = folders;
                    self.current = Some(folder);
                    self.rows = page.rows;
                    self.cursor = page.next.clone();
                    self.exhausted = page.next.is_none();
                    // Le premier écran du démon est exactement ce qu'il faut garder pour le
                    // prochain démarrage.
                    self.save_cache();
                }
                Reply::Hello(hello) => {
                    // Le contrat est partagé en Rust, donc un champ renommé casse à la
                    // compilation. Le numéro reste vérifié : un binaire de coquille et un
                    // service de versions différentes peuvent se croiser sur le même store.
                    if hello.protocol == mailapi::PROTOCOL {
                        self.hello = Some(hello);
                    } else {
                        self.fatal = Some(format!(
                            "cette coquille parle le protocole {}, le service parle {}. \
                             Mettre les deux à jour ensemble.",
                            mailapi::PROTOCOL,
                            hello.protocol
                        ));
                    }
                }
                Reply::Folders(folders) => {
                    self.folders = folders;
                    // À la première liste, ouvrir la boîte de réception : sans ça, la
                    // première image serait un panneau de dossiers et un vide.
                    if self.current.is_none() {
                        let chosen = self
                            .folders
                            .iter()
                            .find(|it| it.kind == "inbox")
                            .or_else(|| self.folders.first())
                            .map(|it| it.id);
                        if let Some(id) = chosen {
                            self.open_folder(id);
                        }
                    }
                }
                Reply::Page {
                    folder,
                    after,
                    page,
                } => {
                    // Une page de rafraîchissement de tête : `after` est vide et on l'avait
                    // demandée. Elle ne suit pas le curseur, donc elle se fusionne au lieu de
                    // s'ajouter.
                    if self.refreshing_head && after.is_none() && self.current == Some(folder) {
                        self.refreshing_head = false;
                        self.merge_head(page.rows, page.next);
                        continue;
                    }

                    self.loading_page = false;
                    // **Le dossier et le curseur doivent tous deux correspondre.** Une page
                    // d'un autre dossier est une réponse en retard, et une page du bon dossier
                    // mais d'un autre curseur l'est aussi — c'est ce second cas qui trouait la
                    // liste quand un import rechargeait le dossier en cours de pagination.
                    if self.current == Some(folder) && self.cursor == after {
                        self.rows.extend(page.rows);
                        self.cursor = page.next.clone();
                        self.exhausted = page.next.is_none();
                        self.save_cache();
                    } else {
                        tracing::debug!(
                            dossier = folder,
                            "page en retard jetée : la liste a changé entre-temps"
                        );
                    }
                }
                Reply::Open(message) => {
                    self.opening = false;
                    if self.bench_state.phase == Phase::Probing {
                        self.probe.open = Some("servi");
                    }
                    match *message {
                        Some(opened) => {
                            if self.bench == Bench::Open {
                                self.bench_state.open_served.push(opened.served_ms);
                            }
                            // Le corps arrive **déjà découpé** : l'analyse a eu lieu sur le fil
                            // d'API, hors du chemin du dessin. Voir `worker::Opened`.
                            self.body = opened.body;
                            // Un nouveau message repart du plafond : « afficher la suite »
                            // vaut pour le message qu'on lisait, pas pour le suivant.
                            self.body_shown = BODY_STEP;
                            // Le fil se demande seulement quand le message en a un : la passe
                            // de threading est dérivée et peut ne pas avoir tourné.
                            self.thread.clear();
                            if opened.message.thread.is_some() {
                                self.worker.ask(Request::Thread {
                                    id: opened.message.row.id,
                                });
                            }
                            self.message = Some(Box::new(opened.message));
                        }
                        None => {
                            self.message = None;
                            self.body.clear();
                            self.thread.clear();
                            self.open_error =
                                Some("ce message n'existe plus dans le store".to_owned());
                        }
                    }
                }
                Reply::Thread(thread) => self.thread = thread.rows,
                Reply::AccountsDetail(found) => {
                    if let Some(page) = &mut self.settings {
                        page.accounts = found;
                    }
                }
                Reply::ConsentUrl(url) => {
                    if let Some(page) = &mut self.settings {
                        // Le formulaire cède la place à l'attente : garder les deux ferait
                        // croire qu'il reste quelque chose à remplir.
                        page.consenting = None;
                        page.consent_url = Some(url);
                    }
                }
                Reply::AccountWritten(said) => {
                    if let Some(page) = &mut self.settings {
                        page.message = Some(said);
                        page.failed = false;
                        page.consenting = None;
                        page.consent_url = None;
                        // Les formulaires se referment sur un succès, et **seulement** sur un
                        // succès : sur un refus, ce qui a été tapé doit rester à l'écran pour
                        // être corrigé. C'est la règle du brouillon qu'un envoi refusé ne ferme
                        // pas.
                        page.declaring = None;
                        page.submitting = None;
                    }
                    // L'écriture a changé les comptes : les relire tout de suite, ici et pour
                    // le reste de l'interface — la fenêtre de rédaction lit `can_send`.
                    self.worker.ask(Request::AccountsDetail);
                    self.worker.ask(Request::Accounts);
                }
                Reply::Source { of, found } => {
                    // Une réponse qui ne correspond plus à ce que la fenêtre demande est jetée,
                    // pas affichée : voir `SourceView`.
                    if let Some(view) = &mut self.source
                        && view.of == of
                    {
                        view.found = *found;
                        if view.found.is_none() {
                            view.error =
                                Some("introuvable, ou son contenu manque du magasin".to_owned());
                        }
                    }
                }
                Reply::Search(results) => {
                    self.searching = false;
                    if self.bench_state.phase == Phase::Probing {
                        self.probe.search = Some("servi");
                    }
                    if results.search_available {
                        self.results = Some(results.rows);
                    } else {
                        self.results = Some(Vec::new());
                        self.status = Some(
                            "l'index plein texte est absent : `mail index` le construit".to_owned(),
                        );
                    }
                }
                Reply::Sources(sources) => self.sources = sources,
                Reply::Jobs(jobs) => self.jobs = jobs,
                Reply::Accounts(accounts) => self.accounts = accounts,
                Reply::Attached(attached) => {
                    // Le fichier est rangé : la fenêtre peut le joindre. Elle n'est peut-être
                    // plus ouverte — l'utilisateur a pu la fermer entre le dépôt et la
                    // réponse — et alors il n'y a rien à faire, sinon un blob orphelin que
                    // `mail doctor` compte.
                    if let Some(draft) = self.compose.as_mut() {
                        draft.attachments.push(*attached);
                    }
                }
                // Une écriture est passée. Le `Changed` de l'abonnement suivra et rechargera
                // ce qu'il faut ; il n'y a rien à faire ici.
                Reply::Acknowledged => {}
                Reply::Complete { token, found } => {
                    // Une réponse en retard est jetée : voir `suggest_token`.
                    if token == self.suggest_token {
                        self.suggestions = found;
                    }
                }
                Reply::Outbox(outbox) => self.outbox = outbox,
                Reply::Drafts(drafts) => self.drafts = drafts,
                Reply::Saved(id) => {
                    // L'identifiant revient dans le brouillon ouvert, s'il l'est encore : sans
                    // lui, l'enregistrement suivant créerait une deuxième ligne.
                    if let Some(compose) = self.compose.as_mut() {
                        compose.draft = id;
                    }
                    self.worker.ask(Request::Drafts);
                }
                Reply::Signature { account, found } => {
                    // La réponse d'un enregistrement passe par ici aussi : l'éditeur se ferme
                    // sur ce qui a été **rangé**, pas sur ce qui a été envoyé. Un document vide
                    // revient donc à « pas de signature » sans que cette fenêtre connaisse la
                    // règle.
                    self.signatures.insert(account, found);
                    if let Some(editor) = self.editor.as_ref()
                        && editor.account == account
                        && editor.saving
                    {
                        self.editor = None;
                        self.status = Some("Signature enregistrée.".to_owned());
                    }
                }
                Reply::Queued(queued) => {
                    // Le message est en file, pas parti. Le dire ainsi : « envoyé » serait
                    // faux tant que le facteur n'a pas eu la réponse du serveur, et c'est
                    // exactement la confusion que le critère 2 cherche à éviter chez
                    // l'utilisateur autant que dans le code.
                    self.sending = false;
                    // Le brouillon de ce message n'a plus de raison d'être : il est en file, et
                    // le laisser ferait réapparaître dans les brouillons un message qui est
                    // parti. Supprimé **après** la mise en file et jamais avant : si l'envoi
                    // avait été refusé, c'est le brouillon qui aurait sauvé la frappe.
                    if let Some(id) = self.compose.as_ref().and_then(|it| it.draft) {
                        self.worker.ask(Request::DeleteDraft { id });
                    }
                    self.compose = None;
                    // **Le délai vient du service, jamais d'une constante d'ici.** Écrire
                    // « 10 secondes » dans la phrase ferait mentir l'interface le jour où le
                    // maintien change, et un client distant peut parler à un démon d'une autre
                    // version que la sienne.
                    self.status = Some(if queued.hold > 0 {
                        format!(
                            "Message en file d'envoi ({} destinataire(s), {} octets). Il partira \
                             dans {} s, même si vous fermez la fenêtre.",
                            queued.recipients.len(),
                            queued.size,
                            queued.hold
                        )
                    } else {
                        format!(
                            "Message en file d'envoi ({} destinataire(s), {} octets). Il partira \
                             même si vous fermez la fenêtre.",
                            queued.recipients.len(),
                            queued.size
                        )
                    });
                    self.cancellable = (queued.hold > 0).then_some(queued.id);
                    self.worker.ask(Request::Outbox);
                    self.worker.ask(Request::Drafts);
                }
                Reply::Cancelled(cancelled) => {
                    // Les deux phrases disent ce qui s'est passé, pas ce qui était demandé. Un
                    // « trop tard » est un cas normal de la fenêtre de rétractation : le
                    // facteur avait déjà la ligne, et le message est parti pour de bon.
                    self.status = Some(if cancelled {
                        "Envoi annulé : le message ne partira pas.".to_owned()
                    } else {
                        "Trop tard : le message était déjà en cours de remise.".to_owned()
                    });
                    self.cancellable = None;
                    self.worker.ask(Request::Outbox);
                }
                Reply::Started => {
                    // La tâche est en file. Forcer la relève tout de suite plutôt que d'attendre
                    // l'intervalle : c'est le moment où l'utilisateur regarde le panneau.
                    self.jobs_polled = None;
                    self.worker.ask(Request::Jobs);
                }
                Reply::Changed => {
                    // **Compté pendant le défilement, et c'est un contrôle du banc.** Le
                    // critère 4 de `docs/PHASE-2.md` mesure le travail par image *pendant*
                    // qu'une synchronisation écrit. Sans ce compteur, un relevé pris après la
                    // fin de la moisson serait indiscernable d'un vrai — et il mesurerait le
                    // critère 2 sous un autre nom.
                    if self.bench_state.phase == Phase::Scrolling {
                        self.bench_state.changed_while_scrolling += 1;
                    }
                    // Le store a bougé — un import, une indexation, un autre client. Les
                    // compteurs et la liste courante sont à relire.
                    //
                    // **Sans toucher au message ouvert.** Un import émet un changement à
                    // chaque lot de 5 000 messages : `open_folder` remettait alors le volet de
                    // lecture à « aucun message sélectionné », une quinzaine de fois pendant un
                    // import du corpus réel. Ce qu'on lit ne dépend pas de ce qui s'écrit.
                    self.worker.ask(Request::Folders);
                    // **La file aussi.** Sans ça, le badge « File (1) » restait affiché après
                    // qu'un message était parti : le facteur travaille en fond, l'écriture bouge
                    // la révision du store, et l'abonnement le dit déjà — il suffisait de
                    // l'écouter. Un compteur qui ne se met à jour qu'au clic est un compteur qui
                    // ment.
                    self.worker.ask(Request::Outbox);
                    // Et les brouillons : un autre client — la CLI, un modèle — peut en avoir
                    // écrit un, et le compteur doit le dire sans qu'on clique.
                    self.worker.ask(Request::Drafts);
                    if let Some(id) = self.current {
                        self.reload_folder(id);
                    }
                }
                Reply::Online => {
                    self.offline = None;
                }
                Reply::Failed {
                    what,
                    message,
                    transport,
                } => {
                    // **Critère 9.** Une panne de transport met l'interface en mode dégradé
                    // visible et n'efface rien de ce qu'elle a ; un refus du service est une
                    // erreur de la demande, affichée comme telle.
                    if transport {
                        self.offline = Some(message.clone());
                    }
                    if self.bench_state.phase == Phase::Probing {
                        match what {
                            "message" => self.probe.open = Some(Probe::verdict(transport)),
                            "recherche" => self.probe.search = Some(Probe::verdict(transport)),
                            _ => {}
                        }
                    }
                    if what == "page" {
                        self.loading_page = false;
                    }
                    if what == "message" {
                        self.opening = false;
                        self.open_error = Some(message.clone());
                    }
                    if what == "recherche" {
                        self.searching = false;
                    }
                    if (what == "compte" || what == "comptes")
                        && let Some(page) = &mut self.settings
                    {
                        // Dans la page, et en couleur d'alerte : un refus affiché seulement
                        // dans la barre d'état passe inaperçu quand on regarde un formulaire.
                        page.message = Some(message.clone());
                        page.failed = true;
                        // L'attente du navigateur s'arrête aussi : un consentement refusé ou
                        // expiré laisserait sinon un écran « en cours » pour toujours, et
                        // aucun bouton pour recommencer.
                        page.consent_url = None;
                    }
                    if what == "source"
                        && let Some(view) = &mut self.source
                    {
                        // Le refus est affiché **dans la fenêtre**, et pas seulement dans la
                        // barre d'état : une fenêtre qui reste vide sans rien dire ferait
                        // croire à un message sans en-têtes.
                        view.error = Some(message.clone());
                    }
                    if what == "envoi" {
                        // **Le brouillon n'est pas fermé.** Un refus veut dire que le message
                        // n'est pas en file — une adresse mal écrite, un compte sans serveur
                        // d'envoi — et jeter ce que l'utilisateur vient d'écrire serait le
                        // punir d'une faute de frappe.
                        self.sending = false;
                    }
                    self.status = Some(format!("{what} : {message}"));
                }
            }
        }
    }

    /// Écrit le cache de lecture, si le mode le demande et si l'écriture précédente est loin.
    ///
    /// **Bornée dans le temps**, pas à chaque page : charger un dossier de 20 000 messages
    /// déclencherait 205 écritures d'un fichier de cent kilo-octets pour un seul résultat utile.
    fn save_cache(&mut self) {
        /// Intervalle minimal entre deux écritures.
        const EVERY: std::time::Duration = std::time::Duration::from_secs(5);

        let Some(host) = self.cache_host.clone() else {
            return;
        };
        // Une purge vaut pour toute la session : voir `cache_disabled`.
        if self.cache_disabled {
            return;
        }
        if self.cache_saved.is_some_and(|last| last.elapsed() < EVERY) {
            return;
        }

        let revision = self
            .hello
            .as_ref()
            .map(|it| it.revision.clone())
            .unwrap_or_default();
        let snapshot =
            crate::cache::Snapshot::new(revision, self.folders.clone(), self.current, &self.rows);
        if !snapshot.worth_writing() {
            return;
        }
        self.cache_saved = Some(Instant::now());
        crate::cache::save(&host, snapshot);
    }

    /// Relève les tâches de fond quand il y en a d'actives.
    ///
    /// Une minuterie ici, contrairement à l'abonnement : la révision du store ne change pas à
    /// chaque page importée, et une barre de progression qui n'avance qu'à la fin ne sert à
    /// rien.
    fn poll_jobs(&mut self, ctx: &egui::Context) {
        let active = self
            .jobs
            .iter()
            .any(|job| job.state == "running" || job.state == "queued");
        if !active && !self.show_import {
            return;
        }
        let due = self
            .jobs_polled
            .is_none_or(|last| last.elapsed() >= JOBS_POLL);
        if due {
            self.jobs_polled = Some(Instant::now());
            self.worker.ask(Request::Jobs);
        }
        ctx.request_repaint_after(JOBS_POLL);
    }
}

impl eframe::App for Shell {
    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let now = Instant::now();
        let delta = self
            .bench_state
            .last
            .map(|last| now.duration_since(last).as_secs_f64() * 1000.0);
        self.bench_state.last = Some(now);
        let work = frame.info().cpu_usage.map(|it| f64::from(it) * 1000.0);

        self.absorb();
        self.poll_jobs(&ctx);

        if let Some(message) = self.fatal.clone() {
            fatal(root, &message);
        } else {
            self.panes(root);
            // Après les panneaux : une fenêtre egui se dessine au-dessus, et l'ordre d'appel
            // décide de qui reçoit le clic.
            self.compose_window(&ctx);
            self.signature_window(&ctx);
            self.source_window(&ctx);
            self.settings_window(&ctx);
            self.keyboard(&ctx);
        }

        // **Deux jalons, et il faut les deux.** La première image est le moment où la fenêtre
        // répond ; les premières lignes sont le moment où elle sert à quelque chose. Les
        // demandes partent avant la création de la fenêtre, donc l'écart est petit — mais il
        // n'est pas nul, et le mesurer vaut mieux que choisir celui qui flatte.
        if !self.announced {
            self.announced = true;
            tracing::info!(
                "{MARK} coquille etape=premiere_image ms={:.1}",
                self.started.elapsed().as_secs_f64() * 1000.0
            );
            // La police de repli — CJK — se charge maintenant, hors du chemin de démarrage.
            theme::spawn_fallback(&ctx);

            // En mesure, la fenêtre est mise devant. **Ce n'est pas de la triche, c'est une
            // condition de validité** : le compositeur bride la cadence d'une fenêtre qui n'a
            // pas le premier plan, et les sondes du 2026-09-02 ont relevé 55 Hz puis 32 Hz sur
            // le même code. Sans ça, l'écart entre la première image et les premières lignes
            // se mesure en images bridées, pas en travail.
            if self.bench != Bench::None {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        if !self.rows_announced && !self.rows.is_empty() {
            self.rows_announced = true;
            tracing::info!(
                "{MARK} coquille etape=premieres_lignes ms={:.1} lignes={}",
                self.started.elapsed().as_secs_f64() * 1000.0,
                self.rows.len()
            );
        }

        match self.bench {
            // Le banc de démarrage se referme sur les **premières lignes**, pas sur la
            // première image : c'est le jalon comparable au `paint` de la coquille Tauri, qui
            // dessinait déjà des lignes venues de son cache.
            Bench::Startup if self.rows_announced => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            // Un store vide n'aura jamais de ligne. Ne pas rester ouvert pour autant : sans
            // cette sortie, l'outillage attendrait sa patience entière pour rien.
            Bench::Startup if self.started.elapsed() > STARTUP_PATIENCE => {
                tracing::warn!("{MARK} coquille diag aucune ligne : store vide ?");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Bench::Startup => ctx.request_repaint(),
            Bench::Scroll => self.advance_bench(&ctx, delta, work),
            Bench::Offline => self.advance_offline(&ctx, delta, work),
            Bench::Open => self.advance_open(&ctx),
            Bench::Signature => self.advance_signature(&ctx, delta, work),
            Bench::None => {}
        }
    }
}

impl Shell {
    /// Les trois panneaux.
    fn panes(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("barre").show(root, |ui| self.top_bar(ui));
        egui::Panel::left("dossiers")
            .exact_size(260.0)
            .show(root, |ui| self.folders_pane(ui));
        egui::Panel::right("lecture")
            .exact_size((root.available_width() * 0.45).clamp(320.0, 900.0))
            .show(root, |ui| self.reader_pane(ui));
        egui::CentralPanel::default().show(root, |ui| self.list_pane(ui));
    }

    /// La barre du haut : recherche, import, état.
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(30.0);

            let field = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .desired_width(360.0)
                    .hint_text("Rechercher — facture, from:banque, \"phrase exacte\""),
            );
            if std::mem::take(&mut self.focus_search) {
                field.request_focus();
            }
            if field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                self.run_search();
            }
            if ui.button("Chercher").clicked() {
                self.run_search();
            }
            if self.results.is_some() && ui.button("Effacer").clicked() {
                self.results = None;
                self.query.clear();
            }

            ui.separator();
            // « Écrire » d'abord : c'est l'action, l'import est une opération de mise en route.
            // Désactivé quand aucun compte ne peut envoyer, plutôt qu'ouvrant un formulaire
            // dont le bouton refuserait sans dire pourquoi — critère 8.
            let can_send = self.default_sender().is_some();
            if ui
                .add_enabled(can_send, egui::Button::new("✉ Écrire"))
                .on_disabled_hover_text(
                    "Aucun compte n'a de serveur d'envoi configuré. Voir `mail account \n                     submission`.",
                )
                .clicked()
            {
                self.start_compose();
            }
            let waiting = self
                .outbox
                .iter()
                .filter(|it| it.state == "queued" || it.state == "sending")
                .count();
            let doubtful = self.outbox.iter().filter(|it| it.doubtful).count();
            let label = match (waiting, doubtful) {
                // Le doute passe devant le compte en attente : c'est le seul état qui demande
                // une décision, et il ne doit pas se noyer dans un nombre.
                (_, 1..) => format!("File ⚠ {doubtful}"),
                (0, 0) => "File".to_owned(),
                (n, 0) => format!("File ({n})"),
            };
            if ui.selectable_label(self.show_outbox, label).clicked() {
                self.show_outbox = !self.show_outbox;
                if self.show_outbox {
                    self.worker.ask(Request::Outbox);
                }
            }
            // Les brouillons à côté de la file : les deux répondent à « qu'est-ce qui n'est pas
            // encore parti ? », et les chercher dans deux endroits éloignés serait absurde.
            let label = if self.drafts.is_empty() {
                "Brouillons".to_owned()
            } else {
                format!("Brouillons ({})", self.drafts.len())
            };
            if ui.selectable_label(self.show_drafts, label).clicked() {
                self.show_drafts = !self.show_drafts;
                if self.show_drafts {
                    self.worker.ask(Request::Drafts);
                }
            }
            if ui.selectable_label(self.show_import, "Importer…").clicked() {
                self.show_import = !self.show_import;
                if self.show_import {
                    self.worker.ask(Request::Sources);
                    self.worker.ask(Request::Jobs);
                }
            }
            if ui
                .selectable_label(self.settings.is_some(), "⚙ Paramètres")
                .on_hover_text("Les comptes : par où ils lisent, par où ils envoient")
                .clicked()
            {
                if self.settings.is_some() {
                    self.settings = None;
                } else {
                    self.open_settings();
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(hello) = &self.hello {
                    ui.weak(format!(
                        "{} messages{}",
                        hello.messages,
                        if hello.search_available {
                            ""
                        } else {
                            " — sans index"
                        }
                    ));
                }
                if self.searching || self.loading_page || self.opening {
                    ui.spinner();
                }
            });
        });

        // **Le bandeau du critère 9.** Il dit ce qui marche encore et ce qui ne marche plus :
        // « rien ne gèle et rien ne ment ». Il ne propose pas de réessayer, parce que
        // l'abonnement le fait déjà tout seul, toutes les deux secondes.
        if let Some(reason) = self.offline.clone() {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(ui.visuals().warn_fg_color, "⚠ Service injoignable.");
                ui.label(
                    "La liste déjà chargée reste consultable ; ouvrir un message et chercher \
                     ne fonctionneront pas.",
                );
                ui.weak(reason);
            });
        }

        if self.show_outbox {
            self.outbox_pane(ui);
        }
        if self.show_drafts {
            self.drafts_pane(ui);
        }
        if self.show_import {
            self.import_pane(ui);
        }
        if let Some(status) = self.status.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, status);
                if let Some(id) = self.cancellable
                    && ui
                        .button("Annuler l'envoi")
                        .on_hover_text(
                            "Retire le message de la file. Ne marche que tant que le facteur \
                             ne l'a pas pris.",
                        )
                        .clicked()
                {
                    // La bannière ne décide de rien : elle demande, et c'est le store qui
                    // tranche. Le bouton disparaît tout de suite pour qu'un second clic ne
                    // parte pas pendant que le premier voyage.
                    self.cancellable = None;
                    self.worker.ask(Request::Cancel { id });
                }
                if ui.small_button("×").clicked() {
                    self.status = None;
                    self.cancellable = None;
                }
            });
        }
    }

    /// Le panneau des brouillons : ce qui n'est pas encore parti et qu'on n'a pas fini.
    ///
    /// ## Rouvrir remet le formulaire dans l'état où il était
    ///
    /// Adresses telles que tapées, fragment inclus, et le même identifiant de brouillon — donc
    /// le prochain enregistrement met à jour la même ligne au lieu d'en créer une deuxième.
    ///
    /// ## Une seule fenêtre de rédaction à la fois
    ///
    /// Rouvrir un brouillon alors qu'on écrit déjà **enregistre** ce qui est en cours avant de
    /// le remplacer. Sans ça, le clic perdrait la frappe — et un panneau qui fait perdre du
    /// texte est pire qu'un panneau absent.
    fn drafts_pane(&mut self, ui: &mut egui::Ui) {
        let mut to_open: Option<dto::Draft> = None;
        let mut to_delete: Option<i64> = None;

        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("Brouillons");
            ui.weak("— enregistrés à la fermeture de la fenêtre, et jamais envoyés d'eux-mêmes.");
        });

        if self.drafts.is_empty() {
            ui.weak("Aucun brouillon.");
            return;
        }

        egui::ScrollArea::vertical()
            .id_salt("brouillons")
            .max_height(DRAFTS_PANE)
            .show(ui, |ui| {
                for draft in &self.drafts {
                    ui.horizontal_wrapped(|ui| {
                        // L'étiquette vient du service : un brouillon sans sujet est le cas
                        // ordinaire, et trois clients qui choisiraient chacun leur repli
                        // afficheraient trois listes différentes.
                        if ui
                            .button(elide(&draft.label, 70))
                            .on_hover_text("Rouvrir ce brouillon")
                            .clicked()
                        {
                            to_open = Some(draft.clone());
                        }
                        ui.weak(short_date(draft.updated_at));
                        if !draft.attachments.is_empty() {
                            ui.weak(format!("📎 {}", draft.attachments.len()));
                        }
                        if ui
                            .small_button("×")
                            .on_hover_text("Jeter ce brouillon")
                            .clicked()
                        {
                            to_delete = draft.id;
                        }
                    });
                }
            });

        if let Some(draft) = to_open {
            // Ce qui est en cours d'écriture est enregistré avant d'être remplacé.
            if let Some(current) = self.compose.clone() {
                self.worker.ask(Request::SaveDraft(Box::new(current)));
            }
            self.compose = Some(Compose {
                account: draft.account,
                to: draft.to,
                cc: draft.cc,
                bcc: draft.bcc,
                subject: draft.subject,
                text: draft.body,
                in_reply_to: draft.in_reply_to,
                references: draft.references,
                sign: draft.sign,
                attachments: draft.attachments,
                draft: draft.id,
            });
            self.show_drafts = false;
        }
        if let Some(id) = to_delete {
            // Si c'est celui qui est ouvert, la fenêtre perd son identifiant : sa fermeture
            // écrira alors une ligne neuve plutôt que de ressusciter celle qu'on vient de
            // jeter.
            if let Some(compose) = self.compose.as_mut()
                && compose.draft == Some(id)
            {
                compose.draft = None;
            }
            self.worker.ask(Request::DeleteDraft { id });
            self.worker.ask(Request::Drafts);
        }
    }
    /// Le compte sous lequel écrire, par défaut.
    ///
    /// Le premier qui peut envoyer. Pas le premier de la liste : ouvrir la fenêtre sur un
    /// compte qui n'a pas de serveur d'envoi ferait un formulaire dont le bouton est
    /// désactivé, sans dire pourquoi.
    fn default_sender(&self) -> Option<i64> {
        self.accounts
            .iter()
            .find(|it| it.can_send && it.enabled)
            .map(|it| it.id)
    }

    /// Ouvre la fenêtre de rédaction, vide.
    fn start_compose(&mut self) {
        let Some(account) = self.default_sender() else {
            self.status = Some(
                "Aucun compte ne peut envoyer. `mail account submission --account N --host …` \
                 pour en configurer un."
                    .to_owned(),
            );
            return;
        };
        self.compose = Some(Compose {
            account,
            ..Compose::default()
        });
    }

    /// Ouvre une réponse au message ouvert, à l'expéditeur seul ou à tout le monde.
    ///
    /// ## Le fil est repris depuis les en-têtes du message, pas reconstruit
    ///
    /// `In-Reply-To` prend le `Message-ID` du message auquel on répond, et `References`
    /// **prolonge** la chaîne existante — RFC 5322 §3.6.4. La reconstruire depuis le fil local
    /// donnerait une chaîne différente de celle des autres clients, et un lecteur qui regroupe
    /// sur `References` verrait deux fils au lieu d'un.
    ///
    /// Un message sans `Message-ID` — ça existe, le corpus en a — donne une réponse sans
    /// `In-Reply-To` plutôt qu'un en-tête inventé.
    ///
    /// ## « À tous » veut dire « tous sauf soi »
    ///
    /// Se mettre en copie de sa propre réponse est le défaut le plus visible d'un « répondre à
    /// tous » écrit vite : le message revient dans sa propre boîte, et sur un fil de dix
    /// échanges il y revient dix fois. Les adresses des comptes connus sont donc retirées de la
    /// copie — c'est la seule chose que la coquille sait de « soi ».
    ///
    /// ## Le message d'origine est cité
    ///
    /// Sans citation, le destinataire d'une réponse ne sait pas à quoi elle répond — surtout si
    /// elle arrive trois jours plus tard. La citation est du texte, préfixé de `> `, comme tous
    /// les clients depuis trente ans : elle se relit dans n'importe quel lecteur, y compris un
    /// lecteur en texte brut.
    fn start_reply(&mut self, everyone: bool) {
        let Some(message) = self.message.as_ref() else {
            return;
        };
        let Some(account) = self.default_sender() else {
            self.status = Some("Aucun compte ne peut envoyer.".to_owned());
            return;
        };

        // **La règle de fil vit dans `mailsmtp::compose`**, pas ici. Elle était écrite dans
        // cette fonction, donc invérifiable ailleurs : le banc du critère 1 la vérifie
        // maintenant sur les vrais fils du corpus, et il appelle la même fonction que cette
        // fenêtre. Deux copies auraient fini par ne plus dire la même chose, et un fil cassé
        // ne se voit que chez le destinataire.
        let threading =
            mailsmtp::compose::reply_threading(message.message_id.as_deref(), &message.references);
        let subject = mailsmtp::compose::reply_subject(&message.row.subject);

        // Les destinataires d'origine, moins soi et moins l'expéditeur — qui est déjà dans
        // « À ». Sans le second filtre, l'expéditeur reçoit la réponse deux fois.
        let cc = if everyone {
            let mine: Vec<&str> = self
                .accounts
                .iter()
                .filter_map(|it| it.address.as_deref())
                .collect();
            let others: Vec<String> = message
                .to
                .iter()
                .filter(|address| {
                    let address = address.to_lowercase();
                    !mine.iter().any(|own| address.contains(&own.to_lowercase()))
                        && !address.contains(&message.row.from.to_lowercase())
                })
                .cloned()
                .collect();
            others.join(", ")
        } else {
            String::new()
        };

        self.compose = Some(Compose {
            account,
            to: message.row.from.clone(),
            cc,
            subject,
            text: quoted(message),
            in_reply_to: threading.in_reply_to,
            references: threading.references,
            ..Compose::default()
        });
    }

    /// Ouvre un transfert du message ouvert.
    ///
    /// ## Un transfert n'est pas une réponse, et n'entre pas dans le fil
    ///
    /// Ni `In-Reply-To`, ni `References` : le message part vers quelqu'un qui n'a pas suivi la
    /// conversation, et l'accrocher au fil d'origine le ferait ranger dans une discussion que
    /// le destinataire n'a jamais vue.
    ///
    /// ## Les pièces jointes ne suivent pas, et c'est dit
    ///
    /// Le brouillon ne désigne que du contenu **déjà dans le magasin de blobs** — c'est ce qui
    /// empêche un client de faire lire un fichier arbitraire au démon. Rattacher les pièces du
    /// message d'origine demanderait de les y ranger d'abord, donc une méthode d'API qui
    /// n'existe pas. Le dire dans le statut vaut mieux que de laisser découvrir l'absence à la
    /// réception.
    fn start_forward(&mut self) {
        let Some(message) = self.message.as_ref() else {
            return;
        };
        let Some(account) = self.default_sender() else {
            self.status = Some("Aucun compte ne peut envoyer.".to_owned());
            return;
        };

        let subject = if message.row.subject.to_lowercase().starts_with("tr:") {
            message.row.subject.clone()
        } else {
            format!("Tr: {}", message.row.subject)
        };

        // **Les pièces jointes du message d'origine sont reprises.** Chacune est rangée dans le
        // magasin par le service — `messages.stage_part` — et arrive par `Reply::Attached`,
        // comme un fichier déposé sur la fenêtre. L'interface n'attend pas : une pièce de 25 Mo
        // se range en fond, et le brouillon la reçoit quand elle est là. Règle 3 du `CLAUDE.md`.
        //
        // Le rang est celui de la liste que le service a rendue, jamais un chemin : voir
        // `mailapi::dispatch::StagePartParams`.
        let pieces = message.attachments.len();
        let id = message.row.id;
        if pieces > 0 {
            self.status = Some(format!(
                "Transfert : reprise de {pieces} pièce(s) jointe(s) en cours…"
            ));
            for part in 0..pieces {
                self.worker.ask(Request::StagePart { id, part });
            }
        }

        self.compose = Some(Compose {
            account,
            subject,
            text: forwarded(message),
            ..Compose::default()
        });
    }
    /// La fenêtre de rédaction.
    ///
    /// ## Une fenêtre, pas un panneau
    ///
    /// Elle se déplace et se redimensionne, et surtout elle **ne remplace pas la liste** : on
    /// écrit en regardant le message auquel on répond. C'est la première chose qui manque dans
    /// les clients qui ouvrent la rédaction en plein écran.
    ///
    /// ## Ce qu'elle ne valide pas
    ///
    /// Les adresses. La validation est chez le démon, qui écrit la commande SMTP ; une copie
    /// ici serait une deuxième règle à tenir d'accord avec la première. Ce que la fenêtre fait
    /// est **montrer** le refus, et ne pas jeter le brouillon quand il arrive.
    /// La fenêtre de l'éditeur de signature.
    ///
    /// ## Une fenêtre du système, pour la même raison que la rédaction
    ///
    /// Elle s'ouvre **depuis** la fenêtre de rédaction, qui est déjà un viewport du système :
    /// un `egui::Window` vivrait dans la fenêtre principale, donc derrière celle de rédaction,
    /// et le bouton qui l'ouvre semblerait ne rien faire.
    ///
    /// ## Elle n'attend rien du réseau
    ///
    /// Enregistrer écrit une ligne dans le store par le service, et la fenêtre ne se ferme
    /// qu'à la réponse — quelques millisecondes sur une écriture locale. Rien ici ne parle à un
    /// serveur : une signature ne quitte la machine qu'à l'intérieur d'un message.
    fn signature_window(&mut self, ctx: &egui::Context) {
        // Sortie de l'état pendant le dessin : le layouter emprunte le document, donc `self`
        // ne peut pas être emprunté en écriture au même moment. Le même motif que le brouillon.
        let Some(mut editor) = self.editor.take() else {
            return;
        };
        let mut open = true;
        let mut outcome = crate::signature::Outcome::Idle;

        let viewport = egui::ViewportBuilder::default()
            .with_title("Signature")
            .with_inner_size([560.0, 420.0])
            .with_min_inner_size([420.0, 320.0]);

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("mailcore-signature"),
            viewport,
            |window, _class| {
                if window.input(|it| it.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(window, |ui| {
                    let account = self
                        .accounts
                        .iter()
                        .find(|it| it.id == editor.account)
                        .and_then(|it| it.address.clone().or_else(|| Some(it.name.clone())))
                        .unwrap_or_else(|| "—".to_owned());
                    ui.horizontal(|ui| {
                        ui.weak("Signature de");
                        ui.label(&account);
                    });
                    ui.separator();
                    outcome = editor.show(ui);
                });
            },
        );

        match outcome {
            crate::signature::Outcome::Save => {
                let document = editor.document().clone();
                editor.saving = true;
                self.worker.ask(Request::SetSignature {
                    account: editor.account,
                    signature: Some(document),
                });
                self.editor = Some(editor);
            }
            // Annuler jette ce qui a été tapé sans rien écrire : la signature rangée n'a pas
            // bougé, et c'est ce que la fenêtre de rédaction affichera toujours.
            crate::signature::Outcome::Cancel => {}
            crate::signature::Outcome::Idle => {
                self.editor = if open { Some(editor) } else { None };
            }
        }
    }

    /// La fenêtre de paramètres : les comptes, et par où ils lisent et envoient.
    ///
    /// ## Elle ne parle pas à l'API, et c'est tout le point
    ///
    /// Déclarer un compte écrit un secret. La demande part donc vers `Link`, qui l'exécute
    /// localement — voir `crate::settings` pour le raisonnement complet, et `Link::stage` pour
    /// le précédent.
    fn settings_window(&mut self, ctx: &egui::Context) {
        let Some(mut page) = self.settings.take() else {
            return;
        };
        let mut open = true;
        let mut outcome = crate::settings::Outcome::Idle;

        let viewport = egui::ViewportBuilder::default()
            .with_title("Paramètres")
            .with_inner_size([720.0, 620.0])
            .with_min_inner_size([460.0, 360.0]);

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("mailcore-settings"),
            viewport,
            |window, _class| {
                if window.input(|it| it.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(window, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            outcome = page.show(ui);
                        });
                });
            },
        );

        match outcome {
            crate::settings::Outcome::Declare(form) => {
                self.worker.ask(Request::DeclareAccount(form));
            }
            crate::settings::Outcome::Submission(form) => {
                self.worker.ask(Request::SetSubmission(form));
            }
            crate::settings::Outcome::Forget(id) => {
                self.worker.ask(Request::ForgetAccount { id });
            }
            crate::settings::Outcome::Consent(form) => {
                self.worker.ask(Request::Consent(form));
            }
            crate::settings::Outcome::Sync(account) => {
                self.worker.ask(Request::StartSync { account });
                self.worker.ask(Request::Jobs);
                // Le panneau des tâches s'ouvre tout seul : une moisson lancée sans rien à
                // regarder ressemble à un bouton qui n'a rien fait, et l'utilisateur reclique.
                self.show_import = true;
                if let Some(page) = &mut self.settings {
                    page.message = Some(format!(
                        "Synchronisation du compte #{account} lancée. Sa progression s'affiche \
                         dans « Importer… »."
                    ));
                    page.failed = false;
                }
            }
            crate::settings::Outcome::Idle => {}
        }

        self.settings = if open { Some(page) } else { None };
    }

    /// Ouvre la page de paramètres et demande l'état des comptes.
    fn open_settings(&mut self) {
        self.settings = Some(crate::settings::Page {
            // La page doit savoir si elle peut écrire **avant** de dessiner un bouton :
            // proposer « Déclarer un compte » à un client distant serait promettre un geste
            // impossible, ce que le critère 8 interdit ailleurs pour la même raison.
            remote: self.worker.is_remote(),
            ..crate::settings::Page::default()
        });
        self.worker.ask(Request::AccountsDetail);
    }

    /// Ouvre la fenêtre de source sur une cible, et demande les octets.
    ///
    /// La demande part **à l'ouverture** et pas avec le message : elle pèse la taille du
    /// message, et l'immense majorité des ouvertures ne la regarde jamais.
    fn open_source(&mut self, of: crate::worker::SourceOf, title: String) {
        self.source = Some(SourceView {
            of,
            title,
            found: None,
            error: None,
        });
        self.worker.ask(Request::Source { of });
    }

    /// La fenêtre « source du message ».
    ///
    /// ## Du texte, et rien qui l'interprète
    ///
    /// Pas de `mailhtml`, pas de blocs, pas de mise en forme : un `TextEdit` en lecture seule
    /// et une police à chasse fixe. C'est le sens de la vue — et c'est aussi ce qui la rend
    /// inoffensive, puisqu'il n'y a rien à interpréter dans un message affiché comme du texte.
    /// Les caractères de contrôle, eux, sont déjà neutralisés par `mailcore::source`.
    fn source_window(&mut self, ctx: &egui::Context) {
        let Some(view) = &self.source else {
            return;
        };
        let mut open = true;

        let viewport = egui::ViewportBuilder::default()
            .with_title(format!("Source — {}", view.title))
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([420.0, 320.0]);

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("mailcore-source"),
            viewport,
            |window, _class| {
                if window.input(|it| it.viewport().close_requested()) {
                    open = false;
                }
                egui::CentralPanel::default().show(window, |ui| {
                    if let Some(error) = &view.error {
                        ui.colored_label(theme::accent(ui.ctx()), error);
                        return;
                    }
                    let Some(source) = &view.found else {
                        ui.weak("lecture…");
                        return;
                    };
                    source_body(ui, source);
                });
            },
        );

        if !open {
            self.source = None;
        }
    }

    fn compose_window(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.compose.clone() else {
            return;
        };
        // La signature du compte choisi, demandée une fois. L'absence de clé dit « on ne sait
        // pas encore » ; sans cette distinction, un compte sans signature serait redemandé
        // soixante fois par seconde.
        //
        // L'entrée est posée **avant** la demande, et c'est ce qui borne la demande à une : la
        // réponse écrasera ce `None` par ce que le store a.
        let unknown = match self.signatures.entry(draft.account) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(None);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        };
        if unknown {
            self.worker.ask(Request::Signature {
                account: draft.account,
            });
        }
        let mut open = true;
        // Ce que le formulaire relève pendant le dessin, et que cette fonction consomme après.
        let mut pending = Pending::default();
        // Les fichiers déposés sur la fenêtre, à ranger dans le magasin.
        let mut dropped: Vec<camino::Utf8PathBuf> = Vec::new();
        // Vrai pendant qu'un fichier survole la fenêtre.
        let mut hovering = false;

        let title = if draft.in_reply_to.is_some() {
            "Répondre"
        } else {
            "Nouveau message"
        };

        // **Une vraie fenêtre du système, pas un panneau flottant.** C'était un
        // `egui::Window`, qui vit *à l'intérieur* de la fenêtre principale : impossible de le
        // déplacer sur un autre écran, absent de la barre des tâches et d'Alt-Tab, et coupé par
        // les bords de la fenêtre parente. Écrire un message en regardant le message auquel on
        // répond était l'intention, et elle n'était pas tenable.
        //
        // Un viewport **immédiat** et non différé : le rappel d'un viewport différé doit être
        // `Send + Sync + 'static`, donc il ne peut pas toucher le brouillon local — il faudrait
        // mettre tout l'état de la rédaction derrière un `Arc<Mutex<…>>`. L'immédiat s'exécute
        // dans l'image courante et emprunte ce qu'il veut.
        //
        // Le prix de l'immédiat : la fenêtre parente redessine quand l'enfant redessine. Pour
        // une fenêtre de rédaction ouverte quelques minutes, c'est sans conséquence.
        //
        // Sur un système sans multi-fenêtre, `egui` retombe sur un panneau intégré
        // (`ViewportClass::Embedded`) : dégradé, pas cassé.
        let viewport = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([620.0, 560.0])
            // Le minimum est posé sur la **fenêtre du système** : c'est lui qui empêche de la
            // réduire jusqu'à ce que les étiquettes ne tiennent plus à côté de leurs champs.
            .with_min_inner_size([COMPOSE_MIN_WIDTH, COMPOSE_MIN_HEIGHT]);

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("mailcore-compose"),
            viewport,
            |window, _class| {
                // La croix de la fenêtre : c'est le système qui la demande, et sans ce relevé
                // elle ne fermerait rien.
                if window.input(|it| it.viewport().close_requested()) {
                    open = false;
                }
                // **Le glisser-déposer, et pas un sélecteur de fichiers.** Un sélecteur
                // demanderait une dépendance de plus — `egui` n'en a pas — et le geste naturel
                // pour joindre un fichier est de le déposer sur la fenêtre. Ce que la
                // plate-forme donne ici est un **chemin**, qui ne quitte pas ce processus :
                // voir `worker::Link::stage`.
                //
                // Les chemins sont relevés dans la fermeture et consommés après : demander le
                // rangement d'ici demanderait `&mut self` alors que le dessin le tient.
                dropped.extend(
                    window
                        .input(|it| it.raw.dropped_files.clone())
                        .iter()
                        .map(|file| file.path().to_path_buf())
                        .filter_map(|path| camino::Utf8PathBuf::from_path_buf(path).ok()),
                );
                hovering = window.input(|it| !it.raw.hovered_files.is_empty());

                egui::CentralPanel::default().show(window, |ui| {
                    self.compose_form(ui, &mut draft, &mut pending);
                    if hovering {
                        // Un retour pendant le survol : sans lui, l'utilisateur ne sait pas
                        // que la fenêtre accepte ce qu'il tient.
                        ui.separator();
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "Déposer pour joindre le fichier.",
                        );
                    }
                });
            },
        );

        if let Some(index) = pending.detach
            && index < draft.attachments.len()
        {
            // Retirée du brouillon seulement : son blob reste dans le magasin et devient
            // orphelin, ce que `mail doctor` compte. Le supprimer ici demanderait de savoir
            // qu'aucun autre brouillon ne l'utilise.
            draft.attachments.remove(index);
        }
        for path in dropped {
            // Le rangement est un aller-retour par le fil du service : il lit le fichier et
            // le compresse, ce qui pour 25 Mo prend le temps qu'il prend. L'interface ne
            // l'attend pas — règle 3 — et la pièce apparaîtra à la réponse.
            self.status = Some(format!("{} : rangement en cours…", path.as_str()));
            self.worker.ask(Request::Attach { path });
        }
        if pending.edit_signature && self.editor.is_none() {
            self.editor = Some(crate::signature::Editor::new(
                draft.account,
                self.signatures.get(&draft.account).cloned().flatten(),
            ));
        }
        if pending.send {
            self.sending = true;
            // **La coquille demande la signature, elle ne la compose pas.** Le brouillon porte
            // `sign`, et c'est le démon qui l'ajoute par `Draft::sign_with` — la même fonction
            // que `mail send --signature`. Une deuxième implémentation ici enverrait un jour à
            // quelqu'un une signature en double.
            self.worker.ask(Request::Send(Box::new(draft.clone())));
        }
        self.settle_completion(&mut draft, pending.active, pending.insert);
        // Le brouillon modifié est **toujours** réécrit en mémoire, y compris quand on ferme :
        // c'est ce qui fait qu'une frappe n'est pas perdue à l'image suivante.
        if open {
            self.compose = Some(draft);
        } else {
            // **Fermer enregistre.** C'était la seule chose qui jetait le brouillon, et
            // fermer la fenêtre par mégarde perdait tout ce qui n'était pas parti. Un
            // brouillon vide n'écrit rien — le service le sait — donc ouvrir puis refermer
            // sans rien taper ne laisse pas de trace.
            //
            // Rien n'est attendu : la fenêtre se ferme tout de suite, et l'écriture arrive
            // par `Reply::Saved`. Règle 3 du `CLAUDE.md`.
            if !pending.send {
                self.worker.ask(Request::SaveDraft(Box::new(draft.clone())));
            }
            self.compose = None;
        }
    }

    /// Le formulaire de rédaction, dans le `Ui` qu'on lui donne.
    ///
    /// Séparé de [`Shell::compose_window`] parce que le premier décide **où** dessiner — une
    /// fenêtre du système — et celui-ci **quoi**. La séparation n'est pas cosmétique : elle est
    /// ce qui a permis de changer de contenant sans toucher au formulaire.
    fn compose_form(&self, ui: &mut egui::Ui, draft: &mut Compose, pending: &mut Pending) {
        // Le champ de destinataires qui a le focus, pour y ancrer la liste des propositions.
        // Relevé dans la grille et consommé après : la liste doit flotter **au-dessus** du
        // formulaire, donc elle se dessine hors de la cellule qui l'a produite.
        let mut focused: Option<egui::Response> = None;

        egui::Grid::new("compose-entetes")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label("De");
                // **La liste montre tous les comptes, pas seulement ceux qui peuvent envoyer.**
                //
                // Elle ne montrait que les seconds, et disparaissait quand il n'y en avait
                // qu'un : sur un profil où un seul compte a un serveur de soumission, il n'y
                // avait donc *rien* à cliquer et rien qui explique pourquoi. « On ne peut pas
                // choisir l'adresse d'envoi ? » était la question, et la réponse était dans un
                // fichier de configuration.
                //
                // Les comptes sans serveur d'envoi apparaissent maintenant, **grisés**, avec au
                // survol la commande qui les rend utilisables. Un choix absent qu'on explique
                // vaut mieux qu'un choix absent.
                let current = self
                    .accounts
                    .iter()
                    .find(|it| it.id == draft.account)
                    .map_or("—", |it| it.address.as_deref().unwrap_or(&it.name));
                if self.accounts.len() <= 1 {
                    ui.label(current);
                } else {
                    egui::ComboBox::from_id_salt("compose-de")
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for account in &self.accounts {
                                let label = account.address.as_deref().unwrap_or(&account.name);
                                if account.can_send {
                                    ui.selectable_value(&mut draft.account, account.id, label);
                                } else {
                                    // Grisé, et jamais sélectionnable : le choisir mettrait en
                                    // file un message que personne ne peut remettre, et le
                                    // démon le refuserait de toute façon.
                                    ui.add_enabled(false, egui::Button::selectable(false, label))
                                        .on_disabled_hover_text(format!(
                                            "Pas de serveur d'envoi. Pour en poser un :\n\
                                         mail account submission --account {}",
                                            account.id
                                        ));
                                }
                            }
                        });
                }
                ui.end_row();

                ui.label("À");
                let to = ui.add(
                    egui::TextEdit::singleline(&mut draft.to)
                        .desired_width(f32::INFINITY)
                        .hint_text("jean@exemple.fr, marie@exemple.fr"),
                );
                if to.has_focus() {
                    pending.active = Some((Field::To, draft.to.clone()));
                    focused = Some(to);
                }
                ui.end_row();

                ui.label("Copie");
                let cc =
                    ui.add(egui::TextEdit::singleline(&mut draft.cc).desired_width(f32::INFINITY));
                if cc.has_focus() {
                    pending.active = Some((Field::Cc, draft.cc.clone()));
                    focused = Some(cc);
                }
                ui.end_row();

                ui.label("Copie cachée");
                let bcc = ui.add(
                    egui::TextEdit::singleline(&mut draft.bcc)
                        .desired_width(f32::INFINITY)
                        .hint_text("invisible pour les autres destinataires"),
                );
                if bcc.has_focus() {
                    pending.active = Some((Field::Bcc, draft.bcc.clone()));
                    focused = Some(bcc);
                }
                ui.end_row();

                ui.label("Sujet");
                ui.add(egui::TextEdit::singleline(&mut draft.subject).desired_width(f32::INFINITY));
                ui.end_row();
            });

        // **Les propositions sont une liste déroulante sous le champ**, et non plus une rangée
        // de pastilles sous le formulaire.
        //
        // La rangée avait deux défauts, et le second était le vrai : elle était laide, et elle
        // ne disait pas à quel champ elle appartenait. Une liste ancrée **sous le champ qui a
        // le focus** répond aux deux — c'est ce que fait n'importe quel champ d'adresses — et
        // elle flotte, donc elle ne pousse plus rien à chaque frappe.
        if let Some((field, _)) = pending.active.as_ref()
            && self.suggesting == Some(*field)
            && !self.suggestions.is_empty()
            && let Some(anchor) = focused.as_ref()
        {
            egui::Popup::from_response(anchor)
                // Ouverte tant qu'il y a des propositions : ce n'est pas un menu qu'on
                // déplie, c'est le retour d'une frappe.
                .open(true)
                // Aussi large que le champ : une liste d'adresses plus étroite que son champ
                // tronque les longues, et c'est la fin qui distingue deux adresses proches.
                .width(anchor.rect.width())
                .gap(2.0)
                .layout(egui::Layout::top_down_justified(egui::Align::Min))
                // La fermeture est décidée par les propositions elles-mêmes : elles
                // disparaissent quand le champ change ou quand le focus s'en va. Un clic
                // dehors ne doit pas la fermer « pour de bon », sinon la frappe suivante ne la
                // ramènerait pas.
                .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
                .show(|ui| {
                    for suggestion in &self.suggestions {
                        // Le nom **et** l'adresse quand les deux existent : une liste de noms
                        // seuls ne permet pas de choisir entre deux homonymes, et c'est
                        // exactement ce que le carnet dérivé du corpus produit.
                        let response = ui.selectable_label(false, &suggestion.label);
                        // Le nombre d'échanges au survol, pas dans la ligne : c'est ce qui
                        // explique l'ordre sans encombrer la liste.
                        let response = response.on_hover_text(format!(
                            "{} envoyé(s), {} reçu(s)",
                            suggestion.seen_to, suggestion.seen_from
                        ));
                        if response.clicked() {
                            pending.insert = Some((*field, suggestion.label.clone()));
                        }
                    }
                });
        }

        // **Les pièces jointes, avec leur taille encodée.** Le tiers de plus que fait le base64
        // est ce qui décide si un message passe la limite du serveur, et le cacher ferait
        // découvrir le refus après le transfert.
        if !draft.attachments.is_empty() {
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.weak("Pièces jointes :");
                for (index, attachment) in draft.attachments.iter().enumerate() {
                    // Quatre caractères pour trois octets, arrondi au supérieur. Les sauts de
                    // ligne du repliage sont négligés ici : c'est un ordre de grandeur affiché,
                    // pas la valeur annoncée au serveur — celle-là est calculée par
                    // `Attachment::encoded_len`, côté service.
                    let encoded = attachment.size.saturating_mul(4).div_ceil(3);
                    if ui
                        .small_button(format!(
                            "{} ({}) ×",
                            attachment.filename,
                            human::bytes(attachment.size)
                        ))
                        .on_hover_text(format!(
                            "{} une fois encodée — cliquer pour retirer",
                            human::bytes(encoded)
                        ))
                        .clicked()
                    {
                        pending.detach = Some(index);
                    }
                }
            });
        }
        ui.separator();
        // **Le corps prend la place qui reste, pour de bon.** Il avait une hauteur figée de
        // 320 px, et le commentaire d'à côté prétendait le contraire : agrandir la fenêtre
        // verticalement n'agrandissait donc rien, ce qui est la seule direction de
        // redimensionnement qui serve.
        //
        // Ce qui suit est réservé : un séparateur et la ligne du bouton. Sans cette
        // soustraction, le corps prendrait tout et pousserait le bouton hors de la fenêtre.
        // Ce que la signature occupe est réservé **avec** le pied : sa ligne de titre est
        // toujours là — il faut bien un bouton pour en ajouter une — et son aperçu seulement
        // quand il y en a une. Sans cette réserve, la signature repousse « Envoyer » hors de la
        // fenêtre, c'est-à-dire exactement le défaut que `COMPOSE_FOOTER` a été écrit pour
        // corriger.
        let signature = self.signatures.get(&draft.account).and_then(Option::as_ref);
        let reserved = COMPOSE_FOOTER
            + SIGNATURE_LINE
            + if signature.is_some() && draft.sign {
                SIGNATURE_PREVIEW
            } else {
                0.0
            };
        let body_height = (ui.available_height() - reserved).max(COMPOSE_MIN_BODY);
        egui::ScrollArea::vertical()
            .max_height(body_height)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut draft.text)
                        .desired_width(f32::INFINITY)
                        // Assez de lignes pour remplir la place disponible : en dessous,
                        // `TextEdit` laisserait du vide sous le curseur et le clic dans ce
                        // vide ne placerait pas le curseur.
                        .desired_rows(rows_for(body_height))
                        .hint_text("Le message."),
                );
            });

        // **La signature est montrée, pas seulement ajoutée.** `docs/PHASE-3.md` : « rien de ce
        // qu'on ajoute au message n'est invisible à l'utilisateur ». Elle n'est pas collée dans
        // le champ du corps — un champ de texte brut perdrait ses styles — donc elle s'affiche
        // ici, telle qu'elle partira, avec ses puces et ses liens.
        ui.separator();
        ui.horizontal(|ui| {
            match signature {
                // **La case, et c'est elle qui rend l'ajout refusable.** La montrer ne suffit
                // pas s'il n'y a aucun moyen de dire non pour ce message-ci : « visible » et
                // « imposé » ne valent pas mieux qu'invisible.
                Some(_) => {
                    ui.checkbox(&mut draft.sign, "Signature :");
                }
                None => {
                    ui.weak("Signature : aucune pour ce compte.");
                }
            }
            if ui
                .small_button(if signature.is_some() {
                    "Modifier"
                } else {
                    "Ajouter"
                })
                .clicked()
            {
                pending.edit_signature = true;
            }
        });
        if let Some(signature) = signature.filter(|_| draft.sign) {
            // Elle est bornée en hauteur : une signature de deux cents lignes ne doit pas
            // pousser le bouton « Envoyer » hors de la fenêtre.
            egui::ScrollArea::vertical()
                .id_salt("compose-signature")
                .max_height(SIGNATURE_PREVIEW)
                .show(ui, |ui| signature_preview(ui, signature));
        }

        ui.separator();
        ui.horizontal(|ui| {
            let ready = draft.is_sendable() && !self.sending;
            if ui
                .add_enabled(ready, egui::Button::new("Envoyer"))
                .clicked()
            {
                pending.send = true;
            }
            if self.sending {
                ui.spinner();
                ui.weak("mise en file…");
            } else if !draft.is_sendable() {
                ui.weak("Au moins un destinataire.");
            } else {
                // Dire ce qui va se passer, parce que ce n'est pas ce qu'un client mail
                // fait d'habitude : le message part du démon, pas de cette fenêtre.
                ui.weak("Le message est mis en file ; il partira même fenêtre fermée.");
            }
        });
    }

    /// Consomme ce que le formulaire a relevé : une proposition cliquée, ou un champ actif.
    ///
    /// ## Pourquoi ce n'est pas dans le formulaire
    ///
    /// Le formulaire prend `&self` — il dessine, il ne décide pas — et ces trois branches
    /// écrivent dans l'état. La séparation n'est donc pas un rangement : c'est ce qui rend
    /// impossible de demander une complétion depuis une fermeture qui tient déjà `ui`.
    fn settle_completion(
        &mut self,
        draft: &mut Compose,
        active: Option<(Field, String)>,
        insert: Option<(Field, String)>,
    ) {
        if let Some((field, text)) = insert {
            // **Remplacer le dernier fragment, pas tout le champ.** L'utilisateur a peut-être
            // déjà trois destinataires ; insérer par-dessus les effacerait.
            let target = match field {
                Field::To => &mut draft.to,
                Field::Cc => &mut draft.cc,
                Field::Bcc => &mut draft.bcc,
            };
            *target = replace_last(target, &text);
            // Les propositions disparaissent après l'insertion : elles portaient sur le
            // fragment qu'on vient de remplacer.
            self.suggestions.clear();
            self.suggesting = None;
            self.suggest_for.clear();
        } else if let Some((field, text)) = active {
            let fragment = last_fragment(&text);
            // **Ne redemander que si le fragment a changé.** `egui` redessine soixante fois par
            // seconde ; une demande par image serait soixante appels par seconde pour un texte
            // immobile, et le fil du service est sériel.
            if self.suggesting != Some(field) || self.suggest_for != fragment {
                self.suggesting = Some(field);
                self.suggest_for = fragment.clone();
                self.suggest_token = self.suggest_token.wrapping_add(1);
                self.worker.ask(Request::Complete {
                    prefix: fragment,
                    token: self.suggest_token,
                });
            }
        } else {
            // Aucun champ de destinataire n'a le focus : la liste n'a plus de sens.
            self.suggestions.clear();
            self.suggesting = None;
        }
    }

    /// Le panneau de la file d'envoi.
    ///
    /// ## Les envois douteux sont traités à part, et c'est tout l'intérêt du panneau
    ///
    /// Un message en `committing` a peut-être été remis. Rien ne le renverra tout seul, et
    /// l'interface doit le dire avec les mots de la situation — pas avec un code, pas avec une
    /// icône d'erreur qui laisserait croire qu'il n'est pas parti. C'est le critère 8 de
    /// `docs/PHASE-3.md` appliqué au cas le plus délicat.
    fn outbox_pane(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        if self.outbox.is_empty() {
            ui.label("File d'envoi vide.");
            return;
        }

        // **Ce qui n'est pas fini d'abord, l'historique ensuite et borné.** Le panneau montrait
        // toutes les lignes, `sent` comprises, et elles ne se purgent jamais : après un mois
        // d'usage, la seule ligne qui demande une décision était noyée dans un mur de
        // « envoyé ».
        let (open, done): (Vec<_>, Vec<_>) = self
            .outbox
            .iter()
            .cloned()
            .partition(|it| it.state != "sent");

        let mut decision: Option<(i64, &'static str)> = None;
        let mut resend: Option<i64> = None;
        // **Le bouton qui rend la promesse vérifiable.** C'est ici, et pas dans le lecteur, que
        // « rien n'est ajouté au message en secret » se contrôle : le lecteur montre ce que les
        // autres nous écrivent, la file montre ce que nous écrivons. Voir `outbox.source`.
        let mut show_source: Option<(i64, String)> = None;
        for line in &open {
            ui.horizontal_wrapped(|ui| {
                ui.set_min_height(ROW_HEIGHT);
                if line.doubtful {
                    ui.colored_label(ui.visuals().warn_fg_color, "⚠ état incertain");
                    ui.label(format!("→ {}", line.recipients.join(", ")));
                } else {
                    let state = match line.state.as_str() {
                        "queued" => "en attente",
                        "sending" => "en cours",
                        "failed" => "échoué",
                        other => other,
                    };
                    ui.label(format!("{state} → {}", line.recipients.join(", ")));
                }
                if source_button(ui).clicked() {
                    show_source = Some((line.id, format!("envoi #{}", line.id)));
                }
            });

            // **La phrase du refus n'est pas en gris.** Elle l'était, avec l'état en texte
            // normal à côté : le mot « échoué » ressortait, et ce qu'il fallait faire
            // s'effaçait. Le critère 8 demande l'inverse — l'état se devine, l'action non.
            if !line.doubtful
                && let Some(error) = &line.last_error
            {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(12.0);
                    ui.label(error);
                });
            }

            // Le geste que la phrase demande, à portée de clic. Sans bouton, « renvoyez le
            // message » n'a qu'une seule réalisation possible dans la coquille : tout
            // réécrire.
            if line.resendable {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(12.0);
                    if ui
                        .button("Renvoyer tel quel")
                        .on_hover_text(
                            "Le serveur a refusé sans prendre le message : le renvoyer ne peut \
                             pas faire de doublon. Il repart en file, le compteur de tentatives \
                             remis à zéro.",
                        )
                        .clicked()
                    {
                        resend = Some(line.id);
                    }
                });
            }

            if line.doubtful {
                // **Les deux issues du doute.** Le panneau disait « rien ne sera renvoyé
                // automatiquement » et n'offrait aucun moyen de décider : un avertissement
                // qu'on ne peut pas lever finit par ne plus être lu, y compris le jour où il
                // compte. Le protocole ne peut pas trancher ; l'utilisateur peut aller
                // regarder ses messages envoyés chez son fournisseur.
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(12.0);
                    ui.weak(
                        "La coupure est tombée entre la fin de l'envoi et la réponse du \
                         serveur : il a peut-être le message. Vérifier dans les messages \
                         envoyés du fournisseur, puis :",
                    );
                });
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(12.0);
                    if ui
                        .button("Il n'est pas arrivé — renvoyer")
                        .on_hover_text(
                            "Le remet en file. À ne faire qu'après avoir vérifié : sinon le \
                             destinataire le reçoit deux fois.",
                        )
                        .clicked()
                    {
                        decision = Some((line.id, "resend"));
                    }
                    if ui
                        .button("Il est arrivé — clore")
                        .on_hover_text("Le marque envoyé, sans rien envoyer. Aucun octet ne part.")
                        .clicked()
                    {
                        decision = Some((line.id, "accept"));
                    }
                });
            }
        }

        if !done.is_empty() {
            ui.separator();
            // Les derniers envoyés seulement, et le total dit ce qui n'est pas montré. Une
            // liste tronquée sans son compte laisserait croire que c'est tout.
            let shown = done.len().min(OUTBOX_HISTORY);
            for line in done.iter().rev().take(shown) {
                ui.horizontal_wrapped(|ui| {
                    ui.set_min_height(ROW_HEIGHT);
                    ui.weak(format!("envoyé → {}", line.recipients.join(", ")));
                    // Sur une ligne **envoyée** surtout : c'est après coup qu'on veut savoir ce
                    // qui est parti, et le message ne reviendra du dossier « Envoyés » qu'à la
                    // prochaine moisson.
                    if source_button(ui).clicked() {
                        show_source = Some((line.id, format!("envoi #{}", line.id)));
                    }
                });
            }
            if done.len() > shown {
                ui.weak(format!(
                    "… et {} autres envoyés",
                    done.len().saturating_sub(shown)
                ));
            }
        }

        if let Some((id, choice)) = decision {
            self.worker.ask(Request::Decide {
                id,
                decision: choice,
            });
            // La ligne va changer d'état côté service ; la relire tout de suite plutôt que
            // d'attendre le `Changed`, parce que c'est le moment où l'utilisateur regarde.
            self.worker.ask(Request::Outbox);
        }
        if let Some(id) = resend {
            self.worker.ask(Request::Retry { id });
            self.worker.ask(Request::Outbox);
        }
        if let Some((id, title)) = show_source {
            self.open_source(crate::worker::SourceOf::Outgoing(id), title);
        }
    }
    /// Le panneau d'import : les sources déclarées, les tâches en cours.
    ///
    /// **Un rang, jamais un chemin** : la coquille choisit dans la liste que le service rend,
    /// exactement comme un client distant (`mailapi`, documentation de `jobs.start`).
    fn import_pane(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        if self.sources.is_empty() {
            ui.label(
                "Aucun profil Thunderbird détecté sur cette machine. Rien à importer : la \
                 coquille ne lit que ce que le service a déclaré.",
            );
        }
        for source in self.sources.clone() {
            ui.horizontal(|ui| {
                ui.set_height(ROW_HEIGHT);
                if ui.button("Importer").clicked() {
                    self.worker.ask(Request::StartImport { source: source.id });
                    self.worker.ask(Request::Jobs);
                }
                ui.weak(source.path);
            });
        }
        if !self.sources.is_empty() {
            ui.horizontal(|ui| {
                ui.set_height(ROW_HEIGHT);
                if ui.button("Reconstruire l'index").clicked() {
                    self.worker.ask(Request::StartIndex);
                    self.worker.ask(Request::Jobs);
                }
                ui.weak("recherche plein texte");
            });
        }

        // **Le bouton qui rend le cache acceptable.** `docs/PRIVACY.md` §8 : les sujets et les
        // expéditeurs d'une boîte sont presque aussi révélateurs que le courrier. Un cache
        // qu'on ne peut pas effacer est une trace qu'on ne peut pas retirer.
        if let Some(host) = self.cache_host.clone() {
            ui.horizontal(|ui| {
                ui.set_height(ROW_HEIGHT);
                let label = if self.cache_disabled {
                    "Cache purgé — plus rien n'est écrit"
                } else {
                    "Purger le cache de lecture"
                };
                if ui
                    .add_enabled(!self.cache_disabled, egui::Button::new(label))
                    .clicked()
                {
                    match crate::cache::purge(&host) {
                        Ok(path) => {
                            // **La purge vaut pour toute la session.** Sans ce drapeau, la
                            // réponse suivante du démon réécrivait le fichier quelques dizaines
                            // de millisecondes plus tard : le bouton n'effaçait alors que
                            // jusqu'à la page suivante, ce qui n'est pas effacer.
                            self.cache_disabled = true;
                            self.cache_saved = None;
                            self.status = Some(format!(
                                "cache effacé : {path}. Plus rien ne sera écrit jusqu'au \
                                 prochain lancement."
                            ));
                            // La liste à l'écran, elle, ne bouge pas : elle vient du démon, pas
                            // du cache. La vider donnerait l'impression d'avoir perdu son
                            // courrier pour avoir nettoyé un fichier.
                        }
                        Err(source) => {
                            self.status = Some(format!("cache non effacé : {source}"));
                        }
                    }
                }
                ui.weak("sujets et expéditeurs gardés hors ligne, pour ce démon");
            });
        }

        for job in &self.jobs {
            ui.horizontal(|ui| {
                ui.set_height(ROW_HEIGHT);
                ui.label(format!("{} — {}", job.kind, job.state));
                match job.fraction {
                    // Une barre indéterminée est honnête ; une barre à zéro qui ne bouge pas
                    // ressemble à une panne.
                    Some(fraction) => {
                        ui.add(egui::ProgressBar::new(fraction).desired_width(180.0));
                    }
                    None if job.state == "running" => {
                        ui.spinner();
                    }
                    None => {}
                }
                if let Some(message) = &job.message {
                    ui.weak(message);
                }
            });
        }
        ui.separator();
    }

    /// Le panneau des dossiers, groupés par compte.
    ///
    /// 96 dossiers sur 11 comptes, et `INBOX` existe dans presque chacun : une liste plate de
    /// chemins complets donne des entrées indiscernables. Le compte en intitulé, le dernier
    /// segment indenté selon la profondeur.
    fn folders_pane(&mut self, ui: &mut egui::Ui) {
        let mut to_open = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut account = None;
                for folder in &self.folders {
                    if account.as_deref() != Some(folder.account.as_str()) {
                        if account.is_some() {
                            ui.add_space(6.0);
                        }
                        ui.label(egui::RichText::new(&folder.account).strong().size(11.0));
                        account = Some(folder.account.clone());
                    }

                    let depth = folder.path.matches('/').count();
                    let leaf = folder.path.rsplit('/').next().unwrap_or(&folder.path);
                    ui.horizontal(|ui| {
                        ui.set_height(ROW_HEIGHT);
                        #[allow(clippy::cast_precision_loss)]
                        ui.add_space(depth as f32 * 12.0);
                        let label = if folder.unread > 0 {
                            egui::RichText::new(leaf).strong()
                        } else {
                            egui::RichText::new(leaf)
                        };
                        if ui
                            .selectable_label(self.current == Some(folder.id), label)
                            .clicked()
                        {
                            to_open = Some(folder.id);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if folder.unread > 0 {
                                ui.colored_label(
                                    theme::accent(ui.ctx()),
                                    folder.unread.to_string(),
                                );
                            } else {
                                ui.weak(folder.total.to_string());
                            }
                        });
                    });
                }
            });
        if let Some(id) = to_open {
            self.open_folder(id);
        }
    }

    /// La liste, virtualisée par le toolkit.
    fn list_pane(&mut self, ui: &mut egui::Ui) {
        let title = if self.results.is_some() {
            "Résultats".to_owned()
        } else {
            self.folder()
                .map_or_else(|| "—".to_owned(), |it| it.path.clone())
        };
        let total = self.total();
        let loaded = self.visible().len();

        ui.horizontal(|ui| {
            ui.set_height(24.0);
            ui.strong(title);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Le compteur est écrit en clair : « 100 / 9702 » ne dit pas de quoi on parle.
                if loaded < total {
                    ui.weak(format!("{loaded} chargés sur {total}"));
                } else {
                    ui.weak(format!("{total} messages"));
                }
            });
        });
        ui.separator();

        let mut to_open = None;
        let mut need_more = false;
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if self.bench_state.phase == Phase::Scrolling {
            area = area.vertical_scroll_offset(self.bench_state.offset);
        }
        area.show_rows(ui, ROW_HEIGHT, total, |ui, range| {
            if range.end > self.visible().len() {
                need_more = true;
            }
            for index in range {
                let Some(row) = self.visible().get(index) else {
                    // Ligne pas encore chargée : un vide de la bonne hauteur, pour que la
                    // barre de défilement reste juste.
                    ui.allocate_space(egui::vec2(ui.available_width(), ROW_HEIGHT));
                    continue;
                };
                if row_widget(ui, row, self.selected == Some(row.id)) {
                    to_open = Some(row.id);
                }
            }
        });

        if need_more {
            self.request_page();
        }
        if let Some(id) = to_open {
            self.open_message(id, false);
        }
    }

    /// Le volet de lecture.
    fn reader_pane(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.open_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
            return;
        }
        if self.message.is_none() {
            ui.weak(if self.opening {
                "Ouverture…"
            } else {
                "Aucun message sélectionné."
            });
            return;
        }

        // **Emprunté, pas cloné.** La première version faisait `self.message.clone()` à chaque
        // image pour contourner l'emprunt : jusqu'à deux mégaoctets de corps HTML recopiés
        // soixante fois par seconde. Le `take` sort la valeur de `self` le temps du dessin et la
        // remet ensuite, ce qui coûte deux déplacements de pointeur.
        let message = self.message.take();
        let body = std::mem::take(&mut self.body);
        let mut to_open = None;
        let mut more = false;
        let mut reply: Option<Answer> = None;

        if let Some(message) = &message {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    header(ui, message);
                    ui.separator();
                    // Les actions d'écriture du lecteur, et elles sont ici plutôt que dans la
                    // barre du haut : elles portent sur **ce** message, et une action
                    // contextuelle placée loin de son objet se cherche.
                    ui.horizontal_wrapped(|ui| {
                        let can_send = self.default_sender().is_some();
                        let refus = "Aucun compte n'a de serveur d'envoi configuré.";
                        if ui
                            .add_enabled(can_send, egui::Button::new("↩ Répondre"))
                            .on_disabled_hover_text(refus)
                            .clicked()
                        {
                            reply = Some(Answer::Sender);
                        }
                        // « À tous » n'a de sens que s'il y a quelqu'un d'autre : sur un
                        // message qui n'a qu'un destinataire, le bouton ferait exactement ce
                        // que fait le précédent.
                        let others = message.to.len() > 1;
                        if ui
                            .add_enabled(can_send && others, egui::Button::new("↩ Répondre à tous"))
                            .on_disabled_hover_text(if others {
                                refus
                            } else {
                                "Ce message n'a qu'un destinataire."
                            })
                            .on_hover_text("Répond à l'expéditeur, avec les autres en copie")
                            .clicked()
                        {
                            reply = Some(Answer::Everyone);
                        }
                        if ui
                            .add_enabled(can_send, egui::Button::new("→ Transférer"))
                            .on_disabled_hover_text(refus)
                            .on_hover_text(
                                "Nouveau message, hors du fil. Les pièces jointes ne suivent pas.",
                            )
                            .clicked()
                        {
                            reply = Some(Answer::Forward);
                        }
                        if ui
                            .button("⛭ Source")
                            .on_hover_text(
                                "Les octets du message, tels qu'ils sont : rien n'est décodé",
                            )
                            .clicked()
                        {
                            reply = Some(Answer::Source);
                        }
                    });
                    ui.separator();
                    self.body_actions(ui, message);
                    // **Le rendez-vous avant le corps, et pas dans les pièces jointes.** Quand
                    // un message porte une invitation, ce qu'on veut savoir est quand, où et
                    // avec qui — pas « invite.ics, 4 Kio ». Le corps d'une invitation Exchange
                    // est de toute façon un doublon en texte de ce que le fichier dit mieux.
                    if let Some(invitation) = &message.invitation {
                        ui.separator();
                        appointment(ui, invitation);
                    }
                    ui.separator();
                    if body.is_empty() {
                        // Pas de partie HTML : le texte tel que le service l'a extrait.
                        ui.label(&message.body);
                    } else {
                        // **Le corps est borné, contrairement à la liste qui est virtualisée.**
                        // Un bloc n'a pas de hauteur fixe, donc `show_rows` ne s'applique pas :
                        // il faudrait mettre en page chaque bloc pour connaître sa taille, ce
                        // qui est exactement le travail qu'on veut éviter. `mailhtml::blocks`
                        // plafonne à 20 000 blocs ; les construire tous à chaque image
                        // coûterait des dizaines de millisecondes sur un mail pathologique.
                        //
                        // Le plafond est donc ici, et il est **visible** : le lecteur sait
                        // qu'il manque quelque chose et peut en demander plus.
                        let shown = self.body_shown.min(body.len());
                        render_blocks(ui, &body[..shown]);
                        if shown < body.len() {
                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.weak(format!("{} blocs affichés sur {} —", shown, body.len()));
                                if ui.button("afficher la suite").clicked() {
                                    more = true;
                                }
                            });
                        }
                    }
                    if message.body_truncated {
                        ui.add_space(6.0);
                        ui.weak("[corps tronqué par le service]");
                    }
                    to_open = self.thread_pane(ui, message.row.id);
                });
        }

        // Remis en place, sauf si le dessin a demandé autre chose entre-temps — `body_actions`
        // peut relancer une ouverture, et c'est alors sa valeur qui doit rester.
        if self.message.is_none() {
            self.message = message;
            self.body = body;
        }
        if more {
            self.body_shown = self.body_shown.saturating_add(BODY_STEP);
        }

        // Après avoir remis le message en place : les trois fonctions le lisent.
        match reply {
            Some(Answer::Sender) => self.start_reply(false),
            Some(Answer::Everyone) => self.start_reply(true),
            Some(Answer::Forward) => self.start_forward(),
            Some(Answer::Source) => {
                if let Some(message) = &self.message {
                    let id = message.row.id;
                    let title = elide(&message.row.subject, 48);
                    self.open_source(crate::worker::SourceOf::Message(id), title);
                }
            }
            None => {}
        }
        if let Some(id) = to_open {
            self.open_message(id, false);
        }
    }

    /// Le fil de discussion du message ouvert, quand il en a un.
    ///
    /// Rendu **après** le corps et non avant : ce qu'on vient d'ouvrir est ce qu'on veut lire,
    /// et le fil est un moyen de naviguer, pas l'objet de la page.
    fn thread_pane(&self, ui: &mut egui::Ui, current: i64) -> Option<i64> {
        // Un fil d'un seul message est le message : l'afficher n'apprendrait rien.
        if self.thread.len() < 2 {
            return None;
        }
        let mut to_open = None;
        ui.add_space(10.0);
        ui.separator();
        ui.label(
            egui::RichText::new(format!("Fil — {} messages", self.thread.len()))
                .strong()
                .size(12.0),
        );
        for row in &self.thread {
            ui.horizontal(|ui| {
                ui.set_height(ROW_HEIGHT);
                let label = format!("{}  {}", short_date(row.date), elide(&row.subject, 44));
                let label = if row.id == current {
                    egui::RichText::new(label).strong()
                } else {
                    egui::RichText::new(label)
                };
                if ui.selectable_label(row.id == current, label).clicked() {
                    to_open = Some(row.id);
                }
            });
        }
        to_open
    }

    /// Les actions et les avertissements attachés au corps.
    fn body_actions(&mut self, ui: &mut egui::Ui, message: &dto::Message) {
        let Some(html) = &message.html else {
            return;
        };

        ui.horizontal_wrapped(|ui| {
            if html.blocked_images > 0 && !self.images_shown {
                if ui
                    .button(format!("Afficher les {} images", html.blocked_images))
                    .clicked()
                {
                    self.open_message(message.row.id, true);
                }
                ui.weak("elles déclencheront des requêtes vers leurs serveurs");
            }
        });

        if !html.trackers.is_empty() {
            ui.add_space(4.0);
            // L'hôte, jamais l'URL — `docs/PRIVACY.md`. Ce qui compte est « qui aurait été
            // prévenu », pas le jeton qui l'aurait identifié.
            let hosts: Vec<&str> = html
                .trackers
                .iter()
                .map(|tracker| tracker.host.as_str())
                .collect();
            ui.colored_label(
                theme::accent(ui.ctx()),
                format!(
                    "{} traceur(s) neutralisé(s) : {}",
                    html.trackers.len(),
                    hosts.join(", ")
                ),
            );
        }
    }

    /// Les raccourcis clavier.
    ///
    /// `j`/`k` et les flèches pour se déplacer, `Entrée` pour ouvrir, `/` pour chercher,
    /// `Échap` pour revenir au dossier. Les mêmes que la liste du front web.
    fn keyboard(&mut self, ctx: &egui::Context) {
        // Rien quand une saisie a le focus : sinon taper « j » dans la recherche déplacerait
        // la sélection.
        if ctx.egui_wants_keyboard_input() {
            return;
        }

        let (down, up, open, search, escape, page) = ctx.input(|input| {
            (
                input.key_pressed(egui::Key::J) || input.key_pressed(egui::Key::ArrowDown),
                input.key_pressed(egui::Key::K) || input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::Enter),
                input.key_pressed(egui::Key::Slash),
                input.key_pressed(egui::Key::Escape),
                i32::from(input.key_pressed(egui::Key::PageDown))
                    - i32::from(input.key_pressed(egui::Key::PageUp)),
            )
        });

        if search {
            self.focus_search = true;
        }
        if escape && self.results.is_some() {
            self.results = None;
            self.query.clear();
        }

        let step = if page != 0 { page * 20 } else { 0 };
        let delta = i32::from(down) - i32::from(up) + step;
        if delta != 0 {
            let rows = self.visible();
            let position = self
                .selected
                .and_then(|id| rows.iter().position(|row| row.id == id));
            let next = match position {
                Some(at) => i64::try_from(at).unwrap_or(0) + i64::from(delta),
                // Sans sélection, une flèche vers le bas prend la première ligne.
                None => 0,
            };
            let next = usize::try_from(next.max(0)).unwrap_or(0);
            if let Some(row) = rows.get(next) {
                let id = row.id;
                self.open_message(id, false);
            } else if next >= rows.len() {
                // Le clavier a dépassé ce qui est chargé : demander la suite plutôt que de
                // bloquer la sélection à la dernière ligne connue.
                self.request_page();
            }
        }
        if open && let Some(id) = self.selected {
            self.open_message(id, self.images_shown);
        }
    }

    /// Fait avancer le banc de mesure d'une image.
    fn advance_bench(&mut self, ctx: &egui::Context, delta: Option<f64>, work: Option<f64>) {
        ctx.request_repaint();
        let sample = delta.zip(work);

        match self.bench_state.phase {
            Phase::Waiting => {
                // Le banc attend que le plus gros dossier soit chargé en entier : défiler sur
                // des lignes vides mesurerait un dessin qu'on n'affiche jamais.
                let biggest = self
                    .folders
                    .iter()
                    .max_by_key(|it| it.total)
                    .map(|it| it.id);
                match biggest {
                    Some(id) if self.current != Some(id) => self.open_folder(id),
                    Some(_) if self.exhausted => {
                        tracing::info!("{MARK} coquille diag {} lignes chargées", self.rows.len());
                        self.bench_state.phase = Phase::Idle;
                    }
                    // **Un dossier qu'on est en train de remplir ne se charge jamais en
                    // entier.** Chaque `Changed` relance la pagination depuis la première page
                    // — c'est correct pour l'affichage, mais ça veut dire qu'« tout chargé »
                    // n'arrive qu'une fois les écritures finies.
                    //
                    // Sans cette patience, le banc du critère 4 ne défilait donc qu'**après**
                    // la moisson, et il mesurait le critère 2 sous un autre nom. Sur le corpus
                    // réel au repos, la pagination se termine en quelques secondes et cette
                    // borne ne change rien.
                    Some(_)
                        if self.started.elapsed() > LOAD_PATIENCE
                            && self.rows.len() >= MIN_SCROLL_ROWS =>
                    {
                        tracing::info!(
                            "{MARK} coquille diag {} lignes chargées, pagination inachevée : \
                             le store est en cours d'écriture",
                            self.rows.len()
                        );
                        self.bench_state.phase = Phase::Idle;
                    }
                    Some(_) => self.request_page(),
                    None => {}
                }
            }
            Phase::Idle => {
                if let Some(sample) = sample {
                    self.bench_state.idle.push(sample);
                }
                if self.bench_state.idle.len() >= IDLE_FRAMES {
                    self.bench_state.phase = Phase::Scrolling;
                }
            }
            Phase::Scrolling => {
                if let Some(sample) = sample {
                    self.bench_state.scrolling.push(sample);
                }
                #[allow(clippy::cast_precision_loss)]
                let distance = self.total() as f32 * ROW_HEIGHT;
                #[allow(clippy::cast_precision_loss)]
                let step = distance / SCROLL_FRAMES as f32;
                self.bench_state.offset += step;
                if self.bench_state.scrolling.len() >= SCROLL_FRAMES {
                    self.report_bench();
                    self.bench_state.phase = Phase::Done;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            // Les deux phases de la coupure appartiennent au banc du critère 9.
            Phase::WaitingDown | Phase::Probing | Phase::Opening | Phase::Typing => {}
            Phase::Pasting | Phase::AfterPaste | Phase::Done => {}
        }
    }

    /// Le banc du critère 5 : ouvrir un message depuis la liste, et chronométrer jusqu'au
    /// dessin.
    ///
    /// ## Ce que l'API ne mesurait pas
    ///
    /// Le critère 5 était relevé côté démon : 4,31 ms pour `messages.get` sur un corps HTML.
    /// C'est la moitié du trajet. L'autre moitié est ce que le critère dit vraiment —
    /// « ouverture d'un message **depuis la liste** » — et elle contient le passage par le fil
    /// d'API, le découpage du corps en blocs, et la mise en page de ces blocs.
    ///
    /// ## Où commence et où s'arrête le chronomètre
    ///
    /// Il part au moment où la demande est émise, comme si l'utilisateur venait de cliquer, et
    /// il s'arrête à la fin de l'image où le corps a été mis en page. La présentation à l'écran
    /// — le `present` et l'attente de la synchronisation verticale — n'y est pas.
    ///
    /// **C'est la même convention que le critère 2**, qui compte le travail par image et non la
    /// cadence du compositeur, et pour la même raison : ce qui est mesurable est ce dont on
    /// répond. Un budget de 50 ms laisse de toute façon trois images de marge à 60 Hz.
    ///
    /// ## Ce qui rendrait le relevé faux
    ///
    /// Ouvrir deux fois le même message : le deuxième passage lit un blob déjà décompressé et
    /// un cache de pages chaud. Le banc ouvre donc **trente messages distincts**, et compte les
    /// blocs dessinés — un p95 flatteur obtenu sur des messages vides se verrait à ce chiffre.
    /// La première ouverture est rapportée à part : elle paie ce que les suivantes trouvent
    /// déjà chaud.
    fn advance_open(&mut self, ctx: &egui::Context) {
        ctx.request_repaint();

        match self.bench_state.phase {
            // Une page suffit : trente ouvertures se prennent dans les cent premières lignes.
            Phase::Waiting => {
                let biggest = self
                    .folders
                    .iter()
                    .max_by_key(|it| it.total)
                    .map(|it| it.id);
                match biggest {
                    Some(id) if self.current != Some(id) => self.open_folder(id),
                    Some(_) if self.rows.len() >= OPEN_SAMPLES => {
                        self.bench_state.phase = Phase::Opening;
                    }
                    Some(_) if self.exhausted => {
                        tracing::warn!(
                            "{MARK} coquille diag {} lignes seulement : banc invalide",
                            self.rows.len()
                        );
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Some(_) => self.request_page(),
                    None => {}
                }
            }
            Phase::Opening => {
                match self.bench_state.open_target {
                    // Une ouverture est en vol. `absorb` a tourné avant `panes` dans cette
                    // image : si le message attendu est là et que rien n'est plus en vol, son
                    // corps vient d'être mis en page, ici, à l'instant.
                    Some(target) => {
                        // Chaque image passée à attendre est comptée, celle-ci comprise. Une
                        // ouverture servie à l'image suivante vaut donc 1.
                        self.bench_state.open_waited += 1;
                        if self.opening {
                            return;
                        }
                        let drawn = self
                            .message
                            .as_ref()
                            .is_some_and(|it| it.row.id == target && !self.body.is_empty());
                        if let Some(at) = self.bench_state.open_at {
                            if drawn {
                                self.bench_state
                                    .opens
                                    .push(at.elapsed().as_secs_f64() * 1000.0);
                                self.bench_state.open_blocks += self.body.len();
                                #[allow(clippy::cast_precision_loss)]
                                self.bench_state
                                    .open_frames
                                    .push(self.bench_state.open_waited as f64);
                            } else {
                                // Ni corps ni message : rien n'a été dessiné, donc il n'y a pas
                                // de délai d'affichage. Compté, pas mélangé.
                                self.bench_state.open_empty += 1;
                            }
                        }
                        self.bench_state.open_at = None;
                        self.bench_state.open_target = None;
                    }
                    // Rien en vol : lancer la suivante, ou refermer.
                    None => {
                        let done = self.bench_state.opens.len() + self.bench_state.open_empty;
                        match self.rows.get(self.bench_state.open_index).map(|it| it.id) {
                            Some(id) if done < OPEN_SAMPLES => {
                                self.bench_state.open_index += 1;
                                self.bench_state.open_target = Some(id);
                                self.bench_state.open_at = Some(Instant::now());
                                self.bench_state.open_waited = 0;
                                self.open_message(id, false);
                            }
                            _ => {
                                self.report_open();
                                self.bench_state.phase = Phase::Done;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                    }
                }
            }
            Phase::Idle | Phase::Scrolling | Phase::WaitingDown | Phase::Probing => {}
            Phase::Typing | Phase::Pasting | Phase::AfterPaste | Phase::Done => {}
        }
    }

    /// Le banc du **critère 5 de la phase 3** : taper dans l'éditeur de signature livré.
    ///
    /// ## Pourquoi il existe alors que la sonde a déjà répondu
    ///
    /// La sonde `mail-spike-richtext` a mesuré la **faisabilité** : un modèle simple met en page
    /// 9 800 glyphes en quatorze microsecondes. Elle ne mesurait pas l'éditeur, qui n'existait
    /// pas — ni son layouter, ni le rattrapage du document sur le tampon à chaque image, ni le
    /// `TextEdit` qui gère curseur et sélection par-dessus. Ce banc mesure ce qui est livré.
    ///
    /// ## Les trois régimes, et ce que chacun dit
    ///
    /// - **au repos** — l'éditeur est ouvert sur 200 lignes stylées, rien ne bouge. `egui` met
    ///   en cache la mise en page d'un `LayoutJob` par son empreinte : ce régime doit donc être
    ///   quasi gratuit, et s'il ne l'est pas, c'est que le layouter reconstruit un job différent
    ///   à chaque image — et le relevé de frappe ne voudrait plus rien dire ;
    /// - **en frappe** — un caractère inséré au milieu du tampon par image. C'est le pire cas
    ///   réel : le texte change, donc l'empreinte change, donc les 200 lignes sont remises en
    ///   page. C'est ce régime que le critère borne ;
    /// - **après un collage** de 50 Ko de HTML — le second régime du critère, celui qui demande
    ///   que l'interface ne fige pas.
    ///
    /// Les contrôles vont dans le relevé, parce que sans eux il est plausible et pas
    /// interprétable : le nombre de glyphes et le nombre d'intervalles stylés. Un p95 obtenu sur
    /// trois lignes serait indiscernable d'un vrai.
    fn advance_signature(&mut self, ctx: &egui::Context, delta: Option<f64>, work: Option<f64>) {
        ctx.request_repaint();
        let sample = delta.zip(work);

        match self.bench_state.phase {
            Phase::Waiting => {
                self.editor = Some(crate::signature::Editor::for_bench(SIGNATURE_LINES));
                self.bench_state.phase = Phase::Idle;
            }
            Phase::Idle => {
                if let Some(sample) = sample {
                    self.bench_state.idle.push(sample);
                }
                if self.bench_state.idle.len() >= IDLE_FRAMES {
                    self.bench_state.phase = Phase::Typing;
                }
            }
            Phase::Typing => {
                if let Some(sample) = sample {
                    self.bench_state.scrolling.push(sample);
                }
                if let Some(editor) = self.editor.as_mut() {
                    // Un caractère accentué : sur un corpus français, c'est le cas ordinaire, et
                    // c'est celui dont les frontières d'octets coûtent quelque chose.
                    editor.type_in_the_middle('é');
                }
                if self.bench_state.scrolling.len() >= SIGNATURE_FRAMES {
                    self.bench_state.phase = Phase::Pasting;
                }
            }
            Phase::Pasting => {
                // Le coût de la conversion, mesuré directement : c'est le travail que le collage
                // ajoute, et il ne dépend pas de la cadence de l'écran.
                let html = pasted_html();
                let at = Instant::now();
                if let Some(editor) = self.editor.as_mut() {
                    editor.paste(&html);
                }
                self.bench_state.paste_ms = Some(at.elapsed().as_secs_f64() * 1000.0);
                self.bench_state.paste_bytes = html.len();
                self.bench_state.phase = Phase::AfterPaste;
            }
            Phase::AfterPaste => {
                // Les images **après** le collage : c'est là que se joue « ne doit pas figer ».
                // La conversion est finie ; ce qui reste est la mise en page du document collé.
                if let Some(sample) = sample {
                    self.bench_state.after_paste.push(sample);
                }
                if self.bench_state.after_paste.len() >= IDLE_FRAMES {
                    self.report_signature();
                    self.bench_state.phase = Phase::Done;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Phase::Scrolling
            | Phase::WaitingDown
            | Phase::Probing
            | Phase::Opening
            | Phase::Done => {}
        }
    }

    /// Écrit le relevé du critère 5 de la phase 3.
    fn report_signature(&self) {
        let typing: Vec<f64> = self.bench_state.scrolling.iter().map(|it| it.0).collect();
        let work: Vec<f64> = self.bench_state.scrolling.iter().map(|it| it.1).collect();
        let idle: Vec<f64> = self.bench_state.idle.iter().map(|it| it.0).collect();
        let after: Vec<f64> = self.bench_state.after_paste.iter().map(|it| it.0).collect();
        let after_work: Vec<f64> = self.bench_state.after_paste.iter().map(|it| it.1).collect();
        let (glyphes, styles) = self
            .editor
            .as_ref()
            .map_or((0, 0), |it| (it.glyphs(), it.styles()));

        tracing::info!(
            "{MARK} coquille critere=5p3 lignes={SIGNATURE_LINES} images={} glyphes={glyphes} \
             styles={styles} repos={:.2} p50={:.2} p95={:.2} pire={:.2} travail_p50={:.2} \
             travail_p95={:.2} travail_pire={:.2} collage_octets={} collage={:.2} \
             apres_p95={:.2} apres_pire={:.2} apres_travail_p95={:.2}",
            typing.len(),
            percentile(&idle, 0.5),
            percentile(&typing, 0.5),
            percentile(&typing, 0.95),
            percentile(&typing, 1.0),
            percentile(&work, 0.5),
            percentile(&work, 0.95),
            percentile(&work, 1.0),
            self.bench_state.paste_bytes,
            self.bench_state.paste_ms.unwrap_or(0.0),
            percentile(&after, 0.95),
            percentile(&after, 1.0),
            percentile(&after_work, 0.95),
        );
    }

    /// Écrit le relevé du critère 5.
    fn report_open(&self) {
        let opens = &self.bench_state.opens;
        let served = &self.bench_state.open_served;
        let frames = &self.bench_state.open_frames;
        tracing::info!(
            "{MARK} coquille critere=5 ouvertures={} vides={} blocs={} premiere={:.2} \
             p50={:.2} p95={:.2} pire={:.2} service_premiere={:.2} service_p50={:.2} \
             service_p95={:.2} service_pire={:.2} images_p50={:.1} images_pire={:.1}",
            opens.len(),
            self.bench_state.open_empty,
            self.bench_state.open_blocks,
            opens.first().copied().unwrap_or(0.0),
            percentile(opens, 0.5),
            percentile(opens, 0.95),
            percentile(opens, 1.0),
            served.first().copied().unwrap_or(0.0),
            percentile(served, 0.5),
            percentile(served, 0.95),
            percentile(served, 1.0),
            percentile(frames, 0.5),
            percentile(frames, 1.0),
        );
    }

    /// Le banc du critère 9 : charger, subir la coupure, vérifier ce qui marche encore.
    ///
    /// ## Ce qu'il vérifie exactement
    ///
    /// Le critère dit : « la liste déjà chargée reste défilable, la recherche et l'ouverture
    /// d'un message échouent proprement avec un état visible, rien ne gèle et rien ne ment ».
    /// Chacun de ces quatre mots se mesure :
    ///
    /// 1. **déjà chargée** — le banc charge un bon millier de lignes avant la coupure ;
    /// 2. **reste défilable** — il défile 600 images après la coupure et relève le travail par
    ///    image, comme le banc du critère 2 ;
    /// 3. **échouent proprement** — il tente une ouverture et une recherche, et classe les
    ///    deux réponses : `echec_propre`, `refus`, ou `servi` ;
    /// 4. **rien ne gèle** — si l'interface bloquait, aucune de ces images n'existerait et le
    ///    relevé ne sortirait pas.
    ///
    /// La coupure elle-même est faite par l'outillage, qui tue le démon en voyant le jalon
    /// `etape=charge`. C'est le seul moyen honnête : un service qu'on coupe soi-même de
    /// l'intérieur ne prouve rien.
    fn advance_offline(&mut self, ctx: &egui::Context, delta: Option<f64>, work: Option<f64>) {
        /// Lignes chargées avant la coupure. Assez pour que le défilement porte sur du vrai
        /// contenu, pas assez pour que le chargement domine la durée du banc.
        const ROWS: usize = 1_000;

        ctx.request_repaint();
        let sample = delta.zip(work);

        match self.bench_state.phase {
            Phase::Waiting => {
                let biggest = self
                    .folders
                    .iter()
                    .max_by_key(|it| it.total)
                    .map(|it| it.id);
                match biggest {
                    Some(id) if self.current != Some(id) => self.open_folder(id),
                    Some(_) if self.rows.len() >= ROWS || self.exhausted => {
                        // L'outillage attend ce jalon pour couper le service.
                        tracing::info!("{MARK} coquille etape=charge lignes={}", self.rows.len());
                        self.bench_state.phase = Phase::WaitingDown;
                    }
                    Some(_) => self.request_page(),
                    None => {}
                }
            }
            Phase::WaitingDown => {
                // L'abonnement découvre la coupure tout seul, en deux secondes au plus.
                if self.offline.is_some() {
                    let target = self.rows.first().map(|row| row.id);
                    self.bench_state.phase = Phase::Probing;
                    if let Some(id) = target {
                        self.worker.ask(Request::Open {
                            id,
                            remote_images: false,
                        });
                    } else {
                        self.probe.open = Some("aucune ligne");
                    }
                    self.worker.ask(Request::Search {
                        query: "facture".to_owned(),
                        limit: RESULTS,
                    });
                }
            }
            Phase::Probing => {
                if self.probe.complete() {
                    // Les séries repartent de zéro : ce qui compte est le défilement **après**
                    // la coupure.
                    self.bench_state.idle.clear();
                    self.bench_state.scrolling.clear();
                    self.bench_state.phase = Phase::Scrolling;
                }
            }
            Phase::Scrolling => {
                if let Some(sample) = sample {
                    self.bench_state.scrolling.push(sample);
                }
                // Le défilement porte sur ce qui est **chargé**, pas sur le total du dossier :
                // le critère parle de ce qui est en cache.
                #[allow(clippy::cast_precision_loss)]
                let distance = self.rows.len() as f32 * ROW_HEIGHT;
                #[allow(clippy::cast_precision_loss)]
                let step = distance / SCROLL_FRAMES as f32;
                self.bench_state.offset += step;
                if self.bench_state.scrolling.len() >= SCROLL_FRAMES {
                    self.report_offline();
                    self.bench_state.phase = Phase::Done;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Phase::Idle | Phase::Opening | Phase::Done => {}
            Phase::Typing | Phase::Pasting | Phase::AfterPaste => {}
        }
    }

    /// Écrit le relevé du critère 9.
    fn report_offline(&self) {
        let deltas: Vec<f64> = self.bench_state.scrolling.iter().map(|it| it.0).collect();
        let work: Vec<f64> = self.bench_state.scrolling.iter().map(|it| it.1).collect();

        tracing::info!(
            "{MARK} coquille critere=9 lignes={} images={} hors_ligne={} ouverture={} \
             recherche={} p50={:.2} p95={:.2} travail_p50={:.2} travail_p95={:.2} \
             travail_pire={:.2}",
            self.rows.len(),
            deltas.len(),
            u8::from(self.offline.is_some()),
            self.probe.open.unwrap_or("sans réponse"),
            self.probe.search.unwrap_or("sans réponse"),
            percentile(&deltas, 0.5),
            percentile(&deltas, 0.95),
            percentile(&work, 0.5),
            percentile(&work, 0.95),
            percentile(&work, 1.0),
        );
    }

    /// Écrit le relevé du critère 2, dans le format des sondes.
    fn report_bench(&self) {
        let deltas: Vec<f64> = self.bench_state.scrolling.iter().map(|it| it.0).collect();
        let work: Vec<f64> = self.bench_state.scrolling.iter().map(|it| it.1).collect();
        let idle_deltas: Vec<f64> = self.bench_state.idle.iter().map(|it| it.0).collect();
        let idle_work: Vec<f64> = self.bench_state.idle.iter().map(|it| it.1).collect();

        tracing::info!(
            "{MARK} coquille critere=2 lignes={} images={} changements={} repos={:.2} \
             p50={:.2} p95={:.2} pire={:.2} travail_repos={:.2} travail_p50={:.2} \
             travail_p95={:.2} travail_pire={:.2}",
            self.rows.len(),
            deltas.len(),
            self.bench_state.changed_while_scrolling,
            percentile(&idle_deltas, 0.5),
            percentile(&deltas, 0.5),
            percentile(&deltas, 0.95),
            percentile(&deltas, 1.0),
            percentile(&idle_work, 0.5),
            percentile(&work, 0.5),
            percentile(&work, 0.95),
            percentile(&work, 1.0),
        );
    }
}

/// La date civile du démarrage, pour savoir si une date de liste est de l'année en cours.
///
/// Une application ouverte à travers un réveillon affichera l'année sur les messages du
/// lendemain. C'est le prix d'un calcul fait une fois, et il est dérisoire à côté de deux mille
/// lectures d'horloge par seconde.
static CURRENT_YEAR: std::sync::OnceLock<(i64, u32, u32, u32, u32)> = std::sync::OnceLock::new();

/// Le percentile d'une série, par interpolation basse.
///
/// Une série vide rend zéro : un banc qui n'a rien relevé doit sortir un chiffre qu'on
/// reconnaît comme absent, pas paniquer sur un index.
fn percentile(series: &[f64], fraction: f64) -> f64 {
    if series.is_empty() {
        return 0.0;
    }
    let mut sorted = series.to_vec();
    sorted.sort_by(f64::total_cmp);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rank = ((fraction * sorted.len() as f64) as usize).min(sorted.len() - 1);
    sorted[rank]
}

/// Une ligne de liste. Rend vrai si elle vient d'être activée.
fn row_widget(ui: &mut egui::Ui, row: &dto::Row, selected: bool) -> bool {
    let mut clicked = false;
    let row_area = ui.horizontal(|ui| {
        ui.set_height(ROW_HEIGHT);
        let strong = row.unread;

        let response = ui.selectable_label(selected, {
            let date = short_date(row.date);
            if strong {
                egui::RichText::new(date).strong()
            } else {
                egui::RichText::new(date).weak()
            }
        });
        clicked |= response.clicked();

        let who = row
            .from_name
            .as_deref()
            .filter(|it| !it.trim().is_empty())
            .unwrap_or(&row.from);
        let sender = egui::RichText::new(elide(who, 28));
        let response = ui.add_sized(
            [190.0, ROW_HEIGHT],
            egui::Label::new(if strong { sender.strong() } else { sender })
                .truncate()
                .selectable(false),
        );
        clicked |= response.clicked();

        let subject = egui::RichText::new(&row.subject);
        let response = ui.add(
            egui::Label::new(if strong { subject.strong() } else { subject })
                .truncate()
                .selectable(false),
        );
        clicked |= response.clicked();

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if row.flagged {
                ui.label("★");
            }
            if row.has_attachments {
                ui.label("📎");
            }
        });
    });

    // **La ligne entière est cliquable, et c'est ce qui manquait.**
    //
    // Un `egui::Label` n'est pas interactif : son `clicked()` rend toujours `false`. Seule la
    // date, qui est un `selectable_label` — donc un bouton — recevait les clics. Cliquer sur
    // l'objet ou sur l'expéditeur, c'est-à-dire sur les neuf dixièmes de la ligne, ne faisait
    // rien du tout. Signalé le 2026-09-08, à la première utilisation réelle de la coquille.
    //
    // `interact` réenregistre le rectangle de la ligne avec une intention de clic, ce qui
    // couvre aussi les blancs entre les colonnes — personne ne vise un mot.
    clicked |= row_area.response.interact(egui::Sense::click()).clicked();
    clicked
}

/// L'en-tête d'un message ouvert.
fn header(ui: &mut egui::Ui, message: &dto::Message) {
    ui.label(
        egui::RichText::new(&message.row.subject)
            .strong()
            .size(15.0),
    );
    ui.add_space(2.0);
    let who = message
        .row
        .from_name
        .as_deref()
        .filter(|it| !it.trim().is_empty())
        .map_or_else(
            || message.row.from.clone(),
            |name| format!("{name} <{}>", message.row.from),
        );
    ui.weak(who);
    if !message.to.is_empty() {
        ui.weak(format!("à {}", message.to.join(", ")));
    }
    ui.weak(long_date(message.row.date));
    if !message.folders.is_empty() {
        let paths: Vec<&str> = message.folders.iter().map(|it| it.path.as_str()).collect();
        ui.weak(format!("dans {}", paths.join(", ")));
    }
    if !message.attachments.is_empty() {
        // Décrites, jamais ouvertes en phase 1.
        for attachment in &message.attachments {
            ui.weak(format!(
                "📎 {} — {} ({} o)",
                attachment.name.as_deref().unwrap_or("sans nom"),
                attachment.mime,
                attachment.size
            ));
        }
    }
}

/// Dessine les blocs d'un corps.
/// Le bouton qui ouvre la source, avec la même étiquette partout.
///
/// Une seule fonction plutôt que trois littéraux : trois emplacements finiraient par porter
/// trois formulations, et l'utilisateur ne reconnaîtrait pas que c'est la même chose.
fn source_button(ui: &mut egui::Ui) -> egui::Response {
    ui.small_button("⛭ Source")
        .on_hover_text("Les octets du message, tels qu'ils sont : rien n'est décodé")
}

/// Le contenu de la fenêtre de source : les réserves, puis les en-têtes, puis le corps.
///
/// Les réserves passent **avant** ce qu'elles qualifient. Une troncature annoncée sous mille
/// lignes n'est jamais lue, et c'est pourtant elle qui dit ce que la vue ne montre pas.
fn source_body(ui: &mut egui::Ui, source: &dto::MessageSource) {
    ui.horizontal_wrapped(|ui| {
        ui.weak(mailapi::human::bytes(source.total));
        if source.invalid_sequences > 0 {
            ui.colored_label(
                theme::accent(ui.ctx()),
                format!(
                    "{} séquence(s) non UTF-8, rendues par « \u{fffd} »",
                    source.invalid_sequences
                ),
            );
        }
        if source.escaped_controls > 0 {
            ui.colored_label(
                theme::accent(ui.ctx()),
                format!(
                    "{} caractère(s) de contrôle rendus visibles",
                    source.escaped_controls
                ),
            );
        }
        if source.headers_truncated {
            ui.colored_label(theme::accent(ui.ctx()), "en-têtes tronqués");
        }
    });
    ui.separator();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Un `TextEdit` en lecture seule plutôt qu'un label : il donne la sélection et la
            // copie, qui sont la moitié de l'intérêt d'une vue de vérification — on veut
            // pouvoir coller un en-tête ailleurs.
            monospace(ui, &source.headers);
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            monospace(ui, &source.body);
            if source.body_truncated {
                ui.add_space(4.0);
                ui.colored_label(
                    theme::accent(ui.ctx()),
                    format!(
                        "corps tronqué — le message fait {}",
                        mailapi::human::bytes(source.total)
                    ),
                );
            }
        });
}

/// Un bloc de texte à chasse fixe, sélectionnable et non modifiable.
fn monospace(ui: &mut egui::Ui, text: &str) {
    // `TextEdit` veut une `String` en écriture. La copie est refaite à chaque image depuis la
    // réponse, donc une frappe disparaît aussitôt : le champ donne la sélection et la copie
    // sans rien laisser modifier. C'est le seul moyen d'avoir l'une sans l'autre dans egui.
    let mut shown = text.to_owned();
    ui.add(
        egui::TextEdit::multiline(&mut shown)
            .font(egui::TextStyle::Monospace)
            .desired_width(f32::INFINITY)
            .interactive(true),
    );
}

fn render_blocks(ui: &mut egui::Ui, body: &[Block]) {
    for block in body {
        match &block.kind {
            Kind::Rule => {
                ui.separator();
            }
            Kind::Image { alt, source } => {
                // **Annoncée, jamais chargée.** C'est la garantie de `docs/PRIVACY.md` obtenue
                // par construction : cette coquille n'a aucun code capable d'aller chercher
                // une ressource distante.
                let label = match (alt.trim().is_empty(), source) {
                    (true, Some(Source::Embedded)) => "[image embarquée]".to_owned(),
                    (true, Some(Source::Remote)) => "[image distante, non chargée]".to_owned(),
                    (true, None) => "[image bloquée]".to_owned(),
                    (false, Some(Source::Embedded)) => format!("[image : {alt}]"),
                    (false, Some(Source::Remote)) => format!("[image distante : {alt}]"),
                    (false, None) => format!("[image bloquée : {alt}]"),
                };
                ui.weak(label);
            }
            Kind::Heading(level) => {
                ui.add_space(4.0);
                let size = match level {
                    1 => 17.0,
                    2 => 15.0,
                    _ => 13.5,
                };
                paragraph(ui, block, Some(size), true, 0.0);
            }
            Kind::Quote(depth) => {
                #[allow(clippy::cast_precision_loss)]
                let indent = f32::from(*depth) * 10.0;
                paragraph(ui, block, None, false, indent);
            }
            Kind::Item { depth, ordered } => {
                let indent = f32::from(*depth) * 12.0;
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(indent);
                    ui.weak(if *ordered { "—" } else { "•" });
                    runs(ui, block);
                });
            }
            Kind::Pre => paragraph(ui, block, None, false, 0.0),
            Kind::Paragraph => paragraph(ui, block, None, false, 0.0),
        }
    }
}

/// Un bloc de texte, avec son indentation et ses fragments.
fn paragraph(ui: &mut egui::Ui, block: &Block, size: Option<f32>, strong: bool, indent: f32) {
    ui.horizontal_wrapped(|ui| {
        if indent > 0.0 {
            ui.add_space(indent);
            // La barre de citation : un filet, pas une couleur de fond — la hiérarchie passe
            // par l'espacement.
            ui.weak("│");
        }
        for run in &block.runs {
            ui.add(egui::Label::new(styled(run, size, strong)));
        }
    });
}

/// Les fragments d'un bloc, sans indentation.
fn runs(ui: &mut egui::Ui, block: &Block) {
    for run in &block.runs {
        ui.add(egui::Label::new(styled(run, None, false)));
    }
}

/// Applique le style d'un fragment.
///
/// Un lien est **souligné et coloré, et rien de plus** : il n'est pas cliquable ici. C'est
/// délibéré — un clic ouvrirait une URL écrite par l'expéditeur, ce qui est exactement la
/// classe d'action qui doit rester explicite.
///
/// C'est aussi, reconnaissons-le, un manque : un lien qu'on ne peut pas suivre du tout est une
/// fonctionnalité absente, pas une protection. Ce qu'il faut est un clic qui **montre l'URL et
/// demande confirmation** avant d'ouvrir. Il a existé une échappatoire « ouvrir dans le
/// navigateur » qui donnait accès aux liens en écrivant le corps assaini dans `%TEMP%` ; elle a
/// été retirée le 2026-09-08, parce qu'elle laissait sur le disque, en clair et pour toujours,
/// le corps du dernier message lu.
fn styled(run: &mailhtml::blocks::Run, size: Option<f32>, strong: bool) -> egui::RichText {
    let mut text = egui::RichText::new(&run.text);
    let Style {
        bold,
        italic,
        code,
        strike,
        link,
    } = &run.style;
    if let Some(size) = size {
        text = text.size(size);
    }
    if *bold || strong {
        text = text.strong();
    }
    if *italic {
        text = text.italics();
    }
    if *code {
        text = text.code();
    }
    if *strike {
        text = text.strikethrough();
    }
    if link.is_some() {
        text = text.underline();
    }
    text
}

/// Les 50 Ko de HTML du banc de collage.
///
/// ## Pourquoi il est fabriqué et pas lu d'un fichier
///
/// Le critère parle d'« un collage de 50 Ko de HTML depuis un navigateur ». Un fichier
/// d'échantillon serait une pièce de plus à garder à jour, et surtout : ce qui coûte est le
/// **nombre de fragments stylés**, pas la provenance des octets. Le motif répété en porte trois
/// par ligne — un gras, un lien, du texte courant — ce qui donne un document dense en
/// changements de style plutôt qu'un mur de texte qui flatterait le relevé.
fn pasted_html() -> String {
    let line = "<p>Une ligne de signature avec un <b>mot en gras</b> et un \
                <a href=\"https://exemple.fr\">lien</a>, puis du texte courant.</p>";
    line.repeat(50_000 / line.len() + 1)
}

/// Un rendez-vous, dessiné comme un rendez-vous.
///
/// ## Ce que ce cadre montre, et dans quel ordre
///
/// Quand, où, avec qui — l'ordre des questions qu'on se pose en recevant une invitation. Le
/// titre d'abord, parce que c'est ce qui permet de reconnaître la réunion dont on parle.
///
/// ## Les réserves sont affichées **à côté** de l'heure, jamais à sa place
///
/// C'est la moitié du critère 6. Une invitation à heure flottante s'affiche « 14:00 » avec la
/// phrase qui dit que ce 14:00 est celui du lecteur ; une invitation sans début lisible
/// n'affiche pas d'heure du tout et dit pourquoi. La panne qu'on évite est un rendez-vous
/// affiché à une heure qui n'est pas la bonne, sans que rien ne le signale.
///
/// ## Aucune requête ne part d'ici
///
/// Les URL de l'invitation sont **listées**, pas suivies : `docs/PRIVACY.md`, règle 5. Comme
/// les liens du corps, elles sont soulignées et non cliquables — ouvrir une adresse écrite par
/// l'organisateur est une action de l'utilisateur, et elle demandera un clic explicite le jour
/// où le volet de lecture saura le faire.
fn appointment(ui: &mut egui::Ui, invitation: &dto::Invitation) {
    let annulé = invitation.kind == "annulation"
        || invitation
            .status
            .as_deref()
            .is_some_and(|it| it.eq_ignore_ascii_case("CANCELLED"));

    ui.horizontal_wrapped(|ui| {
        // Le genre de pièce, en tête : une annulation et une invitation se ressemblent trop
        // pour être distinguées par leur contenu, et les confondre fait aller à une réunion
        // qui n'a pas lieu.
        let label = if annulé {
            egui::RichText::new("✖ Annulé")
                .strong()
                .color(ui.visuals().error_fg_color)
        } else {
            egui::RichText::new(format!("📅 {}", invitation.kind)).strong()
        };
        ui.label(label);
        if let Some(summary) = &invitation.summary {
            ui.label(egui::RichText::new(summary).strong());
        }
    });

    egui::Grid::new("rendez-vous")
        .num_columns(2)
        .spacing([10.0, 3.0])
        .show(ui, |ui| {
            if let Some(start) = &invitation.start_wall {
                ui.weak("Quand");
                ui.vertical(|ui| {
                    // L'heure de fin sur la même ligne quand il y en a une : « 14:00 → 15:00 »
                    // se lit d'un coup d'œil, deux lignes non.
                    let span = match &invitation.end_wall {
                        Some(end) => format!("{start} → {end}"),
                        None => start.clone(),
                    };
                    ui.label(span);
                    if let Some(zone) = &invitation.zone {
                        // Le fuseau est **toujours** affiché, même quand c'est le nôtre : une
                        // heure sans fuseau est ce qui fait rater un rendez-vous, et
                        // l'utilisateur n'a aucun moyen de savoir lequel a été appliqué.
                        ui.weak(zone);
                    }
                });
                ui.end_row();
            }
            if let Some(location) = &invitation.location {
                ui.weak("Où");
                ui.label(location);
                ui.end_row();
            }
            if let Some(organizer) = &invitation.organizer {
                ui.weak("Organisé par");
                ui.label(who(organizer));
                ui.end_row();
            }
            if !invitation.attendees.is_empty() {
                ui.weak("Participants");
                ui.vertical(|ui| {
                    // Bornés : une invitation d'entreprise en porte des dizaines, et le corpus
                    // en a une à 8 150 pour 930 pièces. Le compte total est dit.
                    let shown = invitation.attendees.len().min(ATTENDEES_SHOWN);
                    for attendee in &invitation.attendees[..shown] {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(who(attendee));
                            // La réponse de chacun, en gris : c'est ce qui dit si la réunion
                            // aura du monde, et ce n'est pas ce qu'on lit en premier.
                            ui.weak(format!("— {}", attendee.answer));
                            if !attendee.required {
                                ui.weak("(optionnel)");
                            }
                        });
                    }
                    if invitation.attendees.len() > shown {
                        ui.weak(format!(
                            "… et {} autres",
                            invitation.attendees.len() - shown
                        ));
                    }
                });
                ui.end_row();
            }
            if let Some(rule) = &invitation.recurrence {
                ui.weak("Répétition");
                // **La règle telle qu'écrite, jamais interprétée.** « FREQ=WEEKLY;BYDAY=TH »
                // n'est pas joli, et c'est le prix de ne pas déduire une occurrence de travers
                // — ce qui déplacerait un rendez-vous. Traduire la règle en français est un
                // travail à part, qui commence par savoir laquelle des vingt formes on gère.
                ui.label(egui::RichText::new(rule).monospace().small())
                    .on_hover_text("La règle du fichier, affichée telle quelle : aucune occurrence n'est calculée.");
                ui.end_row();
            }
        });

    if !invitation.urls.is_empty() {
        ui.add_space(2.0);
        ui.weak("Liens de l'invitation (non suivis) :");
        for url in &invitation.urls {
            // Soulignés et **pas cliquables**, comme les liens du corps : la même règle, pour
            // la même raison. Voir `styled`.
            ui.label(
                egui::RichText::new(elide(url, 110))
                    .underline()
                    .color(ui.visuals().hyperlink_color),
            )
            .on_hover_text(url.as_str());
        }
    }

    if !invitation.caveats.is_empty() {
        ui.add_space(2.0);
        for caveat in &invitation.caveats {
            // En couleur d'avertissement, et jamais en erreur : le fichier n'est pas invalide,
            // il est incomplet. La nuance est ce qui permet à quelqu'un de demander la bonne
            // chose à l'organisateur.
            ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {caveat}"));
        }
    }

    if invitation.extra_events > 0 {
        ui.weak(format!(
            "Cette pièce porte {} rendez-vous de plus, non affichés.",
            invitation.extra_events
        ));
    }
}

/// Un participant, tel qu'on l'affiche : son nom, et son adresse quand elle ajoute quelque
/// chose.
///
/// Le nom seul ne permet pas de distinguer deux homonymes, et l'adresse seule est illisible
/// quand un nom existe. Les deux, donc — sauf quand le nom **est** l'adresse, cas fréquent des
/// serveurs qui n'envoient pas de `CN`.
fn who(person: &dto::Participant) -> String {
    match (person.name.as_deref(), person.address.as_deref()) {
        (Some(name), Some(address)) if name != address => format!("{name} <{address}>"),
        (Some(name), _) => name.to_owned(),
        (None, Some(address)) => address.to_owned(),
        (None, None) => "(inconnu)".to_owned(),
    }
}
/// L'aperçu d'une signature, telle qu'elle partira.
///
/// ## Pourquoi elle est **rendue** et pas montrée en texte brut
///
/// Parce que ce qui part porte des styles, et qu'un aperçu qui les cacherait ne dirait pas la
/// vérité sur ce que le destinataire verra. C'est la même exigence que « rien de ce qu'on ajoute
/// au message n'est invisible à l'utilisateur » : la montrer sans ses gras ni ses liens serait
/// la montrer à moitié.
///
/// La puce, elle, est **écrite** ici — « • » — là où l'éditeur ne peut que l'indenter : un
/// aperçu n'a pas de curseur à faire correspondre au texte, donc rien n'empêche d'ajouter le
/// caractère qui rend la liste lisible.
fn signature_preview(ui: &mut egui::Ui, signature: &mailhtml::rich::Document) {
    let mut at = 0usize;
    for (rank, line) in signature.text.split('\n').enumerate() {
        let start = at;
        // `+ 1` pour le saut de ligne que le découpage a consommé.
        at += line.len() + 1;
        let bullet = signature.blocks.get(rank) == Some(&mailhtml::rich::Block::Bullet);
        // Une ligne vide n'a pas de fragment : sans ce cas, les lignes blanches de la signature
        // disparaîtraient de l'aperçu alors qu'elles partent bien dans le message.
        if line.is_empty() {
            ui.label(" ");
            continue;
        }
        let at = start;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if bullet {
                ui.weak("• ");
            }
            for fragment in signature.fragments(at, at + line.len()) {
                let mut text = egui::RichText::new(fragment.text);
                if fragment.style.bold {
                    text = text.strong();
                }
                if fragment.style.italic {
                    text = text.italics();
                }
                if fragment.style.link.is_some() {
                    // Souligné, et **pas cliquable** : la même règle que dans le volet de
                    // lecture, pour la même raison. Ici la cible a été écrite par
                    // l'utilisateur, mais un aperçu qui ouvre un navigateur serait une action
                    // déclenchée par un affichage.
                    text = text.underline().color(ui.visuals().hyperlink_color);
                }
                ui.label(text);
            }
        });
    }
}

/// L'écran d'arrêt : un désaccord de contrat n'est pas rattrapable côté client.
fn fatal(ui: &mut egui::Ui, message: &str) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.label(egui::RichText::new("Incompatible").strong().size(18.0));
            ui.add_space(8.0);
            ui.colored_label(ui.visuals().error_fg_color, message);
            ui.add_space(8.0);
            ui.weak("Rien n'a été lu ni écrit.");
        });
    });
}

/// Coupe une chaîne à `max` caractères, sur une frontière de caractère.
fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

/// Une date courte pour une ligne de liste.
fn short_date(unix: i64) -> String {
    // Pas de dépendance de calendrier : `time` ou `chrono` pour afficher une date de liste
    // serait un arbre de dépendances de plus sur le chemin du démarrage. Le calcul civil
    // grégorien tient en vingt lignes et ne dépend de rien.
    let (year, month, day, _, _) = civil(unix);
    // L'année courante est calculée **une fois par processus**, pas une fois par ligne. La
    // première version relisait l'horloge du système pour chacune des quarante lignes visibles,
    // à chaque image — deux mille quatre cents appels par seconde pour un nombre qui change une
    // fois l'an.
    let now = *CURRENT_YEAR.get_or_init(|| {
        civil(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |it| i64::try_from(it.as_secs()).unwrap_or(0)),
        )
    });
    if unix <= 0 {
        // 102 messages du corpus réel n'ont pas de date exploitable dans leur en-tête. Un
        // tiret est plus honnête qu'une date inventée à partir de zéro.
        return "—".to_owned();
    }
    if year == now.0 {
        format!("{day:02}/{month:02}")
    } else {
        format!("{day:02}/{month:02}/{:02}", year % 100)
    }
}

/// Une date complète pour l'en-tête d'un message.
fn long_date(unix: i64) -> String {
    if unix <= 0 {
        return "date inconnue".to_owned();
    }
    let (year, month, day, hour, minute) = civil(unix);
    format!("{day:02}/{month:02}/{year} {hour:02}:{minute:02}")
}

/// L'expéditeur d'un message, tel qu'une ligne d'attribution le nomme.
///
/// Le nom **et** l'adresse quand les deux existent : le nom seul ne dit pas qui c'était
/// exactement — deux personnes portent le même — et l'adresse seule se lit mal. Un nom absent
/// ou vide laisse l'adresse seule.
fn sender_label(message: &dto::Message) -> String {
    match message.row.from_name.as_deref() {
        Some(name) if !name.trim().is_empty() && name != message.row.from => {
            format!("{name} <{}>", message.row.from)
        }
        _ => message.row.from.clone(),
    }
}

/// Ce qu'une citation peut prendre au plus, en octets.
///
/// 32 Kio : de quoi citer un fil entier, mais pas un message de 256 Kio — le plafond du corps
/// servi par le service. Une citation illimitée mettrait un quart de mégaoctet dans un champ de
/// texte qui se remet en page à chaque frappe, et le critère 5 de la phase 1 borne l'image.
///
/// La troncature est **dite dans le texte**, parce qu'elle part chez le destinataire : une
/// citation coupée en silence laisse croire que l'original s'arrêtait là.
const QUOTE_MAX: usize = 32 * 1024;

/// Le message d'origine, cité comme une réponse le cite.
///
/// La ligne d'attribution d'abord — « Le …, X a écrit : » — puis chaque ligne préfixée de
/// `> `. C'est ce que font tous les clients depuis trente ans, et ça se relit dans n'importe
/// quel lecteur, y compris en texte brut.
///
/// Deux lignes vides sont laissées **au-dessus** : c'est là que l'utilisateur écrit, et un
/// curseur placé avant la citation est ce qu'il attend.
fn quoted(message: &dto::Message) -> String {
    let who = sender_label(message);
    let mut out = format!("\n\nLe {}, {who} a écrit :\n", full_date(message.row.date));
    for line in quote_body(&message.body).lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Le message d'origine, présenté comme un transfert le présente.
///
/// Un bloc d'en-têtes lisible plutôt qu'une citation : celui qui reçoit un transfert veut
/// savoir de qui et de quand vient le message, et non ce que l'expéditeur en pense.
fn forwarded(message: &dto::Message) -> String {
    let mut out = String::from("\n\n-------- Message transféré --------\n");
    let who = sender_label(message);
    out.push_str(&format!("De : {who}\n"));
    out.push_str(&format!("Date : {}\n", full_date(message.row.date)));
    out.push_str(&format!("Sujet : {}\n", message.row.subject));
    if !message.to.is_empty() {
        out.push_str(&format!("À : {}\n", message.to.join(", ")));
    }
    out.push('\n');
    out.push_str(quote_body(&message.body).as_str());
    out
}

/// Le corps à citer, borné et dit quand il est coupé.
fn quote_body(body: &str) -> String {
    if body.len() <= QUOTE_MAX {
        return body.to_owned();
    }
    // Sur une frontière de caractère, sinon la troncature panique sur un accent.
    let mut cut = QUOTE_MAX;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[…] (citation tronquée)", &body[..cut])
}

/// Une date lisible pour une ligne d'attribution : « 10 septembre 2026 à 14:05 ».
///
/// En français et écrite à la main, comme le reste des dates de la coquille : aucune
/// dépendance de calendrier, et [`civil`] fait déjà le calcul grégorien.
fn full_date(unix: i64) -> String {
    const MOIS: [&str; 12] = [
        "janvier",
        "février",
        "mars",
        "avril",
        "mai",
        "juin",
        "juillet",
        "août",
        "septembre",
        "octobre",
        "novembre",
        "décembre",
    ];
    if unix <= 0 {
        // 102 messages du corpus n'ont pas de date exploitable. Un tiret est plus honnête
        // qu'une date inventée à partir de zéro.
        return "—".to_owned();
    }
    let (year, month, day, hour, minute) = civil(unix);
    let mois = MOIS
        .get(usize::try_from(month.saturating_sub(1)).unwrap_or(0))
        .copied()
        .unwrap_or("");
    format!("{day} {mois} {year} à {hour:02}:{minute:02}")
}

/// Convertit des secondes Unix en date civile UTC.
///
/// Algorithme de Howard Hinnant, `days_from_civil` inversé. UTC et non l'heure locale : lire le
/// fuseau du système demande une bibliothèque, et une date de liste à une heure près suffit à
/// se repérer. À corriger le jour où l'affichage de l'heure exacte comptera.
fn civil(unix: i64) -> (i64, u32, u32, u32, u32) {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = if month <= 2 { year + 1 } else { year };
    let hour = u32::try_from(seconds / 3600).unwrap_or(0);
    let minute = u32::try_from((seconds % 3600) / 60).unwrap_or(0);
    (year, month, day, hour, minute)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Un message ouvert, tel que le service le rend.
    fn opened(from: &str, name: Option<&str>, to: &[&str], body: &str) -> dto::Message {
        dto::Message {
            row: dto::Row {
                id: 1,
                date: 1_789_041_600,
                from: from.to_owned(),
                from_name: name.map(ToOwned::to_owned),
                subject: "Le sujet".to_owned(),
                unread: false,
                flagged: false,
                score: None,
                has_attachments: false,
            },
            message_id: Some("<parent@exemple.fr>".to_owned()),
            references: Vec::new(),
            to: to.iter().map(|it| (*it).to_owned()).collect(),
            body: body.to_owned(),
            body_truncated: false,
            html: None,
            attachments: Vec::new(),
            folders: Vec::new(),
            thread: None,
            invitation: None,
        }
    }

    #[test]
    fn a_quote_carries_an_attribution_line_and_prefixes_every_line() {
        // Sans citation, le destinataire d'une réponse ne sait pas à quoi elle répond — surtout
        // si elle arrive trois jours plus tard.
        let message = opened(
            "eloise@exemple.fr",
            Some("Éloïse Durand"),
            &["moi@exemple.fr"],
            "Bonjour,\n\nVoici le rapport.",
        );
        let quote = quoted(&message);
        assert!(quote.starts_with("\n\n"), "l'utilisateur écrit au-dessus");
        assert!(
            quote.contains("Éloïse Durand <eloise@exemple.fr> a écrit :"),
            "{quote}"
        );
        // Chaque ligne, y compris les vides : une ligne vide non préfixée casse la citation
        // chez les lecteurs qui replient sur le `>`.
        for line in quote.lines().skip(3) {
            assert!(line.starts_with('>'), "ligne non citée : {line:?}");
        }
        assert!(quote.contains("> Voici le rapport."));
    }

    #[test]
    fn a_forward_names_the_original_and_does_not_quote_it() {
        // Celui qui reçoit un transfert veut savoir de qui et de quand vient le message, pas ce
        // que l'expéditeur en pense.
        let message = opened(
            "eloise@exemple.fr",
            Some("Éloïse"),
            &["moi@exemple.fr", "jean@ailleurs.fr"],
            "Le corps d'origine.",
        );
        let text = forwarded(&message);
        assert!(text.contains("-------- Message transféré --------"));
        assert!(text.contains("De : Éloïse <eloise@exemple.fr>"));
        assert!(text.contains("Sujet : Le sujet"));
        assert!(text.contains("À : moi@exemple.fr, jean@ailleurs.fr"));
        assert!(text.contains("Le corps d'origine."));
        assert!(!text.contains("> Le corps"), "un transfert ne cite pas");
    }

    #[test]
    fn a_quote_is_bounded_and_says_so() {
        // Un corps peut faire 256 Kio — le plafond du service. Le mettre en entier dans un champ
        // qui se remet en page à chaque frappe coûterait une image, et le critère 5 de la phase
        // 1 borne l'image.
        let long = "x".repeat(QUOTE_MAX * 2);
        let message = opened("a@b.fr", None, &["moi@exemple.fr"], &long);
        let quote = quoted(&message);
        assert!(quote.contains("citation tronquée"), "la coupe est muette");
        assert!(quote.len() < QUOTE_MAX + 4096);
    }

    #[test]
    fn a_quote_never_cuts_a_character_in_half() {
        // Le corpus est français : une troncature à l'octet tombe au milieu d'un accent une
        // fois sur deux, et `String::truncate` panique là.
        let accents = "é".repeat(QUOTE_MAX);
        let message = opened("a@b.fr", None, &["moi@exemple.fr"], &accents);
        let quote = quoted(&message);
        assert!(quote.contains("citation tronquée"));
        // La seule preuve qui compte : le texte est de l'UTF-8 valide, ce que le type garantit
        // — donc l'absence de panique est le test.
        assert!(quote.chars().count() > 1_000);
    }

    #[test]
    fn a_sender_without_a_name_is_named_by_its_address_alone() {
        let message = opened("a@b.fr", None, &["moi@exemple.fr"], "corps");
        assert_eq!(sender_label(&message), "a@b.fr");
        // Et un nom qui **est** l'adresse ne sort pas deux fois : le corpus en est plein.
        let message = opened("a@b.fr", Some("a@b.fr"), &["moi@exemple.fr"], "corps");
        assert_eq!(sender_label(&message), "a@b.fr");
        let message = opened("a@b.fr", Some("  "), &["moi@exemple.fr"], "corps");
        assert_eq!(sender_label(&message), "a@b.fr");
    }

    /// Une source, telle que `messages.source` la rend.
    fn a_source(body: &str) -> dto::MessageSource {
        dto::MessageSource {
            headers: "From: a@b.c\nSubject: essai\n".to_owned(),
            body: body.to_owned(),
            total: 42,
            headers_truncated: false,
            body_truncated: false,
            invalid_sequences: 0,
            escaped_controls: 0,
        }
    }

    #[test]
    fn the_source_view_draws_every_reserve_without_panicking() {
        // La fenêtre de source a été écrite le même jour que la page de paramètres, et avec la
        // même lacune : du dessin sans couverture. Les réserves sont justement les branches
        // qu'on ne voit presque jamais à l'usage, donc celles qu'un essai à la main ne teste
        // pas.
        let states = [
            a_source("corps"),
            dto::MessageSource {
                headers_truncated: true,
                body_truncated: true,
                invalid_sequences: 3,
                escaped_controls: 2,
                total: 48 * 1024 * 1024,
                ..a_source("corps tronqué")
            },
            // Un message vide des deux côtés : le cas d'un accusé de réception sans corps.
            dto::MessageSource {
                headers: String::new(),
                ..a_source("")
            },
        ];
        for source in &states {
            egui::__run_test_ui(|ui| source_body(ui, source));
        }
    }

    // Il n'y a pas de test « le dessin ne modifie pas la source » : `source_body` prend un
    // `&dto::MessageSource`, donc le compilateur le prouve déjà. Un test qui répète une
    // garantie du type ne vérifie rien et coûte une ligne à maintenir.

    #[test]
    fn a_date_without_a_value_is_a_dash_and_not_the_epoch() {
        // 102 messages du corpus n'ont pas de date exploitable. « 1 janvier 1970 » serait une
        // date inventée.
        assert_eq!(full_date(0), "—");
        assert_eq!(full_date(-1), "—");
        // 2026-09-10T12:00:00Z, soit 20 706 jours depuis l'époque plus douze heures.
        assert_eq!(full_date(1_789_041_600), "10 septembre 2026 à 12:00");
    }

    #[test]
    fn civil_dates_match_known_instants() {
        // Époque Unix.
        assert_eq!(civil(0), (1970, 1, 1, 0, 0));
        // 2026-09-02T10:38:39Z, l'instant d'un relevé de `docs/PHASE-1.md`. Valeur vérifiée
        // avec `date -u -d @…` plutôt que calculée de tête : la première écrite ici était
        // fausse d'un jour, et c'est le test qui l'a dit.
        assert_eq!(civil(1_788_345_519), (2026, 9, 2, 10, 38));
        // Le lendemain, pour verrouiller le passage de jour.
        assert_eq!(civil(1_788_431_919), (2026, 9, 3, 10, 38));
        // Une date avant l'époque : le corpus en contient, mal formées.
        assert_eq!(civil(-86_400), (1969, 12, 31, 0, 0));
    }

    #[test]
    fn a_missing_date_is_a_dash() {
        assert_eq!(short_date(0), "—");
        assert_eq!(long_date(0), "date inconnue");
    }

    #[test]
    fn elision_cuts_on_character_boundaries() {
        assert_eq!(elide("court", 10), "court");
        assert_eq!(elide("éééééé", 3), "éé…");
    }
}

/// Combien de messages déjà envoyés le panneau de la file montre.
///
/// Cinq. L'historique ne se purge jamais — c'est le journal d'envoi, et le supprimer perdrait la
/// trace de ce qui est parti — mais le panneau n'est pas là pour le lire : il est là pour montrer
/// ce qui **n'est pas fini**. Sans cette borne, la seule ligne qui demande une décision se
/// retrouvait noyée dans un mur de « envoyé » après un mois d'usage.
const OUTBOX_HISTORY: usize = 5;

/// Largeur minimale de la fenêtre de rédaction.
///
/// 420 px : de quoi garder « Copie cachée » et une adresse complète sur la même ligne. En
/// dessous, l'étiquette et son champ se chevauchent et le formulaire cesse d'être lisible.
const COMPOSE_MIN_WIDTH: f32 = 420.0;

/// Hauteur minimale de la fenêtre de rédaction.
///
/// 300 px : les quatre en-têtes, quelques lignes de corps et le bouton visibles en même temps.
/// Une fenêtre où il faut défiler pour trouver « Envoyer » n'est pas une fenêtre plus petite,
/// c'est une fenêtre cassée.
const COMPOSE_MIN_HEIGHT: f32 = 300.0;

/// Ce que la rédaction réserve sous le corps : un séparateur et la ligne du bouton.
///
/// Sans cette réserve, le corps prend toute la place restante et pousse « Envoyer » hors de la
/// fenêtre — le seul bouton qui compte, et celui qu'on ne verrait plus.
const COMPOSE_FOOTER: f32 = 48.0;

/// Hauteur minimale du corps, quand la fenêtre est à sa taille minimale.
const COMPOSE_MIN_BODY: f32 = 96.0;

/// Combien de participants une invitation affiche avant de dire « et N autres ».
///
/// Douze : de quoi voir une réunion d'équipe en entier. Le corpus réel porte 8 150
/// participants pour 930 pièces, et certaines invitations d'entreprise en ont des dizaines —
/// les dérouler toutes pousserait le corps du message hors de l'écran.
const ATTENDEES_SHOWN: usize = 12;

/// Ce que le panneau des brouillons occupe au plus.
///
/// Un plafond : la liste défile au-delà, plutôt que de repousser la liste des messages hors de
/// l'écran.
const DRAFTS_PANE: f32 = 160.0;

/// La ligne « Signature : … [Modifier] », toujours présente.
const SIGNATURE_LINE: f32 = 24.0;

/// Ce que l'aperçu de la signature occupe au plus, quand il y en a une.
///
/// Un plafond et pas une hauteur : une signature de deux lignes en prend deux. Au-delà, l'aperçu
/// défile — une signature de deux cents lignes ne doit pas manger la fenêtre de rédaction.
const SIGNATURE_PREVIEW: f32 = 96.0;

/// La cohérence des quatre constantes ci-dessus, vérifiée **à la compilation**.
///
/// Si la réserve du bas dépassait la hauteur minimale, le corps serait à son plancher dans
/// toutes les tailles et le calcul de [`rows_for`] ne servirait à rien. Un test l'aurait dit
/// aussi ; un `const` le dit avant qu'il existe un binaire pour le tester.
const _: () = assert!(
    COMPOSE_MIN_HEIGHT - COMPOSE_FOOTER - SIGNATURE_LINE - SIGNATURE_PREVIEW > COMPOSE_MIN_BODY,
    "la fenêtre de rédaction minimale ne laisse pas la place d'un corps utilisable"
);

/// Combien de lignes demander à un `TextEdit` pour remplir une hauteur donnée.
///
/// ## Pourquoi ça ne se déduit pas de la hauteur seule
///
/// `desired_rows` est un nombre de lignes, pas une hauteur : `TextEdit` le multiplie par la
/// hauteur de ligne de la police en vigueur. Coder un nombre en dur revient donc à parier sur
/// une taille de police, et un utilisateur qui grossit le texte se retrouve avec un champ trop
/// haut ou trop court.
///
/// La division par [`LINE_GUESS`] est une approximation assumée : `egui` ne donne pas la hauteur
/// de ligne avant d'avoir mis en page. Elle n'a pas à être juste, seulement à ne jamais rendre
/// zéro — un `desired_rows(0)` donne un champ d'une ligne quelle que soit la place disponible.
///
/// **Fonction pure**, pour être testable.
fn rows_for(height: f32) -> usize {
    // Une hauteur négative ou non finie vient d'un `available_height` pris hors mise en page.
    // `max(0.0)` puis un plancher : le pire cas doit rester un champ utilisable.
    let usable = if height.is_finite() {
        height.max(0.0)
    } else {
        0.0
    };
    ((usable / LINE_GUESS) as usize).max(3)
}

/// Hauteur de ligne supposée, en pixels. Voir [`rows_for`].
const LINE_GUESS: f32 = 18.0;

/// Le fragment en cours de saisie dans un champ d'adresses.
///
/// Un champ contient « jean@x.fr, mar » : ce qu'on complète est « mar », pas tout le champ.
/// Sans ça, taper un second destinataire ne proposerait rien — le champ entier ne correspond à
/// aucune adresse.
///
/// **Fonction pure**, pour être testable : le découpage des adresses est la seule logique de la
/// fenêtre qui ait une bonne et une mauvaise réponse.
fn last_fragment(field: &str) -> String {
    field
        .rsplit([',', ';'])
        .next()
        .unwrap_or(field)
        .trim()
        .to_owned()
}

/// Remplace le fragment en cours par une adresse choisie, et prépare la suivante.
///
/// Le résultat finit par « , » : l'utilisateur vient de choisir un destinataire, il en veut
/// probablement un autre, et lui faire taper le séparateur serait une frappe de plus pour rien.
///
/// **Fonction pure**, comme [`last_fragment`], et pour la même raison.
fn replace_last(field: &str, chosen: &str) -> String {
    match field.rfind([',', ';']) {
        // Tout ce qui précède le dernier séparateur est conservé tel quel, séparateur compris,
        // puis normalisé en « , » — un champ collé depuis Outlook arrive en points-virgules, et
        // mélanger les deux dans un même champ se lit mal.
        Some(cut) => {
            // Séparateurs **et** espaces, dans un seul passage : `" ; , "` alterne les deux, et
            // deux `trim` enchaînés s'arrêtent au premier caractère de l'autre sorte —
            // le résultat gardait un « ; » orphelin. Trouvé par
            // `a_field_of_only_separators_does_not_leave_a_stray_comma`.
            let kept = field[..cut]
                .trim_end_matches(|it: char| it == ',' || it == ';' || it.is_whitespace());
            if kept.is_empty() {
                // Le champ n'était que des séparateurs. Garder le premier écrirait
                // « , marie@y.fr, » : le démon le jette, mais l'utilisateur le voit.
                format!("{chosen}, ")
            } else {
                format!("{kept}, {chosen}, ")
            }
        }
        None => format!("{chosen}, "),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod completion_tests {
    use super::{last_fragment, replace_last};

    #[test]
    fn the_fragment_completed_is_the_last_one_not_the_whole_field() {
        // **Le cas qui rend l'autocomplétion utilisable pour un second destinataire.** Sans ce
        // découpage, le champ entier — « jean@x.fr, mar » — serait la requête, et il ne
        // correspond à aucune adresse : taper un deuxième destinataire ne proposerait rien.
        assert_eq!(last_fragment("jean@x.fr, mar"), "mar");
        assert_eq!(last_fragment("jean@x.fr; mar"), "mar");
        assert_eq!(last_fragment("mar"), "mar");
        assert_eq!(last_fragment("  mar  "), "mar");
    }

    #[test]
    fn a_field_that_ends_on_a_separator_completes_from_nothing() {
        // « jean@x.fr, » veut dire « je vais taper un autre destinataire ». Le fragment est
        // vide, et un fragment vide propose les mieux classées — ce qui est exactement utile.
        assert_eq!(last_fragment("jean@x.fr,"), "");
        assert_eq!(last_fragment("jean@x.fr, "), "");
        assert_eq!(last_fragment(""), "");
    }

    #[test]
    fn inserting_keeps_the_recipients_already_typed() {
        // **La règle de correction de l'insertion.** Remplacer tout le champ effacerait les
        // destinataires déjà saisis, ce qui est le pire défaut qu'une autocomplétion puisse
        // avoir : elle détruirait le travail qu'elle prétend accélérer.
        assert_eq!(
            replace_last("jean@x.fr, mar", "Marie <marie@y.fr>"),
            "jean@x.fr, Marie <marie@y.fr>, "
        );
        assert_eq!(
            replace_last("jean@x.fr, paul@z.fr, mar", "marie@y.fr"),
            "jean@x.fr, paul@z.fr, marie@y.fr, "
        );
    }

    #[test]
    fn inserting_into_an_empty_field_leaves_only_the_chosen_address() {
        assert_eq!(replace_last("", "marie@y.fr"), "marie@y.fr, ");
        assert_eq!(replace_last("mar", "marie@y.fr"), "marie@y.fr, ");
    }

    #[test]
    fn semicolons_are_normalised_to_commas() {
        // Un champ collé depuis Outlook arrive en points-virgules. Mélanger les deux
        // séparateurs dans un même champ se lit mal, et le démon accepte les deux de toute
        // façon — c'est donc un choix d'affichage, pas de correction.
        assert_eq!(
            replace_last("jean@x.fr; mar", "marie@y.fr"),
            "jean@x.fr, marie@y.fr, "
        );
    }

    #[test]
    fn the_result_always_ends_ready_for_the_next_one() {
        // Une virgule finale, pour que l'utilisateur n'ait pas à taper le séparateur. Le
        // découpage côté démon la jette : voir `Compose::recipients`.
        for (field, chosen) in [("", "a@b.fr"), ("x@y.fr, ", "a@b.fr"), ("z", "a@b.fr")] {
            let out = replace_last(field, chosen);
            assert!(out.ends_with(", "), "{out:?}");
        }
    }

    #[test]
    fn a_field_of_only_separators_does_not_leave_a_stray_comma() {
        // Le cas dégénéré : un champ qui n'est que des séparateurs. Sans le `trim_end`, le
        // résultat commencerait par « , » — que le démon jette, mais qui s'affiche.
        assert_eq!(replace_last(",", "a@b.fr"), "a@b.fr, ");
        assert_eq!(replace_last(" , ", "a@b.fr"), "a@b.fr, ");
        assert_eq!(replace_last(" ; , ", "a@b.fr"), "a@b.fr, ");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod compose_size_tests {
    use super::rows_for;

    #[test]
    fn the_body_never_collapses_to_a_single_line() {
        // **Le défaut que ça corrige.** `desired_rows(0)` donne un champ d'une ligne quelle que
        // soit la place disponible, donc un corps invisible dans une fenêtre pourtant grande.
        for height in [0.0, 1.0, 10.0, -50.0, f32::NAN, f32::INFINITY] {
            assert!(
                rows_for(height) >= 3,
                "hauteur {height} donne {} lignes",
                rows_for(height)
            );
        }
    }

    #[test]
    fn a_taller_window_gets_more_lines() {
        // La propriété qui rend l'agrandissement utile : c'était fixé à douze lignes, donc
        // agrandir la fenêtre n'agrandissait pas le corps.
        assert!(rows_for(600.0) > rows_for(300.0));
        assert!(rows_for(300.0) > rows_for(150.0));
    }
}

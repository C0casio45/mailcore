//! Les jobs de fond : import, indexation, threading. Et plus tard sync et embeddings.
//!
//! Règle 3 du `CLAUDE.md` : toute tâche longue écrit dans le store, et l'UI observe le
//! store. Un job qui met vingt minutes n'a aucun effet sur la fluidité de l'interface, parce
//! que l'interface ne l'attend pas — elle regarde ce qui est déjà écrit.
//!
//! ## Ce que ce module change au démon
//!
//! Jusqu'ici le démon ne savait que lire : importer ou réindexer voulait dire aller taper
//! `mail import` **sur la machine du démon**. Dans le déploiement de référence — démon sur
//! une machine dédiée, clients ailleurs — ça veut dire ouvrir un shell distant pour la
//! première action que tout utilisateur fait. Une UI ne pouvait pas proposer « importer mon
//! profil Thunderbird ».
//!
//! ## Un fil dédié, pas le runtime
//!
//! Un import prend quatre minutes et `mailcore` est synchrone. Le poser sur le pool bloquant
//! de tokio immobiliserait un de ses fils pendant tout ce temps, en concurrence avec les
//! lectures qui servent l'interface. Un fil du système d'exploitation dédié coûte quelques
//! kilo-octets de pile et rend le raisonnement trivial : **un seul job à la fois, toujours,
//! par construction**.
//!
//! Cette sérialisation n'est pas qu'une commodité. SQLite n'a qu'un écrivain ; deux imports
//! concurrents passeraient leur temps à se disputer le verrou, et une indexation qui tourne
//! pendant un import indexerait un store en mouvement.
//!
//! ## Pendant qu'un job tourne, l'interface continue de lire
//!
//! Le job ouvre **son propre** [`mailcore::Store`], donc sa propre connexion SQLite. La
//! boîte que servent l'API et MCP reste ouverte et lisible : le mode WAL autorise un
//! écrivain et des lecteurs en même temps. C'est ce qui rend la règle 3 vraie plutôt que
//! souhaitée.
//!
//! Effet de bord utile : parce que le job écrit depuis une autre connexion, `PRAGMA
//! data_version` change pour les lecteurs, donc la révision du store change, donc
//! `store.wait` réveille les clients tout seul. Le mécanisme d'abonnement de l'étape 7
//! marche pour les jobs sans qu'on ait rien ajouté.
//!
//! ## Ce qu'un client peut demander, et ce qu'il ne peut pas
//!
//! **Un client ne nomme jamais un chemin.** Les profils importables sont déclarés par
//! l'opérateur au démarrage (`maild --profile <chemin>`, répétable) ; un client les désigne
//! par leur rang. Voir [`Sources`] pour le raisonnement — c'est la différence entre exposer
//! une commande d'import et offrir à quiconque détient le jeton la lecture de n'importe quel
//! fichier de la machine du démon.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use mailcore::{Mailbox, Progress, Store};
use mailimport::import::{ImportOptions, import_profile};

/// Nombre de jobs terminés conservés dans le registre.
///
/// Assez pour qu'un client qui interroge après coup retrouve le bilan de ce qu'il a lancé,
/// assez peu pour qu'un démon qui tourne des mois ne garde pas un historique sans fin en
/// mémoire. Les jobs en attente et en cours ne sont jamais évincés.
const HISTORY: usize = 32;

/// Ce qu'un job fait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Importe un profil déclaré par l'opérateur, désigné par son rang.
    Import {
        /// Le rang dans [`Sources`].
        source: usize,
        /// Tout lire et tout compter sans rien écrire.
        dry_run: bool,
        /// Importer aussi les comptes de flux RSS.
        include_feeds: bool,
        /// Importer aussi les répertoires de comptes non déclarés dans `prefs.js`.
        include_orphans: bool,
    },
    /// Reconstruit l'index plein texte.
    Index,
    /// Reconstruit les fils de discussion.
    Thread,
    /// Reconstruit le carnet d'adresses depuis le corpus.
    ///
    /// Un job, comme l'indexation, et pour la même raison : c'est un parcours des en-têtes de
    /// tous les messages. Sur le corpus réel, ça se compte en minutes.
    Contacts,
    /// Synchronise les comptes IMAP actifs.
    ///
    /// ## Pourquoi c'est un job et pas une méthode d'API
    ///
    /// Une synchronisation dure des minutes et écrit dans le store. La servir en synchrone
    /// tiendrait le verrou de la boîte pendant tout ce temps — la règle 3 du `CLAUDE.md`
    /// interdit qu'une interface attende ça, et le mécanisme des jobs existe exactement pour
    /// ce cas.
    ///
    /// ## Le secret vient du trousseau **de la machine du démon**
    ///
    /// C'est le démon qui se connecte au serveur IMAP, donc c'est son trousseau qui doit
    /// porter les secrets. `mail account add` refuse `--daemon` pour cette raison : déclarer
    /// le compte depuis un poste client rangerait le secret dans le mauvais trousseau, et la
    /// synchronisation échouerait ici sur un secret introuvable.
    ///
    /// **Conséquence à connaître pour le déploiement de référence** : un démon sur une machine
    /// dédiée a besoin d'un trousseau accessible sur cette machine. Une session Linux sans
    /// Secret Service fait échouer le job avec un message qui le dit. `docs/PRIVACY.md` §7
    /// interdit l'alternative — un secret dans un fichier de configuration ou une variable
    /// d'environnement — donc ce n'est pas une limitation à contourner, c'est une contrainte à
    /// satisfaire.
    Sync {
        /// Ne synchroniser que ce compte. `None` pour tous les comptes actifs.
        account: Option<i64>,
    },
}

impl Kind {
    /// L'étiquette du job, telle qu'elle sort dans l'API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Import { .. } => "import",
            Self::Index => "index",
            Self::Thread => "thread",
            Self::Contacts => "contacts",
            Self::Sync { .. } => "sync",
        }
    }
}

/// Où en est un job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Accepté, en attente du fil de travail.
    Queued,
    /// En cours.
    Running,
    /// Terminé normalement.
    Done,
    /// Arrêté à la demande. Ce qui était écrit reste écrit.
    Cancelled,
    /// Terminé en erreur.
    Failed,
}

impl State {
    /// L'étiquette de l'état, telle qu'elle sort dans l'API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    /// Vrai si plus rien ne bougera.
    #[must_use]
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::Failed)
    }
}

/// L'instantané d'un job, tel qu'un client le voit.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// L'identifiant rendu par `jobs.start`.
    pub id: u64,
    /// Ce que le job fait.
    pub kind: Kind,
    /// Où il en est.
    pub state: State,
    /// Unités faites, dans l'unité du job — des octets pour l'import, des messages sinon.
    pub done: u64,
    /// Unités prévues. `0` tant que le job ne le sait pas.
    pub total: u64,
    /// Le bilan en fin de course, ou la raison de l'échec.
    ///
    /// **Jamais un chemin de fichier ni un fragment de message** : ce texte traverse le
    /// réseau et finit dans l'interface d'un client (`docs/PRIVACY.md`, §8).
    pub message: Option<String>,
    /// Instant de mise en file, en secondes Unix.
    pub queued_at: i64,
    /// Instant de fin, en secondes Unix. Absent tant que le job n'est pas terminé.
    pub finished_at: Option<i64>,
}

/// Les profils que l'opérateur autorise à importer.
///
/// ## Pourquoi un client ne donne pas de chemin
///
/// `jobs.start` est la première méthode de l'API qui **écrit**. Laisser un client passer un
/// chemin arbitraire donnerait à quiconque détient le jeton le droit de faire lire n'importe
/// quel fichier de la machine du démon, de le ranger dans le store, puis de le relire par
/// `search.query`. Ce serait une lecture de fichiers arbitraires déguisée en fonctionnalité
/// d'import — et aucune validation de chemin ne rattrape ça de façon convaincante, parce que
/// le démon n'a aucun moyen de savoir ce que l'opérateur considère comme légitime.
///
/// L'opérateur le sait, lui. Il le déclare au démarrage, et un client ne peut que choisir
/// dans cette liste. Fail closed : sans `--profile`, aucun import n'est possible, et le
/// message d'erreur dit quoi faire.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    paths: Vec<Utf8PathBuf>,
}

impl Sources {
    /// Les profils déclarés par l'opérateur.
    #[must_use]
    pub fn new(paths: Vec<Utf8PathBuf>) -> Self {
        Self { paths }
    }

    /// Les chemins, dans l'ordre où un client les désigne.
    #[must_use]
    pub fn paths(&self) -> &[Utf8PathBuf] {
        &self.paths
    }

    /// Le profil de rang `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&Utf8Path> {
        self.paths.get(index).map(Utf8PathBuf::as_path)
    }

    /// Vrai si aucun profil n'est déclaré.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Ce qu'un job en cours laisse observer.
#[derive(Debug)]
struct Entry {
    snapshot: Snapshot,
    /// La poignée partagée avec le fil de travail. Absente pour un job terminé.
    progress: Option<Arc<Progress>>,
}

impl Entry {
    /// L'instantané, avec les compteurs relus dans la poignée vivante.
    fn read(&self) -> Snapshot {
        let mut snapshot = self.snapshot.clone();
        if let Some(progress) = &self.progress {
            snapshot.done = progress.done();
            snapshot.total = progress.total();
        }
        snapshot
    }
}

/// Le registre et la file des jobs.
#[derive(Clone)]
pub struct Jobs {
    inner: Arc<Mutex<Vec<Entry>>>,
    next_id: Arc<AtomicU64>,
    queue: mpsc::Sender<u64>,
    sources: Arc<Sources>,
}

impl std::fmt::Debug for Jobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jobs").finish_non_exhaustive()
    }
}

/// Ce qui peut empêcher un job de démarrer.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// L'opérateur n'a déclaré aucun profil importable.
    #[error("aucun profil importable n'est déclaré : démarrer maild avec --profile")]
    NoSources,
    /// Le rang demandé ne correspond à aucun profil déclaré.
    #[error("profil inconnu : {0} déclarés")]
    UnknownSource(usize),
    /// Le fil de travail est mort. Ne devrait pas arriver ; le dire plutôt que l'ignorer.
    #[error("le fil de travail des jobs est arrêté")]
    WorkerGone,
}

impl Jobs {
    /// Monte le registre et démarre le fil de travail.
    ///
    /// `mailbox` est la boîte que servent l'API et MCP : le fil s'en sert uniquement pour
    /// remonter l'index après une réindexation, jamais pour écrire.
    #[must_use]
    pub fn start(mailbox: Arc<Mutex<Mailbox>>, store: Utf8PathBuf, sources: Sources) -> Self {
        let (queue, receiver) = mpsc::channel::<u64>();
        let inner = Arc::new(Mutex::new(Vec::<Entry>::new()));
        let jobs = Self {
            inner: Arc::clone(&inner),
            next_id: Arc::new(AtomicU64::new(1)),
            queue,
            sources: Arc::new(sources),
        };

        // Le fil vit aussi longtemps que le démon. Il se termine quand le dernier `Sender`
        // est détruit, c'est-à-dire à l'arrêt du processus.
        let worker = Worker {
            registry: inner,
            mailbox,
            store,
            sources: Arc::clone(&jobs.sources),
        };
        std::thread::Builder::new()
            .name("mailcore-jobs".to_owned())
            .spawn(move || worker.run(&receiver))
            // Ne pas pouvoir créer un fil est une panne du système, pas une condition qu'on
            // sait rattraper. Le démon sert quand même en lecture ; les jobs resteront en
            // file, et `jobs.list` le montrera.
            .map_or_else(
                |source| tracing::error!(%source, "fil de travail des jobs non démarré"),
                |_| tracing::debug!("fil de travail des jobs démarré"),
            );

        jobs
    }

    /// Les profils importables déclarés par l'opérateur.
    #[must_use]
    pub fn sources(&self) -> &Sources {
        &self.sources
    }

    /// Met un job en file et rend son identifiant.
    ///
    /// # Errors
    ///
    /// [`StartError`] si le profil demandé n'existe pas ou si le fil de travail est mort.
    pub fn enqueue(&self, kind: Kind) -> Result<u64, StartError> {
        if let Kind::Import { source, .. } = kind {
            if self.sources.is_empty() {
                return Err(StartError::NoSources);
            }
            if self.sources.get(source).is_none() {
                return Err(StartError::UnknownSource(self.sources.paths().len()));
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = Entry {
            snapshot: Snapshot {
                id,
                kind,
                state: State::Queued,
                done: 0,
                total: 0,
                message: None,
                queued_at: now(),
                finished_at: None,
            },
            progress: None,
        };

        {
            let mut registry = self.lock();
            registry.push(entry);
            prune(&mut registry);
        }

        self.queue.send(id).map_err(|_| StartError::WorkerGone)?;
        tracing::info!(id, kind = kind.as_str(), "job mis en file");
        Ok(id)
    }

    /// Tous les jobs connus, du plus récent au plus ancien.
    #[must_use]
    pub fn list(&self) -> Vec<Snapshot> {
        let registry = self.lock();
        registry.iter().rev().map(Entry::read).collect()
    }

    /// Un job par son identifiant.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<Snapshot> {
        let registry = self.lock();
        registry
            .iter()
            .find(|entry| entry.snapshot.id == id)
            .map(Entry::read)
    }

    /// Demande l'arrêt d'un job.
    ///
    /// Rend l'instantané mis à jour, ou `None` si l'identifiant est inconnu. Un job déjà
    /// terminé est rendu tel quel : annuler ce qui est fini n'est pas une erreur, c'est une
    /// course perdue par le client, et lui rendre une erreur l'obligerait à la traiter.
    ///
    /// L'arrêt est **coopératif** : le job s'arrête à sa prochaine vérification. Un job en
    /// file, lui, passe directement à `cancelled` sans jamais démarrer.
    pub fn cancel(&self, id: u64) -> Option<Snapshot> {
        let mut registry = self.lock();
        let entry = registry.iter_mut().find(|entry| entry.snapshot.id == id)?;

        match entry.snapshot.state {
            State::Running => {
                if let Some(progress) = &entry.progress {
                    progress.cancel();
                }
                tracing::info!(id, "annulation demandée");
            }
            State::Queued => {
                entry.snapshot.state = State::Cancelled;
                entry.snapshot.finished_at = Some(now());
                entry.snapshot.message = Some("annulé avant démarrage".to_owned());
                tracing::info!(id, "job annulé avant démarrage");
            }
            State::Done | State::Cancelled | State::Failed => {}
        }
        Some(entry.read())
    }

    /// Le registre. Un verrou empoisonné vient d'une panique ailleurs ; l'état du registre
    /// reste cohérent, et refuser tout le reste de la session serait pire.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Le fil de travail : un job à la fois, dans l'ordre de la file.
struct Worker {
    registry: Arc<Mutex<Vec<Entry>>>,
    mailbox: Arc<Mutex<Mailbox>>,
    store: Utf8PathBuf,
    sources: Arc<Sources>,
}

impl Worker {
    /// Boucle jusqu'à ce que le dernier émetteur disparaisse.
    fn run(self, queue: &mpsc::Receiver<u64>) {
        while let Ok(id) = queue.recv() {
            self.run_one(id);
        }
        tracing::debug!("fil de travail des jobs terminé");
    }

    /// Exécute un job et met le registre à jour.
    fn run_one(&self, id: u64) {
        let progress = Arc::new(Progress::new());

        // Passage en `running`, sauf si le client a annulé pendant l'attente en file.
        let kind = {
            let mut registry = self.lock();
            let Some(entry) = registry.iter_mut().find(|entry| entry.snapshot.id == id) else {
                return;
            };
            if entry.snapshot.state != State::Queued {
                return;
            }
            entry.snapshot.state = State::Running;
            entry.progress = Some(Arc::clone(&progress));
            entry.snapshot.kind
        };

        tracing::info!(id, kind = kind.as_str(), "job démarré");
        let outcome = self.execute(kind, &progress);

        let (state, message) = match outcome {
            Ok(summary) if progress.is_cancelled() => (State::Cancelled, Some(summary)),
            Ok(summary) => (State::Done, Some(summary)),
            Err(summary) => (State::Failed, Some(summary)),
        };

        {
            let mut registry = self.lock();
            if let Some(entry) = registry.iter_mut().find(|entry| entry.snapshot.id == id) {
                // Les compteurs sont figés dans l'instantané avant de lâcher la poignée :
                // après, plus personne ne la lit et la progression serait perdue.
                entry.snapshot.done = progress.done();
                entry.snapshot.total = progress.total();
                entry.snapshot.state = state;
                entry.snapshot.message = message;
                entry.snapshot.finished_at = Some(now());
                entry.progress = None;
            }
        }
        tracing::info!(id, state = state.as_str(), "job terminé");
    }

    /// Fait le travail. Rend un bilan lisible, ou une raison d'échec.
    ///
    /// Les deux traversent le réseau : ni chemin de fichier, ni fragment de message. La
    /// cause réelle est journalisée ici, côté démon.
    fn execute(&self, kind: Kind, progress: &Progress) -> Result<String, String> {
        let store = Store::open(&self.store).map_err(|source| {
            tracing::error!(%source, "ouverture du store pour un job");
            "le store est inutilisable".to_owned()
        })?;

        match kind {
            Kind::Import {
                source,
                dry_run,
                include_feeds,
                include_orphans,
            } => {
                let Some(profile) = self.sources.get(source) else {
                    return Err("profil inconnu".to_owned());
                };
                let mut options = ImportOptions::new(profile.as_std_path());
                options.dry_run = dry_run;
                options.include_feeds = include_feeds;
                options.include_orphans = include_orphans;

                let stats = import_profile(&store, &options, progress).map_err(|source| {
                    tracing::error!(%source, "import en échec");
                    "l'import a échoué : voir le journal du démon".to_owned()
                })?;
                Ok(format!(
                    "{} messages lus, {} contenus stockés, {} doublons ({:.1} %)",
                    stats.messages_read,
                    stats.blobs_created,
                    stats.duplicates,
                    stats.dedup_ratio()
                ))
            }

            Kind::Index => {
                let stats = mailcore::index::rebuild(&store, progress).map_err(|source| {
                    tracing::error!(%source, "indexation en échec");
                    "l'indexation a échoué : voir le journal du démon".to_owned()
                })?;

                // **Le point de tout ça.** Sans ce rechargement, le démon annoncerait
                // « indexation terminée » puis continuerait de chercher dans l'ancien index
                // jusqu'à son redémarrage.
                let available = self.reload_search();
                Ok(format!(
                    "{} messages indexés, recherche {}",
                    stats.indexed,
                    if available {
                        "disponible"
                    } else {
                        "indisponible"
                    }
                ))
            }

            Kind::Thread => {
                let stats = mailcore::thread::rebuild(&store, progress).map_err(|source| {
                    tracing::error!(%source, "threading en échec");
                    "le threading a échoué : voir le journal du démon".to_owned()
                })?;
                Ok(format!("{} fils reconstruits", stats.threads))
            }

            // **Une reconstruction complète, demandée explicitement.** La moisson, elle, avance le
            // carnet sans tout refaire ; ce job reste le moyen de repartir de zéro quand on
            // soupçonne une dérive — un message dont les références ont disparu garde sa
            // contribution, et seule la reconstruction la retire.
            Kind::Contacts => {
                let stats = mailcore::contacts::rebuild(&store, progress).map_err(|source| {
                    tracing::error!(%source, "carnet en échec");
                    "le carnet a échoué : voir le journal du démon".to_owned()
                })?;
                // Les envois sont dits, parce que c'est le contrôle du carnet : un zéro veut
                // dire que le classement n'a plus qu'un signal sur deux.
                Ok(format!(
                    "{} adresses, {} messages envoyés reconnus sur {} parcourus",
                    stats.addresses, stats.outgoing, stats.scanned
                ))
            }

            Kind::Sync { account } => {
                let said = sync(&store, account, progress)?;
                // **Le carnet suit la moisson**, depuis le 2026-09-11. Avant, une personne à qui
                // on venait d'écrire n'apparaissait en complétion qu'après un
                // `mail contacts rebuild` lancé à la main — autant dire jamais.
                //
                // C'est possible parce que la passe est **incrémentale** : elle ne regarde que
                // ce qu'elle n'a pas encore compté. Une reconstruction complète ici serait
                // intenable, `IDLE` mettant un `Kind::Sync` en file à chaque arrivée de
                // courrier — des minutes de processeur par mail reçu.
                //
                // Elle est faite ici plutôt que dans un job enchaîné : elle ne coûte rien quand
                // rien n'est arrivé, et un second job ferait clignoter le panneau des tâches à
                // chaque message.
                let mut said = said;

                // **L'index suit aussi**, et pour la même raison : sans lui, le courrier
                // arrivait et la recherche l'ignorait jusqu'à une réindexation à la main.
                // L'index d'abord, parce que c'est lui qu'on interroge en premier après une
                // moisson.
                match mailcore::index::update(&store, progress) {
                    Ok(stats) if stats.indexed > 0 => {
                        // Sans ce rechargement, le démon continuerait de chercher dans
                        // l'ancien index jusqu'à son redémarrage. Le même point que `Kind::Index`.
                        self.reload_search();
                        said.push_str(&format!(" — {} message(s) indexés", stats.indexed));
                    }
                    Ok(_) => {}
                    // Aucune des trois passes de suivi ne fait échouer la moisson : le courrier
                    // est arrivé, et c'est ce que l'utilisateur attendait. Le dire, et continuer.
                    Err(source) => {
                        tracing::warn!(%source, "index non mis à jour après la moisson");
                        said.push_str(" — index non mis à jour, voir le journal");
                    }
                }

                // **Les fils suivent, depuis le 2026-09-13.** C'était le dernier des trois
                // dérivés à ne pas suivre la moisson : une réponse arrivait et se présentait
                // comme un message isolé jusqu'à un `mail thread` lancé à la main.
                //
                // Elle est plus qu'un rattrapage des nouveaux — un fil se calcule à partir de
                // messages qui arrivent **après** celui qu'on traite — donc elle recalcule le
                // voisinage touché. Voir `mailcore::thread::update`. Quand ce voisinage est trop
                // large, elle refait tout et le dit : c'est ce que `full_pass` porte, et il est
                // rapporté parce qu'une passe qui a coûté une minute ne doit pas passer pour une
                // passe qui a coûté un millième de seconde.
                match mailcore::thread::update(&store, progress) {
                    Ok(stats) if stats.messages > 0 => {
                        said.push_str(&format!(
                            " — {} message(s) rattachés{}",
                            stats.messages,
                            if stats.full_pass {
                                ", par une reconstruction complète"
                            } else {
                                ""
                            }
                        ));
                    }
                    Ok(_) => {}
                    Err(source) => {
                        tracing::warn!(%source, "fils non mis à jour après la moisson");
                        said.push_str(" — fils non mis à jour, voir le journal");
                    }
                }

                match mailcore::contacts::update(&store, progress) {
                    Ok(stats) if stats.scanned > 0 => {
                        said.push_str(&format!(" — carnet : {} adresses", stats.addresses));
                    }
                    Ok(_) => {}
                    Err(source) => {
                        tracing::warn!(%source, "carnet non mis à jour après la moisson");
                        said.push_str(" — carnet non mis à jour, voir le journal");
                    }
                }
                Ok(said)
            }
        }
    }

    /// Remonte l'index de la boîte servie aux clients.
    fn reload_search(&self) -> bool {
        let mut mailbox = self
            .mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mailbox.reload_search()
    }

    /// Le registre, verrou empoisonné toléré — même raison que dans [`Jobs::lock`].
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Évince les jobs terminés les plus anciens au-delà de [`HISTORY`].
///
/// Ne touche jamais à un job en file ou en cours : leur oubli ferait disparaître un travail
/// qui tourne encore, et le client qui l'observe croirait à une panne.
fn prune(registry: &mut Vec<Entry>) {
    let finished = registry
        .iter()
        .filter(|entry| entry.snapshot.state.is_final())
        .count();
    if finished <= HISTORY {
        return;
    }

    let mut to_drop = finished - HISTORY;
    registry.retain(|entry| {
        if to_drop > 0 && entry.snapshot.state.is_final() {
            to_drop -= 1;
            return false;
        }
        true
    });
}

/// Synchronise les comptes IMAP actifs. Le corps du job [`Kind::Sync`].
///
/// ## Un compte qui échoue n'arrête pas les autres
///
/// Dix comptes, quatre fournisseurs : il y en aura toujours un qui ne répond pas. Un mot de
/// passe applicatif révoqué chez l'un ne doit pas empêcher les neuf autres de se synchroniser,
/// sinon la synchronisation d'ensemble devient aussi fiable que son maillon le plus faible.
///
/// Le job échoue quand **tous** les comptes ont échoué ; il réussit avec un bilan nuancé
/// sinon. C'est ce qui rend l'état du job utile à un planificateur.
///
/// ## Les messages sont vagues côté client, précis dans le journal
///
/// Même règle que le reste de l'API : un message d'erreur traverse le réseau et finit dans le
/// journal d'un client. Le nom d'hôte et l'identifiant restent donc côté démon
/// (`docs/PRIVACY.md` §8), et le client reçoit de quoi agir sans recevoir de quoi profiler.
fn sync(store: &Store, only: Option<i64>, progress: &Progress) -> Result<String, String> {
    let accounts = store.full_accounts().map_err(|source| {
        tracing::error!(%source, "lecture des comptes");
        "les comptes sont illisibles : voir le journal du démon".to_owned()
    })?;

    let wanted: Vec<&mailcore::Account> = accounts
        .iter()
        .filter(|it| it.kind == mailcore::AccountKind::Imap)
        .filter(|it| it.enabled)
        .filter(|it| only.is_none_or(|id| it.id.0 == id))
        .collect();

    if wanted.is_empty() {
        // Pas une erreur : un démon sans compte IMAP est un démon qui sert un corpus importé,
        // et c'est une configuration valide. Le dire est plus utile que d'échouer.
        return Ok("aucun compte IMAP actif à synchroniser".to_owned());
    }

    let mut total = mailsync::AccountReport::default();
    let mut failed = 0_usize;
    let mut cancelled = false;

    for account in &wanted {
        if progress.is_cancelled() {
            cancelled = true;
            break;
        }

        // Le secret est lu **juste avant de s'en servir**, un compte à la fois. Le lire pour
        // tous les comptes d'avance le garderait en mémoire pendant toute la durée du job
        // sans que ça serve à rien.
        let Some(server) = account.server.as_ref() else {
            tracing::warn!(compte = account.id.0, "compte imap sans serveur");
            failed += 1;
            continue;
        };
        // Un mot de passe pour un compte `password`, un jeton d'accès rafraîchi pour un compte
        // `oauth2` — **c'est ici que le rafraîchissement OAuth2 a lieu**, dans le trousseau du
        // démon, et pas dans celui de la machine qui a demandé la synchronisation. Un client
        // MCP distant ne voit jamais ni le jeton ni le rafraîchissement.
        let secret = match mailauth::session::secret_for(
            &server.host,
            &server.username,
            server.auth.as_str(),
            now(),
        ) {
            Ok(secret) => secret,
            Err(source) => {
                // L'hôte et l'identifiant sont dans le journal du démon, pas dans la réponse.
                // Un consentement mort n'est pas réessayable : le dire distingue « à relancer
                // plus tard » de « l'utilisateur doit rouvrir son navigateur ».
                tracing::warn!(
                    compte = account.id.0,
                    %source,
                    reessayable = source.retryable(),
                    "secret indisponible"
                );
                failed += 1;
                continue;
            }
        };

        let credential = mailsync::Credential::for_auth(server.auth, &secret);
        match mailsync::sync_account(store, account, credential, progress) {
            Ok(report) => {
                total.folders += report.folders;
                total.skipped += report.skipped;
                total.fetched += report.fetched;
                total.stored += report.stored;
                total.duplicates += report.duplicates;
                total.vanished += report.vanished;
                cancelled |= report.cancelled;
                if !report.failures.is_empty() {
                    for failure in &report.failures {
                        tracing::warn!(compte = account.id.0, %failure, "dossier en échec");
                    }
                    total.failures.extend(report.failures);
                }
                tracing::info!(
                    compte = account.id.0,
                    dossiers = report.folders,
                    evites = report.skipped,
                    recus = report.fetched,
                    nouveaux = report.stored,
                    "compte synchronisé"
                );
            }
            Err(source) => {
                // La distinction porte : « à réessayer » veut dire qu'un planificateur peut
                // relancer, et insister sur un mot de passe refusé fait bloquer le compte chez
                // le fournisseur.
                tracing::warn!(
                    compte = account.id.0,
                    reessayable = source.retryable(),
                    %source,
                    "compte en échec"
                );
                failed += 1;
            }
        }
    }

    if failed == wanted.len() {
        return Err(format!(
            "aucun des {failed} compte(s) n'a pu être synchronisé : voir le journal du démon"
        ));
    }

    let dedup = total
        .dedup_ratio()
        .map_or_else(String::new, |it| format!(", dédup {it:.1} %"));
    let interrupted = if cancelled { ", interrompu" } else { "" };
    let notes = match (failed, total.failures.len()) {
        (0, 0) => String::new(),
        (comptes, 0) => format!(", {comptes} compte(s) en échec"),
        (0, dossiers) => format!(", {dossiers} dossier(s) en échec"),
        (comptes, dossiers) => {
            format!(", {comptes} compte(s) et {dossiers} dossier(s) en échec")
        }
    };
    // Les dossiers évités sont dits, pas tus : « 80 dossiers en 5 s » sans explication ferait
    // douter du chiffre, et « 78 déjà à jour » le rend lisible.
    let avoided = if total.skipped > 0 {
        format!(" ({} déjà à jour)", total.skipped)
    } else {
        String::new()
    };
    Ok(format!(
        "{} dossier(s){avoided}, {} message(s) reçus, {} nouveau(x){dedup}{notes}{interrupted}",
        total.folders, total.fetched, total.stored
    ))
}

/// L'instant présent en secondes Unix.
///
/// Une horloge remontée avant 1970 rendrait zéro plutôt que de paniquer : une date fausse
/// dans un affichage vaut mieux qu'un démon qui tombe.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Attend qu'un job atteigne un état final, ou abandonne.
    fn wait_final(jobs: &Jobs, id: u64) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let snapshot = jobs.get(id).expect("le job existe");
            if snapshot.state.is_final() {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "le job n'a jamais fini");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Un registre monté sur un store vide et jetable.
    fn jobs(sources: Sources) -> (tempfile::TempDir, Jobs) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let mailbox = Arc::new(Mutex::new(Mailbox::open(&root).unwrap()));
        (dir, Jobs::start(mailbox, root, sources))
    }

    #[test]
    fn an_index_job_runs_and_reports_a_summary() {
        let (_dir, jobs) = jobs(Sources::default());
        let id = jobs.enqueue(Kind::Index).unwrap();
        let snapshot = wait_final(&jobs, id);
        assert_eq!(snapshot.state, State::Done, "{snapshot:?}");
        assert!(snapshot.message.unwrap().contains("indexés"));
        assert!(snapshot.finished_at.is_some());
    }

    #[test]
    fn indexing_makes_search_available_without_a_restart() {
        // La raison d'être du rechargement : le démon a ouvert sa boîte avant que l'index
        // existe, et doit pouvoir chercher après l'avoir construit.
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let mailbox = Arc::new(Mutex::new(Mailbox::open(&root).unwrap()));
        let jobs = Jobs::start(Arc::clone(&mailbox), root, Sources::default());

        let id = jobs.enqueue(Kind::Index).unwrap();
        assert_eq!(wait_final(&jobs, id).state, State::Done);

        let available = mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .search_available();
        assert!(available, "la recherche n'est pas revenue après indexation");
    }

    #[test]
    fn an_import_without_declared_sources_is_refused_with_a_useful_message() {
        // Fail closed : sans `--profile`, aucun import n'est possible.
        let (_dir, jobs) = jobs(Sources::default());
        let error = jobs
            .enqueue(Kind::Import {
                source: 0,
                dry_run: false,
                include_feeds: false,
                include_orphans: false,
            })
            .unwrap_err();
        assert!(matches!(error, StartError::NoSources));
        assert!(error.to_string().contains("--profile"));
    }

    #[test]
    fn a_client_cannot_reach_a_source_the_operator_did_not_declare() {
        let (_dir, jobs) = jobs(Sources::new(vec![Utf8PathBuf::from("/un/profil")]));
        assert!(matches!(
            jobs.enqueue(Kind::Import {
                source: 7,
                dry_run: false,
                include_feeds: false,
                include_orphans: false
            })
            .unwrap_err(),
            StartError::UnknownSource(1)
        ));
    }

    #[test]
    fn a_failing_job_says_so_without_leaking_a_path() {
        // Le profil déclaré n'existe pas : l'import échoue, et le message qui part vers le
        // client ne recopie pas le chemin.
        let (_dir, jobs) = jobs(Sources::new(vec![Utf8PathBuf::from(
            "/un/profil/qui/nexiste/pas/secret",
        )]));
        let id = jobs
            .enqueue(Kind::Import {
                source: 0,
                dry_run: false,
                include_feeds: false,
                include_orphans: false,
            })
            .unwrap();

        let snapshot = wait_final(&jobs, id);
        assert_eq!(snapshot.state, State::Failed);
        let message = snapshot.message.unwrap();
        assert!(!message.contains("secret"), "chemin fuité : {message}");
        assert!(!message.contains('/'), "chemin fuité : {message}");
    }

    #[test]
    fn a_queued_job_can_be_cancelled_before_it_ever_runs() {
        let (_dir, jobs) = jobs(Sources::default());
        // Mis en file puis annulé immédiatement : la course est réelle, donc on accepte les
        // deux issues — annulé sans démarrer, ou terminé avant qu'on ait pu l'annuler.
        let id = jobs.enqueue(Kind::Thread).unwrap();
        jobs.cancel(id);
        let snapshot = wait_final(&jobs, id);
        assert!(
            matches!(snapshot.state, State::Cancelled | State::Done),
            "{snapshot:?}"
        );
    }

    #[test]
    fn cancelling_a_finished_job_is_not_an_error() {
        // Une course perdue par le client, pas une faute. Lui rendre une erreur l'obligerait
        // à la traiter pour rien.
        let (_dir, jobs) = jobs(Sources::default());
        let id = jobs.enqueue(Kind::Index).unwrap();
        let done = wait_final(&jobs, id);
        assert_eq!(done.state, State::Done);

        let after = jobs.cancel(id).expect("le job existe encore");
        assert_eq!(after.state, State::Done, "l'état a été réécrit");
    }

    #[test]
    fn cancelling_an_unknown_job_yields_nothing() {
        let (_dir, jobs) = jobs(Sources::default());
        assert!(jobs.cancel(9_999).is_none());
    }

    #[test]
    fn jobs_run_one_at_a_time_and_in_order() {
        let (_dir, jobs) = jobs(Sources::default());
        let ids: Vec<u64> = (0..5)
            .map(|index| {
                jobs.enqueue(if index % 2 == 0 {
                    Kind::Index
                } else {
                    Kind::Thread
                })
                .unwrap()
            })
            .collect();

        for id in &ids {
            assert_eq!(wait_final(&jobs, *id).state, State::Done);
        }

        // Aucun chevauchement possible : les fins sont non décroissantes dans l'ordre de
        // mise en file, parce qu'un seul fil les exécute.
        let finished: Vec<i64> = ids
            .iter()
            .map(|id| jobs.get(*id).unwrap().finished_at.unwrap())
            .collect();
        assert!(
            finished.windows(2).all(|pair| pair[0] <= pair[1]),
            "les jobs se sont chevauchés : {finished:?}"
        );
    }

    #[test]
    fn the_listing_is_newest_first() {
        let (_dir, jobs) = jobs(Sources::default());
        let first = jobs.enqueue(Kind::Index).unwrap();
        let second = jobs.enqueue(Kind::Thread).unwrap();
        wait_final(&jobs, second);

        let listed = jobs.list();
        assert_eq!(listed[0].id, second);
        assert_eq!(listed[1].id, first);
    }

    #[test]
    fn the_history_is_bounded_but_never_forgets_a_live_job() {
        let (_dir, jobs) = jobs(Sources::default());
        for _ in 0..(HISTORY + 10) {
            let id = jobs.enqueue(Kind::Thread).unwrap();
            wait_final(&jobs, id);
        }
        let listed = jobs.list();
        assert!(
            listed.len() <= HISTORY + 1,
            "historique non borné : {}",
            listed.len()
        );
        assert!(listed.iter().all(|snapshot| snapshot.state.is_final()));
    }

    #[test]
    fn pruning_keeps_running_jobs_whatever_the_history_holds() {
        // Vérifié sur la fonction directement : provoquer trente-trois jobs terminés **plus**
        // un job en cours au bon moment est une course qu'on ne veut pas écrire.
        let mut registry: Vec<Entry> = (0..HISTORY + 5)
            .map(|index| Entry {
                snapshot: Snapshot {
                    id: index as u64,
                    kind: Kind::Index,
                    state: if index == 0 {
                        State::Running
                    } else {
                        State::Done
                    },
                    done: 0,
                    total: 0,
                    message: None,
                    queued_at: 0,
                    finished_at: None,
                },
                progress: None,
            })
            .collect();

        prune(&mut registry);
        assert!(
            registry.iter().any(|entry| entry.snapshot.id == 0),
            "un job en cours a été évincé"
        );
        assert_eq!(
            registry
                .iter()
                .filter(|entry| entry.snapshot.state.is_final())
                .count(),
            HISTORY
        );
    }
}

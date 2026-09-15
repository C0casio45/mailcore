//! Le veilleur : le courrier arrive sans qu'on demande (RFC 2177).
//!
//! ## Ce que `IDLE` est, et ce qu'il n'est pas
//!
//! C'est **du confort par-dessus un mécanisme**, pas le mécanisme. La synchronisation reste
//! celle des jobs — `Kind::Sync` — et tout ce que fait ce module est de la déclencher plus tôt
//! qu'une échéance. C'est pour ça qu'un serveur sans `IDLE`, ou un `IDLE` qui tombe en panne,
//! ne fait rien perdre : l'échéance, elle, arrive toujours.
//!
//! Cette hiérarchie décide de tout le reste du module. Un veilleur ne synchronise pas
//! lui-même, il **met un job en file** ; il ne tient aucun verrou du store ; et il a le droit
//! d'échouer en silence relatif — un compte dont le veilleur est tombé se synchronise encore à
//! l'échéance.
//!
//! ## Un dossier, pas quatre-vingts
//!
//! `IDLE` ne rapporte que la **boîte sélectionnée**. Surveiller les 80 dossiers du corpus
//! demanderait 80 connexions simultanées, là où Gmail en accorde une quinzaine par compte. Le
//! veilleur surveille donc l'`INBOX`, et une arrivée y déclenche une synchronisation du compte
//! **entier** — ce qui rattrape les autres dossiers.
//!
//! Ce n'est pas un compromis regrettable : le courrier qui arrive arrive dans l'`INBOX`, et le
//! reste — un message classé par une règle du serveur, un brouillon écrit sur un téléphone —
//! n'a aucune raison d'être vu à la seconde.
//!
//! ## Pourquoi un fil par compte, et pas de l'async
//!
//! Une connexion en `IDLE` est bloquée sur une lecture presque tout le temps. C'est le cas
//! d'usage exact d'un fil : dix comptes font dix fils qui ne consomment rien. Les passer en
//! async demanderait un client IMAP async, donc une deuxième implémentation du protocole à
//! côté de celle que la moisson utilise — deux analyseurs pour le même serveur finiraient par
//! ne plus lire la même chose.
//!
//! ## La reconnexion est bornée par une attente qui croît
//!
//! Un veilleur qui se reconnecte en boucle serrée devant un serveur qui refuse est ce qui fait
//! bloquer un compte chez un fournisseur. L'attente double à chaque échec jusqu'à un plafond,
//! et c'est une protection de l'utilisateur, pas une politesse.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use camino::Utf8Path;
use mailcore::{Account, AccountKind, AuthKind, Store};

use crate::jobs::{Jobs, Kind};

/// Tranche d'attente d'un `IDLE`.
///
/// C'est la latence d'arrêt du démon : le fil ne regarde le drapeau d'arrêt qu'entre deux
/// tranches. Cinq secondes est le compromis — assez court pour qu'un arrêt soit immédiat aux
/// yeux d'un opérateur, assez long pour que le réveil soit un appel système toutes les cinq
/// secondes et rien de plus.
const TICK: Duration = Duration::from_secs(5);

/// Durée maximale d'un `IDLE` avant de le renouveler.
///
/// La RFC 2177 §3 demande de ressortir de l'`IDLE` **au moins toutes les 29 minutes** : au-delà,
/// un serveur a le droit de considérer la connexion morte et de raccrocher. Vingt-quatre
/// minutes laissent une marge pour une connexion lente sans jamais frôler la limite.
const REARM: Duration = Duration::from_secs(24 * 60);

/// Attente avant de reconnecter après un échec, et son plafond.
///
/// Trente secondes au premier échec, puis le double à chaque fois, jusqu'à un quart d'heure.
/// Un serveur qui refuse refuse en général pour une raison qui ne se règle pas en une seconde —
/// un jeton à renouveler, une panne du fournisseur — et insister est le meilleur moyen de faire
/// bloquer le compte.
const BACKOFF: Duration = Duration::from_secs(30);
const BACKOFF_CAP: Duration = Duration::from_secs(15 * 60);

/// Plancher entre deux mises en attente `IDLE`.
///
/// Une seconde. Il ne sert que face à un serveur qui annoncerait quelque chose à chaque `IDLE` :
/// sans lui, la boucle tournerait à la vitesse du réseau et le fournisseur y verrait un client
/// qui le martèle. En régime normal une annonce est rare, et ce plancher ne coûte rien.
const MIN_ARM: Duration = Duration::from_secs(1);

/// Les veilleurs en cours, un par compte.
///
/// La structure ne sert qu'à les tenir en vie et à les arrêter : tout ce qu'ils font passe par
/// [`Jobs`].
pub struct Watchers {
    stop: Arc<AtomicBool>,
    /// Le fil de supervision, qui possède et joint ceux des comptes.
    supervisor: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Watchers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watchers")
            .field("en_vie", &self.supervisor.is_some())
            .finish()
    }
}

impl Watchers {
    /// Démarre un veilleur par compte IMAP actif.
    ///
    /// Aucun compte synchronisable n'est **pas** une erreur, et c'est dit dans le journal : un
    /// démon ouvert sur un store importé d'un profil Thunderbird n'a aucun compte IMAP, et il
    /// doit servir quand même.
    ///
    /// ## Rien de tout ça n'a lieu sur le chemin de démarrage
    ///
    /// La fonction **rend la main tout de suite** : un fil de supervision lit les comptes, puis
    /// démarre les veilleurs. Lire les comptes demande d'ouvrir le store une seconde fois —
    /// pragmas et contrôle de migration compris — et le critère 1 de `docs/PHASE-1.md` donne
    /// 400 ms à la coquille pour devenir utilisable. Un travail qui n'a aucune raison d'être
    /// fait avant la première image ne doit pas l'être.
    #[must_use]
    pub fn start(store: &Utf8Path, jobs: Jobs) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let supervisor_stop = Arc::clone(&stop);
        let root = store.to_owned();

        let started = std::thread::Builder::new()
            .name("mailcore-veille".to_owned())
            .spawn(move || supervise(&root, &jobs, &supervisor_stop));
        let supervisor = match started {
            Ok(handle) => Some(handle),
            // Ne pas pouvoir créer un fil est une panne du système. Les comptes se
            // synchroniseront à la demande comme avant : c'est exactement la raison pour
            // laquelle `IDLE` est du confort.
            Err(source) => {
                tracing::error!(%source, "veille non démarrée");
                None
            }
        };
        Self { stop, supervisor }
    }

    /// Demande l'arrêt et attend que les fils sortent.
    ///
    /// L'attente est bornée par [`TICK`] : un fil bloqué sur une lecture sort à la fin de sa
    /// tranche.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // La poignée est **retirée** plutôt que déstructurée : `Drop` est implémenté, donc on
        // ne peut pas sortir un champ de `self`. Et c'est très bien ainsi — le `Drop` reposera
        // le drapeau, ce qui est idempotent.
        if let Some(handle) = self.supervisor.take() {
            // Un veilleur qui a paniqué ne doit pas empêcher le démon de s'arrêter.
            drop(handle.join());
        }
        tracing::info!("veille arrêtée");
    }
}

/// Lit les comptes, démarre un veilleur par compte, et les joint à l'arrêt.
///
/// Vit dans son propre fil pour que [`Watchers::start`] ne lise pas le store avant la première
/// image de la coquille.
fn supervise(store: &Utf8Path, jobs: &Jobs, stop: &Arc<AtomicBool>) {
    let accounts = match watchable(store) {
        Ok(accounts) => accounts,
        Err(source) => {
            tracing::warn!(%source, "comptes illisibles : aucune veille");
            return;
        }
    };
    if accounts.is_empty() {
        tracing::info!("aucun compte IMAP actif : aucune veille");
        return;
    }
    // L'arrêt peut être demandé pendant qu'on lisait les comptes — un démon qui démarre et
    // s'arrête aussitôt, ou un test. Ouvrir cinq connexions IMAP à ce moment-là serait du
    // travail pour rien, et cinq déconnexions sales chez le fournisseur.
    if stop.load(Ordering::Relaxed) {
        return;
    }

    let mut threads = Vec::with_capacity(accounts.len());
    for account in accounts {
        let stop = Arc::clone(stop);
        let jobs = jobs.clone();
        let started = std::thread::Builder::new()
            .name(format!("mailcore-veille-{}", account.id.0))
            .spawn(move || watch(&account, &jobs, &stop));
        match started {
            Ok(handle) => threads.push(handle),
            Err(source) => tracing::error!(%source, "veilleur non démarré"),
        }
    }
    tracing::info!(comptes = threads.len(), "veille démarrée");

    for handle in threads {
        drop(handle.join());
    }
}

impl Drop for Watchers {
    /// Le drapeau est posé même si personne n'a appelé [`Watchers::stop`].
    ///
    /// Sans ça, un `Daemon` détruit laisserait ses veilleurs tourner sur un store qui n'est
    /// plus servi — et, en test, un fil par cas de test jusqu'à la fin du binaire.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Les comptes qu'on peut surveiller.
fn watchable(store: &Utf8Path) -> anyhow::Result<Vec<Account>> {
    let store = Store::open(store)?;
    Ok(store
        .full_accounts()?
        .into_iter()
        .filter(|it| it.kind == AccountKind::Imap && it.enabled && it.server.is_some())
        .collect())
}

/// La boucle d'un veilleur : connecter, attendre, déclencher, recommencer.
fn watch(account: &Account, jobs: &Jobs, stop: &AtomicBool) {
    let mut backoff = BACKOFF;
    while !stop.load(Ordering::Relaxed) {
        match session(account, jobs, stop) {
            Ok(()) => {
                // Sortie propre : le démon s'arrête. Rien à réessayer.
                return;
            }
            Err(source) => {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                tracing::warn!(
                    compte = account.id.0,
                    %source,
                    attente = backoff.as_secs(),
                    "veille interrompue : reconnexion différée"
                );
                if !sleep_unless_stopped(backoff, stop) {
                    return;
                }
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
        }
    }
}

/// Une session de veille : une connexion, tenue jusqu'à l'arrêt ou jusqu'à une panne.
///
/// Rend `Ok(())` **seulement** quand l'arrêt a été demandé. Tout le reste est une erreur, donc
/// une reconnexion après attente.
fn session(account: &Account, jobs: &Jobs, stop: &AtomicBool) -> anyhow::Result<()> {
    let server = account
        .server
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("compte sans serveur"))?;

    // Le secret est relu à **chaque** connexion, jamais mémorisé : c'est ce qui fait qu'un
    // jeton OAuth2 rafraîchi entre-temps est pris en compte sans redémarrer le démon, et qu'un
    // secret révoqué cesse de marcher.
    let secret =
        mailauth::session::secret_for(&server.host, &server.username, server.auth.as_str(), now())?;
    let mut client = mailsync::connect(server)?;
    match server.auth {
        AuthKind::Password => client.login(&server.username, &secret)?,
        AuthKind::OAuth2 => client.authenticate_xoauth2(&server.username, &secret)?,
    }
    drop(secret);
    watch_over(&mut client, account.id.0, jobs, stop)
}

/// La veille sur une connexion **déjà authentifiée**.
///
/// ## Pourquoi la connexion est un paramètre
///
/// La même raison que pour `mailsync::sync_account_over` : c'est cette fonction que les tests
/// exercent contre `mailfake`, qui parle en clair. `mailsync::connect` refuse — à juste titre —
/// un serveur non chiffré, donc une fonction qui établirait elle-même la connexion serait
/// intestable. [`session`] est la couche au-dessus : elle chiffre, s'authentifie, puis appelle
/// celle-ci.
///
/// Sans cette séparation, la boucle qui décide **quand synchroniser** n'aurait aucune
/// couverture d'intégration — seules les fonctions pures autour en auraient, ce qui laisserait
/// le mécanisme lui-même non testé.
fn watch_over<S>(
    client: &mut mailsync::Client<S>,
    account: i64,
    jobs: &Jobs,
    stop: &AtomicBool,
) -> anyhow::Result<()>
where
    S: std::io::Read + std::io::Write + mailsync::Timed,
{
    // La même raison qu'à la moisson : plusieurs serveurs n'annoncent `IDLE` qu'une fois
    // authentifiés, et s'en tenir au salut ferait croire qu'ils ne l'ont pas.
    client.refresh_capabilities_if_silent()?;
    let idle = client.has("IDLE");
    tracing::info!(compte = account, idle, "veille en place");

    if !idle {
        // **La connexion est rendue.** Sans `IDLE` il n'y a rien à écouter, et la garder
        // ouverte pour ne jamais la lire prendrait une des quelques connexions simultanées que
        // le fournisseur accorde, pendant des heures, pour rien. L'échéance devient le seul
        // mécanisme — c'est-à-dire le comportement d'avant cette étape, qui reste correct.
        client.logout();
        loop {
            if !sleep_unless_stopped(REARM, stop) {
                return Ok(());
            }
            tracing::debug!(compte = account, "échéance sans IDLE : synchronisation");
            trigger(account, jobs);
        }
    }

    // `IDLE` ne surveille que la boîte courante. L'`INBOX` est la seule qui compte pour une
    // arrivée, et c'est le seul nom de boîte que la RFC 3501 §5.1 rend universel.
    client.examine(b"INBOX")?;

    loop {
        let armed_at = Instant::now();
        let tag = client.idle_start()?;
        let outcome = wait(client, stop)?;
        // Les événements annoncés pendant la sortie comptent autant que celui qui nous a
        // réveillés : on sort d'abord, on décide ensuite.
        client.idle_done(&tag)?;

        // **Un plancher entre deux `IDLE`.** Un serveur qui annoncerait quelque chose à chaque
        // fois qu'on se met en attente — par bavardage, ou parce qu'il compte mal — ferait
        // tourner cette boucle aussi vite que le réseau le permet, et le fournisseur y verrait
        // un client qui le martèle. En régime normal l'annonce est rare et ce plancher ne coûte
        // rien : il ne s'applique que si le tour a duré moins d'une seconde.
        if let Some(left) = MIN_ARM.checked_sub(armed_at.elapsed())
            && !sleep_unless_stopped(left, stop)
        {
            return Ok(());
        }

        match outcome {
            Outcome::Stopped => return Ok(()),
            Outcome::Event => {
                tracing::info!(compte = account, "arrivée annoncée : synchronisation");
                trigger(account, jobs);
            }
            Outcome::Deadline => {
                // **L'échéance déclenche aussi.** Un événement d'`IDLE` peut être perdu — une
                // connexion coupée sans rien dire, un serveur qui n'annonce pas tout — et un
                // veilleur qui ne se fierait qu'aux événements laisserait alors le compte
                // périmé pour toujours. C'est la même raison qui rend `IDLE` du confort : le
                // mécanisme, c'est l'échéance.
                tracing::debug!(compte = account, "échéance : synchronisation");
                trigger(account, jobs);
            }
        }
    }
}

/// Ce qui a mis fin à une attente.
enum Outcome {
    /// Le serveur a annoncé quelque chose.
    Event,
    /// [`REARM`] est écoulé sans rien.
    Deadline,
    /// L'arrêt a été demandé.
    Stopped,
}

/// Attend un événement d'`IDLE`, par tranches, jusqu'à l'échéance.
///
/// La tranche est une **lecture bornée**, pas un sommeil : elle rend l'événement dès qu'il
/// arrive, et n'attend la fin de la tranche que s'il n'arrive rien. Le courrier ne subit donc
/// aucun retard dû au découpage — le découpage ne sert qu'à regarder le drapeau d'arrêt.
fn wait<S>(client: &mut mailsync::Client<S>, stop: &AtomicBool) -> anyhow::Result<Outcome>
where
    S: std::io::Read + std::io::Write + mailsync::Timed,
{
    let deadline = Instant::now() + REARM;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return Ok(Outcome::Stopped);
        }
        if let Some(event) = client.idle_wait(TICK)? {
            tracing::debug!(evenement = %event.text, "IDLE a parlé");
            return Ok(Outcome::Event);
        }
    }
    Ok(Outcome::Deadline)
}

/// Met une synchronisation du compte en file, sauf s'il y en a déjà une qui le couvre.
///
/// ## Pourquoi la garde existe
///
/// Un veilleur peut être réveillé plusieurs fois pendant qu'une synchronisation tourne — dix
/// messages qui arrivent d'affilée, c'est dix annonces. Sans garde, la file accumulerait dix
/// jobs qui feraient dix fois le même travail, dont neuf sur un store déjà à jour.
fn trigger(id: i64, jobs: &Jobs) {
    if covered(&jobs.list(), id) {
        tracing::debug!(compte = id, "synchronisation déjà en cours ou en file");
        return;
    }
    match jobs.enqueue(Kind::Sync { account: Some(id) }) {
        Ok(job) => tracing::info!(compte = id, job, "synchronisation mise en file"),
        Err(source) => tracing::warn!(compte = id, %source, "synchronisation non mise en file"),
    }
}

/// Vrai si un job encore vivant va déjà synchroniser ce compte. **Fonction pure.**
///
/// Séparée de [`trigger`] pour être testable : décider en même temps qu'on met en file
/// demanderait de faire tourner le fil de travail, donc de courser un job qui échoue vite — et
/// un test qui court après une course ne prouve rien de façon reproductible.
///
/// Un job `Sync { account: None }` couvre **tous** les comptes : c'est « synchronise tout », et
/// en ajouter un pour un compte particulier ferait refaire ce qu'il va faire de toute façon.
fn covered(pending: &[crate::jobs::Snapshot], account: i64) -> bool {
    pending.iter().any(|it| {
        !it.state.is_final()
            && matches!(it.kind, Kind::Sync { account: only } if only.is_none_or(|it| it == account))
    })
}

/// Dort, en se réveillant assez souvent pour voir un arrêt. Rend faux si l'arrêt est demandé.
fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(left.min(TICK));
    }
    !stop.load(Ordering::Relaxed)
}

/// L'horloge, en secondes Unix.
fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |it| it.as_secs()),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{covered, sleep_unless_stopped};
    use crate::jobs::{Kind, Snapshot, State};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn job(kind: Kind, state: State) -> Snapshot {
        Snapshot {
            id: 1,
            kind,
            state,
            done: 0,
            total: 0,
            message: None,
            queued_at: 0,
            finished_at: None,
        }
    }

    #[test]
    fn nothing_pending_means_nothing_covers() {
        assert!(!covered(&[], 4));
    }

    #[test]
    fn a_sync_of_this_account_covers_it() {
        let pending = [job(Kind::Sync { account: Some(4) }, State::Running)];
        assert!(covered(&pending, 4));
    }

    #[test]
    fn a_sync_of_every_account_covers_this_one_too() {
        // « Synchronise tout » va passer sur ce compte : en ajouter un ferait le travail deux
        // fois, dont une sur un store déjà à jour.
        let pending = [job(Kind::Sync { account: None }, State::Queued)];
        assert!(covered(&pending, 4));
    }

    #[test]
    fn a_sync_of_another_account_does_not_cover_this_one() {
        let pending = [job(Kind::Sync { account: Some(2) }, State::Running)];
        assert!(!covered(&pending, 4));
    }

    #[test]
    fn a_finished_sync_covers_nothing() {
        // Le cas qui compte : sans le test sur l'état, un compte cesserait d'être synchronisé
        // dès sa première synchronisation réussie — et le veilleur ne servirait plus à rien.
        for state in [State::Done, State::Cancelled, State::Failed] {
            let pending = [job(Kind::Sync { account: Some(4) }, state)];
            assert!(!covered(&pending, 4), "{state:?} ne doit pas couvrir");
        }
    }

    #[test]
    fn a_job_of_another_kind_covers_nothing() {
        let pending = [
            job(Kind::Index, State::Running),
            job(Kind::Thread, State::Queued),
        ];
        assert!(!covered(&pending, 4));
    }

    #[test]
    fn a_sleep_gives_up_as_soon_as_the_stop_flag_is_set() {
        // La borne d'arrêt d'un veilleur. Sans elle, arrêter le démon attendrait l'échéance.
        let stop = AtomicBool::new(true);
        let started = Instant::now();
        assert!(!sleep_unless_stopped(Duration::from_secs(30), &stop));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "l'arrêt n'a pas été vu tout de suite"
        );
    }

    #[test]
    fn a_sleep_that_is_not_stopped_waits_and_says_so() {
        let stop = AtomicBool::new(false);
        let started = Instant::now();
        assert!(sleep_unless_stopped(Duration::from_millis(80), &stop));
        assert!(started.elapsed() >= Duration::from_millis(80));
        assert!(!stop.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod loop_tests {
    //! La boucle de veille, **contre un vrai serveur et un vrai registre de jobs**.
    //!
    //! Ce que les tests des fonctions pures ne disent pas : est-ce qu'une arrivée annoncée
    //! finit par mettre une synchronisation en file ? C'est le mécanisme entier du module, et
    //! c'était la seule partie sans couverture.

    use super::{Jobs, Kind, watch_over};
    use crate::jobs::Sources;
    use mailcore::Mailbox;
    use mailfake::{Config, Fault, Server};
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Un registre de jobs sur un store vide et jetable.
    fn jobs() -> (tempfile::TempDir, Jobs) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let mailbox = Arc::new(Mutex::new(Mailbox::open(&root).unwrap()));
        (dir, Jobs::start(mailbox, root, Sources::default()))
    }

    /// Un client connecté, authentifié, avec un délai de lecture court.
    ///
    /// Le délai court est ce qui rend les tranches d'attente observables : sans lui, un tour de
    /// boucle sur une boîte tranquille durerait le délai par défaut du serveur de test.
    fn connect(server: &Server) -> mailsync::Client<TcpStream> {
        let stream = TcpStream::connect(server.address()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut client = mailsync::Client::greet(stream).unwrap();
        client.login("marie@exemple.fr", "secret").unwrap();
        client
    }

    /// Attend qu'un job de synchronisation apparaisse, ou rend faux au bout de `patience`.
    ///
    /// Sonde le registre plutôt que d'attendre une durée fixe : le veilleur passe par un
    /// aller-retour réseau et par un `EXAMINE` avant de pouvoir annoncer quoi que ce soit, et
    /// une attente fixe serait soit trop courte sur une machine lente, soit du temps perdu.
    fn wait_for_sync(jobs: &Jobs, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        while Instant::now() < deadline {
            if jobs
                .list()
                .iter()
                .any(|it| matches!(it.kind, Kind::Sync { .. }))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn an_announced_arrival_queues_a_synchronisation() {
        // **Le test qui compte.** C'est la raison d'être du module : le serveur dit qu'il s'est
        // passé quelque chose, et une synchronisation part.
        let server = Server::start(Config::with_inbox().announcing_on_idle("* 4 EXISTS")).unwrap();
        let (_dir, jobs) = jobs();
        let mut client = connect(&server);
        let stop = Arc::new(AtomicBool::new(false));

        let watcher_stop = Arc::clone(&stop);
        let watcher_jobs = jobs.clone();
        let watcher =
            std::thread::spawn(move || watch_over(&mut client, 7, &watcher_jobs, &watcher_stop));

        assert!(
            wait_for_sync(&jobs, Duration::from_secs(10)),
            "aucune synchronisation mise en file après une arrivée annoncée"
        );
        stop.store(true, Ordering::Relaxed);
        drop(watcher.join());

        let queued = jobs.list();
        assert!(
            queued
                .iter()
                .any(|it| matches!(it.kind, Kind::Sync { account: Some(7) })),
            "la synchronisation ne vise pas le compte du veilleur : {queued:?}"
        );
    }

    #[test]
    fn a_quiet_server_queues_nothing() {
        // L'autre moitié : un serveur qui se tait ne doit **rien** déclencher avant l'échéance,
        // qui est à vingt-quatre minutes. Un veilleur qui synchroniserait par acquit de
        // conscience à chaque tranche de cinq secondes serait pire que pas de veilleur du tout.
        let server =
            Server::start(Config::with_inbox().with_fault(Fault::IdleStaysSilent)).unwrap();
        let (_dir, jobs) = jobs();
        let mut client = connect(&server);
        let stop = Arc::new(AtomicBool::new(false));

        let watcher_stop = Arc::clone(&stop);
        let watcher_jobs = jobs.clone();
        let watcher =
            std::thread::spawn(move || watch_over(&mut client, 7, &watcher_jobs, &watcher_stop));

        assert!(
            !wait_for_sync(&jobs, Duration::from_secs(2)),
            "une synchronisation a été mise en file alors que le serveur n'a rien dit"
        );
        stop.store(true, Ordering::Relaxed);
        drop(watcher.join());
    }

    #[test]
    fn a_watcher_stops_when_it_is_asked_to() {
        // La borne d'arrêt du démon, mesurée : le fil doit sortir en une tranche, pas attendre
        // la prochaine arrivée — qui peut ne jamais venir.
        let server =
            Server::start(Config::with_inbox().with_fault(Fault::IdleStaysSilent)).unwrap();
        let (_dir, jobs) = jobs();
        let mut client = connect(&server);
        let stop = Arc::new(AtomicBool::new(false));

        let watcher_stop = Arc::clone(&stop);
        let watcher = std::thread::spawn(move || watch_over(&mut client, 7, &jobs, &watcher_stop));

        std::thread::sleep(Duration::from_millis(300));
        let asked = Instant::now();
        stop.store(true, Ordering::Relaxed);
        let outcome = watcher.join();

        assert!(
            asked.elapsed() < super::TICK * 2,
            "l'arrêt a demandé {:?}, soit plus de deux tranches",
            asked.elapsed()
        );
        assert!(
            matches!(outcome, Ok(Ok(()))),
            "un arrêt demandé est une sortie normale, pas une erreur : {outcome:?}"
        );
    }

    #[test]
    fn a_server_without_idle_queues_nothing_before_the_deadline() {
        // Sans `IDLE`, le veilleur rend sa connexion et attend l'échéance. Rien ne doit partir
        // avant — et surtout, il doit rester arrêtable pendant cette attente.
        let server = Server::start(Config::with_inbox().without_idle()).unwrap();
        let (_dir, jobs) = jobs();
        let mut client = connect(&server);
        let stop = Arc::new(AtomicBool::new(false));

        let watcher_stop = Arc::clone(&stop);
        let watcher_jobs = jobs.clone();
        let watcher =
            std::thread::spawn(move || watch_over(&mut client, 7, &watcher_jobs, &watcher_stop));

        assert!(
            !wait_for_sync(&jobs, Duration::from_millis(600)),
            "une synchronisation est partie avant l'échéance"
        );
        let asked = Instant::now();
        stop.store(true, Ordering::Relaxed);
        drop(watcher.join());
        assert!(
            asked.elapsed() < super::TICK * 2,
            "le chemin sans IDLE n'est pas arrêtable en une tranche : {:?}",
            asked.elapsed()
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod courtesy_tests {
    //! La régression du 2026-09-09, vue depuis le veilleur.

    use super::super::watch::watch_over;
    use crate::jobs::{Jobs, Kind, Sources};
    use mailcore::Mailbox;
    use mailfake::{Config, Server};
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn a_courtesy_line_does_not_queue_a_synchronisation() {
        // Le serveur dit `* OK Still here` dès la mise en attente, et rien d'autre. Le veilleur
        // ne doit **rien** déclencher : c'est de la politesse, pas du courrier.
        let server =
            Server::start(Config::with_inbox().announcing_on_idle("* OK Still here")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(dir.path()).unwrap().to_owned();
        let mailbox = Arc::new(Mutex::new(Mailbox::open(&root).unwrap()));
        let jobs = Jobs::start(mailbox, root, Sources::default());

        let stream = TcpStream::connect(server.address()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut client = mailsync::Client::greet(stream).unwrap();
        client.login("marie@exemple.fr", "secret").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let watcher_stop = Arc::clone(&stop);
        let watcher_jobs = jobs.clone();
        let watcher =
            std::thread::spawn(move || watch_over(&mut client, 7, &watcher_jobs, &watcher_stop));

        std::thread::sleep(Duration::from_secs(2));
        let queued = jobs.list();
        stop.store(true, Ordering::Relaxed);
        drop(watcher.join());

        assert!(
            !queued.iter().any(|it| matches!(it.kind, Kind::Sync { .. })),
            "une ligne de courtoisie a déclenché une synchronisation : {queued:?}"
        );
    }
}

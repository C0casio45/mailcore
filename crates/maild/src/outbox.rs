//! Le facteur : la boucle qui vide la file d'envoi, en fond.
//!
//! ## Pourquoi une boucle et pas un envoi synchrone
//!
//! La règle 3 du `CLAUDE.md` : l'interface ne bloque jamais sur le réseau. Un clic sur
//! « envoyer » écrit une ligne dans la file et rend la main ; c'est ce fil-ci qui parle au
//! serveur. Une poignée de main TLS, une authentification et un transfert de 25 Mo sur une
//! liaison montante ordinaire, c'est de l'ordre de la minute — une interface qui attend ça est
//! une interface figée.
//!
//! Le corollaire compte autant : un message est **en file avant d'être parti**, donc il survit
//! à la fermeture de la fenêtre, à l'arrêt du démon et à une coupure de courant. C'est ce que le
//! critère 2 de `docs/PHASE-3.md` demande, et ce fil n'en est que le moteur — la garantie est
//! dans `mailsmtp::queue`.
//!
//! ## Ce que ce fil ne décide pas
//!
//! Ni le doute, ni le recul, ni l'abandon. Tout ça est dans `mailsmtp::queue::deliver_one`, qui
//! est aussi ce qu'appelle `mail send`. Deux moteurs, une seule règle — et c'est délibéré : une
//! deuxième copie de la décision finirait par diverger, et la divergence s'appelle ici un
//! doublon chez le destinataire.
//!
//! ## Le réveil
//!
//! Le fil dort sur une variable de condition, pas sur un délai. [`Postman::nudge`] le réveille
//! quand un client vient de mettre un message en file ; sans elle, un envoi attendrait le tic
//! suivant, et une interface qui met cinq secondes à partir donne l'impression de n'avoir rien
//! fait. Le tic reste, comme filet : il rattrape les messages reportés par un recul, et ceux
//! qu'un autre processus — `mail send` — a mis en file sans pouvoir nous prévenir.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use mailcore::{Account, Outgoing, Store};
use mailsmtp::client::Credential;
use mailsmtp::queue::{Outcome, deliver_one};
use mailsmtp::submit::Submitter;

/// Le filet : à quelle fréquence regarder la file sans avoir été réveillé.
///
/// Trente secondes. Assez court pour qu'un message reporté par le recul minimal — une minute —
/// ne traîne pas, assez long pour qu'un démon au repos ne relise pas la file cent fois par
/// minute pour rien. Le cas normal ne l'utilise pas : [`Postman::nudge`] réveille tout de suite.
const TICK: Duration = Duration::from_secs(30);

/// Combien de messages remettre par passage.
///
/// Dix, et la borne n'est pas là pour la mémoire : elle est là pour qu'un arrêt du démon
/// n'attende pas la remise de cent messages. Entre deux lots, le drapeau d'arrêt est relu.
const BATCH: u32 = 10;

/// Le fil qui vide la file d'envoi.
pub struct Postman {
    stop: Arc<AtomicBool>,
    /// Ce sur quoi le fil dort. Le booléen est le réveil demandé.
    wake: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Postman {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Postman")
            .field("en_vie", &self.handle.is_some())
            .finish()
    }
}

impl Postman {
    /// Démarre le facteur.
    ///
    /// **N'ouvre pas le store sur le chemin appelant**, pour la même raison que
    /// `watch::Watchers::start` : le démon démarre derrière la première image de la coquille, et
    /// le critère 1 de la phase 2 se mesure là.
    #[must_use]
    pub fn start(store: &Utf8Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(false), Condvar::new()));
        let root: Utf8PathBuf = store.to_owned();
        let thread_stop = Arc::clone(&stop);
        let thread_wake = Arc::clone(&wake);

        let started = std::thread::Builder::new()
            .name("mailcore-facteur".to_owned())
            .spawn(move || run(&root, &thread_stop, &thread_wake));
        let handle = match started {
            Ok(handle) => Some(handle),
            // Ne pas pouvoir créer un fil est une panne du système. La file reste remettable à
            // la main par `mail send --flush` : rien n'est perdu, seulement différé.
            Err(source) => {
                tracing::error!(%source, "facteur non démarré : la file ne partira pas seule");
                None
            }
        };
        Self { stop, wake, handle }
    }

    /// Réveille le facteur tout de suite.
    ///
    /// Appelée par `outbox.send` juste après avoir mis un message en file. Sans effet si le fil
    /// n'a pas démarré, et sans effet néfaste si le fil est déjà en train de travailler : le
    /// booléen reste posé et le passage suivant le consomme.
    pub fn nudge(&self) {
        let (posted, condvar) = &*self.wake;
        if let Ok(mut posted) = posted.lock() {
            *posted = true;
        }
        condvar.notify_all();
    }

    /// Demande l'arrêt et attend le fil.
    ///
    /// **Peut attendre la fin d'une remise en cours**, et c'est voulu : couper au milieu d'un
    /// transfert laisserait un message douteux pour rien. Le pire cas est le délai de lecture
    /// SMTP, cinq minutes, sur un serveur qui ne répond plus après le point final.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.nudge();
        if let Some(handle) = self.handle.take() {
            // Un facteur qui a paniqué ne doit pas empêcher le démon de s'arrêter.
            drop(handle.join());
        }
        tracing::info!("facteur arrêté");
    }
}

impl Drop for Postman {
    /// Le drapeau est posé même si personne n'a appelé [`Postman::stop`].
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.nudge();
    }
}

/// La boucle : dormir, regarder la file, remettre, recommencer.
fn run(root: &Utf8Path, stop: &AtomicBool, wake: &(Mutex<bool>, Condvar)) {
    // Le store est ouvert **une fois**, dans ce fil. Le réouvrir à chaque passage coûterait une
    // migration de schéma vérifiée trente fois par minute.
    let store = match Store::open(root) {
        Ok(store) => store,
        Err(source) => {
            tracing::error!(%source, "store illisible : la file ne partira pas seule");
            return;
        }
    };

    tracing::info!("facteur démarré");
    while !stop.load(Ordering::Relaxed) {
        let sent = round(&store, stop);
        if sent > 0 {
            // Quelque chose est parti : peut-être qu'il en reste. Reboucler sans dormir plutôt
            // que d'attendre trente secondes entre deux messages d'un même lot.
            continue;
        }
        sleep_unless_woken(stop, wake);
    }
}

/// Un passage : remet ce qui est remettable, et rend combien de messages sont partis.
///
/// Les comptes sont relus à chaque passage. C'est une lecture SQLite locale, et ça fait qu'un
/// compte configuré pendant que le démon tourne peut envoyer sans redémarrage.
fn round(store: &Store, stop: &AtomicBool) -> usize {
    let now = now();
    let pending = match store.deliverable(now, BATCH) {
        Ok(pending) => pending,
        Err(source) => {
            tracing::warn!(%source, "file illisible");
            return 0;
        }
    };
    if pending.is_empty() {
        return 0;
    }
    let accounts = match store.full_accounts() {
        Ok(accounts) => accounts,
        Err(source) => {
            tracing::warn!(%source, "comptes illisibles : rien ne peut partir");
            return 0;
        }
    };

    let mut sent = 0;
    for job in pending {
        if stop.load(Ordering::Relaxed) {
            // L'arrêt est relu **entre** deux messages, jamais au milieu d'un. Interrompre une
            // remise en cours créerait un doute que personne n'a demandé.
            break;
        }
        if deliver(store, &accounts, &job, now) {
            sent += 1;
        }
    }
    sent
}

/// Remet un message. Vrai s'il est parti.
///
/// Les échecs sont journalisés et **écrits dans la file** par `deliver_one` : c'est là que
/// l'utilisateur les lira, pas dans le journal du démon.
fn deliver(store: &Store, accounts: &[Account], job: &Outgoing, now: i64) -> bool {
    let Some(account) = accounts.iter().find(|it| it.id == job.account) else {
        // Un compte supprimé emporte sa file par cascade, donc ce cas veut dire qu'un compte
        // existe sans être lisible. Le laisser en file plutôt que l'abandonner : le corriger
        // est une action de l'utilisateur.
        tracing::warn!(
            job = job.id.0,
            "compte introuvable : message laissé en file"
        );
        return false;
    };
    let (Some(reading), Some(submission)) = (&account.server, &account.submission) else {
        tracing::warn!(
            job = job.id.0,
            compte = account.id.0,
            "compte sans serveur d'envoi : message laissé en file"
        );
        return false;
    };

    // Le secret est celui de la lecture, et le branchement sur le mécanisme est chez
    // `mailauth` : deux copies finiraient par ne plus lire la même entrée de trousseau.
    let secret = match mailauth::session::secret_for(
        &reading.host,
        &reading.username,
        submission.auth.as_str(),
        now,
    ) {
        Ok(secret) => secret,
        Err(source) => {
            tracing::warn!(job = job.id.0, %source, "secret indisponible");
            return false;
        }
    };

    let mut transport = Submitter {
        server: submission,
        credential: Credential::for_auth(submission.auth, &secret),
    };
    match deliver_one(store, &mut transport, job, now) {
        Ok(Outcome::Sent) => true,
        Ok(Outcome::Doubtful) => {
            // Journalisé en `warn` parce que c'est le seul état qui demande un humain. Le
            // texte que l'utilisateur lira, lui, est dans la ligne de file.
            tracing::warn!(
                job = job.id.0,
                "envoi douteux : aucune reprise automatique, décision attendue"
            );
            false
        }
        Ok(other) => {
            tracing::info!(job = job.id.0, outcome = ?other, "envoi non abouti");
            false
        }
        Err(source) => {
            tracing::warn!(job = job.id.0, %source, "remise impossible");
            false
        }
    }
}

/// Dort jusqu'au tic, jusqu'à un réveil, ou jusqu'à l'arrêt.
///
/// Le booléen est **consommé** au réveil. Sans ça, un `nudge` posé pendant un passage ferait
/// tourner la boucle en continu : le passage suivant trouverait le drapeau encore levé, ne
/// dormirait pas, et recommencerait.
fn sleep_unless_woken(stop: &AtomicBool, wake: &(Mutex<bool>, Condvar)) {
    let (posted, condvar) = wake;
    let Ok(mut held) = posted.lock() else {
        // Le mutex est empoisonné : un autre fil a paniqué en le tenant. Dormir sans lui plutôt
        // que de tourner à vide — la boucle continue de fonctionner, seulement sans réveil
        // immédiat.
        std::thread::sleep(TICK);
        return;
    };
    if *held {
        *held = false;
        return;
    }
    let outcome = condvar.wait_timeout(held, TICK);
    if let Ok((mut held, _)) = outcome {
        *held = false;
    }
    // `stop` est relu par la boucle appelante : le `nudge` de `Postman::stop` a réveillé.
    let _ = stop;
}

/// Secondes Unix.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Postman, TICK, sleep_unless_woken};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Condvar, Mutex};

    fn wake() -> Arc<(Mutex<bool>, Condvar)> {
        Arc::new((Mutex::new(false), Condvar::new()))
    }

    #[test]
    fn a_nudge_posted_before_the_sleep_returns_at_once() {
        // La course qui compte : `outbox.send` réveille pendant que le fil travaille encore, et
        // le fil arrive au sommeil après. Sans le drapeau, il dormirait trente secondes avec un
        // message en file — une interface qui a l'air de n'avoir rien fait.
        let wake = wake();
        if let Ok(mut posted) = wake.0.lock() {
            *posted = true;
        }
        let stop = AtomicBool::new(false);

        let started = std::time::Instant::now();
        sleep_unless_woken(&stop, &wake);
        assert!(
            started.elapsed() < TICK / 2,
            "le réveil posé d'avance a été perdu : {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_flag_is_consumed_so_the_loop_does_not_spin() {
        // Sans la consommation, le passage suivant trouverait le drapeau encore levé et la
        // boucle tournerait en continu — un démon au repos à 100 % d'un cœur.
        let wake = wake();
        if let Ok(mut posted) = wake.0.lock() {
            *posted = true;
        }
        let stop = AtomicBool::new(false);
        sleep_unless_woken(&stop, &wake);

        assert!(
            !*wake.0.lock().unwrap(),
            "le drapeau de réveil n'a pas été consommé"
        );
    }

    #[test]
    fn a_nudge_from_another_thread_wakes_the_sleeper() {
        let wake = wake();
        let stop = Arc::new(AtomicBool::new(false));
        let nudger = Arc::clone(&wake);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if let Ok(mut posted) = nudger.0.lock() {
                *posted = true;
            }
            nudger.1.notify_all();
        });

        let started = std::time::Instant::now();
        sleep_unless_woken(&stop, &wake);
        assert!(
            started.elapsed() < TICK / 2,
            "le dormeur n'a pas été réveillé : {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_postman_on_an_unreadable_store_stops_by_itself_without_panicking() {
        // Un store illisible est une panne du système, pas une raison de faire tomber le démon :
        // le reste de l'API sert encore ce qui est déjà chargé.
        let postman = Postman::start(camino::Utf8Path::new(
            "F:/ce-chemin-n-existe-pas/mailcore-test",
        ));
        postman.stop();
    }

    #[test]
    fn stopping_a_postman_that_never_started_is_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let postman = Postman::start(&root);
        // Deux `nudge` avant l'arrêt : l'un pendant que le fil s'installe, l'autre après.
        postman.nudge();
        postman.nudge();
        postman.stop();
    }
}

//! La poignée partagée d'une tâche longue : où elle en est, et comment l'arrêter.
//!
//! Les trois tâches longues de la phase 1 — import, indexation, threading — la prennent
//! toutes, avec la même signature. Une seule forme à apprendre, et un appelant qui sait
//! suivre l'une sait suivre les autres.
//!
//! ## Sans verrou, et c'est le point
//!
//! Le producteur écrit depuis le fil qui travaille, les lecteurs lisent depuis le fil qui
//! sert l'interface. Avec un `Mutex`, chaque lecture d'une barre de progression prendrait un
//! verrou que la tâche relâche des milliers de fois par seconde — l'interface ralentirait la
//! chose qu'elle observe. Des atomiques ne coûtent rien à personne.
//!
//! `Ordering::Relaxed` partout : ces compteurs ne protègent aucune autre donnée, ils sont la
//! donnée. Voir un compteur en retard d'un cran n'a aucune conséquence, et l'annulation est
//! consultée en boucle — un cran de retard veut dire qu'on s'arrête une itération plus tard.
//!
//! ## Ce que ce type ne fait pas
//!
//! Il ne dit pas *ce que* la tâche fait, seulement combien il en reste. L'unité appartient à
//! la tâche — des octets pour l'import, des messages pour l'indexation — et l'appelant qui
//! affiche une fraction n'a pas besoin de la connaître. Un libellé d'étape serait une chaîne,
//! donc un verrou, donc le problème ci-dessus.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// L'état d'avancement d'une tâche longue, et son drapeau d'annulation.
#[derive(Debug, Default)]
pub struct Progress {
    cancelled: AtomicBool,
    done: AtomicU64,
    total: AtomicU64,
}

impl Progress {
    /// Une poignée neuve : rien de fait, total inconnu, pas annulée.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
        }
    }

    /// Demande l'arrêt.
    ///
    /// **Coopératif** : la tâche s'arrête à sa prochaine vérification, pas à l'instant. Ce
    /// qu'elle a déjà écrit reste écrit — pour l'import comme pour l'indexation, un travail
    /// partiel est valide et le relancer reprend là où il en est, par adressage de contenu
    /// pour l'un et par reconstruction complète pour l'autre.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Vrai si l'arrêt a été demandé. À consulter dans la boucle de travail.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Annonce le volume total prévu, dans l'unité de la tâche.
    ///
    /// Appelable plusieurs fois : une tâche qui découvre son périmètre au fur et à mesure
    /// affine son total, et une barre qui recule un peu vaut mieux qu'une barre qui ment.
    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    /// Avance de `units`.
    pub fn advance(&self, units: u64) {
        self.done.fetch_add(units, Ordering::Relaxed);
    }

    /// Repart de zéro avec un nouveau total.
    ///
    /// Pour une tâche qui **renonce et recommence autrement** — `thread::update` qui retombe sur
    /// une reconstruction complète. Affiner le total ne suffit pas là : ce qui est déjà compté ne
    /// vaut plus rien, et le garder afficherait une barre pleine pendant toute la reconstruction,
    /// c'est-à-dire une barre qui ment jusqu'au bout.
    ///
    /// Recule, donc, et c'est voulu : une barre qui recule dit qu'il se passe autre chose, là où
    /// une barre bloquée à 100 % ressemble à une panne.
    pub fn restart(&self, total: u64) {
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    /// Ce qui est fait.
    #[must_use]
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    /// Ce qui est prévu. `0` quand la tâche ne le sait pas encore.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// La fraction accomplie, entre `0.0` et `1.0`.
    ///
    /// `None` quand le total est inconnu : une barre indéterminée est honnête, une barre à
    /// zéro qui ne bouge pas ressemble à une panne.
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        // Plafonné à 1.0 : un total affiné à la baisse en cours de route ne doit pas produire
        // une barre à 130 %.
        Some((self.done() as f32 / total as f32).min(1.0))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_handle_knows_nothing_and_asks_nothing() {
        let progress = Progress::new();
        assert!(!progress.is_cancelled());
        assert_eq!(progress.done(), 0);
        assert_eq!(progress.total(), 0);
        assert_eq!(progress.fraction(), None);
    }

    #[test]
    fn an_unknown_total_yields_no_fraction_rather_than_zero() {
        // Une barre indéterminée est honnête ; une barre à zéro qui ne bouge pas ressemble
        // à une panne.
        let progress = Progress::new();
        progress.advance(500);
        assert_eq!(progress.fraction(), None);
    }

    #[test]
    fn the_fraction_never_exceeds_one() {
        let progress = Progress::new();
        progress.set_total(10);
        progress.advance(25);
        assert_eq!(progress.fraction(), Some(1.0));
    }

    #[test]
    fn cancellation_is_visible_from_another_thread() {
        let progress = std::sync::Arc::new(Progress::new());
        let worker = std::sync::Arc::clone(&progress);
        let handle = std::thread::spawn(move || {
            while !worker.is_cancelled() {
                worker.advance(1);
                std::thread::yield_now();
            }
            worker.done()
        });

        // Laisser le travailleur démarrer, puis l'arrêter.
        while progress.done() == 0 {
            std::thread::yield_now();
        }
        progress.cancel();
        let done = handle.join().unwrap();
        assert!(done > 0);
        assert!(progress.is_cancelled());
    }

    #[test]
    fn a_revised_total_is_allowed_because_a_task_may_discover_its_scope() {
        let progress = Progress::new();
        progress.set_total(100);
        progress.advance(50);
        assert_eq!(progress.fraction(), Some(0.5));
        progress.set_total(200);
        assert_eq!(progress.fraction(), Some(0.25));
    }
}

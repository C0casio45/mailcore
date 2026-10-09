//! L'heure locale de l'interface, en un seul endroit.
//!
//! ## Pourquoi plus l'heure UTC
//!
//! Les dates étaient affichées en UTC depuis la phase 1, avec en commentaire « à corriger le
//! jour où l'affichage de l'heure exacte comptera ». Il compte depuis que la liste dit « 09:42 »
//! pour un message du jour : en France, c'était 11:42, et un rendez-vous « dans une heure » se
//! lisait faux de deux. Le décalage d'un instant dépend des règles d'été **de son année**, donc
//! d'une base de fuseaux — d'où `jiff`, plutôt qu'un décalage fixe lu une fois qui se tromperait
//! d'une heure sur la moitié du corpus.
//!
//! ## Hors du chemin de démarrage
//!
//! Lire le fuseau du système coûte une lecture de registre sous Windows et l'analyse d'une
//! entrée de la base. [`warm`] le fait sur un fil dès la construction de l'application ; la
//! première ligne de liste, qui en a besoin, le trouve en général prêt, et sinon attend la fin
//! du même calcul plutôt que de le refaire — c'est ce que garantit `OnceLock`.

use std::sync::OnceLock;

/// Le fuseau du système, lu une fois par processus.
static ZONE: OnceLock<jiff::tz::TimeZone> = OnceLock::new();

/// Le fuseau, lu au premier appel. UTC si le système n'en dit rien d'exploitable : une heure
/// décalée vaut mieux qu'une interface qui refuse de s'afficher.
fn zone() -> &'static jiff::tz::TimeZone {
    ZONE.get_or_init(|| {
        jiff::tz::TimeZone::try_system().unwrap_or_else(|error| {
            tracing::debug!(%error, "fuseau du système illisible : dates en UTC");
            jiff::tz::TimeZone::UTC
        })
    })
}

/// Lit le fuseau sur un fil, pour que la première image ne le paie pas.
pub fn warm() {
    let spawned = std::thread::Builder::new()
        .name("mailcore-shell-fuseau".to_owned())
        .spawn(|| {
            let _ = zone();
        });
    if let Err(source) = spawned {
        tracing::debug!(%source, "fil du fuseau non démarré : il sera lu à la première date");
    }
}

/// Le décalage local à l'instant `unix`, en secondes. Zéro pour un instant hors de ce que la
/// base sait représenter — une date d'en-tête aberrante, le corpus en contient.
pub fn offset(unix: i64) -> i64 {
    jiff::Timestamp::from_second(unix).map_or(0, |at| i64::from(zone().to_offset(at).seconds()))
}

/// L'instant présent, en secondes Unix.
///
/// Relu à chaque appel, et c'est voulu : la liste dit « aujourd'hui » et « hier », et une
/// valeur gardée pour la durée du processus ferait mentir une coquille ouverte la veille. Une
/// lecture d'horloge coûte quelques dizaines de nanosecondes ; quarante lignes à soixante
/// images font moins d'un dixième de milliseconde par seconde.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| i64::try_from(it.as_secs()).unwrap_or(0))
}

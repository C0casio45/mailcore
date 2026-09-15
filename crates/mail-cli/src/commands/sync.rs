//! `mail sync` : découvrir les dossiers d'un compte, puis les moissonner.
//!
//! ## Un compte à la fois, et un échec n'arrête pas les autres
//!
//! Dix comptes, quatre fournisseurs : il y en aura toujours un qui ne répond pas. Un mot de
//! passe applicatif révoqué chez l'un ne doit pas empêcher les neuf autres de se
//! synchroniser — sinon la synchronisation d'ensemble devient aussi fiable que son maillon le
//! plus faible.
//!
//! Chaque compte est donc tenté, et le bilan dit ce qui a marché. Le code de sortie est non
//! nul dès qu'un compte a échoué : un script doit pouvoir le savoir.
//!
//! ## Un dossier qui échoue n'arrête pas le compte
//!
//! Même raisonnement d'un cran plus bas. Un dossier au nom exotique, une boîte qu'un
//! administrateur a rendue inaccessible : le reste du compte se synchronise.
//!
//! ## Ce que la commande ne fait pas
//!
//! **Elle ne tourne pas en boucle.** Une synchronisation périodique est une tâche de fond du
//! démon, pas une commande qui ne rend jamais la main. Celle-ci fait un passage et s'arrête —
//! c'est ce qui la rend utilisable dans un planificateur, et vérifiable.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Account, AccountKind, Store};

/// Le bilan d'un compte.
#[derive(Debug, Default)]
struct Tally {
    folders: usize,
    skipped: usize,
    fetched: usize,
    stored: usize,
    duplicates: usize,
    vanished: usize,
    failures: Vec<String>,
}

/// Synchronise les comptes IMAP actifs.
///
/// `only` restreint à un compte, par identifiant.
///
/// # Errors
///
/// Si le store est illisible. Un compte qui échoue n'est **pas** une erreur rendue : il est
/// compté, affiché, et la fonction rend `Ok(false)` pour que l'appelant sorte en échec.
pub fn run(root: Option<&Utf8PathBuf>, only: Option<i64>) -> Result<bool> {
    let root = crate::store_root(root)?;
    let store = Store::open(&root).with_context(|| format!("ouverture du store {root}"))?;

    let accounts = store.full_accounts()?;
    let wanted: Vec<&Account> = accounts
        .iter()
        .filter(|it| it.kind == AccountKind::Imap)
        .filter(|it| only.is_none_or(|id| it.id.0 == id))
        .filter(|it| {
            // Un compte en pause est passé en silence quand on synchronise tout, mais **pas**
            // quand on le nomme : « rien ne s'est passé » sans explication est le pire des
            // retours.
            if it.enabled {
                return true;
            }
            if only.is_some() {
                println!(
                    "Compte #{} en pause — `mail account add` pour le réactiver.",
                    it.id.0
                );
            }
            false
        })
        .collect();

    if wanted.is_empty() {
        match only {
            Some(id) => println!("Aucun compte IMAP actif portant le numéro {id}."),
            None => println!("Aucun compte IMAP actif. Voir `mail account list`."),
        }
        return Ok(true);
    }

    let mut all_fine = true;
    for account in wanted {
        let tally = one(&store, account);
        report(account, &tally);
        all_fine &= tally.failures.is_empty();
    }
    Ok(all_fine)
}

/// Synchronise un compte, en attrapant les échecs plutôt qu'en les propageant.
///
/// **La boucle elle-même vit dans `mailsync::sync_account`**, pas ici. Elle est partagée avec
/// le job de fond du démon : deux copies de « pour chaque dossier, moissonner » divergeraient,
/// et la version locale finirait par se comporter autrement que la version distante sur le
/// même compte.
///
/// Ce qui reste ici est ce qui appartient à la CLI : lire le secret, et présenter le bilan.
fn one(store: &Store, account: &Account) -> Tally {
    let mut tally = Tally::default();
    let Some(server) = account.server.as_ref() else {
        tally
            .failures
            .push("compte sans serveur : incohérent en base".to_owned());
        return tally;
    };

    // Un mot de passe pour un compte `password`, un jeton d'accès rafraîchi pour un compte
    // `oauth2`. Le branchement est chez `mailauth`, pas ici : deux copies finiraient par ne
    // plus lire la même entrée de trousseau.
    let secret = match mailauth::session::secret_for(
        &server.host,
        &server.username,
        server.auth.as_str(),
        now(),
    ) {
        Ok(secret) => secret,
        Err(source) => {
            tally.failures.push(source.to_string());
            return tally;
        }
    };

    // Pas d'annulation en ligne de commande : un Ctrl-C tue le processus, et la moisson
    // reprend au passage suivant parce que l'état de synchronisation n'a pas avancé. La
    // progression sert au démon, qui a un `jobs.cancel`.
    let progress = mailcore::Progress::new();
    let credential = mailsync::Credential::for_auth(server.auth, &secret);
    match mailsync::sync_account(store, account, credential, &progress) {
        Ok(report) => {
            tally.folders = report.folders;
            tally.skipped = report.skipped;
            tally.fetched = report.fetched;
            tally.stored = report.stored;
            tally.duplicates = report.duplicates;
            tally.vanished = report.vanished;
            tally.failures = report.failures;
        }
        Err(source) => tally.failures.push(retryable(&source)),
    }
    tally
}

/// L'instant présent en secondes Unix.
///
/// Sert à décider si un jeton d'accès en cache est encore bon. Une horloge remontée avant 1970
/// rendrait zéro, donc « le jeton est périmé », donc un rafraîchissement inutile — ce qui est
/// la bonne façon de se tromper : le compte marche quand même.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Le message d'une erreur, avec ce qu'il faut en faire.
///
/// La distinction n'est pas cosmétique : « à réessayer » veut dire qu'un planificateur peut
/// relancer, « à corriger » veut dire qu'insister ne servira à rien — et sur un mot de passe
/// refusé, insister fait bloquer le compte chez le fournisseur.
fn retryable(source: &mailsync::Error) -> String {
    if source.retryable() {
        format!("{source} (à réessayer)")
    } else {
        format!("{source} (à corriger)")
    }
}

/// Écrit le bilan d'un compte.
fn report(account: &Account, tally: &Tally) {
    println!();
    println!("Compte #{} — {}", account.id.0, account.display_name);
    println!(
        "  Dossiers        {} ({} déjà à jour, sans EXAMINE)",
        tally.folders, tally.skipped
    );
    println!("  Corps reçus     {}", tally.fetched);
    println!(
        "  Nouveaux        {} ({} déjà connus)",
        tally.stored, tally.duplicates
    );
    if tally.vanished > 0 {
        println!("  Disparus        {}", tally.vanished);
    }

    // La dédup est le pari de la phase 1 : l'afficher est ce qui le vérifie au quotidien.
    if tally.fetched > 0 {
        #[allow(clippy::cast_precision_loss)]
        let rate = tally.duplicates as f64 / tally.fetched as f64 * 100.0;
        println!("  Dédup           {rate:.1} % du reçu était déjà dans le store");
    }

    if tally.failures.is_empty() {
        println!("  Verdict         terminé");
    } else {
        println!("  Verdict         {} échec(s) :", tally.failures.len());
        for failure in &tally.failures {
            println!("    - {failure}");
        }
    }
}

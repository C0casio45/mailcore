//! `mail account probe` : ce que le serveur sait faire, et ce que ça coûte.
//!
//! ## Pourquoi une commande et pas une note dans un journal
//!
//! Le critère 3 de `docs/PHASE-2.md` échoue sur un compte, et la cause y est écrite :
//! dix-huit dossiers, un aller-retour `EXAMINE` chacun, ~0,46 s l'aller-retour. Le correctif
//! dépend entièrement de ce que le serveur accepte — `LIST-STATUS` (RFC 5819) rend l'état de
//! toutes les boîtes en **une** commande, mais seulement s'il l'annonce.
//!
//! Deviner la réponse à partir de ce qu'on croit savoir des capacités d'un fournisseur est
//! exactement ce que le `CLAUDE.md` interdit : « mesuré sur le corpus réel — pas estimé ».
//! Cette commande est la mesure, et elle reste après le correctif parce que la question se
//! reposera au compte suivant.
//!
//! ## Elle ne touche à rien
//!
//! Aucune écriture dans le store, aucune écriture côté serveur : un `LIST`, un `LIST-STATUS`
//! s'il existe, un `EXAMINE` par boîte — toutes des commandes de lecture. Le seul effet de
//! bord est une connexion sortante vers un hôte **déjà configuré**, ce que le critère 5
//! autorise explicitement.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::{Account, AccountKind, AuthKind, Store};
use mailsync::client::Listed;
use mailsync::{Client, MailboxStatus};

/// Les éléments demandés à `LIST-STATUS`.
///
/// Exactement ceux dont `mailsync::plan` a besoin pour décider sans sélectionner la boîte :
/// `UIDVALIDITY` pour savoir si les UID connus valent encore quelque chose, `UIDNEXT` pour
/// savoir si un message est arrivé, `MESSAGES` pour savoir si un message a disparu.
/// `HIGHESTMODSEQ` n'est demandé qu'avec `CONDSTORE` activé, parce qu'un serveur qui ne l'a
/// pas activé a le droit de refuser l'élément (RFC 7162 §3.1.2).
const STATUS_ITEMS: &str = "MESSAGES UIDNEXT UIDVALIDITY";
const STATUS_ITEMS_CONDSTORE: &str = "MESSAGES UIDNEXT UIDVALIDITY HIGHESTMODSEQ";

/// Les extensions qui changent quelque chose pour la moisson, et qu'on veut voir d'un coup
/// d'œil. La liste complète est affichée à part : elle est longue, et c'est celle qu'on relit
/// le jour où un fournisseur se met à répondre autrement.
const WATCHED: [&str; 6] = [
    "CONDSTORE",
    "QRESYNC",
    "LIST-EXTENDED",
    "LIST-STATUS",
    "ESEARCH",
    "IDLE",
];

/// Ce qu'un passage a appris sur un compte.
#[derive(Debug, Default)]
struct Findings {
    /// Les capacités annoncées après authentification.
    capabilities: Vec<String>,
    /// Ce que l'`ENABLE` a réellement obtenu — pas ce qui était annoncé.
    condstore: bool,
    qresync: bool,
    /// Boîtes annoncées par `LIST`, et celles qui sont sélectionnables.
    listed: usize,
    selectable: usize,
    /// Durée du `LIST` nu.
    list_took: Duration,
    /// `LIST-STATUS` : `None` s'il n'est pas annoncé, sinon les `* STATUS` reçus et le temps.
    list_status: Option<(usize, Duration)>,
    /// L'état rendu pour chaque boîte, affiché avec `--detail`.
    ///
    /// C'est ce qui répond à « pourquoi ce dossier-là n'a-t-il pas été sauté ? ». Sans les
    /// valeurs sous les yeux, la réponse se devine, et deviner sur un serveur qu'on ne
    /// contrôle pas est exactement ce que ce projet évite.
    states: Vec<MailboxStatus>,
    /// Durée de chaque `EXAMINE`, dans l'ordre des boîtes.
    examines: Vec<Duration>,
    /// Ce qui a mal tourné, s'il y a lieu.
    failure: Option<String>,
}

/// Interroge les comptes IMAP actifs et affiche ce qu'ils savent faire.
///
/// `only` restreint à un compte, par identifiant.
///
/// # Errors
///
/// Si le store est illisible. Un compte injoignable n'est **pas** une erreur rendue : il est
/// affiché avec sa raison, et la fonction rend `Ok(false)` pour que l'appelant sorte en échec.
pub fn run(root: Option<&Utf8PathBuf>, only: Option<i64>, detail: bool) -> Result<bool> {
    let root = crate::store_root(root)?;
    let store = Store::open(&root).with_context(|| format!("ouverture du store {root}"))?;

    let accounts = store.full_accounts()?;
    let wanted: Vec<&Account> = accounts
        .iter()
        .filter(|it| it.kind == AccountKind::Imap && it.enabled)
        .filter(|it| only.is_none_or(|id| it.id.0 == id))
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
        let findings = one(account);
        report(account, &findings, detail);
        all_fine &= findings.failure.is_none();
    }
    Ok(all_fine)
}

/// Interroge un compte, en attrapant l'échec plutôt qu'en le propageant.
fn one(account: &Account) -> Findings {
    let mut findings = Findings::default();
    let Some(server) = account.server.as_ref() else {
        findings.failure = Some("compte sans serveur : incohérent en base".to_owned());
        return findings;
    };

    // Le même chemin de trousseau que `mail sync` : une deuxième façon de lire le secret
    // finirait par ne plus lire la même entrée.
    let secret = match mailauth::session::secret_for(
        &server.host,
        &server.username,
        server.auth.as_str(),
        now(),
    ) {
        Ok(secret) => secret,
        Err(source) => {
            findings.failure = Some(source.to_string());
            return findings;
        }
    };

    let mut client = match mailsync::connect(server) {
        Ok(client) => client,
        Err(source) => {
            findings.failure = Some(source.to_string());
            return findings;
        }
    };
    let authenticated = match server.auth {
        AuthKind::Password => client.login(&server.username, &secret),
        AuthKind::OAuth2 => client.authenticate_xoauth2(&server.username, &secret),
    };
    if let Err(source) = authenticated {
        findings.failure = Some(source.to_string());
        return findings;
    }

    if let Err(source) = interrogate(&mut client, &mut findings) {
        findings.failure = Some(source.to_string());
    }
    client.logout();
    findings
}

/// Le dialogue lui-même, une fois authentifié.
fn interrogate<S: Read + Write>(client: &mut Client<S>, findings: &mut Findings) -> Result<()> {
    // Plusieurs serveurs n'annoncent leurs capacités complètes qu'une fois authentifiés :
    // celles du salut ne disent rien de ce qui nous intéresse ici.
    client.refresh_capabilities()?;
    findings.capabilities = client.capabilities().to_vec();
    findings.capabilities.sort_unstable();

    // **Avant tout `EXAMINE`**, comme dans la moisson : `ENABLE` n'est pas valide une fois une
    // boîte sélectionnée (RFC 5161 §3.1).
    let enabled = mailsync::enable(client)?;
    findings.condstore = enabled.condstore;
    findings.qresync = enabled.qresync;

    let started = Instant::now();
    let boxes = client.list()?;
    findings.list_took = started.elapsed();
    findings.listed = boxes.len();
    let selectable: Vec<Listed> = boxes.into_iter().filter(Listed::selectable).collect();
    findings.selectable = selectable.len();

    // **La question qui décide du correctif du critère 3.** `LIST-STATUS` demande l'état de
    // toutes les boîtes en une commande ; s'il répond, les dix-huit allers-retours deviennent
    // un seul.
    if client.has("LIST-STATUS") {
        let items = if enabled.condstore {
            STATUS_ITEMS_CONDSTORE
        } else {
            STATUS_ITEMS
        };
        let started = Instant::now();
        let states = client.list_status(items)?;
        let took = started.elapsed();
        findings.list_status = Some((states.len(), took));
        findings.states = states;
    }

    // Le coût qu'on cherche à supprimer, mesuré tel quel : une boîte, un aller-retour.
    for mailbox in &selectable {
        let started = Instant::now();
        client.examine(&mailbox.name)?;
        findings.examines.push(started.elapsed());
    }
    Ok(())
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

/// Affiche ce qu'on a trouvé.
fn report(account: &Account, findings: &Findings, detail: bool) {
    let host = account
        .server
        .as_ref()
        .map_or_else(|| "?".to_owned(), |it| format!("{}:{}", it.host, it.port));
    println!("#{} {} — {host}", account.id.0, account.display_name);

    if let Some(reason) = &findings.failure {
        println!("  échec : {reason}");
        println!();
        return;
    }

    for name in WATCHED {
        let announced = findings
            .capabilities
            .iter()
            .any(|it| it.eq_ignore_ascii_case(name));
        let mark = if announced { "annoncé" } else { "—" };
        println!("  {name:<14} {mark}");
    }
    println!(
        "  ENABLE         condstore {}, qresync {}",
        oui(findings.condstore),
        oui(findings.qresync)
    );
    println!(
        "  LIST           {} boîtes, {} sélectionnables — {} ms",
        findings.listed,
        findings.selectable,
        findings.list_took.as_millis()
    );
    match findings.list_status {
        Some((answered, took)) => println!(
            "  LIST-STATUS    {answered} STATUS pour {} boîtes — {} ms",
            findings.selectable,
            took.as_millis()
        ),
        None => println!("  LIST-STATUS    non annoncé"),
    }

    // La dispersion avant la médiane, comme partout ailleurs dans ce projet : deux `EXAMINE`
    // qui s'écartent d'un facteur trois ne se résument pas par leur milieu.
    if findings.examines.is_empty() {
        println!("  EXAMINE        aucune boîte sélectionnable");
    } else {
        let total: Duration = findings.examines.iter().sum();
        let mut sorted: Vec<u128> = findings.examines.iter().map(Duration::as_millis).collect();
        sorted.sort_unstable();
        println!(
            "  EXAMINE        {} boîtes — {} ms au total, min {} ms, médiane {} ms, max {} ms",
            findings.examines.len(),
            total.as_millis(),
            sorted[0],
            sorted[sorted.len() / 2],
            sorted[sorted.len() - 1],
        );
    }
    println!("  capacités      {}", findings.capabilities.join(" "));

    // Les valeurs elles-mêmes, sur demande. C'est ce qui permet de répondre à « pourquoi ce
    // dossier-là n'a-t-il pas été sauté ? » en regardant, plutôt qu'en supposant : les trois
    // nombres affichés sont exactement ceux que `mailsync::plan` compare à l'état local.
    if detail && !findings.states.is_empty() {
        println!("  état des boîtes :");
        for state in &findings.states {
            println!(
                "    {:<40} messages {:>7}  uidnext {:>8}  uidvalidity {:>10}  modseq {}",
                mailsync::mutf7::decode(&state.name),
                show(state.messages),
                show(state.uidnext),
                show(state.uidvalidity),
                state
                    .highest_modseq
                    .map_or_else(|| "—".to_owned(), |it| it.to_string()),
            );
        }
    }
    println!();
}

/// Un nombre, ou un tiret quand le serveur ne l'a pas donné.
///
/// Un champ absent n'est **pas** un zéro : zéro veut dire « la boîte est vide », et les
/// confondre à l'affichage ferait chercher un bug là où il n'y a qu'un silence.
fn show(value: Option<u32>) -> String {
    value.map_or_else(|| "—".to_owned(), |it| it.to_string())
}

/// « oui » ou « non », pour que la sortie se lise sans décoder un booléen.
const fn oui(value: bool) -> &'static str {
    if value { "oui" } else { "non" }
}

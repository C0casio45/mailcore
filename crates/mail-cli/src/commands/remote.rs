//! Les mêmes commandes, mais contre un démon plutôt que contre le store local.
//!
//! ## Pourquoi la CLI a besoin de ça
//!
//! Le déploiement de référence met le démon sur une machine dédiée et les clients ailleurs.
//! Une CLI qui n'ouvre que le store local n'y sert qu'à une chose : être lancée en SSH sur la
//! machine du démon. C'est précisément ce qu'on cherchait à éviter en écrivant l'API.
//!
//! C'est aussi la validation que les tests ne donnent pas : `mail --daemon` est le premier
//! **vrai** client de `mailapi`, écrit par-dessus le contrat sans rien savoir de l'intérieur
//! du démon. Si le contrat est mal fichu, ça se voit ici avant de se voir dans le front.
//!
//! ## Ce qui reste local
//!
//! `mail doctor` inspecte les blobs, les orphelins, la dérive de l'index. Ça demande un accès
//! au store, pas une API : l'exposer voudrait dire ouvrir un chemin de lecture arbitraire dans
//! le stockage, ce que `docs/PRIVACY.md` et le bon sens déconseillent tous les deux. La
//! commande refuse en mode distant, et dit pourquoi.

use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};
use mailapi::client::Client;
use mailapi::dto;
use serde_json::json;

use mailapi::human;

/// Intervalle entre deux relevés d'une tâche de fond qu'on suit.
///
/// 500 ms : assez pour qu'une barre bouge visiblement, assez peu pour qu'un import de quatre
/// minutes ne représente que quelques centaines de requêtes triviales.
const POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// Ouvre une connexion vers un démon, jeton pris dans le trousseau.
///
/// # Errors
///
/// Si aucun jeton n'est enregistré, ou si le démon est injoignable.
pub fn connect(daemon: &str) -> Result<Client> {
    let token = mailapi::token::load(daemon)?;
    Ok(Client::connect(daemon, &token)?)
}

/// `mail daemon login` : enregistre le jeton d'un démon dans le trousseau.
///
/// Le jeton se lit sur **l'entrée standard**, jamais en argument. Un argument de ligne de
/// commande est visible dans la table des processus par les autres utilisateurs de la
/// machine, et atterrit dans l'historique du shell — pour un secret qui ouvre une boîte mail
/// entière, ni l'un ni l'autre n'est acceptable (`docs/PRIVACY.md`, §7).
///
/// # Errors
///
/// Si l'entrée standard est vide ou si le trousseau est inutilisable.
pub fn login(daemon: &str) -> Result<()> {
    eprint!("Jeton pour {daemon} (entrée standard) : ");
    std::io::stderr().flush().ok();

    let mut token = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut token)
        .context("lecture du jeton")?;
    let token = token.trim();

    if token.is_empty() {
        bail!("jeton vide : rien n'a été enregistré");
    }

    mailapi::token::store(daemon, token)?;
    println!("Jeton enregistré pour {daemon} dans le trousseau du système.");

    // Vérifier tout de suite plutôt que de laisser découvrir à la prochaine commande qu'on a
    // enregistré un jeton qui ne marche pas.
    match connect(daemon).and_then(|mut client| Ok(client.call("server.hello", json!(null))?)) {
        Ok(hello) => {
            println!(
                "Démon joignable : {} messages, protocole {}.",
                hello["messages"], hello["protocol"]
            );
            Ok(())
        }
        Err(source) => {
            println!("Attention : le jeton est enregistré mais le démon n'a pas répondu.");
            println!("  {source}");
            Ok(())
        }
    }
}

/// `mail daemon logout` : oublie le jeton d'un démon.
///
/// # Errors
///
/// Si le trousseau est inutilisable.
pub fn logout(daemon: &str) -> Result<()> {
    mailapi::token::forget(daemon)?;
    println!("Jeton oublié pour {daemon}.");
    Ok(())
}

/// `mail daemon status` : ce que le démon dit de lui-même.
///
/// # Errors
///
/// Si le démon est injoignable.
pub fn status(daemon: &str) -> Result<()> {
    let mut client = connect(daemon)?;
    let hello: dto::Hello = client.typed("server.hello", json!(null))?;

    println!("Démon       {daemon}");
    println!(
        "Version     {} (protocole {})",
        hello.version, hello.protocol
    );
    println!("Messages    {}", hello.messages);
    println!(
        "Recherche   {}",
        if hello.search_available {
            "disponible"
        } else {
            "index absent — lancer `mail index`"
        }
    );
    println!("Révision    {}", hello.revision);

    let sources: Vec<dto::Source> = client.typed("jobs.sources", json!(null))?;
    if sources.is_empty() {
        println!("Import      aucun profil déclaré côté démon");
    } else {
        println!("Import      {} profil(s) importable(s) :", sources.len());
        for source in &sources {
            println!("            [{}] {}", source.id, source.path);
        }
    }

    let jobs: Vec<dto::Job> = client.typed("jobs.list", json!(null))?;
    if let Some(last) = jobs.first() {
        println!(
            "Dernière tâche  #{} {} — {}",
            last.id, last.kind, last.state
        );
    }
    Ok(())
}

/// `mail source --daemon` : les octets d'un message, servis par le démon.
///
/// ## Pourquoi celle-ci passe à distance, contrairement à `doctor`
///
/// `doctor` est refusée à distance parce qu'elle demanderait un chemin de lecture arbitraire
/// dans le stockage. Ici, ce qui est lu est désigné par un identifiant de message ou de file —
/// les mêmes que ceux que le client voit déjà — et le démon rend le contenu d'un message, qu'il
/// rend déjà par `messages.get`. Aucune capacité nouvelle.
///
/// # Errors
///
/// Si le démon est injoignable, ou si l'identifiant ne désigne rien.
pub fn source(daemon: &str, target: super::source::Target, headers_only: bool) -> Result<()> {
    let mut client = connect(daemon)?;
    let (what, method, id) = target.named();
    let found: Option<dto::MessageSource> = client.typed(method, json!({"id": id}))?;
    let Some(source) = found else {
        anyhow::bail!("{what} : introuvable côté démon, ou son contenu manque du magasin");
    };
    super::source::print(&what, &source, headers_only);
    Ok(())
}

/// `mail search --daemon` : la recherche, servie par le démon.
///
/// # Errors
///
/// Si le démon est injoignable ou la requête invalide.
pub fn search(daemon: &str, query: &str, limit: usize) -> Result<()> {
    let mut client = connect(daemon)?;
    let started = std::time::Instant::now();
    let results: dto::Results =
        client.typed("search.query", json!({"query": query, "limit": limit}))?;
    let elapsed = started.elapsed();

    if !results.search_available {
        println!("L'index plein texte est absent côté démon : aucun résultat possible.");
        println!("Lancer `mail index --daemon {daemon}`.");
        return Ok(());
    }

    println!("{} résultat(s) en {elapsed:.1?}\n", results.count);
    for row in &results.rows {
        let who = row.from_name.as_deref().unwrap_or(&row.from);
        println!(
            "{}  {}  {}",
            human::date(row.date),
            truncate(who, 28),
            row.subject
        );
    }
    if results.truncated {
        println!("\n(limite atteinte — il y a probablement d'autres résultats)");
    }
    Ok(())
}

/// `mail stats --daemon` : l'état du store, vu du démon.
///
/// # Errors
///
/// Si le démon est injoignable.
pub fn stats(daemon: &str) -> Result<()> {
    let mut client = connect(daemon)?;
    let stats: dto::Stats = client.typed("store.stats", json!(null))?;

    println!("Démon              {daemon}");
    println!("Comptes            {}", stats.accounts);
    println!("Dossiers           {}", stats.folders);
    println!("Contenus uniques   {}", stats.messages);
    println!("Références         {}", stats.refs);
    println!("Octets RFC 5322    {}", human::bytes(stats.raw_bytes));
    if stats.unthreaded > 0 {
        println!(
            "Non rattachés      {} — lancer `mail thread --daemon {daemon}`",
            stats.unthreaded
        );
    }
    Ok(())
}

/// Lance une tâche de fond sur le démon et la suit jusqu'au bout.
///
/// `follow` à faux rend la main dès que la tâche est acceptée : un import de quatre minutes
/// n'a aucune raison d'immobiliser un terminal, et `mail daemon status` dira où il en est.
///
/// # Errors
///
/// Si le démon est injoignable ou refuse la tâche.
/// Ce qu'on demande au démon de lancer.
///
/// Regroupé plutôt que passé en six paramètres : trois booléens adjacents dans un appel sont
/// une invitation à les échanger sans que le compilateur dise un mot.
#[derive(Debug)]
pub struct JobRequest<'a> {
    /// `import`, `index`, `thread` ou `sync`.
    pub kind: &'a str,
    /// Le compte à synchroniser, pour une tâche `sync`. `None` pour tous.
    pub account: Option<i64>,
    /// Le rang du profil, pour un import.
    pub source: Option<usize>,
    /// Tout lire sans rien écrire.
    pub dry_run: bool,
    /// Importer aussi les comptes de flux RSS.
    pub include_feeds: bool,
    /// Importer aussi les répertoires de comptes non déclarés.
    pub include_orphans: bool,
    /// Suivre la tâche jusqu'au bout plutôt que rendre la main.
    pub follow: bool,
}

pub fn job(daemon: &str, request: &JobRequest<'_>) -> Result<()> {
    let JobRequest {
        kind,
        account,
        source,
        dry_run,
        include_feeds,
        include_orphans,
        follow,
    } = *request;
    let mut client = connect(daemon)?;

    let mut params = json!({
        "kind": kind,
        "dry_run": dry_run,
        "include_feeds": include_feeds,
        "include_orphans": include_orphans,
    });
    if let Some(source) = source {
        params["source"] = json!(source);
    }
    if let Some(account) = account {
        params["account"] = json!(account);
    }

    let accepted: dto::Job = client.typed("jobs.start", params)?;
    let id = accepted.id;
    println!("Tâche #{id} ({}) acceptée.", accepted.kind);

    if !follow {
        println!("Suivi : `mail daemon status --daemon {daemon}`");
        return Ok(());
    }

    let started = std::time::Instant::now();
    let mut last_line = 0usize;
    loop {
        let job: Option<dto::Job> = client.typed("jobs.get", json!({ "id": id }))?;
        let Some(job) = job else {
            bail!("la tâche #{id} a disparu du registre du démon");
        };

        if matches!(job.state.as_str(), "done" | "failed" | "cancelled") {
            // Effacer la ligne de progression avant le bilan.
            eprint!("\r{}\r", " ".repeat(last_line));
            println!("Durée              {:.1?}", started.elapsed());
            println!("État               {}", job.state);
            if let Some(message) = &job.message {
                println!("Bilan              {message}");
            }
            return if job.state == "failed" {
                bail!("la tâche a échoué")
            } else {
                Ok(())
            };
        }

        // La progression va sur la sortie d'erreur : la sortie standard est celle qu'on
        // redirige dans un fichier, et une barre qui se réécrit n'y a rien à faire.
        let line = progress_line(&job);
        eprint!("\r{}\r{line}", " ".repeat(last_line));
        std::io::stderr().flush().ok();
        last_line = line.chars().count();

        std::thread::sleep(POLL);
    }
}

/// La ligne de progression d'une tâche.
fn progress_line(job: &dto::Job) -> String {
    match job.fraction {
        Some(fraction) => {
            let filled = (fraction * 30.0).round() as usize;
            format!(
                "[{}{}] {:>5.1} %  {}",
                "#".repeat(filled.min(30)),
                " ".repeat(30 - filled.min(30)),
                fraction * 100.0,
                unit(job)
            )
        }
        // Total inconnu : pas de barre à zéro qui ressemblerait à une panne.
        None => format!("... {}", unit(job)),
    }
}

/// Ce que la tâche compte, dans son unité.
fn unit(job: &dto::Job) -> String {
    if job.kind == "import" {
        human::bytes(job.done)
    } else {
        format!("{} messages", job.done)
    }
}

/// Coupe une chaîne à `width` caractères, sans couper au milieu d'un caractère.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return format!("{text:<width$}");
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn job_of(kind: &str, done: u64, total: u64) -> dto::Job {
        dto::Job {
            id: 1,
            kind: kind.to_owned(),
            state: "running".to_owned(),
            done,
            total,
            fraction: (total > 0).then(|| done as f32 / total as f32),
            message: None,
            queued_at: 0,
            finished_at: None,
        }
    }

    #[test]
    fn an_unknown_total_shows_no_bar_at_all() {
        // Une barre à zéro qui ne bouge pas ressemble à une panne.
        let line = progress_line(&job_of("index", 500, 0));
        assert!(line.starts_with("..."), "{line}");
        assert!(!line.contains('['), "{line}");
    }

    #[test]
    fn the_bar_has_a_fixed_width_whatever_the_fraction() {
        // Sans ça, la ligne effacée ne recouvre pas la précédente et laisse des résidus.
        let widths: Vec<usize> = [0, 1, 500, 999, 1000]
            .into_iter()
            .map(|done| {
                let line = progress_line(&job_of("index", done, 1000));
                let bar = line.split(']').next().unwrap();
                bar.chars().count()
            })
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "largeurs inégales : {widths:?}"
        );
    }

    #[test]
    fn a_full_bar_does_not_overflow() {
        // Un total révisé à la baisse en cours de route ne doit pas produire une barre plus
        // longue que sa boîte.
        let line = progress_line(&job_of("index", 2_000, 1_000));
        assert!(line.contains(&"#".repeat(30)), "{line}");
        assert!(!line.contains(&"#".repeat(31)), "{line}");
    }

    #[test]
    fn an_import_counts_bytes_and_the_rest_counts_messages() {
        // L'unité appartient à la tâche : afficher « 5 000 000 messages » pour un import
        // serait faux.
        assert!(progress_line(&job_of("import", 1_500_000, 3_000_000)).contains("Mio"));
        assert!(progress_line(&job_of("index", 5_000, 10_000)).contains("messages"));
    }

    #[test]
    fn truncation_never_splits_a_character() {
        // Des noms d'expéditeurs accentués, il y en a partout dans le corpus.
        let long = "Ééééééééééééééééééééééééééééééééé";
        let cut = truncate(long, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn a_short_name_is_padded_so_columns_line_up() {
        assert_eq!(truncate("ab", 5).chars().count(), 5);
    }
}

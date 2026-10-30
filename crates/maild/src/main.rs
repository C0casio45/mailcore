//! # maild
//!
//! Le démon. Il stocke, indexe et expose. Tout le reste — l'UI, un LLM, la CLI — n'est
//! qu'un client. Voir `docs/ARCHITECTURE.md`.
//!
//! ## Les deux transports, tous les deux de phase 1
//!
//! - **stdio** — le client tourne à côté du démon. Pas d'authentification : qui peut lancer
//!   le processus peut déjà lire le store. C'est le transport à garder fonctionnel en
//!   permanence, parce que c'est lui qui permet d'isoler un bug du transport d'un bug du
//!   cœur.
//! - **HTTP streamable** — le client est ailleurs sur le réseau. Jeton porteur obligatoire,
//!   TLS obligatoire hors bouclage, **refus de démarrer sinon** (critère 10).
//!
//! Un serveur MCP joignable seulement en stdio n'est utilisable que depuis la machine du
//! démon, ce qui exclut le déploiement de référence de `docs/PHASE-1.md`.
//!
//! ## `stdio --protocol mcp` est un lecteur, et rien d'autre
//!
//! C'est le processus qu'un client — Claude Code — lance de lui-même, à côté de l'application qui
//! tourne déjà. Il ouvre la boîte par `Mailbox::open_reader` et sert les cinq outils **sans monter
//! de [`Service`]** : ni migration, ni création, ni jobs, ni veilleurs `IDLE`, ni facteur, ni passe
//! de fond des vecteurs ou des PDF, ni recherche par le sens. L'écrivain d'un store est
//! l'application — le démon embarqué de la coquille — ou un démon complet : `maild http`, ou
//! `maild stdio --protocol api`.
//!
//! Jusqu'au 2026-10-30, ce processus passait par `Service::open` comme les autres : lancé sur le
//! corpus de mesure, il l'a migré de façon irréversible et y a lancé la moisson et les passes de
//! fond. Sur le store du quotidien, il aurait été un second écrivain complet à côté de la coquille
//! — deux moissons, deux connexions `IDLE` par compte, deux passes de fond.
//!
//! La règle est tenue **au point d'ouverture**, pas par ce qu'on choisit d'appeler ensuite : la
//! connexion SQLite d'un lecteur est ouverte en lecture par le système, le store refuse d'écrire
//! un blob ou un verrou, et l'index plein texte n'est jamais créé — voir `Store::open_reader`.

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use mailcore::Mailbox;
use mailmcp::MailServer;

use maild::Service;
use maild::config::{Config, Listen};
use maild::{api, config};

/// Le démon mailcore.
#[derive(Debug, Parser)]
#[command(name = "maild", version, about)]
struct Cli {
    /// Racine du store. Par défaut, le répertoire de données de l'utilisateur.
    #[arg(long, global = true, env = "MAILCORE_STORE")]
    store: Option<Utf8PathBuf>,

    /// Un profil qu'un client aura le droit de faire importer. Répétable.
    ///
    /// **Sans ce drapeau, aucun import ne peut être déclenché à distance.** C'est
    /// volontaire : un client désigne un profil par son rang dans cette liste et ne nomme
    /// jamais de chemin. Laisser un client donner un chemin reviendrait à offrir, à
    /// quiconque détient le jeton, la lecture de n'importe quel fichier de cette machine —
    /// rangé dans le store, puis relisible par `search.query`.
    ///
    /// L'opérateur, lui, sait ce qui est légitime. C'est à lui de l'écrire ici.
    #[arg(long = "profile", global = true, env = "MAILCORE_TB_PROFILE")]
    profiles: Vec<Utf8PathBuf>,

    /// Répertoire du front à servir sur le même port que l'API.
    ///
    /// Sans ce drapeau, le démon ne sert que `/api` et `/mcp`. Avec, le front est joignable
    /// à la racine — y compris depuis un simple onglet de navigateur, ce que
    /// `docs/ARCHITECTURE.md` demande explicitement.
    #[arg(long, global = true, env = "MAILCORE_UI")]
    ui_dir: Option<Utf8PathBuf>,

    #[command(subcommand)]
    command: Command,
}

/// Les deux surfaces du démon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Protocol {
    /// Les outils MCP, pour un modèle de langage.
    Mcp,
    /// L'API JSON-RPC, pour l'UI et la CLI.
    Api,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Sert un protocole sur l'entrée et la sortie standard.
    ///
    /// Le transport d'un client local — Claude Code lancé sur cette machine, ou l'UI qui
    /// démarre le démon en processus fils. Rien n'est journalisé sur la sortie standard :
    /// elle porte le protocole.
    ///
    /// **Un seul protocole à la fois**, contrairement à HTTP qui sert les deux : il n'y a
    /// qu'un flux d'entrée, et rien dans un message ne dirait auquel il appartient.
    ///
    /// **`mcp` ne fait que lire** : le store est ouvert sans création ni migration — un schéma
    /// ancien est refusé —, et rien d'autre n'est monté : ni moisson, ni facteur, ni passe de
    /// fond. Un client le lance de lui-même, à côté de l'application qui écrit déjà ; monter le
    /// démon complet, comme jusqu'au 2026-10-30, migrait le store et doublait chaque tâche de fond.
    /// `api` monte le démon complet, qui écrit.
    Stdio {
        /// Le protocole à servir : `mcp` pour un modèle, `api` pour l'UI ou la CLI.
        #[arg(long, default_value = "mcp")]
        protocol: Protocol,
    },

    /// Sert MCP **et** l'API JSON-RPC en HTTP, sur le même port.
    ///
    /// Exige un jeton dans `MAILCORE_TOKEN`, et TLS hors bouclage. Refuse de démarrer
    /// autrement — c'est un fail closed, pas un avertissement.
    Http {
        /// Adresse d'écoute.
        #[arg(long, default_value = "127.0.0.1:7847")]
        listen: std::net::SocketAddr,
        /// Certificat TLS au format PEM. Obligatoire hors bouclage.
        #[arg(long)]
        tls_cert: Option<Utf8PathBuf>,
        /// Clé privée TLS au format PEM. Obligatoire hors bouclage.
        #[arg(long)]
        tls_key: Option<Utf8PathBuf>,
        /// Servir en clair sur une interface non locale, sous la responsabilité de
        /// l'opérateur.
        ///
        /// Pour le cas du conteneur : `0.0.0.0` y est la seule adresse atteignable, et ce
        /// qui confine est la publication de port de l'hôte, que le démon ne voit pas.
        /// N'exempte jamais du jeton. Le démon le rappelle à chaque démarrage.
        #[arg(long)]
        insecure_no_tls: bool,
    },

    /// Génère un jeton porteur et l'affiche. N'écrit rien sur le disque.
    Token,

    /// Vérifie la configuration sans rien servir.
    ///
    /// Utile pour valider un déploiement avant de l'installer en service : la même
    /// validation que celle du démarrage, sans ouvrir de socket.
    Check {
        /// Adresse d'écoute à valider.
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Certificat TLS au format PEM.
        #[arg(long)]
        tls_cert: Option<Utf8PathBuf>,
        /// Clé privée TLS au format PEM.
        #[arg(long)]
        tls_key: Option<Utf8PathBuf>,
        /// Voir `http --insecure-no-tls`.
        #[arg(long)]
        insecure_no_tls: bool,
    },
}

fn main() -> Result<()> {
    // **En première ligne**, avant l'analyse des arguments : lancé en passe de fond des
    // vecteurs, ce même exécutable fait la passe et termine ici. Voir `maild::vectors`.
    let launch = maild::vectors::Launch::dispatch();
    let cli = Cli::parse();

    // Le transport stdio **est** la sortie standard : y écrire un journal corromprait le
    // protocole. Les traces partent donc toujours sur la sortie d'erreur.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        // `html5ever=error` : l'analyseur avertit à chaque construction d'arbre inhabituelle
        // — « foster parenting not implemented » sur un tableau mal formé, par exemple. Sur
        // du courrier réel c'est le cas **normal**, pas l'exception : mesuré, ça produit
        // plusieurs avertissements par message ouvert. Un journal qui déborde à chaque
        // lecture est un journal que personne ne lit, donc plus de journal du tout.
        //
        // `tantivy=warn` : tantivy écrit en `info` le chemin de son index à chaque validation,
        // donc à chaque moisson — `docs/PRIVACY.md` §8. Ses avertissements restent.
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("info,html5ever=error,tantivy=warn")
        });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    // Une panique n'y laisse que son emplacement : voir `maild::panics`.
    maild::panics::install();

    match cli.command {
        Command::Token => {
            // Sur la sortie standard, seul, pour être capturable par un script.
            println!("{}", config::generate_token()?);
            Ok(())
        }
        Command::Check {
            listen,
            tls_cert,
            tls_key,
            insecure_no_tls,
        } => check(Requested {
            store: cli.store,
            profiles: cli.profiles,
            ui_dir: cli.ui_dir,
            listen: listen.map_or(Listen::Local, Listen::Tcp),
            tls_cert,
            tls_key,
            allow_plaintext: insecure_no_tls,
        }),
        Command::Stdio { protocol } => {
            let requested = Requested {
                store: cli.store,
                profiles: cli.profiles,
                ui_dir: cli.ui_dir,
                listen: Listen::Local,
                tls_cert: None,
                tls_key: None,
                allow_plaintext: false,
            };
            match protocol {
                // Le lecteur : pas de `Service`, donc pas de passe de fond à lancer, et `launch`
                // n'a pas d'usage ici.
                Protocol::Mcp => serve_mcp_reader(requested),
                Protocol::Api => serve(requested, launch),
            }
        }
        Command::Http {
            listen,
            tls_cert,
            tls_key,
            insecure_no_tls,
        } => serve(
            Requested {
                store: cli.store,
                profiles: cli.profiles,
                ui_dir: cli.ui_dir,
                listen: Listen::Tcp(listen),
                tls_cert,
                tls_key,
                allow_plaintext: insecure_no_tls,
            },
            launch,
        ),
    }
}

/// Ce que la ligne de commande demande, avant validation.
///
/// Regroupé plutôt que passé en huit paramètres : au-delà de trois ou quatre, l'ordre des
/// arguments devient l'endroit où les bugs se cachent — deux `Option<Utf8PathBuf>` adjacentes
/// s'échangent sans que le compilateur dise un mot.
#[derive(Debug)]
struct Requested {
    store: Option<Utf8PathBuf>,
    profiles: Vec<Utf8PathBuf>,
    ui_dir: Option<Utf8PathBuf>,
    listen: Listen,
    tls_cert: Option<Utf8PathBuf>,
    tls_key: Option<Utf8PathBuf>,
    allow_plaintext: bool,
}

/// Construit et valide la configuration, sans rien ouvrir.
fn resolve(requested: Requested) -> Result<Config> {
    let store = match requested.store {
        Some(root) => root,
        None => mailcore::store::default_root().context("répertoire de données par défaut")?,
    };
    let config = Config {
        store,
        listen: requested.listen,
        token: Config::token_from_env(),
        tls_cert: requested.tls_cert,
        tls_key: requested.tls_key,
        allow_plaintext: requested.allow_plaintext,
        profiles: requested.profiles,
        ui_dir: requested.ui_dir,
    };
    config.validate()?;
    Ok(config)
}

/// `maild check` : valide une configuration et dit ce qu'elle implique.
fn check(requested: Requested) -> Result<()> {
    let config = resolve(requested)?;

    println!("Store       {}", config.store);
    match &config.listen {
        Listen::Local => println!("Écoute      socket locale (stdio)"),
        Listen::Tcp(address) => println!("Écoute      {address}"),
    }
    println!(
        "Jeton       {}",
        if config.token.is_some() {
            "présent"
        } else {
            "absent — sans objet pour la socket locale"
        }
    );
    println!(
        "TLS         {}",
        if config.tls_cert.is_some() {
            "configuré"
        } else {
            "aucun"
        }
    );
    if config.profiles.is_empty() {
        println!("Import      aucun profil déclaré — jobs.start refusera les imports");
    } else {
        println!(
            "Import      {} profil(s) déclaré(s) :",
            config.profiles.len()
        );
        for (rang, profil) in config.profiles.iter().enumerate() {
            println!("            [{rang}] {profil}");
        }
    }
    if config.serves_plaintext_off_loopback() {
        println!(
            "\nEN CLAIR    trafic non chiffré sur une interface non locale, autorisé par\n\
             \x20           --insecure-no-tls. À ne garder que si une barrière extérieure au\n\
             \x20           démon confine ce port."
        );
    }
    println!("\nConfiguration acceptée.");
    Ok(())
}

/// `maild stdio --protocol mcp` : ouvre la boîte **en lecture** et sert les outils MCP, sans rien
/// monter d'autre — voir l'en-tête du module.
fn serve_mcp_reader(requested: Requested) -> Result<()> {
    let config = resolve(requested)?;
    if !config.profiles.is_empty() {
        tracing::warn!("--profile sans effet : le serveur MCP en stdio ne lance aucune tâche");
    }

    let mailbox = Mailbox::open_reader(&config.store)
        .with_context(|| format!("ouverture en lecture du store {}", config.store))?;
    tracing::info!(
        "MCP sur stdio, en lecture seule : ni migration, ni moisson, ni passe de fond — \
         l'application, ou maild http, écrit le store"
    );

    let runtime = tokio::runtime::Runtime::new().context("runtime tokio")?;
    runtime.block_on(api::stdio::serve(MailServer::new(mailbox)))
}

/// Monte le démon complet — celui qui écrit — et sert l'API en stdio, ou MCP et l'API en HTTP.
fn serve(requested: Requested, launch: maild::vectors::Launch) -> Result<()> {
    let config = resolve(requested)?;

    // Rappelé à chaque démarrage, pas seulement au premier : un drapeau posé une fois dans
    // un fichier de déploiement se fait oublier, et l'oubli porte ici sur une boîte mail.
    if config.serves_plaintext_off_loopback() {
        tracing::warn!(
            "trafic en clair sur une interface non locale (--insecure-no-tls) : le \
             confinement dépend entièrement de ce qui entoure le démon"
        );
    }

    // La boîte s'ouvre avant le runtime : si le store est illisible, autant le dire tout de
    // suite plutôt qu'après avoir ouvert une socket.
    //
    // Le montage vient de la bibliothèque, pas d'ici : c'est le même que celui dont se sert la
    // coquille Tauri en mode embarqué. Deux montages parallèles finiraient par diverger, et
    // celui de l'UI serait le moins relu des deux.
    let service = Service::open(&config.store, config.profiles.clone(), launch)?;

    if config.profiles.is_empty() {
        tracing::info!(
            "aucun profil importable déclaré : jobs.start refusera les imports (voir --profile)"
        );
    } else {
        tracing::info!(
            profils = config.profiles.len(),
            "profils importables déclarés"
        );
    }

    let client_api = service.api.clone();

    let runtime = tokio::runtime::Runtime::new().context("runtime tokio")?;

    let served = match config.listen {
        // En stdio, le démon complet ne sert que l'API : MCP y est servi par le lecteur,
        // `serve_mcp_reader`, et cette combinaison-là n'a plus de chemin.
        Listen::Local => runtime.block_on(api::stdio::serve_api(client_api)),
        Listen::Tcp(address) => {
            let server = MailServer::from_shared(std::sync::Arc::clone(&service.mailbox));
            runtime.block_on(api::http::serve(server, client_api, address, &config))
        }
    };

    // **Les veilleurs sont arrêtés avant de rendre la main**, et pas laissés à la fin du
    // processus. Un fil tué au milieu d'un `IDLE` laisse la connexion se faire réinitialiser :
    // le serveur d'en face le compte comme une déconnexion sale, et certains fournisseurs
    // limitent un compte qui en accumule. Attendre les tranches coûte au pire quelques
    // secondes, une fois, à l'arrêt.
    service.watchers.stop();
    served
}

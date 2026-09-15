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

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
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
    let cli = Cli::parse();

    // Le transport stdio **est** la sortie standard : y écrire un journal corromprait le
    // protocole. Les traces partent donc toujours sur la sortie d'erreur.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        // `html5ever=error` : l'analyseur avertit à chaque construction d'arbre inhabituelle
        // — « foster parenting not implemented » sur un tableau mal formé, par exemple. Sur
        // du courrier réel c'est le cas **normal**, pas l'exception : mesuré, ça produit
        // plusieurs avertissements par message ouvert. Un journal qui déborde à chaque
        // lecture est un journal que personne ne lit, donc plus de journal du tout.
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,html5ever=error"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

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
        Command::Stdio { protocol } => serve(
            Requested {
                store: cli.store,
                profiles: cli.profiles,
                ui_dir: cli.ui_dir,
                listen: Listen::Local,
                tls_cert: None,
                tls_key: None,
                allow_plaintext: false,
            },
            protocol,
        ),
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
            // Sans objet en HTTP : les deux protocoles sont servis, sur deux chemins.
            Protocol::Mcp,
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

/// Ouvre la boîte et sert le protocole.
fn serve(requested: Requested, protocol: Protocol) -> Result<()> {
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
    let service = Service::open(&config.store, config.profiles.clone())?;

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

    let server = MailServer::from_shared(std::sync::Arc::clone(&service.mailbox));
    let client_api = service.api.clone();

    let runtime = tokio::runtime::Runtime::new().context("runtime tokio")?;

    let served = match config.listen {
        Listen::Local => match protocol {
            Protocol::Mcp => runtime.block_on(api::stdio::serve(server)),
            Protocol::Api => runtime.block_on(api::stdio::serve_api(client_api)),
        },
        Listen::Tcp(address) => {
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

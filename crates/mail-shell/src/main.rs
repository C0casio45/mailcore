//! La coquille native de mailcore : **une fenêtre, aucun webview au démarrage**.
//!
//! ## Pourquoi elle existe, en un chiffre
//!
//! Le critère 1 de `docs/PHASE-1.md` demande moins de 400 ms du lancement à une interface
//! utilisable. Mesuré le 2026-09-02 sur la machine de référence :
//!
//! | Coquille | Démarrage à froid |
//! |---|---|
//! | Tauri + Solid | 1 050–1 570 ms |
//! | egui, OpenGL | 465–481 ms |
//!
//! Une sonde `wry` nue a montré que **la création du webview coûte à elle seule ~935 ms**, et
//! que Tauri n'y ajoute rien de mesurable : le budget entier est dépensé avant que la première
//! ligne de code de l'interface s'exécute. Aucun réglage ne rattrape ça, d'où cette coquille.
//!
//! ## Ce qu'elle n'est pas
//!
//! **Elle ne remplace rien du cœur.** Elle est un client de plus, exactement ce que
//! l'architecture démon + coquilles prévoit (`docs/ARCHITECTURE.md`) : le store, l'index,
//! l'assainissement HTML et le contrat `mailapi` ne bougent pas, et la coquille Tauri reste le
//! client de référence de l'API — c'est elle qui garde un vrai moteur de rendu sous la main
//! pour le mail HTML qui en a besoin.
//!
//! ## Le mode embarqué, et ce qu'il implique
//!
//! Comme la coquille Tauri, celle-ci ouvre le store elle-même et **appelle le service en
//! fonction** : rien n'écoute, il n'y a pas de port et pas de jeton parce qu'il n'y a rien à
//! authentifier. Le service monté est `maild::Service`, le même que le démon.
//!
//! ## Les jalons de mesure
//!
//! Les mêmes clés que les sondes et que la coquille Tauri, pour que
//! `cargo xtask measure-ui --shell shell` les lise sans traduction : `etape=store`,
//! `etape=contexte`, `etape=premiere_image`. Ils sortent par `tracing`, pas par `println!` :
//! ici on est dans une application, pas dans une sonde.

// **Pas de console derrière la fenêtre, dans ce qu'on livre.**
//
// Sans ça, lancer la coquille depuis le menu Démarrer ouvre une fenêtre de terminal noire à
// côté d'elle : Windows en alloue une à tout exécutable du sous-système console. Pour une
// application de bureau, c'est un défaut de livraison, pas un détail.
//
// **Seulement en release**, et c'est délibéré : en debug, la console est le moyen le plus court
// de voir les traces `tracing` pendant qu'on développe. Le prix, en release, est que les modes
// en ligne de commande de ce même binaire — `--purge-cache`, `--help` — n'affichent plus rien
// quand on les lance depuis un terminal. Ils restent utilisables depuis un script, qui redirige
// la sortie : le sous-système ne change que l'allocation d'une console, pas les descripteurs
// hérités. C'est aussi pour ça que `xtask measure-ui` continue de lire les relevés — il les
// prend sur un tuyau.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod app;
mod cache;
mod settings;
mod signature;
mod theme;
mod worker;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap::Parser;

/// Les arguments de la coquille.
///
/// Deux, et les deux ont un défaut utilisable : sans rien, la coquille ouvre le store de
/// l'utilisateur en mode embarqué. C'est le mode d'une application de bureau, et c'est celui
/// qui n'expose rien.
#[derive(Debug, Parser)]
#[command(name = "mail-shell", about = "Client mail natif de mailcore.")]
struct Cli {
    /// Racine du store. Par défaut, le répertoire de données de l'utilisateur.
    ///
    /// Ignoré en mode distant : c'est le démon qui détient le store.
    #[arg(long, env = "MAILCORE_STORE")]
    store: Option<Utf8PathBuf>,

    /// Adresse d'un démon — `hôte` ou `hôte:port`. Sans ce drapeau, le store est ouvert
    /// directement, dans ce processus.
    ///
    /// Le jeton n'est **jamais** passé en argument : il vit dans le trousseau du système,
    /// déposé par `mail daemon login` (`docs/PRIVACY.md`, §7).
    ///
    /// Le client parle en clair et refuse d'envoyer un jeton hors de la machine. Pour un démon
    /// distant, monter un tunnel chiffré et viser `127.0.0.1` — c'est le déploiement que
    /// `docs/ARCHITECTURE.md` recommande de toute façon.
    #[arg(long, env = "MAILCORE_DAEMON")]
    daemon: Option<String>,

    /// Efface le cache de lecture de ce démon, puis sort sans ouvrir de fenêtre.
    ///
    /// Le même effet que le bouton « Purger le cache de lecture » de l'interface, en ligne de
    /// commande — pour un script, pour l'outillage de mesure, et pour qui veut effacer sans
    /// avoir à ouvrir l'application (`docs/PRIVACY.md`, §8).
    #[arg(long, requires = "daemon")]
    purge_cache: bool,
}

/// La variable qui demande une mesure au lieu d'un démarrage ordinaire.
const BENCH_ENV: &str = "MAILCORE_UI_BENCH";

/// Le préfixe des lignes de relevé. Sans accent : c'est une clé lue par un programme.
const MARK: &str = "MESURE-UI";

/// Ce que l'outillage attend de cette exécution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bench {
    /// Relever la première image, puis se refermer.
    Startup,
    /// Charger le plus gros dossier, défiler, relever le travail par image, se refermer.
    Scroll,
    /// Charger, attendre que le service tombe, puis vérifier que la liste défile encore et que
    /// les actions échouent proprement — **critère 9**.
    Offline,
    /// Ouvrir des messages depuis la liste et relever le délai jusqu'à leur affichage —
    /// **critère 5**, la moitié que l'API ne mesure pas.
    Open,
    /// Taper dans l'éditeur de signature et relever le travail par image — **critère 5 de la
    /// phase 3**, sur l'éditeur livré et non plus sur la sonde.
    Signature,
    /// Le régime normal : rester ouverte.
    None,
}

impl Bench {
    /// Lit la demande dans l'environnement.
    fn from_env() -> Self {
        match std::env::var(BENCH_ENV).as_deref() {
            Ok("startup") => Self::Startup,
            Ok("scroll") => Self::Scroll,
            Ok("offline") => Self::Offline,
            Ok("open") => Self::Open,
            Ok("signature") => Self::Signature,
            _ => Self::None,
        }
    }
}

/// La racine du store.
///
/// La même variable que le démon et que la coquille Tauri : quelqu'un qui passe d'une coquille
/// à l'autre doit retrouver son courrier, pas le réimporter.
fn store_root(requested: Option<Utf8PathBuf>) -> Result<Utf8PathBuf> {
    match requested {
        Some(path) if !path.as_str().trim().is_empty() => Ok(path),
        _ => mailcore::store::default_root().context("répertoire de données par défaut"),
    }
}

/// Monte le dos demandé : un démon distant, ou le service dans ce processus.
fn open_backend(cli: &Cli, started: std::time::Instant) -> Result<worker::Backend> {
    if let Some(host) = &cli.daemon {
        // Le jeton vient du trousseau, jamais de la ligne de commande ni de l'environnement.
        // Son absence est une erreur exploitable : elle nomme la commande qui le dépose.
        let token = mailapi::token::load(host).with_context(|| {
            format!(
                "aucun jeton pour {host} dans le trousseau du système. \
                 `mail daemon login --daemon {host}` le dépose."
            )
        })?;
        tracing::info!(demon = %host, "mode distant");
        return Ok(worker::Backend::Remote {
            host: host.clone(),
            token,
        });
    }

    let store = store_root(cli.store.clone())?;
    let profiles = maild::config::discover_profiles();
    tracing::info!(store = %store, profils = profiles.len(), "mode embarqué");

    let service = maild::Service::open(&store, profiles)?;
    tracing::info!(
        "{MARK} coquille etape=store ms={:.1}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(worker::Backend::Embedded(service))
}

fn main() -> Result<()> {
    // Avant toute autre chose : c'est le zéro du critère 1.
    let started = std::time::Instant::now();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        // Même raison que dans le démon : `html5ever` avertit à chaque tableau mal formé, ce
        // qui est le cas normal du courrier réel.
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,html5ever=error"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // La purge se fait avant tout le reste : elle n'ouvre ni store, ni fenêtre, ni connexion.
    // Rien à monter pour effacer un fichier.
    if cli.purge_cache {
        let host = cli
            .daemon
            .as_deref()
            .context("`--purge-cache` demande `--daemon`")?;
        let path = cache::purge(host)?;
        tracing::info!(fichier = %path, "cache de lecture effacé");
        return Ok(());
    }

    let bench = Bench::from_env();
    let backend = open_backend(&cli, started)?;

    // **Le cache de lecture, avant la fenêtre.** En mode distant, c'est ce qui permet au
    // premier écran d'exister sans attendre le réseau — la note du critère 1. C'est une lecture
    // d'un fichier de cent kilo-octets, quelques millisecondes, et elle a lieu ici plutôt que
    // dans l'interface pour que la première image la trouve déjà faite.
    //
    // En mode embarqué, rien : le store est local et s'ouvre en 31 ms, un cache serait une
    // deuxième copie des mêmes octets.
    let cache_host = cli.daemon.clone();
    let seed = cache_host.as_deref().and_then(cache::load);
    if let Some(seed) = &seed {
        tracing::info!(
            "{MARK} coquille etape=cache ms={:.1} dossiers={} lignes={}",
            started.elapsed().as_secs_f64() * 1000.0,
            seed.folders.len(),
            seed.rows.len()
        );
    }

    // Le fil d'API démarre avant la fenêtre : les premières demandes sont donc déjà en vol
    // pendant que le contexte graphique se crée.
    //
    // **L'ordre compte.** Le fil est sériel, et `server.hello` calcule les statistiques du
    // store — un compte de blobs et de références sur 102 894 lignes. Le demander en premier
    // retardait `folders.list`, donc la première page, donc les premières lignes à l'écran :
    // 115 ms mesurés entre la première image et la première ligne. Les dossiers d'abord, la
    // présentation ensuite.
    let worker = worker::Worker::start(backend);
    worker.ask(worker::Request::Bootstrap { limit: app::PAGE });
    worker.ask(worker::Request::Sources);
    worker.ask(worker::Request::Hello);
    // Après le premier écran, et pour la même raison que `hello` : elles ne servent qu'au
    // moment où l'utilisateur veut écrire, et le bouton reste désactivé jusque-là. Retarder
    // les lignes à l'écran pour savoir qui peut envoyer serait le mauvais arbitrage.
    worker.ask(worker::Request::Accounts);
    worker.ask(worker::Request::Outbox);
    // Les brouillons avec la file, et pour la même raison : ce sont les deux réponses à
    // « qu'est-ce qui n'est pas encore parti ? », et le compteur du bouton doit être juste dès
    // le premier écran.
    worker.ask(worker::Request::Drafts);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([720.0, 420.0])
        .with_title("mailcore");

    // **Où poser la fenêtre, quand on la pose pour mesurer.**
    //
    // Un banc du critère 2 ou 4 ouvre la coquille une fois par exécution, et elle apparaît
    // au-dessus de ce que l'utilisateur est en train de faire. `MAILCORE_UI_POSITION` la
    // déporte, en coordonnées du bureau virtuel : sur Windows comme sur X11, un deuxième
    // écran à droite d'un premier en 1920 de large commence à `1920,0`.
    //
    // Une variable d'environnement et non une option de ligne de commande : c'est le harnais
    // de mesure qui la pose, pas un utilisateur, et l'application livrée ne doit pas gagner un
    // réglage de fenêtre qu'aucune interface n'expose.
    if let Some(position) = position_from_env() {
        viewport = viewport.with_position(position);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "mailcore",
        options,
        Box::new(move |cx| {
            tracing::info!(
                "{MARK} coquille etape=contexte ms={:.1}",
                started.elapsed().as_secs_f64() * 1000.0
            );
            theme::install(&cx.egui_ctx);
            worker.attach(&cx.egui_ctx);
            Ok(Box::new(app::Shell::new(app::Setup {
                worker,
                started,
                bench,
                cache_host,
                seed,
            })))
        }),
    )
    .map_err(|error| anyhow::anyhow!("eframe : {error}"))
}

/// La position demandée par `MAILCORE_UI_POSITION`, au format `x,y`.
///
/// Rend `None` — donc « laisse le système décider » — quand la variable est absente, vide, ou
/// illisible. **Une valeur de travers n'empêche pas la coquille de s'ouvrir** : une fenêtre mal
/// placée est un désagrément, refuser de démarrer serait une panne, et c'est le genre de
/// variable qu'on tape à la main un jour de mesure.
fn position_from_env() -> Option<[f32; 2]> {
    parse_position(&std::env::var(POSITION_ENV).ok()?)
}

/// La variable qui déporte la fenêtre, pour un banc de mesure.
const POSITION_ENV: &str = "MAILCORE_UI_POSITION";

/// Analyse un `x,y`. **Fonction pure**, pour être testable : poser une variable
/// d'environnement dans un test est un état global au processus, et `set_var` est `unsafe`
/// depuis l'édition 2024 — ce crate l'interdit.
fn parse_position(raw: &str) -> Option<[f32; 2]> {
    let (x, y) = raw.trim().split_once(',')?;
    let x: f32 = x.trim().parse().ok()?;
    let y: f32 = y.trim().parse().ok()?;
    // Un NaN ou un infini poserait la fenêtre nulle part, ce qui sur certains gestionnaires
    // veut dire « invisible » — pire que mal placée.
    (x.is_finite() && y.is_finite()).then_some([x, y])
}

#[cfg(test)]
mod position_tests {
    use super::parse_position;

    #[test]
    fn a_pair_of_coordinates_is_read() {
        assert_eq!(parse_position("1920,0"), Some([1920.0, 0.0]));
        assert_eq!(parse_position(" 1920 , 12 "), Some([1920.0, 12.0]));
    }

    #[test]
    fn a_negative_origin_is_valid() {
        // Un écran **à gauche** du principal a des coordonnées négatives sur Windows. Les
        // refuser rendrait la moitié des configurations à deux écrans inatteignable.
        assert_eq!(parse_position("-1920,0"), Some([-1920.0, 0.0]));
    }

    #[test]
    fn what_is_not_a_pair_is_ignored_rather_than_fatal() {
        // Une fenêtre mal placée est un désagrément ; refuser de démarrer serait une panne, et
        // c'est le genre de variable qu'on tape à la main un jour de mesure.
        for raw in ["1920", "", "a,b", ",", "1920,", ",0"] {
            assert_eq!(parse_position(raw), None, "pour {raw:?}");
        }
    }

    #[test]
    fn a_position_that_is_not_a_place_is_refused() {
        for raw in ["NaN,0", "inf,0", "0,-inf"] {
            assert_eq!(parse_position(raw), None, "pour {raw:?}");
        }
    }
}

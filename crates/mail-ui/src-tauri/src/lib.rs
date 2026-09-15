//! La coquille Tauri de mailcore.
//!
//! ## Le mode par défaut : embarqué, sans socket, sans jeton
//!
//! L'application ouvre le store elle-même et **appelle le service en fonction**. Rien
//! n'écoute. Il n'y a pas de port, pas de jeton, pas de configuration réseau.
//!
//! C'est la réponse à « je ne veux pas d'un serveur pour lire mon courrier », et c'est
//! **plus sûr** que le mode réseau, pas moins :
//!
//! - en mode démon, `maild` écoute sur le bouclage, et tout processus local peut frapper à
//!   cette porte — y compris un onglet de navigateur exécutant du JavaScript hostile, la
//!   menace même qui impose le jeton sur le bouclage (`docs/PHASE-1.md`, critère 10) ;
//! - ici, le seul canal est l'IPC du webview que l'application a créé. Il n'y a pas de jeton
//!   parce qu'il n'y a rien à authentifier.
//!
//! **Aucune logique n'est réimplémentée.** [`maild::Service`] monte exactement ce que monte
//! le démon, et [`maild::api::jsonrpc::Api::handle_message`] est la même fonction que celle
//! servie par HTTP. Deux surfaces parallèles finiraient par diverger, et celle de l'UI serait
//! la moins relue des deux.
//!
//! ## Ce que ça déplace comme responsabilité
//!
//! La frontière de confiance devient « notre propre page ne doit pas être compromise ».
//! Ce n'était déjà pas différent — le jeton d'un client vit dans un stockage que sa page peut
//! lire, donc il ne protégeait pas d'une injection. Ce qui protège est ailleurs, et reste en
//! place : le corps d'un message est rendu dans une `<iframe>` `sandbox` à origine opaque,
//! sous la CSP que le démon fournit, et l'application a sa propre CSP stricte
//! (`docs/PRIVACY.md`).
//!
//! Une seule commande est exposée à la page. Pas de lecture de fichier, pas de shell, pas de
//! `http` : la liste des capacités est courte parce que la surface doit l'être.
//!
//! ## Le mode distant
//!
//! Se connecter à un démon déjà en place reste possible : la page appelle alors le même
//! contrat en HTTP, et le jeton vit dans le trousseau du système via `mailapi::token` — ce qui
//! satisfait `docs/PRIVACY.md` §7, contrairement au `sessionStorage` d'un onglet de
//! navigateur.
//!
//! Ce que la coquille ne fait **pas** : lancer un démon en processus fils. Ce serait un
//! troisième montage à maintenir pour le bénéfice de personne — qui veut un démon le lance,
//! qui n'en veut pas a le mode embarqué.

#![forbid(unsafe_code)]

use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use tauri::{Manager, State};

/// Où le service embarqué vit, et ce qu'il autorise à importer.
struct Embedded {
    service: maild::Service,
    /// Les profils Thunderbird détectés sur cette machine.
    ///
    /// En mode embarqué, il n'y a pas d'opérateur distinct de l'utilisateur : celui qui lance
    /// l'application est celui dont on lit le courrier. La liste est donc découverte, pas
    /// déclarée en ligne de commande — mais elle reste une **liste**, et la page choisit un
    /// rang dedans. Elle ne nomme jamais un chemin, exactement comme un client distant.
    profiles: Vec<Utf8PathBuf>,
}

/// L'état de l'application.
///
/// `Mutex` parce que Tauri exige `Send + Sync` sur l'état partagé et que le service porte une
/// boîte mail derrière son propre verrou. Le verrou d'ici n'est pris que pour cloner la
/// poignée d'API, jamais pendant un appel : sinon deux requêtes de la page se sérialiseraient
/// à l'entrée alors que le service sait déjà les gérer.
struct AppState {
    embedded: Mutex<Option<Embedded>>,
    /// Le départ du chronomètre du critère 1.
    ///
    /// Pris à l'entrée de [`run`], donc avant la création du webview. Il manque à ce compte le
    /// chargement de l'exécutable par le système, que le processus ne peut pas voir de
    /// l'intérieur : c'est `cargo xtask measure-ui` qui l'ajoute, en chronométrant depuis le
    /// lancement. Les deux chiffres sont rapportés, et leur écart est ce que coûte le
    /// démarrage du processus lui-même.
    started: Instant,
    /// La mesure demandée par l'outillage, ou `None` en usage normal.
    bench: Option<String>,
}

/// Sert un message JSON-RPC, en fonction, sans passer par le réseau.
///
/// C'est le remplacement exact du `POST /api` : même contrat, même dispatch, même code. Le
/// front ne fait que choisir son transport (`api.ts`).
#[tauri::command]
async fn api(state: State<'_, AppState>, message: String) -> Result<Option<String>, String> {
    // La poignée est clonée sous le verrou, puis le verrou est relâché : l'appel lui-même ne
    // doit pas retenir l'état de l'application.
    let handle = {
        let guard = state
            .embedded
            .lock()
            .map_err(|_| "état de l'application empoisonné".to_owned())?;
        match guard.as_ref() {
            Some(embedded) => embedded.service.api.clone(),
            None => return Err("aucune boîte ouverte".to_owned()),
        }
    };
    Ok(handle.handle_message(&message).await)
}

/// Ce que la page a besoin de savoir au démarrage.
#[derive(Debug, serde::Serialize)]
struct Startup {
    /// `embedded` ou `remote`. La page en déduit son transport.
    mode: &'static str,
    /// Les profils importables, par rang. Jamais un chemin donné par la page.
    profiles: Vec<String>,
}

/// Dit à la page dans quel mode elle tourne.
#[tauri::command]
fn startup(state: State<'_, AppState>) -> Result<Startup, String> {
    let guard = state
        .embedded
        .lock()
        .map_err(|_| "état de l'application empoisonné".to_owned())?;
    let profiles = guard
        .as_ref()
        .map(|embedded| {
            embedded
                .profiles
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Startup {
        mode: "embedded",
        profiles,
    })
}

/// La variable qui demande une mesure au lieu d'un démarrage ordinaire.
///
/// `startup` relève le critère 1 puis referme l'application ; `scroll` charge le plus gros
/// dossier et relève le critère 2. Sans elle, aucun de ces chemins n'est atteint.
const BENCH_ENV: &str = "MAILCORE_UI_BENCH";

/// Le préfixe des lignes de relevé, pour que l'outillage les retrouve sur la sortie d'erreur.
///
/// Sans accent, volontairement : c'est une clé lue par un programme, pas une phrase.
const MARK: &str = "MESURE-UI";

/// Millisecondes écoulées depuis le début du démarrage, pour les traces d'étapes.
///
/// Le critère 1 s'est révélé se jouer **presque entièrement hors de la page** : 146 ms de
/// JavaScript pour un démarrage d'une seconde. Sans ces jalons, il n'y avait aucun moyen de
/// dire lequel de l'ouverture du store, de la création de la fenêtre ou du webview payait le
/// reste — et donc aucun moyen de savoir quoi corriger.
fn since(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// Une étape du démarrage, telle que la page la voit.
#[derive(Debug, serde::Deserialize)]
struct Marks {
    /// `paint` — l'interface répond, sans aller-retour. `settled` — après le premier appel.
    phase: String,
    /// Millisecondes depuis l'origine temporelle du document, c'est-à-dire la navigation.
    ms: f64,
    /// Lignes affichées à cet instant. Un démarrage sur un cache vide n'est pas comparable à
    /// un démarrage sur un cache plein, et le relevé doit le dire.
    rows: u32,
}

/// Consigne une étape du démarrage — critère 1.
///
/// La page ne connaît que son propre temps ; la coquille ajoute le sien, qui commence avant la
/// création du webview. La ligne porte donc les deux.
#[tauri::command]
fn ready(app: tauri::AppHandle, state: State<'_, AppState>, marks: Marks) {
    let process = state.started.elapsed().as_secs_f64() * 1000.0;
    tracing::info!(
        "{MARK} critere=1 phase={} processus_ms={process:.1} page_ms={:.1} lignes={}",
        marks.phase,
        marks.ms,
        marks.rows
    );

    // La mesure du seul démarrage n'a rien à faire de plus : refermer libère l'outillage, qui
    // enchaîne la répétition suivante sans avoir à tuer un processus vivant.
    if marks.phase == "settled" && state.bench.as_deref() == Some("startup") {
        app.exit(0);
    }
}

/// Dit à la page quelle mesure l'outillage attend d'elle.
#[tauri::command]
fn bench(state: State<'_, AppState>) -> Option<String> {
    state.bench.clone()
}

/// Rapatrie un diagnostic de la page vers le journal.
///
/// **Le webview n'a pas de console qu'on puisse lire.** Une erreur de JavaScript au chargement
/// donne une fenêtre vide et rien d'autre : pas de trace, pas de code de sortie, rien à
/// chercher. Cette commande, avec le script d'amorçage de [`DIAG_SCRIPT`], est le seul moyen
/// qu'un échec dans la page laisse quelque chose derrière lui.
#[tauri::command]
fn diag(message: String) {
    tracing::warn!("{MARK} diag {message}");
}

/// Le script qui installe le rapport d'erreur dans la page, avant tout autre script.
///
/// Posé par `initialization_script`, donc exécuté avant notre paquet : une erreur à
/// l'évaluation du module principal est encore attrapée. Il ne dépend de rien d'autre que de
/// l'IPC — s'il ne rapporte rien, c'est que l'amorçage de Tauri lui-même n'a pas eu lieu, et
/// c'est déjà une information.
///
/// Chargé **seulement** quand une mesure est demandée : en usage normal, la page ne porte
/// aucun code de diagnostic.
const DIAG_SCRIPT: &str = r#"
(() => {
  const send = (message) => {
    try {
      window.__TAURI_INTERNALS__.invoke('diag', { message: String(message).slice(0, 400) });
    } catch (cause) {
      /* Rien à faire : s'il n'y a pas d'IPC, il n'y a pas de canal. */
    }
  };
  window.addEventListener('error', (event) => {
    send('erreur JS: ' + (event.message ?? '') + ' @ ' + (event.filename ?? '') + ':' + (event.lineno ?? 0));
  });
  window.addEventListener('unhandledrejection', (event) => {
    send('promesse rejetée: ' + (event.reason && event.reason.message ? event.reason.message : event.reason));
  });
  window.addEventListener('securitypolicyviolation', (event) => {
    send('CSP a bloqué ' + event.violatedDirective + ' sur ' + event.blockedURI);
  });
  document.addEventListener('DOMContentLoaded', () => {
    send('document prêt, internals=' + typeof window.__TAURI_INTERNALS__);
  });
})();
"#;

/// Le relevé d'un banc de défilement, tel que la page l'a mesuré.
///
/// `camelCase` : les noms viennent d'un objet TypeScript, où c'est la convention. Le reste du
/// contrat passe par `mailapi::dto`, qui est en `snake_case` des deux côtés ; ici l'objet est
/// écrit à la main dans `bench.ts`, donc c'est lui qui décide.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Frames {
    rows: u32,
    frames: u32,
    distance: f64,
    /// La cadence observée au repos, juste avant de défiler. Elle situe les deltas ; elle ne
    /// dit rien de notre code — voir `bench.ts`.
    baseline: f64,
    p50: f64,
    p95: f64,
    worst: f64,
    /// Images perdues par rapport à cette cadence, et non par rapport à 16,7 ms.
    dropped: u32,
    /// Le travail par image au repos, en ms : la ligne de base de `work_p95`.
    rest_work: f64,
    /// **Le chiffre du critère 2.** Le travail que le défilement impose à une image, en ms,
    /// indépendamment de la fréquence de l'écran. C'est lui qui se compare à 16,7 ms.
    work_p50: f64,
    work_p95: f64,
    work_worst: f64,
}

/// Consigne le relevé de défilement — critère 2, puis referme.
#[tauri::command]
fn bench_report(app: tauri::AppHandle, frames: Frames) {
    tracing::info!(
        "{MARK} critere=2 lignes={} images={} distance={:.0} repos={:.2} p50={:.2} p95={:.2} \
         pire={:.2} perdues={} travail_repos={:.2} travail_p50={:.2} travail_p95={:.2} \
         travail_pire={:.2}",
        frames.rows,
        frames.frames,
        frames.distance,
        frames.baseline,
        frames.p50,
        frames.p95,
        frames.worst,
        frames.dropped,
        frames.rest_work,
        frames.work_p50,
        frames.work_p95,
        frames.work_worst
    );
    app.exit(0);
}

/// Les profils Thunderbird importables sur cette machine.
///
/// **La découverte vit dans `maild::config`**, pas ici : c'est le service qui décide ce qui est
/// importable — un client choisit un rang dans `jobs.sources`, il ne nomme jamais un chemin —
/// et la coquille native en a besoin aussi. Deux copies de la règle « `prefs.js` marque un
/// profil » étaient une copie de trop.
fn discover_profiles() -> Vec<Utf8PathBuf> {
    maild::config::discover_profiles()
}

/// La racine du store de l'application.
///
/// Le même répertoire que celui du démon, et la même variable d'environnement pour le
/// changer : quelqu'un qui passe du mode embarqué au mode démon doit retrouver son courrier,
/// pas le réimporter — et un store de mesure doit pouvoir être ouvert par les deux.
fn store_root() -> Result<Utf8PathBuf> {
    if let Ok(requested) = std::env::var("MAILCORE_STORE")
        && !requested.trim().is_empty()
    {
        return Ok(Utf8PathBuf::from(requested));
    }
    mailcore::store::default_root().context("répertoire de données par défaut")
}

/// Monte et lance l'application.
///
/// # Errors
///
/// Si le store est inutilisable ou si Tauri ne démarre pas.
pub fn run() -> Result<()> {
    // Avant toute autre chose : c'est le zéro du critère 1.
    let started = Instant::now();

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        // Même raison que dans le démon : `html5ever` avertit à chaque tableau mal formé, ce
        // qui est le cas normal du courrier réel.
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,html5ever=error"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    // Dit tout de suite ce qu'une fenêtre vide voudrait dire. Sans `custom-protocol`, Tauri
    // charge `devUrl` : si aucun serveur Vite ne tourne, le webview affiche sa propre page
    // d'erreur et l'application semble morte sans qu'une seule ligne l'explique.
    #[cfg(not(feature = "custom-protocol"))]
    tracing::warn!(
        "construit sans la caractéristique `custom-protocol` : la fenêtre chargera le serveur \
         de développement (devUrl) et non le paquet embarqué. Pour une application qui tourne \
         seule : cargo build --release -p mail-ui --features custom-protocol"
    );

    let store = store_root()?;
    let profiles = discover_profiles();
    tracing::info!(store = %store, profils = profiles.len(), "mode embarqué");

    let service = maild::Service::open(&store, profiles.clone())?;
    tracing::info!(depuis_ms = since(started), "store ouvert");

    // Nommée `requested` et non `bench` : une variable locale du même nom que la commande
    // masquerait la fonction dans l'espace des valeurs, et `generate_handler!` la désigne par
    // son chemin.
    let requested = std::env::var(BENCH_ENV).ok().filter(|it| !it.is_empty());
    let diagnose = requested.is_some();
    if let Some(asked) = &requested {
        tracing::info!("banc demandé : {asked}");
    }
    let state = AppState {
        embedded: Mutex::new(Some(Embedded { service, profiles })),
        started,
        bench: requested,
    };

    let mut builder = tauri::Builder::default();
    if diagnose {
        builder = builder.append_invoke_initialization_script(DIAG_SCRIPT);
    }

    builder
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            api,
            startup,
            ready,
            bench,
            bench_report,
            diag
        ])
        // Ce que la coquille voit du chargement de la page. Sans ça, « la fenêtre est vide »
        // et « la page n'a jamais été demandée » se ressemblent trop.
        .on_page_load(|webview, payload| {
            let depuis_ms = since(webview.state::<AppState>().started);
            tracing::info!(depuis_ms, url = %payload.url(), evenement = ?payload.event(), "page");
        })
        .setup(|app| {
            tracing::info!(
                depuis_ms = since(app.state::<AppState>().started),
                "fenêtre montée"
            );

            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title("mailcore");

                // **Une fenêtre sans premier plan voit sa cadence d'images réduite** par le
                // compositeur. Deux exécutions du banc ont mesuré 55 Hz puis 32 Hz au repos
                // sans qu'une ligne de code change. La fenêtre est donc mise devant pour la
                // durée de la mesure — ce qui ne rend pas la cadence fiable pour autant, d'où
                // le relevé du travail par image, qui n'en dépend pas.
                if app.state::<AppState>().bench.is_some() {
                    let _ = window.set_focus();
                    let _ = window.set_always_on_top(true);
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .context("démarrage de Tauri")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    /// Le contenu de l'attribut `content` du `<meta>` de CSP.
    fn meta_csp(html: &str) -> &str {
        let at = html
            .find("http-equiv=\"Content-Security-Policy\"")
            .expect("le <meta> de CSP a disparu d'index.html");
        let rest = &html[at..];
        let start = rest.find("content=\"").expect("attribut content absent") + "content=\"".len();
        let end = rest[start..].find('"').expect("attribut content non fermé");
        &rest[start..start + end]
    }

    /// Une politique, découpée en directives et en sources, quelle que soit sa mise en forme.
    fn directives(policy: &str) -> BTreeMap<String, BTreeSet<String>> {
        policy
            .split(';')
            .filter_map(|clause| {
                let mut words = clause.split_whitespace();
                let name = words.next()?;
                Some((
                    name.to_owned(),
                    words.map(ToOwned::to_owned).collect::<BTreeSet<_>>(),
                ))
            })
            .collect()
    }

    /// La même, lue depuis l'objet JSON de `tauri.conf.json`.
    fn from_config(value: &serde_json::Value) -> BTreeMap<String, BTreeSet<String>> {
        value
            .as_object()
            .expect("app.security.csp doit être un objet")
            .iter()
            .map(|(name, sources)| {
                let set = match sources {
                    serde_json::Value::String(one) => {
                        one.split_whitespace().map(ToOwned::to_owned).collect()
                    }
                    serde_json::Value::Array(many) => many
                        .iter()
                        .filter_map(|it| it.as_str())
                        .map(ToOwned::to_owned)
                        .collect(),
                    other => panic!("source de CSP inattendue : {other}"),
                };
                (name.clone(), set)
            })
            .collect()
    }

    /// **Deux politiques, deux hôtes, un seul contenu.**
    ///
    /// Le `<meta>` d'`index.html` protège le mode onglet, où le démon ne pose aucun en-tête.
    /// `tauri.conf.json` protège la coquille — et c'est la seule qui **puisse** la protéger,
    /// puisque Tauri y ajoute le nonce de son script d'amorçage à chaque démarrage. La
    /// construction pour la coquille retire donc le `<meta>` (voir `vite.config.ts`).
    ///
    /// Deux fichiers pour une même politique veut dire qu'un durcissement d'un côté peut
    /// laisser l'autre en arrière. Ce test l'interdit : la seule différence tolérée est
    /// `connect-src`, où la coquille ajoute le canal d'IPC que l'onglet n'a pas.
    #[test]
    fn les_deux_politiques_disent_la_meme_chose() {
        let html = include_str!("../../web/index.html");
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();

        let tab = directives(meta_csp(html));
        let shell = from_config(&config["app"]["security"]["csp"]);

        let named = |it: &BTreeMap<String, BTreeSet<String>>| {
            it.keys().cloned().collect::<BTreeSet<String>>()
        };
        assert_eq!(
            named(&tab),
            named(&shell),
            "les deux politiques ne couvrent pas les mêmes directives"
        );

        for (directive, sources) in &tab {
            if directive == "connect-src" {
                continue;
            }
            assert_eq!(
                sources,
                shell.get(directive).unwrap(),
                "la directive {directive} diffère entre index.html et tauri.conf.json"
            );
        }
    }

    /// Le canal d'IPC est accordé à la coquille, et refusé à l'onglet.
    ///
    /// Dans un onglet, `http://ipc.localhost` est une destination qui n'est pas le démon :
    /// l'autoriser contredirait le critère 8. Dans la coquille, l'interdire coupe l'unique
    /// canal de l'application — c'est ce qui a produit une fenêtre inerte à la première
    /// exécution réelle.
    #[test]
    fn le_canal_ipc_est_le_seul_ecart() {
        let html = include_str!("../../web/index.html");
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();

        let tab = directives(meta_csp(html));
        let shell = from_config(&config["app"]["security"]["csp"]);

        let tab_connect = tab.get("connect-src").unwrap();
        let shell_connect = shell.get("connect-src").unwrap();

        assert_eq!(
            tab_connect,
            &["'self'".to_owned()].into_iter().collect(),
            "l'onglet ne doit parler qu'au démon qui le sert"
        );
        for channel in ["ipc:", "http://ipc.localhost"] {
            assert!(
                shell_connect.contains(channel),
                "la coquille a besoin de {channel} dans connect-src"
            );
        }
        assert!(
            shell_connect.is_superset(tab_connect),
            "la coquille doit au moins autoriser ce que l'onglet autorise"
        );
    }

    /// Aucune des deux politiques ne laisse passer une origine distante.
    ///
    /// Le pendant, pour l'application elle-même, du test de `mailhtml::csp` sur le corps d'un
    /// message : `docs/PRIVACY.md` interdit toute requête sortante vers autre chose que le
    /// démon, et une politique est le seul endroit où ça se vérifie sans exécuter la page.
    #[test]
    fn aucune_origine_distante() {
        let html = include_str!("../../web/index.html");
        let config = include_str!("../tauri.conf.json");
        let shell: serde_json::Value = serde_json::from_str(config).unwrap();
        let policies = [
            meta_csp(html).to_owned(),
            shell["app"]["security"]["csp"].to_string(),
        ];

        for policy in policies {
            for forbidden in ["https://", "http://*", "*.", " *"] {
                assert!(
                    !policy.contains(forbidden),
                    "origine distante « {forbidden} » dans une politique : {policy}"
                );
            }
            // `http://ipc.localhost` est la seule adresse en clair tolérée, et c'est un canal
            // local du webview, pas une destination réseau.
            for at in policy.match_indices("http://") {
                assert!(
                    policy[at.0..].starts_with("http://ipc.localhost"),
                    "adresse en clair inattendue dans une politique : {policy}"
                );
            }
        }
    }
}

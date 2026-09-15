//! Mesure des critères 4 et 5 **à travers l'API du démon**, pas en processus.
//!
//! ## Pourquoi ce relevé existe
//!
//! `measure-search` mesure l'index depuis le même processus que le store : 643 µs de p95 sur
//! le corpus réel. C'est le plancher, pas le critère. Le critère 4 de `docs/PHASE-1.md` se
//! relève **bout en bout depuis le client**, et le budget de 50 ms a été écrit pour un démon
//! au bout du réseau. Ce module ajoute tout ce qui sépare les deux :
//!
//! - la sérialisation JSON de la requête et de la réponse ;
//! - le passage par `spawn_blocking` et le verrou de la boîte ;
//! - la pile HTTP d'`axum`, la vérification du jeton en temps constant ;
//! - un aller-retour TCP réel.
//!
//! **Le même jeu de requêtes que `measure-search`**, construit par la même fonction. C'est ce
//! qui rend la comparaison des deux relevés valide : la différence est le transport, et rien
//! d'autre.
//!
//! ## Ce que ce relevé ne dit pas
//!
//! Il tourne sur le bouclage. La latence d'un réseau local — quelques centaines de
//! microsecondes de plus par aller-retour, et un chiffrement TLS à payer — n'y est pas. Le
//! relevé du déploiement de référence est à prendre par l'utilisateur, sur ses deux machines ;
//! celui-ci isole ce que **notre code** coûte au-delà de l'index, ce qui est la seule part
//! qu'on peut corriger.
//!
//! ## Le critère 5, mesurable pour la première fois
//!
//! « Ouverture d'un message depuis la liste, < 50 ms ». Il n'était pas relevable avant que
//! l'API existe : ouvrir un message est une lecture de blob, une décompression zstd et un
//! parse MIME, tout ça côté démon, plus le corps qui revient sur le fil. C'est la requête la
//! plus lourde de l'API, et c'est celle qu'un utilisateur déclenche à chaque clic.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use camino::Utf8PathBuf;
use mailcore::Store;
use serde_json::json;

use crate::client::Client;

/// Le budget des critères 4 et 5.
const BUDGET: Duration = Duration::from_millis(50);

/// Nombre de résultats demandés par requête — une page d'écran, comme `measure-search`.
const PAGE: usize = 50;

/// Le jeton de la mesure.
///
/// En dur, et c'est sans conséquence : le démon n'écoute que sur le bouclage, il vit le temps
/// de la mesure, et le store est un répertoire de mesure. Un jeton tiré au hasard ne
/// protégerait de rien de plus et rendrait la commande non reproductible.
pub(crate) const TOKEN: &str = "jeton-de-mesure-local-non-secret-mais-assez-long";

/// Un démon lancé pour la durée de la mesure, tué à la sortie.
pub(crate) struct Daemon {
    child: std::process::Child,
    pub(crate) address: SocketAddr,
}

impl Daemon {
    /// Tue le démon maintenant, sans attendre la fin de la mesure.
    ///
    /// C'est ce que le banc du critère 9 fait au milieu de son exécution : couper le service
    /// pour de vrai. `Drop` le referait, et un deuxième `kill` sur un processus déjà mort est
    /// sans effet.
    pub(crate) fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Mesure les critères 4 et 5 à travers l'API.
pub fn measure(store_root: &Utf8PathBuf, repeats: usize) -> Result<()> {
    // Le jeu de requêtes se construit avant de lancer le démon, en lisant le store
    // directement : c'est la même fonction que `measure-search`, donc les deux relevés
    // portent sur les mêmes requêtes et leur différence est le transport.
    let queries = {
        let store =
            Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
        crate::measure::build_queries(&store)?
    };

    let daemon = spawn(store_root)?;
    let mut client = Client::connect(daemon.address, TOKEN)?;

    let hello = client.call("server.hello", json!(null))?;
    println!("Store       {store_root}");
    println!("Démon       {} sur {}", hello["version"], daemon.address);
    println!("Protocole   {}", hello["protocol"]);
    println!("Messages    {}", hello["messages"]);
    println!("Recherche   {}", hello["search_available"]);
    if hello["search_available"] != json!(true) {
        bail!("index plein texte absent : lancer `mail index` avant de mesurer");
    }
    println!("Requêtes    {} × {repeats} répétitions", queries.len());

    // Chauffe : la première requête paie l'ouverture des segments côté démon et le
    // remplissage des caches. La mesurer ferait passer un coût de démarrage pour une latence.
    for query in queries.iter().take(20) {
        drop(client.call("search.query", json!({"query": query, "limit": PAGE}))?);
    }

    let mut search = Vec::with_capacity(queries.len() * repeats);
    let mut ids = Vec::new();
    let mut empty = 0usize;

    for _ in 0..repeats {
        for query in &queries {
            let started = Instant::now();
            let result = client.call("search.query", json!({"query": query, "limit": PAGE}))?;
            search.push(started.elapsed());

            let rows = result["rows"].as_array().map_or(0, Vec::len);
            if rows == 0 {
                empty += 1;
            }
            // De quoi mesurer le critère 5 sur des messages réellement trouvés, plutôt que
            // sur des identifiants tirés au hasard qui pointeraient peut-être ailleurs.
            if ids.len() < 500
                && let Some(id) = result["rows"][0]["id"].as_i64()
            {
                ids.push(id);
            }
        }
    }

    let paging = measure_paging(&mut client, repeats)?;

    // **Chauffe obligatoire avant de comparer les deux formes de corps.** Sans elle, la
    // première mesurée paie la lecture des blobs depuis le disque et la seconde les retrouve
    // dans le cache de pages du système : le premier relevé fait sans chauffe donnait un
    // rendu HTML *plus rapide* que le texte, ce qui est impossible — le HTML fait tout ce que
    // fait le texte, plus l'assainissement. Ce n'était pas un résultat, c'était l'ordre.
    for id in &ids {
        drop(client.call("messages.get", json!({"id": id}))?);
        drop(client.call("messages.get", json!({"id": id, "body": "html"}))?);
    }

    let open_text = measure_open(&mut client, &ids, false)?;
    let open_html = measure_open(&mut client, &ids, true)?;

    report("Recherche — critère 4", &search, empty);
    report("Pagination — critère 2 (part réseau)", &paging, 0);
    report("Ouverture, corps texte — critère 5", &open_text, 0);
    report("Ouverture, corps HTML assaini — critère 5", &open_html, 0);

    println!("\n--- critères ---");
    verdict(4, "recherche p95, bout en bout", percentile(&search, 0.95));
    verdict(
        5,
        "ouverture d'un message p95, corps texte",
        percentile(&open_text, 0.95),
    );
    // Celui-ci est le vrai, pour l'UI : c'est ce que le volet de lecture demandera.
    verdict(
        5,
        "ouverture d'un message p95, corps HTML",
        percentile(&open_html, 0.95),
    );

    println!(
        "\nRelevé sur le bouclage, en clair. Le déploiement de référence ajoute la latence du\n\
         réseau local et le chiffrement TLS : ce relevé isole ce que notre code coûte au-delà\n\
         de l'index, la part réseau est à mesurer sur deux machines."
    );
    Ok(())
}

/// Mesure la pagination : ce que l'UI fait en défilant.
fn measure_paging(client: &mut Client, repeats: usize) -> Result<Vec<Duration>> {
    let folders = client.call("folders.list", json!(null))?;
    let folders = folders.as_array().context("aucun dossier")?;

    // Le plus gros dossier : c'est celui qui met la pagination par clé à l'épreuve, et sur ce
    // corpus c'est `[Gmail]/Tous les messages`.
    let biggest = folders
        .iter()
        .max_by_key(|folder| folder["total"].as_u64().unwrap_or(0))
        .context("aucun dossier")?;
    let folder = biggest["id"].as_i64().context("dossier sans identifiant")?;
    println!(
        "\nPagination  {} ({} messages)",
        biggest["path"], biggest["total"]
    );

    let mut latencies = Vec::new();
    for _ in 0..repeats {
        let mut after: Option<String> = None;
        // Vingt pages de cent : deux mille lignes, soit bien plus qu'un utilisateur ne
        // défile d'un geste, et assez pour que le coût d'une page tardive se voie s'il existe.
        for _ in 0..20 {
            let mut params = json!({"folder": folder, "limit": 100});
            if let Some(cursor) = &after {
                params["after"] = json!(cursor);
            }
            let started = Instant::now();
            let page = client.call("messages.page", params)?;
            latencies.push(started.elapsed());

            match page["next"].as_str() {
                Some(next) => after = Some(next.to_owned()),
                None => break,
            }
        }
    }
    Ok(latencies)
}

/// Mesure l'ouverture d'un message : blob, décompression, parse MIME, corps sur le fil.
///
/// `html` demande en plus l'assainissement complet et le relevé des traceurs — c'est ce que
/// le volet de lecture appellera, donc c'est ce chiffre-là qui décide du critère 5 pour
/// l'UI. Les deux sont mesurés sur **les mêmes messages**, pour que leur écart soit
/// exactement le coût du rendu.
fn measure_open(client: &mut Client, ids: &[i64], html: bool) -> Result<Vec<Duration>> {
    anyhow::ensure!(
        !ids.is_empty(),
        "aucun message trouvé par la recherche : rien à ouvrir"
    );

    let mut latencies = Vec::with_capacity(ids.len());
    let mut blocked = 0usize;
    let mut trackers = 0usize;

    for id in ids {
        let params = if html {
            json!({"id": id, "body": "html"})
        } else {
            json!({"id": id})
        };
        let started = Instant::now();
        let message = client.call("messages.get", params)?;
        latencies.push(started.elapsed());
        // Un `null` voudrait dire qu'on mesure le coût d'un message absent, c'est-à-dire
        // presque rien. Mieux vaut le savoir que publier un chiffre flatteur.
        anyhow::ensure!(!message.is_null(), "le message {id} est introuvable");

        if html {
            blocked += message["html"]["blocked_images"].as_u64().unwrap_or(0) as usize;
            trackers += message["html"]["trackers"].as_array().map_or(0, Vec::len);
        }
    }

    if html {
        // Sur le corpus réel, ces deux chiffres disent ce que le blocage évite vraiment.
        println!(
            "\nBlocage     {blocked} images distantes retirées, {trackers} signaux de traçage \
             sur {} messages",
            ids.len()
        );
    }
    Ok(latencies)
}

/// Affiche les percentiles d'une série.
fn report(label: &str, latencies: &[Duration], empty: usize) {
    println!("\n{label}");
    println!("  mesures   {}", latencies.len());
    if empty > 0 {
        println!("  sans résultat {empty}");
    }
    for (name, quantile) in [
        ("p50", 0.50),
        ("p90", 0.90),
        ("p95", 0.95),
        ("p99", 0.99),
        ("max", 1.00),
    ] {
        println!("  {name}       {:?}", percentile(latencies, quantile));
    }
}

/// Le percentile d'une série, qui n'a pas besoin d'être triée.
fn percentile(latencies: &[Duration], quantile: f64) -> Duration {
    if latencies.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() as f64 - 1.0) * quantile).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Rend le verdict d'un critère.
fn verdict(number: u8, label: &str, measured: Duration) {
    let mark = if measured < BUDGET { "OK   " } else { "ÉCHEC" };
    println!("{mark} critère {number} — {label} : {measured:?} (seuil < {BUDGET:?})");
}

/// Démarre `maild` sur un port libre du bouclage et attend qu'il accepte.
pub(crate) fn spawn(store_root: &Utf8PathBuf) -> Result<Daemon> {
    // Un port libre choisi par le système, puis relâché. Lier `:0` dans le démon lui-même ne
    // nous dirait pas quel port il a obtenu.
    let address = {
        let probe = TcpListener::bind("127.0.0.1:0").context("port libre")?;
        probe.local_addr().context("adresse du port")?
    };

    // `cargo xtask` compile en debug par défaut ; mesurer un binaire debug donnerait des
    // chiffres qui ne veulent rien dire. Le binaire release est donc construit d'abord, et
    // c'est lui qu'on lance.
    println!("Compilation de maild en release…");
    let built = std::process::Command::new(env!("CARGO"))
        .args(["build", "--release", "-p", "maild"])
        .status()
        .context("compilation de maild")?;
    anyhow::ensure!(built.success(), "la compilation de maild a échoué");

    let binary = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("racine du workspace")?
        .join("target")
        .join("release")
        .join(if cfg!(windows) { "maild.exe" } else { "maild" });

    let child = std::process::Command::new(binary.as_std_path())
        .args([
            "--store",
            store_root.as_str(),
            "http",
            "--listen",
            &address.to_string(),
        ])
        .env("MAILCORE_TOKEN", TOKEN)
        // Les traces du démon partent sur la sortie d'erreur et brouilleraient le relevé.
        .env("RUST_LOG", "warn,html5ever=error")
        .stdout(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("démarrage de {binary}"))?;

    let daemon = Daemon { child, address };

    // Attendre que la socket accepte. Le démon ouvre le store et monte l'index plein texte
    // avant d'écouter : sur un store de 4,5 Go ça prend un instant, et un délai fixe serait
    // soit trop court, soit du temps perdu.
    for _ in 0..600 {
        if TcpStream::connect(address).is_ok() {
            return Ok(daemon);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("le démon n'a pas ouvert sa socket en 30 s");
}

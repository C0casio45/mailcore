//! Mesure des critères 1 et 2 **dans la vraie coquille**, pas dans un navigateur.
//!
//! ## Ce que cette commande ajoute à ce que la page sait
//!
//! `performance.now()` compte depuis l'origine temporelle du document. Le critère 1 de
//! `docs/PHASE-1.md` parle de « démarrage à froid », ce qui commence bien avant : chargement
//! de l'exécutable par le système, ouverture du store, montage de l'index plein texte,
//! création du webview. Rien de tout ça n'est visible depuis la page.
//!
//! Cette commande chronomètre donc de l'extérieur : instant du `spawn`, puis instant où la
//! ligne de relevé arrive sur la sortie d'erreur de l'application. Ce que la page rapporte est
//! conservé à côté, parce que la différence dit **où** part le temps — un démarrage lent par
//! l'ouverture du store et un démarrage lent par notre JavaScript ne se corrigent pas au même
//! endroit.
//!
//! ## Pourquoi en release, et plusieurs fois
//!
//! `cargo xtask` compile en debug. Mesurer un webview piloté par du Rust debug donnerait un
//! chiffre sans rapport avec ce qu'un utilisateur installe. La compilation release est donc
//! faite d'abord.
//!
//! **Sauf pour un relevé qu'on publie : là il faut `--no-build`.** Compiler puis chronométrer
//! aussitôt mesure une machine occupée à écrire ses artefacts et à les faire scanner. Les
//! démarrages du 2026-09-02 en étaient gonflés d'un facteur trois, et le critère 5 de 45 %.
//! L'ordre correct est : compiler, laisser la machine retomber, puis mesurer sans recompiler.
//!
//! Plusieurs répétitions parce que la première paie le cache de pages du système sur le store
//! et sur l'exécutable. Les deux chiffres sont rapportés : la première exécution est le
//! démarrage à froid, la médiane des suivantes est le démarrage ordinaire. Le critère porte
//! sur le premier, et le second dit s'il y a un cache à chauffer ou un coût permanent.
//!
//! ## Ce que cette commande ne remplace pas
//!
//! Elle ne clique pas, ne fait pas défiler à la molette et ne voit pas un pixel : elle lit des
//! lignes de texte. Le relevé de défilement vient de `requestAnimationFrame` dans la page,
//! donc c'est le coût de **nos** images, pas la latence d'entrée du système. Le critère 8
//! étage 2 — un message piégé rendu dans un vrai webview — n'est pas ici : c'est le binaire
//! `mailprivacy`, qui monte son propre webview et n'a pas besoin d'un pilote pour ça.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use camino::Utf8PathBuf;

/// Le budget du critère 1.
const BUDGET: Duration = Duration::from_millis(400);

/// Le budget d'une image à 60 fps — critère 2.
const BUDGET_FRAME_MS: f64 = 1000.0 / 60.0;

/// Au-delà, on considère que l'application ne rendra pas la main.
///
/// Généreux : le banc de défilement charge le plus gros dossier du corpus page par page avant
/// de mesurer, ce qui prend des secondes et non des millisecondes.
const PATIENCE: Duration = Duration::from_secs(300);

/// Le préfixe des lignes de relevé posé par la coquille.
const MARK: &str = "MESURE-UI";

/// Traces conservées pour le message d'erreur quand une exécution ne rapporte rien.
const TAIL: usize = 10;

/// Le relevé d'une exécution.
#[derive(Debug)]
struct Run {
    /// Du `spawn` à la ligne `phase=paint` : le critère 1.
    interactive: Option<Duration>,
    /// Du `spawn` à la ligne `phase=settled` : ce que le premier appel ajoute.
    settled: Option<Duration>,
    /// Ce que la page a rapporté de son propre temps, pour la ligne `paint`.
    page_ms: Option<f64>,
    /// Ce que la coquille a rapporté de son temps de processus, pour la ligne `paint`.
    process_ms: Option<f64>,
    /// Lignes affichées au moment où l'interface est devenue utilisable.
    rows: Option<u32>,
    /// Fenêtre et contexte graphique prêts, en ms depuis l'entrée du `main` — sonde native.
    context_ms: Option<f64>,
    /// Le relevé de défilement, si le banc en a produit un.
    frames: Option<String>,
    /// Vrai si la coquille a relu un cache de lecture au démarrage — mode distant.
    from_cache: bool,
}

/// Une application lancée pour la mesure, tuée à la sortie quoi qu'il arrive.
struct App(Child);

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Quelle coquille est mesurée.
///
/// Les deux sont chronométrées **par le même harnais et le même protocole** : c'est la seule
/// façon que la comparaison veuille dire quelque chose. Elles écrivent leurs jalons avec la
/// même clé et les mêmes noms de champs, à un mot près.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// `mail-ui`, la coquille Tauri : webview, front Solid.
    Tauri,
    /// `mail-shell`, la coquille native : egui sur OpenGL, cliente du même service.
    Egui,
    /// `mail-spike-native`, la sonde egui jetable — le plancher, sans interface complète.
    SpikeGl,
    /// `mail-spike-software`, la sonde egui à rendu logiciel, sans contexte GPU.
    SpikeCpu,
}

impl Shell {
    /// Le nom du jalon de première image, propre à chaque coquille.
    const fn paint_marker(self) -> &'static str {
        match self {
            Self::Tauri => "phase=paint",
            // Pour la coquille native, le jalon comparable est celui des **premières
            // lignes** : la première image de la coquille Tauri portait déjà des lignes venues
            // de son cache de lecture. Depuis que le premier écran arrive en un seul
            // aller-retour, les deux jalons coïncident de toute façon.
            Self::Egui => "etape=premieres_lignes",
            Self::SpikeGl | Self::SpikeCpu => "etape=premiere_image",
        }
    }

    /// Le paquet Cargo et le nom de l'exécutable.
    const fn package(self) -> (&'static str, &'static str) {
        match self {
            Self::Tauri => ("mail-ui", "mail-ui"),
            Self::Egui => ("mail-shell", "mail-shell"),
            Self::SpikeGl => ("mail-spike-native", "mail-spike-native"),
            Self::SpikeCpu => ("mail-spike-software", "mail-spike-software"),
        }
    }
}

/// Mesure les critères 1, 2 et 9 dans une coquille.
pub fn measure(
    store_root: &Utf8PathBuf,
    shell: Shell,
    bench: &str,
    runs: usize,
    account: Option<i64>,
) -> Result<()> {
    if bench == "offline" {
        anyhow::ensure!(
            shell == Shell::Egui,
            "le banc `offline` demande la coquille native : elle est la seule à savoir viser un \
             démon distant, et il faut un service à couper"
        );
        return offline(store_root);
    }
    if bench == "startup-remote" {
        anyhow::ensure!(
            shell == Shell::Egui,
            "le banc `startup-remote` demande la coquille native : elle est la seule à savoir \
             viser un démon distant"
        );
        return remote_startup(store_root, runs);
    }
    if bench == "sync-real" {
        anyhow::ensure!(
            shell == Shell::Egui,
            "le banc `sync-real` demande la coquille native : c'est celle qu'on livre, et la \
             seule que le critère 4 concerne"
        );
        return during_real_sync(store_root, account, runs);
    }
    if bench == "sync" {
        anyhow::ensure!(
            shell == Shell::Egui,
            "le banc `sync` demande la coquille native : c'est celle qu'on livre, et la seule \
             que le critère 4 concerne"
        );
        return during_sync(runs);
    }
    if bench == "signature" {
        anyhow::ensure!(
            shell == Shell::Egui,
            "le banc `signature` demande la coquille native : c'est la seule qui a un \
             éditeur de signature"
        );
        return signature(runs);
    }
    if bench == "open" {
        anyhow::ensure!(
            shell == Shell::Egui,
            "le banc `open` demande la coquille native : c'est la seule qui dessine un corps de \
             message sans moteur de rendu"
        );
        return opening(store_root, runs);
    }
    if !matches!(bench, "startup" | "scroll") {
        bail!(
            "banc inconnu : {bench} — attendu `startup`, `startup-remote`, `scroll`, `open`, \
             `sync`, `sync-real` ou `offline`"
        );
    }
    if runs == 0 {
        bail!("il faut au moins une exécution");
    }

    let binary = match shell {
        Shell::Tauri => build()?,
        other => build_rust(other)?,
    };

    println!("Store       {store_root}");
    println!("Coquille    {shell:?}");
    println!("Binaire     {binary}");
    println!("Banc        {bench} × {runs}");
    println!();

    let mut collected = Vec::with_capacity(runs);
    for index in 0..runs {
        let run = once(&binary, store_root, shell, bench, None)?;
        println!(
            "  {:>2}. interactif {:>9}   installé {:>9}{}",
            index + 1,
            run.interactive.map_or_else(|| "—".to_owned(), millis),
            run.settled.map_or_else(|| "—".to_owned(), millis),
            run.rows
                .map_or_else(String::new, |it| format!("   {it} lignes en cache")),
        );
        collected.push(run);
    }
    println!();

    report(&collected);
    Ok(())
}

/// Mesure le **critère 4 de `docs/PHASE-2.md`** : le travail par image **pendant une
/// synchronisation**.
///
/// ## Ce que ce banc ferme
///
/// Le critère 2 de la phase 1 défilait sur une liste au repos : volet de lecture vide, rien
/// qui écrivait dans le store. C'était un trou reconnu dans `docs/PHASE-1.md`, et le voici
/// bouché — la règle 3 du `CLAUDE.md` dit *l'UI ne bloque jamais sur le réseau ou sur un
/// import*, et une synchronisation est exactement les deux à la fois.
///
/// ## La forme du banc, et pourquoi c'est deux processus
///
/// La coquille défile dans son processus ; la moisson écrit depuis celui du banc. Ce n'est pas
/// une commodité de mise en œuvre, c'est la forme réelle : en déploiement de référence, c'est
/// le démon qui écrit et la coquille qui lit. Faire les deux dans un seul processus mesurerait
/// une contention de verrou qui n'existe pas en production.
///
/// La coquille lit donc un store que **quelqu'un d'autre est en train de remplir**, avec tout
/// ce que ça implique : révisions qui changent, `Changed` reçus, listes rechargées au milieu
/// du défilement.
///
/// ## Pourquoi la moisson ne passe pas par `mail sync`
///
/// `mailsync::connect` exige TLS, et `mailfake` parle en clair — c'est son propre test qui
/// l'exige. Le banc injecte donc le flux, comme les tests d'intégration de `mailsync`, et
/// appelle le **vrai** `discover` puis le **vrai** `harvest`. Ce qui est court-circuité est la
/// couche de chiffrement, pas la moisson.
fn during_sync(runs: usize) -> Result<()> {
    if runs == 0 {
        bail!("il faut au moins une exécution");
    }
    let binary = build_rust(Shell::Egui)?;

    println!("Binaire     {binary}");
    println!("Banc        sync × {runs}");
    println!();

    let mut lines = Vec::with_capacity(runs);
    for index in 0..runs {
        let (line, loaded) = one_sync_run(&binary)?;
        match line {
            Some(line) => {
                println!("  {:>2}. {loaded} lignes en magasin   {line}", index + 1);
                lines.push(line);
            }
            None => println!("  {:>2}. aucun relevé", index + 1),
        }
    }
    println!();

    anyhow::ensure!(
        !lines.is_empty(),
        "aucune exécution n'a rendu de relevé du critère 2 sous synchronisation"
    );
    verdict_of(
        &lines,
        "contre `mailfake`",
        "`SYNC_MESSAGES`, ou abaisser `SYNC_WARMUP`",
    )
}

/// Une exécution : un store neuf, une moisson en cours, la coquille qui défile dedans.
///
/// Rend le relevé de la coquille et le nombre de messages que la moisson avait écrits au
/// moment où la coquille s'est refermée. Le second sert de **contrôle** : si la moisson n'a
/// rien écrit pendant le défilement, le banc ne mesure que le critère 2 sous un autre nom.
fn one_sync_run(binary: &Utf8PathBuf) -> Result<(Option<String>, u64)> {
    let dir = tempfile::tempdir().context("répertoire de travail")?;
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
        .map_err(|it| anyhow::anyhow!("chemin non UTF-8 : {}", it.display()))?;

    // Le serveur : un dossier assez gros pour que la moisson dure plus longtemps que le
    // défilement. 20 000 messages ≈ 45 s d'écriture de blobs, le banc en demande ~15.
    let messages: Vec<mailfake::Message> = (1..=SYNC_MESSAGES)
        .map(|uid| mailfake::Message::simple(uid, &format!("message {uid}")))
        .collect();
    let server = mailfake::Server::start(
        mailfake::Config::with_inbox()
            .with_mailboxes(vec![mailfake::Mailbox::new("INBOX", 1000, messages)]),
    )
    .context("démarrage du serveur de test")?;

    let account = {
        let store = mailcore::Store::open(&root).context("ouverture du store")?;
        let writer = store.writer()?;
        let account = writer.upsert_imap_account(
            "banc",
            &mailcore::Server {
                host: "127.0.0.1".to_owned(),
                port: server.address().port(),
                username: "marie@exemple.fr".to_owned(),
                auth: mailcore::AuthKind::Password,
                security: mailcore::Security::Tls,
            },
        )?;
        writer.commit()?;
        account
    };

    // La moisson tourne sur son fil, dans ce processus. La coquille sera un autre processus.
    let harvest_root = root.clone();
    let address = server.address();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_for_thread = std::sync::Arc::clone(&stop);
    let harvest = std::thread::spawn(move || -> Result<u64> {
        let store = mailcore::Store::open(&harvest_root)?;
        let stream = std::net::TcpStream::connect(address)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut client = mailsync::Client::greet(stream)?;
        client.login("marie@exemple.fr", "secret")?;
        let folders = mailsync::discover(&mut client, &store, account)?;
        for folder in &folders {
            if stop_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            mailsync::harvest(&mut client, &store, folder.folder, &folder.remote_name)?;
        }
        Ok(store.stats()?.messages)
    });

    // On attend que la coquille ait de quoi défiler. Sans ça, son banc trouverait un dossier
    // vide, se plaindrait et se refermerait sans rien mesurer.
    let ready = wait_for_rows(&root, SYNC_WARMUP)?;
    println!("      moisson en cours, {ready} messages en magasin — la coquille démarre");

    let run = once(binary, &root, Shell::Egui, "scroll", None)?;

    // La coquille est refermée. La moisson peut s'arrêter : ce qui restait à télécharger
    // n'apporterait rien à la mesure, et le banc ne doit pas durer trois minutes.
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let written = match harvest.join() {
        Ok(Ok(written)) => written,
        Ok(Err(source)) => {
            println!("      la moisson a échoué : {source}");
            0
        }
        Err(_) => {
            println!("      le fil de moisson a paniqué");
            0
        }
    };

    anyhow::ensure!(
        written > ready,
        "la moisson n'a rien écrit pendant le défilement ({ready} → {written}) : le banc ne \
         mesurerait que le critère 2 sous un autre nom"
    );
    Ok((run.frames, written))
}

/// Attend que le store contienne au moins `wanted` messages.
///
/// Sonde le store plutôt que d'attendre une durée fixe : la vitesse d'écriture des blobs
/// dépend du disque et de l'antivirus, et une attente fixe serait soit trop courte sur une
/// machine lente, soit du temps perdu sur une rapide.
fn wait_for_rows(root: &Utf8PathBuf, wanted: u64) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let store = mailcore::Store::open(root).context("ouverture du store")?;
        let count = store.stats()?.messages;
        drop(store);
        if count >= wanted {
            return Ok(count);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "la moisson n'a écrit que {count} messages en deux minutes"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Messages servis par le serveur de test du banc du critère 4.
///
/// 60 000, et le premier chiffre était **trois fois trop petit**. Avec 20 000, la coquille
/// n'atteignait « tout chargé » qu'à la fin de la moisson : chaque `Changed` relance sa
/// pagination depuis la première page, donc elle ne rattrape le dossier que quand les
/// écritures s'arrêtent. Le relevé sortait avec **un seul changement** reçu pendant les
/// 600 images — un recouvrement symbolique.
///
/// Corrigé en deux endroits : la coquille se contente maintenant de ce qu'elle a chargé au
/// bout de 25 s (`app::LOAD_PATIENCE`), et il y a assez de messages pour que la moisson soit
/// encore loin d'être finie à ce moment-là. Le compteur `changements` du relevé est ce qui
/// vérifie que le recouvrement a réellement eu lieu.
const SYNC_MESSAGES: u32 = 60_000;

/// Messages à avoir en magasin avant de lancer la coquille.
///
/// Assez pour que le défilement porte sur quelque chose, assez peu pour que la moisson soit
/// loin d'être finie.
const SYNC_WARMUP: u64 = 2_000;

/// Mesure la **moitié manquante du critère 5** : de la ligne cliquée au corps dessiné.
///
/// Le critère 5 était relevé côté API — 4,31 ms pour `messages.get`. Le critère ne dit pas
/// « la réponse de l'API », il dit « l'ouverture d'un message **depuis la liste** ». Ce banc
/// mesure le trajet entier, dans la coquille : demande, fil d'API, découpage du corps en
/// blocs, mise en page.
///
/// Le seuil est de **50 ms**, et il ne se compare pas à la cadence : voir la convention de
/// `Shell::advance_open`.
fn opening(store_root: &Utf8PathBuf, runs: usize) -> Result<()> {
    if runs == 0 {
        bail!("il faut au moins une exécution");
    }

    let binary = build_rust(Shell::Egui)?;

    println!("Store       {store_root}");
    println!("Binaire     {binary}");
    println!("Banc        open × {runs}");
    println!();

    let mut lines = Vec::with_capacity(runs);
    for index in 0..runs {
        let run = once(&binary, store_root, Shell::Egui, "open", None)?;
        match run.frames.as_deref() {
            Some(line) => {
                println!("  {:>2}. {line}", index + 1);
                lines.push(line.to_owned());
            }
            // Pas de relevé : le dire, et ne pas le remplacer par un silence qui ressemble à
            // un succès.
            None => println!("  {:>2}. aucun relevé", index + 1),
        }
    }
    println!();

    anyhow::ensure!(
        !lines.is_empty(),
        "aucune exécution n'a rendu de relevé du critère 5"
    );

    println!("Critère 5 — ouverture d'un message depuis la liste, budget 50 ms");
    let number = |line: &str, key: &str| -> Option<f64> { field(line, key)?.parse().ok() };

    // Le pire p95 des exécutions, pas leur moyenne : un critère se juge sur le mauvais jour.
    let worst = lines
        .iter()
        .filter_map(|it| number(it, "p95"))
        .fold(f64::NAN, f64::max);
    let empty: f64 = lines.iter().filter_map(|it| number(it, "vides")).sum();
    let blocks: f64 = lines.iter().filter_map(|it| number(it, "blocs")).sum();
    let first = lines.first().and_then(|it| number(it, "premiere"));

    let served = lines
        .iter()
        .filter_map(|it| number(it, "service_p95"))
        .fold(f64::NAN, f64::max);

    if let Some(first) = first {
        println!("  Première ouverture       {first:.2} ms (blob froid, MIME jamais lu)");
    }
    let waited = lines
        .iter()
        .filter_map(|it| number(it, "images_p50"))
        .fold(f64::NAN, f64::max);

    println!("  p95, pire exécution      {worst:.2} ms");
    println!("  Dont le service          {served:.2} ms de p95 — voir `Opened::served_ms`");
    println!("  Images d'attente         {waited:.1} en médiane — le reste du délai est la");
    println!("                           cadence de l'écran, pas notre travail");
    println!("  Blocs dessinés           {blocks:.0} en tout, {empty:.0} message(s) sans corps");
    println!(
        "  Verdict                  {}",
        if worst.is_nan() {
            "non mesuré"
        } else if worst <= 50.0 {
            "passé"
        } else {
            "ÉCHOUÉ"
        }
    );
    if blocks < 1.0 {
        println!("  Aucun bloc dessiné : ce zéro ne prouve rien, le banc est invalide.");
    }
    Ok(())
}

/// Mesure le **critère 5 de la phase 3** : l'éditeur de signature, en frappe et en collage.
///
/// ## Ce qu'il mesure de plus que la sonde
///
/// `mail-spike-richtext` a répondu à la question de faisabilité sur un modèle nu. Ce banc
/// mesure l'**éditeur livré** : son layouter, le rattrapage du document sur le tampon à chaque
/// image, et le `TextEdit` d'`egui` qui gère curseur et sélection par-dessus. Les trois
/// n'existaient pas quand la sonde a rendu ses chiffres.
///
/// ## Comment lire le relevé
///
/// Le seuil porte sur `p95` en **frappe** : un caractère inséré au milieu d'un document de 200
/// lignes, par image. Le reste est là pour que ce chiffre soit interprétable :
///
/// - `glyphes` et `styles` sont les contrôles. Un p95 obtenu sur trois lignes serait
///   indiscernable d'un vrai, et la sonde a appris qu'un compteur constant quand l'entrée varie
///   est le signe qu'on mesure la mauvaise chose ;
/// - `repos` dit ce que coûte une image quand rien ne change. Il doit être du même ordre que la
///   frappe — à quatorze microsecondes de mise en page, c'est le compositeur qui domine, pas
///   notre travail. Un repos **beaucoup** plus bas voudrait dire que la frappe fait autre chose
///   que remettre en page ;
/// - `collage` est le coût de la conversion de 50 Ko de HTML, et `apres_p95` celui des images
///   qui suivent : c'est la moitié « ne doit pas figer » du critère.
fn signature(runs: usize) -> Result<()> {
    if runs == 0 {
        bail!("il faut au moins une exécution");
    }

    let binary = build_rust(Shell::Egui)?;
    // Un store jetable : ce banc n'ouvre aucun message et n'a besoin d'aucun compte. Viser le
    // store réel ne lui apporterait rien et le ferait dépendre de ce qu'il contient — la leçon
    // de `measure-attachment`, qui a laissé 533 Mo dans les données de production.
    let temporary = tempfile::tempdir().context("store jetable")?;
    let store_root = camino::Utf8Path::from_path(temporary.path())
        .context("chemin non UTF-8")?
        .to_owned();

    println!("Store       {store_root} (jetable)");
    println!("Binaire     {binary}");
    println!("Banc        signature × {runs}");
    println!();

    let mut lines = Vec::with_capacity(runs);
    for index in 0..runs {
        let run = once(&binary, &store_root, Shell::Egui, "signature", None)?;
        match run.frames.as_deref() {
            Some(line) => {
                println!("  {:>2}. {line}", index + 1);
                lines.push(line.to_owned());
            }
            None => println!("  {:>2}. aucun relevé", index + 1),
        }
    }
    println!();

    anyhow::ensure!(
        !lines.is_empty(),
        "aucune exécution n'a rendu de relevé du critère 5"
    );

    let number = |line: &str, key: &str| -> Option<f64> { field(line, key)?.parse().ok() };
    let worst = |key: &str| -> f64 {
        lines
            .iter()
            .filter_map(|it| number(it, key))
            .fold(f64::NAN, f64::max)
    };

    // Le pire p95 des exécutions, pas leur moyenne : un critère se juge sur le mauvais jour.
    let typing = worst("p95");
    let glyphs = lines.first().and_then(|it| number(it, "glyphes"));
    let styles = lines.first().and_then(|it| number(it, "styles"));

    println!("Critère 5 — l'éditeur de signature, budget 16,7 ms par image");
    println!("  p95 en frappe, pire exécution  {typing:.2} ms");
    println!("  Pire image                     {:.2} ms", worst("pire"));
    println!(
        "  Au repos                       {:.2} ms — du même ordre que la frappe, et c'est",
        worst("repos")
    );
    println!("                                 attendu : la mise en page ne domine pas l'image");
    println!(
        "  Collage de {:.0} octets      {:.2} ms de conversion",
        worst("collage_octets"),
        worst("collage")
    );
    println!(
        "  Images après le collage        {:.2} ms en p95, {:.2} ms au pire",
        worst("apres_p95"),
        worst("apres_pire")
    );
    match (glyphs, styles) {
        (Some(glyphs), Some(styles)) => println!(
            "  Contrôle                       {glyphs:.0} glyphes, {styles:.0} intervalles \
             stylés"
        ),
        _ => println!("  Contrôle                       absent : le relevé ne prouve rien"),
    }
    println!(
        "  Verdict                        {}",
        if typing.is_nan() {
            "non mesuré"
        } else if typing <= 16.7 {
            "passé"
        } else {
            "ÉCHOUÉ"
        }
    );
    // Un document vide donnerait un p95 parfait sans rien mesurer. Le dire, plutôt que de
    // laisser un chiffre flatteur passer pour un relevé.
    if glyphs.is_none_or(|it| it < 1000.0) {
        println!("  Moins de mille glyphes mis en page : ce relevé est invalide.");
    }
    Ok(())
}

/// Mesure le **critère 1 en mode distant** : ce que le cache de lecture achète.
///
/// ## Pourquoi ce banc est distinct de `startup`
///
/// La note du critère 1 est explicite : « avec un démon distant, il impose en plus qu'elle
/// n'attende pas le premier aller-retour réseau — donc qu'elle ouvre sur son cache de
/// lecture ». Le banc `startup` mesure le mode embarqué, où il n'y a pas de réseau à attendre
/// et pas de cache à tenir. Celui-ci mesure l'autre moitié.
///
/// Il fait donc **deux mesures qui se comparent** :
///
/// - la **première** exécution part d'un cache purgé : le premier écran attend le réseau ;
/// - les **suivantes** trouvent le cache écrit par la précédente : le premier écran est dessiné
///   avant le premier aller-retour.
///
/// L'écart entre les deux est ce que le cache achète, sur le bouclage — c'est-à-dire le plancher.
/// Sur un vrai réseau, il ne peut que grandir : c'est un aller-retour qui disparaît.
fn remote_startup(store_root: &Utf8PathBuf, runs: usize) -> Result<()> {
    if runs == 0 {
        bail!("il faut au moins une exécution");
    }

    let binary = build_rust(Shell::Egui)?;
    let daemon = crate::api::spawn(store_root)?;
    let host = daemon.address.to_string();
    let _token = TokenGuard::deposit(&host, &binary)?;

    println!("Store       {store_root}");
    println!("Démon       {host}");
    println!("Binaire     {binary}");
    println!("Banc        startup-remote × {runs}");
    println!();

    // La purge se fait par la coquille elle-même : elle seule connaît le chemin de son cache,
    // et le dupliquer ici serait une deuxième vérité à maintenir.
    let purged = Command::new(binary.as_std_path())
        .args(["--daemon", &host, "--purge-cache"])
        .env("RUST_LOG", "warn")
        .status()
        .context("purge du cache de lecture")?;
    anyhow::ensure!(purged.success(), "la purge du cache a échoué");

    let mut collected = Vec::with_capacity(runs);
    for index in 0..runs {
        let run = once(&binary, store_root, Shell::Egui, "startup", Some(&host))?;
        println!(
            "  {:>2}. interactif {:>9}   {}{}",
            index + 1,
            run.interactive.map_or_else(|| "—".to_owned(), millis),
            if run.from_cache {
                "cache relu   "
            } else {
                "sans cache   "
            },
            run.rows
                .map_or_else(String::new, |it| format!("{it} lignes")),
        );
        collected.push(run);
    }
    println!();

    println!("Critère 1 en mode distant — budget 400 ms");
    let cold = collected.iter().find(|it| !it.from_cache);
    let warm: Vec<Duration> = collected
        .iter()
        .filter(|it| it.from_cache)
        .filter_map(|it| it.interactive)
        .collect();

    if let Some(cold) = cold.and_then(|it| it.interactive) {
        println!("  Sans cache               {}", millis(cold));
    }
    let mut sorted = warm.clone();
    sorted.sort_unstable();
    if let Some(median) = sorted.get(sorted.len() / 2) {
        println!("  Avec cache, médiane      {}", millis(*median));
    }
    if let (Some(cold), Some(median)) = (
        cold.and_then(|it| it.interactive),
        sorted.get(sorted.len() / 2),
    ) {
        let gain = cold.saturating_sub(*median);
        println!("  Ce que le cache achète   {}", millis(gain));
    }
    println!(
        "  Verdict                  {}",
        match sorted.get(sorted.len() / 2) {
            Some(median) if *median <= BUDGET => "passé",
            Some(_) => "ÉCHOUÉ",
            None => "non mesuré — aucun démarrage n'a relu de cache",
        }
    );
    println!("  Mesuré sur le bouclage : l'aller-retour réseau y est négligeable, donc l'écart");
    println!("  ci-dessus est le **plancher** de ce que le cache apporte. Sur un vrai réseau,");
    println!("  c'est un aller-retour complet qui disparaît du chemin du premier pixel.");
    Ok(())
}

/// Mesure le critère 9 : **la liste reste consultable quand le service tombe**.
///
/// ## Pourquoi ce banc est fait comme ça
///
/// Le critère se lit : « la liste déjà chargée reste défilable, la recherche et l'ouverture
/// d'un message échouent proprement avec un état visible, rien ne gèle et rien ne ment ». Il
/// exige donc **un vrai service à couper**, ce que le mode embarqué n'a pas : quand le service
/// est dans le processus, il tombe avec lui.
///
/// D'où le montage : un vrai `maild` sur le bouclage, la coquille en mode distant, et le démon
/// **tué de l'extérieur** au milieu de l'exécution. Un service qu'on couperait de l'intérieur
/// ne prouverait rien — l'application saurait qu'elle a coupé.
///
/// ## Le jeton
///
/// La coquille lit le jeton dans le trousseau du système, jamais dans un argument ni dans
/// l'environnement (`docs/PRIVACY.md`, §7). Le banc en dépose donc un, puis **le retire**, y
/// compris en cas d'échec — une entrée de trousseau laissée derrière une mesure est un secret
/// oublié.
fn offline(store_root: &Utf8PathBuf) -> Result<()> {
    /// Au-delà, la coquille n'a pas rapporté son relevé et le banc renonce.
    const PATIENCE: Duration = Duration::from_secs(120);

    let binary = build_rust(Shell::Egui)?;
    let mut daemon = crate::api::spawn(store_root)?;
    let host = daemon.address.to_string();

    // Le jeton est retiré du trousseau quoi qu'il arrive ensuite : le garde le fait au `Drop`.
    let _token = TokenGuard::deposit(&host, &binary)?;

    println!("Store       {store_root}");
    println!("Démon       {host}");
    println!("Binaire     {binary}");
    println!("Banc        offline");
    println!();

    let mut child = Command::new(binary.as_std_path())
        .args(["--daemon", &host])
        .env("MAILCORE_UI_BENCH", "offline")
        .env("RUST_LOG", "info,html5ever=error")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("démarrage de {binary}"))?;

    let stderr = child.stderr.take().context("sortie d'erreur absente")?;
    let app = App(child);
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                return;
            }
        }
    });

    let deadline = Instant::now() + PATIENCE;
    let mut report = None;
    let mut cut = false;
    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();

    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("la coquille n'a rien rapporté en {} s", PATIENCE.as_secs());
        }
        let Ok(line) = receiver.recv_timeout(left) else {
            break;
        };

        if line.contains("etape=charge") && !cut {
            cut = true;
            let loaded = field(&line, "lignes").unwrap_or_else(|| "?".to_owned());
            println!("  {loaded} lignes chargées — coupure du démon");
            daemon.kill();
            continue;
        }
        if line.contains("critere=9") {
            report = line.find(MARK).map(|at| line[at..].trim().to_owned());
            break;
        }
        if !line.contains(MARK) {
            tail.push_back(line);
            if tail.len() > TAIL {
                tail.pop_front();
            }
        }
    }

    drop(app);

    let Some(report) = report else {
        let traces = tail
            .iter()
            .map(|it| format!("  {it}"))
            .collect::<Vec<_>>()
            .join("\n");
        bail!("aucun relevé du critère 9.\nDernières traces :\n{traces}");
    };

    println!();
    println!("Critère 9 — la liste reste consultable, service injoignable");
    println!("  {report}");

    let offline = field(&report, "hors_ligne").as_deref() == Some("1");
    let open = field(&report, "ouverture").unwrap_or_default();
    let search = field(&report, "recherche").unwrap_or_default();
    let work: Option<f64> = field(&report, "travail_p95").and_then(|it| it.parse().ok());

    println!(
        "  État dégradé visible     {}",
        if offline { "oui" } else { "NON" }
    );
    println!("  Ouverture d'un message   {open}");
    println!("  Recherche                {search}");
    if let Some(work) = work {
        println!("  Travail par image        {work:.2} ms en défilant après la coupure");
    }

    // Le verdict tient en une conjonction, et chaque terme est un mot du critère.
    let clean = open == "echec_propre" && search == "echec_propre";
    let fluid = work.is_some_and(|it| it <= BUDGET_FRAME_MS);
    println!(
        "  Verdict                  {}",
        match (offline, clean, fluid) {
            (true, true, true) => "passé — rien n'a gelé, rien n'a menti",
            (false, _, _) => "ÉCHOUÉ — la coupure n'a pas été signalée à l'utilisateur",
            (_, false, _) => "ÉCHOUÉ — une action n'a pas échoué proprement",
            (_, _, false) => "ÉCHOUÉ — le défilement ne tient plus son image",
        }
    );
    Ok(())
}

/// Ce qu'une mesure en mode distant laisse derrière elle, et qui doit disparaître.
///
/// Deux traces, et les deux sont des secrets :
///
/// - **le jeton**, déposé dans le trousseau du système pour que la coquille puisse s'en servir ;
/// - **le cache de lecture**, écrit par la coquille sous le nom du démon de mesure. Chaque
///   exécution utilise un port éphémère, donc chaque mesure créait un fichier de plus contenant
///   les sujets et les expéditeurs du plus gros dossier du corpus. Après quelques relevés, le
///   répertoire de données en contenait une collection.
///
/// Le garde efface les deux à la sortie, **y compris si la mesure échoue**. Une trace laissée
/// derrière une mesure est une trace que personne ne pense à nettoyer.
struct TokenGuard {
    host: String,
    /// Le binaire de la coquille, qui sait purger son propre cache.
    shell: Utf8PathBuf,
}

impl TokenGuard {
    /// Dépose le jeton de mesure pour cet hôte.
    fn deposit(host: &str, shell: &Utf8PathBuf) -> Result<Self> {
        mailapi::token::store(host, crate::api::TOKEN)
            .with_context(|| format!("dépôt du jeton de mesure pour {host}"))?;
        Ok(Self {
            host: host.to_owned(),
            shell: shell.clone(),
        })
    }
}

impl Drop for TokenGuard {
    fn drop(&mut self) {
        // La purge passe par la coquille : elle seule connaît le chemin de son cache, et le
        // dupliquer ici serait une deuxième vérité à maintenir.
        let purged = Command::new(self.shell.as_std_path())
            .args(["--daemon", &self.host, "--purge-cache"])
            .env("RUST_LOG", "warn")
            .status();
        match purged {
            Ok(status) if status.success() => {}
            Ok(status) => println!("  (cache de mesure non purgé : code {status})"),
            Err(source) => println!("  (cache de mesure non purgé : {source})"),
        }

        // Un échec de retrait se dit et ne fait rien tomber : la mesure est finie, et la seule
        // conséquence est une entrée de trousseau à nettoyer par `mail daemon logout`.
        if let Err(source) = mailapi::token::forget(&self.host) {
            println!("  (jeton de mesure non retiré du trousseau : {source})");
        }
    }
}

/// Compile une coquille purement Rust en release et rend le chemin de son exécutable.
///
/// Pas de `npm` ici, et c'est déjà une différence qui compte : une coquille native n'a pas de
/// paquet web à produire, donc pas de chaîne d'outils JavaScript à installer pour la construire.
fn build_rust(shell: Shell) -> Result<Utf8PathBuf> {
    let (package, binary) = shell.package();
    let path = release_path(binary)?;

    if skip_build() {
        anyhow::ensure!(
            path.exists(),
            "`--no-build` demandé mais {path} n'existe pas : compiler d'abord, puis laisser la \
             machine retomber au repos"
        );
        println!("Compilation sautée (--no-build)");
        return Ok(path);
    }

    println!("Compilation de {package} en release…");
    let built = Command::new(env!("CARGO"))
        .args(["build", "--release", "-p", package])
        .status()
        .with_context(|| format!("compilation de {package}"))?;
    anyhow::ensure!(built.success(), "la compilation de {package} a échoué");

    Ok(path)
}

/// Le chemin d'un exécutable du profil release.
fn release_path(binary: &str) -> Result<Utf8PathBuf> {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("racine du workspace")?
        .to_path_buf();
    let file = if cfg!(windows) {
        format!("{binary}.exe")
    } else {
        binary.to_owned()
    };
    Ok(root.join("target").join("release").join(file))
}

/// Vrai quand l'appelant a demandé qu'on ne compile pas.
///
/// ## Pourquoi ça existe, et pourquoi c'est un drapeau global
///
/// **Mesurer un démarrage juste après une compilation donne un chiffre faux.** Les relevés du
/// 2026-09-02 étaient gonflés d'un facteur trois pour cette raison, et le 2026-09-03 le même
/// piège s'est refermé sur le critère 5 : 32,7 ms d'abord, 47,2 ms ensuite, parce que le
/// harnais recompilait 58 s juste avant de chronométrer. Sur un budget de 50 ms, l'erreur
/// n'était plus une nuance.
///
/// Compiler d'abord soi-même ne suffit pas : `cargo build` lancé depuis ce processus ne rend
/// pas toujours l'empreinte identique à celle d'un `cargo build` lancé depuis un shell — les
/// variables d'environnement posées par `cargo run` entrent dans l'empreinte des scripts de
/// construction, donc `ring` et ce qui en dépend se recompilent. Le seul moyen sûr est de
/// **pouvoir dire au harnais de ne rien compiler**.
///
/// Un drapeau global plutôt qu'un paramètre : la demande traverse cinq fonctions dont aucune
/// n'a de décision à prendre là-dessus, et un booléen de plus dans chacune serait cinq
/// occasions de l'oublier.
fn skip_build() -> bool {
    SKIP_BUILD.load(std::sync::atomic::Ordering::Relaxed)
}

static SKIP_BUILD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Enregistre la demande de l'appelant, une fois, avant toute mesure.
pub fn set_skip_build(skip: bool) {
    SKIP_BUILD.store(skip, std::sync::atomic::Ordering::Relaxed);
}

/// Compile la coquille Tauri en release et rend le chemin de l'exécutable.
///
/// Le front est embarqué à la compilation par `tauri::generate_context!` : construire le
/// paquet web d'abord n'est donc pas une commodité, c'est une condition. `npm` est appelé ici
/// parce qu'il n'y a pas d'autre manière de produire ce paquet — c'est la contrainte de
/// runtime que le `CLAUDE.md` demande de nommer quand on sort de Rust.
fn build() -> Result<Utf8PathBuf> {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("racine du workspace")?
        .to_path_buf();
    let web = root.join("crates").join("mail-ui").join("web");

    if skip_build() {
        let path = release_path("mail-ui")?;
        anyhow::ensure!(
            path.exists(),
            "`--no-build` demandé mais {path} n'existe pas : compiler d'abord, front compris"
        );
        println!("Compilation sautée (--no-build)");
        return Ok(path);
    }

    println!("Construction du front pour la coquille…");
    let built = Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
        .args(["run", "build:tauri"])
        .current_dir(web.as_std_path())
        .status()
        .with_context(|| format!("npm run build:tauri dans {web}"))?;
    anyhow::ensure!(built.success(), "la construction du front a échoué");

    // `--features custom-protocol` n'est pas une option de performance : sans elle, Tauri
    // charge `devUrl` au lieu du paquet embarqué, et la fenêtre s'ouvre sur la page d'erreur
    // du webview parce que rien n'écoute sur le port de Vite. Voir `mail-ui/Cargo.toml`.
    println!("Compilation de mail-ui en release…");
    let built = Command::new(env!("CARGO"))
        .args([
            "build",
            "--release",
            "-p",
            "mail-ui",
            "--features",
            "custom-protocol",
        ])
        .status()
        .context("compilation de mail-ui")?;
    anyhow::ensure!(built.success(), "la compilation de mail-ui a échoué");

    Ok(root.join("target").join("release").join(if cfg!(windows) {
        "mail-ui.exe"
    } else {
        "mail-ui"
    }))
}

/// Lance l'application une fois et lit son relevé.
fn once(
    binary: &Utf8PathBuf,
    store_root: &Utf8PathBuf,
    shell: Shell,
    bench: &str,
    daemon: Option<&str>,
) -> Result<Run> {
    let mut command = Command::new(binary.as_std_path());
    if let Some(host) = daemon {
        // En mode distant, la coquille ne touche pas au store : c'est le démon qui l'a.
        command.args(["--daemon", host]);
    }
    let mut child = command
        .env("MAILCORE_STORE", store_root.as_str())
        .env("MAILCORE_UI_BENCH", bench)
        // Les traces de l'application sortent sur l'erreur standard, et c'est par là que le
        // relevé arrive. `info` suffit ; le reste brouillerait la lecture.
        .env("RUST_LOG", "info,html5ever=error")
        // `MAILCORE_UI_POSITION` n'a pas besoin d'être posée ici : l'environnement du banc est
        // hérité tel quel, faute d'`env_clear`. La poser à la main serait une deuxième vérité
        // à maintenir. `MAILCORE_UI_POSITION=1920,0 cargo xtask measure-ui …` suffit donc à
        // déporter la fenêtre sur un second écran, ce qui rend un banc supportable pendant
        // qu'on travaille.
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("démarrage de {binary}"))?;

    // Le chronomètre part **après** le `spawn` : ce qui précède est notre propre préparation,
    // pas le démarrage de l'application. Ce que le système met à charger l'exécutable est en
    // revanche bien compté, puisque le processus n'a encore rien exécuté de son `main`.
    let started = Instant::now();

    let stderr = child.stderr.take().context("sortie d'erreur absente")?;
    let app = App(child);

    // La lecture se fait dans un fil, et le résultat arrive par un canal : un `read_line`
    // direct sur une application qui ne rend jamais la main bloquerait la mesure pour de bon.
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            // L'instant est pris ici, au plus près de l'arrivée de la ligne.
            if sender.send((Instant::now(), line)).is_err() {
                return;
            }
        }
    });

    let mut run = Run {
        interactive: None,
        settled: None,
        page_ms: None,
        process_ms: None,
        rows: None,
        context_ms: None,
        frames: None,
        from_cache: false,
    };
    let deadline = started + PATIENCE;
    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();

    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!(
                "l'application n'a rien rapporté en {} s",
                PATIENCE.as_secs()
            );
        }
        let Ok((at, line)) = receiver.recv_timeout(left) else {
            // Le canal s'est fermé : l'application a rendu la main. C'est le cas normal en
            // fin de banc, et c'est une erreur si rien n'a été relevé — traité plus bas.
            break;
        };
        if !line.contains(MARK) {
            // Gardé pour le message d'erreur. Écrit après avoir cherché pourquoi une
            // exécution ne rapportait rien : les traces de l'application disaient exactement
            // où elle s'était arrêtée, et les jeter les rendait invisibles.
            tail.push_back(line);
            if tail.len() > TAIL {
                tail.pop_front();
            }
            continue;
        }
        let elapsed = at.duration_since(started);

        if line.contains(shell.paint_marker()) {
            run.interactive = Some(elapsed);
            // Ces trois champs n'existent que dans la coquille Tauri ; la sonde native n'a pas
            // de « temps de page » distinct de son temps de processus. Absents, ils restent à
            // `None` et le rapport les omet.
            run.page_ms = field(&line, "page_ms").and_then(|it| it.parse().ok());
            run.process_ms = field(&line, "processus_ms").and_then(|it| it.parse().ok());
            run.rows = field(&line, "lignes").and_then(|it| it.parse().ok());
        } else if line.contains("phase=settled") {
            run.settled = Some(elapsed);
        } else if line.contains("etape=cache") {
            // La coquille a trouvé un cache : ce démarrage-là n'attend pas le réseau.
            run.from_cache = true;
        } else if line.contains("etape=contexte") {
            // Le jalon propre à la sonde native : fenêtre et contexte graphique prêts, avant
            // la première image. L'équivalent de « fenêtre montée » côté Tauri.
            run.context_ms = field(&line, "ms").and_then(|it| it.parse().ok());
        } else if line.contains("critere=2") || line.contains("critere=5") {
            // Rapportée telle quelle : la page a déjà fait le calcul, et le refaire ici
            // n'ajouterait qu'une occasion de le déformer.
            run.frames = line.find(MARK).map(|at| line[at..].trim().to_owned());
            break;
        } else if let Some(at) = line.find("diag ") {
            // Les notes du banc : ce qu'il a chargé, et pourquoi il s'est arrêté là.
            println!("      {}", line[at + "diag ".len()..].trim());
        }
    }

    drop(app);

    // Un relevé de critère prouve à lui seul que la coquille a peint : il vient d'un banc qui
    // compte des centaines d'images. L'exigence ci-dessous vise l'autre panne — une page qui
    // n'appelle jamais sa coquille — et elle ne doit pas condamner un banc qui n'a pas de
    // **lignes** à afficher : le jalon de la coquille native est `etape=premieres_lignes`, donc
    // il n'arrive jamais sur un store vide, ce qui est exactement le cas du banc `signature`.
    if run.interactive.is_none() && run.frames.is_none() {
        let traces = if tail.is_empty() {
            "  (l'application n'a rien tracé)".to_owned()
        } else {
            tail.iter()
                .map(|it| format!("  {it}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        bail!(
            "aucun relevé du critère 1 : la page n'a pas appelé la coquille. Un écran inerte \
             vient d'ordinaire d'une CSP qui bloque l'IPC — voir vite.config.ts.\n\
             Dernières traces de l'application :\n{traces}"
        );
    }
    Ok(run)
}

/// Extrait `clé=valeur` d'une ligne de relevé.
fn field(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let at = line.find(&needle)? + needle.len();
    let rest = &line[at..];
    let end = rest.find(' ').unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

/// Formate une durée en millisecondes.
fn millis(it: Duration) -> String {
    format!("{:.1} ms", it.as_secs_f64() * 1000.0)
}

/// Le rapport final, verdict compris.
fn report(runs: &[Run]) {
    let Some(first) = runs.first() else {
        return;
    };

    println!("Critère 1 — démarrage à froid jusqu'à l'interface, budget 400 ms");
    if let Some(cold) = first.interactive {
        println!("  À froid (1re exécution)  {}", millis(cold));
    }

    // La médiane des exécutions suivantes : le démarrage ordinaire, cache du système chaud.
    let mut warm: Vec<Duration> = runs
        .iter()
        .skip(1)
        .filter_map(|it| it.interactive)
        .collect();
    warm.sort_unstable();
    if let Some(median) = warm.get(warm.len() / 2) {
        println!("  Médiane à chaud          {}", millis(*median));
    }
    if let Some(context) = first.context_ms {
        // Sonde native : fenêtre et contexte graphique prêts. Le reste, c'est la première
        // image — et c'est ce rapport-là qui dit si le budget est mangé par le toolkit ou par
        // notre mise en page.
        println!("  Fenêtre et contexte      {context:.1} ms");
    }
    if let (Some(process), Some(page)) = (first.process_ms, first.page_ms) {
        // La part du processus contient celle de la page : le webview est créé par la
        // coquille, donc le temps de la page est inclus dans le sien.
        println!("  Dont la page             {page:.1} ms sur {process:.1} ms de processus");
    }
    if let Some(settled) = first.settled {
        println!(
            "  Interface installée      {} (après le 1er appel)",
            millis(settled)
        );
    }

    println!(
        "  Verdict                  {}",
        match first.interactive.map(|it| it <= BUDGET) {
            Some(true) => "passé",
            Some(false) => "ÉCHOUÉ",
            None => "non mesuré",
        }
    );

    if let Some(frames) = runs.iter().find_map(|it| it.frames.as_deref()) {
        println!();
        println!("Critère 2 — défilement, 60 fps constant");
        println!("  {frames}");

        let number = |key: &str| -> Option<f64> { field(frames, key)?.parse().ok() };
        let baseline = number("repos");
        let dropped = number("perdues");
        let work = number("travail_p95");
        let rest_work = number("travail_repos");

        // **Le budget de 16,7 ms se compare au travail par image, pas à la cadence.** La
        // cadence appartient au compositeur : il ralentit une fenêtre sans premier plan, et
        // deux exécutions du même code ont donné 55 Hz puis 32 Hz. La comparer au budget
        // dirait « échoué » pour une raison qui n'est pas la nôtre.
        if let Some(hz) = baseline.filter(|it| *it > 0.0).map(|it| 1000.0 / it) {
            println!("  Cadence observée         {hz:.1} Hz au repos (le compositeur décide)");
        }
        if let (Some(work), Some(rest)) = (work, rest_work) {
            println!(
                "  Travail par image        {work:.2} ms de p95 en défilant, {rest:.2} ms au repos"
            );
        }
        if let Some(dropped) = dropped {
            println!("  Images perdues           {dropped:.0} sur la série, contre le repos");
        }

        // Le compte d'images perdues n'existe que dans le banc du front web ; la sonde native
        // ne le calcule pas. Absent, le verdict se prend sur le seul travail par image — qui
        // est de toute façon le chiffre qui se compare au budget.
        let verdict = match (work, dropped) {
            (Some(work), _) if work > BUDGET_FRAME_MS => {
                format!("ÉCHOUÉ — {work:.2} ms de travail par image pour un budget de 16,7 ms")
            }
            (Some(_), Some(dropped)) if dropped > 0.0 => {
                "ÉCHOUÉ — des images ont été perdues".to_owned()
            }
            (Some(work), _) => format!(
                "passé — {work:.2} ms par image, dans le budget de 16,7 ms d'une image à 60 fps"
            ),
            _ => "non mesuré".to_owned(),
        };
        println!("  Verdict                  {verdict}");
        println!("  Le travail par image est l'écart entre le début de l'image, décidé par le");
        println!("  compositeur, et l'instant où notre rappel reprend la main : la mise à jour");
        println!("  du défilement y est incluse. La latence d'entrée d'une vraie molette, non —");
        println!("  ça demanderait un pilote de navigateur.");
    }
}

/// Mesure le **critère 4 sur un vrai serveur** : le travail par image pendant une **vraie**
/// moisson complète.
///
/// ## Pourquoi un store jetable et non le store réel
///
/// Le critère dit « pendant une sync complète ». Sur le store réel, une moisson complète est
/// devenue **impossible à provoquer** : le correctif de reprise du 2026-09-08 ne redemande
/// aucun corps déjà possédé, ce qui est une bonne propriété du produit et coûte cette mesure.
/// C'est la même réserve que celle écrite pour le critère 2.
///
/// Un store neuf la rend possible sans rien casser : le compte y est déclaré avec le **même**
/// hôte et le même identifiant, donc le secret du trousseau est retrouvé, et le serveur est
/// lu comme d'habitude — en lecture seule. Le store réel n'est pas ouvert du tout.
///
/// ## Ce que ça mesure de plus que le banc contre `mailfake`
///
/// Le relevé du 2026-09-03 court-circuitait TLS : `mailfake` parle en clair. Ici la moisson
/// passe par `mailsync::connect`, donc par `rustls`, la vérification de certificat, OAuth2
/// quand le compte l'utilise, et des corps de messages réels — dont les pièces jointes, qui
/// sont ce qui fait vraiment travailler l'écriture de blobs.
///
/// ## Tous les comptes à la fois, et la bande passante reste bornée
///
/// C'est ce que dit le critère — « pendant que dix comptes se synchronisent » — et le premier
/// jet visait un seul compte, ce qui était **insuffisant** : la fenêtre de défilement dure
/// ~3,3 s, et une moisson sur une liaison lente n'y valide que deux ou trois lots. Mesuré le
/// 2026-09-09 : deux changements du store reçus avec un compte, zéro avec un autre, et le
/// contrôle du banc a refusé ce dernier relevé.
///
/// Le coût en bande passante ne suit pas la taille des comptes : les moissons sont **annulées
/// dès la coquille refermée**, donc on paie la durée du banc et rien de plus — ~1 500 messages
/// par exécution, contre les 105 508 du corpus. `--account` reste là pour isoler un
/// fournisseur quand on cherche à comprendre lequel écrit vite.
fn during_real_sync(store_root: &Utf8PathBuf, account: Option<i64>, runs: usize) -> Result<()> {
    if runs == 0 {
        bail!("il faut au moins une exécution");
    }
    let targets = pick(store_root, account)?;
    let binary = build_rust(Shell::Egui)?;

    // **Les traces des moissons sont visibles, sur demande.** Ce banc est le seul endroit du
    // projet qui fasse écrire cinq moissons dans le même store, et c'est lui qui a trouvé la
    // contention de verrou du 2026-09-09. Sans abonné, l'échec ne dit pas à quelle étape il
    // est tombé, et diagnostiquer une contention sans savoir qui tenait le verrou revient à
    // deviner. `RUST_LOG=mailsync=debug` suffit.
    //
    // Sur l'erreur standard : la sortie standard porte le rapport.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // Un abonné déjà posé n'est pas une erreur : le banc mesure, il ne tient pas au journal.
    drop(
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init(),
    );

    println!("Binaire     {binary}");
    println!(
        "Comptes     {} — {}",
        targets.len(),
        targets
            .iter()
            .map(|it| format!("#{}", it.id.0))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("Banc        sync-real × {runs}");
    println!(
        "Store réel  {store_root} — **lu pour trouver les comptes, jamais écrit**\n\
         Chaque exécution moissonne dans un store jetable, et le jette après.\n"
    );

    let mut lines = Vec::with_capacity(runs);
    for index in 0..runs {
        let (line, written) = one_real_sync_run(&binary, &targets)?;
        match line {
            Some(line) => {
                println!("  {:>2}. {written} messages écrits   {line}", index + 1);
                lines.push(line);
            }
            None => println!("  {:>2}. aucun relevé", index + 1),
        }
    }
    println!();

    anyhow::ensure!(
        !lines.is_empty(),
        "aucune exécution n'a rendu de relevé : le banc n'a rien mesuré"
    );
    verdict_of(
        &lines,
        "sur un vrai serveur",
        "la taille du compte visé, ou abaisser `REAL_SYNC_WARMUP`",
    )
}

/// Les comptes à moissonner : celui demandé, ou **tous les actifs**.
///
/// Tous par défaut, parce que c'est ce que dit le critère 4 — « pendant que dix comptes se
/// synchronisent ». `--account` reste là pour isoler un fournisseur quand on cherche à
/// comprendre lequel écrit vite, et pour ne pas dépenser la bande passante des autres.
fn pick(store_root: &Utf8PathBuf, account: Option<i64>) -> Result<Vec<mailcore::Account>> {
    let store =
        mailcore::Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let active: Vec<mailcore::Account> = store
        .full_accounts()?
        .into_iter()
        .filter(|it| it.kind == mailcore::AccountKind::Imap && it.enabled && it.server.is_some())
        .collect();

    anyhow::ensure!(
        !active.is_empty(),
        "aucun compte IMAP actif dans {store_root}"
    );
    if let Some(wanted) = account {
        let one = active
            .into_iter()
            .find(|it| it.id.0 == wanted)
            .with_context(|| format!("aucun compte IMAP actif portant le numéro {wanted}"))?;
        return Ok(vec![one]);
    }
    Ok(active)
}

/// Une exécution : store neuf, **toutes** les moissons en cours, la coquille qui défile dedans.
///
/// ## Pourquoi tous les comptes à la fois
///
/// Le critère 4 est écrit ainsi : « la coquille qui défile pendant que **dix comptes** se
/// synchronisent ». Un seul compte réel ne fait pas travailler le store autant qu'on croit : la
/// fenêtre de défilement du banc dure ~3,3 s — 600 images à ~5,5 ms — et une moisson sur une
/// liaison à 260 ms d'aller-retour n'y valide que deux ou trois lots.
///
/// Mesuré le 2026-09-09 : deux changements du store reçus pendant le défilement avec un compte,
/// **zéro** avec un compte sur une liaison rapide mais peu fourni — le contrôle du banc a
/// refusé ce dernier relevé, et il avait raison. Plusieurs moissons concurrentes multiplient la
/// fréquence des commits, ce qui est à la fois plus fidèle au critère et plus dur pour la
/// coquille : plusieurs écrivains sur la même base SQLite.
fn one_real_sync_run(
    binary: &Utf8PathBuf,
    targets: &[mailcore::Account],
) -> Result<(Option<String>, u64)> {
    let dir = tempfile::tempdir().context("répertoire de travail")?;
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
        .map_err(|it| anyhow::anyhow!("chemin non UTF-8 : {}", it.display()))?;

    // Les comptes sont déclarés dans le store jetable avec le **même** hôte et le même
    // identifiant que dans le store réel : c'est la clé sous laquelle le trousseau range le
    // secret, et les déclarer autrement ferait échouer l'authentification sur un secret
    // introuvable.
    let mut jobs = Vec::with_capacity(targets.len());
    {
        let store = mailcore::Store::open(&root).context("ouverture du store jetable")?;
        let writer = store.writer()?;
        for target in targets {
            let server = target
                .server
                .as_ref()
                .context("compte sans serveur : incohérent en base")?
                .clone();
            let id = writer.upsert_imap_account(&target.display_name, &server)?;
            jobs.push((id, server));
        }
        writer.commit()?;
    }

    // Les secrets sont lus **avant** de lancer les fils, dans le processus du banc :
    // `secret_for` rafraîchit un jeton OAuth2 si besoin, et un échec doit se voir tout de suite
    // plutôt que dans un fil dont personne ne lit le résultat avant la fin.
    let mut secrets = Vec::with_capacity(jobs.len());
    for (id, server) in &jobs {
        let secret = mailauth::session::secret_for(
            &server.host,
            &server.username,
            server.auth.as_str(),
            now_seconds(),
        )
        .with_context(|| format!("secret du compte {}", server.username))?;
        secrets.push((*id, server.auth, secret));
    }

    // L'annulation passe par la `Progress` de la moisson : c'est le mécanisme réel du démon,
    // coopératif et vérifié entre deux lots de cent messages. Un drapeau à nous mesurerait
    // autre chose. **Une seule pour toutes les moissons** : on les arrête ensemble.
    let progress = std::sync::Arc::new(mailcore::Progress::new());
    let mut harvests = Vec::with_capacity(secrets.len());
    for (id, auth, secret) in secrets {
        let harvest_progress = std::sync::Arc::clone(&progress);
        let harvest_root = root.clone();
        harvests.push(std::thread::spawn(move || -> Result<()> {
            let store = mailcore::Store::open(&harvest_root)?;
            let account = store
                .full_accounts()?
                .into_iter()
                .find(|it| it.id == id)
                .context("le compte vient de disparaître du store jetable")?;
            let credential = mailsync::Credential::for_auth(auth, &secret);
            let report = mailsync::sync_account(&store, &account, credential, &harvest_progress)?;
            if !report.failures.is_empty() {
                println!("      #{} dossiers en échec : {:?}", id.0, report.failures);
            }
            Ok(())
        }));
    }

    // On attend que la coquille ait de quoi défiler : son banc trouverait sinon un dossier
    // vide, se plaindrait, et se refermerait sans rien mesurer.
    let ready = wait_for_rows(&root, REAL_SYNC_WARMUP)?;
    println!(
        "      {} moisson(s) en cours, {ready} messages en magasin — la coquille démarre",
        harvests.len()
    );

    let run = once(binary, &root, Shell::Egui, "scroll", None)?;

    // La coquille est refermée : ce qui reste à télécharger n'apporte rien à la mesure, et le
    // banc ne doit pas durer le temps de tous les comptes.
    progress.cancel();
    for handle in harvests {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(source)) => println!("      une moisson a échoué : {source:#}"),
            Err(_) => println!("      un fil de moisson a paniqué"),
        }
    }
    let written = mailcore::Store::open(&root)?.stats()?.messages;

    anyhow::ensure!(
        written > ready,
        "les moissons n'ont rien écrit pendant le défilement ({ready} → {written}) : le banc ne \
         mesurerait que le critère 2 sous un autre nom. Viser plus de comptes, ou abaisser \
         REAL_SYNC_WARMUP."
    );
    Ok((run.frames, written))
}

/// Messages à avoir en magasin avant de lancer la coquille, sur un vrai serveur.
///
/// Plus bas que le seuil du banc contre `mailfake` — 2 000 là-bas — parce qu'un vrai compte
/// n'a pas 60 000 messages dans un seul dossier, et que le recouvrement compte plus que la
/// longueur de la liste : ce qu'on mesure est le travail par image **pendant** des écritures,
/// pas la capacité à défiler longtemps.
const REAL_SYNC_WARMUP: u64 = 600;

/// L'horloge, en secondes Unix.
fn now_seconds() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |it| it.as_secs()),
    )
    .unwrap_or(i64::MAX)
}

/// Le verdict du critère 4, à partir des relevés de la coquille.
///
/// Partagé par les deux bancs — celui contre `mailfake` et celui contre un vrai serveur — pour
/// que les deux chiffres soient comparables ligne à ligne. Deux calculs de p95 écrits
/// séparément finiraient par ne plus dire la même chose.
fn verdict_of(lines: &[String], what: &str, remedy: &str) -> Result<()> {
    let number = |line: &str, key: &str| -> Option<f64> { field(line, key)?.parse().ok() };

    println!("Critère 4 {what} — travail par image pendant une sync, budget 16,7 ms");

    // **Le contrôle du banc, avant le chiffre.** Un relevé pris après la fin de la moisson
    // serait indiscernable d'un vrai, et il mesurerait le critère 2 sous un autre nom. Le
    // compteur vient de la coquille : ce sont les changements du store qu'elle a reçus
    // **pendant** ses 600 images de défilement.
    let changes = lines
        .iter()
        .filter_map(|it| number(it, "changements"))
        .fold(f64::INFINITY, f64::min);
    println!("  Changements reçus        {changes:.0} pendant le défilement");
    anyhow::ensure!(
        changes.is_finite() && changes > 0.0,
        "aucun changement du store reçu pendant le défilement : la moisson était finie avant \
         que la coquille commence à défiler, et ce relevé ne mesure pas le critère 4.\n\
         Augmenter {remedy}."
    );

    // Le pire p95 des exécutions : un critère se juge sur le mauvais jour.
    let work = lines
        .iter()
        .filter_map(|it| number(it, "travail_p95"))
        .fold(f64::NAN, f64::max);
    let worst = lines
        .iter()
        .filter_map(|it| number(it, "travail_pire"))
        .fold(f64::NAN, f64::max);
    let rows = lines
        .iter()
        .filter_map(|it| number(it, "lignes"))
        .fold(f64::NAN, f64::max);

    println!("  Lignes défilées          {rows:.0}");
    println!("  Travail par image, p95   {work:.2} ms");
    println!("  Pire image               {worst:.2} ms");
    println!(
        "  Verdict                  {}",
        if work.is_nan() {
            "non mesuré"
        } else if work <= BUDGET_FRAME_MS {
            "passé"
        } else {
            "ÉCHOUÉ"
        }
    );
    println!("  La cadence de l'écran n'entre pas dans ce chiffre : voir le critère 2 de");
    println!("  docs/PHASE-1.md, qui explique pourquoi c'est le travail qu'on compare au");
    println!("  budget et pas l'intervalle entre deux images.");
    Ok(())
}

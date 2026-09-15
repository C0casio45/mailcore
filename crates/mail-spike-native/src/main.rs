//! Sonde jetable : **une coquille native, sur le vrai store, mesurée comme l'autre**.
//!
//! ## La question qu'elle tranche
//!
//! La sonde `mail-spike-webview` a montré que le webview coûte ~935 ms à créer sur cette
//! machine, et que Tauri n'y ajoute rien : le critère 1 (`< 400 ms`) est hors de portée pour
//! toute architecture qui crée un webview au démarrage. Reste la question qui décide :
//! **une fenêtre native avec un vrai rendu, ça coûte combien, et est-ce que la liste dense
//! tient ses images ?**
//!
//! Les jalons portent la même clé `MESURE-UI` et le même découpage que la coquille Tauri, pour
//! que les deux relevés se lisent côte à côte sans traduction :
//!
//! - `store` — ouverture SQLite et montage de l'index, à comparer aux 31 ms de la coquille ;
//! - `premiere_image` — l'équivalent exact du jalon `paint` : l'interface est à l'écran avec
//!   des lignes réelles dedans, et elle répond ;
//! - `critere=2` — le travail par image en défilant, mesuré comme dans le front web.
//!
//! ## Ce qu'elle fait, et ce qu'elle ne fait pas
//!
//! Elle ouvre le store, prend le plus gros dossier, en charge les lignes et les affiche dans
//! une liste virtualisée. **Elle lit `mailcore::Store` en direct**, sans JSON-RPC ni tokio :
//! c'est le plancher, et c'est ce qu'on veut connaître. Une vraie coquille passerait par
//! `maild::Service` comme la coquille Tauri — 31 ms d'ouverture, mesurés, et rien d'autre en
//! plus sur le chemin du démarrage.
//!
//! Pas de volet de lecture, pas de recherche, pas de cache. Ce n'est pas une application,
//! c'est une réponse à deux questions chiffrées.
//!
//! `eprintln!` plutôt que `tracing`, comme l'autre sonde : ce qui est mesuré ici est un temps
//! de démarrage, et une pile de journalisation serait une chose de plus à initialiser dedans.
//!
//! ## Son statut
//!
//! **Jetable.** Elle répond, le chiffre va dans `docs/PHASE-1.md`, elle disparaît — ou elle
//! devient le point de départ de la coquille si la décision va dans ce sens.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use eframe::egui;
use mailcore::store::read::ListItem;
use mailcore::{FolderId, Store};

/// Hauteur d'une ligne, en points. La même que le thème du front web.
const ROW_HEIGHT: f32 = 22.0;

/// Lignes chargées avant la première image. Un écran large, pas toute la boîte.
const FIRST_PAGE: u32 = 100;

/// Lignes par page ensuite, en fond.
const PAGE: u32 = 500;

/// Images observées au repos, puis en défilant. Mêmes nombres que le banc du front web.
const IDLE_FRAMES: usize = 120;
const SCROLL_FRAMES: usize = 600;

/// Ce que la sonde doit faire de sa vie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bench {
    /// Relever la première image, puis se refermer.
    Startup,
    /// Charger tout le dossier, défiler, relever le travail par image, se refermer.
    Scroll,
    /// Rester ouverte : pour regarder à quoi ça ressemble.
    None,
}

/// Les lignes du dossier, remplies au fur et à mesure par le fil de fond.
///
/// Un `Vec` et pas une carte : les lignes arrivent dans l'ordre d'affichage — du plus récent au
/// plus ancien — parce que la pagination du store est par clé sur `(date, id)`. La position dans
/// le `Vec` **est** la position dans la liste, donc la virtualisation n'a rien à chercher.
#[derive(Debug, Default)]
struct Loaded {
    items: Vec<ListItem>,
    done: bool,
}

fn main() -> Result<()> {
    // Avant toute autre chose : c'est le zéro du critère 1, comme dans la coquille.
    let started = Instant::now();
    let since = move || started.elapsed().as_secs_f64() * 1000.0;

    let bench = match std::env::var("MAILCORE_UI_BENCH").as_deref() {
        Ok("startup") => Bench::Startup,
        Ok("scroll") => Bench::Scroll,
        _ => Bench::None,
    };

    let root = std::env::var("MAILCORE_STORE").context("MAILCORE_STORE doit nommer un store")?;
    let root = camino::Utf8PathBuf::from(root);

    let store = Store::open(&root).with_context(|| format!("ouverture de {root}"))?;
    let folders = store.folders()?;
    let counts = store.folder_counts()?;
    eprintln!("MESURE-UI natif etape=store ms={:.1}", since());

    // Le plus gros dossier : c'est celui sur lequel le critère 2 se joue.
    let (folder, total) = folders
        .iter()
        .filter_map(|it| counts.get(&it.id).map(|(total, _)| (it, *total)))
        .max_by_key(|(_, total)| *total)
        .map(|(it, total)| (it.id, total))
        .context("aucun dossier dans ce store")?;
    let path = folders
        .iter()
        .find(|it| it.id == folder)
        .map_or_else(String::new, |it| it.path.clone());

    // La première page avant d'ouvrir la fenêtre : la première image doit porter de vraies
    // lignes, sinon le jalon ne mesure pas la même chose que le `paint` du front web.
    let first = store.page(folder, None, FIRST_PAGE)?;
    let loaded = Arc::new(Mutex::new(Loaded {
        items: first,
        done: false,
    }));

    // Le reste en fond. Le fil ouvre sa propre poignée : `Store` porte une connexion SQLite,
    // qui n'est pas faite pour traverser les fils.
    spawn_loader(root.clone(), folder, Arc::clone(&loaded));

    // Réglable pour la même raison que dans la sonde logicielle : il faut pouvoir comparer les
    // deux moteurs à nombre de pixels égal. C'est là que le rendu logiciel se casse et pas ici.
    let size = std::env::var("MAILCORE_UI_SIZE")
        .ok()
        .and_then(|it| {
            let (width, height) = it.split_once('x')?;
            Some([
                width.trim().parse::<f32>().ok()?,
                height.trim().parse::<f32>().ok()?,
            ])
        })
        .unwrap_or([1280.0, 800.0]);
    eprintln!(
        "MESURE-UI natif etape=taille largeur={} hauteur={}",
        size[0], size[1]
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size([600.0, 400.0])
            .with_title("mailcore — sonde native"),
        ..Default::default()
    };

    eframe::run_native(
        "mailcore-spike",
        options,
        Box::new(move |cx| {
            // Le jalon qui sépare l'initialisation d'eframe — fenêtre, contexte graphique,
            // pipelines — de la construction de la première image.
            eprintln!("MESURE-UI natif etape=contexte ms={:.1}", since());
            dense_theme(&cx.egui_ctx);
            Ok(Box::new(App {
                since: Box::new(since),
                bench,
                folders,
                counts,
                folder,
                path,
                total,
                loaded,
                announced: false,
                phase: Phase::Waiting,
                offset: 0.0,
                idle: Series::default(),
                scrolling: Series::default(),
                last: None,
                snapshot: Vec::new(),
                done: false,
            }))
        }),
    )
    .map_err(|error| anyhow::anyhow!("eframe : {error}"))
}

/// Charge le dossier page par page, dans un fil, sans jamais bloquer l'interface.
///
/// C'est la règle 3 du `CLAUDE.md` appliquée au plus petit cas possible : l'interface dessine
/// ce qu'elle a, le chargement avance à côté, et une page lente ne retient aucune image.
fn spawn_loader(root: camino::Utf8PathBuf, folder: FolderId, loaded: Arc<Mutex<Loaded>>) {
    std::thread::spawn(move || {
        let Ok(store) = Store::open(&root) else {
            return;
        };
        let mut cursor = {
            let Ok(guard) = loaded.lock() else {
                return;
            };
            Store::next_cursor(&guard.items)
        };
        loop {
            let Ok(page) = store.page(folder, cursor, PAGE) else {
                return;
            };
            if page.is_empty() {
                if let Ok(mut guard) = loaded.lock() {
                    guard.done = true;
                }
                return;
            }
            cursor = Store::next_cursor(&page);
            let Ok(mut guard) = loaded.lock() else {
                return;
            };
            guard.items.extend(page);
            if cursor.is_none() {
                guard.done = true;
                return;
            }
        }
    });
}

/// Le thème : dense, sans effet. Comme `theme.css`, en beaucoup plus court.
///
/// Volontairement minimal : une sonde n'a pas à être belle, elle a à être **dense**, parce que
/// la densité est ce qui décide du coût par image. Le reste du thème appartient à la vraie
/// coquille, si elle voit le jour.
fn dense_theme(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 0.0);
    });
}

/// Une série d'images : leur intervalle, et le travail fait dedans.
#[derive(Debug, Default)]
struct Series {
    /// Intervalles entre images, en ms. La cadence, décidée par le compositeur.
    deltas: Vec<f64>,
    /// Temps processeur de l'image, en ms, tel qu'`eframe` le rapporte : mise en page,
    /// construction du maillage et dessin. C'est l'équivalent du « travail par image » du
    /// front web, et c'est ce qui se compare au budget de 16,7 ms.
    work: Vec<f64>,
}

impl Series {
    fn push(&mut self, delta: Option<f64>, work: Option<f64>) {
        if let Some(delta) = delta {
            self.deltas.push(delta);
        }
        if let Some(work) = work {
            self.work.push(work);
        }
    }

    fn percentile(series: &[f64], fraction: f64) -> f64 {
        if series.is_empty() {
            return 0.0;
        }
        let mut sorted = series.to_vec();
        sorted.sort_by(f64::total_cmp);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rank = ((fraction * sorted.len() as f64) as usize).min(sorted.len() - 1);
        sorted[rank]
    }
}

/// Où en est le banc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Attend que le dossier soit entièrement chargé.
    Waiting,
    /// Relève la cadence au repos.
    Idle,
    /// Défile.
    Scrolling,
    /// Terminé, la fenêtre se referme.
    Done,
}

struct App {
    since: Box<dyn Fn() -> f64>,
    bench: Bench,
    folders: Vec<mailcore::Folder>,
    counts: std::collections::HashMap<FolderId, (u64, u64)>,
    folder: FolderId,
    path: String,
    total: u64,
    loaded: Arc<Mutex<Loaded>>,
    announced: bool,
    phase: Phase,
    offset: f32,
    idle: Series,
    scrolling: Series,
    last: Option<Instant>,
    /// Les lignes telles que la dernière image les a vues. Recopiée seulement quand le fil
    /// de chargement en a ajouté — voir `ui`.
    snapshot: Vec<ListItem>,
    done: bool,
}

impl eframe::App for App {
    /// **`ui` et non `update`** : depuis eframe 0.36, l'application reçoit directement une
    /// `Ui` racine plutôt qu'un `Context`, et les panneaux s'y posent par `show_inside`.
    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let ctx = &ctx;
        let now = Instant::now();
        let delta = self
            .last
            .map(|last| now.duration_since(last).as_secs_f64() * 1000.0);
        self.last = Some(now);
        // `cpu_usage` est le temps de l'image précédente, en secondes.
        let work = frame.info().cpu_usage.map(|it| f64::from(it) * 1000.0);

        // **Une copie par page reçue, pas une par image.** Le premier jet clonait les 20 544
        // lignes à chaque image : quelques millisecondes de faux coût par image, imputées au
        // toolkit alors qu'elles venaient de la sonde. `try_lock` en plus, pour qu'une image
        // n'attende jamais le fil de chargement.
        if let Ok(guard) = self.loaded.try_lock() {
            if guard.items.len() != self.snapshot.len() {
                self.snapshot = guard.items.clone();
            }
            self.done = guard.done;
        }
        let items = std::mem::take(&mut self.snapshot);

        self.draw(root, &items);
        self.snapshot = items;
        let items = &self.snapshot;
        let done = self.done;

        // Le jalon de la première image est posé **après** le dessin : à cet instant,
        // l'interface est réellement à l'écran avec des lignes dedans.
        if !self.announced {
            self.announced = true;
            eprintln!(
                "MESURE-UI natif etape=premiere_image ms={:.1} lignes={} total={}",
                (self.since)(),
                items.len(),
                self.total
            );
            if self.bench == Bench::Startup {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }

        if self.bench == Bench::Scroll {
            self.advance(ctx, delta, work, items.len(), done);
        }
    }
}

impl App {
    /// Trois panneaux, sans fioriture : les dossiers, l'en-tête, la liste virtualisée.
    fn draw(&mut self, root: &mut egui::Ui, items: &[ListItem]) {
        egui::Panel::left("dossiers")
            .exact_size(240.0)
            .show(root, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for folder in &self.folders {
                        let (total, unread) =
                            self.counts.get(&folder.id).copied().unwrap_or((0, 0));
                        let last = folder.path.rsplit('/').next().unwrap_or(&folder.path);
                        let label = if unread > 0 {
                            format!("{last}  ({unread})")
                        } else {
                            last.to_owned()
                        };
                        ui.horizontal(|ui| {
                            ui.set_height(ROW_HEIGHT);
                            // La sélection n'est pas branchée : la sonde mesure, elle ne navigue pas.
                            let _ = ui.selectable_label(folder.id == self.folder, label);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.weak(total.to_string());
                                },
                            );
                        });
                    }
                });
            });

        egui::Panel::top("entete").show(root, |ui| {
            ui.horizontal(|ui| {
                ui.set_height(28.0);
                ui.strong(&self.path);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(format!("{} chargés sur {}", items.len(), self.total));
                });
            });
        });

        egui::CentralPanel::default().show(root, |ui| {
            // `show_rows` est la virtualisation d'egui : il ne construit que les lignes de
            // l'intervalle visible, et la hauteur totale vient du compte, pas du contenu.
            // C'est le même principe que la cale du front web, fourni par le toolkit.
            let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
            if self.phase == Phase::Scrolling {
                area = area.vertical_scroll_offset(self.offset);
            }
            #[allow(clippy::cast_possible_truncation)]
            let total = self.total as usize;
            area.show_rows(ui, ROW_HEIGHT, total, |ui, range| {
                for index in range {
                    ui.horizontal(|ui| {
                        ui.set_height(ROW_HEIGHT);
                        match items.get(index) {
                            Some(item) => {
                                ui.add_sized(
                                    [58.0, ROW_HEIGHT],
                                    egui::Label::new(short_date(item.date)).truncate(),
                                );
                                ui.add_sized(
                                    [200.0, ROW_HEIGHT],
                                    egui::Label::new(who(item)).truncate(),
                                );
                                ui.add(egui::Label::new(&item.subject).truncate());
                            }
                            // Ligne pas encore chargée : un vide, comme dans le front web. Un
                            // mot qui défile serait plus agité et ne dirait rien de plus.
                            None => {
                                ui.label("");
                            }
                        }
                    });
                }
            });
        });
    }

    /// Fait avancer le banc d'une image.
    fn advance(
        &mut self,
        ctx: &egui::Context,
        delta: Option<f64>,
        work: Option<f64>,
        loaded: usize,
        done: bool,
    ) {
        // Une image par tour, sans attendre un événement : c'est ce qui rend la série
        // comparable à celle du front web, pilotée par `requestAnimationFrame`.
        ctx.request_repaint();

        match self.phase {
            Phase::Waiting => {
                if done {
                    eprintln!(
                        "MESURE-UI natif diag {loaded} lignes sur {} dans « {} »",
                        self.total, self.path
                    );
                    self.phase = Phase::Idle;
                }
            }
            Phase::Idle => {
                self.idle.push(delta, work);
                if self.idle.deltas.len() >= IDLE_FRAMES {
                    self.phase = Phase::Scrolling;
                }
            }
            Phase::Scrolling => {
                self.scrolling.push(delta, work);
                #[allow(clippy::cast_precision_loss)]
                let distance = self.total as f32 * ROW_HEIGHT;
                #[allow(clippy::cast_precision_loss)]
                let step = distance / SCROLL_FRAMES as f32;
                self.offset += step;
                if self.scrolling.deltas.len() >= SCROLL_FRAMES {
                    self.report(loaded);
                    self.phase = Phase::Done;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Phase::Done => {}
        }
    }

    /// Écrit le relevé du critère 2, dans le même format que celui de la coquille.
    fn report(&self, loaded: usize) {
        let p = Series::percentile;
        eprintln!(
            "MESURE-UI natif critere=2 lignes={loaded} images={} repos={:.2} p50={:.2} \
             p95={:.2} pire={:.2} travail_repos={:.2} travail_p50={:.2} travail_p95={:.2} \
             travail_pire={:.2}",
            self.scrolling.deltas.len(),
            p(&self.idle.deltas, 0.5),
            p(&self.scrolling.deltas, 0.5),
            p(&self.scrolling.deltas, 0.95),
            p(&self.scrolling.deltas, 1.0),
            p(&self.idle.work, 0.5),
            p(&self.scrolling.work, 0.5),
            p(&self.scrolling.work, 0.95),
            p(&self.scrolling.work, 1.0),
        );
    }
}

/// Le nom à afficher : celui de l'expéditeur, à défaut son adresse.
fn who(item: &ListItem) -> &str {
    match item.from_name.as_deref() {
        Some(name) if !name.trim().is_empty() => name,
        _ => &item.from_addr,
    }
}

/// Une date courte, sans dépendance de plus.
///
/// Volontairement grossier — la sonde mesure des images, pas des calendriers. Une vraie
/// coquille formaterait à la locale du système.
fn short_date(unix: i64) -> String {
    if unix <= 0 {
        return "—".to_owned();
    }
    let days = unix / 86_400;
    let years = 1970 + days / 365;
    let day_of_year = days % 365;
    format!(
        "{:02}/{:02}/{}",
        day_of_year % 31 + 1,
        day_of_year / 31 + 1,
        years % 100
    )
}

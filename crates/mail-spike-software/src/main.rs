//! Sonde jetable : **egui rastérisé par le processeur, sans contexte GPU**.
//!
//! ## La question qu'elle tranche
//!
//! Trois relevés du 2026-09-02 encadrent celui-ci :
//!
//! | Sonde | Premier pixel / première image |
//! |---|---|
//! | `wry` nu — webview | 1 229–1 280 ms |
//! | `eframe`/`egui` sur OpenGL | 465–481 ms |
//! | `winit` + `softbuffer`, fond uni | 291–333 ms |
//!
//! La troisième dit que **la création du contexte OpenGL coûte ~150 ms** et qu'une fenêtre à
//! rendu logiciel atteint son premier pixel bien sous le budget de 400 ms du critère 1. Mais
//! elle ne dessine qu'un fond uni. Il reste donc la moitié de la question :
//!
//! **une fois qu'on y met une vraie interface — 20 544 lignes, une liste virtualisée, du texte
//! partout — est-ce que le démarrage reste sous 400 ms, et est-ce que le défilement tient son
//! image à 16,7 ms sans GPU ?**
//!
//! La rastérisation d'une fenêtre de 1280×800 par le processeur n'est pas gratuite : c'est un
//! million de pixels par image. La sonde OpenGL fait 1,47 ms de travail par image ; si le
//! logiciel en fait 12, le critère 2 devient serré et le choix se paie ailleurs.
//!
//! ## Ce qui est mesuré, et comment
//!
//! Les mêmes jalons que `mail-spike-native`, avec le même harnais
//! (`cargo xtask measure-ui --shell software`), pour que les trois relevés se lisent en
//! colonnes. Le travail par image est ici la somme de deux temps :
//!
//! - la construction de l'interface — mise en page et tessellation par egui, mesurée autour de
//!   l'appel à `ui` ;
//! - la **rastérisation**, rapportée par `SoftwareBackend::last_frame_time()`.
//!
//! C'est comparable au `cpu_usage` d'`eframe`, qui couvre les deux, et c'est ce qui se compare
//! au budget de 16,7 ms.
//!
//! ## Le code est un doublon assumé de `mail-spike-native`
//!
//! Les deux sondes ne peuvent pas partager une ligne : l'une vit sur egui 0.36 par `eframe`,
//! l'autre sur egui 0.34 par `egui_software_backend`. Un crate commun devrait choisir une
//! version, ce qui détruirait la comparaison. La duplication est le prix du relevé, et les deux
//! sondes sont jetables.
//!
//! ## Son statut
//!
//! **Jetable.** Elle répond, le chiffre va dans `docs/PHASE-1.md`.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context as _, Result};
use egui_software_backend::{
    App, SoftwareBackend, SoftwareBackendAppConfiguration, run_app_with_software_backend,
};
use mailcore::store::read::ListItem;
use mailcore::{FolderId, Store};

/// Hauteur d'une ligne, en points. La même que le front web et la sonde OpenGL.
const ROW_HEIGHT: f32 = 22.0;

/// Lignes chargées avant la première image.
const FIRST_PAGE: u32 = 100;

/// Lignes par page ensuite, en fond.
const PAGE: u32 = 500;

/// Images observées au repos, puis en défilant. Mêmes nombres que les autres bancs.
const IDLE_FRAMES: usize = 120;
const SCROLL_FRAMES: usize = 600;

/// Ce que la sonde doit faire de sa vie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bench {
    Startup,
    Scroll,
    None,
}

/// Les lignes du dossier, remplies au fur et à mesure par le fil de fond.
#[derive(Debug, Default)]
struct Loaded {
    items: Vec<ListItem>,
    done: bool,
}

/// Où en est le banc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Idle,
    Scrolling,
    Done,
}

/// Une série d'images : leur intervalle, et le travail fait dedans.
#[derive(Debug, Default)]
struct Series {
    deltas: Vec<f64>,
    work: Vec<f64>,
}

impl Series {
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

fn main() -> Result<()> {
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
    eprintln!("MESURE-UI logiciel etape=store ms={:.1}", since());

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

    let first = store.page(folder, None, FIRST_PAGE)?;
    let loaded = Arc::new(Mutex::new(Loaded {
        items: first,
        done: false,
    }));
    spawn_loader(root.clone(), folder, Arc::clone(&loaded));

    // La taille est réglable, et ce n'est pas un détail : la rastérisation logicielle coûte
    // proportionnellement au nombre de pixels. Un relevé en 1280×800 ne dit rien de ce que
    // ferait le même code sur un écran 4K, qui a huit fois plus de pixels à peindre.
    let size = std::env::var("MAILCORE_UI_SIZE").ok().and_then(|it| {
        let (width, height) = it.split_once('x')?;
        Some([
            width.trim().parse::<f32>().ok()?,
            height.trim().parse::<f32>().ok()?,
        ])
    });
    let size = size.unwrap_or([1280.0, 800.0]);
    eprintln!(
        "MESURE-UI logiciel etape=taille largeur={} hauteur={}",
        size[0], size[1]
    );

    let settings = SoftwareBackendAppConfiguration::default()
        .viewport_builder(
            egui::ViewportBuilder::default()
                .with_inner_size(size)
                .with_min_inner_size([600.0, 400.0]),
        )
        .title(Some("mailcore — sonde logicielle".to_owned()));

    run_app_with_software_backend(settings, move |ctx| {
        // Le jalon qui sépare la fenêtre du dessin, comme dans la sonde OpenGL.
        eprintln!("MESURE-UI logiciel etape=contexte ms={:.1}", since());
        dense_theme(&ctx);
        Probe {
            since: Box::new(since),
            bench,
            folders: folders.clone(),
            counts: counts.clone(),
            folder,
            path: path.clone(),
            total,
            loaded: Arc::clone(&loaded),
            announced: false,
            phase: Phase::Waiting,
            offset: 0.0,
            idle: Series::default(),
            scrolling: Series::default(),
            last: None,
            snapshot: Vec::new(),
            done: false,
        }
    })
    .map_err(|error| anyhow::anyhow!("backend logiciel : {error}"))
}

/// Charge le dossier page par page, dans un fil, sans jamais bloquer l'interface.
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

/// Le thème : dense, sans effet.
fn dense_theme(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 0.0);
    });
}

struct Probe {
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
    snapshot: Vec<ListItem>,
    done: bool,
}

impl App for Probe {
    fn ui(&mut self, root: &mut egui::Ui, backend: &mut SoftwareBackend) {
        // Le temps de rastérisation n'est pas relevé par défaut : il faut le demander.
        if !backend.is_capture_frame_time() {
            backend.set_capture_frame_time(true);
        }

        let ctx = root.ctx().clone();
        let now = Instant::now();
        let delta = self
            .last
            .map(|last| now.duration_since(last).as_secs_f64() * 1000.0);
        self.last = Some(now);

        // La rastérisation de l'image **précédente**, rapportée par le backend.
        let raster = backend
            .last_frame_time()
            .map(|it| it.as_secs_f64() * 1000.0)
            .unwrap_or_default();

        // Une copie par page reçue, pas une par image — même raison que dans la sonde OpenGL.
        if let Ok(guard) = self.loaded.try_lock() {
            if guard.items.len() != self.snapshot.len() {
                self.snapshot = guard.items.clone();
            }
            self.done = guard.done;
        }

        let items = std::mem::take(&mut self.snapshot);
        let layout_started = Instant::now();
        self.draw(root, &items);
        let layout = layout_started.elapsed().as_secs_f64() * 1000.0;
        self.snapshot = items;

        if !self.announced {
            self.announced = true;
            eprintln!(
                "MESURE-UI logiciel etape=premiere_image ms={:.1} lignes={} total={}",
                (self.since)(),
                self.snapshot.len(),
                self.total
            );
            if self.bench == Bench::Startup {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }

        if self.bench == Bench::Scroll {
            // Le travail par image : la mise en page de cette image plus la rastérisation de
            // la précédente. C'est l'équivalent du `cpu_usage` d'`eframe`.
            self.advance(&ctx, delta, layout + raster);
        }
    }
}

impl Probe {
    /// Trois panneaux : les dossiers, l'en-tête, la liste virtualisée.
    fn draw(&mut self, root: &mut egui::Ui, items: &[ListItem]) {
        // `show_inside` et non `show` : en egui 0.34, c'est `show` qui est déprécié — la
        // convention s'est inversée en 0.36, que la sonde OpenGL utilise. Deux sondes, deux
        // versions d'egui, deux API : le prix de la comparaison.
        egui::Panel::left("dossiers")
            .exact_size(240.0)
            .show_inside(root, |ui| {
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
                            // La sélection n'est pas branchée : la sonde mesure, elle ne
                            // navigue pas.
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

        egui::Panel::top("entete").show_inside(root, |ui| {
            ui.horizontal(|ui| {
                ui.set_height(28.0);
                ui.strong(&self.path);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(format!("{} chargés sur {}", items.len(), self.total));
                });
            });
        });

        egui::CentralPanel::default().show_inside(root, |ui| {
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
    fn advance(&mut self, ctx: &egui::Context, delta: Option<f64>, work: f64) {
        ctx.request_repaint();

        match self.phase {
            Phase::Waiting => {
                if self.done {
                    eprintln!(
                        "MESURE-UI logiciel diag {} lignes sur {} dans « {} »",
                        self.snapshot.len(),
                        self.total,
                        self.path
                    );
                    self.phase = Phase::Idle;
                }
            }
            Phase::Idle => {
                if let Some(delta) = delta {
                    self.idle.deltas.push(delta);
                    self.idle.work.push(work);
                }
                if self.idle.deltas.len() >= IDLE_FRAMES {
                    self.phase = Phase::Scrolling;
                }
            }
            Phase::Scrolling => {
                if let Some(delta) = delta {
                    self.scrolling.deltas.push(delta);
                    self.scrolling.work.push(work);
                }
                #[allow(clippy::cast_precision_loss)]
                let distance = self.total as f32 * ROW_HEIGHT;
                #[allow(clippy::cast_precision_loss)]
                let step = distance / SCROLL_FRAMES as f32;
                self.offset += step;
                if self.scrolling.deltas.len() >= SCROLL_FRAMES {
                    self.report();
                    self.phase = Phase::Done;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Phase::Done => {}
        }
    }

    /// Écrit le relevé du critère 2, dans le même format que les autres bancs.
    fn report(&self) {
        let p = Series::percentile;
        eprintln!(
            "MESURE-UI logiciel critere=2 lignes={} images={} repos={:.2} p50={:.2} p95={:.2} \
             pire={:.2} travail_repos={:.2} travail_p50={:.2} travail_p95={:.2} \
             travail_pire={:.2}",
            self.snapshot.len(),
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

/// Une date courte, sans dépendance de plus. Grossier : la sonde mesure des images.
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

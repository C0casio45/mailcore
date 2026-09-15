//! Sonde jetable : **le plancher d'une fenêtre sans contexte GPU**.
//!
//! ## La question qu'elle tranche
//!
//! La sonde native mesure 449 ms de « fenêtre et contexte graphique » avant sa première image,
//! et le budget du critère 1 est manqué de 65 à 80 ms. La sonde `wry` a montré qu'un fil
//! d'événements coûte ~125 ms et une fenêtre ~155 ms de plus. Il resterait donc ~170 ms pour la
//! création du contexte OpenGL — et si c'est vrai, un rendu **logiciel** ferait passer le
//! démarrage sous les 400 ms.
//!
//! Le conditionnel est le sujet de cette sonde. Elle crée la même chose qu'`eframe` **moins le
//! contexte GPU** : un fil d'événements `winit`, une fenêtre, une surface `softbuffer` — un
//! simple tampon de pixels en mémoire partagée avec le système — et présente une image. Le
//! jalon final est l'équivalent de `premiere_image` : des pixels à l'écran.
//!
//! Ce que la réponse décide :
//!
//! - si le plancher logiciel est ~300 ms, les ~150 ms de contexte GL sont récupérables et ça
//!   vaut la peine de chercher un rasteriseur CPU pour egui — au prix d'un crate en 0.0.3
//!   (`egui_software_backend`) qui épingle egui à une version antérieure à celle d'eframe ;
//! - s'il est proche de 449 ms, le coût est la **fenêtre**, pas le GPU, et toute cette piste
//!   est sans objet. C'est une demi-journée économisée.
//!
//! `eprintln!` et pas `tracing`, même raison que les autres sondes : on mesure un démarrage.
//!
//! ## Son statut
//!
//! **Jetable.** Elle répond à une question, le chiffre va dans `docs/PHASE-1.md`.

#![forbid(unsafe_code)]

use std::num::NonZeroU32;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// La fenêtre partagée entre la surface et nous.
///
/// `Rc` et non `Arc` : tout vit sur le fil de l'interface, et `softbuffer` demande à posséder
/// une poignée sur la fenêtre en plus de la nôtre.
type Shared = std::rc::Rc<Window>;

/// La surface logicielle, liée à la fenêtre qu'elle présente.
type Surface = softbuffer::Surface<Shared, Shared>;

/// L'état de la sonde, entre la création du fil et la première image.
struct Probe {
    started: Instant,
    /// La fenêtre et sa surface vivent ensemble : la surface emprunte la fenêtre.
    surface: Option<(Shared, Surface)>,
    announced: bool,
}

impl Probe {
    fn since(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }
}

impl ApplicationHandler for Probe {
    fn resumed(&mut self, target: &ActiveEventLoop) {
        // `resumed` est appelé une fois au démarrage sur bureau : c'est là qu'une application
        // winit 0.30 crée sa fenêtre.
        let attributes = Window::default_attributes()
            .with_title("sonde logicielle")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
        let Ok(window) = target.create_window(attributes) else {
            eprintln!("MESURE-UI sonde-logicielle etape=echec_fenetre");
            return;
        };
        let window: Shared = std::rc::Rc::new(window);
        eprintln!(
            "MESURE-UI sonde-logicielle etape=fenetre ms={:.1}",
            self.since()
        );

        // `softbuffer` : un tampon de pixels que le système présente tel quel. Pas de
        // contexte GL, pas de périphérique GPU, pas de pipeline à compiler.
        let Ok(context) = softbuffer::Context::new(std::rc::Rc::clone(&window)) else {
            eprintln!("MESURE-UI sonde-logicielle etape=echec_contexte");
            return;
        };
        let Ok(surface) = softbuffer::Surface::new(&context, std::rc::Rc::clone(&window)) else {
            eprintln!("MESURE-UI sonde-logicielle etape=echec_surface");
            return;
        };
        eprintln!(
            "MESURE-UI sonde-logicielle etape=surface ms={:.1}",
            self.since()
        );

        self.surface = Some((window, surface));
    }

    fn window_event(&mut self, target: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => target.exit(),
            WindowEvent::RedrawRequested => {
                let Some((window, surface)) = self.surface.as_mut() else {
                    return;
                };
                let size = window.inner_size();
                let (Some(width), Some(height)) =
                    (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                else {
                    return;
                };
                if surface.resize(width, height).is_err() {
                    return;
                }
                let Ok(mut buffer) = surface.buffer_mut() else {
                    return;
                };
                // Un fond uni : la sonde mesure le chemin jusqu'au pixel, pas le dessin.
                buffer.fill(0x0018_1818);
                if buffer.present().is_err() {
                    return;
                }

                if !self.announced {
                    self.announced = true;
                    eprintln!(
                        "MESURE-UI sonde-logicielle etape=premiere_image ms={:.1}",
                        self.since()
                    );
                    // La question est posée et répondue.
                    target.exit();
                }
            }
            _ => {}
        }
    }
}

fn main() -> Result<(), winit::error::EventLoopError> {
    let started = Instant::now();

    let event_loop = EventLoop::new()?;
    eprintln!(
        "MESURE-UI sonde-logicielle etape=fil_evenements ms={:.1}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut probe = Probe {
        started,
        surface: None,
        announced: false,
    };
    event_loop.run_app(&mut probe)
}

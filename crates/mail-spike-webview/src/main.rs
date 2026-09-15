//! Sonde jetable : **combien coûte un webview, sans rien autour**.
//!
//! ## La question qu'elle tranche
//!
//! Le relevé du critère 1 du 2026-09-02 donne 800 à 1 390 ms entre le début du processus et le
//! moment où la fenêtre de la coquille Tauri est montée — avant que la page existe. La question
//! qui décide de l'architecture de l'UI est : est-ce que ce temps est **le plancher du moteur
//! de rendu du système**, ou est-ce que c'est ce que Tauri ajoute par-dessus ?
//!
//! Les deux réponses mènent ailleurs. Si c'est Tauri, on cherche l'option de configuration qui
//! l'évite. Si c'est le moteur, aucune configuration n'y changera rien et la seule sortie est
//! de ne pas créer de webview au démarrage — donc une coquille native, avec le moteur
//! réservé au corps des messages et créé paresseusement.
//!
//! ## Comment elle mesure
//!
//! Une fenêtre, un webview, une page en `data:` qui ne fait qu'appeler l'IPC dès qu'elle
//! s'exécute. Aucun store, aucune CSP, aucune capacité, aucun assainissement : ce qui reste est
//! le moteur et le fil d'événements. Les jalons sont écrits sur la sortie d'erreur avec la même
//! clé `MESURE-UI` que la coquille, pour qu'ils se lisent de la même façon.
//!
//! `println!` et `eprintln!` plutôt que `tracing` : cette sonde n'a pas de dépendance au-delà de
//! `wry` et `tao`, et c'est précisément ce qui la rend comparable. Une pile de journalisation
//! serait une chose de plus à charger au démarrage.
//!
//! ## Son statut
//!
//! **Jetable.** Elle répond à une question, la réponse va dans `docs/PHASE-1.md`, et elle
//! disparaît. Elle n'est pas un morceau de l'application et rien ne doit en dépendre.

#![forbid(unsafe_code)]

use std::time::Instant;

use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

/// La page la plus petite qui puisse dire « j'y suis ».
///
/// En `data:` et non dans un fichier : la sonde doit être un seul exécutable, sans ressource à
/// côté dont la lecture entrerait dans la mesure.
const PAGE: &str = "data:text/html,\
    <!doctype html><meta charset=utf-8><title>sonde</title>\
    <script>window.ipc.postMessage('page')</script>";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let since = move || started.elapsed().as_secs_f64() * 1000.0;

    let event_loop = EventLoop::new();
    eprintln!("MESURE-UI sonde etape=fil_evenements ms={:.1}", since());

    let window = WindowBuilder::new()
        .with_title("sonde webview")
        .with_inner_size(tao::dpi::LogicalSize::new(900.0, 600.0))
        .build(&event_loop)?;
    eprintln!("MESURE-UI sonde etape=fenetre ms={:.1}", since());

    let _webview = WebViewBuilder::new()
        .with_url(PAGE)
        .with_ipc_handler(move |request| {
            // Le premier message de la page : c'est l'instant où du code à nous s'exécute
            // dans le moteur. C'est l'équivalent exact du jalon `paint` de la coquille.
            eprintln!(
                "MESURE-UI sonde etape=page ms={:.1} message={}",
                since(),
                request.body()
            );
            // La sonde a répondu à sa question. Rester ouverte n'apprendrait rien de plus, et
            // il faut qu'elle rende la main à l'outillage qui la lance en boucle.
            std::process::exit(0);
        })
        .build(&window)?;
    eprintln!("MESURE-UI sonde etape=webview_construit ms={:.1}", since());

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                eprintln!("MESURE-UI sonde etape=fil_demarre ms={:.1}", since());
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
}

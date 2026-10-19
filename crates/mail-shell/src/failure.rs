//! La fenêtre d'une coquille qui ne démarre pas.
//!
//! **Un service qui ne s'ouvre pas le dit dans une fenêtre**, parce qu'en release il n'y a pas
//! d'autre endroit où le dire : la coquille est du sous-système Windows, donc sans console, et
//! l'erreur que `main` rendait partait sur une sortie que personne ne lit. Un clic dans le menu
//! Démarrer ne faisait **rien du tout** — ni fenêtre, ni message.
//!
//! C'est arrivé le 2026-09-23 : le store avait été passé au schéma 14 par un binaire plus récent,
//! la coquille du raccourci n'en lisait que 13, et elle refusait — à juste titre — sans que rien
//! ne se voie. Un refus juste qu'on ne voit pas ne se distingue pas d'une panne.
//!
//! Ce que la fenêtre dit suit la règle du critère 8 de la phase 3 : **quoi, pourquoi, et quoi
//! faire**, quand on sait quoi faire. Le détail de la chaîne d'erreurs reste affiché et
//! copiable, parce que c'est lui qu'on colle dans un rapport.
//!
//! **Ce qu'elle ne couvre pas**, écrit pour ne pas le croire couvert : une erreur d'arguments
//! (clap sort avant, et `--purge-cache` est lancé par un banc sans personne pour fermer une
//! fenêtre), une panique, et un pilote OpenGL absent — la fenêtre d'erreur passe par le même
//! `glow` que la fenêtre principale, donc elle échouerait de la même façon. Les trois restent
//! sans fenêtre depuis le menu Démarrer, mais plus muets depuis le 2026-10-19 : ils s'écrivent
//! dans le journal sur disque (`crate::journal`) — la panique par son seul emplacement, son
//! message pouvant citer du courrier (`maild::panics`). La page de paramètres dit où il est.

/// Ce que la fenêtre affiche. Calculé une fois, hors du dessin : c'est la partie qui se teste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    /// Ce qui s'est passé, en une phrase.
    pub summary: String,
    /// Le geste qui répare, quand il est connu. `None` plutôt qu'un conseil deviné : un conseil
    /// faux fait chercher au mauvais endroit.
    pub remedy: Option<String>,
    /// La chaîne d'erreurs complète, une cause par ligne.
    pub detail: String,
}

/// Explique une erreur de démarrage.
///
/// La reconnaissance se fait **par le type**, jamais par le texte : chercher « schéma » dans un
/// message confondrait n'importe quelle erreur qui cite un chemin contenant ce mot, et la phrase
/// changera le jour où quelqu'un la reformule.
pub fn explain(error: &anyhow::Error) -> Explanation {
    let detail = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    let schema = error
        .chain()
        .find_map(|cause| match cause.downcast_ref::<mailcore::Error>() {
            Some(mailcore::Error::UnsupportedSchema { found, supported }) => {
                Some((*found, *supported))
            }
            _ => None,
        });

    match schema {
        Some((found, supported)) => Explanation {
            summary: format!(
                "Ce courrier a été rangé par une version plus récente de mailcore \
                 (schéma {found}), et cette version-ci ne sait lire que jusqu'au schéma \
                 {supported}."
            ),
            // **Fermer d'abord**, parce que c'est ce processus qui tient l'exécutable : sous
            // Windows, un `.exe` qui tourne ne se remplace pas, et suivre le conseil fenêtre
            // ouverte donnait « Accès refusé » sans rien qui dise pourquoi. Dire quoi faire
            // oblige à rendre le geste possible.
            //
            // Et la promesse est **la plus étroite qui soit vraie** : aucune migration n'est
            // appliquée, c'est `migrations::apply` qui refuse. « Rien n'a été écrit » serait
            // faux au bord — `Store::open` crée les répertoires et passe le fichier en WAL
            // avant de lire la version.
            remedy: Some(
                "Aucune migration n'a été appliquée : le store a été refusé avant que son \
                 schéma ne soit touché. Fermez cette fenêtre, mettez l'application à jour — \
                 depuis les sources, « cargo build --release -p mail-shell » — puis \
                 relancez-la."
                    .to_owned(),
            ),
            detail,
        },
        // Pas de phrase qui répète le titre : ce qu'on sait est dans le détail, et c'est là
        // qu'il faut regarder.
        None => Explanation {
            summary: "La cause est dans le détail ci-dessous, qui peut être copié tel quel dans \
                      un rapport."
                .to_owned(),
            remedy: None,
            detail,
        },
    }
}

/// Ce qu'un utilisateur a demandé à la fenêtre pendant une image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Rien.
    Idle,
    /// Copier le détail.
    Copy,
    /// Fermer.
    Close,
}

/// Dessine l'explication et rend ce qui a été cliqué.
///
/// Rendre l'action plutôt que l'exécuter est ce qui rend le dessin testable sans fenêtre, et ce
/// qui permet de vérifier qu'une fenêtre que personne n'a cliquée **ne demande rien**.
pub fn draw(ui: &mut egui::Ui, explanation: &Explanation) -> Outcome {
    let mut outcome = Outcome::Idle;
    ui.heading("mailcore ne peut pas démarrer");
    ui.add_space(8.0);
    ui.label(&explanation.summary);
    if let Some(remedy) = &explanation.remedy {
        ui.add_space(4.0);
        ui.label(remedy);
    }
    ui.add_space(8.0);
    ui.label(egui::RichText::new("Détail").strong());
    // Même procédé que la vue source : un champ dont la copie est refaite à chaque image, donc
    // sélectionnable sans être modifiable.
    let mut shown = explanation.detail.clone();
    egui::ScrollArea::vertical()
        .max_height(160.0)
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut shown)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
            );
        });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Copier le détail").clicked() {
            outcome = Outcome::Copy;
        }
        if ui.button("Fermer").clicked() {
            outcome = Outcome::Close;
        }
    });
    outcome
}

/// L'application d'une seule fenêtre qui porte l'explication.
struct Failure {
    explanation: Explanation,
}

impl eframe::App for Failure {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        egui::CentralPanel::default().show(root, |ui| match draw(ui, &self.explanation) {
            Outcome::Idle => {}
            Outcome::Copy => ctx.copy_text(self.explanation.detail.clone()),
            Outcome::Close => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        });
    }
}

/// Montre l'erreur dans une fenêtre et attend qu'elle soit fermée.
///
/// Ne rend rien : l'appelant rend l'erreur d'origine ensuite, pour que le code de sortie reste un
/// échec — un script qui lance la coquille doit le voir. Une fenêtre qui ne s'ouvre pas non plus
/// n'efface pas l'erreur qu'elle devait montrer ; son propre échec part sur la sortie d'erreur,
/// donc ne se voit que depuis un terminal — l'angle mort que la tête du module écrit.
pub fn show(error: &anyhow::Error) {
    tracing::error!("démarrage impossible : {error:#}");
    let explanation = explain(error);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([600.0, 360.0])
            .with_min_inner_size([420.0, 260.0])
            .with_title("mailcore — démarrage impossible"),
        ..Default::default()
    };
    let shown = eframe::run_native(
        "mailcore-failure",
        options,
        Box::new(move |cx| {
            crate::theme::install(&cx.egui_ctx);
            Ok(Box::new(Failure { explanation }))
        }),
    );
    if let Err(window) = shown {
        tracing::error!(%window, "la fenêtre d'erreur n'a pas pu s'ouvrir");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Explanation, Outcome, draw, explain};
    use anyhow::Context;

    /// L'erreur telle que `maild::Service::open` la rend : la cause de `mailcore`, enveloppée
    /// d'un contexte qui nomme le store.
    fn schema_error(found: u32, supported: u32) -> anyhow::Error {
        Err::<(), _>(mailcore::Error::UnsupportedSchema { found, supported })
            .context("ouverture du store C:\\courrier")
            .unwrap_err()
    }

    #[test]
    fn a_store_from_a_newer_binary_names_both_versions_and_the_remedy() {
        let explanation = explain(&schema_error(14, 13));
        assert!(explanation.summary.contains("schéma 14"), "{explanation:?}");
        assert!(
            explanation.summary.contains("jusqu'au schéma 13"),
            "{explanation:?}"
        );
        let remedy = explanation.remedy.expect("un geste connu");
        // Fermer **avant** de recompiler : c'est ce processus qui tient l'exécutable, et Windows
        // refuse de remplacer un `.exe` qui tourne.
        let close = remedy.find("Fermez cette fenêtre").expect("fermer d'abord");
        let build = remedy
            .find("cargo build --release -p mail-shell")
            .expect("la commande");
        assert!(close < build, "{remedy}");
        // Et rien de plus large que ce qui est garanti : le refus précède les migrations, pas
        // toute écriture.
        assert!(!remedy.contains("avant toute écriture"), "{remedy}");
    }

    #[test]
    fn the_detail_keeps_every_cause_one_per_line() {
        // Le chemin du store est dans le contexte, la version dans la cause : les deux servent,
        // et un rapport qui n'aurait que l'un des deux ferait redemander l'autre.
        let explanation = explain(&schema_error(14, 13));
        let lines: Vec<&str> = explanation.detail.lines().collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("ouverture du store C:\\courrier"));
        assert!(lines[1].contains("14") && lines[1].contains("13"));
    }

    #[test]
    fn any_other_error_is_shown_without_an_invented_remedy() {
        let error = anyhow::anyhow!("aucun jeton pour hote dans le trousseau du système")
            .context("ouverture du démon");
        let explanation = explain(&error);
        assert_eq!(explanation.remedy, None);
        assert!(explanation.detail.contains("aucun jeton"));
    }

    #[test]
    fn recognition_is_by_type_not_by_words() {
        // Le contrôle inverse du premier test : un message qui **cite** la phrase du refus de
        // schéma, sans en être un, ne reçoit pas le conseil de recompiler.
        let error = anyhow::anyhow!("version de schéma non gérée : 14 (attendu au plus 13)");
        assert_eq!(explain(&error).remedy, None);
    }

    #[test]
    fn a_window_nobody_clicked_asks_for_nothing() {
        for explanation in [
            explain(&schema_error(14, 13)),
            Explanation {
                summary: String::new(),
                remedy: None,
                detail: String::new(),
            },
        ] {
            let mut outcome = Outcome::Copy;
            egui::__run_test_ui(|ui| outcome = draw(ui, &explanation));
            assert_eq!(outcome, Outcome::Idle, "{explanation:?}");
        }
    }
}

//! Le thème et les polices. **Dense, sobre, sans effet** — `docs/PHASE-1.md`, « Thème ».
//!
//! Un outil de travail, pas une vitrine : ligne de liste à 22 points, une quarantaine de
//! messages visibles sans défiler, des niveaux de gris et un seul accent, la hiérarchie par la
//! graisse et l'espacement plutôt que par la couleur. Clair et sombre, en suivant le réglage du
//! système.
//!
//! ## Les polices, et pourquoi elles se chargent en deux temps
//!
//! egui embarque ses propres polices : du latin et des émoji monochromes. Deux manques pour un
//! client mail réel, et le corpus les chiffre (`cargo xtask corpus-scripts`) :
//!
//! - la police embarquée n'est pas celle du système, et le thème demande « Segoe UI sur
//!   Windows, pas de police web » ;
//! - **10,79 % des messages du corpus ont du CJK ou des émoji dans leur sujet.** Sans police de
//!   repli, un dixième de la liste s'affiche en rectangles.
//!
//! D'où deux temps, et c'est le critère 1 qui l'impose :
//!
//! 1. **avant la première image**, la police d'interface du système — quelques centaines de
//!    kilo-octets, quelques millisecondes ;
//! 2. **après**, sur un fil, la police de repli CJK. `msyh.ttc` de Windows fait plusieurs
//!    dizaines de mégaoctets : la charger sur le chemin de démarrage coûterait plus que le
//!    budget entier. L'interface s'ouvre donc en latin, et les idéogrammes apparaissent une
//!    fraction de seconde plus tard.
//!
//! Ce deuxième temps est un compromis assumé et visible. L'alternative — embarquer une police
//! CJK dans le binaire — ajouterait 15 Mo à l'exécutable pour un dixième des messages, et
//! contredirait « pas de police embarquée ».

use std::sync::Arc;

/// Hauteur d'une ligne de liste, en points. Le thème la fixe, la virtualisation en dépend.
pub const ROW_HEIGHT: f32 = 22.0;

/// Le nom sous lequel la police d'interface est enregistrée.
const UI_FONT: &str = "systeme";

/// Le nom de la police de repli.
const FALLBACK_FONT: &str = "repli";

/// Les polices d'interface du système, par ordre de préférence.
///
/// Des chemins et non une énumération par une bibliothèque : `font-kit` et ses semblables
/// interrogent le catalogue du système, ce qui coûte 50 à 150 ms sur un Windows chargé en
/// polices — un tiers du budget du critère 1 pour trouver un fichier dont on connaît le nom.
const UI_CANDIDATES: &[&str] = &[
    // Windows.
    "C:/Windows/Fonts/segoeui.ttf",
    // macOS. `.ttc` en premier n'aurait pas de sens : les collections ne se chargent pas
    // toutes, d'où le repli sur un `.ttf` simple juste après.
    "/System/Library/Fonts/SFNS.ttf",
    "/Library/Fonts/Arial.ttf",
    // Linux, dans l'ordre de ce qu'on trouve le plus souvent installé.
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
];

/// Les polices de repli couvrant le CJK, par ordre de préférence.
const FALLBACK_CANDIDATES: &[&str] = &[
    // Windows : Meiryo est un `.ttc` comme les autres, mais `msgothic`/`simsun` sont souvent
    // là aussi. On essaie plusieurs formes parce qu'une collection peut être refusée.
    "C:/Windows/Fonts/msyh.ttc",
    "C:/Windows/Fonts/simsun.ttc",
    "C:/Windows/Fonts/meiryo.ttc",
    "C:/Windows/Fonts/YuGothM.ttc",
    // macOS.
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    // Linux.
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
];

/// Lit le premier fichier lisible de la liste.
fn first_readable(candidates: &[&str]) -> Option<(String, Vec<u8>)> {
    candidates.iter().find_map(|path| {
        std::fs::read(path)
            .ok()
            .map(|bytes| ((*path).to_owned(), bytes))
    })
}

/// Installe le thème et la police d'interface. À appeler avant la première image.
pub fn install(ctx: &egui::Context) {
    if let Some((path, bytes)) = first_readable(UI_CANDIDATES) {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            UI_FONT.to_owned(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
        // En tête des proportionnelles : la police du système décide, les polices embarquées
        // d'egui restent derrière et servent de repli pour ce qu'elle ne couvre pas — les
        // émoji, notamment.
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.insert(0, UI_FONT.to_owned());
        }
        ctx.set_fonts(fonts);
        tracing::debug!(police = %path, "police d'interface");
    } else {
        // Aucune police du système trouvée : egui a les siennes, l'interface est lisible.
        // Ce n'est pas une panne, c'est un thème moins fidèle.
        tracing::debug!("aucune police système trouvée, polices d'egui conservées");
    }

    dense(ctx);
}

/// Charge la police de repli sur un fil, puis réveille l'interface.
///
/// À appeler **après** la première image : voir l'en-tête du module.
pub fn spawn_fallback(ctx: &egui::Context) {
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("mailcore-shell-polices".to_owned())
        .spawn(move || {
            let Some((path, bytes)) = first_readable(FALLBACK_CANDIDATES) else {
                tracing::debug!("aucune police CJK trouvée : les idéogrammes seront absents");
                return;
            };

            // `FontDefinitions::default()` puis les deux ajouts : reconstruire la définition
            // complète est le seul moyen d'ajouter une famille après coup, egui ne sachant pas
            // fusionner.
            let mut fonts = egui::FontDefinitions::default();
            if let Some((_, ui)) = first_readable(UI_CANDIDATES) {
                fonts
                    .font_data
                    .insert(UI_FONT.to_owned(), Arc::new(egui::FontData::from_owned(ui)));
            }
            fonts.font_data.insert(
                FALLBACK_FONT.to_owned(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                if fonts.font_data.contains_key(UI_FONT) {
                    family.insert(0, UI_FONT.to_owned());
                }
                // En **dernier** : c'est un repli, il ne doit pas décider du dessin du latin.
                family.push(FALLBACK_FONT.to_owned());
            }

            tracing::debug!(police = %path, "police de repli chargée");
            ctx.set_fonts(fonts);
            ctx.request_repaint();
        });
    if let Err(source) = spawned {
        tracing::debug!(%source, "fil de chargement des polices non démarré");
    }
}

/// L'accent, en clair puis en sombre. Un seul, pour la sélection, le non-lu, le focus et le
/// bouton de l'action principale d'un écran — et **rien d'autre**.
///
/// ## Quatre couleurs, un sens chacune
///
/// L'inventaire du 2026-10-09, avant la refonte, a trouvé l'accent employé pour cinq choses —
/// sélection, non-lu, traceurs neutralisés, refus des paramètres, compte en pause — et la
/// bannière d'état peinte en rouge d'erreur même pour « Signature enregistrée. ». Une couleur
/// qui veut tout dire ne dit plus rien, et un succès en rouge apprend à ne plus lire le rouge
/// le jour où il compte. D'où une couleur par sens : l'accent pour ce qu'on regarde, le rouge
/// pour ce qui a échoué ([`error`]), l'ambre pour ce qui demande un geste ([`warn`]), le vert
/// pour ce qui est fait ([`ok`]). Le reste est en gris.
const ACCENT_LIGHT: egui::Color32 = egui::Color32::from_rgb(0x1B, 0x5E, 0xB8);
const ACCENT_DARK: egui::Color32 = egui::Color32::from_rgb(0x69, 0xA8, 0xFF);

/// Ce qui a échoué.
const ERROR_LIGHT: egui::Color32 = egui::Color32::from_rgb(0xB3, 0x26, 0x1E);
const ERROR_DARK: egui::Color32 = egui::Color32::from_rgb(0xFF, 0x8A, 0x80);

/// Ce qui demande un geste : un doute à trancher, un modèle à installer, un service injoignable.
const WARN_LIGHT: egui::Color32 = egui::Color32::from_rgb(0x8A, 0x5A, 0x00);
const WARN_DARK: egui::Color32 = egui::Color32::from_rgb(0xE0, 0xB0, 0x4C);

/// Ce qui est fait.
const OK_LIGHT: egui::Color32 = egui::Color32::from_rgb(0x2E, 0x6B, 0x3A);
const OK_DARK: egui::Color32 = egui::Color32::from_rgb(0x81, 0xC9, 0x95);

/// La densité et les couleurs, pour les deux thèmes.
fn dense(ctx: &egui::Context) {
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        ctx.style_mut_of(theme, |style| {
            // Densité avant respiration : l'espacement vertical entre lignes est nul, la
            // hauteur vient de la ligne elle-même.
            style.spacing.item_spacing = egui::vec2(8.0, 0.0);
            style.spacing.button_padding = egui::vec2(6.0, 2.0);
            style.spacing.menu_margin = egui::Margin::same(4);
            style.spacing.scroll.bar_width = 10.0;
            style.spacing.interact_size.y = ROW_HEIGHT;

            let dark = theme == egui::Theme::Dark;
            let accent = if dark { ACCENT_DARK } else { ACCENT_LIGHT };
            style.visuals.selection.bg_fill = accent.gamma_multiply(0.35);
            style.visuals.selection.stroke = egui::Stroke::new(1.0, accent);
            style.visuals.hyperlink_color = accent;
            // Les couleurs d'egui pour l'erreur et l'avertissement sont **les nôtres** : une
            // partie du code les lit par `visuals()`, et deux rouges voisins à l'écran — celui
            // d'egui, celui du thème — seraient lus comme deux sens différents.
            style.visuals.error_fg_color = if dark { ERROR_DARK } else { ERROR_LIGHT };
            style.visuals.warn_fg_color = if dark { WARN_DARK } else { WARN_LIGHT };

            // Pas de coin arrondi décoratif, pas d'ombre portée : bordures 1 px entre
            // panneaux, et rien d'autre. **Les coins n'avaient jamais été mis à zéro** — seules
            // les ombres l'étaient —, donc chaque bouton et chaque fenêtre gardait l'arrondi
            // d'egui, contre ce que l'en-tête de ce module affirmait depuis la phase 1.
            style.visuals.window_shadow = egui::epaint::Shadow::NONE;
            style.visuals.popup_shadow = egui::epaint::Shadow::NONE;
            style.visuals.window_corner_radius = egui::CornerRadius::ZERO;
            style.visuals.menu_corner_radius = egui::CornerRadius::ZERO;
            for widget in [
                &mut style.visuals.widgets.noninteractive,
                &mut style.visuals.widgets.inactive,
                &mut style.visuals.widgets.hovered,
                &mut style.visuals.widgets.active,
                &mut style.visuals.widgets.open,
            ] {
                widget.corner_radius = egui::CornerRadius::ZERO;
            }
        });
    }
}

/// Une couleur du thème courant, choisie entre sa variante claire et sa variante sombre.
fn pick(ctx: &egui::Context, light: egui::Color32, dark: egui::Color32) -> egui::Color32 {
    if ctx.theme() == egui::Theme::Dark {
        dark
    } else {
        light
    }
}

/// La couleur d'accent du thème courant : sélection, non-lu, focus, action principale.
pub fn accent(ctx: &egui::Context) -> egui::Color32 {
    pick(ctx, ACCENT_LIGHT, ACCENT_DARK)
}

/// Ce qui a échoué.
pub fn error(ctx: &egui::Context) -> egui::Color32 {
    pick(ctx, ERROR_LIGHT, ERROR_DARK)
}

/// Ce qui demande un geste.
pub fn warn(ctx: &egui::Context) -> egui::Color32 {
    pick(ctx, WARN_LIGHT, WARN_DARK)
}

/// Ce qui est fait.
pub fn ok(ctx: &egui::Context) -> egui::Color32 {
    pick(ctx, OK_LIGHT, OK_DARK)
}

/// Ce qu'une ligne d'état veut dire, et donc sa couleur.
///
/// Un `enum` plutôt qu'une couleur passée par l'appelant : la question à se poser en écrivant
/// une phrase est « qu'est-ce que c'est ? », pas « de quelle couleur ? ». Celle qui posait la
/// seconde a peint un succès en rouge pendant un mois.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// C'est fait.
    Done,
    /// Ça a échoué.
    Failed,
    /// Ça demande un geste.
    Attention,
    /// Pour information : ni réussite ni échec.
    Info,
}

impl Tone {
    /// La couleur de ce sens, dans le thème courant.
    pub fn color(self, ctx: &egui::Context) -> egui::Color32 {
        match self {
            Self::Done => ok(ctx),
            Self::Failed => error(ctx),
            Self::Attention => warn(ctx),
            Self::Info => ctx.global_style().visuals.weak_text_color(),
        }
    }
}

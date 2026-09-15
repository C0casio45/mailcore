//! Sonde jetable : **un éditeur riche dans `egui`, mesuré avant d'être écrit pour de bon**.
//!
//! ## La question qu'elle tranche
//!
//! Le critère 5 de `docs/PHASE-3.md` demande **moins de 16,7 ms par image en p95** sur un
//! document de 200 lignes avec gras, italique, liens et listes — et qu'un collage de 50 Ko de
//! HTML ne fige pas l'interface.
//!
//! L'ordre de travail met cette étape en dernier des morceaux d'interface pour une raison
//! précise : *si un éditeur riche performant n'est pas atteignable dans `egui`, il vaut mieux le
//! découvrir avec le reste de la phase déjà livré.* Cette sonde est le moyen de le découvrir sans
//! écrire l'éditeur.
//!
//! ## Ce qu'elle mesure, et pourquoi ce sont les bons chiffres
//!
//! Trois régimes, et seul le deuxième décide :
//!
//! - **au repos** — le document est affiché, rien ne bouge. `egui` met en cache la mise en page
//!   d'un `LayoutJob` par son empreinte, donc ce régime doit être quasi gratuit. S'il ne l'est
//!   pas, c'est que la sonde reconstruit un job différent à chaque image, et le relevé de frappe
//!   ne voudrait rien dire ;
//! - **en frappe** — un caractère inséré par image. C'est **le pire cas réel** : le texte change,
//!   donc l'empreinte change, donc `egui` remet en page les 200 lignes. C'est ce régime que le
//!   critère borne ;
//! - **au collage** — 50 Ko de HTML convertis en une fois. Une image longue est acceptable ; une
//!   interface qui ne répond plus pendant une seconde ne l'est pas.
//!
//! Le contrôle qui rend le relevé interprétable : le nombre de **glyphes** mis en page. Un p95
//! flatteur obtenu sur un document de trois lignes serait indiscernable d'un vrai.
//!
//! ## Le modèle de document, et pourquoi il est aussi simple
//!
//! Un `String` plus une liste d'intervalles stylés. Pas de rope, pas de piece table : une
//! signature fait quelques centaines de lignes, et le coût d'une insertion est le décalage des
//! intervalles qui suivent — quelques dizaines d'entiers. Ce qui coûte, c'est la **mise en
//! page**, et aucune structure de document ne l'évite.
//!
//! C'est aussi ce qui rend la sonde honnête : si le modèle simple tient le critère, le modèle
//! compliqué n'a pas de raison d'être.
//!
//! ## Son statut
//!
//! **Jetable.** Elle répond, le chiffre va dans `docs/PHASE-3.md`, elle disparaît — ou elle
//! devient le point de départ de l'éditeur si la décision va dans ce sens.

#![forbid(unsafe_code)]

use std::time::Instant;

/// Combien de lignes le document de mesure porte. Le critère en demande 200.
///
/// Réglable par `MAILCORE_SPIKE_LINES`, et ce n'est pas du confort : le premier relevé a donné
/// **le même coût au repos et en frappe**, ce qui a deux explications contradictoires — la mise
/// en page ne se refait pas, ou elle est si peu chère que l'écart est du bruit. Les distinguer
/// demande de faire varier la taille : si le coût suit, la mise en page a bien lieu.
fn lines() -> usize {
    std::env::var("MAILCORE_SPIKE_LINES")
        .ok()
        .and_then(|it| it.parse().ok())
        .unwrap_or(200)
}

/// Combien d'images mesurer dans chaque régime.
///
/// 240 : quatre secondes à 60 Hz, assez pour qu'un p95 porte sur des dizaines d'images et pas
/// sur trois.
const FRAMES: usize = 240;

/// Le budget du critère 5 : une image à 60 Hz.
const BUDGET_MS: f64 = 16.7;

/// Ce qu'un intervalle de texte porte comme style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// Rien de particulier.
    Plain,
    /// Gras.
    Strong,
    /// Italique.
    Emphasis,
    /// Un lien. La cible n'est pas dans la sonde : ce qui coûte est le style, pas l'URL.
    Link,
    /// Une puce de liste. Le style diffère du texte courant par son indentation.
    Bullet,
}

/// Un intervalle stylé du document.
#[derive(Debug, Clone, Copy)]
struct Span {
    /// Décalage de début, en octets.
    at: usize,
    /// Longueur, en octets.
    len: usize,
    /// Ce qu'il porte.
    style: Style,
}

/// Le document : le texte, et ce qui le décore.
///
/// ## Les intervalles sont triés et disjoints
///
/// C'est l'invariant qui rend la construction du `LayoutJob` linéaire : un seul parcours, sans
/// recherche. Le maintenir à l'insertion coûte le décalage des intervalles qui suivent le
/// curseur, ce qui est le seul travail qu'une insertion demande en dehors de la mise en page.
#[derive(Debug, Default)]
struct Document {
    text: String,
    spans: Vec<Span>,
}

impl Document {
    /// Un document de mesure : `lines` lignes mêlant les cinq styles.
    ///
    /// La répartition imite une signature riche réelle — du texte courant, des mots en gras, des
    /// liens, une liste — plutôt qu'un document uniforme. Un document tout en gras aurait un
    /// seul intervalle et mesurerait la mise en page sans mesurer le découpage.
    fn generated(lines: usize) -> Self {
        let mut it = Self::default();
        for line in 0..lines {
            match line % 5 {
                0 => it.push("Cordialement, ", Style::Plain),
                1 => {
                    it.push("Marie Dupont", Style::Strong);
                    it.push(" — ", Style::Plain);
                    it.push("directrice technique", Style::Emphasis);
                }
                2 => {
                    it.push("  • ", Style::Bullet);
                    it.push(
                        "un élément de liste avec un peu de texte courant",
                        Style::Plain,
                    );
                }
                3 => {
                    it.push("https://exemple.fr/une/page/assez/longue", Style::Link);
                }
                _ => it.push(
                    "Une ligne de texte courant, assez longue pour que la mise en page ait \
                     quelque chose à faire dessus.",
                    Style::Plain,
                ),
            }
            it.push("\n", Style::Plain);
        }
        it
    }

    /// Ajoute un fragment stylé à la fin.
    fn push(&mut self, fragment: &str, style: Style) {
        self.spans.push(Span {
            at: self.text.len(),
            len: fragment.len(),
            style,
        });
        self.text.push_str(fragment);
    }

    /// Insère un caractère au milieu du document, et décale ce qui suit.
    ///
    /// **Au milieu et non à la fin** : une insertion en fin de texte ne décale aucun intervalle,
    /// donc elle mesurerait le meilleur cas. Un utilisateur qui corrige une signature tape au
    /// milieu.
    fn type_one(&mut self, glyph: char) {
        let at = self.middle();
        self.text.insert(at, glyph);
        let grew = glyph.len_utf8();
        for span in &mut self.spans {
            if span.at >= at {
                span.at += grew;
            } else if span.at + span.len > at {
                // L'insertion tombe **dans** cet intervalle : il grandit, il ne se décale pas.
                span.len += grew;
            }
        }
    }

    /// Une frontière de caractère au milieu du texte.
    ///
    /// Sur une frontière, sinon `insert` panique. Un document dont le milieu tombe au milieu
    /// d'un caractère accentué existe — le document de mesure en a.
    fn middle(&self) -> usize {
        let mut at = self.text.len() / 2;
        while at > 0 && !self.text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    /// Construit la mise en page à partir du texte et des intervalles.
    ///
    /// ## C'est la fonction dont le coût décide de tout
    ///
    /// Elle tourne à **chaque image**, et `egui` met en cache la galère qu'elle décrit par
    /// l'empreinte du `LayoutJob`. Deux conséquences :
    ///
    /// - au repos, elle doit produire un job **identique** à celui de l'image précédente, sinon
    ///   le cache ne sert jamais et la mise en page se refait soixante fois par seconde ;
    /// - en frappe, le job change forcément, donc la mise en page se refait. C'est ce coût-là
    ///   que le critère borne, et aucune astuce de modèle ne l'évite.
    fn layout(&self, wrap_width: f32, fonts: &egui::FontDefinitions) -> egui::text::LayoutJob {
        let _ = fonts;
        let mut job = egui::text::LayoutJob {
            wrap: egui::text::TextWrapping {
                max_width: wrap_width,
                ..Default::default()
            },
            ..Default::default()
        };
        for span in &self.spans {
            let Some(slice) = self.text.get(span.at..span.at + span.len) else {
                continue;
            };
            job.append(slice, 0.0, format_for(span.style));
        }
        job
    }

    /// Le nombre de glyphes du document — le contrôle du relevé.
    fn glyphs(&self) -> usize {
        self.text.chars().count()
    }
}

/// Le format `egui` d'un style.
fn format_for(style: Style) -> egui::TextFormat {
    let mut format = egui::TextFormat {
        font_id: egui::FontId::proportional(14.0),
        ..Default::default()
    };
    match style {
        Style::Plain => {}
        Style::Strong => {
            format.color = egui::Color32::WHITE;
            // Pas de police grasse séparée dans la sonde : ce qui coûte en mise en page est le
            // **changement de format**, qui coupe la section, pas le dessin du glyphe. Une
            // vraie police grasse changerait les largeurs, pas l'ordre de grandeur.
            format.font_id = egui::FontId::proportional(15.0);
        }
        Style::Emphasis => {
            format.italics = true;
        }
        Style::Link => {
            format.color = egui::Color32::from_rgb(0x5A, 0x9C, 0xF0);
            format.underline = egui::Stroke::new(1.0, egui::Color32::from_rgb(0x5A, 0x9C, 0xF0));
        }
        Style::Bullet => {
            format.color = egui::Color32::GRAY;
        }
    }
    format
}

/// Où en est la sonde.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Les premières images, jetées : la première mise en page paie le chargement des polices.
    Warmup,
    /// Le document affiché, rien ne bouge.
    Idle,
    /// Un caractère inséré par image.
    Typing,
    /// La même chose sur un document **quatre fois plus gros**.
    ///
    /// C'est le contrôle de la sonde, et il a remplacé un mauvais. Le premier comparait le
    /// repos à la frappe, en supposant que la mise en page domine l'image : elle ne domine pas
    /// — elle coûte une fraction de milliseconde, et l'écart se noie dans le reste. Le contrôle
    /// disait donc « ? » sur un relevé parfaitement bon.
    ///
    /// Ce qui distingue vraiment « la mise en page a lieu » de « elle est sautée » est que le
    /// coût **suive la taille du document**. Un cache jamais invalidé donnerait le même chiffre
    /// aux deux tailles.
    TypingBig,
    /// Le collage de 50 Ko, une fois.
    Pasting,
    /// Fini : les chiffres sont sortis.
    Done,
}

/// L'état de la sonde.
struct Probe {
    document: Document,
    phase: Phase,
    frames: usize,
    idle: Vec<f64>,
    typing: Vec<f64>,
    /// Le même régime, sur un document quadruple. Voir [`Phase::TypingBig`].
    typing_big: Vec<f64>,
    /// Les glyphes du document quadruple.
    glyphs_big: usize,
    paste_ms: f64,
    paste_glyphs: usize,
    /// Le nombre de glyphes du document **mesuré**, relevé à la construction.
    ///
    /// Et pas lu à la fin : la phase de collage remplace le document, donc le compter au
    /// moment du bilan rapportait la taille du collage — 22 295 glyphes quelle que soit la
    /// taille demandée. Le contrôle qui rend le relevé interprétable était donc faux, et il
    /// l'était en silence.
    glyphs: usize,
    started: Instant,
}

impl Probe {
    fn new() -> Self {
        let document = Document::generated(lines());
        Self {
            glyphs: document.glyphs(),
            document,
            phase: Phase::Warmup,
            frames: 0,
            idle: Vec::with_capacity(FRAMES),
            typing: Vec::with_capacity(FRAMES),
            typing_big: Vec::with_capacity(FRAMES),
            glyphs_big: 0,
            paste_ms: 0.0,
            paste_glyphs: 0,
            started: Instant::now(),
        }
    }
}

impl eframe::App for Probe {
    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        // `cpu_usage` est le temps de l'image **précédente**, en secondes. C'est la mesure qui
        // compte : le temps qu'`egui` a passé, pas le temps entre deux réveils — celui-là est
        // borné par le compositeur et ne dirait rien du travail.
        let work = frame.info().cpu_usage.map(|it| f64::from(it) * 1000.0);

        // La sonde redessine sans arrêt : sans ça, `egui` attendrait un événement et le régime
        // au repos ne serait jamais mesuré.
        ctx.request_repaint();

        // Le `Ui` racine que le harnais donne : dessiner dedans plutôt que d'ouvrir un panneau
        // central évite un niveau d'imbrication qui n'apprend rien.
        {
            let ui = root;
            let width = ui.available_width();
            let mut text = self.document.text.clone();
            let document = &self.document;
            let mut layouter = |ui: &egui::Ui, _: &dyn egui::TextBuffer, wrap: f32| {
                let _ = ui;
                let job = document.layout(wrap.min(width), &egui::FontDefinitions::default());
                // `layout_job` prend le cache par mutation : c'est **lui** qui met en cache la
                // galère, et c'est tout le mécanisme dont le régime « au repos » dépend.
                ui.ctx().fonts_mut(|fonts| fonts.layout_job(job))
            };
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .desired_width(f32::INFINITY)
                        .layouter(&mut layouter),
                );
            });
        }

        self.frames += 1;
        match self.phase {
            // Vingt images de chauffe : la première mise en page paie le chargement des polices
            // et le remplissage de l'atlas de texture. Les mesurer ferait passer un coût de
            // démarrage pour une latence de frappe.
            Phase::Warmup if self.frames > 20 => {
                self.phase = Phase::Idle;
                self.frames = 0;
            }
            Phase::Idle => {
                if let Some(ms) = work {
                    self.idle.push(ms);
                }
                if self.frames > FRAMES {
                    self.phase = Phase::Typing;
                    self.frames = 0;
                }
            }
            Phase::Typing => {
                if let Some(ms) = work {
                    self.typing.push(ms);
                }
                // Un caractère par image. C'est plus rapide qu'un humain, et c'est voulu : le
                // critère borne le coût **d'une** frappe, et le mesurer à cadence maximale
                // évite qu'un cache d'`egui` se vide entre deux.
                self.document.type_one('é');
                if self.frames > FRAMES {
                    // Le document quadruple, et le compteur de glyphes avec : c'est la
                    // comparaison qui prouve que la mise en page a lieu.
                    self.document = Document::generated(lines() * 4);
                    self.glyphs_big = self.document.glyphs();
                    self.phase = Phase::TypingBig;
                    self.frames = 0;
                }
            }
            Phase::TypingBig => {
                if let Some(ms) = work {
                    self.typing_big.push(ms);
                }
                self.document.type_one('é');
                if self.frames > FRAMES {
                    self.phase = Phase::Pasting;
                    self.frames = 0;
                }
            }
            Phase::Pasting => {
                let started = Instant::now();
                let pasted = paste_html(&big_html());
                self.paste_glyphs = pasted.glyphs();
                self.document = pasted;
                self.paste_ms = started.elapsed().as_secs_f64() * 1000.0;
                self.phase = Phase::Done;
            }
            Phase::Done => {
                self.report(&ctx);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Phase::Warmup => {}
        }
    }
}

impl Probe {
    /// Sort les chiffres, et le verdict.
    fn report(&self, ctx: &egui::Context) {
        let p = |samples: &[f64], quantile: f64| -> f64 {
            if samples.is_empty() {
                return 0.0;
            }
            let mut sorted = samples.to_vec();
            sorted.sort_by(f64::total_cmp);
            let index = ((sorted.len() as f64 - 1.0) * quantile).round() as usize;
            sorted[index.min(sorted.len() - 1)]
        };

        eprintln!(
            "MESURE-UI richtext etape=document lignes={} glyphes={}",
            lines(),
            self.glyphs
        );
        eprintln!(
            "MESURE-UI richtext regime=repos images={} p50={:.2} p95={:.2} max={:.2}",
            self.idle.len(),
            p(&self.idle, 0.50),
            p(&self.idle, 0.95),
            p(&self.idle, 1.00),
        );
        eprintln!(
            "MESURE-UI richtext regime=frappe images={} p50={:.2} p95={:.2} max={:.2}",
            self.typing.len(),
            p(&self.typing, 0.50),
            p(&self.typing, 0.95),
            p(&self.typing, 1.00),
        );
        eprintln!(
            "MESURE-UI richtext regime=frappe-quadruple images={} glyphes={} p50={:.2} \
             p95={:.2} max={:.2}",
            self.typing_big.len(),
            self.glyphs_big,
            p(&self.typing_big, 0.50),
            p(&self.typing_big, 0.95),
            p(&self.typing_big, 1.00),
        );

        eprintln!(
            "MESURE-UI richtext regime=collage ms={:.1} glyphes={}",
            self.paste_ms, self.paste_glyphs,
        );

        let typing_p95 = p(&self.typing, 0.95);
        eprintln!();
        eprintln!(
            "{} critère 5 — frappe p95 : {typing_p95:.2} ms sur {} glyphes (seuil < {BUDGET_MS} ms)",
            if typing_p95 < BUDGET_MS {
                "OK   "
            } else {
                "ÉCHEC"
            },
            self.glyphs
        );
        // **Le contrôle, mesuré directement.** Voir `measure_layout` : à ces échelles le relevé
        // par image ne distingue plus les régimes, et un cache jamais invalidé donnerait un p95
        // excellent qui ne mesurerait rien. La mise en page doit coûter proportionnellement à la
        // taille du document.
        let (small_glyphs, small_ms) = measure_layout(ctx, lines(), 200);
        let (big_glyphs, big_ms) = measure_layout(ctx, lines() * 4, 200);
        eprintln!(
            "MESURE-UI richtext mise-en-page glyphes={small_glyphs} ms={small_ms:.3} | \
             glyphes={big_glyphs} ms={big_ms:.3}"
        );
        eprintln!(
            "{} contrôle — quadrupler le document quadruple la mise en page : {:.1}× pour \
             {:.1}× de glyphes",
            if big_ms > small_ms * 2.0 {
                "OK   "
            } else {
                "ÉCHEC"
            },
            big_ms / small_ms.max(f64::MIN_POSITIVE),
            big_glyphs as f64 / small_glyphs.max(1) as f64,
        );
        eprintln!(
            "       la mise en page seule coûte {small_ms:.3} ms : c'est ce qui explique que \
             repos et frappe soient du même ordre — le reste de l'image domine."
        );
        eprintln!(
            "       repos p95 {:.2} ms — du même ordre que la frappe, ce qui est **attendu** : \
             la mise en page coûte une fraction de l'image.",
            p(&self.idle, 0.95)
        );
        eprintln!(
            "MESURE-UI richtext etape=fin ms={:.1}",
            self.started.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// Mesure la mise en page **directement**, hors du cycle d'image.
///
/// ## Pourquoi il a fallu ça
///
/// Le relevé par `cpu_usage` donne des images entières **sous la milliseconde** — trente à
/// soixante-dix fois sous le budget. À cette échelle, le bruit d'ordonnancement du système
/// domine le travail : deux exécutions de la même sonde ont donné 0,22 ms et 0,50 ms en p95, soit
/// 2,3× d'écart. Le `CLAUDE.md` dit qu'un tel écart n'est pas du bruit mais un symptôme, et le
/// symptôme est ici que **la mesure ne distingue plus les régimes** : le contrôle de mise à
/// l'échelle a passé une fois et échoué la suivante, sur le même code.
///
/// Ce que cette fonction mesure, elle, est le seul travail qui dépende de la taille du
/// document : construire le `LayoutJob` et le mettre en page. Pas de dessin, pas de
/// tessellation, pas de présentation — donc pas de bruit d'ordonnancement dedans.
///
/// ## Le texte change à chaque itération
///
/// Sinon `egui` rend la galère du cache et la mesure vaudrait zéro. Une insertion par itération,
/// comme une frappe.
fn measure_layout(ctx: &egui::Context, lines: usize, iterations: usize) -> (usize, f64) {
    let mut document = Document::generated(lines);
    let glyphs = document.glyphs();
    let started = Instant::now();
    for _ in 0..iterations {
        document.type_one('é');
        let job = document.layout(800.0, &egui::FontDefinitions::default());
        let galley = ctx.fonts_mut(|fonts| fonts.layout_job(job));
        // `black_box` n'existe pas ici sans `std::hint` : lire une propriété de la galère
        // suffit à empêcher l'élision, et c'est plus lisible qu'une incantation.
        assert!(galley.rect.height() >= 0.0);
    }
    let per = started.elapsed().as_secs_f64() * 1000.0 / iterations as f64;
    (glyphs, per)
}

/// 50 Ko de HTML, de la forme qu'un navigateur met dans le presse-papier.
///
/// Des balises imbriquées, des attributs, des entités : ce qui coûte à convertir, pas un
/// paragraphe propre.
fn big_html() -> String {
    let mut out = String::with_capacity(52 * 1024);
    out.push_str("<div>");
    while out.len() < 50 * 1024 {
        out.push_str(
            "<p style=\"margin:0;font-family:Arial\">Un <b>paragraphe</b> avec de l'<i>italique</i>, \
             un <a href=\"https://exemple.fr/page?x=1&amp;y=2\">lien</a> et une entit&eacute;.</p>\
             <ul><li>premier</li><li>second</li></ul>",
        );
    }
    out.push_str("</div>");
    out
}

/// Convertit du HTML en document stylé.
///
/// ## Ce qu'elle réutilise, et pourquoi c'est la bonne nouvelle
///
/// `mailhtml::blocks` est déjà ce que la coquille emploie pour **afficher** un corps de
/// message : des blocs typés, sans moteur de rendu. Et il rend des `Run` — des fragments avec
/// leur style — donc **les styles en ligne survivent** : un mot en gras au milieu d'un
/// paragraphe arrive en gras.
///
/// C'était la question ouverte de cette sonde, et la réponse est meilleure que prévu : le
/// convertisseur de collage n'a pas à être écrit, il existe. Un convertisseur écrit exprès
/// serait une deuxième implémentation du même découpage — et c'est celle qui recevrait moins de
/// tests que celle qui affiche le courrier reçu.
///
/// ## Ce qui n'est pas préservé
///
/// La taille et la police d'origine, les couleurs, les marges. `blocks` les jette exprès :
/// `docs/PRIVACY.md` veut un corps de message rendu sans moteur, et une signature qui
/// recopierait le CSS d'un site serait aussi une signature qui recopierait ses polices
/// distantes. C'est une limite **voulue**, pas une lacune.
fn paste_html(html: &str) -> Document {
    let mut out = Document::default();
    for block in mailhtml::blocks::blocks(html) {
        // Le préfixe du bloc porte sa nature : c'est ce que la mise en page fait d'un titre ou
        // d'une puce, sans avoir à modéliser un arbre.
        match &block.kind {
            mailhtml::blocks::Kind::Item { depth, .. } => {
                out.push(&"  ".repeat(usize::from(*depth) + 1), Style::Bullet);
                out.push("• ", Style::Bullet);
            }
            mailhtml::blocks::Kind::Quote(depth) => {
                out.push(&"> ".repeat(usize::from(*depth).max(1)), Style::Emphasis);
            }
            _ => {}
        }
        for run in &block.runs {
            // Les styles en ligne, tels que le sanitizer les a laissés. L'ordre compte : un
            // lien gras est un lien, parce que c'est sa cible qui décide de ce que l'utilisateur
            // doit voir.
            let style = if run.style.link.is_some() {
                Style::Link
            } else if run.style.bold || matches!(block.kind, mailhtml::blocks::Kind::Heading(_)) {
                Style::Strong
            } else if run.style.italic {
                Style::Emphasis
            } else {
                Style::Plain
            };
            out.push(&run.text, style);
        }
        if !block.runs.is_empty() {
            out.push("\n", Style::Plain);
        }
    }
    out
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "sonde éditeur riche",
        options,
        Box::new(|_| Ok(Box::new(Probe::new()))),
    )
}

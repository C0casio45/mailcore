//! L'éditeur de signature : une sélection, quatre boutons de style, et rien qui bloque.
//!
//! ## Ce qu'il n'est pas
//!
//! Ce n'est pas un traitement de texte, et ce n'est pas non plus le corps d'un message. C'est
//! l'éditeur du seul morceau de courrier qu'on écrit **une fois pour toutes** : une signature.
//! `docs/PHASE-3.md`, critère 5, borne son coût par image, et la sonde `mail-spike-richtext` a
//! répondu à la question de faisabilité — quatorze microsecondes de mise en page pour 9 800
//! glyphes. Ce qui restait, et qui est ici, est le travail d'interface : sélectionner, styler,
//! ranger.
//!
//! ## Le modèle vit dans `mailhtml`, pas ici
//!
//! [`mailhtml::rich::Document`] porte le texte, les intervalles stylés et la nature des lignes ;
//! il sait se relire depuis du HTML collé et s'écrire en HTML pour un message. Cet éditeur n'en
//! est qu'une **vue** : il n'a pas de modèle à lui, et les deux traductions qui comptent — celle
//! du collage et celle de l'envoi — sont testées sans fenêtre.
//!
//! ## Comment un `TextEdit` d'`egui` peut éditer un document stylé
//!
//! `egui` n'a pas d'éditeur riche : il a un champ de texte qui édite une `String`, plus un
//! **layouter** qui décide comment cette `String` est mise en page. C'est la porte par laquelle
//! passe tout ce qui suit :
//!
//! - le champ édite `buffer`, une `String` ordinaire — donc curseur, sélection, glisser,
//!   annuler, coller et raccourcis du système marchent sans qu'on écrive une ligne ;
//! - à chaque image, [`mailhtml::rich::Document::reconcile`] rattrape le document sur le
//!   tampon, en gardant les styles de ce qui n'a pas changé ;
//! - le layouter construit une section par intervalle stylé, ce qui donne le gras et l'italique
//!   à l'écran.
//!
//! Le prix de ce montage est écrit sur [`Editor::sections`] : pendant l'image où une touche
//! est frappée, le layouter voit le tampon **un caractère en avance** sur le document. La borne
//! est là pour que ce décalage soit invisible plutôt que fatal.

use mailhtml::rich::{Block, Document, Style};

/// De combien une ligne à puce est décalée, en points.
///
/// Une puce ne peut pas être **écrite** par un layouter : la galère qu'il rend doit porter
/// exactement le texte qu'on lui donne, donc y ajouter « • » ferait un texte que le curseur ne
/// saurait plus parcourir. L'indentation est ce qu'un layouter peut dire, et elle suffit à
/// distinguer une puce d'un paragraphe pendant qu'on écrit — l'aperçu, lui, montre la puce.
const BULLET_INDENT: f32 = 14.0;

/// Ce que l'utilisateur a demandé pendant cette image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Rien de décisif : il écrit.
    Idle,
    /// Enregistrer, et fermer.
    Save,
    /// Fermer sans enregistrer.
    Cancel,
}

/// Lequel des styles de caractère un bouton bascule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Toggle {
    Bold,
    Italic,
}

/// L'éditeur ouvert sur la signature d'un compte.
#[derive(Debug)]
pub struct Editor {
    /// Le compte dont c'est la signature. Une signature est par compte : celle d'une adresse
    /// professionnelle sur un message personnel serait une fuite de contexte.
    pub account: i64,
    /// Vrai pendant que l'enregistrement est en vol.
    pub saving: bool,
    /// Le document, seule source de vérité des styles.
    document: Document,
    /// Ce que le champ de texte édite.
    buffer: String,
    /// La sélection courante, en **octets**, telle que la dernière image l'a vue.
    selection: Option<(usize, usize)>,
    /// La cible du prochain lien, telle que tapée.
    target: String,
}

impl Editor {
    /// Ouvre l'éditeur sur la signature d'un compte, ou sur une page blanche.
    #[must_use]
    pub fn new(account: i64, signature: Option<Document>) -> Self {
        let document = signature.unwrap_or_default();
        Self {
            account,
            saving: false,
            buffer: document.text.clone(),
            document,
            selection: None,
            target: String::new(),
        }
    }

    /// Le document tel qu'il est, pour l'enregistrer ou l'afficher ailleurs.
    #[must_use]
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// Une signature de mesure : `lines` lignes mêlant les quatre styles.
    ///
    /// ## Pourquoi la forme du document fait partie du banc
    ///
    /// Le critère 5 demande « un document de 200 lignes avec gras, italique, liens et listes ».
    /// Un document uniforme aurait **un** intervalle et mesurerait la mise en page sans mesurer
    /// le découpage ; un document de trois lignes donnerait un p95 flatteur indiscernable d'un
    /// vrai. La répartition imite donc une signature riche réelle, et c'est la même que celle de
    /// la sonde `mail-spike-richtext` — pour que les deux relevés se comparent.
    #[must_use]
    pub fn for_bench(lines: usize) -> Self {
        let mut document = Document::default();
        let push = |document: &mut Document, fragment: &str, style: &Style| {
            let at = document.text.len();
            document.text.push_str(fragment);
            if !style.is_plain() {
                document.spans.push(mailhtml::rich::Span {
                    at,
                    len: fragment.len(),
                    style: style.clone(),
                });
            }
        };
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        let italic = Style {
            italic: true,
            ..Style::default()
        };
        let link = Style {
            link: Some("https://exemple.fr/une/page/assez/longue".to_owned()),
            ..Style::default()
        };
        let mut bullets = Vec::with_capacity(lines);
        for line in 0..lines {
            match line % 5 {
                0 => push(&mut document, "Cordialement, ", &Style::default()),
                1 => {
                    push(&mut document, "Éloïse Durand", &bold);
                    push(&mut document, " — ", &Style::default());
                    push(&mut document, "directrice technique", &italic);
                }
                2 => push(
                    &mut document,
                    "un élément de liste avec un peu de texte courant",
                    &Style::default(),
                ),
                3 => push(&mut document, "https://exemple.fr/une/page", &link),
                _ => push(
                    &mut document,
                    "Une ligne de texte courant, assez longue pour que la mise en page ait \
                     quelque chose à faire dessus.",
                    &Style::default(),
                ),
            }
            bullets.push(if line % 5 == 2 {
                Block::Bullet
            } else {
                Block::Paragraph
            });
            if line + 1 < lines {
                push(&mut document, "\n", &Style::default());
            }
        }
        document.normalise();
        document.blocks = bullets;
        Self::new(0, Some(document))
    }

    /// Insère un caractère **au milieu du tampon**, comme une frappe le ferait.
    ///
    /// Pour le banc du critère 5, et par le même chemin qu'une vraie frappe : le tampon change,
    /// et l'image suivante rattrape le document. Au milieu et non à la fin, parce qu'une
    /// insertion en fin ne décale aucun intervalle et mesurerait le meilleur cas.
    pub fn type_in_the_middle(&mut self, glyph: char) {
        let mut at = self.buffer.len() / 2;
        while at > 0 && !self.buffer.is_char_boundary(at) {
            at -= 1;
        }
        self.buffer.insert(at, glyph);
    }

    /// Remplace le contenu par un collage de HTML.
    ///
    /// ## Ce que ce chemin sert, et ce qu'il ne sert pas encore
    ///
    /// La conversion est [`mailhtml::rich::Document::from_html`], donc les deux étages qui
    /// servent déjà à lire le courrier. C'est ce que mesure le second régime du critère 5 —
    /// « un collage de 50 Ko de HTML ne doit pas figer l'interface ».
    ///
    /// **Le presse-papiers d'`egui` ne donne que du texte brut** : `Event::Paste` porte une
    /// `String`, sans variante HTML. Un Ctrl-V depuis un navigateur arrive donc ici en texte nu
    /// — que le champ colle très bien, sans styles. Cette fonction est le point d'entrée du
    /// jour où le presse-papiers en donnera, et c'est par elle que le banc mesure le coût.
    pub fn paste(&mut self, html: &str) {
        self.document = Document::from_html(html);
        self.buffer = self.document.text.clone();
        self.selection = None;
    }

    /// Le nombre de glyphes du document — le **contrôle** du relevé.
    ///
    /// Un p95 obtenu sur un document de trois lignes serait indiscernable d'un vrai. Ce chiffre
    /// est ce qui rend le relevé interprétable, et la sonde a appris qu'il faut le lire : un
    /// compteur resté constant quand l'entrée varie est le signe le plus lisible qu'on mesure
    /// la mauvaise chose.
    #[must_use]
    pub fn glyphs(&self) -> usize {
        self.document.text.chars().count()
    }

    /// Le nombre d'intervalles stylés — l'autre moitié du contrôle.
    #[must_use]
    pub fn styles(&self) -> usize {
        self.document.spans.len()
    }

    /// Dessine l'éditeur, et rend ce que l'utilisateur a demandé.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Outcome {
        // **Rattraper le document sur le tampon avant de dessiner.** Le tampon porte ce que la
        // frappe de l'image précédente y a mis, et le layouter qui suit se sert des intervalles
        // du document : les remettre d'accord d'abord est ce qui garde les styles.
        self.document.reconcile(&self.buffer);

        let mut outcome = Outcome::Idle;
        ui.horizontal_wrapped(|ui| {
            let selected = self.selected();
            // Les boutons de caractère n'ont de sens que sur une sélection : une sélection vide
            // est un curseur, et `Document::apply` ne style rien sur un intervalle vide — un
            // bouton actif qui ne fait rien serait pire qu'un bouton grisé.
            if ui
                .add_enabled(selected, egui::Button::new("Gras"))
                .on_hover_text("Gras sur la sélection")
                .clicked()
            {
                self.toggle(Toggle::Bold);
            }
            if ui
                .add_enabled(selected, egui::Button::new("Italique"))
                .on_hover_text("Italique sur la sélection")
                .clicked()
            {
                self.toggle(Toggle::Italic);
            }
            // La puce porte sur des **lignes**, donc elle marche aussi sans sélection : la
            // ligne du curseur suffit.
            if ui
                .button("Puce")
                .on_hover_text("Met les lignes touchées en liste, ou les remet en paragraphe")
                .clicked()
            {
                self.toggle_bullets();
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.target)
                    .desired_width(190.0)
                    .hint_text("https://exemple.fr"),
            );
            // Un lien demande une sélection à porter, et une cible **qui peut sortir**.
            //
            // **C'est ici que se joue l'honnêteté de l'éditeur.** La sortie HTML écarte une
            // cible refusée en silence : `exemple.fr` sans schéma, un `javascript:` collé depuis
            // une page. Sans ce contrôle, le bouton l'accepterait, l'aperçu la soulignerait, et
            // le message partirait sans le lien — l'utilisateur ne l'apprendrait jamais. Le
            // prédicat est celui de `mailhtml`, le même que celui de la sortie : deux règles
            // séparées finiraient par ne plus dire la même chose.
            let target = self.target.trim().to_owned();
            let acceptable = mailhtml::rich::link_may_leave(&target);
            if ui
                .add_enabled(selected && acceptable, egui::Button::new("Lien"))
                .on_hover_text("Fait de la sélection un lien vers cette adresse")
                .clicked()
            {
                self.set_link(Some(target.clone()));
                self.target.clear();
            }
            if !target.is_empty() && !acceptable {
                // Dire **pourquoi**, et pas seulement griser : un bouton inerte sans explication
                // se lit comme une panne.
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Adresse refusée : il faut https://, http://, mailto: ou tel:",
                );
            }
            if ui
                .add_enabled(selected, egui::Button::new("Sans lien"))
                .on_hover_text("Retire le lien de la sélection")
                .clicked()
            {
                self.set_link(None);
            }
        });

        ui.separator();
        // Le champ, et le layouter qui fait tout le travail visible.
        let output = {
            // Le document est emprunté par le layouter, donc il ne peut pas l'être en écriture
            // par ailleurs pendant le dessin : la référence est prise ici, et courte.
            let document = &self.document;
            let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap: f32| {
                let mut job = Self::sections(document, text.as_str(), ui.visuals());
                job.wrap.max_width = wrap;
                ui.fonts_mut(|fonts| fonts.layout_job(job))
            };
            egui::TextEdit::multiline(&mut self.buffer)
                .desired_width(f32::INFINITY)
                .desired_rows(10)
                .hint_text("Cordialement,\nPrénom Nom")
                .layouter(&mut layouter)
                .show(ui)
        };

        // La sélection, relevée **après** le dessin : c'est le champ qui sait où elle est, et
        // elle est en index de caractères là-bas.
        self.selection = output.cursor_range.map(|range| {
            let (from, to) = (range.primary.index.0, range.secondary.index.0);
            let text = self.buffer.as_str();
            (byte_of(text, from.min(to)), byte_of(text, from.max(to)))
        });

        ui.separator();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.saving, egui::Button::new("Enregistrer"))
                .clicked()
            {
                outcome = Outcome::Save;
            }
            if ui.button("Annuler").clicked() {
                outcome = Outcome::Cancel;
            }
            if self.saving {
                ui.spinner();
            }
            // **Dire ce que la signature emporte, et ce qu'elle n'emporte pas.** Un éditeur
            // riche laisse croire qu'il garde tout ce qu'on colle ; celui-ci garde le gras,
            // l'italique, les liens et les puces, et jette polices, tailles et couleurs. C'est
            // écrit ici plutôt que dans une documentation que personne n'ouvre.
            ui.weak(
                "Gras, italique, liens et puces. Ni police, ni couleur, ni image : \
                 rien qui fasse partir une requête chez le destinataire.",
            );
        });

        outcome
    }

    /// Vrai si la sélection porte sur au moins un caractère.
    fn selected(&self) -> bool {
        self.selection.is_some_and(|(from, to)| from < to)
    }

    /// Les fragments homogènes de la sélection, avec leurs bornes en octets.
    ///
    /// Relevés d'abord et appliqués ensuite : `Document::apply` prend `&mut self`, et styler en
    /// parcourant les fragments qu'on est en train de lire ne compile pas — pour une bonne
    /// raison, puisque chaque application redécoupe les intervalles.
    fn selected_fragments(&self) -> Vec<(usize, usize, Style)> {
        let Some((from, to)) = self.selection else {
            return Vec::new();
        };
        let mut at = from;
        self.document
            .fragments(from, to)
            .into_iter()
            .map(|fragment| {
                let start = at;
                at += fragment.text.len();
                (start, at, fragment.style)
            })
            .collect()
    }

    /// Bascule un style de caractère sur la sélection.
    ///
    /// ## La bascule est fragment par fragment, et c'est ce qui préserve le reste
    ///
    /// Poser un style unique sur toute la sélection écraserait ce qu'elle contient : mettre en
    /// gras une phrase qui contient un lien lui retirerait le lien. Chaque fragment garde donc
    /// **son** style, avec un seul bit changé.
    ///
    /// Le sens de la bascule est décidé une fois pour toute la sélection : si tout est déjà
    /// gras, le bouton dégrasse ; sinon il grasse. Sans cette décision globale, une sélection
    /// mi-grasse s'inverserait par morceaux et le bouton n'aurait pas d'effet lisible.
    fn toggle(&mut self, what: Toggle) {
        let fragments = self.selected_fragments();
        let read = |style: &Style| match what {
            Toggle::Bold => style.bold,
            Toggle::Italic => style.italic,
        };
        let all = !fragments.is_empty() && fragments.iter().all(|(.., style)| read(style));
        for (from, to, mut style) in fragments {
            match what {
                Toggle::Bold => style.bold = !all,
                Toggle::Italic => style.italic = !all,
            }
            self.document.apply(from, to, &style);
        }
    }

    /// Pose ou retire un lien sur la sélection, en gardant gras et italique.
    fn set_link(&mut self, target: Option<String>) {
        for (from, to, mut style) in self.selected_fragments() {
            style.link = target.clone();
            self.document.apply(from, to, &style);
        }
    }

    /// Bascule la nature des lignes que la sélection touche.
    ///
    /// Le sens est décidé globalement, comme pour le gras : si toutes sont déjà des puces, le
    /// bouton les remet en paragraphes.
    fn toggle_bullets(&mut self) {
        let (from, to) = self.selection.unwrap_or((0, 0));
        let first = self.document.line_at(from);
        let last = self.document.line_at(to);
        let all_bullets =
            (first..=last).all(|line| self.document.blocks.get(line) == Some(&Block::Bullet));
        for line in first..=last {
            let is_bullet = self.document.blocks.get(line) == Some(&Block::Bullet);
            if is_bullet == all_bullets {
                self.document.toggle_bullet(line);
            }
        }
    }

    /// Construit la mise en page du texte que le champ affiche.
    ///
    /// ## Elle est bornée sur le texte reçu, et pas sur celui du document
    ///
    /// Pendant l'image où une touche est frappée, `egui` applique l'évènement puis appelle le
    /// layouter : le texte reçu porte donc le caractère de trop que le document n'a pas encore.
    /// Deux conséquences, et la première est la seule qui compte :
    ///
    /// - **les sections sont découpées dans le texte reçu**, jamais dans celui du document. Une
    ///   section hors bornes ou à cheval sur un caractère ferait paniquer la mise en page, et un
    ///   éditeur qui plante sur une lettre accentuée est inutilisable en français ;
    /// - une frontière de style peut être en retard d'un caractère pendant une image. À
    ///   soixante images par seconde, ça ne se voit pas ; le document est rattrapé au début de
    ///   l'image suivante.
    fn sections(document: &Document, text: &str, visuals: &egui::Visuals) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        // `append` et non des `byte_range` écrits à la main : il ajoute le texte **et** sa
        // section d'un seul geste, donc la couverture est juste par construction. Des bornes
        // calculées à part se désaccorderaient du texte au premier cas limite, et un
        // désaccord ici déplace le curseur du champ.
        let mut at = 0usize;
        for (rank, line) in text.split('\n').enumerate() {
            let start = at;
            let end = start + line.len();
            at = end + 1;
            let mut indent = if document.blocks.get(rank) == Some(&Block::Bullet) {
                BULLET_INDENT
            } else {
                0.0
            };
            let mut cut = start;
            for span in &document.spans {
                let from = span.at.max(cut);
                let to = (span.at + span.len).min(end);
                if from >= to || to > text.len() {
                    continue;
                }
                if !text.is_char_boundary(from) || !text.is_char_boundary(to) {
                    continue;
                }
                if let Some(plain) = text.get(cut..from) {
                    job.append(
                        plain,
                        std::mem::take(&mut indent),
                        format_for(&Style::default(), visuals),
                    );
                }
                if let Some(styled) = text.get(from..to) {
                    job.append(
                        styled,
                        std::mem::take(&mut indent),
                        format_for(&span.style, visuals),
                    );
                }
                cut = to;
            }
            // Le reste de la ligne, **et son saut de ligne** : sans lui, le texte de la galère
            // ne serait pas celui du champ et le curseur se décalerait d'une ligne.
            let tail = at.min(text.len());
            if let Some(rest) = text.get(cut..tail) {
                job.append(
                    rest,
                    std::mem::take(&mut indent),
                    format_for(&Style::default(), visuals),
                );
            }
        }
        job
    }
}

/// Le format `egui` d'un style du document.
///
/// ## Le gras est une couleur, et ce n'est pas un raccourci
///
/// Aucune police grasse n'est enregistrée dans la coquille — `theme::install` installe une
/// proportionnelle et un repli CJK, pas une famille par graisse. `RichText::strong()`, qui sert
/// déjà à rendre le gras d'un message reçu, éclaircit la couleur du texte ; ce format fait la
/// même chose, pour que l'éditeur et le volet de lecture montrent le gras de la même façon.
///
/// Les couleurs viennent du thème et non d'une constante : la coquille se lit en clair comme en
/// sombre, et un bleu écrit en dur disparaîtrait dans l'un des deux.
fn format_for(style: &Style, visuals: &egui::Visuals) -> egui::TextFormat {
    let mut format = egui::TextFormat {
        font_id: egui::FontId::proportional(14.0),
        italics: style.italic,
        color: if style.bold {
            visuals.strong_text_color()
        } else {
            visuals.text_color()
        },
        ..Default::default()
    };
    if style.link.is_some() {
        // Souligné **et** coloré : la couleur seule est invisible pour une partie des
        // daltoniens, et un lien qui ne se voit pas dans l'éditeur se perd à la relecture.
        format.color = visuals.hyperlink_color;
        format.underline = egui::Stroke::new(1.0, format.color);
    }
    format
}

/// Le décalage en octets d'un index de caractère.
///
/// `egui` compte les caractères, le document compte les octets, et sur un corpus français
/// l'écart n'est pas une subtilité : « Éloïse » fait six caractères et huit octets. Un index
/// au-delà du texte rend sa longueur — c'est ce que rend `chars().count()`, et la fin du texte
/// est la bonne réponse.
fn byte_of(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or_else(|| text.len(), |(at, _)| at)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Editor, Toggle, byte_of};
    use mailhtml::rich::{Block, Document, Style};

    /// Un éditeur ouvert sur un document, avec une sélection posée à la main.
    ///
    /// Poser la sélection à la main est ce qui rend ces tests possibles sans fenêtre : elle est
    /// relevée du champ de texte pendant le dessin, et tout ce qui la consomme est du calcul.
    fn editor(text: &str, selection: Option<(usize, usize)>) -> Editor {
        let mut it = Editor::new(1, Some(Document::plain(text)));
        it.selection = selection;
        it
    }

    #[test]
    fn a_byte_offset_is_not_a_character_count() {
        // Le piège du corpus français, et il est de l'autre côté de la frontière `egui` : le
        // champ compte les caractères, le document compte les octets.
        assert_eq!(byte_of("Éloïse", 0), 0);
        assert_eq!(byte_of("Éloïse", 1), 2, "« É » fait deux octets");
        assert_eq!(byte_of("Éloïse", 6), "Éloïse".len());
        assert_eq!(byte_of("Éloïse", 99), "Éloïse".len(), "au-delà : la fin");
        assert_eq!(byte_of("", 3), 0);
    }

    #[test]
    fn bold_applies_to_the_selection_and_only_to_it() {
        let mut it = editor("Marie Dupont", Some((0, 5)));
        it.toggle(Toggle::Bold);
        assert!(it.document().style_at(0).bold, "{:?}", it.document().spans);
        assert!(!it.document().style_at(6).bold);
    }

    #[test]
    fn bold_on_an_already_bold_selection_removes_it() {
        // La bascule : le même bouton met et retire, sinon il n'y a pas de moyen de dégrasser.
        let mut it = editor("Marie", Some((0, 5)));
        it.toggle(Toggle::Bold);
        assert!(it.document().style_at(0).bold);
        it.toggle(Toggle::Bold);
        assert!(!it.document().style_at(0).bold, "{:?}", it.document().spans);
        assert!(it.document().spans.is_empty(), "{:?}", it.document().spans);
    }

    #[test]
    fn a_half_bold_selection_becomes_entirely_bold() {
        // Le sens de la bascule est décidé pour toute la sélection. Fragment par fragment, une
        // sélection mi-grasse s'inverserait par morceaux et le bouton n'aurait pas d'effet
        // lisible.
        let mut it = editor("Marie Dupont", Some((0, 5)));
        it.toggle(Toggle::Bold);
        it.selection = Some((0, 12));
        it.toggle(Toggle::Bold);
        assert!(it.document().style_at(0).bold);
        assert!(it.document().style_at(11).bold, "{:?}", it.document().spans);
        assert_eq!(
            it.document().spans.len(),
            1,
            "les deux moitiés n'ont pas fusionné"
        );
    }

    #[test]
    fn bolding_a_selection_that_contains_a_link_keeps_the_link() {
        // **Ce que la bascule fragment par fragment protège.** Poser un style unique sur la
        // sélection retirerait le lien, et l'utilisateur le perdrait en appuyant sur « Gras ».
        let mut it = editor("voir notre site ici", Some((5, 15)));
        it.set_link(Some("https://exemple.fr".to_owned()));
        it.selection = Some((0, 19));
        it.toggle(Toggle::Bold);

        let document = it.document();
        assert!(document.style_at(0).bold);
        assert!(document.style_at(6).bold);
        assert_eq!(
            document.style_at(6).link.as_deref(),
            Some("https://exemple.fr"),
            "le lien est parti avec le gras : {:?}",
            document.spans
        );
    }

    #[test]
    fn italic_and_bold_live_together() {
        let mut it = editor("les deux", Some((0, 8)));
        it.toggle(Toggle::Bold);
        it.toggle(Toggle::Italic);
        assert!(it.document().style_at(0).bold);
        assert!(
            it.document().style_at(0).italic,
            "{:?}",
            it.document().spans
        );
    }

    #[test]
    fn a_style_button_without_a_selection_does_nothing() {
        // Une sélection vide est un curseur. Un bouton qui stylerait le document entier serait
        // une surprise irréversible d'un seul clic.
        let mut it = editor("Marie", Some((3, 3)));
        it.toggle(Toggle::Bold);
        assert!(it.document().spans.is_empty(), "{:?}", it.document().spans);

        let mut it = editor("Marie", None);
        it.toggle(Toggle::Bold);
        assert!(it.document().spans.is_empty());
    }

    #[test]
    fn the_link_button_accepts_exactly_what_the_message_can_carry() {
        // **Le contrôle qui empêche l'éditeur de mentir.** La sortie HTML écarte une cible
        // refusée en silence ; si le bouton l'acceptait, l'aperçu la soulignerait et le message
        // partirait sans elle. Le prédicat est celui de `mailhtml`, donc exactement celui de la
        // sortie — et ce test le vérifie des deux côtés, pour que les deux ne divergent pas.
        for good in [
            "https://exemple.fr",
            "http://exemple.fr",
            "mailto:marie@exemple.fr",
            "tel:+33123456789",
        ] {
            assert!(mailhtml::rich::link_may_leave(good), "{good}");
            let mut it = editor("ici", Some((0, 3)));
            it.set_link(Some(good.to_owned()));
            assert!(it.document().to_html().contains(good), "{good}");
        }
        for bad in [
            "exemple.fr",
            "/relatif",
            "javascript:alert(1)",
            "data:text/html,x",
            "",
            "   ",
        ] {
            assert!(!mailhtml::rich::link_may_leave(bad.trim()), "{bad}");
            // Et si une telle cible arrivait quand même dans le document — un store abîmé, un
            // client de l'API — la sortie ne la porterait pas.
            let mut it = editor("ici", Some((0, 3)));
            it.set_link(Some(bad.to_owned()));
            assert!(!it.document().to_html().contains("href"), "{bad}");
        }
    }

    #[test]
    fn a_link_can_be_removed_without_touching_the_rest_of_the_style() {
        let mut it = editor("ici", Some((0, 3)));
        it.toggle(Toggle::Bold);
        it.set_link(Some("https://exemple.fr".to_owned()));
        it.set_link(None);
        assert_eq!(it.document().style_at(0).link, None);
        assert!(it.document().style_at(0).bold, "le gras est parti aussi");
    }

    #[test]
    fn the_bullet_button_works_on_the_cursor_line_and_on_a_multi_line_selection() {
        // La puce porte sur des lignes : elle doit marcher sans sélection, sur la ligne du
        // curseur, et couvrir toutes les lignes qu'une sélection touche.
        let mut it = editor("un\ndeux\ntrois", Some((4, 4)));
        it.toggle_bullets();
        assert_eq!(
            it.document().blocks,
            vec![Block::Paragraph, Block::Bullet, Block::Paragraph]
        );

        it.selection = Some((0, 13));
        it.toggle_bullets();
        assert_eq!(
            it.document().blocks,
            vec![Block::Bullet, Block::Bullet, Block::Bullet],
            "une sélection mi-puces doit tout mettre en puces"
        );

        it.toggle_bullets();
        assert_eq!(
            it.document().blocks,
            vec![Block::Paragraph, Block::Paragraph, Block::Paragraph],
            "et le second appui doit tout remettre en paragraphes"
        );
    }

    #[test]
    fn the_layout_covers_the_text_it_was_given_exactly() {
        // **L'invariant que la mise en page d'`egui` exige** : les sections doivent couvrir le
        // texte, sans trou ni recouvrement, ou le curseur se décale et le champ affiche autre
        // chose que ce qu'on tape.
        let mut document = Document::plain("Éloïse Durand\ndirectrice\nfin");
        document.apply(
            0,
            "Éloïse".len(),
            &Style {
                bold: true,
                ..Style::default()
            },
        );
        document.toggle_bullet(1);

        let job = Editor::sections(&document, &document.text, &egui::Visuals::dark());
        let mut at = 0usize;
        for section in &job.sections {
            assert_eq!(section.byte_range.start.0, at, "trou ou recouvrement");
            at = section.byte_range.end.0;
        }
        assert_eq!(
            at,
            document.text.len(),
            "la fin du texte n'est pas couverte"
        );
        assert_eq!(job.text, document.text);
    }

    #[test]
    fn the_layout_never_slices_a_character_even_when_the_buffer_is_ahead() {
        // **Le décalage d'une image.** `egui` applique la frappe puis appelle le layouter : le
        // texte reçu porte un caractère que le document n'a pas encore. Les sections sont donc
        // découpées dans le texte reçu — et sur un accent, un découpage naïf paniquerait.
        let mut document = Document::plain("été");
        document.apply(
            0,
            "été".len(),
            &Style {
                bold: true,
                ..Style::default()
            },
        );

        for ahead in ["ét", "é", "", "étéx", "été\nligne", "e"] {
            let job = Editor::sections(&document, ahead, &egui::Visuals::dark());
            assert_eq!(job.text, ahead, "{ahead:?}");
            let mut at = 0usize;
            for section in &job.sections {
                assert_eq!(section.byte_range.start.0, at, "{ahead:?}");
                assert!(
                    ahead.is_char_boundary(section.byte_range.start.0),
                    "{ahead:?}"
                );
                assert!(
                    ahead.is_char_boundary(section.byte_range.end.0),
                    "{ahead:?}"
                );
                at = section.byte_range.end.0;
            }
            assert_eq!(at, ahead.len(), "{ahead:?} : couverture incomplète");
        }
    }

    #[test]
    fn a_bullet_line_is_indented_once_and_not_once_per_style() {
        // `leading_space` s'applique à la première rangée d'une section : le répéter décalerait
        // la ligne à chaque changement de style.
        let mut document = Document::plain("puce grasse ici");
        document.apply(
            5,
            11,
            &Style {
                bold: true,
                ..Style::default()
            },
        );
        document.toggle_bullet(0);

        let job = Editor::sections(&document, &document.text, &egui::Visuals::dark());
        let indented = job
            .sections
            .iter()
            .filter(|it| it.leading_space > 0.0)
            .count();
        assert_eq!(indented, 1, "{:?}", job.sections);
        assert!(job.sections[0].leading_space > 0.0);
    }

    #[test]
    fn typing_in_the_buffer_carries_into_the_document_on_the_next_frame() {
        // Le montage complet, sans fenêtre : le champ écrit dans le tampon, et l'image suivante
        // rattrape le document en gardant les styles.
        let mut it = editor("Marie", Some((0, 5)));
        it.toggle(Toggle::Bold);
        it.buffer.push_str(" Dupont");
        it.document.reconcile(&it.buffer);

        assert_eq!(it.document().text, "Marie Dupont");
        assert!(it.document().style_at(0).bold, "{:?}", it.document().spans);
        // Le texte tapé après un mot en gras hérite de son style, ce qui est le comportement
        // attendu quand on complète un mot — et `Document::insert` en porte le test.
        assert_eq!(it.document().to_html(), "<p><b>Marie Dupont</b></p>");
    }
}

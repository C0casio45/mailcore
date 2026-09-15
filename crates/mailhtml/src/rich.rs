//! Un document stylé : le modèle de l'éditeur de signature, **et sa conversion en HTML**.
//!
//! ## Pourquoi il est dans `mailhtml` et pas dans la coquille
//!
//! Parce qu'il fait deux traductions, et que les deux vivent ici : du HTML vers le document
//! — c'est [`Document::from_html`], qui n'est qu'un habillage de [`crate::blocks`] — et du
//! document vers le HTML, qui part dans un message.
//!
//! Le mettre dans la coquille aurait mis la seconde traduction dans un binaire d'interface, donc
//! hors de portée de `mailsmtp` qui doit l'écrire. Et le HTML qui part dans un message n'est pas
//! une affaire de présentation : c'est un format de fil, et il se teste sans fenêtre.
//!
//! ## Ce que le modèle est, et pourquoi il est aussi simple
//!
//! Un `String` plus une liste d'intervalles stylés, triés et disjoints. La sonde
//! `mail-spike-richtext` a mesuré ce qui coûte : **la mise en page, quatorze microsecondes pour
//! 9 800 glyphes**, et une insertion ne fait que décaler quelques dizaines d'entiers. Aucun rope
//! ni piece table n'aurait amélioré le chiffre qui compte, et le modèle compliqué n'a donc pas
//! de raison d'être.
//!
//! ## Le HTML produit n'a pas de style
//!
//! Ni police, ni couleur, ni marge, ni `class`. Trois raisons, dans l'ordre :
//!
//! **Ce que le destinataire voit doit être ce que l'utilisateur a écrit.** Une signature qui
//! impose une police impose aussi son absence : le lecteur qui ne l'a pas voit un repli que
//! personne n'a choisi.
//!
//! **Aucune ressource distante.** `docs/PRIVACY.md`, règle 5 : une `@font-face` ou une image de
//! fond dans une signature ferait sortir une requête chez chaque destinataire, à chaque
//! ouverture. Le HTML d'ici ne peut pas en porter, parce qu'il n'a pas d'attribut où en mettre.
//!
//! **Ce qui est court se relit.** La liste des balises émises tient en une ligne : `<p>`, `<b>`,
//! `<i>`, `<a href>`, `<ul>`, `<li>`, `<br>`. Un lecteur peut vérifier qu'il n'y a rien d'autre.

/// Ce qu'un intervalle de texte porte comme style.
///
/// Les quatre que la RFC 2046 rend lisibles partout, et rien de plus. Pas de couleur, pas de
/// taille : voir la documentation du module.
/// Pas `Copy` : la cible d'un lien est une `String`. Le style se clone, et un clone de
/// quelques octets par intervalle est sans conséquence — la sonde a mesuré que le coût est la
/// mise en page.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Style {
    /// Gras.
    pub bold: bool,
    /// Italique.
    pub italic: bool,
    /// La cible d'un lien, s'il y en a un.
    ///
    /// Portée par le style et non par un intervalle à part : un lien peut être gras, et deux
    /// dimensions séparées demanderaient de gérer leur recouvrement.
    pub link: Option<String>,
}

impl Style {
    /// Vrai si ce style n'ajoute rien.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        !self.bold && !self.italic && self.link.is_none()
    }
}

/// Ce qu'une ligne représente.
///
/// Le niveau de bloc est porté par la **ligne** et non par un intervalle : une puce s'applique à
/// une ligne entière, et un intervalle de puce à cheval sur deux lignes n'aurait pas de sens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Block {
    /// Un paragraphe ordinaire.
    #[default]
    Paragraph,
    /// Un élément de liste à puces.
    Bullet,
}

/// Un intervalle stylé.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    /// Décalage de début, en octets.
    pub at: usize,
    /// Longueur, en octets.
    pub len: usize,
    /// Ce qu'il porte.
    pub style: Style,
}

/// Un document stylé.
///
/// ## La sérialisation est un format rangé, pas un détail
///
/// C'est sous cette forme que la signature d'un compte vit dans le store — et pas en HTML,
/// parce qu'une ligne blanche ne revient pas d'un aller-retour HTML : voir le test
/// `a_blank_line_does_not_come_back_from_html_and_that_is_the_display_rule`. Les noms de champs
/// sont donc le format sur disque, et en renommer un est une migration.
///
/// Un document relu peut venir d'octets que ce code n'a pas écrits, donc les invariants ne sont
/// **pas** supposés à la lecture : les deux listes se replient sur vide, [`Document::normalise`]
/// remet une nature par ligne, et [`Document::fragments`] écarte un intervalle qui trancherait
/// un caractère.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Document {
    /// Le texte, avec ses fins de ligne.
    pub text: String,
    /// Les intervalles stylés, **triés par `at` et disjoints**.
    ///
    /// C'est l'invariant qui rend la construction d'une mise en page linéaire : un seul
    /// parcours, sans recherche. Il est tenu par les méthodes de ce type ; un appelant qui
    /// écrirait directement dans le champ peut le rompre, et [`Document::normalise`] le
    /// rétablit.
    #[serde(default)]
    pub spans: Vec<Span>,
    /// La nature de chaque ligne, dans l'ordre. Une entrée par ligne du texte.
    #[serde(default)]
    pub blocks: Vec<Block>,
}

impl Document {
    /// Un document d'une seule ligne de texte brut.
    #[must_use]
    pub fn plain(text: &str) -> Self {
        let mut it = Self {
            text: text.to_owned(),
            spans: Vec::new(),
            blocks: Vec::new(),
        };
        it.retag_blocks();
        it
    }

    /// Vrai si le document ne contient rien d'affichable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Vrai si le document porte quelque chose qu'une partie texte ne peut pas montrer.
    ///
    /// ## C'est ce qui décide si un message a une partie HTML
    ///
    /// Un message dont le corps et la signature sont du texte nu n'a **rien** à gagner à partir
    /// en `multipart/alternative` : deux parties identiques, une frontière MIME, et un lecteur
    /// de plus à qui faire confiance pour choisir. La partie HTML n'est ajoutée que quand elle
    /// dit quelque chose de plus — un gras, un italique, un lien, une puce.
    #[must_use]
    pub fn is_formatted(&self) -> bool {
        !self.spans.is_empty() || self.blocks.iter().any(|it| *it != Block::Paragraph)
    }

    /// Aligne le document sur un texte réécrit de l'extérieur, en gardant les styles.
    ///
    /// ## Pourquoi cette fonction existe
    ///
    /// L'éditeur de la coquille est un `egui::TextEdit`, et un `TextEdit` édite une `String` :
    /// il insère, supprime, colle et annule sans rien dire de ce qu'il a fait. Reconstruire le
    /// document à chaque frappe perdrait tous les styles ; les garder demande de retrouver **ce
    /// qui a changé**, et c'est ce que fait cette fonction.
    ///
    /// Le calcul est le plus simple qui marche : le plus long préfixe commun, le plus long
    /// suffixe commun, et une substitution entre les deux. Une frappe au milieu d'une signature
    /// ne déplace donc que les intervalles qui la suivent — le coût qu'a mesuré la sonde.
    ///
    /// ## Ce qu'elle ne prétend pas faire
    ///
    /// Ce n'est pas un `diff` : deux modifications éloignées dans la même image sont vues comme
    /// une seule substitution qui couvre les deux, donc les styles **entre** elles sont perdus.
    /// Ça n'arrive pas en tapant — une frappe change un endroit — et le cas où ça arrive, un
    /// remplacement de tout le texte, doit justement perdre les styles du texte remplacé.
    pub fn reconcile(&mut self, updated: &str) {
        if updated == self.text {
            return;
        }
        let prefix = self
            .text
            .as_bytes()
            .iter()
            .zip(updated.as_bytes())
            .take_while(|(old, new)| old == new)
            .count();
        // Sur une frontière de caractère, et jamais au-delà du plus court des deux textes :
        // « é » remplacé par « e » a un octet commun qui coupe le caractère en deux.
        let prefix = floor_boundary(&self.text, floor_boundary(updated, prefix));

        let tail = self.text.as_bytes()[prefix..]
            .iter()
            .rev()
            .zip(updated.as_bytes()[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let old_end = ceil_boundary(&self.text, self.text.len() - tail);
        let new_end = ceil_boundary(updated, updated.len() - tail);
        // Les deux fins ont pu remonter différemment sur une frontière : ce qui est repris de
        // part et d'autre doit rester égal, sinon la substitution mangerait un caractère.
        let (old_end, new_end) = if self.text.len() - old_end == updated.len() - new_end {
            (old_end, new_end)
        } else {
            (self.text.len(), updated.len())
        };

        if old_end > prefix {
            self.remove(prefix, old_end);
        }
        if new_end > prefix {
            // `insert` fait hériter le style de ce qui précède, ce qui est le comportement
            // voulu : taper à la fin d'un mot en gras continue en gras.
            let inserted = updated[prefix..new_end].to_owned();
            self.insert(prefix, &inserted);
        }
        // Le repli qui rend la fonction totale : si les décalages n'ont pas mené au texte
        // demandé — un cas que le calcul ci-dessus ne devrait pas produire — le texte gagne et
        // les styles partent. Un éditeur qui affiche autre chose que ce que l'utilisateur a
        // tapé serait pire que un qui perd un gras.
        if self.text != updated {
            *self = Self::plain(updated);
        }
    }

    /// Le style qui couvre un décalage, ou le style neutre.
    #[must_use]
    pub fn style_at(&self, at: usize) -> Style {
        self.spans
            .iter()
            .find(|span| span.at <= at && at < span.at + span.len)
            .map_or_else(Style::default, |span| span.style.clone())
    }

    /// Applique un style à un intervalle, en découpant ce qui le recouvre.
    ///
    /// ## Le découpage est la seule vraie opération du modèle
    ///
    /// Mettre en gras une sélection qui commence au milieu d'un lien doit couper le lien en
    /// deux : sa première moitié reste un lien simple, la seconde devient un lien gras. Sans le
    /// découpage, l'un des deux styles écraserait l'autre — et l'utilisateur perdrait un lien en
    /// appuyant sur « gras ».
    ///
    /// Un intervalle vide ne fait rien : une sélection vide est un curseur, et appuyer sur
    /// « gras » sans rien sélectionner ne doit pas styler le document entier.
    pub fn apply(&mut self, from: usize, to: usize, style: &Style) {
        let (from, to) = (from.min(to), from.max(to));
        if from >= to || to > self.text.len() {
            return;
        }
        let mut kept: Vec<Span> = Vec::with_capacity(self.spans.len() + 2);
        for span in &self.spans {
            let end = span.at + span.len;
            // Ce qui dépasse **avant** la sélection reste tel quel.
            if span.at < from {
                kept.push(Span {
                    at: span.at,
                    len: from.min(end) - span.at,
                    style: span.style.clone(),
                });
            }
            // Ce qui dépasse **après** aussi.
            if end > to {
                let start = to.max(span.at);
                kept.push(Span {
                    at: start,
                    len: end - start,
                    style: span.style.clone(),
                });
            }
        }
        kept.push(Span {
            at: from,
            len: to - from,
            style: style.clone(),
        });
        self.spans = kept;
        self.normalise();
    }

    /// Trie, jette les vides et fusionne les voisins de même style.
    ///
    /// La fusion n'est pas cosmétique : sans elle, mettre en gras caractère par caractère
    /// laisserait un intervalle par caractère, et la mise en page couperait une section par
    /// glyphe. Sur une signature de 200 lignes, c'est le seul chemin par lequel le critère 5
    /// pourrait retomber.
    pub fn normalise(&mut self) {
        // **Un intervalle qui ne dit rien n'est pas un intervalle.** `apply` en pose un pour
        // *retirer* un style — dégrasser, c'est appliquer le style sans le gras — et le garder
        // aurait deux conséquences : le document resterait « formaté » aux yeux de
        // [`Document::is_formatted`], donc un message sans aucun style partirait quand même en
        // `multipart/alternative` ; et la signature rangée grossirait d'un intervalle par
        // aller-retour sur un bouton.
        self.spans.retain(|it| it.len > 0 && !it.style.is_plain());
        self.spans.sort_by_key(|it| it.at);
        let mut merged: Vec<Span> = Vec::with_capacity(self.spans.len());
        for span in self.spans.drain(..) {
            match merged.last_mut() {
                Some(last) if last.at + last.len == span.at && last.style == span.style => {
                    last.len += span.len;
                }
                _ => merged.push(span),
            }
        }
        self.spans = merged;
        self.retag_blocks();
    }

    /// Remet la liste des natures de ligne à la longueur du texte.
    ///
    /// Les lignes existantes gardent la leur ; les nouvelles sont des paragraphes. Sans ça, une
    /// insertion de saut de ligne décalerait toutes les natures d'un cran — la puce sauterait
    /// à la ligne suivante.
    fn retag_blocks(&mut self) {
        let count = self.text.split('\n').count();
        self.blocks.resize(count, Block::Paragraph);
    }

    /// Insère du texte, en décalant ce qui suit.
    ///
    /// Le texte inséré hérite du style de ce qui le précède immédiatement : c'est ce qu'attend
    /// quelqu'un qui tape à la fin d'un mot en gras.
    pub fn insert(&mut self, at: usize, text: &str) {
        if at > self.text.len() || !self.text.is_char_boundary(at) || text.is_empty() {
            return;
        }
        let grew = text.len();
        // Le nombre de lignes **avant** l'insertion, pour savoir où insérer les natures des
        // lignes créées.
        let line = self.text[..at].split('\n').count() - 1;
        self.text.insert_str(at, text);

        for span in &mut self.spans {
            if span.at >= at {
                span.at += grew;
            } else if span.at + span.len >= at {
                // L'insertion tombe dans cet intervalle, ou juste à sa fin : il grandit. « Juste
                // à sa fin » est ce qui fait qu'un caractère tapé après un mot en gras est
                // gras.
                span.len += grew;
            }
        }

        let added = text.matches('\n').count();
        for _ in 0..added {
            let at = (line + 1).min(self.blocks.len());
            self.blocks.insert(at, Block::Paragraph);
        }
        self.normalise();
    }

    /// Retire un intervalle de texte.
    pub fn remove(&mut self, from: usize, to: usize) {
        let (from, to) = (from.min(to), from.max(to));
        if from >= to || to > self.text.len() {
            return;
        }
        if !self.text.is_char_boundary(from) || !self.text.is_char_boundary(to) {
            return;
        }
        let shrunk = to - from;
        let removed_lines = self.text[from..to].matches('\n').count();
        let line = self.text[..from].split('\n').count() - 1;
        self.text.replace_range(from..to, "");

        for span in &mut self.spans {
            let end = span.at + span.len;
            if span.at >= to {
                span.at -= shrunk;
            } else if end > from {
                // Recouvrement partiel ou total : la partie supprimée disparaît de l'intervalle.
                let start = span.at.max(from);
                let stop = end.min(to);
                span.len -= stop - start;
                if span.at > from {
                    span.at = from;
                }
            }
        }
        for _ in 0..removed_lines {
            if line + 1 < self.blocks.len() {
                self.blocks.remove(line + 1);
            }
        }
        self.normalise();
    }

    /// Ajoute un autre document à la fin de celui-ci, styles et natures de ligne compris.
    ///
    /// ## C'est par là qu'une signature entre dans un message
    ///
    /// Et c'est une opération sur le **document**, pas sur le message : la signature est
    /// ajoutée au corps que l'utilisateur a sous les yeux, dans la fenêtre de rédaction, où il
    /// peut la relire et la modifier. `docs/PHASE-3.md` le demande — « rien de ce qu'on ajoute
    /// au message n'est invisible à l'utilisateur » — et ça exclut de la coller au moment de
    /// l'envoi, quand plus personne ne la regarde.
    ///
    /// Les deux documents sont séparés par une ligne blanche s'il y a quelque chose des deux
    /// côtés. Rien n'est ajouté à un document vide : signer un message vide donnerait un corps
    /// qui commence par deux lignes blanches.
    pub fn append(&mut self, other: &Self) {
        if other.is_empty() {
            return;
        }
        if self.is_empty() {
            // Le document d'accueil n'a que des blancs : il n'y a rien à séparer, et garder ses
            // lignes blanches mettrait la signature en bas d'une page vide.
            self.clone_from(other);
            return;
        }
        let separator = "\n\n";
        let at = self.text.len() + separator.len();
        self.text.push_str(separator);
        self.text.push_str(&other.text);
        self.spans.extend(other.spans.iter().map(|span| Span {
            at: span.at + at,
            len: span.len,
            style: span.style.clone(),
        }));
        // `normalise` remet une nature par ligne — toutes en paragraphe, y compris la ligne
        // blanche du séparateur — puis les natures de l'autre document remplacent les
        // dernières. Les recopier après est ce qui garde ses puces.
        self.normalise();
        let from = self.blocks.len().saturating_sub(other.blocks.len());
        self.blocks.truncate(from);
        self.blocks.extend_from_slice(&other.blocks);
    }

    /// Bascule la nature d'une ligne entre paragraphe et puce.
    pub fn toggle_bullet(&mut self, line: usize) {
        if let Some(block) = self.blocks.get_mut(line) {
            *block = match *block {
                Block::Paragraph => Block::Bullet,
                Block::Bullet => Block::Paragraph,
            };
        }
    }

    /// Le rang de la ligne qui contient un décalage.
    #[must_use]
    pub fn line_at(&self, at: usize) -> usize {
        self.text[..at.min(self.text.len())]
            .split('\n')
            .count()
            .saturating_sub(1)
    }
}

/// Les schémas qu'un lien de signature peut porter en sortie.
///
/// La liste est plus courte que celle de [`crate::sanitize`] : elle n'a pas `data`. Un `data:`
/// n'est pas une cible, c'est un document arbitraire déguisé en lien — et celui-ci partirait
/// dans le courrier de quelqu'un d'autre, pas dans une fenêtre qu'on contrôle.
const LINK_SCHEMES: [&str; 4] = ["http", "https", "mailto", "tel"];

/// Vrai si cette cible de lien peut sortir dans un message.
///
/// Le schéma est reconnu par [`crate::sanitize::scheme_of`] et non par un analyseur d'URL, pour
/// la raison écrite là-bas : il faut voir ce qu'un **moteur de rendu** verrait, `java\tscript:`
/// compris. Deux implémentations de cette reconnaissance divergeraient, et c'est celle du
/// courrier sortant qui recevrait le moins de tests.
///
/// Une URL sans schéma est refusée : relative, elle se résoudrait contre l'origine du lecteur du
/// destinataire, ce qui ne veut rien dire et peut viser autre chose que ce que l'utilisateur
/// croyait écrire.
///
/// ## Publique, et c'est ce qui empêche l'éditeur de mentir
///
/// La sortie HTML écarte une cible refusée **en silence** : le texte reste, le `href` part. Sans
/// ce prédicat, une interface laisserait poser un lien vers `exemple.fr` — sans schéma, donc
/// refusé — et l'afficherait souligné dans son aperçu alors que le message ne le porterait pas.
/// Montrer une chose et en envoyer une autre est précisément ce qu'un éditeur ne doit pas faire,
/// et le seul moyen de l'éviter est que la fenêtre puisse poser la question **avant**.
#[must_use]
pub fn link_may_leave(target: &str) -> bool {
    crate::sanitize::scheme_of(target).is_some_and(|scheme| LINK_SCHEMES.contains(&scheme.as_str()))
}

/// Un fragment de texte homogène : une tranche du document et le style qui la couvre.
///
/// ## Pourquoi ce type existe plutôt qu'une boucle chez chaque appelant
///
/// Deux appelants ont besoin exactement du même découpage : la sortie HTML ici, et le
/// `layouter` de la coquille qui construit une section d'`egui` par fragment. Le faire deux fois
/// donnerait deux découpages à faire coïncider — et celui de l'affichage se verrait tout de
/// suite, celui du message envoyé jamais.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment<'a> {
    /// La tranche de texte, empruntée au document.
    pub text: &'a str,
    /// Ce qui la couvre.
    pub style: Style,
}

impl Document {
    /// Reprend un document depuis du HTML — c'est le **collage**.
    ///
    /// ## Le collage passe par les deux étages qui servent déjà à lire le courrier
    ///
    /// [`crate::sanitize::clean`] puis [`crate::blocks::blocks`], dans cet ordre, et aucun code
    /// d'analyse en propre. Ce n'est pas de l'économie : un presse-papiers rempli par un
    /// navigateur est une **entrée hostile** au même titre qu'un corps de message, et le
    /// convertisseur écrit exprès serait une deuxième implémentation du même découpage — celle
    /// qui recevrait moins de tests que celle qui affiche le courrier reçu.
    ///
    /// La politique est celle du défaut, donc `allow_remote_images: false`. Un collage depuis un
    /// site en emporterait les images de fond, et elles partiraient chercher une requête chez
    /// **chaque destinataire, à chaque ouverture** : `docs/PRIVACY.md`, règle 5, appliquée au
    /// courrier qu'on écrit.
    ///
    /// ## Ce qui ne survit pas, et c'est voulu
    ///
    /// Taille, police, couleurs, marges — le modèle n'a pas où les mettre. Le code et le barré
    /// de [`crate::blocks::Style`] non plus : les styles d'ici sont ceux qui se lisent partout.
    /// Les titres et les citations deviennent des paragraphes, les séparateurs et les images
    /// disparaissent — une image n'a pas de représentation dans ce modèle, et fabriquer du texte
    /// depuis son `alt` mettrait dans la signature des mots que l'utilisateur n'a pas écrits.
    #[must_use]
    pub fn from_html(html: &str) -> Self {
        let cleaned = crate::sanitize::clean(html, crate::sanitize::Policy::default());
        let mut it = Self::default();
        let mut kinds: Vec<Block> = Vec::new();

        for block in crate::blocks::blocks(&cleaned.html) {
            let kind = match block.kind {
                crate::blocks::Kind::Item { .. } => Block::Bullet,
                crate::blocks::Kind::Rule | crate::blocks::Kind::Image { .. } => continue,
                crate::blocks::Kind::Paragraph
                | crate::blocks::Kind::Heading(_)
                | crate::blocks::Kind::Quote(_)
                | crate::blocks::Kind::Pre => Block::Paragraph,
            };
            if block.runs.iter().all(|run| run.text.trim().is_empty()) {
                continue;
            }
            if !kinds.is_empty() {
                it.text.push('\n');
            }
            for run in &block.runs {
                // **Un fragment ne peut pas porter de saut de ligne.** La nature d'un bloc est
                // portée par la ligne, donc un `<pre>` qui garde ses retours désynchroniserait
                // `blocks` du texte — une puce se retrouverait sur la mauvaise ligne. Le
                // remplacement se fait avant de mesurer la longueur, sinon les intervalles
                // désigneraient des octets qui ont bougé.
                let text = run.text.replace(['\n', '\r'], " ");
                let style = Style {
                    bold: run.style.bold,
                    italic: run.style.italic,
                    link: run.style.link.clone().filter(|to| link_may_leave(to)),
                };
                let at = it.text.len();
                it.text.push_str(&text);
                if !style.is_plain() {
                    it.spans.push(Span {
                        at,
                        len: text.len(),
                        style,
                    });
                }
            }
            kinds.push(kind);
        }

        it.normalise();
        // `normalise` a remis une nature par ligne, toutes en paragraphe. Les natures relevées
        // au passage les remplacent — il y en a exactement une par ligne émise, par
        // construction. Le document vide compte une ligne, et il lui en faut donc une.
        if kinds.is_empty() {
            kinds.push(Block::Paragraph);
        }
        debug_assert_eq!(kinds.len(), it.blocks.len(), "une nature par ligne");
        it.blocks = kinds;
        it
    }

    /// Les fragments homogènes d'un intervalle, dans l'ordre.
    ///
    /// Les trous entre intervalles stylés sortent en style neutre : l'appelant reçoit une
    /// couverture complète de `[from, to)` et n'a pas à retrouver ce qui manque. Un intervalle
    /// vide, ou hors du texte, ne rend rien.
    #[must_use]
    pub fn fragments(&self, from: usize, to: usize) -> Vec<Fragment<'_>> {
        let (from, to) = (from.min(to), to.min(self.text.len()));
        if from >= to || !self.text.is_char_boundary(from) || !self.text.is_char_boundary(to) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut at = from;
        // Les intervalles sont triés et disjoints : un seul parcours suffit, sans recherche.
        // C'est l'invariant que `normalise` maintient, et la raison qu'il existe.
        for span in &self.spans {
            let start = span.at.max(from);
            let end = (span.at + span.len).min(to);
            if start >= end {
                continue;
            }
            // **Un intervalle dont les bornes coupent un caractère est ignoré, pas rapiécé.**
            // Ce document peut venir d'un store, donc d'octets que ce code n'a pas écrits ;
            // trancher dessus paniquerait. Le texte sort alors en style neutre — perdre un gras
            // sur un document abîmé est le bon échec, et le rapiécer silencieusement déplacerait
            // le style sans le dire.
            if !self.text.is_char_boundary(start) || !self.text.is_char_boundary(end) {
                continue;
            }
            if at < start {
                out.push(Fragment {
                    text: &self.text[at..start],
                    style: Style::default(),
                });
            }
            out.push(Fragment {
                text: &self.text[start..end],
                style: span.style.clone(),
            });
            at = end;
        }
        if at < to {
            out.push(Fragment {
                text: &self.text[at..to],
                style: Style::default(),
            });
        }
        out
    }

    /// Les lignes du document : bornes en octets, et nature.
    fn lines(&self) -> impl Iterator<Item = (usize, usize, Block)> + '_ {
        let mut at = 0usize;
        self.text.split('\n').enumerate().map(move |(rank, line)| {
            let from = at;
            // `+ 1` pour le saut de ligne que le découpage a consommé.
            at += line.len() + 1;
            (
                from,
                from + line.len(),
                self.blocks.get(rank).copied().unwrap_or_default(),
            )
        })
    }

    /// Le HTML qui part dans un message.
    ///
    /// ## Sept balises, et rien d'autre
    ///
    /// `<p>`, `<b>`, `<i>`, `<a href>`, `<ul>`, `<li>`, `<br>`. Aucun attribut en dehors du
    /// `href`, donc **nulle part** où une police, une couleur ou une ressource distante pourrait
    /// se glisser : la garantie tient à l'absence d'un endroit où l'écrire, pas à une liste
    /// noire. Un test vérifie qu'aucune autre balise ne sort.
    ///
    /// Un lien dont le schéma ne peut pas sortir **perd son `href` et garde son texte** : le
    /// texte est ce que l'utilisateur voulait dire, la cible est ce qui serait un piège.
    #[must_use]
    pub fn to_html(&self) -> String {
        let mut out = String::new();
        let mut in_list = false;
        for (from, to, kind) in self.lines() {
            match kind {
                Block::Bullet => {
                    if !in_list {
                        out.push_str("<ul>");
                        in_list = true;
                    }
                    out.push_str("<li>");
                    self.write_inline(&mut out, from, to);
                    out.push_str("</li>");
                }
                Block::Paragraph => {
                    if in_list {
                        out.push_str("</ul>");
                        in_list = false;
                    }
                    if from == to {
                        // Une ligne vide est une ligne, pas un paragraphe vide : un `<p></p>` se
                        // replie à rien chez une partie des lecteurs, et l'espacement voulu
                        // disparaîtrait. `<p><br></p>` est la forme que les lecteurs répandus
                        // rendent comme une ligne blanche et une seule.
                        //
                        // Elle ne **revient** pas par [`Document::from_html`] : `crate::blocks`
                        // jette les blocs entièrement blancs, délibérément — voir le test qui
                        // épingle cette limite.
                        out.push_str("<p><br></p>");
                    } else {
                        out.push_str("<p>");
                        self.write_inline(&mut out, from, to);
                        out.push_str("</p>");
                    }
                }
            }
        }
        if in_list {
            out.push_str("</ul>");
        }
        out
    }

    /// Écrit les fragments d'une ligne, avec leurs balises de style.
    ///
    /// L'imbrication est `<a>` dehors, puis `<b>`, puis `<i>`, et l'ordre est fixe : deux
    /// fragments voisins qui l'ordonneraient différemment produiraient un chevauchement, ce
    /// qu'aucun lecteur ne rattrape de la même façon.
    fn write_inline(&self, out: &mut String, from: usize, to: usize) {
        for fragment in self.fragments(from, to) {
            let link = fragment
                .style
                .link
                .as_deref()
                .filter(|target| link_may_leave(target));
            if let Some(target) = link {
                out.push_str("<a href=\"");
                escape_into(out, target);
                out.push_str("\">");
            }
            if fragment.style.bold {
                out.push_str("<b>");
            }
            if fragment.style.italic {
                out.push_str("<i>");
            }
            escape_into(out, fragment.text);
            if fragment.style.italic {
                out.push_str("</i>");
            }
            if fragment.style.bold {
                out.push_str("</b>");
            }
            if link.is_some() {
                out.push_str("</a>");
            }
        }
    }

    /// Le texte brut qui part dans le même message, en face du HTML.
    ///
    /// ## La cible d'un lien y est écrite en clair
    ///
    /// `multipart/alternative` veut deux versions du **même** message. Un lecteur en texte brut
    /// ne peut pas cliquer : si la cible n'est pas écrite, elle est perdue pour lui, et il lit
    /// « notre site » sans savoir lequel. Elle n'est répétée que si elle diffère du texte, sinon
    /// une URL collée telle quelle sortirait deux fois de suite.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (rank, (from, to, kind)) in self.lines().enumerate() {
            if rank > 0 {
                out.push('\n');
            }
            if kind == Block::Bullet {
                out.push_str("- ");
            }
            for fragment in self.fragments(from, to) {
                out.push_str(fragment.text);
                let target = fragment
                    .style
                    .link
                    .as_deref()
                    .filter(|target| link_may_leave(target))
                    // Une URL collée telle quelle est déjà son propre texte : la répéter la
                    // ferait sortir deux fois de suite.
                    .filter(|target| *target != fragment.text);
                if let Some(target) = target {
                    out.push_str(" <");
                    out.push_str(target);
                    out.push('>');
                }
            }
        }
        out
    }
}

/// Le plus grand décalage à une frontière de caractère qui ne dépasse pas `at`.
///
/// Un décalage au-delà du texte est ramené à sa fin : c'est ce dont [`Document::reconcile`] a
/// besoin quand les deux textes n'ont pas la même longueur.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Le plus petit décalage à une frontière de caractère qui ne descend pas sous `at`.
fn ceil_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Échappe ce qui serait pris pour du balisage.
///
/// Les quatre caractères, guillemet compris : la même fonction sert pour le texte et pour la
/// valeur d'un `href`, et deux échappements dont un seul couvre les attributs sont exactement
/// comment on finit par appeler le mauvais.
fn escape_into(out: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(character),
        }
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Block, Document, Span, Style};

    fn bold() -> Style {
        Style {
            bold: true,
            ..Style::default()
        }
    }

    fn link(to: &str) -> Style {
        Style {
            link: Some(to.to_owned()),
            ..Style::default()
        }
    }

    #[test]
    fn applying_a_style_to_the_middle_of_a_span_cuts_it_in_three() {
        // **Le découpage, qui est la seule vraie opération du modèle.** Sans lui, mettre en
        // gras une partie d'un lien écraserait le lien, et l'utilisateur le perdrait en
        // appuyant sur un bouton de style.
        let mut it = Document::plain("un lien entier");
        it.apply(0, 14, &link("https://exemple.fr"));
        assert_eq!(it.spans.len(), 1);

        it.apply(3, 7, &bold());
        assert_eq!(it.spans.len(), 3, "{:?}", it.spans);
        assert_eq!(it.style_at(0).link.as_deref(), Some("https://exemple.fr"));
        assert!(it.style_at(4).bold);
        assert_eq!(it.style_at(10).link.as_deref(), Some("https://exemple.fr"));
    }

    #[test]
    fn an_empty_selection_styles_nothing() {
        // Une sélection vide est un curseur. Appuyer sur « gras » sans rien sélectionner ne
        // doit pas styler le document entier.
        let mut it = Document::plain("du texte");
        it.apply(4, 4, &bold());
        assert!(it.spans.is_empty(), "{:?}", it.spans);
        assert!(!it.style_at(4).bold);
    }

    #[test]
    fn removing_a_style_removes_the_span_and_not_only_its_style() {
        // Dégrasser, c'est appliquer le style **sans** le gras : l'intervalle posé ne dit alors
        // plus rien. Le garder laisserait `is_formatted` répondre vrai, donc ferait partir un
        // `multipart/alternative` pour un message sans aucun style — et ferait grossir la
        // signature rangée d'un intervalle par aller-retour sur le bouton.
        let mut it = Document::plain("Marie");
        it.apply(0, 5, &bold());
        assert!(it.is_formatted());

        it.apply(0, 5, &Style::default());
        assert!(it.spans.is_empty(), "{:?}", it.spans);
        assert!(!it.is_formatted());
        assert_eq!(it.to_html(), "<p>Marie</p>");
    }

    #[test]
    fn adjacent_spans_of_the_same_style_are_merged() {
        // **La fusion n'est pas cosmétique.** Mettre en gras caractère par caractère laisserait
        // un intervalle par caractère, et la mise en page couperait une section par glyphe : le
        // seul chemin par lequel le critère 5 pourrait retomber.
        let mut it = Document::plain("abcdefgh");
        for at in 0..8 {
            it.apply(at, at + 1, &bold());
        }
        assert_eq!(it.spans.len(), 1, "{:?}", it.spans);
        assert_eq!(it.spans[0].len, 8);
    }

    #[test]
    fn typing_after_a_bold_word_stays_bold() {
        // Ce qu'attend quelqu'un qui tape à la fin d'un mot en gras. L'inverse — un caractère
        // qui perd le style — oblige à re-sélectionner à chaque mot.
        let mut it = Document::plain("gras");
        it.apply(0, 4, &bold());
        it.insert(4, "s");
        assert!(it.style_at(4).bold, "{:?}", it.spans);
        assert_eq!(it.text, "grass");
    }

    #[test]
    fn an_insertion_before_a_span_shifts_it_without_styling_it() {
        let mut it = Document::plain("gras");
        it.apply(0, 4, &bold());
        let prefix = "très ";
        it.insert(0, prefix);
        assert_eq!(it.text, "très gras");
        assert!(!it.style_at(0).bold, "le texte inséré a pris le style");
        // Le décalage est en **octets** : « très » en fait six, pas cinq. Écrire 5 ici passait
        // pour une évidence et désignait le « s » — le test échouait sur une bonne
        // implémentation.
        assert!(it.style_at(prefix.len()).bold, "{:?}", it.spans);
        assert_eq!(it.spans[0].at, prefix.len());
    }

    #[test]
    fn removing_text_shrinks_the_spans_that_covered_it() {
        let mut it = Document::plain("abcdefgh");
        it.apply(2, 6, &bold());
        it.remove(3, 5);
        assert_eq!(it.text, "abcfgh");
        assert_eq!(it.spans.len(), 1, "{:?}", it.spans);
        assert_eq!(it.spans[0].len, 2, "{:?}", it.spans);
    }

    #[test]
    fn removing_a_whole_span_removes_it() {
        let mut it = Document::plain("abcdefgh");
        it.apply(2, 6, &bold());
        it.remove(2, 6);
        assert_eq!(it.text, "abgh");
        assert!(it.spans.is_empty(), "{:?}", it.spans);
    }

    #[test]
    fn a_new_line_does_not_move_the_bullet_of_the_next_one() {
        // **Le décalage qu'un `resize` naïf provoque.** Sans insérer la nature de la ligne
        // créée au bon rang, la puce sauterait à la ligne suivante à chaque saut de ligne
        // ajouté au-dessus.
        let mut it = Document::plain("premiere\ndeuxieme");
        it.toggle_bullet(1);
        assert_eq!(it.blocks, vec![Block::Paragraph, Block::Bullet]);

        it.insert(0, "avant\n");
        assert_eq!(
            it.blocks,
            vec![Block::Paragraph, Block::Paragraph, Block::Bullet],
            "la puce a bougé"
        );
    }

    #[test]
    fn an_insertion_on_a_character_boundary_is_the_only_one_accepted() {
        // Un décalage au milieu d'un caractère accentué ferait paniquer `insert_str`. Refuser
        // plutôt que paniquer : un éditeur qui plante sur un accent est inutilisable en
        // français.
        let mut it = Document::plain("été");
        let before = it.clone();
        it.insert(1, "x");
        assert_eq!(it, before, "une insertion hors frontière a été acceptée");
        it.insert(2, "x");
        assert_eq!(it.text, "éxté");
    }

    #[test]
    fn a_removal_outside_the_text_does_nothing() {
        let mut it = Document::plain("court");
        let before = it.clone();
        it.remove(3, 100);
        assert_eq!(it, before);
        it.remove(1, 1);
        assert_eq!(it, before);
    }

    #[test]
    fn a_style_that_adds_nothing_is_plain() {
        assert!(Style::default().is_plain());
        assert!(!bold().is_plain());
        assert!(!link("https://x.fr").is_plain());
    }

    #[test]
    fn the_line_of_an_offset_is_found() {
        let it = Document::plain("une\ndeux\ntrois");
        assert_eq!(it.line_at(0), 0);
        assert_eq!(it.line_at(4), 1);
        assert_eq!(it.line_at(9), 2);
        // Hors du texte : la dernière ligne, plutôt qu'une panique.
        assert_eq!(it.line_at(999), 2);
    }

    #[test]
    fn a_pasted_bold_word_keeps_its_style() {
        // La bonne surprise de la sonde : `mailhtml::blocks` rend déjà des fragments stylés,
        // donc le collage n'a pas de convertisseur à lui.
        let it = Document::from_html("<p>Cordialement, <b>Marie</b></p>");
        assert_eq!(it.text, "Cordialement, Marie");
        assert!(!it.style_at(0).bold);
        assert!(it.style_at(14).bold, "{:?}", it.spans);
    }

    #[test]
    fn a_pasted_javascript_link_loses_its_target_and_keeps_its_text() {
        // **Le cas qui compte de tout ce fichier.** Ce HTML part chez quelqu'un d'autre : un
        // `href` piégé qui survit au collage est un piège qu'on a posté soi-même. Le texte
        // reste, parce que c'est ce que l'utilisateur voulait dire.
        for hostile in [
            r#"<a href="javascript:alert(1)">clique</a>"#,
            r#"<a href="JavaScript:alert(1)">clique</a>"#,
            r#"<a href="java&#9;script:alert(1)">clique</a>"#,
            r#"<a href="data:text/html,<h1>x">clique</a>"#,
            r#"<a href="/relatif">clique</a>"#,
        ] {
            let it = Document::from_html(hostile);
            assert_eq!(it.text, "clique", "{hostile}");
            assert_eq!(it.style_at(0).link, None, "{hostile} : la cible a survécu");
            assert!(
                !it.to_html().contains("href"),
                "{hostile} : un href est sorti — {}",
                it.to_html()
            );
        }
    }

    #[test]
    fn a_link_that_may_leave_survives_the_paste_and_the_output() {
        for good in [
            "https://exemple.fr/page",
            "http://exemple.fr",
            "mailto:marie@exemple.fr",
            "tel:+33123456789",
        ] {
            let it = Document::from_html(&format!(r#"<a href="{good}">ici</a>"#));
            assert_eq!(it.style_at(0).link.as_deref(), Some(good), "{good}");
            assert!(it.to_html().contains(good), "{good} : {}", it.to_html());
        }
    }

    #[test]
    fn a_pasted_remote_image_leaves_nothing_behind() {
        // `docs/PRIVACY.md` règle 5, appliquée au courrier qu'on **écrit** : une image de fond
        // ramassée sur un site partirait chercher une requête chez chaque destinataire, à chaque
        // ouverture. Le modèle n'a pas où la mettre, et la sortie n'a pas d'attribut où l'écrire.
        let it = Document::from_html(
            r#"<p>avant</p><img src="https://pisteur.example/p.gif" alt="logo"><p>après</p>"#,
        );
        assert_eq!(it.text, "avant\naprès");
        let html = it.to_html();
        assert!(!html.contains("img"), "{html}");
        assert!(!html.contains("pisteur"), "{html}");
        // L'`alt` non plus : ce serait mettre dans la signature un mot que personne n'a tapé.
        assert!(!html.contains("logo"), "{html}");
    }

    #[test]
    fn text_that_looks_like_markup_is_escaped_on_the_way_out() {
        // Quelqu'un qui **tape** `<b>` dans sa signature veut lire `<b>`, pas devenir gras. Et
        // sans échappement, un `<` tapé dans un nom casserait le message chez le destinataire.
        let it = Document::plain(r#"3 < 5 & "guillemets" <b>pas gras</b>"#);
        let html = it.to_html();
        assert_eq!(
            html,
            "<p>3 &lt; 5 &amp; &quot;guillemets&quot; &lt;b&gt;pas gras&lt;/b&gt;</p>"
        );
        // Et l'aller-retour le rend tel quel : c'est la preuve que l'échappement est complet.
        assert_eq!(Document::from_html(&html).text, it.text);
    }

    #[test]
    fn an_ampersand_in_a_link_target_is_escaped_without_being_doubled() {
        // Un `&` non échappé dans un `href` fait un attribut tronqué chez le destinataire ; un
        // `&` échappé deux fois fait une URL qui ne mène plus au bon endroit.
        let target = "https://exemple.fr/x?a=1&b=2";
        let mut it = Document::plain("ici");
        it.apply(0, 3, &link(target));
        let html = it.to_html();
        assert!(html.contains("a=1&amp;b=2"), "{html}");
        assert_eq!(
            Document::from_html(&html).style_at(0).link.as_deref(),
            Some(target),
            "l'aller-retour a abîmé la cible"
        );
    }

    #[test]
    fn a_document_survives_a_round_trip_through_html() {
        // **La propriété qui tranche les cas limites** : puces consécutives, styles voisins,
        // accents. Ce qui ne revient pas identique est ce qui arriverait déformé chez le
        // destinataire — et le destinataire, c'est aussi nous quand le message revient des
        // envoyés par IMAP.
        //
        // Les lignes blanches sont hors de la propriété, et le test suivant dit pourquoi.
        let mut it = Document::plain("Cordialement,\nMarie Dupont\ndirectrice\nun\ndeux\nfin");
        it.apply(14, 26, &bold());
        it.apply(27, 37, &link("https://exemple.fr"));
        it.toggle_bullet(3);
        it.toggle_bullet(4);

        let back = Document::from_html(&it.to_html());
        assert_eq!(back.text, it.text, "le texte a changé");
        assert_eq!(back.blocks, it.blocks, "les natures de ligne ont changé");
        assert_eq!(back.spans, it.spans, "les styles ont changé");
    }

    #[test]
    fn a_blank_line_does_not_come_back_from_html_and_that_is_the_display_rule() {
        // **Une limite, épinglée plutôt que laissée vague.** `crate::blocks` jette les blocs
        // entièrement blancs — `blocks("<div></div><p>  </p>")` est vide, et c'est la bonne
        // règle pour *afficher* du courrier : un `<div>` vide ne doit pas fabriquer une ligne.
        // `from_html` en hérite, parce qu'il passe par là exprès.
        //
        // Conséquence, et c'est pourquoi elle est acceptable : la ligne blanche de
        // « Cordialement, » survit **dans le document**, qui est ce qui est rangé et ce qui est
        // édité. Elle ne se perd qu'au retour d'un HTML — donc sur un message relu depuis le
        // serveur, où une ligne blanche en moins est une nuance d'affichage et pas un message
        // faux. Ranger la signature en HTML la perdrait à chaque ouverture de l'éditeur : c'est
        // la raison pour laquelle elle ne s'y range pas.
        let it = Document::plain("haut\n\nbas");
        assert_eq!(it.to_html(), "<p>haut</p><p><br></p><p>bas</p>");
        assert_eq!(Document::from_html(&it.to_html()).text, "haut\nbas");
    }

    #[test]
    fn a_span_that_splits_a_character_is_ignored_rather_than_sliced() {
        // Un document peut venir d'un store, donc d'octets que ce code n'a pas écrits. Trancher
        // au milieu d'un caractère accentué paniquerait — et paniquer en écrivant le HTML d'un
        // message, c'est perdre le message.
        let mut it = Document::plain("été");
        it.spans.push(Span {
            at: 1,
            len: 2,
            style: bold(),
        });
        let found = it.fragments(0, it.text.len());
        assert_eq!(found.iter().map(|it| it.text).collect::<String>(), "été");
        assert!(!found.iter().any(|it| it.style.bold), "{found:?}");
        assert_eq!(it.to_html(), "<p>été</p>");
        assert_eq!(it.to_text(), "été");
    }

    #[test]
    fn consecutive_bullets_share_one_list() {
        // Un `<ul>` par puce donne une liste de listes : l'indentation double à chaque ligne
        // chez le destinataire.
        let mut it = Document::plain("un\ndeux\ntexte\ntrois");
        it.toggle_bullet(0);
        it.toggle_bullet(1);
        it.toggle_bullet(3);
        assert_eq!(
            it.to_html(),
            "<ul><li>un</li><li>deux</li></ul><p>texte</p><ul><li>trois</li></ul>"
        );
    }

    #[test]
    fn an_empty_line_is_emitted_as_one_blank_line_and_not_as_nothing() {
        // Un `<p></p>` se replie à rien chez une partie des lecteurs, et la ligne blanche que
        // l'utilisateur a mise entre son nom et son titre disparaîtrait **chez le
        // destinataire**. Ce qu'elle devient au retour est l'affaire du test voisin.
        let it = Document::plain("haut\n\n\nbas");
        assert_eq!(
            it.to_html(),
            "<p>haut</p><p><br></p><p><br></p><p>bas</p>",
            "deux lignes blanches doivent en donner deux"
        );
        assert_eq!(it.to_text(), "haut\n\n\nbas");
    }

    #[test]
    fn only_the_seven_documented_tags_are_emitted() {
        // La documentation du module affirme une liste de sept balises et dit qu'un lecteur peut
        // la vérifier. Ceci est cette vérification — sinon l'affirmation vieillit toute seule.
        let mut it = Document::plain("Marie\nitalique\nlien\npuce\n\nfin");
        it.apply(0, 5, &bold());
        it.apply(6, 14, &{
            Style {
                italic: true,
                ..Style::default()
            }
        });
        it.apply(15, 19, &link("https://exemple.fr"));
        it.toggle_bullet(3);

        let html = it.to_html();
        let mut seen: Vec<String> = Vec::new();
        let mut rest = html.as_str();
        while let Some(at) = rest.find('<') {
            rest = &rest[at + 1..];
            let end = rest.find('>').unwrap_or(rest.len());
            let tag = rest[..end]
                .trim_start_matches('/')
                .split([' ', '\t'])
                .next()
                .unwrap_or_default()
                .to_owned();
            if !seen.contains(&tag) {
                seen.push(tag);
            }
            rest = &rest[end..];
        }
        seen.sort();
        assert_eq!(
            seen,
            vec!["a", "b", "br", "i", "li", "p", "ul"],
            "une balise hors liste est sortie — {html}"
        );
        // Et un seul nom d'attribut, celui du lien.
        assert_eq!(html.matches('=').count(), 1, "{html}");
        assert_eq!(html.matches("href=").count(), 1, "{html}");
    }

    #[test]
    fn a_paste_that_carries_newlines_inside_a_run_keeps_one_nature_per_line() {
        // **Le piège que la nature portée par la ligne tend au collage.** Un `<pre>` garde ses
        // retours ; s'ils entraient dans le texte, il y aurait plus de lignes que de natures et
        // une puce se retrouverait sur la mauvaise. `debug_assert` l'attrape en test, mais la
        // sortie doit être juste en release aussi.
        let it = Document::from_html("<pre>une\ndeux\ntrois</pre><ul><li>puce</li></ul>");
        assert_eq!(
            it.text.split('\n').count(),
            it.blocks.len(),
            "{:?} pour {:?}",
            it.blocks,
            it.text
        );
        assert_eq!(*it.blocks.last().unwrap(), Block::Bullet);
        assert!(
            it.to_html().ends_with("<ul><li>puce</li></ul>"),
            "{}",
            it.to_html()
        );
    }

    #[test]
    fn malformed_html_produces_a_poorer_document_and_never_a_panic() {
        // La règle de `crate::blocks` et de `crate::text`, héritée ici : un presse-papiers est
        // une entrée hostile, et un éditeur qui plante sur un collage de travers est inutilisable.
        for hostile in [
            "<p>pas fermé",
            "<<<>>>",
            "<p>a</p",
            "&pasuneentite; &#xZZ; &",
            "<b><i><b><i>imbrication de travers</b></i>",
            "<ul><li>puce sans fin",
            "<p>\u{0}\u{1}\u{feff}</p>",
            "",
            "<script>alert(1)</script>",
            "<style>p{background:url(https://pisteur.example/x)}</style>",
        ] {
            let it = Document::from_html(hostile);
            // L'invariant que tout le reste suppose, sur chacune de ces entrées.
            assert_eq!(
                it.text.split('\n').count(),
                it.blocks.len(),
                "{hostile} : {:?} pour {:?}",
                it.blocks,
                it.text
            );
            let html = it.to_html();
            assert!(!html.contains("pisteur"), "{hostile} : {html}");
            assert!(!html.contains("alert"), "{hostile} : {html}");
            // Et l'aller-retour est stable : le deuxième passage ne change plus rien.
            assert_eq!(Document::from_html(&html).text, it.text, "{hostile}");
        }
    }

    #[test]
    fn a_paste_of_fifty_kilobytes_of_html_stays_one_document() {
        // Le collage que le critère 5 borne. Ici ce n'est pas le temps qui est vérifié — la
        // sonde l'a mesuré à 0,4 ms — mais que rien ne se perde ni ne se duplique à l'échelle.
        let line = "<p>Une ligne de signature avec un <b>mot en gras</b> et un \
                    <a href=\"https://exemple.fr\">lien</a>.</p>";
        let big = line.repeat(50_000 / line.len() + 1);
        let it = Document::from_html(&big);
        let lines = it.text.split('\n').count();
        assert_eq!(lines, big.matches("<p>").count());
        assert_eq!(it.blocks.len(), lines);
        // Deux intervalles par ligne — le gras et le lien — et pas un par caractère : c'est la
        // fusion de `normalise` qui tient le critère 5 sur un document collé.
        assert_eq!(it.spans.len(), lines * 2, "{} intervalles", it.spans.len());
    }

    #[test]
    fn the_plain_part_says_where_a_link_goes() {
        // Un lecteur en texte brut ne peut pas cliquer : sans la cible écrite, il lit « notre
        // site » sans savoir lequel.
        let mut it = Document::plain("notre site\nhttps://exemple.fr\npuce");
        it.apply(0, 10, &link("https://exemple.fr/page"));
        it.apply(11, 30, &link("https://exemple.fr"));
        it.toggle_bullet(2);
        assert_eq!(
            it.to_text(),
            "notre site <https://exemple.fr/page>\nhttps://exemple.fr\n- puce",
        );
    }

    #[test]
    fn the_plain_part_of_a_refused_link_carries_no_target() {
        // Le pendant en texte brut du test qui compte : un `javascript:` écrit en clair dans la
        // partie texte serait un piège lisible, donc cliquable à la main.
        let mut it = Document::plain("clique");
        it.spans.push(Span {
            at: 0,
            len: 6,
            style: link("javascript:alert(1)"),
        });
        assert_eq!(it.to_text(), "clique");
        assert!(!it.to_html().contains("javascript"), "{}", it.to_html());
    }

    #[test]
    fn fragments_cover_the_whole_range_without_a_hole() {
        // Ce que la sortie HTML et le `layouter` de la coquille supposent tous les deux : la
        // concaténation des fragments est le texte, sans trou ni recouvrement.
        let mut it = Document::plain("abcdefghij");
        it.apply(2, 4, &bold());
        it.apply(6, 8, &link("https://exemple.fr"));
        let found = it.fragments(0, it.text.len());
        assert_eq!(
            found.iter().map(|it| it.text).collect::<String>(),
            "abcdefghij"
        );
        assert_eq!(found.len(), 5, "{found:?}");
        // Un sous-intervalle qui coupe un style au milieu rend la moitié, avec le style.
        let cut = it.fragments(3, 7);
        assert_eq!(cut.iter().map(|it| it.text).collect::<String>(), "defg");
        assert!(cut[0].style.bold);
        assert_eq!(cut[0].text, "d");
    }

    #[test]
    fn fragments_of_a_boundary_that_splits_a_character_render_nothing() {
        // Le même refus que `insert` : une borne au milieu d'un caractère accentué ferait
        // paniquer le découpage, et un éditeur qui plante sur un accent est inutilisable en
        // français.
        let it = Document::plain("été");
        assert!(it.fragments(0, 1).is_empty());
        assert!(it.fragments(1, 3).is_empty());
        assert_eq!(it.fragments(0, 2).len(), 1);
        assert!(it.fragments(5, 2).is_empty());
        assert!(it.fragments(0, 999).len() == 1, "hors du texte : borné");
    }

    /// Une signature riche, celle qui sert aux tests d'ajout.
    ///
    /// Les bornes sont **calculées** et non écrites en dur : « Éloïse Durand » fait quinze
    /// octets pour treize caractères, et un 14 écrit à la main désigne « Éloïse Duran ». Le test
    /// `an_insertion_before_a_span_shifts_it_without_styling_it` porte déjà cette leçon ; elle
    /// s'applique à la fabrication du document autant qu'à ce qu'on en attend.
    fn a_signature() -> Document {
        let text = "Éloïse Durand\ndirectrice\nexemple.fr";
        let name = "Éloïse Durand";
        let site = "exemple.fr";
        let mut it = Document::plain(text);
        it.apply(0, name.len(), &bold());
        let at = text.find(site).unwrap();
        it.apply(at, at + site.len(), &link("https://exemple.fr"));
        it.toggle_bullet(1);
        it
    }

    #[test]
    fn a_signature_added_to_a_body_keeps_its_styles_and_its_bullets() {
        // Le décalage des intervalles est la seule vraie opération de l'ajout, et il est en
        // **octets** : « Éloïse » n'en fait pas six.
        let mut it = Document::plain("Bonjour,\nvoici le rapport.");
        it.append(&a_signature());

        assert_eq!(
            it.text,
            "Bonjour,\nvoici le rapport.\n\nÉloïse Durand\ndirectrice\nexemple.fr"
        );
        assert_eq!(it.blocks.len(), it.text.split('\n').count());
        let at = it.text.find("Éloïse").unwrap();
        assert!(it.style_at(at).bold, "{:?}", it.spans);
        assert!(
            !it.style_at(0).bold,
            "le corps a pris le style de la signature"
        );
        assert_eq!(
            it.style_at(it.text.find("exemple.fr").unwrap())
                .link
                .as_deref(),
            Some("https://exemple.fr")
        );
        // La puce est sur « directrice », pas sur une ligne du corps ni sur le séparateur.
        let bullets: Vec<usize> = it
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, kind)| **kind == Block::Bullet)
            .map(|(rank, _)| rank)
            .collect();
        assert_eq!(bullets, vec![4], "{:?}", it.blocks);
    }

    #[test]
    fn a_signature_on_an_empty_body_does_not_start_with_two_blank_lines() {
        // Signer un message vide — le cas d'une fenêtre de rédaction qui s'ouvre — ne doit pas
        // pousser la signature en bas d'une page blanche.
        let mut it = Document::plain("");
        it.append(&a_signature());
        assert_eq!(it, a_signature());

        let mut it = Document::plain("  \n \n");
        it.append(&a_signature());
        assert_eq!(it, a_signature());
    }

    #[test]
    fn an_empty_signature_adds_nothing_at_all() {
        // Un compte sans signature ne doit pas ajouter de ligne blanche en fin de message.
        let mut it = Document::plain("Bonjour,");
        let before = it.clone();
        it.append(&Document::plain(""));
        assert_eq!(it, before);
        it.append(&Document::plain("   \n\t"));
        assert_eq!(it, before);
    }

    #[test]
    fn a_body_and_its_signature_come_out_as_one_pair_of_parts() {
        // Ce que `mailsmtp::Draft` emporte : les deux versions du **même** message. Le corps est
        // échappé par le même chemin que la signature, donc un `<` tapé dans le corps ne casse
        // pas le HTML du message.
        let mut it = Document::plain("Bonjour,\n3 < 5 ?");
        it.append(&a_signature());
        assert_eq!(
            it.to_html(),
            "<p>Bonjour,</p><p>3 &lt; 5 ?</p><p><br></p><p><b>Éloïse Durand</b></p>\
             <ul><li>directrice</li></ul><p><a href=\"https://exemple.fr\">exemple.fr</a></p>"
        );
        assert_eq!(
            it.to_text(),
            "Bonjour,\n3 < 5 ?\n\nÉloïse Durand\n- directrice\nexemple.fr <https://exemple.fr>"
        );
    }

    #[test]
    fn typing_one_glyph_through_the_buffer_keeps_the_styles() {
        // Ce que fait un `TextEdit` à chaque frappe : il rend une `String` réécrite, sans dire
        // ce qu'il a changé. Les styles doivent survivre, sinon l'éditeur les perd à la
        // première lettre.
        let mut it = Document::plain("Marie Dupont");
        it.apply(0, 5, &bold());
        it.reconcile("Maries Dupont");
        assert_eq!(it.text, "Maries Dupont");
        assert_eq!(it.spans.len(), 1, "{:?}", it.spans);
        assert_eq!(it.spans[0].len, 6, "le gras n'a pas suivi la frappe");
        assert!(it.style_at(5).bold);
        assert!(!it.style_at(7).bold);
    }

    #[test]
    fn deleting_through_the_buffer_shrinks_the_styles() {
        let mut it = Document::plain("Marie Dupont");
        it.apply(0, 5, &bold());
        it.reconcile("Mare Dupont");
        assert_eq!(it.text, "Mare Dupont");
        assert_eq!(it.spans[0].len, 4, "{:?}", it.spans);
    }

    #[test]
    fn an_accent_replaced_by_a_letter_does_not_split_a_character() {
        // **Le piège de la frontière.** « é » et « e » partagent leur premier octet : un préfixe
        // ou un suffixe commun compté en octets tombe au milieu du caractère, et trancher là
        // paniquerait. Le texte d'arrivée doit sortir juste dans les deux sens.
        for (from, to) in [
            ("été", "ete"),
            ("ete", "été"),
            ("ae", "aé"),
            ("aé", "ae"),
            ("éé", "é"),
            ("é", "éé"),
            ("étage", "étuve"),
            ("🇫🇷", "🇫"),
        ] {
            let mut it = Document::plain(from);
            it.apply(0, from.len(), &bold());
            it.reconcile(to);
            assert_eq!(it.text, to, "{from:?} → {to:?}");
            for span in &it.spans {
                assert!(span.at + span.len <= it.text.len(), "{:?}", it.spans);
            }
        }

        // Le style survit quand la modification est **locale**, ce qui est le cas d'une frappe :
        // seule la fin change, le début est repris tel quel.
        let mut it = Document::plain("étage");
        it.apply(0, "ét".len(), &bold());
        it.reconcile("étuve");
        assert_eq!(it.text, "étuve");
        assert!(it.style_at(0).bold, "{:?}", it.spans);

        // Et voici la limite que la documentation de `reconcile` annonce, épinglée pour qu'on
        // sache qu'elle est connue : quand le **premier** caractère change et que rien ne
        // coïncide en fin, il n'y a plus de préfixe ni de suffixe communs, donc la substitution
        // couvre tout et les styles du texte remplacé partent avec lui. Ça n'arrive pas en
        // tapant ; ça arrive en remplaçant tout, où c'est le comportement voulu.
        let mut it = Document::plain("été");
        it.apply(0, "été".len(), &bold());
        it.reconcile("ete");
        assert_eq!(it.text, "ete");
        assert!(it.spans.is_empty(), "{:?}", it.spans);
    }

    #[test]
    fn pasting_over_a_selection_replaces_the_styles_it_covered() {
        let mut it = Document::plain("avant MILIEU après");
        it.apply(6, 12, &bold());
        it.reconcile("avant collé après");
        assert_eq!(it.text, "avant collé après");
        // Le texte collé n'a pas de raison d'être gras : il remplace ce qui l'était.
        assert!(!it.style_at(6).bold, "{:?}", it.spans);
    }

    #[test]
    fn a_new_line_typed_through_the_buffer_keeps_the_bullets_in_place() {
        // Le pendant, par le tampon, de
        // `a_new_line_does_not_move_the_bullet_of_the_next_one`.
        let mut it = Document::plain("premiere\ndeuxieme");
        it.toggle_bullet(1);
        it.reconcile("avant\npremiere\ndeuxieme");
        assert_eq!(
            it.blocks,
            vec![Block::Paragraph, Block::Paragraph, Block::Bullet],
            "la puce a bougé"
        );
    }

    #[test]
    fn an_unchanged_buffer_changes_nothing() {
        // Appelé à **chaque image** : le cas où rien n'a bougé doit être un test d'égalité et
        // pas une reconstruction. Sans ça, une signature affichée sans être touchée perdrait
        // ses styles au bout d'une image.
        let mut it = a_signature();
        let before = it.clone();
        it.reconcile(&before.text.clone());
        assert_eq!(it, before);
    }

    #[test]
    fn clearing_and_refilling_the_buffer_never_panics_and_always_lands_on_the_text() {
        // La propriété qui compte pour une fonction appelée sur tout ce qu'un utilisateur peut
        // faire au clavier — y compris coller un accent au milieu d'un autre, tout effacer,
        // annuler. Ce qui est affiché doit être ce qui a été tapé, quoi qu'il advienne des
        // styles.
        let etapes = [
            "",
            "a",
            "été",
            "é",
            "ete",
            "un texte plus long\navec deux lignes",
            "un texte plus long",
            "",
            "🇫🇷 drapeau",
            "🇫🇷",
            "fin",
        ];
        let mut it = a_signature();
        for etape in etapes {
            it.reconcile(etape);
            assert_eq!(it.text, etape, "le tampon et le document ont divergé");
            assert_eq!(
                it.blocks.len(),
                it.text.split('\n').count(),
                "une nature par ligne, sur {etape:?}"
            );
            // Et les intervalles restent dans le texte : c'est ce que la sortie HTML suppose.
            for span in &it.spans {
                assert!(span.at + span.len <= it.text.len(), "{:?}", it.spans);
            }
            let _ = it.to_html();
        }
    }

    #[test]
    fn only_a_document_that_says_more_than_its_text_is_formatted() {
        // Ce qui décide qu'un message a une partie HTML. Un corps et une signature en texte nu
        // n'ont rien à gagner à partir en deux parties identiques.
        assert!(!Document::plain("Cordialement,\nMarie").is_formatted());
        assert!(!Document::plain("").is_formatted());

        let mut bolded = Document::plain("Marie");
        bolded.apply(0, 5, &bold());
        assert!(bolded.is_formatted());

        let mut bulleted = Document::plain("un\ndeux");
        bulleted.toggle_bullet(1);
        assert!(bulleted.is_formatted());

        // Un lien aussi : le texte brut ne peut pas rendre une cible cliquable.
        let mut linked = Document::plain("ici");
        linked.apply(0, 3, &link("https://exemple.fr"));
        assert!(linked.is_formatted());
    }
}

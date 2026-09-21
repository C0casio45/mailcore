//! Ce qu'on donne au modèle, et ce qu'on lui refuse.
//!
//! Un embedding est une **moyenne** : chaque mot qu'on laisse entrer tire le vecteur vers lui.
//! Ce module décide donc de deux choses, et `docs/PHASE-4.md` les avait nommées comme des pièges
//! avant qu'une ligne soit écrite — la citation, et la longueur.
//!
//! ## La citation, et pourquoi elle se retire ici et pas ailleurs
//!
//! Un fil de quarante messages contient trente-neuf copies du texte cité. Les vectoriser tels
//! quels donnerait quarante vecteurs presque identiques : une recherche rendrait **le fil
//! entier** là où elle devrait rendre le message qui répond.
//!
//! La phase impose que l'endroit où la citation est retirée soit **le même** que celui qui la
//! reconnaît pour l'affichage, faute de quoi les deux divergeraient sans que rien ne le dise. Ce
//! module passe donc par le chemin d'affichage exact — les parties HTML concaténées comme dans
//! `Mailbox::render`, [`mailhtml::sanitize::clean`], puis [`mailhtml::blocks::blocks`] — et jette
//! les blocs que ce dernier a marqués [`mailhtml::blocks::Kind::Quote`]. Il n'y a pas de
//! deuxième reconnaissance de citation dans le projet, et il ne doit pas y en avoir.
//!
//! C'est plus cher qu'aplatir le HTML comme le fait l'index plein texte. C'est assumé : le
//! critère 3 demande de **mesurer** le coût de la passe, pas de le minimiser, et une citation mal
//! retirée se paierait sur le critère 1, qui est celui qui décide si la phase valait la peine.
//!
//! ## La longueur, et le piège que ce modèle-ci déplace
//!
//! La phase l'écrivait ainsi : « tout modèle a une fenêtre, et elle est courte ». Pour le modèle
//! choisi, c'est **faux techniquement et vrai en pratique**, et la nuance mérite d'être écrite
//! parce qu'elle change ce qu'on peut se permettre.
//!
//! `model2vec` n'a pas d'attention : un vecteur de message est la moyenne des vecteurs de ses
//! tokens. Rien n'empêche donc d'encoder 1,4 Mo — ça ne plantera pas, ça ne tronquera pas en
//! silence. Mais la moyenne de deux cent mille tokens est le **centre de gravité de la langue**,
//! pas celui du message : plus le texte est long, plus le vecteur ressemble à celui de tous les
//! autres textes longs. La fenêtre n'est pas une limite du modèle, c'est une limite du sens.
//!
//! D'où [`BUDGET`], et d'où le fait qu'il soit nommé : c'est un réglage de qualité, il change le
//! rappel, et l'étape 6 est là pour le mesurer plutôt que pour le subir.

use mailhtml::blocks::Kind;

/// Combien de caractères du corps entrent dans le vecteur.
///
/// ## D'où vient ce nombre
///
/// Deux mille caractères, soit trois cents à quatre cents mots de français — la longueur d'un
/// mail qu'on lit sans faire défiler. Il est choisi, pas mesuré, et c'est dit ici pour que
/// personne ne le prenne pour un résultat : l'étape 6 le fera varier contre le jeu de requêtes du
/// critère 1, qui est la seule façon honnête de le régler.
///
/// Ce qu'on sait déjà sans mesurer : le prendre plus grand dilue, le prendre plus petit coupe
/// l'information qui arrive après la formule de politesse. Les deux directions font mal, ce qui
/// est la signature d'un réglage qui a un optimum et pas une monotonie.
pub const BUDGET: usize = 2_000;

/// Le texte d'un message, prêt pour le modèle.
///
/// L'ordre est délibéré : le sujet d'abord, puis l'expéditeur, puis le corps. Il ne change rien
/// à la moyenne — une somme est commutative — mais il rend le texte lisible dans un test qui
/// échoue, et c'est sur ces textes-là qu'on diagnostiquera un mauvais rappel.
///
/// Sur le **poids** : le sujet compte pour ses quelques mots contre les trois cents du corps,
/// donc il pèse peu. C'est peut-être un défaut — un sujet est en général la phrase la plus
/// informative d'un message — mais le corriger demanderait de pondérer deux vecteurs, et une
/// pondération devinée est exactement ce que `docs/PHASE-4.md` refuse pour la fusion des scores.
/// La question est ouverte et appartient à l'étape 6.
#[must_use]
pub fn compose(subject: &str, from_name: Option<&str>, body: &str) -> String {
    let mut out = String::with_capacity(BUDGET + 128);
    push_part(&mut out, subject.trim());
    if let Some(name) = from_name {
        push_part(&mut out, name.trim());
    }
    push_part(&mut out, body);
    out
}

/// Ajoute un morceau non vide, séparé par un point.
///
/// Un point et non une espace : les trois morceaux sont des phrases distinctes, et les coller
/// bout à bout fabriquerait des mots composés que le tokeniseur découperait de travers.
fn push_part(out: &mut String, part: &str) {
    if part.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push_str(". ");
    }
    out.push_str(part);
}

/// Le texte d'un message analysé, prêt pour le modèle.
///
/// C'est la porte d'entrée réelle de la passe, et elle est ici pour que le chemin soit écrit **à
/// un seul endroit** : concaténer les parties HTML comme le fait `Mailbox::render`, en retirer
/// les citations, tronquer, et composer avec le sujet et l'expéditeur.
///
/// ## La boucle est celle de `Mailbox::render`, à la ligne près
///
/// Les parties sont parcourues dans l'ordre du message, une partie texte passe par
/// [`mailhtml::plain::to_html`] et une partie HTML est prise telle quelle. C'est **exactement**
/// ce que fait le rendu, et c'est la condition pour que la citation soit reconnue au même
/// endroit des deux côtés.
///
/// La conversion n'est pas laissée à `mail-parser::body_html`, qui se contente de remplacer les
/// sauts de ligne par des `<br/>` : une citation en texte brut en ressortait en texte ordinaire,
/// donc invisible à `mailhtml::blocks`, donc avalée par cette passe. Le défaut a été trouvé ici
/// le 2026-09-21, et il valait pour l'affichage aussi.
///
/// La propriété est **vérifiée** sur des octets RFC 5322 réels plus bas, pas déduite d'une
/// documentation : c'est le genre de comportement qu'une mise à jour de dépendance change sans
/// le dire, et il porte à lui seul une moitié du bénéfice de l'étape.
#[must_use]
pub fn from_message(
    parsed: &mail_parser::Message<'_>,
    subject: &str,
    from_name: Option<&str>,
) -> String {
    let mut html = String::new();
    for id in &parsed.html_body {
        match parsed.part(*id).map(|part| &part.body) {
            Some(mail_parser::PartType::Text(text)) => {
                html.push_str(&mailhtml::plain::to_html(text));
            }
            Some(mail_parser::PartType::Html(part)) => html.push_str(part),
            _ => {}
        }
    }
    compose(subject, from_name, &body(&html))
}

/// Le corps d'un message, cité retiré et tronqué au budget.
///
/// ## Un message qui n'est qu'une citation garde sa citation
///
/// Le cas existe et il est fréquent : un transfert sans un mot ajouté, une réponse qui tient dans
/// l'objet. Retirer les citations y laisse **zéro caractère**, donc un vecteur nul, donc un
/// message qui ne sortira jamais d'aucune recherche — une disparition silencieuse, la pire forme
/// de défaut pour cette phase.
///
/// Le repli est donc de reprendre le texte cité quand il ne reste rien d'autre. Ce n'est pas une
/// exception à la règle du fil de quarante messages : ce qui rendait les quarante vecteurs
/// identiques était la citation **en plus** du texte propre. Ici il n'y a pas de texte propre, et
/// le seul sens disponible est celui qu'on a reçu.
///
/// ## Il n'y a pas de paramètre de politique, et c'est la garantie
///
/// [`mailhtml::Policy`] permet à un appelant d'autoriser les `src` distants — ce que fait la
/// coquille quand l'utilisateur clique sur « Afficher les images ». Une passe de fond n'a
/// personne à qui demander : elle traite tout le corpus, sans clic, souvent pendant que la
/// fenêtre est fermée. Lui donner le paramètre reviendrait à lui donner la possibilité de le
/// recevoir à `true` un jour, par un appelant distrait ou par un défaut qui change.
///
/// C'est la règle de `Link::stage` et de `accounts.add`, transposée : ce qui ne doit jamais
/// arriver n'a pas de porte d'entrée. La règle 5 de `CLAUDE.md` est ainsi tenue par la signature
/// et non par la vigilance — étant entendu que ce module ne va de toute façon chercher aucune
/// ressource, [`mailhtml::blocks`] ne sachant pas le faire.
#[must_use]
pub fn body(html: &str) -> String {
    let cleaned = mailhtml::sanitize::clean(html, mailhtml::Policy::default());
    let blocks = mailhtml::blocks::blocks(&cleaned.html);

    let mut own = String::new();
    let mut quoted = String::new();
    for block in &blocks {
        // Une image n'apporte que son texte alternatif, qui est en général « image001.png ». Une
        // règle horizontale n'apporte rien. Les deux sont du bruit dans une moyenne.
        if matches!(block.kind, Kind::Rule | Kind::Image { .. }) {
            continue;
        }
        let target = if matches!(block.kind, Kind::Quote(_)) {
            &mut quoted
        } else {
            &mut own
        };
        if target.len() >= BUDGET {
            continue;
        }
        push_block(target, &block.text());
    }

    let chosen = if own.trim().is_empty() { quoted } else { own };
    truncate(chosen)
}

/// Ajoute le texte d'un bloc, séparé par une espace.
fn push_block(out: &mut String, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(text);
}

/// Coupe au budget, sur une frontière de caractère.
///
/// La frontière n'est pas une élégance : `String::truncate` **panique** au milieu d'un caractère
/// multioctet, et « à », « é », « ç » en sont. Sur un corpus français, couper à l'aveugle
/// planterait la passe sur une bonne partie du corpus.
fn truncate(mut text: String) -> String {
    if text.len() <= BUDGET {
        return text;
    }
    let mut cut = BUDGET;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// **La vérification qui porte la moitié de l'étape.**
    ///
    /// Un mail en texte brut cite avec des `>` en début de ligne, pas avec du balisage. Si la
    /// conversion de `mail-parser` ne les rend pas en `<blockquote>`, alors `blocks` ne voit
    /// aucune citation, et toutes les citations du courrier en texte brut entrent dans les
    /// vecteurs — silencieusement, avec pour seul symptôme un rappel médiocre qu'on mettrait sur
    /// le compte du modèle.
    #[test]
    fn a_plain_text_quote_is_recognised_as_a_quote() {
        let raw = b"From: Plomberie Martin <devis@exemple.invalid>\r\n\
                    Subject: Re: chaudiere\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    Bien recu, merci.\r\n\
                    \r\n\
                    > Voici le devis pour la chaudiere.\r\n\
                    > Cordialement.\r\n";
        let parser = mail_parser::MessageParser::default();
        let parsed = parser.parse(raw.as_slice()).unwrap();
        let found = from_message(&parsed, "Re: chaudiere", Some("Plomberie Martin"));

        assert!(
            found.contains("Bien recu"),
            "le texte propre a disparu : {found}"
        );
        assert!(
            !found.contains("devis"),
            "la citation en texte brut est entrée dans le vecteur : {found}"
        );
    }

    /// Le contrôle inverse du précédent : sans les `>`, les mêmes mots restent. Sans lui, le test
    /// ci-dessus passerait tout aussi bien si `from_message` rendait toujours une chaîne vide.
    #[test]
    fn the_same_plain_text_without_the_markers_stays() {
        let raw = b"From: Plomberie Martin <devis@exemple.invalid>\r\n\
                    Subject: chaudiere\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    Voici le devis pour la chaudiere.\r\n";
        let parser = mail_parser::MessageParser::default();
        let parsed = parser.parse(raw.as_slice()).unwrap();
        let found = from_message(&parsed, "chaudiere", None);
        assert!(
            found.contains("devis"),
            "le texte légitime a disparu : {found}"
        );
    }

    #[test]
    fn a_quote_does_not_enter_the_vector() {
        let html = "<p>Bien reçu, merci.</p><blockquote><p>Voici le devis pour la \
                    chaudière.</p></blockquote>";
        let found = body(html);
        assert!(found.contains("Bien reçu"));
        assert!(
            !found.contains("chaudière"),
            "la citation est entrée dans le texte : {found}"
        );
    }

    /// Le contrôle négatif du précédent. Vérifier qu'une citation est retirée ne prouve rien si
    /// le texte propre l'est aussi — c'est la leçon de `Stuffing`, phase 3.
    #[test]
    fn what_is_not_a_quote_stays() {
        let html = "<p>Voici le devis pour la chaudière.</p>";
        assert!(body(html).contains("chaudière"));
    }

    #[test]
    fn a_message_that_is_only_a_quote_keeps_it_rather_than_vanishing() {
        let html = "<blockquote><p>Voici le devis pour la chaudière.</p></blockquote>";
        let found = body(html);
        assert!(
            found.contains("chaudière"),
            "un transfert sans commentaire a produit un texte vide : {found:?}"
        );
    }

    #[test]
    fn nested_quotes_are_dropped_at_every_depth() {
        let html = "<p>ok</p><blockquote><p>alpha</p><blockquote><p>beta</p></blockquote>\
                    </blockquote>";
        let found = body(html);
        assert!(!found.contains("alpha"));
        assert!(!found.contains("beta"));
    }

    #[test]
    fn a_long_body_is_cut_at_the_budget() {
        let html = format!("<p>{}</p>", "mot ".repeat(BUDGET));
        assert!(body(&html).len() <= BUDGET);
    }

    /// Le cas qui fait paniquer `truncate` si la frontière n'est pas cherchée. « é » fait deux
    /// octets, donc une répétition tombe forcément à cheval sur la coupe.
    #[test]
    fn cutting_in_the_middle_of_an_accent_does_not_panic() {
        let html = format!("<p>{}</p>", "é".repeat(BUDGET));
        let found = body(&html);
        assert!(found.len() <= BUDGET);
        assert!(found.ends_with('é'));
    }

    #[test]
    fn an_empty_body_gives_an_empty_text() {
        assert!(body("").is_empty());
        assert!(body("<p>   </p>").trim().is_empty());
    }

    #[test]
    fn the_composed_text_carries_subject_sender_and_body() {
        let found = compose("Devis chaudière", Some("Plomberie Martin"), "Bonjour,");
        assert!(found.contains("Devis chaudière"));
        assert!(found.contains("Plomberie Martin"));
        assert!(found.contains("Bonjour,"));
    }

    #[test]
    fn a_missing_sender_name_does_not_leave_a_hole() {
        let found = compose("Devis", None, "Bonjour");
        assert_eq!(found, "Devis. Bonjour");
    }

    #[test]
    fn an_empty_subject_does_not_open_the_text_with_a_separator() {
        let found = compose("", None, "Bonjour");
        assert_eq!(found, "Bonjour");
    }

    /// Une entrée hostile : du balisage non fermé, un `<script>`, une profondeur absurde. La
    /// règle de `mailhtml` est qu'aucune de ces entrées ne produit d'erreur, et elle doit valoir
    /// jusqu'ici — une passe de fond qui panique sur un message du corpus est une passe qui ne
    /// finit jamais.
    #[test]
    fn hostile_markup_produces_text_and_never_a_panic() {
        let deep = "<blockquote>".repeat(200);
        let cases = [
            "<p>un<p>deux",
            deep.as_str(),
            "<script>alert('x')</script><p>visible</p>",
            "<p>&eacute;&amp;&lt;&#x41;</p>",
            "<<<>>><p",
        ];
        for case in cases {
            let _ = body(case);
        }
        assert!(
            body("<script>secret</script><p>visible</p>").contains("visible"),
            "le texte légitime a disparu avec le script"
        );
    }
}

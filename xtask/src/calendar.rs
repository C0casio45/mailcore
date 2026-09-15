//! Ce que le corpus réel contient comme invitations — **et le critère 6**.
//!
//! ## Pourquoi ce relevé existe avant le lecteur
//!
//! La phase 1 a appris que le corpus décide. Une invitation `text/calendar` peut venir
//! d'Outlook, de Google, de Thunderbird, d'un script maison ou d'une passerelle de 2009, et ces
//! producteurs n'écrivent pas la même chose : certains embarquent une `VTIMEZONE`, d'autres
//! nomment un `TZID` sans le définir, d'autres n'écrivent que de l'UTC. Écrire un lecteur
//! d'après la RFC seule, c'est écrire un lecteur pour un corpus imaginaire.
//!
//! Ce module fait donc deux choses, dans cet ordre historique :
//!
//! - [`survey`] — **ce qu'il y a**. Combien de pièces `text/calendar`, quelles propriétés,
//!   quels fuseaux, quels producteurs. Des formes et des comptes, jamais un contenu ;
//! - [`measure`] — **le critère 6**. Le lecteur passé sur *toutes* les pièces du corpus : lues
//!   juste, ou refusées en nommant ce qui manque.
//!
//! ## Ce qui ne sort pas de ces commandes
//!
//! Aucun sujet, aucun résumé d'événement, aucune adresse, aucun lieu. Une invitation dit avec
//! qui l'utilisateur déjeune et où : c'est plus intime qu'un sujet de message. Ce qui est
//! affiché est le **nom** des propriétés rencontrées, le **nom** des fuseaux, et des comptes.
//! Un relevé qu'on peut coller dans un journal de projet sans le relire deux fois.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::Store;
use std::collections::BTreeMap;

/// Ce qu'une pièce de calendrier trouvée dans le corpus a comme forme.
#[derive(Debug, Default)]
struct Shapes {
    /// Nombre de pièces `text/calendar` rencontrées.
    parts: usize,
    /// Nombre de messages qui en portent au moins une.
    messages: usize,
    /// Les octets de la plus grosse pièce, pour savoir si une borne est nécessaire.
    largest: usize,
    /// Combien de pièces par nom de propriété.
    properties: BTreeMap<String, usize>,
    /// Combien de pièces par valeur de `METHOD`.
    methods: BTreeMap<String, usize>,
    /// Combien de pièces par identifiant de fuseau nommé.
    zones: BTreeMap<String, usize>,
    /// Combien de pièces par producteur (`PRODID`), qui dit quel logiciel a écrit le fichier.
    producers: BTreeMap<String, usize>,
    /// Combien de pièces portent une `VTIMEZONE`, donc leurs propres décalages.
    with_vtimezone: usize,
    /// Combien nomment un fuseau **sans** le définir : le cas qui décide de ce qu'on sait faire.
    named_zone_without_definition: usize,
    /// Combien n'ont que de l'UTC, donc rien à résoudre.
    utc_only: usize,
    /// Combien portent une règle de répétition, **dans un événement** — pas celles des
    /// fuseaux, qui en ont une par observance et gonfleraient le compte de moitié.
    recurring: usize,
    /// Combien de pièces par nom de composant : `VEVENT`, `VTIMEZONE`, `VTODO`…
    components: BTreeMap<String, usize>,
    /// Pièces dont le contenu ne se décode pas en texte.
    ///
    /// Comptées à part et jamais mêlées aux autres : une pièce qu'on n'a pas su décoder n'est
    /// pas une pièce sans rendez-vous, c'est un trou dans la couverture du relevé.
    undecodable: usize,
}

/// Relève la forme des pièces `text/calendar` du corpus. N'écrit rien.
///
/// # Errors
///
/// Si le store est illisible.
pub fn survey(store_root: &Utf8PathBuf) -> Result<()> {
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let rows = store.all_for_indexing()?;
    anyhow::ensure!(!rows.is_empty(), "store vide");

    let mut shapes = Shapes::default();
    let mut unreadable = 0usize;
    for row in &rows {
        let Ok(raw) = store.blobs().read(row.blob) else {
            unreadable += 1;
            continue;
        };
        let parts = calendar_parts(&raw);
        if parts.is_empty() {
            continue;
        }
        shapes.messages += 1;
        for text in parts {
            shapes.parts += 1;
            if text.trim().is_empty() {
                shapes.undecodable += 1;
                continue;
            }
            shapes.largest = shapes.largest.max(text.len());
            note(&mut shapes, &text);
        }
    }

    println!("Store                        {store_root}");
    println!("Messages lus                 {}", rows.len());
    if unreadable > 0 {
        // Un blob absent n'est pas une absence d'invitation : c'est un trou dans la couverture,
        // et le cacher rendrait le relevé plus flatteur qu'il n'est.
        println!("Blobs illisibles             {unreadable} — non inspectés");
    }
    println!("Messages avec calendrier     {}", shapes.messages);
    println!("Pièces text/calendar         {}", shapes.parts);
    if shapes.parts == 0 {
        println!();
        println!("Aucune invitation dans ce store : le critère 6 n'y est pas mesurable.");
        return Ok(());
    }
    if shapes.undecodable > 0 {
        println!(
            "Pièces non décodables        {} — comptées, jamais mêlées au reste",
            shapes.undecodable
        );
    }
    println!("Plus grosse pièce            {} octets", shapes.largest);
    println!("Avec VTIMEZONE embarquée     {}", shapes.with_vtimezone);
    println!(
        "Fuseau nommé sans définition {}",
        shapes.named_zone_without_definition
    );
    println!("UTC seulement                {}", shapes.utc_only);
    println!("Avec répétition (RRULE)      {}", shapes.recurring);

    show("Composants", &shapes.components);
    show("Méthodes", &shapes.methods);
    show("Fuseaux nommés", &shapes.zones);
    show("Producteurs", &shapes.producers);
    show("Propriétés rencontrées", &shapes.properties);
    Ok(())
}

/// Affiche un histogramme, du plus fréquent au moins fréquent.
fn show(title: &str, counts: &BTreeMap<String, usize>) {
    if counts.is_empty() {
        return;
    }
    println!();
    println!("{title} ({}) :", counts.len());
    let mut sorted: Vec<(&String, &usize)> = counts.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (name, count) in sorted.iter().take(40) {
        println!("  {name:<44} {count:>6}");
    }
    if sorted.len() > 40 {
        println!("  … et {} autres", sorted.len() - 40);
    }
}

/// Relève la forme d'une pièce, sans en garder le contenu.
fn note(shapes: &mut Shapes, text: &str) {
    let mut named_zones = Vec::new();
    let mut has_vtimezone = false;
    let mut has_utc = false;
    let mut has_floating = false;
    let mut in_event = false;

    for line in mailcal::unfold(text) {
        let Some(property) = mailcal::Property::parse(&line) else {
            continue;
        };
        let name = property.name.to_ascii_uppercase();
        // Les propriétés `X-` sont comptées sous un seul nom : il y en a des centaines, une par
        // producteur, et la liste complète noierait celles qui comptent.
        let key = if name.starts_with("X-") {
            "X-… (extension)".to_owned()
        } else {
            name.clone()
        };
        *shapes.properties.entry(key).or_default() += 1;

        match name.as_str() {
            "METHOD" => {
                *shapes
                    .methods
                    .entry(property.value.to_ascii_uppercase())
                    .or_default() += 1;
            }
            "PRODID" => {
                // Le producteur, réduit à son éditeur : un `PRODID` complet porte un numéro de
                // version qui ferait autant de lignes que de versions.
                *shapes
                    .producers
                    .entry(producer(&property.value))
                    .or_default() += 1;
            }
            "BEGIN" => {
                let component = property.value.trim().to_ascii_uppercase();
                if component == "VTIMEZONE" {
                    has_vtimezone = true;
                }
                in_event = component == "VEVENT";
                *shapes.components.entry(component).or_default() += 1;
            }
            "END" => in_event = false,
            // **Seules celles d'un événement.** Une `VTIMEZONE` porte une `RRULE` par
            // observance — « le dernier dimanche de mars » — et les compter avec les autres
            // annonçait 1 705 répétitions pour 930 pièces, soit un chiffre qui ne veut rien
            // dire. Un contrôle faux est pire qu'un contrôle absent.
            "RRULE" if in_event => shapes.recurring += 1,
            "DTSTART" | "DTEND" | "RECURRENCE-ID" | "EXDATE" | "RDATE" => {
                match property.parameter("TZID") {
                    Some(zone) => named_zones.push(zone.to_owned()),
                    None if property.value.ends_with('Z') => has_utc = true,
                    None => has_floating = true,
                }
            }
            _ => {}
        }
    }

    if has_vtimezone {
        shapes.with_vtimezone += 1;
    }
    for zone in &named_zones {
        *shapes.zones.entry(zone.clone()).or_default() += 1;
    }
    if !named_zones.is_empty() && !has_vtimezone {
        shapes.named_zone_without_definition += 1;
    }
    if has_utc && named_zones.is_empty() && !has_floating {
        shapes.utc_only += 1;
    }
}

/// L'éditeur d'un `PRODID`, sans son numéro de version.
///
/// Un `PRODID` ressemble à `-//Microsoft Corporation//Outlook 16.0 MIMEDIR//EN`. Ce qui
/// intéresse ici est « Microsoft », pas « 16.0 » : compter les versions donnerait une ligne par
/// mise à jour du logiciel de chaque correspondant.
fn producer(value: &str) -> String {
    let cleaned = value.trim_start_matches('-').trim_start_matches('/');
    let first = cleaned.split("//").find(|it| !it.trim().is_empty());
    let name = first.unwrap_or(value).trim();
    let short: String = name.chars().take(38).collect();
    if short.is_empty() {
        "(vide)".to_owned()
    } else {
        short
    }
}

/// Le bilan du critère 6 sur un corpus.
#[derive(Debug, Default)]
struct Verdict {
    /// Pièces `text/calendar` rencontrées.
    parts: usize,
    /// Pièces avec un **instant absolu** : une seconde précise sur la ligne du temps.
    with_instant: usize,
    /// Pièces affichables dont l'instant manque, avec la raison nommée — heure flottante,
    /// fuseau non défini. Lues juste : l'heure murale est la bonne réponse.
    readable_with_caveat: usize,
    /// Pièces **refusées**, avec la raison nommée : il n'y a rien à afficher.
    refused: usize,
    /// Pièces ni plaçables ni refusées — **le cas qui ne doit pas exister**.
    ///
    /// Une pièce qu'on ne sait pas lire *et* dont on ne sait pas dire pourquoi est la seule
    /// panne du critère : l'interface n'aurait alors rien à afficher et rien à expliquer.
    silent: usize,
    /// Combien de pièces par manque, par nom de variante.
    gaps: BTreeMap<String, usize>,
    /// Combien de pièces ont un organisateur lisible.
    with_organizer: usize,
    /// Combien ont au moins un participant.
    with_attendees: usize,
    /// Le total des participants lus, pour que le compte précédent soit interprétable.
    attendees: usize,
    /// Combien ont une fin, écrite ou déduite d'une durée.
    with_end: usize,
    /// Combien ont un titre.
    with_summary: usize,
    /// Combien portent une journée entière.
    all_day: usize,
    /// Combien ont un instant résolu depuis une `VTIMEZONE` embarquée.
    resolved_by_vtimezone: usize,
    /// Combien portent une URL, montrée jamais suivie.
    with_urls: usize,
    /// Combien portent plus d'un `VEVENT`.
    multi_event: usize,
    /// Le temps total de lecture, pour dire ce que ça coûte.
    micros: u128,
    /// La pièce la plus lente, en microsecondes.
    slowest: u128,
    /// Pièces dont le texte contient `VEVENT` alors qu'aucun événement n'a été lu.
    ///
    /// **Le contrôle qui empêche un bogue de se cacher derrière un refus.** « Cette pièce ne
    /// contient aucun rendez-vous » est une réponse acceptable du critère 6 ; elle devient un
    /// mensonge si le rendez-vous est là et que le lecteur ne l'a pas vu. Doit rester à zéro.
    missed_events: usize,
    /// Pièces dont le contenu ne se décode pas en texte, comptées à part.
    undecodable: usize,
}

/// Mesure le **critère 6** : le lecteur passé sur toutes les pièces du corpus.
///
/// ## Ce que « tenu » veut dire ici
///
/// Le critère demande que tout soit lu juste « ou refusé en nommant ce qui manque ». Les deux
/// branches sont des succès ; la panne est la troisième, celle d'une pièce qu'on ne lit pas et
/// dont on ne sait pas dire pourquoi. Le verdict porte donc sur `silencieuses = 0`, et les deux
/// autres colonnes sont là pour qu'on voie de quoi le corpus est fait.
///
/// **Aucune requête réseau ne peut partir d'ici** : `mailcal` n'a aucune dépendance, donc aucun
/// client HTTP, et il n'ouvre rien. Le test d'intégration du dépôt qui verrouille la règle 5 en
/// CI couvre le reste du chemin.
///
/// # Errors
///
/// Si le store est illisible.
pub fn measure(store_root: &Utf8PathBuf, show_gaps: bool) -> Result<()> {
    let store = Store::open(store_root).with_context(|| format!("ouverture de {store_root}"))?;
    let rows = store.all_for_indexing()?;
    anyhow::ensure!(!rows.is_empty(), "store vide");

    let mut verdict = Verdict::default();
    let mut unreadable = 0usize;
    // Les manques, avec l'identifiant du message qui les porte : de quoi aller voir. Bornés,
    // parce que la liste sert à enquêter et pas à tout relire.
    let mut examples: Vec<(i64, String)> = Vec::new();

    for row in &rows {
        let Ok(raw) = store.blobs().read(row.blob) else {
            unreadable += 1;
            continue;
        };
        for text in calendar_parts(&raw) {
            verdict.parts += 1;
            if text.trim().is_empty() {
                // Non décodable : comptée, et pas lue. La compter comme « refusée » gonflerait
                // un chiffre du critère avec un trou de couverture.
                verdict.undecodable += 1;
                continue;
            }
            let at = std::time::Instant::now();
            let invitation = mailcal::read(&text);
            let elapsed = at.elapsed().as_micros();
            verdict.micros += elapsed;
            verdict.slowest = verdict.slowest.max(elapsed);

            // Les trois issues s'excluent, et c'est ce qui rend le relevé lisible : un instant,
            // une heure murale avec sa réserve, ou un refus motivé. La quatrième — ni l'un ni
            // l'autre et rien à dire — est le seul échec du critère.
            let refused = invitation.gaps.iter().any(mailcal::Gap::is_refusal);
            if invitation.has_instant() {
                verdict.with_instant += 1;
            } else if invitation.is_readable() && !refused {
                verdict.readable_with_caveat += 1;
            } else if refused {
                verdict.refused += 1;
            } else {
                verdict.silent += 1;
                examples.push((row.id.0, "illisible sans raison nommée".to_owned()));
            }

            if invitation.gaps.contains(&mailcal::Gap::NoEvent)
                && text.to_ascii_uppercase().contains("VEVENT")
            {
                verdict.missed_events += 1;
                examples.push((row.id.0, "VEVENT présent mais non lu".to_owned()));
            }

            for gap in &invitation.gaps {
                *verdict.gaps.entry(gap_kind(gap)).or_default() += 1;
                if show_gaps && examples.len() < 40 && !invitation.has_instant() {
                    examples.push((row.id.0, gap.to_string()));
                }
            }

            if invitation.organizer.is_some() {
                verdict.with_organizer += 1;
            }
            if !invitation.attendees.is_empty() {
                verdict.with_attendees += 1;
            }
            verdict.attendees += invitation.attendees.len();
            if invitation.end.is_some() {
                verdict.with_end += 1;
            }
            if invitation.summary.is_some() {
                verdict.with_summary += 1;
            }
            if invitation
                .start
                .as_ref()
                .is_some_and(mailcal::Moment::is_all_day)
            {
                verdict.all_day += 1;
            }
            if let Some(mailcal::Zone::Named {
                offset: Some(_), ..
            }) = invitation.start.as_ref().map(|it| &it.zone)
            {
                verdict.resolved_by_vtimezone += 1;
            }
            if !invitation.urls.is_empty() {
                verdict.with_urls += 1;
            }
            if invitation.extra_events > 0 {
                verdict.multi_event += 1;
            }
        }
    }

    report(store_root, &verdict, unreadable, rows.len());
    if show_gaps && !examples.is_empty() {
        println!();
        println!("Manques relevés, avec le message qui les porte :");
        for (id, gap) in examples.iter().take(40) {
            println!("  #{id:<8} {gap}");
        }
    }
    Ok(())
}

/// Écrit le bilan.
fn report(store_root: &Utf8PathBuf, verdict: &Verdict, unreadable: usize, messages: usize) {
    println!("Store                        {store_root}");
    println!("Messages lus                 {messages}");
    if unreadable > 0 {
        println!("Blobs illisibles             {unreadable} — non inspectés");
    }
    println!("Pièces text/calendar         {}", verdict.parts);
    if verdict.parts == 0 {
        println!();
        println!("Aucune invitation dans ce store : le critère 6 n'y est pas mesurable.");
        return;
    }

    let share = |count: usize| {
        #[allow(clippy::cast_precision_loss)]
        let value = count as f64 * 100.0 / verdict.parts as f64;
        format!("{count:>6}   {value:>6.2} %")
    };
    println!();
    println!("Critère 6 — lues juste, ou refusées en nommant ce qui manque");
    println!(
        "  Instant absolu lu          {}",
        share(verdict.with_instant)
    );
    println!(
        "  Heure murale + réserve     {}   (heure flottante, fuseau non défini)",
        share(verdict.readable_with_caveat)
    );
    println!("  Refusées, raison nommée    {}", share(verdict.refused));
    println!(
        "  Illisibles sans raison     {}   ← le seul échec possible",
        share(verdict.silent)
    );
    println!();
    println!("Ce que le lecteur a tiré du corpus :");
    println!(
        "  Titre                      {}",
        share(verdict.with_summary)
    );
    println!(
        "  Organisateur               {}",
        share(verdict.with_organizer)
    );
    println!(
        "  Participants               {}  ({} en tout)",
        share(verdict.with_attendees),
        verdict.attendees
    );
    println!("  Fin connue                 {}", share(verdict.with_end));
    println!("  Journée entière            {}", share(verdict.all_day));
    println!(
        "  Instant depuis VTIMEZONE   {}",
        share(verdict.resolved_by_vtimezone)
    );
    println!("  Porte une URL              {}", share(verdict.with_urls));
    println!(
        "  Plusieurs événements       {}",
        share(verdict.multi_event)
    );

    if !verdict.gaps.is_empty() {
        println!();
        println!("Manques, par nature :");
        let mut sorted: Vec<(&String, &usize)> = verdict.gaps.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (kind, count) in sorted {
            println!("  {kind:<42} {count:>6}");
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let average = verdict.micros as f64 / verdict.parts as f64;
    println!();
    println!(
        "Coût de lecture              {average:.1} µs en moyenne, {} µs au pire",
        verdict.slowest
    );
    if verdict.undecodable > 0 {
        println!(
            "  Pièces non décodables      {:>6}   ← non lues, trou de couverture",
            verdict.undecodable
        );
    }
    println!(
        "  VEVENT présent, non lu     {:>6}   ← doit rester à zéro",
        verdict.missed_events
    );
    println!(
        "Verdict                      {}",
        if verdict.silent == 0 && verdict.missed_events == 0 {
            "passé — aucune pièce illisible sans raison nommée"
        } else {
            "ÉCHOUÉ — des pièces sont illisibles sans raison nommée"
        }
    );
    println!("Requêtes réseau              0 — `mailcal` n'a aucune dépendance");
}

/// Le nom d'une variante de manque, pour l'histogramme.
///
/// Le nom seul, **sans sa valeur** : une date illisible porterait la valeur écrite par
/// l'expéditeur, et l'histogramme d'un corpus entier n'a pas à en être rempli.
fn gap_kind(gap: &mailcal::Gap) -> String {
    match gap {
        mailcal::Gap::NoEvent => "aucun rendez-vous dans la pièce",
        mailcal::Gap::NoStart => "pas de début",
        mailcal::Gap::UnreadableDate { property, .. } => {
            return format!("date illisible ({property})");
        }
        mailcal::Gap::UnknownZone { .. } => "fuseau nommé mais non défini",
        mailcal::Gap::FloatingTime { .. } => "heure sans fuseau",
        mailcal::Gap::NoEnd => "pas de fin",
        mailcal::Gap::UnreadableDuration { .. } => "durée illisible",
        mailcal::Gap::Truncated => "pièce tronquée",
    }
    .to_owned()
}
/// Les pièces `text/calendar` d'un message, décodées en texte.
///
/// ## Pourquoi le parcours est fait ici et pas par `parsed.attachments()`
///
/// Une invitation n'est pas toujours une pièce jointe. Outlook l'envoie en
/// `multipart/alternative` à côté du corps — donc dans le corps, pas dans les pièces — et
/// Google l'envoie **les deux fois** : une part inline et une pièce `invite.ics`. Ne regarder
/// que les pièces jointes en manquerait la moitié, et ne regarder que le corps l'autre.
fn calendar_parts(raw: &[u8]) -> Vec<String> {
    use mail_parser::{MessageParser, MimeHeaders as _};

    let Some(parsed) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in parsed.parts.iter() {
        // `is_content_type` compare sans tenir compte de la casse, ce qui est ce que la RFC
        // 2045 demande : `TEXT/CALENDAR` et `text/calendar` sont le même type.
        if !part.is_content_type("text", "calendar") {
            continue;
        }
        // `part.text_contents()` décode le transfert — base64, quoted-printable — et le jeu de
        // caractères déclaré. Un `.ics` en base64 est le cas ordinaire chez Outlook.
        if let Some(text) = part.text_contents() {
            out.push(text.to_owned());
        } else {
            // Pas de texte exploitable : gardé comme pièce vide plutôt que passé sous silence,
            // pour que le compte des pièces reste celui du corpus.
            out.push(String::new());
        }
    }
    out
}

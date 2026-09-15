//! D'une pièce `text/calendar` à un rendez-vous — ou à un refus qui nomme ce qui manque.
//!
//! ## Le « ou » est la moitié importante du critère 6
//!
//! Le critère demande que les dates, le fuseau, l'organisateur et les participants soient lus
//! **juste** sur toutes les pièces du corpus, « ou refusés en nommant ce qui manque ». Les deux
//! branches sont des succès ; la seule panne est d'afficher faux. [`Invitation::gaps`] est donc
//! aussi importante que le reste : c'est par elle qu'une interface sait dire « cette invitation
//! ne dit pas dans quel fuseau elle est » plutôt que d'inventer une heure.
//!
//! ## Un seul événement, et lequel
//!
//! Une invitation reçue par courrier porte un `VEVENT`, parfois deux quand le producteur joint
//! l'occurrence modifiée d'une série. Ce module lit le **premier** `VEVENT` et compte les
//! autres dans [`Invitation::extra_events`] : afficher un rendez-vous est le besoin, et choisir
//! silencieusement parmi plusieurs serait pire que dire qu'il y en a plusieurs.
//!
//! Les `VTODO`, `VJOURNAL` et `VFREEBUSY` sont ignorés. Ils ne sont pas des rendez-vous, et le
//! corpus n'en porte pas — voir le relevé du journal.

use crate::ics::{Property, unfold};
use crate::time::{Civil, Moment, Observance, Timezone, Zone, parse_offset};

/// Ce que la méthode d'un fichier iCalendar demande au lecteur.
///
/// RFC 5546. Elle change ce que l'interface doit dire : une demande s'affiche comme une
/// invitation, une annulation comme un rendez-vous **annulé**, et une réponse comme la réponse
/// de quelqu'un. Les confondre ferait afficher une réunion annulée comme si elle avait lieu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// `REQUEST` : on vous invite, ou on met à jour une invitation.
    Request,
    /// `CANCEL` : le rendez-vous est annulé.
    Cancel,
    /// `REPLY` : quelqu'un répond à une invitation.
    Reply,
    /// `PUBLISH` : un événement diffusé, sans réponse attendue.
    Publish,
    /// `COUNTER` : une contre-proposition — « pas à 14 h, plutôt à 16 h ».
    ///
    /// Présente dans le corpus réel (deux fois sur 930 pièces), donc lue : la classer en
    /// `Other` la ferait afficher comme une invitation ordinaire, et l'utilisateur croirait
    /// qu'on lui propose le créneau d'origine.
    Counter,
    /// Une méthode que ce lecteur ne connaît pas, ou aucune méthode déclarée.
    Other(String),
}

impl Method {
    /// Lit une valeur de `METHOD`.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_uppercase().as_str() {
            "REQUEST" => Self::Request,
            "CANCEL" => Self::Cancel,
            "REPLY" => Self::Reply,
            "PUBLISH" => Self::Publish,
            "COUNTER" => Self::Counter,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Ce qu'un humain en lit, en français.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Request => "invitation",
            Self::Cancel => "annulation",
            Self::Reply => "réponse",
            Self::Publish => "événement",
            Self::Counter => "contre-proposition",
            Self::Other(_) => "pièce de calendrier",
        }
    }
}

/// Où en est un participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// N'a pas encore répondu.
    Pending,
    /// Accepte.
    Accepted,
    /// Refuse.
    Declined,
    /// Accepte sous réserve.
    Tentative,
    /// A délégué, ou un état que ce lecteur ne connaît pas.
    Other,
}

impl Answer {
    /// Lit un `PARTSTAT`.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value.unwrap_or_default().to_ascii_uppercase().as_str() {
            "ACCEPTED" => Self::Accepted,
            "DECLINED" => Self::Declined,
            "TENTATIVE" => Self::Tentative,
            "NEEDS-ACTION" | "" => Self::Pending,
            _ => Self::Other,
        }
    }

    /// Ce qu'un humain en lit.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "sans réponse",
            Self::Accepted => "accepte",
            Self::Declined => "refuse",
            Self::Tentative => "peut-être",
            Self::Other => "autre",
        }
    }
}

/// Un organisateur ou un participant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    /// Le nom affiché, s'il y en a un — le paramètre `CN`.
    pub name: Option<String>,
    /// L'adresse, sans son `mailto:`. Absente quand la valeur n'est pas une adresse de courrier.
    pub address: Option<String>,
    /// Où en est sa réponse. Sans objet pour un organisateur, qui vaut alors `Pending`.
    pub answer: Answer,
    /// Vrai si sa présence est demandée et non optionnelle.
    pub required: bool,
}

impl Person {
    /// Lit un `ORGANIZER` ou un `ATTENDEE`.
    ///
    /// ## La valeur n'est pas forcément une adresse
    ///
    /// RFC 5545 : c'est un `CAL-ADDRESS`, donc une URI. `mailto:` est ce que tout le monde
    /// écrit, mais le corpus contient aussi des salles de réunion en `urn:` et des identifiants
    /// internes de serveur. Une valeur qui n'est pas un `mailto:` est donc gardée **comme
    /// nom** quand il n'y en a pas d'autre, et jamais présentée comme une adresse : écrire à
    /// `urn:x-resource:salle-12` n'irait nulle part.
    #[must_use]
    pub fn parse(property: &Property) -> Self {
        let raw = property.value.trim();
        let address = raw
            .strip_prefix("mailto:")
            .or_else(|| raw.strip_prefix("MAILTO:"))
            .map(str::trim)
            .filter(|it| it.contains('@'))
            .map(str::to_owned);
        let name = property
            .parameter("CN")
            .map(str::trim)
            .filter(|it| !it.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                // Pas de `CN` et pas d'adresse : la valeur brute est tout ce qu'on a.
                if address.is_none() && !raw.is_empty() {
                    Some(raw.to_owned())
                } else {
                    None
                }
            });
        let role = property.parameter("ROLE").unwrap_or("REQ-PARTICIPANT");
        Self {
            name,
            address,
            answer: Answer::parse(property.parameter("PARTSTAT")),
            required: !role.eq_ignore_ascii_case("OPT-PARTICIPANT")
                && !role.eq_ignore_ascii_case("NON-PARTICIPANT"),
        }
    }

    /// Ce qu'on affiche : le nom, à défaut l'adresse.
    #[must_use]
    pub fn label(&self) -> &str {
        self.name
            .as_deref()
            .or(self.address.as_deref())
            .unwrap_or("(inconnu)")
    }
}

/// Ce qui manque, ou ce qui n'a pas pu être lu.
///
/// **Nommé, jamais deviné.** Chaque variante correspond à quelque chose qu'une interface peut
/// dire à l'utilisateur en une phrase, et c'est la moitié du critère 6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gap {
    /// Aucun `VEVENT` : ce n'est pas un rendez-vous.
    NoEvent,
    /// Pas de `DTSTART` : il n'y a pas de rendez-vous sans début.
    NoStart,
    /// Une date qu'on n'a pas su lire, nommée avec sa valeur.
    UnreadableDate {
        /// La propriété fautive, `DTSTART` par exemple.
        property: String,
        /// La valeur telle qu'écrite, pour qu'un humain voie ce qui cloche.
        value: String,
    },
    /// Un `TZID` que le fichier nomme sans le définir : l'instant reste inconnu.
    UnknownZone {
        /// Le fuseau nommé.
        id: String,
    },
    /// Une heure sans fuseau : elle désigne « l'heure locale », donc aucun instant précis.
    ///
    /// **Une remarque, pas un refus.** RFC 5545 §3.3.5 : une heure flottante est celle de qui
    /// la lit, donc l'heure murale *est* la bonne réponse. Ce qui doit être dit à l'utilisateur
    /// est qu'elle n'est rattachée à aucun fuseau — voir [`Gap::is_refusal`].
    FloatingTime {
        /// La propriété concernée.
        property: String,
    },
    /// Ni `DTEND` ni `DURATION` : la fin est inconnue.
    NoEnd,
    /// Une `DURATION` qu'on n'a pas su lire.
    UnreadableDuration {
        /// La valeur telle qu'écrite.
        value: String,
    },
    /// Le fichier a été tronqué à la lecture : voir [`crate::ics::MAX_LINES`].
    Truncated,
}

impl Gap {
    /// Vrai si ce manque empêche d'afficher le rendez-vous.
    ///
    /// ## La distinction que l'interface a besoin de faire
    ///
    /// Tous les manques ne se valent pas. Sans début, il n'y a rien à montrer — c'est un refus.
    /// Sans fin, ou sans fuseau, il reste un rendez-vous parfaitement affichable **assorti
    /// d'une phrase** : « 14:00, heure locale » se lit, « rien » ne se lit pas.
    ///
    /// Les mélanger ferait afficher « invitation illisible » sur les 27 pièces à heure
    /// flottante du corpus réel, que tous les autres clients montrent à leur heure murale.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        match self {
            Self::NoEvent | Self::NoStart | Self::Truncated => true,
            // Une date de **début** illisible est un refus ; une fin illisible n'empêche rien.
            Self::UnreadableDate { property, .. } => property.eq_ignore_ascii_case("DTSTART"),
            // L'heure murale et le nom du fuseau restent lisibles ; seul l'instant manque.
            Self::UnknownZone { .. }
            | Self::FloatingTime { .. }
            | Self::NoEnd
            | Self::UnreadableDuration { .. } => false,
        }
    }
}

impl std::fmt::Display for Gap {
    /// Une phrase par manque, en français, prête à afficher.
    ///
    /// Ces phrases sont ce que l'utilisateur lira à la place d'une heure inventée. Elles disent
    /// **ce qui manque au fichier**, et non « erreur » : le fichier n'est pas invalide, il est
    /// incomplet, et la nuance est ce qui permet à quelqu'un de demander la bonne chose à
    /// l'organisateur.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEvent => write!(f, "cette pièce ne contient aucun rendez-vous"),
            Self::NoStart => write!(f, "l'invitation ne dit pas quand le rendez-vous commence"),
            Self::UnreadableDate { property, value } => {
                write!(f, "la date {property} est illisible : « {value} »")
            }
            Self::UnknownZone { id } => write!(
                f,
                "le fuseau « {id} » est nommé mais pas défini dans l'invitation : \
                 l'heure exacte ne peut pas être calculée"
            ),
            Self::FloatingTime { property } => write!(
                f,
                "la date {property} n'a pas de fuseau : elle désigne l'heure locale du lecteur"
            ),
            Self::NoEnd => write!(f, "l'invitation ne dit pas quand le rendez-vous finit"),
            Self::UnreadableDuration { value } => {
                write!(f, "la durée est illisible : « {value} »")
            }
            Self::Truncated => write!(f, "la pièce est trop longue : elle a été tronquée"),
        }
    }
}

/// Un rendez-vous lu dans une pièce `text/calendar`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitation {
    /// Ce que le fichier demande : invitation, annulation, réponse.
    pub method: Method,
    /// Le titre, déséchappé. `None` quand la propriété est absente.
    pub summary: Option<String>,
    /// Le début, quand il y en a un.
    pub start: Option<Moment>,
    /// La fin, quand elle est écrite ou déductible d'une durée.
    pub end: Option<Moment>,
    /// Le lieu, déséchappé et tel qu'écrit.
    pub location: Option<String>,
    /// L'organisateur.
    pub organizer: Option<Person>,
    /// Les participants, dans l'ordre du fichier.
    pub attendees: Vec<Person>,
    /// La description, déséchappée et **jamais interprétée** : ni HTML, ni liens cliquables.
    pub description: Option<String>,
    /// L'identifiant de l'événement, qui relie une mise à jour à l'invitation d'origine.
    pub uid: Option<String>,
    /// Le numéro de révision : une invitation renvoyée corrigée porte un numéro plus grand.
    pub sequence: Option<u64>,
    /// La règle de répétition, **telle qu'écrite**. Affichée, jamais développée en occurrences.
    pub recurrence: Option<String>,
    /// L'occurrence visée, quand l'invitation ne porte que sur une date d'une série.
    ///
    /// 215 pièces du corpus en ont une : c'est « la réunion du 12, exceptionnellement à 15 h ».
    /// Sans la lire, cette invitation s'afficherait comme si elle déplaçait toute la série.
    pub recurrence_id: Option<Moment>,
    /// L'état déclaré de l'événement : `CONFIRMED`, `TENTATIVE`, `CANCELLED`.
    ///
    /// ## Il ne dit pas la même chose que la méthode, et il faut les deux
    ///
    /// `METHOD:CANCEL` est ce que **le message** demande ; `STATUS:CANCELLED` est ce que
    /// **l'événement** est. Un producteur peut envoyer une mise à jour (`REQUEST`) d'un
    /// événement annulé, et 857 pièces du corpus portent un `STATUS`. Ne lire que la méthode
    /// afficherait un rendez-vous annulé comme s'il avait lieu.
    pub status: Option<String>,
    /// Les URL que l'invitation porte — conférence, organisateur, agenda.
    ///
    /// **Rendues pour être montrées, jamais suivies.** `docs/PRIVACY.md`, règle 5 : rien ne part
    /// tant que l'utilisateur n'a pas cliqué, et ce crate n'a de toute façon rien pour partir.
    pub urls: Vec<String>,
    /// Le nombre de `VEVENT` au-delà du premier.
    pub extra_events: usize,
    /// Ce qui manque, ou n'a pas pu être lu.
    pub gaps: Vec<Gap>,
}

impl Invitation {
    /// Vrai si le rendez-vous a un **instant absolu** — une seconde précise sur la ligne du
    /// temps.
    ///
    /// C'est ce qu'il faut pour poser un rappel, trier par date, ou comparer deux rendez-vous.
    /// Une journée entière compte : son instant est sa date.
    #[must_use]
    pub fn has_instant(&self) -> bool {
        self.start
            .as_ref()
            .is_some_and(|start| start.unix.is_some() || start.is_all_day())
    }

    /// Vrai si le rendez-vous peut être **affiché** à une date et une heure.
    ///
    /// ## Pourquoi ce n'est pas la même question que l'instant
    ///
    /// Deux cas ont une heure murale parfaitement lisible sans avoir d'instant absolu :
    ///
    /// - une **heure flottante** — sans `TZID` ni `Z`. RFC 5545 §3.3.5 : elle désigne l'heure
    ///   locale de qui la lit, donc « 14:00 » *est* la réponse. 27 pièces du corpus en portent,
    ///   et les refuser afficherait « illisible » sur des invitations que tout autre client
    ///   montre à 14:00 ;
    /// - un **fuseau nommé mais non défini** : l'heure murale et le nom du fuseau se lisent,
    ///   seul le décalage manque.
    ///
    /// Dans les deux cas l'interface a de quoi afficher, **à condition de dire lequel** — et
    /// c'est ce que le `Gap` correspondant lui donne. La panne serait d'afficher « 14:00 » sans
    /// dire que ce 14:00 est peut-être celui d'un autre pays.
    #[must_use]
    pub fn is_readable(&self) -> bool {
        self.start.is_some()
    }
}

/// Lit une pièce `text/calendar`.
///
/// Ne peut pas échouer : une pièce vide, tronquée, ou qui n'est pas du calendrier rend une
/// invitation dont les `gaps` disent pourquoi. C'est la règle de tous les analyseurs d'entrée
/// hostile du dépôt, et ici elle a une conséquence de plus : une invitation illisible ne doit
/// pas empêcher d'**afficher le message** qui la porte.
#[must_use]
pub fn read(text: &str) -> Invitation {
    let lines = unfold(text);
    let mut gaps = Vec::new();
    if lines.len() >= crate::ics::MAX_LINES {
        gaps.push(Gap::Truncated);
    }

    // Premier passage : les fuseaux. Ils doivent être connus **avant** de résoudre une date,
    // et rien ne garantit qu'ils soient écrits avant dans le fichier — Google les met après
    // l'événement.
    let zones = timezones(&lines);

    let mut method = Method::Other(String::new());
    let mut summary = None;
    let mut location = None;
    let mut description = None;
    let mut uid = None;
    let mut sequence = None;
    let mut recurrence = None;
    let mut recurrence_raw = None;
    let mut status = None;
    let mut organizer = None;
    let mut attendees = Vec::new();
    let mut urls = Vec::new();
    let mut start_raw = None;
    let mut end_raw = None;
    let mut duration = None;
    let mut events = 0usize;
    // Vrai pendant qu'on est **dans** le premier `VEVENT`. Sans ce suivi, une `SUMMARY` de
    // `VTIMEZONE` ou d'un second événement écraserait celle du rendez-vous.
    let mut inside = false;
    let mut depth_other = 0usize;

    for line in &lines {
        let Some(property) = Property::parse(line) else {
            continue;
        };
        if property.is("BEGIN") {
            let component = property.value.trim().to_ascii_uppercase();
            if component == "VEVENT" {
                events += 1;
                inside = events == 1;
            } else if inside {
                // Un composant imbriqué dans l'événement — une `VALARM`. Ses propriétés ne sont
                // pas celles du rendez-vous : un `DTSTART` d'alarme est le moment du rappel.
                depth_other += 1;
            }
            continue;
        }
        if property.is("END") {
            let component = property.value.trim().to_ascii_uppercase();
            if component == "VEVENT" {
                inside = false;
            } else {
                depth_other = depth_other.saturating_sub(1);
            }
            continue;
        }
        // La méthode est au niveau du calendrier, hors événement.
        if property.is("METHOD") && !inside {
            method = Method::parse(&property.value);
            continue;
        }
        if !inside || depth_other > 0 {
            continue;
        }

        match property.name.to_ascii_uppercase().as_str() {
            "SUMMARY" => summary = Some(property.text()),
            "LOCATION" => location = Some(property.text()),
            "DESCRIPTION" => description = Some(property.text()),
            "UID" => uid = Some(property.value.trim().to_owned()),
            "SEQUENCE" => sequence = property.value.trim().parse().ok(),
            "RRULE" => recurrence = Some(property.value.trim().to_owned()),
            "RECURRENCE-ID" => recurrence_raw = Some(property),
            "STATUS" => status = Some(property.value.trim().to_ascii_uppercase()),
            // Une pièce jointe d'événement est souvent une URL — un lien de visioconférence.
            "ATTACH" => collect_url(&property, &mut urls),
            "ORGANIZER" => {
                organizer = Some(Person::parse(&property));
                collect_url(&property, &mut urls);
            }
            "ATTENDEE" => {
                attendees.push(Person::parse(&property));
                collect_url(&property, &mut urls);
            }
            "URL" => collect_url(&property, &mut urls),
            "DTSTART" => start_raw = Some(property),
            "DTEND" => end_raw = Some(property),
            "DURATION" => duration = Some(property.value.trim().to_owned()),
            _ => {}
        }
    }

    if events == 0 {
        gaps.push(Gap::NoEvent);
    }

    let start = start_raw
        .as_ref()
        .and_then(|property| moment(property, &zones, &mut gaps));
    if start_raw.is_none() && events > 0 {
        gaps.push(Gap::NoStart);
    }

    let end = match (&end_raw, &duration, &start) {
        (Some(property), _, _) => moment(property, &zones, &mut gaps),
        (None, Some(text), Some(start)) => match parse_duration(text) {
            Some(seconds) => Some(shift(start, seconds)),
            None => {
                gaps.push(Gap::UnreadableDuration {
                    value: text.clone(),
                });
                None
            }
        },
        _ => None,
    };
    if end.is_none() && events > 0 && start.is_some() {
        gaps.push(Gap::NoEnd);
    }

    // Les URL que la description porte sont relevées aussi : c'est là que les producteurs
    // mettent le lien de visioconférence, et c'est ce que l'utilisateur cherche.
    if let Some(text) = &description {
        for found in urls_in(text) {
            if !urls.contains(&found) {
                urls.push(found);
            }
        }
    }

    let recurrence_id = recurrence_raw
        .as_ref()
        .and_then(|property| moment(property, &zones, &mut gaps));

    Invitation {
        method,
        summary,
        start,
        end,
        location,
        organizer,
        attendees,
        description,
        uid,
        sequence,
        recurrence,
        recurrence_id,
        status,
        urls,
        extra_events: events.saturating_sub(1),
        gaps,
    }
}

/// Résout une propriété de date en instant, et note ce qui manque.
fn moment(property: &Property, zones: &[Timezone], gaps: &mut Vec<Gap>) -> Option<Moment> {
    let name = property.name.to_ascii_uppercase();
    let Some((wall, utc, date_only)) = Civil::parse(&property.value) else {
        gaps.push(Gap::UnreadableDate {
            property: name,
            value: property.value.trim().to_owned(),
        });
        return None;
    };

    // `VALUE=DATE` est la façon déclarée d'écrire une journée entière ; une valeur sans heure
    // l'est aussi, en pratique. Les deux comptent.
    let all_day = date_only
        || property
            .parameter("VALUE")
            .is_some_and(|it| it.eq_ignore_ascii_case("DATE"));
    if all_day {
        return Some(Moment {
            wall,
            zone: Zone::AllDay,
            // L'instant d'une journée entière est son début à minuit, sans fuseau : c'est ce
            // qu'une journée entière veut dire, et le fuseau du lecteur ne la déplace pas.
            unix: Some(wall.as_unix_utc()),
        });
    }
    if utc {
        return Some(Moment {
            wall,
            zone: Zone::Utc,
            unix: Some(wall.as_unix_utc()),
        });
    }
    match property.parameter("TZID") {
        Some(id) => {
            let offset = zones
                .iter()
                .find(|zone| zone.id.eq_ignore_ascii_case(id))
                .and_then(|zone| zone.offset_at(&wall));
            if offset.is_none() {
                gaps.push(Gap::UnknownZone { id: id.to_owned() });
            }
            Some(Moment {
                wall,
                zone: Zone::Named {
                    id: id.to_owned(),
                    offset,
                },
                // L'heure murale moins le décalage donne l'instant : 14:00 en +02:00 est
                // 12:00 UTC.
                unix: offset.map(|offset| wall.as_unix_utc() - i64::from(offset)),
            })
        }
        None => {
            gaps.push(Gap::FloatingTime { property: name });
            Some(Moment {
                wall,
                zone: Zone::Floating,
                unix: None,
            })
        }
    }
}

/// Décale un instant d'une durée, en gardant son fuseau et son heure murale d'accord.
///
/// ## Le décalage du fuseau est celui du **début**
///
/// Une réunion qui enjambe le changement d'heure finirait donc affichée avec une heure de fin
/// fausse d'une heure. C'est connu, c'est rare — il faut une réunion qui traverse 3 h du matin
/// le dernier dimanche de mars — et le corriger demanderait de porter le fuseau entier dans
/// chaque `Moment` au lieu d'un décalage. L'instant, lui, reste juste dans tous les cas : c'est
/// lui qui compte pour placer le rendez-vous.
fn shift(from: &Moment, seconds: i64) -> Moment {
    let offset = match &from.zone {
        Zone::Named { offset, .. } => offset.map_or(0, i64::from),
        _ => 0,
    };
    let unix = from.unix.map(|it| it + seconds);
    // L'heure murale se recalcule depuis l'instant décalé : sans ça, une réunion d'une heure
    // commencée à 14:00 finirait « à 14:00 ». Sans instant — heure flottante — l'arithmétique
    // se fait sur l'heure murale elle-même, qui est tout ce qu'on a.
    let wall = match unix {
        Some(unix) => civil_from_unix(unix + offset),
        None => civil_from_unix(from.wall.as_unix_utc() + seconds),
    };
    Moment {
        wall,
        zone: from.zone.clone(),
        unix,
    }
}

/// La date civile d'un instant traité comme de l'UTC.
///
/// L'inverse de [`Civil::as_unix_utc`], et le même algorithme lu à l'envers.
fn civil_from_unix(unix: i64) -> Civil {
    let days = unix.div_euclid(86_400);
    let rest = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    Civil {
        year,
        month,
        day,
        hour: (rest / 3_600) as u32,
        minute: ((rest % 3_600) / 60) as u32,
        second: (rest % 60) as u32,
    }
}

/// Jours depuis l'époque vers date civile, algorithme de Howard Hinnant.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_shifted + 2) / 5 + 1;
    let month = if month_shifted < 10 {
        month_shifted + 3
    } else {
        month_shifted - 9
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (
        (year + i64::from(month <= 2)) as i32,
        month as u32,
        day as u32,
    )
}

/// Lit une `DURATION` de la RFC 5545 §3.3.6 : `PT1H30M`, `P1D`, `-PT15M`.
///
/// Les semaines et les jours sont acceptés ; les mois et les années n'existent pas dans ce
/// format, précisément parce qu'ils n'ont pas de durée fixe.
fn parse_duration(value: &str) -> Option<i64> {
    let value = value.trim();
    let (sign, rest) = match value.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix(['P', 'p'])?;
    let mut seconds = 0i64;
    let mut number = String::new();
    let mut in_time = false;
    let mut seen = false;
    for character in rest.chars() {
        match character {
            'T' | 't' => in_time = true,
            '0'..='9' => number.push(character),
            unit => {
                let count: i64 = number.parse().ok()?;
                number.clear();
                let factor = match (unit, in_time) {
                    ('W' | 'w', _) => 604_800,
                    ('D' | 'd', _) => 86_400,
                    ('H' | 'h', true) => 3_600,
                    ('M' | 'm', true) => 60,
                    ('S' | 's', true) => 1,
                    // `M` hors partie horaire serait des mois, que ce format n'a pas.
                    _ => return None,
                };
                seconds = seconds.checked_add(count.checked_mul(factor)?)?;
                seen = true;
            }
        }
    }
    // Un nombre sans unité à la fin — `PT1` — est une durée qu'on ne sait pas lire.
    if !number.is_empty() || !seen {
        return None;
    }
    Some(sign * seconds)
}

/// Une observance en cours de lecture, avant d'être complète.
///
/// Une structure et non un tuple : à six champs dont trois `Option` du même type, une
/// inversion passerait le compilateur sans broncher — et une inversion de `TZOFFSETTO` et
/// `TZOFFSETFROM` décale tout d'une heure.
#[derive(Debug, Default)]
struct Partial {
    is_dst: bool,
    offset_to: Option<i32>,
    offset_from: Option<i32>,
    starts: Option<Civil>,
    month: Option<u32>,
    weekday: Option<(i32, u32)>,
}

/// Les fuseaux définis par le fichier.
fn timezones(lines: &[String]) -> Vec<Timezone> {
    let mut out: Vec<Timezone> = Vec::new();
    let mut id: Option<String> = None;
    let mut observances: Vec<Observance> = Vec::new();
    // L'observance en cours de lecture : `STANDARD` ou `DAYLIGHT`.
    let mut current: Option<Partial> = None;
    let mut inside = false;

    for line in lines {
        let Some(property) = Property::parse(line) else {
            continue;
        };
        let value = property.value.trim().to_ascii_uppercase();
        if property.is("BEGIN") {
            match value.as_str() {
                "VTIMEZONE" => {
                    inside = true;
                    id = None;
                    observances = Vec::new();
                }
                "STANDARD" if inside => current = Some(Partial::default()),
                "DAYLIGHT" if inside => {
                    current = Some(Partial {
                        is_dst: true,
                        ..Partial::default()
                    });
                }
                _ => {}
            }
            continue;
        }
        if property.is("END") {
            match value.as_str() {
                "VTIMEZONE" if inside => {
                    if let Some(id) = id.take() {
                        out.push(Timezone {
                            id,
                            observances: std::mem::take(&mut observances),
                        });
                    }
                    inside = false;
                }
                "STANDARD" | "DAYLIGHT" => {
                    if let Some(partial) = current.take() {
                        // Un `TZOFFSETTO` manquant rend l'observance inutilisable : sans lui, il
                        // n'y a rien à appliquer. Elle est jetée, et la date qui s'y réfère
                        // tombera dans `Gap::UnknownZone` — ce qui est la bonne réponse.
                        if let Some(offset_to) = partial.offset_to {
                            observances.push(Observance {
                                offset_to,
                                offset_from: partial.offset_from.unwrap_or(offset_to),
                                starts: partial.starts.unwrap_or_default(),
                                month: partial.month,
                                weekday: partial.weekday,
                                is_dst: partial.is_dst,
                            });
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if !inside {
            continue;
        }
        if property.is("TZID") && current.is_none() {
            id = Some(property.value.trim().to_owned());
            continue;
        }
        let Some(state) = current.as_mut() else {
            continue;
        };
        match property.name.to_ascii_uppercase().as_str() {
            "TZOFFSETTO" => state.offset_to = parse_offset(&property.value),
            "TZOFFSETFROM" => state.offset_from = parse_offset(&property.value),
            "DTSTART" => state.starts = Civil::parse(&property.value).map(|(civil, _, _)| civil),
            "RRULE" => {
                let (month, weekday) = yearly_rule(&property.value);
                state.month = month;
                state.weekday = weekday;
            }
            _ => {}
        }
    }
    out
}

/// Lit le mois et le jour visés d'une `RRULE` annuelle de fuseau.
///
/// Seule la forme utilisée par les fuseaux est lue : `FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU`. Une
/// autre forme rend `(None, None)`, donc une observance sans répétition — et
/// [`Observance::transition`] retombe alors sur son `DTSTART`, ce qui est le comportement
/// honnête pour une règle qu'on ne comprend pas.
fn yearly_rule(value: &str) -> (Option<u32>, Option<(i32, u32)>) {
    let mut month = None;
    let mut weekday = None;
    for piece in value.split(';') {
        let Some((key, raw)) = piece.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "BYMONTH" => {
                month = raw
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|it| (1..=12).contains(it))
            }
            "BYDAY" => weekday = parse_byday(raw.trim()),
            _ => {}
        }
    }
    (month, weekday)
}

/// Lit un `BYDAY` de fuseau : `-1SU`, `2SU`, `SU`.
fn parse_byday(value: &str) -> Option<(i32, u32)> {
    // Le premier jour d'une liste : un fuseau n'en a qu'un, et une liste viendrait d'un
    // producteur qui écrit autre chose qu'une règle de fuseau.
    let value = value.split(',').next()?.trim();
    let at = value.len().checked_sub(2)?;
    let (rank, day) = value.split_at(at);
    let weekday = match day.to_ascii_uppercase().as_str() {
        "SU" => 0,
        "MO" => 1,
        "TU" => 2,
        "WE" => 3,
        "TH" => 4,
        "FR" => 5,
        "SA" => 6,
        _ => return None,
    };
    let rank = if rank.is_empty() {
        1
    } else {
        rank.parse::<i32>().ok()?
    };
    Some((rank, weekday))
}

/// Relève l'URL d'une propriété — la valeur elle-même, ou son paramètre.
fn collect_url(property: &Property, urls: &mut Vec<String>) {
    let mut candidates = Vec::new();
    if is_web_url(property.value.trim()) {
        candidates.push(property.value.trim().to_owned());
    }
    // Les producteurs mettent le lien de visioconférence dans un paramètre `X-` de
    // l'organisateur, ou dans `ALTREP`.
    for (_, value) in &property.params {
        if is_web_url(value.trim()) {
            candidates.push(value.trim().to_owned());
        }
    }
    for url in candidates {
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
}

/// Les URL d'un texte libre.
fn urls_in(text: &str) -> Vec<String> {
    text.split(|it: char| it.is_whitespace() || it == '<' || it == '>' || it == '"')
        .map(|piece| piece.trim_end_matches([',', '.', ';', ')', ']']))
        .filter(|piece| is_web_url(piece))
        .map(str::to_owned)
        .collect()
}

/// Vrai pour une URL qu'une interface peut proposer d'ouvrir.
///
/// `http` et `https` seulement. Un `javascript:` ou un `file:` dans une invitation n'a rien à
/// faire dans une liste de liens proposés : c'est la même liste blanche que celle des liens
/// sortants de `mailhtml::rich`, et pour la même raison.
fn is_web_url(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    (lowered.starts_with("http://") || lowered.starts_with("https://")) && value.len() > 10
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Answer, Gap, Method, read};
    use crate::time::Zone;

    /// Une invitation de la forme que le corpus porte le plus : Exchange, un fuseau nommé à la
    /// windows, et sa `VTIMEZONE` embarquée.
    ///
    /// « Romance Standard Time » est le nom Windows de l'heure d'Europe centrale, et c'est le
    /// `TZID` le plus fréquent du corpus réel — 1 079 occurrences. Aucune base de fuseaux ne
    /// connaît ce nom : c'est la `VTIMEZONE` du fichier qui le définit, et c'est tout l'intérêt
    /// de la lire.
    fn exchange() -> String {
        [
            "BEGIN:VCALENDAR",
            "PRODID:-//Microsoft Exchange Server 2010",
            "VERSION:2.0",
            "METHOD:REQUEST",
            "BEGIN:VTIMEZONE",
            "TZID:Romance Standard Time",
            "BEGIN:STANDARD",
            "DTSTART:16011028T030000",
            "TZOFFSETFROM:+0200",
            "TZOFFSETTO:+0100",
            "RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=10",
            "END:STANDARD",
            "BEGIN:DAYLIGHT",
            "DTSTART:16010325T020000",
            "TZOFFSETFROM:+0100",
            "TZOFFSETTO:+0200",
            "RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=3",
            "END:DAYLIGHT",
            "END:VTIMEZONE",
            "BEGIN:VEVENT",
            "UID:040000008200E00074C5B7101A82E008",
            "SUMMARY:Réunion de suivi",
            "DTSTART;TZID=Romance Standard Time:20260910T140000",
            "DTEND;TZID=Romance Standard Time:20260910T150000",
            "LOCATION:Salle Jaurès\\, 2e étage",
            "ORGANIZER;CN=\"Durand, Éloïse\":mailto:eloise@exemple.fr",
            "ATTENDEE;CN=Jean Martin;PARTSTAT=ACCEPTED;ROLE=REQ-PARTICIPANT:mailto:jean@x.fr",
            "ATTENDEE;CN=Salle 12;ROLE=OPT-PARTICIPANT;PARTSTAT=NEEDS-ACTION:mailto:s12@x.fr",
            "SEQUENCE:3",
            "STATUS:CONFIRMED",
            "BEGIN:VALARM",
            "ACTION:DISPLAY",
            "TRIGGER:-PT15M",
            "DTSTART:20260910T134500",
            "END:VALARM",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n")
    }

    #[test]
    fn an_exchange_invitation_is_read_whole() {
        let it = read(&exchange());
        assert_eq!(it.method, Method::Request);
        assert_eq!(it.summary.as_deref(), Some("Réunion de suivi"));
        // Le lieu est déséchappé : sans ça, l'utilisateur lit une virgule précédée d'une
        // contre-oblique.
        assert_eq!(it.location.as_deref(), Some("Salle Jaurès, 2e étage"));
        assert_eq!(it.sequence, Some(3));
        assert_eq!(it.status.as_deref(), Some("CONFIRMED"));
        assert!(it.gaps.is_empty(), "{:?}", it.gaps);
        assert!(it.has_instant());

        // **L'organisateur, dont le nom contient une virgule entre guillemets.**
        let organizer = it.organizer.as_ref().unwrap();
        assert_eq!(organizer.name.as_deref(), Some("Durand, Éloïse"));
        assert_eq!(organizer.address.as_deref(), Some("eloise@exemple.fr"));

        assert_eq!(it.attendees.len(), 2);
        assert_eq!(it.attendees[0].answer, Answer::Accepted);
        assert!(it.attendees[0].required);
        assert_eq!(it.attendees[1].answer, Answer::Pending);
        assert!(!it.attendees[1].required, "une salle est optionnelle ici");
    }

    #[test]
    fn the_instant_comes_from_the_embedded_timezone_and_not_from_a_guess() {
        // **Le calcul qui décide de l'heure affichée.** Le 10 septembre est en heure d'été, donc
        // +02:00 : 14:00 locale est 12:00 UTC. Un lecteur qui appliquerait +01:00 — l'heure
        // d'hiver, ou le décalage « du fuseau » sans regarder la date — afficherait 13:00.
        let it = read(&exchange());
        let start = it.start.as_ref().unwrap();
        assert_eq!(start.wall.hour, 14, "l'heure murale est celle du fichier");
        match &start.zone {
            Zone::Named { id, offset } => {
                assert_eq!(id, "Romance Standard Time");
                assert_eq!(*offset, Some(7_200), "septembre est en +02:00");
            }
            other => panic!("fuseau inattendu : {other:?}"),
        }
        // 2026-09-10T12:00:00Z.
        assert_eq!(start.unix, Some(20_706 * 86_400 + 12 * 3_600));
        // Et la fin est une heure plus tard, à la même heure murale + 1.
        let end = it.end.as_ref().unwrap();
        assert_eq!(end.wall.hour, 15);
        assert_eq!(end.unix, Some(start.unix.unwrap() + 3_600));
    }

    #[test]
    fn the_same_invitation_in_winter_gets_the_winter_offset() {
        // Le contrôle qui prouve que la date entre dans le calcul : la même invitation en
        // décembre doit donner +01:00. Sans lui, un décalage figé passerait le test précédent.
        let winter = exchange().replace("20260910", "20261210");
        let it = read(&winter);
        let start = it.start.as_ref().unwrap();
        match &start.zone {
            Zone::Named { offset, .. } => {
                assert_eq!(*offset, Some(3_600), "décembre est en +01:00")
            }
            other => panic!("fuseau inattendu : {other:?}"),
        }
        // 14:00 locale en +01:00 est 13:00 UTC.
        let expected = crate::time::Civil {
            year: 2026,
            month: 12,
            day: 10,
            hour: 13,
            minute: 0,
            second: 0,
        };
        assert_eq!(start.unix, Some(expected.as_unix_utc()));
    }

    #[test]
    fn an_alarm_inside_the_event_does_not_become_the_appointment() {
        // La `VALARM` de la pièce porte son propre `DTSTART` — le moment du rappel, quinze
        // minutes avant. Le prendre pour le début du rendez-vous l'afficherait à 13:45.
        let it = read(&exchange());
        assert_eq!(it.start.as_ref().unwrap().wall.hour, 14);
    }

    #[test]
    fn a_google_invitation_in_utc_is_read_without_any_timezone() {
        let text = [
            "BEGIN:VCALENDAR",
            "PRODID:-//Google Inc//Google Calendar 70.9054//EN",
            "METHOD:REQUEST",
            "BEGIN:VEVENT",
            "DTSTART:20260910T120000Z",
            "DTEND:20260910T130000Z",
            "SUMMARY:Point hebdo",
            "DESCRIPTION:Rejoindre : https://meet.example.com/abc-defg\\nUn lien de plus.",
            "ORGANIZER;CN=Éloïse:mailto:eloise@exemple.fr",
            "UID:abc123@google.com",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        assert!(it.gaps.is_empty(), "{:?}", it.gaps);
        assert_eq!(it.start.as_ref().unwrap().zone, Zone::Utc);
        assert_eq!(
            it.start.as_ref().unwrap().unix,
            Some(20_706 * 86_400 + 12 * 3_600)
        );
        // L'URL de visioconférence est relevée depuis la description : c'est ce que
        // l'utilisateur cherche dans une invitation.
        assert_eq!(
            it.urls,
            vec!["https://meet.example.com/abc-defg".to_owned()]
        );
    }

    #[test]
    fn no_url_of_a_scheme_that_should_not_be_offered_is_collected() {
        // La même liste blanche que les liens sortants de `mailhtml::rich`, et pour la même
        // raison : une interface qui propose d'ouvrir ce lien ne doit pas proposer un piège.
        let text = [
            "BEGIN:VCALENDAR",
            "BEGIN:VEVENT",
            "DTSTART:20260910T120000Z",
            "URL:javascript:alert(1)",
            "DESCRIPTION:file:///etc/passwd et ftp://x.fr/y et https://bon.example/ok",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        assert_eq!(it.urls, vec!["https://bon.example/ok".to_owned()]);
    }

    #[test]
    fn a_floating_time_is_read_as_local_and_said_to_be_local() {
        // **27 pièces du corpus réel.** RFC 5545 §3.3.5 : sans `TZID` ni `Z`, l'heure est celle
        // de qui la lit — donc « 14:00 » est la bonne réponse, et il n'y a pas d'instant absolu.
        // Les refuser afficherait « illisible » sur des invitations que tout autre client montre.
        let text = [
            "BEGIN:VCALENDAR",
            "BEGIN:VEVENT",
            "DTSTART:20260910T140000",
            "DTEND:20260910T150000",
            "SUMMARY:Déjeuner",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        let start = it.start.as_ref().unwrap();
        assert_eq!(start.wall.hour, 14);
        assert_eq!(start.zone, Zone::Floating);
        assert_eq!(start.unix, None, "aucun instant ne peut être affirmé");
        assert!(it.is_readable(), "l'heure murale se lit");
        assert!(!it.has_instant());
        // Et la réserve est **dite**, pour les deux dates.
        let floating: Vec<&Gap> = it
            .gaps
            .iter()
            .filter(|gap| matches!(gap, Gap::FloatingTime { .. }))
            .collect();
        assert_eq!(floating.len(), 2, "{:?}", it.gaps);
        assert!(!floating[0].is_refusal(), "une remarque, pas un refus");
        assert!(floating[0].to_string().contains("heure locale"));
    }

    #[test]
    fn a_named_zone_without_its_definition_keeps_the_wall_time_and_says_what_is_missing() {
        // Zéro pièce du corpus est dans ce cas — tous les producteurs rencontrés embarquent leur
        // `VTIMEZONE` — mais le format l'autorise, et un lecteur qui affirmerait un décalage
        // aurait besoin d'une base de fuseaux pour le faire. Voir l'en-tête du crate.
        let text = [
            "BEGIN:VCALENDAR",
            "BEGIN:VEVENT",
            "DTSTART;TZID=Europe/Paris:20260910T140000",
            "SUMMARY:Sans définition",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        let start = it.start.as_ref().unwrap();
        assert_eq!(start.wall.hour, 14, "l'heure murale reste lisible");
        assert_eq!(start.unix, None, "l'instant n'est pas inventé");
        assert!(matches!(start.zone, Zone::Named { offset: None, .. }));
        let gap = it
            .gaps
            .iter()
            .find(|gap| matches!(gap, Gap::UnknownZone { .. }))
            .unwrap();
        assert!(!gap.is_refusal(), "l'heure murale se lit encore");
        assert!(gap.to_string().contains("Europe/Paris"));
        assert!(it.is_readable());
        assert!(!it.has_instant());
    }

    #[test]
    fn an_all_day_event_has_no_time_and_no_timezone_to_apply() {
        let text = [
            "BEGIN:VCALENDAR",
            "BEGIN:VEVENT",
            "DTSTART;VALUE=DATE:20260910",
            "DTEND;VALUE=DATE:20260911",
            "SUMMARY:Congé",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        let start = it.start.as_ref().unwrap();
        assert!(start.is_all_day());
        assert_eq!(start.zone, Zone::AllDay);
        assert!(it.has_instant(), "une journée entière se place à sa date");
        assert!(it.gaps.is_empty(), "{:?}", it.gaps);
    }

    #[test]
    fn a_duration_replaces_a_missing_end() {
        let text = [
            "BEGIN:VCALENDAR",
            "BEGIN:VEVENT",
            "DTSTART:20260910T120000Z",
            "DURATION:PT1H30M",
            "SUMMARY:Atelier",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        let end = it.end.as_ref().unwrap();
        assert_eq!(
            end.unix,
            Some(it.start.as_ref().unwrap().unix.unwrap() + 5_400)
        );
        // L'heure murale de la fin suit l'instant : 13:30, et non « 12:00 » figé.
        assert_eq!((end.wall.hour, end.wall.minute), (13, 30));
        assert!(it.gaps.is_empty(), "{:?}", it.gaps);
    }

    #[test]
    fn a_duration_that_cannot_be_read_is_named_and_does_not_invent_an_end() {
        for hostile in ["PT", "P", "1H", "PT1", "PTX", ""] {
            let text = format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:20260910T120000Z\r\n\
                 DURATION:{hostile}\r\nEND:VEVENT\r\nEND:VCALENDAR"
            );
            let it = read(&text);
            assert!(it.end.is_none(), "{hostile:?} a produit une fin");
            assert!(
                it.gaps
                    .iter()
                    .any(|gap| matches!(gap, Gap::UnreadableDuration { .. } | Gap::NoEnd)),
                "{hostile:?} : {:?}",
                it.gaps
            );
            // Le rendez-vous reste affichable : seule sa fin manque.
            assert!(it.has_instant());
        }
    }

    #[test]
    fn a_cancellation_says_that_it_cancels() {
        // `METHOD:CANCEL` et `STATUS:CANCELLED` ne disent pas la même chose — l'un est ce que le
        // message demande, l'autre ce que l'événement est — et les deux doivent ressortir. Les
        // confondre afficherait une réunion annulée comme si elle avait lieu.
        let text = exchange()
            .replace("METHOD:REQUEST", "METHOD:CANCEL")
            .replace("STATUS:CONFIRMED", "STATUS:CANCELLED");
        let it = read(&text);
        assert_eq!(it.method, Method::Cancel);
        assert_eq!(it.method.label(), "annulation");
        assert_eq!(it.status.as_deref(), Some("CANCELLED"));
    }

    #[test]
    fn a_reply_carries_the_answer_of_the_person_who_replied() {
        let text = [
            "BEGIN:VCALENDAR",
            "METHOD:REPLY",
            "BEGIN:VEVENT",
            "DTSTART:20260910T120000Z",
            "ATTENDEE;PARTSTAT=DECLINED;CN=Jean:mailto:jean@x.fr",
            "UID:abc",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        assert_eq!(it.method, Method::Reply);
        assert_eq!(it.attendees[0].answer, Answer::Declined);
        assert_eq!(it.attendees[0].answer.label(), "refuse");
    }

    #[test]
    fn a_counter_proposal_is_not_an_invitation() {
        // Deux pièces du corpus. Classée en `Other`, elle s'afficherait comme une invitation et
        // l'utilisateur croirait qu'on lui propose le créneau d'origine.
        let text = exchange().replace("METHOD:REQUEST", "METHOD:COUNTER");
        assert_eq!(read(&text).method, Method::Counter);
    }

    #[test]
    fn a_recurring_event_reports_its_rule_without_expanding_it() {
        // **Aucune occurrence n'est déduite.** Une occurrence déduite de travers déplacerait un
        // rendez-vous, et développer une règle demande un calendrier complet.
        let text = [
            "BEGIN:VCALENDAR",
            "BEGIN:VEVENT",
            "DTSTART:20260910T120000Z",
            "RRULE:FREQ=WEEKLY;BYDAY=TH;COUNT=10",
            "SUMMARY:Point hebdo",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        assert_eq!(
            it.recurrence.as_deref(),
            Some("FREQ=WEEKLY;BYDAY=TH;COUNT=10")
        );
    }

    #[test]
    fn an_invitation_for_one_occurrence_of_a_series_says_which_one() {
        // 215 pièces du corpus portent un `RECURRENCE-ID`. Sans le lire, cette invitation
        // s'afficherait comme si elle déplaçait toute la série.
        let text = [
            "BEGIN:VCALENDAR",
            "METHOD:REQUEST",
            "BEGIN:VEVENT",
            "UID:serie-1",
            "RECURRENCE-ID:20260910T120000Z",
            "DTSTART:20260910T150000Z",
            "SUMMARY:Point hebdo (exceptionnellement à 15 h)",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        let occurrence = it.recurrence_id.as_ref().unwrap();
        assert_eq!(occurrence.wall.hour, 12);
        assert_eq!(it.start.as_ref().unwrap().wall.hour, 15);
    }

    #[test]
    fn several_events_in_one_piece_are_counted_rather_than_chosen_between() {
        // 31 pièces du corpus en portent plusieurs. Choisir en silence serait pire que le dire.
        let text = [
            "BEGIN:VCALENDAR",
            "BEGIN:VEVENT",
            "DTSTART:20260910T120000Z",
            "SUMMARY:Le premier",
            "END:VEVENT",
            "BEGIN:VEVENT",
            "DTSTART:20260911T120000Z",
            "SUMMARY:Le second",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        assert_eq!(it.summary.as_deref(), Some("Le premier"));
        assert_eq!(it.extra_events, 1);
        assert_eq!(it.start.as_ref().unwrap().wall.day, 10);
    }

    #[test]
    fn a_piece_without_any_event_is_refused_and_says_so() {
        // Deux pièces du corpus, et le contrôle du banc vérifie qu'aucune ne cache un `VEVENT`
        // que le lecteur aurait raté.
        let text = "BEGIN:VCALENDAR\r\nPRODID:-//x//y\r\nEND:VCALENDAR";
        let it = read(text);
        assert!(it.gaps.contains(&Gap::NoEvent));
        assert!(it.gaps.iter().any(Gap::is_refusal));
        assert!(!it.is_readable());
        assert!(it.start.is_none());
    }

    #[test]
    fn an_event_without_a_start_is_refused() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:x\r\nEND:VEVENT\r\nEND:VCALENDAR";
        let it = read(text);
        assert!(it.gaps.contains(&Gap::NoStart));
        assert!(it.gaps.iter().any(Gap::is_refusal));
        assert!(!it.is_readable());
    }

    #[test]
    fn an_unreadable_start_is_a_refusal_but_an_unreadable_end_is_not() {
        // La distinction que l'interface a besoin de faire : sans début il n'y a rien à montrer ;
        // sans fin il reste un rendez-vous.
        let broken_start = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:pas-une-date\r\n\
                            END:VEVENT\r\nEND:VCALENDAR";
        let it = read(broken_start);
        assert!(it.gaps.iter().any(Gap::is_refusal), "{:?}", it.gaps);
        assert!(!it.is_readable());

        let broken_end = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:20260910T120000Z\r\n\
                          DTEND:pas-une-date\r\nEND:VEVENT\r\nEND:VCALENDAR";
        let it = read(broken_end);
        assert!(!it.gaps.iter().any(Gap::is_refusal), "{:?}", it.gaps);
        assert!(it.has_instant());
        assert!(it.end.is_none());
    }

    #[test]
    fn a_hostile_piece_never_panics_and_always_says_something() {
        // La règle de tous les analyseurs d'entrée hostile du dépôt. Ces octets viennent du
        // réseau, et une invitation de travers ne doit pas empêcher d'afficher le message qui
        // la porte.
        for hostile in [
            "",
            "\u{0}\u{1}\u{2}",
            "BEGIN:VEVENT",
            "BEGIN:VEVENT\r\nDTSTART",
            "END:VEVENT\r\nBEGIN:VEVENT",
            "BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nEND:VCALENDAR",
            "BEGIN:VEVENT\r\nDTSTART;TZID=:20260910T120000\r\nEND:VEVENT",
            "BEGIN:VEVENT\r\nDTSTART:20260910T120000Z\r\nATTENDEE:\r\nORGANIZER:\r\nEND:VEVENT",
            &"BEGIN:VEVENT\r\n".repeat(5_000),
            &format!(
                "BEGIN:VCALENDAR\r\nSUMMARY:{}\r\nEND:VCALENDAR",
                "é".repeat(10_000)
            ),
        ] {
            let it = read(hostile);
            // Le contrat : soit c'est lisible, soit il y a au moins un manque nommé. Jamais ni
            // l'un ni l'autre — c'est exactement le « illisible sans raison » du banc.
            assert!(
                it.is_readable() || !it.gaps.is_empty(),
                "{:?} : ni lisible ni motivé",
                &hostile[..hostile.len().min(40)]
            );
            for gap in &it.gaps {
                assert!(!gap.to_string().is_empty());
            }
        }
    }

    #[test]
    fn an_attendee_that_is_not_a_mail_address_is_not_presented_as_one() {
        // Le corpus contient des salles et des ressources en `urn:`. Écrire à
        // `urn:x-resource:salle-12` n'irait nulle part, et l'afficher comme une adresse
        // laisserait croire le contraire.
        let text = [
            "BEGIN:VCALENDAR",
            "BEGIN:VEVENT",
            "DTSTART:20260910T120000Z",
            "ATTENDEE:urn:x-resource:salle-12",
            "END:VEVENT",
            "END:VCALENDAR",
        ]
        .join("\r\n");
        let it = read(&text);
        assert_eq!(it.attendees[0].address, None);
        assert_eq!(it.attendees[0].label(), "urn:x-resource:salle-12");
    }
}

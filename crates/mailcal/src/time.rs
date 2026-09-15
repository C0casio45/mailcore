//! Les dates d'une invitation : ce qui est écrit, et l'instant que ça désigne.
//!
//! ## Les quatre façons d'écrire une heure, et ce qu'on peut en faire
//!
//! Voir l'en-tête du crate. En deux mots : l'UTC et la journée entière ne demandent rien, un
//! `TZID` accompagné de sa `VTIMEZONE` se résout avec les décalages que le fichier porte
//! lui-même, et un `TZID` sans définition **n'est pas résolu** — il est nommé comme manquant.
//!
//! ## Aucune base de fuseaux, et c'est un choix
//!
//! Ni téléchargée, ni embarquée. Une base embarquée affirmerait un décalage que le fichier ne
//! dit pas, avec *notre* version des règles plutôt que celle du producteur — et les règles
//! changent : le Chili, le Maroc, l'Iran en ont changé pendant la vie de ce corpus. Quand le
//! fichier porte sa `VTIMEZONE`, il porte les règles de celui qui a écrit l'heure, et c'est la
//! seule source qui ne peut pas se désaccorder d'elle-même.

/// Une date-heure civile, telle qu'écrite dans le fichier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Civil {
    /// L'année, grégorienne.
    pub year: i32,
    /// Le mois, de 1 à 12.
    pub month: u32,
    /// Le jour, de 1 à 31.
    pub day: u32,
    /// L'heure, de 0 à 23.
    pub hour: u32,
    /// La minute, de 0 à 59.
    pub minute: u32,
    /// La seconde, de 0 à 59.
    pub second: u32,
}

impl Civil {
    /// Lit une valeur `DATE` ou `DATE-TIME` de la RFC 5545 §3.3.4 et §3.3.5.
    ///
    /// Les formes acceptées, et rien d'autre : `20260910`, `20260910T140000`,
    /// `20260910T140000Z`. Le `Z` est rendu à part, parce qu'il **change le sens** de la valeur
    /// et non sa forme.
    ///
    /// Rend `None` sur tout le reste, y compris une date impossible — un 31 février, un mois
    /// 13. Un lecteur qui corrigerait en silence afficherait un rendez-vous à une date que
    /// personne n'a écrite.
    #[must_use]
    pub fn parse(value: &str) -> Option<(Self, bool, bool)> {
        let value = value.trim();
        let (body, utc) = match value.strip_suffix(['Z', 'z']) {
            Some(body) => (body, true),
            None => (value, false),
        };
        let (date, time) = match body.split_once(['T', 't']) {
            Some((date, time)) => (date, Some(time)),
            None => (body, None),
        };
        if date.len() != 8 || !date.bytes().all(|it| it.is_ascii_digit()) {
            return None;
        }
        let number = |from: usize, to: usize| date.get(from..to)?.parse::<u32>().ok();
        let year = i32::try_from(number(0, 4)?).ok()?;
        let month = number(4, 6)?;
        let day = number(6, 8)?;

        let (hour, minute, second) = match time {
            None => (0, 0, 0),
            Some(time) => {
                if time.len() != 6 || !time.bytes().all(|it| it.is_ascii_digit()) {
                    return None;
                }
                let piece = |from: usize, to: usize| time.get(from..to)?.parse::<u32>().ok();
                (piece(0, 2)?, piece(2, 4)?, piece(4, 6)?)
            }
        };

        let it = Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        };
        if !it.is_valid() {
            return None;
        }
        Some((it, utc, time.is_none()))
    }

    /// Vrai si cette date existe.
    ///
    /// La seconde 60 est acceptée : elle existe dans le calendrier UTC — une seconde
    /// intercalaire — et un producteur qui l'écrit n'a pas tort. L'heure 24 non : elle
    /// désignerait minuit du jour suivant, et la RFC ne l'autorise pas.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.month >= 1
            && self.month <= 12
            && self.day >= 1
            && self.day <= days_in_month(self.year, self.month)
            && self.hour <= 23
            && self.minute <= 59
            && self.second <= 60
    }

    /// Les secondes depuis l'époque, en traitant cette date comme de l'UTC.
    ///
    /// ## Une troisième implémentation du calendrier grégorien dans ce dépôt
    ///
    /// `mailsmtp::compose::rfc5322_date` fait le sens inverse — jours vers date civile — et
    /// `mail-shell::app::civil` aussi, pour la colonne de date de la liste. Aucune des deux
    /// n'est réutilisable ici : ce crate ne dépend de rien, et c'est **lui** que `mailcore`
    /// devra dépendre pour lire une invitation, donc la flèche va dans l'autre sens.
    ///
    /// Le jour où ces vingt lignes existent en quatre exemplaires, elles auront gagné leur
    /// propre maison. À trois, l'indirection coûterait plus que la copie — c'est l'arbitrage
    /// déjà écrit pour « verrou + `spawn_blocking` » dans `maild::api`.
    ///
    /// L'algorithme est celui de Howard Hinnant, comme les deux autres : pas de cas
    /// particulier, pas de table, et il tient dans une expression.
    #[must_use]
    pub const fn as_unix_utc(&self) -> i64 {
        let days = days_from_civil(self.year, self.month, self.day);
        days * 86_400
            + (self.hour as i64) * 3_600
            + (self.minute as i64) * 60
            // Une seconde 60 vaut la seconde 59 : l'instant Unix ne compte pas les
            // intercalaires, et prétendre le contraire décalerait tout d'une seconde.
            + if self.second >= 60 { 59 } else { self.second as i64 }
    }

    /// Le jour de la semaine, `0` pour dimanche.
    #[must_use]
    pub const fn weekday(&self) -> u32 {
        let days = days_from_civil(self.year, self.month, self.day);
        // 1970-01-01 était un jeudi, soit 4.
        let shifted = (days + 4) % 7;
        #[allow(clippy::cast_sign_loss)]
        if shifted < 0 {
            (shifted + 7) as u32
        } else {
            shifted as u32
        }
    }
}

/// Le nombre de jours d'un mois.
#[must_use]
pub const fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Vrai pour une année bissextile grégorienne.
#[must_use]
pub const fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Les jours depuis 1970-01-01, algorithme de Howard Hinnant.
#[must_use]
pub const fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year as i64 - if month <= 2 { 1 } else { 0 };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_shifted = (month as i64 + 9) % 12;
    let day_of_year = (153 * month_shifted + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// D'où une heure tient son sens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Zone {
    /// De l'UTC, écrit avec un `Z`.
    Utc,
    /// Une journée entière : `VALUE=DATE`, sans heure. Il n'y a pas de fuseau à appliquer.
    AllDay,
    /// Un fuseau nommé, avec le décalage que la `VTIMEZONE` du fichier lui donne — ou `None`
    /// quand le fichier ne le définit pas.
    Named {
        /// Le `TZID`, tel qu'écrit.
        id: String,
        /// Le décalage en secondes à l'est de l'UTC, quand il est connu.
        offset: Option<i32>,
    },
    /// Une heure sans fuseau : « flottante », RFC 5545 §3.3.5. Elle désigne l'heure locale du
    /// lecteur, quelle qu'elle soit — donc aucun instant précis.
    Floating,
}

/// Une date-heure d'invitation : ce qui est écrit, d'où ça tient son sens, et l'instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moment {
    /// L'heure murale, telle qu'écrite par le producteur.
    pub wall: Civil,
    /// Ce qui donne un sens à cette heure murale.
    pub zone: Zone,
    /// L'instant en secondes Unix, **quand il est déterminé**.
    ///
    /// `None` pour une heure flottante et pour un fuseau nommé sans définition : dans les deux
    /// cas, l'instant dépend d'une information que le fichier ne porte pas. Afficher une heure
    /// murale avec son fuseau est honnête ; affirmer un instant faux ne l'est pas.
    pub unix: Option<i64>,
}

impl Moment {
    /// Vrai si cette date désigne une journée entière.
    #[must_use]
    pub fn is_all_day(&self) -> bool {
        self.zone == Zone::AllDay
    }
}

/// Une observance d'un fuseau : un décalage, et quand il commence à s'appliquer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observance {
    /// Le décalage **après** la transition, en secondes à l'est de l'UTC.
    pub offset_to: i32,
    /// Le décalage **avant**. Sert à interpréter l'heure de la transition, qui est écrite en
    /// heure locale d'avant le changement.
    pub offset_from: i32,
    /// Quand cette observance a commencé, tel qu'écrit dans son `DTSTART`.
    pub starts: Civil,
    /// Le mois de la répétition annuelle, s'il y en a une.
    pub month: Option<u32>,
    /// Le jour de la semaine visé, et son rang dans le mois : `(-1, 0)` pour « le dernier
    /// dimanche », `(2, 1)` pour « le deuxième lundi ».
    pub weekday: Option<(i32, u32)>,
    /// Vrai pour une observance d'heure d'été.
    pub is_dst: bool,
}

impl Observance {
    /// L'heure murale — dans le décalage d'avant — à laquelle cette observance commence, pour
    /// une année donnée.
    ///
    /// Sans règle de répétition, c'est son `DTSTART` : l'observance ne commence qu'une fois.
    #[must_use]
    pub fn transition(&self, year: i32) -> Civil {
        let (Some(month), Some((rank, weekday))) = (self.month, self.weekday) else {
            return self.starts;
        };
        let day = nth_weekday(year, month, rank, weekday);
        Civil {
            year,
            month,
            day,
            ..self.starts
        }
    }
}

/// Le jour du mois du `rank`-ième `weekday`. `rank` négatif compte depuis la fin.
#[must_use]
pub fn nth_weekday(year: i32, month: u32, rank: i32, weekday: u32) -> u32 {
    let length = days_in_month(year, month);
    if length == 0 {
        return 1;
    }
    if rank >= 0 {
        let first = Civil {
            year,
            month,
            day: 1,
            ..Civil::default()
        }
        .weekday();
        let shift = (weekday + 7 - first) % 7;
        #[allow(clippy::cast_sign_loss)]
        let rank = rank.max(1) as u32;
        let day = 1 + shift + (rank - 1) * 7;
        day.min(length)
    } else {
        let last = Civil {
            year,
            month,
            day: length,
            ..Civil::default()
        }
        .weekday();
        let shift = (last + 7 - weekday) % 7;
        let back = shift + (rank.unsigned_abs() - 1) * 7;
        length.saturating_sub(back).max(1)
    }
}

/// Un fuseau tel que le fichier le définit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timezone {
    /// Le `TZID`, qui est la clé par laquelle les dates y renvoient.
    pub id: String,
    /// Ses observances, dans l'ordre du fichier.
    pub observances: Vec<Observance>,
}

impl Timezone {
    /// Le décalage à appliquer à une heure murale de ce fuseau.
    ///
    /// ## Comment l'observance est choisie
    ///
    /// Une `VTIMEZONE` d'invitation en porte une ou deux : l'heure d'hiver et l'heure d'été,
    /// chacune avec une règle du genre « le dernier dimanche de mars ». Pour une heure murale
    /// donnée, on calcule les deux transitions de **son** année et on prend celle qui s'applique
    /// — la dernière atteinte.
    ///
    /// Une seule observance : son décalage, sans calcul. C'est le cas des fuseaux sans heure
    /// d'été, et celui des producteurs qui n'écrivent que l'observance en cours.
    ///
    /// Aucune observance : `None`. Un `VTIMEZONE` vide ne définit rien, et le dire est plus
    /// utile que de rendre zéro — qui serait de l'UTC, donc une affirmation fausse.
    #[must_use]
    pub fn offset_at(&self, wall: &Civil) -> Option<i32> {
        match self.observances.as_slice() {
            [] => None,
            [single] => Some(single.offset_to),
            many => {
                // La dernière transition atteinte par cette heure murale. Les transitions sont
                // écrites en heure locale d'avant le changement, ce qui est exactement ce qu'on
                // compare à une heure murale.
                let mut best: Option<(Civil, i32)> = None;
                for observance in many {
                    let at = observance.transition(wall.year);
                    if at <= *wall && best.is_none_or(|(previous, _)| at >= previous) {
                        best = Some((at, observance.offset_to));
                    }
                }
                // Avant la première transition de l'année : c'est la dernière de l'année
                // précédente qui s'applique, donc l'observance dont la transition est la plus
                // tardive. En janvier sous l'hémisphère nord, c'est l'heure d'hiver.
                best.map(|(_, offset)| offset).or_else(|| {
                    many.iter()
                        .max_by_key(|observance| observance.transition(wall.year))
                        .map(|observance| observance.offset_to)
                })
            }
        }
    }
}

/// Lit un décalage `TZOFFSETTO` ou `TZOFFSETFROM` : `+0200`, `-0500`, `+020000`.
#[must_use]
pub fn parse_offset(value: &str) -> Option<i32> {
    let value = value.trim();
    let (sign, digits) = match value.as_bytes().first()? {
        b'+' => (1, &value[1..]),
        b'-' => (-1, &value[1..]),
        _ => (1, value),
    };
    if !digits.bytes().all(|it| it.is_ascii_digit()) || (digits.len() != 4 && digits.len() != 6) {
        return None;
    }
    let piece = |from: usize, to: usize| digits.get(from..to)?.parse::<i32>().ok();
    let hours = piece(0, 2)?;
    let minutes = piece(2, 4)?;
    let seconds = if digits.len() == 6 { piece(4, 6)? } else { 0 };
    if minutes > 59 || seconds > 59 {
        return None;
    }
    Some(sign * (hours * 3_600 + minutes * 60 + seconds))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Civil, Observance, Timezone, nth_weekday, parse_offset};

    #[test]
    fn the_three_forms_of_a_date_are_read_and_the_z_is_not_a_digit() {
        let (civil, utc, all_day) = Civil::parse("20260910T140000Z").unwrap();
        assert_eq!((civil.year, civil.month, civil.day), (2026, 9, 10));
        assert_eq!((civil.hour, civil.minute, civil.second), (14, 0, 0));
        assert!(utc);
        assert!(!all_day);

        let (civil, utc, all_day) = Civil::parse("20260910").unwrap();
        assert_eq!((civil.year, civil.month, civil.day), (2026, 9, 10));
        assert!(!utc);
        assert!(all_day, "une valeur DATE désigne une journée entière");

        let (_, utc, all_day) = Civil::parse("20260910T140000").unwrap();
        assert!(!utc);
        assert!(!all_day);
    }

    #[test]
    fn an_impossible_date_is_refused_rather_than_corrected() {
        // **Un lecteur qui corrige affiche un rendez-vous à une date que personne n'a écrite.**
        for hostile in [
            "20260231T140000", // 31 février
            "20261310T140000", // mois 13
            "20260900T140000", // jour 0
            "20260910T250000", // heure 25
            "20260910T146000", // minute 60
            "2026091",         // trop court
            "20260910T1400",   // heure incomplète
            "abcdefgh",
            "",
            "20260910T14000Z",
        ] {
            assert_eq!(Civil::parse(hostile), None, "{hostile:?} accepté");
        }
        // Le 29 février d'une bissextile existe, celui d'une autre non.
        assert!(Civil::parse("20240229").is_some());
        assert_eq!(Civil::parse("20230229"), None);
        // Une seconde intercalaire est acceptée, et vaut la 59 en instant.
        let (leap, _, _) = Civil::parse("20161231T235960Z").unwrap();
        let (before, _, _) = Civil::parse("20161231T235959Z").unwrap();
        assert_eq!(leap.as_unix_utc(), before.as_unix_utc());
    }

    #[test]
    fn the_epoch_and_a_few_known_instants_round_trip() {
        let epoch = Civil {
            year: 1970,
            month: 1,
            day: 1,
            ..Civil::default()
        };
        assert_eq!(epoch.as_unix_utc(), 0);
        assert_eq!(epoch.weekday(), 4, "1970-01-01 était un jeudi");

        let (known, _, _) = Civil::parse("20260910T140000Z").unwrap();
        // 2026-09-10T14:00:00Z, vérifié à la main : 20 706 jours depuis l'époque.
        assert_eq!(known.as_unix_utc(), 20_706 * 86_400 + 14 * 3_600);

        // Une date d'avant l'époque, où l'arithmétique des ères se joue.
        let old = Civil {
            year: 1969,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59,
            second: 59,
        };
        assert_eq!(old.as_unix_utc(), -1);
    }

    #[test]
    fn the_nth_weekday_of_a_month_is_found_from_both_ends() {
        // Les règles de fuseau réelles : « le dernier dimanche de mars », « le premier dimanche
        // de novembre », « le deuxième dimanche de mars ».
        assert_eq!(
            nth_weekday(2026, 3, -1, 0),
            29,
            "dernier dimanche de mars 2026"
        );
        assert_eq!(
            nth_weekday(2026, 10, -1, 0),
            25,
            "dernier dimanche d'octobre 2026"
        );
        assert_eq!(
            nth_weekday(2026, 11, 1, 0),
            1,
            "premier dimanche de novembre 2026"
        );
        assert_eq!(
            nth_weekday(2026, 3, 2, 0),
            8,
            "deuxième dimanche de mars 2026"
        );
        // Un rang qui dépasse le mois est ramené dans le mois plutôt que de déborder.
        assert!(nth_weekday(2026, 2, 5, 0) <= 28);
        // Un mois impossible ne panique pas.
        assert_eq!(nth_weekday(2026, 13, 1, 0), 1);
    }

    #[test]
    fn an_offset_is_read_in_both_lengths_and_both_directions() {
        assert_eq!(parse_offset("+0200"), Some(7_200));
        assert_eq!(parse_offset("-0500"), Some(-18_000));
        assert_eq!(parse_offset("+0530"), Some(19_800));
        assert_eq!(parse_offset("+020000"), Some(7_200));
        assert_eq!(parse_offset("0200"), Some(7_200));
        for hostile in ["+2", "+02:00", "abcd", "", "+0260", "+02000"] {
            assert_eq!(parse_offset(hostile), None, "{hostile:?} accepté");
        }
    }

    /// Le fuseau que la moitié du corpus porte : heure d'Europe centrale, avec son heure d'été.
    fn paris() -> Timezone {
        Timezone {
            id: "Europe/Paris".to_owned(),
            observances: vec![
                Observance {
                    offset_to: 7_200,
                    offset_from: 3_600,
                    starts: Civil {
                        year: 1981,
                        month: 3,
                        day: 29,
                        hour: 2,
                        ..Civil::default()
                    },
                    month: Some(3),
                    weekday: Some((-1, 0)),
                    is_dst: true,
                },
                Observance {
                    offset_to: 3_600,
                    offset_from: 7_200,
                    starts: Civil {
                        year: 1996,
                        month: 10,
                        day: 27,
                        hour: 3,
                        ..Civil::default()
                    },
                    month: Some(10),
                    weekday: Some((-1, 0)),
                    is_dst: false,
                },
            ],
        }
    }

    #[test]
    fn a_summer_time_and_a_winter_time_are_told_apart() {
        // **Le calcul qui décide si un rendez-vous est affiché à la bonne heure.** Une erreur
        // ici décale d'une heure la moitié de l'année, ce qui est exactement le genre de faute
        // qu'on ne voit qu'en arrivant en retard.
        let zone = paris();
        let summer = Civil::parse("20260710T140000").unwrap().0;
        let winter = Civil::parse("20261210T140000").unwrap().0;
        assert_eq!(zone.offset_at(&summer), Some(7_200), "juillet est en +02");
        assert_eq!(zone.offset_at(&winter), Some(3_600), "décembre est en +01");

        // Janvier : avant la première transition de l'année, donc l'heure d'hiver de l'année
        // précédente. Le cas que le calcul « la dernière transition atteinte » rate si on
        // oublie de retomber sur la plus tardive.
        let january = Civil::parse("20260115T090000").unwrap().0;
        assert_eq!(zone.offset_at(&january), Some(3_600));
    }

    #[test]
    fn the_hours_around_a_transition_fall_on_the_right_side() {
        let zone = paris();
        // Le dernier dimanche de mars 2026 est le 29, transition à 02:00 locale.
        let before = Civil::parse("20260329T015900").unwrap().0;
        let after = Civil::parse("20260329T030000").unwrap().0;
        assert_eq!(zone.offset_at(&before), Some(3_600));
        assert_eq!(zone.offset_at(&after), Some(7_200));

        // Et à l'automne, le dernier dimanche d'octobre est le 25, transition à 03:00.
        let before = Civil::parse("20261025T025900").unwrap().0;
        let after = Civil::parse("20261025T040000").unwrap().0;
        assert_eq!(zone.offset_at(&before), Some(7_200));
        assert_eq!(zone.offset_at(&after), Some(3_600));
    }

    #[test]
    fn a_zone_with_one_observance_needs_no_calculation() {
        // Un fuseau sans heure d'été, ou un producteur qui n'écrit que l'observance en cours.
        let zone = Timezone {
            id: "Asia/Tokyo".to_owned(),
            observances: vec![Observance {
                offset_to: 32_400,
                offset_from: 32_400,
                starts: Civil::default(),
                month: None,
                weekday: None,
                is_dst: false,
            }],
        };
        let any = Civil::parse("20260710T140000").unwrap().0;
        assert_eq!(zone.offset_at(&any), Some(32_400));
    }

    #[test]
    fn a_zone_that_defines_nothing_resolves_to_nothing() {
        // **Zéro serait de l'UTC**, donc une affirmation fausse sur un fichier qui ne dit rien.
        let zone = Timezone {
            id: "Europe/Paris".to_owned(),
            observances: Vec::new(),
        };
        assert_eq!(zone.offset_at(&Civil::default()), None);
    }
}

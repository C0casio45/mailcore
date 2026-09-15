//! Mise en forme des nombres pour la sortie utilisateur.
//!
//! ## Pourquoi c'est dans le crate du contrat
//!
//! Parce que **deux** surfaces l'affichent : la CLI et la coquille. Une copie chacune serait
//! sans danger — une divergence sur une étiquette d'octets est cosmétique, pas corruptrice,
//! contrairement au doublage des points — mais elle serait quand même deux endroits à corriger
//! le jour où on préfère « 4,1 Mo » à « 4.1 Mio ».
//!
//! `mailapi` porte déjà les types que ces deux surfaces affichent, donc c'est là que la mise en
//! forme de ces types appartient.

/// Une taille en octets, en unités binaires.
#[must_use]
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["o", "Kio", "Mio", "Gio", "Tio"];
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1024.0 && unit < UNITS.len() - 1 {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} o")
    } else {
        format!("{scaled:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_to_binary_units() {
        assert_eq!(bytes(0), "0 o");
        assert_eq!(bytes(1023), "1023 o");
        assert_eq!(bytes(1024), "1.0 Kio");
        assert_eq!(bytes(1_500_000), "1.4 Mio");
        assert_eq!(bytes(10_871_635_968), "10.1 Gio");
    }

    #[test]
    fn does_not_overflow_on_the_largest_size() {
        assert!(bytes(u64::MAX).ends_with("Tio"));
    }
}

/// Une date Unix en `AAAA-MM-JJ`.
///
/// Calendrier grégorien proleptique, calculé à la main : ajouter `chrono` ou `time` pour
/// formater une date dans une sortie CLI ne vaut pas la dépendance. Les heures ne sont pas
/// affichées — dans une liste de résultats, le jour suffit à situer.
#[must_use]
pub fn date(unix_seconds: i64) -> String {
    if unix_seconds <= 0 {
        return "          ".to_owned();
    }
    let days = unix_seconds.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Convertit un nombre de jours depuis 1970-01-01 en date civile.
///
/// Algorithme de Howard Hinnant (« chrono-Compatible Low-Level Date Algorithms »), transcrit
/// tel quel plutôt que réinventé : les calculs de calendrier sont un nid à erreurs d'un jour.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod date_tests {
    use super::date;

    #[test]
    fn formats_known_dates() {
        assert_eq!(date(0), "          ");
        assert_eq!(date(1), "1970-01-01");
        assert_eq!(date(1_700_000_000), "2023-11-14");
        assert_eq!(date(946_684_800), "2000-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
    }

    #[test]
    fn a_missing_or_absurd_date_does_not_panic() {
        assert_eq!(date(-1), "          ");
        assert_eq!(date(i64::MIN), "          ");
        assert!(!date(i64::MAX).is_empty());
    }
}

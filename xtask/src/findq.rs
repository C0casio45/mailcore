//! `queries-find` — de quoi écrire le jeu de requêtes du critère 1, sans jamais rien chercher.
//!
//! ## Ce qu'il fait
//!
//! Deux choses, qui sont les deux moitiés d'une entrée du jeu :
//!
//! - **tirer des messages au hasard** (`--sample`), pour choisir quoi chercher. Le tirage a une
//!   graine : il est rejouable, donc on ne relance pas jusqu'à tomber sur un échantillon qui
//!   plaît — ce qui reviendrait à choisir ses cibles ;
//! - **lister les messages dont le sujet ou l'expéditeur contient un texte** (`--subject`,
//!   `--from`), avec leur empreinte, pour écrire la liste des réponses acceptables. Sur un corpus
//!   réel, « votre colis a été livré » a des dizaines de bonnes réponses, et n'en accepter qu'une
//!   mesurerait la chance du moteur.
//!
//! ## Ce qu'il s'interdit
//!
//! **Il ne lance aucun moteur**, ni plein texte ni sémantique. Un jeu de requêtes écrit en
//! regardant ce que le moteur rend finit par ressembler au moteur : c'est la règle de l'étape 2
//! de `docs/PHASE-4.md`, qui met le banc avant le moteur. Le repli en minuscules est celui de
//! Rust, Unicode — la leçon du carnet d'adresses : `lower()` de SQLite ne replie que l'ASCII.
//!
//! Il n'écrit rien. Le store n'est que lu, et ce qu'il imprime nomme des messages réels : la
//! sortie ne va jamais dans `docs/`.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use mailcore::store::read::IndexRow;

/// Ce qu'on demande à l'outil.
pub struct Options {
    /// La racine du store, lue seulement.
    pub store: Utf8PathBuf,
    /// Combien de messages tirer au hasard. Exclusif des filtres.
    pub sample: Option<usize>,
    /// La graine du tirage.
    pub seed: u64,
    /// Un texte que le sujet doit contenir.
    pub subject: Option<String>,
    /// Un texte que l'adresse ou le nom de l'expéditeur doit contenir.
    pub from: Option<String>,
    /// Montrer le début du corps de chaque message.
    pub body: bool,
}

/// Combien de caractères de corps montrer : assez pour comprendre de quoi parle le message.
const BODY_CHARS: usize = 400;

/// Imprime l'échantillon ou les messages qui correspondent aux filtres.
///
/// # Errors
///
/// Si le store est illisible, ou si on demande à la fois un tirage et un filtre.
pub fn run(options: &Options) -> Result<()> {
    let filtered = options.subject.is_some() || options.from.is_some();
    anyhow::ensure!(
        options.sample.is_some() != filtered,
        "soit --sample, soit --subject et/ou --from : un tirage filtré choisirait déjà ses cibles"
    );

    let mailbox = crate::guard::open_mailbox(&options.store)
        .with_context(|| format!("ouverture de la boîte {}", options.store))?;
    let rows = mailbox
        .store()
        .all_for_indexing()
        .context("lecture des messages du store")?;

    let picked: Vec<&IndexRow> = if let Some(count) = options.sample {
        sample(&rows, count, options.seed)
    } else {
        let subject = options.subject.as_deref().map(str::to_lowercase);
        let from = options.from.as_deref().map(str::to_lowercase);
        rows.iter()
            .filter(|row| {
                subject
                    .as_ref()
                    .is_none_or(|it| row.subject.to_lowercase().contains(it.as_str()))
            })
            .filter(|row| {
                from.as_ref().is_none_or(|it| {
                    row.from_addr.to_lowercase().contains(it.as_str())
                        || row
                            .from_name
                            .as_deref()
                            .is_some_and(|name| name.to_lowercase().contains(it.as_str()))
                })
            })
            .collect()
    };

    println!("{} message(s) sur {}\n", picked.len(), rows.len());
    for row in picked {
        let who = row
            .from_name
            .as_deref()
            .filter(|it| !it.trim().is_empty())
            .unwrap_or(&row.from_addr);
        println!("{}  {}", day(row.date), row.blob.to_hex());
        println!("    de    {who} <{}>", row.from_addr);
        println!("    sujet {}", row.subject);
        if options.body
            && let Some(detail) = mailbox.message(row.id).context("relecture d'un message")?
        {
            let text: String = detail
                .body
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(BODY_CHARS)
                .collect();
            println!("    corps {text}");
        }
    }
    Ok(())
}

/// `count` lignes distinctes, tirées par un générateur à graine.
///
/// Un `xorshift` suffit : on veut un tirage rejouable et sans biais visible, pas une garantie
/// cryptographique, et une dépendance de plus pour ça serait disproportionnée.
fn sample(rows: &[IndexRow], count: usize, seed: u64) -> Vec<&IndexRow> {
    let mut state = seed | 1;
    let mut chosen: Vec<usize> = Vec::with_capacity(count.min(rows.len()));
    while chosen.len() < count.min(rows.len()) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let len = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        let index = usize::try_from(state % len).unwrap_or(0);
        if !chosen.contains(&index) {
            chosen.push(index);
        }
    }
    chosen.sort_unstable();
    chosen.into_iter().map(|index| &rows[index]).collect()
}

/// `AAAA-MM-JJ` en UTC, pour dater une ligne. L'heure locale n'apporterait rien ici.
fn day(seconds: i64) -> String {
    // Hinnant, « civil_from_days » : du nombre de jours depuis 1970 à une date grégorienne.
    let days = seconds.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let of_era = days - era * 146_097;
    let year_of_era = (of_era - of_era / 1_460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * of_year + 2) / 153;
    let day = of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_are_gregorian_dates() {
        assert_eq!(day(0), "1970-01-01");
        // 2000 est bissextile, 1900 ne l'était pas : le 29 février 2000 existe.
        assert_eq!(day(951_782_400), "2000-02-29");
        assert_eq!(day(1_789_041_600), "2026-09-10");
        assert_eq!(day(-86_400), "1969-12-31");
    }
}

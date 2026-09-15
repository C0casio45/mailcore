//! Sondes en lecture seule sur un profil Thunderbird.
//!
//! Tout ce module lit et ne fait qu'écrire **hors du profil**. Le profil est en production ;
//! le critère 7 de `docs/PHASE-1.md` exige exactement zéro écriture, et l'outil qui vérifie
//! ce critère ne peut pas être celui qui le viole.
//!
//! Trois choses à en tirer avant d'écrire une ligne dans le store :
//!
//! - **L'inventaire** ([`scan`]) — combien de comptes, de dossiers, d'octets, et quel est le
//!   plus gros fichier. Métadonnées seulement, instantané.
//! - **Le relevé** ([`snapshot`] / [`diff`]) — taille et `mtime` de chaque fichier, pour
//!   encadrer un import et prouver le critère 7.
//! - **La sonde** ([`probe`]) — le lecteur mbox réel lâché sur le corpus réel. Rend le
//!   nombre de messages, le taux de dédup mesuré, l'espace mort des messages supprimés non
//!   compactés, et la convention de *From-mangling* de l'écrivain. Aucun octet écrit.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use camino::Utf8PathBuf;
use mailimport::mbox::{Mangling, MboxReader};
use mailimport::{mozilla, tree};

/// Tampon de lecture par fichier. 1 Mio : assez grand pour que le coût par appel système
/// disparaisse, assez petit pour rester invisible dans le budget du critère 3.
const READ_BUFFER: usize = 1024 * 1024;

/// Inventaire du profil, sans ouvrir un seul fichier.
pub fn scan(profile: &Path) -> Result<()> {
    let scan = tree::scan_profile(profile).context("parcours du profil")?;

    println!("Profil       {}", profile.display());
    println!("Dossiers     {}", scan.files.len());
    println!("Octets       {}", human(scan.total_bytes));
    println!("Écartés      {} fichiers non-mbox", scan.skipped);
    if !scan.unreadable.is_empty() {
        println!("ILLISIBLES   {} répertoires", scan.unreadable.len());
        for path in &scan.unreadable {
            println!("             {}", path.display());
        }
    }

    // Le drapeau `declared` est affiché : un compte que `prefs.js` ne connaît pas est le reste
    // d'un compte supprimé, et l'import l'écarte par défaut. Autant que la sonde le dise,
    // sinon l'écart entre ce qu'elle voit et ce qui s'importe se découvre par soustraction.
    let mut accounts: Vec<(&str, &str, bool)> = scan
        .files
        .iter()
        .map(|f| (f.account.as_str(), f.account_kind.as_str(), f.declared))
        .collect();
    accounts.sort_unstable();
    accounts.dedup();
    println!("Comptes      {}", accounts.len());
    for (name, kind, declared) in &accounts {
        let mark = if *declared {
            ""
        } else {
            "   ORPHELIN — absent de prefs.js, écarté à l'import"
        };
        println!("             [{kind}] {name}{mark}");
    }

    println!("\nLes 15 plus gros dossiers :");
    for file in scan.files.iter().take(15) {
        println!(
            "  {:>10}  {:<12} {}",
            human(file.size),
            file.account,
            file.folder
        );
    }

    let lossy = scan.files.iter().filter(|f| f.name_is_lossy).count();
    if lossy > 0 {
        println!("\n{lossy} noms de dossiers n'étaient pas de l'UTF-8 valide.");
    }
    Ok(())
}

/// Écrit un relevé `chemin\ttaille\tmtime` de tout le profil.
///
/// Le fichier de sortie est **hors du profil**, toujours. Écrire le relevé à côté de ce
/// qu'il observe invaliderait le relevé suivant.
pub fn snapshot(profile: &Path, out: &Utf8PathBuf) -> Result<()> {
    if out.as_std_path().starts_with(profile) {
        bail!("le relevé écrirait dans le profil observé : {out}");
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("création de {parent}"))?;
    }

    let mut rows = Vec::new();
    let mut unreadable = 0u64;
    collect(profile, profile, &mut rows, &mut unreadable)?;
    rows.sort();

    let file =
        std::fs::File::create(out.as_std_path()).with_context(|| format!("écriture de {out}"))?;
    let mut writer = std::io::BufWriter::new(file);
    for (path, size, mtime) in &rows {
        writeln!(writer, "{path}\t{size}\t{mtime}")?;
    }
    writer.flush()?;

    println!("{} fichiers relevés dans {out}", rows.len());
    if unreadable > 0 {
        println!("{unreadable} entrées illisibles, ignorées");
    }
    Ok(())
}

/// Compare deux relevés et rend une erreur si le profil a bougé.
///
/// Nuance qui rend le critère 7 exploitable : Thunderbird tourne en parallèle et modifie
/// légitimement ses propres fichiers. Une différence sur un `.msf` est du travail de
/// Thunderbird ; une différence sur un mbox, que nous sommes seuls à lire, est un échec.
/// La sortie sépare les deux.
pub fn diff(before: &Utf8PathBuf, after: &Utf8PathBuf) -> Result<()> {
    let before_rows = read_snapshot(before)?;
    let after_rows = read_snapshot(after)?;

    let mut changed_mbox = Vec::new();
    let mut changed_other = Vec::new();

    for (path, state) in &before_rows {
        match after_rows.get(path) {
            Some(now) if now == state => {}
            Some(_) => classify(path, "modifié", &mut changed_mbox, &mut changed_other),
            None => classify(path, "disparu", &mut changed_mbox, &mut changed_other),
        }
    }
    for path in after_rows.keys() {
        if !before_rows.contains_key(path) {
            classify(path, "apparu", &mut changed_mbox, &mut changed_other);
        }
    }

    println!("Relevés      {} → {}", before_rows.len(), after_rows.len());
    println!("Mbox touchés {}", changed_mbox.len());
    for line in &changed_mbox {
        println!("  ÉCHEC  {line}");
    }
    println!(
        "Autres       {} (Thunderbird travaille en parallèle)",
        changed_other.len()
    );
    for line in changed_other.iter().take(10) {
        println!("  ....   {line}");
    }

    if changed_mbox.is_empty() {
        println!("\nCritère 7 : aucun mbox modifié.");
        Ok(())
    } else {
        bail!("critère 7 en échec : {} mbox modifiés", changed_mbox.len())
    }
}

/// Lâche le lecteur mbox sur le corpus réel et mesure ce qu'il y trouve.
///
/// `budget` plafonne les octets lus par fichier, pour pouvoir sonder vite. Sans plafond, la
/// sonde lit tout — c'est alors une mesure du critère 6, et un vrai galop d'essai du lecteur.
pub fn probe(profile: &Path, budget: Option<u64>) -> Result<()> {
    let scan = tree::scan_profile(profile).context("parcours du profil")?;
    let started = Instant::now();

    let mut stats = ProbeStats::default();
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut message = Vec::with_capacity(64 * 1024);

    for file in &scan.files {
        let handle = match std::fs::File::open(&file.path) {
            Ok(handle) => handle,
            Err(err) => {
                println!("  illisible : {} ({err})", file.folder);
                stats.unreadable_files += 1;
                continue;
            }
        };
        let reader: Box<dyn BufRead> = match budget {
            Some(limit) => Box::new(BufReader::with_capacity(READ_BUFFER, handle.take(limit))),
            None => Box::new(BufReader::with_capacity(READ_BUFFER, handle)),
        };
        let mut mbox = MboxReader::new(reader).with_mangling(Mangling::None);

        loop {
            match mbox.read_message_into(&mut message) {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(err) => {
                    println!("  {} : {err}", file.folder);
                    stats.failed_messages += 1;
                    break;
                }
            }

            stats.raw_bytes += message.len() as u64;
            count_mangling(&message, &mut stats);

            let mozilla = mozilla::strip_in_place(&mut message);
            if mozilla.stripped_lines > 0 {
                stats.mozilla_headers += 1;
            }
            if mozilla.is_expunged() {
                stats.expunged += 1;
                stats.expunged_bytes += message.len() as u64;
                continue;
            }

            stats.messages += 1;
            stats.stored_bytes += message.len() as u64;
            if !seen.insert(*mailcore::BlobHash::of(&message).as_bytes()) {
                stats.duplicates += 1;
                stats.duplicate_bytes += message.len() as u64;
            }
        }

        let reader_stats = mbox.stats();
        stats.leading_garbage += reader_stats.leading_garbage;
    }

    stats.report(started.elapsed(), seen.len(), budget);
    Ok(())
}

#[derive(Debug, Default)]
struct ProbeStats {
    messages: u64,
    duplicates: u64,
    expunged: u64,
    failed_messages: u64,
    unreadable_files: u64,
    raw_bytes: u64,
    stored_bytes: u64,
    duplicate_bytes: u64,
    expunged_bytes: u64,
    leading_garbage: u64,
    /// Messages où un en-tête `X-Mozilla-Status` a effectivement été trouvé.
    ///
    /// Sans ce compteur, « zéro message supprimé » est ambigu : ça peut vouloir dire
    /// « aucun supprimé » ou « on n'a jamais su lire l'en-tête ».
    mozilla_headers: u64,
    /// Lignes `>From ` — présentes quelle que soit la convention.
    quoted_from: u64,
    /// Lignes `>>From ` ou plus. **Preuve** que l'écrivain est `mboxrd` : `mboxo` ne peut
    /// pas en produire, puisqu'il n'échappe jamais une ligne déjà échappée.
    double_quoted_from: u64,
}

impl ProbeStats {
    fn report(&self, elapsed: std::time::Duration, unique: usize, budget: Option<u64>) {
        let seconds = elapsed.as_secs_f64().max(f64::EPSILON);

        println!("\n--- sonde ---");
        if let Some(limit) = budget {
            println!("PARTIEL      plafond de {} par fichier", human(limit));
        }
        println!("Durée        {elapsed:.1?}");
        println!(
            "Débit        {}/s sur {} lus",
            human((self.raw_bytes as f64 / seconds) as u64),
            human(self.raw_bytes)
        );
        println!();
        println!("Messages     {}", self.messages);
        println!("Uniques      {unique}");
        println!(
            "Doublons     {} ({}), soit {} récupérables",
            self.duplicates,
            percent(self.duplicates, self.messages),
            human(self.duplicate_bytes)
        );
        println!(
            "En-têtes TB  {} messages portaient un X-Mozilla-*",
            self.mozilla_headers
        );
        println!(
            "Supprimés    {} non compactés, {} d'espace mort",
            self.expunged,
            human(self.expunged_bytes)
        );
        println!("En échec     {}", self.failed_messages);
        println!("Illisibles   {} fichiers", self.unreadable_files);
        if self.leading_garbage > 0 {
            println!(
                "Préambules   {} avant un premier séparateur",
                human(self.leading_garbage)
            );
        }
        println!();
        println!("Lignes >From    {}", self.quoted_from);
        println!("Lignes >>From   {}", self.double_quoted_from);
        if self.double_quoted_from > 0 {
            println!("→ L'écrivain échappe les lignes déjà échappées : convention mboxrd.");
        } else if self.quoted_from > 0 {
            println!("→ Aucun >>From : indistinguable. mboxo est possible, prudence.");
        } else {
            println!("→ Aucune ligne échappée dans l'échantillon : rien à conclure.");
        }
        println!();
        println!(
            "Taux de dédup   {} des messages, {} des octets",
            percent(self.duplicates, self.messages),
            percent(self.duplicate_bytes, self.stored_bytes)
        );
    }
}

/// Compte les lignes échappées, sans rien modifier.
fn count_mangling(message: &[u8], stats: &mut ProbeStats) {
    let mut at_line_start = true;
    let mut quotes = 0usize;

    for (index, &byte) in message.iter().enumerate() {
        if byte == b'\n' {
            at_line_start = true;
            quotes = 0;
            continue;
        }
        if at_line_start && byte == b'>' {
            quotes += 1;
            continue;
        }
        if quotes > 0 && message[index..].starts_with(b"From ") {
            if quotes == 1 {
                stats.quoted_from += 1;
            } else {
                stats.double_quoted_from += 1;
            }
        }
        at_line_start = false;
        quotes = 0;
    }
}

/// Relève récursivement (chemin relatif, taille, mtime en nanosecondes).
fn collect(
    root: &Path,
    dir: &Path,
    rows: &mut Vec<(String, u64, u128)>,
    unreadable: &mut u64,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            *unreadable += 1;
            return Ok(());
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            *unreadable += 1;
            continue;
        };
        if meta.is_dir() {
            collect(root, &path, rows, unreadable)?;
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let relative = path.strip_prefix(root).unwrap_or(&path);
        rows.push((
            relative.to_string_lossy().replace('\\', "/"),
            meta.len(),
            mtime,
        ));
    }
    Ok(())
}

fn read_snapshot(path: &Utf8PathBuf) -> Result<std::collections::HashMap<String, (u64, u128)>> {
    let text = std::fs::read_to_string(path.as_std_path())
        .with_context(|| format!("lecture du relevé {path}"))?;
    let mut out = std::collections::HashMap::new();

    for (number, line) in text.lines().enumerate() {
        let mut fields = line.rsplitn(3, '\t');
        let mtime = fields.next().unwrap_or_default();
        let size = fields.next().unwrap_or_default();
        let Some(file) = fields.next() else {
            bail!("{path}, ligne {} : trois colonnes attendues", number + 1);
        };
        let size: u64 = size
            .parse()
            .with_context(|| format!("{path}, ligne {} : taille illisible", number + 1))?;
        let mtime: u128 = mtime
            .parse()
            .with_context(|| format!("{path}, ligne {} : mtime illisible", number + 1))?;
        out.insert(file.to_owned(), (size, mtime));
    }
    Ok(out)
}

/// Range une différence selon qu'elle porte sur un mbox ou sur un fichier de Thunderbird.
fn classify(path: &str, what: &str, mbox: &mut Vec<String>, other: &mut Vec<String>) {
    let is_index = path.rsplit_once('.').is_some_and(|(_, ext)| {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "msf" | "dat" | "log" | "html" | "json" | "sqlite" | "sqlite3" | "bak" | "tmp"
        )
    });
    let line = format!("{what} : {path}");
    if is_index {
        other.push(line);
    } else {
        mbox.push(line);
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["o", "Kio", "Mio", "Gio", "Tio"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} o")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "n/a".to_owned();
    }
    format!("{:.1} %", (part as f64 / whole as f64) * 100.0)
}

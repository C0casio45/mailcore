//! Les PDF du corpus, **avant** d'en lire un seul : combien, sous quelle forme, et lesquels
//! regarder pour écrire la vérité terrain — `docs/PHASE-4.md`, entrée du 2026-10-20.
//!
//! ## Deux commandes, une seule règle
//!
//! `pdf-census` compte, `pdf-sample` tire. Les deux reconnaissent un PDF par
//! [`mailcore::attachtext::pieces`] — la fonction que l'enfant extracteur appellera —, jamais par
//! une copie : un banc qui réimplémente la règle qu'il compte ne compte rien. C'est la leçon du
//! recensement du 2026-09-22, qui avait sa propre copie du type MIME et ne classait pas comme
//! `attachtext` une pièce sans `Content-Type`.
//!
//! ## Une réplique, jamais le store
//!
//! `Store::open` applique les migrations : ouvrir le corpus — au schéma 15 le 2026-10-20 — avec un
//! binaire au schéma 16 l'écrirait. Les deux commandes ouvrent donc une copie d'`index.sqlite`,
//! journal WAL compris, les blobs liés et lus en place, sous `target/measure-pdf/`.
//!
//! ## Ce qui s'imprime, ce qui s'écrit
//!
//! `pdf-census` n'imprime que des comptes. `pdf-sample` écrit des documents réels — c'est son
//! rôle : quelqu'un doit les lire à l'œil — et refuse donc tout répertoire de sortie situé dans
//! le dépôt, qui est public.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use mail_parser::MimeHeaders;
use mailcore::Store;
use mailcore::attachtext::{Route, Verdict, pieces};

use crate::vectors::{copy_index, link_blobs, work_dir_in};

/// Ce qui suit un verdict dans le tableau croisé.
#[derive(Debug, Default)]
struct Tally {
    pieces: u64,
    messages: u64,
}

/// Un PDF accepté, par empreinte.
#[derive(Debug)]
pub(crate) struct Distinct {
    pub(crate) size: u64,
    pub(crate) route: Route,
    pub(crate) depth: u8,
    pub(crate) name: String,
    pub(crate) carriers: Vec<Carrier>,
}

/// Un message qui porte une pièce.
#[derive(Debug, Clone)]
pub(crate) struct Carrier {
    pub(crate) blob: mailcore::BlobHash,
    pub(crate) date: i64,
    pub(crate) subject: String,
}

/// Ce que le parcours d'un store rend.
#[derive(Debug, Default)]
pub(crate) struct Survey {
    messages: u64,
    unreadable: u64,
    pieces: u64,
    verdicts: BTreeMap<(Route, bool, bool, bool, u8), Tally>,
    accepted_pieces: u64,
    accepted_messages: u64,
    accepted_bytes: u64,
    pub(crate) distinct: HashMap<blake3::Hash, Distinct>,
    per_message: Vec<u64>,
    over_16_mib: u64,
    over_64_mib: u64,
    pieces_over_64_mib: u64,
    nested: u64,
    tnef_messages: u64,
    scan: Vec<Duration>,
}

/// Seize mébioctets : ce que la passe des vecteurs lit au plus d'un message (`MESSAGE_CAP`).
const SIXTEEN_MIB: u64 = 16 * 1024 * 1024;
const SIXTY_FOUR_MIB: u64 = 64 * 1024 * 1024;

/// Ouvre une réplique de lecture de `store` dans un répertoire jetable.
///
/// Le répertoire d'abord, le lien ensuite, le store enfin : détruits dans l'ordre inverse, le
/// lien des blobs est retiré **avant** l'effacement du répertoire — voir `vectors::Linked`.
pub(crate) fn with_replica<T>(
    store: &Utf8PathBuf,
    work: impl FnOnce(&Store) -> Result<T>,
) -> Result<T> {
    let dir = work_dir_in(&std::path::absolute("target/measure-pdf")?)?;
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
        .map_err(|path| anyhow::anyhow!("chemin non UTF-8 : {}", path.display()))?;
    copy_index(store, &root)?;
    let link = link_blobs(&store.join("blobs"), &root.join("blobs"))?;
    let replica = Store::open(&root).context("ouverture de la réplique")?;
    let result = work(&replica);
    drop(replica);
    drop(link);
    drop(dir);
    result
}

/// Parcourt les messages et range chaque pièce selon son verdict.
pub(crate) fn survey(store: &Store) -> Result<Survey> {
    let rows = store.all_for_indexing().context("lecture des messages")?;
    let parser = mail_parser::MessageParser::default();
    let mut out = Survey::default();
    let total = rows.len();

    for (seen, row) in rows.iter().enumerate() {
        if seen % 5_000 == 0 {
            eprint!("\r  {seen} / {total} messages lus…");
        }
        out.messages += 1;
        let Ok(raw) = store.blobs().read(row.blob) else {
            out.unreadable += 1;
            continue;
        };
        let started = Instant::now();
        let Some(parsed) = parser.parse(&raw) else {
            out.unreadable += 1;
            continue;
        };
        let found = pieces(&parsed);
        // Le coût d'un balayage, celui que la passe de fond paiera pour chaque message : analyser
        // le MIME et rendre les verdicts — pas lire le blob, que l'index lit déjà.
        out.scan.push(started.elapsed());

        let mut kinds: HashSet<(Route, bool, bool, bool, u8)> = HashSet::new();
        let mut accepted_here = 0u64;
        let mut tnef = false;
        for piece in &found {
            out.pieces += 1;
            let verdict = piece.verdict;
            tnef |= piece.part.content_type().is_some_and(|ct| {
                ct.subtype()
                    .is_some_and(|sub| sub.eq_ignore_ascii_case("ms-tnef"))
            });
            if verdict.route == Route::Undeclared && !verdict.signature {
                continue;
            }
            let key = key_of(&verdict);
            out.verdicts.entry(key).or_default().pieces += 1;
            kinds.insert(key);
            if !verdict.accepted() {
                continue;
            }
            let bytes = piece.part.contents();
            let size = bytes.len() as u64;
            accepted_here += 1;
            out.accepted_pieces += 1;
            out.accepted_bytes += size;
            if verdict.depth > 0 {
                out.nested += 1;
            }
            if size > SIXTY_FOUR_MIB {
                out.pieces_over_64_mib += 1;
            }
            let carrier = Carrier {
                blob: row.blob,
                date: row.date,
                subject: row.subject.clone(),
            };
            out.distinct
                .entry(blake3::hash(bytes))
                .and_modify(|seen| seen.carriers.push(carrier.clone()))
                .or_insert_with(|| Distinct {
                    size,
                    route: verdict.route,
                    depth: verdict.depth,
                    name: piece.part.attachment_name().unwrap_or("").to_owned(),
                    carriers: vec![carrier],
                });
        }
        for key in kinds {
            out.verdicts.entry(key).or_default().messages += 1;
        }
        if tnef {
            out.tnef_messages += 1;
        }
        if accepted_here > 0 {
            out.accepted_messages += 1;
            out.per_message.push(accepted_here);
            let len = raw.len() as u64;
            if len > SIXTEEN_MIB {
                out.over_16_mib += 1;
            }
            if len > SIXTY_FOUR_MIB {
                out.over_64_mib += 1;
            }
        }
    }
    eprintln!("\r  {total} / {total} messages lus.   ");
    Ok(out)
}

fn key_of(verdict: &Verdict) -> (Route, bool, bool, bool, u8) {
    (
        verdict.route,
        verdict.signature,
        verdict.text_decoded,
        verdict.encoding_problem,
        verdict.depth,
    )
}

/// Point d'entrée de `pdf-census`.
///
/// # Errors
///
/// Si le store est introuvable ou si la réplique ne peut pas être faite.
pub fn census(store: &Utf8PathBuf) -> Result<()> {
    let survey = with_replica(store, survey)?;
    print_census(store, &survey);
    Ok(())
}

fn print_census(store: &Utf8Path, s: &Survey) {
    println!("Store              {store}  (lu sur une réplique)");
    println!("Messages           {}", s.messages);
    if s.unreadable > 0 {
        println!(
            "Illisibles         {}  (blob absent ou en-têtes cassés)",
            s.unreadable
        );
    }
    println!(
        "Pièces feuilles    {}  (messages joints traversés)",
        s.pieces
    );
    println!();

    println!("Pièces qui ressemblent à un PDF — type ou nom, ou signature :");
    println!(
        "  {:<18} {:>9} {:>9} {:>9} {:>5} {:>8} {:>9}",
        "voie", "signature", "en texte", "encodage", "prof.", "pièces", "messages"
    );
    for (key, tally) in &s.verdicts {
        let (route, signature, text, encoding, depth) = *key;
        let accepted = Verdict {
            route,
            signature,
            text_decoded: text,
            encoding_problem: encoding,
            depth,
        }
        .accepted();
        println!(
            "  {:<18} {:>9} {:>9} {:>9} {:>5} {:>8} {:>9}  {}",
            format!("{route:?}"),
            yes(signature),
            yes(text),
            yes(encoding),
            depth,
            tally.pieces,
            tally.messages,
            if accepted { "retenue" } else { "écartée" }
        );
    }
    println!();

    let distinct_bytes: u64 = s.distinct.values().map(|d| d.size).sum();
    let shared = s.distinct.values().filter(|d| d.carriers.len() > 1).count();
    println!(
        "Retenues           {} pièces dans {} messages",
        s.accepted_pieces, s.accepted_messages
    );
    println!(
        "Empreintes         {} distinctes ({} portées par plusieurs messages)",
        s.distinct.len(),
        shared
    );
    println!(
        "Octets décodés     {} au total, {} distincts",
        mailapi::human::bytes(s.accepted_bytes),
        mailapi::human::bytes(distinct_bytes)
    );
    let mut sizes: Vec<u64> = s.distinct.values().map(|d| d.size).collect();
    sizes.sort_unstable();
    println!(
        "Taille d'un PDF    p50 {}  p90 {}  p99 {}  max {}",
        human_at(&sizes, 50),
        human_at(&sizes, 90),
        human_at(&sizes, 99),
        sizes
            .last()
            .map_or_else(|| "—".to_owned(), |&v| mailapi::human::bytes(v))
    );
    let mut per = s.per_message.clone();
    per.sort_unstable();
    println!(
        "PDF par message    p50 {}  p99 {}  max {}  (messages à plus de 10 : {})",
        at(&per, 50).unwrap_or(0),
        at(&per, 99).unwrap_or(0),
        per.last().copied().unwrap_or(0),
        per.iter().filter(|&&n| n > 10).count()
    );
    println!(
        "Imbriqués          {} pièces dans un message joint",
        s.nested
    );
    println!(
        "Gros messages      {} à plus de 16 Mio (la passe des vecteurs n'en lit que 16), {} à plus de 64 Mio",
        s.over_16_mib, s.over_64_mib
    );
    println!("Pièces > 64 Mio    {}", s.pieces_over_64_mib);
    println!(
        "ms-tnef            {} messages (hors périmètre)",
        s.tnef_messages
    );
    println!();

    if cfg!(debug_assertions) {
        println!(
            "Balayage           non chronométré en profil de débogage — cargo run --release -p xtask -- pdf-census"
        );
    } else {
        let mut scan = s.scan.clone();
        scan.sort_unstable();
        let total: Duration = scan.iter().sum();
        println!(
            "Balayage MIME      {:.1} s pour {} messages : p50 {}  p95 {}  max {}",
            total.as_secs_f64(),
            scan.len(),
            micros(at(&scan, 50)),
            micros(at(&scan, 95)),
            micros(scan.last().copied())
        );
    }
}

fn yes(value: bool) -> &'static str {
    if value { "oui" } else { "non" }
}

/// Le centile `p` d'une suite triée, au plus proche rang.
fn at<T: Copy>(sorted: &[T], p: usize) -> Option<T> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() * p).div_ceil(100).max(1) - 1;
    sorted.get(rank.min(sorted.len() - 1)).copied()
}

fn human_at(sorted: &[u64], p: usize) -> String {
    at(sorted, p).map_or_else(|| "—".to_owned(), mailapi::human::bytes)
}

fn micros(value: Option<Duration>) -> String {
    value.map_or_else(|| "—".to_owned(), |d| format!("{} µs", d.as_micros()))
}

// --- Le tirage ---------------------------------------------------------------------------------

/// Ce qu'on demande au tirage.
#[derive(Debug)]
pub struct SampleOptions {
    pub store: Utf8PathBuf,
    pub out: Utf8PathBuf,
    pub count: usize,
    pub reserve: usize,
    pub seed: String,
}

/// Une entrée du squelette de vérité terrain.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Entry {
    pub(crate) id: usize,
    pub(crate) set: String,
    pub(crate) hash: String,
    pub(crate) file: String,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) route: String,
    pub(crate) depth: u8,
    pub(crate) blobs: Vec<String>,
    pub(crate) first_date: String,
    pub(crate) first_subject: String,
    pub(crate) scan: bool,
    pub(crate) terms: Vec<String>,
    #[serde(default)]
    pub(crate) note: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Skeleton {
    pub(crate) pdf: Vec<Entry>,
}

/// L'ordre du tirage : l'empreinte des octets, rehachée avec la graine. Une empreinte BLAKE3 est
/// uniforme, donc cet ordre est un tirage sans remise — et il se refait à l'identique, ce qu'un
/// générateur aléatoire ne garantirait que s'il était lui-même épinglé.
fn draw_key(seed: &str, hash: &blake3::Hash) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(seed.as_bytes());
    hasher.update(hash.as_bytes());
    hasher.finalize()
}

/// Refuse un répertoire de sortie qui tomberait dans le dépôt : il recevrait des documents réels,
/// et le dépôt est public.
fn refuse_inside_repository(out: &Utf8Path) -> Result<()> {
    let repository = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("racine du dépôt")?;
    let repository = std::fs::canonicalize(repository).context("racine du dépôt")?;
    // L'ancêtre existant le plus proche : `canonicalize` demande un chemin qui existe, et rend la
    // casse réelle du disque — `F:\Dev` et `F:\dev` sont le même répertoire sous Windows.
    let mut probe = std::path::absolute(out.as_std_path()).context("chemin de sortie")?;
    while !probe.exists() {
        probe = probe
            .parent()
            .map(std::path::Path::to_path_buf)
            .context("aucun ancêtre du chemin de sortie n'existe")?;
    }
    let probe = std::fs::canonicalize(&probe).context("chemin de sortie")?;
    anyhow::ensure!(
        !probe.starts_with(&repository),
        "{out} est dans le dépôt, qui est public : le tirage y déposerait des documents réels"
    );
    Ok(())
}

/// Refuse une sortie qui serait publiée : dans le dépôt, seul `measurements/` est permis — il est
/// ignoré par git, et c'est là que vivent déjà les jeux de requêtes, qui nomment des messages réels.
fn refuse_published(out: &Utf8Path) -> Result<()> {
    if refuse_inside_repository(out).is_ok() {
        return Ok(());
    }
    let measurements = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("racine du dépôt")?
        .join("measurements");
    let measurements =
        std::fs::canonicalize(measurements.as_std_path()).context("measurements/ introuvable")?;
    let parent = out
        .parent()
        .filter(|p| !p.as_str().is_empty())
        .unwrap_or(Utf8Path::new("."));
    let parent = std::fs::canonicalize(parent.as_std_path())
        .with_context(|| format!("{parent} introuvable"))?;
    anyhow::ensure!(
        parent.starts_with(&measurements),
        "{out} est dans le dépôt, qui est public : seul measurements/, ignoré par git, est permis"
    );
    Ok(())
}

/// Point d'entrée de `pdf-sample`.
///
/// # Errors
///
/// Si la sortie est dans le dépôt ou n'est pas vide, si le store est illisible, ou si une pièce
/// tirée ne se retrouve plus.
pub fn sample(options: &SampleOptions) -> Result<()> {
    refuse_inside_repository(&options.out)?;
    if options.out.exists() {
        anyhow::ensure!(
            std::fs::read_dir(options.out.as_std_path())?
                .next()
                .is_none(),
            "{} n'est pas vide : le tirage n'écrase rien",
            options.out
        );
    }
    std::fs::create_dir_all(options.out.as_std_path())
        .with_context(|| format!("création de {}", options.out))?;

    let wanted = options.count + options.reserve;
    let written = with_replica(&options.store, |store| {
        let survey = survey(store)?;
        let mut order: Vec<(&blake3::Hash, &Distinct)> = survey.distinct.iter().collect();
        order.sort_by_key(|(hash, _)| *draw_key(&options.seed, hash).as_bytes());
        let chosen: Vec<(&blake3::Hash, &Distinct)> = order.into_iter().take(wanted).collect();
        anyhow::ensure!(
            chosen.len() == wanted,
            "{} PDF distincts seulement, {wanted} demandés",
            chosen.len()
        );

        let parser = mail_parser::MessageParser::default();
        let mut entries = Vec::with_capacity(wanted);
        for (index, (hash, distinct)) in chosen.iter().enumerate() {
            let first = &distinct.carriers[0];
            let raw = store
                .blobs()
                .read(first.blob)
                .context("relecture d'un porteur")?;
            let parsed = parser.parse(&raw).context("relecture d'un porteur")?;
            let bytes = pieces(&parsed)
                .into_iter()
                .filter(|piece| piece.verdict.accepted())
                .map(|piece| piece.part.contents().to_vec())
                .find(|bytes| blake3::hash(bytes) == **hash)
                .context("la pièce tirée ne se retrouve pas dans son message")?;
            let short = &hash.to_hex()[..8];
            let file = format!("{:02}-{short}.pdf", index + 1);
            std::fs::write(options.out.join(&file).as_std_path(), &bytes)
                .with_context(|| format!("écriture de {file}"))?;
            entries.push(Entry {
                id: index + 1,
                set: if index < options.count {
                    "decision"
                } else {
                    "reserve"
                }
                .to_owned(),
                hash: hash.to_hex().to_string(),
                file,
                name: distinct.name.clone(),
                size: distinct.size,
                route: format!("{:?}", distinct.route),
                depth: distinct.depth,
                blobs: distinct
                    .carriers
                    .iter()
                    .map(|c| c.blob.to_string())
                    .collect(),
                first_date: mailapi::human::date(first.date),
                first_subject: first.subject.clone(),
                scan: false,
                terms: Vec::new(),
                note: String::new(),
            });
        }
        Ok((entries, survey.distinct.len()))
    })?;

    let (entries, population) = written;
    let mut text = format!(
        "# Vérité terrain des PDF — mailcore, phase 4. HORS DÉPÔT : ce fichier nomme des documents réels.\n\
         # Tirage : graine {seed:?}, {wanted} empreintes sur {population} PDF distincts retenus, store {store}.\n\
         # « decision » : les {count} qui décident du choix de la bibliothèque. « reserve » : les {reserve}\n\
         # qui servent à mettre le banc au point sans toucher aux premiers.\n\
         # Pour chaque PDF : 4 à 8 termes qu'on chercherait, lus À L'ŒIL dans le document, jamais dans la\n\
         # sortie d'un extracteur ; `scan = true` si le document n'a pas de texte lisible à l'œil.\n\n",
        seed = options.seed,
        store = options.store,
        count = options.count,
        reserve = options.reserve,
    );
    text.push_str(&toml::to_string_pretty(&Skeleton { pdf: entries }).context("squelette")?);
    let skeleton = options.out.join("pdf-terms.skeleton.toml");
    std::fs::write(skeleton.as_std_path(), text)
        .with_context(|| format!("écriture de {skeleton}"))?;
    println!("{wanted} PDF et leur squelette écrits dans {}", options.out);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_draw_is_the_same_twice_and_changes_with_the_seed() {
        let a = blake3::hash(b"a");
        let b = blake3::hash(b"b");
        assert_eq!(draw_key("graine", &a), draw_key("graine", &a));
        assert_ne!(draw_key("graine", &a), draw_key("autre", &a));
        assert_ne!(draw_key("graine", &a), draw_key("graine", &b));
    }

    /// Le tirage dépose des documents réels : jamais dans le dépôt, même écrit avec une autre
    /// casse ou par un chemin relatif.
    #[test]
    fn a_sample_is_refused_inside_the_repository() {
        let inside = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("../measurements/x");
        assert!(refuse_inside_repository(&inside).is_err());
        // Sous Windows, la casse ne fait pas un autre répertoire.
        if cfg!(windows) {
            let upper = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR").to_uppercase()).join("y");
            assert!(refuse_inside_repository(&upper).is_err());
        }
        // Le contrôle inverse : un répertoire temporaire, hors du dépôt, est accepté.
        let outside = tempfile::tempdir().unwrap();
        let outside = Utf8PathBuf::from_path_buf(outside.path().join("tirage")).unwrap();
        assert!(refuse_inside_repository(&outside).is_ok());
    }

    #[test]
    fn a_centile_of_nothing_is_nothing() {
        assert_eq!(at::<u64>(&[], 50), None);
        assert_eq!(at(&[1, 2, 3, 4], 50), Some(2));
        assert_eq!(at(&[1, 2, 3, 4], 100), Some(4));
        assert_eq!(at(&[7], 1), Some(7));
    }
}

// --- Le gel de la vérité terrain ----------------------------------------------------------------

/// Ce qu'un lecteur a relevé pour une entrée du tirage.
#[derive(Debug, serde::Deserialize)]
struct Reading {
    id: usize,
    scan: bool,
    terms: Vec<String>,
    #[serde(default)]
    note: String,
}

/// Une liste de relevés, telle qu'écrite à la main, ou sous `items` — la forme que rend un lot de
/// lecteurs, éventuellement sous `result`.
fn readings_of(text: &str) -> Result<Vec<Reading>> {
    #[derive(serde::Deserialize)]
    struct Items {
        items: Vec<Reading>,
    }
    #[derive(serde::Deserialize)]
    struct Wrapped {
        result: Items,
    }
    if let Ok(list) = serde_json::from_str::<Vec<Reading>>(text) {
        return Ok(list);
    }
    if let Ok(items) = serde_json::from_str::<Items>(text) {
        return Ok(items.items);
    }
    Ok(serde_json::from_str::<Wrapped>(text)?.result.items)
}

/// Ce qu'on demande au gel.
#[derive(Debug)]
pub struct FreezeOptions {
    pub skeleton: Utf8PathBuf,
    pub readings: Vec<Utf8PathBuf>,
    pub rule: Utf8PathBuf,
    pub out: Utf8PathBuf,
}

/// Verse les relevés dans le squelette, la règle de décision en tête, et imprime l'empreinte du
/// fichier gelé.
///
/// ## Pourquoi un programme et pas une édition à la main
///
/// Cinquante entrées portent chacune une empreinte de 64 caractères et la liste de leurs
/// porteurs : une recopie à la main en perdrait une. Et le gel ne vaut que s'il est vérifiable —
/// l'empreinte imprimée est celle qu'on écrit dans le journal, et le banc la recalcule.
///
/// # Errors
///
/// Si la sortie existe déjà — un fichier gelé ne se réécrit pas —, si une entrée du squelette n'a
/// pas de relevé ou en a deux, ou si un relevé désigne une entrée absente.
pub fn freeze(options: &FreezeOptions) -> Result<()> {
    anyhow::ensure!(
        !options.out.exists(),
        "{} existe : un fichier gelé ne se réécrit pas — le changer se signale dans le journal",
        options.out
    );
    refuse_published(&options.out)?;
    let skeleton = std::fs::read_to_string(options.skeleton.as_std_path())
        .with_context(|| format!("lecture de {}", options.skeleton))?;
    let mut skeleton: Skeleton = toml::from_str(&skeleton).context("squelette illisible")?;
    let mut readings: HashMap<usize, Reading> = HashMap::new();
    for path in &options.readings {
        let text = std::fs::read_to_string(path.as_std_path())
            .with_context(|| format!("lecture de {path}"))?;
        let batch = readings_of(&text).with_context(|| format!("relevés illisibles : {path}"))?;
        for reading in batch {
            let id = reading.id;
            anyhow::ensure!(
                readings.insert(id, reading).is_none(),
                "entrée {id} relevée deux fois"
            );
        }
    }
    for entry in &mut skeleton.pdf {
        let reading = readings
            .remove(&entry.id)
            .with_context(|| format!("entrée {} sans relevé", entry.id))?;
        anyhow::ensure!(
            (4..=8).contains(&reading.terms.len()),
            "entrée {} : {} termes, 4 à 8 attendus",
            entry.id,
            reading.terms.len()
        );
        entry.scan = reading.scan;
        entry.terms = reading.terms;
        entry.note = reading.note;
    }
    if let Some(id) = readings.keys().next() {
        anyhow::bail!("relevé {id} sans entrée dans le squelette");
    }

    let rule = std::fs::read_to_string(options.rule.as_std_path())
        .with_context(|| format!("lecture de {}", options.rule))?;
    let mut text = rule;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push('\n');
    text.push_str(&toml::to_string_pretty(&skeleton).context("vérité terrain")?);
    std::fs::write(options.out.as_std_path(), &text)
        .with_context(|| format!("écriture de {}", options.out))?;
    let decision = skeleton.pdf.iter().filter(|e| e.set == "decision").count();
    let scans = skeleton
        .pdf
        .iter()
        .filter(|e| e.set == "decision" && e.scan)
        .count();
    println!("Gelé              {}", options.out);
    println!(
        "Empreinte BLAKE3  {}",
        blake3::hash(text.as_bytes()).to_hex()
    );
    println!(
        "Entrées           {} ({decision} de décision dont {scans} scans, {} de réserve)",
        skeleton.pdf.len(),
        skeleton.pdf.len() - decision
    );
    Ok(())
}

/// Point d'entrée de `pdf-catch-up` : la première lecture des PDF d'un store, telle que le démon la
/// fera au premier lancement après la migration — sur une réplique, `runs` fois, la première jetée
/// pour les temps.
///
/// Elle appelle [`maild::attachments::catch_up`], donc **l'enfant livré** : le binaire `maild`
/// compilé en release, relancé avec `--mailcore-attachments`. La crête de mémoire est celle des
/// enfants de ce processus, relevée toutes les 20 ms.
///
/// # Errors
///
/// Si le binaire est introuvable, la réplique impossible, ou la lecture en échec.
pub fn catch_up(store: &Utf8PathBuf, runs: usize) -> Result<()> {
    crate::semantic::release_only("pdf-catch-up")?;
    let program = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("racine du workspace")?
        .join("target")
        .join("release")
        .join(if cfg!(windows) { "maild.exe" } else { "maild" });
    let built = std::fs::metadata(program.as_std_path())
        .and_then(|meta| meta.modified())
        .with_context(|| format!("{program} introuvable : cargo build --release -p maild"))?;
    println!("Store              {store}  (lu sur une réplique)");
    println!(
        "Exécutable         {program} (compilé il y a {} min)",
        built.elapsed().unwrap_or_default().as_secs() / 60
    );
    println!("Exécutions         {runs} — la première jetée pour les temps");
    println!();

    let mut walls = Vec::new();
    for run in 1..=runs {
        let measured = with_replica(store, |replica| {
            let root = replica.root().to_owned();
            // Après un point de contrôle : voir `pdfeffect::sqlite_size`.
            let size = |root: &Utf8Path| crate::pdfeffect::sqlite_size(root);
            let before = size(&root);
            let pending = replica.unscanned_total()?;
            let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let sampler = {
                let (peak, done) = (std::sync::Arc::clone(&peak), std::sync::Arc::clone(&done));
                std::thread::spawn(move || children_peak(&peak, &done))
            };
            let started = Instant::now();
            let caught = maild::attachments::catch_up(&root, program.as_std_path());
            let wall = started.elapsed();
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = sampler.join();
            let (marked, remarked) = caught
                .map_err(|failure| anyhow::anyhow!("lecture en échec : {}", failure.code()))?;
            Ok((
                pending,
                marked,
                remarked,
                wall,
                peak.load(std::sync::atomic::Ordering::SeqCst),
                before,
                size(&root),
                replica.pdf_counts()?,
                replica.unscanned_total()?,
            ))
        })?;
        let (pending, marked, remarked, wall, peak, before, after, counts, left) = measured;
        println!(
            "Exécution {run}/{runs}     {pending} à lire, {marked} marqués, {remarked} remis, {left} restants \
             — {:.1} s, crête d'un enfant {:.1} Mio",
            wall.as_secs_f64(),
            peak as f64 / 1_048_576.0
        );
        if run == 1 {
            println!(
                "  base              {:.1} Mio avant, {:.1} Mio après (+{:.1} Mio)",
                before as f64 / 1_048_576.0,
                after as f64 / 1_048_576.0,
                after.saturating_sub(before) as f64 / 1_048_576.0
            );
            for (state, count) in &counts {
                println!("  {:<16}  {count}", format!("{state:?}"));
            }
        } else {
            walls.push(wall);
        }
    }
    if !walls.is_empty() {
        let secs: Vec<f64> = walls.iter().map(Duration::as_secs_f64).collect();
        let (low, high) = secs
            .iter()
            .fold((f64::MAX, 0.0_f64), |(l, h), s| (l.min(*s), h.max(*s)));
        println!();
        println!(
            "Temps retenus      {} — dispersion (max − min) / max {:.2}",
            secs.iter()
                .map(|s| format!("{s:.1} s"))
                .collect::<Vec<_>>()
                .join(", "),
            if high > 0.0 { (high - low) / high } else { 0.0 }
        );
    }
    Ok(())
}

/// La plus grande mémoire résidente d'un enfant de ce processus, relevée jusqu'à `done`.
pub(crate) fn children_peak(
    peak: &std::sync::atomic::AtomicU64,
    done: &std::sync::atomic::AtomicBool,
) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let me = Pid::from_u32(std::process::id());
    let mut system = System::new();
    while !done.load(std::sync::atomic::Ordering::SeqCst) {
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        for process in system.processes().values() {
            if process.parent() == Some(me) {
                peak.fetch_max(process.memory(), std::sync::atomic::Ordering::SeqCst);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

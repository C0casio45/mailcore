//! Vérifie une migration de schéma **sur une copie du store réel**, chronomètre en main.
//!
//! ## Pourquoi ça ne peut pas se limiter à un test unitaire
//!
//! Les tests de `store::migrations` tournent sur une base en mémoire de trois lignes. Ils
//! prouvent que le schéma dit ce qu'on croit ; ils ne disent rien de ce que la migration coûte
//! sur 102 760 messages et 43 Mo d'index, ni de ce qu'elle abîme.
//!
//! Deux questions, et une seule façon honnête d'y répondre :
//!
//! - **combien de temps ?** `ALTER TABLE ADD COLUMN` est censé être en temps constant en
//!   SQLite moderne — mais « censé » n'est pas une mesure, et une migration qui réécrirait la
//!   table ferait attendre l'utilisateur sur 43 Mo ;
//! - **qu'est-ce qui a bougé ?** Les comptes de lignes sont relevés avant et après. Une
//!   migration qui perd une référence est une migration qui perd du courrier.
//!
//! ## Sur une copie, jamais sur l'original
//!
//! La commande **refuse** de travailler sur un fichier qui n'est pas dans un répertoire
//! jetable qu'elle a créé elle-même : elle copie d'abord, migre la copie, et laisse
//! l'original intact. C'est la règle 1 du `CLAUDE.md` appliquée à ce qui n'est pas le profil
//! Thunderbird — un store de mesure est cher à reconstruire, 240 s d'import.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use camino::Utf8PathBuf;
use rusqlite::Connection;

/// Les tables dont le nombre de lignes doit être identique de part et d'autre.
///
/// Pas `sqlite_*` ni les nouvelles tables : on compare ce qui existait avant.
const WITNESSES: &[&str] = &["accounts", "folders", "messages", "refs", "threads"];

/// Migre une copie de `db` et rend le relevé.
pub fn check(db: &Utf8PathBuf, work: &Utf8PathBuf) -> Result<()> {
    anyhow::ensure!(db.exists(), "{db} n'existe pas");
    anyhow::ensure!(
        db != work,
        "la copie et l'original sont le même chemin : ce serait migrer l'original"
    );

    std::fs::create_dir_all(
        work.parent()
            .context("le chemin de travail n'a pas de parent")?,
    )
    .with_context(|| format!("création de {work}"))?;

    println!("Original    {db}");
    println!("Copie       {work}");

    let copied = Instant::now();
    let bytes = std::fs::copy(db.as_std_path(), work.as_std_path())
        .with_context(|| format!("copie de {db} vers {work}"))?;
    println!(
        "Copié       {:.1} Mo en {:.1} s",
        bytes as f64 / 1_048_576.0,
        copied.elapsed().as_secs_f64()
    );
    println!();

    // Ouverte sans les pragmas du store : ce qu'on mesure est la migration, pas le réglage
    // de SQLite. Les clés étrangères sont mises **avant** la migration, comme à l'ouverture
    // réelle, sinon une contrainte cassée passerait inaperçue ici et échouerait chez
    // l'utilisateur.
    let conn = Connection::open(work.as_std_path()).with_context(|| format!("ouverture {work}"))?;
    conn.pragma_update(None, "foreign_keys", true)
        .context("PRAGMA foreign_keys")?;

    let before = version(&conn)?;
    let witnesses_before = witnesses(&conn)?;
    println!(
        "Version     {before} → cible {}",
        mailcore::store::migrations::SCHEMA_VERSION
    );
    if before == mailcore::store::migrations::SCHEMA_VERSION {
        println!();
        println!("Déjà à jour : cette copie ne mesure rien. Repartir d'un store à l'ancienne");
        println!("version, ou il n'y a rien à vérifier.");
        return Ok(());
    }

    let at = Instant::now();
    mailcore::store::migrations::apply(&conn).context("migration")?;
    let took = at.elapsed();

    let after = version(&conn)?;
    let witnesses_after = witnesses(&conn)?;

    println!();
    println!("Migration   {:.1} ms", took.as_secs_f64() * 1000.0);
    println!("Version     {after}");
    println!();

    println!("Lignes, avant → après");
    let mut lost = Vec::new();
    for (name, count) in &witnesses_before {
        let now = witnesses_after
            .iter()
            .find(|(it, _)| it == name)
            .map_or(0, |(_, it)| *it);
        let verdict = if now == *count { "" } else { "  ← A BOUGÉ" };
        println!("  {name:<10} {count:>9} → {now:>9}{verdict}");
        if now != *count {
            lost.push(name.clone());
        }
    }

    // `PRAGMA foreign_key_check` sur la base entière : une migration qui laisse une
    // référence orpheline est une migration qui casse le store plus tard, pas maintenant.
    let orphans = foreign_key_violations(&conn)?;
    println!();
    println!("Références orphelines  {orphans}");

    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .context("PRAGMA integrity_check")?;
    println!("Intégrité SQLite       {integrity}");

    println!();
    if !lost.is_empty() {
        bail!(
            "la migration a changé le nombre de lignes de : {}",
            lost.join(", ")
        );
    }
    if orphans > 0 {
        bail!("la migration laisse {orphans} référence(s) orpheline(s)");
    }
    if integrity != "ok" {
        bail!("intégrité SQLite : {integrity}");
    }
    println!("Verdict     passé — rien perdu, rien orphelin, intégrité intacte");
    Ok(())
}

/// `PRAGMA user_version`.
fn version(conn: &Connection) -> Result<u32> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("PRAGMA user_version")
}

/// Le nombre de lignes de chaque table témoin.
fn witnesses(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut out = Vec::with_capacity(WITNESSES.len());
    for name in WITNESSES {
        // Le nom vient d'une constante de ce fichier, pas d'une entrée : l'interpoler est
        // sans risque, et `PRAGMA`/nom de table ne se paramètrent pas.
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |row| {
                row.get(0)
            })
            .with_context(|| format!("comptage de {name}"))?;
        out.push(((*name).to_owned(), count));
    }
    Ok(out)
}

/// Le nombre de violations de clé étrangère dans toute la base.
fn foreign_key_violations(conn: &Connection) -> Result<usize> {
    let mut statement = conn
        .prepare("PRAGMA foreign_key_check")
        .context("PRAGMA foreign_key_check")?;
    let mut rows = statement.query([]).context("foreign_key_check")?;
    let mut count = 0;
    while rows
        .next()
        .context("lecture de foreign_key_check")?
        .is_some()
    {
        count += 1;
    }
    Ok(count)
}

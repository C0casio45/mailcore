//! Ouverture de `index.sqlite` et pragmas.

use camino::Utf8Path;
use rusqlite::Connection;

use crate::error::Result;

/// Ouvre — et crée si besoin — l'index de métadonnées, pragmas appliqués et schéma migré.
///
/// # Errors
///
/// [`crate::Error::Sqlite`] si le fichier ne peut pas être ouvert ou si la migration
/// échoue, [`crate::Error::UnsupportedSchema`] si le store vient d'un binaire plus récent.
pub fn open(path: &Utf8Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    super::migrations::apply(&conn)?;
    Ok(conn)
}

/// Applique les pragmas de connexion.
///
/// | Pragma | Valeur | Pourquoi |
/// |---|---|---|
/// | `journal_mode` | `WAL` | Ce qui permet aux clients de lire pendant que le démon écrit. Prérequis de la règle 3 : l'UI ne bloque jamais. Persistant, écrit dans le fichier. |
/// | `synchronous` | `NORMAL` | Compromis assumé : en WAL, `NORMAL` ne risque de perdre que les dernières transactions sur coupure d'alimentation, jamais l'intégrité du fichier. Un store perdu se réimporte ; `FULL` coûterait un `fsync` par transaction sur un import de plusieurs centaines de milliers de messages. |
/// | `foreign_keys` | `ON` | Désactivé par défaut dans SQLite, et par connexion. Sans lui, `refs` accepterait des références vers des dossiers inexistants — la seule contrainte qui garde le modèle cohérent. |
/// | `busy_timeout` | 5 s | Un lecteur qui tombe sur un verrou d'écriture attend au lieu d'échouer. |
/// | `temp_store` | `MEMORY` | Les tris et index temporaires en RAM plutôt que sur disque. |
///
/// Volontairement absents : `cache_size` et `mmap_size`. Ce sont des réglages à mesurer sur
/// le corpus réel à l'étape 4, pas à devenir des nombres magiques choisis d'avance.
///
/// # Errors
///
/// [`crate::Error::Sqlite`] si un pragma est refusé.
pub fn apply_pragmas(conn: &Connection) -> Result<()> {
    // `journal_mode` rend une ligne, donc `query_row` et non `execute`. La valeur rendue est
    // ignorée sciemment : une base en mémoire répond « memory » et c'est correct.
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;

    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8Path::from_path(dir.path())
            .unwrap()
            .join("index.sqlite");
        let conn = open(&path).unwrap();
        (dir, conn)
    }

    #[test]
    fn opens_migrated_and_ready() {
        let (_dir, conn) = temp_db();
        assert_eq!(
            super::super::migrations::user_version(&conn).unwrap(),
            super::super::migrations::SCHEMA_VERSION
        );
    }

    #[test]
    fn wal_is_enabled_on_a_real_file() {
        let (_dir, conn) = temp_db();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn foreign_keys_are_on() {
        let (_dir, conn) = temp_db();
        let on: bool = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert!(on);
    }

    #[test]
    fn a_reader_is_not_blocked_by_an_open_writer() {
        // Le prérequis de « l'UI ne bloque jamais », vérifié plutôt que supposé : pendant
        // qu'une transaction d'écriture est ouverte et non validée, un autre processus doit
        // pouvoir lire — et voir l'état d'avant.
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8Path::from_path(dir.path())
            .unwrap()
            .join("index.sqlite");

        let writer = open(&path).unwrap();
        let reader = open(&path).unwrap();

        writer
            .execute(
                "INSERT INTO accounts (id, kind, display_name) VALUES (1, 'mbox', 'A')",
                [],
            )
            .unwrap();

        let tx = writer.unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO accounts (id, kind, display_name) VALUES (2, 'mbox', 'B')",
            [],
        )
        .unwrap();

        // Écriture en cours, non validée : la lecture passe et ne voit qu'une ligne.
        let seen: i64 = reader
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seen, 1, "le lecteur a été bloqué ou a vu du non-validé");

        tx.commit().unwrap();

        let seen: i64 = reader
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seen, 2);
    }
}

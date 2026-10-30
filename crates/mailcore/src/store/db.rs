//! Ouverture de `index.sqlite` et pragmas.

use camino::Utf8Path;
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};

use crate::error::{Error, Result};

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

/// Ouvre l'index de métadonnées **en lecture**, sans jamais le créer, le migrer ni l'écrire.
///
/// Le pendant de [`open`] pour `Store::open_reader`. La première barrière suffit aujourd'hui ;
/// chacune des deux autres tient seule ce que la première tient, pour le jour où elle tomberait —
/// un drapeau changé par commodité, une connexion partagée avec un écrivain. Un test vérifie
/// chacune des trois séparément, puisqu'aucune écriture refusée ne dirait laquelle a refusé :
///
/// - **`SQLITE_OPEN_READ_ONLY`, sans `SQLITE_OPEN_CREATE`** : la base est ouverte en lecture par
///   le système. Aucune instruction ne peut y écrire, ni le point de contrôle que la dernière
///   connexion fait en se fermant — il demande un verrou exclusif, que SQLite refuse sur une
///   poignée en lecture sous Windows, et que `fcntl` refuse sous Unix. Un fichier absent est une
///   erreur au lieu d'une base vide ;
/// - **`query_only`** : une écriture est refusée avant d'atteindre le fichier ;
/// - **pas de point de contrôle à la fermeture** (`SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE`) : sans la
///   première barrière, la dernière connexion reporterait le journal dans la base puis effacerait
///   `-wal` et `-shm` — vérifié en retirant les deux.
///
/// Et **aucun `PRAGMA journal_mode = WAL`** : c'est une écriture dans l'en-tête de la base.
///
/// ## Ce qu'un lecteur écrit quand même : l'index partagé du journal
///
/// Une base en mode WAL se lit à travers deux fichiers voisins, et la documentation de SQLite
/// (« Read-Only Databases ») dit à quelle condition un lecteur peut s'en servir : `-wal` et `-shm`
/// **existent déjà**, ou le répertoire permet de les **créer**. Ce que ça donne, établi par les tests
/// ci-dessous :
///
/// - `-shm` est **écrit** — c'est la mémoire partagée par où lecteurs et écrivains se
///   coordonnent : marques de lecture, et l'index du journal que la première connexion reconstruit ;
/// - `-wal` n'est **jamais écrit** : il est lu, et seul un écrivain y ajoute ;
/// - les deux **sont créés s'ils manquent** — `-wal` vide, `-shm` de 32 Kio —, puis laissés sur le
///   disque, faute de point de contrôle à la fermeture ;
/// - un `-shm` en lecture seule — `SQLITE_READONLY_CANTINIT` dans la documentation — ne remonte
///   pas jusqu'ici : SQLite lit alors le journal dans une mémoire privée ;
/// - ni présents ni créables, la lecture est refusée par [`Error::WalUnavailable`], qui nomme
///   ce qui manque.
///
/// # Errors
///
/// [`Error::Sqlite`] si le fichier ne s'ouvre pas, [`Error::WalUnavailable`] si son journal ne
/// peut être ni ouvert ni créé, [`Error::SchemaBehind`] ou [`Error::UnsupportedSchema`] s'il n'est
/// pas au schéma de ce binaire.
pub fn open_reader(path: &Utf8Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    // Les mêmes qu'un écrivain, pour la même raison : un lecteur qui tombe sur le verrou d'un
    // point de contrôle attend au lieu d'échouer.
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "query_only", true)?;

    // **La première lecture est celle qui ouvre le journal** : `open_with_flags` n'a ouvert que la
    // base. Un refus d'ouvrir à ce moment-là porte donc sur `-wal` ou `-shm`, pas sur la base.
    match super::migrations::require_current(&conn) {
        Err(Error::Sqlite(source)) if journal_unavailable(&source) => Err(Error::WalUnavailable {
            path: path.to_owned(),
            source,
        }),
        Err(other) => Err(other),
        Ok(()) => Ok(conn),
    }
}

/// L'échec d'un lecteur qui ne peut ni ouvrir ni créer `-wal` ou `-shm`.
///
/// Deux codes pour le même cas, selon le système : Windows rend l'échec d'ouverture ordinaire,
/// Unix distingue un répertoire sans droit d'écriture (`SQLITE_READONLY_DIRECTORY`).
fn journal_unavailable(source: &rusqlite::Error) -> bool {
    let rusqlite::Error::SqliteFailure(failure, _) = source else {
        return false;
    };
    failure.code == rusqlite::ErrorCode::CannotOpen
        || failure.extended_code == rusqlite::ffi::SQLITE_READONLY_DIRECTORY
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
    use super::super::migrations::SCHEMA_VERSION;
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

    // --- `open_reader` ---------------------------------------------------------------------

    /// Une base migrée, un compte, connexion fermée : comme la laisse un écrivain qui s'arrête
    /// proprement — la dernière connexion a reporté le journal et effacé `-wal` et `-shm`.
    fn closed_base() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8Path::from_path(dir.path())
            .unwrap()
            .join("index.sqlite");
        let writer = open(&path).unwrap();
        writer
            .execute(
                "INSERT INTO accounts (id, kind, display_name) VALUES (1, 'mbox', 'A')",
                [],
            )
            .unwrap();
        drop(writer);
        (dir, path)
    }

    /// Comme [`closed_base`], mais l'écrivain s'arrête **sans point de contrôle** : un second compte
    /// n'est que dans `-wal`, et `-shm` reste. C'est l'état du corpus de mesure, qui a ses deux
    /// fichiers voisins. `checkpointed` reporte le journal avant de partir, sans l'effacer.
    fn base_with_journal(checkpointed: bool) -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let (dir, path) = closed_base();
        let writer = open(&path).unwrap();
        writer
            .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
            .unwrap();
        writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        writer
            .execute(
                "INSERT INTO accounts (id, kind, display_name) VALUES (2, 'mbox', 'journal')",
                [],
            )
            .unwrap();
        if checkpointed {
            let _: (i64, i64, i64) = writer
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .unwrap();
        }
        drop(writer);
        assert!(neighbour(&path, "-wal").is_file(), "précondition : -wal");
        assert!(neighbour(&path, "-shm").is_file(), "précondition : -shm");
        (dir, path)
    }

    fn neighbour(path: &Utf8Path, suffix: &str) -> camino::Utf8PathBuf {
        camino::Utf8PathBuf::from(format!("{path}{suffix}"))
    }

    fn accounts(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap()
    }

    /// Ce que la base seule contient, sans son journal : une copie du fichier, ailleurs.
    fn accounts_without_the_journal(path: &Utf8Path) -> i64 {
        let elsewhere = tempfile::tempdir().unwrap();
        let copy = elsewhere.path().join("index.sqlite");
        std::fs::copy(path, &copy).unwrap();
        let conn = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        accounts(&conn)
    }

    #[test]
    fn a_write_through_a_reader_fails_in_sqlite() {
        let (_dir, path) = closed_base();
        let reader = open_reader(&path).unwrap();
        let refused = reader
            .execute(
                "INSERT INTO accounts (id, kind, display_name) VALUES (9, 'mbox', 'Z')",
                [],
            )
            .unwrap_err();
        assert_eq!(
            refused.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ReadOnly),
            "{refused}"
        );
        // Une migration n'est pas plus permise qu'une ligne.
        assert!(reader.execute_batch("CREATE TABLE t (x)").is_err());
        assert!(
            reader
                .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .is_err()
        );
        drop(reader);
        assert_eq!(accounts(&open(&path).unwrap()), 1);
    }

    /// Les trois barrières, chacune vérifiée seule : l'une tombée, une autre refuserait encore à
    /// sa place, et les tests de comportement ne le verraient pas.
    #[test]
    fn the_three_barriers_of_a_reader_are_up() {
        let (_dir, path) = closed_base();
        let reader = open_reader(&path).unwrap();
        assert!(
            reader.is_readonly(rusqlite::MAIN_DB).unwrap(),
            "la base n'est pas ouverte en lecture par SQLite"
        );
        let query_only: bool = reader
            .query_row("PRAGMA query_only", [], |r| r.get(0))
            .unwrap();
        assert!(query_only, "query_only n'est pas posé");
        assert!(
            reader
                .db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE)
                .unwrap(),
            "la fermeture ferait un point de contrôle"
        );
    }

    #[test]
    fn a_reader_neither_creates_nor_migrates() {
        // Pas de fichier : une erreur, et rien de créé.
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8Path::from_path(dir.path())
            .unwrap()
            .join("index.sqlite");
        assert!(open_reader(&path).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        // Un schéma ancien — **un vrai**, que `open` migrerait : refusé, et ni la base ni son
        // journal n'ont bougé d'un octet. Un store dont on n'aurait réécrit que la version ne se
        // migre pas — le pas suivant recrée une table qui existe —, et un lecteur qui migrerait
        // passerait ce test sans rien écrire.
        let older = SCHEMA_VERSION - 1;
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8Path::from_path(dir.path())
            .unwrap()
            .join("index.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            apply_pragmas(&conn).unwrap();
            super::super::migrations::apply_up_to(&conn, older).unwrap();
        }
        assert!(
            !neighbour(&path, "-wal").exists(),
            "précondition : une base fermée"
        );
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            open_reader(&path),
            Err(Error::SchemaBehind { found, expected })
                if found == older && expected == SCHEMA_VERSION
        ));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "la base a été écrite"
        );
        // Et pas davantage le journal : une migration validée y resterait, invisible dans la base
        // tant que personne n'a fait de point de contrôle.
        assert_eq!(
            std::fs::metadata(neighbour(&path, "-wal"))
                .map(|m| m.len())
                .ok(),
            Some(0),
            "le journal a été écrit"
        );
        let version: u32 = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, older, "le store a été migré");
    }

    /// Le cas du corpus de mesure : `-wal` porte des pages que la base n'a pas encore, `-shm` est là.
    /// Le lecteur les voit, et ne touche ni à la base ni au journal — un point de contrôle à la
    /// fermeture aurait écrit l'une et effacé l'autre : c'est ce que fait `open`, l'écrivain.
    #[test]
    fn a_reader_reads_what_only_the_journal_holds_and_writes_neither() {
        let (_dir, path) = base_with_journal(false);
        assert_eq!(
            accounts_without_the_journal(&path),
            1,
            "précondition : le second compte n'est que dans le journal"
        );
        let base = std::fs::read(&path).unwrap();
        let wal = std::fs::read(neighbour(&path, "-wal")).unwrap();

        let reader = open_reader(&path).unwrap();
        assert_eq!(accounts(&reader), 2, "le journal n'a pas été lu");
        drop(reader);

        assert_eq!(std::fs::read(&path).unwrap(), base, "la base a été écrite");
        assert_eq!(
            std::fs::read(neighbour(&path, "-wal")).unwrap(),
            wal,
            "le journal a été écrit ou effacé"
        );
    }

    /// Un journal entièrement reporté : la dernière connexion qui se ferme l'efface d'ordinaire —
    /// il n'y a rien à écrire dans la base, seulement deux fichiers à retirer. Un lecteur ne le
    /// fait pas.
    #[test]
    fn a_reader_erases_no_journal_when_it_closes() {
        let (_dir, path) = base_with_journal(true);
        let wal = std::fs::read(neighbour(&path, "-wal")).unwrap();
        assert!(!wal.is_empty(), "précondition : un journal non vide");

        let reader = open_reader(&path).unwrap();
        assert_eq!(accounts(&reader), 2);
        drop(reader);

        assert_eq!(
            std::fs::read(neighbour(&path, "-wal")).ok(),
            Some(wal),
            "le journal a été effacé ou écrit"
        );
        assert!(neighbour(&path, "-shm").is_file(), "-shm a été effacé");
    }

    /// Ce qu'un lecteur laisse derrière lui sur une base fermée proprement — sans `-wal` ni `-shm` :
    /// SQLite les crée pour lire (documenté), et faute de point de contrôle à la fermeture, ils
    /// restent. `-wal` est vide ; la base n'a pas bougé.
    #[test]
    fn a_reader_creates_the_missing_journal_files_and_leaves_them_empty_handed() {
        let (_dir, path) = closed_base();
        let (wal, shm) = (neighbour(&path, "-wal"), neighbour(&path, "-shm"));
        assert!(!wal.exists() && !shm.exists(), "précondition : base fermée");
        let base = std::fs::read(&path).unwrap();

        let reader = open_reader(&path).unwrap();
        assert_eq!(accounts(&reader), 1);
        drop(reader);

        assert_eq!(std::fs::read(&path).unwrap(), base, "la base a été écrite");
        assert_eq!(
            std::fs::metadata(&wal).map(|m| m.len()).ok(),
            Some(0),
            "-wal : créé vide et laissé"
        );
        // Une région de l'index du journal : 32 Kio, la taille de page que SQLite lui donne.
        assert_eq!(
            std::fs::metadata(&shm).map(|m| m.len()).ok(),
            Some(32 * 1024),
            "-shm : créé et laissé"
        );
    }

    /// `SQLITE_READONLY_CANTINIT` : un `-shm` qu'on ne peut pas écrire, et aucun écrivain pour en
    /// répondre. La documentation dit que SQLite lit alors le journal dans une mémoire privée, et
    /// que le code ne remonte pas — vérifié, sur une ligne qui n'est **que** dans le journal.
    #[test]
    fn a_read_only_shm_still_reads_through_the_journal() {
        let (_dir, path) = base_with_journal(false);
        assert_eq!(accounts_without_the_journal(&path), 1, "précondition");
        let shm = neighbour(&path, "-shm");
        let set_readonly = |readonly: bool| {
            let mut permissions = std::fs::metadata(&shm).unwrap().permissions();
            permissions.set_readonly(readonly);
            std::fs::set_permissions(&shm, permissions).unwrap();
        };
        set_readonly(true);
        assert!(
            std::fs::OpenOptions::new().write(true).open(&shm).is_err(),
            "précondition : -shm est encore inscriptible — le test ne passerait pas par CANTINIT"
        );
        let shm_before = std::fs::read(&shm).unwrap();

        let read = open_reader(&path).map(|reader| accounts(&reader));

        let shm_after = std::fs::read(&shm).unwrap();
        // Rendu inscriptible avant toute assertion : un `-shm` en lecture seule empêcherait le
        // répertoire temporaire de s'effacer sous Windows.
        set_readonly(false);
        assert_eq!(read.unwrap(), 2, "le journal n'a pas été lu");
        assert_eq!(shm_after, shm_before);
    }

    /// Ni présents ni créables : la documentation dit qu'un lecteur ne peut pas lire. Le refus
    /// nomme ce qui manque, au lieu de « unable to open database file », qui accuse la base.
    ///
    /// Le journal impossible à ouvrir est fabriqué par un **répertoire** à sa place : portable, et
    /// le même chemin dans SQLite qu'un répertoire sans droit d'écriture sous Windows. Sous Unix,
    /// où ce dernier cas rend un autre code, il a son test ci-dessous.
    #[test]
    fn a_journal_that_cannot_be_opened_is_named_in_the_refusal() {
        let (_dir, path) = closed_base();
        std::fs::create_dir(neighbour(&path, "-wal")).unwrap();
        let refused = open_reader(&path).unwrap_err();
        assert!(
            matches!(refused, Error::WalUnavailable { .. }),
            "{refused:?}"
        );
        assert!(refused.to_string().contains("-wal et -shm"), "{refused}");
    }

    /// Le cas réel sous Unix : un répertoire sans droit d'écriture, et pas de journal. SQLite rend
    /// `SQLITE_READONLY_DIRECTORY`, et non l'échec d'ouverture de Windows.
    #[cfg(unix)]
    #[test]
    fn a_journal_that_cannot_be_created_is_named_in_the_refusal() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, path) = closed_base();
        let lock = |mode: u32| {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        };
        lock(0o555);
        let writable = std::fs::File::create(dir.path().join("sonde")).is_ok();
        let refused = open_reader(&path).map(drop);
        lock(0o755);
        assert!(
            !writable,
            "précondition : le répertoire est encore inscriptible — compte privilégié ?"
        );
        assert!(
            matches!(refused, Err(Error::WalUnavailable { .. })),
            "{refused:?}"
        );
    }
}

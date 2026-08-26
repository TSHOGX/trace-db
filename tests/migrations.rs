use rusqlite::Connection;
use tempfile::tempdir;
use tracedb::{open_database, SchemaVersionMismatch, TraceDb};

const HISTORICAL_V1: &str = include_str!("fixtures/migrations/v1_baseline.sql");

#[test]
fn a_historical_archive_is_refused_with_reingest_guidance() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("historical-v1.db");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(HISTORICAL_V1).unwrap();
    drop(connection);

    // The normalized layer is a rebuildable projection, so TraceDB refuses a
    // foreign schema instead of carrying per-column upgrade paths. The refusal is
    // asserted as a type rather than as a message: prose is guidance a release may
    // reword, while the versions are the contract a caller recovers from.
    let error = match TraceDb::open(&path) {
        Ok(_) => panic!("a historical archive must not open"),
        Err(error) => error,
    };
    let mismatch = error
        .downcast_ref::<SchemaVersionMismatch>()
        .unwrap_or_else(|| panic!("refusal must survive anyhow wrapping as a type: {error:?}"));
    assert_eq!(mismatch.found, 1);
    assert_eq!(mismatch.expected, tracedb::store::SCHEMA_VERSION);
}

#[test]
fn a_foreign_schema_refusal_is_recoverable_by_rebuilding_in_process() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("historical-v1.db");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(HISTORICAL_V1).unwrap();
    drop(connection);

    // Recovery is the point of the typed error: an embedded host has no CLI to
    // shell out to, so it must be able to discard a derived projection itself.
    let database = TraceDb::open_or_rebuild(&path).unwrap();
    drop(database);

    let metadata = open_database(&path).unwrap();
    let version: String = metadata
        .query_row(
            "SELECT value FROM schema_meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, tracedb::store::SCHEMA_VERSION.to_string());
    // The rebuilt archive is empty and usable, not merely re-stamped.
    assert_eq!(
        metadata
            .query_row("SELECT count(*) FROM sessions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn rebuilding_removes_the_wal_sidecars_with_the_archive() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("sidecars.db");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(HISTORICAL_V1).unwrap();
    drop(connection);

    // A `-wal` left beside a fresh database is adopted by it and replays frames
    // from an archive that no longer exists, so all three files go together.
    let wal = directory.path().join("sidecars.db-wal");
    let shm = directory.path().join("sidecars.db-shm");
    std::fs::write(&wal, b"stale write-ahead log").unwrap();
    std::fs::write(&shm, b"stale shared memory index").unwrap();

    tracedb::store::remove_archive_files(&path).unwrap();

    for orphan in [&path, &wal, &shm] {
        assert!(
            !orphan.exists(),
            "removal must not leave {} behind",
            orphan.display()
        );
    }
    // Removing an already-absent archive is not an error: recovery has to be
    // callable without first probing for what exists.
    tracedb::store::remove_archive_files(&path).unwrap();
}

#[test]
fn open_or_rebuild_preserves_an_archive_that_failed_for_any_other_reason() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("corrupt.db");
    // A file that is not a database at all fails to open for a reason no rebuild
    // is known to be safe for, so it must survive to be diagnosed by a human.
    let corrupt = b"this is not an SQLite database".to_vec();
    std::fs::write(&path, &corrupt).unwrap();

    let error = match TraceDb::open_or_rebuild(&path) {
        Ok(_) => panic!("a corrupt archive must not open"),
        Err(error) => error,
    };
    assert!(
        error.downcast_ref::<SchemaVersionMismatch>().is_none(),
        "a corrupt archive must not be reported as a schema mismatch: {error:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), corrupt);
}

#[test]
fn a_fresh_archive_records_the_current_schema_contract() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("fresh.db");
    TraceDb::open(&path).unwrap();

    let metadata = open_database(&path).unwrap();
    let values = ["schema_version", "tokenizer"]
        .into_iter()
        .map(|key| {
            metadata
                .query_row("SELECT value FROM schema_meta WHERE key=?1", [key], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        [
            tracedb::store::SCHEMA_VERSION.to_string(),
            tracedb::store::PORTABLE_TOKENIZER.to_string(),
        ]
    );
}

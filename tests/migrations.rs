use rusqlite::Connection;
use tempfile::tempdir;
use tracedb::{open_database, TraceDb};

const HISTORICAL_V1: &str = include_str!("fixtures/migrations/v1_baseline.sql");

#[test]
fn a_historical_archive_is_refused_with_reingest_guidance() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("historical-v1.db");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(HISTORICAL_V1).unwrap();
    drop(connection);

    // The normalized layer is a rebuildable projection, so TraceDB refuses a
    // foreign schema instead of carrying per-column upgrade paths.
    let error = match TraceDb::open(&path) {
        Ok(_) => panic!("a historical archive must not open"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("schema version 1") && error.contains("re-run `trace-db ingest`"),
        "unexpected refusal message: {error}"
    );
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

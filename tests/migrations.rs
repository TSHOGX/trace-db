use rusqlite::Connection;
use tempfile::tempdir;
use tracedb::{open_database, SearchRequest, TraceDb};

const HISTORICAL_V1: &str = include_str!("fixtures/migrations/v1_baseline.sql");

#[test]
fn historical_v1_fixture_migrates_without_losing_searchable_data() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("historical-v1.db");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(HISTORICAL_V1).unwrap();
    drop(connection);

    let database = TraceDb::open(&path).unwrap();
    let stats = database.stats().unwrap();
    assert_eq!(stats.total_sessions, 1);
    assert_eq!(stats.total_events, 2);

    let matches = database
        .search(SearchRequest::new("legacy deploy"))
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].id, "codex:historical-v1");

    let metadata = open_database(&path).unwrap();
    let values = ["schema_version", "tokenizer", "archive_contract"]
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
            tracedb::store::ARCHIVE_CONTRACT.to_string(),
        ]
    );
}

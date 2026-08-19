//! End-to-end: load the built extension into a bundled SQLite, register the
//! `jieba` tokenizer, and verify real FTS5 behavior for Chinese, English
//! (with Porter stemming), and mixed text — including that offset-dependent
//! snippet() works, which proves byte offsets survive the round trip.

use rusqlite::Connection;

fn extension_path() -> String {
    let ext = if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    };
    let base = env!("CARGO_MANIFEST_DIR");
    let candidates = [
        format!("{base}/target/debug/libfts5jieba.{ext}"),
        format!("{base}/target/debug/deps/libfts5jieba.{ext}"),
    ];
    for c in candidates.iter() {
        if std::path::Path::new(c).exists() {
            return c.clone();
        }
    }
    panic!("built extension not found; run `cargo build` first. tried: {candidates:?}");
}

fn open() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    unsafe {
        conn.load_extension_enable().unwrap();
        conn.load_extension(extension_path(), Some("sqlite3_fts5jieba_init"))
            .unwrap();
        conn.load_extension_disable().unwrap();
    }
    conn
}

fn count(conn: &Connection, query: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM docs WHERE docs MATCH ?1",
        [query],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn chinese_word_segmentation() {
    let conn = open();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE docs USING fts5(body, tokenize='jieba');
         INSERT INTO docs(body) VALUES ('中华人民共和国国歌');",
    )
    .unwrap();
    // jieba segments into words; sub-words should match.
    assert_eq!(count(&conn, "中华"), 1);
    assert_eq!(count(&conn, "共和国"), 1);
    // a substring that is NOT a jieba word boundary should not match
    assert_eq!(count(&conn, "国国"), 0);
}

#[test]
fn english_stemming_matches_inflections() {
    let conn = open();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE docs USING fts5(body, tokenize='jieba');
         INSERT INTO docs(body) VALUES ('He was running quickly');",
    )
    .unwrap();
    // document 'running' indexes both 'running' and stem 'run'
    assert_eq!(count(&conn, "running"), 1, "exact form");
    assert_eq!(count(&conn, "run"), 1, "stem of document term matches");
    // query 'runs' -> stem 'run' -> matches the doc's 'run' posting
    assert_eq!(count(&conn, "runs"), 1, "query inflection matches via stem");
    assert_eq!(count(&conn, "quick"), 1, "quickly -> quick");
}

#[test]
fn mixed_chinese_english_numbers() {
    let conn = open();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE docs USING fts5(body, tokenize='jieba');
         INSERT INTO docs(body) VALUES ('Apple 苹果手机 iPhone 15');",
    )
    .unwrap();
    assert_eq!(count(&conn, "apple"), 1, "case-folded english");
    assert_eq!(count(&conn, "苹果"), 1, "chinese word");
    assert_eq!(count(&conn, "手机"), 1, "chinese word");
    assert_eq!(count(&conn, "iphone"), 1, "case-folded");
    assert_eq!(count(&conn, "15"), 1, "number token");
}

#[test]
fn nostem_arg_disables_stemming() {
    let conn = open();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE docs USING fts5(body, tokenize='jieba nostem');
         INSERT INTO docs(body) VALUES ('running');",
    )
    .unwrap();
    assert_eq!(count(&conn, "running"), 1, "exact still matches");
    assert_eq!(count(&conn, "run"), 0, "no stem posting when nostem");
}

#[test]
fn diacritics_are_folded() {
    let conn = open();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE docs USING fts5(body, tokenize='jieba');
         INSERT INTO docs(body) VALUES ('Café résumé');",
    )
    .unwrap();
    assert_eq!(count(&conn, "cafe"), 1);
    assert_eq!(count(&conn, "resume"), 1);
}

#[test]
fn snippet_offsets_are_correct() {
    // Wrong byte offsets would slice mid-codepoint inside SQLite. A clean
    // highlight of the ORIGINAL-cased token proves offsets point at the source.
    let conn = open();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE docs USING fts5(body, tokenize='jieba');
         INSERT INTO docs(body) VALUES ('the Quick 棕色 Fox');",
    )
    .unwrap();
    let s: String = conn
        .query_row(
            "SELECT snippet(docs, 0, '[', ']', '...', 8) FROM docs WHERE docs MATCH 'quick'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        s.contains("[Quick]"),
        "expected highlighted original token, got: {s}"
    );

    // And highlight a Chinese word to be sure multibyte offsets are right too.
    let s: String = conn
        .query_row(
            "SELECT snippet(docs, 0, '[', ']', '...', 8) FROM docs WHERE docs MATCH '棕色'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        s.contains("[棕色]"),
        "expected highlighted chinese word, got: {s}"
    );
}

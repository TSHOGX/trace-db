//! The shipped artifact must be loadable and must expose the jieba tokenizer.
//!
//! The tokenizer itself is tested in its own repository; this only proves that the
//! wrapper cdylib re-exports the entry points, since a missing export would build fine
//! and fail at runtime.

use rusqlite::Connection;

fn extension_path() -> std::path::PathBuf {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let exe = std::env::current_exe().unwrap();
    let profile = exe.parent().and_then(std::path::Path::parent).unwrap();
    [profile.to_path_buf(), profile.join("deps")]
        .iter()
        .map(|d| d.join(format!("{prefix}fts5jieba.{ext}")))
        .find(|p| p.exists())
        .expect("built extension not found; run `cargo build -p trace-db-tokenizer` first")
}

fn load(entry: &str) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    unsafe {
        conn.load_extension_enable().unwrap();
        conn.load_extension(extension_path(), Some(entry)).unwrap();
        conn.load_extension_disable().unwrap();
    }
    conn
}

#[test]
fn both_entry_points_load_and_register_the_tokenizer() {
    for entry in ["sqlite3_fts5jieba_init", "sqlite3_extension_init"] {
        let conn = load(entry);
        conn.execute_batch(
            "CREATE VIRTUAL TABLE d USING fts5(b, tokenize='jieba');
             INSERT INTO d VALUES ('running 中华人民共和国');",
        )
        .unwrap();
        let n = |q: &str| -> i64 {
            conn.query_row("SELECT count(*) FROM d WHERE d MATCH ?1", [q], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(n("run"), 1, "{entry}: English stem");
        assert_eq!(n("中华"), 1, "{entry}: Chinese segmentation");
    }
}

#!/usr/bin/env bash
# Cross-host smoke test: load the SAME built extension into every available
# SQLite host and run one mixed Chinese/English MATCH. Proves the artifact is
# language-agnostic. Skips hosts that aren't installed.
#
# Usage: scripts/smoke.sh [path-to-lib-without-extension]
#   defaults to target/release/libfts5jieba

set -uo pipefail
cd "$(dirname "$0")/.."

case "$(uname -s)" in
  Darwin) EXT=dylib ;;
  Linux)  EXT=so ;;
  *)      EXT=dll ;;
esac

LIB="${1:-$PWD/target/release/libfts5jieba}"
if [ ! -f "$LIB.$EXT" ]; then
  echo "extension not found: $LIB.$EXT — run 'cargo build --release' first" >&2
  exit 1
fi

# macOS system SQLite often has .load / extension loading compiled out; prefer
# Homebrew's build when present.
BREW_SQLITE_BIN=/opt/homebrew/opt/sqlite/bin/sqlite3
BREW_SQLITE_LIB=/opt/homebrew/opt/sqlite/lib/libsqlite3.dylib

pass=0
fail=0
check() { # name, actual
  if [ "$2" = "run=1 zh=1" ]; then echo "  ✓ $1"; pass=$((pass+1));
  else echo "  ✗ $1 — got: $2"; fail=$((fail+1)); fi
}

SQL=".load $LIB
CREATE VIRTUAL TABLE d USING fts5(b, tokenize='jieba');
INSERT INTO d VALUES('running 中华人民共和国');
SELECT 'run='||count(*) FROM d WHERE d MATCH 'run';
SELECT 'zh='||count(*) FROM d WHERE d MATCH '中华';"

echo "sqlite3 CLI:"
if [ -x "$BREW_SQLITE_BIN" ]; then
  out=$(printf '%s\n' "$SQL" | "$BREW_SQLITE_BIN" ":memory:" 2>&1 | tr '\n' ' ' | sed 's/ *$//')
  check "homebrew sqlite3" "$out"
else
  echo "  - skipped (no $BREW_SQLITE_BIN; system sqlite3 has .load disabled)"
fi

echo "Python:"
if command -v python3 >/dev/null 2>&1; then
  out=$(python3 - "$LIB.$EXT" <<'PY' 2>&1
import sqlite3, sys
try:
    c = sqlite3.connect(":memory:")
    c.enable_load_extension(True)
    c.load_extension(sys.argv[1])
    c.execute("CREATE VIRTUAL TABLE d USING fts5(b, tokenize='jieba')")
    c.execute("INSERT INTO d VALUES('running 中华人民共和国')")
    r = c.execute("SELECT count(*) FROM d WHERE d MATCH 'run'").fetchone()[0]
    z = c.execute("SELECT count(*) FROM d WHERE d MATCH '中华'").fetchone()[0]
    print(f"run={r} zh={z}")
except Exception as e:
    print("ERR", type(e).__name__, e)
PY
)
  check "python3 sqlite3" "$out"
else
  echo "  - skipped (no python3)"
fi

echo "Bun:"
if command -v bun >/dev/null 2>&1; then
  TS=$(mktemp /tmp/smoke.XXXX.ts)
  cat > "$TS" <<'TSEOF'
import { Database } from "bun:sqlite";
try {
  // On macOS, bun's bundled SQLite disallows extension loading; point at brew's.
  const brew = "/opt/homebrew/opt/sqlite/lib/libsqlite3.dylib";
  try { Database.setCustomSQLite(brew); } catch {}
  const db = new Database(":memory:");
  db.loadExtension(process.argv[2]);
  db.run("CREATE VIRTUAL TABLE d USING fts5(b, tokenize='jieba')");
  db.run("INSERT INTO d VALUES('running 中华人民共和国')");
  const r = db.query("SELECT count(*) c FROM d WHERE d MATCH 'run'").get().c;
  const z = db.query("SELECT count(*) c FROM d WHERE d MATCH '中华'").get().c;
  console.log(`run=${r} zh=${z}`);
} catch (e) { console.log("ERR", String(e)); }
TSEOF
  out=$(bun "$TS" "$LIB.$EXT" 2>&1 | tr '\n' ' ' | sed 's/ *$//')
  rm -f "$TS"
  check "bun:sqlite" "$out"
else
  echo "  - skipped (no bun)"
fi

echo
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]

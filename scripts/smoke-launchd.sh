#!/usr/bin/env bash
set -euo pipefail

trace_db_bin="${1:-target/debug/trace-db}"
[[ "$(uname -s)" == "Darwin" ]] || {
  echo "launchd smoke requires macOS" >&2
  exit 2
}
[[ -x "$trace_db_bin" ]] || {
  echo "TraceDB binary is not executable: $trace_db_bin" >&2
  exit 2
}

workspace="$(mktemp -d "${TMPDIR:-/tmp}/tracedb-launchd-smoke.XXXXXX")"
database="$workspace/trace.db"
native_root="$workspace/native"
mkdir -p "$native_root"
cat > "$native_root/rollout-smoke.jsonl" <<'EOF'
{"type":"session_meta","payload":{"id":"launchd-smoke","cwd":"/tmp/tracedb-launchd-smoke"}}
EOF

cleanup() {
  "$trace_db_bin" --db "$database" daemon uninstall >/dev/null 2>&1 || true
  rm -rf "$workspace"
}
trap cleanup EXIT

"$trace_db_bin" --db "$database" daemon install \
  --interval 60 --agent codex --root "$native_root"
for _ in $(seq 1 30); do
  if "$trace_db_bin" --db "$database" stats --json | grep -q '"totalSessions": 1'; then
    break
  fi
  sleep 1
done
"$trace_db_bin" --db "$database" stats --json | grep -q '"totalSessions": 1'

first_pid="$(launchctl list com.tracedb.watch-daemon | awk '/"PID"/ {gsub(/[^0-9]/,"",$3); print $3}')"
[[ "$first_pid" =~ ^[1-9][0-9]*$ ]]
kill -9 "$first_pid"
second_pid=""
for _ in $(seq 1 30); do
  second_pid="$(launchctl list com.tracedb.watch-daemon 2>/dev/null | awk '/"PID"/ {gsub(/[^0-9]/,"",$3); print $3}')"
  if [[ "$second_pid" =~ ^[1-9][0-9]*$ && "$second_pid" != "$first_pid" ]]; then
    break
  fi
  sleep 1
done
[[ "$second_pid" =~ ^[1-9][0-9]*$ && "$second_pid" != "$first_pid" ]]

"$trace_db_bin" --db "$database" daemon stop
"$trace_db_bin" --db "$database" daemon status | grep -q 'Installed but not running'
"$trace_db_bin" --db "$database" daemon start
"$trace_db_bin" --db "$database" daemon status | grep -q 'Running'
"$trace_db_bin" --db "$database" daemon uninstall
test ! -e "$HOME/Library/LaunchAgents/com.tracedb.watch-daemon.plist"
echo "launchd daemon smoke: ok"

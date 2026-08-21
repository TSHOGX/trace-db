#!/usr/bin/env bash
set -euo pipefail

trace_db_bin="${1:-target/debug/trace-db}"
command -v systemctl >/dev/null 2>&1 || {
  echo "systemctl is required for the Linux daemon smoke" >&2
  exit 2
}
[[ -x "$trace_db_bin" ]] || {
  echo "TraceDB binary is not executable: $trace_db_bin" >&2
  exit 2
}

system_state="$(systemctl --user is-system-running 2>/dev/null || true)"
case "$system_state" in
  running|degraded) ;;
  *)
    echo "systemd user manager is not ready: ${system_state:-unavailable}" >&2
    exit 2
    ;;
esac

workspace="$(mktemp -d "${TMPDIR:-/tmp}/tracedb-systemd-smoke.XXXXXX")"
database="$workspace/trace.db"
native_root="$workspace/native"
mkdir -p "$native_root"
cat > "$native_root/rollout-smoke.jsonl" <<'EOF'
{"type":"session_meta","payload":{"id":"systemd-smoke","cwd":"/tmp/tracedb-systemd-smoke"}}
EOF

cleanup() {
  "$trace_db_bin" --db "$database" daemon uninstall >/dev/null 2>&1 || true
  rm -rf "$workspace"
}
trap cleanup EXIT

"$trace_db_bin" --db "$database" daemon install \
  --interval 60 --agent codex --root "$native_root"
unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/tracedb-watch.service"
test -f "$unit"
systemd-analyze verify "$unit"

for _ in $(seq 1 30); do
  if "$trace_db_bin" --db "$database" stats --json | grep -q '"totalSessions": 1'; then
    break
  fi
  sleep 1
done
"$trace_db_bin" --db "$database" stats --json | grep -q '"totalSessions": 1'

first_pid="$(systemctl --user show tracedb-watch.service --property MainPID --value)"
[[ "$first_pid" =~ ^[1-9][0-9]*$ ]]
systemctl --user kill --kill-whom=main --signal=KILL tracedb-watch.service
second_pid=""
for _ in $(seq 1 30); do
  second_pid="$(systemctl --user show tracedb-watch.service --property MainPID --value)"
  if [[ "$second_pid" =~ ^[1-9][0-9]*$ && "$second_pid" != "$first_pid" ]]; then
    break
  fi
  sleep 1
done
[[ "$second_pid" =~ ^[1-9][0-9]*$ && "$second_pid" != "$first_pid" ]]

cat >> "$native_root/rollout-smoke.jsonl" <<'EOF'
{"type":"response_item","timestamp":"2026-08-21T00:00:00Z","payload":{"type":"message","id":"assistant-smoke","role":"assistant","content":"incremental daemon smoke"}}
EOF
for _ in $(seq 1 30); do
  if "$trace_db_bin" --db "$database" stats --json | grep -q '"totalEvents": 1'; then
    break
  fi
  sleep 1
done
"$trace_db_bin" --db "$database" stats --json | grep -q '"totalEvents": 1'

"$trace_db_bin" --db "$database" daemon stop
"$trace_db_bin" --db "$database" daemon status | grep -q 'Installed but not running'
"$trace_db_bin" --db "$database" daemon start
"$trace_db_bin" --db "$database" daemon status | grep -q 'Running'
"$trace_db_bin" --db "$database" daemon uninstall
test ! -e "$unit"
echo "systemd user daemon smoke: ok"

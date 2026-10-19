#!/usr/bin/env bash
# End-to-end test against real PostgreSQL 18 + pgBackRest containers.
#
# Needs Docker and python3. Creates containers and a volume named pgback-e2e-*, uses
# its own label prefix (so it never touches other containers), and removes everything
# on exit. Run from anywhere: scripts/e2e.sh
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
IMG=pgback-e2e/postgres-pgbackrest:18
PREFIX=e2e.pg.backup
NAMES=(pgback-e2e-db1 pgback-e2e-db2 pgback-e2e-mail)

cleanup() {
  [[ -f $WORK/run/pgback.pid ]] && kill "$(cat "$WORK/run/pgback.pid")" 2>/dev/null || true
  docker rm -fv "${NAMES[@]}" >/dev/null 2>&1 || true
  docker volume rm pgback-e2e-repo >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

pass() { printf '  \033[32mok\033[0m   %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; [[ -f $WORK/last.log ]] && tail -20 "$WORK/last.log"; exit 1; }
# check "description" command... : passes when the command exits 0
check() { local desc=$1; shift; if "$@" >"$WORK/last.log" 2>&1; then pass "$desc"; else fail "$desc"; fi; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); sys.exit(0 if ($1) else 1)"; }

echo "building"
cargo build --quiet --manifest-path "$ROOT/Cargo.toml"
B="$ROOT/target/debug/pgback"
docker build -q -t "$IMG" "$ROOT/examples/docker" >/dev/null

echo "starting PostgreSQL 18 containers"
docker rm -fv "${NAMES[@]}" >/dev/null 2>&1 || true
docker volume rm pgback-e2e-repo >/dev/null 2>&1 || true
docker volume create pgback-e2e-repo >/dev/null
docker run -d --name pgback-e2e-db1 -e POSTGRES_PASSWORD=e2e -l "$PREFIX.enable=true" \
  -v pgback-e2e-repo:/var/lib/pgbackrest "$IMG" -c archive_mode=on \
  -c "archive_command=pgbackrest --config=/etc/pgbackrest/pgbackrest.conf --stanza=pgback-e2e-db1 archive-push %p" >/dev/null
docker run -d --name pgback-e2e-db2 -e POSTGRES_PASSWORD=e2e -e POSTGRES_USER=app \
  -l "$PREFIX.enable=yes" -l "$PREFIX.stanza=orders" -v pgback-e2e-repo:/var/lib/pgbackrest "$IMG" >/dev/null
docker run -d --name pgback-e2e-mail -p 127.0.0.1::1025 -p 127.0.0.1::8025 axllent/mailpit >/dev/null
SMTP_PORT=$(docker port pgback-e2e-mail 1025/tcp | head -1 | cut -d: -f2)
MAIL_API=http://127.0.0.1:$(docker port pgback-e2e-mail 8025/tcp | head -1 | cut -d: -f2)/api/v1
HTTP_PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
for db in pgback-e2e-db1:postgres pgback-e2e-db2:app; do
  for _ in $(seq 60); do docker exec -u postgres "${db%%:*}" pg_isready -q -U "${db##*:}" && break; sleep 1; done
done

cat >"$WORK/pgback.yaml" <<EOF
host_id: e2e-host
docker: { label_prefix: $PREFIX }
pgbackrest:
  manage_archive_command: true
  repos:
    - { type: posix, path: /var/lib/pgbackrest, retention_full: 2, cipher_pass: e2e-cipher }
backup: { schedule: "0 2 * * 0", incremental_backup_interval: 6h, retry_backoff: 1s }
hooks: { dir: $WORK/hooks }
notifications:
  notify_on: [failure]
  email: { method: mail, from: pgback@e2e.test, to: [dba@e2e.test], smtp_host: 127.0.0.1, smtp_port: $SMTP_PORT }
monitoring: { metrics_file: $WORK/state/metrics.json, listen: "127.0.0.1:$HTTP_PORT" }
daemon: { pid_file: $WORK/run/pgback.pid, log_file: $WORK/daemon.log, discovery_interval: 10s, shutdown_timeout: 2m }
EOF
chmod 600 "$WORK/pgback.yaml"
P() { "$B" -c "$WORK/pgback.yaml" "$@"; }

# Assertions (each reads pgback's output; `json` evaluates a Python expression over `d`).
discovers_both() { P discover --format json | json 'len(d)==2 and d[1]["stanza"]=="orders" and d[1]["pg_path"]=="/var/lib/postgresql/18/docker"'; }
sync_pending_restart() { P --config_sync | grep 'restart PostgreSQL to enable archive_mode' >/dev/null; }
sync_unchanged() { [[ $(P --config_sync | grep -c ' unchanged ') == 2 ]]; }
first_run_full() { [[ $(P run | grep -c ' full  success') == 2 ]]; }
second_run_incr() { P run --container orders | grep ' incr  success' >/dev/null; }
forced_diff() { P run --container pgback-e2e-db1 --force_backup_type diff | grep ' diff  success' >/dev/null; }
nothing_due() { P run --if_due | grep 'no target is due' >/dev/null; }
history_counts() { P history --format json | json 'sorted((t["stanza"], len(t["backups"])) for t in d)==[("orders",2),("pgback-e2e-db1",2)]'; }

echo "discovery and config sync"
check "discovers both labelled containers (PG18 PGDATA detected)" discovers_both
check "config sync sets archiving on db2 (restart pending)" sync_pending_restart
docker restart pgback-e2e-db2 >/dev/null
for _ in $(seq 60); do docker exec -u postgres pgback-e2e-db2 pg_isready -q -U app && break; sleep 1; done
check "pgbackrest check passes on both after restart" P check
check "second sync leaves config unchanged" sync_unchanged

echo "backups"
check "first run takes full backups" first_run_full
check "second run takes an incremental" second_run_incr
check "forced diff" forced_diff
check "--if_due skips targets that are not due" nothing_due
check "history reports every backup" history_counts

echo "hooks and alerts"
"$B" -c "$WORK/pgback.yaml" --create_pre_hook 10-gate --create_post_hook 90-record >/dev/null
printf 'echo "maintenance window" >&2; exit 7\n' >>"$WORK/hooks/pre-backup.d/10-gate"
printf 'echo "$PGBACK_STANZA $PGBACK_STATUS" >> %s/post.log\n' "$WORK" >>"$WORK/hooks/post-backup.d/90-record"
set +e; P run --container orders >/dev/null 2>&1; code=$?; set -e
[[ $code == 1 ]] && pass "failing pre-hook fails the run (exit 1)" || fail "failing pre-hook fails the run (exit $code)"
check "post-hook ran with status=failure" grep -qx "orders failure" "$WORK/post.log"
failure_mail() { curl -sf "$MAIL_API/messages" | json 'any("1 of 1 backup failed" in m["Subject"] for m in d["messages"])'; }
check "failure email delivered" failure_mail
rm "$WORK/hooks/pre-backup.d/10-gate"
before=$(md5sum <"$WORK/state/metrics.json")
dry_run_logs() { P --dry_run run 2>&1 | grep 'dry run: would run backup' >/dev/null; }
check "dry run executes nothing" dry_run_logs
[[ $before == "$(md5sum <"$WORK/state/metrics.json")" ]] && pass "dry run records nothing" || fail "dry run records nothing"

echo "daemon"
set +e; P status_all >/dev/null 2>&1; code=$?; set -e
[[ $code == 3 ]] && pass "status_all exits 3 while stopped" || fail "status_all exits 3 while stopped (got $code)"
check "start_all" P start_all --incremental_backup_interval 20s
check "/health answers" curl -sf --retry 10 --retry-connrefused --retry-delay 1 "http://127.0.0.1:$HTTP_PORT/health"
for _ in $(seq 90); do grep -q '"reason":"interval_elapsed"' "$WORK/daemon.log" 2>/dev/null && grep -q '"message":"backup finished"' "$WORK/daemon.log" && break; sleep 1; done
check "daemon schedules an incremental on its own" grep -q '"reason":"interval_elapsed"' "$WORK/daemon.log"
metrics_exported() { curl -sf "http://127.0.0.1:$HTTP_PORT/metrics" | grep '^pgback_last_backup_success{host="e2e-host",target="local/pgback-e2e-db1"' >/dev/null; }
# The failed target retries after its 1s backoff; wait for it to recover.
healthy_soon() { for _ in $(seq 30); do P status_all >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }
check "/metrics exports results" metrics_exported
check "status_all exits 0 once every target is healthy" healthy_soon
check "stop_all stops the daemon" P stop_all
check "pid file released" test ! -e "$WORK/run/pgback.pid"

echo "all end-to-end checks passed"

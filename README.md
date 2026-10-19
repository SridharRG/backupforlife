# pgback

**Fleet orchestration for [pgBackRest] across Dockerised PostgreSQL.** One static binary
per Docker host discovers PostgreSQL containers by label, keeps their pgBackRest
configuration in sync, schedules full and incremental backups, runs lifecycle hooks,
alerts via Slack, Google Chat or email, and exports health and Prometheus metrics.

```
                 ┌──────────────────────── Docker host ────────────────────────┐
                 │                                                             │
 pgback daemon ──┼─▶ /var/run/docker.sock ──▶ containers labelled              │
 (systemd)       │        Docker API              pg.backup.enable=true        │
   │             │                                  │                          │
   │  discover · render pgbackrest.conf · exec      ▼                          │
   │             │                         ┌─ postgres:18 + pgbackrest ─┐      │
   │             │                         │ archive_command → pgBackRest│─────┼──▶ repo: posix / S3 /
   │             │                         │ pgbackrest backup ─────────│─────┼──▶ GCS / Azure / MinIO
   │             │                         └────────────────────────────┘      │   (zstd + AES-256,
   ├─ pre/post hooks (host scripts)                                            │    streamed, no
   ├─ Slack · Google Chat · SES / SMTP                                         │    host disk)
   └─ /health /status /metrics · metrics.json · node_exporter textfile         │
                 └─────────────────────────────────────────────────────────────┘
```

pgBackRest does the data path: it streams straight from `PGDATA` to the repository
(nothing is staged on the host), compresses and encrypts, handles full / differential /
incremental backups, WAL archiving, retention and point-in-time restore. pgback is the
control plane around it. It talks to Docker over its API, so the host needs **no `docker`
CLI, no PostgreSQL client and no runtime libraries**, just the binary.

[pgBackRest]: https://pgbackrest.org

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Container labels](#container-labels)
- [Backup policy](#backup-policy)
- [CLI reference](#cli-reference)
- [Hooks](#hooks)
- [Notifications](#notifications)
- [Running as a service](#running-as-a-service)
- [Monitoring](#monitoring)
- [Scaling across hosts](#scaling-across-hosts)
- [Security notes](#security-notes)
- [Restoring](#restoring)

## Install

```bash
cargo install pgback
```

Or build a fully static Linux binary to copy onto hosts:

```bash
rustup target add x86_64-unknown-linux-musl
# ring needs a C compiler for the target: install musl-tools, or reuse gcc:
CC_x86_64_unknown_linux_musl=gcc cargo build --release --target x86_64-unknown-linux-musl
# target/x86_64-unknown-linux-musl/release/pgback (~7 MB, static-pie; runs even FROM scratch)
```

**Requirements**

- pgBackRest **inside each PostgreSQL image**, because the server calls it from `archive_command`.
  [`examples/docker/Dockerfile`](examples/docker/Dockerfile) adds it to `postgres:18-alpine`.
- Access to the Docker API (root or the `docker` group on the host).

## Quick start

```bash
# 1. Write a commented config (mode 0600), then set your repository and alert channels.
sudo pgback --init_config            # → /etc/pgback/pgback.yaml

# 2. Opt containers in with labels (see examples/docker-compose.yml).
docker compose -f examples/docker-compose.yml up -d --build

# 3. See what pgback found.
sudo pgback discover

# 4. Push pgbackrest.conf into every container and create the stanzas.
sudo pgback --config_sync
sudo pgback check                    # pgBackRest + WAL archiving per container

# 5. Back up now, then hand scheduling to the daemon.
sudo pgback run
sudo pgback install_service --path /etc/systemd/system/pgback.service
sudo systemctl daemon-reload && sudo systemctl enable --now pgback
sudo pgback status_all
```

`discover` on a PG18 host looks like:

```
TARGET         CONTAINER     IMAGE                        STANZA   PGDATA                         FULL SCHEDULE  INCR EVERY  PGBACKREST
local/db_prod  8ffd65444281  local/postgres-pgbackrest:18  db_prod  /var/lib/postgresql/18/docker  0 2 * * 0      6h          2.57.0
local/orders   bb905e6116ef  local/postgres-pgbackrest:18  orders   /var/lib/postgresql/18/docker  0 2 * * 0      6h          2.57.0
```

## Container labels

Only **running** containers with `pg.backup.enable=true` (or `yes`, `1`, `on`) are managed.
The prefix is configurable via `docker.label_prefix`.

| Label | Default | Meaning |
|---|---|---|
| `pg.backup.enable` | — | Opt in. |
| `pg.backup.stanza` | container name | pgBackRest stanza (`[A-Za-z0-9_-]`). |
| `pg.backup.schedule` | `backup.schedule` | Cron (5 fields, local time) for **full** backups. |
| `pg.backup.incremental_interval` | `backup.incremental_backup_interval` | Incremental cadence between fulls (`6h`, `0` = off). |
| `pg.backup.retention` | repo `retention_full` | Full-backup retention: count (`4`) or days (`14d`, `2w`). |
| `pg.backup.pg_path` | `$PGDATA`, then config | Data directory. PG18 images set `/var/lib/postgresql/18/docker`. |
| `pg.backup.pg_port` | `$PGPORT`, then `5432` | |
| `pg.backup.pg_user` | `$POSTGRES_USER`, then `postgres` | Database role pgBackRest connects as. |
| `pg.backup.exec_user` | `pgbackrest.exec_user` (`postgres`) | OS user pgBackRest runs as. |

Unknown `pg.backup.*` labels produce a warning, so a typo like `shedule` is reported rather
than silently ignored. Two containers claiming the same stanza in the same repository are
both refused until one gets its own `pg.backup.stanza`.

`pg.backup.databases` is accepted but has no effect: pgBackRest backs up the whole cluster.
To restore selected databases, use `pgbackrest restore --db-include`.

## Backup policy

There are two knobs per target:

- **`schedule`** (cron) says when *full* backups are owed.
- **`incremental_interval`** says how long after the previous backup an *incremental* is due.

The backup type is chosen in this order:

1. `--force_backup_type` wins if given.
2. Otherwise it is **full** if the stanza has no full backup, or if a schedule slot has passed since the last full. Missed slots are caught up.
3. Otherwise it is **incr**.

Because of this, `pgback run` produces fulls on schedule no matter how often cron calls it.

- The **daemon** starts a backup whenever one is due.
- **`pgback run --if_due`** does the same from cron: `*/15 * * * * pgback run --if_due`.
- **`pgback run`** always backs up now.

After a failure a target backs off before retrying: `retry_backoff`, doubling per failure,
capped at `retry_max_backoff`. `backup.timeout` aborts a runaway backup with
`pgbackrest stop --force` and then re-enables the stanza.

## CLI reference

```
pgback [GLOBAL FLAGS] [ACTION FLAGS] [SUBCOMMAND]
```

Flags use the enterprise `snake_case` spelling; `kebab-case` (`--dry-run`) works too. Every
flag is global, so it can appear before or after the subcommand. Action flags run first and
compose with a subcommand: `pgback --config_sync run`.

| General | |
|---|---|
| `-c, --config <PATH>` | Config file (`/etc/pgback/pgback.yaml`, env `PGBACK_CONFIG`). |
| `--init_config` | Write the commented default config (refuses to overwrite without `--overwrite`). |
| `--config_sync` | Render and push `pgbackrest.conf` (atomic, `exec_user`-owned, mode 0640), create stanzas, report archiving. |
| `--dry_run` | Change nothing: print redacted configs, log commands, skip hooks, don't send alerts or record state. Read-only inspection still runs. |
| `--debug_mode` | Debug logging (`PGBACK_LOG` takes EnvFilter syntax for finer control). |
| `--log_format text\|json` | JSON logs for Sensu, Loki or ELK. |
| `--container <SEL,…>` | Limit to containers by name, `endpoint/name`, stanza or ID prefix. |

| Backup | |
|---|---|
| `--force_backup_type full\|incr\|diff` | Override the schedule-based choice. |
| `--incremental_backup_interval <DUR>` | Override the cadence (`30m`, `6h`, `1d`; `0` disables). |

| Hooks | |
|---|---|
| `--create_pre_hook <NAME>` | Scaffold `hooks/pre-backup.d/<NAME>` (executable, documented env). |
| `--create_post_hook <NAME>` | Scaffold `hooks/post-backup.d/<NAME>`. |

| Notifications | |
|---|---|
| `--slack_notification_url <URL>` | Slack incoming webhook. |
| `--google_chat_notification_url <URL>` | Google Chat space webhook. |
| `--from_email <ADDR>` / `--to_email <ADDR,…>` | Email sender and recipients. |
| `--email_send_method aws\|mail` | `aws` = Amazon SES (SMTP interface); `mail` = any SMTP relay or local MTA. |
| `--email_send_access_key <KEY>` | SES access key ID. |
| `--email_send_secret_key <KEY>` | SES secret key; prefer `PGBACK_EMAIL_SEND_SECRET_KEY`. |

Every flag also reads a `PGBACK_*` environment variable (shown in `--help`). Secrets are
hidden from help output.

| Subcommand | |
|---|---|
| `discover [--format json]` | Opted-in containers, resolved policy and pgBackRest version. |
| `run [--if_due]` | Back up now (or only what is due). |
| `history [--format json]` | `pgbackrest info` per target. |
| `check` | pgBackRest installed, archiving configured, `pgbackrest check` passes. |
| `test_notify` | Send a test message to every channel. |
| `daemon` | Scheduler in the foreground (systemd `ExecStart`). |
| `start_all` / `stop_all [--wait D] [--immediate]` | Background daemon control. |
| `status_all [--format json]` | Daemon state plus last run, last full, last backup and next due per target. |
| `install_service [--path P]` | Print or write a systemd unit. |

**Exit codes:**

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | A backup, sync or check failed, or a Docker endpoint was unreachable |
| 2 | Usage or configuration error |
| 3 | Daemon not running (`status_all`) |

## Hooks

Executables in `hooks.dir/pre-backup.d/` and `post-backup.d/` run **on the host**, once per
target, in lexical order, much like `run-parts`.

- A failing **pre** hook aborts that target's backup and is reported.
- **Post** hooks always run once the pre phase was reached, so cleanup can rely on them.
- Files ending in `.disabled`, `.sample` or `~` are skipped.
- Each hook is killed with its whole process group after `hooks.timeout`.

```bash
pgback --create_pre_hook 10-pause-cron --create_post_hook 90-resume-cron
```

Hooks receive `PGBACK_PHASE`, `PGBACK_HOST_ID`, `PGBACK_TARGET`, `PGBACK_ENDPOINT`,
`PGBACK_CONTAINER_ID`, `PGBACK_CONTAINER_NAME`, `PGBACK_STANZA`, `PGBACK_BACKUP_TYPE` and
`PGBACK_DRY_RUN`. Post hooks additionally get `PGBACK_STATUS` (`success|failure`),
`PGBACK_ERROR`, `PGBACK_DURATION_SECS` and `PGBACK_BACKUP_LABEL`.

## Notifications

`notifications.notify_on: [failure]` is the default; add `success` for every run. One
summary is sent per `run` invocation (and per job in the daemon). It includes each target's
result and, for failures, the last lines of pgBackRest output:

```
🚨 pgback on db-host-01: 1 of 2 backups failed
✅ local/db_prod — full 20261019-020000F in 4m12s, 30.0 GiB
❌ local/orders — incr failed after 3s: pgbackrest backup exited with code 47: … ERROR: [047]: unable to create path …
```

| Channel | Config |
|---|---|
| Slack | `slack.webhook_url` or `--slack_notification_url` |
| Google Chat | `google_chat.webhook_url` or `--google_chat_notification_url` |
| Email (SES) | `email.method: aws`, `access_key` + `secret_key`, `aws_region`. Sent via `email-smtp.<region>.amazonaws.com:587`. The SES SMTP password is derived from the IAM secret key with AWS's documented algorithm, so no AWS SDK is needed. |
| Email (SMTP) | `email.method: mail`, `smtp_host`/`smtp_port`/`smtp_tls`/credentials. Defaults to `localhost:25`, which suits a local MTA. |

Webhook calls retry on 429/5xx. Webhook URLs (which embed tokens) never appear in logs or
errors. TLS uses rustls with bundled Mozilla roots, so alerts work on minimal hosts.

`pgback test_notify` verifies every channel.

## Running as a service

**systemd (recommended):**

```bash
pgback install_service --path /etc/systemd/system/pgback.service
systemctl daemon-reload && systemctl enable --now pgback
systemctl reload pgback     # SIGHUP: reload config, re-discover
```

Secrets referenced as `${VAR}` in the config go in `/etc/pgback/pgback.env`.

**Without systemd:** `pgback start_all` detaches a daemon (JSON logs to `daemon.log_file`),
`pgback stop_all` stops it, and `pgback status_all` reports on it. Overrides given to
`start_all` (e.g. `--incremental_backup_interval`) apply to that daemon only.

The daemon:

- re-discovers containers every `discovery_interval`, picks up new ones automatically and syncs their config;
- backs up anything with no backups at once;
- on `SIGTERM`, stops scheduling and waits up to `shutdown_timeout` for running backups (a second signal exits at once; backups continue inside their containers);
- holds an `flock`ed PID file, so a stale file never looks alive.

## Monitoring

- **`monitoring.metrics_file`** (`/var/log/pgback/metrics.json`): per-target last status, type, label, sizes, durations, failure counts. Written atomically and safe with concurrent writers. `status_all` reads it.
- **`monitoring.prometheus_textfile`**: the same data for node_exporter's textfile collector, refreshed after every run.
- **`monitoring.listen`** (daemon): three endpoints.
  - `GET /health` is liveness (always 200 while the daemon runs).
  - `GET /status` returns 200, or **503 while any target's last backup failed**, which suits Sensu or uptime checks.
  - `GET /metrics` is Prometheus format: `pgback_last_backup_success`, `pgback_last_success_timestamp_seconds`, `pgback_last_backup_duration_seconds`, `pgback_last_backup_size_bytes`, `pgback_consecutive_failures`, `pgback_backups_total`, `pgback_backup_in_progress`, …

Sensu check example:

```yaml
command: check-http.rb -u http://localhost:8000/status    # or: pgback status_all
```

Alert on staleness with Prometheus too, e.g.
`time() - pgback_last_success_timestamp_seconds > 2 * 86400`.

## Scaling across hosts

**Agent per host (recommended).** Install pgback and the systemd unit on every Docker host,
with the same YAML distributed by your config management. Each agent only touches its local
socket. Point all hosts at one bucket and keep them apart with `{host}` in the repository
path:

```yaml
pgbackrest:
  repos:
    - type: s3
      path: /pgbackrest/{host}
      retention_full: 14d
      cipher_pass: ${PGBACK_CIPHER_PASS}
      s3: { bucket: pg-backups, region: eu-west-1, key: ${AWS_ACCESS_KEY_ID}, key_secret: ${AWS_SECRET_ACCESS_KEY} }
      options: { bundle: y, block: y }
```

**Central scheduler.** List remote daemons under `docker.endpoints` (`tcp://host:2376` with
`tls: {ca, cert, key}`) and use `{endpoint}` in the repository path. A remote Docker API is
root on that host, so protect it with mutual TLS.

Concurrency per agent is bounded by `backup.max_parallel`. Failures are isolated per target,
and one unreachable endpoint never blocks the others.

## Security notes

- Keep `pgback.yaml` at mode 0600 (pgback warns otherwise) and inject secrets with `${VAR}`. `--init_config` writes 0600.
- Secrets are redacted from `--dry_run` output and never logged. pgBackRest itself masks them in its command lines.
- `pgbackrest.conf` is written inside each container owned by `exec_user` with mode 0640, through a temp file and rename.
- Access to the Docker socket is root-equivalent; run pgback as root or a member of `docker`, nothing broader.
- Set `cipher_pass` to encrypt repositories with AES-256. Keep a copy of the passphrase outside the fleet, or the backups cannot be restored.

## Restoring

Restores are deliberate, hands-on operations and are done with pgBackRest directly. For
example, to restore into a fresh container from the same image with the repository mounted:

```bash
docker exec -u postgres db_prod pgbackrest --stanza=db_prod info
# stop PostgreSQL, then, in a container with the data volume and repository attached:
pgbackrest --stanza=db_prod --delta restore                 # latest
pgbackrest --stanza=db_prod --type=time --target="2026-10-19 09:30:00+00" restore   # PITR
```

`pgback history` lists the labels available for `--set`.

## Development

```bash
cargo test                   # unit tests: cron, policy, rendering, info parsing, hooks, HTTP, CLI
cargo clippy --all-targets
scripts/e2e.sh               # end to end against real postgres:18-alpine + pgBackRest (needs Docker)
```

`scripts/e2e.sh` builds the image in `examples/docker/`, starts two PostgreSQL 18
containers and a mailpit SMTP sink under a private label prefix, and checks the following:

- discovery and config sync, including `archive_mode` via `ALTER SYSTEM` and the restart it needs;
- `pgbackrest check`;
- full, incremental, forced-diff and `--if_due` runs;
- history;
- hook failure handling, post-hook environment and alert email;
- dry-run side effects;
- the daemon lifecycle: scheduling, `/health`, `/metrics` and `status_all` exit codes.

It removes everything it created on exit. MSRV is Rust 1.88.
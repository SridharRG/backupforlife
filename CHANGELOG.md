# Changelog

## 0.1.0 — unreleased

First release.

- Docker label discovery (`pg.backup.*`) over the Docker API on any number of endpoints
  (unix socket, TCP, mutual TLS), with PGDATA/PGPORT/POSTGRES_USER detection (PostgreSQL 18 layout included).
- `--config_sync`: per-container `pgbackrest.conf` rendering (posix, S3, GCS, Azure repos, retention,
  AES-256 encryption) pushed atomically; stanza creation; optional `archive_command` management.
- Schedule-aware backup type selection (full on cron slots incl. missed ones, incremental in between),
  `--force_backup_type`, `--incremental_backup_interval`, `run --if_due` for cron.
- Pre/post backup hooks with scaffolding (`--create_pre_hook`, `--create_post_hook`).
- Alerts via Slack, Google Chat, Amazon SES (SMTP, password derived from IAM keys) and SMTP relays.
- Scheduler daemon with auto-discovery, auto config sync, retry backoff, graceful shutdown and SIGHUP reload;
  `start_all` / `stop_all` / `status_all`; systemd unit generator.
- `metrics.json`, Prometheus textfile output and `/health`, `/status`, `/metrics` endpoints.

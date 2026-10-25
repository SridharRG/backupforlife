# pgback user guide

This guide takes you from a server with PostgreSQL running in Docker to automatic,
encrypted backups in Amazon S3 with alerts in Google Chat. It also covers checking
backups, day-to-day operation and restores.

Each step builds on the previous one. Follow them in order the first time.

**You'll end up with:**

- pgback installed on your Docker host and running as a systemd service.
- Every PostgreSQL container you label being found and backed up automatically. By default that's a full backup every Sunday at 02:00 and an incremental every 6 hours.
- Backups compressed, encrypted with AES-256 and stored in S3.
- A Google Chat message whenever a backup fails.
- A tested way to restore a database, to the latest state or to any point in time.

---

## Contents

**Part 1: First-time setup**

1. [How it works (two minutes)](#1-how-it-works)
2. [Check the prerequisites](#2-check-the-prerequisites)
3. [Install pgback on the server](#3-install-pgback-on-the-server)
4. [Add pgBackRest to your PostgreSQL image](#4-add-pgbackrest-to-your-postgresql-image)
5. [Mark the databases you want backed up](#5-mark-the-databases-you-want-backed-up)
6. [Create the pgback configuration](#6-create-the-pgback-configuration)
7. [Detect your databases](#7-detect-your-databases)
8. [Send backups to Amazon S3](#8-send-backups-to-amazon-s3)
9. [Push the configuration into your containers](#9-push-the-configuration-into-your-containers)
10. [Take your first backup](#10-take-your-first-backup)
11. [Get alerts in Google Chat](#11-get-alerts-in-google-chat)
12. [Turn on automatic backups](#12-turn-on-automatic-backups)
13. [Monitoring (optional)](#13-monitoring-optional)

**Part 2: Everyday use**

14. [Day-to-day tasks](#14-day-to-day-tasks)

**Part 3: Restoring**

15. [Restore a database](#15-restore-a-database)

**Reference**

16. [Troubleshooting](#16-troubleshooting)
17. [Where things live](#17-where-things-live)
18. [Command cheat sheet](#18-command-cheat-sheet)

---

# Part 1: First-time setup

## 1. How it works

Two programs work together:

- **pgBackRest** does the actual backup. It runs *inside* each PostgreSQL container. It reads
  the database files and streams them, compressed and encrypted, straight to S3. Nothing is
  copied to the server's disk first, so you don't need spare space for backups. It also
  archives PostgreSQL's WAL, the change log that makes point-in-time restores possible.
- **pgback** is the manager. It runs once per server and does five jobs:
  - finds PostgreSQL containers by their Docker labels;
  - writes each container's pgBackRest configuration;
  - decides when to take full or incremental backups;
  - runs your optional hook scripts;
  - sends alerts and reports status.

```
 your server
 ├── pgback (systemd service)
 │     ├─ finds containers labelled pg.backup.enable=true
 │     ├─ writes /etc/pgbackrest/pgbackrest.conf inside each one
 │     ├─ runs "pgbackrest backup" in them on schedule ───────────┐
 │     └─ alerts → Google Chat / Slack / email                    │
 └── PostgreSQL containers (with pgBackRest installed)            │
                                     ── backups + WAL ──▶  S3 bucket (encrypted)
```

**Full** backups copy everything. **Incremental** backups copy only what changed since the
previous backup, so they're small and fast. A restore needs the last full backup plus the
incrementals after it, and pgBackRest handles that for you.

## 2. Check the prerequisites

You need:

| What | Why | Check with |
|---|---|---|
| A Linux server with Docker | pgback talks to Docker's API | `docker version` |
| Root access (`sudo`) | the Docker socket is root-only | `sudo docker ps` |
| PostgreSQL in Docker | e.g. `postgres:18-alpine` or `postgres:18` | `sudo docker ps` |
| An AWS account and an S3 bucket | backup storage (step 8) | — |
| A Google Workspace account | Google Chat webhooks (step 11) | — |

The server itself needs nothing else: no `docker` CLI plugins, no PostgreSQL client and no
Python. pgback is a single program.

> **Note:** these steps use `sudo` for every pgback command, because pgback must reach
> `/var/run/docker.sock`.

## 3. Install pgback on the server

Choose **one** of the options below.

### Option A: copy the ready-made binary (recommended)

Build one static binary, on your laptop or any Linux build machine, and copy it to every
server. It has no dependencies and runs on any x86-64 Linux.

On the **build machine** (needs [Rust](https://rustup.rs) and gcc):

```bash
git clone https://github.com/SridharRG/backupforlife.git && cd backupforlife
rustup target add x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=gcc cargo build --release --target x86_64-unknown-linux-musl
scp target/x86_64-unknown-linux-musl/release/pgback you@db-host-01:/tmp/pgback
```

On the **server**:

```bash
sudo install -m 0755 /tmp/pgback /usr/local/bin/pgback
pgback --version
```

### Option B: install with Cargo on the server

This needs Rust and a C compiler on the server (`sudo apt install build-essential` on Debian/Ubuntu).

```bash
curl https://sh.rustup.rs -sSf | sh       # if Rust isn't installed yet
cargo install pgback
sudo install -m 0755 ~/.cargo/bin/pgback /usr/local/bin/pgback
pgback --version
```

> **Note:** copy the binary to `/usr/local/bin` either way. `sudo` does not search
> `~/.cargo/bin`.

> **ARM servers (aarch64):** use Option B on the server itself, or build Option A with
> `--target aarch64-unknown-linux-musl` (for example with
> [`cross`](https://github.com/cross-rs/cross)).

## 4. Add pgBackRest to your PostgreSQL image

pgBackRest must be installed **inside** each PostgreSQL container, because PostgreSQL calls it
whenever it archives WAL. The official `postgres` images don't include it, so you extend the
image with a short Dockerfile.

**For `postgres:18-alpine`** (the setup pgback is tested against). This is
[`examples/docker/Dockerfile`](examples/docker/Dockerfile):

```dockerfile
FROM postgres:18-alpine
RUN apk add --no-cache pgbackrest \
 && mkdir -p /etc/pgbackrest /var/lib/pgbackrest /var/log/pgbackrest /var/spool/pgbackrest \
 && chown -R postgres:postgres /etc/pgbackrest /var/lib/pgbackrest /var/log/pgbackrest /var/spool/pgbackrest
```

**For the Debian-based `postgres:18`.** Its apt sources already include the PostgreSQL
repository. This variant follows the same pattern but has not been through pgback's test suite:

```dockerfile
FROM postgres:18
RUN apt-get update && apt-get install -y --no-install-recommends pgbackrest \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /etc/pgbackrest /var/lib/pgbackrest /var/log/pgbackrest /var/spool/pgbackrest \
 && chown -R postgres:postgres /etc/pgbackrest /var/lib/pgbackrest /var/log/pgbackrest /var/spool/pgbackrest
```

Put the Dockerfile next to your `docker-compose.yml` (for example in `./pgimage/Dockerfile`)
and have Compose build it. This is shown in the next step.

## 5. Mark the databases you want backed up

pgback only touches containers you opt in with **Docker labels**. Other containers are never
read or changed.

Here is a complete `docker-compose.yml` for one database:

```yaml
services:
  db_prod:
    build: ./pgimage                      # the Dockerfile from step 4
    image: local/postgres-pgbackrest:18
    container_name: db_prod
    restart: unless-stopped
    environment:
      POSTGRES_USER: app
      POSTGRES_PASSWORD: change-me
      POSTGRES_DB: shop
    labels:
      - "pg.backup.enable=true"           # required: opt in
      - "pg.backup.stanza=db_prod"        # name of this database's backups (default: container name)
      # optional per-database overrides:
      # - "pg.backup.schedule=0 1 * * *"         # full backup every day at 01:00
      # - "pg.backup.incremental_interval=2h"    # incremental every 2 hours
      # - "pg.backup.retention=30d"              # keep 30 days of backups
    volumes:
      - pgdata:/var/lib/postgresql        # PostgreSQL 18 keeps its data under here

volumes:
  pgdata:
```

Apply it:

```bash
cd /opt/db                     # wherever your docker-compose.yml lives
sudo docker compose up -d --build
```

> **Note:** Docker can't add labels to a running container. `docker compose up -d` recreates
> the container with the new labels. The data lives in the `pgdata` volume, so nothing is lost.

Using `docker run` instead? Add `--label pg.backup.enable=true --label pg.backup.stanza=db_prod`.

<details>
<summary>All labels</summary>

| Label | Default | Meaning |
|---|---|---|
| `pg.backup.enable` | — | `true` / `yes` / `1` opts the container in |
| `pg.backup.stanza` | container name | Name of the backup set; letters, digits, `-`, `_` |
| `pg.backup.schedule` | `0 2 * * 0` | When **full** backups happen (cron, server local time) |
| `pg.backup.incremental_interval` | `6h` | How long after the last backup an incremental is due; `0` = never |
| `pg.backup.retention` | from config | How long to keep backups: `14d`, `2w` (days) or `4` (number of full backups) |
| `pg.backup.pg_path` | container's `$PGDATA` | Data directory (detected automatically) |
| `pg.backup.pg_port` | `$PGPORT` or `5432` | |
| `pg.backup.pg_user` | `$POSTGRES_USER` or `postgres` | Database user for backups |

</details>

## 6. Create the pgback configuration

Generate the commented default configuration:

```bash
sudo pgback --init_config
```

This writes `/etc/pgback/pgback.yaml`, readable only by root (mode 0600). It holds your S3
keys and alert URLs. Most defaults are fine. You'll change two sections in the next steps:
`pgbackrest.repos` (where backups go) and `notifications` (where alerts go).

Two settings to consider now:

```yaml
# Give this server a stable name. It appears in alerts and in the S3 path.
host_id: db-host-01

pgbackrest:
  # Let pgback configure WAL archiving inside PostgreSQL for you (recommended).
  manage_archive_command: true
```

Open the file with `sudo nano /etc/pgback/pgback.yaml` (or your editor) and set them.

> **Why `manage_archive_command`?** pgBackRest needs PostgreSQL's `archive_mode=on` and an
> `archive_command` that hands WAL to pgBackRest. With this setting, pgback configures both for
> you in step 9. You only restart the container once afterwards.

## 7. Detect your databases

Check what pgback can see:

```bash
sudo pgback discover
```

```
TARGET         CONTAINER     IMAGE                         STANZA   PGDATA                         FULL SCHEDULE  INCR EVERY  PGBACKREST
local/db_prod  8ffd65444281  local/postgres-pgbackrest:18  db_prod  /var/lib/postgresql/18/docker  0 2 * * 0      6h          2.57.0
```

What the columns mean:

- **TARGET** is how pgback names the database: `<docker endpoint>/<container name>`.
- **STANZA** is the backup set name.
- **PGDATA** is the data directory, read from the container automatically.
- **FULL SCHEDULE** and **INCR EVERY** show when backups will run.
- **PGBACKREST** shows the version installed in the container. `MISSING` means step 4 hasn't been applied to that image.

If the list is empty, the container isn't running or doesn't have `pg.backup.enable=true`
(see [Troubleshooting](#16-troubleshooting)).

## 8. Send backups to Amazon S3

### 8.1 Create the bucket

In the AWS console (S3 → Create bucket):

- **Name:** e.g. `acme-pg-backups`. **Region:** close to your servers, e.g. `us-east-1`.
- **Block all public access:** on (the default).
- **Default encryption:** SSE-S3 (the default). pgBackRest encrypts as well; the two layers don't conflict.
- **Lifecycle rules:** don't add any that delete or expire objects. pgBackRest manages retention itself, and deleting its files breaks restores.

### 8.2 Create an IAM user for backups

In IAM → Users → Create user (e.g. `pgback-db-host-01`), attach an inline policy that allows
only this bucket. Replace `acme-pg-backups`:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:ListBucket"],
      "Resource": "arn:aws:s3:::acme-pg-backups"
    },
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
      "Resource": "arn:aws:s3:::acme-pg-backups/pgbackrest/*"
    }
  ]
}
```

Then go to Security credentials → **Create access key** and copy the **Access key ID** and
**Secret access key**.

> **On EC2?** You can use the instance's IAM role instead of keys. Set `key_type: auto` and
> leave out `key` and `key_secret`. Containers can only reach the instance role if the metadata
> hop limit is 2:
> `aws ec2 modify-instance-metadata-options --instance-id i-xxxx --http-put-response-hop-limit 2 --http-endpoint enabled`

### 8.3 Create an encryption passphrase

pgBackRest encrypts every backup with this passphrase before it leaves the server:

```bash
openssl rand -base64 48
```

> **Warning:** store the passphrase in your password manager **now**. Without it, the backups
> cannot be restored by anyone. It also can't be changed later for the same S3 path.

### 8.4 Point pgback at S3

In `/etc/pgback/pgback.yaml`, find the existing `pgbackrest:` section. Keep its other
settings, and replace its `repos:` list (the `type: posix` entry) with:

```yaml
  # inside the existing "pgbackrest:" section — keep the two-space indent
  repos:
    - type: s3
      path: /pgbackrest/{host}          # {host} becomes host_id, so servers can share one bucket
      retention_full: 14d               # keep 14 days of backups (or a count, e.g. 4)
      cipher_pass: "PASTE-THE-PASSPHRASE-FROM-8.3"
      s3:
        bucket: acme-pg-backups
        region: us-east-1
        key: "AKIA................"     # access key ID from 8.2
        key_secret: "PASTE-THE-SECRET-ACCESS-KEY"
      options:
        bundle: y                       # pack small files together: fewer S3 requests
        block: y                        # block-level incrementals: smaller incremental backups
```

Your backups will appear in S3 under `pgbackrest/db-host-01/backup/db_prod/`, with WAL under
`pgbackrest/db-host-01/archive/db_prod/`.

> **Use one repository.** pgback writes backups to the first repository in the list, so keep
> exactly one entry under `repos:`.

<details>
<summary>Prefer keeping secrets out of the YAML?</summary>

Any value can reference an environment variable as `${NAME}`:

```yaml
      cipher_pass: ${PGBACK_CIPHER_PASS}
      s3:
        key: ${AWS_ACCESS_KEY_ID}
        key_secret: ${AWS_SECRET_ACCESS_KEY}
```

Put the values in `/etc/pgback/pgback.env`, which the systemd service in step 12 loads automatically:

```bash
sudo tee /etc/pgback/pgback.env >/dev/null <<'EOF'
PGBACK_CIPHER_PASS=...
AWS_ACCESS_KEY_ID=AKIA...
AWS_SECRET_ACCESS_KEY=...
EOF
sudo chmod 600 /etc/pgback/pgback.env
```

Commands you run by hand then need the same variables. Create a small wrapper and use
`sudo pgbackctl …` wherever this guide says `sudo pgback …`:

```bash
sudo tee /usr/local/sbin/pgbackctl >/dev/null <<'EOF'
#!/bin/sh
set -a; . /etc/pgback/pgback.env; set +a
exec /usr/local/bin/pgback "$@"
EOF
sudo chmod 0750 /usr/local/sbin/pgbackctl
```

</details>

<details>
<summary>MinIO or another S3-compatible service</summary>

```yaml
      s3:
        bucket: pg-backups
        region: us-east-1                # any value MinIO accepts
        endpoint: minio.internal:9000    # host[:port]
        uri_style: path                  # MinIO needs path-style URLs
        verify_tls: false                # only if MinIO uses a self-signed certificate
        key: "..."
        key_secret: "..."
```

pgBackRest always uses HTTPS, so MinIO must serve TLS. Create the bucket beforehand
(`mc mb myminio/pg-backups`). The PostgreSQL containers must be able to reach the endpoint.

</details>

## 9. Push the configuration into your containers

This step does three things for each database:

- writes the pgBackRest configuration into the container (`/etc/pgbackrest/pgbackrest.conf`, readable only by the postgres user);
- creates the backup set (the "stanza") in S3;
- with `manage_archive_command: true`, turns on WAL archiving.

```bash
sudo pgback --config_sync
```

```
TARGET         CONFIG   STANZA  ARCHIVING
local/db_prod  written  ok      restart PostgreSQL to enable archive_mode (set archive_command, set archive_mode=on)
```

Turning on `archive_mode` needs one PostgreSQL restart. Restart the container, then sync again:

```bash
sudo docker restart db_prod
sudo pgback --config_sync
```

```
TARGET         CONFIG     STANZA  ARCHIVING
local/db_prod  unchanged  ok      ok
```

Now let pgBackRest verify the whole chain, from PostgreSQL through WAL archiving to S3:

```bash
sudo pgback check
```

```
TARGET         PGBACKREST  ARCHIVING  CHECK
local/db_prod  2.57.0      ok         ok
```

All `ok` means you're ready to back up. Run `--config_sync` again whenever you change the
repository settings in `pgback.yaml`. Once the daemon is running (step 12), it does this
automatically.

> **Want to see the file before it's written?** `sudo pgback --config_sync --dry_run` prints
> the configuration for each container with passwords masked, and changes nothing.

## 10. Take your first backup

```bash
sudo pgback run
```

```
TARGET         TYPE  STATUS   DURATION  LABEL             SIZE      ERROR
local/db_prod  full  success  3s        20261019-181918F  32.2 MiB
```

The first backup is always **full**. Run it again and you get a small **incremental**:

```
TARGET         TYPE  STATUS   DURATION  LABEL                              SIZE      ERROR
local/db_prod  incr  success  1s        20261019-181918F_20261019-181922I  32.2 MiB
```

See every backup that exists:

```bash
sudo pgback history
```

```
TARGET         LABEL                              TYPE  STARTED           DURATION  SIZE      STORED   NOTE
local/db_prod  20261019-181918F_20261019-181922I  incr  2026-10-19 23:49  1s        32.2 MiB  408 B
local/db_prod  20261019-181918F                   full  2026-10-19 23:49  3s        32.2 MiB  3.8 MiB
```

**SIZE** is the database size. **STORED** is what that backup added to S3 after compression
and encryption, which is why incrementals are tiny.

If you have the AWS CLI, you can confirm the files landed:
`aws s3 ls --recursive s3://acme-pg-backups/pgbackrest/db-host-01/backup/ | head`.

Back up only one database: `sudo pgback run --container db_prod`.

## 11. Get alerts in Google Chat

### 11.1 Create the webhook in Google Chat

1. Open [Google Chat](https://chat.google.com) and go to the space that should receive alerts. Create one (e.g. "DB backups") if needed.
2. Click the **space name** at the top, then choose **Apps & integrations**.
3. Click **Add webhooks**. Enter a name (e.g. `pgback`) and, optionally, an avatar URL. Click **Save**.
4. Next to the new webhook, click **⋮** → **Copy link**. It looks like
   `https://chat.googleapis.com/v1/spaces/AAAA…/messages?key=…&token=…`

> **Note:** webhooks are a Google Workspace feature. If you don't see the option, your Workspace
> admin has to allow incoming webhooks for Chat. Treat the URL like a password: anyone who has
> it can post to the space.

### 11.2 Add it to pgback

The `notifications:` section already exists in `/etc/pgback/pgback.yaml`. Add `google_chat:`
to it, so it reads:

```yaml
notifications:
  notify_on: [failure]              # alert only when something fails; [failure, success] for every run
  google_chat:
    webhook_url: "https://chat.googleapis.com/v1/spaces/AAAA.../messages?key=...&token=..."
```

Keep the quotes; the URL contains `&` and `=`.

### 11.3 Send a test message

```bash
sudo pgback test_notify
```

```
google_chat: sent
```

A message titled "pgback test notification from db-host-01" appears in the space. When a
backup fails, the alert looks like this:

```
🚨 pgback on db-host-01: 1 of 2 backups failed
✅ local/db_prod — incr 20261019-181918F_20261019-181922I in 1s, 32.2 MiB
❌ local/orders — incr failed after 0s: pgbackrest backup exited with code 47: … ERROR: [047]: unable to create path …
local/orders — pgBackRest output
  (the last lines of pgBackRest's log, to help you diagnose it)
```

pgback also alerts when it can't reach Docker, and when a database's backup has failed.
Failed backups are retried after 15 minutes, 30 minutes, 1 hour and so on, up to every 6 hours.

<details>
<summary>Slack and email instead of (or as well as) Google Chat</summary>

```yaml
notifications:
  notify_on: [failure]
  slack:
    webhook_url: "https://hooks.slack.com/services/T000/B000/XXXX"   # Slack app → Incoming Webhooks
  email:
    method: aws                     # Amazon SES
    from: pgback@example.com        # must be a verified SES identity
    to: [dba@example.com, oncall@example.com]
    aws_region: us-east-1
    access_key: "AKIA..."           # IAM user allowed ses:SendRawEmail
    secret_key: "..."
    # method: mail                  # or any SMTP server / local mail relay:
    # smtp_host: smtp.example.com
    # smtp_port: 587
    # smtp_tls: starttls
    # smtp_username: pgback
    # smtp_password: "..."
```

All configured channels receive every alert. `sudo pgback test_notify` tests them all.

</details>

## 12. Turn on automatic backups

Install pgback as a systemd service:

```bash
sudo pgback install_service --path /etc/systemd/system/pgback.service
sudo systemctl daemon-reload
sudo systemctl enable --now pgback
```

Check that it's running and what it plans to do:

```bash
sudo pgback status_all
```

```
daemon: running (pid 41237); http://0.0.0.0:8000/status
metrics: /var/log/pgback/metrics.json (updated 2m ago)

TARGET         STANZA   LAST RUN        LAST FULL  LAST BACKUP  NEXT
local/db_prod  db_prod  success 2m ago  1d3h ago   2m ago       in 5h58m
```

From now on the service handles everything:

- Every minute it looks for new labelled containers. A new database gets its configuration and a first full backup automatically.
- It takes full backups on the schedule (Sunday 02:00 by default) and incrementals every 6 hours in between. If the server was off when a full was due, the full happens at the next opportunity.
- It retries failed backups with increasing delays and alerts you.

Useful service commands:

```bash
sudo journalctl -u pgback -f        # live log (JSON)
sudo systemctl reload pgback        # after editing pgback.yaml: reload without a restart
sudo systemctl stop pgback          # pause scheduling; a running backup is allowed to finish
```

<details>
<summary>No systemd? Use cron or the built-in background mode</summary>

**Cron.** Create `/etc/cron.d/pgback`. `--if_due` backs up only what is due, so running it
every 15 minutes follows the same schedule as the service:

```
*/15 * * * * root /usr/local/bin/pgback run --if_due --log_format json >> /var/log/pgback/cron.log 2>&1
```

**Background mode.** `sudo pgback start_all` starts the scheduler in the background,
`sudo pgback stop_all` stops it, and `sudo pgback status_all` reports on it.

Use only one of: the systemd service, cron, or `start_all`.

</details>

## 13. Monitoring (optional)

While the service runs it answers on port 8000:

| URL | Use |
|---|---|
| `http://server:8000/health` | Is pgback alive? Always 200 while it runs. |
| `http://server:8000/status` | 200 when all backups are fine, **503 when any database's last backup failed**. Point uptime monitors or Sensu here. |
| `http://server:8000/metrics` | Prometheus metrics: last success time, durations, sizes, failure counts. |

A useful Prometheus alert: no successful backup in two days:
`time() - pgback_last_success_timestamp_seconds > 172800`.

The port listens on all interfaces by default. To keep it local, set
`monitoring.listen: 127.0.0.1:8000`; to turn it off, delete the line. Then run
`sudo systemctl restart pgback`.

---

# Part 2: Everyday use

## 14. Day-to-day tasks

**Is everything OK?**

```bash
sudo pgback status_all     # exit code 0 = healthy, 1 = a backup is failing, 3 = service not running
sudo pgback history        # every backup in S3
```

**Take a backup right now** (before a risky migration, for example):

```bash
sudo pgback run --container db_prod                            # incremental (or full if one is due)
sudo pgback run --container db_prod --force_backup_type full   # force a full backup
```

**Add another database.** Give its container the labels from step 5, configure it with
archiving as in step 4 or let pgback do it, then `docker compose up -d`. Within a minute the
service writes its configuration and takes a full backup. If the container did not start with
`archive_mode=on`, the service log says *"restart PostgreSQL to enable archive_mode"*. Restart
the container once. Until then its backups fail and you get an alert; the next retry after the
restart succeeds.

**Change when backups run:**

- For one database, change its labels and run `docker compose up -d`.
- For all databases, edit `backup:` in `pgback.yaml` and run `sudo systemctl reload pgback`.

```yaml
backup:
  schedule: "0 2 * * 0"              # full: Sundays 02:00 (cron: minute hour day month weekday)
  incremental_backup_interval: 6h    # incremental: 6 hours after the previous backup
```

Common schedules: `0 1 * * *` (daily 01:00), `0 3 * * 1-5` (weekdays 03:00), `0 2 1 * *`
(first of the month).

**Change how long backups are kept:** set `retention_full` in `pgback.yaml` (all databases), or
the `pg.backup.retention` label (one database). Then run `sudo pgback --config_sync`.
pgBackRest removes expired backups and their WAL after the next backup.

**Run your own script before or after each backup.** Typical uses are pausing a batch job or
pinging a monitoring URL.

```bash
sudo pgback --create_pre_hook 10-pause-jobs --create_post_hook 90-resume-jobs
sudo nano /etc/pgback/hooks/pre-backup.d/10-pause-jobs
```

The generated scripts list the variables they receive (`PGBACK_TARGET`, `PGBACK_STANZA`,
`PGBACK_BACKUP_TYPE`; post hooks also get `PGBACK_STATUS` and more). If a pre-backup script
exits with an error, that backup is skipped and you're alerted. Post-backup scripts always run.

**Stop backing up a database.** Remove its `pg.backup.enable` label (or set it to `false`) and
run `docker compose up -d`. Its existing backups stay in S3 until they expire or you delete them.

**Preview anything without changing it:** add `--dry_run` to any command.

---

# Part 3: Restoring

## 15. Restore a database

pgBackRest does the restore. pgback keeps it fed with the right configuration. A restore
**replaces** the database's data, so read the whole procedure first.

All examples use these variables. Set them first:

```bash
NAME=db_prod                                         # container name
STANZA=db_prod                                       # from `sudo pgback discover`
IMAGE=$(sudo docker inspect -f '{{.Config.Image}}' $NAME)
```

**Pick a restore point:**

| You want | Restore options |
|---|---|
| The latest data (e.g. the disk was damaged) | *(none)* |
| The data as it was at a moment, e.g. just before a bad `DROP TABLE` | `--type=time "--target=2026-10-19 09:30:00+05:30" --target-action=promote` |
| Exactly one backup, from `pgback history` | `--set=20261019-181918F --type=immediate --target-action=promote` |

### 15.1 Restore on the same server

```bash
# 1. Pause the backup service and stop the database.
sudo systemctl stop pgback
sudo docker stop $NAME

# 2. Copy pgBackRest's config out of the container. It contains the S3 keys and
#    passphrase; /root is readable only by root, and you delete the copy in step 5.
sudo docker cp $NAME:/etc/pgbackrest/pgbackrest.conf /root/pgbackrest-$NAME.conf
sudo chmod 644 /root/pgbackrest-$NAME.conf
```

```bash
# 3a. Restore the LATEST data …
sudo docker run --rm --volumes-from $NAME -u postgres \
  -v /root/pgbackrest-$NAME.conf:/etc/pgbackrest/pgbackrest.conf:ro \
  --entrypoint pgbackrest $IMAGE \
  --stanza=$STANZA --delta --log-level-console=info restore

# 3b. … OR the data as it was at a point in time (just before the mistake).
sudo docker run --rm --volumes-from $NAME -u postgres \
  -v /root/pgbackrest-$NAME.conf:/etc/pgbackrest/pgbackrest.conf:ro \
  --entrypoint pgbackrest $IMAGE \
  --stanza=$STANZA --delta --log-level-console=info \
  --type=time "--target=2026-10-19 09:30:00+05:30" --target-action=promote restore
```

To restore one specific backup instead, use the `--set=… --type=immediate --target-action=promote`
options from the table in place of the `--type=time …` ones.

```bash
# 4. Start the database. PostgreSQL replays WAL from S3 up to your restore point.
sudo docker start $NAME
sudo docker logs --tail 20 $NAME       # look for "archive recovery complete"

# 5. Clean up, take a fresh full backup, resume the service.
sudo shred -u /root/pgbackrest-$NAME.conf
sudo pgback run --container $NAME --force_backup_type full
sudo systemctl start pgback
```

`--delta` makes the restore fast: only files that differ from the backup are rewritten.

> **Note:** if the database reaches S3 through a specific Docker network (for example a
> self-hosted MinIO), add `--network <that network>` to the `docker run` in step 3.

### 15.2 Restore onto a new server (the old one is gone)

1. On the new server, install pgback (step 3) and copy over `/etc/pgback/pgback.yaml`, your
   `docker-compose.yml` and its `pgimage/` directory. Set `host_id` in `pgback.yaml` to the
   **old** server's name, so pgback keeps using the same S3 path.
   **Don't run `docker compose up` yet.** It would start an empty, freshly initialised database.
2. Build the image and create the container (and its empty volume) **without starting it**:

   ```bash
   cd /opt/db && sudo docker compose create --build db_prod
   ```

3. Write a small pgBackRest config for the restore. Use the values from your `pgback.yaml`;
   `repo1-path` is your `path` with `{host}` replaced by the old server's name:

   ```bash
   sudo tee /root/pgbackrest-restore.conf >/dev/null <<'EOF'
   [global]
   repo1-type=s3
   repo1-path=/pgbackrest/db-host-01
   repo1-s3-bucket=acme-pg-backups
   repo1-s3-region=us-east-1
   repo1-s3-endpoint=s3.us-east-1.amazonaws.com
   repo1-s3-key=AKIA................
   repo1-s3-key-secret=YOUR-SECRET-ACCESS-KEY
   repo1-cipher-type=aes-256-cbc
   repo1-cipher-pass=YOUR-PASSPHRASE
   log-level-console=info

   [db_prod]
   pg1-path=/var/lib/postgresql/18/docker
   EOF
   sudo chmod 644 /root/pgbackrest-restore.conf
   ```

4. Restore into the new, empty volume. pgBackRest creates the data directory:

   ```bash
   sudo docker run --rm --volumes-from db_prod -u postgres \
     -v /root/pgbackrest-restore.conf:/etc/pgbackrest/pgbackrest.conf:ro \
     --entrypoint pgbackrest local/postgres-pgbackrest:18 \
     --stanza=db_prod restore
   ```

5. Give the new container the same config, because PostgreSQL fetches WAL from S3 when it
   first starts. Then start it:

   ```bash
   sudo docker cp /root/pgbackrest-restore.conf db_prod:/etc/pgbackrest/pgbackrest.conf
   sudo docker start db_prod
   sudo docker logs --tail 20 db_prod      # "archive recovery complete"
   ```

6. Hand it back to pgback and resume backups:

   ```bash
   sudo shred -u /root/pgbackrest-restore.conf
   sudo pgback --config_sync --container db_prod    # rewrites the config properly (postgres-owned, 0640)
   sudo pgback run --container db_prod --force_backup_type full
   ```

   Then continue with step 12 to turn on the service.

> **Tip:** rehearse this on a spare machine once. A backup you haven't restored isn't a
> backup yet.

---

# Reference

## 16. Troubleshooting

| You see | What it means / what to do |
|---|---|
| `config file /etc/pgback/pgback.yaml not found` | Run `sudo pgback --init_config` (step 6). |
| `environment variable `X` is not set` | The config uses `${X}`. Use the `pgbackctl` wrapper from step 8.4, or put the value in the YAML. |
| `permission denied … docker.sock` | Run pgback with `sudo`. |
| `discover` shows nothing | Container not running, or label missing or misspelled: `sudo docker inspect -f '{{json .Config.Labels}}' db_prod`. |
| `ignoring unknown label pg.backup.shedule` | A label is misspelled; pgback lists the ones it ignored. |
| PGBACKREST column says `MISSING` | The image lacks pgBackRest; do step 4 and recreate the container. |
| ARCHIVING: `archive_mode=off` | Set `manage_archive_command: true` and run `--config_sync`, or add `-c archive_mode=on` to the container. Then restart it. |
| ARCHIVING: `restart PostgreSQL to enable archive_mode` | `sudo docker restart <container>`. |
| `stanza ... is claimed by local/a, local/b` | Two containers share a backup name. Give each a unique `pg.backup.stanza` label. |
| `check` fails with `[082] … was not archived before the timeout` | WAL isn't reaching S3: check archiving status, S3 keys and network access from the container. |
| `[103]`, `403`, `AccessDenied`, `SignatureDoesNotMatch` | Wrong keys, region or IAM policy (step 8.2), or the bucket name doesn't match. |
| `[040] unable to restore … contains files` | Add `--delta` to the restore command. |
| Restore succeeded but the container exits on start | Its `/etc/pgbackrest/pgbackrest.conf` is missing or wrong; see step 15.2, step 5. |
| `google_chat returned HTTP 400/403/404` | The webhook URL is incomplete (copy it again, with quotes in YAML) or was deleted from the space. |
| `pgback daemon is already running` | It's running as a service already; check `systemctl status pgback`. |
| `status_all` exits with 3 | The service isn't running: `sudo systemctl start pgback`. |

More detail: add `--debug_mode` to any command, and see `sudo journalctl -u pgback`.

## 17. Where things live

| Path | What |
|---|---|
| `/usr/local/bin/pgback` | The program |
| `/etc/pgback/pgback.yaml` | Configuration (0600, root only) |
| `/etc/pgback/pgback.env` | Optional secrets for `${VAR}` references |
| `/etc/pgback/hooks/pre-backup.d/`, `post-backup.d/` | Your hook scripts |
| `/etc/systemd/system/pgback.service` | The service |
| `/var/log/pgback/metrics.json` | Latest result for every database |
| `/run/pgback.pid` | Service PID file |
| *inside each container:* `/etc/pgbackrest/pgbackrest.conf` | Written by pgback; don't edit by hand |
| S3: `pgbackrest/<host_id>/backup/<stanza>/` and `…/archive/<stanza>/` | Backups and WAL |

## 18. Command cheat sheet

```bash
sudo pgback --init_config                      # create /etc/pgback/pgback.yaml
sudo pgback discover                           # which databases will be backed up
sudo pgback --config_sync                      # push config into containers, create stanzas
sudo pgback check                              # verify archiving and S3 end to end
sudo pgback run                                # back up everything now
sudo pgback run --container db_prod --force_backup_type full
sudo pgback run --if_due                       # only what's due (for cron)
sudo pgback history                            # list backups
sudo pgback test_notify                        # test Google Chat / Slack / email
sudo pgback status_all                         # service + per-database status
sudo pgback install_service --path /etc/systemd/system/pgback.service
sudo pgback --create_pre_hook 10-name          # scaffold a hook script
sudo pgback <anything> --dry_run               # preview without changing anything
pgback --help                                  # every option
```

For the full option reference, see [README.md](README.md).

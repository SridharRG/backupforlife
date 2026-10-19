//! pgBackRest integration: per-container `pgbackrest.conf` rendering, command lines,
//! and parsing of `pgbackrest info --output=json`.

use std::fmt::{self, Write as _};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::config::{Config, RepoConfig, RepoKind, Retention};
use crate::target::Target;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum BackupType {
    /// Copy the whole cluster
    Full,
    /// Changes since the last backup of any type
    Incr,
    /// Changes since the last full backup
    Diff,
}

impl BackupType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Incr => "incr",
            Self::Diff => "diff",
        }
    }
}

impl fmt::Display for BackupType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `pgbackrest --config=… --stanza=… [extra…] <command>`
pub fn command(cfg: &Config, stanza: &str, op: &str, extra: &[&str]) -> Vec<String> {
    let mut cmd = vec![
        "pgbackrest".to_string(),
        format!("--config={}", cfg.pgbackrest.config_path),
        format!("--stanza={stanza}"),
    ];
    cmd.extend(extra.iter().map(|s| s.to_string()));
    cmd.push(op.to_string());
    cmd
}

/// The archive_command PostgreSQL must use for this target.
pub fn archive_command(cfg: &Config, stanza: &str) -> String {
    format!(
        "pgbackrest --config={} --stanza={stanza} archive-push %p",
        cfg.pgbackrest.config_path
    )
}

/// Renders the complete pgbackrest.conf for one target.
pub fn render_conf(cfg: &Config, target: &Target, host_id: &str) -> Result<String> {
    let pgbr = &cfg.pgbackrest;
    let mut global: Vec<(String, String)> = Vec::new();
    let mut set = |k: &str, v: String| global.push((k.to_string(), v));

    for (i, repo) in pgbr.repos.iter().enumerate() {
        render_repo(i + 1, repo, target, host_id, &mut set);
    }
    set("process-max", pgbr.process_max.to_string());
    set("compress-type", pgbr.compress_type.clone());
    if let Some(level) = pgbr.compress_level {
        set("compress-level", level.to_string());
    }
    set("start-fast", yn(pgbr.start_fast));
    if pgbr.archive_async {
        set("archive-async", "y".into());
        set("spool-path", pgbr.spool_path.clone());
    }
    set("log-level-console", pgbr.log_level_console.clone());
    set("log-level-file", pgbr.log_level_file.clone());
    global.extend(
        pgbr.global_options
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    let p = &target.policy;
    let mut stanza = vec![
        ("pg1-path".to_string(), p.pg_path.clone()),
        ("pg1-port".to_string(), p.pg_port.to_string()),
        ("pg1-user".to_string(), p.pg_user.clone()),
    ];
    if let Some(socket) = &pgbr.pg_socket_path {
        stanza.push(("pg1-socket-path".into(), socket.clone()));
    }
    stanza.extend(
        pgbr.stanza_options
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Managed by pgback {}; changes are overwritten by `pgback --config_sync`.",
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(out, "# target: {}  host: {host_id}", target.key());
    write_section(&mut out, "global", &global)?;
    write_section(&mut out, &p.stanza, &stanza)?;
    Ok(out)
}

fn render_repo(
    idx: usize,
    repo: &RepoConfig,
    target: &Target,
    host_id: &str,
    set: &mut impl FnMut(&str, String),
) {
    let mut opt = |name: &str, value: String| set(&format!("repo{idx}-{name}"), value);
    opt("type", repo.kind.as_str().into());
    let default_path = match repo.kind {
        RepoKind::Posix => "/var/lib/pgbackrest",
        _ => "/pgbackrest",
    };
    let path = repo
        .path
        .as_deref()
        .unwrap_or(default_path)
        .replace("{host}", host_id)
        .replace("{endpoint}", &target.endpoint);
    opt("path", path);

    match repo.kind {
        RepoKind::Posix => {}
        RepoKind::S3 => {
            let s3 = repo.s3.as_ref().expect("validated: s3 section present");
            opt("s3-bucket", s3.bucket.clone());
            opt("s3-region", s3.region.clone());
            opt(
                "s3-endpoint",
                s3.endpoint
                    .clone()
                    .unwrap_or_else(|| format!("s3.{}.amazonaws.com", s3.region)),
            );
            if let Some(key) = &s3.key {
                opt("s3-key", key.expose().into());
            }
            if let Some(secret) = &s3.key_secret {
                opt("s3-key-secret", secret.expose().into());
            }
            if let Some(kt) = &s3.key_type {
                opt("s3-key-type", kt.clone());
            }
            if let Some(style) = &s3.uri_style {
                opt("s3-uri-style", style.clone());
            }
            if let Some(verify) = s3.verify_tls {
                opt("storage-verify-tls", yn(verify));
            }
        }
        RepoKind::Gcs => {
            let gcs = repo.gcs.as_ref().expect("validated: gcs section present");
            opt("gcs-bucket", gcs.bucket.clone());
            if let Some(key) = &gcs.key {
                opt("gcs-key", key.clone());
            }
            if let Some(kt) = &gcs.key_type {
                opt("gcs-key-type", kt.clone());
            }
            if let Some(ep) = &gcs.endpoint {
                opt("gcs-endpoint", ep.clone());
            }
        }
        RepoKind::Azure => {
            let az = repo
                .azure
                .as_ref()
                .expect("validated: azure section present");
            opt("azure-account", az.account.clone());
            opt("azure-container", az.container.clone());
            opt("azure-key", az.key.expose().into());
            if let Some(kt) = &az.key_type {
                opt("azure-key-type", kt.clone());
            }
            if let Some(ep) = &az.endpoint {
                opt("azure-endpoint", ep.clone());
            }
        }
    }

    match target.policy.retention_full.or(repo.retention_full) {
        Some(Retention::Count(n)) => {
            opt("retention-full-type", "count".into());
            opt("retention-full", n.to_string());
        }
        Some(Retention::Days(d)) => {
            opt("retention-full-type", "time".into());
            opt("retention-full", d.to_string());
        }
        None => {}
    }
    if let Some(diff) = repo.retention_diff {
        opt("retention-diff", diff.to_string());
    }
    if let Some(pass) = &repo.cipher_pass {
        opt("cipher-type", "aes-256-cbc".into());
        opt("cipher-pass", pass.expose().into());
    }
    for (k, v) in &repo.options {
        opt(k, v.clone());
    }
}

fn write_section(out: &mut String, name: &str, options: &[(String, String)]) -> Result<()> {
    let _ = write!(out, "\n[{name}]\n");
    for (k, v) in options {
        if v.contains(['\n', '\r']) {
            bail!("pgBackRest option `{k}` contains a line break");
        }
        let _ = writeln!(out, "{k}={v}");
    }
    Ok(())
}

fn yn(b: bool) -> String {
    if b { "y" } else { "n" }.into()
}

/// Masks credential values so a rendered config can be printed or logged.
pub fn redact(conf: &str) -> String {
    const SECRET_SUFFIXES: [&str; 6] = [
        "-key",
        "-key-secret",
        "-pass",
        "-secret",
        "-token",
        "-password",
    ];
    conf.lines()
        .map(|line| match line.split_once('=') {
            Some((k, _))
                if !line.starts_with('#')
                    && SECRET_SUFFIXES.iter().any(|s| k.trim().ends_with(s)) =>
            {
                format!("{k}=********")
            }
            _ => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// One stanza from `pgbackrest info --output=json`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StanzaInfo {
    pub name: String,
    /// 0 ok, 1 missing stanza path, 2 no valid backups, 3 missing stanza data, … 99 other.
    pub status_code: i64,
    pub status_message: String,
    pub backup_in_progress: bool,
    /// Oldest first.
    pub backups: Vec<BackupRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackupRecord {
    pub label: String,
    #[serde(rename = "type")]
    pub kind: BackupType,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub size_bytes: u64,
    pub repo_size_bytes: u64,
    /// pgBackRest found page checksum errors in this backup.
    pub error: bool,
}

/// What scheduling needs to know about a stanza's history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct History {
    pub last_full_start: Option<DateTime<Utc>>,
    pub last_backup_start: Option<DateTime<Utc>>,
}

impl StanzaInfo {
    pub const STATUS_MISSING_STANZA_PATH: i64 = 1;
    pub const STATUS_MISSING_STANZA_DATA: i64 = 3;

    pub fn missing_stanza(&self) -> bool {
        matches!(
            self.status_code,
            Self::STATUS_MISSING_STANZA_PATH | Self::STATUS_MISSING_STANZA_DATA
        )
    }

    pub fn history(&self) -> History {
        History {
            last_full_start: self
                .backups
                .iter()
                .filter(|b| b.kind == BackupType::Full)
                .map(|b| b.started_at)
                .max(),
            last_backup_start: self.backups.iter().map(|b| b.started_at).max(),
        }
    }

    pub fn latest(&self) -> Option<&BackupRecord> {
        self.backups.iter().max_by_key(|b| b.started_at)
    }
}

#[derive(Deserialize)]
struct RawStanza {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: RawStatus,
    #[serde(default)]
    backup: Vec<RawBackup>,
}

#[derive(Deserialize, Default)]
struct RawStatus {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    lock: Option<RawLock>,
}

#[derive(Deserialize)]
struct RawLock {
    #[serde(default)]
    backup: Option<RawHeld>,
}

#[derive(Deserialize)]
struct RawHeld {
    #[serde(default)]
    held: bool,
}

#[derive(Deserialize)]
struct RawBackup {
    label: String,
    #[serde(rename = "type")]
    kind: BackupType,
    timestamp: RawTimestamps,
    #[serde(default)]
    info: RawSizes,
    #[serde(default)]
    error: Option<bool>,
}

#[derive(Deserialize)]
struct RawTimestamps {
    start: i64,
    stop: i64,
}

#[derive(Deserialize, Default)]
struct RawSizes {
    #[serde(default)]
    size: u64,
    #[serde(default)]
    repository: RawRepoSize,
}

#[derive(Deserialize, Default)]
struct RawRepoSize {
    #[serde(default)]
    size: u64,
}

pub fn parse_info(json: &str) -> Result<Vec<StanzaInfo>> {
    let raw: Vec<RawStanza> = serde_json::from_str(json)?;
    let epoch = |secs: i64| DateTime::from_timestamp(secs, 0).unwrap_or_default();
    Ok(raw
        .into_iter()
        .map(|s| {
            let mut backups: Vec<BackupRecord> = s
                .backup
                .into_iter()
                .map(|b| BackupRecord {
                    label: b.label,
                    kind: b.kind,
                    started_at: epoch(b.timestamp.start),
                    finished_at: epoch(b.timestamp.stop),
                    size_bytes: b.info.size,
                    repo_size_bytes: b.info.repository.size,
                    error: b.error.unwrap_or(false),
                })
                .collect();
            backups.sort_by_key(|b| b.started_at);
            StanzaInfo {
                name: s.name,
                status_code: s.status.code,
                status_message: s.status.message,
                backup_in_progress: s.status.lock.and_then(|l| l.backup).is_some_and(|b| b.held),
                backups,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::target::tests::sample_target;

    const INFO: &str = r#"[{"archive":[{"database":{"id":1,"repo-key":1},"id":"18-1","max":"000000010000000000000006","min":"000000010000000000000001"}],
      "backup":[
        {"archive":{"start":"000000010000000000000004","stop":"000000010000000000000004"},"backrest":{"format":5,"version":"2.57.0"},
         "database":{"id":1,"repo-key":1},"error":false,
         "info":{"delta":8192,"repository":{"delta":1024,"size":1024},"size":8192},
         "label":"20261019-020000F_20261019-080000I","lsn":{"start":"0/4000028","stop":"0/4000120"},
         "prior":"20261019-020000F","reference":["20261019-020000F"],"timestamp":{"start":1760853600,"stop":1760853610},"type":"incr"},
        {"archive":{"start":"000000010000000000000002","stop":"000000010000000000000002"},"backrest":{"format":5,"version":"2.57.0"},
         "database":{"id":1,"repo-key":1},"error":false,
         "info":{"delta":31457280,"repository":{"delta":4194304,"size":4194304},"size":31457280},
         "label":"20261019-020000F","lsn":{"start":"0/2000028","stop":"0/2000100"},
         "prior":null,"reference":null,"timestamp":{"start":1760832000,"stop":1760832042},"type":"full"}],
      "cipher":"none","db":[{"id":1,"repo-key":1,"system-id":7562000000000000000,"version":"18"}],
      "name":"db_prod","repo":[{"cipher":"none","key":1,"status":{"code":0,"message":"ok"}}],
      "status":{"code":0,"lock":{"backup":{"held":false},"restore":{"held":false}},"message":"ok"}}]"#;

    #[test]
    fn parses_info_json() {
        let stanzas = parse_info(INFO).unwrap();
        let s = &stanzas[0];
        assert_eq!(s.name, "db_prod");
        assert_eq!(s.status_code, 0);
        assert!(!s.backup_in_progress);
        assert_eq!(s.backups.len(), 2);
        assert_eq!(s.backups[0].kind, BackupType::Full, "sorted oldest first");
        assert_eq!(s.backups[0].size_bytes, 31_457_280);
        assert_eq!(s.backups[0].repo_size_bytes, 4_194_304);
        let h = s.history();
        assert_eq!(h.last_full_start.unwrap().timestamp(), 1_760_832_000);
        assert_eq!(h.last_backup_start.unwrap().timestamp(), 1_760_853_600);
        assert_eq!(
            s.latest().unwrap().label,
            "20261019-020000F_20261019-080000I"
        );
    }

    #[test]
    fn parses_missing_stanza() {
        let json = r#"[{"backup":[],"db":[],"name":"new","repo":[{"cipher":"none","key":1,"status":{"code":1,"message":"missing stanza path"}}],"status":{"code":1,"lock":{"backup":{"held":false}},"message":"missing stanza path"}}]"#;
        let s = &parse_info(json).unwrap()[0];
        assert!(s.missing_stanza());
        assert_eq!(s.history(), History::default());
    }

    #[test]
    fn renders_conf_with_s3_retention_and_cipher() {
        let yaml = r#"
pgbackrest:
  repos:
    - type: s3
      path: /pgbackrest/{host}
      retention_full: 14d
      cipher_pass: s3cr3t-pass
      s3: { bucket: backups, region: eu-west-1, key: AKIAEXAMPLE, key_secret: wJalr, uri_style: path }
      options: { bundle: true }
  global_options: { archive-timeout: 120 }
"#;
        let cfg = Config::from_yaml(yaml, &|_| None).unwrap();
        cfg.validate().unwrap();
        let target = sample_target(&cfg, "db_prod", &[]);
        let conf = render_conf(&cfg, &target, "host-1").unwrap();
        for line in [
            "[global]",
            "repo1-type=s3",
            "repo1-path=/pgbackrest/host-1",
            "repo1-s3-bucket=backups",
            "repo1-s3-endpoint=s3.eu-west-1.amazonaws.com",
            "repo1-s3-key=AKIAEXAMPLE",
            "repo1-s3-key-secret=wJalr",
            "repo1-s3-uri-style=path",
            "repo1-retention-full-type=time",
            "repo1-retention-full=14",
            "repo1-cipher-type=aes-256-cbc",
            "repo1-cipher-pass=s3cr3t-pass",
            "repo1-bundle=y",
            "compress-type=zst",
            "start-fast=y",
            "archive-timeout=120",
            "[db_prod]",
            "pg1-path=/var/lib/postgresql/18/docker",
            "pg1-user=app",
        ] {
            assert!(
                conf.lines().any(|l| l == line),
                "missing `{line}` in:\n{conf}"
            );
        }

        let redacted = redact(&conf);
        for secret in ["AKIAEXAMPLE", "wJalr", "s3cr3t-pass"] {
            assert!(!redacted.contains(secret), "{secret} leaked:\n{redacted}");
        }
        assert!(redacted.contains("repo1-s3-bucket=backups"));
    }

    #[test]
    fn label_retention_overrides_repo_retention() {
        let cfg = Config::default();
        let target = sample_target(&cfg, "db", &[("pg.backup.retention", "4")]);
        let conf = render_conf(&cfg, &target, "h").unwrap();
        assert!(
            conf.contains("repo1-retention-full-type=count\nrepo1-retention-full=4\n"),
            "{conf}"
        );
    }

    #[test]
    fn rejects_line_breaks() {
        let mut cfg = Config::default();
        cfg.pgbackrest
            .global_options
            .insert("log-path".into(), "/x\n[evil]".into());
        let target = sample_target(&cfg, "db", &[]);
        assert!(render_conf(&cfg, &target, "h").is_err());
    }

    #[test]
    fn builds_commands() {
        let cfg = Config::default();
        assert_eq!(
            command(&cfg, "db", "backup", &["--type=full"]),
            [
                "pgbackrest",
                "--config=/etc/pgbackrest/pgbackrest.conf",
                "--stanza=db",
                "--type=full",
                "backup"
            ]
        );
        assert_eq!(
            archive_command(&cfg, "db"),
            "pgbackrest --config=/etc/pgbackrest/pgbackrest.conf --stanza=db archive-push %p"
        );
    }
}

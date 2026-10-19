//! YAML configuration: schema, defaults, `${VAR}` expansion and validation.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Deserializer};

use crate::cron::Schedule;
use crate::duration::HumanDuration;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/pgback/pgback.yaml";
pub const DEFAULT_TEMPLATE: &str = include_str!("default_config.yaml");

/// A string that never appears in `Debug` output or logs.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Identifies this host in alerts, metrics and `{host}` repo paths. Defaults to the hostname.
    pub host_id: Option<String>,
    pub docker: DockerConfig,
    pub pgbackrest: PgBackRestConfig,
    pub backup: BackupConfig,
    pub hooks: HooksConfig,
    pub notifications: NotificationsConfig,
    pub monitoring: MonitoringConfig,
    pub daemon: DaemonConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DockerConfig {
    pub endpoints: Vec<EndpointConfig>,
    pub label_prefix: String,
    pub timeout_secs: u64,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            endpoints: vec![EndpointConfig {
                name: "local".into(),
                url: "unix:///var/run/docker.sock".into(),
                tls: None,
            }],
            label_prefix: "pg.backup".into(),
            timeout_secs: 120,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub ca: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PgBackRestConfig {
    /// Path of pgbackrest.conf *inside* each container.
    pub config_path: String,
    /// OS user that runs pgbackrest inside the container.
    pub exec_user: String,
    /// Fallbacks for containers that do not set PGDATA / PGPORT / POSTGRES_USER.
    pub pg_path: String,
    pub pg_port: u16,
    pub pg_user: Option<String>,
    pub pg_socket_path: Option<String>,
    pub process_max: u32,
    pub compress_type: String,
    pub compress_level: Option<u32>,
    pub start_fast: bool,
    pub archive_async: bool,
    pub spool_path: String,
    pub log_level_console: String,
    pub log_level_file: String,
    /// Run `stanza-create` automatically when a backup finds no stanza.
    pub auto_stanza_create: bool,
    /// Point archive_command at pgBackRest via ALTER SYSTEM during config sync.
    pub manage_archive_command: bool,
    pub repos: Vec<RepoConfig>,
    #[serde(deserialize_with = "scalar_map")]
    pub global_options: BTreeMap<String, String>,
    #[serde(deserialize_with = "scalar_map")]
    pub stanza_options: BTreeMap<String, String>,
}

impl Default for PgBackRestConfig {
    fn default() -> Self {
        Self {
            config_path: "/etc/pgbackrest/pgbackrest.conf".into(),
            exec_user: "postgres".into(),
            pg_path: "/var/lib/postgresql/18/docker".into(),
            pg_port: 5432,
            pg_user: None,
            pg_socket_path: None,
            process_max: 2,
            compress_type: "zst".into(),
            compress_level: Some(3),
            start_fast: true,
            archive_async: false,
            spool_path: "/var/spool/pgbackrest".into(),
            log_level_console: "info".into(),
            log_level_file: "off".into(),
            auto_stanza_create: true,
            manage_archive_command: false,
            repos: vec![RepoConfig::posix("/var/lib/pgbackrest")],
            global_options: BTreeMap::new(),
            stanza_options: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoKind {
    Posix,
    S3,
    Gcs,
    Azure,
}

impl RepoKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Posix => "posix",
            Self::S3 => "s3",
            Self::Gcs => "gcs",
            Self::Azure => "azure",
        }
    }
}

/// One pgBackRest repository (`repoN-*` options). Up to 256 may be configured.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    #[serde(rename = "type")]
    pub kind: RepoKind,
    /// Repository path; `{host}` and `{endpoint}` are substituted per target.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub retention_full: Option<Retention>,
    #[serde(default)]
    pub retention_diff: Option<u32>,
    /// Enables `aes-256-cbc` repository encryption.
    #[serde(default)]
    pub cipher_pass: Option<Secret>,
    #[serde(default)]
    pub s3: Option<S3Config>,
    #[serde(default)]
    pub gcs: Option<GcsConfig>,
    #[serde(default)]
    pub azure: Option<AzureConfig>,
    /// Extra `repoN-<key>` options, e.g. `bundle: y`.
    #[serde(default, deserialize_with = "scalar_map")]
    pub options: BTreeMap<String, String>,
}

impl RepoConfig {
    fn posix(path: &str) -> Self {
        Self {
            kind: RepoKind::Posix,
            path: Some(path.into()),
            retention_full: Some(Retention::Count(2)),
            retention_diff: None,
            cipher_pass: None,
            s3: None,
            gcs: None,
            azure: None,
            options: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    /// Defaults to `s3.<region>.amazonaws.com`.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub key: Option<Secret>,
    #[serde(default)]
    pub key_secret: Option<Secret>,
    /// `shared` (default), `auto` (instance profile) or `web-id`.
    #[serde(default)]
    pub key_type: Option<String>,
    /// `host` (default) or `path` — MinIO usually needs `path`.
    #[serde(default)]
    pub uri_style: Option<String>,
    #[serde(default)]
    pub verify_tls: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcsConfig {
    pub bucket: String,
    /// Path to the service-account key file inside the container.
    #[serde(default)]
    pub key: Option<String>,
    /// `service` (default), `token` or `auto`.
    #[serde(default)]
    pub key_type: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AzureConfig {
    pub account: String,
    pub container: String,
    pub key: Secret,
    /// `shared` (default) or `sas`.
    #[serde(default)]
    pub key_type: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// Full-backup retention: a backup count (`4`) or whole days (`7d`, `2w`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    Count(u32),
    Days(u32),
}

impl FromStr for Retention {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let invalid = || {
            format!(
                "invalid retention `{s}`: use a backup count (e.g. 4) or whole days (e.g. 7d, 2w)"
            )
        };
        if let Ok(n) = s.parse::<u32>() {
            return if n == 0 {
                Err(invalid())
            } else {
                Ok(Self::Count(n))
            };
        }
        let (number, factor) = match s.char_indices().last() {
            Some((i, 'd')) => (&s[..i], 1),
            Some((i, 'w')) => (&s[..i], 7),
            _ => return Err(invalid()),
        };
        match number
            .parse::<u32>()
            .ok()
            .and_then(|n| n.checked_mul(factor))
        {
            Some(days) if days > 0 => Ok(Self::Days(days)),
            _ => Err(invalid()),
        }
    }
}

impl fmt::Display for Retention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count(n) => write!(f, "{n}"),
            Self::Days(d) => write!(f, "{d}d"),
        }
    }
}

impl<'de> Deserialize<'de> for Retention {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Scalar::deserialize(deserializer)? {
            Scalar::Int(n) => u32::try_from(n)
                .ok()
                .filter(|n| *n > 0)
                .map(Self::Count)
                .ok_or_else(|| {
                    serde::de::Error::custom("retention count must be a positive integer")
                }),
            Scalar::Str(s) => s.parse().map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "retention must be a count or a number of days",
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupConfig {
    /// Cron schedule for full backups (local time). Labels may override it.
    pub schedule: Schedule,
    /// Incremental cadence between fulls for the daemon and `run --if_due`. `0` disables.
    pub incremental_backup_interval: HumanDuration,
    pub max_parallel: usize,
    /// Abort a backup that runs longer than this. `0` disables.
    pub timeout: HumanDuration,
    pub retry_backoff: HumanDuration,
    pub retry_max_backoff: HumanDuration,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            schedule: "0 2 * * 0".parse().expect("valid default schedule"),
            incremental_backup_interval: HumanDuration::from_secs(6 * 3600),
            max_parallel: 2,
            timeout: HumanDuration::from_secs(0),
            retry_backoff: HumanDuration::from_secs(15 * 60),
            retry_max_backoff: HumanDuration::from_secs(6 * 3600),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    pub dir: PathBuf,
    pub timeout: HumanDuration,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            dir: "/etc/pgback/hooks".into(),
            timeout: HumanDuration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyOn {
    Success,
    Failure,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    pub notify_on: Vec<NotifyOn>,
    pub slack: Option<WebhookConfig>,
    pub google_chat: Option<WebhookConfig>,
    pub email: Option<EmailConfig>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            notify_on: vec![NotifyOn::Failure],
            slack: None,
            google_chat: None,
            email: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    pub webhook_url: Secret,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum EmailMethod {
    /// Amazon SES over its SMTP interface (STARTTLS, port 587)
    Aws,
    /// Any SMTP relay or local MTA (defaults to localhost:25)
    #[default]
    Mail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpTls {
    None,
    Opportunistic,
    Starttls,
    Tls,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmailConfig {
    pub method: EmailMethod,
    pub from: String,
    pub to: Vec<String>,
    pub subject_prefix: Option<String>,
    /// SES region for `method: aws` (default `us-east-1`).
    pub aws_region: Option<String>,
    /// IAM access key ID; doubles as the SES SMTP username.
    pub access_key: Option<Secret>,
    /// IAM secret key; the SES SMTP password is derived from it.
    pub secret_key: Option<Secret>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_tls: Option<SmtpTls>,
    pub smtp_username: Option<String>,
    /// Used verbatim instead of deriving one (also for SES SMTP credentials).
    pub smtp_password: Option<Secret>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitoringConfig {
    pub metrics_file: PathBuf,
    /// node_exporter textfile-collector output, refreshed after every run.
    pub prometheus_textfile: Option<PathBuf>,
    /// Daemon HTTP endpoint for /health, /status and /metrics.
    pub listen: Option<String>,
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            metrics_file: "/var/log/pgback/metrics.json".into(),
            prometheus_textfile: None,
            listen: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub pid_file: PathBuf,
    /// stdout/stderr of a daemon launched by `start_all`.
    pub log_file: PathBuf,
    pub discovery_interval: HumanDuration,
    /// Push pgbackrest.conf to new or changed containers automatically.
    pub auto_config_sync: bool,
    /// How long a stopping daemon waits for running backups.
    pub shutdown_timeout: HumanDuration,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            pid_file: "/run/pgback.pid".into(),
            log_file: "/var/log/pgback/daemon.log".into(),
            discovery_interval: HumanDuration::from_secs(60),
            auto_config_sync: true,
            shutdown_timeout: HumanDuration::from_secs(300),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "config file {} not found; create one with `pgback --init_config --config {}`",
                path.display(),
                path.display()
            ),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        warn_if_exposed(path);
        let cfg = Self::from_yaml(&raw, &|name| std::env::var(name).ok())
            .with_context(|| format!("invalid config {}", path.display()))?;
        Ok(cfg)
    }

    pub fn from_yaml(raw: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let mut value: serde_yaml_ng::Value = serde_yaml_ng::from_str(raw)?;
        if value.is_null() {
            value = serde_yaml_ng::Value::Mapping(Default::default());
        }
        expand_env(&mut value, env).map_err(anyhow::Error::msg)?;
        Ok(serde_yaml_ng::from_value(value)?)
    }

    pub fn host_id(&self) -> String {
        self.host_id
            .clone()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(system_hostname)
    }

    /// Checks everything that serde cannot, reporting all problems at once.
    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        let mut problem = |msg: String| problems.push(msg);

        // Docker
        if self.docker.endpoints.is_empty() {
            problem("docker.endpoints must list at least one endpoint".into());
        }
        let mut names = HashSet::new();
        for ep in &self.docker.endpoints {
            if !is_simple_name(&ep.name) {
                problem(format!(
                    "docker endpoint name `{}` may only contain letters, digits, `_`, `-` and `.`",
                    ep.name
                ));
            }
            if !names.insert(ep.name.as_str()) {
                problem(format!("docker endpoint name `{}` is used twice", ep.name));
            }
            let scheme_ok = ["unix://", "tcp://", "http://", "https://"]
                .iter()
                .any(|s| ep.url.starts_with(s));
            if !scheme_ok {
                problem(format!(
                    "docker endpoint `{}`: url `{}` must start with unix://, tcp://, http:// or https://",
                    ep.name, ep.url
                ));
            }
            if ep.tls.is_some() && ep.url.starts_with("unix://") {
                problem(format!(
                    "docker endpoint `{}`: tls is only valid for tcp:// or https:// urls",
                    ep.name
                ));
            }
        }
        let prefix = &self.docker.label_prefix;
        if prefix.is_empty() || prefix.contains(['=', ' ', ',']) {
            problem(format!(
                "docker.label_prefix `{prefix}` must be non-empty and contain no `=`, `,` or spaces"
            ));
        }

        // pgBackRest
        let pgbr = &self.pgbackrest;
        if pgbr.exec_user.is_empty() {
            problem("pgbackrest.exec_user must not be empty".into());
        }
        if !pgbr.config_path.starts_with('/') || pgbr.config_path.ends_with('/') {
            problem("pgbackrest.config_path must be an absolute file path".into());
        }
        if !["none", "bz2", "gz", "lz4", "zst"].contains(&pgbr.compress_type.as_str()) {
            problem(format!(
                "pgbackrest.compress_type `{}` must be one of none, bz2, gz, lz4, zst",
                pgbr.compress_type
            ));
        }
        if pgbr.process_max == 0 {
            problem("pgbackrest.process_max must be at least 1".into());
        }
        if pgbr.repos.is_empty() || pgbr.repos.len() > 256 {
            problem("pgbackrest.repos must list between 1 and 256 repositories".into());
        }
        for (i, repo) in pgbr.repos.iter().enumerate() {
            validate_repo(i + 1, repo, &mut problem);
        }
        for (section, map) in [
            ("global_options", &pgbr.global_options),
            ("stanza_options", &pgbr.stanza_options),
        ] {
            for key in map.keys() {
                if !is_option_key(key) {
                    problem(format!(
                        "pgbackrest.{section}: `{key}` is not a valid pgBackRest option name"
                    ));
                }
            }
        }

        // Backup policy
        if self
            .backup
            .schedule
            .next_after(&chrono::Utc::now())
            .is_none()
        {
            problem(format!(
                "backup.schedule `{}` never fires",
                self.backup.schedule
            ));
        }
        if self.backup.max_parallel == 0 {
            problem("backup.max_parallel must be at least 1".into());
        }

        // Notifications
        for (name, hook) in [
            ("slack", &self.notifications.slack),
            ("google_chat", &self.notifications.google_chat),
        ] {
            if let Some(hook) = hook
                && !hook.webhook_url.expose().starts_with("https://")
            {
                problem(format!(
                    "notifications.{name}.webhook_url must be an https:// URL"
                ));
            }
        }
        if let Some(email) = &self.notifications.email {
            if email.from.is_empty() {
                problem("notifications.email.from is required (or pass --from_email)".into());
            }
            if email.to.is_empty() {
                problem(
                    "notifications.email.to needs at least one recipient (or pass --to_email)"
                        .into(),
                );
            }
            if email.method == EmailMethod::Aws {
                if email.access_key.as_ref().is_none_or(Secret::is_empty) {
                    problem("notifications.email.access_key is required for method aws (or pass --email_send_access_key)".into());
                }
                let has_password = [&email.secret_key, &email.smtp_password]
                    .iter()
                    .any(|s| s.as_ref().is_some_and(|s| !s.is_empty()));
                if !has_password {
                    problem("notifications.email: method aws needs secret_key (or smtp_password); pass --email_send_secret_key or PGBACK_EMAIL_SEND_SECRET_KEY".into());
                }
            }
        }

        // Monitoring
        if let Some(listen) = &self.monitoring.listen
            && listen.parse::<SocketAddr>().is_err()
        {
            problem(format!(
                "monitoring.listen `{listen}` must be an address like 0.0.0.0:8000"
            ));
        }

        if problems.is_empty() {
            Ok(())
        } else {
            bail!("invalid configuration:\n  - {}", problems.join("\n  - "))
        }
    }
}

fn validate_repo(idx: usize, repo: &RepoConfig, problem: &mut impl FnMut(String)) {
    let at = format!("pgbackrest.repos[{}] ({})", idx - 1, repo.kind.as_str());
    if let Some(path) = &repo.path {
        if !path.starts_with('/') {
            problem(format!("{at}: path must be absolute"));
        }
        let mut rest = path.as_str();
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                problem(format!("{at}: unterminated `{{` in path"));
                break;
            };
            let name = &rest[open + 1..open + close];
            if !["host", "endpoint"].contains(&name) {
                problem(format!(
                    "{at}: unknown placeholder `{{{name}}}` in path (use {{host}} or {{endpoint}})"
                ));
            }
            rest = &rest[open + close + 1..];
        }
    }
    let sections = [
        (RepoKind::S3, repo.s3.is_some()),
        (RepoKind::Gcs, repo.gcs.is_some()),
        (RepoKind::Azure, repo.azure.is_some()),
    ];
    for (kind, present) in sections {
        if present && kind != repo.kind {
            problem(format!(
                "{at}: has a `{}` section but type is `{}`",
                kind.as_str(),
                repo.kind.as_str()
            ));
        }
    }
    match repo.kind {
        RepoKind::Posix => {}
        RepoKind::S3 => match &repo.s3 {
            None => problem(format!("{at}: missing `s3:` section")),
            Some(s3) => {
                if s3.bucket.is_empty() || s3.region.is_empty() {
                    problem(format!("{at}: s3.bucket and s3.region are required"));
                }
                let keyless = matches!(s3.key_type.as_deref(), Some("auto" | "web-id"));
                let has = |s: &Option<Secret>| s.as_ref().is_some_and(|s| !s.is_empty());
                if !(keyless || has(&s3.key) && has(&s3.key_secret)) {
                    problem(format!(
                        "{at}: s3.key and s3.key_secret are required unless s3.key_type is auto or web-id"
                    ));
                }
            }
        },
        RepoKind::Gcs => match &repo.gcs {
            None => problem(format!("{at}: missing `gcs:` section")),
            Some(gcs) => {
                if gcs.bucket.is_empty() {
                    problem(format!("{at}: gcs.bucket is required"));
                }
                if gcs.key.is_none() && gcs.key_type.as_deref() != Some("auto") {
                    problem(format!(
                        "{at}: gcs.key is required unless gcs.key_type is auto"
                    ));
                }
            }
        },
        RepoKind::Azure => match &repo.azure {
            None => problem(format!("{at}: missing `azure:` section")),
            Some(az) => {
                if az.account.is_empty() || az.container.is_empty() || az.key.is_empty() {
                    problem(format!(
                        "{at}: azure.account, azure.container and azure.key are required"
                    ));
                }
            }
        },
    }
    for key in repo.options.keys() {
        if !is_option_key(key) {
            problem(format!(
                "{at}: `{key}` is not a valid pgBackRest option name"
            ));
        }
    }
}

pub fn is_simple_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn is_option_key(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// YAML scalars accepted for free-form pgBackRest options.
#[derive(Deserialize)]
#[serde(untagged)]
enum Scalar {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

fn scalar_map<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let raw = BTreeMap::<String, Scalar>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|(k, v)| {
            let v = match v {
                // pgBackRest spells booleans y/n.
                Scalar::Bool(b) => if b { "y" } else { "n" }.to_string(),
                Scalar::Int(i) => i.to_string(),
                Scalar::Float(f) => f.to_string(),
                Scalar::Str(s) => s,
            };
            (k, v)
        })
        .collect())
}

/// Expands `${VAR}`, `${VAR:-default}` and `$$` in every string value (not in keys or comments).
fn expand_env(
    value: &mut serde_yaml_ng::Value,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    use serde_yaml_ng::Value;
    match value {
        Value::String(s) => {
            if s.contains('$') {
                *s = expand_str(s, env)?;
            }
        }
        Value::Sequence(items) => {
            for item in items {
                expand_env(item, env)?;
            }
        }
        Value::Mapping(map) => {
            for (_, v) in map.iter_mut() {
                expand_env(v, env)?;
            }
        }
        Value::Tagged(tagged) => expand_env(&mut tagged.value, env)?,
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn expand_str(s: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
        } else if let Some(body) = after.strip_prefix('{') {
            let end = body
                .find('}')
                .ok_or("unterminated `${` in a config value")?;
            let expr = &body[..end];
            let (name, default) = match expr.split_once(":-") {
                Some((name, default)) => (name, Some(default)),
                None => (expr, None),
            };
            let valid = name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid {
                return Err(format!(
                    "invalid environment variable name `{name}` in `${{{expr}}}`"
                ));
            }
            let value = match (env(name), default) {
                (Some(v), Some(d)) if v.is_empty() => d.to_string(),
                (Some(v), _) => v,
                (None, Some(d)) => d.to_string(),
                (None, None) => return Err(format!("environment variable `{name}` is not set")),
            };
            out.push_str(&value);
            rest = &body[end + 1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn warn_if_exposed(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{:o}", meta.permissions().mode() & 0o777),
            "config file is readable by group/others; it may contain secrets (chmod 600)"
        );
    }
}

pub fn system_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `buf.len()` bytes and gethostname NUL-terminates on success.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(name) = std::str::from_utf8(&buf[..end])
            && !name.is_empty()
        {
            return name.to_string();
        }
    }
    "localhost".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "SET" => Some("value".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn default_template_parses_and_validates() {
        let cfg = Config::from_yaml(DEFAULT_TEMPLATE, &env).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.pgbackrest.repos.len(), 1);
        assert_eq!(cfg.monitoring.listen.as_deref(), Some("0.0.0.0:8000"));
        assert_eq!(cfg.pgbackrest.log_level_file, "off");
    }

    #[test]
    fn empty_file_uses_defaults() {
        let cfg = Config::from_yaml("", &env).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.docker.endpoints[0].url, "unix:///var/run/docker.sock");
    }

    #[test]
    fn env_expansion() {
        assert_eq!(expand_str("a-${SET}-b", &env).unwrap(), "a-value-b");
        assert_eq!(
            expand_str("${MISSING:-fallback}", &env).unwrap(),
            "fallback"
        );
        assert_eq!(expand_str("${EMPTY:-fallback}", &env).unwrap(), "fallback");
        assert_eq!(expand_str("${EMPTY}", &env).unwrap(), "");
        assert_eq!(
            expand_str("cost $5 and $$HOME", &env).unwrap(),
            "cost $5 and $HOME"
        );
        assert!(
            expand_str("${MISSING}", &env)
                .unwrap_err()
                .contains("MISSING")
        );
        assert!(expand_str("${SET", &env).is_err());
        assert!(expand_str("${1BAD}", &env).is_err());
    }

    #[test]
    fn expansion_ignores_comments_and_reaches_nested_values() {
        let yaml = "# key: ${NOT_SET_ANYWHERE}\nnotifications:\n  slack:\n    webhook_url: https://hooks.example/${SET}\n";
        let cfg = Config::from_yaml(yaml, &env).unwrap();
        assert_eq!(
            cfg.notifications.slack.unwrap().webhook_url.expose(),
            "https://hooks.example/value"
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = Config::from_yaml("pgbackrest:\n  reops: []\n", &env).unwrap_err();
        assert!(format!("{err:#}").contains("reops"), "{err:#}");
    }

    #[test]
    fn retention_parsing() {
        assert_eq!("4".parse::<Retention>().unwrap(), Retention::Count(4));
        assert_eq!("7d".parse::<Retention>().unwrap(), Retention::Days(7));
        assert_eq!("2w".parse::<Retention>().unwrap(), Retention::Days(14));
        for bad in ["0", "0d", "12h", "d", "-1", "1.5d"] {
            assert!(bad.parse::<Retention>().is_err(), "{bad}");
        }
    }

    #[test]
    fn validation_collects_problems() {
        let yaml = r#"
docker:
  endpoints:
    - { name: a, url: ftp://x }
    - { name: a, url: unix:///s }
pgbackrest:
  compress_type: brotli
  repos:
    - type: s3
      path: relative/{nope}
      s3: { bucket: b, region: r }
notifications:
  email: { method: aws, from: "" }
monitoring:
  listen: "8000"
"#;
        let err = Config::from_yaml(yaml, &env)
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
        for needle in [
            "ftp://",
            "used twice",
            "brotli",
            "path must be absolute",
            "{nope}",
            "s3.key and s3.key_secret",
            "email.from",
            "email.to",
            "access_key",
            "secret_key",
            "monitoring.listen",
        ] {
            assert!(err.contains(needle), "missing `{needle}` in:\n{err}");
        }
    }

    #[test]
    fn option_maps_accept_scalars() {
        let yaml = "pgbackrest:\n  global_options:\n    archive-timeout: 120\n    repo1-bundle: true\n    log-path: /var/log/pgbackrest\n";
        let cfg = Config::from_yaml(yaml, &env).unwrap();
        let opts = &cfg.pgbackrest.global_options;
        assert_eq!(opts["archive-timeout"], "120");
        assert_eq!(opts["repo1-bundle"], "y");
        assert_eq!(opts["log-path"], "/var/log/pgbackrest");
    }

    #[test]
    fn secrets_are_redacted_in_debug() {
        let s = Secret::new("hunter2");
        assert!(!format!("{s:?}").contains("hunter2"));
    }
}

//! A discovered PostgreSQL container and the backup policy resolved for it.
//!
//! Precedence for every setting: CLI override > container label > container env > config.

use std::collections::HashMap;
use std::time::Duration;

use crate::config::{Config, Retention};
use crate::cron::Schedule;
use crate::duration::parse_duration;
use crate::pgbackrest::BackupType;

/// Operator overrides from the command line.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub force_backup_type: Option<BackupType>,
    pub incremental_backup_interval: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Docker endpoint name from the config.
    pub endpoint: String,
    pub id: String,
    pub name: String,
    pub image: String,
    pub policy: Policy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub stanza: String,
    pub pg_path: String,
    pub pg_port: u16,
    pub pg_user: String,
    pub exec_user: String,
    /// Full-backup schedule.
    pub schedule: Schedule,
    pub incremental_interval: Option<Duration>,
    /// Overrides every repo's `retention_full` when set.
    pub retention_full: Option<Retention>,
}

impl Target {
    /// Stable identity across container re-creation: `endpoint/name`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.endpoint, self.name)
    }

    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }

    /// Matches a `--container` selector: name, `endpoint/name`, stanza or an ID prefix.
    pub fn matches(&self, selector: &str) -> bool {
        selector == self.name
            || selector == self.key()
            || selector == self.policy.stanza
            || (selector.len() >= 4 && self.id.starts_with(selector))
    }
}

const KNOWN_LABELS: &[&str] = &[
    "enable",
    "stanza",
    "schedule",
    "incremental_interval",
    "incremental_backup_interval",
    "retention",
    "pg_path",
    "pg_port",
    "pg_user",
    "exec_user",
    // pg_dump-style hint; physical backups always cover the whole cluster.
    "databases",
];

pub fn label_enabled(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "1" | "on"
    )
}

pub fn is_stanza_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// Container names allow `.`; stanzas are kept to `[A-Za-z0-9_-]`.
fn stanza_from_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    s.chars().take(64).collect()
}

pub struct ContainerFacts<'a> {
    pub endpoint: &'a str,
    pub id: &'a str,
    pub name: &'a str,
    pub image: &'a str,
    pub labels: &'a HashMap<String, String>,
    pub env: &'a [String],
}

/// Builds a target from container facts, returning it with non-fatal warnings.
pub fn resolve(
    cfg: &Config,
    overrides: &Overrides,
    facts: &ContainerFacts<'_>,
) -> Result<(Target, Vec<String>), String> {
    let prefix = &cfg.docker.label_prefix;
    let label = |key: &str| {
        facts
            .labels
            .get(&format!("{prefix}.{key}"))
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    };
    let env: HashMap<&str, &str> = facts.env.iter().filter_map(|e| e.split_once('=')).collect();

    let mut warnings = Vec::new();
    let mut unknown: Vec<&str> = facts
        .labels
        .keys()
        .filter_map(|k| k.strip_prefix(prefix.as_str())?.strip_prefix('.'))
        .filter(|k| !KNOWN_LABELS.contains(k))
        .collect();
    unknown.sort_unstable();
    for key in unknown {
        warnings.push(format!(
            "{}/{}: ignoring unknown label `{prefix}.{key}`",
            facts.endpoint, facts.name
        ));
    }

    let stanza = match label("stanza") {
        Some(s) if is_stanza_name(s) => s.to_string(),
        Some(s) => {
            return Err(format!(
                "label `{prefix}.stanza={s}` may only contain letters, digits, `_` and `-`"
            ));
        }
        None => stanza_from_name(facts.name),
    };
    if !is_stanza_name(&stanza) {
        return Err(format!(
            "cannot derive a stanza name from `{}`; set `{prefix}.stanza`",
            facts.name
        ));
    }

    let schedule = match label("schedule") {
        Some(raw) => raw
            .parse::<Schedule>()
            .map_err(|e| format!("label `{prefix}.schedule`: {e}"))?,
        None => cfg.backup.schedule.clone(),
    };

    let incremental_interval = match overrides.incremental_backup_interval {
        Some(d) => Some(d),
        None => {
            match label("incremental_interval").or_else(|| label("incremental_backup_interval")) {
                Some(raw) => Some(
                    parse_duration(raw)
                        .map_err(|e| format!("label `{prefix}.incremental_interval`: {e}"))?,
                ),
                None => Some(cfg.backup.incremental_backup_interval.0),
            }
        }
    }
    .filter(|d| !d.is_zero());

    let retention_full = label("retention")
        .map(|raw| {
            raw.parse::<Retention>()
                .map_err(|e| format!("label `{prefix}.retention`: {e}"))
        })
        .transpose()?;

    let pg_path = label("pg_path")
        .or(env.get("PGDATA").copied())
        .unwrap_or(&cfg.pgbackrest.pg_path)
        .to_string();
    if !pg_path.starts_with('/') {
        return Err(format!("PGDATA `{pg_path}` must be an absolute path"));
    }

    let pg_port = match label("pg_port").or(env.get("PGPORT").copied()) {
        Some(raw) => raw
            .parse::<u16>()
            .map_err(|_| format!("invalid PostgreSQL port `{raw}`"))?,
        None => cfg.pgbackrest.pg_port,
    };

    let pg_user = label("pg_user")
        .or(env.get("POSTGRES_USER").copied().filter(|u| !u.is_empty()))
        .or(cfg.pgbackrest.pg_user.as_deref())
        .unwrap_or("postgres")
        .to_string();

    let exec_user = label("exec_user")
        .unwrap_or(&cfg.pgbackrest.exec_user)
        .to_string();

    let target = Target {
        endpoint: facts.endpoint.to_string(),
        id: facts.id.to_string(),
        name: facts.name.to_string(),
        image: facts.image.to_string(),
        policy: Policy {
            stanza,
            pg_path,
            pg_port,
            pg_user,
            exec_user,
            schedule,
            incremental_interval,
            retention_full,
        },
    };
    Ok((target, warnings))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sample_target(cfg: &Config, name: &str, labels: &[(&str, &str)]) -> Target {
        let labels: HashMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let env = vec![
            "PGDATA=/var/lib/postgresql/18/docker".to_string(),
            "POSTGRES_USER=app".to_string(),
        ];
        let facts = ContainerFacts {
            endpoint: "local",
            id: "0123456789abcdef0123",
            name,
            image: "postgres:18-alpine",
            labels: &labels,
            env: &env,
        };
        resolve(cfg, &Overrides::default(), &facts).unwrap().0
    }

    #[test]
    fn defaults_come_from_env_and_config() {
        let cfg = Config::default();
        let t = sample_target(&cfg, "db.prod", &[("pg.backup.enable", "true")]);
        assert_eq!(t.policy.stanza, "db-prod");
        assert_eq!(t.policy.pg_path, "/var/lib/postgresql/18/docker");
        assert_eq!(t.policy.pg_user, "app");
        assert_eq!(t.policy.pg_port, 5432);
        assert_eq!(t.policy.exec_user, "postgres");
        assert_eq!(t.policy.schedule.as_str(), "0 2 * * 0");
        assert_eq!(
            t.policy.incremental_interval,
            Some(Duration::from_secs(6 * 3600))
        );
        assert_eq!(t.key(), "local/db.prod");
        assert!(
            t.matches("db.prod")
                && t.matches("local/db.prod")
                && t.matches("db-prod")
                && t.matches("0123")
        );
        assert!(!t.matches("012") && !t.matches("other"));
    }

    #[test]
    fn labels_override_defaults() {
        let cfg = Config::default();
        let t = sample_target(
            &cfg,
            "db",
            &[
                ("pg.backup.stanza", "orders"),
                ("pg.backup.schedule", "30 1 * * *"),
                ("pg.backup.incremental_interval", "0"),
                ("pg.backup.retention", "7d"),
                ("pg.backup.pg_port", "6432"),
            ],
        );
        assert_eq!(t.policy.stanza, "orders");
        assert_eq!(t.policy.schedule.as_str(), "30 1 * * *");
        assert_eq!(t.policy.incremental_interval, None);
        assert_eq!(t.policy.retention_full, Some(Retention::Days(7)));
        assert_eq!(t.policy.pg_port, 6432);
    }

    #[test]
    fn cli_override_beats_labels() {
        let cfg = Config::default();
        let labels: HashMap<String, String> = [(
            "pg.backup.incremental_interval".to_string(),
            "1h".to_string(),
        )]
        .into();
        let facts = ContainerFacts {
            endpoint: "local",
            id: "abcd1234",
            name: "db",
            image: "",
            labels: &labels,
            env: &[],
        };
        let overrides = Overrides {
            incremental_backup_interval: Some(Duration::from_secs(1800)),
            ..Default::default()
        };
        let (t, _) = resolve(&cfg, &overrides, &facts).unwrap();
        assert_eq!(
            t.policy.incremental_interval,
            Some(Duration::from_secs(1800))
        );
    }

    #[test]
    fn invalid_labels_are_errors_and_unknown_labels_warn() {
        let cfg = Config::default();
        let check = |labels: &[(&str, &str)]| {
            let labels: HashMap<String, String> = labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let facts = ContainerFacts {
                endpoint: "local",
                id: "abcd1234",
                name: "db",
                image: "",
                labels: &labels,
                env: &[],
            };
            resolve(&cfg, &Overrides::default(), &facts)
        };
        assert!(
            check(&[("pg.backup.schedule", "every day")])
                .unwrap_err()
                .contains("schedule")
        );
        assert!(
            check(&[("pg.backup.stanza", "a/b")])
                .unwrap_err()
                .contains("stanza")
        );
        assert!(
            check(&[("pg.backup.retention", "12h")])
                .unwrap_err()
                .contains("retention")
        );
        assert!(
            check(&[("pg.backup.pg_port", "99999")])
                .unwrap_err()
                .contains("port")
        );
        let (_, warnings) = check(&[("pg.backup.shedule", "0 1 * * *")]).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("pg.backup.shedule"));
    }

    #[test]
    fn enable_values() {
        for yes in ["true", "TRUE", "yes", "1", "on"] {
            assert!(label_enabled(yes));
        }
        for no in ["false", "0", "", "nope"] {
            assert!(!label_enabled(no));
        }
    }
}

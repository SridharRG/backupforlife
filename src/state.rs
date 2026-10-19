//! Per-target results persisted to `monitoring.metrics_file` as JSON. Shared by
//! `run`, the daemon and `status_all` (and readable by Sensu checks), with
//! Prometheus text rendering for `/metrics` and the node_exporter textfile collector.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::backup::{JobResult, JobStatus};
use crate::pgbackrest::BackupType;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StateFile {
    pub version: u32,
    pub host_id: String,
    pub updated_at: Option<DateTime<Utc>>,
    pub targets: BTreeMap<String, TargetState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TargetState {
    pub endpoint: String,
    pub container_id: String,
    pub stanza: String,
    pub last_status: Option<JobStatus>,
    pub last_reason: Option<String>,
    pub last_type: Option<BackupType>,
    pub last_started_at: Option<DateTime<Utc>>,
    pub last_finished_at: Option<DateTime<Utc>>,
    pub last_duration_secs: f64,
    pub last_error: Option<String>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_success_type: Option<BackupType>,
    pub last_backup_label: Option<String>,
    pub last_size_bytes: Option<u64>,
    pub last_repo_size_bytes: Option<u64>,
    pub consecutive_failures: u32,
    pub success_total: u64,
    pub failure_total: u64,
}

impl TargetState {
    fn apply(&mut self, r: &JobResult) {
        self.endpoint = r.endpoint.clone();
        self.container_id = r.container_id.clone();
        self.stanza = r.stanza.clone();
        self.last_status = Some(r.status);
        self.last_reason = Some(r.reason.clone());
        self.last_type = r.backup_type;
        self.last_started_at = Some(r.started_at);
        self.last_finished_at = Some(r.finished_at);
        self.last_duration_secs = r.duration().as_secs_f64();
        self.last_error = r.error.clone();
        if r.failed() {
            self.consecutive_failures += 1;
            self.failure_total += 1;
        } else {
            self.consecutive_failures = 0;
            self.success_total += 1;
            self.last_success_at = Some(r.finished_at);
            self.last_success_type = r.backup_type;
            self.last_backup_label = r.label.clone();
            self.last_size_bytes = r.size_bytes;
            self.last_repo_size_bytes = r.repo_size_bytes;
        }
    }

    pub fn is_failing(&self) -> bool {
        self.last_status == Some(JobStatus::Failed)
    }
}

/// Reads the state file; a missing file is an empty state.
pub fn load(path: &Path) -> Result<StateFile> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StateFile::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Merges job results into the state file under an exclusive lock (concurrent
/// `run` invocations and the daemon may all write it). Dry runs are not recorded.
pub fn record(path: &Path, host_id: &str, results: &[JobResult]) -> Result<StateFile> {
    let _lock = FileLock::exclusive(&sibling(path, ".lock"))?;
    let mut state = match load(path) {
        Ok(state) => state,
        Err(e) => {
            let aside = sibling(path, ".corrupt");
            tracing::warn!(error = %format!("{e:#}"), moved_to = %aside.display(), "state file unreadable; starting fresh");
            let _ = std::fs::rename(path, &aside);
            StateFile::default()
        }
    };
    for r in results.iter().filter(|r| r.status != JobStatus::DryRun) {
        state.targets.entry(r.target.clone()).or_default().apply(r);
    }
    state.version = 1;
    state.host_id = host_id.to_string();
    state.updated_at = Some(Utc::now());
    write_atomic(path, &serde_json::to_vec_pretty(&state)?)?;
    Ok(state)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// Write-to-temp, fsync, rename: readers never see a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = sibling(path, &format!(".tmp{}", std::process::id()));
    let mut file = File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

struct FileLock(#[allow(dead_code)] File);

impl FileLock {
    fn exclusive(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: valid open fd; the lock is released when the file is closed.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking {}", path.display()));
        }
        Ok(Self(file))
    }
}

/// Daemon-only facts for `/metrics`.
pub struct Live<'a> {
    pub started_at: DateTime<Utc>,
    pub in_progress: &'a BTreeSet<String>,
}

pub fn prometheus(state: &StateFile, live: Option<&Live<'_>>) -> String {
    let mut out = String::new();
    let host = &state.host_id;
    let labels = |key: &str, t: &TargetState| {
        format!(
            "host=\"{}\",target=\"{}\",stanza=\"{}\"",
            escape(host),
            escape(key),
            escape(&t.stanza)
        )
    };
    let mut gauge = |name: &str, help: &str, kind: &str, samples: Vec<(String, f64)>| {
        if samples.is_empty() {
            return;
        }
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        for (labels, value) in samples {
            let _ = writeln!(out, "{name}{{{labels}}} {value}");
        }
    };
    let each = |f: &dyn Fn(&TargetState) -> Option<f64>| -> Vec<(String, f64)> {
        state
            .targets
            .iter()
            .filter_map(|(k, t)| f(t).map(|v| (labels(k, t), v)))
            .collect()
    };
    let ts = |t: Option<DateTime<Utc>>| t.map(|t| t.timestamp() as f64);

    gauge(
        "pgback_last_backup_success",
        "Whether the most recent backup attempt succeeded (1) or failed (0).",
        "gauge",
        each(&|t| {
            t.last_status
                .map(|s| if s == JobStatus::Failed { 0.0 } else { 1.0 })
        }),
    );
    gauge(
        "pgback_last_backup_timestamp_seconds",
        "When the most recent backup attempt finished.",
        "gauge",
        each(&|t| ts(t.last_finished_at)),
    );
    gauge(
        "pgback_last_backup_duration_seconds",
        "Duration of the most recent backup attempt.",
        "gauge",
        each(&|t| t.last_status.map(|_| t.last_duration_secs)),
    );
    gauge(
        "pgback_last_success_timestamp_seconds",
        "When the most recent successful backup finished.",
        "gauge",
        each(&|t| ts(t.last_success_at)),
    );
    gauge(
        "pgback_last_backup_size_bytes",
        "Database size captured by the latest successful backup.",
        "gauge",
        each(&|t| t.last_size_bytes.map(|v| v as f64)),
    );
    gauge(
        "pgback_last_backup_repo_size_bytes",
        "Repository size of the latest successful backup.",
        "gauge",
        each(&|t| t.last_repo_size_bytes.map(|v| v as f64)),
    );
    gauge(
        "pgback_consecutive_failures",
        "Backup failures since the last success.",
        "gauge",
        each(&|t| Some(t.consecutive_failures as f64)),
    );

    let mut totals = Vec::new();
    for (k, t) in &state.targets {
        totals.push((
            format!("{},result=\"success\"", labels(k, t)),
            t.success_total as f64,
        ));
        totals.push((
            format!("{},result=\"failure\"", labels(k, t)),
            t.failure_total as f64,
        ));
    }
    gauge(
        "pgback_backups_total",
        "Backup attempts recorded, by result.",
        "counter",
        totals,
    );

    if let Some(live) = live {
        let running = state
            .targets
            .iter()
            .map(|(k, t)| {
                (
                    labels(k, t),
                    if live.in_progress.contains(k) {
                        1.0
                    } else {
                        0.0
                    },
                )
            })
            .collect();
        gauge(
            "pgback_backup_in_progress",
            "Whether a backup is running now.",
            "gauge",
            running,
        );
        gauge(
            "pgback_daemon_start_time_seconds",
            "When the pgback daemon started.",
            "gauge",
            vec![(
                format!("host=\"{}\"", escape(host)),
                live.started_at.timestamp() as f64,
            )],
        );
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn result(target: &str, status: JobStatus) -> JobResult {
        let start = Utc::now();
        JobResult {
            target: target.into(),
            endpoint: "local".into(),
            container_id: "abc".into(),
            stanza: "db".into(),
            reason: "manual".into(),
            backup_type: Some(BackupType::Incr),
            status,
            started_at: start,
            finished_at: start + TimeDelta::seconds(10),
            label: (status == JobStatus::Success).then(|| "L1".into()),
            size_bytes: (status == JobStatus::Success).then_some(100),
            repo_size_bytes: None,
            error: (status == JobStatus::Failed).then(|| "boom".into()),
            warnings: vec![],
            log_tail: vec![],
            history: None,
        }
    }

    #[test]
    fn records_and_merges_results() {
        let dir = std::env::temp_dir().join(format!("pgback-state-{}", std::process::id()));
        let path = dir.join("metrics.json");
        let _ = std::fs::remove_dir_all(&dir);

        record(
            &path,
            "h",
            &[
                result("local/a", JobStatus::Success),
                result("local/b", JobStatus::DryRun),
            ],
        )
        .unwrap();
        let state = record(&path, "h", &[result("local/a", JobStatus::Failed)]).unwrap();
        assert_eq!(state.targets.len(), 1, "dry runs are not recorded");
        let a = &state.targets["local/a"];
        assert!(a.is_failing());
        assert_eq!(
            (a.consecutive_failures, a.success_total, a.failure_total),
            (1, 1, 1)
        );
        assert_eq!(
            a.last_backup_label.as_deref(),
            Some("L1"),
            "last success is kept across a failure"
        );
        assert_eq!(load(&path).unwrap().targets.len(), 1);

        std::fs::write(&path, b"{not json").unwrap();
        let state = record(&path, "h", &[result("local/c", JobStatus::Success)]).unwrap();
        assert_eq!(state.targets.len(), 1);
        assert!(dir.join("metrics.json.corrupt").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn renders_prometheus() {
        let mut state = StateFile {
            host_id: "h".into(),
            ..Default::default()
        };
        state
            .targets
            .entry("local/a\"x".into())
            .or_default()
            .apply(&result("local/a\"x", JobStatus::Success));
        let in_progress: BTreeSet<String> = ["local/a\"x".to_string()].into();
        let text = prometheus(
            &state,
            Some(&Live {
                started_at: Utc::now(),
                in_progress: &in_progress,
            }),
        );
        assert!(text.contains("# TYPE pgback_last_backup_success gauge"));
        assert!(
            text.contains(
                "pgback_last_backup_success{host=\"h\",target=\"local/a\\\"x\",stanza=\"db\"} 1"
            ),
            "{text}"
        );
        assert!(text.contains("pgback_backups_total{host=\"h\",target=\"local/a\\\"x\",stanza=\"db\",result=\"success\"} 1"));
        assert!(text.contains(
            "pgback_backup_in_progress{host=\"h\",target=\"local/a\\\"x\",stanza=\"db\"} 1"
        ));
        assert!(
            !text.contains("repo_size_bytes{"),
            "absent values are omitted"
        );
    }
}

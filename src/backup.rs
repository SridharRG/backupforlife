//! Backup scheduling decisions and the per-target job pipeline:
//! stanza check → backup type → pre-hooks → `pgbackrest backup` → post-hooks.

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Local, TimeDelta, TimeZone, Utc};
use futures_util::StreamExt;
use serde::Serialize;

use crate::app::Ctx;
use crate::config::BackupConfig;
use crate::cron::Schedule;
use crate::docker::{StreamKind, exec_capture, exec_lines};
use crate::duration::format_duration;
use crate::hooks::{self, Phase};
use crate::pgbackrest::{self, BackupType, History, StanzaInfo};
use crate::target::{Policy, Target};

/// Flags that keep capture-mode pgBackRest output machine-readable.
const QUIET: [&str; 2] = ["--log-level-console=off", "--log-level-stderr=warn"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DueReason {
    /// The stanza has no full backup yet.
    NoBackups,
    /// A full-backup schedule slot has passed since the last full.
    FullScheduled,
    /// The incremental interval has elapsed since the last backup.
    IntervalElapsed,
}

impl DueReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoBackups => "no_backups",
            Self::FullScheduled => "full_scheduled",
            Self::IntervalElapsed => "interval_elapsed",
        }
    }
}

/// True when a schedule slot has come and gone since `last_full` started.
fn full_owed<Tz: TimeZone>(
    now: &DateTime<Tz>,
    schedule: &Schedule,
    last_full: DateTime<Utc>,
) -> bool {
    schedule
        .next_after(&last_full.with_timezone(&now.timezone()))
        .is_some_and(|slot| slot <= *now)
}

/// Why a backup is due now, if it is. Shared by the daemon and `run --if_due`.
pub fn due_reason<Tz: TimeZone>(
    now: &DateTime<Tz>,
    policy: &Policy,
    history: &History,
) -> Option<DueReason> {
    let Some(last_full) = history.last_full_start else {
        return Some(DueReason::NoBackups);
    };
    if full_owed(now, &policy.schedule, last_full) {
        return Some(DueReason::FullScheduled);
    }
    let interval = policy.incremental_interval?;
    let last = history.last_backup_start.unwrap_or(last_full);
    let elapsed = (now.with_timezone(&Utc) - last)
        .to_std()
        .unwrap_or_default();
    (elapsed >= interval).then_some(DueReason::IntervalElapsed)
}

/// Full when forced, when no full exists, or when a scheduled full was missed; else incremental.
/// This makes `pgback run` from any cron cadence produce fulls on schedule.
pub fn decide_type<Tz: TimeZone>(
    now: &DateTime<Tz>,
    policy: &Policy,
    history: &History,
    forced: Option<BackupType>,
) -> BackupType {
    if let Some(forced) = forced {
        return forced;
    }
    match history.last_full_start {
        Some(last_full) if !full_owed(now, &policy.schedule, last_full) => BackupType::Incr,
        _ => BackupType::Full,
    }
}

/// When the next backup will become due (now, if it already is).
pub fn next_due<Tz: TimeZone>(
    now: &DateTime<Tz>,
    policy: &Policy,
    history: &History,
) -> Option<DateTime<Utc>> {
    let now_utc = now.with_timezone(&Utc);
    if due_reason(now, policy, history).is_some() {
        return Some(now_utc);
    }
    let last_full = history.last_full_start?;
    let full = policy
        .schedule
        .next_after(&last_full.with_timezone(&now.timezone()))
        .map(|t| t.with_timezone(&Utc));
    let incr = policy.incremental_interval.and_then(|iv| {
        let last = history.last_backup_start.unwrap_or(last_full);
        TimeDelta::from_std(iv).ok().map(|iv| last + iv)
    });
    [full, incr].into_iter().flatten().min()
}

/// Exponential backoff after consecutive failures: base, 2×base, 4×base … capped.
pub fn retry_after(
    cfg: &BackupConfig,
    consecutive_failures: u32,
    last_failure: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if consecutive_failures == 0 {
        return None;
    }
    let cap = cfg.retry_max_backoff.0;
    let factor = 2u32.saturating_pow(consecutive_failures.saturating_sub(1).min(31));
    let delay = cfg
        .retry_backoff
        .0
        .checked_mul(factor)
        .unwrap_or(cap)
        .min(cap);
    Some(last_failure + TimeDelta::from_std(delay).unwrap_or(TimeDelta::days(1)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Success,
    Failed,
    DryRun,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::DryRun => "dry-run",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobResult {
    pub target: String,
    pub endpoint: String,
    pub container_id: String,
    pub stanza: String,
    /// `manual`, or a [`DueReason`] for scheduled runs.
    pub reason: String,
    pub backup_type: Option<BackupType>,
    pub status: JobStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub label: Option<String>,
    pub size_bytes: Option<u64>,
    pub repo_size_bytes: Option<u64>,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Last lines of pgBackRest output, attached to failure alerts.
    #[serde(skip)]
    pub log_tail: Vec<String>,
    /// Stanza history after the job, for the scheduler.
    #[serde(skip)]
    pub history: Option<History>,
}

impl JobResult {
    pub fn duration(&self) -> Duration {
        (self.finished_at - self.started_at)
            .to_std()
            .unwrap_or_default()
    }

    pub fn failed(&self) -> bool {
        self.status == JobStatus::Failed
    }
}

/// One invocation's worth of results, as handed to notifiers.
#[derive(Debug, Clone)]
pub struct Report {
    pub host_id: String,
    pub trigger: String,
    pub results: Vec<JobResult>,
    /// Failures not tied to one backup, e.g. an unreachable Docker endpoint.
    pub errors: Vec<String>,
}

impl Report {
    pub fn failures(&self) -> usize {
        self.results.iter().filter(|r| r.failed()).count() + self.errors.len()
    }
}

/// Bounded buffer of the most recent output lines.
struct LogTail {
    lines: VecDeque<String>,
    cap: usize,
}

impl LogTail {
    fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn push(&mut self, line: &str) {
        if self.lines.len() == self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(line.to_string());
    }

    /// pgBackRest `ERROR:` lines if any, else the last few lines.
    fn error_summary(&self) -> String {
        let errors: Vec<&str> = self
            .lines
            .iter()
            .filter(|l| l.contains("ERROR:"))
            .map(String::as_str)
            .collect();
        if !errors.is_empty() {
            return errors.join("\n");
        }
        let skip = self.lines.len().saturating_sub(3);
        self.lines
            .iter()
            .skip(skip)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Reads the stanza's state from the repository.
pub async fn query_info(ctx: &Ctx, target: &Target) -> Result<StanzaInfo> {
    let docker = ctx.docker.client(&target.endpoint)?;
    let stanza = &target.policy.stanza;
    let mut extra = vec!["--output=json"];
    extra.extend(QUIET);
    let cmd = pgbackrest::command(&ctx.cfg, stanza, "info", &extra);
    let out = exec_capture(docker, &target.id, &target.policy.exec_user, &cmd).await?;
    if !out.success() {
        bail!(
            "pgbackrest info failed (exit {}): {}",
            out.exit_code,
            out.tail(5)
        );
    }
    pgbackrest::parse_info(&out.stdout)
        .map_err(|e| anyhow!("unreadable `pgbackrest info` output: {e}"))?
        .into_iter()
        .find(|s| s.name == *stanza)
        .ok_or_else(|| anyhow!("pgbackrest info did not report stanza `{stanza}`"))
}

pub async fn stanza_create(ctx: &Ctx, target: &Target) -> Result<()> {
    let docker = ctx.docker.client(&target.endpoint)?;
    let cmd = pgbackrest::command(&ctx.cfg, &target.policy.stanza, "stanza-create", &QUIET);
    let out = exec_capture(docker, &target.id, &target.policy.exec_user, &cmd).await?;
    if !out.success() {
        bail!(
            "stanza-create failed (exit {}): {}",
            out.exit_code,
            out.tail(5)
        );
    }
    Ok(())
}

async fn ensure_stanza(ctx: &Ctx, target: &Target) -> Result<StanzaInfo> {
    let info = query_info(ctx, target).await?;
    if !info.missing_stanza() {
        return Ok(info);
    }
    let stanza = &target.policy.stanza;
    if !ctx.cfg.pgbackrest.auto_stanza_create {
        bail!("stanza `{stanza}` does not exist in the repository; run `pgback --config_sync`");
    }
    if ctx.dry_run {
        tracing::info!(target_key = %target.key(), stanza, "dry run: would run stanza-create");
        return Ok(info);
    }
    tracing::info!(target_key = %target.key(), stanza, "creating stanza");
    stanza_create(ctx, target).await?;
    query_info(ctx, target).await
}

/// Runs one backup end to end. Never panics or returns early: every outcome is a `JobResult`.
pub async fn run_job(ctx: &Ctx, target: &Target, reason: &str) -> JobResult {
    let key = target.key();
    let started_at = Utc::now();
    let mut tail = LogTail::new(30);
    let mut backup_type = None;
    let mut pre_hooks_reached = false;

    let outcome: Result<Option<StanzaInfo>> = async {
        let info = ensure_stanza(ctx, target).await?;
        let ty = decide_type(&Local::now(), &target.policy, &info.history(), ctx.overrides.force_backup_type);
        backup_type = Some(ty);
        tracing::info!(target_key = %key, stanza = %target.policy.stanza, backup_type = %ty, reason, "backup starting");

        pre_hooks_reached = true;
        let env = hook_env(ctx, target, Phase::Pre, Some(ty));
        let failures = hooks::run(&ctx.cfg.hooks.dir, Phase::Pre, ctx.cfg.hooks.timeout.0, &env, ctx.dry_run, &key).await;
        if let Some(failure) = failures.first() {
            bail!("pre-backup {failure}");
        }

        if ctx.dry_run {
            let cmd = pgbackrest::command(&ctx.cfg, &target.policy.stanza, "backup", &[&format!("--type={ty}")]);
            tracing::info!(target_key = %key, cmd = cmd.join(" "), "dry run: would run backup");
            return Ok(None);
        }
        execute_backup(ctx, target, ty, &mut tail).await?;
        match query_info(ctx, target).await {
            Ok(info) => Ok(Some(info)),
            Err(e) => {
                tracing::warn!(target_key = %key, error = %format!("{e:#}"), "backup succeeded but refreshing info failed");
                Ok(None)
            }
        }
    }
    .await;

    let mut result = JobResult {
        target: key.clone(),
        endpoint: target.endpoint.clone(),
        container_id: target.id.clone(),
        stanza: target.policy.stanza.clone(),
        reason: reason.to_string(),
        backup_type,
        status: JobStatus::Failed,
        started_at,
        finished_at: Utc::now(),
        label: None,
        size_bytes: None,
        repo_size_bytes: None,
        error: None,
        warnings: Vec::new(),
        log_tail: tail.lines.into_iter().collect(),
        history: None,
    };
    match outcome {
        Ok(info) => {
            result.status = if ctx.dry_run {
                JobStatus::DryRun
            } else {
                JobStatus::Success
            };
            if let Some(info) = info {
                result.history = Some(info.history());
                if let Some(latest) = info.latest() {
                    result.label = Some(latest.label.clone());
                    result.size_bytes = Some(latest.size_bytes);
                    result.repo_size_bytes = latest.repo_size_bytes;
                }
            }
        }
        Err(e) => result.error = Some(format!("{e:#}")),
    }

    // Post hooks pair with pre hooks: they run whenever the pre phase was reached,
    // so a hook that paused something can always resume it.
    if pre_hooks_reached {
        let env = hook_env_post(ctx, target, &result);
        let failures = hooks::run(
            &ctx.cfg.hooks.dir,
            Phase::Post,
            ctx.cfg.hooks.timeout.0,
            &env,
            ctx.dry_run,
            &key,
        )
        .await;
        result
            .warnings
            .extend(failures.iter().map(|f| format!("post-backup {f}")));
    }
    result.finished_at = Utc::now();

    let duration = format_duration(result.duration());
    match &result.error {
        None => tracing::info!(
            target_key = %key,
            status = result.status.as_str(),
            backup_type = result.backup_type.map(BackupType::as_str),
            label = result.label.as_deref(),
            duration,
            "backup finished"
        ),
        Some(error) => tracing::error!(target_key = %key, error, duration, "backup failed"),
    }
    result
}

async fn execute_backup(
    ctx: &Ctx,
    target: &Target,
    ty: BackupType,
    tail: &mut LogTail,
) -> Result<()> {
    let docker = ctx.docker.client(&target.endpoint)?;
    let cmd = pgbackrest::command(
        &ctx.cfg,
        &target.policy.stanza,
        "backup",
        &[&format!("--type={ty}")],
    );
    let key = target.key();
    tracing::debug!(target_key = %key, cmd = cmd.join(" "), "exec");

    let exec = exec_lines(
        docker,
        &target.id,
        &target.policy.exec_user,
        &cmd,
        |kind, line| {
            if line.trim().is_empty() {
                return;
            }
            tail.push(line);
            // pgBackRest writes its own level into each console line.
            if line.contains(" ERROR: ") {
                tracing::error!(target_key = %key, "{line}");
            } else if line.contains(" WARN: ") || kind == StreamKind::Stderr {
                tracing::warn!(target_key = %key, "{line}");
            } else {
                tracing::info!(target_key = %key, "{line}");
            }
        },
    );
    let code = match ctx.cfg.backup.timeout.non_zero() {
        None => exec.await?,
        Some(limit) => match tokio::time::timeout(limit, exec).await {
            Ok(code) => code?,
            Err(_) => {
                abort_backup(ctx, target).await;
                bail!(
                    "backup timed out after {}; running pgBackRest processes were stopped",
                    format_duration(limit)
                );
            }
        },
    };
    if code != 0 {
        bail!(
            "pgbackrest backup exited with code {code}: {}",
            tail.error_summary()
        );
    }
    Ok(())
}

/// Terminates a runaway backup inside the container, then re-enables pgBackRest.
async fn abort_backup(ctx: &Ctx, target: &Target) {
    let Ok(docker) = ctx.docker.client(&target.endpoint) else {
        return;
    };
    for (op, extra) in [("stop", &["--force"][..]), ("start", &[][..])] {
        let cmd = pgbackrest::command(&ctx.cfg, &target.policy.stanza, op, extra);
        match exec_capture(docker, &target.id, &target.policy.exec_user, &cmd).await {
            Ok(out) if out.success() => {}
            Ok(out) => {
                tracing::error!(target_key = %target.key(), op, output = out.tail(3), "pgbackrest {op} failed")
            }
            Err(e) => {
                tracing::error!(target_key = %target.key(), op, error = %format!("{e:#}"), "pgbackrest {op} failed")
            }
        }
    }
}

fn hook_env(
    ctx: &Ctx,
    target: &Target,
    phase: Phase,
    ty: Option<BackupType>,
) -> Vec<(String, String)> {
    let pairs = [
        ("PGBACK_PHASE", phase.as_str().to_string()),
        ("PGBACK_HOST_ID", ctx.host_id.clone()),
        ("PGBACK_TARGET", target.key()),
        ("PGBACK_ENDPOINT", target.endpoint.clone()),
        ("PGBACK_CONTAINER_ID", target.id.clone()),
        ("PGBACK_CONTAINER_NAME", target.name.clone()),
        ("PGBACK_STANZA", target.policy.stanza.clone()),
        (
            "PGBACK_BACKUP_TYPE",
            ty.map(|t| t.as_str()).unwrap_or_default().to_string(),
        ),
        (
            "PGBACK_DRY_RUN",
            if ctx.dry_run { "1" } else { "0" }.to_string(),
        ),
    ];
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn hook_env_post(ctx: &Ctx, target: &Target, result: &JobResult) -> Vec<(String, String)> {
    let mut env = hook_env(ctx, target, Phase::Post, result.backup_type);
    let status = if result.failed() {
        "failure"
    } else {
        "success"
    };
    env.push(("PGBACK_STATUS".into(), status.into()));
    env.push((
        "PGBACK_ERROR".into(),
        result.error.clone().unwrap_or_default(),
    ));
    env.push((
        "PGBACK_DURATION_SECS".into(),
        (Utc::now() - result.started_at).num_seconds().to_string(),
    ));
    env.push((
        "PGBACK_BACKUP_LABEL".into(),
        result.label.clone().unwrap_or_default(),
    ));
    env
}

/// Runs jobs with at most `backup.max_parallel` at once; results sorted by target.
pub async fn run_many(ctx: &Ctx, jobs: Vec<(Target, String)>) -> Vec<JobResult> {
    let mut results: Vec<JobResult> = futures_util::stream::iter(jobs)
        .map(|(target, reason)| async move { run_job(ctx, &target, &reason).await })
        .buffer_unordered(ctx.cfg.backup.max_parallel.max(1))
        .collect()
        .await;
    results.sort_by(|a, b| a.target.cmp(&b.target));
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::target::tests::sample_target;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Weekly full on Sunday 02:00, incrementals every 6h.
    fn policy() -> Policy {
        let cfg = Config::default();
        sample_target(&cfg, "db", &[]).policy
    }

    fn history(full: &str, last: &str) -> History {
        History {
            last_full_start: Some(utc(full)),
            last_backup_start: Some(utc(last)),
        }
    }

    #[test]
    fn no_backups_means_full_now() {
        let now = utc("2026-01-07T10:00:00Z");
        assert_eq!(
            due_reason(&now, &policy(), &History::default()),
            Some(DueReason::NoBackups)
        );
        assert_eq!(
            decide_type(&now, &policy(), &History::default(), None),
            BackupType::Full
        );
        assert_eq!(next_due(&now, &policy(), &History::default()), Some(now));
    }

    #[test]
    fn incrementals_between_scheduled_fulls() {
        // Full ran Sunday 2026-01-04 02:00; last incremental Wednesday 06:00.
        let h = history("2026-01-04T02:00:00Z", "2026-01-07T06:00:00Z");
        let early = utc("2026-01-07T11:59:00Z");
        assert_eq!(due_reason(&early, &policy(), &h), None);
        assert_eq!(
            next_due(&early, &policy(), &h),
            Some(utc("2026-01-07T12:00:00Z"))
        );
        let later = utc("2026-01-07T12:00:00Z");
        assert_eq!(
            due_reason(&later, &policy(), &h),
            Some(DueReason::IntervalElapsed)
        );
        assert_eq!(decide_type(&later, &policy(), &h, None), BackupType::Incr);
    }

    #[test]
    fn missed_full_slot_promotes_to_full() {
        // Last full a week and a bit ago: the Sunday 2026-01-11 02:00 slot has passed.
        let h = history("2026-01-04T02:00:00Z", "2026-01-11T00:00:00Z");
        let now = utc("2026-01-11T02:00:30Z");
        assert_eq!(
            due_reason(&now, &policy(), &h),
            Some(DueReason::FullScheduled)
        );
        assert_eq!(decide_type(&now, &policy(), &h, None), BackupType::Full);
        assert_eq!(
            decide_type(&now, &policy(), &h, Some(BackupType::Diff)),
            BackupType::Diff
        );
    }

    #[test]
    fn disabled_incrementals_wait_for_the_schedule() {
        let mut p = policy();
        p.incremental_interval = None;
        let h = history("2026-01-04T02:00:00Z", "2026-01-04T02:00:00Z");
        let now = utc("2026-01-09T00:00:00Z");
        assert_eq!(due_reason(&now, &p, &h), None);
        assert_eq!(next_due(&now, &p, &h), Some(utc("2026-01-11T02:00:00Z")));
    }

    #[test]
    fn retry_backoff_doubles_and_caps() {
        let cfg = Config::default().backup; // 15m base, 6h cap
        let t = utc("2026-01-01T00:00:00Z");
        assert_eq!(retry_after(&cfg, 0, t), None);
        assert_eq!(retry_after(&cfg, 1, t), Some(utc("2026-01-01T00:15:00Z")));
        assert_eq!(retry_after(&cfg, 3, t), Some(utc("2026-01-01T01:00:00Z")));
        assert_eq!(retry_after(&cfg, 10, t), Some(utc("2026-01-01T06:00:00Z")));
        assert_eq!(
            retry_after(&cfg, u32::MAX, t),
            Some(utc("2026-01-01T06:00:00Z"))
        );
    }

    #[test]
    fn log_tail_prefers_error_lines() {
        let mut tail = LogTail::new(3);
        for line in [
            "a",
            "P00  ERROR: [056]: unable to find primary cluster",
            "b",
            "c",
            "d",
        ] {
            tail.push(line);
        }
        assert_eq!(tail.lines.len(), 3);
        assert_eq!(tail.error_summary(), "b\nc\nd", "error line was evicted");
        tail.push("P00  ERROR: [082]: WAL segment was not archived");
        assert!(tail.error_summary().contains("[082]"));
    }
}

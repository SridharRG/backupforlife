//! Background scheduling: the foreground `daemon` loop (what systemd runs) and the
//! `start_all` / `stop_all` / `status_all` service actions.
//!
//! The loop ticks every 30 s. Each tick re-discovers containers when due, pushes
//! config to new or changed ones, and starts backups whose full schedule or
//! incremental interval has come round (bounded by `backup.max_parallel`). A failed
//! target backs off exponentially. SIGTERM/SIGINT stop scheduling and wait for
//! running backups (up to `daemon.shutdown_timeout`; a second signal exits at once),
//! SIGHUP reloads the configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, TimeDelta, Utc};
use futures_util::FutureExt;
use futures_util::future::join_all;
use serde::Serialize;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};

use crate::app::{Ctx, Outcome, apply_selectors, failed_result, persist};
use crate::backup::{self, JobResult, JobStatus, Report, due_reason, next_due, retry_after};
use crate::cli::{Cli, LogFormat, OutputFormat};
use crate::docker;
use crate::duration::format_duration;
use crate::http::{self, Shared, Snapshot, TargetView};
use crate::pgbackrest::{self, BackupType, History};
use crate::state::{self, StateFile};
use crate::sync;
use crate::table::{self, local_time, relative};
use crate::target::Target;

const TICK: Duration = Duration::from_secs(30);

/// Scheduler bookkeeping for one target.
struct Managed {
    target: Target,
    history: Option<History>,
    /// Hash of the pgbackrest.conf last pushed successfully.
    synced_hash: Option<u64>,
    last_sync_error: Option<String>,
    running: bool,
    consecutive_failures: u32,
    last_failure_at: Option<DateTime<Utc>>,
}

impl Managed {
    fn retry_at(&self, ctx: &Ctx) -> Option<DateTime<Utc>> {
        retry_after(
            &ctx.cfg.backup,
            self.consecutive_failures,
            self.last_failure_at?,
        )
    }
}

pub async fn run_foreground(cli: &Cli, initial: Arc<Ctx>) -> Result<Outcome> {
    let mut ctx = initial;
    let pid_file = ctx.cfg.daemon.pid_file.clone();
    let _lock = PidLock::acquire(&pid_file)?;
    let started_at = Utc::now();
    tracing::info!(pid = std::process::id(), version = env!("CARGO_PKG_VERSION"), host = %ctx.host_id, dry_run = ctx.dry_run, "daemon started");

    let mut state_cache = state::load(&ctx.cfg.monitoring.metrics_file).unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), "ignoring unreadable state file");
        StateFile::default()
    });
    let shared: Shared = Arc::new(RwLock::new(Snapshot {
        host_id: ctx.host_id.clone(),
        started_at,
        stopping: false,
        state: state_cache.clone(),
        in_progress: BTreeSet::new(),
        targets: Vec::new(),
    }));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let http_task = match &ctx.cfg.monitoring.listen {
        Some(addr) => {
            let listener = http::bind(addr).await?;
            tracing::info!(listen = %addr, "serving /health, /status and /metrics");
            Some(tokio::spawn(http::serve(
                listener,
                shared.clone(),
                shutdown_rx,
            )))
        }
        None => None,
    };

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<JobResult>();
    let mut permits = Arc::new(Semaphore::new(ctx.cfg.backup.max_parallel));
    let mut managed: BTreeMap<String, Managed> = BTreeMap::new();
    let mut discovery_errors = BTreeSet::new();
    let mut next_discovery = Instant::now();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stopping = false;
    let mut stop_deadline = Instant::now() + Duration::from_secs(86_400 * 365);

    loop {
        tokio::select! {
            _ = ticker.tick(), if !stopping => {
                if Instant::now() >= next_discovery {
                    refresh_targets(&ctx, &mut managed, &mut discovery_errors, &state_cache).await;
                    next_discovery = Instant::now() + ctx.cfg.daemon.discovery_interval.0.max(Duration::from_secs(10));
                }
                start_due_jobs(&ctx, &mut managed, &permits, &done_tx);
            }
            Some(result) = done_rx.recv() => {
                complete_job(&ctx, &mut managed, &mut state_cache, result).await;
            }
            _ = sigterm.recv() => {
                if stopping { break; }
                stopping = begin_stop(&ctx, &managed, &mut stop_deadline, "SIGTERM");
            }
            _ = sigint.recv() => {
                if stopping { break; }
                stopping = begin_stop(&ctx, &managed, &mut stop_deadline, "SIGINT");
            }
            _ = sighup.recv() => match Ctx::build(cli) {
                Ok(reloaded) => {
                    if reloaded.cfg.daemon.pid_file != ctx.cfg.daemon.pid_file || reloaded.cfg.monitoring.listen != ctx.cfg.monitoring.listen {
                        tracing::warn!("daemon.pid_file and monitoring.listen changes need a restart");
                    }
                    ctx = Arc::new(reloaded);
                    permits = Arc::new(Semaphore::new(ctx.cfg.backup.max_parallel));
                    next_discovery = Instant::now();
                    tracing::info!("configuration reloaded");
                }
                Err(e) => tracing::error!(error = %format!("{e:#}"), "reload failed; keeping the current configuration"),
            },
            _ = tokio::time::sleep_until(stop_deadline), if stopping => {
                let running: Vec<&str> = managed.iter().filter(|(_, m)| m.running).map(|(k, _)| k.as_str()).collect();
                tracing::warn!(running = running.join(", "), "shutdown timeout reached; backups continue inside their containers");
                break;
            }
        }
        publish(&shared, &ctx, &managed, &state_cache, started_at, stopping);
        if stopping && !managed.values().any(|m| m.running) {
            break;
        }
    }

    let _ = shutdown_tx.send(true);
    if let Some(task) = http_task {
        let _ = task.await;
    }
    tracing::info!("daemon stopped");
    Ok(Outcome::Success)
}

fn begin_stop(
    ctx: &Ctx,
    managed: &BTreeMap<String, Managed>,
    deadline: &mut Instant,
    signal: &str,
) -> bool {
    let running = managed.values().filter(|m| m.running).count();
    let grace = ctx.cfg.daemon.shutdown_timeout.0;
    *deadline = Instant::now() + grace;
    tracing::info!(signal, running, grace = %format_duration(grace), "stopping: no new backups; waiting for running ones (signal again to exit now)");
    true
}

/// Re-discovers containers, syncs config where it changed and refreshes history for new targets.
async fn refresh_targets(
    ctx: &Arc<Ctx>,
    managed: &mut BTreeMap<String, Managed>,
    last_errors: &mut BTreeSet<String>,
    state: &StateFile,
) {
    let mut discovery = docker::discover(&ctx.cfg, &ctx.overrides, &ctx.docker).await;
    apply_selectors(&ctx.selectors, &mut discovery.targets);
    for warning in &discovery.warnings {
        tracing::debug!("{warning}");
    }

    // Alert once per new discovery problem, not on every pass.
    let current: BTreeSet<String> = discovery.errors.iter().cloned().collect();
    let new_errors: Vec<String> = current.difference(last_errors).cloned().collect();
    for error in &new_errors {
        tracing::error!("{error}");
    }
    if !new_errors.is_empty() {
        let report = Report {
            host_id: ctx.host_id.clone(),
            trigger: "daemon discovery".into(),
            results: vec![],
            errors: new_errors,
        };
        let ctx = ctx.clone();
        tokio::spawn(async move { ctx.notifier.report(&report).await });
    }
    *last_errors = current;

    let present: BTreeSet<String> = discovery.targets.iter().map(Target::key).collect();
    managed.retain(|key, m| {
        let keep = present.contains(key) || m.running;
        if !keep {
            tracing::info!(target_key = %key, "container gone; no longer scheduled");
        }
        keep
    });

    let mut needs_history = Vec::new();
    for target in discovery.targets {
        let key = target.key();
        match managed.get_mut(&key) {
            Some(m) if m.target == target => {}
            Some(m) => {
                tracing::info!(target_key = %key, container = target.short_id(), "container changed");
                m.target = target;
                needs_history.push(key);
            }
            None => {
                tracing::info!(target_key = %key, stanza = %target.policy.stanza, "now scheduling target");
                let prev = state.targets.get(&key);
                let failures = prev.map_or(0, |p| p.consecutive_failures);
                managed.insert(
                    key.clone(),
                    Managed {
                        target,
                        history: None,
                        synced_hash: None,
                        last_sync_error: None,
                        running: false,
                        consecutive_failures: failures,
                        last_failure_at: if failures > 0 {
                            prev.and_then(|p| p.last_finished_at)
                        } else {
                            None
                        },
                    },
                );
                needs_history.push(key);
            }
        }
    }

    // Config sync wherever the rendered file differs from what was last pushed.
    let mut syncs = Vec::new();
    if ctx.cfg.daemon.auto_config_sync {
        for (key, m) in managed.iter() {
            if m.running {
                continue;
            }
            match pgbackrest::render_conf(&ctx.cfg, &m.target, &ctx.host_id) {
                Ok(conf) => {
                    let hash = hash_of(&conf);
                    if m.synced_hash != Some(hash) {
                        syncs.push((key.clone(), m.target.clone(), hash));
                    }
                }
                Err(e) => {
                    tracing::error!(target_key = %key, error = %format!("{e:#}"), "cannot render pgbackrest.conf")
                }
            }
        }
    }
    let synced = join_all(syncs.into_iter().map(|(key, target, hash)| async move {
        let result = sync::sync_target(ctx, &target).await;
        (key, hash, result)
    }))
    .await;
    for (key, hash, result) in synced {
        let Some(m) = managed.get_mut(&key) else {
            continue;
        };
        match result {
            Ok(outcome) => {
                m.synced_hash = Some(hash);
                m.last_sync_error = None;
                if !needs_history.contains(&key) {
                    needs_history.push(key.clone());
                }
                tracing::info!(target_key = %key, config = outcome.config, archiving = %outcome.archiving, "auto config sync");
            }
            Err(e) => {
                let error = format!("{e:#}");
                if m.last_sync_error.as_ref() != Some(&error) {
                    tracing::error!(target_key = %key, error = %error, "auto config sync failed");
                }
                m.last_sync_error = Some(error);
            }
        }
    }

    let histories = join_all(needs_history.into_iter().filter_map(|key| {
        let target = managed.get(&key)?.target.clone();
        Some(async move { (key, backup::query_info(ctx, &target).await) })
    }))
    .await;
    for (key, info) in histories {
        let Some(m) = managed.get_mut(&key) else {
            continue;
        };
        match info {
            Ok(info) => m.history = Some(info.history()),
            Err(e) => {
                tracing::warn!(target_key = %key, error = %format!("{e:#}"), "could not read backup history")
            }
        }
    }
}

fn hash_of(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn start_due_jobs(
    ctx: &Arc<Ctx>,
    managed: &mut BTreeMap<String, Managed>,
    permits: &Arc<Semaphore>,
    done: &mpsc::UnboundedSender<JobResult>,
) {
    let (now, now_utc) = (Local::now(), Utc::now());
    for (key, m) in managed.iter_mut() {
        if m.running || m.retry_at(ctx).is_some_and(|at| now_utc < at) {
            continue;
        }
        let Some(reason) = due_reason(&now, &m.target.policy, &m.history.unwrap_or_default())
        else {
            continue;
        };
        tracing::info!(target_key = %key, reason = reason.as_str(), "backup due");
        m.running = true;
        let (ctx, target, permits, done) =
            (ctx.clone(), m.target.clone(), permits.clone(), done.clone());
        tokio::spawn(async move {
            let _permit = permits.acquire_owned().await;
            let job =
                AssertUnwindSafe(backup::run_job(&ctx, &target, reason.as_str())).catch_unwind();
            let result = job.await.unwrap_or_else(|_| {
                failed_result(&target, reason.as_str(), "backup task panicked".into())
            });
            let _ = done.send(result);
        });
    }
}

async fn complete_job(
    ctx: &Arc<Ctx>,
    managed: &mut BTreeMap<String, Managed>,
    state_cache: &mut StateFile,
    result: JobResult,
) {
    if let Some(m) = managed.get_mut(&result.target) {
        m.running = false;
        match result.status {
            JobStatus::Failed => {
                m.consecutive_failures += 1;
                m.last_failure_at = Some(result.finished_at);
                if let Some(at) = m.retry_at(ctx) {
                    tracing::warn!(target_key = %result.target, failures = m.consecutive_failures, retry_at = %local_time(at), "will retry");
                }
            }
            JobStatus::Success | JobStatus::DryRun => {
                m.consecutive_failures = 0;
                m.last_failure_at = None;
                // Without a fresh `info` (or in a dry run) assume the backup landed as planned.
                m.history = Some(result.history.unwrap_or_else(|| {
                    let mut h = m.history.unwrap_or_default();
                    h.last_backup_start = Some(result.started_at);
                    if result.backup_type == Some(BackupType::Full) {
                        h.last_full_start = Some(result.started_at);
                    }
                    h
                }));
            }
        }
    }
    if !ctx.dry_run
        && let Some(state) = persist(ctx, std::slice::from_ref(&result)).await
    {
        *state_cache = state;
    }
    let report = Report {
        host_id: ctx.host_id.clone(),
        trigger: format!("scheduled backup ({})", result.reason),
        results: vec![result],
        errors: vec![],
    };
    let ctx = ctx.clone();
    tokio::spawn(async move { ctx.notifier.report(&report).await });
}

fn publish(
    shared: &Shared,
    ctx: &Ctx,
    managed: &BTreeMap<String, Managed>,
    state: &StateFile,
    started_at: DateTime<Utc>,
    stopping: bool,
) {
    let (now, now_utc) = (Local::now(), Utc::now());
    let targets = managed
        .iter()
        .map(|(key, m)| {
            let retry_after = m.retry_at(ctx).filter(|at| *at > now_utc);
            let due = next_due(&now, &m.target.policy, &m.history.unwrap_or_default());
            TargetView {
                target: key.clone(),
                stanza: m.target.policy.stanza.clone(),
                running: m.running,
                failing: m.consecutive_failures > 0,
                consecutive_failures: m.consecutive_failures,
                // A backoff pushes the next attempt out.
                next_due: if m.running {
                    None
                } else {
                    due.max(retry_after)
                },
                retry_after,
            }
        })
        .collect();
    let snapshot = Snapshot {
        host_id: ctx.host_id.clone(),
        started_at,
        stopping,
        state: state.clone(),
        in_progress: managed
            .iter()
            .filter(|(_, m)| m.running)
            .map(|(k, _)| k.clone())
            .collect(),
        targets,
    };
    *shared
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot;
}

/// `flock`ed PID file: held for the daemon's lifetime, so a stale file never looks alive.
pub struct PidLock {
    _file: File,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    Stopped,
    Running { pid: Option<u32> },
}

impl PidLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening pid file {}", path.display()))?;
        // SAFETY: valid fd; the lock lives as long as `file`.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                let pid = read_pid(path)
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".into());
                bail!(
                    "pgback daemon is already running (pid {pid}, {})",
                    path.display()
                );
            }
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        file.sync_all()?;
        Ok(Self {
            _file: file,
            path: path.to_path_buf(),
        })
    }

    pub fn probe(path: &Path) -> DaemonState {
        let Ok(file) = File::open(path) else {
            return DaemonState::Stopped;
        };
        // SAFETY: valid fd; a shared lock succeeds only if no daemon holds the exclusive one.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return DaemonState::Stopped;
        }
        DaemonState::Running {
            pid: read_pid(path),
        }
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    let mut s = String::new();
    File::open(path).ok()?.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

pub async fn start_all(cli: &Cli, ctx: &Ctx) -> Result<Outcome> {
    let daemon = &ctx.cfg.daemon;
    if let DaemonState::Running { pid } = PidLock::probe(&daemon.pid_file) {
        println!(
            "pgback daemon is already running (pid {})",
            pid.map_or("?".into(), |p| p.to_string())
        );
        return Ok(Outcome::Success);
    }
    let exe = std::env::current_exe().context("locating the pgback binary")?;
    let config = std::path::absolute(&cli.config)?;
    let format = match cli.log_format.unwrap_or(LogFormat::Json) {
        LogFormat::Json => "json",
        LogFormat::Text => "text",
    };
    let mut args: Vec<String> = vec![
        "--config".into(),
        config.display().to_string(),
        "--log_format".into(),
        format.into(),
    ];
    if cli.debug_mode {
        args.push("--debug_mode".into());
    }
    if !cli.containers.is_empty() {
        args.extend(["--container".into(), cli.containers.join(",")]);
    }
    args.push("daemon".into());

    if ctx.dry_run {
        println!(
            "dry run: would start `{} {}` (logs to {})",
            exe.display(),
            args.join(" "),
            daemon.log_file.display()
        );
        return Ok(Outcome::Success);
    }

    if let Some(dir) = daemon
        .log_file
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&daemon.log_file)
        .with_context(|| format!("opening {}", daemon.log_file.display()))?;
    let mut command = std::process::Command::new(&exe);
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Flags travel as environment variables so secrets stay off the daemon's command line.
    command.envs(cli.notify.to_env());
    if let Some(ty) = cli.backup.force_backup_type {
        command.env("PGBACK_FORCE_BACKUP_TYPE", ty.as_str());
    }
    if let Some(iv) = cli.backup.incremental_backup_interval {
        command.env(
            "PGBACK_INCREMENTAL_BACKUP_INTERVAL",
            iv.as_secs().to_string(),
        );
    }
    // SAFETY: setsid is async-signal-safe; it detaches the daemon from our terminal session.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;

    for _ in 0..100 {
        if let Some(status) = child.try_wait()? {
            bail!(
                "daemon exited during startup ({status}); see {}:\n{}",
                daemon.log_file.display(),
                log_tail(&daemon.log_file, 10)
            );
        }
        if let DaemonState::Running { pid: Some(pid) } = PidLock::probe(&daemon.pid_file)
            && pid == child.id()
        {
            println!(
                "pgback daemon started (pid {pid}); logs: {}",
                daemon.log_file.display()
            );
            return Ok(Outcome::Success);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!(
        "pgback daemon launched (pid {}) but has not taken {} yet; check {}",
        child.id(),
        daemon.pid_file.display(),
        daemon.log_file.display()
    );
    Ok(Outcome::Failed)
}

fn log_tail(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

pub async fn stop_all(ctx: &Ctx, wait: Option<Duration>, immediate: bool) -> Result<Outcome> {
    let pid_file = &ctx.cfg.daemon.pid_file;
    let pid = match PidLock::probe(pid_file) {
        DaemonState::Stopped => {
            println!("pgback daemon is not running");
            return Ok(Outcome::Success);
        }
        DaemonState::Running { pid: None } => bail!(
            "a daemon holds {} but its pid is unreadable",
            pid_file.display()
        ),
        DaemonState::Running { pid: Some(pid) } => pid,
    };
    if ctx.dry_run {
        println!("dry run: would send SIGTERM to the pgback daemon (pid {pid})");
        return Ok(Outcome::Success);
    }
    let signal = |pid: u32| {
        // SAFETY: plain syscall on a pid we read from our own pid file.
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            let err = std::io::Error::last_os_error();
            return Err(anyhow::Error::new(err).context(format!("signalling pid {pid}")));
        }
        Ok(())
    };
    signal(pid)?;
    if immediate {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = signal(pid);
    }
    let limit = wait.unwrap_or(ctx.cfg.daemon.shutdown_timeout.0 + Duration::from_secs(15));
    println!(
        "sent SIGTERM to pgback daemon (pid {pid}); {}",
        if immediate {
            "exiting now".to_string()
        } else {
            format!(
                "waiting up to {} for running backups",
                format_duration(limit)
            )
        }
    );
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if PidLock::probe(pid_file) == DaemonState::Stopped {
            println!("pgback daemon stopped");
            return Ok(Outcome::Success);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    println!(
        "pgback daemon (pid {pid}) is still running after {}",
        format_duration(limit)
    );
    Ok(Outcome::Failed)
}

#[derive(Serialize)]
struct StatusView {
    target: String,
    stanza: String,
    last_status: Option<JobStatus>,
    last_type: Option<BackupType>,
    last_finished_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    consecutive_failures: u32,
    last_full_at: Option<DateTime<Utc>>,
    last_backup_at: Option<DateTime<Utc>>,
    next_due: Option<DateTime<Utc>>,
    retry_after: Option<DateTime<Utc>>,
}

pub async fn status_all(ctx: &Ctx, format: OutputFormat) -> Result<Outcome> {
    let daemon = PidLock::probe(&ctx.cfg.daemon.pid_file);
    let state = state::load(&ctx.cfg.monitoring.metrics_file).unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), "state file unreadable");
        StateFile::default()
    });
    let mut discovery = docker::discover(&ctx.cfg, &ctx.overrides, &ctx.docker).await;
    apply_selectors(&ctx.selectors, &mut discovery.targets);
    let infos = join_all(discovery.targets.iter().map(|t| backup::query_info(ctx, t))).await;

    let (now, now_utc) = (Local::now(), Utc::now());
    let mut views = Vec::new();
    for (t, info) in discovery.targets.iter().zip(infos) {
        let key = t.key();
        let prev = state.targets.get(&key);
        let history = info.as_ref().map(|i| i.history()).unwrap_or_default();
        let retry = prev
            .and_then(|p| retry_after(&ctx.cfg.backup, p.consecutive_failures, p.last_finished_at?))
            .filter(|at| *at > now_utc);
        views.push(StatusView {
            target: key,
            stanza: t.policy.stanza.clone(),
            last_status: prev.and_then(|p| p.last_status),
            last_type: prev.and_then(|p| p.last_type),
            last_finished_at: prev.and_then(|p| p.last_finished_at),
            last_error: prev
                .and_then(|p| p.last_error.clone())
                .or_else(|| info.as_ref().err().map(|e| format!("{e:#}"))),
            consecutive_failures: prev.map_or(0, |p| p.consecutive_failures),
            last_full_at: history.last_full_start,
            last_backup_at: history.last_backup_start,
            next_due: retry.or_else(|| next_due(&now, &t.policy, &history)),
            retry_after: retry,
        });
    }
    let failing = discovery
        .targets
        .iter()
        .any(|t| state.targets.get(&t.key()).is_some_and(|p| p.is_failing()))
        || !discovery.errors.is_empty();

    if format == OutputFormat::Json {
        let (running, pid) = match daemon {
            DaemonState::Running { pid } => (true, pid),
            DaemonState::Stopped => (false, None),
        };
        let body = serde_json::json!({
            "daemon": { "running": running, "pid": pid, "pid_file": ctx.cfg.daemon.pid_file },
            "host": ctx.host_id,
            "state_updated_at": state.updated_at,
            "targets": views,
            "errors": discovery.errors,
        });
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else {
        match daemon {
            DaemonState::Running { pid } => {
                let listen = ctx
                    .cfg
                    .monitoring
                    .listen
                    .as_deref()
                    .map(|l| format!("; http://{l}/status"))
                    .unwrap_or_default();
                println!(
                    "daemon: running (pid {}){listen}",
                    pid.map_or("?".into(), |p| p.to_string())
                );
            }
            DaemonState::Stopped => {
                println!("daemon: not running (start it with `pgback start_all` or systemd)")
            }
        }
        if let Some(at) = state.updated_at {
            println!(
                "metrics: {} (updated {})",
                ctx.cfg.monitoring.metrics_file.display(),
                relative(at, now_utc)
            );
        }
        println!();
        let when = |t: Option<DateTime<Utc>>| {
            t.map(|t| relative(t, now_utc))
                .unwrap_or_else(|| "-".into())
        };
        let rows: Vec<Vec<String>> = views
            .iter()
            .map(|v| {
                let last = match (v.last_status, v.last_finished_at) {
                    (Some(JobStatus::Failed), Some(at)) => format!(
                        "FAILED {} ({}x)",
                        relative(at, now_utc),
                        v.consecutive_failures
                    ),
                    (Some(status), Some(at)) => {
                        format!("{} {}", status.as_str(), relative(at, now_utc))
                    }
                    _ => "-".into(),
                };
                let next = match (v.retry_after, v.next_due) {
                    (Some(at), _) => format!("retry {}", relative(at, now_utc)),
                    (None, Some(at)) if at <= now_utc + TimeDelta::seconds(1) => "now".into(),
                    (None, at) => when(at),
                };
                vec![
                    v.target.clone(),
                    v.stanza.clone(),
                    last,
                    when(v.last_full_at),
                    when(v.last_backup_at),
                    next,
                ]
            })
            .collect();
        if rows.is_empty() {
            println!("no opted-in containers found");
        } else {
            table::print(
                &[
                    "TARGET",
                    "STANZA",
                    "LAST RUN",
                    "LAST FULL",
                    "LAST BACKUP",
                    "NEXT",
                ],
                &rows,
            );
        }
        for v in views
            .iter()
            .filter(|v| v.last_status == Some(JobStatus::Failed))
        {
            if let Some(e) = &v.last_error {
                println!("\n{}: {}", v.target, e.lines().next().unwrap_or_default());
            }
        }
        for e in &discovery.errors {
            eprintln!("error: {e}");
        }
    }

    Ok(match daemon {
        DaemonState::Stopped => Outcome::NotRunning,
        DaemonState::Running { .. } if failing => Outcome::Failed,
        DaemonState::Running { .. } => Outcome::Success,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_lock_is_exclusive_and_released() {
        let path =
            std::env::temp_dir().join(format!("pgback-pid-{}/pgback.pid", std::process::id()));
        assert_eq!(PidLock::probe(&path), DaemonState::Stopped);
        let lock = PidLock::acquire(&path).unwrap();
        assert_eq!(
            PidLock::probe(&path),
            DaemonState::Running {
                pid: Some(std::process::id())
            }
        );
        let err = PidLock::acquire(&path).err().unwrap().to_string();
        assert!(err.contains("already running"), "{err}");
        drop(lock);
        assert_eq!(PidLock::probe(&path), DaemonState::Stopped);
        assert!(!path.exists());
        // A stale file left behind by a crash is not mistaken for a live daemon.
        std::fs::write(&path, "999999\n").unwrap();
        assert_eq!(PidLock::probe(&path), DaemonState::Stopped);
        drop(PidLock::acquire(&path).unwrap());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}

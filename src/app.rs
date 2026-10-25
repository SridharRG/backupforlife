//! Shared execution context and command dispatch.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{Local, Utc};
use futures_util::future::join_all;
use serde::Serialize;

use crate::backup::{self, JobResult, JobStatus, Report, due_reason, retry_after};
use crate::cli::{Cli, Command, OutputFormat};
use crate::config::{Config, DEFAULT_TEMPLATE, EmailConfig, HooksConfig, Secret, WebhookConfig};
use crate::docker::{self, Discovery, DockerPool};
use crate::duration::format_duration;
use crate::hooks::{self, Phase};
use crate::notify::{Dispatcher, Notification, Severity};
use crate::table::{self, human_bytes, local_time};
use crate::target::{Overrides, Target};
use crate::{daemon, state, sync};

/// Everything a command needs, built once from the CLI and the config file.
pub struct Ctx {
    pub cfg: Config,
    pub host_id: String,
    pub dry_run: bool,
    pub overrides: Overrides,
    pub selectors: Vec<String>,
    pub docker: DockerPool,
    pub notifier: Dispatcher,
}

impl Ctx {
    pub fn build(cli: &Cli) -> Result<Self> {
        let mut cfg = Config::load(&cli.config)?;
        apply_cli_overrides(&mut cfg, cli);
        cfg.validate()?;
        let notifier = Dispatcher::from_config(&cfg.notifications, cli.dry_run)?;
        Ok(Self {
            host_id: cfg.host_id(),
            dry_run: cli.dry_run,
            overrides: Overrides {
                force_backup_type: cli.backup.force_backup_type,
                incremental_backup_interval: cli.backup.incremental_backup_interval,
            },
            selectors: cli.containers.clone(),
            docker: DockerPool::connect(&cfg),
            notifier,
            cfg,
        })
    }
}

fn apply_cli_overrides(cfg: &mut Config, cli: &Cli) {
    let n = &cli.notify;
    if let Some(url) = &n.slack_notification_url {
        cfg.notifications.slack = Some(WebhookConfig {
            webhook_url: Secret::new(url),
        });
    }
    if let Some(url) = &n.google_chat_notification_url {
        cfg.notifications.google_chat = Some(WebhookConfig {
            webhook_url: Secret::new(url),
        });
    }
    // Sender, recipients or method switch email on; keys alone only fill in an existing section,
    // so exporting PGBACK_EMAIL_SEND_* for the config does not conjure a half-configured channel.
    let enables_email =
        n.from_email.is_some() || !n.to_email.is_empty() || n.email_send_method.is_some();
    if enables_email {
        cfg.notifications
            .email
            .get_or_insert_with(EmailConfig::default);
    }
    if let Some(email) = cfg.notifications.email.as_mut() {
        if let Some(from) = &n.from_email {
            email.from = from.clone();
        }
        if !n.to_email.is_empty() {
            email.to = n.to_email.clone();
        }
        if let Some(method) = n.email_send_method {
            email.method = method;
        }
        if let Some(key) = &n.email_send_access_key {
            email.access_key = Some(Secret::new(key));
        }
        if let Some(key) = &n.email_send_secret_key {
            email.secret_key = Some(Secret::new(key));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Success,
    Failed,
    NotRunning,
}

impl From<Outcome> for ExitCode {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::Success => ExitCode::SUCCESS,
            Outcome::Failed => ExitCode::from(1),
            Outcome::NotRunning => ExitCode::from(3),
        }
    }
}

pub async fn run(cli: Cli) -> Result<Outcome> {
    let a = &cli.actions;
    if a.init_config {
        init_config(&cli.config, a.overwrite, cli.dry_run)?;
    }
    if a.create_pre_hook.is_some() || a.create_post_hook.is_some() {
        let dir = hooks_dir(&cli.config)?;
        for (phase, name) in [
            (Phase::Pre, &a.create_pre_hook),
            (Phase::Post, &a.create_post_hook),
        ] {
            if let Some(name) = name {
                let path = hooks::scaffold(&dir, phase, name, a.overwrite, cli.dry_run)?;
                if !cli.dry_run {
                    println!(
                        "created {} — edit it; it runs {} every backup",
                        path.display(),
                        if phase == Phase::Pre {
                            "before"
                        } else {
                            "after"
                        }
                    );
                }
            }
        }
    }
    if !a.config_sync && cli.command.is_none() {
        if !cli.has_action() {
            bail!("nothing to do: give an action flag or a subcommand (see --help)");
        }
        return Ok(Outcome::Success);
    }
    if let Some(Command::InstallService { path }) = &cli.command {
        return install_service(&cli, path.as_deref());
    }

    let ctx = Arc::new(Ctx::build(&cli)?);
    let mut outcome = Outcome::Success;
    if a.config_sync {
        outcome = outcome.max(sync::config_sync(&ctx).await?);
    }
    let Some(command) = &cli.command else {
        return Ok(outcome);
    };
    let sub = match command {
        Command::Discover { format } => discover(&ctx, *format).await?,
        Command::Run { if_due } => run_backups(&ctx, *if_due).await?,
        Command::History { format } => history(&ctx, *format).await?,
        Command::Check => sync::check(&ctx).await?,
        Command::TestNotify => test_notify(&ctx).await?,
        Command::Daemon => daemon::run_foreground(&cli, ctx.clone()).await?,
        Command::StartAll => daemon::start_all(&cli, &ctx).await?,
        Command::StopAll { wait, immediate } => daemon::stop_all(&ctx, *wait, *immediate).await?,
        Command::StatusAll { format } => daemon::status_all(&ctx, *format).await?,
        Command::InstallService { .. } => unreachable!("handled above"),
    };
    Ok(outcome.max(sub))
}

/// The hooks directory, from the config if there is one (hook scaffolding works before init).
fn hooks_dir(config: &Path) -> Result<PathBuf> {
    if !config.exists() {
        return Ok(HooksConfig::default().dir);
    }
    Ok(Config::load(config)?.hooks.dir)
}

fn init_config(path: &Path, overwrite: bool, dry_run: bool) -> Result<()> {
    if dry_run {
        print!("{DEFAULT_TEMPLATE}");
        eprintln!("dry run: would write {} (mode 0600)", path.display());
        return Ok(());
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).mode(0o600);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options.open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => anyhow::anyhow!(
            "{} already exists (pass --overwrite to replace it)",
            path.display()
        ),
        _ => anyhow::Error::new(e).context(format!("creating {}", path.display())),
    })?;
    file.write_all(DEFAULT_TEMPLATE.as_bytes())?;
    println!(
        "wrote {} — review the repos and notifications sections, then run `pgback discover`",
        path.display()
    );
    Ok(())
}

/// Discovers targets and narrows them to `--container` selectors, failing on unknown selectors.
pub async fn discover_targets(ctx: &Ctx) -> Result<Discovery> {
    let mut discovery = docker::discover(&ctx.cfg, &ctx.overrides, &ctx.docker).await;
    for warning in &discovery.warnings {
        tracing::warn!("{warning}");
    }
    let unmatched = apply_selectors(&ctx.selectors, &mut discovery.targets);
    if !unmatched.is_empty() {
        for error in &discovery.errors {
            tracing::error!("{error}");
        }
        bail!("no opted-in container matches {}", unmatched.join(", "));
    }
    Ok(discovery)
}

/// Keeps targets matching any selector; returns selectors that matched nothing.
pub fn apply_selectors(selectors: &[String], targets: &mut Vec<Target>) -> Vec<String> {
    if selectors.is_empty() {
        return Vec::new();
    }
    let unmatched = selectors
        .iter()
        .filter(|s| !targets.iter().any(|t| t.matches(s)))
        .cloned()
        .collect();
    targets.retain(|t| selectors.iter().any(|s| t.matches(s)));
    unmatched
}

/// Logs discovery errors; returns whether there were any.
pub fn report_discovery_errors(discovery: &Discovery) -> bool {
    for error in &discovery.errors {
        tracing::error!("{error}");
    }
    !discovery.errors.is_empty()
}

#[derive(Serialize)]
struct DiscoveredView<'a> {
    target: String,
    endpoint: &'a str,
    container_id: &'a str,
    container_name: &'a str,
    image: &'a str,
    stanza: &'a str,
    pg_path: &'a str,
    pg_port: u16,
    pg_user: &'a str,
    full_schedule: &'a str,
    incremental_interval_secs: Option<u64>,
    pgbackrest_version: Option<String>,
}

async fn discover(ctx: &Ctx, format: OutputFormat) -> Result<Outcome> {
    let discovery = discover_targets(ctx).await?;
    let failed = report_discovery_errors(&discovery);
    let versions = join_all(
        discovery
            .targets
            .iter()
            .map(|t| sync::probe_version(ctx, t)),
    )
    .await;
    let views: Vec<DiscoveredView<'_>> = discovery
        .targets
        .iter()
        .zip(versions)
        .map(|(t, version)| DiscoveredView {
            target: t.key(),
            endpoint: &t.endpoint,
            container_id: &t.id,
            container_name: &t.name,
            image: &t.image,
            stanza: &t.policy.stanza,
            pg_path: &t.policy.pg_path,
            pg_port: t.policy.pg_port,
            pg_user: &t.policy.pg_user,
            full_schedule: t.policy.schedule.as_str(),
            incremental_interval_secs: t.policy.incremental_interval.map(|d| d.as_secs()),
            pgbackrest_version: version,
        })
        .collect();

    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&views)?),
        OutputFormat::Table if views.is_empty() => {
            println!(
                "no running containers labelled {}.enable=true",
                ctx.cfg.docker.label_prefix
            );
        }
        OutputFormat::Table => {
            let rows: Vec<Vec<String>> = discovery
                .targets
                .iter()
                .zip(&views)
                .map(|(t, v)| {
                    vec![
                        v.target.clone(),
                        t.short_id().to_string(),
                        t.image.clone(),
                        t.policy.stanza.clone(),
                        t.policy.pg_path.clone(),
                        t.policy.schedule.to_string(),
                        t.policy
                            .incremental_interval
                            .map(format_duration)
                            .unwrap_or_else(|| "off".into()),
                        v.pgbackrest_version
                            .clone()
                            .unwrap_or_else(|| "MISSING".into()),
                    ]
                })
                .collect();
            table::print(
                &[
                    "TARGET",
                    "CONTAINER",
                    "IMAGE",
                    "STANZA",
                    "PGDATA",
                    "FULL SCHEDULE",
                    "INCR EVERY",
                    "PGBACKREST",
                ],
                &rows,
            );
        }
    }
    let missing = views.iter().any(|v| v.pgbackrest_version.is_none());
    if missing {
        tracing::warn!(
            "pgbackrest is missing from some containers; install it in the PostgreSQL image"
        );
    }
    Ok(if failed || missing {
        Outcome::Failed
    } else {
        Outcome::Success
    })
}

async fn run_backups(ctx: &Ctx, if_due: bool) -> Result<Outcome> {
    let discovery = discover_targets(ctx).await?;
    report_discovery_errors(&discovery);

    let jobs: Vec<(Target, String)> = if if_due {
        select_due(ctx, discovery.targets).await
    } else {
        discovery
            .targets
            .into_iter()
            .map(|t| (t, "manual".to_string()))
            .collect()
    };
    if jobs.is_empty() && discovery.errors.is_empty() {
        println!(
            "nothing to back up{}",
            if if_due { " (no target is due)" } else { "" }
        );
        return Ok(Outcome::Success);
    }

    let results = backup::run_many(ctx, jobs).await;
    print_results(&results);
    let trigger = if if_due {
        "scheduled run (--if_due)"
    } else {
        "manual run"
    };
    Ok(finish(ctx, results, discovery.errors, trigger).await)
}

/// The targets a daemon tick would back up now, honouring failure backoff from the state file.
async fn select_due(ctx: &Ctx, targets: Vec<Target>) -> Vec<(Target, String)> {
    let state = state::load(&ctx.cfg.monitoring.metrics_file).unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), "ignoring unreadable state file");
        Default::default()
    });
    let infos = join_all(targets.iter().map(|t| backup::query_info(ctx, t))).await;
    let (now, now_utc) = (Local::now(), Utc::now());
    let mut due = Vec::new();
    for (target, info) in targets.into_iter().zip(infos) {
        let key = target.key();
        if let Some(prev) = state.targets.get(&key)
            && let Some(at) = prev.last_finished_at
            && let Some(until) = retry_after(&ctx.cfg.backup, prev.consecutive_failures, at)
            && now_utc < until
        {
            tracing::info!(target_key = %key, retry_at = %local_time(until), "skipping: backing off after failures");
            continue;
        }
        // Unknown history (e.g. no stanza yet) counts as "no backups": the job reports the real error.
        let history = match &info {
            Ok(info) => info.history(),
            Err(e) => {
                tracing::debug!(target_key = %key, error = %format!("{e:#}"), "history unavailable");
                Default::default()
            }
        };
        match due_reason(&now, &target.policy, &history) {
            Some(reason) => due.push((target, reason.as_str().to_string())),
            None => tracing::info!(target_key = %key, "not due"),
        }
    }
    due
}

fn print_results(results: &[JobResult]) {
    if results.is_empty() {
        return;
    }
    let rows: Vec<Vec<String>> = results
        .iter()
        .map(|r| {
            vec![
                r.target.clone(),
                r.backup_type
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "-".into()),
                r.status.as_str().to_string(),
                format_duration(r.duration()),
                r.label.clone().unwrap_or_else(|| "-".into()),
                r.size_bytes.map(human_bytes).unwrap_or_else(|| "-".into()),
                r.error
                    .as_deref()
                    .and_then(|e| e.lines().next())
                    .unwrap_or_default()
                    .to_string(),
            ]
        })
        .collect();
    table::print(
        &[
            "TARGET", "TYPE", "STATUS", "DURATION", "LABEL", "SIZE", "ERROR",
        ],
        &rows,
    );
}

/// Records results, refreshes metrics outputs and notifies. Returns the run outcome.
pub async fn finish(
    ctx: &Ctx,
    results: Vec<JobResult>,
    errors: Vec<String>,
    trigger: &str,
) -> Outcome {
    if !ctx.dry_run && !results.is_empty() {
        persist(ctx, &results).await;
    }
    let report = Report {
        host_id: ctx.host_id.clone(),
        trigger: trigger.to_string(),
        results,
        errors,
    };
    ctx.notifier.report(&report).await;
    if report.failures() > 0 {
        Outcome::Failed
    } else {
        Outcome::Success
    }
}

/// Writes the state file and the Prometheus textfile (if configured). Errors are logged.
pub async fn persist(ctx: &Ctx, results: &[JobResult]) -> Option<state::StateFile> {
    let metrics = ctx.cfg.monitoring.metrics_file.clone();
    let textfile = ctx.cfg.monitoring.prometheus_textfile.clone();
    let host = ctx.host_id.clone();
    let results = results.to_vec();
    let outcome = tokio::task::spawn_blocking(move || -> Result<state::StateFile> {
        let state = state::record(&metrics, &host, &results)?;
        if let Some(path) = textfile {
            state::write_atomic(&path, state::prometheus(&state, None).as_bytes())?;
        }
        Ok(state)
    })
    .await;
    match outcome {
        Ok(Ok(state)) => Some(state),
        Ok(Err(e)) => {
            tracing::error!(error = %format!("{e:#}"), "could not write metrics");
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "metrics writer panicked");
            None
        }
    }
}

#[derive(Serialize)]
struct HistoryView {
    target: String,
    stanza: String,
    status_code: Option<i64>,
    status: String,
    backups: Vec<crate::pgbackrest::BackupRecord>,
}

async fn history(ctx: &Ctx, format: OutputFormat) -> Result<Outcome> {
    let discovery = discover_targets(ctx).await?;
    let mut failed = report_discovery_errors(&discovery);
    let infos = join_all(discovery.targets.iter().map(|t| backup::query_info(ctx, t))).await;
    let mut views = Vec::new();
    for (t, info) in discovery.targets.iter().zip(infos) {
        let view = match info {
            Ok(info) => HistoryView {
                target: t.key(),
                stanza: info.name.clone(),
                status_code: Some(info.status_code),
                status: info.status_message.clone(),
                backups: info.backups,
            },
            Err(e) => {
                failed = true;
                HistoryView {
                    target: t.key(),
                    stanza: t.policy.stanza.clone(),
                    status_code: None,
                    status: format!("{e:#}"),
                    backups: vec![],
                }
            }
        };
        views.push(view);
    }

    if format == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&views)?);
        return Ok(if failed {
            Outcome::Failed
        } else {
            Outcome::Success
        });
    }
    let mut rows = Vec::new();
    for v in &views {
        if v.backups.is_empty() {
            rows.push(vec![
                v.target.clone(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                "-".into(),
                v.status.clone(),
            ]);
        }
        for b in v.backups.iter().rev() {
            let duration = (b.finished_at - b.started_at).to_std().unwrap_or_default();
            rows.push(vec![
                v.target.clone(),
                b.label.clone(),
                b.kind.to_string(),
                local_time(b.started_at),
                format_duration(duration),
                human_bytes(b.size_bytes),
                human_bytes(b.repo_delta_bytes),
                if b.error {
                    "checksum errors".into()
                } else {
                    String::new()
                },
            ]);
        }
    }
    table::print(
        &[
            "TARGET", "LABEL", "TYPE", "STARTED", "DURATION", "SIZE", "STORED", "NOTE",
        ],
        &rows,
    );
    Ok(if failed {
        Outcome::Failed
    } else {
        Outcome::Success
    })
}

async fn test_notify(ctx: &Ctx) -> Result<Outcome> {
    let channels = ctx.notifier.channel_names();
    if channels.is_empty() {
        bail!(
            "no notification channels configured; set notifications.* in the config or pass \
             --slack_notification_url / --google_chat_notification_url / --from_email + --to_email"
        );
    }
    let notification = Notification {
        severity: Severity::Info,
        title: format!("pgback test notification from {}", ctx.host_id),
        lines: vec![
            "Alerts from pgback reach this channel.".into(),
            format!(
                "_pgback {} · channels: {}_",
                env!("CARGO_PKG_VERSION"),
                channels.join(", ")
            ),
        ],
        details: vec![],
    };
    let mut failed = false;
    for (channel, result) in ctx.notifier.send_all(&notification).await {
        match result {
            Ok(()) if ctx.dry_run => println!("{channel}: dry run, not sent"),
            Ok(()) => println!("{channel}: sent"),
            Err(e) => {
                failed = true;
                println!("{channel}: FAILED — {e:#}");
            }
        }
    }
    Ok(if failed {
        Outcome::Failed
    } else {
        Outcome::Success
    })
}

fn install_service(cli: &Cli, path: Option<&Path>) -> Result<Outcome> {
    let exe = std::env::current_exe().context("locating the pgback binary")?;
    let config = std::path::absolute(&cli.config)?;
    let unit = format!(
        "[Unit]\n\
         Description=pgback - pgBackRest orchestration for Dockerised PostgreSQL\n\
         Documentation=https://crates.io/crates/pgback\n\
         After=docker.service network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         # Secrets referenced as ${{VAR}} in the config, and PGBACK_* flag variables.\n\
         EnvironmentFile=-/etc/pgback/pgback.env\n\
         ExecStart={exe} --config {config} --log_format json daemon\n\
         ExecReload=/bin/kill -HUP $MAINPID\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         # Leave room for daemon.shutdown_timeout so running backups can be recorded.\n\
         TimeoutStopSec=6min\n\
         KillMode=mixed\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        exe = exe.display(),
        config = config.display(),
    );
    match path {
        None => print!("{unit}"),
        Some(p) if cli.dry_run => {
            print!("{unit}");
            eprintln!("dry run: would write {}", p.display());
        }
        Some(p) => {
            std::fs::write(p, &unit).with_context(|| format!("writing {}", p.display()))?;
            println!(
                "wrote {}\nnext: systemctl daemon-reload && systemctl enable --now pgback",
                p.display()
            );
        }
    }
    Ok(Outcome::Success)
}

/// Used by the daemon for jobs that never produced a result (e.g. a panic).
pub fn failed_result(target: &Target, reason: &str, error: String) -> JobResult {
    let now = Utc::now();
    JobResult {
        target: target.key(),
        endpoint: target.endpoint.clone(),
        container_id: target.id.clone(),
        stanza: target.policy.stanza.clone(),
        reason: reason.to_string(),
        backup_type: None,
        status: JobStatus::Failed,
        started_at: now,
        finished_at: now,
        label: None,
        size_bytes: None,
        repo_size_bytes: None,
        error: Some(error),
        warnings: vec![],
        log_tail: vec![],
        history: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn email_keys_alone_do_not_enable_email() {
        let cli = Cli::try_parse_from(["pgback", "--email_send_secret_key", "s", "run"]).unwrap();
        let mut cfg = Config::default();
        apply_cli_overrides(&mut cfg, &cli);
        assert!(cfg.notifications.email.is_none());

        let cli = Cli::try_parse_from([
            "pgback",
            "--from_email",
            "a@x.io",
            "--to_email",
            "b@x.io",
            "--email_send_secret_key",
            "s",
            "run",
        ])
        .unwrap();
        apply_cli_overrides(&mut cfg, &cli);
        let email = cfg.notifications.email.as_ref().unwrap();
        assert_eq!((email.from.as_str(), email.to.len()), ("a@x.io", 1));
        assert_eq!(email.secret_key.as_ref().unwrap().expose(), "s");
    }

    #[test]
    fn selectors_filter_and_report_unmatched() {
        let cfg = Config::default();
        let mut targets = vec![
            crate::target::tests::sample_target(&cfg, "a", &[]),
            crate::target::tests::sample_target(&cfg, "b", &[]),
        ];
        let unmatched = apply_selectors(&["local/b".into(), "zzz".into()], &mut targets);
        assert_eq!(unmatched, ["zzz"]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "b");
    }
}

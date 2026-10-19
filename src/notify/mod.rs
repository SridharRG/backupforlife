//! Multi-channel alerting. Each channel implements [`Notifier`]; the [`Dispatcher`]
//! applies `notify_on` and fans a message out to every configured channel.

mod email;
mod webhook;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::future::join_all;

use crate::backup::{JobStatus, Report};
use crate::config::{NotificationsConfig, NotifyOn};
use crate::duration::format_duration;
use crate::table::human_bytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Success,
    Failure,
    Info,
}

impl Severity {
    fn emoji(self) -> &'static str {
        match self {
            Self::Success => "✅",
            Self::Failure => "🚨",
            Self::Info => "ℹ️",
        }
    }
}

/// A channel-neutral message; each channel renders it in its own format.
#[derive(Debug, Clone)]
pub struct Notification {
    pub severity: Severity,
    pub title: String,
    pub lines: Vec<String>,
    /// `(heading, preformatted text)` blocks, e.g. pgBackRest output of a failure.
    pub details: Vec<(String, String)>,
}

#[async_trait]
pub trait Notifier: Send + Sync {
    fn name(&self) -> &'static str;
    async fn send(&self, notification: &Notification) -> Result<()>;
}

pub struct Dispatcher {
    channels: Vec<Box<dyn Notifier>>,
    notify_on: Vec<NotifyOn>,
    dry_run: bool,
}

impl Dispatcher {
    pub fn from_config(cfg: &NotificationsConfig, dry_run: bool) -> Result<Self> {
        let mut channels: Vec<Box<dyn Notifier>> = Vec::new();
        if cfg.slack.is_some() || cfg.google_chat.is_some() {
            let http = http_client()?;
            if let Some(slack) = &cfg.slack {
                channels.push(Box::new(webhook::Webhook::slack(
                    http.clone(),
                    slack.webhook_url.clone(),
                )));
            }
            if let Some(chat) = &cfg.google_chat {
                channels.push(Box::new(webhook::Webhook::google_chat(
                    http,
                    chat.webhook_url.clone(),
                )));
            }
        }
        if let Some(email) = &cfg.email {
            channels.push(Box::new(email::Email::new(email)?));
        }
        Ok(Self {
            channels,
            notify_on: cfg.notify_on.clone(),
            dry_run,
        })
    }

    pub fn channel_names(&self) -> Vec<&'static str> {
        self.channels.iter().map(|c| c.name()).collect()
    }

    fn wants(&self, report: &Report) -> bool {
        if report.results.is_empty() && report.errors.is_empty() {
            return false;
        }
        let event = if report.failures() > 0 {
            NotifyOn::Failure
        } else {
            NotifyOn::Success
        };
        self.notify_on.contains(&event)
    }

    /// Sends a report summary if `notify_on` asks for it. Delivery failures are logged.
    pub async fn report(&self, report: &Report) {
        if self.channels.is_empty() || !self.wants(report) {
            return;
        }
        let notification = Notification::from_report(report);
        for (channel, result) in self.send_all(&notification).await {
            match result {
                Ok(()) => tracing::info!(channel, "notification sent"),
                Err(e) => {
                    tracing::error!(channel, error = %format!("{e:#}"), "notification failed")
                }
            }
        }
    }

    /// Sends to every channel concurrently, returning each channel's outcome.
    pub async fn send_all(&self, notification: &Notification) -> Vec<(&'static str, Result<()>)> {
        if self.dry_run {
            return self
                .channels
                .iter()
                .map(|c| {
                    tracing::info!(channel = c.name(), title = %notification.title, "dry run: would send notification");
                    (c.name(), Ok(()))
                })
                .collect();
        }
        join_all(
            self.channels
                .iter()
                .map(|c| async move { (c.name(), c.send(notification).await) }),
        )
        .await
    }
}

impl Notification {
    pub fn from_report(report: &Report) -> Self {
        let host = &report.host_id;
        let total = report.results.len();
        let failed_jobs = report.results.iter().filter(|r| r.failed()).count();
        let errors = report.errors.len();
        let backups = match (total, failed_jobs) {
            (0, _) => None,
            (n, 0) => Some(format!("{n} backup{} succeeded", plural(n))),
            (n, f) => Some(format!("{f} of {n} backup{} failed", plural(n))),
        };
        let errors_part = (errors > 0).then(|| format!("{errors} error{}", plural(errors)));
        let summary: Vec<String> = [backups, errors_part].into_iter().flatten().collect();
        let severity = if report.failures() == 0 {
            Severity::Success
        } else {
            Severity::Failure
        };
        let title = format!("pgback on {host}: {}", summary.join(", "));

        let mut lines = Vec::new();
        let mut details = Vec::new();
        for r in &report.results {
            let kind = r.backup_type.map(|t| t.as_str()).unwrap_or("backup");
            let took = format_duration(r.duration());
            match r.status {
                JobStatus::Failed => {
                    let error = r.error.as_deref().unwrap_or("unknown error");
                    let first = error.lines().next().unwrap_or_default();
                    lines.push(format!(
                        "❌ {} — {kind} failed after {took}: {first}",
                        r.target
                    ));
                    if !r.log_tail.is_empty() {
                        let skip = r.log_tail.len().saturating_sub(15);
                        details.push((
                            format!("{} — pgBackRest output", r.target),
                            r.log_tail[skip..].join("\n"),
                        ));
                    } else if error.lines().count() > 1 {
                        details.push((r.target.clone(), error.to_string()));
                    }
                }
                JobStatus::Success | JobStatus::DryRun => {
                    let label = r
                        .label
                        .as_deref()
                        .map(|l| format!(" {l}"))
                        .unwrap_or_default();
                    let size = r
                        .size_bytes
                        .map(|s| format!(", {}", human_bytes(s)))
                        .unwrap_or_default();
                    lines.push(format!("✅ {} — {kind}{label} in {took}{size}", r.target));
                }
            }
            for warning in &r.warnings {
                lines.push(format!("⚠️ {}: {warning}", r.target));
            }
        }
        for error in &report.errors {
            lines.push(format!("⚠️ {error}"));
        }
        lines.push(format!(
            "_{} · pgback {}_",
            report.trigger,
            env!("CARGO_PKG_VERSION")
        ));
        Self {
            severity,
            title,
            lines,
            details,
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Slack mrkdwn and Google Chat share `*bold*`, `_italic_` and ``` blocks.
pub fn render_chat(n: &Notification, limit: usize) -> String {
    let mut out = format!("*{} {}*\n", n.severity.emoji(), n.title);
    for line in &n.lines {
        out.push_str(line);
        out.push('\n');
    }
    for (heading, body) in &n.details {
        out.push_str(&format!(
            "*{heading}*\n```\n{}\n```\n",
            body.replace("```", "'''")
        ));
    }
    truncate(&out, limit)
}

pub fn render_plain(n: &Notification) -> String {
    let mut out = format!("{}\n\n", n.title);
    for line in &n.lines {
        out.push_str(line.trim_matches('_'));
        out.push('\n');
    }
    for (heading, body) in &n.details {
        out.push_str(&format!("\n--- {heading} ---\n{body}\n"));
    }
    out
}

/// Truncates on a char boundary, marking the cut.
pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let marker = "\n…(truncated)";
    let keep = max_chars.saturating_sub(marker.chars().count());
    s.chars().take(keep).collect::<String>() + marker
}

/// HTTPS client with bundled Mozilla roots, so alerts work on hosts without a CA store.
fn http_client() -> Result<reqwest::Client> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("pgback/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::JobResult;
    use crate::pgbackrest::BackupType;
    use chrono::{TimeDelta, Utc};

    fn result(target: &str, status: JobStatus, error: Option<&str>, tail: &[&str]) -> JobResult {
        let start = Utc::now();
        JobResult {
            target: target.into(),
            endpoint: "local".into(),
            container_id: "abc".into(),
            stanza: "db".into(),
            reason: "manual".into(),
            backup_type: Some(BackupType::Full),
            status,
            started_at: start,
            finished_at: start + TimeDelta::seconds(252),
            label: (status == JobStatus::Success).then(|| "20261019-020000F".into()),
            size_bytes: (status == JobStatus::Success).then_some(31_457_280),
            repo_size_bytes: None,
            error: error.map(Into::into),
            warnings: vec![],
            log_tail: tail.iter().map(|s| s.to_string()).collect(),
            history: None,
        }
    }

    fn report(results: Vec<JobResult>, errors: Vec<String>) -> Report {
        Report {
            host_id: "host-1".into(),
            trigger: "manual run".into(),
            results,
            errors,
        }
    }

    #[test]
    fn summarises_failures_with_output() {
        let r = report(
            vec![
                result("local/a", JobStatus::Success, None, &[]),
                result(
                    "local/b",
                    JobStatus::Failed,
                    Some("pgbackrest backup exited with code 82"),
                    &["P00 ERROR: [082]: WAL not archived"],
                ),
            ],
            vec![],
        );
        let n = Notification::from_report(&r);
        assert_eq!(n.severity, Severity::Failure);
        assert_eq!(n.title, "pgback on host-1: 1 of 2 backups failed");
        let chat = render_chat(&n, 10_000);
        assert!(
            chat.contains("✅ local/a — full 20261019-020000F in 4m12s, 30.0 MiB"),
            "{chat}"
        );
        assert!(
            chat.contains(
                "❌ local/b — full failed after 4m12s: pgbackrest backup exited with code 82"
            ),
            "{chat}"
        );
        assert!(
            chat.contains("```\nP00 ERROR: [082]: WAL not archived\n```"),
            "{chat}"
        );
        let plain = render_plain(&n);
        assert!(
            plain.starts_with("pgback on host-1: 1 of 2 backups failed\n")
                && !plain.contains("```")
        );
    }

    #[test]
    fn endpoint_errors_alone_are_failures() {
        let n = Notification::from_report(&report(
            vec![],
            vec!["docker endpoint `x`: connection refused".into()],
        ));
        assert_eq!(n.severity, Severity::Failure);
        assert_eq!(n.title, "pgback on host-1: 1 error");

        let mixed = report(
            vec![result("local/a", JobStatus::Success, None, &[])],
            vec!["endpoint down".into()],
        );
        let n = Notification::from_report(&mixed);
        assert_eq!(n.severity, Severity::Failure);
        assert_eq!(n.title, "pgback on host-1: 1 backup succeeded, 1 error");
    }

    #[test]
    fn notify_on_filters_reports() {
        let d = |on: Vec<NotifyOn>| Dispatcher {
            channels: vec![],
            notify_on: on,
            dry_run: false,
        };
        let ok = report(vec![result("a", JobStatus::Success, None, &[])], vec![]);
        let bad = report(vec![result("a", JobStatus::Failed, Some("x"), &[])], vec![]);
        assert!(!d(vec![NotifyOn::Failure]).wants(&ok));
        assert!(d(vec![NotifyOn::Failure]).wants(&bad));
        assert!(d(vec![NotifyOn::Success]).wants(&ok));
        assert!(!d(vec![NotifyOn::Failure, NotifyOn::Success]).wants(&report(vec![], vec![])));
    }

    #[test]
    fn truncation_is_char_safe() {
        let s = "é".repeat(100);
        let t = truncate(&s, 20);
        assert_eq!(t.chars().count(), 20);
        assert!(t.ends_with("(truncated)"));
        assert_eq!(truncate("short", 20), "short");
    }
}

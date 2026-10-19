//! Lifecycle hooks: executables in `<dir>/pre-backup.d/` and `<dir>/post-backup.d/`,
//! run on the host in lexical order (like run-parts) around every backup.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use crate::config::is_simple_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Pre,
    Post,
}

impl Phase {
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Pre => "pre-backup.d",
            Self::Post => "post-backup.d",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pre => "pre",
            Self::Post => "post",
        }
    }
}

const TEMPLATE: &str = r#"#!/bin/sh
# pgback __PHASE__-backup hook: __NAME__
#
# Runs on the Docker host __WHEN__ each backup, once per target, in lexical order
# with the other hooks in this directory. __EXIT__
# Hooks are killed after `hooks.timeout` (default 10m).
#
# Environment:
#   PGBACK_PHASE            pre | post
#   PGBACK_HOST_ID          host_id from the config (defaults to the hostname)
#   PGBACK_TARGET           endpoint/container, e.g. local/db_prod
#   PGBACK_ENDPOINT         Docker endpoint name
#   PGBACK_CONTAINER_ID     full container ID (usable with `docker exec`)
#   PGBACK_CONTAINER_NAME   container name
#   PGBACK_STANZA           pgBackRest stanza
#   PGBACK_BACKUP_TYPE      full | incr | diff
#   PGBACK_DRY_RUN          1 when pgback runs with --dry_run (hooks are then skipped)
# post-backup only:
#   PGBACK_STATUS           success | failure
#   PGBACK_ERROR            failure reason (empty on success)
#   PGBACK_DURATION_SECS    wall-clock seconds the backup took
#   PGBACK_BACKUP_LABEL     pgBackRest label of the new backup (empty on failure)
set -eu

echo "__PHASE__-backup hook __NAME__: ${PGBACK_TARGET} (${PGBACK_BACKUP_TYPE})"
"#;

/// Creates an executable hook script from a commented template.
pub fn scaffold(
    dir: &Path,
    phase: Phase,
    name: &str,
    overwrite: bool,
    dry_run: bool,
) -> Result<PathBuf> {
    if !is_simple_name(name) || name.starts_with('.') {
        bail!(
            "hook name `{name}` may only contain letters, digits, `_`, `-` and `.` (e.g. 10-notify-maintenance)"
        );
    }
    let phase_dir = dir.join(phase.dir_name());
    let path = phase_dir.join(name);
    let (when, exit) = match phase {
        Phase::Pre => ("before", "A non-zero exit aborts that target's backup."),
        Phase::Post => (
            "after",
            "Post hooks always run, even when the backup failed.",
        ),
    };
    let body = TEMPLATE
        .replace("__PHASE__", phase.as_str())
        .replace("__NAME__", name)
        .replace("__WHEN__", when)
        .replace("__EXIT__", exit);

    if dry_run {
        println!("# would write {} (mode 0750):\n{body}", path.display());
        return Ok(path);
    }
    std::fs::create_dir_all(&phase_dir)
        .with_context(|| format!("creating {}", phase_dir.display()))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).mode(0o750);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options.open(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => anyhow::anyhow!(
            "{} already exists (pass --overwrite to replace it)",
            path.display()
        ),
        _ => anyhow::Error::new(e).context(format!("creating {}", path.display())),
    })?;
    std::io::Write::write_all(&mut file, body.as_bytes())?;
    // `mode` above only applies to new files.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750))?;
    Ok(path)
}

/// Executable hook files for a phase, sorted. A missing directory means no hooks.
pub fn list(dir: &Path, phase: Phase) -> Result<Vec<PathBuf>> {
    let phase_dir = dir.join(phase.dir_name());
    let entries = match std::fs::read_dir(&phase_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", phase_dir.display())),
    };
    let mut hooks = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.starts_with('.')
            || name.ends_with('~')
            || name.ends_with(".sample")
            || name.ends_with(".disabled")
        {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if meta.permissions().mode() & 0o111 == 0 {
            tracing::warn!(hook = %path.display(), "skipping hook that is not executable (chmod +x to enable)");
            continue;
        }
        hooks.push(path);
    }
    hooks.sort();
    Ok(hooks)
}

#[derive(Debug)]
pub struct HookFailure {
    pub hook: PathBuf,
    pub reason: String,
}

impl std::fmt::Display for HookFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self
            .hook
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?");
        write!(f, "hook `{name}` {}", self.reason)
    }
}

/// Runs a phase's hooks. Pre hooks stop at the first failure; post hooks all run.
pub async fn run(
    dir: &Path,
    phase: Phase,
    timeout: Duration,
    env: &[(String, String)],
    dry_run: bool,
    target: &str,
) -> Vec<HookFailure> {
    let hooks = match list(dir, phase) {
        Ok(hooks) => hooks,
        Err(e) => {
            return vec![HookFailure {
                hook: dir.join(phase.dir_name()),
                reason: format!("could not be listed: {e:#}"),
            }];
        }
    };
    let mut failures = Vec::new();
    for hook in hooks {
        if dry_run {
            tracing::info!(target_key = %target, hook = %hook.display(), "dry run: would run {}-backup hook", phase.as_str());
            continue;
        }
        tracing::info!(target_key = %target, hook = %hook.display(), "running {}-backup hook", phase.as_str());
        if let Err(reason) = run_one(&hook, timeout, env, target).await {
            failures.push(HookFailure { hook, reason });
            if phase == Phase::Pre {
                break;
            }
        }
    }
    failures
}

async fn run_one(
    hook: &Path,
    timeout: Duration,
    env: &[(String, String)],
    target: &str,
) -> Result<(), String> {
    let mut child = Command::new(hook)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group so a timeout can kill everything the hook started.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to start: {e}"))?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let hook_name = hook
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();

    let run = async {
        let (_, last_err, status) = tokio::join!(
            relay(stdout, &hook_name, target, false),
            relay(stderr, &hook_name, target, true),
            child.wait()
        );
        (last_err, status)
    };
    match tokio::time::timeout(timeout, run).await {
        Ok((_, Ok(status))) if status.success() => Ok(()),
        Ok((last_err, Ok(status))) => {
            let detail = last_err.map(|l| format!(": {l}")).unwrap_or_default();
            let how = match status.code() {
                Some(code) => format!("exited with code {code}"),
                None => "was killed by a signal".to_string(),
            };
            Err(format!("{how}{detail}"))
        }
        Ok((_, Err(e))) => Err(format!("could not be waited on: {e}")),
        Err(_) => {
            if let Some(pid) = pid {
                // SAFETY: plain syscall; a negative pid addresses the hook's process group.
                unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            }
            Err(format!(
                "timed out after {}",
                crate::duration::format_duration(timeout)
            ))
        }
    }
}

/// Logs a hook's output line by line; returns the last stderr line for error messages.
async fn relay(
    stream: impl AsyncRead + Unpin,
    hook: &str,
    target: &str,
    is_stderr: bool,
) -> Option<String> {
    let mut lines = BufReader::new(stream).lines();
    let mut last = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if is_stderr {
            tracing::warn!(target_key = %target, hook, "{line}");
            if !line.trim().is_empty() {
                last = Some(line);
            }
        } else {
            tracing::info!(target_key = %target, hook, "{line}");
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pgback-hooks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_hook(dir: &Path, phase: Phase, name: &str, body: &str, mode: u32) {
        let d = dir.join(phase.dir_name());
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn scaffold_creates_executable_and_refuses_overwrite() {
        let dir = tempdir("scaffold");
        let path = scaffold(&dir, Phase::Pre, "10-maintenance", false, false).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o750
        );
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("#!/bin/sh") && body.contains("aborts that target's backup"));
        assert!(
            scaffold(&dir, Phase::Pre, "10-maintenance", false, false)
                .unwrap_err()
                .to_string()
                .contains("--overwrite")
        );
        scaffold(&dir, Phase::Pre, "10-maintenance", true, false).unwrap();
        assert!(scaffold(&dir, Phase::Post, "../escape", false, false).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn list_skips_disabled_and_non_executable() {
        let dir = tempdir("list");
        write_hook(&dir, Phase::Pre, "20-b", "true", 0o755);
        write_hook(&dir, Phase::Pre, "10-a", "true", 0o755);
        write_hook(&dir, Phase::Pre, "30-c.disabled", "true", 0o755);
        write_hook(&dir, Phase::Pre, "40-d", "true", 0o644);
        let names: Vec<_> = list(&dir, Phase::Pre)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["10-a", "20-b"]);
        assert!(list(&dir, Phase::Post).unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn pre_hooks_stop_at_first_failure_post_hooks_continue() {
        let dir = tempdir("run");
        let marker = dir.join("ran");
        write_hook(&dir, Phase::Pre, "10-fail", "echo boom >&2; exit 3", 0o755);
        write_hook(
            &dir,
            Phase::Pre,
            "20-never",
            &format!("touch {}", marker.display()),
            0o755,
        );
        write_hook(&dir, Phase::Post, "10-fail", "exit 1", 0o755);
        write_hook(
            &dir,
            Phase::Post,
            "20-env",
            &format!("echo \"$PGBACK_STATUS\" > {}", marker.display()),
            0o755,
        );
        let env = vec![("PGBACK_STATUS".to_string(), "failure".to_string())];

        let failures = run(&dir, Phase::Pre, Duration::from_secs(10), &env, false, "t").await;
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].to_string().contains("10-fail") && failures[0].reason.contains("boom"),
            "{}",
            failures[0]
        );
        assert!(!marker.exists());

        let failures = run(&dir, Phase::Post, Duration::from_secs(10), &env, false, "t").await;
        assert_eq!(failures.len(), 1);
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "failure");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn hooks_time_out() {
        let dir = tempdir("timeout");
        write_hook(&dir, Phase::Pre, "10-slow", "sleep 30", 0o755);
        let started = std::time::Instant::now();
        let failures = run(
            &dir,
            Phase::Pre,
            Duration::from_millis(300),
            &[],
            false,
            "t",
        )
        .await;
        assert!(failures[0].reason.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn dry_run_executes_nothing() {
        let dir = tempdir("dry");
        let marker = dir.join("ran");
        write_hook(
            &dir,
            Phase::Pre,
            "10-touch",
            &format!("touch {}", marker.display()),
            0o755,
        );
        assert!(
            run(&dir, Phase::Pre, Duration::from_secs(5), &[], true, "t")
                .await
                .is_empty()
        );
        assert!(!marker.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

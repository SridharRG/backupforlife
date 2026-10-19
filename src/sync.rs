//! `--config_sync` (push pgbackrest.conf, create stanzas, optionally set
//! archive_command) and `check` (verify pgBackRest and WAL archiving per target).

use anyhow::{Context, Result, bail};
use bollard::Docker;
use futures_util::StreamExt;

use crate::app::{Ctx, Outcome, discover_targets, report_discovery_errors};
use crate::backup::stanza_create;
use crate::docker::{exec_capture, upload_file};
use crate::pgbackrest::{self, archive_command};
use crate::table;
use crate::target::Target;

/// Concurrent per-target operations for sync and check.
const CONCURRENCY: usize = 8;

pub struct SyncOutcome {
    pub config: &'static str,
    pub stanza: &'static str,
    pub archiving: String,
}

pub async fn config_sync(ctx: &Ctx) -> Result<Outcome> {
    let discovery = discover_targets(ctx).await?;
    let mut failed = report_discovery_errors(&discovery);
    if discovery.targets.is_empty() {
        println!("config sync: no opted-in containers found");
        return Ok(if failed {
            Outcome::Failed
        } else {
            Outcome::Success
        });
    }
    let outcomes: Vec<_> = futures_util::stream::iter(&discovery.targets)
        .map(|t| async move { (t, sync_target(ctx, t).await) })
        .buffered(CONCURRENCY)
        .collect()
        .await;

    let mut rows = Vec::new();
    for (t, outcome) in outcomes {
        match outcome {
            Ok(o) => rows.push(vec![t.key(), o.config.into(), o.stanza.into(), o.archiving]),
            Err(e) => {
                failed = true;
                tracing::error!(target_key = %t.key(), error = %format!("{e:#}"), "config sync failed");
                rows.push(vec![
                    t.key(),
                    "FAILED".into(),
                    "-".into(),
                    first_line(&format!("{e:#}")),
                ]);
            }
        }
    }
    if !ctx.dry_run {
        table::print(&["TARGET", "CONFIG", "STANZA", "ARCHIVING"], &rows);
    }
    Ok(if failed {
        Outcome::Failed
    } else {
        Outcome::Success
    })
}

/// Pushes the rendered config, configures archiving if managed, and creates the stanza.
pub async fn sync_target(ctx: &Ctx, t: &Target) -> Result<SyncOutcome> {
    let conf = pgbackrest::render_conf(&ctx.cfg, t, &ctx.host_id)?;
    let path = &ctx.cfg.pgbackrest.config_path;
    if ctx.dry_run {
        println!("# ---- {} (container {}) -> {path}", t.key(), t.short_id());
        print!("{}", pgbackrest::redact(&conf));
        println!(
            "# then: stanza-create --stanza={}{}\n",
            t.policy.stanza,
            if ctx.cfg.pgbackrest.manage_archive_command {
                "; ALTER SYSTEM SET archive_command"
            } else {
                ""
            }
        );
        return Ok(SyncOutcome {
            config: "dry-run",
            stanza: "dry-run",
            archiving: "-".into(),
        });
    }

    let docker = ctx.docker.client(&t.endpoint)?;
    let config = if push_conf(docker, ctx, t, &conf).await? {
        "written"
    } else {
        "unchanged"
    };
    let mut changes = Vec::new();
    if ctx.cfg.pgbackrest.manage_archive_command {
        changes = configure_archiving(docker, ctx, t).await?;
    }
    stanza_create(ctx, t).await?;
    let mut archiving = match archive_settings(docker, ctx, t).await {
        Ok(a) => archiving_status(&a, &ctx.cfg, &t.policy.stanza),
        Err(e) => format!("unknown ({})", first_line(&format!("{e:#}"))),
    };
    if !changes.is_empty() {
        archiving = format!("{archiving} ({})", changes.join(", "));
    }
    tracing::info!(target_key = %t.key(), config, "config synced");
    Ok(SyncOutcome {
        config,
        stanza: "ok",
        archiving,
    })
}

/// Writes pgbackrest.conf atomically (temp file + rename), owned by the exec user, mode 0640.
/// Returns false when the container already has identical content.
async fn push_conf(docker: &Docker, ctx: &Ctx, t: &Target, conf: &str) -> Result<bool> {
    let path = ctx.cfg.pgbackrest.config_path.as_str();
    let (dir, file) = path
        .rsplit_once('/')
        .context("config_path must be absolute")?;
    let dir = if dir.is_empty() { "/" } else { dir };

    let current = exec_capture(docker, &t.id, "root", &["cat".into(), path.into()]).await?;
    if current.success() && current.stdout == conf {
        return Ok(false);
    }
    root_exec(docker, t, &["mkdir", "-p", dir]).await?;
    let tmp_name = format!(".{file}.pgback-tmp");
    upload_file(docker, &t.id, dir, &tmp_name, conf.as_bytes(), 0o600).await?;
    let tmp_path = format!("{}/{tmp_name}", dir.trim_end_matches('/'));
    let install =
        r#"set -e; g="$(id -gn "$1")"; chown "$1:$g" "$2"; chmod 0640 "$2"; mv -f "$2" "$3""#;
    root_exec(
        docker,
        t,
        &[
            "sh",
            "-c",
            install,
            "sh",
            &t.policy.exec_user,
            &tmp_path,
            path,
        ],
    )
    .await?;
    Ok(true)
}

async fn root_exec(docker: &Docker, t: &Target, cmd: &[&str]) -> Result<()> {
    let cmd: Vec<String> = cmd.iter().map(|s| s.to_string()).collect();
    let out = exec_capture(docker, &t.id, "root", &cmd).await?;
    if !out.success() {
        bail!(
            "`{}` failed in {} (exit {}): {}",
            cmd[0],
            t.key(),
            out.exit_code,
            out.tail(3)
        );
    }
    Ok(())
}

fn psql(t: &Target, ctx: &Ctx, statements: &[String]) -> Vec<String> {
    let p = &t.policy;
    let mut cmd: Vec<String> = [
        "psql",
        "-X",
        "-A",
        "-t",
        "-q",
        "-v",
        "ON_ERROR_STOP=1",
        "-d",
        "postgres",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    cmd.extend([
        "-p".into(),
        p.pg_port.to_string(),
        "-U".into(),
        p.pg_user.clone(),
    ]);
    if let Some(socket) = &ctx.cfg.pgbackrest.pg_socket_path {
        cmd.extend(["-h".into(), socket.clone()]);
    }
    for sql in statements {
        cmd.extend(["-c".into(), sql.clone()]);
    }
    cmd
}

/// WAL archiving settings of a running cluster.
struct Archiving {
    mode: String,
    /// `archive_mode` was changed (e.g. by ALTER SYSTEM) and waits for a restart.
    mode_pending_restart: bool,
    /// The configured archive_command. PostgreSQL shows `(disabled)` while
    /// archive_mode is off, so the value is then read from the config files.
    command: String,
}

async fn archive_settings(docker: &Docker, ctx: &Ctx, t: &Target) -> Result<Archiving> {
    let sql = "SELECT m.setting || chr(31) || m.pending_restart::text || chr(31) || \
               CASE WHEN current_setting('archive_command') <> '(disabled)' THEN current_setting('archive_command') \
               ELSE coalesce((SELECT f.setting FROM pg_file_settings f WHERE f.name = 'archive_command' \
                              AND f.error IS NULL ORDER BY f.seqno DESC LIMIT 1), '') END \
               FROM pg_settings m WHERE m.name = 'archive_mode'"
        .to_string();
    let out = exec_capture(docker, &t.id, &t.policy.exec_user, &psql(t, ctx, &[sql])).await?;
    if !out.success() {
        bail!("psql failed (exit {}): {}", out.exit_code, out.tail(2));
    }
    let mut fields = out.stdout.trim_end_matches('\n').splitn(3, '\u{1f}');
    match (fields.next(), fields.next(), fields.next()) {
        (Some(mode), Some(pending), Some(command)) => Ok(Archiving {
            mode: mode.to_string(),
            mode_pending_restart: pending == "true",
            command: command.to_string(),
        }),
        _ => bail!("unexpected psql output: {}", out.stdout.trim()),
    }
}

/// `ok`, or what stands between this cluster and working WAL archiving.
fn archiving_status(a: &Archiving, cfg: &crate::config::Config, stanza: &str) -> String {
    let mode = match (a.mode.as_str(), a.mode_pending_restart) {
        ("off", true) => Some("restart PostgreSQL to enable archive_mode".to_string()),
        ("off", false) => {
            Some("archive_mode=off (set it to on and restart PostgreSQL)".to_string())
        }
        _ => None,
    };
    let words: Vec<&str> = a.command.split_whitespace().collect();
    let command = if !(a.command.contains("pgbackrest") && words.contains(&"archive-push")) {
        Some(format!(
            "archive_command must be: {}",
            archive_command(cfg, stanza)
        ))
    } else if !words.contains(&format!("--stanza={stanza}").as_str()) {
        Some(format!(
            "archive_command uses another stanza; expected --stanza={stanza}"
        ))
    } else {
        None
    };
    let problems: Vec<String> = [mode, command].into_iter().flatten().collect();
    if problems.is_empty() {
        "ok".into()
    } else {
        problems.join("; ")
    }
}

/// Points archive_command at pgBackRest and turns archive_mode on (effective after a
/// restart). Returns the changes made.
async fn configure_archiving(docker: &Docker, ctx: &Ctx, t: &Target) -> Result<Vec<String>> {
    let current = archive_settings(docker, ctx, t).await?;
    let wanted = archive_command(&ctx.cfg, &t.policy.stanza);
    let mut statements = Vec::new();
    let mut changes = Vec::new();
    if current.command != wanted {
        statements.push(format!(
            "ALTER SYSTEM SET archive_command = '{}'",
            wanted.replace('\'', "''")
        ));
        changes.push("set archive_command".to_string());
    }
    if current.mode == "off" && !current.mode_pending_restart {
        statements.push("ALTER SYSTEM SET archive_mode = 'on'".into());
        changes.push("set archive_mode=on".to_string());
    }
    if statements.is_empty() {
        return Ok(changes);
    }
    statements.push("SELECT pg_reload_conf()".into());
    let out = exec_capture(
        docker,
        &t.id,
        &t.policy.exec_user,
        &psql(t, ctx, &statements),
    )
    .await?;
    if !out.success() {
        bail!(
            "configuring archiving failed (exit {}): {}",
            out.exit_code,
            out.tail(3)
        );
    }
    tracing::info!(target_key = %t.key(), changes = changes.join(", "), "archiving configured");
    Ok(changes)
}

/// `pgBackRest 2.57.0` → `2.57.0`; `None` if pgbackrest is missing from the container.
pub async fn probe_version(ctx: &Ctx, t: &Target) -> Option<String> {
    let docker = ctx.docker.client(&t.endpoint).ok()?;
    let out = exec_capture(
        docker,
        &t.id,
        &t.policy.exec_user,
        &["pgbackrest".into(), "version".into()],
    )
    .await
    .ok()?;
    out.success().then(|| {
        out.stdout
            .trim()
            .trim_start_matches("pgBackRest ")
            .to_string()
    })
}

pub async fn check(ctx: &Ctx) -> Result<Outcome> {
    let discovery = discover_targets(ctx).await?;
    let mut failed = report_discovery_errors(&discovery);
    if discovery.targets.is_empty() {
        println!("check: no opted-in containers found");
        return Ok(if failed {
            Outcome::Failed
        } else {
            Outcome::Success
        });
    }
    let rows: Vec<(Vec<String>, bool)> = futures_util::stream::iter(&discovery.targets)
        .map(|t| async move { check_target(ctx, t).await })
        .buffered(CONCURRENCY)
        .collect()
        .await;
    failed |= rows.iter().any(|(_, ok)| !ok);
    let rows: Vec<Vec<String>> = rows.into_iter().map(|(row, _)| row).collect();
    table::print(&["TARGET", "PGBACKREST", "ARCHIVING", "CHECK"], &rows);
    Ok(if failed {
        Outcome::Failed
    } else {
        Outcome::Success
    })
}

async fn check_target(ctx: &Ctx, t: &Target) -> (Vec<String>, bool) {
    let Some(version) = probe_version(ctx, t).await else {
        return (
            vec![
                t.key(),
                "missing".into(),
                "-".into(),
                "pgbackrest is not installed in the container".into(),
            ],
            false,
        );
    };
    let docker = match ctx.docker.client(&t.endpoint) {
        Ok(d) => d,
        Err(e) => return (vec![t.key(), version, "-".into(), format!("{e:#}")], false),
    };
    let archiving = match archive_settings(docker, ctx, t).await {
        Ok(a) => archiving_status(&a, &ctx.cfg, &t.policy.stanza),
        Err(e) => format!("unknown ({})", first_line(&format!("{e:#}"))),
    };
    let mut ok = archiving == "ok";
    let check = if ctx.dry_run {
        "skipped (dry run)".to_string()
    } else {
        let cmd = pgbackrest::command(
            &ctx.cfg,
            &t.policy.stanza,
            "check",
            &["--log-level-console=error"],
        );
        match exec_capture(docker, &t.id, &t.policy.exec_user, &cmd).await {
            Ok(out) if out.success() => "ok".into(),
            Ok(out) => {
                ok = false;
                first_line(&out.tail(1))
            }
            Err(e) => {
                ok = false;
                first_line(&format!("{e:#}"))
            }
        }
    };
    (vec![t.key(), version, archiving, check], ok)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn settings(mode: &str, pending: bool, command: &str) -> Archiving {
        Archiving {
            mode: mode.into(),
            mode_pending_restart: pending,
            command: command.into(),
        }
    }

    #[test]
    fn archiving_status_messages() {
        let cfg = Config::default();
        let good = archive_command(&cfg, "db");
        let status = |mode, pending, command: &str, stanza| {
            archiving_status(&settings(mode, pending, command), &cfg, stanza)
        };
        assert_eq!(status("on", false, &good, "db"), "ok");
        assert_eq!(status("always", false, &good, "db"), "ok");
        assert!(status("off", false, &good, "db").contains("archive_mode=off"));
        assert_eq!(
            status("off", true, &good, "db"),
            "restart PostgreSQL to enable archive_mode"
        );
        assert!(status("on", false, "", "db").contains("archive_command must be"));
        assert!(status("off", false, "(disabled)", "db").contains("; archive_command must be"));
        assert!(status("on", false, &good, "db2").contains("another stanza"));
        // `--stanza=db` must not match `--stanza=db2`.
        assert!(
            status("on", false, &archive_command(&cfg, "db2"), "db").contains("another stanza")
        );
    }
}

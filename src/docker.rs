//! Docker Engine API access over the socket (or TCP/TLS): discovery, exec and file upload.
//! No `docker` CLI is needed on the host.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::query_parameters::{ListContainersOptions, UploadToContainerOptions};
use bollard::{API_DEFAULT_VERSION, Docker};
use futures_util::StreamExt;
use futures_util::future::join_all;

use crate::config::{Config, EndpointConfig};
use crate::target::{ContainerFacts, Overrides, Target, label_enabled, resolve};

/// Limit for captured exec output (e.g. `pgbackrest info --output=json`).
const CAPTURE_LIMIT: usize = 64 << 20;
/// A streamed line longer than this is emitted in pieces.
const MAX_LINE: usize = 64 << 10;

struct Endpoint {
    name: String,
    url: String,
    client: Result<Docker, String>,
}

/// One client per configured Docker endpoint. A broken endpoint does not prevent
/// the others from working; its error surfaces during discovery.
pub struct DockerPool {
    endpoints: Vec<Endpoint>,
}

impl DockerPool {
    pub fn connect(cfg: &Config) -> Self {
        let timeout = cfg.docker.timeout_secs;
        let endpoints = cfg
            .docker
            .endpoints
            .iter()
            .map(|ep| Endpoint {
                name: ep.name.clone(),
                url: ep.url.clone(),
                client: connect_one(ep, timeout),
            })
            .collect();
        Self { endpoints }
    }

    pub fn client(&self, endpoint: &str) -> Result<&Docker> {
        let ep = self
            .endpoints
            .iter()
            .find(|ep| ep.name == endpoint)
            .ok_or_else(|| anyhow!("unknown docker endpoint `{endpoint}`"))?;
        ep.client
            .as_ref()
            .map_err(|e| anyhow!("docker endpoint `{}` ({}): {e}", ep.name, ep.url))
    }
}

fn connect_one(ep: &EndpointConfig, timeout: u64) -> Result<Docker, String> {
    let result = if ep.url.starts_with("unix://") {
        Docker::connect_with_unix(&ep.url, timeout, API_DEFAULT_VERSION)
    } else if let Some(tls) = &ep.tls {
        Docker::connect_with_ssl(
            &ep.url,
            &tls.key,
            &tls.cert,
            &tls.ca,
            timeout,
            API_DEFAULT_VERSION,
        )
    } else if ep.url.starts_with("https://") {
        return Err("https:// endpoints need `tls: { ca, cert, key }`".into());
    } else {
        Docker::connect_with_http(&ep.url, timeout, API_DEFAULT_VERSION)
    };
    result.map_err(|e| e.to_string())
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub targets: Vec<Target>,
    /// Problems that stop a container (or a whole endpoint) from being backed up.
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Finds running containers labelled `<prefix>.enable=true` on every endpoint.
pub async fn discover(cfg: &Config, overrides: &Overrides, pool: &DockerPool) -> Discovery {
    let results = join_all(
        pool.endpoints
            .iter()
            .map(|ep| discover_endpoint(cfg, overrides, ep)),
    )
    .await;
    let mut out = Discovery::default();
    for result in results {
        match result {
            Ok(found) => {
                out.targets.extend(found.targets);
                out.errors.extend(found.errors);
                out.warnings.extend(found.warnings);
            }
            Err(e) => out.errors.push(e),
        }
    }
    reject_stanza_collisions(cfg, &mut out);
    out.targets.sort_by_key(Target::key);
    out
}

async fn discover_endpoint(
    cfg: &Config,
    overrides: &Overrides,
    ep: &Endpoint,
) -> Result<Discovery, String> {
    let context =
        |e: &dyn std::fmt::Display| format!("docker endpoint `{}` ({}): {e}", ep.name, ep.url);
    let docker = ep.client.as_ref().map_err(|e| context(e))?;
    let enable_label = format!("{}.enable", cfg.docker.label_prefix);
    let filters = HashMap::from([
        ("label".to_string(), vec![enable_label.clone()]),
        ("status".to_string(), vec!["running".to_string()]),
    ]);
    let options = ListContainersOptions {
        all: false,
        filters: Some(filters),
        ..Default::default()
    };
    let summaries = docker
        .list_containers(Some(options))
        .await
        .map_err(|e| context(&error_chain(&e)))?;

    let candidates: Vec<_> = summaries
        .into_iter()
        .filter(|s| {
            s.labels
                .as_ref()
                .and_then(|l| l.get(&enable_label))
                .is_some_and(|v| label_enabled(v))
        })
        .collect();
    let inspected = join_all(candidates.iter().map(|s| async move {
        let id = s.id.clone().unwrap_or_default();
        docker.inspect_container(&id, None).await
    }))
    .await;

    let mut out = Discovery::default();
    for (summary, inspect) in candidates.iter().zip(inspected) {
        let id = summary.id.clone().unwrap_or_default();
        let name = summary
            .names
            .as_ref()
            .and_then(|names| names.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| id.chars().take(12).collect());
        let info = match inspect {
            Ok(info) => info,
            Err(e) => {
                out.errors.push(format!(
                    "{}/{name}: inspect failed: {}",
                    ep.name,
                    error_chain(&e)
                ));
                continue;
            }
        };
        let env = info.config.and_then(|c| c.env).unwrap_or_default();
        let labels = summary.labels.clone().unwrap_or_default();
        let facts = ContainerFacts {
            endpoint: &ep.name,
            id: &id,
            name: &name,
            image: summary.image.as_deref().unwrap_or_default(),
            labels: &labels,
            env: &env,
        };
        match resolve(cfg, overrides, &facts) {
            Ok((target, warnings)) => {
                out.targets.push(target);
                out.warnings.extend(warnings);
            }
            Err(e) => out.errors.push(format!("{}/{name}: {e}", ep.name)),
        }
    }
    Ok(out)
}

/// Two containers writing the same stanza into the same repository path would corrupt
/// each other's backups, so both are refused until one gets a distinct stanza label.
fn reject_stanza_collisions(cfg: &Config, discovery: &mut Discovery) {
    let per_endpoint = cfg
        .pgbackrest
        .repos
        .iter()
        .all(|r| r.path.as_deref().unwrap_or("").contains("{endpoint}"));
    let namespace = |t: &Target| {
        if per_endpoint {
            format!("{}/{}", t.endpoint, t.policy.stanza)
        } else {
            t.policy.stanza.clone()
        }
    };
    let mut owners: HashMap<String, Vec<String>> = HashMap::new();
    for t in &discovery.targets {
        owners.entry(namespace(t)).or_default().push(t.key());
    }
    let mut collisions: Vec<_> = owners
        .into_iter()
        .filter(|(_, keys)| keys.len() > 1)
        .collect();
    if collisions.is_empty() {
        return;
    }
    collisions.sort();
    for (stanza, keys) in &collisions {
        discovery.errors.push(format!(
            "stanza `{stanza}` is claimed by {}; give each a distinct `{}.stanza` label",
            keys.join(", "),
            cfg.docker.label_prefix
        ));
    }
    discovery
        .targets
        .retain(|t| !collisions.iter().any(|(stanza, _)| *stanza == namespace(t)));
}

/// An error with its `source()` chain; bollard's top-level messages hide the cause
/// (e.g. "client error (Connect)" without "connection refused").
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let msg = cause.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        source = cause.source();
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub struct Captured {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    /// The last `n` non-empty lines of stderr, else stdout, for error messages.
    pub fn tail(&self, n: usize) -> String {
        let source = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let lines: Vec<&str> = source.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }
}

/// Runs `cmd` in `container` as `user` and collects its output.
pub async fn exec_capture(
    docker: &Docker,
    container: &str,
    user: &str,
    cmd: &[String],
) -> Result<Captured> {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut overflow = false;
    let exit_code = exec_raw(docker, container, user, cmd, |kind, bytes| {
        let buf = match kind {
            StreamKind::Stdout => &mut stdout,
            StreamKind::Stderr => &mut stderr,
        };
        if buf.len() + bytes.len() > CAPTURE_LIMIT {
            overflow = true;
        } else {
            buf.extend_from_slice(bytes);
        }
    })
    .await?;
    if overflow {
        bail!(
            "output of `{}` exceeded {} MiB",
            cmd.join(" "),
            CAPTURE_LIMIT >> 20
        );
    }
    Ok(Captured {
        exit_code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Runs `cmd` and hands each output line to `on_line` as it arrives. Returns the exit code.
pub async fn exec_lines<F>(
    docker: &Docker,
    container: &str,
    user: &str,
    cmd: &[String],
    mut on_line: F,
) -> Result<i64>
where
    F: FnMut(StreamKind, &str) + Send,
{
    let (mut out_buf, mut err_buf) = (Vec::<u8>::new(), Vec::<u8>::new());
    let result = exec_raw(docker, container, user, cmd, |kind, bytes| {
        let buf = match kind {
            StreamKind::Stdout => &mut out_buf,
            StreamKind::Stderr => &mut err_buf,
        };
        buf.extend_from_slice(bytes);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            on_line(kind, String::from_utf8_lossy(&line).trim_end());
        }
        if buf.len() > MAX_LINE {
            on_line(kind, &String::from_utf8_lossy(buf));
            buf.clear();
        }
    })
    .await;
    for (kind, buf) in [(StreamKind::Stdout, out_buf), (StreamKind::Stderr, err_buf)] {
        if !buf.is_empty() {
            on_line(kind, String::from_utf8_lossy(&buf).trim_end());
        }
    }
    result
}

async fn exec_raw<F>(
    docker: &Docker,
    container: &str,
    user: &str,
    cmd: &[String],
    mut sink: F,
) -> Result<i64>
where
    F: FnMut(StreamKind, &[u8]) + Send,
{
    let what = || format!("exec `{}` in {container}", cmd.join(" "));
    let exec = docker
        .create_exec(
            container,
            CreateExecOptions::<String> {
                cmd: Some(cmd.to_vec()),
                user: Some(user.to_string()),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .with_context(what)?;

    match docker.start_exec(&exec.id, None).await.with_context(what)? {
        StartExecResults::Attached { mut output, .. } => {
            while let Some(chunk) = output.next().await {
                match chunk.with_context(what)? {
                    LogOutput::StdOut { message } | LogOutput::Console { message } => {
                        sink(StreamKind::Stdout, &message)
                    }
                    LogOutput::StdErr { message } => sink(StreamKind::Stderr, &message),
                    LogOutput::StdIn { .. } => {}
                }
            }
        }
        StartExecResults::Detached => bail!("{}: exec unexpectedly detached", what()),
    }

    // The stream closes as the process exits; the exit code can lag by a moment.
    for _ in 0..100 {
        let info = docker.inspect_exec(&exec.id).await.with_context(what)?;
        if info.running != Some(true) {
            return info
                .exit_code
                .ok_or_else(|| anyhow!("{}: no exit code reported", what()));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    bail!("{}: process still running after its output closed", what())
}

/// Writes one file into `dir` inside the container via the archive API (no shell needed).
/// The file is root-owned with `mode`; callers fix ownership afterwards if needed.
pub async fn upload_file(
    docker: &Docker,
    container: &str,
    dir: &str,
    file_name: &str,
    content: &[u8],
    mode: u32,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_path(file_name)?;
    header.set_size(content.len() as u64);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(chrono::Utc::now().timestamp().max(0) as u64);
    header.set_cksum();
    let mut archive = tar::Builder::new(Vec::new());
    archive.append(&header, content)?;
    let bytes = archive.into_inner()?;

    let options = UploadToContainerOptions {
        path: dir.to_string(),
        ..Default::default()
    };
    docker
        .upload_to_container(container, Some(options), bollard::body_full(bytes.into()))
        .await
        .with_context(|| format!("uploading {dir}/{file_name} to {container}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::tests::sample_target;

    #[test]
    fn colliding_stanzas_are_rejected() {
        let cfg = Config::default();
        let mut d = Discovery {
            targets: vec![
                sample_target(&cfg, "a", &[("pg.backup.stanza", "shared")]),
                sample_target(&cfg, "b", &[("pg.backup.stanza", "shared")]),
                sample_target(&cfg, "c", &[]),
            ],
            ..Default::default()
        };
        reject_stanza_collisions(&cfg, &mut d);
        assert_eq!(d.targets.len(), 1);
        assert_eq!(d.targets[0].name, "c");
        assert!(d.errors[0].contains("local/a, local/b"), "{:?}", d.errors);
    }

    #[test]
    fn captured_tail_prefers_stderr() {
        let c = Captured {
            exit_code: 1,
            stdout: "a\nb\n".into(),
            stderr: "x\n\ny\nz\n".into(),
        };
        assert_eq!(c.tail(2), "y\nz");
        let c = Captured {
            exit_code: 1,
            stdout: "a\nb\n".into(),
            stderr: " \n".into(),
        };
        assert_eq!(c.tail(5), "a\nb");
    }
}

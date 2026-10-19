//! A deliberately tiny HTTP/1.1 endpoint for the daemon: `/health` (liveness),
//! `/status` (503 while any target is failing) and `/metrics` (Prometheus).

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::state::{Live, StateFile, prometheus};

#[derive(Debug, Clone, Serialize)]
pub struct TargetView {
    pub target: String,
    pub stanza: String,
    pub running: bool,
    pub failing: bool,
    pub consecutive_failures: u32,
    pub next_due: Option<DateTime<Utc>>,
    pub retry_after: Option<DateTime<Utc>>,
}

/// What the daemon publishes for the endpoint; replaced wholesale on each tick.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub host_id: String,
    pub started_at: DateTime<Utc>,
    pub stopping: bool,
    pub state: StateFile,
    pub in_progress: BTreeSet<String>,
    pub targets: Vec<TargetView>,
}

pub type Shared = Arc<RwLock<Snapshot>>;

const MAX_REQUEST: usize = 8 << 10;

pub async fn bind(addr: &str) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding monitoring.listen {addr}"))
}

pub async fn serve(listener: TcpListener, shared: Shared, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle(stream, &shared).await {
                            tracing::debug!(error = %e, "http request failed");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "http accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = shutdown.changed() => break,
        }
    }
}

async fn handle(mut stream: TcpStream, shared: &Shared) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let read = tokio::time::timeout(Duration::from_secs(5), async {
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < MAX_REQUEST {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok::<_, std::io::Error>(())
    })
    .await;
    if !matches!(read, Ok(Ok(()))) {
        return Ok(());
    }

    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
    let (method, path) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or("/"),
    );
    let (status, content_type, body) = {
        let snapshot = shared
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        route(method, path, &snapshot)
    };
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Service Unavailable",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(body.as_bytes()).await?;
    }
    stream.shutdown().await
}

fn route(method: &str, path: &str, snap: &Snapshot) -> (u16, &'static str, String) {
    const JSON: &str = "application/json";
    if method != "GET" && method != "HEAD" {
        return (405, "text/plain", "method not allowed\n".into());
    }
    match path.split('?').next().unwrap_or_default() {
        "/health" | "/healthz" => {
            let body = serde_json::json!({
                "status": "ok",
                "host": snap.host_id,
                "version": env!("CARGO_PKG_VERSION"),
                "started_at": snap.started_at,
                "uptime_secs": (Utc::now() - snap.started_at).num_seconds(),
                "stopping": snap.stopping,
                "targets": snap.targets.len(),
                "running": snap.in_progress.len(),
            });
            (200, JSON, body.to_string())
        }
        "/status" => {
            let failing: Vec<&str> = snap
                .targets
                .iter()
                .filter(|t| t.failing)
                .map(|t| t.target.as_str())
                .collect();
            let body = serde_json::json!({
                "status": if failing.is_empty() { "ok" } else { "failing" },
                "host": snap.host_id,
                "failing": failing,
                "targets": snap.targets,
            });
            (
                if failing.is_empty() { 200 } else { 503 },
                JSON,
                body.to_string(),
            )
        }
        "/metrics" => {
            let live = Live {
                started_at: snap.started_at,
                in_progress: &snap.in_progress,
            };
            (
                200,
                "text/plain; version=0.0.4",
                prometheus(&snap.state, Some(&live)),
            )
        }
        "/" => (
            200,
            "text/plain",
            "pgback daemon: /health /status /metrics\n".into(),
        ),
        _ => (404, "text/plain", "not found\n".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(failing: bool) -> Snapshot {
        Snapshot {
            host_id: "h".into(),
            started_at: Utc::now(),
            stopping: false,
            state: StateFile::default(),
            in_progress: BTreeSet::new(),
            targets: vec![TargetView {
                target: "local/db".into(),
                stanza: "db".into(),
                running: false,
                failing,
                consecutive_failures: u32::from(failing),
                next_due: None,
                retry_after: None,
            }],
        }
    }

    #[test]
    fn routes() {
        assert_eq!(route("GET", "/health", &snapshot(true)).0, 200);
        assert_eq!(route("GET", "/status", &snapshot(false)).0, 200);
        let (code, _, body) = route("GET", "/status?verbose", &snapshot(true));
        assert_eq!(code, 503);
        assert!(body.contains("\"failing\":[\"local/db\"]"), "{body}");
        assert_eq!(route("GET", "/metrics", &snapshot(false)).0, 200);
        assert_eq!(route("GET", "/nope", &snapshot(false)).0, 404);
        assert_eq!(route("POST", "/health", &snapshot(false)).0, 405);
    }

    #[tokio::test]
    async fn serves_over_tcp() {
        let listener = bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared: Shared = Arc::new(RwLock::new(snapshot(true)));
        let (tx, rx) = watch::channel(false);
        let server = tokio::spawn(serve(listener, shared, rx));

        let mut conn = TcpStream::connect(addr).await.unwrap();
        conn.write_all(b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).await.unwrap();
        assert!(
            response.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "{response}"
        );
        assert!(response.ends_with("}"));

        tx.send(true).unwrap();
        server.await.unwrap();
    }
}

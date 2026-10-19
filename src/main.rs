//! pgback — fleet orchestration for pgBackRest across Dockerised PostgreSQL.
//!
//! pgBackRest does the data path (streaming, compression, encryption, repositories,
//! WAL archiving); pgback discovers containers by label, renders and pushes their
//! pgBackRest configuration, decides full vs. incremental, runs hooks, schedules,
//! alerts and exports metrics. It talks to Docker over its API, so the host needs
//! neither the `docker` CLI nor any PostgreSQL tooling.

mod app;
mod backup;
mod cli;
mod config;
mod cron;
mod daemon;
mod docker;
mod duration;
mod hooks;
mod http;
mod notify;
mod pgbackrest;
mod state;
mod sync;
mod table;
mod target;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, LogFormat};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = cli.log_format.unwrap_or_default();
    init_logging(cli.debug_mode, format);
    // One process-wide TLS provider for Docker-over-TLS, webhooks and SMTP.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("pgback: cannot start async runtime: {e}");
            return ExitCode::from(2);
        }
    };
    match runtime.block_on(app::run(cli)) {
        Ok(outcome) => outcome.into(),
        Err(e) => {
            match format {
                LogFormat::Json => tracing::error!(error = %format!("{e:#}"), "fatal"),
                LogFormat::Text => eprintln!("error: {e:#}"),
            }
            ExitCode::from(2)
        }
    }
}

/// Logs go to stderr so stdout stays clean for tables and `--format json`.
/// `PGBACK_LOG` (EnvFilter syntax) overrides the level, e.g. `PGBACK_LOG=pgback=trace`.
fn init_logging(debug: bool, format: LogFormat) {
    let default = if debug {
        "info,pgback=debug"
    } else {
        "warn,pgback=info"
    };
    let filter = EnvFilter::try_from_env("PGBACK_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match format {
        LogFormat::Json => builder
            .with_ansi(false)
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .init(),
        LogFormat::Text => builder.with_ansi(color).with_target(debug).init(),
    }
}

//! Command-line interface. Flags use the `snake_case` spelling of enterprise backup
//! wrappers (`--dry_run`); the kebab-case form (`--dry-run`) is accepted too.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::{DEFAULT_CONFIG_PATH, EmailMethod};
use crate::duration::parse_duration;
use crate::pgbackrest::BackupType;

#[derive(Parser)]
#[command(
    name = "pgback",
    version,
    about = "Fleet orchestration for pgBackRest across Dockerised PostgreSQL",
    long_about = "Discovers PostgreSQL containers by Docker label, keeps their pgBackRest configuration in sync, \
                  schedules full and incremental backups, runs pre/post hooks and alerts via Slack, Google Chat \
                  or email.\n\nExit codes: 0 success, 1 a backup/sync/check failed, 2 usage or configuration \
                  error, 3 daemon not running (status_all).",
    arg_required_else_help = true,
    max_term_width = 110
)]
pub struct Cli {
    /// Configuration file
    #[arg(short = 'c', long, global = true, env = "PGBACK_CONFIG", default_value = DEFAULT_CONFIG_PATH)]
    pub config: PathBuf,

    /// Show what would happen; change nothing (read-only inspection still runs)
    #[arg(long = "dry_run", visible_alias = "dry-run", global = true)]
    pub dry_run: bool,

    /// Verbose logging (debug level)
    #[arg(
        long = "debug_mode",
        visible_alias = "debug-mode",
        global = true,
        env = "PGBACK_DEBUG_MODE"
    )]
    pub debug_mode: bool,

    /// Log format on stderr [default: text; json for a daemon launched by start_all]
    #[arg(
        long = "log_format",
        visible_alias = "log-format",
        value_enum,
        global = true,
        env = "PGBACK_LOG_FORMAT"
    )]
    pub log_format: Option<LogFormat>,

    /// Limit to these containers (name, endpoint/name, stanza or ID prefix; comma-separated)
    #[arg(
        long = "container",
        global = true,
        value_delimiter = ',',
        value_name = "SELECTOR",
        env = "PGBACK_CONTAINER"
    )]
    pub containers: Vec<String>,

    #[command(flatten)]
    pub actions: ActionArgs,

    #[command(flatten)]
    pub backup: BackupArgs,

    #[command(flatten)]
    pub notify: NotifyArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Args, Debug, Default)]
#[command(next_help_heading = "Actions (run before any subcommand)")]
pub struct ActionArgs {
    /// Write a commented default config to --config (mode 0600)
    #[arg(long = "init_config", visible_alias = "init-config", global = true)]
    pub init_config: bool,

    /// Render pgbackrest.conf for every target, push it into the container and create stanzas
    #[arg(long = "config_sync", visible_alias = "config-sync", global = true)]
    pub config_sync: bool,

    /// Scaffold an executable pre-backup hook script, e.g. 10-pause-jobs
    #[arg(
        long = "create_pre_hook",
        visible_alias = "create-pre-hook",
        value_name = "NAME",
        global = true
    )]
    pub create_pre_hook: Option<String>,

    /// Scaffold an executable post-backup hook script, e.g. 90-resume-jobs
    #[arg(
        long = "create_post_hook",
        visible_alias = "create-post-hook",
        value_name = "NAME",
        global = true
    )]
    pub create_post_hook: Option<String>,

    /// Let --init_config and --create_*_hook replace existing files
    #[arg(long, global = true)]
    pub overwrite: bool,
}

#[derive(Args, Debug, Default, Clone)]
#[command(next_help_heading = "Backup overrides")]
pub struct BackupArgs {
    /// Force the backup type instead of the schedule-based choice
    #[arg(
        long = "force_backup_type",
        visible_alias = "force-backup-type",
        value_enum,
        global = true,
        env = "PGBACK_FORCE_BACKUP_TYPE"
    )]
    pub force_backup_type: Option<BackupType>,

    /// Incremental cadence between fulls, e.g. 6h; 0 disables (overrides labels and config)
    #[arg(
        long = "incremental_backup_interval",
        visible_alias = "incremental-backup-interval",
        value_name = "DURATION",
        value_parser = parse_duration,
        global = true,
        env = "PGBACK_INCREMENTAL_BACKUP_INTERVAL"
    )]
    pub incremental_backup_interval: Option<Duration>,
}

#[derive(Args, Debug, Default, Clone)]
#[command(next_help_heading = "Notification overrides")]
pub struct NotifyArgs {
    /// Slack incoming-webhook URL
    #[arg(
        long = "slack_notification_url",
        visible_alias = "slack-notification-url",
        value_name = "URL",
        global = true,
        env = "PGBACK_SLACK_NOTIFICATION_URL",
        hide_env_values = true
    )]
    pub slack_notification_url: Option<String>,

    /// Google Chat space webhook URL
    #[arg(
        long = "google_chat_notification_url",
        visible_alias = "google-chat-notification-url",
        value_name = "URL",
        global = true,
        env = "PGBACK_GOOGLE_CHAT_NOTIFICATION_URL",
        hide_env_values = true
    )]
    pub google_chat_notification_url: Option<String>,

    /// Sender address for email alerts
    #[arg(
        long = "from_email",
        visible_alias = "from-email",
        value_name = "ADDRESS",
        global = true,
        env = "PGBACK_FROM_EMAIL"
    )]
    pub from_email: Option<String>,

    /// Recipients for email alerts (comma-separated)
    #[arg(
        long = "to_email",
        visible_alias = "to-email",
        value_name = "ADDRESS",
        value_delimiter = ',',
        global = true,
        env = "PGBACK_TO_EMAIL"
    )]
    pub to_email: Vec<String>,

    /// How email is delivered
    #[arg(
        long = "email_send_method",
        visible_alias = "email-send-method",
        value_enum,
        global = true,
        env = "PGBACK_EMAIL_SEND_METHOD"
    )]
    pub email_send_method: Option<EmailMethod>,

    /// AWS access key ID for SES (the SES SMTP username)
    #[arg(
        long = "email_send_access_key",
        visible_alias = "email-send-access-key",
        value_name = "KEY",
        global = true,
        env = "PGBACK_EMAIL_SEND_ACCESS_KEY",
        hide_env_values = true
    )]
    pub email_send_access_key: Option<String>,

    /// AWS secret key for SES; prefer the environment variable over the flag
    #[arg(
        long = "email_send_secret_key",
        visible_alias = "email-send-secret-key",
        value_name = "KEY",
        global = true,
        env = "PGBACK_EMAIL_SEND_SECRET_KEY",
        hide_env_values = true
    )]
    pub email_send_secret_key: Option<String>,
}

impl NotifyArgs {
    /// Environment variables that reproduce these flags (used when spawning the daemon,
    /// so secrets never appear on its command line).
    pub fn to_env(&self) -> Vec<(&'static str, String)> {
        let mut env = Vec::new();
        let mut put = |name: &'static str, value: Option<&String>| {
            if let Some(v) = value {
                env.push((name, v.clone()));
            }
        };
        put(
            "PGBACK_SLACK_NOTIFICATION_URL",
            self.slack_notification_url.as_ref(),
        );
        put(
            "PGBACK_GOOGLE_CHAT_NOTIFICATION_URL",
            self.google_chat_notification_url.as_ref(),
        );
        put("PGBACK_FROM_EMAIL", self.from_email.as_ref());
        put(
            "PGBACK_EMAIL_SEND_ACCESS_KEY",
            self.email_send_access_key.as_ref(),
        );
        put(
            "PGBACK_EMAIL_SEND_SECRET_KEY",
            self.email_send_secret_key.as_ref(),
        );
        if !self.to_email.is_empty() {
            env.push(("PGBACK_TO_EMAIL", self.to_email.join(",")));
        }
        if let Some(method) = self.email_send_method {
            env.push((
                "PGBACK_EMAIL_SEND_METHOD",
                method
                    .to_possible_value()
                    .expect("no skipped variants")
                    .get_name()
                    .to_string(),
            ));
        }
        env
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// List opted-in PostgreSQL containers on every Docker endpoint
    Discover {
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Back up targets now (all, or those chosen with --container)
    Run {
        /// Only back up targets whose full schedule or incremental interval is due (for cron)
        #[arg(long = "if_due", visible_alias = "if-due")]
        if_due: bool,
    },
    /// Show pgBackRest backup history per target
    History {
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Verify pgBackRest, WAL archiving and repository access per target
    Check,
    /// Send a test message to every configured notification channel
    #[command(name = "test_notify", visible_alias = "test-notify")]
    TestNotify,
    /// Run the scheduler in the foreground (for systemd)
    Daemon,
    /// Start the scheduler daemon in the background
    #[command(name = "start_all", visible_alias = "start-all")]
    StartAll,
    /// Stop the background daemon, letting running backups finish
    #[command(name = "stop_all", visible_alias = "stop-all")]
    StopAll {
        /// How long to wait for the daemon to exit [default: daemon.shutdown_timeout + 15s]
        #[arg(long, value_parser = parse_duration, value_name = "DURATION")]
        wait: Option<Duration>,
        /// Do not wait for running backups (they continue inside their containers)
        #[arg(long)]
        immediate: bool,
    },
    /// Daemon state plus last result and next due backup for every target
    #[command(name = "status_all", visible_alias = "status-all")]
    StatusAll {
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Print a systemd unit for the daemon, or write it with --path
    #[command(name = "install_service", visible_alias = "install-service")]
    InstallService {
        /// Where to write the unit, e.g. /etc/systemd/system/pgback.service
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}

impl Cli {
    pub fn has_action(&self) -> bool {
        let a = &self.actions;
        a.init_config
            || a.config_sync
            || a.create_pre_hook.is_some()
            || a.create_post_hook.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_enterprise_flags_in_any_position() {
        let cli = Cli::try_parse_from([
            "pgback",
            "--config_sync",
            "run",
            "--dry_run",
            "--force_backup_type",
            "full",
            "--incremental_backup_interval",
            "6h",
            "--slack_notification_url",
            "https://hooks.slack.com/services/x",
            "--email_send_method",
            "aws",
            "--to_email",
            "a@x.io,b@x.io",
            "--container",
            "db1,db2",
        ])
        .unwrap();
        assert!(cli.actions.config_sync && cli.dry_run);
        assert!(matches!(cli.command, Some(Command::Run { if_due: false })));
        assert_eq!(cli.backup.force_backup_type, Some(BackupType::Full));
        assert_eq!(
            cli.backup.incremental_backup_interval,
            Some(Duration::from_secs(21_600))
        );
        assert_eq!(cli.notify.email_send_method, Some(EmailMethod::Aws));
        assert_eq!(cli.notify.to_email, ["a@x.io", "b@x.io"]);
        assert_eq!(cli.containers, ["db1", "db2"]);
    }

    #[test]
    fn kebab_aliases_and_daemon_subcommands() {
        let cli =
            Cli::try_parse_from(["pgback", "--dry-run", "--debug-mode", "status-all"]).unwrap();
        assert!(cli.dry_run && cli.debug_mode);
        assert!(matches!(cli.command, Some(Command::StatusAll { .. })));
        for sub in [
            "start_all",
            "stop_all",
            "status_all",
            "install_service",
            "test_notify",
        ] {
            assert!(Cli::try_parse_from(["pgback", sub]).is_ok(), "{sub}");
        }
        let cli = Cli::try_parse_from(["pgback", "--init_config"]).unwrap();
        assert!(cli.has_action() && cli.command.is_none());
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Cli::try_parse_from(["pgback", "run", "--force_backup_type", "weekly"]).is_err());
        assert!(
            Cli::try_parse_from(["pgback", "run", "--incremental_backup_interval", "soon"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["pgback", "--email_send_method", "pigeon", "run"]).is_err());
    }

    #[test]
    fn notify_args_round_trip_through_env() {
        let args = NotifyArgs {
            slack_notification_url: Some("https://hooks.slack.com/x".into()),
            to_email: vec!["a@x.io".into(), "b@x.io".into()],
            email_send_method: Some(EmailMethod::Mail),
            ..Default::default()
        };
        let env = args.to_env();
        assert!(env.contains(&("PGBACK_TO_EMAIL", "a@x.io,b@x.io".into())));
        assert!(env.contains(&("PGBACK_EMAIL_SEND_METHOD", "mail".into())));
        assert!(env.iter().all(|(k, _)| *k != "PGBACK_FROM_EMAIL"));
    }
}

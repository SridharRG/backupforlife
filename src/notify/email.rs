//! Email via Amazon SES (SMTP interface) or any SMTP relay, using lettre.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use sha2::Sha256;

use super::{Notification, Notifier, render_plain};
use crate::config::{EmailConfig, EmailMethod, Secret, SmtpTls};

pub struct Email {
    from: Mailbox,
    to: Vec<Mailbox>,
    subject_prefix: String,
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl Email {
    pub fn new(cfg: &EmailConfig) -> Result<Self> {
        let from: Mailbox = cfg
            .from
            .parse()
            .with_context(|| format!("invalid email from address `{}`", cfg.from))?;
        let to = cfg
            .to
            .iter()
            .map(|addr| {
                addr.parse::<Mailbox>()
                    .with_context(|| format!("invalid email recipient `{addr}`"))
            })
            .collect::<Result<Vec<_>>>()?;
        let timeout = Some(Duration::from_secs(30));
        let non_empty = |s: &Option<Secret>| {
            s.as_ref()
                .filter(|s| !s.is_empty())
                .map(|s| s.expose().to_string())
        };

        let transport = match cfg.method {
            EmailMethod::Aws => {
                let region = cfg.aws_region.as_deref().unwrap_or("us-east-1");
                let host = cfg
                    .smtp_host
                    .clone()
                    .unwrap_or_else(|| format!("email-smtp.{region}.amazonaws.com"));
                let username =
                    non_empty(&cfg.access_key).context("email method aws needs access_key")?;
                let password = match (non_empty(&cfg.smtp_password), non_empty(&cfg.secret_key)) {
                    (Some(password), _) => password,
                    (None, Some(secret)) => ses_smtp_password(&secret, region),
                    (None, None) => bail!("email method aws needs secret_key or smtp_password"),
                };
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)?
                    .port(cfg.smtp_port.unwrap_or(587))
                    .credentials(Credentials::new(username, password))
                    .timeout(timeout)
                    .build()
            }
            EmailMethod::Mail => {
                let host = cfg.smtp_host.clone().unwrap_or_else(|| "localhost".into());
                let local = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1");
                let mode = cfg.smtp_tls.unwrap_or(if local {
                    SmtpTls::None
                } else {
                    SmtpTls::Opportunistic
                });
                let port = cfg.smtp_port.unwrap_or(match mode {
                    SmtpTls::Tls => 465,
                    SmtpTls::Starttls => 587,
                    SmtpTls::None | SmtpTls::Opportunistic => 25,
                });
                let params =
                    || TlsParameters::new(host.clone()).map_err(|e| anyhow!("smtp tls: {e}"));
                let tls = match mode {
                    SmtpTls::None => Tls::None,
                    SmtpTls::Opportunistic => Tls::Opportunistic(params()?),
                    SmtpTls::Starttls => Tls::Required(params()?),
                    SmtpTls::Tls => Tls::Wrapper(params()?),
                };
                let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host)
                    .port(port)
                    .tls(tls)
                    .timeout(timeout);
                if let Some(user) = &cfg.smtp_username {
                    builder = builder.credentials(Credentials::new(
                        user.clone(),
                        non_empty(&cfg.smtp_password).unwrap_or_default(),
                    ));
                }
                builder.build()
            }
        };
        Ok(Self {
            from,
            to,
            subject_prefix: cfg
                .subject_prefix
                .clone()
                .unwrap_or_else(|| "[pgback]".into()),
            transport,
        })
    }
}

#[async_trait]
impl Notifier for Email {
    fn name(&self) -> &'static str {
        "email"
    }

    async fn send(&self, notification: &Notification) -> Result<()> {
        let mut builder = Message::builder()
            .from(self.from.clone())
            .subject(format!("{} {}", self.subject_prefix, notification.title))
            .header(ContentType::TEXT_PLAIN);
        for to in &self.to {
            builder = builder.to(to.clone());
        }
        let message = builder.body(render_plain(notification))?;
        self.transport
            .send(message)
            .await
            .map_err(|e| anyhow!("smtp delivery failed: {e}"))?;
        Ok(())
    }
}

/// Derives the SES SMTP password from an IAM secret access key, per AWS's documented
/// algorithm (SigV4-style HMAC chain over a fixed date, then version byte 0x04).
pub fn ses_smtp_password(secret_access_key: &str, region: &str) -> String {
    fn sign(key: &[u8], msg: &[u8]) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
        mac.update(msg);
        mac.finalize().into_bytes().to_vec()
    }
    let mut signature = sign(format!("AWS4{secret_access_key}").as_bytes(), b"11111111");
    let chain: [&[u8]; 4] = [region.as_bytes(), b"ses", b"aws4_request", b"SendRawEmail"];
    for part in chain {
        signature = sign(&signature, part);
    }
    let mut versioned = vec![0x04];
    versioned.extend(signature);
    base64::engine::general_purpose::STANDARD.encode(versioned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ses_password_matches_reference_implementation() {
        // Reference values from AWS's published Python algorithm.
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_eq!(
            ses_smtp_password(secret, "us-east-1"),
            "BLBM/9hSUELfq8Gw+rU1YcBjkOxGbhT2XG763xVLGWL9"
        );
        assert_eq!(
            ses_smtp_password(secret, "eu-west-1"),
            "BMW5RDrXmmVs0lV7GpI4oLkHXpZ4stDsk6q91z1g38Pk"
        );
    }

    #[test]
    fn rejects_bad_addresses() {
        let cfg = EmailConfig {
            from: "not an address".into(),
            to: vec!["dba@example.com".into()],
            ..Default::default()
        };
        assert!(Email::new(&cfg).is_err());
    }
}

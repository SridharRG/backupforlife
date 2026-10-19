//! Slack incoming webhooks and Google Chat space webhooks (both take `{"text": …}`).

use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;

use super::{Notification, Notifier, render_chat, truncate};
use crate::config::Secret;

pub struct Webhook {
    name: &'static str,
    url: Secret,
    client: reqwest::Client,
    /// Message size the service accepts, in characters.
    limit: usize,
}

impl Webhook {
    pub fn slack(client: reqwest::Client, url: Secret) -> Self {
        Self {
            name: "slack",
            url,
            client,
            limit: 39_000,
        }
    }

    pub fn google_chat(client: reqwest::Client, url: Secret) -> Self {
        Self {
            name: "google_chat",
            url,
            client,
            limit: 4_000,
        }
    }
}

#[async_trait]
impl Notifier for Webhook {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn send(&self, notification: &Notification) -> Result<()> {
        let body = serde_json::json!({ "text": render_chat(notification, self.limit) });
        let mut last_error = anyhow!("{}: no attempt made", self.name);
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
            }
            // The URL embeds the webhook token, so it must never reach an error message.
            match self.client.post(self.url.expose()).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    last_error =
                        anyhow!("{} returned HTTP {status}{}", self.name, error_body(&text));
                    if !(status.is_server_error()
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS)
                    {
                        break;
                    }
                }
                Err(e) => last_error = anyhow!("{} request failed: {}", self.name, e.without_url()),
            }
        }
        Err(last_error)
    }
}

/// Slack answers with short text (`invalid_token`) and Google with JSON; HTML error
/// pages (proxies, wrong URL) carry nothing useful.
fn error_body(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() || text.starts_with('<') {
        String::new()
    } else {
        format!(": {}", truncate(&text, 160))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies_are_condensed() {
        assert_eq!(error_body("invalid_token"), ": invalid_token");
        assert_eq!(error_body("<!doctype html><html>…</html>"), "");
        assert_eq!(
            error_body("{\n  \"error\": {\"code\": 400}\n}"),
            ": { \"error\": {\"code\": 400} }"
        );
        assert_eq!(error_body(""), "");
    }
}

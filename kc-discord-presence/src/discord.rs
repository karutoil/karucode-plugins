//! A Discord webhook client: post one message, edit it in place, delete it.
//!
//! A webhook is the whole reason this plugin needs no bot: no application, no
//! bot user, no gateway socket, no privileged intent, no TLS websocket client.
//! The operator creates a webhook in a channel and pastes the URL; from then on
//! this plugin is a publisher.
//!
//! Two rules shape the code:
//!
//! * **The URL is the credential.** It never appears in a log line, a tool
//!   result, a panel payload or an error message - only in the request itself.
//!   Everything else goes through [`crate::config::redact_webhook`].
//! * **Discord's limits are hard.** A `429` carries `retry_after` and must be
//!   obeyed rather than retried in a loop; a `404` on an edit means the message
//!   was deleted by hand and the caller must post a new one.

use serde_json::Value;
use std::time::Duration;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub const USER_AGENT: &str = concat!("karucode-discord-presence/", env!("CARGO_PKG_VERSION"));

/// A parsed webhook URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Webhook {
    /// `https://discord.com/api/v10` (or the configured override).
    pub base: String,
    pub id: String,
    pub token: String,
    pub thread_id: Option<String>,
}

impl Webhook {
    /// `api_base` overrides the host entirely, which is how a test points this
    /// client at a local server.
    pub fn parse_with_base(url: &str, api_base: Option<&str>) -> Result<Self, DiscordError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(DiscordError::BadUrl(
                "no webhook URL: paste one from Discord's channel settings (Integrations -> \
                 Webhooks -> Copy Webhook URL)"
                    .to_string(),
            ));
        }
        let (scheme, host, path, query) = split_url(url).ok_or_else(|| {
            DiscordError::BadUrl(format!(
                "`{}` is not an http(s) URL; expected something like \
                 https://discord.com/api/webhooks/<id>/<token>",
                redact(url)
            ))
        })?;
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let Some(pos) = segments.iter().position(|s| *s == "webhooks") else {
            return Err(DiscordError::BadUrl(format!(
                "`{}` has no `/webhooks/` in its path; copy the URL from the channel's webhook \
                 settings rather than the browser address bar",
                redact(url)
            )));
        };
        let id = segments.get(pos + 1).copied().unwrap_or_default();
        let token = segments.get(pos + 2).copied().unwrap_or_default();
        if id.is_empty() || token.is_empty() {
            return Err(DiscordError::BadUrl(format!(
                "`{}` is missing its webhook id or token",
                redact(url)
            )));
        }
        if !id.chars().all(|c| c.is_ascii_digit()) {
            return Err(DiscordError::BadUrl(format!(
                "`{id}` is not a Discord webhook id (it should be all digits)"
            )));
        }
        let base = match api_base.map(str::trim).filter(|b| !b.is_empty()) {
            Some(base) => base.trim_end_matches('/').to_string(),
            None => {
                let prefix = &segments[..pos];
                let versioned = prefix
                    .last()
                    .map(|p| is_version(p))
                    .unwrap_or(false);
                if versioned {
                    format!("{scheme}://{host}/{}", prefix.join("/"))
                } else {
                    format!("{scheme}://{host}/api/{}", crate::config::API_VERSION)
                }
            }
        };
        let thread_id = query.as_deref().and_then(|q| {
            q.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                if key == "thread_id" && !value.is_empty() {
                    Some(value.to_string())
                } else {
                    None
                }
            })
        });
        Ok(Self {
            base,
            id: id.to_string(),
            token: token.to_string(),
            thread_id,
        })
    }

    /// The URL with the token replaced, for anything a human reads.
    pub fn redacted(&self) -> String {
        format!("{}/webhooks/{}/***", self.base, self.id)
    }

    fn endpoint(&self, suffix: &str, wait: bool) -> String {
        let mut url = format!(
            "{}/webhooks/{}/{}{}",
            self.base, self.id, self.token, suffix
        );
        let mut params: Vec<String> = Vec::new();
        if wait {
            params.push("wait=true".to_string());
        }
        if let Some(thread) = self.thread_id.as_deref() {
            params.push(format!("thread_id={thread}"));
        }
        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }
        url
    }
}

/// A message this plugin created, so it can edit it later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRef {
    pub id: String,
    pub channel_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiscordError {
    BadUrl(String),
    Network(String),
    /// Discord answered with an error status. `message` is Discord's own text.
    Status {
        status: u16,
        message: String,
        code: Option<i64>,
    },
    /// `429`: wait `retry_after` seconds, then try again.
    RateLimited {
        retry_after: f64,
        message: String,
    },
    /// The message an edit targeted is gone (deleted by hand, or the webhook was
    /// recreated).
    NotFound,
}

impl DiscordError {
    /// Replace the webhook token wherever a message quotes a URL.
    ///
    /// A transport error carries the URL it failed on, and that URL contains the
    /// token - which must never reach a log line, a tool result or the panel.
    pub fn redacted(mut self, webhook: &Webhook) -> Self {
        let token = webhook.token.as_str();
        if token.is_empty() {
            return self;
        }
        let fix = |text: String| text.replace(token, "***");
        match &mut self {
            DiscordError::BadUrl(message) => *message = fix(message.clone()),
            DiscordError::Network(message) => *message = fix(message.clone()),
            DiscordError::Status { message, .. } => *message = fix(message.clone()),
            DiscordError::RateLimited { message, .. } => *message = fix(message.clone()),
            DiscordError::NotFound => {}
        }
        self
    }

    /// The message a person should read. Never contains the token.
    pub fn user_message(&self) -> String {
        match self {
            DiscordError::BadUrl(m) => m.clone(),
            DiscordError::Network(m) => format!("cannot reach Discord: {m}"),
            DiscordError::Status {
                status,
                message,
                code,
            } => match code {
                Some(code) => format!("Discord refused the message ({status}, code {code}): {message}"),
                None => format!("Discord refused the message ({status}): {message}"),
            },
            DiscordError::RateLimited { retry_after, .. } => {
                format!("Discord is rate limiting this webhook; retrying in {retry_after:.1}s")
            }
            DiscordError::NotFound => {
                "the message this plugin was editing no longer exists; a new one will be posted"
                    .to_string()
            }
        }
    }
}

impl std::fmt::Display for DiscordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.user_message())
    }
}

impl std::error::Error for DiscordError {}

/// A webhook client bound to one webhook.
#[derive(Debug, Clone)]
pub struct Client {
    pub webhook: Webhook,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(webhook: Webhook) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build();
        Self { webhook, agent }
    }

    /// Post a new message and wait for it, so the caller learns its id.
    pub fn post(&self, payload: &Value) -> Result<MessageRef, DiscordError> {
        let url = self.webhook.endpoint("", true);
        let response = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_json(payload);
        match response {
            Ok(response) => parse_message(response),
            Err(error) => Err(translate(error, false).redacted(&self.webhook)),
        }
    }

    /// Edit a message this webhook posted.
    pub fn edit(&self, message_id: &str, payload: &Value) -> Result<MessageRef, DiscordError> {
        let url = self
            .webhook
            .endpoint(&format!("/messages/{message_id}"), false);
        let response = self
            .agent
            .request("PATCH", &url)
            .set("Content-Type", "application/json")
            .send_json(payload);
        match response {
            Ok(response) => parse_message(response),
            // A 404 here means the message is gone; the caller reposts.
            Err(error) => Err(translate(error, true).redacted(&self.webhook)),
        }
    }

    /// Delete a message this webhook posted. A message that is already gone is
    /// not an error.
    pub fn delete(&self, message_id: &str) -> Result<(), DiscordError> {
        let url = self
            .webhook
            .endpoint(&format!("/messages/{message_id}"), false);
        match self.agent.delete(&url).call() {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(error) => Err(translate(error, false).redacted(&self.webhook)),
        }
    }
}

fn parse_message(response: ureq::Response) -> Result<MessageRef, DiscordError> {
    let body: Value = response.into_json().map_err(|e| {
        DiscordError::Network(format!("Discord's answer was not JSON: {e}"))
    })?;
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        return Err(DiscordError::Network(
            "Discord accepted the message but did not return its id (is `wait=true` honoured?)"
                .to_string(),
        ));
    }
    Ok(MessageRef {
        id,
        channel_id: body
            .get("channel_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Turn a transport/status failure into this plugin's error.
///
/// `gone_is_not_found` is only true for an edit: there a `404` means the message
/// was deleted by hand and the caller should repost. On a *post* the same `404`
/// means the webhook itself is unknown (a bad id or token), which is a real
/// refusal and must be reported with Discord's own words.
fn translate(error: ureq::Error, gone_is_not_found: bool) -> DiscordError {
    match error {
        ureq::Error::Status(status, response) => {
            let retry_header = response
                .header("retry-after")
                .and_then(|v| v.parse::<f64>().ok());
            let body: Value = response.into_json().unwrap_or(Value::Null);
            let message = body
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message")
                .to_string();
            let code = body.get("code").and_then(Value::as_i64);
            let retry_after = body
                .get("retry_after")
                .and_then(Value::as_f64)
                .or(retry_header);
            if status == 429 {
                return DiscordError::RateLimited {
                    retry_after: retry_after.unwrap_or(1.0).clamp(0.5, 300.0),
                    message,
                };
            }
            if status == 404 && gone_is_not_found {
                return DiscordError::NotFound;
            }
            DiscordError::Status {
                status,
                message,
                code,
            }
        }
        ureq::Error::Transport(transport) => DiscordError::Network(transport.to_string()),
    }
}

fn is_version(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some('v') => !chars.as_str().is_empty() && chars.as_str().chars().all(|c| c.is_ascii_digit()),
        _ => false,
    }
}

fn split_url(url: &str) -> Option<(String, String, String, Option<String>)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (authority, tail) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos + 1..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return None;
    }
    let (path, query) = match tail.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query.to_string())),
        None => (tail.to_string(), None),
    };
    Some((scheme, authority.to_string(), path, query))
}

/// Redact a URL for an error message, without depending on the config module.
fn redact(url: &str) -> String {
    crate::config::redact_webhook(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_urls_discord_actually_hands_out() {
        let webhook = Webhook::parse_with_base("https://discord.com/api/webhooks/123456789/abcDEF-123_x", None).unwrap();
        assert_eq!(webhook.base, "https://discord.com/api/v10");
        assert_eq!(webhook.id, "123456789");
        assert_eq!(webhook.token, "abcDEF-123_x");
        assert!(webhook.thread_id.is_none());

        let webhook =
            Webhook::parse_with_base("https://discordapp.com/api/webhooks/42/tok/?thread_id=99", None).unwrap();
        assert_eq!(webhook.base, "https://discordapp.com/api/v10");
        assert_eq!(webhook.thread_id.as_deref(), Some("99"));

        let webhook = Webhook::parse_with_base("https://discord.com/api/v9/webhooks/7/t", None).unwrap();
        assert_eq!(webhook.base, "https://discord.com/api/v9");
    }

    #[test]
    fn a_pasted_url_is_never_echoed_with_its_token() {
        let error = Webhook::parse_with_base("https://discord.com/api/hooks/1/secret-token", None).unwrap_err();
        let text = error.user_message();
        assert!(!text.contains("secret-token"), "{text}");
        assert!(text.contains("webhooks"), "{text}");

        let webhook = Webhook::parse_with_base("https://discord.com/api/webhooks/1/secret-token", None).unwrap();
        assert_eq!(webhook.redacted(), "https://discord.com/api/v10/webhooks/1/***");
        assert!(!webhook.redacted().contains("secret-token"));
    }

    #[test]
    fn rejects_what_is_not_a_webhook() {
        for bad in [
            "",
            "not a url",
            "ftp://discord.com/api/webhooks/1/t",
            "https://discord.com/api/webhooks/abc/t",
            "https://discord.com/api/webhooks/1",
            "https://discord.com/channels/1/2",
        ] {
            assert!(Webhook::parse_with_base(bad, None).is_err(), "`{bad}` should be refused");
        }
    }

    #[test]
    fn an_api_base_override_replaces_the_host() {
        let webhook = Webhook::parse_with_base(
            "https://discord.com/api/webhooks/1/t",
            Some("http://127.0.0.1:9999/api/v10/"),
        )
        .unwrap();
        assert_eq!(webhook.base, "http://127.0.0.1:9999/api/v10");
    }

    #[test]
    fn the_endpoint_carries_wait_and_the_thread() {
        let webhook = Webhook {
            base: "https://discord.com/api/v10".to_string(),
            id: "1".to_string(),
            token: "t".to_string(),
            thread_id: Some("55".to_string()),
        };
        assert_eq!(
            webhook.endpoint("", true),
            "https://discord.com/api/v10/webhooks/1/t?wait=true&thread_id=55"
        );
        assert_eq!(
            webhook.endpoint("/messages/9", false),
            "https://discord.com/api/v10/webhooks/1/t/messages/9?thread_id=55"
        );
    }

    #[test]
    fn a_transport_error_does_not_leak_the_token_it_quotes() {
        // Nothing listens on port 1, so this is a real connect failure. ureq's
        // transport error quotes the URL it failed on - which carries the token.
        let webhook = Webhook::parse_with_base(
            "https://discord.com/api/webhooks/1/super-secret-token",
            Some("http://127.0.0.1:1/api/v10"),
        )
        .unwrap();
        let client = Client::new(webhook);
        let error = client
            .post(&serde_json::json!({"content": "hi"}))
            .expect_err("nothing is listening");
        let text = error.user_message();
        assert!(!text.contains("super-secret-token"), "{text}");
        assert!(text.contains("***"), "{text}");
    }

    #[test]
    fn a_429_is_reported_with_its_retry_after() {
        let error = DiscordError::RateLimited {
            retry_after: 2.5,
            message: "You are being rate limited.".to_string(),
        };
        assert!(matches!(error, DiscordError::RateLimited { .. }));
        assert!(error.user_message().contains("2.5s"));
    }
}

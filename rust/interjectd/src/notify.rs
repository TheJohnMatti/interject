//! Delivery. Decides *whether* to interrupt a human and hands the message to a
//! transport; it is not itself a notification service (DESIGN.md §6).
//!
//! The return path is deliberately inverted: rather than expecting a phone to
//! reach the daemon, the daemon holds a long-lived outbound subscription to an
//! ntfy topic and reads answers from it. That works behind NAT with no hosting,
//! no port forwarding and no public URL — the property that lets one-tap answers
//! work for a daemon on a laptop.
//!
//! Because anyone who learns a topic name can publish to it, every action button
//! carries a token derived from a per-daemon secret. An answer with a bad token
//! is discarded, so the topic name alone does not confer the ability to answer.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;

use crate::types::InboxBatch;

pub const DEFAULT_NTFY_BASE: &str = "https://ntfy.sh";
/// ntfy renders at most three action buttons.
const MAX_ACTIONS: usize = 3;

#[derive(Debug, Clone, Default)]
pub struct NotifyConfig {
    pub ntfy_base: String,
    /// Topic notifications are published to.
    pub ntfy_topic: Option<String>,
    /// Topic the daemon subscribes to for answers from action buttons.
    pub answer_topic: Option<String>,
    /// Generic webhook receiving the raw event as JSON.
    pub webhook: Option<String>,
}

impl NotifyConfig {
    pub fn enabled(&self) -> bool {
        self.ntfy_topic.is_some() || self.webhook.is_some()
    }
}

/// `hmac-sha256(secret, key)`, truncated to 32 hex characters.
///
/// Truncation is fine here: the token only has to be unforgeable, and a 128-bit
/// tag keeps the ntfy `Actions` header within its length budget.
pub fn answer_token(secret: &str, key: &str) -> String {
    let mut mac =
        <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(key.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(32)
        .collect()
}

pub fn verify_answer_token(secret: &str, key: &str, presented: &str) -> bool {
    let expected = answer_token(secret, key);
    // Constant-time comparison: these are equal-length hex strings.
    expected.len() == presented.len()
        && expected
            .bytes()
            .zip(presented.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[derive(Debug, Clone)]
pub enum Event {
    /// New questions are waiting, already grouped into batches.
    Questions(Vec<InboxBatch>),
    /// Signals that owed a heartbeat and went quiet.
    Silence(Vec<String>),
}

/// One rendered notification, kept separate from sending so it can be asserted
/// on in tests without any network involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub body: String,
    pub priority: Option<&'static str>,
    pub tags: Option<&'static str>,
    /// Pre-formatted ntfy `Actions` header, if the message is answerable in one tap.
    pub actions: Option<String>,
}

fn label_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Render a batch, attaching one-tap actions only when they are unambiguous.
///
/// A batch of one with a small option set is answerable from the notification. A
/// batch of many is not — guessing which of 137 listings a tap referred to would
/// be worse than making someone open the inbox.
pub fn render_questions(batch: &InboxBatch, config: &NotifyConfig, secret: &str) -> Message {
    let title = if batch.count > 1 {
        format!("{} ({} waiting)", batch.prompt, batch.count)
    } else {
        batch.prompt.clone()
    };

    let body = if batch.count > 1 {
        format!(
            "{} questions in {}. Answer with: interjectd inbox --answer",
            batch.count, batch.batch_key
        )
    } else {
        batch
            .questions
            .first()
            .and_then(|q| q.context.as_ref())
            .map(|c| c.to_string())
            .unwrap_or_else(|| batch.batch_key.clone())
    };

    let actions = match (batch.count, &config.answer_topic) {
        (1, Some(topic)) => batch.questions.first().and_then(|question| {
            let choices: Vec<Value> = match batch.kind.as_str() {
                "approve" => vec![json!(true), json!(false)],
                "choice" => batch
                    .options
                    .as_ref()
                    .and_then(|o| o.as_array().cloned())
                    .unwrap_or_default(),
                // Free text cannot be answered by a button.
                _ => Vec::new(),
            };
            if choices.is_empty() || choices.len() > MAX_ACTIONS {
                return None;
            }
            let token = answer_token(secret, &question.key);
            let rendered = choices
                .iter()
                .map(|choice| {
                    let payload = json!({
                        "key": question.key,
                        "value": choice,
                        "token": token,
                    });
                    // ntfy parses this header positionally; the JSON body is
                    // wrapped in single quotes because it contains commas.
                    format!(
                        "http, {}, {}/{}, method=POST, body='{}'",
                        label_of(choice),
                        config.ntfy_base.trim_end_matches('/'),
                        topic,
                        payload
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            Some(rendered)
        }),
        _ => None,
    };

    Message {
        title,
        body,
        priority: None,
        tags: Some("question"),
        actions,
    }
}

pub fn render_silence(names: &[String]) -> Message {
    Message {
        title: format!(
            "{} signal{} went quiet",
            names.len(),
            if names.len() == 1 { "" } else { "s" }
        ),
        body: names.join(", "),
        priority: Some("high"),
        tags: Some("warning"),
        actions: None,
    }
}

pub struct Notifier {
    config: NotifyConfig,
    secret: String,
}

impl Notifier {
    pub fn new(config: NotifyConfig, secret: String) -> Self {
        Self { config, secret }
    }

    pub fn config(&self) -> &NotifyConfig {
        &self.config
    }

    pub fn messages(&self, event: &Event) -> Vec<Message> {
        match event {
            Event::Questions(batches) => batches
                .iter()
                .map(|batch| render_questions(batch, &self.config, &self.secret))
                .collect(),
            Event::Silence(names) if names.is_empty() => Vec::new(),
            Event::Silence(names) => vec![render_silence(names)],
        }
    }

    /// Deliver an event to every configured sink. Blocking; call from a blocking
    /// context. A failing sink is reported but never stops the others.
    pub fn deliver(&self, event: &Event) -> Vec<anyhow::Error> {
        let mut failures = Vec::new();
        for message in self.messages(event) {
            if let Some(topic) = &self.config.ntfy_topic {
                if let Err(error) = self.publish_ntfy(topic, &message) {
                    failures.push(error.context("ntfy delivery failed"));
                }
            }
            if let Some(url) = &self.config.webhook {
                if let Err(error) = post_webhook(url, &message) {
                    failures.push(error.context("webhook delivery failed"));
                }
            }
        }
        failures
    }

    fn publish_ntfy(&self, topic: &str, message: &Message) -> Result<()> {
        let url = format!("{}/{}", self.config.ntfy_base.trim_end_matches('/'), topic);
        let mut request = ureq::post(&url)
            .header("Title", &message.title)
            .header("Markdown", "no");
        if let Some(tags) = message.tags {
            request = request.header("Tags", tags);
        }
        if let Some(priority) = message.priority {
            request = request.header("Priority", priority);
        }
        if let Some(actions) = &message.actions {
            request = request.header("Actions", actions);
        }
        request
            .send(message.body.as_str())
            .with_context(|| format!("publishing to {url}"))?;
        Ok(())
    }
}

fn post_webhook(url: &str, message: &Message) -> Result<()> {
    ureq::post(url)
        .send_json(json!({
            "title": message.title,
            "body": message.body,
            "actions": message.actions,
        }))
        .with_context(|| format!("posting to {url}"))?;
    Ok(())
}

/// One answer, as a tapped action button publishes it.
#[derive(Debug, Deserialize)]
pub struct AnswerCallback {
    pub key: String,
    pub value: Value,
    pub token: String,
}

/// The envelope ntfy wraps published messages in on its `/json` stream.
#[derive(Debug, Deserialize)]
struct NtfyEnvelope {
    event: String,
    #[serde(default)]
    message: Option<String>,
}

/// Decode one line of an ntfy stream into an answer, or `None` if the line is
/// not one (keepalives, open events, malformed payloads, bad tokens).
pub fn decode_stream_line(line: &str, secret: &str) -> Option<AnswerCallback> {
    let envelope: NtfyEnvelope = serde_json::from_str(line).ok()?;
    if envelope.event != "message" {
        return None;
    }
    let callback: AnswerCallback = serde_json::from_str(&envelope.message?).ok()?;
    if !verify_answer_token(secret, &callback.key, &callback.token) {
        tracing::warn!(key = %callback.key, "discarded an answer with a bad token");
        return None;
    }
    Some(callback)
}

/// Subscribe to the answer topic forever, applying each verified answer.
///
/// Runs on its own thread because the connection is a long-lived blocking read.
/// Reconnects with backoff: a dropped subscription must not silently stop
/// answers from arriving, which would be this project's own failure mode.
pub fn run_answer_subscriber<F>(config: NotifyConfig, secret: String, apply: F)
where
    F: Fn(AnswerCallback) + Send + 'static,
{
    let Some(topic) = config.answer_topic.clone() else {
        return;
    };
    let url = format!("{}/{}/json", config.ntfy_base.trim_end_matches('/'), topic);
    std::thread::spawn(move || {
        let mut backoff = Duration::from_secs(1);
        loop {
            match subscribe_once(&url, &secret, &apply) {
                Ok(()) => {
                    tracing::info!(%url, "answer subscription closed; reconnecting");
                    backoff = Duration::from_secs(1);
                }
                Err(error) => {
                    tracing::warn!(%url, %error, "answer subscription failed; retrying");
                }
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });
}

fn subscribe_once<F>(url: &str, secret: &str, apply: &F) -> Result<()>
where
    F: Fn(AnswerCallback),
{
    let response = ureq::get(url).call().context("opening the answer stream")?;
    let reader = BufReader::new(response.into_body().into_reader());
    for line in reader.lines() {
        let line = line.context("reading the answer stream")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(callback) = decode_stream_line(&line, secret) {
            apply(callback);
        }
    }
    Ok(())
}

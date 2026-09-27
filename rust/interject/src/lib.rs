//! Rust client for [interject](https://github.com/TheJohnMatti/interject).
//!
//! A durable `ask()` primitive: stop a program, ask a human, resume days later
//! on another machine.
//!
//! ```no_run
//! use interject::{Ask, Client};
//!
//! let client = Client::from_env();
//! let vehicle = client.ask(
//!     Ask::new("Is this a car?")
//!         .id("vehicle_type")
//!         .options(["car", "motorcycle", "boat"])
//!         .context(serde_json::json!({"title": "2018 Honda CBR"}))
//!         .ttl_secs(48 * 3600),
//! )?;
//! # Ok::<(), interject::Error>(())
//! ```
//!
//! The wire format is documented in `docs/PROTOCOL.md`; this crate and the
//! Python client are independent implementations of it, with no shared FFI.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub const DEFAULT_URL: &str = "http://127.0.0.1:8787";
pub const DEFAULT_PROJECT: &str = "default";
const DEFAULT_WAIT_SECS: u64 = 30;

/// Canonical JSON for a question's context.
///
/// Must agree byte-for-byte with the Python client, which uses
/// `json.dumps(sort_keys=True, separators=(",", ":"), ensure_ascii=False)`.
/// `serde_json` already sorts object keys and emits no insignificant
/// whitespace or ASCII escapes, so plain serialisation is the canonical form.
fn canon(context: Option<&Value>) -> String {
    match context {
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
        None => "{}".to_string(),
    }
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `sha256(canon(context))`, lowercase hex.
pub fn context_digest(context: Option<&Value>) -> String {
    sha256_hex(&canon(context))
}

/// `sha256(project \0 question_id \0 context_digest)`, lowercase hex.
pub fn question_key(project: &str, question_id: &str, context: Option<&Value>) -> String {
    sha256_hex(&format!(
        "{project}\0{question_id}\0{}",
        context_digest(context)
    ))
}

/// What to do when `wait` elapses with the question still open (DESIGN.md D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnTimeout {
    /// Return `Err(Error::Suspended)`; the caller exits and retries later.
    Suspend,
    /// Keep long-polling.
    Block,
    /// Return the declared default, recording it so a replay is deterministic.
    Default,
}

impl OnTimeout {
    fn as_str(self) -> &'static str {
        match self {
            OnTimeout::Suspend => "suspend",
            OnTimeout::Block => "block",
            OnTimeout::Default => "default",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The question is still open and `on_timeout` was `Suspend`. Exit cleanly;
    /// the next run replays the key and returns the answer if one has arrived.
    #[error("question {id:?} is still open (key {key}); exit and retry later")]
    Suspended { key: String, id: String },
    #[error("question {id:?} expired with no default (key {key})")]
    Expired { key: String, id: String },
    #[error("daemon rejected the project token")]
    Unauthorized,
    #[error("interjectd said {status}: {message}")]
    Api { status: u16, message: String },
    #[error("could not reach interjectd: {0}")]
    Unreachable(String),
    #[error("interjectd returned an unusable response: {0}")]
    Protocol(String),
    #[error("invalid request: {0}")]
    Invalid(String),
}

/// A question to ask. Build with [`Ask::new`] and the chained setters.
#[derive(Debug, Clone)]
pub struct Ask {
    prompt: String,
    id: Option<String>,
    kind: Option<String>,
    options: Option<Value>,
    context: Option<Value>,
    context_ref: Option<String>,
    suggest: Option<Value>,
    ttl_seconds: Option<i64>,
    default: Option<Value>,
    on_timeout: OnTimeout,
    wait_seconds: u64,
    priority: i64,
    batch_key: Option<String>,
    origin: Option<Value>,
}

impl Ask {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            id: None,
            kind: None,
            options: None,
            context: None,
            context_ref: None,
            suggest: None,
            ttl_seconds: None,
            default: None,
            on_timeout: OnTimeout::Suspend,
            wait_seconds: DEFAULT_WAIT_SECS,
            priority: 5,
            batch_key: None,
            origin: None,
        }
    }

    /// A stable id for this question class. Strongly recommended: without it
    /// the daemon has nothing durable to key on but the caller's own choice.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    pub fn options<I, S>(mut self, options: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Value>,
    {
        self.options = Some(Value::Array(options.into_iter().map(Into::into).collect()));
        self
    }

    pub fn context(mut self, context: Value) -> Self {
        self.context = Some(context);
        self
    }

    pub fn context_ref(mut self, reference: impl Into<String>) -> Self {
        self.context_ref = Some(reference.into());
        self
    }

    pub fn suggest(mut self, value: Value, confidence: f64) -> Self {
        self.suggest = Some(serde_json::json!({"value": value, "confidence": confidence}));
        self
    }

    pub fn ttl_secs(mut self, seconds: i64) -> Self {
        self.ttl_seconds = Some(seconds);
        self
    }

    pub fn default(mut self, value: Value) -> Self {
        self.default = Some(value);
        self
    }

    pub fn on_timeout(mut self, policy: OnTimeout) -> Self {
        self.on_timeout = policy;
        self
    }

    pub fn wait_secs(mut self, seconds: u64) -> Self {
        self.wait_seconds = seconds;
        self
    }

    pub fn priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }

    pub fn batch_key(mut self, batch_key: impl Into<String>) -> Self {
        self.batch_key = Some(batch_key.into());
        self
    }

    pub fn origin(mut self, origin: Value) -> Self {
        self.origin = Some(origin);
        self
    }

    fn resolved_kind(&self) -> String {
        if let Some(kind) = &self.kind {
            return kind.clone();
        }
        if self.options.is_some() {
            return "choice".to_string();
        }
        if matches!(self.default, Some(Value::Bool(_))) {
            return "approve".to_string();
        }
        "text".to_string()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Answer {
    pub value: Value,
    pub source: String,
    #[serde(default)]
    pub answered_by: Option<String>,
    pub answered_at: String,
    #[serde(default)]
    pub latency_ms: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    pub key: String,
    pub state: String,
    #[serde(default)]
    pub answer: Option<Answer>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub created: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboxQuestion {
    pub key: String,
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub context: Option<Value>,
    #[serde(default)]
    pub context_ref: Option<String>,
    #[serde(default)]
    pub suggest: Option<Value>,
    pub created_at: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub priority: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboxBatch {
    pub batch_key: String,
    pub prompt: String,
    pub kind: String,
    #[serde(default)]
    pub options: Option<Value>,
    pub count: usize,
    pub questions: Vec<InboxQuestion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Inbox {
    pub batches: Vec<InboxBatch>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Signal {
    pub name: String,
    pub state: String,
    pub last_seen: String,
    #[serde(default)]
    pub expect_every_seconds: Option<i64>,
    #[serde(default)]
    pub expect_by: Option<String>,
    #[serde(default)]
    pub due_at: Option<String>,
}

/// A roll-up: what is waiting, and whether anything has gone quiet.
#[derive(Debug, Clone, Deserialize)]
pub struct Digest {
    pub open: i64,
    #[serde(default)]
    pub oldest_created_at: Option<String>,
    #[serde(default)]
    pub silent_signals: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Signals {
    signals: Vec<Signal>,
}

#[derive(Serialize)]
struct AnswerBody<'a> {
    key: &'a str,
    value: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    answered_by: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a str>,
}

/// A connection to one project on one daemon.
#[derive(Debug, Clone)]
pub struct Client {
    base: String,
    project: String,
    token: Option<String>,
}

impl Client {
    pub fn new(base: impl Into<String>, project: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            project: project.into(),
            token,
        }
    }

    /// Read `INTERJECT_URL`, `INTERJECT_PROJECT` and `INTERJECT_TOKEN`.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var("INTERJECT_URL").unwrap_or_else(|_| DEFAULT_URL.to_string()),
            std::env::var("INTERJECT_PROJECT").unwrap_or_else(|_| DEFAULT_PROJECT.to_string()),
            std::env::var("INTERJECT_TOKEN").ok(),
        )
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Ask a human, and return their answer.
    pub fn ask(&self, ask: Ask) -> Result<Value, Error> {
        let id = ask
            .id
            .clone()
            .ok_or_else(|| Error::Invalid("Ask::id is required".into()))?;
        if ask.on_timeout == OnTimeout::Default && ask.default.is_none() {
            return Err(Error::Invalid(
                "OnTimeout::Default requires Ask::default".into(),
            ));
        }
        let key = question_key(&self.project, &id, ask.context.as_ref());

        let mut body = serde_json::json!({
            "key": key,
            "id": id,
            "prompt": ask.prompt,
            "kind": ask.resolved_kind(),
            "options": ask.options,
            "context": ask.context,
            "context_ref": ask.context_ref,
            "suggest": ask.suggest,
            "ttl_seconds": ask.ttl_seconds,
            "on_timeout": ask.on_timeout.as_str(),
            "priority": ask.priority,
            "batch_key": ask.batch_key.clone().unwrap_or_else(|| id.clone()),
            "origin": ask.origin,
        });
        if let Some(default) = &ask.default {
            body["default"] = default.clone();
        }

        let snapshot: Snapshot = self.send("POST", "/v0/questions", Some(&body))?;
        if let Some(value) = self.resolve(&snapshot, &key, &id, ask.default.as_ref())? {
            return Ok(value);
        }

        loop {
            let path = format!("/v0/questions/{key}?wait={}", ask.wait_seconds);
            let snapshot: Snapshot = self.send("GET", &path, None)?;
            if let Some(value) = self.resolve(&snapshot, &key, &id, ask.default.as_ref())? {
                return Ok(value);
            }
            match ask.on_timeout {
                OnTimeout::Block => continue,
                OnTimeout::Default => {
                    let default = ask.default.clone().unwrap_or(Value::Null);
                    // Record it, so a later replay returns this same value
                    // rather than defaulting a second time.
                    let _: Snapshot = self.send(
                        "POST",
                        "/v0/answers",
                        Some(
                            &serde_json::to_value(AnswerBody {
                                key: &key,
                                value: &default,
                                answered_by: None,
                                source: Some("default"),
                            })
                            .map_err(|e| Error::Protocol(e.to_string()))?,
                        ),
                    )?;
                    return Ok(default);
                }
                OnTimeout::Suspend => return Err(Error::Suspended { key, id }),
            }
        }
    }

    /// Report that a named signal is alive and when it is next due. Silence past
    /// the deadline is itself an event.
    pub fn heartbeat(&self, name: &str, expect_every_seconds: i64) -> Result<(), Error> {
        let body = serde_json::json!({
            "name": name,
            "expect_every_seconds": expect_every_seconds,
        });
        let _: Value = self.send("POST", "/v0/signals/heartbeat", Some(&body))?;
        Ok(())
    }

    /// Declare a one-shot deadline instead of a recurring interval.
    pub fn expect_by(&self, name: &str, deadline_rfc3339: &str) -> Result<(), Error> {
        let body = serde_json::json!({ "name": name, "expect_by": deadline_rfc3339 });
        let _: Value = self.send("POST", "/v0/signals/heartbeat", Some(&body))?;
        Ok(())
    }

    /// Open questions, grouped by batch — the human surface's read path.
    pub fn inbox(&self, limit: usize, batch_key: Option<&str>) -> Result<Inbox, Error> {
        let mut path = format!("/v0/inbox?limit={limit}");
        if let Some(batch) = batch_key {
            path.push_str(&format!("&batch_key={batch}"));
        }
        self.send("GET", &path, None)
    }

    /// Answer a question on a human's behalf.
    pub fn answer(
        &self,
        key: &str,
        value: &Value,
        answered_by: Option<&str>,
    ) -> Result<Snapshot, Error> {
        let body = serde_json::to_value(AnswerBody {
            key,
            value,
            answered_by,
            source: None,
        })
        .map_err(|e| Error::Protocol(e.to_string()))?;
        self.send("POST", "/v0/answers", Some(&body))
    }

    /// A batched roll-up instead of a stream of interruptions.
    pub fn digest(&self) -> Result<Digest, Error> {
        self.send("GET", "/v0/digest", None)
    }

    pub fn signals(&self) -> Result<Vec<Signal>, Error> {
        let signals: Signals = self.send("GET", "/v0/signals", None)?;
        Ok(signals.signals)
    }

    fn resolve(
        &self,
        snapshot: &Snapshot,
        key: &str,
        id: &str,
        default: Option<&Value>,
    ) -> Result<Option<Value>, Error> {
        match snapshot.state.as_str() {
            "answered" => snapshot
                .answer
                .as_ref()
                .map(|a| Some(a.value.clone()))
                .ok_or_else(|| {
                    Error::Protocol("daemon reported 'answered' without an answer".into())
                }),
            "expired" => match default {
                Some(value) => Ok(Some(value.clone())),
                None => Err(Error::Expired {
                    key: key.to_string(),
                    id: id.to_string(),
                }),
            },
            "open" => Ok(None),
            other => Err(Error::Protocol(format!("unknown state {other:?}"))),
        }
    }

    fn send<T: for<'de> Deserialize<'de>>(
        &self,
        _method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, Error> {
        let url = format!("{}{}", self.base, path);
        // Long polls hold the connection for `wait` seconds, so the read timeout
        // has to be generous rather than the library default.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(330)))
            .build()
            .into();

        // ureq 3 gives POST and GET distinct builder types, so the two paths
        // cannot be unified into one variable.
        let (status, text) = if let Some(payload) = body {
            let mut request = agent
                .post(&url)
                .header("X-Interject-Project", &self.project)
                .header("Accept", "application/json");
            if let Some(token) = &self.token {
                request = request.header("Authorization", &format!("Bearer {token}"));
            }
            let mut response = request
                .send_json(payload)
                .map_err(|e| Error::Unreachable(e.to_string()))?;
            let status = response.status().as_u16();
            let text = response
                .body_mut()
                .read_to_string()
                .map_err(|e| Error::Protocol(e.to_string()))?;
            (status, text)
        } else {
            let mut request = agent
                .get(&url)
                .header("X-Interject-Project", &self.project)
                .header("Accept", "application/json");
            if let Some(token) = &self.token {
                request = request.header("Authorization", &format!("Bearer {token}"));
            }
            let mut response = request
                .call()
                .map_err(|e| Error::Unreachable(e.to_string()))?;
            let status = response.status().as_u16();
            let text = response
                .body_mut()
                .read_to_string()
                .map_err(|e| Error::Protocol(e.to_string()))?;
            (status, text)
        };

        if status == 401 || status == 403 {
            return Err(Error::Unauthorized);
        }
        if status >= 400 {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.clone());
            return Err(Error::Api { status, message });
        }
        serde_json::from_str(&text).map_err(|e| Error::Protocol(format!("{e}: {text}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_digest_is_order_independent() {
        let a = serde_json::json!({"a": 1, "b": 2});
        let b = serde_json::json!({"b": 2, "a": 1});
        assert_eq!(context_digest(Some(&a)), context_digest(Some(&b)));
    }

    #[test]
    fn missing_context_equals_empty_context() {
        let empty = serde_json::json!({});
        assert_eq!(context_digest(None), context_digest(Some(&empty)));
    }

    #[test]
    fn key_matches_the_cross_language_vector() {
        // The same vector is asserted by the Python suite, which is how the two
        // independent client implementations are kept honest about D2.
        let context = serde_json::json!({"price": 4200, "title": "2018 Honda CBR"});
        assert_eq!(
            question_key("test", "vehicle_type", Some(&context)),
            "71fc55aa1f4d17f46aa9d5ccadd45350baa69a309deb5b17d879d20e873e4f88"
        );
    }
}

//! Wire types. These mirror `docs/PROTOCOL.md` exactly; every client depends on
//! the field names here, so treat renames as breaking changes.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Distinguish a field that is present-but-null from one that is absent.
///
/// `Option<T>` alone cannot: serde maps an explicit JSON `null` to `None`, which
/// would make a declared default of `null` indistinguishable from no default.
/// With `#[serde(default)]` supplying `None` for an absent field, this only runs
/// when the key is present, so present-null becomes `Some(Value::Null)`.
fn present_or_absent<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// `POST /v0/questions` — register (or replay) a question.
#[derive(Debug, Deserialize)]
pub struct NewQuestion {
    pub key: String,
    pub id: String,
    pub prompt: String,
    pub kind: String,
    #[serde(default)]
    pub options: Option<Value>,
    #[serde(default)]
    pub context: Option<Value>,
    #[serde(default)]
    pub context_ref: Option<String>,
    #[serde(default)]
    pub suggest: Option<Value>,
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
    /// Present-but-null must stay distinguishable from absent, because a
    /// declared default of `null` is a legitimate answer. See `present_or_absent`.
    #[serde(default, deserialize_with = "present_or_absent")]
    pub default: Option<Value>,
    #[serde(default = "default_on_timeout")]
    pub on_timeout: String,
    #[serde(default = "default_priority")]
    pub priority: i64,
    #[serde(default)]
    pub batch_key: Option<String>,
    #[serde(default)]
    pub origin: Option<Value>,
    #[serde(default)]
    pub shadow_of: Option<String>,
}

fn default_on_timeout() -> String {
    "suspend".to_string()
}

fn default_priority() -> i64 {
    5
}

#[derive(Debug, Clone, Serialize)]
pub struct Answer {
    pub value: Value,
    pub source: String,
    pub answered_by: Option<String>,
    pub answered_at: String,
    pub latency_ms: Option<i64>,
}

/// The response shape shared by question registration and polling.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub key: String,
    pub state: String,
    pub answer: Option<Answer>,
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct NewAnswer {
    pub key: String,
    pub value: Value,
    #[serde(default)]
    pub answered_by: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InboxQuestion {
    pub key: String,
    pub id: String,
    pub prompt: String,
    pub context: Option<Value>,
    pub context_ref: Option<String>,
    pub suggest: Option<Value>,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub priority: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct InboxBatch {
    pub batch_key: String,
    pub prompt: String,
    pub kind: String,
    pub options: Option<Value>,
    pub count: usize,
    pub questions: Vec<InboxQuestion>,
}

#[derive(Debug, Serialize)]
pub struct Inbox {
    pub batches: Vec<InboxBatch>,
}

#[derive(Debug, Deserialize)]
pub struct NewHeartbeat {
    pub name: String,
    #[serde(default)]
    pub expect_every_seconds: Option<i64>,
    #[serde(default)]
    pub expect_by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Signal {
    pub name: String,
    pub state: String,
    pub last_seen: String,
    pub expect_every_seconds: Option<i64>,
    pub expect_by: Option<String>,
    /// When this signal is (or was) next due — useful for showing how late it is.
    pub due_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Signals {
    pub signals: Vec<Signal>,
}

/// `GET /v0/digest` — a roll-up rather than a stream (DESIGN.md §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestResponse {
    pub open: i64,
    pub oldest_created_at: Option<String>,
    pub silent_signals: Vec<String>,
}

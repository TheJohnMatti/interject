//! Deciding whether to ask at all.
//!
//! Everything else in this project moves a question to a human. This module is
//! the part that tries not to. Prior art solves transport — Slack approval bots,
//! Temporal signals, Airflow sensors — and leaves "should this interrupt anyone?"
//! to the caller. That is the interesting problem, and it is the one that can be
//! measured.
//!
//! The rule is deliberately conservative. A class is auto-answered only when all
//! of the following hold:
//!
//! 1. someone explicitly enabled it (`policies.enabled`, off by default);
//! 2. the suggestion's confidence clears `threshold`;
//! 3. there are at least `min_samples` past cases to judge by;
//! 4. measured agreement with humans on that class is at least `agreement_target`.
//!
//! Agreement needs no special machinery to bootstrap: every ordinary question
//! that carried a suggestion and was then answered by a human is a free data
//! point comparing the two. Once a class starts being auto-answered humans stop
//! seeing it, so a `shadow_rate` fraction of auto-answered questions are *also*
//! asked of a human. A shadow answer never changes the pipeline's result — it
//! exists only to keep the agreement estimate honest, forever, with nobody
//! auditing anything.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::store;
use crate::types::NewQuestion;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub question_id: String,
    pub threshold: f64,
    pub agreement_target: f64,
    pub shadow_rate: f64,
    pub min_samples: i64,
    pub enabled: bool,
}

impl Policy {
    /// The default policy: ask a human. Auto-answering is never implicit.
    pub fn disabled(question_id: &str) -> Self {
        Self {
            question_id: question_id.to_string(),
            threshold: 0.95,
            agreement_target: 0.98,
            shadow_rate: 0.1,
            min_samples: 20,
            enabled: false,
        }
    }
}

pub fn load_policy(conn: &Connection, project: &str, question_id: &str) -> Result<Policy> {
    let found = conn
        .query_row(
            "SELECT threshold, agreement_target, shadow_rate, min_samples, enabled
               FROM policies WHERE project = ?1 AND question_id = ?2",
            params![project, question_id],
            |row| {
                Ok(Policy {
                    question_id: question_id.to_string(),
                    threshold: row.get(0)?,
                    agreement_target: row.get(1)?,
                    shadow_rate: row.get(2)?,
                    min_samples: row.get(3)?,
                    enabled: row.get::<_, i64>(4)? != 0,
                })
            },
        )
        .optional()?;
    Ok(found.unwrap_or_else(|| Policy::disabled(question_id)))
}

pub fn save_policy(conn: &Connection, project: &str, policy: &Policy) -> Result<()> {
    conn.execute(
        "INSERT INTO policies
             (project, question_id, threshold, agreement_target, shadow_rate, min_samples, enabled)
         VALUES (?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(project, question_id) DO UPDATE SET
             threshold        = excluded.threshold,
             agreement_target = excluded.agreement_target,
             shadow_rate      = excluded.shadow_rate,
             min_samples      = excluded.min_samples,
             enabled          = excluded.enabled",
        params![
            project,
            policy.question_id,
            policy.threshold,
            policy.agreement_target,
            policy.shadow_rate,
            policy.min_samples,
            i64::from(policy.enabled),
        ],
    )?;
    Ok(())
}

pub fn policies(conn: &Connection, project: &str) -> Result<Vec<Policy>> {
    let mut statement = conn.prepare(
        "SELECT question_id, threshold, agreement_target, shadow_rate, min_samples, enabled
           FROM policies WHERE project = ?1 ORDER BY question_id",
    )?;
    let rows = statement.query_map(params![project], |row| {
        Ok(Policy {
            question_id: row.get(0)?,
            threshold: row.get(1)?,
            agreement_target: row.get(2)?,
            shadow_rate: row.get(3)?,
            min_samples: row.get(4)?,
            enabled: row.get::<_, i64>(5)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// How a class is doing: how often the machine agreed with the human, and how
/// many interruptions that bought.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Calibration {
    pub question_id: String,
    /// Questions that carried a suggestion and were answered by a human — the
    /// cases where the two can be compared.
    pub compared: i64,
    pub agreements: i64,
    pub agreement_rate: Option<f64>,
    pub auto_answered: i64,
    pub human_answered: i64,
    /// Fraction of answers that never reached a human.
    pub ask_reduction: Option<f64>,
    pub shadow_compared: i64,
    pub shadow_agreements: i64,
    pub enabled: bool,
}

/// Agreement between suggestion and human answer for one class.
///
/// Counts only human answers on questions that carried a suggestion, which
/// includes shadow questions — so a class that has stopped being shown to humans
/// keeps accumulating evidence through its shadow sample.
fn agreement_counts(conn: &Connection, project: &str, question_id: &str) -> Result<(i64, i64)> {
    let mut statement = conn.prepare(
        "SELECT q.suggest, a.value
           FROM questions q JOIN answers a ON a.question_key = q.key
          WHERE q.project = ?1 AND q.id = ?2
            AND q.suggest IS NOT NULL AND a.source = 'human'",
    )?;
    let rows = statement
        .query_map(params![project, question_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut compared = 0;
    let mut agreed = 0;
    for (suggest_raw, answer_raw) in rows {
        let suggested = serde_json::from_str::<Value>(&suggest_raw)
            .ok()
            .and_then(|s| s.get("value").cloned());
        let answered = serde_json::from_str::<Value>(&answer_raw).ok();
        if let (Some(suggested), Some(answered)) = (suggested, answered) {
            compared += 1;
            if suggested == answered {
                agreed += 1;
            }
        }
    }
    Ok((compared, agreed))
}

fn count(conn: &Connection, sql: &str, project: &str, question_id: &str) -> Result<i64> {
    Ok(conn.query_row(sql, params![project, question_id], |row| row.get(0))?)
}

pub fn calibration(conn: &Connection, project: &str, question_id: &str) -> Result<Calibration> {
    let (compared, agreements) = agreement_counts(conn, project, question_id)?;
    let auto_answered = count(
        conn,
        "SELECT COUNT(*) FROM questions q JOIN answers a ON a.question_key = q.key
          WHERE q.project = ?1 AND q.id = ?2 AND a.source = 'auto'",
        project,
        question_id,
    )?;
    let human_answered = count(
        conn,
        "SELECT COUNT(*) FROM questions q JOIN answers a ON a.question_key = q.key
          WHERE q.project = ?1 AND q.id = ?2 AND a.source = 'human' AND q.shadow_of IS NULL",
        project,
        question_id,
    )?;
    let (shadow_compared, shadow_agreements) = {
        let mut statement = conn.prepare(
            "SELECT q.suggest, a.value
               FROM questions q JOIN answers a ON a.question_key = q.key
              WHERE q.project = ?1 AND q.id = ?2 AND q.shadow_of IS NOT NULL
                AND a.source = 'human' AND q.suggest IS NOT NULL",
        )?;
        let rows = statement
            .query_map(params![project, question_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut total = 0;
        let mut agreed = 0;
        for (suggest_raw, answer_raw) in rows {
            let suggested = serde_json::from_str::<Value>(&suggest_raw)
                .ok()
                .and_then(|s| s.get("value").cloned());
            let answered = serde_json::from_str::<Value>(&answer_raw).ok();
            if let (Some(suggested), Some(answered)) = (suggested, answered) {
                total += 1;
                if suggested == answered {
                    agreed += 1;
                }
            }
        }
        (total, agreed)
    };

    let total_answers = auto_answered + human_answered;
    Ok(Calibration {
        question_id: question_id.to_string(),
        compared,
        agreements,
        agreement_rate: (compared > 0).then(|| agreements as f64 / compared as f64),
        auto_answered,
        human_answered,
        ask_reduction: (total_answers > 0).then(|| auto_answered as f64 / total_answers as f64),
        shadow_compared,
        shadow_agreements,
        enabled: load_policy(conn, project, question_id)?.enabled,
    })
}

/// Every class this project has seen, with its numbers.
pub fn calibration_report(conn: &Connection, project: &str) -> Result<Vec<Calibration>> {
    let mut statement =
        conn.prepare("SELECT DISTINCT id FROM questions WHERE project = ?1 ORDER BY id")?;
    let ids = statement
        .query_map(params![project], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ids.iter()
        .map(|id| calibration(conn, project, id))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Show it to a human.
    Ask,
    /// Answer it from the suggestion. `shadow` means also ask a human, for
    /// measurement only.
    Auto { shadow: bool },
}

/// A deterministic 0..1 draw for shadow sampling.
fn coin() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // Without randomness, sample everything rather than nothing: measuring
        // too much is a cost, measuring nothing is a silent loss of calibration.
        return 0.0;
    }
    (u64::from_le_bytes(bytes) as f64) / (u64::MAX as f64)
}

/// Decide what to do with a freshly registered question.
pub fn decide(conn: &Connection, project: &str, question: &NewQuestion) -> Result<Decision> {
    let Some(suggest) = &question.suggest else {
        return Ok(Decision::Ask);
    };
    let policy = load_policy(conn, project, &question.id)?;
    if !policy.enabled {
        return Ok(Decision::Ask);
    }
    let confidence = suggest
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if confidence < policy.threshold {
        return Ok(Decision::Ask);
    }
    if suggest.get("value").is_none() {
        return Ok(Decision::Ask);
    }

    // Cold start: with too little evidence, asking is the only defensible move.
    let (compared, agreements) = agreement_counts(conn, project, &question.id)?;
    if compared < policy.min_samples {
        return Ok(Decision::Ask);
    }
    if (agreements as f64 / compared as f64) < policy.agreement_target {
        return Ok(Decision::Ask);
    }
    Ok(Decision::Auto {
        shadow: coin() < policy.shadow_rate,
    })
}

/// Ask an external service to propose an answer (the daemon-supplied half of D6).
///
/// Deliberately a URL rather than an embedded model vendor: the daemon never
/// needs to hold an API key, and anyone can point it at whatever they already
/// run. The service receives the question and returns `{"value": ..,
/// "confidence": 0..1}`; anything else is treated as "no suggestion", because a
/// broken suggester must degrade to asking a human rather than to guessing.
pub fn fetch_suggestion(url: &str, question: &NewQuestion) -> Option<Value> {
    let payload = serde_json::json!({
        "id": question.id,
        "prompt": question.prompt,
        "kind": question.kind,
        "options": question.options,
        "context": question.context,
    });
    let mut response = ureq::post(url)
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .send_json(&payload)
        .map_err(|error| tracing::warn!(%url, %error, "suggester unreachable"))
        .ok()?;
    let suggestion: Value = response
        .body_mut()
        .read_json()
        .map_err(|error| tracing::warn!(%url, %error, "suggester returned unusable JSON"))
        .ok()?;

    let confidence = suggestion.get("confidence").and_then(Value::as_f64)?;
    if suggestion.get("value").is_none() || !(0.0..=1.0).contains(&confidence) {
        tracing::warn!(%url, "suggester returned no value or an out-of-range confidence");
        return None;
    }
    Some(suggestion)
}

/// The key of the shadow twin of a question. Derived so it is stable and cannot
/// collide with a real question's key.
pub fn shadow_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{key}\0shadow").as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Auto-answer a question from its suggestion, optionally creating the shadow
/// twin a human will answer for measurement.
pub fn apply_auto_answer(
    conn: &Connection,
    project: &str,
    question: &NewQuestion,
    shadow: bool,
) -> Result<()> {
    let value = question
        .suggest
        .as_ref()
        .and_then(|s| s.get("value").cloned())
        .unwrap_or(Value::Null);
    store::put_answer(conn, project, &question.key, &value, "auto", Some("triage"))?;

    if shadow {
        let mut twin: NewQuestion = serde_json::from_value(serde_json::to_value(question)?)?;
        twin.key = shadow_key(&question.key);
        twin.shadow_of = Some(question.key.clone());
        // Shadow questions are for measurement; they should never be the most
        // urgent thing in someone's inbox.
        twin.priority = 1;
        store::upsert_question(conn, project, &twin)?;
    }
    Ok(())
}

//! SQLite persistence. Timestamps are stored as ISO-8601 UTC **text** so that
//! lexical ordering equals chronological ordering, the same choice `punctual`
//! made for the same reason.

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use crate::types::{Answer, InboxBatch, InboxQuestion, NewQuestion, Signal, Snapshot};

pub const SCHEMA_VERSION: i64 = 1;

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {path}"))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);

        CREATE TABLE IF NOT EXISTS questions (
            key          TEXT PRIMARY KEY,
            project      TEXT NOT NULL,
            id           TEXT NOT NULL,
            prompt       TEXT NOT NULL,
            kind         TEXT NOT NULL,
            options      TEXT,
            context      TEXT,
            context_ref  TEXT,
            suggest      TEXT,
            default_value TEXT,
            has_default  INTEGER NOT NULL DEFAULT 0,
            on_timeout   TEXT NOT NULL,
            priority     INTEGER NOT NULL,
            batch_key    TEXT,
            origin       TEXT,
            shadow_of    TEXT,
            created_at   TEXT NOT NULL,
            expires_at   TEXT,
            state        TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS questions_open
            ON questions (project, state, priority DESC, created_at);
        CREATE INDEX IF NOT EXISTS questions_expiry
            ON questions (state, expires_at);

        CREATE TABLE IF NOT EXISTS answers (
            question_key TEXT PRIMARY KEY REFERENCES questions(key) ON DELETE CASCADE,
            value        TEXT NOT NULL,
            source       TEXT NOT NULL,
            answered_by  TEXT,
            answered_at  TEXT NOT NULL,
            latency_ms   INTEGER
        );

        CREATE TABLE IF NOT EXISTS signals (
            name                 TEXT NOT NULL,
            project              TEXT NOT NULL,
            expect_every_seconds INTEGER,
            expect_by            TEXT,
            last_seen            TEXT NOT NULL,
            state                TEXT NOT NULL,
            PRIMARY KEY (name, project)
        );
        "#,
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (k, v) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

fn to_text(value: &Option<Value>) -> Option<String> {
    value.as_ref().map(|v| v.to_string())
}

fn from_text(text: Option<String>) -> Option<Value> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

/// Register a question, or return the existing one untouched.
///
/// Idempotent on `key`: this is both the ask path and the replay path, so a
/// second call with the same key must never overwrite state or reset the TTL.
pub fn upsert_question(conn: &Connection, project: &str, q: &NewQuestion) -> Result<bool> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT project FROM questions WHERE key = ?1",
            params![q.key],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(owner) = existing {
        if owner != project {
            anyhow::bail!("key belongs to another project");
        }
        return Ok(false);
    }

    let created_at = now();
    let expires_at = q.ttl_seconds.and_then(|ttl| {
        parse_time(&created_at).map(|t| {
            (t + chrono::Duration::seconds(ttl)).to_rfc3339_opts(SecondsFormat::Millis, true)
        })
    });

    conn.execute(
        "INSERT INTO questions (
            key, project, id, prompt, kind, options, context, context_ref, suggest,
            default_value, has_default, on_timeout, priority, batch_key, origin,
            shadow_of, created_at, expires_at, state
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,'open')",
        params![
            q.key,
            project,
            q.id,
            q.prompt,
            q.kind,
            to_text(&q.options),
            to_text(&q.context),
            q.context_ref,
            to_text(&q.suggest),
            q.default.as_ref().map(|v| v.to_string()),
            i64::from(q.default.is_some()),
            q.on_timeout,
            q.priority,
            q.batch_key.clone().unwrap_or_else(|| q.id.clone()),
            to_text(&q.origin),
            q.shadow_of,
            created_at,
            expires_at,
        ],
    )?;
    Ok(true)
}

/// Mark every open question whose TTL has elapsed as expired.
///
/// Called before each read as well as by the background sweeper, so a question
/// is never reported as `open` after its deadline even if the sweeper is idle.
pub fn expire_due(conn: &Connection) -> Result<usize> {
    let changed = conn.execute(
        "UPDATE questions SET state = 'expired'
          WHERE state = 'open' AND expires_at IS NOT NULL AND expires_at <= ?1",
        params![now()],
    )?;
    Ok(changed)
}

fn read_answer(conn: &Connection, key: &str) -> Result<Option<Answer>> {
    let answer = conn
        .query_row(
            "SELECT value, source, answered_by, answered_at, latency_ms
               FROM answers WHERE question_key = ?1",
            params![key],
            |row: &Row| {
                let raw: String = row.get(0)?;
                Ok(Answer {
                    value: serde_json::from_str(&raw).unwrap_or(Value::Null),
                    source: row.get(1)?,
                    answered_by: row.get(2)?,
                    answered_at: row.get(3)?,
                    latency_ms: row.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(answer)
}

pub fn snapshot(conn: &Connection, project: &str, key: &str) -> Result<Option<Snapshot>> {
    expire_due(conn)?;
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT state, expires_at FROM questions WHERE key = ?1 AND project = ?2",
            params![key, project],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((state, expires_at)) = row else {
        return Ok(None);
    };
    Ok(Some(Snapshot {
        key: key.to_string(),
        state,
        answer: read_answer(conn, key)?,
        expires_at,
        created: None,
    }))
}

pub enum AnswerOutcome {
    Stored(Snapshot),
    UnknownKey,
    AlreadyAnswered,
}

pub fn put_answer(
    conn: &Connection,
    project: &str,
    key: &str,
    value: &Value,
    source: &str,
    answered_by: Option<&str>,
) -> Result<AnswerOutcome> {
    expire_due(conn)?;
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT state, created_at FROM questions WHERE key = ?1 AND project = ?2",
            params![key, project],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((state, created_at)) = row else {
        return Ok(AnswerOutcome::UnknownKey);
    };
    if state == "answered" {
        return Ok(AnswerOutcome::AlreadyAnswered);
    }

    let answered_at = now();
    let latency_ms = match (parse_time(&created_at), parse_time(&answered_at)) {
        (Some(start), Some(end)) => Some((end - start).num_milliseconds()),
        _ => None,
    };

    conn.execute(
        "INSERT INTO answers (question_key, value, source, answered_by, answered_at, latency_ms)
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            key,
            value.to_string(),
            source,
            answered_by,
            answered_at,
            latency_ms
        ],
    )?;
    // An expired question can still be answered — applying a declared default is
    // itself an answer, and it must be recorded so a replay is deterministic.
    conn.execute(
        "UPDATE questions SET state = 'answered' WHERE key = ?1",
        params![key],
    )?;

    let snapshot = snapshot(conn, project, key)?
        .ok_or_else(|| anyhow::anyhow!("question vanished while being answered"))?;
    Ok(AnswerOutcome::Stored(snapshot))
}

/// Open questions, grouped by `batch_key` so a surface renders one screen per
/// class rather than one per question (DESIGN.md §5).
pub fn inbox(
    conn: &Connection,
    project: &str,
    limit: usize,
    batch_key: Option<&str>,
) -> Result<Vec<InboxBatch>> {
    expire_due(conn)?;
    let mut statement = conn.prepare(
        "SELECT key, id, prompt, kind, options, context, context_ref, suggest,
                created_at, expires_at, priority, batch_key
           FROM questions
          WHERE project = ?1 AND state = 'open'
            AND (?2 IS NULL OR batch_key = ?2)
          ORDER BY priority DESC, created_at ASC
          LIMIT ?3",
    )?;
    let rows = statement.query_map(params![project, batch_key, limit as i64], |row| {
        Ok((
            InboxQuestion {
                key: row.get(0)?,
                id: row.get(1)?,
                prompt: row.get(2)?,
                context: from_text(row.get(5)?),
                context_ref: row.get(6)?,
                suggest: from_text(row.get(7)?),
                created_at: row.get(8)?,
                expires_at: row.get(9)?,
                priority: row.get(10)?,
            },
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            from_text(row.get(4)?),
            row.get::<_, String>(11)?,
        ))
    })?;

    let mut batches: Vec<InboxBatch> = Vec::new();
    for row in rows {
        let (question, prompt, kind, options, batch) = row?;
        match batches.iter_mut().find(|b| b.batch_key == batch) {
            Some(existing) => existing.questions.push(question),
            None => batches.push(InboxBatch {
                batch_key: batch,
                prompt,
                kind,
                options,
                count: 0,
                questions: vec![question],
            }),
        }
    }
    for batch in &mut batches {
        batch.count = batch.questions.len();
    }
    Ok(batches)
}

pub fn heartbeat(
    conn: &Connection,
    project: &str,
    name: &str,
    expect_every_seconds: Option<i64>,
    expect_by: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO signals (name, project, expect_every_seconds, expect_by, last_seen, state)
              VALUES (?1,?2,?3,?4,?5,'live')
         ON CONFLICT(name, project) DO UPDATE SET
              expect_every_seconds = excluded.expect_every_seconds,
              expect_by            = excluded.expect_by,
              last_seen            = excluded.last_seen,
              state                = 'live'",
        params![name, project, expect_every_seconds, expect_by, now()],
    )?;
    Ok(())
}

/// Compute when a signal is next due, from whichever deadline it declared.
fn due_at(
    last_seen: &str,
    expect_every_seconds: Option<i64>,
    expect_by: Option<&str>,
) -> Option<String> {
    if let Some(by) = expect_by {
        return Some(by.to_string());
    }
    let every = expect_every_seconds?;
    let seen = parse_time(last_seen)?;
    Some((seen + chrono::Duration::seconds(every)).to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Flip signals whose deadline has passed to `silent`.
///
/// Returns the names that transitioned on this call, so the caller can notify
/// exactly once per transition rather than on every sweep.
pub fn detect_silence(conn: &Connection, project: Option<&str>) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT name, project, expect_every_seconds, expect_by, last_seen
           FROM signals
          WHERE state = 'live' AND (?1 IS NULL OR project = ?1)",
    )?;
    let rows = statement
        .query_map(params![project], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let current = now();
    let mut gone_silent = Vec::new();
    for (name, proj, every, by, last_seen) in rows {
        let Some(due) = due_at(&last_seen, every, by.as_deref()) else {
            continue;
        };
        if due <= current {
            conn.execute(
                "UPDATE signals SET state = 'silent' WHERE name = ?1 AND project = ?2",
                params![name, proj],
            )?;
            gone_silent.push(name);
        }
    }
    Ok(gone_silent)
}

pub fn signals(conn: &Connection, project: &str) -> Result<Vec<Signal>> {
    detect_silence(conn, Some(project))?;
    let mut statement = conn.prepare(
        "SELECT name, state, last_seen, expect_every_seconds, expect_by
           FROM signals WHERE project = ?1 ORDER BY name",
    )?;
    let rows = statement.query_map(params![project], |row| {
        let name: String = row.get(0)?;
        let state: String = row.get(1)?;
        let last_seen: String = row.get(2)?;
        let every: Option<i64> = row.get(3)?;
        let by: Option<String> = row.get(4)?;
        Ok(Signal {
            due_at: due_at(&last_seen, every, by.as_deref()),
            name,
            state,
            last_seen,
            expect_every_seconds: every,
            expect_by: by,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

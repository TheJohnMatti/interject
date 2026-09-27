//! SQLite persistence. Timestamps are stored as ISO-8601 UTC **text** so that
//! lexical ordering equals chronological ordering, the same choice `punctual`
//! made for the same reason.

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use crate::types::{Answer, InboxBatch, InboxQuestion, NewQuestion, Signal, Snapshot};

pub const SCHEMA_VERSION: i64 = 6;

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

fn schema_version(conn: &Connection) -> Result<i64> {
    let raw: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = 'schema_version'", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(raw.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Stepwise migrations. Each step moves the schema up exactly one version, so an
/// old database on disk upgrades in place rather than needing to be thrown away.
fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);")?;

    let mut version = schema_version(conn)?;
    while version < SCHEMA_VERSION {
        match version {
            0 => conn.execute_batch(V1)?,
            1 => conn.execute_batch(V2)?,
            2 => conn.execute_batch(V3)?,
            3 => conn.execute_batch(V4)?,
            4 => conn.execute_batch(V5)?,
            5 => conn.execute_batch(V6)?,
            other => anyhow::bail!("no migration from schema version {other}"),
        }
        version += 1;
        conn.execute(
            "INSERT OR REPLACE INTO meta (k, v) VALUES ('schema_version', ?1)",
            params![version.to_string()],
        )?;
    }
    if version > SCHEMA_VERSION {
        anyhow::bail!(
            "database is at schema version {version}, newer than this build understands ({SCHEMA_VERSION})"
        );
    }
    Ok(())
}

const V1: &str = r#"
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
"#;

/// v6 adds routing (O1): who a question is for, and who is currently looking at
/// it. Claims are advisory — the real protection against two people answering
/// the same thing is that answers are write-once — so they carry an expiry and
/// nothing depends on them being released.
const V6: &str = r#"
    ALTER TABLE questions ADD COLUMN assigned_to TEXT;
    ALTER TABLE questions ADD COLUMN claimed_by TEXT;
    ALTER TABLE questions ADD COLUMN claim_expires_at TEXT;
    CREATE INDEX IF NOT EXISTS questions_assigned
        ON questions (project, state, assigned_to);
"#;

/// v5 records when a question's stored context was dropped, which resolves the
/// retention question the design left open (O2): what grows forever, and does
/// clearing it break replay?
const V5: &str = r#"
    ALTER TABLE questions ADD COLUMN pruned_at TEXT;
    CREATE INDEX IF NOT EXISTS questions_prunable
        ON questions (state, pruned_at);
"#;

/// v4 adds real tokens and device pairing (D4, D9). Tokens are stored only as
/// SHA-256 digests, so a stolen database does not hand over working credentials.
const V4: &str = r#"
    CREATE TABLE IF NOT EXISTS tokens (
        token_sha256 TEXT PRIMARY KEY,
        project      TEXT NOT NULL,
        label        TEXT,
        kind         TEXT NOT NULL,
        created_at   TEXT NOT NULL,
        last_used_at TEXT
    );
    CREATE TABLE IF NOT EXISTS pairings (
        code       TEXT PRIMARY KEY,
        project    TEXT NOT NULL,
        label      TEXT,
        expires_at TEXT NOT NULL,
        used_at    TEXT
    );
"#;

/// v3 adds per-class triage policy. `enabled` defaults to 0: auto-answering is
/// opt-in per question class, because a tool that starts deciding things on a
/// human's behalf without being asked would be the wrong default.
const V3: &str = r#"
    CREATE TABLE IF NOT EXISTS policies (
        project          TEXT NOT NULL,
        question_id      TEXT NOT NULL,
        threshold        REAL NOT NULL DEFAULT 0.95,
        agreement_target REAL NOT NULL DEFAULT 0.98,
        shadow_rate      REAL NOT NULL DEFAULT 0.1,
        min_samples      INTEGER NOT NULL DEFAULT 20,
        enabled          INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (project, question_id)
    );
    CREATE INDEX IF NOT EXISTS questions_by_class ON questions (project, id, state);
"#;

/// v2 adds the bookkeeping a notifier needs: when a question was last announced,
/// so coalescing can announce a batch once instead of on every sweep.
const V2: &str = r#"
    ALTER TABLE questions ADD COLUMN notified_at TEXT;
    CREATE INDEX IF NOT EXISTS questions_unnotified
        ON questions (project, state, notified_at);
"#;

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
            shadow_of, created_at, expires_at, assigned_to, state
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,'open')",
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
            q.assign_to,
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

/// A stable per-daemon secret, created on first use and kept in `meta`.
///
/// Answer callbacks are authenticated against it (see `notify::answer_token`),
/// so knowing a notification topic is not enough to answer questions.
pub fn answer_secret(conn: &Connection) -> Result<String> {
    let existing: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = 'answer_secret'", [], |row| {
            row.get(0)
        })
        .optional()?;
    if let Some(secret) = existing {
        return Ok(secret);
    }
    // 256 bits from the OS, via getrandom on every platform we target.
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    conn.execute(
        "INSERT INTO meta (k, v) VALUES ('answer_secret', ?1)",
        params![secret],
    )?;
    Ok(secret)
}

/// Open questions that have never been announced, grouped like the inbox.
///
/// Coalescing is mandatory rather than optional (DESIGN.md §5): four hundred
/// questions of one class must reach a human as one notification.
pub fn pending_notifications(conn: &Connection, limit: usize) -> Result<Vec<InboxBatch>> {
    expire_due(conn)?;
    let mut statement = conn.prepare(
        "SELECT key, id, prompt, kind, options, context, context_ref, suggest,
                created_at, expires_at, priority, batch_key, project
           FROM questions
          WHERE state = 'open' AND notified_at IS NULL
          ORDER BY priority DESC, created_at ASC
          LIMIT ?1",
    )?;
    let rows = statement.query_map(params![limit as i64], |row| {
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
            row.get::<_, String>(12)?,
        ))
    })?;

    let mut batches: Vec<InboxBatch> = Vec::new();
    for row in rows {
        let (question, prompt, kind, options, batch, project) = row?;
        // Two projects may legitimately use the same batch name, so the grouping
        // key has to include the project.
        let batch_key = format!("{project}/{batch}");
        match batches.iter_mut().find(|b| b.batch_key == batch_key) {
            Some(existing) => existing.questions.push(question),
            None => batches.push(InboxBatch {
                batch_key,
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

pub fn mark_notified(conn: &Connection, keys: &[String]) -> Result<()> {
    let stamp = now();
    for key in keys {
        conn.execute(
            "UPDATE questions SET notified_at = ?1 WHERE key = ?2",
            params![stamp, key],
        )?;
    }
    Ok(())
}

/// Look up which project owns a key, so an answer arriving out of band (from a
/// phone tap, say) can be applied without the caller naming a project.
pub fn project_of_key(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT project FROM questions WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?)
}

/// Counts for a digest: what is waiting, and how stale the oldest item is.
pub struct Digest {
    pub open: i64,
    pub oldest_created_at: Option<String>,
    pub silent_signals: Vec<String>,
}

pub fn digest(conn: &Connection, project: &str) -> Result<Digest> {
    expire_due(conn)?;
    detect_silence(conn, Some(project))?;
    let open: i64 = conn.query_row(
        "SELECT COUNT(*) FROM questions WHERE project = ?1 AND state = 'open'",
        params![project],
        |row| row.get(0),
    )?;
    let oldest_created_at: Option<String> = conn
        .query_row(
            "SELECT MIN(created_at) FROM questions WHERE project = ?1 AND state = 'open'",
            params![project],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let mut statement = conn.prepare(
        "SELECT name FROM signals WHERE project = ?1 AND state = 'silent' ORDER BY name",
    )?;
    let silent_signals = statement
        .query_map(params![project], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Digest {
        open,
        oldest_created_at,
        silent_signals,
    })
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    Ok(buffer.iter().map(|b| format!("{b:02x}")).collect())
}

/// Mint a token for a project. The plaintext is returned once and never stored.
pub fn create_token(
    conn: &Connection,
    project: &str,
    label: Option<&str>,
    kind: &str,
) -> Result<String> {
    let token = random_hex(24)?;
    conn.execute(
        "INSERT INTO tokens (token_sha256, project, label, kind, created_at)
         VALUES (?1,?2,?3,?4,?5)",
        params![sha256_hex(&token), project, label, kind, now()],
    )?;
    Ok(token)
}

/// Who a bearer token speaks for: its project, and the label it was minted with.
///
/// The label doubles as an identity for routing (O1) — a device token labelled
/// "john-phone" names its answerer without needing a user system.
pub fn resolve_token(conn: &Connection, token: &str) -> Result<Option<(String, Option<String>)>> {
    let digest = sha256_hex(token);
    let project: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT project, label FROM tokens WHERE token_sha256 = ?1",
            params![digest],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if project.is_some() {
        conn.execute(
            "UPDATE tokens SET last_used_at = ?1 WHERE token_sha256 = ?2",
            params![now(), digest],
        )?;
    }
    Ok(project)
}

/// Whether the daemon has been deliberately locked down.
///
/// Only *project* tokens count. Pairing a phone mints a `device` token, and
/// someone adding a phone to their own laptop's daemon has not asked for their
/// local pipelines to start failing with 401 — which is exactly what happened
/// the first time this was demonstrated. Locking down is a separate act:
/// `interjectd token create`, or `--token`.
pub fn any_tokens(conn: &Connection) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tokens WHERE kind = 'project'",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub struct TokenRow {
    pub project: String,
    pub label: Option<String>,
    pub kind: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

pub fn tokens(conn: &Connection) -> Result<Vec<TokenRow>> {
    let mut statement = conn.prepare(
        "SELECT project, label, kind, created_at, last_used_at FROM tokens ORDER BY created_at",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(TokenRow {
            project: row.get(0)?,
            label: row.get(1)?,
            kind: row.get(2)?,
            created_at: row.get(3)?,
            last_used_at: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A short, single-use pairing code (D9). Short because a human retypes it on a
/// phone; safe because it expires in minutes and can only be redeemed once.
pub fn create_pairing(
    conn: &Connection,
    project: &str,
    label: Option<&str>,
    ttl_seconds: i64,
) -> Result<String> {
    let code = random_hex(4).map(|hex| hex.to_uppercase())?;
    let expires_at = parse_time(&now())
        .map(|t| {
            (t + chrono::Duration::seconds(ttl_seconds))
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        })
        .unwrap_or_else(now);
    conn.execute(
        "INSERT INTO pairings (code, project, label, expires_at) VALUES (?1,?2,?3,?4)",
        params![code, project, label, expires_at],
    )?;
    Ok(code)
}

pub enum PairOutcome {
    Paired { token: String, project: String },
    UnknownOrExpired,
}

/// Redeem a pairing code for a long-lived device token.
pub fn redeem_pairing(conn: &Connection, code: &str) -> Result<PairOutcome> {
    let row: Option<(String, Option<String>, String, Option<String>)> = conn
        .query_row(
            "SELECT project, label, expires_at, used_at FROM pairings WHERE code = ?1",
            params![code.to_uppercase()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((project, label, expires_at, used_at)) = row else {
        return Ok(PairOutcome::UnknownOrExpired);
    };
    // Single use, and only before it expires.
    if used_at.is_some() || expires_at <= now() {
        return Ok(PairOutcome::UnknownOrExpired);
    }

    let token = create_token(conn, &project, label.as_deref(), "device")?;
    conn.execute(
        "UPDATE pairings SET used_at = ?1 WHERE code = ?2",
        params![now(), code.to_uppercase()],
    )?;
    Ok(PairOutcome::Paired { token, project })
}

/// Drop the stored context of questions settled more than `retain_days` ago.
///
/// Resolves O2. Three things had to be decided, and the answers are not
/// symmetric:
///
/// * **The answer row is never touched.** Replay is the load-bearing property of
///   this whole design, so a pruned question must still hand back its answer.
/// * **`context` is what gets dropped**, because it is the only field that can
///   be arbitrarily large — a caller may put a whole listing in it.
/// * **`suggest` is kept**, despite also being droppable, because the
///   calibration history is computed from `suggest` against the human's answer.
///   Pruning it would quietly rewrite the agreement numbers, which is precisely
///   the kind of silent degradation the triage layer exists to avoid.
pub fn prune_context(conn: &Connection, retain_days: i64) -> Result<usize> {
    if retain_days <= 0 {
        return Ok(0);
    }
    let cutoff = parse_time(&now())
        .map(|t| {
            (t - chrono::Duration::days(retain_days)).to_rfc3339_opts(SecondsFormat::Millis, true)
        })
        .unwrap_or_else(now);

    let pruned = conn.execute(
        "UPDATE questions
            SET context = NULL, pruned_at = ?1
          WHERE state IN ('answered', 'expired')
            AND pruned_at IS NULL
            AND context IS NOT NULL
            AND created_at <= ?2",
        params![now(), cutoff],
    )?;
    Ok(pruned)
}

/// Who a question is for, and who is currently looking at it.
pub struct Routing {
    pub assigned_to: Option<String>,
    pub claimed_by: Option<String>,
    pub claim_expires_at: Option<String>,
}

pub enum ClaimOutcome {
    Claimed(Routing),
    /// Someone else holds a live claim. Advisory, so callers may still answer.
    HeldBy(String),
    UnknownKey,
    AlreadyAnswered,
}

/// Take an advisory claim on a question.
///
/// Claims exist so two people do not spend effort on the same question, not to
/// make answering safe — that is what write-once answers are for. They expire so
/// that someone who wanders off mid-question does not block it forever.
pub fn claim(
    conn: &Connection,
    project: &str,
    key: &str,
    by: &str,
    ttl_seconds: i64,
) -> Result<ClaimOutcome> {
    expire_due(conn)?;
    let row: Option<(String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT state, claimed_by, claim_expires_at
               FROM questions WHERE key = ?1 AND project = ?2",
            params![key, project],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((state, claimed_by, claim_expires_at)) = row else {
        return Ok(ClaimOutcome::UnknownKey);
    };
    if state != "open" {
        return Ok(ClaimOutcome::AlreadyAnswered);
    }

    let current = now();
    let held_by_other = match (&claimed_by, &claim_expires_at) {
        (Some(holder), Some(expiry)) => holder != by && expiry.as_str() > current.as_str(),
        _ => false,
    };
    if held_by_other {
        return Ok(ClaimOutcome::HeldBy(claimed_by.unwrap_or_default()));
    }

    let expires_at = parse_time(&current)
        .map(|t| {
            (t + chrono::Duration::seconds(ttl_seconds))
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        })
        .unwrap_or_else(now);
    conn.execute(
        "UPDATE questions SET claimed_by = ?1, claim_expires_at = ?2 WHERE key = ?3",
        params![by, expires_at, key],
    )?;
    Ok(ClaimOutcome::Claimed(Routing {
        assigned_to: None,
        claimed_by: Some(by.to_string()),
        claim_expires_at: Some(expires_at),
    }))
}

/// Give up a claim, so the question reappears for everyone immediately.
pub fn release_claim(conn: &Connection, project: &str, key: &str, by: &str) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE questions SET claimed_by = NULL, claim_expires_at = NULL
          WHERE key = ?1 AND project = ?2 AND claimed_by = ?3",
        params![key, project, by],
    )?;
    Ok(changed > 0)
}

pub fn assign(conn: &Connection, project: &str, key: &str, to: Option<&str>) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE questions SET assigned_to = ?1 WHERE key = ?2 AND project = ?3",
        params![to, key, project],
    )?;
    Ok(changed > 0)
}

/// What an inbox read should leave out.
pub struct InboxFilter<'a> {
    /// Show what this person should work on: assigned to them, or to nobody.
    pub for_identity: Option<&'a str>,
    /// Include questions someone else is currently looking at.
    pub include_claimed: bool,
}

/// Open questions for one person, grouped by batch.
///
/// Two exclusions, both advisory rather than enforced: questions assigned to
/// somebody else, and questions another person holds a live claim on.
pub fn inbox_for(
    conn: &Connection,
    project: &str,
    limit: usize,
    batch_key: Option<&str>,
    filter: &InboxFilter<'_>,
) -> Result<Vec<InboxBatch>> {
    expire_due(conn)?;
    let current = now();
    let mut statement = conn.prepare(
        "SELECT key, id, prompt, kind, options, context, context_ref, suggest,
                created_at, expires_at, priority, batch_key
           FROM questions
          WHERE project = ?1 AND state = 'open'
            AND (?2 IS NULL OR batch_key = ?2)
            AND (?3 IS NULL OR assigned_to IS NULL OR assigned_to = ?3)
            AND (?4 = 1
                 OR claimed_by IS NULL
                 OR (?3 IS NOT NULL AND claimed_by = ?3)
                 OR claim_expires_at IS NULL
                 OR claim_expires_at <= ?5)
          ORDER BY priority DESC, created_at ASC
          LIMIT ?6",
    )?;
    let rows = statement.query_map(
        params![
            project,
            batch_key,
            filter.for_identity,
            i64::from(filter.include_claimed),
            current,
            limit as i64
        ],
        |row| {
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
        },
    )?;

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

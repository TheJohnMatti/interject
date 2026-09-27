//! Retention (design question O2): what grows forever, and does clearing it
//! break replay?

use interjectd::store;
use interjectd::triage;
use interjectd::types::NewQuestion;
use rusqlite::Connection;
use serde_json::json;

fn conn() -> Connection {
    store::open(":memory:").expect("in-memory store")
}

fn question(n: usize) -> NewQuestion {
    serde_json::from_value(json!({
        "key": format!("{n:064x}"),
        "id": "vehicle_type",
        "prompt": "Is this a car?",
        "kind": "choice",
        "options": ["car", "motorcycle"],
        "context": {"title": "2018 Honda CBR", "blob": "x".repeat(1000)},
        "suggest": {"value": "motorcycle", "confidence": 0.94},
    }))
    .expect("fixture")
}

/// Backdate a question so retention has something old to find.
fn age(conn: &Connection, key: &str, days: i64) {
    let when = (chrono::Utc::now() - chrono::Duration::days(days))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    conn.execute(
        "UPDATE questions SET created_at = ?1 WHERE key = ?2",
        rusqlite::params![when, key],
    )
    .unwrap();
}

fn stored_context(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT context FROM questions WHERE key = ?1",
        rusqlite::params![key],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn retention_off_by_default_keeps_everything() {
    let conn = conn();
    let q = question(1);
    store::upsert_question(&conn, "p", &q).unwrap();
    store::put_answer(&conn, "p", &q.key, &json!("motorcycle"), "human", None).unwrap();
    age(&conn, &q.key, 999);

    assert_eq!(store::prune_context(&conn, 0).unwrap(), 0);
    assert!(stored_context(&conn, &q.key).is_some());
}

#[test]
fn an_open_question_is_never_pruned_however_old() {
    let conn = conn();
    let q = question(2);
    store::upsert_question(&conn, "p", &q).unwrap();
    age(&conn, &q.key, 999);

    // Its context is the only thing that makes it answerable.
    assert_eq!(store::prune_context(&conn, 1).unwrap(), 0);
    assert!(stored_context(&conn, &q.key).is_some());
}

#[test]
fn a_recently_settled_question_is_kept() {
    let conn = conn();
    let q = question(3);
    store::upsert_question(&conn, "p", &q).unwrap();
    store::put_answer(&conn, "p", &q.key, &json!("motorcycle"), "human", None).unwrap();

    assert_eq!(store::prune_context(&conn, 30).unwrap(), 0);
    assert!(stored_context(&conn, &q.key).is_some());
}

#[test]
fn pruning_drops_the_context_but_never_the_answer() {
    let conn = conn();
    let q = question(4);
    store::upsert_question(&conn, "p", &q).unwrap();
    store::put_answer(
        &conn,
        "p",
        &q.key,
        &json!("motorcycle"),
        "human",
        Some("john"),
    )
    .unwrap();
    age(&conn, &q.key, 60);

    assert_eq!(store::prune_context(&conn, 30).unwrap(), 1);
    assert!(stored_context(&conn, &q.key).is_none());

    // Replay is the load-bearing property of the whole design: a pruned question
    // must still hand back its answer.
    let snapshot = store::snapshot(&conn, "p", &q.key).unwrap().unwrap();
    assert_eq!(snapshot.state, "answered");
    assert_eq!(snapshot.answer.unwrap().value, json!("motorcycle"));
}

#[test]
fn pruning_keeps_the_suggestion_so_calibration_does_not_drift() {
    let conn = conn();
    for n in 0..4 {
        let q = question(100 + n);
        store::upsert_question(&conn, "p", &q).unwrap();
        store::put_answer(&conn, "p", &q.key, &json!("motorcycle"), "human", None).unwrap();
        age(&conn, &q.key, 60);
    }
    let before = triage::calibration(&conn, "p", "vehicle_type").unwrap();
    assert_eq!((before.compared, before.agreements), (4, 4));

    assert_eq!(store::prune_context(&conn, 30).unwrap(), 4);

    // Agreement is computed from suggest against the human's answer, so dropping
    // suggest would silently rewrite these numbers.
    let after = triage::calibration(&conn, "p", "vehicle_type").unwrap();
    assert_eq!((after.compared, after.agreements), (4, 4));
    assert_eq!(after.agreement_rate, Some(1.0));
}

#[test]
fn pruning_is_idempotent() {
    let conn = conn();
    let q = question(5);
    store::upsert_question(&conn, "p", &q).unwrap();
    store::put_answer(&conn, "p", &q.key, &json!("car"), "human", None).unwrap();
    age(&conn, &q.key, 60);

    assert_eq!(store::prune_context(&conn, 30).unwrap(), 1);
    // A second sweep must not keep rewriting rows it already cleared.
    assert_eq!(store::prune_context(&conn, 30).unwrap(), 0);
}

#[test]
fn an_expired_question_is_prunable_too() {
    let conn = conn();
    let mut value = serde_json::to_value(question(6)).unwrap();
    value["ttl_seconds"] = json!(-1);
    let q: NewQuestion = serde_json::from_value(value).unwrap();
    store::upsert_question(&conn, "p", &q).unwrap();
    store::expire_due(&conn).unwrap();
    age(&conn, &q.key, 60);

    assert_eq!(store::prune_context(&conn, 30).unwrap(), 1);
}

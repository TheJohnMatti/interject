//! Store-level tests: identity, idempotency, expiry, answer outcomes, grouping.

use interjectd::store::{self, AnswerOutcome};
use interjectd::types::NewQuestion;
use serde_json::{json, Value};

fn question(key: &str, id: &str) -> NewQuestion {
    serde_json::from_value(json!({
        "key": key,
        "id": id,
        "prompt": "Is this a car?",
        "kind": "choice",
        "options": ["car", "motorcycle"],
        "context": {"title": "CBR"},
        "on_timeout": "suspend",
        "priority": 5,
    }))
    .expect("test fixture should deserialise")
}

fn key_of(n: u8) -> String {
    std::iter::repeat_n(format!("{n:02x}"), 32).collect()
}

fn conn() -> rusqlite::Connection {
    store::open(":memory:").expect("in-memory store should open")
}

#[test]
fn registering_twice_is_idempotent_and_does_not_reset_state() {
    let conn = conn();
    let key = key_of(1);
    assert!(store::upsert_question(&conn, "p", &question(&key, "vt")).unwrap());
    // The second registration is the replay path: it must not create a new row.
    assert!(!store::upsert_question(&conn, "p", &question(&key, "vt")).unwrap());

    store::put_answer(&conn, "p", &key, &json!("car"), "human", Some("john")).unwrap();
    assert!(!store::upsert_question(&conn, "p", &question(&key, "vt")).unwrap());

    let snapshot = store::snapshot(&conn, "p", &key).unwrap().unwrap();
    assert_eq!(snapshot.state, "answered");
    assert_eq!(snapshot.answer.unwrap().value, json!("car"));
}

#[test]
fn a_key_is_scoped_to_its_project() {
    let conn = conn();
    let key = key_of(2);
    store::upsert_question(&conn, "one", &question(&key, "vt")).unwrap();

    // Same key, different project: a collision across tenants must be refused
    // rather than silently answered with another project's data.
    assert!(store::upsert_question(&conn, "two", &question(&key, "vt")).is_err());
    assert!(store::snapshot(&conn, "two", &key).unwrap().is_none());
    assert!(store::snapshot(&conn, "one", &key).unwrap().is_some());
}

#[test]
fn answering_an_unknown_key_is_reported_not_invented() {
    let conn = conn();
    let outcome = store::put_answer(&conn, "p", &key_of(3), &json!("car"), "human", None).unwrap();
    assert!(matches!(outcome, AnswerOutcome::UnknownKey));
}

#[test]
fn answers_are_write_once() {
    let conn = conn();
    let key = key_of(4);
    store::upsert_question(&conn, "p", &question(&key, "vt")).unwrap();
    store::put_answer(&conn, "p", &key, &json!("car"), "human", None).unwrap();

    let second = store::put_answer(&conn, "p", &key, &json!("motorcycle"), "human", None).unwrap();
    assert!(matches!(second, AnswerOutcome::AlreadyAnswered));

    let snapshot = store::snapshot(&conn, "p", &key).unwrap().unwrap();
    assert_eq!(snapshot.answer.unwrap().value, json!("car"));
}

#[test]
fn a_null_default_stays_distinguishable_from_no_default() {
    let conn = conn();
    let with_null: NewQuestion = serde_json::from_value(json!({
        "key": key_of(5), "id": "a", "prompt": "?", "kind": "text", "default": null,
    }))
    .unwrap();
    let without: NewQuestion = serde_json::from_value(json!({
        "key": key_of(6), "id": "b", "prompt": "?", "kind": "text",
    }))
    .unwrap();

    assert_eq!(with_null.default, Some(Value::Null));
    assert_eq!(without.default, None);
    store::upsert_question(&conn, "p", &with_null).unwrap();
    store::upsert_question(&conn, "p", &without).unwrap();
}

#[test]
fn a_ttl_in_the_past_expires_on_the_next_read() {
    let conn = conn();
    let key = key_of(7);
    let expiring: NewQuestion = serde_json::from_value(json!({
        "key": key, "id": "ttl", "prompt": "?", "kind": "text", "ttl_seconds": -1,
    }))
    .unwrap();
    store::upsert_question(&conn, "p", &expiring).unwrap();

    // Lazy expiry: a read must never report an open question past its deadline,
    // even when the background sweeper has not run.
    let snapshot = store::snapshot(&conn, "p", &key).unwrap().unwrap();
    assert_eq!(snapshot.state, "expired");
}

#[test]
fn an_expired_question_can_still_record_its_default() {
    let conn = conn();
    let key = key_of(8);
    let expiring: NewQuestion = serde_json::from_value(json!({
        "key": key, "id": "ttl", "prompt": "?", "kind": "text",
        "ttl_seconds": -1, "default": "skipped",
    }))
    .unwrap();
    store::upsert_question(&conn, "p", &expiring).unwrap();
    assert_eq!(store::expire_due(&conn).unwrap(), 1);

    // D8: applying a default is itself an answer, and must be durable so that a
    // replay returns the same value instead of defaulting a second time.
    let outcome = store::put_answer(&conn, "p", &key, &json!("skipped"), "default", None).unwrap();
    assert!(matches!(outcome, AnswerOutcome::Stored(_)));
    let snapshot = store::snapshot(&conn, "p", &key).unwrap().unwrap();
    assert_eq!(snapshot.state, "answered");
    assert_eq!(snapshot.answer.unwrap().source, "default");
}

#[test]
fn the_inbox_groups_by_batch_and_orders_by_priority() {
    let conn = conn();
    for (n, (id, batch, priority)) in [
        ("vt", "vehicle", 5),
        ("vt", "vehicle", 9),
        ("label", "labels", 5),
    ]
    .iter()
    .enumerate()
    {
        let q: NewQuestion = serde_json::from_value(json!({
            "key": key_of(20 + n as u8), "id": id, "prompt": "?", "kind": "text",
            "batch_key": batch, "priority": priority,
        }))
        .unwrap();
        store::upsert_question(&conn, "p", &q).unwrap();
    }

    let batches = store::inbox(&conn, "p", 50, None).unwrap();
    assert_eq!(batches.len(), 2);
    let vehicle = batches.iter().find(|b| b.batch_key == "vehicle").unwrap();
    assert_eq!(vehicle.count, 2);
    // Highest priority first within a batch.
    assert_eq!(vehicle.questions[0].priority, 9);

    let only_labels = store::inbox(&conn, "p", 50, Some("labels")).unwrap();
    assert_eq!(only_labels.len(), 1);
}

#[test]
fn answered_questions_leave_the_inbox() {
    let conn = conn();
    let key = key_of(9);
    store::upsert_question(&conn, "p", &question(&key, "vt")).unwrap();
    assert_eq!(store::inbox(&conn, "p", 50, None).unwrap().len(), 1);

    store::put_answer(&conn, "p", &key, &json!("car"), "human", None).unwrap();
    assert!(store::inbox(&conn, "p", 50, None).unwrap().is_empty());
}

#[test]
fn a_signal_goes_silent_once_and_only_once() {
    let conn = conn();
    store::heartbeat(&conn, "p", "etl", Some(-1), None).unwrap();

    // The transition is reported exactly once, so a caller can notify per
    // transition rather than on every sweep.
    assert_eq!(store::detect_silence(&conn, None).unwrap(), vec!["etl"]);
    assert!(store::detect_silence(&conn, None).unwrap().is_empty());

    let signals = store::signals(&conn, "p").unwrap();
    assert_eq!(signals[0].state, "silent");

    // A fresh heartbeat revives it, and it can then go silent again.
    store::heartbeat(&conn, "p", "etl", Some(-1), None).unwrap();
    assert_eq!(store::detect_silence(&conn, None).unwrap(), vec!["etl"]);
}

#[test]
fn signals_are_scoped_to_their_project() {
    let conn = conn();
    store::heartbeat(&conn, "one", "etl", Some(3600), None).unwrap();
    store::heartbeat(&conn, "two", "etl", Some(3600), None).unwrap();

    assert_eq!(store::signals(&conn, "one").unwrap().len(), 1);
    assert_eq!(store::signals(&conn, "two").unwrap().len(), 1);
}

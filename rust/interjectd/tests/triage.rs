//! Triage: the decision about whether to ask a human at all, and the numbers
//! that keep that decision honest.

use interjectd::store;
use interjectd::triage::{self, Decision, Policy};
use interjectd::types::NewQuestion;
use serde_json::json;

fn conn() -> rusqlite::Connection {
    store::open(":memory:").expect("in-memory store")
}

fn question(n: usize, suggested: Option<&str>, confidence: f64) -> NewQuestion {
    let mut value = json!({
        "key": format!("{n:064x}"),
        "id": "vehicle_type",
        "prompt": "What kind of vehicle is this?",
        "kind": "choice",
        "options": ["car", "motorcycle"],
        "context": {"n": n},
    });
    if let Some(suggested) = suggested {
        value["suggest"] = json!({"value": suggested, "confidence": confidence});
    }
    serde_json::from_value(value).expect("fixture")
}

fn enabled_policy() -> Policy {
    Policy {
        question_id: "vehicle_type".to_string(),
        threshold: 0.9,
        agreement_target: 0.9,
        shadow_rate: 0.0,
        min_samples: 4,
        enabled: true,
    }
}

/// Build agreement history: `total` questions carrying a suggestion, of which
/// `agreeing` were answered by a human with the suggested value.
fn seed_history(conn: &rusqlite::Connection, total: usize, agreeing: usize) {
    for n in 0..total {
        let q = question(1000 + n, Some("car"), 0.99);
        store::upsert_question(conn, "p", &q).unwrap();
        let answer = if n < agreeing {
            json!("car")
        } else {
            json!("motorcycle")
        };
        store::put_answer(conn, "p", &q.key, &answer, "human", Some("john")).unwrap();
    }
}

#[test]
fn without_a_suggestion_there_is_nothing_to_decide() {
    let conn = conn();
    triage::save_policy(&conn, "p", &enabled_policy()).unwrap();
    assert_eq!(
        triage::decide(&conn, "p", &question(1, None, 0.0)).unwrap(),
        Decision::Ask
    );
}

#[test]
fn auto_answering_is_off_until_someone_turns_it_on() {
    let conn = conn();
    seed_history(&conn, 10, 10);
    // No policy stored at all: the default must be to ask.
    assert_eq!(
        triage::decide(&conn, "p", &question(1, Some("car"), 1.0)).unwrap(),
        Decision::Ask
    );
}

#[test]
fn a_low_confidence_suggestion_is_never_used() {
    let conn = conn();
    seed_history(&conn, 10, 10);
    triage::save_policy(&conn, "p", &enabled_policy()).unwrap();
    assert_eq!(
        triage::decide(&conn, "p", &question(1, Some("car"), 0.5)).unwrap(),
        Decision::Ask
    );
}

#[test]
fn a_cold_class_is_always_asked() {
    let conn = conn();
    triage::save_policy(&conn, "p", &enabled_policy()).unwrap();
    // Only three compared cases against min_samples of four: too little evidence
    // to start deciding on someone's behalf, however confident the model is.
    seed_history(&conn, 3, 3);
    assert_eq!(
        triage::decide(&conn, "p", &question(1, Some("car"), 1.0)).unwrap(),
        Decision::Ask
    );
}

#[test]
fn a_well_calibrated_class_stops_asking() {
    let conn = conn();
    triage::save_policy(&conn, "p", &enabled_policy()).unwrap();
    seed_history(&conn, 10, 10);
    assert_eq!(
        triage::decide(&conn, "p", &question(1, Some("car"), 1.0)).unwrap(),
        Decision::Auto { shadow: false }
    );
}

#[test]
fn a_class_that_disagrees_with_humans_keeps_asking() {
    let conn = conn();
    triage::save_policy(&conn, "p", &enabled_policy()).unwrap();
    // 6/10 agreement against a target of 0.9.
    seed_history(&conn, 10, 6);
    assert_eq!(
        triage::decide(&conn, "p", &question(1, Some("car"), 1.0)).unwrap(),
        Decision::Ask
    );
}

#[test]
fn shadow_sampling_is_certain_at_a_rate_of_one() {
    let conn = conn();
    let mut policy = enabled_policy();
    policy.shadow_rate = 1.0;
    triage::save_policy(&conn, "p", &policy).unwrap();
    seed_history(&conn, 10, 10);
    assert_eq!(
        triage::decide(&conn, "p", &question(1, Some("car"), 1.0)).unwrap(),
        Decision::Auto { shadow: true }
    );
}

#[test]
fn an_auto_answer_records_its_source_and_can_spawn_a_shadow_twin() {
    let conn = conn();
    let q = question(1, Some("car"), 1.0);
    store::upsert_question(&conn, "p", &q).unwrap();
    triage::apply_auto_answer(&conn, "p", &q, true).unwrap();

    let snapshot = store::snapshot(&conn, "p", &q.key).unwrap().unwrap();
    assert_eq!(snapshot.state, "answered");
    let answer = snapshot.answer.unwrap();
    assert_eq!(answer.value, json!("car"));
    // Auditable: an answer nobody gave is marked as such.
    assert_eq!(answer.source, "auto");

    // The twin exists for a human, at the bottom of the pile.
    let twin = store::snapshot(&conn, "p", &triage::shadow_key(&q.key))
        .unwrap()
        .unwrap();
    assert_eq!(twin.state, "open");
    let inbox = store::inbox(&conn, "p", 50, None).unwrap();
    assert_eq!(inbox[0].questions[0].priority, 1);
}

#[test]
fn a_shadow_answer_does_not_change_the_pipelines_result() {
    let conn = conn();
    let q = question(1, Some("car"), 1.0);
    store::upsert_question(&conn, "p", &q).unwrap();
    triage::apply_auto_answer(&conn, "p", &q, true).unwrap();

    // The human disagrees, on the shadow copy.
    let twin_key = triage::shadow_key(&q.key);
    store::put_answer(
        &conn,
        "p",
        &twin_key,
        &json!("motorcycle"),
        "human",
        Some("john"),
    )
    .unwrap();

    // The original answer is untouched: measurement must not rewrite history.
    let snapshot = store::snapshot(&conn, "p", &q.key).unwrap().unwrap();
    assert_eq!(snapshot.answer.unwrap().value, json!("car"));

    // But it is counted as a disagreement.
    let report = triage::calibration(&conn, "p", "vehicle_type").unwrap();
    assert_eq!(report.shadow_compared, 1);
    assert_eq!(report.shadow_agreements, 0);
}

#[test]
fn the_calibration_numbers_add_up() {
    let conn = conn();
    seed_history(&conn, 10, 9);
    // Three auto-answers on top of the ten human ones.
    for n in 0..3 {
        let q = question(2000 + n, Some("car"), 1.0);
        store::upsert_question(&conn, "p", &q).unwrap();
        triage::apply_auto_answer(&conn, "p", &q, false).unwrap();
    }

    let report = triage::calibration(&conn, "p", "vehicle_type").unwrap();
    assert_eq!(report.compared, 10);
    assert_eq!(report.agreements, 9);
    assert_eq!(report.agreement_rate, Some(0.9));
    assert_eq!(report.auto_answered, 3);
    assert_eq!(report.human_answered, 10);
    // 3 of 13 answers never reached a human.
    let reduction = report.ask_reduction.unwrap();
    assert!((reduction - 3.0 / 13.0).abs() < 1e-9, "got {reduction}");
}

#[test]
fn a_report_covers_every_class_the_project_has_seen() {
    let conn = conn();
    seed_history(&conn, 2, 2);
    let mut other = question(3000, Some("Ford Focus"), 0.7);
    other.id = "cluster_label".to_string();
    store::upsert_question(&conn, "p", &other).unwrap();

    let report = triage::calibration_report(&conn, "p").unwrap();
    let ids: Vec<&str> = report.iter().map(|c| c.question_id.as_str()).collect();
    assert_eq!(ids, vec!["cluster_label", "vehicle_type"]);
}

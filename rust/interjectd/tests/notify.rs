//! Rendering and token tests. No network: these assert on the message a sink
//! *would* send, which is the part that is easy to get subtly wrong.

use interjectd::notify::{
    answer_token, decode_stream_line, render_questions, render_silence, verify_answer_token,
    NotifyConfig,
};
use interjectd::types::{InboxBatch, InboxQuestion};
use serde_json::json;

const SECRET: &str = "0123456789abcdef0123456789abcdef";

fn config() -> NotifyConfig {
    NotifyConfig {
        ntfy_base: "https://ntfy.example".to_string(),
        ntfy_topic: Some("notify-topic".to_string()),
        answer_topic: Some("answer-topic".to_string()),
        webhook: None,
    }
}

fn question(key: &str) -> InboxQuestion {
    InboxQuestion {
        key: key.to_string(),
        id: "vehicle_type".to_string(),
        prompt: "Is this a car?".to_string(),
        context: Some(json!({"title": "2018 Honda CBR"})),
        context_ref: None,
        suggest: None,
        created_at: "2026-09-26T00:00:00.000Z".to_string(),
        expires_at: None,
        priority: 5,
    }
}

fn batch(kind: &str, options: Option<serde_json::Value>, count: usize) -> InboxBatch {
    let questions: Vec<InboxQuestion> =
        (0..count).map(|n| question(&format!("{n:064x}"))).collect();
    InboxBatch {
        batch_key: "p/vehicle_type".to_string(),
        prompt: "Is this a car?".to_string(),
        kind: kind.to_string(),
        options,
        count,
        questions,
    }
}

#[test]
fn a_token_verifies_only_against_its_own_key() {
    let token = answer_token(SECRET, "abc");
    assert!(verify_answer_token(SECRET, "abc", &token));
    assert!(!verify_answer_token(SECRET, "abd", &token));
    assert!(!verify_answer_token("another-secret", "abc", &token));
    assert!(!verify_answer_token(SECRET, "abc", "short"));
    assert_eq!(token.len(), 32);
}

#[test]
fn a_single_choice_question_is_answerable_in_one_tap() {
    let message = render_questions(
        &batch("choice", Some(json!(["car", "moto"])), 1),
        &config(),
        SECRET,
    );
    let actions = message
        .actions
        .expect("a single choice should carry actions");

    assert_eq!(actions.matches("http, ").count(), 2);
    assert!(actions.contains("https://ntfy.example/answer-topic"));
    assert!(actions.contains("\"value\":\"car\""));
    // The token travels with the button, so knowing the topic is not enough.
    assert!(actions.contains(&answer_token(SECRET, &format!("{:064x}", 0))));
}

#[test]
fn an_approve_question_gets_two_buttons() {
    let message = render_questions(&batch("approve", None, 1), &config(), SECRET);
    let actions = message.actions.expect("approve should carry actions");
    assert_eq!(actions.matches("http, ").count(), 2);
    assert!(actions.contains("\"value\":true"));
    assert!(actions.contains("\"value\":false"));
}

#[test]
fn free_text_cannot_be_answered_by_a_button() {
    let message = render_questions(&batch("text", None, 1), &config(), SECRET);
    assert!(message.actions.is_none());
}

#[test]
fn too_many_options_falls_back_to_the_inbox() {
    // ntfy renders at most three buttons; silently dropping options would be worse
    // than telling someone to open the inbox.
    let many = json!(["a", "b", "c", "d"]);
    let message = render_questions(&batch("choice", Some(many), 1), &config(), SECRET);
    assert!(message.actions.is_none());
}

#[test]
fn a_batch_of_many_is_announced_once_and_carries_no_buttons() {
    let message = render_questions(
        &batch("choice", Some(json!(["car", "moto"])), 137),
        &config(),
        SECRET,
    );

    // Coalescing is the point: one notification, with the count in the title.
    assert!(
        message.title.contains("137 waiting"),
        "title was {:?}",
        message.title
    );
    assert!(message.body.contains("interjectd inbox --answer"));
    // A tap could not say *which* of the 137 it meant, so there are no buttons.
    assert!(message.actions.is_none());
}

#[test]
fn without_an_answer_topic_there_are_no_buttons() {
    let mut without = config();
    without.answer_topic = None;
    let message = render_questions(
        &batch("choice", Some(json!(["car", "moto"])), 1),
        &without,
        SECRET,
    );
    assert!(message.actions.is_none());
}

#[test]
fn silence_is_rendered_as_a_high_priority_warning() {
    let message = render_silence(&["etl.nightly".to_string(), "scan".to_string()]);
    assert!(message.title.contains("2 signals"));
    assert!(message.body.contains("etl.nightly"));
    assert_eq!(message.priority, Some("high"));
}

#[test]
fn a_stream_line_decodes_into_an_answer() {
    let key = "a".repeat(64);
    let inner = json!({"key": key, "value": "car", "token": answer_token(SECRET, &key)});
    let line = json!({"event": "message", "message": inner.to_string()}).to_string();

    let decoded = decode_stream_line(&line, SECRET).expect("should decode");
    assert_eq!(decoded.key, key);
    assert_eq!(decoded.value, json!("car"));
}

#[test]
fn a_forged_token_is_discarded() {
    let key = "b".repeat(64);
    let inner = json!({"key": key, "value": "car", "token": "f".repeat(32)});
    let line = json!({"event": "message", "message": inner.to_string()}).to_string();
    assert!(decode_stream_line(&line, SECRET).is_none());
}

#[test]
fn keepalives_and_noise_are_ignored() {
    for line in [
        r#"{"event":"open"}"#,
        r#"{"event":"keepalive"}"#,
        r#"{"event":"message"}"#,
        r#"{"event":"message","message":"not json"}"#,
        "not json at all",
        "",
    ] {
        assert!(
            decode_stream_line(line, SECRET).is_none(),
            "line {line:?} should be ignored"
        );
    }
}

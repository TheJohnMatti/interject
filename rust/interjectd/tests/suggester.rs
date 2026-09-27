//! The daemon-supplied half of D6: an external service proposing answers.
//!
//! A URL rather than an embedded model vendor, so the daemon never holds an API
//! key. The tests below pin the behaviour that matters most — a broken or
//! dishonest suggester must degrade to asking a human, never to guessing.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use interject::{Ask, Client, Error};
use interjectd::daemon::{self, ServeOptions};
use interjectd::notify::NotifyConfig;
use serde_json::{json, Value};
use tokio::runtime::Runtime;

#[derive(Clone)]
struct Suggester {
    reply: Arc<Mutex<Value>>,
    seen: Arc<Mutex<Vec<Value>>>,
}

async fn suggest(State(state): State<Suggester>, Json(question): Json<Value>) -> Json<Value> {
    state.seen.lock().unwrap().push(question);
    Json(state.reply.lock().unwrap().clone())
}

struct Harness {
    _runtime: Runtime,
    client: Client,
    suggester: Suggester,
    db: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.db.display()));
        }
    }
}

fn start(label: &str, reply: Value, reachable: bool) -> Harness {
    let runtime = Runtime::new().expect("runtime");
    let suggester = Suggester {
        reply: Arc::new(Mutex::new(reply)),
        seen: Arc::new(Mutex::new(Vec::new())),
    };

    let suggester_url = {
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind suggester");
        let addr = listener.local_addr().unwrap();
        if reachable {
            let router = Router::new()
                .route("/suggest", post(suggest))
                .with_state(suggester.clone());
            runtime.spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
        }
        // When `reachable` is false the listener is dropped, so the port refuses
        // connections: the daemon must cope with a suggester that is simply down.
        format!("http://{addr}/suggest")
    };

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db = std::env::temp_dir().join(format!("interjectd-sugg-{label}-{nanos}.sqlite3"));
    let (addr, serving) = runtime
        .block_on(daemon::bind(ServeOptions {
            addr: "127.0.0.1:0".to_string(),
            db: db.to_string_lossy().to_string(),
            max_wait: 300,
            token: None,
            sweep_secs: 60,
            notify_debounce_secs: 60,
            notify: NotifyConfig::default(),
            suggester_url: Some(suggester_url),
        }))
        .expect("daemon binds");
    runtime.spawn(serving);

    Harness {
        _runtime: runtime,
        client: Client::new(format!("http://{addr}"), "test", None),
        suggester,
        db,
    }
}

fn question() -> Ask {
    Ask::new("Is this a car?")
        .id("vehicle_type")
        .options(["car", "motorcycle"])
        .context(json!({"title": "2018 Honda CBR"}))
        .wait_secs(0)
}

#[test]
fn the_daemon_fills_in_a_missing_suggestion() {
    let harness = start(
        "fills",
        json!({"value": "motorcycle", "confidence": 0.93}),
        true,
    );
    let key = match harness.client.ask(question()) {
        Err(Error::Suspended { key, .. }) => key,
        other => panic!("expected Suspended, got {other:?}"),
    };

    // The suggester saw the question, with enough to judge it.
    let seen = harness.suggester.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["prompt"], json!("Is this a car?"));
    assert_eq!(seen[0]["context"]["title"], json!("2018 Honda CBR"));

    // And the suggestion is attached to the stored question, where a human sees
    // it and the triage layer can learn from it.
    let inbox = harness.client.inbox(50, None).unwrap();
    let stored = &inbox.batches[0].questions[0];
    assert_eq!(stored.key, key);
    assert_eq!(
        stored.suggest.as_ref().unwrap()["value"],
        json!("motorcycle")
    );
}

#[test]
fn a_client_supplied_suggestion_wins() {
    let harness = start(
        "client-wins",
        json!({"value": "boat", "confidence": 0.99}),
        true,
    );
    harness
        .client
        .ask(question().suggest(json!("car"), 0.5))
        .ok();

    let inbox = harness.client.inbox(50, None).unwrap();
    let stored = &inbox.batches[0].questions[0];
    assert_eq!(stored.suggest.as_ref().unwrap()["value"], json!("car"));
    // The daemon should not have bothered the suggester at all.
    assert!(harness.suggester.seen.lock().unwrap().is_empty());
}

#[test]
fn an_unreachable_suggester_still_asks_the_human() {
    let harness = start("down", json!({}), false);
    // The question must still register, just without a suggestion. Degrading to
    // asking is correct; failing the call would take the pipeline down with it.
    assert!(matches!(
        harness.client.ask(question()),
        Err(Error::Suspended { .. })
    ));
    let inbox = harness.client.inbox(50, None).unwrap();
    assert_eq!(inbox.batches[0].count, 1);
    assert!(inbox.batches[0].questions[0].suggest.is_none());
}

#[test]
fn a_nonsense_suggestion_is_ignored() {
    for (label, reply) in [
        ("no-value", json!({"confidence": 0.99})),
        ("no-confidence", json!({"value": "car"})),
        ("out-of-range", json!({"value": "car", "confidence": 4.2})),
        ("not-an-object", json!("car")),
    ] {
        let harness = start(label, reply, true);
        assert!(matches!(
            harness.client.ask(question()),
            Err(Error::Suspended { .. })
        ));
        let inbox = harness.client.inbox(50, None).unwrap();
        assert!(
            inbox.batches[0].questions[0].suggest.is_none(),
            "{label}: a malformed suggestion must be discarded, not stored"
        );
    }
}

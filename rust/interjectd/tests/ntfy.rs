//! The one-tap loop, end to end, with a stub ntfy server standing in for
//! ntfy.sh: the daemon publishes a notification with action buttons, a "tap"
//! publishes one button's payload back, the daemon's outbound subscription picks
//! it up, and a waiting caller resumes.
//!
//! This is the claim M2 exists to make, so it is tested without a phone and
//! without touching the real ntfy.sh.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use interject::{Ask, Client, Error};
use interjectd::daemon::{self, ServeOptions};
use interjectd::notify::NotifyConfig;
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
struct Published {
    topic: String,
    title: Option<String>,
    actions: Option<String>,
    body: String,
}

#[derive(Clone)]
struct Ntfy {
    published: Arc<Mutex<Vec<Published>>>,
    /// Messages published to any topic, fanned out to subscribers of that topic.
    stream: broadcast::Sender<(String, String)>,
}

async fn publish(
    Path(topic): Path<String>,
    State(stub): State<Ntfy>,
    headers: HeaderMap,
    body: String,
) -> &'static str {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    stub.published.lock().unwrap().push(Published {
        topic: topic.clone(),
        title: header("title"),
        actions: header("actions"),
        body: body.clone(),
    });
    let _ = stub.stream.send((topic, body));
    "ok"
}

/// ntfy's newline-delimited JSON stream, for the topic being subscribed to.
async fn subscribe(Path(topic): Path<String>, State(stub): State<Ntfy>) -> Response {
    let receiver = stub.stream.subscribe();
    let wanted = topic;
    let lines = tokio_stream::StreamExt::filter_map(
        tokio_stream::wrappers::BroadcastStream::new(receiver),
        move |item| match item {
            Ok((topic, body)) if topic == wanted => {
                let line = json!({"event": "message", "message": body}).to_string();
                Some(Ok::<_, std::io::Error>(format!("{line}\n")))
            }
            _ => None,
        },
    );
    Response::builder()
        .header("content-type", "application/x-ndjson")
        .body(axum::body::Body::from_stream(lines))
        .expect("a valid streaming response")
}

struct Harness {
    _runtime: Runtime,
    client: Client,
    ntfy: Ntfy,
    _db: PathGuard,
}

struct PathGuard(std::path::PathBuf);

impl Drop for PathGuard {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn start(label: &str) -> Harness {
    let runtime = Runtime::new().expect("runtime");
    let (stream, _) = broadcast::channel(64);
    let ntfy = Ntfy {
        published: Arc::new(Mutex::new(Vec::new())),
        stream,
    };

    // 1. the stub ntfy server
    let stub_addr = {
        let stub = ntfy.clone();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind stub");
        let addr = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/{topic}", post(publish))
            .route("/{topic}/json", get(subscribe))
            .with_state(stub);
        runtime.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        addr
    };

    // 2. the real daemon, pointed at it
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db = std::env::temp_dir().join(format!("interjectd-ntfy-{label}-{nanos}.sqlite3"));
    let options = ServeOptions {
        addr: "127.0.0.1:0".to_string(),
        db: db.to_string_lossy().to_string(),
        max_wait: 300,
        token: None,
        sweep_secs: 1,
        notify_debounce_secs: 1,
        notify: NotifyConfig {
            ntfy_base: format!("http://{stub_addr}"),
            ntfy_topic: Some("notify".to_string()),
            answer_topic: Some("answers".to_string()),
            webhook: None,
        },
        suggester_url: None,
    };
    let (addr, serving) = runtime
        .block_on(daemon::bind(options))
        .expect("daemon should bind");
    runtime.spawn(serving);

    Harness {
        _runtime: runtime,
        client: Client::new(format!("http://{addr}"), "test", None),
        ntfy,
        _db: PathGuard(db),
    }
}

impl Harness {
    /// Wait for a notification on a topic, or give up.
    fn await_publication(&self, topic: &str, timeout: Duration) -> Published {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if let Some(found) = self
                .ntfy
                .published
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.topic == topic)
                .cloned()
            {
                return found;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("nothing was published to {topic} within {timeout:?}");
    }

    fn publications(&self, topic: &str) -> Vec<Published> {
        self.ntfy
            .published
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.topic == topic)
            .cloned()
            .collect()
    }
}

/// Pull one action's JSON payload out of a rendered ntfy `Actions` header.
fn payload_of_action(actions: &str, label: &str) -> Value {
    let action = actions
        .split(';')
        .find(|a| a.contains(&format!("http, {label},")))
        .unwrap_or_else(|| panic!("no action labelled {label} in {actions}"));
    let body = action
        .split_once("body='")
        .and_then(|(_, rest)| rest.rsplit_once('\''))
        .map(|(json, _)| json)
        .expect("an action body");
    serde_json::from_str(body).expect("the action body should be JSON")
}

#[test]
fn a_tapped_button_answers_the_question_and_resumes_the_caller() {
    let harness = start("tap");

    // A program asks and waits.
    let asking = {
        let client = harness.client.clone();
        std::thread::spawn(move || {
            client.ask(
                Ask::new("Is this a car?")
                    .id("vehicle_type")
                    .options(["car", "motorcycle"])
                    .context(json!({"title": "2018 Honda CBR"}))
                    .wait_secs(60),
            )
        })
    };

    // The daemon announces it with one-tap buttons.
    let notification = harness.await_publication("notify", Duration::from_secs(15));
    assert_eq!(notification.title.as_deref(), Some("Is this a car?"));
    let actions = notification
        .actions
        .expect("the notification should carry actions");

    // Simulate the tap: ntfy posts the button's body to the answer topic.
    let payload = payload_of_action(&actions, "motorcycle");
    ureq::post(format!("http://{}/answers", stub_host(&actions)))
        .send_json(&payload)
        .expect("the tap should publish");

    // The daemon's outbound subscription picks it up and the caller resumes.
    let answer = asking.join().expect("the asking thread should not panic");
    assert_eq!(
        answer.expect("the caller should get an answer"),
        json!("motorcycle")
    );
}

/// The stub's host, recovered from the action URL the daemon rendered.
fn stub_host(actions: &str) -> String {
    let url = actions
        .split(", ")
        .find(|part| part.starts_with("http://"))
        .expect("an action URL");
    url.trim_start_matches("http://")
        .trim_end_matches("/answers")
        .to_string()
}

#[test]
fn a_forged_tap_is_ignored() {
    let harness = start("forged");
    let asking = {
        let client = harness.client.clone();
        std::thread::spawn(move || {
            client.ask(
                Ask::new("Deploy?")
                    .id("deploy_gate")
                    .kind("approve")
                    .context(json!({"sha": "abc"}))
                    .wait_secs(3),
            )
        })
    };
    let notification = harness.await_publication("notify", Duration::from_secs(15));
    let actions = notification.actions.expect("actions");
    let mut payload = payload_of_action(&actions, "true");

    // Tamper with the token. The topic name alone must not be enough to answer.
    payload["token"] = json!("f".repeat(32));
    ureq::post(format!("http://{}/answers", stub_host(&actions)))
        .send_json(&payload)
        .expect("publish");

    // The caller is not answered: it times out and suspends instead.
    match asking.join().unwrap() {
        Err(Error::Suspended { .. }) => {}
        other => panic!("a forged tap should not answer anything, got {other:?}"),
    }
}

#[test]
fn a_burst_of_questions_becomes_one_notification() {
    let harness = start("burst");

    // Forty questions of the same class, asked as fast as possible.
    for n in 0..40 {
        let _ = harness.client.ask(
            Ask::new("Is this a car?")
                .id("vehicle_type")
                .options(["car", "motorcycle"])
                .context(json!({"n": n}))
                .wait_secs(0),
        );
    }

    let notification = harness.await_publication("notify", Duration::from_secs(15));
    // Coalescing: one notification for the batch, with the count in the title,
    // rather than forty interruptions.
    assert!(
        notification
            .title
            .as_deref()
            .unwrap_or("")
            .contains("waiting"),
        "expected a coalesced title, got {:?}",
        notification.title
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        harness.publications("notify").len(),
        1,
        "a batch should be announced once, not repeatedly"
    );
}

#[test]
fn a_silent_signal_is_announced_once() {
    let harness = start("silence");
    harness
        .client
        .heartbeat("etl.nightly", 1)
        .expect("heartbeat");

    let notification = harness.await_publication("notify", Duration::from_secs(15));
    assert!(notification
        .title
        .as_deref()
        .unwrap_or("")
        .contains("went quiet"));
    assert!(notification.body.contains("etl.nightly"));

    // Reported per transition, so an ongoing outage is not a repeating alarm.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(harness.publications("notify").len(), 1);
}

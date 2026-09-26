//! Integration tests against the real HTTP stack, driven by the real Rust
//! client. Nothing here is mocked: an axum server is bound to an ephemeral port
//! over a real SQLite file, and the client talks to it over TCP.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use interject::{Ask, Client, Error, OnTimeout};
use interjectd::{api, store};
use serde_json::json;
use tokio::runtime::Runtime;

/// A running daemon. Dropping it stops the server; the database file is kept so
/// a second harness can reopen it and prove state survived a restart.
struct Daemon {
    runtime: Option<Runtime>,
    client: Client,
    db: PathBuf,
}

impl Daemon {
    fn start(db: &Path, max_wait: u64, token: Option<String>) -> Self {
        let runtime = Runtime::new().expect("runtime");
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let conn = store::open(db.to_str().unwrap()).expect("open store");
        let state = api::AppState::new(conn, max_wait, token.clone());
        runtime.spawn(async move {
            let _ = axum::serve(listener, api::router(state)).await;
        });

        Self {
            runtime: Some(runtime),
            client: Client::new(format!("http://{addr}"), "test", token),
            db: db.to_path_buf(),
        }
    }

    fn client(&self) -> &Client {
        &self.client
    }

    fn restart(mut self) -> Self {
        let db = self.db.clone();
        drop(self.runtime.take());
        Daemon::start(&db, 300, None)
    }
}

/// A unique database path per test, removed when the guard drops.
struct TempDb(PathBuf);

impl TempDb {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!("interjectd-{label}-{nanos}.sqlite3")))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn vehicle_question() -> Ask {
    Ask::new("Is this a car?")
        .id("vehicle_type")
        .options(["car", "motorcycle"])
        .context(json!({"title": "2018 Honda CBR"}))
}

#[test]
fn an_unanswered_question_suspends_the_caller() {
    let db = TempDb::new("suspend");
    let daemon = Daemon::start(db.path(), 300, None);

    let result = daemon.client().ask(vehicle_question().wait_secs(0));
    match result {
        Err(Error::Suspended { id, .. }) => assert_eq!(id, "vehicle_type"),
        other => panic!("expected Suspended, got {other:?}"),
    }
}

#[test]
fn answering_lets_the_next_run_resume() {
    let db = TempDb::new("replay");
    let daemon = Daemon::start(db.path(), 300, None);

    let key = match daemon.client().ask(vehicle_question().wait_secs(0)) {
        Err(Error::Suspended { key, .. }) => key,
        other => panic!("expected Suspended, got {other:?}"),
    };
    daemon
        .client()
        .answer(&key, &json!("motorcycle"), Some("test"))
        .expect("answer should be accepted");

    let answer = daemon
        .client()
        .ask(vehicle_question().wait_secs(0))
        .expect("the replay should return the stored answer");
    assert_eq!(answer, json!("motorcycle"));
}

#[test]
fn a_waiting_caller_is_woken_by_the_answer_not_by_a_timer() {
    let db = TempDb::new("wake");
    let daemon = Daemon::start(db.path(), 300, None);
    let key = interject::question_key("test", "deploy_gate", Some(&json!({"sha": "abc"})));

    let answering = {
        let client = daemon.client().clone();
        let key = key.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            client
                .answer(&key, &json!(true), Some("test"))
                .expect("answer");
        })
    };

    let started = Instant::now();
    let answer = daemon
        .client()
        .ask(
            Ask::new("Deploy to production?")
                .id("deploy_gate")
                .kind("approve")
                .context(json!({"sha": "abc"}))
                .wait_secs(30),
        )
        .expect("the long poll should deliver the answer");
    let elapsed = started.elapsed();
    answering.join().unwrap();

    assert_eq!(answer, json!(true));
    // If the poll had fallen back to its own timer this would be ~30s. The point
    // of the broadcast channel is that it returns as soon as the answer lands.
    assert!(elapsed < Duration::from_secs(5), "woken late: {elapsed:?}");
}

#[test]
fn state_survives_a_daemon_restart() {
    let db = TempDb::new("restart");
    let daemon = Daemon::start(db.path(), 300, None);

    let key = match daemon.client().ask(vehicle_question().wait_secs(0)) {
        Err(Error::Suspended { key, .. }) => key,
        other => panic!("expected Suspended, got {other:?}"),
    };
    daemon
        .client()
        .answer(&key, &json!("car"), Some("test"))
        .unwrap();

    // The whole premise is that a question outlives the processes involved.
    let restarted = daemon.restart();
    let answer = restarted
        .client()
        .ask(vehicle_question().wait_secs(0))
        .expect("the answer should have survived the restart");
    assert_eq!(answer, json!("car"));
}

#[test]
fn an_unanswered_question_survives_a_restart_too() {
    let db = TempDb::new("restart-open");
    let daemon = Daemon::start(db.path(), 300, None);
    assert!(daemon
        .client()
        .ask(vehicle_question().wait_secs(0))
        .is_err());

    let restarted = daemon.restart();
    let inbox = restarted.client().inbox(50, None).expect("inbox");
    assert_eq!(inbox.batches.len(), 1);
    assert_eq!(inbox.batches[0].count, 1);
}

#[test]
fn a_second_answer_is_refused() {
    let db = TempDb::new("conflict");
    let daemon = Daemon::start(db.path(), 300, None);
    let key = match daemon.client().ask(vehicle_question().wait_secs(0)) {
        Err(Error::Suspended { key, .. }) => key,
        other => panic!("expected Suspended, got {other:?}"),
    };
    daemon.client().answer(&key, &json!("car"), None).unwrap();

    match daemon.client().answer(&key, &json!("motorcycle"), None) {
        Err(Error::Api { status, .. }) => assert_eq!(status, 409),
        other => panic!("expected 409, got {other:?}"),
    }
}

#[test]
fn answering_an_unknown_key_is_a_404() {
    let db = TempDb::new("unknown");
    let daemon = Daemon::start(db.path(), 300, None);
    let key = "0".repeat(64);

    match daemon.client().answer(&key, &json!("car"), None) {
        Err(Error::Api { status, .. }) => assert_eq!(status, 404),
        other => panic!("expected 404, got {other:?}"),
    }
}

#[test]
fn a_malformed_key_is_rejected() {
    let db = TempDb::new("badkey");
    let daemon = Daemon::start(db.path(), 300, None);
    // Bypass the client, which always computes a well-formed key.
    let response = ureq::post(format!("{}/v0/questions", daemon.client().base()))
        .header("X-Interject-Project", "test")
        .send_json(json!({"key": "not-a-hash", "id": "x", "prompt": "?", "kind": "text"}));
    match response {
        Err(ureq::Error::StatusCode(status)) => assert_eq!(status, 400),
        other => panic!("expected a 400, got {other:?}"),
    }
}

#[test]
fn on_timeout_default_records_the_default_so_replays_agree() {
    let db = TempDb::new("default");
    let daemon = Daemon::start(db.path(), 300, None);
    let ask = || {
        Ask::new("Anyone there?")
            .id("ttl_demo")
            .kind("text")
            .default(json!("skipped"))
            .on_timeout(OnTimeout::Default)
            .wait_secs(0)
    };

    assert_eq!(daemon.client().ask(ask()).unwrap(), json!("skipped"));

    // The second call must read the recorded answer rather than default again,
    // which is what makes a defaulted branch deterministic on replay.
    assert_eq!(daemon.client().ask(ask()).unwrap(), json!("skipped"));
    let inbox = daemon.client().inbox(50, None).unwrap();
    assert!(
        inbox.batches.is_empty(),
        "a defaulted question should leave the inbox"
    );
}

#[test]
fn a_ttl_expires_to_its_default_without_a_human() {
    let db = TempDb::new("ttl");
    let daemon = Daemon::start(db.path(), 300, None);
    let ask = || {
        Ask::new("Anyone there?")
            .id("ttl_expiry")
            .kind("text")
            .ttl_secs(1)
            .default(json!("gave_up"))
            .wait_secs(0)
    };

    assert!(matches!(
        daemon.client().ask(ask()),
        Err(Error::Suspended { .. })
    ));
    std::thread::sleep(Duration::from_millis(1400));
    // D8: expiry applies the default quietly, and the caller simply gets it.
    assert_eq!(daemon.client().ask(ask()).unwrap(), json!("gave_up"));
}

#[test]
fn an_expired_question_without_a_default_is_an_error_not_a_guess() {
    let db = TempDb::new("ttl-nodefault");
    let daemon = Daemon::start(db.path(), 300, None);
    let ask = || {
        Ask::new("Anyone?")
            .id("ttl_none")
            .kind("text")
            .ttl_secs(1)
            .wait_secs(0)
    };

    assert!(matches!(
        daemon.client().ask(ask()),
        Err(Error::Suspended { .. })
    ));
    std::thread::sleep(Duration::from_millis(1400));
    match daemon.client().ask(ask()) {
        Err(Error::Expired { id, .. }) => assert_eq!(id, "ttl_none"),
        other => panic!("expected Expired, got {other:?}"),
    }
}

#[test]
fn the_daemon_caps_how_long_a_poll_may_hold_a_connection() {
    let db = TempDb::new("maxwait");
    // A client asking for 30s against a 1s cap must come back in about a second.
    let daemon = Daemon::start(db.path(), 1, None);

    let started = Instant::now();
    assert!(matches!(
        daemon.client().ask(vehicle_question().wait_secs(30)),
        Err(Error::Suspended { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cap not honoured"
    );
}

#[test]
fn a_token_is_required_when_the_daemon_was_given_one() {
    let db = TempDb::new("token");
    let daemon = Daemon::start(db.path(), 300, Some("s3cret".to_string()));

    // The harness client holds the right token.
    assert!(matches!(
        daemon.client().ask(vehicle_question().wait_secs(0)),
        Err(Error::Suspended { .. })
    ));

    let anonymous = Client::new(daemon.client().base(), "test", None);
    assert!(matches!(
        anonymous.ask(vehicle_question().wait_secs(0)),
        Err(Error::Unauthorized)
    ));
    let wrong = Client::new(daemon.client().base(), "test", Some("nope".into()));
    assert!(matches!(
        wrong.ask(vehicle_question().wait_secs(0)),
        Err(Error::Unauthorized)
    ));
}

#[test]
fn signals_report_silence_over_http() {
    let db = TempDb::new("signals");
    let daemon = Daemon::start(db.path(), 300, None);

    daemon
        .client()
        .heartbeat("etl.nightly", 1)
        .expect("heartbeat");
    let live = daemon.client().signals().unwrap();
    assert_eq!(live[0].state, "live");

    std::thread::sleep(Duration::from_millis(1200));
    let silent = daemon.client().signals().unwrap();
    assert_eq!(silent[0].state, "silent");
    assert!(silent[0].due_at.is_some());
}

#[test]
fn a_heartbeat_needs_exactly_one_deadline() {
    let db = TempDb::new("heartbeat-args");
    let daemon = Daemon::start(db.path(), 300, None);
    let response = ureq::post(format!("{}/v0/signals/heartbeat", daemon.client().base()))
        .header("X-Interject-Project", "test")
        .send_json(json!({"name": "x"}));
    match response {
        Err(ureq::Error::StatusCode(status)) => assert_eq!(status, 400),
        other => panic!("expected a 400, got {other:?}"),
    }
}

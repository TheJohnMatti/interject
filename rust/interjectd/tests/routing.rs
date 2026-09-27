//! Routing (design question O1): when several people share a project, who
//! answers what?
//!
//! Two mechanisms, both deliberately advisory. Assignment says who a question is
//! *for*; a claim says who is looking at it *now*. Neither makes answering safe —
//! that is what write-once answers are for — they exist so two people do not
//! spend effort on the same question.

use std::path::PathBuf;

use interject::{Ask, Client, Error};
use interjectd::{api, store};
use serde_json::json;
use tokio::runtime::Runtime;

struct Daemon {
    _runtime: Runtime,
    base: String,
    db: PathBuf,
}

impl Daemon {
    fn start(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db = std::env::temp_dir().join(format!("interjectd-routing-{label}-{nanos}.sqlite3"));
        let runtime = Runtime::new().expect("runtime");
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind");
        let addr = listener.local_addr().unwrap();
        let conn = store::open(db.to_str().unwrap()).expect("store");
        let state = api::AppState::new(conn, 300, None);
        runtime.spawn(async move {
            let _ = axum::serve(listener, api::router(state)).await;
        });
        Self {
            _runtime: runtime,
            base: format!("http://{addr}"),
            db,
        }
    }

    fn admin(&self) -> rusqlite::Connection {
        store::open(self.db.to_str().unwrap()).expect("store")
    }

    /// A client answering as a named person.
    fn as_person(&self, who: &str) -> Client {
        Client::new(&self.base, "team", None).as_identity(who)
    }

    fn anonymous(&self) -> Client {
        Client::new(&self.base, "team", None)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.db.display()));
        }
    }
}

fn ask(n: i32) -> Ask {
    Ask::new("Is this a car?")
        .id("vehicle_type")
        .options(["car", "motorcycle"])
        .context(json!({"n": n}))
        .wait_secs(0)
}

fn register(client: &Client, n: i32) -> String {
    match client.ask(ask(n)) {
        Err(Error::Suspended { key, .. }) => key,
        other => panic!("expected Suspended, got {other:?}"),
    }
}

#[test]
fn an_unassigned_question_is_in_everyones_inbox() {
    let daemon = Daemon::start("pool");
    register(&daemon.anonymous(), 1);

    for who in ["ana", "ben"] {
        let inbox = daemon.as_person(who).inbox(50, None).unwrap();
        assert_eq!(
            inbox.batches[0].count, 1,
            "{who} should see the shared pool"
        );
    }
}

#[test]
fn an_assigned_question_reaches_only_its_person() {
    let daemon = Daemon::start("assigned");
    daemon
        .anonymous()
        .ask(ask(1).assign_to("ana"))
        .expect_err("registers and suspends");

    assert_eq!(
        daemon
            .as_person("ana")
            .inbox(50, None)
            .unwrap()
            .batches
            .len(),
        1
    );
    assert!(
        daemon
            .as_person("ben")
            .inbox(50, None)
            .unwrap()
            .batches
            .is_empty(),
        "a question assigned to someone else is not Ben's problem"
    );
    // But it is still visible when you deliberately look at everything.
    let everything = daemon
        .as_person("ben")
        .inbox_for(50, None, None, true)
        .unwrap();
    assert_eq!(everything.batches.len(), 1);
}

#[test]
fn assigning_later_moves_a_question_out_of_the_pool() {
    let daemon = Daemon::start("assign-later");
    let key = register(&daemon.anonymous(), 1);
    assert_eq!(
        daemon
            .as_person("ben")
            .inbox(50, None)
            .unwrap()
            .batches
            .len(),
        1
    );

    daemon.anonymous().assign(&key, Some("ana")).unwrap();
    assert!(daemon
        .as_person("ben")
        .inbox(50, None)
        .unwrap()
        .batches
        .is_empty());
    assert_eq!(
        daemon
            .as_person("ana")
            .inbox(50, None)
            .unwrap()
            .batches
            .len(),
        1
    );

    // And un-assigning returns it to everyone.
    daemon.anonymous().assign(&key, None).unwrap();
    assert_eq!(
        daemon
            .as_person("ben")
            .inbox(50, None)
            .unwrap()
            .batches
            .len(),
        1
    );
}

#[test]
fn a_claim_hides_a_question_from_everyone_else() {
    let daemon = Daemon::start("claim");
    let key = register(&daemon.anonymous(), 1);

    let claim = daemon.as_person("ana").claim(&key, None).unwrap();
    assert_eq!(claim.claimed_by.as_deref(), Some("ana"));

    assert!(
        daemon
            .as_person("ben")
            .inbox(50, None)
            .unwrap()
            .batches
            .is_empty(),
        "Ben should not spend time on what Ana is already reading"
    );
    // Ana still sees her own claim, or she could not answer it.
    assert_eq!(
        daemon
            .as_person("ana")
            .inbox(50, None)
            .unwrap()
            .batches
            .len(),
        1
    );
}

#[test]
fn a_second_claim_is_refused_but_answering_is_not() {
    let daemon = Daemon::start("contest");
    let key = register(&daemon.anonymous(), 1);
    daemon.as_person("ana").claim(&key, None).unwrap();

    match daemon.as_person("ben").claim(&key, None) {
        Err(Error::Api { status, message }) => {
            assert_eq!(status, 409);
            assert!(
                message.contains("ana"),
                "should say who holds it: {message}"
            );
        }
        other => panic!("expected 409, got {other:?}"),
    }

    // Claims are advisory. The real protection is that answers are write-once,
    // so Ben *can* answer, and then Ana's attempt is what fails.
    daemon
        .as_person("ben")
        .answer(&key, &json!("car"), Some("ben"))
        .unwrap();
    match daemon
        .as_person("ana")
        .answer(&key, &json!("motorcycle"), Some("ana"))
    {
        Err(Error::Api { status, .. }) => assert_eq!(status, 409),
        other => panic!("expected the second answer to lose, got {other:?}"),
    }
}

#[test]
fn releasing_returns_a_question_to_the_pool_at_once() {
    let daemon = Daemon::start("release");
    let key = register(&daemon.anonymous(), 1);
    daemon.as_person("ana").claim(&key, None).unwrap();
    assert!(daemon
        .as_person("ben")
        .inbox(50, None)
        .unwrap()
        .batches
        .is_empty());

    daemon.as_person("ana").release(&key).unwrap();
    assert_eq!(
        daemon
            .as_person("ben")
            .inbox(50, None)
            .unwrap()
            .batches
            .len(),
        1
    );
    // And now Ben can take it.
    assert!(daemon.as_person("ben").claim(&key, None).is_ok());
}

#[test]
fn a_claim_expires_so_wandering_off_does_not_hide_it_forever() {
    let daemon = Daemon::start("expiry");
    let key = register(&daemon.anonymous(), 1);
    daemon.as_person("ana").claim(&key, Some(1)).unwrap();
    assert!(daemon
        .as_person("ben")
        .inbox(50, None)
        .unwrap()
        .batches
        .is_empty());

    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert_eq!(
        daemon
            .as_person("ben")
            .inbox(50, None)
            .unwrap()
            .batches
            .len(),
        1,
        "an abandoned claim must not hide a question indefinitely"
    );
    assert!(daemon.as_person("ben").claim(&key, None).is_ok());
}

#[test]
fn reclaiming_your_own_question_is_fine() {
    let daemon = Daemon::start("reclaim");
    let key = register(&daemon.anonymous(), 1);
    daemon.as_person("ana").claim(&key, None).unwrap();
    // Refreshing a claim you already hold must not be a conflict with yourself.
    assert!(daemon.as_person("ana").claim(&key, None).is_ok());
}

#[test]
fn an_answered_question_cannot_be_claimed() {
    let daemon = Daemon::start("claim-answered");
    let key = register(&daemon.anonymous(), 1);
    daemon
        .anonymous()
        .answer(&key, &json!("car"), None)
        .unwrap();

    match daemon.as_person("ana").claim(&key, None) {
        Err(Error::Api { status, .. }) => assert_eq!(status, 409),
        other => panic!("expected 409, got {other:?}"),
    }
}

#[test]
fn a_token_label_is_the_identity_and_outranks_the_header() {
    let daemon = Daemon::start("identity");
    let ana = store::create_token(&daemon.admin(), "team", Some("ana"), "device").unwrap();
    let key = {
        let client = Client::new(&daemon.base, "team", Some(ana.clone()));
        register(&client, 1)
    };

    // Claim as Ana, using her token, while the header claims to be Ben.
    let pretending = Client::new(&daemon.base, "team", Some(ana)).as_identity("ben");
    let claim = pretending.claim(&key, None).unwrap();
    assert_eq!(
        claim.claimed_by.as_deref(),
        Some("ana"),
        "identity must come from the token, not from a header anyone can set"
    );
}

#[test]
fn claiming_without_any_identity_is_a_clear_error() {
    let daemon = Daemon::start("no-identity");
    let key = register(&daemon.anonymous(), 1);

    match daemon.anonymous().claim(&key, None) {
        Err(Error::Api { status, message }) => {
            assert_eq!(status, 400);
            assert!(message.contains("identity"), "unhelpful message: {message}");
        }
        other => panic!("expected a 400 explaining the problem, got {other:?}"),
    }
}

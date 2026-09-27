//! Tokens, pairing, and the switch from a single-user daemon to a multi-tenant
//! one. The rule that matters: once any token exists, open mode is over.

use std::path::{Path, PathBuf};

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
        let db = std::env::temp_dir().join(format!("interjectd-auth-{label}-{nanos}.sqlite3"));

        let runtime = Runtime::new().expect("runtime");
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind");
        let addr = listener.local_addr().unwrap();
        let conn = store::open(db.to_str().unwrap()).expect("open store");
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

    /// A second connection to the same database, which is exactly what the
    /// `interjectd token create` and `interjectd pair` commands use.
    fn admin(&self) -> rusqlite::Connection {
        store::open(self.db.to_str().unwrap()).expect("open store")
    }

    fn client(&self, project: &str, token: Option<&str>) -> Client {
        Client::new(&self.base, project, token.map(str::to_string))
    }

    fn db_path(&self) -> &Path {
        &self.db
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.db.display()));
        }
    }
}

fn question() -> Ask {
    Ask::new("Is this a car?")
        .id("vehicle_type")
        .options(["car", "motorcycle"])
        .context(json!({"n": 1}))
        .wait_secs(0)
}

#[test]
fn with_no_tokens_the_daemon_is_open() {
    let daemon = Daemon::start("open");
    // A local single-user daemon should work with no setup at all.
    assert!(matches!(
        daemon.client("anything", None).ask(question()),
        Err(Error::Suspended { .. })
    ));
}

#[test]
fn minting_the_first_token_closes_open_mode() {
    let daemon = Daemon::start("closes");
    let token = store::create_token(&daemon.admin(), "mine", Some("laptop"), "project").unwrap();

    // Anonymous access is over, for every project.
    assert!(matches!(
        daemon.client("mine", None).ask(question()),
        Err(Error::Unauthorized)
    ));
    assert!(matches!(
        daemon.client("default", None).ask(question()),
        Err(Error::Unauthorized)
    ));
    // And the minted token works.
    assert!(matches!(
        daemon.client("mine", Some(&token)).ask(question()),
        Err(Error::Suspended { .. })
    ));
}

#[test]
fn a_token_decides_its_own_project_regardless_of_the_header() {
    let daemon = Daemon::start("scoped");
    let mine = store::create_token(&daemon.admin(), "mine", None, "project").unwrap();

    // Register under the token's real project, while *claiming* another one.
    // The header must not be able to reach across tenants.
    let liar = daemon.client("someone_else", Some(&mine));
    let key = match liar.ask(question()) {
        Err(Error::Suspended { key, .. }) => key,
        other => panic!("expected Suspended, got {other:?}"),
    };

    // The row landed in "mine", so a differently-scoped key is not found there.
    let honest = daemon.client("mine", Some(&mine));
    // The client computed the key from the project it was told, which was a lie,
    // so the honest client's own key differs — but the stored row is in "mine".
    assert!(
        honest.peek(&key).unwrap().is_some(),
        "the question should belong to the token's project"
    );

    let other_token = store::create_token(&daemon.admin(), "other", None, "project").unwrap();
    let stranger = daemon.client("mine", Some(&other_token));
    assert!(
        stranger.peek(&key).unwrap().is_none(),
        "another project's token must not see this question"
    );
}

#[test]
fn an_unknown_token_is_refused_rather_than_treated_as_anonymous() {
    let daemon = Daemon::start("unknown");
    store::create_token(&daemon.admin(), "mine", None, "project").unwrap();
    assert!(matches!(
        daemon
            .client("mine", Some("not-a-real-token"))
            .ask(question()),
        Err(Error::Unauthorized)
    ));
}

#[test]
fn a_pairing_code_can_be_redeemed_exactly_once() {
    let daemon = Daemon::start("pair");
    let code = store::create_pairing(&daemon.admin(), "mine", Some("phone"), 600).unwrap();

    let redeem = |code: &str| {
        ureq::post(format!("{}/v0/pair", daemon.base)).send_json(json!({"code": code}))
    };

    let mut response = redeem(&code).expect("a fresh code should pair");
    let body: serde_json::Value = response.body_mut().read_json().unwrap();
    let token = body["token"].as_str().expect("a token").to_string();
    assert_eq!(body["project"], json!("mine"));

    // The token works, and carries the project from the code.
    assert!(matches!(
        daemon.client("ignored", Some(&token)).ask(question()),
        Err(Error::Suspended { .. })
    ));

    // Single use.
    match redeem(&code) {
        Err(ureq::Error::StatusCode(status)) => assert_eq!(status, 401),
        other => panic!("a reused code must be refused, got {other:?}"),
    }
}

#[test]
fn an_expired_or_unknown_pairing_code_is_refused() {
    let daemon = Daemon::start("pair-expired");
    let expired = store::create_pairing(&daemon.admin(), "mine", None, -1).unwrap();

    for code in [expired.as_str(), "ZZZZZZZZ"] {
        match ureq::post(format!("{}/v0/pair", daemon.base)).send_json(json!({"code": code})) {
            Err(ureq::Error::StatusCode(status)) => assert_eq!(status, 401),
            other => panic!("code {code} should be refused, got {other:?}"),
        }
    }
}

#[test]
fn pairing_codes_are_case_insensitive_because_people_retype_them() {
    let daemon = Daemon::start("pair-case");
    let code = store::create_pairing(&daemon.admin(), "mine", None, 600).unwrap();
    let typed = code.to_lowercase();

    let mut response = ureq::post(format!("{}/v0/pair", daemon.base))
        .send_json(json!({"code": typed}))
        .expect("a lowercased code should still pair");
    let body: serde_json::Value = response.body_mut().read_json().unwrap();
    assert!(body["token"].as_str().is_some());
}

#[test]
fn tokens_are_never_stored_in_the_clear() {
    let daemon = Daemon::start("hashed");
    let token = store::create_token(&daemon.admin(), "mine", None, "project").unwrap();

    let raw = std::fs::read(daemon.db_path()).expect("read the database file");
    let haystack = String::from_utf8_lossy(&raw);
    assert!(
        !haystack.contains(&token),
        "the plaintext token must not appear in the database"
    );
    // The digest is what is stored, and it still resolves.
    assert_eq!(
        store::resolve_token(&daemon.admin(), &token)
            .unwrap()
            .as_deref(),
        Some("mine")
    );
}

#[test]
fn the_web_inbox_is_served_by_the_daemon_itself() {
    let daemon = Daemon::start("web");
    let mut response = ureq::get(&daemon.base)
        .call()
        .expect("the page should be served");
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/html; charset=utf-8"
    );
    let page = response.body_mut().read_to_string().unwrap();
    assert!(page.contains("<title>interject</title>"));
    // The page must be able to pair itself, or a phone can never get a token.
    assert!(page.contains("/v0/pair"));
}

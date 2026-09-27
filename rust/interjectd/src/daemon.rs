//! Process wiring: the HTTP server plus the background tasks that make expiry,
//! announcements and one-tap answers happen without anyone watching.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::{api, notify, store};

pub struct ServeOptions {
    pub addr: String,
    pub db: String,
    pub max_wait: u64,
    pub token: Option<String>,
    pub sweep_secs: u64,
    pub notify_debounce_secs: u64,
    pub notify: notify::NotifyConfig,
    /// Service asked to propose answers for questions arriving without one (D6).
    pub suggester_url: Option<String>,
}

/// Bind the listener and wire up every background task, returning the bound
/// address plus a future that serves until shutdown.
///
/// Split this way so that tests can start a complete daemon — announce loop,
/// sweeper and answer subscriber included — on an ephemeral port, rather than
/// testing only the router and hoping the wiring is right.
pub async fn bind(
    options: ServeOptions,
) -> Result<(
    std::net::SocketAddr,
    impl std::future::Future<Output = Result<()>>,
)> {
    let conn = store::open(&options.db)?;
    let secret = store::answer_secret(&conn)?;
    let state = api::AppState::new(conn, options.max_wait, options.token)
        .with_suggester(options.suggester_url.clone());
    let notifier = Arc::new(notify::Notifier::new(
        options.notify.clone(),
        secret.clone(),
    ));

    // Answers arriving from a tapped action button. The daemon subscribes
    // outbound, so this works without the phone being able to reach it.
    if options.notify.answer_topic.is_some() {
        let answering_state = state.clone();
        notify::run_answer_subscriber(options.notify.clone(), secret, move |callback| {
            apply_remote_answer(&answering_state, callback);
        });
        tracing::info!("subscribed for one-tap answers");
    }

    // Expiry and silence detection also happen lazily on read, so this sweeper
    // exists to make them happen when nobody is reading.
    let sweeper_db = state.db();
    let sweep_notifier = Arc::clone(&notifier);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(options.sweep_secs.max(1)));
        loop {
            ticker.tick().await;
            let db = Arc::clone(&sweeper_db);
            let notifier = Arc::clone(&sweep_notifier);
            let _ = tokio::task::spawn_blocking(move || sweep(&db, &notifier)).await;
        }
    });

    // Announce new questions on a debounce, so a burst of four hundred arrives
    // as one notification per batch rather than four hundred interruptions.
    if notifier.config().enabled() {
        let announce_db = state.db();
        let announce_notifier = Arc::clone(&notifier);
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(Duration::from_secs(options.notify_debounce_secs.max(1)));
            loop {
                ticker.tick().await;
                let db = Arc::clone(&announce_db);
                let notifier = Arc::clone(&announce_notifier);
                let _ = tokio::task::spawn_blocking(move || announce(&db, &notifier)).await;
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(&options.addr)
        .await
        .with_context(|| format!("binding {}", options.addr))?;
    let addr = listener
        .local_addr()
        .context("resolving the bound address")?;

    let serving = async move {
        axum::serve(listener, api::router(state))
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        tracing::info!("interjectd stopped");
        Ok(())
    };
    Ok((addr, serving))
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown requested");
}

/// Apply an answer that arrived out of band, e.g. from a phone tap. The token
/// was already verified by the subscriber.
fn apply_remote_answer(state: &api::AppState, callback: notify::AnswerCallback) {
    let db = state.db();
    let applied = {
        let conn = match db.lock() {
            Ok(conn) => conn,
            Err(_) => return,
        };
        let project = match store::project_of_key(&conn, &callback.key) {
            Ok(Some(project)) => project,
            _ => {
                tracing::warn!(key = %callback.key, "answer for an unknown question");
                return;
            }
        };
        store::put_answer(
            &conn,
            &project,
            &callback.key,
            &callback.value,
            "human",
            Some("tap"),
        )
    };
    match applied {
        Ok(store::AnswerOutcome::Stored(_)) => {
            // Wake anything long-polling for this key immediately.
            state.notify_answered(&callback.key);
            tracing::info!(key = %callback.key, "answered by tap");
        }
        Ok(store::AnswerOutcome::AlreadyAnswered) => {
            tracing::info!(key = %callback.key, "tap ignored; already answered")
        }
        Ok(store::AnswerOutcome::UnknownKey) => {}
        Err(error) => tracing::warn!(%error, "could not apply a tapped answer"),
    }
}

/// Send one notification per un-announced batch, then mark them announced.
fn announce(db: &Arc<Mutex<Connection>>, notifier: &notify::Notifier) {
    let batches = {
        let conn = match db.lock() {
            Ok(conn) => conn,
            Err(_) => return,
        };
        match store::pending_notifications(&conn, 500) {
            Ok(batches) => batches,
            Err(error) => {
                tracing::warn!(%error, "could not read pending notifications");
                return;
            }
        }
    };
    if batches.is_empty() {
        return;
    }

    let keys: Vec<String> = batches
        .iter()
        .flat_map(|batch| batch.questions.iter().map(|q| q.key.clone()))
        .collect();
    let failures = notifier.deliver(&notify::Event::Questions(batches));
    if !failures.is_empty() {
        for error in &failures {
            tracing::warn!(%error, "notification failed");
        }
        // Leave them un-announced so the next tick tries again.
        return;
    }
    if let Ok(conn) = db.lock() {
        if let Err(error) = store::mark_notified(&conn, &keys) {
            tracing::warn!(%error, "could not record that questions were announced");
        }
    }
}

fn sweep(db: &Arc<Mutex<Connection>>, notifier: &notify::Notifier) {
    let silent = {
        let conn = match db.lock() {
            Ok(conn) => conn,
            Err(_) => return,
        };
        match store::expire_due(&conn) {
            Ok(n) if n > 0 => tracing::info!(expired = n, "questions passed their TTL"),
            Err(error) => tracing::warn!(%error, "expiry sweep failed"),
            _ => {}
        }
        match store::detect_silence(&conn, None) {
            Ok(names) => names,
            Err(error) => {
                tracing::warn!(%error, "silence sweep failed");
                Vec::new()
            }
        }
    };
    if silent.is_empty() {
        return;
    }
    for name in &silent {
        tracing::warn!(signal = %name, "signal went silent past its deadline");
    }
    // Reported once per transition, so a long outage is not a repeating alarm.
    for error in notifier.deliver(&notify::Event::Silence(silent)) {
        tracing::warn!(%error, "silence notification failed");
    }
}

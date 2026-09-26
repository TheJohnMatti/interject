//! `interjectd` — the daemon, plus the CLI a human answers questions with.

use interjectd::{api, store};

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use interject::{Client, Inbox};
use rusqlite::Connection;
use serde_json::Value;

const DEFAULT_ADDR: &str = "127.0.0.1:8787";

#[derive(Parser)]
#[command(
    name = "interjectd",
    about = "A durable ask() primitive: stop a program, ask a human, resume.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon.
    Serve {
        #[arg(long, default_value = DEFAULT_ADDR, env = "INTERJECT_ADDR")]
        addr: String,
        #[arg(long, default_value = "interject.sqlite3", env = "INTERJECT_DB")]
        db: String,
        /// Longest a single long-poll may hold a connection, in seconds.
        #[arg(long, default_value_t = api::DEFAULT_MAX_WAIT_SECS)]
        max_wait: u64,
        /// Require this bearer token on every request.
        #[arg(long, env = "INTERJECT_TOKEN")]
        token: Option<String>,
        /// How often to sweep for expired questions and silent signals.
        #[arg(long, default_value_t = 10)]
        sweep_secs: u64,
    },
    /// List open questions.
    Inbox {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        batch_key: Option<String>,
        /// Answer each question interactively instead of only listing them.
        #[arg(long)]
        answer: bool,
    },
    /// Answer one question by key.
    Answer {
        key: String,
        /// The answer. Parsed as JSON when possible, otherwise sent as a string.
        value: String,
        #[arg(long)]
        as_user: Option<String>,
    },
    /// Show declared signals and whether any have gone silent.
    Signals,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            addr,
            db,
            max_wait,
            token,
            sweep_secs,
        } => serve(addr, db, max_wait, token, sweep_secs),
        Command::Inbox {
            limit,
            batch_key,
            answer,
        } => inbox(limit, batch_key.as_deref(), answer),
        Command::Answer {
            key,
            value,
            as_user,
        } => answer_one(&key, &value, as_user.as_deref()),
        Command::Signals => signals(),
    }
}

#[tokio::main]
async fn serve(
    addr: String,
    db: String,
    max_wait: u64,
    token: Option<String>,
    sweep_secs: u64,
) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "interjectd=info".into()),
        )
        .init();

    let conn = store::open(&db)?;
    let state = api::AppState::new(conn, max_wait, token);

    // Expiry and silence detection also happen lazily on read, so this sweeper
    // exists to make them happen when nobody is reading.
    let sweeper_db = state.db();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(sweep_secs.max(1)));
        loop {
            ticker.tick().await;
            sweep(&sweeper_db);
        }
    });

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, db = %db, "interjectd listening");

    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("interjectd stopped");
    Ok(())
}

fn sweep(db: &Arc<Mutex<Connection>>) {
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
        Ok(names) if !names.is_empty() => {
            for name in names {
                tracing::warn!(signal = %name, "signal went silent past its deadline");
            }
        }
        Err(error) => tracing::warn!(%error, "silence sweep failed"),
        _ => {}
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown requested");
}

fn print_inbox(inbox: &Inbox) {
    if inbox.batches.is_empty() {
        println!("nothing waiting on you.");
        return;
    }
    for batch in &inbox.batches {
        println!("\n{}  ({} waiting)", batch.prompt, batch.count);
        println!("  batch: {}  kind: {}", batch.batch_key, batch.kind);
        if let Some(options) = &batch.options {
            println!("  options: {options}");
        }
        for question in &batch.questions {
            let context = question
                .context
                .as_ref()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string());
            let suggested = question
                .suggest
                .as_ref()
                .and_then(|s| s["value"].as_str().map(str::to_string))
                .map(|v| format!("  (suggested: {v})"))
                .unwrap_or_default();
            println!("  {}  {}{}", &question.key[..12], context, suggested);
        }
    }
    println!();
}

fn inbox(limit: usize, batch_key: Option<&str>, interactive: bool) -> Result<()> {
    let client = Client::from_env();
    let inbox = client
        .inbox(limit, batch_key)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !interactive {
        print_inbox(&inbox);
        return Ok(());
    }

    for batch in &inbox.batches {
        let options: Vec<String> = batch
            .options
            .as_ref()
            .and_then(|o| o.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
            .collect();

        for question in &batch.questions {
            println!("\n{}", batch.prompt);
            if let Some(context) = &question.context {
                println!("  {context}");
            }
            if let Some(suggest) = &question.suggest {
                println!("  suggested: {suggest}");
            }
            let raw = if options.is_empty() {
                prompt_line("  answer (blank to skip): ")?
            } else {
                for (index, option) in options.iter().enumerate() {
                    println!("  {}) {}", index + 1, option);
                }
                let choice = prompt_line("  choose a number (blank to skip): ")?;
                match choice.trim().parse::<usize>() {
                    Ok(n) if n >= 1 && n <= options.len() => options[n - 1].clone(),
                    _ => String::new(),
                }
            };
            if raw.trim().is_empty() {
                println!("  skipped.");
                continue;
            }
            client
                .answer(&question.key, &parse_value(raw.trim()), Some("cli"))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("  answered.");
        }
    }
    Ok(())
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line)
}

/// Accept `true`, `42`, `"text"` or bare text, so the CLI is pleasant to use
/// without making the caller think about JSON quoting.
fn parse_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn answer_one(key: &str, value: &str, as_user: Option<&str>) -> Result<()> {
    let client = Client::from_env();
    let snapshot = client
        .answer(key, &parse_value(value), as_user.or(Some("cli")))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{} -> {}", &snapshot.key[..12], snapshot.state);
    Ok(())
}

fn signals() -> Result<()> {
    let client = Client::from_env();
    let signals = client.signals().map_err(|e| anyhow::anyhow!("{e}"))?;
    if signals.is_empty() {
        println!("no signals declared.");
        return Ok(());
    }
    for signal in signals {
        let marker = if signal.state == "silent" {
            "SILENT"
        } else {
            "live  "
        };
        println!(
            "{marker}  {}  last seen {}{}",
            signal.name,
            signal.last_seen,
            signal
                .due_at
                .map(|d| format!("  due {d}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

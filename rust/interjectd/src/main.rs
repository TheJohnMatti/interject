//! `interjectd` — the daemon, plus the CLI a human answers questions with.

use interjectd::daemon::{self, ServeOptions};
use interjectd::{api, notify};

use std::io::{self, Write};

use anyhow::Result;
use clap::{Parser, Subcommand};
use interject::{Client, Inbox};
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
        /// ntfy server to publish to and subscribe from.
        #[arg(long, default_value = notify::DEFAULT_NTFY_BASE, env = "INTERJECT_NTFY_BASE")]
        ntfy_base: String,
        /// ntfy topic notifications are published to.
        #[arg(long, env = "INTERJECT_NTFY_TOPIC")]
        ntfy_topic: Option<String>,
        /// ntfy topic the daemon subscribes to for one-tap answers. Treat it as
        /// a secret; answers are additionally signed, but the topic should not
        /// be guessable.
        #[arg(long, env = "INTERJECT_ANSWER_TOPIC")]
        answer_topic: Option<String>,
        /// Extra sink: POST each rendered notification as JSON here.
        #[arg(long, env = "INTERJECT_WEBHOOK")]
        webhook: Option<String>,
        /// How long to wait before announcing new questions, so a burst arrives
        /// as one notification instead of hundreds.
        #[arg(long, default_value_t = 10)]
        notify_debounce_secs: u64,
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
    /// Print a roll-up of what is waiting.
    Digest,
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
            ntfy_base,
            ntfy_topic,
            answer_topic,
            webhook,
            notify_debounce_secs,
        } => serve(ServeOptions {
            addr,
            db,
            max_wait,
            token,
            sweep_secs,
            notify_debounce_secs,
            notify: notify::NotifyConfig {
                ntfy_base,
                ntfy_topic,
                answer_topic,
                webhook,
            },
        }),
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
        Command::Digest => digest(),
    }
}

#[tokio::main]
async fn serve(options: ServeOptions) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "interjectd=info".into()),
        )
        .init();

    let (addr, serving) = daemon::bind(options).await?;
    tracing::info!(%addr, "interjectd listening");
    serving.await
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

fn digest() -> Result<()> {
    let client = Client::from_env();
    let digest = client.digest().map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{} question(s) waiting on you.", digest.open);
    if let Some(oldest) = digest.oldest_created_at {
        println!("oldest since {oldest}");
    }
    if digest.silent_signals.is_empty() {
        println!("no signals have gone quiet.");
    } else {
        println!("gone quiet: {}", digest.silent_signals.join(", "));
    }
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

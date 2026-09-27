//! `interjectd` — the daemon, plus the CLI a human answers questions with.

use interjectd::daemon::{self, ServeOptions};
use interjectd::{api, notify, store};

use std::io::{self, Write};

use anyhow::Result;
use clap::{Parser, Subcommand};
use interject::{Client, Inbox, Policy};
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
        /// Service that proposes answers for questions arriving without a
        /// suggestion. Receives the question as JSON, returns
        /// {"value": .., "confidence": 0..1}. A client-supplied suggestion wins.
        #[arg(long, env = "INTERJECT_SUGGESTER_URL")]
        suggester_url: Option<String>,
        /// Drop the stored context of questions settled more than this many days
        /// ago. Answers are always kept, so replay still works. 0 keeps all.
        #[arg(long, default_value_t = 0)]
        retain_days: i64,
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
    /// Show agreement and ask-reduction per question class.
    Calibration,
    /// Print a short code that pairs a phone or browser with this daemon.
    Pair {
        #[arg(long, default_value = "default", env = "INTERJECT_PROJECT")]
        project: String,
        /// A note for your own benefit, e.g. "pixel" or "work laptop".
        #[arg(long)]
        label: Option<String>,
        /// How long the code stays valid, in seconds.
        #[arg(long, default_value_t = 600)]
        ttl: i64,
        #[arg(long, default_value = "interject.sqlite3", env = "INTERJECT_DB")]
        db: String,
    },
    /// Manage long-lived tokens.
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// Inspect or change the triage policy for a question class.
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },
}

#[derive(Subcommand)]
enum TokenAction {
    /// Mint a token for a project and print it once.
    Create {
        #[arg(long, default_value = "default", env = "INTERJECT_PROJECT")]
        project: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, default_value = "interject.sqlite3", env = "INTERJECT_DB")]
        db: String,
    },
    /// List tokens. Only their labels and use, never the secrets.
    List {
        #[arg(long, default_value = "interject.sqlite3", env = "INTERJECT_DB")]
        db: String,
    },
}

#[derive(Subcommand)]
enum PolicyAction {
    /// List every class that has a policy.
    List,
    /// Set the policy for one class. Unspecified fields keep their current value.
    Set {
        question_id: String,
        /// Allow this class to be auto-answered when the other conditions hold.
        #[arg(long)]
        enable: bool,
        /// Stop auto-answering this class.
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
        /// Minimum confidence before a suggestion may be used at all.
        #[arg(long)]
        threshold: Option<f64>,
        /// Minimum measured agreement with humans before trusting this class.
        #[arg(long)]
        agreement_target: Option<f64>,
        /// Fraction of auto-answers also shown to a human, for measurement only.
        #[arg(long)]
        shadow_rate: Option<f64>,
        /// How many compared cases are needed before auto-answering at all.
        #[arg(long)]
        min_samples: Option<i64>,
    },
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
            suggester_url,
            retain_days,
        } => serve(ServeOptions {
            addr,
            db,
            max_wait,
            token,
            sweep_secs,
            notify_debounce_secs,
            suggester_url,
            retain_days,
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
        Command::Calibration => calibration(),
        Command::Pair {
            project,
            label,
            ttl,
            db,
        } => pair(&db, &project, label.as_deref(), ttl),
        Command::Token { action } => token(action),
        Command::Policy { action } => policy(action),
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

/// Pairing and token minting touch the database directly rather than going over
/// HTTP: they are how you get a credential in the first place, so requiring one
/// would be circular. Both therefore run on the machine that owns the database.
fn pair(db: &str, project: &str, label: Option<&str>, ttl: i64) -> Result<()> {
    let conn = store::open(db)?;
    let code = store::create_pairing(&conn, project, label, ttl)?;
    println!("pairing code: {code}");
    println!("valid for {ttl}s, single use. Open the daemon's web inbox and type it in.");
    Ok(())
}

fn token(action: TokenAction) -> Result<()> {
    match action {
        TokenAction::Create { project, label, db } => {
            let conn = store::open(&db)?;
            let token = store::create_token(&conn, &project, label.as_deref(), "project")?;
            println!("{token}");
            eprintln!(
                "\nThis is shown once and stored only as a hash. Note that creating the first \n\
                 token turns off open mode: every request now needs one."
            );
            Ok(())
        }
        TokenAction::List { db } => {
            let conn = store::open(&db)?;
            let rows = store::tokens(&conn)?;
            if rows.is_empty() {
                println!("no tokens; this daemon is in open mode.");
                return Ok(());
            }
            for row in rows {
                println!(
                    "{:<12} {:<8} {:<20} created {}{}",
                    row.project,
                    row.kind,
                    row.label.unwrap_or_else(|| "-".to_string()),
                    row.created_at,
                    row.last_used_at
                        .map(|t| format!("  last used {t}"))
                        .unwrap_or_else(|| "  never used".to_string()),
                );
            }
            Ok(())
        }
    }
}

fn percent(value: Option<f64>) -> String {
    value
        .map(|v| format!("{:.1}%", v * 100.0))
        .unwrap_or_else(|| "-".to_string())
}

fn calibration() -> Result<()> {
    let client = Client::from_env();
    let classes = client.calibration().map_err(|e| anyhow::anyhow!("{e}"))?;
    if classes.is_empty() {
        println!("no questions yet, so nothing to calibrate.");
        return Ok(());
    }
    println!(
        "{:<24} {:>8} {:>10} {:>8} {:>8} {:>12}  auto-answering",
        "class", "compared", "agreement", "auto", "human", "asks saved"
    );
    for class in classes {
        println!(
            "{:<24} {:>8} {:>10} {:>8} {:>8} {:>12}  {}",
            class.question_id,
            class.compared,
            percent(class.agreement_rate),
            class.auto_answered,
            class.human_answered,
            percent(class.ask_reduction),
            if class.enabled { "on" } else { "off" },
        );
        if class.shadow_compared > 0 {
            println!(
                "{:>26}shadow: {}/{} agreed",
                "", class.shadow_agreements, class.shadow_compared
            );
        }
    }
    Ok(())
}

fn policy(action: PolicyAction) -> Result<()> {
    let client = Client::from_env();
    match action {
        PolicyAction::List => {
            let policies = client.policies().map_err(|e| anyhow::anyhow!("{e}"))?;
            if policies.is_empty() {
                println!("no policies set; every class asks a human.");
                return Ok(());
            }
            for p in policies {
                println!(
                    "{:<24} {}  threshold {:.2}  agreement>={:.2}  shadow {:.0}%  min {}",
                    p.question_id,
                    if p.enabled { "on " } else { "off" },
                    p.threshold,
                    p.agreement_target,
                    p.shadow_rate * 100.0,
                    p.min_samples,
                );
            }
            Ok(())
        }
        PolicyAction::Set {
            question_id,
            enable,
            disable,
            threshold,
            agreement_target,
            shadow_rate,
            min_samples,
        } => {
            // Start from the stored policy so unspecified flags are untouched.
            let existing = client
                .policies()
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .into_iter()
                .find(|p| p.question_id == question_id);
            let mut policy = existing.unwrap_or(Policy {
                question_id: question_id.clone(),
                threshold: 0.95,
                agreement_target: 0.98,
                shadow_rate: 0.1,
                min_samples: 20,
                enabled: false,
            });
            if enable {
                policy.enabled = true;
            }
            if disable {
                policy.enabled = false;
            }
            if let Some(v) = threshold {
                policy.threshold = v;
            }
            if let Some(v) = agreement_target {
                policy.agreement_target = v;
            }
            if let Some(v) = shadow_rate {
                policy.shadow_rate = v;
            }
            if let Some(v) = min_samples {
                policy.min_samples = v;
            }
            let saved = client
                .set_policy(&policy)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "{}: auto-answering {}, threshold {:.2}, agreement>={:.2}, shadow {:.0}%, min {} samples",
                saved.question_id,
                if saved.enabled { "ON" } else { "off" },
                saved.threshold,
                saved.agreement_target,
                saved.shadow_rate * 100.0,
                saved.min_samples,
            );
            Ok(())
        }
    }
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

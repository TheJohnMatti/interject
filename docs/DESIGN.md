# interject — design

**A durable `ask()` primitive: any program can stop, ask a human, and resume —
days later, on another machine, in one tap.**

Convention follows `punctual/docs/DESIGN.md`: `D#` = locked decision, `O#` = open
question, `M#` = milestone. Decisions below were locked 2026-09-26.

---

## 1. The problem, measured

Not hypothetical — all of the following was measured on the author's machine on
2026-09-26.

**Silent death.** `auto_sniper_ml`'s pipeline stopped working at 20:50 on
Sep 23, when a `run_once.sh` invocation hung while holding the single-flight
lock. It was still alive 2 days 21 hours later. Every 5-minute scan since fired,
saw the lock, and exited — 2,765 lock-exits total. Three days of a pipeline that
was installed, enabled, "running," and doing nothing. Nobody was told. Across
its whole life it sent essentially one notification in 338 notify cycles.

**The ask gets hand-rolled every time.** The same "machine needs a human
judgment" handoff existed three times, in two repos, with three incompatible
file protocols: `auto_sniper_ml/data/clusters/label_requests.json`,
`auto_sniper_ml/data/clusters/vehicle_type_requests.json`, and
`alpha_machine/data/generation/thesis_requests.json` — plus two bespoke skills
(`label-clusters`, `classify-vehicle`) whose only job was to service two of them.

**A project died while blocked on a human.** `alpha_machine` took 203 commits in
5 days, then stopped with `ftmo_demo` correctly BLOCKED pending two human
attestations (`risk_acknowledgements`, `funnel:` terms). It needed two commands
from a person and never got them.

The generalisation: a modern pipeline is mostly autonomous but has a handful of
points where human judgment is irreducible. Today each of those is either a
bespoke file protocol nobody is watching, a blocking `input()` at a terminal
nobody is sitting at, or a hard stop.

## 2. The primitive

```python
from interject import ask

vehicle = ask(
    "Is this a car?",
    options=["car", "motorcycle", "boat", "trailer", "equipment"],
    context={"title": listing.title, "price": listing.price, "url": listing.url},
    id="vehicle_type",
    ttl="48h",
    default="skip",
    suggest={"value": "motorcycle", "confidence": 0.94},
)
```

The call registers the question, lets everything not downstream of it keep
running, reaches the human wherever they are, and returns the answer to *this
line* of *this program* — even if the process exited and was restarted days later
on different hardware. It may never reach a human at all, if the triage layer is
confident enough (§6).

**Two halves.** `ask()` is inbound: the machine needs you. `heartbeat()` is
outbound: the machine owes you a signal and went quiet. A tool with only one half
is half a tool, because both answer the same question — *does this deserve a
slice of scarce human attention right now?*

```python
from interject import heartbeat
heartbeat("autosniper.scan", expect_every="15m")
```

One line, and it is what would have caught the hung scraper on Sep 23 rather
than Sep 26.

## 3. Locked decisions

### D1 — A protocol, not FFI bindings

The core is a single Rust daemon, `interjectd`. Clients speak the HTTP/JSON
protocol in `docs/PROTOCOL.md`. There are **no language bindings** — each client
is a thin, independent library that speaks the protocol natively.

Rationale: no ABI, no per-platform wheel matrix (cibuildwheel × manylinux ×
musl × universal2, forever); the boundary has to cross machines anyway, since a
run suspended on one host must be answerable from a phone and resumable by a
worker elsewhere, so an in-process binding buys nothing; a new language becomes a
weekend rather than a binding project; and the protocol being the product surface
forces it to stay documented.

Ships as `interject` on PyPI (pure Python, stdlib only) and `interject` on
crates.io (client + daemon). Both names were verified available on 2026-09-26.

### D2 — Durability by idempotent replay, not process snapshots

No CRIU, no fork-and-freeze. A question has a content-addressed identity, the
answer is durable server-side, and a re-executed program asking the same question
is handed the stored answer immediately.

`question_key = sha256(project \x00 question_id \x00 sha256(canon(context)))`

This one decision buys deduplication, coalescing and crash-replay together, and
it is the same shape as `punctual`'s shipped `step()` contract.

**The replay contract, stated honestly:** between process starts, caller code
must be deterministic up to each ask site. Compute the context differently on the
second run and you get a new key and a new question.

**Known wart:** with `id=` omitted, the id is derived from `(file, function,
line)`, so editing the file changes the key and re-asks. Explicit stable `id=` is
recommended for anything long-lived, and `interject lint` warns on omission.

### D3 — Suspend by default, never block forever

`ask(wait=...)` long-polls for `wait` seconds (default 30). On timeout,
`on_timeout` governs:

| mode | behaviour | use |
|---|---|---|
| `suspend` *(default)* | raise `Suspended`; caller exits cleanly. The next run replays the key and returns instantly if answered. | daemons, scheduled jobs, CI |
| `block` | keep long-polling | interactive scripts with a human present |
| `default` | return the declared `default`, recorded as `source="default"` | best-effort enrichment |

The default is `suspend` **because of the hung scraper in §1**: a process that
blocks for three days holding a lock is the exact bug this project is named
after. A human-in-the-loop tool whose default mode is "hold resources and wait
for a human" would reproduce the disease it claims to cure.

### D4 — Multi-tenant protocol from day one, self-hosted first

Every request carries a project token and every row is project-scoped, but the
first artifact is one self-hosted binary over SQLite. Hosted mode is then a
deployment and an auth backend, not a rewrite.

### D5 — Independent of `punctual` for now

No dependency either way, and no bridge package yet. `punctual` remains the
scheduler and `interject` handles only the human edge; a `punctual-interject`
bridge may be revisited once `interject` has shipped something.

### D6 — Triage: suggestions from either side, decisions never from the model

A `suggest` may arrive two ways, and both are supported:

1. **Client-supplied** — the caller already ran a model and passes
   `suggest={"value": ..., "confidence": ...}`.
2. **Daemon-supplied** — the daemon is optionally configured with a model
   provider and generates a suggestion for questions that arrive without one.

Client-supplied always wins when both are available. Daemon-side generation is
opt-in per project, because it means the daemon holds an API key and sees the
question context.

Policy, per question class:

```
if suggest.confidence >= threshold and rolling_agreement(class) >= target:
        auto-answer, record source="auto"
else:
        ask the human
```

**Continuous calibration by shadow sampling.** A configurable fraction of
auto-answered questions are *also* asked of the human, marked `shadow_of`. The
shadow answer never changes the pipeline's result — it only measures agreement,
keeping the threshold honest forever without anyone auditing anything. It yields
the single number the project is judged on:

> asks reduced 83% at 99.1% measured agreement

Both failure modes are measurable — asking about what it could have resolved
(noise), and auto-answering what needed judgment (error) — so every claim in the
README is falsifiable. The model proposes; it never decides whether it was right.
The human is ground truth, always.

### D7 — Context stored, with an optional reference

Context is stored in the daemon by default, so an inbox is self-contained on a
phone with no access to the caller's filesystem. `context_ref` (a path or URL) is
an optional additional pointer that a surface may resolve, for bulk payloads or
for data the caller would rather not hand over.

### D8 — TTL expiry applies the default silently

On expiry the declared `default` is applied and recorded as `source="default"`.
No escalation notification — expiry is the *quiet* path by design, since a
question that already failed to be worth answering should not generate a second
interruption. The event is still recorded and appears in the digest, so a silent
default is auditable after the fact even though it is never interruptive.

### D9 — Device pairing for authentication

A human surface (phone, browser) is paired to a project with a short-lived code
issued by the CLI, exchanging it for a long-lived device token. No passwords, no
OAuth provider dependency, and it degrades gracefully to a purely local
single-user setup.

## 4. Data model

- **question** — `id`, `key`, `project`, `origin` (repo/job/run/site), `prompt`,
  `kind`, `options`, `context`, `context_ref`, `suggest`, `created_at`,
  `expires_at`, `default_value`, `on_timeout`, `priority`, `batch_key`,
  `shadow_of`, `state`
- **answer** — `question_key`, `value`, `source` (`human` | `auto` | `default`),
  `answered_by`, `answered_at`, `latency_ms`
- **signal** — `name`, `project`, `expect_every_seconds` | `expect_by`,
  `last_seen`, `state` (`live` | `silent` | `acked`)
- **policy** — per question-class auto-answer threshold, agreement target,
  shadow sample rate
- **device** — paired human surfaces and their tokens

`kind` ∈ `choice` | `multi` | `text` | `number` | `approve` | `rank` | `label`.
`approve` exists because `alpha_machine`'s blocker was exactly two approvals.

## 5. Delivery surfaces

Reuse the transport that already exists rather than rebuilding it: `ntfy` (with
action buttons, one tap per answer) and, later, the sink pattern `punctual`
already proved with built-in ntfy/Slack/Discord.

- **ntfy with action buttons** — one tap answers a `choice` or `approve`
- **CLI inbox** — `interject` opens a TUI; single-keystroke, batched
- **Web inbox** — one page, no build step, phone-first (required by D4)
- **Digest** — a scheduled roll-up rather than a stream

**Coalescing is mandatory, not a feature.** Four hundred "is this a car?"
questions arrive as one screen, deduplicated by key and ordered by information
gain — never as four hundred notifications. A tool that floods has failed its own
premise.

## 6. Non-goals

- Not an orchestrator. `punctual`, Temporal and Airflow schedule; `interject`
  handles only the human edge.
- Not a chat interface. Questions are typed and structured so they can be keyed,
  batched and auto-answered; free-form chat defeats all three.
- No process snapshotting (D2).
- Not a notification service. It decides *whether* to notify; ntfy delivers.

## 7. Prior art, honestly

- **Temporal** has durable human-in-the-loop signals but requires adopting
  Temporal wholesale — workers, workflows, a cluster. `interject` is one function
  call inside a script that already exists.
- **Airflow / Prefect** have sensors and manual-approval tasks, scoped to one
  DAG in one deployment, with no triage and no mobile answer path.
- **Slack approval bots** are bespoke per use case and have no durability — if
  the process dies, the approval is orphaned.
- **`input()`** blocks a process at a terminal nobody is sitting at.

The gap: the smallest possible dependency that gives durable, triaged human input
to *any* program, plus the calibration machinery that makes it ask less over time.

## 8. Milestones

Sliced the way `punctual` was — one PR per slice.

- **M0 — the loop closes. ✅ DONE 2026-09-26.** SQLite store with WAL,
  `POST /questions` (idempotent on key), long-poll woken by a broadcast channel
  rather than DB polling, `POST /answers`, `/v0/inbox`, `/healthz`, Python and
  Rust clients, and an `interjectd inbox --answer` CLI. Verified by
  `scripts/e2e.sh`, which runs in CI: a Python program asks, suspends, is
  answered through the CLI, and resumes with the answer.
- **M1 — durability. ✅ DONE 2026-09-26.** 26 tests over the store and the real
  HTTP stack: replay, restart survival (both answered and still-open questions),
  write-once answers, TTL expiry to a declared default, expiry without a default
  as an error rather than a guess, the long-poll waking on an answer rather than
  a timer, the daemon's cap on poll duration, and token rejection. Also fixed a
  real bug the tests found: serde collapses a present JSON `null` into `None`, so
  a declared default of `null` had been indistinguishable from no default.
- **M2 — reach.** ntfy sink with action buttons, coalescing, batch screens,
  digests. *Demo: close the laptop, tap the phone, the run resumes.*
- **M3 — triage.** `suggest` both ways, auto-answer policy, shadow sampling,
  calibration report. *Demo: the ask-reduction number.*
- **M4 — the outbound half.** `heartbeat()`/`expect()`, silence detection,
  escalation. *Demo: catch a hung job holding a lock — §1 reproduced on purpose.*
- **M5 — anyone.** Web inbox, project tokens, device pairing, Postgres, hosted
  deployment.
- **M6 — dogfood and delete.** Port all three `*_requests.json` protocols and
  both skills onto `interject` and delete the bespoke plumbing. The diff that
  removes three hand-rolled protocols from two repos is the best README artifact
  available.

## 9. Open questions

- **O1** — Routing. With "anyone" as the audience, who answers a given question?
  A project-wide inbox is enough for one person; teams need assignment, or at
  minimum claim-on-answer so two people don't answer the same thing.
- **O2** — Retention. Answered questions with stored context grow forever.
  Sweep policy, and does a purged answer break replay?
- **O3** — Are `rank` and `label` in the v0 `kind` set, or deferred? They carry
  most of the UI cost and neither is needed for the motivating cases.
- **O4** — Default shadow sample rate, and whether it should decay as agreement
  stabilises.
- **O5** — Does the daemon own scheduling of digests, or is that a cron job that
  calls it?

# interject

**A durable `ask()` primitive.** Any program can stop, ask a human, and resume —
days later, on another machine, in one tap.

```python
from interject import ask

vehicle = ask(
    "Is this a car?",
    options=["car", "motorcycle", "boat", "trailer"],
    id="vehicle_type",
    context={"title": listing.title, "price": listing.price},
    suggest={"value": "motorcycle", "confidence": 0.94},
)
```

That call registers a question, lets everything not downstream of it keep
running, reaches you wherever you are, and returns the answer **to this line of
this program** — even if the process exited and was restarted two days later on
different hardware. It may also never reach you at all, if the triage layer is
confident enough. That is the point.

The other half of the same idea, because a machine that has gone quiet also
needs your attention:

```python
from interject import heartbeat

heartbeat("autosniper.scan", expect_every="15m")   # silence past 15m is an event
```

## Why

Modern pipelines are mostly autonomous with a handful of points where human
judgment is irreducible. Today every one of those points is a bespoke JSON file
nobody is watching, a blocking `input()` at a terminal nobody is sitting at, or
a hard stop.

The motivating incident, measured rather than imagined: a scraping pipeline on
the author's machine stopped working at 20:50 on 2026-09-23, when a run hung
while holding a single-flight lock. Every 5-minute invocation for the next three
days fired, saw the lock, and exited — 2,765 no-ops. The pipeline was installed,
enabled, "running," and doing nothing. Nobody was told. It sent one notification
in its entire life, across 338 cycles.

Separately, the same "a machine needs a human judgment" handoff had been
hand-rolled three times across two repositories, with three incompatible file
formats and two bespoke scripts whose only job was to service them.

## Quickstart

```bash
# 1. run the daemon
cd rust && cargo run -p interjectd -- serve --db interject.sqlite3

# 2. from anywhere, ask something
export PYTHONPATH=python/src
python3 -c "
import interject
print(interject.ask('Is this a car?', options=['car','moto'], id='vehicle_type', wait=60))
"

# 3. answer it, in another terminal
cargo run -p interjectd -- inbox --answer
```

Step 2 resumes the moment you answer in step 3. Kill it between the two and run
it again — it returns the stored answer instead of asking twice, which is the
whole point.

`scripts/e2e.sh` runs that entire sequence unattended, including the live
long-poll, TTL expiry and silence detection.

## One-tap answers from a phone

```bash
interjectd serve \
  --ntfy-topic your-notifications \
  --answer-topic your-answers-topic-keep-this-secret
```

Subscribe to the first topic in the ntfy app. Questions arrive as notifications
with buttons; tapping one publishes to the second topic, which the daemon reads
over a long-lived **outbound** connection. No public URL, no port forwarding, no
tunnel — it works from a laptop behind NAT.

Every button carries an HMAC of the question key, so knowing the answer topic is
not enough to answer anything. Point `--ntfy-base` at your own ntfy instance if
you would rather nothing transited a third party.

## Status

**M0, M1, M2 and M4 are done.** The loop closes, state survives restarts,
questions reach a phone and are answerable in one tap, and a job that goes quiet
when it owed you a signal says so. Still to come: triage and calibration (M3),
the web inbox and hosted mode (M5), and porting real pipelines onto it (M6).
Nothing is published to PyPI or crates.io yet.

Design and milestones are in [docs/DESIGN.md](docs/DESIGN.md), the wire protocol
in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Architecture

A single Rust daemon plus a documented HTTP/JSON protocol. There are no FFI
bindings — each language gets an independent thin client speaking the protocol
natively (`python/`, and a Rust client alongside the daemon). Adding a language
is a weekend, not a build matrix.

```
  your script ──┐
  GH Action ────┼── HTTP/JSON ──► interjectd ──► SQLite | Postgres
  your agent ───┘                     │
                                      ├──► ntfy (action buttons)
                                      ├──► CLI inbox (TUI)
                                      └──► web inbox (phone)
```

## License

Apache-2.0.

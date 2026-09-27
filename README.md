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

## Asking less over time

A question may carry a machine's proposal:

```python
interject.ask("Is this a car?", options=["car", "moto"], id="vehicle_type",
              context=listing, suggest={"value": "moto", "confidence": 0.94})
```

By default that changes nothing — the human still decides. Auto-answering is
opt-in per question class, and even then it requires the confidence to clear a
threshold, a minimum number of past cases to judge by, and **measured** agreement
with human answers above a target:

```bash
interjectd policy set vehicle_type --enable --threshold 0.95 --min-samples 50
interjectd calibration
```

```
class                    compared  agreement     auto    human   asks saved  auto-answering
vehicle_type                  312      99.4%      264       48        84.6%  on
                            shadow: 26/26 agreed
```

Agreement needs no setup to bootstrap: every ordinary question that carried a
suggestion and was then answered by a human is a free comparison. Once a class
stops being shown to people, a `shadow_rate` fraction of auto-answers are *also*
asked of a human — the answer never changes the pipeline's result, it only keeps
the estimate honest. Both failure modes are therefore measurable: asking about
what it could have resolved, and resolving what it should have asked.

Every auto-answer is recorded with `source: "auto"`, so "nobody decided this" is
never a mystery.

## Answering from a browser

The daemon serves its own inbox, so there is nothing else to deploy:

```bash
interjectd pair --label phone      # prints a short, single-use code
```

Open the daemon's address in a browser, type the code, and it holds a device
token from then on. The page groups questions by batch, shows each one's context
and any machine suggestion, and answers with one tap.

Tokens are stored only as SHA-256 digests. Creating the first one closes open
mode: every request then needs a token, and the project comes from the token
rather than from a header, so one tenant cannot reach another's questions.

## Status

**M0 through M5 are done, bar Postgres; M6 is in progress.** The loop closes,
state survives restarts, questions reach a phone and are answerable in one tap or
from the built-in web inbox, a job that goes quiet when it owed you a signal says
so, and a well-calibrated question class stops being asked at all.

Nothing is published to PyPI or crates.io yet. See
[docs/DESIGN.md](docs/DESIGN.md) for what each milestone did and what was
deliberately left out.

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

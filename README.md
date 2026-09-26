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

## Status

Pre-M0. Design is in [docs/DESIGN.md](docs/DESIGN.md), the wire protocol in
[docs/PROTOCOL.md](docs/PROTOCOL.md). Nothing is published yet.

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

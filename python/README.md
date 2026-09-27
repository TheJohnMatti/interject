# interject

**A durable `ask()` primitive.** A program can stop, ask a human, and resume —
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

## Why

Modern pipelines are mostly autonomous with a handful of points where human
judgment is irreducible. Today each of those is a bespoke JSON file nobody is
watching, a blocking `input()` at a terminal nobody is sitting at, or a hard
stop.

## How it behaves

**It does not block.** By default `ask` long-polls briefly and then raises
`Suspended`, so the caller exits cleanly. The next run recomputes the same
content-addressed key and gets the stored answer immediately:

```python
import interject

try:
    kind = interject.ask("Is this a car?", options=["car", "moto"],
                         id="vehicle_type", context=listing, wait=0)
except interject.Suspended:
    return  # somebody will answer; the next run picks it up
```

Pass `on_timeout="block"` for an interactive script, or `on_timeout="default"`
with a `default=` for best-effort enrichment.

**Scanning a backlog does not create a backlog of questions.** `ask` registers as
it checks, so use `peek` to look without creating:

```python
key = interject.key_for("vehicle_type", listing)
if interject.peek_answer(key) is None:
    ...  # not answered yet
```

**Silence is the other half.** A job can declare what it owes you, and going
quiet becomes an event:

```python
interject.heartbeat("nightly.etl", expect_every="26h")
```

## Installation

```bash
pip install interject
```

No dependencies, Python 3.9+. It talks to an `interjectd` daemon over HTTP —
see the [repository](https://github.com/TheJohnMatti/interject) for running one,
for the one-tap phone inbox, and for the triage layer that makes it ask less
over time.

## Configuration

| variable | default | meaning |
|---|---|---|
| `INTERJECT_URL` | `http://127.0.0.1:8787` | daemon base URL |
| `INTERJECT_TOKEN` | *(none)* | project or device token |
| `INTERJECT_PROJECT` | `default` | project name, for daemons without tokens |
| `INTERJECT_IDENTITY` | *(none)* | who you answer as, for routing |

Licensed under Apache-2.0.

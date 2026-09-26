# interject wire protocol — v0

HTTP/1.1 with JSON bodies. This document is the contract every client
implements; there are no FFI bindings (see DESIGN.md D1).

Base path: `/v0`. Default listen address: `127.0.0.1:8787`.

## Authentication

```
Authorization: Bearer <project-token>
```

The token identifies the project, which scopes every row. For single-tenant
local development the daemon may be run with `--open` and no token, in which
case `X-Interject-Project: <name>` selects the project.

## Question identity

A question's identity is content-addressed. Clients MUST compute it exactly as
follows, because the key is how crash-replay, deduplication and coalescing all
work.

```
canon(context)  = JSON with sorted keys, separators "," and ":",
                  non-ASCII preserved, encoded UTF-8
context_digest  = sha256(canon(context))                      -> 64 hex chars
question_key    = sha256(project \x00 question_id \x00 context_digest)
```

`context` absent is treated as `{}`. Both digests are lowercase hex.

Two consequences, by design: asking the "same" question twice yields one queue
entry and one answer; and a program re-executed after a crash recomputes the
same key and is handed the stored answer immediately.

## POST /v0/questions

Register a question. **Idempotent on `key`.** This is both the ask path and the
replay path — a client always calls this first, even when resuming.

```json
{
  "key": "<64 hex>",
  "id": "vehicle_type",
  "prompt": "Is this a car?",
  "kind": "choice",
  "options": ["car", "motorcycle", "boat", "trailer"],
  "context": { "title": "2018 Honda CBR", "price": 4200 },
  "context_ref": null,
  "suggest": { "value": "motorcycle", "confidence": 0.94 },
  "ttl_seconds": 172800,
  "default": "skip",
  "on_timeout": "suspend",
  "priority": 5,
  "batch_key": "autosniper.vehicle_type",
  "origin": {
    "repo": "auto_sniper_ml",
    "job": "scan",
    "run": "01J8...",
    "site": "src/ml/vehicle_type.py:88"
  }
}
```

`kind` is one of `choice`, `multi`, `text`, `number`, `approve`, `rank`, `label`.

`context` and `context_ref` follow DESIGN.md D7: context is stored in the daemon
by default so the inbox is self-contained on a phone, and `context_ref` is an
optional pointer (path or URL) the answering surface may resolve for bulk or
sensitive payloads. At least one of the two SHOULD be present for any question a
human will see.

Response `200`:

```json
{
  "key": "<64 hex>",
  "state": "open",
  "created": true,
  "answer": null,
  "expires_at": "2026-09-28T18:04:00Z"
}
```

`state` is `open`, `answered` or `expired`. When `answered`, `answer` is
populated and the client returns immediately — this is the replay hit.

## GET /v0/questions/{key}?wait=30

Long-poll for an answer. `wait` is seconds, `0` for an immediate check, capped
by the daemon (default cap 300).

Response `200` on answer:

```json
{
  "key": "<64 hex>",
  "state": "answered",
  "answer": {
    "value": "motorcycle",
    "source": "human",
    "answered_by": "john",
    "answered_at": "2026-09-26T18:41:02Z",
    "latency_ms": 41233
  }
}
```

Response `200` on timeout: `{"key": "...", "state": "open"}`. The client then
applies `on_timeout` locally (DESIGN.md D3). `404` if the key is unknown.

`source` is `human`, `auto` (answered by the triage layer) or `default`
(TTL expired and the declared default was applied).

## POST /v0/answers

```json
{ "key": "<64 hex>", "value": "motorcycle", "answered_by": "john", "source": "human" }
```

`source` is optional and defaults to `human`. A client passes `"default"` when it
applied a declared default locally under `on_timeout="default"`, so that the
value is recorded once and a later replay returns the same answer rather than
re-defaulting — the default must be as durable as a human's answer.

`200` with the stored answer. `409` if already answered by a different source;
answers are write-once except for shadow rows (see below).

## GET /v0/inbox?limit=50&batch_key=

Open questions for the authenticated project, newest first, grouped by
`batch_key` so a surface can render one screen per class rather than one per
question.

```json
{
  "batches": [
    {
      "batch_key": "autosniper.vehicle_type",
      "prompt": "Is this a car?",
      "kind": "choice",
      "options": ["car", "motorcycle", "boat", "trailer"],
      "count": 137,
      "questions": [ { "key": "...", "context": {...}, "suggest": {...} } ]
    }
  ]
}
```

## POST /v0/signals/heartbeat

The outbound half. Declares that a named signal is alive and when it is next
expected.

```json
{ "name": "autosniper.scan", "expect_every_seconds": 900 }
```

Silence past `expect_every_seconds` transitions the signal to `silent`, which is
itself an event delivered to the configured sinks. `expect_by` (an absolute
RFC 3339 timestamp) is accepted as an alternative for one-shot expectations.

## GET /v0/signals

```json
{ "signals": [
  { "name": "autosniper.scan", "state": "silent",
    "last_seen": "2026-09-23T20:50:11Z", "expect_every_seconds": 900 }
] }
```

## Shadow questions

When the triage layer auto-answers, it may additionally enqueue the same
question for a human with `shadow_of` set to the answered key. A shadow answer
never changes the pipeline's result; it exists only to measure agreement and
keep the auto-answer threshold honest (DESIGN.md D6). Shadow rows are the one
exception to write-once answers.

## Errors

```json
{ "error": { "code": "unknown_key", "message": "no question with that key" } }
```

Codes: `unauthorized`, `unknown_key`, `already_answered`, `invalid_request`,
`expired`, `rate_limited`, `internal`.

## Versioning

The path carries the major version. Additive fields are not breaking; clients
MUST ignore unknown fields.

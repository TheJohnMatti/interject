# interject

Rust client for [interject](https://github.com/TheJohnMatti/interject) — a
durable `ask()` primitive. A program can stop, ask a human, and resume days
later on another machine.

```rust
use interject::{Ask, Client};

let client = Client::from_env();
let vehicle = client.ask(
    Ask::new("Is this a car?")
        .id("vehicle_type")
        .options(["car", "motorcycle", "boat"])
        .context(serde_json::json!({"title": "2018 Honda CBR"}))
        .ttl_secs(48 * 3600),
)?;
# Ok::<(), interject::Error>(())
```

The question is content-addressed, so calling this again with the same `id` and
`context` returns the stored answer instead of asking twice — which is what lets
a crashed or restarted program resume where it left off. By default `ask`
suspends rather than blocking: it returns `Error::Suspended`, the caller exits
cleanly, and the next run picks up the answer.

This crate talks to `interjectd` over the HTTP protocol documented in the
repository. It is an independent implementation of that protocol, sharing no
code with the Python client; a cross-language question-key vector is pinned in
both test suites so the two cannot drift.

Licensed under Apache-2.0.

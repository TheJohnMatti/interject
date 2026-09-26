# interjectd + the Rust client

A cargo workspace with two crates:

- **`interject`** — the Rust client. Independent implementation of
  [`docs/PROTOCOL.md`](../docs/PROTOCOL.md), sharing no code with the Python
  client. A pinned cross-language key vector in both test suites keeps the two
  honest about question identity (DESIGN.md D2).
- **`interjectd`** — the daemon, and the CLI a human answers questions with.

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings

# run it
cargo run -p interjectd -- serve --db interject.sqlite3

# answer things
cargo run -p interjectd -- inbox --answer
cargo run -p interjectd -- signals
```

The store is SQLite with WAL. Timestamps are ISO-8601 UTC text so that lexical
ordering equals chronological ordering.

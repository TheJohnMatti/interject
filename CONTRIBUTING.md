# Contributing

## Layout

```
python/   the Python client (stdlib only, 3.9+)
rust/     a cargo workspace: `interject` (client) and `interjectd` (daemon + CLI)
docs/     DESIGN.md is the decision log; PROTOCOL.md is the wire contract
scripts/  e2e.sh runs the whole cross-language loop
```

The two clients are **independent implementations** of `docs/PROTOCOL.md` and
share no code. A cross-language question-key vector is pinned in both test
suites, so if canonical JSON ever diverges between them, one of the two fails.

## Running things

```bash
# Python
cd python && uv venv --python 3.12 && uv pip install -e ".[dev]"
.venv/bin/python -m pytest -q --cov=interject
.venv/bin/ruff check src tests && .venv/bin/mypy src/interject

# Rust
cd rust && cargo test
cargo clippy --all-targets -- -D warnings && cargo fmt --check

# Everything, together
./scripts/e2e.sh
```

`pre-commit install` wires ruff, `cargo fmt` and clippy into your commits.

## What good looks like here

- **Decisions go in `docs/DESIGN.md`** as `D#`, with the reasoning and the cost.
  Open questions are `O#`. Keep it current instead of explaining a choice twice.
- **Protocol changes are breaking changes** unless they are additive; clients
  must ignore unknown fields.
- **Tests state the behaviour, not the implementation.** A test name should read
  as a claim about the system, and the body should fail if that claim stops
  being true — `a_waiting_caller_is_woken_by_the_answer_not_by_a_timer` asserts
  on elapsed time, so a regression to polling fails rather than merely slows.
- **Failure paths degrade towards asking a human.** A suggester that is down, a
  malformed suggestion, an expired question with no default: none of these may
  turn into a guess.

## Releasing

Releases are entirely tag-driven and publish to PyPI and crates.io at once:

```bash
# bump the version in python/pyproject.toml AND rust/Cargo.toml to match
git tag v0.1.0 && git push origin v0.1.0
```

The workflow refuses to start if the tag and the two manifests disagree, because
a release that half-succeeds across two registries is much worse than one that
never begins.

One-time setup before the first release:

- PyPI: configure a trusted publisher for this repository and the `release`
  environment, so nothing needs an API token in secrets.
- crates.io: add `CARGO_REGISTRY_TOKEN` to the `release` environment.

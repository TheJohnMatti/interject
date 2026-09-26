# interjectd

The Rust daemon. Not started yet — see the M0 milestone in
[../docs/DESIGN.md](../docs/DESIGN.md) and the contract it must implement in
[../docs/PROTOCOL.md](../docs/PROTOCOL.md).

Until it exists, the Python client is tested against an in-process stub daemon
(`python/tests/conftest.py`) that implements the same endpoints.

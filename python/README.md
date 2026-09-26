# interject (Python client)

Pure-stdlib client for the [interject](../README.md) protocol. No dependencies,
Python 3.9+.

```python
from interject import ask, heartbeat

ok = ask("Deploy to production?", kind="approve", id="deploy_gate")
heartbeat("nightly.etl", expect_every="26h")
```

Configured by environment:

| variable | default | meaning |
|---|---|---|
| `INTERJECT_URL` | `http://127.0.0.1:8787` | daemon base URL |
| `INTERJECT_TOKEN` | *(none)* | project token, sent as a bearer token |
| `INTERJECT_PROJECT` | `default` | project name (used when the daemon runs `--open`) |

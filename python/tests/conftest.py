"""A stub daemon, so the client can be tested without the Rust daemon existing.

It implements just enough of docs/PROTOCOL.md to exercise the client's control
flow, and records every request so tests can assert on the wire format.
"""
from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import parse_qs, urlparse

import pytest


class StubDaemon:
    """In-process daemon double. Thread-safe; the HTTP server is threaded."""

    def __init__(self) -> None:
        self.questions: dict[str, dict[str, Any]] = {}
        self.answers: dict[str, dict[str, Any]] = {}
        self.expired: set = set()
        self.heartbeats: list[dict[str, Any]] = []
        self.requests: list[tuple[str, str, dict[str, Any] | None]] = []
        self._lock = threading.Lock()

    # -- helpers tests use directly -------------------------------------------------

    def answer(self, key: str, value: Any, source: str = "human") -> None:
        with self._lock:
            self.answers[key] = {
                "value": value,
                "source": source,
                "answered_by": "test",
                "answered_at": "2026-09-26T18:00:00Z",
                "latency_ms": 1,
            }

    def answer_after(self, key: str, value: Any, delay: float) -> threading.Thread:
        def run() -> None:
            time.sleep(delay)
            self.answer(key, value)

        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        return thread

    def expire(self, key: str) -> None:
        with self._lock:
            self.expired.add(key)

    def state_of(self, key: str) -> str:
        with self._lock:
            if key in self.answers:
                return "answered"
            if key in self.expired:
                return "expired"
            return "open"

    def snapshot(self, key: str) -> dict[str, Any]:
        state = self.state_of(key)
        with self._lock:
            return {
                "key": key,
                "state": state,
                "answer": self.answers.get(key),
                "expires_at": None,
            }


def _make_handler(stub: StubDaemon):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args: Any) -> None:  # keep test output clean
            pass

        def _body(self) -> dict[str, Any] | None:
            length = int(self.headers.get("Content-Length") or 0)
            if not length:
                return None
            return json.loads(self.rfile.read(length).decode("utf-8"))

        def _send(self, status: int, payload: dict[str, Any]) -> None:
            raw = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_POST(self) -> None:
            path = urlparse(self.path).path
            body = self._body()
            stub.requests.append(("POST", path, body))

            if path == "/v0/questions":
                assert body is not None
                key = body["key"]
                created = key not in stub.questions
                if created:
                    stub.questions[key] = body
                payload = stub.snapshot(key)
                payload["created"] = created
                self._send(200, payload)
                return

            if path == "/v0/answers":
                assert body is not None
                stub.answer(body["key"], body["value"], body.get("source", "human"))
                self._send(200, stub.snapshot(body["key"]))
                return

            if path == "/v0/signals/heartbeat":
                assert body is not None
                stub.heartbeats.append(body)
                self._send(200, {"name": body["name"], "state": "live"})
                return

            self._send(404, {"error": {"code": "not_found", "message": path}})

        def do_GET(self) -> None:
            parsed = urlparse(self.path)
            stub.requests.append(("GET", parsed.path, None))

            if parsed.path.startswith("/v0/questions/"):
                key = parsed.path.rsplit("/", 1)[-1]
                if key not in stub.questions:
                    self._send(404, {"error": {"code": "unknown_key", "message": key}})
                    return
                wait = float((parse_qs(parsed.query).get("wait") or ["0"])[0])
                deadline = time.time() + wait
                while stub.state_of(key) == "open" and time.time() < deadline:
                    time.sleep(0.02)
                self._send(200, stub.snapshot(key))
                return

            self._send(404, {"error": {"code": "not_found", "message": parsed.path}})

    return Handler


@pytest.fixture()
def daemon(monkeypatch):
    """Run a stub daemon on a free port and point the client at it."""
    import interject

    # The module-level client caches its Config on first use; reset it so each
    # test binds to its own stub port.
    interject._default_client = None

    stub = StubDaemon()
    server = ThreadingHTTPServer(("127.0.0.1", 0), _make_handler(stub))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    host, port = server.server_address[:2]
    monkeypatch.setenv("INTERJECT_URL", f"http://{host}:{port}")
    monkeypatch.setenv("INTERJECT_PROJECT", "test")
    monkeypatch.delenv("INTERJECT_TOKEN", raising=False)
    stub.url = f"http://{host}:{port}"
    try:
        yield stub
    finally:
        server.shutdown()
        server.server_close()
        interject._default_client = None

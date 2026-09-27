"""interject — a durable ``ask()`` primitive.

Stop a program, ask a human, resume days later on another machine:

    >>> from interject import ask
    >>> vehicle = ask("Is this a car?", options=["car", "moto"], id="vehicle_type")

and the outbound half of the same idea, where silence is the event:

    >>> from interject import heartbeat
    >>> heartbeat("nightly.etl", expect_every="26h")
"""
from __future__ import annotations

from typing import Any

from ._client import DEFAULT_PRIORITY, DEFAULT_WAIT, KINDS, ON_TIMEOUT, Client
from ._duration import Duration
from ._keys import context_digest, question_key
from .errors import (
    Expired,
    InterjectError,
    ProtocolError,
    Suspended,
    Unauthorized,
    Unreachable,
)

__all__ = [
    "DEFAULT_PRIORITY",
    "DEFAULT_WAIT",
    "KINDS",
    "ON_TIMEOUT",
    "Client",
    "Duration",
    "Expired",
    "InterjectError",
    "ProtocolError",
    "Suspended",
    "Unauthorized",
    "Unreachable",
    "__version__",
    "answer",
    "ask",
    "context_digest",
    "heartbeat",
    "inbox",
    "key_for",
    "peek",
    "peek_answer",
    "question_key",
    "release",
]

__version__ = "0.1.0"

_default_client: Client | None = None


def _client() -> Client:
    """The process-wide client, configured from the environment on first use."""
    global _default_client
    if _default_client is None:
        _default_client = Client()
    return _default_client


def ask(prompt: str, **kwargs: Any) -> Any:
    """Ask a human using the process-wide client. See :meth:`Client.ask`."""
    kwargs.setdefault("_stacklevel", 3)
    return _client().ask(prompt, **kwargs)


def answer(key: str, value: Any, **kwargs: Any) -> Any:
    """Answer a question using the process-wide client. See :meth:`Client.answer`."""
    return _client().answer(key, value, **kwargs)


def inbox(**kwargs: Any) -> Any:
    """Open questions, grouped by batch. See :meth:`Client.inbox`."""
    return _client().inbox(**kwargs)


def claim(key: str, ttl_seconds: int | None = None) -> Any:
    """Hold a question while you work on it. See :meth:`Client.claim`."""
    return _client().claim(key, ttl_seconds)


def release(key: str) -> Any:
    """Give up a claim. See :meth:`Client.release`."""
    return _client().release(key)


def assign(key: str, to: str | None) -> Any:
    """Route a question to one person. See :meth:`Client.assign`."""
    return _client().assign(key, to)


def key_for(id: str, context: Any = None) -> str:
    """The key the process-wide client would use. See :meth:`Client.key_for`."""
    return _client().key_for(id, context)


def peek(key: str) -> Any:
    """Look at a question without creating one. See :meth:`Client.peek`."""
    return _client().peek(key)


def peek_answer(key: str, default: Any = None) -> Any:
    """The stored answer for ``key``, or ``default``. See :meth:`Client.peek_answer`."""
    return _client().peek_answer(key, default)


def heartbeat(name: str, **kwargs: Any) -> None:
    """Report a live signal using the process-wide client. See :meth:`Client.heartbeat`."""
    return _client().heartbeat(name, **kwargs)

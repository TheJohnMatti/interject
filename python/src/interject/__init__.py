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
    "ask",
    "context_digest",
    "heartbeat",
    "question_key",
]

__version__ = "0.0.1.dev0"

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


def heartbeat(name: str, **kwargs: Any) -> None:
    """Report a live signal using the process-wide client. See :meth:`Client.heartbeat`."""
    return _client().heartbeat(name, **kwargs)

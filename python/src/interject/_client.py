"""The ``ask()`` primitive and its outbound twin, ``heartbeat()``."""
from __future__ import annotations

import os
import sys
from collections.abc import Mapping, Sequence
from typing import Any

from ._config import Config
from ._duration import Duration, seconds
from ._http import Transport
from ._keys import question_key
from .errors import Expired, ProtocolError, Suspended

_UNSET = object()

#: Question kinds the protocol defines. See docs/PROTOCOL.md.
KINDS = ("choice", "multi", "text", "number", "approve", "rank", "label")

#: What to do when ``wait`` elapses with the question still open. See DESIGN.md D3.
ON_TIMEOUT = ("suspend", "block", "default")

DEFAULT_WAIT = 30
DEFAULT_PRIORITY = 5


class Client:
    """A connection to one project on one daemon.

    Usually you want the module-level :func:`ask` and :func:`heartbeat`, which
    share a process-wide client configured from the environment. Construct this
    directly to talk to more than one project or daemon from one process.
    """

    def __init__(
        self,
        url: str | None = None,
        token: str | None = None,
        project: str | None = None,
        identity: str | None = None,
    ) -> None:
        self._config = Config(url=url, token=token, project=project, identity=identity)
        self._transport = Transport(self._config)

    @property
    def project(self) -> str:
        return self._config.project

    def ask(
        self,
        prompt: str,
        *,
        options: Sequence[Any] | None = None,
        kind: str | None = None,
        context: Mapping[str, Any] | None = None,
        context_ref: str | None = None,
        id: str | None = None,
        ttl: Duration = None,
        default: Any = _UNSET,
        on_timeout: str = "suspend",
        suggest: Mapping[str, Any] | None = None,
        wait: Duration = DEFAULT_WAIT,
        priority: int = DEFAULT_PRIORITY,
        batch_key: str | None = None,
        origin: Mapping[str, Any] | None = None,
        assign_to: str | None = None,
        _stacklevel: int = 2,
    ) -> Any:
        """Ask a human, and return their answer.

        The question is content-addressed (DESIGN.md D2), so calling this again
        with the same ``id`` and ``context`` returns the stored answer instead of
        asking twice — which is what makes a crashed or restarted program resume
        where it left off.

        Raises :class:`~interject.errors.Suspended` when ``wait`` elapses with
        the question still open and ``on_timeout`` is ``"suspend"`` (the
        default). Let it propagate and exit; the next run will pick up the
        answer.
        """
        if on_timeout not in ON_TIMEOUT:
            raise ValueError(f"on_timeout must be one of {ON_TIMEOUT}, got {on_timeout!r}")
        if kind is None:
            kind = _infer_kind(options, default)
        if kind not in KINDS:
            raise ValueError(f"kind must be one of {KINDS}, got {kind!r}")
        if on_timeout == "default" and default is _UNSET:
            raise ValueError('on_timeout="default" requires a default= value')
        if kind in ("choice", "multi", "rank") and not options:
            raise ValueError(f"kind={kind!r} requires options=")

        question_id = id or _derive_id(_stacklevel)
        key = question_key(self._config.project, question_id, context)
        wait_seconds = seconds(wait) or 0

        body: dict[str, Any] = {
            "key": key,
            "id": question_id,
            "prompt": prompt,
            "kind": kind,
            "options": list(options) if options is not None else None,
            "context": dict(context) if context is not None else None,
            "context_ref": context_ref,
            "suggest": dict(suggest) if suggest is not None else None,
            "ttl_seconds": seconds(ttl),
            "on_timeout": on_timeout,
            "priority": priority,
            "batch_key": batch_key or question_id,
            "origin": dict(origin) if origin is not None else _derive_origin(_stacklevel),
            "assign_to": assign_to,
        }
        if default is not _UNSET:
            body["default"] = default

        status, payload = self._transport.request(
            "POST", "/v0/questions", body, timeout=_timeout_for(0)
        )
        if status >= 400:
            raise ProtocolError(_error_message(payload, status, "registering question"))

        resolved = self._resolve(payload, key, question_id, default)
        if resolved is not _UNSET:
            return resolved

        # Not answered yet. Long-poll, then apply the timeout policy.
        while True:
            status, payload = self._transport.request(
                "GET",
                f"/v0/questions/{key}?wait={wait_seconds}",
                timeout=_timeout_for(wait_seconds),
            )
            if status >= 400:
                raise ProtocolError(_error_message(payload, status, "polling question"))

            resolved = self._resolve(payload, key, question_id, default)
            if resolved is not _UNSET:
                return resolved

            if on_timeout == "block":
                continue
            if on_timeout == "default":
                # Record the default so a later replay returns the same value.
                self._transport.request(
                    "POST",
                    "/v0/answers",
                    {"key": key, "value": default, "source": "default"},
                    timeout=_timeout_for(0),
                )
                return default
            raise Suspended(key, question_id)

    def key_for(self, id: str, context: Mapping[str, Any] | None = None) -> str:
        """The question key this client would use for ``id`` and ``context``."""
        return question_key(self._config.project, id, context)

    def peek(self, key: str) -> dict[str, Any] | None:
        """Look at a question without creating one.

        Returns the snapshot, or ``None`` if no question with that key exists.
        :meth:`ask` deliberately registers as it checks, which makes it the wrong
        tool for "has this been answered yet?" over a large backlog — this is that
        tool.
        """
        status, payload = self._transport.request(
            "GET", f"/v0/questions/{key}?wait=0", timeout=_timeout_for(0)
        )
        if status == 404:
            return None
        if status >= 400:
            raise ProtocolError(_error_message(payload, status, "peeking at a question"))
        return dict(payload)

    def peek_answer(self, key: str, default: Any = _UNSET) -> Any:
        """The answer for ``key``, or ``default`` if it is unknown or unanswered."""
        snapshot = self.peek(key)
        if snapshot is None or snapshot.get("state") != "answered":
            if default is _UNSET:
                return None
            return default
        answer = snapshot.get("answer") or {}
        return answer.get("value")

    def answer(
        self,
        key: str,
        value: Any,
        answered_by: str | None = None,
        source: str | None = None,
    ) -> dict[str, Any]:
        """Answer a question on a human's behalf.

        The answering side of the protocol, for building a surface in Python.
        Pipelines call :meth:`ask`; inboxes call this.
        """
        body: dict[str, Any] = {"key": key, "value": value}
        if answered_by is not None:
            body["answered_by"] = answered_by
        if source is not None:
            body["source"] = source
        status, payload = self._transport.request(
            "POST", "/v0/answers", body, timeout=_timeout_for(0)
        )
        if status >= 400:
            raise ProtocolError(_error_message(payload, status, "answering a question"))
        return dict(payload)

    def inbox(
        self,
        limit: int = 50,
        batch_key: str | None = None,
        for_identity: str | None = None,
        all_of_them: bool = False,
    ) -> dict[str, Any]:
        """Open questions, grouped by batch.

        By default this is *your* inbox: questions assigned to someone else, and
        questions another person currently holds a claim on, are left out.
        ``all_of_them`` lifts both filters.
        """
        path = f"/v0/inbox?limit={limit}"
        if batch_key is not None:
            path += f"&batch_key={batch_key}"
        if for_identity is not None:
            path += f"&for={for_identity}"
        if all_of_them:
            path += "&all=true"
        status, payload = self._transport.request("GET", path, timeout=_timeout_for(0))
        if status >= 400:
            raise ProtocolError(_error_message(payload, status, "reading the inbox"))
        return dict(payload)

    def claim(self, key: str, ttl_seconds: int | None = None) -> dict[str, Any]:
        """Take an advisory hold on a question while you work on it.

        Advisory: it stops two people duplicating effort, but answering is still
        governed by answers being write-once. Claims expire, so wandering off
        mid-question does not hide it from everyone else for long.
        """
        status, payload = self._transport.request(
            "POST",
            f"/v0/questions/{key}/claim",
            {"ttl_seconds": ttl_seconds},
            timeout=_timeout_for(0),
        )
        if status >= 400:
            raise ProtocolError(_error_message(payload, status, "claiming a question"))
        return dict(payload)

    def release(self, key: str) -> bool:
        """Give up a claim, so the question reappears for everyone at once."""
        status, payload = self._transport.request(
            "POST", f"/v0/questions/{key}/release", {}, timeout=_timeout_for(0)
        )
        if status >= 400:
            raise ProtocolError(_error_message(payload, status, "releasing a claim"))
        return bool(payload.get("released"))

    def assign(self, key: str, to: str | None) -> None:
        """Route a question to one person, or back to the pool with ``None``."""
        status, payload = self._transport.request(
            "POST", f"/v0/questions/{key}/assign", {"to": to}, timeout=_timeout_for(0)
        )
        if status >= 400:
            raise ProtocolError(_error_message(payload, status, "assigning a question"))

    def heartbeat(self, name: str, *, expect_every: Duration = None, expect_by: str | None = None) -> None:
        """Declare that a named signal is alive, and when it is next due.

        Silence past the deadline is itself an event — the outbound half of the
        same idea as :meth:`ask`. Exactly one of ``expect_every`` or
        ``expect_by`` should be given.
        """
        if (expect_every is None) == (expect_by is None):
            raise ValueError("pass exactly one of expect_every= or expect_by=")
        body: dict[str, Any] = {"name": name}
        if expect_every is not None:
            body["expect_every_seconds"] = seconds(expect_every)
        else:
            body["expect_by"] = expect_by
        status, payload = self._transport.request(
            "POST", "/v0/signals/heartbeat", body, timeout=_timeout_for(0)
        )
        if status >= 400:
            raise ProtocolError(_error_message(payload, status, "sending heartbeat"))

    def _resolve(
        self, payload: Mapping[str, Any], key: str, question_id: str, default: Any
    ) -> Any:
        """Turn a protocol response into an answer, or ``_UNSET`` if still open."""
        state = payload.get("state")
        if state == "answered":
            answer = payload.get("answer")
            if not isinstance(answer, dict) or "value" not in answer:
                raise ProtocolError("daemon reported 'answered' without an answer value")
            return answer["value"]
        if state == "expired":
            if default is not _UNSET:
                return default
            raise Expired(key, question_id)
        if state != "open":
            raise ProtocolError(f"daemon reported unknown state {state!r}")
        return _UNSET


def _timeout_for(wait_seconds: int) -> float:
    """Read timeout: always longer than the long-poll the daemon will hold."""
    return float(wait_seconds) + 15.0


def _infer_kind(options: Sequence[Any] | None, default: Any) -> str:
    if options:
        return "choice"
    if isinstance(default, bool):
        return "approve"
    return "text"


def _derive_id(stacklevel: int) -> str:
    """Fall back to ``file:function:line`` when no explicit ``id=`` was given.

    This is deliberately fragile in a documented way: editing the file moves the
    line number, which changes the question key and re-asks the question. Pass
    an explicit ``id=`` for anything long-lived (DESIGN.md D2).
    """
    try:
        frame = sys._getframe(stacklevel)
    except ValueError:  # pragma: no cover - shallower stack than expected
        return "anonymous"
    return f"{os.path.basename(frame.f_code.co_filename)}:{frame.f_code.co_name}:{frame.f_lineno}"


def _derive_origin(stacklevel: int) -> dict[str, Any]:
    try:
        frame = sys._getframe(stacklevel)
        site = f"{frame.f_code.co_filename}:{frame.f_lineno}"
    except ValueError:  # pragma: no cover
        site = None
    origin: dict[str, Any] = {"site": site}
    for field, variable in (("repo", "INTERJECT_REPO"), ("job", "INTERJECT_JOB"), ("run", "INTERJECT_RUN")):
        value = os.environ.get(variable)
        if value:
            origin[field] = value
    return origin


def _error_message(payload: Mapping[str, Any], status: int, doing: str) -> str:
    error = payload.get("error")
    if isinstance(error, dict) and error.get("message"):
        return "{} failed ({}): {}".format(doing, status, error["message"])
    return f"{doing} failed with status {status}"

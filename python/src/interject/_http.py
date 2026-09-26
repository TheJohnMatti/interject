"""Minimal JSON-over-HTTP transport. Standard library only, on purpose.

The client has no third-party dependencies so that adding ``interject`` to an
existing pipeline can never create a version conflict. See DESIGN.md D1.
"""
from __future__ import annotations

import json
import time
import urllib.error
import urllib.request
from typing import Any

from ._config import Config
from .errors import ProtocolError, Unauthorized, Unreachable

_RETRY_BACKOFF = (0.2, 0.6, 1.8)
_USER_AGENT = "interject-python/0.0.1"


class Transport:
    """Speaks ``docs/PROTOCOL.md`` over HTTP/1.1."""

    def __init__(self, config: Config) -> None:
        self._config = config

    @property
    def config(self) -> Config:
        return self._config

    def _headers(self) -> dict[str, str]:
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json",
            "User-Agent": _USER_AGENT,
            "X-Interject-Project": self._config.project,
        }
        if self._config.token:
            headers["Authorization"] = f"Bearer {self._config.token}"
        return headers

    def request(
        self,
        method: str,
        path: str,
        body: dict[str, Any] | None = None,
        timeout: float = 35.0,
    ) -> tuple[int, dict[str, Any]]:
        """Perform one request, retrying transport errors and 5xx responses.

        4xx responses are returned to the caller rather than raised, because
        several of them (``404`` on an unknown key, ``409`` on an already
        answered question) are ordinary control flow for this protocol.
        """
        url = f"{self._config.url}{path}"
        payload = None if body is None else json.dumps(body).encode("utf-8")
        last_error: BaseException | None = None

        for attempt in range(len(_RETRY_BACKOFF) + 1):
            request = urllib.request.Request(url, data=payload, method=method)
            for name, value in self._headers().items():
                request.add_header(name, value)
            try:
                with urllib.request.urlopen(request, timeout=timeout) as response:
                    return response.status, _decode(response.read())
            except urllib.error.HTTPError as error:
                raw = error.read()
                if error.code == 401 or error.code == 403:
                    raise Unauthorized(_message(raw, "daemon rejected the project token"))
                if error.code < 500:
                    return error.code, _decode(raw, allow_empty=True)
                last_error = error
            except (urllib.error.URLError, OSError) as error:
                last_error = error

            if attempt < len(_RETRY_BACKOFF):
                time.sleep(_RETRY_BACKOFF[attempt])

        raise Unreachable(
            f"could not reach interjectd at {self._config.url} ({last_error})"
        )


def _decode(raw: bytes, allow_empty: bool = False) -> dict[str, Any]:
    if not raw and allow_empty:
        return {}
    try:
        decoded = json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError) as error:
        raise ProtocolError(f"daemon returned a body that is not JSON: {error}")
    if not isinstance(decoded, dict):
        raise ProtocolError(f"daemon returned {type(decoded).__name__}, expected a JSON object")
    return decoded


def _message(raw: bytes, fallback: str) -> str:
    try:
        payload = json.loads(raw.decode("utf-8"))
        return str(payload["error"]["message"])
    except Exception:  # noqa: BLE001 - never let error-body parsing mask the real error
        return fallback

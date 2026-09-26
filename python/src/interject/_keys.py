"""Question identity.

Every client MUST compute keys exactly as specified in ``docs/PROTOCOL.md``,
because the key is the mechanism behind crash-replay, deduplication and
coalescing alike. See DESIGN.md D2.
"""
from __future__ import annotations

import hashlib
import json
from collections.abc import Mapping
from typing import Any

_NUL = "\x00"


def canon(context: Mapping[str, Any] | None) -> bytes:
    """Canonical JSON encoding of a question's context.

    Sorted keys, no insignificant whitespace, non-ASCII preserved, UTF-8. A
    missing context is treated as ``{}`` so that omitting it and passing an
    empty mapping produce the same key.
    """
    return json.dumps(
        {} if context is None else dict(context),
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")


def context_digest(context: Mapping[str, Any] | None) -> str:
    return hashlib.sha256(canon(context)).hexdigest()


def question_key(project: str, question_id: str, context: Mapping[str, Any] | None) -> str:
    """``sha256(project \\x00 question_id \\x00 context_digest)``, lowercase hex."""
    material = _NUL.join((project, question_id, context_digest(context)))
    return hashlib.sha256(material.encode("utf-8")).hexdigest()

"""Client configuration, read from the environment."""
from __future__ import annotations

import os

DEFAULT_URL = "http://127.0.0.1:8787"
DEFAULT_PROJECT = "default"


class Config:
    """Where the daemon is and who we are to it."""

    __slots__ = ("project", "token", "url")

    def __init__(
        self,
        url: str | None = None,
        token: str | None = None,
        project: str | None = None,
    ) -> None:
        self.url = (url or os.environ.get("INTERJECT_URL") or DEFAULT_URL).rstrip("/")
        self.token = token if token is not None else os.environ.get("INTERJECT_TOKEN")
        self.project = project or os.environ.get("INTERJECT_PROJECT") or DEFAULT_PROJECT

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return "Config(url={!r}, project={!r}, token={})".format(
            self.url, self.project, "set" if self.token else "unset"
        )

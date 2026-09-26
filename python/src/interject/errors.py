"""Exceptions raised by the interject client."""


class InterjectError(Exception):
    """Base class for every error this library raises."""


class Suspended(InterjectError):
    """A question is still open and ``on_timeout="suspend"`` was in effect.

    The caller is expected to let this propagate and exit cleanly. When the
    program runs again it recomputes the same question key and, if the question
    has been answered by then, :func:`interject.ask` returns immediately
    instead of raising.
    """

    def __init__(self, key: str, question_id: str) -> None:
        super().__init__(
            f"question {question_id!r} is still open (key {key[:12]}); exit and retry later"
        )
        self.key = key
        self.question_id = question_id


class Expired(InterjectError):
    """The question's TTL elapsed and no default was declared."""

    def __init__(self, key: str, question_id: str) -> None:
        super().__init__(
            f"question {question_id!r} expired with no default (key {key[:12]})"
        )
        self.key = key
        self.question_id = question_id


class Unauthorized(InterjectError):
    """The daemon rejected the project token."""


class ProtocolError(InterjectError):
    """The daemon returned a response this client could not understand."""


class Unreachable(InterjectError):
    """The daemon could not be reached."""

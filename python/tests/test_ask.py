"""The ask() control flow: replay, suspend, block, default, expiry."""

import pytest

from interject import (
    Client,
    Expired,
    Suspended,
    ask,
    heartbeat,
    key_for,
    peek,
    peek_answer,
    question_key,
)


def _key(daemon, question_id="q", context=None, project="test"):
    return question_key(project, question_id, context)


def test_answer_already_present_returns_immediately(daemon):
    """The replay path: a restarted program must not ask twice (DESIGN.md D2)."""
    client = Client()
    key = _key(daemon, "vehicle_type", {"title": "CBR"})
    daemon.answer(key, "motorcycle")

    result = client.ask(
        "Is this a car?",
        options=["car", "motorcycle"],
        id="vehicle_type",
        context={"title": "CBR"},
        wait=0,
    )

    assert result == "motorcycle"
    # One POST to register, and no long-poll at all.
    assert [method for method, _, _ in daemon.requests] == ["POST"]


def test_open_question_suspends_by_default(daemon):
    """D3: never block forever. The caller is expected to exit and retry."""
    with pytest.raises(Suspended) as raised:
        ask("Deploy?", kind="approve", id="deploy", wait=0)

    assert raised.value.question_id == "deploy"
    assert raised.value.key == _key(daemon, "deploy")


def test_replay_after_suspension_returns_the_answer(daemon):
    """The whole point: crash, get answered, rerun, resume."""
    with pytest.raises(Suspended) as raised:
        ask("Deploy?", kind="approve", id="deploy", context={"sha": "abc"}, wait=0)

    daemon.answer(raised.value.key, True)

    assert ask("Deploy?", kind="approve", id="deploy", context={"sha": "abc"}, wait=0) is True


def test_answer_arriving_during_the_long_poll(daemon):
    key = _key(daemon, "label")
    daemon.answer_after(key, "Honda Accord", delay=0.15)

    assert ask("Label this cluster", id="label", kind="text", wait=5) == "Honda Accord"


def test_on_timeout_default_returns_and_records_the_default(daemon):
    result = ask("Is this a car?", options=["car", "moto"], id="vt", wait=0,
                 default="skip", on_timeout="default")

    assert result == "skip"
    # Recorded server-side so a later replay is deterministic, not re-defaulted.
    recorded = daemon.answers[_key(daemon, "vt")]
    assert (recorded["value"], recorded["source"]) == ("skip", "default")


def test_on_timeout_default_requires_a_default():
    with pytest.raises(ValueError, match="requires a default"):
        ask("Whatever", kind="text", id="x", on_timeout="default", wait=0)


def test_expired_question_falls_back_to_the_default(daemon):
    key = _key(daemon, "vt")
    ask_kwargs = {"options": ["car", "moto"], "id": "vt", "wait": 0, "default": "skip"}
    with pytest.raises(Suspended):
        ask("Is this a car?", **ask_kwargs)  # registers it
    daemon.expire(key)

    assert ask("Is this a car?", **ask_kwargs) == "skip"


def test_expired_question_without_a_default_raises(daemon):
    with pytest.raises(Suspended):
        ask("Is this a car?", options=["car", "moto"], id="vt", wait=0)
    daemon.expire(_key(daemon, "vt"))

    with pytest.raises(Expired):
        ask("Is this a car?", options=["car", "moto"], id="vt", wait=0)


def test_wire_format_of_a_registration(daemon):
    with pytest.raises(Suspended):
        ask(
            "Is this a car?",
            options=["car", "moto"],
            id="vehicle_type",
            context={"title": "CBR"},
            suggest={"value": "moto", "confidence": 0.94},
            ttl="48h",
            default="skip",
            batch_key="autosniper.vt",
            wait=0,
        )

    _, path, body = daemon.requests[0]
    assert path == "/v0/questions"
    assert body["key"] == _key(daemon, "vehicle_type", {"title": "CBR"})
    assert body["kind"] == "choice"
    assert body["ttl_seconds"] == 172800
    assert body["default"] == "skip"
    assert body["on_timeout"] == "suspend"
    assert body["suggest"] == {"value": "moto", "confidence": 0.94}
    assert body["batch_key"] == "autosniper.vt"
    assert "test_ask.py:" in body["origin"]["site"]


def test_kind_is_inferred_from_options_and_default(daemon):
    for kwargs, expected in (
        ({"options": ["a", "b"]}, "choice"),
        ({"default": True}, "approve"),
        ({}, "text"),
    ):
        daemon.requests.clear()
        with pytest.raises(Suspended):
            ask("?", id=f"k-{expected}", wait=0, **kwargs)
        assert daemon.requests[0][2]["kind"] == expected


@pytest.mark.parametrize(
    "kwargs,match",
    [
        ({"on_timeout": "whenever"}, "on_timeout must be one of"),
        ({"kind": "vibes"}, "kind must be one of"),
        ({"kind": "choice"}, "requires options"),
    ],
)
def test_argument_validation(kwargs, match):
    with pytest.raises(ValueError, match=match):
        ask("?", id="x", wait=0, **kwargs)


def test_missing_id_falls_back_to_the_call_site(daemon):
    """Documented wart: the derived id moves when the file is edited."""
    with pytest.raises(Suspended):
        ask("?", kind="text", wait=0)

    assert daemon.requests[0][2]["id"].startswith("test_ask.py:test_missing_id_falls_back")


def test_heartbeat_sends_the_expected_interval(daemon):
    heartbeat("autosniper.scan", expect_every="15m")

    assert daemon.heartbeats == [{"name": "autosniper.scan", "expect_every_seconds": 900}]


def test_heartbeat_needs_exactly_one_deadline():
    with pytest.raises(ValueError, match="exactly one"):
        heartbeat("x")
    with pytest.raises(ValueError, match="exactly one"):
        heartbeat("x", expect_every="1m", expect_by="2026-01-01T00:00:00Z")


def test_peek_does_not_create_a_question(daemon):
    """The distinction that makes scanning a backlog possible at all."""
    key = _key(daemon, "vt", {"n": 1})
    assert peek(key) is None
    # Nothing was registered by looking.
    assert daemon.questions == {}

    with pytest.raises(Suspended):
        ask("?", options=["a", "b"], id="vt", context={"n": 1}, wait=0)
    assert peek(key)["state"] == "open"
    assert peek_answer(key) is None

    daemon.answer(key, "car")
    assert peek(key)["state"] == "answered"
    assert peek_answer(key) == "car"


def test_peek_answer_returns_the_given_default_when_unknown(daemon):
    assert peek_answer(_key(daemon, "nope"), default="fallback") == "fallback"


def test_key_for_matches_question_key(daemon):
    assert key_for("vt", {"n": 1}) == _key(daemon, "vt", {"n": 1})

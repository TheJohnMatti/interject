"""Routing from the Python side: claims, assignment, and inbox filters."""

import pytest

from interject import Client, ProtocolError, Suspended, ask, assign, claim, release


def _register(daemon, n=1):
    with pytest.raises(Suspended) as raised:
        ask("Is this a car?", options=["car", "moto"], id="vt", context={"n": n}, wait=0)
    return raised.value.key


def test_a_claim_names_its_holder(daemon):
    key = _register(daemon)
    held = Client(identity="ana").claim(key)

    assert held["claimed_by"] == "ana"
    assert daemon.claims[key] == "ana"


def test_a_second_person_is_refused_the_claim(daemon):
    key = _register(daemon)
    Client(identity="ana").claim(key)

    with pytest.raises(ProtocolError, match="ana"):
        Client(identity="ben").claim(key)


def test_releasing_hands_the_question_back(daemon):
    key = _register(daemon)
    Client(identity="ana").claim(key)
    assert Client(identity="ana").release(key) is True
    assert key not in daemon.claims

    # And now someone else can take it.
    assert Client(identity="ben").claim(key)["claimed_by"] == "ben"


def test_the_module_level_helpers_use_the_shared_client(daemon, monkeypatch):
    monkeypatch.setenv("INTERJECT_IDENTITY", "ana")
    import interject

    interject._default_client = None
    key = _register(daemon)

    assert claim(key)["claimed_by"] == "ana"
    assert release(key) is True
    assign(key, "ben")
    assert daemon.assignments[key] == "ben"
    assign(key, None)
    assert daemon.assignments[key] is None


def test_assign_to_travels_with_the_question(daemon):
    with pytest.raises(Suspended):
        ask("?", kind="text", id="vt", context={"n": 9}, wait=0, assign_to="ana")

    assert daemon.requests[0][2]["assign_to"] == "ana"


def test_the_identity_header_is_sent_when_configured(daemon):
    assert Client(identity="ana")._transport._headers()["X-Interject-Identity"] == "ana"
    assert "X-Interject-Identity" not in Client()._transport._headers()


def test_inbox_filters_build_the_expected_query(daemon):
    client = Client(identity="ana")
    client.inbox()
    client.inbox(limit=10, batch_key="vt", for_identity="ben", all_of_them=True)

    plain, filtered = daemon.inbox_queries
    assert plain == "limit=50"
    assert "limit=10" in filtered
    assert "batch_key=vt" in filtered
    assert "for=ben" in filtered
    # `all` lifts both filters; `include_claimed` would only lift one.
    assert "all=true" in filtered

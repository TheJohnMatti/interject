"""Transport behaviour: retries, and turning HTTP failures into useful errors."""

import pytest

from interject import Client, ProtocolError, Unauthorized, Unreachable, ask


def test_a_server_error_is_retried(daemon):
    """A 5xx is usually transient, so one must not take a pipeline down."""
    daemon.fail_next(503)
    with pytest.raises(Exception) as raised:
        ask("?", kind="text", id="retry", wait=0)

    # It got past the 503 and reached the real handler, which suspends.
    assert raised.typename == "Suspended"
    assert daemon.attempts >= 2


def test_repeated_server_errors_eventually_surface(daemon):
    for _ in range(6):
        daemon.fail_next(500)
    with pytest.raises(Unreachable):
        ask("?", kind="text", id="always-fails", wait=0)


def test_a_rejected_token_is_not_retried_as_a_transport_problem(daemon):
    daemon.fail_next(401)
    with pytest.raises(Unauthorized):
        ask("?", kind="text", id="unauthorized", wait=0)


def test_a_client_error_is_reported_not_retried(daemon):
    daemon.fail_next(400)
    with pytest.raises(ProtocolError, match="boom"):
        ask("?", kind="text", id="bad-request", wait=0)
    # 4xx means the request itself was wrong; repeating it would not help.
    assert daemon.attempts == 1


def test_a_non_json_body_is_a_protocol_error(daemon):
    daemon.fail_next(400, "<html>not json</html>")
    with pytest.raises(ProtocolError):
        ask("?", kind="text", id="html", wait=0)


def test_an_unreachable_daemon_says_so_with_its_address():
    # Port 1 is reserved and nothing will be listening on it.
    client = Client(url="http://127.0.0.1:1", project="test")
    with pytest.raises(Unreachable, match="127.0.0.1:1"):
        client.ask("?", kind="text", id="nowhere", wait=0)


def test_the_token_is_sent_as_a_bearer_header(daemon):
    client = Client(token="s3cret", project="test")
    assert client._transport._headers()["Authorization"] == "Bearer s3cret"


def test_no_token_means_no_authorization_header(daemon):
    assert "Authorization" not in Client(project="test")._transport._headers()

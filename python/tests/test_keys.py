"""Question identity is the load-bearing part of the protocol (DESIGN.md D2)."""

import hashlib
import json

import pytest

from interject import context_digest, question_key
from interject._duration import seconds


def test_key_matches_the_spec_exactly():
    project, question_id, context = "test", "vehicle_type", {"b": 2, "a": 1}
    canon = json.dumps(context, sort_keys=True, separators=(",", ":")).encode("utf-8")
    expected_context = hashlib.sha256(canon).hexdigest()
    expected_key = hashlib.sha256(
        f"{project}\x00{question_id}\x00{expected_context}".encode()
    ).hexdigest()

    assert context_digest(context) == expected_context
    assert question_key(project, question_id, context) == expected_key
    assert len(expected_key) == 64


def test_cross_language_vector():
    """Pinned so the Rust client cannot drift from this one (DESIGN.md D1/D2).

    The identical assertion lives in rust/interject/src/lib.rs. If canonical JSON
    ever diverges between the two implementations, one of these two fails.
    """
    assert question_key("test", "vehicle_type", {"price": 4200, "title": "2018 Honda CBR"}) == (
        "71fc55aa1f4d17f46aa9d5ccadd45350baa69a309deb5b17d879d20e873e4f88"
    )


def test_key_ignores_context_key_order():
    assert question_key("p", "q", {"a": 1, "b": 2}) == question_key("p", "q", {"b": 2, "a": 1})


def test_missing_context_equals_empty_context():
    assert question_key("p", "q", None) == question_key("p", "q", {})


@pytest.mark.parametrize(
    "changed",
    [
        ("other", "q", {"a": 1}),
        ("p", "other", {"a": 1}),
        ("p", "q", {"a": 2}),
    ],
)
def test_every_component_changes_the_key(changed):
    baseline = question_key("p", "q", {"a": 1})
    assert question_key(*changed) != baseline


def test_non_ascii_context_is_preserved_not_escaped():
    # ensure_ascii=False per the spec, so the digest is over the UTF-8 text.
    canon = json.dumps({"t": "Citroën"}, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    assert context_digest({"t": "Citroën"}) == hashlib.sha256(canon.encode("utf-8")).hexdigest()


@pytest.mark.parametrize(
    "value,expected",
    [(None, None), (90, 90), (2.5, 2), ("30", 30), ("15m", 900), ("48h", 172800), ("2d", 172800), ("1w", 604800)],
)
def test_duration_parsing(value, expected):
    assert seconds(value) == expected


@pytest.mark.parametrize("value", ["", "soon", "15x", "-5"])
def test_duration_rejects_nonsense(value):
    with pytest.raises(ValueError):
        seconds(value)


def test_duration_rejects_bool_as_a_type_error():
    # bool is an int subclass, so it would otherwise silently mean 1 or 0 seconds.
    with pytest.raises(TypeError):
        seconds(True)


def test_the_version_constant_matches_the_package_metadata():
    """Caught a real drift: the wheel said 0.1.0 while __version__ said 0.0.1.dev0."""
    import pathlib
    import re

    import interject

    pyproject = pathlib.Path(__file__).resolve().parents[1] / "pyproject.toml"
    declared = re.search(r'^version = "([^"]+)"', pyproject.read_text(), re.M).group(1)
    assert interject.__version__ == declared

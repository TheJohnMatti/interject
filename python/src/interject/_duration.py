"""Human-friendly durations: ``"48h"``, ``"15m"``, ``90``, ``2.5``."""
from __future__ import annotations

import re
from typing import Union

Duration = Union[str, int, float, None]

_UNITS = {"s": 1, "m": 60, "h": 3600, "d": 86400, "w": 604800}
_PATTERN = re.compile(r"^\s*(\d+(?:\.\d+)?)\s*([smhdw])?\s*$", re.IGNORECASE)


def seconds(value: Duration) -> int | None:
    """Return whole seconds, or ``None`` when ``value`` is ``None``."""
    if value is None:
        return None
    if isinstance(value, bool):  # bool is an int subclass; reject it explicitly
        raise TypeError("duration must be a number or a string like '15m', not a bool")
    if isinstance(value, (int, float)):
        if value < 0:
            raise ValueError("duration must not be negative")
        return int(value)
    match = _PATTERN.match(value)
    if not match:
        raise ValueError(
            f"could not parse duration {value!r}; use seconds or a suffix of s/m/h/d/w"
        )
    magnitude, unit = match.groups()
    return int(float(magnitude) * _UNITS[(unit or "s").lower()])

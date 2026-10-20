"""tests/conformance.py `identical`: retry-after / retry_after within one second are the same response."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import conformance  # noqa: E402


def resp(header, body, status=429):
    return {"req": "POST /auth/login", "status": status, "mw": {"retry-after": header},
            "body": {"detail": {"retry_after": body, "code": "rate_limited"}}}


def test_one_second_apart_is_identical():
    assert conformance.identical(resp("59", 59), resp("60", 60))
    assert conformance.identical(resp("60", 59), resp("59", 60))


def test_two_seconds_apart_differs():
    assert not conformance.identical(resp("59", 59), resp("61", 61))
    assert not conformance.identical(resp("59", 59), resp("60", 61))


def test_other_differences_stay():
    assert not conformance.identical(resp("59", 59), resp("59", 59, status=200))
    a, b = resp("59", 59), resp("60", 60)
    b["body"]["detail"]["code"] = "other"
    assert not conformance.identical(a, b)

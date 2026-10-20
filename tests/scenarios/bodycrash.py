"""Scenario of fixtures/bodycrash: the `Exception` handler's Request object (no receive channel)."""
from tests.scenarios.bodyapp import GZ, gz

STEPS: list = [
    ("GET", "/health", None),
    ("POST", "/boom", {"name": "x"}),
    ("POST", "/boom", gz({"name": "y"}), GZ),
    ("GET", "/boom", None),
]


def reset(db: str) -> None:
    pass

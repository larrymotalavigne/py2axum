"""Scenario of fixtures/resolveapp: `from package import name` where the package's `__init__` rebinds a submodule's
name, a class held as a class attribute called through the class held as a value, methods read on their class. No
database, no write."""

REFERENCE_APP = "fixtures.resolveapp.main:app"

STEPS: list = [("GET", "/health", None)]
STEPS += [("GET", p, None) for p in ("/rebind", "/rebind?text=A%20b%20%20C", "/class-attr", "/class-attr?lang=de",
                                     "/methods", "/methods?n=3", "/methods?n=x", "/methods/no-self")]


def reset(db: str) -> None:
    pass

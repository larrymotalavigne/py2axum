"""Submodule shadowed by its own function through `from .starred import *` in `rebind/__init__.py`."""


def starred(n: int) -> list[int]:
    return [n] * n

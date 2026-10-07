"""Data module imported lazily with importlib.import_module (never at import time of the app)."""
from pydantic import BaseModel

BIG_TABLE = {"fr": ["Paris", "Lyon"], "de": ["Berlin"]}
FACTOR = 3 * 7


class Point(BaseModel):
    x: int
    y: int = 0


DEFAULT_POINT = {"x": 4}


def scale(v: int, by: int = 2) -> int:
    return v * by * FACTOR

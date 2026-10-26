"""Module variables bound by several module-level statements: the statements run in order at startup, a name
read between two bindings sees the earlier value."""
import os

from fastapi import APIRouter

router = APIRouter(prefix="/modspan")

MODE = os.getenv("P2A_MODSPAN_UNSET")
FIRST = MODE
if not MODE:
    MODE = "default"
SUFFIX = MODE + "-x"

COUNT = 1
for _i in range(4):
    COUNT += _i

TRIES = 0
try:
    TRIES = int("not a number")
except ValueError:
    TRIES = -1

LEVELS = ["a"]
LEVELS = LEVELS + ["b"]
WIDTH: int = 3
WIDTH = WIDTH * 2


def mode() -> str:
    return MODE


@router.get("/")
async def values():
    return {"mode": MODE, "first": FIRST, "suffix": SUFFIX, "count": COUNT, "i": _i, "tries": TRIES,
            "levels": LEVELS, "width": WIDTH, "fn": mode()}

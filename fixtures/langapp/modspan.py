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


# several route decorators on one function: one registration each (the lowest first), each with its own options
@router.get("/multi/a")
@router.post("/multi/b", status_code=201)
@router.put("/multi/{name}")
async def multi(name: str = "none"):
    return {"name": name}


@router.get("/iter")
async def stored_iter():
    """`it = iter(x)` kept in a variable: next() advances it, the rest goes to a loop or list()"""
    it = iter([1, 2, 3, 4, 5])
    first = next(it)
    second = next(it, None)
    rest = []
    for x in it:
        rest.append(x)
        if x == 4:
            break
    tail = list(it)
    words = iter("ab")
    letters = [next(words), next(words), next(words, "end")]
    pairs = iter({"k": 1, "j": 2})
    try:
        next(iter([]))
        empty = "no error"
    except StopIteration:
        empty = "StopIteration"
    return {"first": first, "second": second, "rest": rest, "tail": tail, "letters": letters,
            "keys": [next(pairs), next(pairs)], "same": iter(it) is it, "empty": empty}


@router.get("/ddlambda")
async def dd_lambda():
    """defaultdict(lambda: <literal>): each missing key gets a fresh copy of the literal"""
    from collections import defaultdict

    stats = defaultdict(lambda: {"n": 0, "tags": []})
    for k in ("a", "b", "a"):
        stats[k]["n"] += 1
        stats[k]["tags"].append(k)
    nested = defaultdict(lambda: defaultdict(int))
    nested["x"]["y"] += 2
    nested["x"]["z"] += 1
    zero = defaultdict(lambda: 0)
    zero["q"] += 5
    c = stats.copy()
    c["new"]["n"] = 9
    return {"stats": stats, "shared": stats["a"]["tags"] is stats["b"]["tags"], "nested": nested, "zero": zero,
            "copy_type": type(c).__name__, "copy_new": c["new"], "orig_has_new": "new" in stats}

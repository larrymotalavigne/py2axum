"""collections.defaultdict and string.Formatter."""
import string
from collections import defaultdict

from fastapi import APIRouter

router = APIRouter(prefix="/colls")


@router.post("/count")
async def count(words: list[str]):
    by_len: dict[int, int] = defaultdict(int)
    groups = defaultdict(list)
    first = defaultdict(str, {"seed": "s"})
    for w in words:
        by_len[len(w)] += 1
        groups[w[:1]].append(w)
        first[w[:1]] += w[-1:]
    probe = by_len[99]  # a read of a missing key inserts it
    return {"by_len": by_len, "groups": groups, "first": first, "probe": probe, "has99": 99 in by_len,
            "get": by_len.get(1000), "plain": repr(dict(by_len)), "repr": repr(defaultdict(int, a=1)),
            "nested": repr(defaultdict(list))}


@router.post("/render")
async def render(body: dict):
    tpl = body["template"]
    values = body.get("values", {})
    try:
        out = string.Formatter().vformat(tpl, (), defaultdict(str, values))
    except (ValueError, IndexError, KeyError) as e:
        return {"error": type(e).__name__, "msg": str(e)}
    return {"out": out, "fmt": string.Formatter().format("{0}-{x}", 1, x=2)}


@router.post("/format")
async def fmt(body: dict):
    try:
        return {"out": body["template"].format(*body.get("args", []), **body.get("kw", {}))}
    except (ValueError, IndexError, KeyError) as e:
        return {"error": type(e).__name__, "msg": str(e)}


@router.post("/percent")
async def percent(body: dict):
    try:
        return {"out": body["fmt"] % tuple(body["args"])}
    except (ValueError, TypeError) as e:
        return {"error": type(e).__name__}


# a module-level endpoint shadowed by a later one of the same name: both routes stay registered
@router.get("/dup/{x}")
async def dup(x: str):
    return {"first": x}


@router.get("/dup/two/{x}")
async def dup(x: str, n: int = 1):
    return {"second": x, "n": n}

"""copy.copy / copy.deepcopy: what is shared, what is rebuilt, cycles and shared references, models and classes."""
import copy
from collections import Counter, defaultdict, deque
from copy import deepcopy
from dataclasses import dataclass, field
from datetime import date

from fastapi import APIRouter
from pydantic import BaseModel

router = APIRouter(prefix="/cp")


class Tag(BaseModel):
    name: str
    meta: dict[str, list[int]] = {}


class Doc(BaseModel, extra="allow"):
    id: int
    tags: list[Tag] = []
    opt: str | None = None


@dataclass
class Box:
    items: list[int] = field(default_factory=list)
    label: str = "box"


class Plain:
    def __init__(self, xs):
        self.xs = xs
        self.n = len(xs)


@router.get("/containers")
async def containers():
    inner = [1, 2]
    src = {"a": inner, "b": inner, "t": (inner, 3), "ft": (1, "x"), "s": {1, 2}, "d": date(2026, 1, 2)}
    sh, dp = copy.copy(src), deepcopy(src)
    sh["a"].append(3)
    out = {
        "shallow_shares": sh["b"] is src["b"], "deep_rebuilds": dp["a"] is not src["a"],
        "deep_keeps_sharing": dp["a"] is dp["b"] and dp["t"][0] is dp["a"],
        "atomic_tuple_same": dp["ft"] is src["ft"], "tuple_with_list_new": dp["t"] is not src["t"],
        "shallow_tuple_same": copy.copy(src["t"]) is src["t"],
        "deep_values": dp, "src_after": src, "set_new": dp["s"] is not src["s"] and dp["s"] == src["s"],
    }
    cyc = [1]
    cyc.append(cyc)
    c2 = deepcopy(cyc)
    out["cycle"] = c2[1] is c2 and c2 is not cyc
    return out


@router.get("/collections")
async def collections():
    dd = defaultdict(list, {"a": [1]})
    c = Counter("abca")
    dq = deque([[1], [2]], maxlen=3)
    d2, c2, q2 = deepcopy(dd), copy.copy(c), deepcopy(dq)
    d2["new"].append(1)
    q2[0].append(9)
    return {"dd": type(d2).__name__, "dd_missing": d2["new"], "dd_src": dict(dd), "counter": type(c2).__name__,
            "counter_missing": c2["zz"], "deque": list(q2), "deque_src": list(dq), "maxlen": q2.maxlen,
            "fset": deepcopy(frozenset({1, 2})) == frozenset({1, 2})}


@router.get("/models")
async def models():
    d = Doc(id=1, tags=[Tag(name="a", meta={"k": [1]})], extra_field=[1, 2])
    s, dp = copy.copy(d), copy.deepcopy(d)
    s.tags.append(Tag(name="b"))
    dp.tags[0].meta["k"].append(2)
    dp.extra_field.append(3)
    return {"src": d.model_dump(), "shallow": s.model_dump(), "deep": dp.model_dump(),
            "fields_set": sorted(dp.model_fields_set), "deep_tag_new": dp.tags[0] is not d.tags[0],
            "shallow_tag_same": s.tags is d.tags, "eq": copy.deepcopy(d) == d}


@router.get("/classes")
async def classes():
    b = Box([1, 2])
    p = Plain([[1], [2]])
    b2, p2, p3 = copy.copy(b), deepcopy(p), copy.copy(p)
    b2.items.append(3)
    p2.xs[0].append(5)
    p3.n = 99
    return {"box": [b.items, b2.items, b2.label, b2 == b], "plain_deep": [p.xs, p2.xs, p2.n],
            "plain_shallow": [p3.xs is p.xs, p.n, p3.n], "types": [type(b2).__name__, type(p2).__name__]}

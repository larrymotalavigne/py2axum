"""typing.NamedTuple classes: built like a dataclass, a tuple everywhere else (indexing, unpacking, equality and
hash with tuples, concatenation, JSON as an array), `_asdict()`, `_replace()`, `_make()`, `_fields`, read-only."""
import json
from typing import NamedTuple

from fastapi import APIRouter

router = APIRouter(prefix="/nt")


class Match(NamedTuple):
    """a resolved suggestion"""
    address: str
    share_id: int = 0
    tags: tuple = ("a",)

    def label(self) -> str:
        return f"{self.address}#{self.share_id}"

    @property
    def upper(self) -> str:
        return self.address.upper()


class Pair(NamedTuple):
    a: int
    b: int


def _t(f):
    try:
        return repr(f())
    except Exception as e:  # noqa: BLE001
        return f"{type(e).__name__}: {e}"


@router.get("/basics")
async def basics():
    m = Match("rue", 3)
    a, s, t = m
    p = Pair(1, 2)
    return {
        "repr": repr(m), "str": str(m), "unpack": [a, s, list(t)], "index": [m[0], m[-1], list(m[0:2])],
        "len": len(m), "eq": [m == ("rue", 3, ("a",)), p == (1, 2), p == Pair(1, 2), p != (2, 1), p < (1, 3)],
        "hash": hash(p) == hash((1, 2)), "in": ["rue" in m, 9 in p], "concat": repr(p + (3,)), "mul": repr(p * 2),
        "isinstance": [isinstance(m, tuple), isinstance(m, Match), isinstance(p, Match)], "type": type(m).__name__,
        "methods": [m.label(), m.upper, m.count("rue"), p.index(2)], "asdict": m._asdict(),
        "replace": repr(m._replace(share_id=9)), "make": repr(Pair._make([5, 6])), "fields": [Match._fields, m._fields],
        "defaults": repr(Match._field_defaults), "sorted": sorted([Pair(2, 1), Pair(1, 5)]), "dict": {p: "x"}[(1, 2)],
        "json": json.dumps(m), "set": len({p, (1, 2)}), "bool": [bool(Pair(0, 0))], "list": list(p), "tuple": repr(tuple(p)),
        "kw": repr(Pair(b=2, a=1)),
    }


@router.get("/errors")
async def errors():
    m = Match("rue")
    p = Pair(1, 2)
    return [
        _t(lambda: Pair(1)), _t(lambda: Pair(1, 2, 3)), _t(lambda: Pair(1, c=3)), _t(lambda: Pair(1, a=2)),
        _t(lambda: setattr(m, "address", "x")), _t(lambda: setattr(m, "nope", 1)), _t(lambda: m._replace(zz=1)),
        _t(lambda: Pair._make([1])), _t(lambda: p[5]), _t(lambda: m.model_dump()), _t(lambda: m.nope),
        _t(lambda: p.index(7)),
    ]


@router.get("/response")
async def response():
    """returned as is: an array (FastAPI's jsonable_encoder), nested too"""
    return {"one": Pair(1, 2), "many": [Match("a"), Match("b", 2, ())]}

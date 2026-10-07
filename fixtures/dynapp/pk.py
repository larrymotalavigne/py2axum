"""pickle in CPython's format: values written here are read by CPython and the other way round."""
import base64
import pickle
from dataclasses import dataclass
from datetime import UTC, date, datetime, time, timedelta, timezone
from decimal import Decimal
from enum import Enum
from uuid import UUID

from fastapi import APIRouter
from pydantic import BaseModel

router = APIRouter(prefix="/pk")


class Entry:
    __slots__ = ("created_at", "hard_expiry", "soft_expiry", "value")

    def __init__(self, value, created_at, soft_expiry, hard_expiry):
        self.value = value
        self.created_at = created_at
        self.soft_expiry = soft_expiry
        self.hard_expiry = hard_expiry


class Bag:
    def __init__(self, **kw):
        for k, v in kw.items():
            setattr(self, k, v)


class Color(Enum):
    RED = "red"
    BLUE = "blue"


class Item(BaseModel):
    id: int
    name: str = "x"
    tags: list[str] = []


@dataclass
class Point:
    x: int
    y: int = 0


def sample():
    shared = [1, 2]
    return {
        "scalars": [None, True, False, 0, 255, 65535, -1, 2**31, -(2**40), 2**62, 1.5, -0.0, "é" * 3, "x" * 300, b"\x00\xff"],
        "dates": [datetime(2026, 1, 2, 3, 4, 5, 6), datetime(2026, 1, 2, 3, 4, 5, tzinfo=UTC),
                  datetime(2026, 6, 1, 12, 0, tzinfo=timezone(timedelta(hours=2))), date(2026, 2, 3), time(4, 5, 6, 7),
                  timedelta(days=-1, seconds=5, microseconds=3)],
        "misc": [Decimal("1.50"), Decimal("-3"), UUID(int=5), UUID("12345678-1234-5678-1234-567812345678"), Color.BLUE, (1, (2, 3)), {4}],
        "shared": [shared, shared],
        "objects": [Item(id=1, tags=["a"]), Point(3), Bag(a=1, b=[2])],
    }


def _b64(b: bytes) -> str:
    return base64.b64encode(b).decode()


@router.get("/dump")
async def dump():
    e = Entry(sample(), 1.5, 2.0, 3.0)
    return {"slots": _b64(pickle.dumps(e)), "proto4": _b64(pickle.dumps([1, "a"], protocol=4))}


@router.post("/load")
async def load(body: dict):
    e = pickle.loads(base64.b64decode(body["b64"]))
    v = e.value
    return {"type": type(e).__name__, "times": [e.created_at, e.soft_expiry, e.hard_expiry],
            "scalars": [repr(x) for x in v["scalars"]], "dates": [repr(x) for x in v["dates"]],
            "misc": [repr(x) for x in v["misc"]], "shared": v["shared"][0] is v["shared"][1],
            "objects": [repr(o) if not isinstance(o, Bag) else sorted(vars(o).items()) for o in v["objects"]],
            "again": _b64(pickle.dumps(e)) == _b64(pickle.dumps(pickle.loads(pickle.dumps(e))))}


@router.get("/errors")
async def errors():
    out = []
    for f in (lambda: pickle.loads(b"\x80\x05garbage"), lambda: pickle.loads(b""), lambda: pickle.loads("str")):
        try:
            f()
        except Exception as e:  # noqa: BLE001
            out.append(type(e).__name__)
    return out

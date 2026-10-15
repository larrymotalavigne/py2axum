"""orjson: `loads` (its decoder's messages and positions), `dumps` with every option and `default=`, and
FastAPI's `ORJSONResponse` (returned, and as `response_class=`)."""
import base64
import datetime as dt
import enum
import json
import uuid
import zoneinfo
from dataclasses import dataclass, field
from decimal import Decimal

import orjson
from fastapi import APIRouter
from fastapi.responses import ORJSONResponse
from pydantic import BaseModel

router = APIRouter(prefix="/orj")


class Color(enum.Enum):
    RED = "red"
    PAIR = (1, 2)


class Size(str, enum.Enum):
    S = "s"


class Level(enum.IntEnum):
    HIGH = 3


@dataclass
class Point:
    x: int
    y: float
    _hidden: str = "h"
    tags: list = field(default_factory=list)


@dataclass
class Empty:
    pass


class Item(BaseModel):
    name: str
    price: float


@router.post("/loads")
async def loads(body: dict):
    """the document as text (`doc`), as bytes (`b64`), or another input type (`kind`)"""
    if "b64" in body:
        doc = base64.b64decode(body["b64"])
    elif body.get("kind") == "int":
        doc = 1
    elif body.get("kind") == "none":
        doc = None
    elif body.get("as_bytes"):
        doc = body["doc"].encode()
    else:
        doc = body["doc"]
    try:
        v = orjson.loads(doc)
    except orjson.JSONDecodeError as e:
        return {"err": type(e).__name__, "msg": str(e), "pos": e.pos, "lineno": e.lineno, "colno": e.colno,
                "emsg": e.msg, "json": isinstance(e, json.JSONDecodeError), "value": isinstance(e, ValueError)}
    return {"repr": repr(v), "type": type(v).__name__}


@router.post("/dumps")
async def dumps(body: dict):
    """a JSON value read from the request, serialized with `opt`"""
    try:
        out = orjson.dumps(body.get("value"), option=body.get("opt"))
    except orjson.JSONEncodeError as e:
        return {"err": type(e).__name__, "msg": str(e)}
    return {"out": out.decode(), "type": type(out).__name__}


def _catalog():
    paris = zoneinfo.ZoneInfo("Europe/Paris")
    return {
        "datetimes": [dt.datetime(2020, 1, 2, 3, 4, 5), dt.datetime(2020, 1, 2, 3, 4, 5, 12345),
                      dt.datetime(2020, 1, 2, 3, 4, 5, 123456, tzinfo=dt.timezone.utc),
                      dt.datetime(1, 1, 1, tzinfo=dt.timezone(dt.timedelta(hours=-3, seconds=-31))),
                      dt.datetime(999, 12, 31, 23, 59, 59, 999999, tzinfo=dt.timezone(dt.timedelta(minutes=330))),
                      dt.datetime(2021, 7, 1, 12, tzinfo=paris), dt.datetime(2021, 1, 1, 12, tzinfo=paris)],
        "dates": [dt.date(2020, 1, 2), dt.date(5, 6, 7), dt.date(9999, 12, 31)],
        "times": [dt.time(0, 0), dt.time(1, 2, 3, 4), dt.time(1, 2, 3, 45), dt.time(1, 2, 3, 456), dt.time(1, 2, 3, 4567),
                  dt.time(1, 2, 3, 45678), dt.time(23, 59, 59, 999999)],
        "uuid": [uuid.UUID("12345678-1234-5678-1234-567812345678"), uuid.UUID(int=0)],
        "enums": [Color.RED, Color.PAIR, Size.S, Level.HIGH],
        "dataclass": [Point(1, 2.5), Point(-1, 0.1, "x", [Point(2, 3)]), Empty()],
        "tuple": (1, (2, ()), [3, ()]),
        "floats": [0.1, 1e16, 1e-7, 1.5e300, -0.0, 5e-324, 123456789.123, float("nan"), float("inf"), -float("inf"), 1e22, 0.000123],
        "ints": [0, -1, 2**53, -(2**53), 2**53 + 1, 9223372036854775807, -9223372036854775807 - 1],
        "strings": ["", "é", "\x00\x1f\x7f", '"\\/', "  ", "😀", "tab\there\nnl\rcr\x08\x0c"],
        "keys": {"b": 1, "a": 2, "é": 3, "B": 4, "": 5, "aa": 6},
        "nonstr": {1: "i", None: "n", True: "t", 1.5: "f", dt.date(2020, 1, 1): "d", "s": "s",  # noqa: F601 (1 and True: one key, the first object kept)
                   uuid.UUID(int=1): "u", Color.RED: "e", Level.HIGH: "l", dt.datetime(2020, 1, 1): "dt",
                   dt.time(1, 2): "tm", -0.0: "z", float("nan"): "nan"},
        "dup": {1: "a", "1": "b"},
        "nested": {"a": [1, {"b": [], "c": {}}, [[]]], "d": {"e": None}},
        "empty": [[], {}, (), ""],
    }


def _unsupported():
    return {"decimal": Decimal("1.5"), "bytes": b"x", "set": {1}, "delta": dt.timedelta(1),
            "tz": dt.timezone.utc, "zone": zoneinfo.ZoneInfo("UTC"), "model": Item(name="a", price=1),
            "type": int, "exc": ValueError("x")}


def _default(o):
    if isinstance(o, Decimal):
        return str(o)
    if isinstance(o, BaseModel):
        return o.model_dump()
    if isinstance(o, (set, frozenset)):
        return sorted(o)
    if isinstance(o, dt.timedelta):
        return o.total_seconds()
    raise TypeError(f"no default for {type(o).__name__}")


def _forever(o):
    return Decimal(1)


@router.get("/catalog/{name}")
async def catalog(name: str, opt: int = 0):
    try:
        out = orjson.dumps(_catalog()[name], option=opt)
    except orjson.JSONEncodeError as e:
        return {"err": type(e).__name__, "msg": str(e)}
    return {"out": out.decode()}


@router.get("/unsupported/{name}")
async def unsupported(name: str, default: str = "none"):
    fn = {"none": None, "fn": _default, "str": str, "forever": _forever}[default]
    try:
        out = orjson.dumps(_unsupported()[name], default=fn)
    except TypeError as e:
        cause = e.__cause__
        return {"err": type(e).__name__, "msg": str(e), "cause": None if cause is None else [type(cause).__name__, str(cause)]}
    return {"out": out.decode()}


@router.get("/limits/{what}")
async def limits(what: str, n: int):
    """containers nested `n` deep (orjson stops at 254), tuples, dataclasses"""
    v: object = 1
    for _ in range(n):
        v = {"k": v} if what == "dict" else (v,) if what == "tuple" else [v]
    try:
        return {"len": len(orjson.dumps(v))}
    except orjson.JSONEncodeError as e:
        return {"err": str(e)}


@router.get("/errors/{what}")
async def errors(what: str):
    try:
        if what == "strict":
            orjson.dumps([2**53], option=orjson.OPT_STRICT_INTEGER)
        elif what == "strict_ok":
            return {"out": orjson.dumps([2**53 - 1, -(2**53 - 1)], option=orjson.OPT_STRICT_INTEGER).decode()}
        elif what == "badopt":
            orjson.dumps(1, option=1 << 20)
        elif what == "badopt_type":
            orjson.dumps(1, option="x")
        elif what == "key":
            orjson.dumps({(1, 2): 1}, option=orjson.OPT_NON_STR_KEYS)
        elif what == "key_str":
            orjson.dumps({1: 2})
        elif what == "key_sorted":
            orjson.dumps({"a": 1, 2: 3}, option=orjson.OPT_SORT_KEYS)
        elif what == "passthrough_dt":
            orjson.dumps(dt.date(2020, 1, 1), option=orjson.OPT_PASSTHROUGH_DATETIME)
        elif what == "passthrough_dt_default":
            return {"out": orjson.dumps(dt.date(2020, 1, 1), option=orjson.OPT_PASSTHROUGH_DATETIME, default=str).decode()}
        elif what == "passthrough_dc":
            orjson.dumps(Point(1, 2), option=orjson.OPT_PASSTHROUGH_DATACLASS)
        elif what == "positional":
            return {"out": orjson.dumps(Decimal("2.5"), str, orjson.OPT_INDENT_2).decode()}
    except orjson.JSONEncodeError as e:
        return {"err": type(e).__name__, "msg": str(e)}
    return {"ok": True}


@router.get("/roundtrip")
async def roundtrip():
    doc = orjson.dumps(_catalog()["nested"], option=orjson.OPT_SORT_KEYS | orjson.OPT_INDENT_2)
    return {"same": orjson.loads(doc) == _catalog()["nested"], "bytes": isinstance(doc, bytes)}


@router.get("/response")
async def response(kind: str = "plain"):
    """ORJSONResponse returned by the endpoint (FastAPI renders it with OPT_NON_STR_KEYS)"""
    if kind == "nonstr":
        return ORJSONResponse(content={1: "a", None: [1.5, 1e-7]}, status_code=201)
    if kind == "bad":
        return ORJSONResponse(content={"d": Decimal(1)})
    return ORJSONResponse(content={"b": [1, 2.0, "é"], "a": None}, headers={"x-o": "1"})


@router.get("/response_class", response_class=ORJSONResponse)
async def response_class():
    return {"floats": [1e-7, 1e16, 0.1], "when": dt.datetime(2020, 1, 1, 12), "text": "é "}


@router.get("/response_model", response_class=ORJSONResponse, response_model=Item)
async def response_model():
    return {"name": "x", "price": 1e-7, "extra": 1}

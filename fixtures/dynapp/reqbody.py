"""The raw request body: `await request.body()` / `request.json()`, read twice, after a body parameter or a
dependency, empty or large, and `model_validate_json` / `TypeAdapter.validate_json` / `Json[T]` on it (pydantic's
JSON mode: its own type errors and exactness).

/reqbody/track is a web app's analytics ingest as it was until 16/10/2026 (a 204 whatever the body,
`navigator.sendBeacon` sends text/plain): `model_validate_json(await request.body())` got `str(bytes)` in the
binary, every event was dropped."""
import datetime as dt
import json
import uuid
from dataclasses import dataclass
from decimal import Decimal

from fastapi import APIRouter, Depends, Request
from pydantic import BaseModel, Field, Json, TypeAdapter, ValidationError

from .db import DbDep

router = APIRouter(prefix="/reqbody")

TRACKED: list = []


class TrackRequest(BaseModel):
    event: str = Field(..., max_length=64)
    properties: dict | None = None
    session_id: str | None = Field(None, max_length=64)


@router.post("/track", status_code=204)
async def track(request: Request, db: DbDep):
    try:
        body = TrackRequest.model_validate_json(await request.body())
    except Exception:
        return
    TRACKED.append(body.model_dump())


@router.get("/tracked")
async def tracked():
    out = list(TRACKED)
    TRACKED.clear()
    return out


@router.post("/raw")
async def raw(request: Request):
    b = await request.body()
    again = await request.body()
    return {"len": len(b), "type": type(b).__name__, "head": repr(b[:40]), "same": again == b}


@router.post("/json")
async def as_json(request: Request):
    try:
        v = await request.json()
    except Exception as e:
        return {"error": type(e).__name__, "msg": str(e)}
    return {"value": v, "again": (await request.json()) == v}


class Item(BaseModel):
    name: str
    n: int = 0


@router.post("/after-param")
async def after_param(item: Item, request: Request):
    b = await request.body()
    return {"item": item.model_dump(), "len": len(b), "json": await request.json()}


async def body_dep(request: Request) -> bytes:
    return await request.body()


@router.post("/after-dep")
async def after_dep(request: Request, first: bytes = Depends(body_dep)):
    second = await request.body()
    return {"len": len(first), "same": first == second}


class Inner(BaseModel):
    x: int


@dataclass
class Point:
    y: int


class Rich(BaseModel):
    ints: list[int] | None = None
    pair: tuple[int, str] | None = None
    tags: set[int] | None = None
    frozen: frozenset[int] | None = None
    mapping: dict[str, int] | None = None
    inner: Inner | None = None
    inners: list[Inner] | None = None
    point: Point | None = None
    delay: dt.timedelta | None = None
    amount: Decimal | None = None
    day: dt.date | str | None = None
    when: dt.datetime | str | None = None
    raw: bytes | str | None = None
    ident: uuid.UUID | str | None = None
    price: Decimal | str | None = None
    flag: bool | None = None


class WithJson(BaseModel):
    j: Json[list[int]]
    k: Json | None = None


def _errors(e: ValidationError) -> dict:
    return {"errors": e.errors(include_url=False), "text": str(e), "count": e.error_count()}


@router.post("/mvj/{kind}")
async def mvj(kind: str, request: Request):
    b = await request.body()
    try:
        if kind == "bytes":
            m = Rich.model_validate_json(b)
        elif kind == "str":
            m = Rich.model_validate_json(b.decode())
        elif kind == "track":
            m = TrackRequest.model_validate_json(b)
        elif kind == "adapter":
            try:
                return {"value": TypeAdapter(list[int]).validate_json(b)}
            except ValidationError as e:
                # (no str(e): the runtime writes a TypeAdapter's error without its title)
                return {"errors": e.errors(include_url=False), "count": e.error_count()}
        elif kind == "python":
            m = Rich.model_validate(json.loads(b))
        elif kind == "jsonmode":
            m = WithJson.model_validate_json(b)
        elif kind == "jsonfield":
            m = WithJson.model_validate({"j": b.decode()})
        else:
            m = Rich.model_validate_json(None)
    except ValidationError as e:
        return _errors(e)
    if isinstance(m, Rich):
        # (m.frozen left out: the runtime holds a frozenset as a set, type() says "set")
        kinds = [type(v).__name__ for v in (m.pair, m.tags, m.inner, m.point, m.delay, m.amount, m.day,
                                            m.when, m.raw, m.ident, m.price)]
        return {"dump": m.model_dump(mode="json"), "types": kinds, "amount": repr(m.amount)}
    return {"dump": m.model_dump(mode="json")}


@router.post("/mvj-none")
async def mvj_none():
    try:
        Rich.model_validate_json(5)
    except ValidationError as e:
        return _errors(e)
    return {}

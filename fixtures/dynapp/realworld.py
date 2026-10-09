"""Constructs met in real applications: a field
dumped under another key than the one it reads (`serialization_alias=`, `validation_alias=`), `nonlocal`,
TypedDict, `Model.__table__` introspection, StaleDataError on a row deleted behind the session."""
import asyncio
import datetime as dt
import html
import os
from pathlib import Path
import html as html_lib
from typing import Any, TypedDict

from fastapi import APIRouter, Response
from icalendar import Alarm, Calendar, Event
from pydantic import BaseModel, ConfigDict, Field, ValidationError
from sqlalchemy import JSON, Integer, String, delete, false, func, select
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy.orm.exc import StaleDataError

from .db import DbDep
from .models import Notice

router = APIRouter(prefix="/realworld")


class NoticeRead(BaseModel):
    """Read from the ORM object itself: the attribute read is the validation key."""
    model_config = ConfigDict(from_attributes=True)

    id: int
    extra_data: dict | None = Field(default=None, serialization_alias="metadata")
    label: str


class NoticeOut(BaseModel):
    """The ORM attribute is `extra_data`, the API key `metadata` (`.metadata` is SQLAlchemy's MetaData)."""
    model_config = ConfigDict(from_attributes=True)

    id: int
    extra_data: dict | None = Field(default=None, serialization_alias="metadata")
    label: str = Field(alias="lbl", serialization_alias="tag")


class Renamed(BaseModel):
    user_name: str = Field(validation_alias="userName")
    city: str = Field("Paris", alias="c", validation_alias="town")
    zip_code: str | None = Field(None, serialization_alias="zip")


class Populated(BaseModel):
    model_config = ConfigDict(populate_by_name=True)

    full_name: str = Field(alias="fullName", serialization_alias="name")


@router.post("/notices")
async def add_notices(db: DbDep):
    db.add(Notice(extra_data={"k": [1, 2]}, label="first"))
    db.add(Notice(extra_data=None))
    await db.flush()
    return {"ok": True}


@router.get("/notices", response_model=list[NoticeOut])
async def list_notices(db: DbDep):
    rows = (await db.execute(select(Notice).order_by(Notice.id))).scalars().all()
    return [NoticeOut.model_validate({"id": n.id, "extra_data": n.extra_data, "lbl": n.label}) for n in rows]


@router.get("/notices/read", response_model=list[NoticeRead])
async def read_notices(db: DbDep):
    return (await db.execute(select(Notice).order_by(Notice.id))).scalars().all()


@router.get("/notices/orm")
async def orm_notices(db: DbDep):
    rows = (await db.execute(select(Notice).order_by(Notice.id))).scalars().all()
    out = [NoticeOut(id=n.id, extra_data=n.extra_data, lbl=n.label) for n in rows]
    return {"dump": [o.model_dump() for o in out], "by_alias": [o.model_dump(by_alias=True) for o in out],
            "json": [o.model_dump_json(by_alias=True) for o in out],
            "include": [o.model_dump(by_alias=True, include={"extra_data"}) for o in out]}


@router.get("/notices/first", response_model=NoticeOut, response_model_exclude={"label"})
async def first_notice(db: DbDep):
    n = (await db.execute(select(Notice).order_by(Notice.id))).scalars().first()
    return {"id": n.id, "extra_data": n.extra_data, "lbl": n.label}


@router.post("/renamed", response_model=Renamed)
async def renamed(body: Renamed):
    return body


@router.post("/renamed/dump")
async def renamed_dump(body: dict):
    try:
        r = Renamed(**body)
    except ValidationError as e:
        return {"errors": e.errors(include_url=False)}
    return {"dump": r.model_dump(), "by_alias": r.model_dump(by_alias=True), "attrs": [r.user_name, r.city, r.zip_code]}


@router.post("/populated")
async def populated(body: Populated):
    return {"body": body, "dump": body.model_dump(), "by_alias": body.model_dump(by_alias=True)}


async def echo(v):
    return v


# ---- nonlocal: a nested function rebinding a variable of the enclosing one
def split_codes(raw: list) -> dict:
    all_codes: list[str] = []
    primary: str | None = None
    count = 0

    def add(code: Any, *, is_primary: bool = False) -> None:
        nonlocal primary, count
        c = str(code).strip()
        if not c:
            return
        count += 1
        if c not in all_codes:
            all_codes.append(c)
        if is_primary and primary is None:
            primary = c

    for i, code in enumerate(raw):
        add(code, is_primary=i == 1)
    seen = primary  # read after the nested calls: the rebound value

    def outer():
        total = 0

        def inner(n):
            nonlocal total
            total += n
            return total

        inner(2)
        inner(3)
        return total, (lambda: total)()

    return {"codes": all_codes, "primary": primary, "seen": seen, "count": count, "outer": outer()}


def counter():
    n = 0

    def bump():
        nonlocal n
        n += 1
        return n

    def peek():
        return n

    return bump, peek


SHADOWED = "global"


def shadowing():
    def f():
        SHADOWED = "local of f"  # noqa: F841
        return SHADOWED

    return [f(), SHADOWED]


def unbound_free():
    def read():
        return later

    try:
        out = read()
    except NameError as e:
        out = str(e)
    later = 1
    return [out, read()]


@router.post("/nonlocal")
async def nonlocal_route(body: dict):
    body = body["codes"]
    bump, peek = counter()
    bump()
    bump()
    late = [lambda: i * 10 for i in range(3)]
    fns = []
    for k in range(3):
        fns.append(lambda: k)
    x = 1
    get_x = lambda: x  # noqa: E731
    x = 2

    async def waited():
        return seen_list

    seen_list = ["late"]
    waited_out = await asyncio.wait_for(waited(), timeout=5)
    async def gen_wait():
        return await asyncio.wait_for(echo(seen_list), timeout=5)

    def fact(n):
        return 1 if n <= 1 else n * fact(n - 1)

    return {"split": split_codes(body), "counter": [bump(), peek()], "unbound": unbound_free(),
            "late": [f() for f in late], "loop": [f() for f in fns], "rebound": get_x(), "fact": fact(5),
            "shadow": shadowing(), "waited": [waited_out, await gen_wait()]}


# ---- TypedDict: calling the class builds a plain dict
class Qualification(TypedDict):
    """Result of a check."""

    qualified: bool
    reasons: list[str]


class Scored(Qualification, total=False):
    score: float


def qualify(amount: float) -> Qualification:
    if amount < 0:
        return Qualification(qualified=False, reasons=["negative"])
    return Scored({"reasons": []}, qualified=True, score=amount * 2)


@router.get("/typeddict")
async def typeddict(amount: float):
    q = qualify(amount)
    q["extra"] = type(q).__name__
    return {"q": q, "partial": Qualification(qualified=True), "keys": list(Scored(score=1.0, qualified=False))}


# ---- Model.__table__: the column collection, a column's type
@router.patch("/notices/{nid}/fields")
async def patch_fields(nid: int, body: dict, db: DbDep):
    n = await db.get(Notice, nid)
    if n is None:
        return {"found": False}
    columns = Notice.__table__.columns
    kinds = {}
    for field, value in body.items():
        if field in columns and isinstance(columns[field].type, JSON):
            kinds[field] = "json"
        elif field in columns:
            kinds[field] = str(columns[field].type)
        else:
            kinds[field] = None
    try:
        columns["nope"]
        missing = None
    except KeyError as e:
        missing = repr(e)
    t = Notice.__table__
    return {"kinds": kinds, "table": t.name, "keys": list(columns.keys()), "len": len(t.c),
            "cols": [[c.name, c.key, c.nullable, c.primary_key, str(c.type), repr(c.type),
                      isinstance(c.type, (String, Integer)), isinstance(c.type, JSONB), isinstance(c.type, JSON)]
                     for c in t.columns],
            "str": [str(columns[0]), str(t)], "get": [columns.get("nope") is None, columns.get("lbl").name],
            "missing": missing}


# ---- StaleDataError: the row of a loaded object deleted by another statement before the flush
@router.post("/notices/{nid}/stale")
async def stale(nid: int, db: DbDep):
    n = await db.get(Notice, nid)
    if n is None:
        return {"found": False}
    await db.execute(delete(Notice).where(Notice.id == nid))
    n.label = "changed"
    try:
        await db.flush()
    except StaleDataError as exc:
        await db.rollback()
        return {"stale": str(exc), "type": type(exc).__name__}
    return {"stale": None}


# ---- html.unescape: CPython's tables and its longest-prefix rule
@router.post("/unescape")
async def unescape(body: dict):
    out = []
    for s in body["texts"]:
        try:
            out.append(html.unescape(s))
        except (TypeError, ValueError) as e:
            out.append([type(e).__name__, str(e)])
    return {"out": out, "lib": html_lib.unescape(html.escape("<a href='x'>&</a>"))}


# ---- elements of a JSON column in SQL: col[key].as_boolean() / as_string() / ...
@router.post("/notices/json")
async def json_elements(db: DbDep):
    db.add_all([Notice(extra_data={"flag": True, "n": "7", "f": 1.5, "s": "x", "a": {"b": "deep"}, "l": [10, 20]}),
                Notice(extra_data={"flag": False, "n": 2}), Notice(extra_data={"other": 1}), Notice(extra_data=None)])
    await db.flush()
    flagged = func.coalesce(Notice.extra_data["flag"].as_boolean(), false())
    on = (await db.execute(select(Notice.id).where(flagged.is_(True)).order_by(Notice.id))).scalars().all()
    off = (await db.execute(select(Notice.id).where(flagged.is_(False)).order_by(Notice.id))).scalars().all()
    rows = (await db.execute(select(Notice.id, Notice.extra_data["n"].as_integer(), Notice.extra_data["f"].as_float(),
                                    Notice.extra_data["s"].as_string(), Notice.extra_data["a"].as_json())
                             .where(Notice.extra_data.is_not(None)).order_by(Notice.id))).all()
    return {"on": list(on), "off": list(off), "rows": [list(r) for r in rows]}


# ---- icalendar 7.0: a calendar export built from rows
@router.post("/calendar.ics")
async def calendar(body: dict):
    cal = Calendar()
    cal.add("prodid", "-//Py2axum//fixture//FR")
    cal.add("version", "2.0")
    cal.add("calscale", "GREGORIAN")
    cal.add("x-wr-calname", "Échéances, liste; test")
    for i, item in enumerate(body["items"]):
        ev = Event()
        ev.add("summary", item["title"])
        day = dt.date.fromisoformat(item["day"])
        ev.add("dtstart", day)
        ev.add("dtend", day)
        ev.add("description", "\n".join(item.get("lines", [])))
        ev.add("url", f"https://example.test/items/{i}?a=b,c")
        ev.add("uid", f"item-{i}@example.test")
        alarm = Alarm()
        alarm.add("action", "DISPLAY")
        alarm.add("description", f"Demain : {item['title']}")
        b = item.get("before", {"days": -1})
        alarm.add("trigger", dt.timedelta(days=b.get("days", 0), hours=b.get("hours", 0), seconds=b.get("seconds", 0)))
        ev.add_component(alarm)
        cal.add_component(ev)
    cal.add("summary", "repeated")
    cal.add("summary", "twice")
    return Response(content=cal.to_ical(), media_type="text/calendar")


# ---- os.remove / os.unlink
@router.post("/remove")
async def remove_files():
    out = []
    for name, fn in (("a", os.remove), ("b", os.unlink)):
        p = f"/tmp/py2axum-realworld-{name}.txt"
        with open(p, "w") as f:
            f.write("x")
        fn(p if name == "a" else Path(p))
        out.append(os.path.exists(p))
        for bad in (p, 3.5, "/tmp/nul\x00x"):
            try:
                fn(bad)
            except (OSError, TypeError, ValueError) as e:
                out.append([type(e).__name__, str(e), getattr(e, "errno", None)])
    return out

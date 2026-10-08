"""Edges found by tests/difftest.py (generation from the OpenAPI): request bodies as FastAPI decodes them
(CPython's json module, a `null` body, embedded fields), json.loads messages, bool from numbers, loops left
through `finally`."""
import json
from datetime import date, datetime, timedelta
from pathlib import Path
from decimal import Decimal

from fastapi import APIRouter, Body, File, Form, Request, UploadFile
from pydantic import BaseModel
from sqlalchemy import select, tuple_

from .db import DbDep
from .models import Task, Ticket

router = APIRouter(prefix="/edges")


class Item(BaseModel):
    name: str
    n: int = 0
    flag: bool | None = None


@router.post("/model")
async def model(item: Item):
    return item


@router.post("/opt")
async def opt(item: Item | None = None):
    return {"got": item}


@router.post("/embed")
async def embed(item: Item, k: int = Body(5)):
    return {"item": item, "k": k}


@router.post("/loads")
async def loads(request: Request):
    """json.loads of the raw body: the value, or CPython's exact error."""
    raw = await request.body()
    try:
        return {"value": json.loads(raw)}
    except json.JSONDecodeError as e:
        return {"error": str(e)}
    except ValueError as e:
        return {"other": type(e).__name__}


@router.get("/bool")
async def bools():
    out = []
    for v in (2.0, 0.5, 1.0, -0.0, Decimal("1"), Decimal("2"), Decimal("0.5"), b"yes", b"maybe", "TRUE", " true"):
        try:
            out.append(Item(name="x", flag=v).flag)
        except Exception as e:  # noqa: BLE001
            out.append(e.errors()[0]["type"])
    return out


@router.get("/loop")
async def loop():
    out = []
    for i in range(6):
        try:
            if i == 1:
                continue
            if i == 4:
                break
            out.append(i)
        finally:
            out.append(f"f{i}")
    while True:
        with open("storage-test/fixed.pdf", "rb") as f:
            out.append(len(f.read()))
            break
    for i in range(4):
        with open("storage-test/fixed.pdf", "rb") as f:
            if i < 2:
                continue
        out.append(f"w{i}")
    for i in range(3):
        try:
            try:
                if i == 2:
                    break
            finally:
                out.append(f"inner{i}")
        finally:
            out.append(f"outer{i}")
    else:
        out.append("no break")
    for i in range(3):
        try:
            try:
                raise ValueError(i)
            except ValueError:
                if i == 1:
                    break
        finally:
            out.append(f"h{i}")
    else:
        out.append("else")
    return out


@router.get("/strclass")
async def strclass():
    """str.is*() on characters where Rust's char predicates differ from CPython's"""
    return [[c.isdigit(), c.isdecimal(), c.isnumeric(), c.isalpha(), c.isalnum(), c.isspace()]
            for c in ("¾", "٣", "²", "x", "\x1c", "\x85", " ", "ǅ", "ͅ", "Ⅻ", "1")]


@router.post("/jsonb/{code}")
async def jsonb_put(code: str, body: dict, db: DbDep):
    db.add(Ticket(code=code, data=body["data"], raw=body["raw"]))
    await db.commit()
    return {"ok": code}


@router.get("/jsonb/{code}")
async def jsonb_get(code: str, db: DbDep):
    """read back from JSONB and JSON columns (the driver's float parsing)"""
    t = (await db.execute(select(Ticket).where(Ticket.code == code))).scalar_one()
    return {"data": t.data, "raw": t.raw}


@router.get("/types")
async def types(i: int | None = None, d: date | None = None, dt: datetime | None = None):
    return {"i": i, "d": d, "dt": dt}


class Stamp(BaseModel):
    d: date | None = None
    dt: datetime | None = None


@router.post("/stamp")
async def stamp(s: Stamp):
    return s


@router.post("/form")
async def form(username: str = Form(), password: str = Form("")):
    return {"username": username[:20], "n": len(username), "password": len(password)}


@router.post("/upload")
async def upload(f: UploadFile = File()):
    return {"name": f.filename, "size": len(await f.read())}


@router.get("/dates")
async def dates():
    """date bounds (years 1-9999), timedelta.days rounded down, constructors' messages"""
    out = []
    for f in (lambda: date(2020, 1, 2) + timedelta(hours=-1), lambda: date(2020, 1, 2) - timedelta(hours=1),
              lambda: date(2020, 1, 2) - timedelta(hours=-1), lambda: date(9999, 12, 31) + timedelta(days=1),
              lambda: date(1, 1, 1) - timedelta(days=1), lambda: datetime(9999, 12, 31, 23) + timedelta(hours=2),
              lambda: date.today() + timedelta(days=4825814), lambda: date(0, 1, 1), lambda: date(2020, 13, 1),
              lambda: date(2020, 2, 30), lambda: datetime(2020, 1, 1, 24), lambda: datetime(2020, 1, 1, 0, 0, 0, 10**6)):
        try:
            out.append(str(f()))
        except (ValueError, OverflowError) as e:
            out.append(f"{type(e).__name__}: {e}")
    return out


@router.get("/fmt")
async def fmt():
    return [f"{-0.0:.2f}", f"{-0.0:g}", f"{-0.0:e}", f"{-0.0:.1%}", f"{-0.0:,.1f}", f"{-0.0:>8.1f}", f"{-0.001:.2f}",
            f"{float('-nan'):.1f}", "%.2f" % -0.0]


@router.get("/nul/{name}")
async def nul(name: str):
    p = Path("storage-test") / name
    out = [p.exists(), p.is_file()]
    for f in (lambda: p.resolve(), lambda: open(p)):
        try:
            f()
            out.append("ok")
        except ValueError as e:
            out.append(str(e))
        except OSError as e:
            out.append(type(e).__name__)
    return out


class Rent(BaseModel):
    rent: float
    ratio: float | None = None
    parts: list[float] = []


@router.get("/inf", response_model=Rent)
async def inf(x: float = 9.853965795955205e307):
    """a response_model renders NaN and infinities as null (pydantic's dump_json, FastAPI 0.130+)"""
    big = x * 10
    return {"rent": big, "ratio": big - big, "parts": [x, -big]}


@router.get("/tuple-in")
async def tuple_in(db: DbDep, big: int = 2017109161287):
    """tuple_().in_(): no `::INTEGER` cast in SQLAlchemy's rendering, an id out of range matches nothing"""
    rows = (await db.execute(select(Task.title).where(tuple_(Task.title, Task.id).in_([("x", 1), ("y", big)])))).all()
    return [r[0] for r in rows]

"""Security review of the runtime (docs/advanced/security.md): each route pins a behaviour the binary must share with
FastAPI where it matters for safety. Request values that reach SQL as names (labels, subquery aliases,
`index_elements`) are quoted as SQLAlchemy quotes them, values are bound; `Path.resolve()` follows symlinks
below a missing tail; unhandled errors leak nothing; sizes chosen by the request raise, never abort."""
import secrets
from pathlib import Path
from typing import Any

import bcrypt

from fastapi import APIRouter, Body, File, Request, Response, UploadFile
from pydantic import BaseModel
from sqlalchemy import func, select, text
from sqlalchemy.dialects.postgresql import insert as pg_insert

from .db import DbDep
from .models import Membership, Owner

router = APIRouter(prefix="/sec")

# made by tests/scenarios/dynapp.reset: BASE/link -> OUTSIDE (a directory next to BASE)
BASE = Path("/tmp/py2axum-sec/base")


async def _owners(db) -> int:
    return await db.scalar(select(func.count(Owner.id)))


@router.get("/label")
async def label(name: str, db: DbDep):
    """A label named by the request: an identifier, quoted (never spliced into the SQL)."""
    rows = (await db.execute(select(func.count(Owner.id).label(name)))).mappings().all()
    return {"rows": [dict(r) for r in rows], "owners": await _owners(db)}


@router.get("/order")
async def order(sort: str, db: DbDep):
    """`order_by("<string>")`: a label of the statement, else SQLAlchemy's CompileError."""
    try:
        rows = (await db.execute(select(Owner.name.label("n")).order_by(sort).limit(1))).all()
    except Exception as e:  # noqa: BLE001
        return {"err": type(e).__name__}
    return {"n": len(rows), "owners": await _owners(db)}


@router.get("/filter")
async def filter_(q: str, db: DbDep):
    """Values are bound parameters whatever they contain."""
    out = {}
    for name, cond in (("eq", Owner.name == q), ("contains", Owner.name.contains(q)), ("ilike", Owner.name.ilike(q)),
                       ("in", Owner.name.in_([q, "zz"])), ("startswith", Owner.name.startswith(q))):
        out[name] = await db.scalar(select(func.count(Owner.id)).where(cond))
    out["owners"] = await _owners(db)
    return out


@router.get("/text")
async def text_(q: str, db: DbDep):
    """`text()` with bound parameters: the value is never re-parsed for placeholders."""
    a = (await db.execute(text("SELECT count(*) FROM owners WHERE name = :n"), {"n": q})).scalar()
    b = (await db.execute(text("SELECT CAST(:v AS text) || ''"), {"v": q})).scalar()
    return {"count": a, "echo": b, "owners": await _owners(db)}


@router.get("/sub")
async def sub(name: str, db: DbDep):
    """A subquery alias named by the request."""
    sq = select(Owner.id.label("i")).subquery(name)
    rows = (await db.execute(select(sq.c.i).order_by(sq.c.i).limit(2))).all()
    return {"n": len(rows), "owners": await _owners(db)}


@router.post("/upsert")
async def upsert(col: str, db: DbDep):
    """`index_elements` given as a string: a column name, quoted (an unknown one is the database's error)."""
    owner = await db.scalar(select(Owner.id).order_by(Owner.id).limit(1))
    stmt = pg_insert(Membership).values(owner_id=owner, role="sec", note="n").on_conflict_do_nothing(index_elements=["owner_id", col])
    try:
        await db.execute(stmt)
    except Exception as e:  # noqa: BLE001
        await db.rollback()
        return {"err": type(e).__name__}
    await db.rollback()
    return {"ok": True}


@router.get("/resolve")
async def resolve(name: str):
    """The usual upload guard: `(BASE / name).resolve()` must stay below BASE, a symlink followed."""
    dest = (BASE / name).resolve()
    try:
        rel = dest.relative_to(BASE.resolve())
    except ValueError:
        return Response(status_code=400)
    return {"rel": str(rel)}


@router.get("/header")
async def header(v: str, response: Response):
    """A response header value from the request: CR/LF cannot reach the client (500 on both sides)."""
    response.headers["x-echo"] = v
    return {"ok": True}


@router.get("/boom")
async def boom():
    """An unhandled exception: the client sees Starlette's bare 500, not the message."""
    raise ValueError("password=hunter2 postgresql://user:pw@db/x")


@router.get("/dup")
async def dup(db: DbDep):
    """A database error (IntegrityError): no SQL, no constraint name in the response."""
    owner = await db.scalar(select(Owner.id).order_by(Owner.id).limit(1))
    db.add(Owner(id=owner, name="dup"))
    await db.flush()


@router.get("/token")
async def token(n: int):
    try:
        return {"len": len(secrets.token_urlsafe(n)), "hex": len(secrets.token_hex(n))}
    except (ValueError, MemoryError, OverflowError) as e:
        return {"err": type(e).__name__, "msg": str(e)}


@router.get("/repeat")
async def repeat(n: int):
    """A repetition count from the request: MemoryError/OverflowError, never an aborted process."""
    try:
        return {"len": len("ab" * n)}
    except (MemoryError, OverflowError) as e:
        return {"err": type(e).__name__}


@router.post("/upload")
async def upload(files: list[UploadFile] = File(default=[])):
    return {"names": [f.filename for f in files]}


# ---- tests/security_check.py (the binary alone, hostile inputs; the process must survive every one)
class Deep(BaseModel):
    s: str


@router.post("/echo")
async def echo(body: Any = Body(...)):
    return body


@router.post("/model")
async def model(body: Deep):
    return {"ok": True}


@router.get("/cookies")
async def cookies(request: Request):
    return {"n": len(request.cookies)}


@router.get("/hash")
async def hash_(pw: str = "s3cret"):
    """bcrypt at its default cost (~0.25 s of CPU): must not stall the other requests."""
    return {"ok": bcrypt.checkpw(pw.encode(), bcrypt.hashpw(pw.encode(), bcrypt.gensalt()))}

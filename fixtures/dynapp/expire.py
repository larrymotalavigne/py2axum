"""session.expire(obj) / expire(obj, [names]) on an AsyncSession: what is expired, the refresh-expire cascade,
changes not flushed that are lost, errors."""
from decimal import Decimal

from fastapi import APIRouter
from sqlalchemy import select
from sqlalchemy.exc import InvalidRequestError, SQLAlchemyError

from .db import DbDep
from .models import Project, Task

router = APIRouter(prefix="/expire")


def keys(o) -> list[str]:
    return sorted(k for k in o.__dict__ if not k.startswith("_"))


def masked(e: Exception) -> list[str]:
    msg = str(e)
    if " at 0x" in msg:
        msg = msg[: msg.index(" at 0x")] + msg[msg.index(">", msg.index(" at 0x")):]
    return [type(e).__name__, msg]


@router.post("/run")
async def run(db: DbDep):
    p = Project(name="p", budget=Decimal("1.50"))
    t = Task(title="t1")
    p.tasks.append(t)
    db.add(p)
    await db.commit()
    pid = p.id
    # (not compared: __dict__ right after the INSERT, nor the class of the lazy load error, StatementError in
    # SQLAlchemy and MissingGreenlet in the runtime)
    out = {}
    # the whole object, and its tasks through cascade="all" (refresh-expire): their changes are lost
    p.name, t.title = "changed", "tch"
    db.expire(p)
    out["whole"] = [keys(p), keys(t), "tasks" in p.__dict__]
    try:
        p.id
    except SQLAlchemyError:
        out["whole_key"] = "load needed"
    await db.flush()
    out["db"] = [list(r) for r in await db.execute(select(Project.name, Task.title).join(Task.project).where(Project.id == pid))]
    await db.refresh(p)
    await db.refresh(t)
    out["refreshed"] = [p.name, t.title, keys(p), keys(t)]
    # only the attributes named: the other changes stay
    p.name, p.budget = "n2", Decimal("9.25")
    db.expire(p, ["name"])
    out["partial"] = [keys(p), str(p.budget)]
    await db.flush()
    out["db2"] = [[r[0], str(r[1])] for r in await db.execute(select(Project.name, Project.budget).where(Project.id == pid))]
    await db.refresh(p, ["name", "tasks"])
    out["rel_loaded"] = [keys(p), len(p.tasks)]
    db.expire(p, ["tasks", "budget"])
    out["rel"] = [keys(p), keys(t)]
    db.expire(t, attribute_names=("title",))
    db.expire(t, attribute_names=None)
    out["kw"] = keys(t)
    await db.refresh(t)
    db.expire(t, [])
    out["empty"] = keys(t)
    await db.refresh(t)
    # the key: kept for the identity, gone from __dict__, read through a load
    db.expire(t, ["id"])
    out["pk"] = keys(t)
    try:
        t.id
    except SQLAlchemyError:
        out["pk_read"] = "load needed"
    await db.refresh(t)
    out["pk_back"] = [keys(t), t.id > 0]
    # errors
    errs = {}
    for name, names in (("unknown", ["nope"]), ("str", "title"), ("int", [1]), ("prop", ["__class__"])):
        try:
            db.expire(t, names)
            errs[name] = "ok"
        except KeyError as e:
            errs[name] = ["KeyError", str(e)]
    try:
        db.expire(p, ["name", "nope"])
    except KeyError as e:
        errs["then"] = [str(e), "name" in p.__dict__, "budget" in p.__dict__]
    for name, obj in (("transient", Project(name="x")), ("dict", {"a": 1}), ("none", None), ("int_obj", 3)):
        try:
            db.expire(obj)
            errs[name] = "ok"
        except InvalidRequestError as e:
            errs[name] = masked(e)
    pending = Task(title="pending")
    db.add(pending)
    try:
        db.expire(pending, ["title"])
    except InvalidRequestError as e:
        errs["pending"] = masked(e)
    await db.flush()
    await db.delete(pending)
    try:
        db.expire(pending)
        errs["deleted_unflushed"] = "ok"
    except InvalidRequestError as e:
        errs["deleted_unflushed"] = masked(e)
    await db.flush()
    try:
        db.expire(pending)
    except InvalidRequestError as e:
        errs["deleted"] = masked(e)
    db.expunge(t)
    try:
        db.expire(t)
    except InvalidRequestError as e:
        errs["detached"] = masked(e)
    out["errs"] = errs
    await db.rollback()
    return out

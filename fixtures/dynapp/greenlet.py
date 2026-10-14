"""An attribute that needs IO, read in an AsyncSession: an expired column, an expired primary key, a lazy
relationship not loaded (one-to-many, many-to-one). SQLAlchemy raises `StatementError` whose `.orig` (and
`__cause__`) is the `MissingGreenlet`: `except MissingGreenlet` does not catch it, `except StatementError`
does. `str(e)` is compared up to its first line (docs/tutorial/sql.md: the `[SQL: ...]` lines are not
reproduced). Also `.orig` of a database error."""
from fastapi import APIRouter
from sqlalchemy import select
from sqlalchemy.exc import DBAPIError, IntegrityError, MissingGreenlet, SQLAlchemyError, StatementError

from .db import DbDep
from .models import Archive, Owner, Project, Task

router = APIRouter(prefix="/greenlet")


def caught(read) -> dict:
    """which clause catches the error of `read()`, and what it carries"""
    try:
        try:
            read()
            return {"clause": "none"}
        except MissingGreenlet:
            return {"clause": "MissingGreenlet"}
    except StatementError as e:
        return {
            "clause": "StatementError",
            "type": type(e).__name__,
            "first": str(e).split("\n")[0],
            "args": list(e.args),
            "repr": repr(e).split("\\n")[0],
            "orig": [type(e.orig).__name__, str(e.orig)],
            "orig_is_cause": e.orig is e.__cause__,
            "isinstance": [isinstance(e, SQLAlchemyError), isinstance(e, DBAPIError),
                           isinstance(e.orig, MissingGreenlet), isinstance(e.orig, StatementError)],
        }


@router.post("/run")
async def run(db: DbDep):
    owner = Owner(name="o")
    p = Project(name="p", owner=owner)
    p.tasks.append(Task(title="t1"))
    db.add(p)
    await db.commit()  # expire_on_commit: every attribute of p is expired, its key too
    out = {"expired_column": caught(lambda: p.name), "expired_key": caught(lambda: p.id)}
    await db.refresh(p)
    out["refreshed"] = p.name
    db.expunge_all()
    q = (await db.execute(select(Project).where(Project.name == "p"))).scalar_one()
    out["lazy_one_to_many"] = caught(lambda: q.tasks)
    t = (await db.execute(select(Task).where(Task.title == "t1"))).scalar_one()
    db.expunge(q)  # not in the identity map any more: the many-to-one needs a SELECT
    out["lazy_many_to_one"] = caught(lambda: t.project)
    # caught by its base class, as the applications do
    try:
        q2 = (await db.execute(select(Project).where(Project.name == "p"))).scalar_one()
        q2.tasks
        out["base"] = "none"
    except SQLAlchemyError as e:
        out["base"] = [type(e).__name__, type(e.orig).__name__]
    await db.rollback()
    return out


@router.post("/dbapi")
async def dbapi(db: DbDep):
    """`.orig` of a database error: the driver's exception, also its `__cause__`"""
    db.add(Owner(id=990001, name="a"))
    await db.commit()
    db.add(Owner(id=990001, name="b"))
    try:
        await db.commit()
    except IntegrityError as e:
        await db.rollback()
        return {"orig": type(e.orig).__name__, "orig_is_cause": e.orig is e.__cause__,
                "orig_first": str(e.orig).split("\n")[0], "first": str(e).split("\n")[0]}
    return "no error"


@router.get("/read/{what}")
async def read(what: str, db: DbDep):
    """One read needing IO, with SQLAlchemy 2.0 too (MissingGreenlet itself there): an expired column, the key
    of an object expired as a whole, a deferred column, a many-to-one whose target is expired, a one-to-many
    never loaded. Rows made here and rolled back."""
    p = Project(name="greenlet")
    a = Archive(name="greenlet", blob=b"x")
    db.add_all([p, a])
    await db.flush()
    t = Task(title="greenlet", project_id=p.id)
    db.add(t)
    await db.flush()
    aid, tid = a.id, t.id
    if what != "o2m":
        db.expire_all()
    if what in ("deferred", "m2o"):
        # reloaded without its deferred column / the task alone (its project stays expired)
        await db.execute(select(Archive).where(Archive.id == aid))
        await db.execute(select(Task).where(Task.id == tid))
    try:
        if what == "column":
            got = p.name
        elif what == "key":
            got = p.id
        elif what == "deferred":
            got = a.blob
        elif what == "m2o":
            got = t.project
        else:
            got = p.tasks
        out = {"error": None, "got": repr(got)[:20]}
    except SQLAlchemyError as e:
        orig = getattr(e, "orig", None)
        out = {"error": type(e).__name__, "statement_error": isinstance(e, StatementError), "str": str(e).split("\n")[0],
               "orig": type(orig).__name__}
    await db.rollback()
    return out

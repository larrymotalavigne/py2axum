"""Shapes that made reference cycles in the runtime (CPython's cyclic GC frees them, Arc does not): a nested
function naming itself, objects a failed flush, a rollback or a DELETE left linked, a stream cut by the client
while a `yield` dependency is open. Measured by bench/leak_isolate.py (kind `leak_*` of bench/soak.py)."""
import asyncio

from fastapi import APIRouter
from fastapi.responses import StreamingResponse
from sqlalchemy import select, text
from sqlalchemy.exc import IntegrityError

from .db import DbDep
from .models import Project, Task

router = APIRouter(prefix="/leak")


@router.get("/selfref")
async def selfref(n: int = 3):
    payload = "x" * 100000

    def walk(k: int) -> int:
        return len(payload) if k <= 0 else walk(k - 1)

    return {"v": walk(n)}


@router.get("/selfref_cell")
async def selfref_cell(n: int = 3):
    payload = "y" * 100000

    def walk(k: int) -> int:
        return len(payload) + k if k <= 0 else walk(k - 1)

    # `walk` read by another closure too: a cell variable of selfref_cell
    pick = lambda: walk  # noqa: E731
    return {"v": pick()(n), "same": pick() is walk}


def make_fact(base: int):
    def fact(k: int) -> int:
        return base if k <= 1 else k * fact(k - 1)

    return fact


def make_twice():
    def twice(k: int) -> int:
        return k if k <= 0 else 2 + twice(k - 1)

    twice.calls = 0
    return twice


@router.get("/selfref_escape")
async def selfref_escape(n: int = 5):
    """The function outlives the frame that defined it: it still finds itself."""
    f, g = make_fact(1), make_twice()
    held = [make_fact(2)]
    return {"v": f(n), "g": g(n), "held": held[0](n), "name": f.__qualname__, "calls": g.calls,
            "fresh": make_fact(1) is not make_fact(1)}


async def _seed(db) -> int:
    return (await db.execute(text("INSERT INTO projects (name) VALUES ('seed') RETURNING id"))).scalar_one()


@router.post("/dup_flush")
async def dup_flush(db: DbDep):
    """An INSERT that fails (duplicate key) after its children were linked to it."""
    pid = await _seed(db)
    p = Project(id=pid, name="dup", tasks=[Task(title="a" * 50), Task(title="b")])
    db.add(p)
    try:
        await db.flush()
    except IntegrityError:
        await db.rollback()
        return {"dup": True, "tasks": len(p.tasks)}
    return {"dup": False}


@router.post("/rolled_back")
async def rolled_back(db: DbDep):
    """Objects INSERTed, then the transaction rolled back: transient again, still linked."""
    p = Project(name="rb", tasks=[Task(title="c"), Task(title="d")])
    db.add(p)
    await db.flush()
    await db.rollback()
    return {"rolled_back": True}


@router.post("/deleted")
async def deleted(db: DbDep):
    """Objects DELETEd (cascade) in the same session that loaded both sides of the relationship."""
    p = Project(name="del", tasks=[Task(title="e"), Task(title="f")])
    db.add(p)
    await db.flush()
    await db.delete(p)
    await db.flush()
    gone = await db.scalar(select(Project.id).where(Project.id == p.id))
    tasks = len(p.tasks)
    await db.rollback()
    return {"gone": gone is None, "tasks": tasks}


@router.get("/stream_dep")
async def stream_dep(db: DbDep, n: int = 5, pause: float = 0.02):
    """A stream with a `yield` dependency (the session) open until its end: cut by the client halfway."""
    first = await db.scalar(text("SELECT 1"))

    async def parts():
        for i in range(n):
            await asyncio.sleep(pause)
            yield f"{first}:{i}\n"

    return StreamingResponse(parts(), media_type="text/plain")

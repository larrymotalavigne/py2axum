"""Shapes that made reference cycles in the runtime (CPython's cyclic GC frees them, Arc does not): a nested
function naming itself, objects a failed flush, a rollback or a DELETE left linked, a stream cut by the client
while a `yield` dependency is open. Measured by bench/leak_isolate.py (kind `leak_*` of bench/soak.py)."""
import asyncio
import logging

from fastapi import APIRouter, Request
from fastapi.responses import StreamingResponse
from sqlalchemy import select, text
from sqlalchemy.exc import IntegrityError

from .db import DbDep
from .models import Project, Task

router = APIRouter(prefix="/leak")
log = logging.getLogger(__name__)


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


@router.get("/task_state")
async def task_state(request: Request, n: int = 3):
    """A task kept in the request's own state (request -> task -> its coroutine's context -> request)."""
    payload = "z" * 100000

    async def work():
        return len(payload) + n

    request.state.task = asyncio.create_task(work())
    return {"v": await request.state.task, "done": request.state.task.done()}


async def _boom(n: int) -> int:
    await asyncio.sleep(0)
    raise ValueError(f"boom {n}")


@router.get("/task_failed_state")
async def task_failed_state(request: Request, n: int = 3):
    """A task that failed, kept in the request's state and never awaited: asyncio logs « Task exception was never
    retrieved » once the request is gone (CPython frees it with the scope, by reference count)."""
    payload = "v" * 100000
    ended = asyncio.Event()
    request.state.task = asyncio.create_task(_boom(n))
    request.state.task.add_done_callback(lambda t: ended.set())
    await ended.wait()
    return {"done": request.state.task.done(), "n": len(payload)}


async def _ticks(n: int):
    try:
        for i in range(n):
            yield i
    finally:
        log.warning("ticks closed after %s", n)


@router.get("/agen_state")
async def agen_state(request: Request, n: int = 3, start: bool = True):
    """An async generator kept in the request's state, suspended at its first `yield` (or never started): closed
    once the request is gone, its `finally` runs."""
    payload = "u" * 100000
    request.state.gen = _ticks(n)
    first = await anext(request.state.gen) if start else None
    return {"first": first, "n": len(payload)}


@router.post("/rolled_back/state")
async def rolled_back_state(db: DbDep, delete: bool = False):
    """After the rollback, objects INSERTed in the transaction are transient again: out of the session, keeping
    their attributes (key included) and their loaded relationships; added again, they INSERT with that key."""
    p = Project(name="rb", tasks=[Task(title="c"), Task(title="d")])
    db.add(p)
    await db.flush()
    pid, tid = p.id, p.tasks[0].id
    if delete:
        await db.delete(p)
        await db.flush()
    await db.rollback()
    out = {"in": p in db, "task_in": p.tasks[0] in db, "id": p.id == pid, "name": p.name, "tasks": len(p.tasks),
           "tid": p.tasks[0].id == tid, "back": p.tasks[0].project is p, "rev": p.tasks[0].revision,
           "keys": sorted(k for k in vars(p) if not k.startswith("_"))}
    db.add(p)
    await db.flush()
    out["again"] = [p.id == pid, p in db, len(p.tasks), p.tasks[1].id is not None]
    await db.rollback()
    return out


@router.post("/dup_flush/state")
async def dup_flush_state(db: DbDep):
    """A failed flush, then the rollback: the parent and its children transient, still linked both ways."""
    pid = await _seed(db)
    p = Project(id=pid, name="dup", tasks=[Task(title="a"), Task(title="b")])
    db.add(p)
    try:
        await db.flush()
    except IntegrityError:
        await db.rollback()
    return {"in": p in db, "id": p.id == pid, "tasks": len(p.tasks), "back": p.tasks[0].project is p,
            "tid": p.tasks[0].id, "task_in": p.tasks[0] in db}


@router.get("/stream_dep")
async def stream_dep(db: DbDep, n: int = 5, pause: float = 0.02):
    """A stream with a `yield` dependency (the session) open until its end: cut by the client halfway."""
    first = await db.scalar(text("SELECT 1"))

    async def parts():
        for i in range(n):
            await asyncio.sleep(pause)
            yield f"{first}:{i}\n"

    return StreamingResponse(parts(), media_type="text/plain")

"""Coroutine objects: calls not awaited, asyncio.gather, tasks, async context managers, Semaphore/Lock."""
import asyncio

from fastapi import APIRouter

router = APIRouter(prefix="/aio")
LOG: list = []


async def slow(n, fail=False):
    await asyncio.sleep(0.01 * n)
    LOG.append(n)
    if fail:
        raise ValueError(f"bad {n}")
    return n * 10


class Svc:
    def __init__(self):
        self.calls = 0

    async def fetch(self, k):
        self.calls += 1
        return f"v{k}"

    def plain(self, k):
        return k


class CM:
    def __init__(self, suppress):
        self.suppress = suppress
        self.events = []

    async def __aenter__(self):
        self.events.append("enter")
        return self

    async def __aexit__(self, et, e, tb):
        self.events.append(f"exit:{et.__name__ if et else None}:{e}")
        return self.suppress


@router.get("/sleep/{ms}")
async def sleep_route(ms: int):
    """a request that stays in flight for `ms` milliseconds (graceful shutdown tests)"""
    await asyncio.sleep(ms / 1000)
    return {"slept_ms": ms}


@router.get("/gather")
async def gather_route():
    LOG.clear()
    r = await asyncio.gather(slow(3), slow(1), slow(2))
    order = list(LOG)
    r2 = await asyncio.gather(slow(1), slow(2, fail=True), return_exceptions=True)
    err = None
    try:
        await asyncio.gather(*(slow(i, fail=i == 2) for i in range(3)))
    except ValueError as e:
        err = str(e)
    coros = [slow(i) for i in range(3)]
    r3 = await asyncio.gather(*coros)
    return {"r": r, "order": order, "r2": [x if not isinstance(x, Exception) else f"{type(x).__name__}: {x}" for x in r2],
            "err": err, "r3": r3}


@router.get("/coro")
async def coro_route():
    c = slow(1)
    v = await c
    again = None
    try:
        await c
    except RuntimeError as e:
        again = str(e)
    s = Svc()
    pending = s.fetch(5)
    before = s.calls
    got = await pending
    t = asyncio.create_task(slow(2))
    tv = await t
    return {"v": v, "again": again, "before": before, "got": got, "after": s.calls, "plain": s.plain(3), "task": tv}


@router.get("/acm")
async def acm():
    out = []
    a = CM(False)
    async with a as x:
        out.append(x is a)
    b = CM(True)
    async with b:
        raise KeyError("k")
    c = CM(False)
    try:
        async with c:
            raise ValueError("v")
    except ValueError:
        out.append("propagated")
    sem = asyncio.Semaphore(2)
    async with sem:
        out.append(sem.locked())
    lock = asyncio.Lock()
    async with lock:
        out.append(lock.locked())
    out.append(lock.locked())
    return {"out": out, "a": a.events, "b": b.events, "c": c.events}

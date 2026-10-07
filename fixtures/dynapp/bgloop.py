"""threading + a background asyncio loop: Lock/RLock/Event/Thread, run_coroutine_threadsafe, wrap_future,
run_in_executor, `with` on locks and on a project class."""
import asyncio
import random
import threading

from fastapi import APIRouter
from sqlalchemy import func, select
from sqlalchemy.ext.asyncio import AsyncEngine, AsyncSession, async_sessionmaker

from .db import DbDep
from .models import Owner

router = APIRouter(prefix="/bg")

_bg_loop = None
_bg_thread = None
_init_lock = threading.Lock()


def _ensure_bg_loop():
    global _bg_loop, _bg_thread
    if _bg_loop is not None and _bg_loop.is_running():
        return _bg_loop
    with _init_lock:
        if _bg_loop is None or not (_bg_loop.is_running() and _bg_thread is not None and _bg_thread.is_alive()):
            ready = threading.Event()
            loop = asyncio.new_event_loop()

            def _run():
                loop.call_soon(ready.set)
                loop.run_forever()

            t = threading.Thread(target=_run, name="bg-loop", daemon=True)
            t.start()
            ready.wait()
            _bg_loop = loop
            _bg_thread = t
    return _bg_loop


async def _await_on_bg(coro):
    return await asyncio.wrap_future(asyncio.run_coroutine_threadsafe(coro, _ensure_bg_loop()))


async def work(x):
    await asyncio.sleep(0.01)
    return x * 2


class Store:
    def __init__(self):
        self._lock = threading.RLock()
        self.data = {}
        self._locks = {}

    def get(self, k):
        with self._lock:
            with self._lock:
                return self.data.get(k)

    def put(self, k, v):
        with self._lock:
            self.data[k] = v

    def try_lock(self, k):
        with self._lock:
            lock = self._locks.setdefault(k, threading.Lock())
        return lock.acquire(blocking=False)

    def release(self, k):
        self._locks[k].release()


class Tracer:
    def __init__(self, swallow):
        self.swallow = swallow
        self.log = []

    def __enter__(self):
        self.log.append("in")
        return self

    def __exit__(self, et, e, tb):
        self.log.append(f"out:{et.__name__ if et else None}")
        return self.swallow


STORE = Store()


@router.get("/run")
async def run():
    main = threading.get_ident()
    r = await _await_on_bg(work(21))
    same = _ensure_bg_loop() is _ensure_bg_loop()
    STORE.put("a", r)
    got = STORE.get("a")
    locks = [STORE.try_lock("k"), STORE.try_lock("k")]
    STORE.release("k")
    locks.append(STORE.try_lock("k"))
    STORE.release("k")
    err = None
    try:
        STORE.release("k")
    except RuntimeError as e:
        err = str(e)
    rl = threading.RLock()
    rerr = None
    try:
        rl.release()
    except RuntimeError as e:
        rerr = str(e)
    loop = asyncio.get_running_loop()
    other = await loop.run_in_executor(None, threading.get_ident)
    ev = threading.Event()
    waited = [ev.wait(0.01), ev.is_set()]
    ev.set()
    waited.append(ev.wait(1))
    a, b = Tracer(False), Tracer(True)
    with a as x:
        same_t = x is a
    with b:
        raise KeyError("swallowed")
    return {"r": r, "same": same, "got": got, "locks": locks, "err": err, "rerr": rerr, "other": other != main,
            "loop": asyncio.get_running_loop() is loop, "waited": waited, "tracer": [a.log, b.log, same_t],
            "thread": [_bg_thread.name, _bg_thread.is_alive(), _bg_thread.daemon]}


async def agen(n):
    for i in range(n):
        await asyncio.sleep(0)
        yield i * 3


@router.get("/misc")
async def misc(db: DbDep):
    r, u, s = random.random(), random.uniform(1, 2), random.sample([1, 2, 3], 2)
    bind = db.bind
    fresh = async_sessionmaker(bind=bind, autocommit=False, autoflush=False, expire_on_commit=False)()
    n = await fresh.scalar(select(func.count()).select_from(Owner))
    await fresh.aclose()
    xs = [x async for x in agen(4) if x != 3]
    return {"r": 0 <= r < 1, "u": 1 <= u <= 2, "s": len(set(s)) == 2, "bind": isinstance(bind, AsyncEngine),
            "sess": [isinstance(db, AsyncSession), isinstance(fresh, AsyncSession), isinstance(db, (AsyncEngine, AsyncSession))],
            "n": n >= 0, "xs": xs}

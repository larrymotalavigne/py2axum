"""The application lifespan (`FastAPI(lifespan=...)`), async generators driven by hand (`anext`, `aclose`,
`asend`, `athrow`), `@asynccontextmanager`, `contextlib.suppress`, `contextvars.ContextVar`, `Task.cancel`."""
import asyncio
import contextlib
from contextlib import asynccontextmanager
from contextvars import ContextVar

from fastapi import APIRouter

router = APIRouter(prefix="/life")

STATE = {"started": False, "ticks": 0, "stopped": False}
EVENTS: list = []
_current: ContextVar[str] = ContextVar("dynapp_current")
_with_default: ContextVar[int] = ContextVar("dynapp_counter", default=7)


async def _tick_loop():
    while True:
        await asyncio.sleep(0.02)
        STATE["ticks"] += 1


@asynccontextmanager
async def lifespan(_app):
    task: asyncio.Task | None = asyncio.create_task(_tick_loop())
    STATE["started"] = True
    async with resource("lifespan"):
        yield
    STATE["stopped"] = True
    if task is not None:
        task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await task
    print("lifespan: stopped, ticked:", STATE["ticks"] > 0, "cancelled:", task.cancelled(), flush=True)


@asynccontextmanager
async def resource(name):
    EVENTS.append(f"open:{name}")
    try:
        yield name.upper()
    except KeyError as e:
        EVENTS.append(f"caught:{e}")
    finally:
        EVENTS.append(f"close:{name}")


@asynccontextmanager
async def reraise(name):
    EVENTS.append(f"open:{name}")
    try:
        yield name
    finally:
        EVENTS.append(f"close:{name}")


@asynccontextmanager
async def no_yield():
    if False:
        yield


async def numbers(n):
    EVENTS.append("gen:start")
    try:
        for i in range(n):
            got = yield i
            if got is not None:
                EVENTS.append(f"sent:{got}")
    except ValueError as e:
        EVENTS.append(f"thrown:{e}")
        yield -1
    finally:
        EVENTS.append("gen:end")


async def dep_like():
    EVENTS.append("dep:open")
    try:
        yield "session"
    finally:
        EVENTS.append("dep:close")


@router.get("/state")
async def state():
    return {"started": STATE["started"], "ticking": STATE["ticks"] > 0, "stopped": STATE["stopped"]}


@router.get("/acm")
async def acm():
    EVENTS.clear()
    async with resource("a") as r:
        EVENTS.append(f"body:{r}")
    async with resource("b"):
        raise KeyError("swallowed")
    try:
        async with reraise("c"):
            raise ValueError("kept")
    except ValueError as e:
        EVENTS.append(f"outer:{e}")
    try:
        async with no_yield():
            pass
    except RuntimeError as e:
        EVENTS.append(f"runtime:{e}")
    return EVENTS


@router.get("/agen")
async def agen():
    EVENTS.clear()
    g = numbers(3)
    EVENTS.append("created")
    a = await anext(g)
    b = await g.asend("x")
    c = await g.athrow(ValueError("boom"))
    await g.aclose()
    try:
        await anext(g)
    except StopAsyncIteration:
        EVENTS.append("stop")
    h = numbers(5)
    first = await anext(h)
    await h.aclose()
    d = dep_like()
    v = await anext(d)
    EVENTS.append(f"got:{v}")
    await d.aclose()
    provider = dep_like
    p = provider()
    w = await anext(p)
    await p.aclose()
    return {"values": [a, b, c, first, w], "events": EVENTS}


@router.get("/suppress")
async def suppress():
    out = []
    with contextlib.suppress(KeyError, IndexError):
        out.append("in")
        {}["x"]
        out.append("never")
    with contextlib.suppress(KeyError):
        out.append("clean")
    try:
        with contextlib.suppress(KeyError):
            raise ValueError("passes")
    except ValueError as e:
        out.append(str(e))
    return out


async def _read_current():
    return _current.get("none")


@router.get("/ctxvar")
async def ctxvar():
    out = [_current.get("dflt"), _with_default.get()]
    try:
        _current.get()
    except LookupError:
        out.append("lookup")
    tok = _current.set("a")
    out.append(_current.get())
    tok2 = _current.set("b")
    out.append(await _read_current())
    _current.reset(tok2)
    out.append(_current.get())
    _current.reset(tok)
    out.append(_current.get("gone"))
    t = _with_default.set(1)
    out.append(_with_default.get())
    _with_default.reset(t)
    out.append(_with_default.get())
    return out


@router.get("/ctxvar/isolated")
async def ctxvar_isolated():
    # a fresh request: nothing set by an earlier one leaks
    return [_current.get("fresh")]


@router.get("/cancel")
async def cancel():
    async def sleeper():
        await asyncio.sleep(10)
        return "late"

    t = asyncio.create_task(sleeper())
    await asyncio.sleep(0.01)
    first = t.cancel()
    out = [first]
    try:
        await t
    except asyncio.CancelledError:
        out.append("cancelled")
    out += [t.done(), t.cancelled(), t.cancel()]
    done = asyncio.create_task(_read_current())
    out.append(await done)
    out += [done.cancel(), done.cancelled()]
    return out

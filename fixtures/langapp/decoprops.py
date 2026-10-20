"""Class decorators of library-shaped code: a property's setter (`@name.setter`), a property without one, an
inherited setter, the accessors reached through augmented assignment, setattr() and a custom __setattr__."""
import functools
from contextlib import contextmanager

from fastapi import APIRouter

router = APIRouter(prefix="/decop")


class Box:
    """A value that checks itself on assignment."""

    def __init__(self, v=1):
        self._v = 0
        self.history = []
        self.v = v  # runs the setter

    @property
    def v(self):
        """the value"""
        return self._v

    @v.setter
    def v(self, x):
        if not isinstance(x, int):
            raise TypeError(f"v must be an int, not {type(x).__name__}")
        if x < 0:
            raise ValueError("v must not be negative")
        self.history.append(x)
        self._v = x

    @property
    def double(self):
        return self._v * 2

    @property
    def label(self):
        return f"box:{self._v}"


class Wide(Box):
    """Overrides the getter only: the inherited setter is gone (a new property without one)."""

    def __init__(self, v=1):
        self._v = v
        self.history = []

    @property
    def v(self):
        return self._v + 1000


class Plain(Box):
    """Inherits the property and its setter untouched."""

    scale = 3

    def bump(self):
        self.v += self.scale
        return self.v


class Lazy:
    @property
    def shadow(self):
        return "class"

    note = "kept"


def attempt(fn):
    try:
        return {"ok": fn()}
    except Exception as e:  # the error class and message are what CPython raises
        return {"error": type(e).__name__, "message": str(e)}


@router.get("/setter")
async def setter_route(n: int = 5):
    b = Box()
    out = {"initial": b.v}
    b.v = n
    out["after"] = b.v
    out["double"] = b.double
    b.v += 2
    out["aug"] = b.v
    out["negative"] = attempt(lambda: setattr(b, "v", -1))
    out["bad_type"] = attempt(lambda: setattr(b, "v", "x"))
    out["history"] = list(b.history)
    out["state"] = b._v
    return out


@router.get("/readonly")
async def readonly_route():
    b = Box(4)

    def set_double():
        b.double = 3

    def set_label():
        b.label += "!"

    return {"double": attempt(set_double), "label": attempt(set_label), "value": b.double, "text": b.label}


@router.get("/inherit")
async def inherit_route():
    p = Plain(2)
    p.v = 10
    out = {"plain": p.v, "bump": p.bump(), "history": list(p.history)}

    def set_wide():
        w = Wide(1)
        w.v = 5

    out["wide"] = attempt(set_wide)
    out["wide_read"] = Wide(1).v
    return out


@router.get("/instance-attr")
async def instance_attr_route():
    lz = Lazy()
    out = {"before": lz.shadow}
    out["set_property"] = attempt(lambda: setattr(lz, "shadow", 1))
    lz.note = "shadowed"  # a class attribute may be shadowed by the instance
    out["note"] = [lz.note, Lazy.note]
    return out


# ---- decorators applied to methods as values -------------------------------------------------------------

EVENTS: list = []


def logged(fn):
    @functools.wraps(fn)
    def wrapper(self, *args, **kwargs):
        EVENTS.append(f"call {fn.__name__}")
        return fn(self, *args, **kwargs)

    return wrapper


def retried(times):
    """A decorator factory: its argument is read at class creation."""

    def deco(fn):
        @functools.wraps(fn)
        async def wrapper(self, *args, **kwargs):
            last = None
            for attempt in range(times):
                try:
                    return await fn(self, *args, **kwargs)
                except ValueError as e:
                    last = e
                    EVENTS.append(f"retry {attempt}")
            raise last

        return wrapper

    return deco


class Stream:
    def __init__(self, name):
        self.name = name
        self.opened = 0

    @contextmanager
    def open(self, fail=False):
        """a context manager built from a generator"""
        self.opened += 1
        EVENTS.append(f"open {self.name}")
        try:
            yield f"<{self.name}>"
        except KeyError as e:
            EVENTS.append(f"caught {e}")
            if fail:
                raise
        finally:
            EVENTS.append(f"close {self.name}")

    @logged
    def size(self, factor=1):
        """the size"""
        return len(self.name) * factor

    @retried(3)
    async def fetch(self, n):
        if n < 2:
            raise ValueError(f"too small: {n}")
        return n * 10

    @functools.cached_property
    def heavy(self):
        EVENTS.append(f"heavy {self.name}")
        return self.name.upper()


@contextmanager
def scope(label):
    EVENTS.append(f"enter {label}")
    yield label
    EVENTS.append(f"leave {label}")


@router.get("/methods")
async def methods_route():
    EVENTS.clear()
    st = Stream("abc")
    out = {}
    with st.open() as handle:
        out["handle"] = handle
    try:
        with st.open(fail=True) as handle:
            raise KeyError("boom")
    except KeyError as e:
        out["raised"] = str(e)
    with st.open() as handle:
        try:
            raise KeyError("swallowed")
        except KeyError:
            pass
    with st.open():
        pass
    out["size"] = [st.size(), st.size(3), st.size(factor=2)]
    out["heavy"] = [st.heavy, st.heavy, "heavy" in vars(st)]
    st.heavy = "assigned"
    out["heavy_set"] = st.heavy
    out["fetch"] = [await st.fetch(5), attempt_fetch_ok(await attempt_async(st, 0))]
    with scope("x") as lab:
        out["scope"] = lab
    out["events"] = list(EVENTS)
    return out


async def attempt_async(st, n):
    try:
        return {"ok": await st.fetch(n)}
    except ValueError as e:
        return {"error": str(e)}


def attempt_fetch_ok(v):
    return v


@router.get("/ctx-errors")
async def ctx_errors_route():
    st = Stream("e")
    cm = st.open()
    out = {"type": type(cm).__name__}

    async def use_async():
        async with cm:
            pass

    try:
        await use_async()
    except Exception as e:
        out["async_with"] = [type(e).__name__, str(e)]

    @contextmanager
    def no_yield():
        return
        yield

    @contextmanager
    def twice():
        yield 1
        yield 2

    for name, mk in (("no_yield", no_yield), ("twice", twice)):
        try:
            with mk():
                pass
        except Exception as e:
            out[name] = [type(e).__name__, str(e)]
    return out

"""Protocol methods of plain classes: iteration, containers, truth, arithmetic (dynrt/proto.rs)."""
import logging
from typing import Generic, Protocol, TypeVar

from fastapi import APIRouter, HTTPException

from . import conf, flags
from .lazy import Greeter, tools

router = APIRouter(prefix="/p")
log = logging.getLogger("protoapp")
T = TypeVar("T")


class Countdown:
    def __init__(self, n, fail_at=None):
        self.n = n
        self.fail_at = fail_at

    def __iter__(self):
        return self

    def __next__(self):
        if self.n <= 0:
            raise StopIteration
        if self.n == self.fail_at:
            raise ValueError(f"boom at {self.n}")
        log.warning("next %s", self.n)
        self.n -= 1
        return self.n + 1


class Naturals:
    """endless: only a lazy loop with break ends"""

    def __init__(self):
        self.i = 0

    def __iter__(self):
        return self

    def __next__(self):
        self.i += 1
        return self.i


class Bag:
    def __init__(self, *items):
        self._items = list(items)

    def __iter__(self):
        return iter(self._items)

    def __len__(self):
        return len(self._items)

    def __contains__(self, x):
        log.warning("contains %r", x)
        return x in self._items

    def __getitem__(self, i):
        return self._items[i]

    def __setitem__(self, i, v):
        self._items[i] = v

    def __delitem__(self, i):
        del self._items[i]


class Squares:
    """the old sequence protocol: __getitem__ until IndexError"""

    def __init__(self, n):
        self.n = n

    def __getitem__(self, i):
        if i >= self.n:
            raise IndexError(i)
        return i * i


class Flag:
    def __init__(self, on, size=None):
        self.on = on
        self.size = size

    def __bool__(self):
        return self.on


class NotIter:
    def __iter__(self):
        return Flag(True)


class Sized:
    def __init__(self, n):
        self.n = n

    def __len__(self):
        return self.n


class Vec:
    def __init__(self, x, y):
        self.x = x
        self.y = y

    def __add__(self, o):
        if isinstance(o, Vec):
            return Vec(self.x + o.x, self.y + o.y)
        if isinstance(o, (int, float)):
            return Vec(self.x + o, self.y + o)
        return NotImplemented

    def __radd__(self, o):
        return self.__add__(o)

    def __mul__(self, k):
        if not isinstance(k, (int, float)):
            return NotImplemented
        return Vec(self.x * k, self.y * k)

    def __rmul__(self, k):
        return self * k

    def __sub__(self, o):
        return Vec(self.x - o.x, self.y - o.y)

    def __neg__(self):
        return Vec(-self.x, -self.y)

    def __abs__(self):
        return (self.x ** 2 + self.y ** 2) ** 0.5

    def __iadd__(self, o):
        self.x += o
        self.y += o
        return self

    def __eq__(self, o):
        return isinstance(o, Vec) and (self.x, self.y) == (o.x, o.y)

    def __repr__(self):
        return f"Vec({self.x}, {self.y})"

    def d(self):
        return [self.x, self.y]


class Vec3(Vec):
    def __radd__(self, o):
        return "Vec3.__radd__ first"


class Stop:
    """`stop_a | stop_b` (tenacity's stop_base)"""

    def __init__(self, name):
        self.name = name

    def __or__(self, other):
        return Stop(f"({self.name}|{other.name})")

    def __and__(self, other):
        return Stop(f"({self.name}&{other.name})")


class Cfg:
    DEFAULT_TOLERANCE = 300
    PREFIX = "v"

    def check(self, value, tolerance=DEFAULT_TOLERANCE, label=PREFIX + "1"):
        return {"value": value, "tolerance": tolerance, "label": label, "ok": value <= tolerance}

    def __getstate__(self):  # only pickle and copy call it: ignored (the application uses neither)
        raise RuntimeError("never called")

    def __reduce__(self):
        raise RuntimeError("never called")


class Box(Generic[T]):
    def __init__(self, item: T):
        self.item = item

    def get(self) -> T:
        return self.item


class Named(Protocol):
    def name(self) -> str: ...


class Base:
    def kind(self):
        return "base"


class Child(Base):
    pass


class StrBox(Box["str"]):
    pass


class Typed(Box[int]):
    def twice(self):
        return self.item * 2


def dead_code(x):
    if x:
        return "yes"
    return "no"
    print(undefined_name)  # never runs: CPython compiles it, names unresolved included  # noqa: F821


@router.get("/loop/{n}")
async def loop(n: int, fail_at: int | None = None, stop: int | None = None):
    out = []
    try:
        for x in Countdown(n, fail_at):
            log.warning("body %s", x)
            out.append(x)
            if stop is not None and x == stop:
                break
        else:
            out.append("else")
    except ValueError as e:
        out.append(str(e))
    return {"out": out}


@router.get("/naturals/{n}")
async def naturals(n: int):
    got = []
    for i in Naturals():
        if i > n:
            break
        got.append(i)
    it = Naturals()
    return {"got": got, "next": [next(it), next(it)], "first_even": next(x for x in [1, 2, 3] if x % 2 == 0)}


@router.get("/consume/{n}")
async def consume(n: int):
    c = Countdown(n)
    return {"list": list(Countdown(n)), "sorted": sorted(Countdown(n)), "sum": sum(Countdown(n)),
            "comp": [x * 10 for x in Countdown(n) if x % 2], "max": max(Countdown(n)) if n else None,
            "tuple": tuple(c), "after": list(c), "join": "-".join(str(x) for x in Countdown(n)),
            "dict": {x: x * x for x in Countdown(n)}, "in": 2 in Countdown(n), "any": any(x > 2 for x in Countdown(n))}


@router.get("/bag")
async def bag(probe: str = "b"):
    b = Bag("a", "b", "c")
    b[0] = "A"
    del b[2]
    return {"len": len(b), "in": probe in b, "items": list(b), "first": b[0], "last": b[-1], "bool": bool(b),
            "empty": bool(Bag()), "not": not Bag(), "loop": [x for x in b], "sq": list(Squares(4)),
            "sq_in": 9 in Squares(4), "sq_out": 10 in Squares(4)}


@router.get("/bag/{i}")
async def bag_item(i: int):
    return {"v": Bag("a", "b")[i]}


@router.get("/truth")
async def truth():
    out = {"t": bool(Flag(True)), "f": bool(Flag(False)), "if": "yes" if Flag(False) else "no",
           "len0": bool(Sized(0)), "len3": bool(Sized(3)), "len": len(Sized(5)), "and": Flag(True) and "x",
           "or": Flag(False) or "y"}
    while Flag(False):
        out["never"] = True
    return out


@router.get("/truth/bad/{which}")
async def truth_bad(which: str):
    if which == "bool":
        return {"v": bool(Flag(1))}
    if which == "neg":
        return {"v": len(Sized(-1))}
    if which == "str":
        return {"v": len(Sized("3"))}
    if which == "iter":
        return {"v": list(NotIter())}
    if which == "noiter":
        return {"v": list(Flag(True))}
    if which == "nolen":
        return {"v": len(Flag(True))}
    if which == "noitem":
        return {"v": Flag(True)[0]}
    if which == "noset":
        f = Flag(True)
        f[0] = 1
    raise HTTPException(404)


@router.get("/vec")
async def vec(k: int = 2):
    a, b = Vec(1, 2), Vec(3, 5)
    c = a + b
    c += 10
    return {"add": (a + b).d(), "radd": (1 + a).d(), "addn": (a + 1).d(), "mul": (a * k).d(), "rmul": (k * a).d(),
            "sub": (b - a).d(), "neg": (-a).d(), "abs": abs(Vec(3, 4)), "iadd": c.d(), "eq": a == Vec(1, 2),
            "ne": a != b, "sum": sum([a, b], Vec(0, 0)).d(), "sub3": Vec(1, 1) + Vec3(2, 2),
            "repr": repr(a), "f": f"{a!r}"}


@router.get("/vec/bad/{which}")
async def vec_bad(which: str):
    a = Vec(1, 2)
    if which == "mulvec":
        return {"v": a * a}
    if which == "addstr":
        return {"v": a + "x"}
    if which == "raddstr":
        return {"v": "x" + a}
    if which == "nodiv":
        return {"v": a / 2}
    if which == "noinv":
        return {"v": ~a}
    raise HTTPException(404)


@router.get("/stop")
async def stop():
    s = Stop("a") | Stop("b") & Stop("c")
    return {"name": s.name}


@router.get("/misc/{v}")
async def misc(v: int):
    conf.calls += 1
    return {"check": Cfg().check(v), "check2": Cfg().check(v, 10), "dead": dead_code(v), "box": Box(v).get(),
            "typed": Typed(v).twice(), "strbox": StrBox("s").get(), "child": Child().kind(), "key": conf.current(), "greet": Greeter("x").hello(),
            "shout": tools.shout("hi")}


@router.post("/key/{k}")
async def set_key(k: str):
    old = conf.api_key
    conf.api_key = k
    return {"old": old, "new": conf.current(), "attr": conf.api_key}


@router.get("/flags/{s}")
async def flag_info(s: str):
    try:
        missing = flags.get({}, "x")
    except KeyError as e:
        missing = repr(e)
    return {**flags.info(s), "missing": missing}

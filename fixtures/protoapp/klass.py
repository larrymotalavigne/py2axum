"""Class protocols libraries use (marche A, refusals P2A0111): `metaclass=ABCMeta`, `@abstractmethod` without
ABCMeta, a version test in a class body, copy methods of a class nothing copies, `__str__`/`__repr__`/`__bool__` of
an exception class (validators' ValidationError)."""
import abc
import logging
import sys
from abc import ABCMeta, abstractmethod

from fastapi import APIRouter

router = APIRouter(prefix="/k")
log = logging.getLogger("protoapp")


class Shape(metaclass=ABCMeta):
    @abstractmethod
    def area(self):
        ...

    @abstractmethod
    def name(self):
        ...

    def describe(self):
        return f"{self.name()}:{self.area()}"


class Square(Shape):
    def __init__(self, s):
        self.s = s

    def area(self):
        return self.s * self.s

    def name(self):
        return "square"


class Half(Shape):
    """area still abstract"""

    def name(self):
        return "half"


class Loose:
    """@abstractmethod without ABCMeta: CPython instantiates it (only ABCMeta computes __abstractmethods__)"""

    @abstractmethod
    def run(self):
        return "ran"


class Versioned(metaclass=abc.ABCMeta):
    if (3, 8) <= sys.version_info < (3, 10):
        kind = "old"
    else:
        kind = "new"


class Stateful:
    """pickle/copy methods: nothing reached copies or pickles, the class is kept"""

    def __init__(self):
        self.x = 1

    def __getstate__(self):
        return {"x": self.x}

    def __setstate__(self, st):
        self.x = st["x"]


class Failure(Exception):
    def __init__(self, func, args, message=""):
        if message:
            self.reason = message
        self.func = func
        self.extra = args

    def __repr__(self):
        return f"Failure(func={self.func}, args={self.extra})"

    def __str__(self):
        return repr(self)

    def __bool__(self):
        return False


class SubFailure(Failure):
    """inherits __str__/__repr__/__bool__"""


class Plain(Exception):
    def __repr__(self):
        return "PlainRepr"


class Broken(Exception):
    def __str__(self):
        raise ValueError("no text")


class Wrong(Exception):
    def __str__(self):
        return 42


@router.get("/abc/square/{n}")
async def square(n: int):
    sq = Square(n)
    return {"d": sq.describe(), "is": isinstance(sq, Shape), "sub": issubclass(Square, Shape)}


@router.get("/abc/bad/{w}")
async def abstract(w: str):
    try:
        obj = Half() if w == "half" else Shape()
    except TypeError as e:
        return {"error": str(e)}
    return {"made": obj.name()}


@router.get("/abc/loose")
async def loose():
    return {"run": Loose().run()}


@router.get("/abc/versioned")
async def versioned():
    return {"cls": Versioned.kind, "inst": Versioned().kind}


@router.get("/stateful")
async def stateful():
    s = Stateful()
    s.x += 1
    return {"x": s.x}


@router.get("/exc/{w}")
async def exc(w: str):
    if w == "sub":
        f = SubFailure("check", {"a": 1})
    else:
        f = Failure("check", {"a": 1}, "bad" if w == "msg" else "")
    out = {"str": str(f), "repr": repr(f), "bool": bool(f), "fstr": f"<{f}>", "not": not f,
           "reason": getattr(f, "reason", None), "args": list(f.args), "pct": "%s" % (f,)}
    if f:
        out["truthy"] = True
    if w == "raise":
        raise f
    if w == "log":
        try:
            raise f
        except Failure:
            log.exception("caught %r", f)
    return out


@router.get("/exc/other/{w}")
async def exc_other(w: str):
    if w == "plain":
        p = Plain("a", 2)
        return {"str": str(p), "repr": repr(p), "bool": bool(p)}
    if w == "broken":
        return {"str": str(Broken("x"))}
    if w == "broken-raise":
        raise Broken("x")
    if w == "wrong":
        return {"str": str(Wrong())}
    raise Wrong()


def _inner(n):
    if n > 1:
        raise ValueError(f"too big {n}")
    return n


@router.get("/exc/tb/{n}")
async def exc_tb(n: int):
    """traceback.format_exc(): the handled exception's chain with its project frames"""
    import traceback

    outside = traceback.format_exc()
    try:
        try:
            _inner(n)
        except ValueError as e:
            raise Failure("tb", {"n": n}) from e
    except Failure:
        return {"outside": outside, "tb": traceback.format_exc()}
    return {"outside": outside}

"""Project dunder methods (__eq__, __str__, __repr__, __hash__ = None) and hashability, frozen models."""
from dataclasses import dataclass

from fastapi import APIRouter, HTTPException, Request
from typing import Annotated

from pydantic import BaseModel, ConfigDict, Field, ValidationError, field_validator

router = APIRouter(prefix="/dunders")


class Addr(BaseModel):
    street: str | None = None
    country: str | None = None
    lat: float | None = None

    __hash__ = None  # unhashable: __eq__ defined on a mutable model

    def __eq__(self, other):
        return self.street == other.street and (self.country == other.country or (self.country is None and other.country == ""))

    def __str__(self):
        return f"Addr<street='{self.street}',country='{self.country}'>"


class Plain(BaseModel):
    a: int = 1


class Frozen(BaseModel):
    model_config = ConfigDict(frozen=True)
    a: int
    b: str = "x"


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y

    def __eq__(self, other):
        return isinstance(other, Point) and self.x == other.x

    def __repr__(self):
        return f"Point({self.x}, {self.y})"


class Bag:
    def __init__(self, n):
        self.n = n


@dataclass
class Pair:
    a: int
    b: int = 0

    def __str__(self):
        return f"<{self.a}|{self.b}>"


@dataclass(frozen=True)
class Key:
    a: int


def _raise(e):
    raise e


def _try(f):
    try:
        return f()
    except Exception as e:  # noqa: BLE001
        return f"{type(e).__name__}: {e}"


@router.get("/eq")
async def eq():
    a1 = Addr(street="1 rue", country=None)
    a2 = Addr(street="1 rue", country="")
    a3 = Addr(street="2 rue", country="FR")
    p1, p2, p3 = Point(1, 2), Point(1, 5), Point(2, 2)
    return {
        "addr": [a1 == a2, a2 == a1, a1 != a3, a1 in [a3, a2], a2 in [a1], [a1, a2, a3].count(a2), a1 in [a1]],
        "index": [_try(lambda: [a3, a2].index(a1)), [a3, a1].index(a2), _try(lambda: [a3].remove(a1)), _try(lambda: [1].index(2))],
        "addr_none": _try(lambda: a1 == None),  # noqa: E711
        "str": [str(a1), f"{a3}", repr(a1)],
        "plain": [Plain() == Plain(), Plain(a=2) != Plain()],
        "points": [p1 == p2, p1 == p3, p1 != p2, repr([p1, p3]), str(p1), p1 in [p3, p2]],
        "bags": [Bag(1) == Bag(1), (b := Bag(1)) == b],
        "pair": [str(Pair(1, 2)), repr(Pair(1)), Pair(1) == Pair(1, 0), f"{Pair(3)!s}"],
    }


@router.get("/hash")
async def hashing():
    out = {}
    for name, f in [
        ("addr", lambda: {Addr(): 1}),
        ("plain", lambda: hash(Plain())),
        ("point", lambda: {Point(1, 1)}),
        ("pair", lambda: {Pair(1): 1}),
        ("bag", lambda: len({Bag(1), Bag(1)})),
        ("key", lambda: len({Key(1), Key(1), Key(2)})),
        ("frozen", lambda: len({Frozen(a=1), Frozen(a=1), Frozen(a=2)})),
    ]:
        out[name] = _try(f)
    return out


@router.get("/frozen")
async def frozen():
    f = Frozen(a=1)
    errs = []
    try:
        f.a = 2
    except ValidationError as e:
        errs.append(e.errors())
    try:
        Key(1).a = 3
    except Exception as e:  # noqa: BLE001
        errs.append(f"{type(e).__name__}: {e}")
    return {"f": f, "eq": f == Frozen(a=1, b="x"), "errs": errs}


class Defaults(BaseModel):
    """Field(validate_default=True) and field options given in Annotated[...] metadata."""
    quality: Annotated[str, Field(validate_default=True)] = ""
    rank: Annotated[int, Field(validate_default=True)] = "7"
    bad: Annotated[int, Field(validate_default=True)] = "x"
    plain: int = "9"
    nick: Annotated[str, Field(alias="nickName")] = "n"
    upper: Annotated[str, Field(validate_default=True)] = "abc"

    @field_validator("upper")
    @classmethod
    def up(cls, v):
        return v.upper()


@router.post("/vdefault")
async def vdefault(body: dict):
    try:
        d = Defaults(**body)
    except ValidationError as e:
        return {"errors": e.errors(include_url=False)}
    return {"d": d, "set": sorted(d.model_fields_set), "dump": d.model_dump(exclude_unset=True), "types": [type(d.rank).__name__, type(d.plain).__name__]}


@router.get("/jsonresp")
async def jsonresp(kind: str = "ok"):
    from datetime import datetime

    from fastapi.responses import JSONResponse

    content = {"ok": {"a": [1, 2.5, None, True], "é": "x"}, "model": Plain(), "dt": datetime(2026, 1, 1), "nan": float("nan")}[kind]
    return JSONResponse(content={"v": content})


class BaseHTTPError(HTTPException):
    """A business-error base class: class attributes read before super().__init__, overridden by subclasses."""
    message: str = ""
    field: str = ""
    status: str = "warning"
    status_code: int = 409
    info: str = ""
    teams: str = None

    def __init__(self, info=None, message=None, *args, **kwargs) -> None:
        self.args = args
        self.kwargs = kwargs
        if info is not None:
            self.info = info
        if message is not None:
            self.message = message
        self.teams = kwargs.get("teams", self.teams)
        detail = None if self.status_code in (204, 304) else {"message": self.message, "status": self.status, "info": self.info}
        super().__init__(status_code=self.status_code, detail=detail, headers=None)

    def summary(self):
        return f"{self.status}:{self.message}"

    @property
    def loud(self):
        return self.message.upper()


class Unavailable(BaseHTTPError):
    status_code = 504
    message = "service is unavailable"
    status = "error"


class Limited(BaseHTTPError):
    status_code = 403
    message = "limit reached"
    teams = "alerts"


class ProviderMissing(BaseHTTPError):
    status_code = 403
    info = "config problem"

    def __init__(self, provider: str = None, country: str = None) -> None:
        message = f"Provider {provider} is not available in {country}" if provider else f"Nothing in {country}"
        super().__init__(info=self.info, message=message)


@router.get("/raise/{kind}")
async def raise_kind(kind: str):
    if kind == "unavailable":
        raise Unavailable()
    if kind == "limited":
        raise Limited(info="x", teams="other")
    if kind == "provider":
        raise ProviderMissing("arcgis", "FR")
    if kind == "base":
        raise BaseHTTPError("i", "m", 1, 2, extra=3)
    e = Limited()
    return {"cls": [Unavailable.status_code, Unavailable.message, Limited.teams, BaseHTTPError.status],
            "inst": [e.status_code, e.teams, e.summary(), e.loud, e.args, e.kwargs, e.detail],
            "caught": _try(lambda: _raise(Unavailable()))}


@router.get("/builtins")
async def builtins_route():
    import logging
    import math
    from datetime import datetime

    logging.warning("root warning %s", 1)
    xs = [3, 0, 5, None, 2]
    return {
        "math": [round(math.radians(180), 12), math.degrees(math.pi), round(math.sin(1), 15), math.cos(0), round(math.atan2(1, 2), 15),
                 round(math.sqrt(2), 15), math.hypot(3, 4), math.copysign(2, -0.0), math.isfinite(1.0), _try(lambda: math.asin(2))],
        "today": type(datetime.today()).__name__,
        "filter": [list(filter(None, xs)), list(filter(lambda x: x is not None and x > 1, xs))],
        "map": [list(map(str, [1, 2])), list(map(lambda a, b: a + b, [1, 2, 3], [10, 20]))],
        "id": [id(xs) == id(xs), id(xs) == id(list(xs))],
    }


@router.get("/smallbatch")
async def smallbatch():
    from datetime import datetime as _dt, date as _date, time as _time

    table = str.maketrans({"-": " ", ".": None})
    t2 = str.maketrans("ab", "xy", "c")
    cols = ["a", "b"]
    return {
        "translate": ["28 av.-Edouard".translate(table), "abcabc".translate(t2)],
        "combine": [repr(_dt.combine(_date(2026, 1, 2), _time.min)), repr(_dt.combine(_date(2026, 1, 2), _time(5, 6)))],
        "dump": [Frozen(a=1).model_dump(exclude={"b"}), Frozen(a=1).model_dump(include=["b"]), Item0(id=1).model_dump(mode="json", exclude=("tags",))],
        "list": list({1, 2} - {2}),
        "cols": len(cols),
    }


class Item0(BaseModel):
    id: int
    tags: list[str] = []


@router.post("/proxied/{name}")
async def proxied(name: str, request: Request, q: str = ""):
    """Declared --python-side in the conformance build: the binary relays it to the Python app."""
    return {"name": name, "q": q, "body": (await request.body()).decode(), "ua": request.headers.get("x-probe")}

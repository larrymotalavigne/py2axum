"""`Model.model_construct(...)` (no validation: aliases, validation aliases, defaults, `_fields_set`, extras,
private attributes, computed fields, an instance returned under a response_model), plain classes holding
functions as `staticmethod(...)` class attributes, and names reached through `from module import *` (imported ones
included, `__all__` honoured)."""
import json
import time
from collections import Counter
from datetime import date

from fastapi import APIRouter
from pydantic import AliasChoices, AliasPath, BaseModel, ConfigDict, Field, PrivateAttr, computed_field

from .starexp import *  # noqa: F403 (HTTPException comes from fastapi through it)
from .starexp_all import *  # noqa: F403 (only what its __all__ lists)

router = APIRouter(prefix="/construct")


class Point(BaseModel):
    x: float
    y: float = 0.0
    label: str | None = None


class Track(BaseModel):
    name: str
    points: list[Point] = Field(default_factory=list)
    tags: list[str] = []
    rank: int = Field(default=1, alias="Rank")
    code: str = Field(default="-", validation_alias=AliasChoices("code", "short_code"))
    city: str = Field(default="?", validation_alias=AliasPath("place", "city"))
    kind: str = "plain"
    _hits: int = PrivateAttr(default=0)

    @computed_field
    @property
    def size(self) -> int:
        return len(self.points)


class Open(BaseModel):
    model_config = ConfigDict(extra="allow")
    a: int = 0


class Closed(BaseModel):
    model_config = ConfigDict(extra="forbid")
    a: int = 0


class Derived(BaseModel):
    a: int = 2
    b: int = Field(default_factory=lambda data: data["a"] * 10)
    when: date = date(2024, 1, 31)


class Child(Track):
    level: int = 3


def _view(m: BaseModel) -> dict:
    return {"dump": m.model_dump(), "set": sorted(m.model_fields_set), "json": m.model_dump_json()}


@router.get("/basic")
async def basic():
    t = Track.model_construct(name="north", tags=["a"], unknown=1)
    return {**_view(t), "alias": t.model_dump(by_alias=True), "unset": t.model_dump(exclude_unset=True),
            "extra": t.model_extra, "hits": t._hits, "eq": t == Track.model_construct(name="north", tags=["a"])}


@router.get("/aliases")
async def aliases():
    a = Track.model_construct(name="a", Rank=7, rank=99, short_code="sc", place={"city": "Lyon", "zip": 1})
    b = Track.model_construct(name="b", code="c1", short_code="c2", city="ignored")
    c = Track.model_construct(name="c", rank=5)
    return {"a": _view(a), "b": _view(b), "c": _view(c)}


@router.get("/fields-set")
async def fields_set():
    t = Track.model_construct({"name", "kind"}, name="n", tags=["x"])
    u = Track.model_construct(_fields_set=set(), name="m")
    return {"t": _view(t), "u": _view(u), "t_unset": t.model_dump(exclude_unset=True)}


@router.get("/extra")
async def extra():
    o = Open.model_construct(a=1, q=2, r=[1])
    c = Closed.model_construct(a=3, q=4)
    return {"open": _view(o), "open_extra": o.model_extra, "q": o.q, "closed": _view(c)}


@router.get("/defaults")
async def defaults():
    return {"given": _view(Derived.model_construct(a=5)), "none": _view(Derived.model_construct()),
            "b": _view(Derived.model_construct(b=1))}


@router.get("/nested")
async def nested():
    pts = [Point.model_construct(x=1, label="a"), Point.model_construct(x=2.5, y=1)]
    t = Track.model_construct(name="n", points=pts)
    return {**_view(t), "first": t.points[0].x, "copy": t.model_copy(update={"kind": "copy"}).model_dump()}


@router.get("/private")
async def private():
    t = Track.model_construct(name="p", _hits=4)
    t._hits += 1
    return {"hits": t._hits, "dump": t.model_dump()}


@router.get("/inherited")
async def inherited():
    c = Child.model_construct(name="k", level=9)
    return {**_view(c), "is_track": isinstance(c, Track), "cls": type(c).__name__}


@router.get("/on-instance")
async def on_instance():
    t = Track.model_construct(name="i")
    u = t.model_construct(name="j", kind="k")
    return {"u": _view(u), "same_class": type(u) is Track}


@router.get("/spread")
async def spread(name: str = "s", kind: str = "k"):
    raw = {"name": name, "kind": kind, "tags": [name, kind]}
    return _view(Track.model_construct(**raw))


@router.get("/response", response_model=Track)
async def response():
    return Track.model_construct(name="r", points=[Point.model_construct(x=1.5)], tags=["t"])


@router.get("/response-unset", response_model=Track, response_model_exclude_unset=True)
async def response_unset():
    return Track.model_construct(name="u", kind="given")


@router.get("/list", response_model=list[Point])
async def points():
    return [Point.model_construct(x=float(i), y=i * 2.0) for i in range(3)]


# ---------------------------------------------------------------- functions as class attributes


def pair(**kw):
    return sorted(kw.items())


def scaled(x, factor=2):
    return x * factor


class Factory:
    build_impl = staticmethod(pair)
    dumps = staticmethod(json.dumps)
    scale = staticmethod(scaled)
    clock = staticmethod(time.time)
    counter_type = Counter

    def __init__(self, **params):
        self.params = params
        self.made = []

    def build(self):
        out = self.build_impl(**self.params)
        self.made.append(out)
        return out

    def text(self):
        return self.dumps(sorted(self.params))


class Reverse(Factory):
    build_impl = staticmethod(lambda **kw: sorted(kw, reverse=True))


FACTORY = Factory(b=2, a=1)
REVERSE = Reverse(b=2, a=1, c=3)


@router.get("/static")
async def static():
    return {
        "build": FACTORY.build(),
        "reverse": REVERSE.build(),
        "text": REVERSE.text(),
        "scale": [FACTORY.scale(3), Factory.scale(4, factor=3), Reverse.scale(5)],
        "clock": isinstance(Factory.clock(), float) and FACTORY.clock() > 0,
        "counter": dict(FACTORY.counter_type("aab")),
        "made": len(FACTORY.made) > 0,
    }


# ---------------------------------------------------------------- `from module import *`


@router.get("/star/{code}")
async def star(code: int):
    if code >= 400:
        raise HTTPException(status_code=code, detail=STAR_DETAIL)  # noqa: F405
    return {"exported": exported(), "limit": STAR_LIMIT}  # noqa: F405

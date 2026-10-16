"""`model_post_init(self, context)`: called once the fields are valid, before the after validators, with context
None; a ValueError or an AssertionError there is a validation error of the model, anything else propagates. Private
attributes whose default is computed (`PrivateAttr(default_factory=datetime.now)`) are set just before it."""
from datetime import datetime
from typing import Any

from fastapi import APIRouter
from pydantic import BaseModel, PrivateAttr, ValidationError, field_validator, model_validator

router = APIRouter(prefix="/postx")
CALLS: list = []


def stamp() -> str:
    CALLS.append("stamp")
    return f"s{len(CALLS)}"


class Doubled(BaseModel):
    x: int
    _y: int = 0
    _log: list = PrivateAttr(default_factory=list)
    _seen: str = PrivateAttr(default_factory=stamp)
    _at: datetime = PrivateAttr(default_factory=datetime.now)

    @field_validator("x")
    @classmethod
    def positive(cls, v: int) -> int:
        CALLS.append(f"field {v}")
        if v < 0:
            raise ValueError("negative")
        return v

    def model_post_init(self, context: Any) -> None:
        CALLS.append(f"post {self.x} {self._y} {self._seen} {context!r} {type(self._at).__name__}")
        if self.x == 0:
            raise ValueError("zero")
        if self.x == 1:
            assert self.x != 1, "one"
        if self.x == 2:
            raise KeyError("two")
        self._y = self.x * 2
        self._log.append(self.x)

    @model_validator(mode="after")
    def after(self):
        CALLS.append(f"after {self._y}")
        return self


class Child(Doubled):
    z: str = "z"


class Outer(BaseModel):
    inner: Doubled
    items: list[Child] = []


class OnlyFactory(BaseModel):
    n: int
    _made: str = PrivateAttr(default_factory=stamp)


@router.post("/doubled")
def doubled(item: Doubled):
    out = {"y": item._y, "log": item._log, "seen": item._seen, "dump": item.model_dump(), "calls": list(CALLS)}
    CALLS.clear()
    return out


@router.post("/outer")
def outer(item: Outer):
    out = {"inner": item.inner._y, "items": [(c._y, c.z, c._seen) for c in item.items], "calls": list(CALLS)}
    CALLS.clear()
    return out


@router.post("/direct")
def direct(body: dict):
    try:
        m = Child(**body)
        out = {"y": m._y, "seen": m._seen, "json": m.model_dump_json()}
    except ValidationError as e:
        out = {"errors": e.errors(include_url=False)}
    out["calls"] = list(CALLS)
    CALLS.clear()
    return out


@router.post("/only")
def only(item: OnlyFactory):
    a = OnlyFactory.model_validate({"n": 3})
    out = {"made": item._made, "other": a._made, "calls": list(CALLS)}
    CALLS.clear()
    return out

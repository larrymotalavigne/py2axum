"""`pydantic.Json` and `Json[T]`: the str or bytes input parsed by jiter (pydantic-core's parser: its error messages
and positions), then validated as T."""
from typing import Any

from fastapi import APIRouter
from pydantic import BaseModel, Json, ValidationError

router = APIRouter(prefix="/jsonx")


class Raw(BaseModel):
    x: Json


class Ints(BaseModel):
    x: Json[list[int]]


class Typed(BaseModel):
    x: Json[dict[str, Any]] | None = None
    y: Json[int] = "0"


@router.post("/raw")
def raw(item: Raw) -> Raw:
    return item


@router.post("/ints")
def ints(item: Ints) -> Ints:
    return item


@router.post("/typed")
def typed(item: Typed):
    return {"dump": item.model_dump(), "json": item.model_dump_json()}


@router.post("/direct")
def direct(body: dict):
    try:
        v = body["v"].encode() if body.get("bytes") else body["v"]
        return {"x": Raw(x=v).x}
    except ValidationError as e:
        return {"errors": e.errors(include_url=False), "str": str(e)}

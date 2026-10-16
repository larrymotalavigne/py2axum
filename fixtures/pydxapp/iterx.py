"""`Iterable[T]` fields: pydantic-core's ValidatorIterator (any iterable accepted, items validated when reached:
next(), a loop, list(), model_dump(mode="json"), model_dump_json() and the response)."""
from collections.abc import Iterable
from typing import Iterable as TIterable, Optional

from pydantic import BaseModel, ConfigDict, Field, TypeAdapter, ValidationError
from fastapi import APIRouter

router = APIRouter(prefix="/iterx")


class Names(BaseModel):
    f: Iterable[str]


class Ints(BaseModel):
    y: TIterable[int]


class Maybe(BaseModel):
    f: Optional[Iterable[int]] = None
    tag: str = "t"


class StrictInts(BaseModel):
    model_config = ConfigDict(strict=True)
    y: Iterable[int]


class Upper(BaseModel):
    model_config = ConfigDict(str_to_upper=True)
    f: Iterable[str] = Field(description="upper-cased items")


class Nested(BaseModel):
    rows: Iterable[Ints]


def _err(e: Exception) -> dict:
    if isinstance(e, ValidationError):
        return {"err": type(e).__name__, "title": e.title, "msg": str(e), "errors": e.errors(include_url=False)}
    return {"err": type(e).__name__, "msg": str(e)}


@router.post("/names")
def names(item: Names) -> Names:
    return item


@router.post("/ints")
def ints(item: Ints) -> Ints:
    return item


@router.post("/maybe")
def maybe(item: Maybe) -> Maybe:
    return item


@router.post("/strict")
def strict(item: StrictInts) -> StrictInts:
    return item


@router.post("/upper")
def upper(item: Upper) -> Upper:
    return item


@router.post("/nested")
def nested(item: Nested) -> Nested:
    return item


@router.post("/plain")
def plain(item: Ints):
    """no response model: jsonable_encoder dumps the model in JSON mode"""
    return item


@router.post("/use")
def use(body: dict):
    how = body.get("how")
    try:
        m = Ints(y=body["y"])
        if how == "next":
            return {"out": [next(m.y), next(m.y)], "type": type(m.y).__name__}
        if how == "next_default":
            return {"out": [next(m.y, "end") for _ in range(4)]}
        if how == "stop":
            out = []
            while True:
                try:
                    out.append(next(m.y))
                except StopIteration:
                    return {"out": out}
        if how == "loop":
            out = []
            for v in m.y:
                out.append(v * 2)
            return {"out": out}
        if how == "list":
            return {"out": list(m.y), "again": list(m.y)}
        if how == "dump_json_mode":
            return {"out": m.model_dump(mode="json"), "again": m.model_dump(mode="json")}
        if how == "dump_json":
            return {"out": m.model_dump_json(), "again": m.model_dump_json()}
        if how == "adapter":
            return {"out": list(TypeAdapter(Iterable[int]).validate_python(body["y"]))}
        if how == "strict":
            return {"out": list(StrictInts(y=body["y"]).y)}
        if how == "partial":
            first = next(m.y)
            return {"first": first, "rest": m.model_dump(mode="json")}
        return {"how": how}
    except (ValidationError, TypeError, ValueError) as e:
        return _err(e)


@router.get("/schema")
def schema():
    return {"names": Names.model_json_schema(), "maybe": Maybe.model_json_schema()}

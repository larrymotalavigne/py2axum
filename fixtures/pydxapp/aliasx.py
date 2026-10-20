"""Alias generators written as project functions of several statements, a generator taken from another model's
config (`Other.model_config["alias_generator"]`), a config spread from another model's (`**Base.model_config`),
`hide_input_in_errors` (str() of the ValidationError only: errors() and the 422 keep the input), and
pydantic-settings' `env_ignore_empty`."""
from typing import Any

from fastapi import APIRouter
from pydantic import AliasGenerator, BaseModel, ConfigDict, Field, ValidationError
from pydantic.alias_generators import to_camel as lib_camel

router = APIRouter(prefix="/aliasx")


def to_camel(snake: str) -> str:
    """snake_case -> camelCase (an `if`, a starred unpacking and a generator)"""
    if not snake or '_' not in snake:
        return snake
    first, *rest = snake.split('_')
    return first + ''.join(word.capitalize() for word in rest)


def dashed(name: str) -> str:
    out = []
    for i, part in enumerate(name.split("_")):
        if not part:
            continue
        if i == 0 and part.startswith("x"):
            out.append(part.upper())
            continue
        out.append(part)
    return "-".join(out) or name


class CamelModel(BaseModel):
    model_config = ConfigDict(alias_generator=to_camel, populate_by_name=True, from_attributes=True)


class TaskIn(CamelModel):
    task_name: str = Field(min_length=1, max_length=20)
    cron_expression: str
    config_json: dict[str, Any] | str = Field(default_factory=dict)
    account_id: int = Field(gt=0)
    enabled: bool = True
    a__b: int = 0

    model_config = ConfigDict(alias_generator=CamelModel.model_config["alias_generator"], populate_by_name=True,
                              extra="forbid")


class Dashed(BaseModel):
    model_config = ConfigDict(alias_generator=dashed)

    x_ray_id: int
    long_field_name: str = "d"
    plain: bool = False


class Base(BaseModel):
    model_config = ConfigDict(alias_generator=AliasGenerator(alias=lib_camel), populate_by_name=True)


class BaseResponse(Base):
    model_config = ConfigDict(**(Base.model_config or {}), from_attributes=True)


class Item(BaseResponse):
    item_id: int
    display_name: str


class Strict(Base):
    model_config = {**Base.model_config, "extra": "forbid"}

    page_size: int = 10


class Secret(BaseModel):
    model_config = ConfigDict(hide_input_in_errors=True)

    pass_code: int
    nested_list: list[int] = []


class Inner(BaseModel):
    value: int


class HiddenOuter(BaseModel):
    model_config = ConfigDict(hide_input_in_errors=True)

    inner: Inner


class ShownOuter(BaseModel):
    secret: Secret


class HiddenChild(Secret):
    extra_field: str = "e"


@router.post("/task")
async def task(body: TaskIn):
    return {"dump": body.model_dump(), "alias": body.model_dump(by_alias=True), "model": body}


@router.post("/task/raw")
async def task_raw(body: dict):
    try:
        t = TaskIn.model_validate(body)
    except ValidationError as e:
        return {"str": str(e), "errors": e.errors(include_url=False)}
    return t.model_dump(by_alias=True)


@router.post("/dashed", response_model=Dashed)
async def dashed_route(body: Dashed):
    return body


@router.post("/item", response_model=Item)
async def item(body: Item):
    return body


@router.post("/item/dump")
async def item_dump(body: Item):
    return [body.model_dump(), body.model_dump(by_alias=True)]


@router.post("/strict")
async def strict(body: Strict):
    return body.model_dump(by_alias=True)


@router.post("/secret")
async def secret(body: Secret):
    return body


@router.post("/hidden/{which}")
async def hidden(which: str, body: dict):
    cls = {"secret": Secret, "outer": HiddenOuter, "shown": ShownOuter, "child": HiddenChild}[which]
    try:
        cls.model_validate(body)
    except ValidationError as e:
        return {"str": str(e), "repr": repr(e), "errors": e.errors(include_url=False)}
    return {"ok": True}



# ---- model-level allow_inf_nan=False (every float of the model, in containers too) and validate_default=False
class Finite(BaseModel):
    model_config = ConfigDict(extra="forbid", allow_inf_nan=False, validate_default=False)

    x: float
    xs: list[float] = []
    y: float = Field(default=0, ge=0)
    inner: Inner | None = None


class FiniteChild(Finite):
    z: dict[str, float] = {}


@router.post("/finite")
async def finite(body: FiniteChild):
    return body


# ---- pydantic-settings env_ignore_empty (PYDX_* exported by scripts_start_pydxapp.sh)
from pydantic_settings import BaseSettings, SettingsConfigDict  # noqa: E402


class IgnoreEmpty(BaseSettings):
    model_config = SettingsConfigDict(env_prefix="PYDX_", env_ignore_empty=True)

    EMPTY_STR: str = "dflt"
    EMPTY_INT: int = 3
    SET_STR: str = "d"


class KeepEmpty(BaseSettings):
    model_config = SettingsConfigDict(env_prefix="PYDX_")

    EMPTY_STR: str = "dflt"
    SET_STR: str = "d"
    EMPTY_INT: int = 3


class IgnoreEmptyRequired(IgnoreEmpty):
    EMPTY_REQ: int


@router.get("/settings/{which}")
async def settings_route(which: str):
    cls = {"ignore": IgnoreEmpty, "keep": KeepEmpty, "required": IgnoreEmptyRequired}[which]
    try:
        return cls().model_dump()
    except ValidationError as e:
        return {"str": str(e), "errors": e.errors(include_url=False)}

"""A stdlib dataclass inside a model is validated with the model's config (pydantic pushes no config of its own):
str_to_lower, str_strip_whitespace, str_max_length, use_enum_values reach its fields, nested dataclasses and
containers included; `@with_config(...)` gives it its own config instead; a pydantic dataclass keeps its own."""
import enum
from dataclasses import dataclass, field

import pydantic
from fastapi import APIRouter
from pydantic import BaseModel, ConfigDict, with_config

router = APIRouter(prefix="/cfgx")


class Color(enum.Enum):
    RED = "red"
    BLUE = "blue"


@dataclass
class UserWithoutConfig:
    name: str


@dataclass
@with_config(str_to_lower=False)
class UserWithConfig:
    name: str


@dataclass
class Tagged:
    label: str
    tags: list[str] = field(default_factory=list)
    color: Color = Color.RED
    owner: UserWithoutConfig | None = None


@dataclass
@with_config(ConfigDict(str_strip_whitespace=True))
class Stripped:
    name: str


@pydantic.dataclasses.dataclass
class PydUser:
    name: str


class Parent(BaseModel):
    user_1: UserWithoutConfig
    user_2: UserWithConfig

    model_config = ConfigDict(str_to_lower=True)


class Rich(BaseModel):
    model_config = ConfigDict(str_to_upper=True, str_max_length=6, use_enum_values=True)
    items: list[Tagged] = []
    one: Tagged | None = None
    strip: Stripped | None = None
    pyd: PydUser | None = None


@router.post("/parent")
def parent(item: Parent) -> Parent:
    return item


@router.post("/rich")
def rich(item: Rich):
    return {"repr": repr(item), "dump": item.model_dump(mode="json")}


@router.post("/plain")
def plain(item: UserWithoutConfig) -> UserWithoutConfig:
    return item

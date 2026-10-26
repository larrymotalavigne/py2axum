"""Patterns of public FastAPI applications (Netflix Dispatch, the full-stack template...): enums deriving from a
common enum without members, pydantic `Field()` extra keywords (json_schema_extra), printed warnings."""
import warnings
from enum import StrEnum

from fastapi import APIRouter
from pydantic import BaseModel, Field

router = APIRouter(prefix="/pub")


class AppEnum(StrEnum):
    """common base of the application's enums (Dispatch's DispatchEnum)"""

    pass


class Visibility(AppEnum):
    open = "Open"
    restricted = "Restricted"


class Thing(BaseModel):
    visibility: Visibility = Visibility.open
    enabled: bool | None = Field(True, nullable=True)
    name: str = Field(..., min_length=1, nullable=False)


@router.post("/things")
async def things(thing: Thing):
    return {"thing": thing, "value": thing.visibility.value, "str": str(thing.visibility),
            "is_str": isinstance(thing.visibility, str), "members": [v.value for v in Visibility]}


@router.get("/warn")
async def warn(n: int = 0):
    warnings.warn(f"careful {n}")  # noqa: B028  (the default stacklevel)
    warnings.warn("once", RuntimeWarning, stacklevel=1)
    return {"n": n}

"""Enum columns read back into schemas (an ORM object through `response_model`, `from_attributes`): pydantic-core
takes a str/int subclass member as its value, a plain member as `str(value)` in a `str` field."""
from typing import Literal

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, ConfigDict, Field
from sqlalchemy import select

from .db import DbDep
from .enums import Channel, Level, Priority, Status
from .models import EnumRow

router = APIRouter(prefix="/enumcols")


class AsStr(BaseModel):
    """The shape that broke: `status: str` over a (str, Enum) column."""
    model_config = ConfigDict(from_attributes=True)
    id: int
    pri: str
    state: str
    chan: str
    lvl: str
    pri_val: str
    maybe: str | None


class AsEnum(BaseModel):
    model_config = ConfigDict(from_attributes=True)
    pri: Priority
    state: Status
    chan: Channel
    lvl: Level
    pri_val: Priority
    maybe: Priority | None


class AsUse(AsEnum):
    model_config = ConfigDict(from_attributes=True, use_enum_values=True)


class AsLiteral(BaseModel):
    model_config = ConfigDict(from_attributes=True)
    pri: Literal["low", "medium", "high"]
    chan: Literal["mail", "sms"]
    lvl: Literal[1, 2]
    maybe: Literal["low", "medium", "high"] | None


class AsNumbers(BaseModel):
    model_config = ConfigDict(from_attributes=True)
    lvl: int
    lvl_f: float
    lvl_b: bool
    state_i: int


class Short(BaseModel):
    model_config = ConfigDict(from_attributes=True)
    pri: str = Field(max_length=3)


VIEWS = {"str": AsStr, "enum": AsEnum, "use": AsUse, "literal": AsLiteral}


class NewRow(BaseModel):
    pri: Priority
    state: Status
    chan: Channel
    lvl: Level
    pri_val: Priority
    maybe: Priority | None = None


@router.post("")
async def create(body: NewRow, db: DbDep):
    row = EnumRow(**body.model_dump())
    db.add(row)
    await db.commit()
    return {"id": row.id}


async def _row(db, row_id: int) -> EnumRow:
    row = (await db.execute(select(EnumRow).where(EnumRow.id == row_id))).scalar_one_or_none()
    if row is None:
        raise HTTPException(404)
    return row


@router.get("/{row_id}/str", response_model=AsStr)
async def as_str(row_id: int, db: DbDep):
    return await _row(db, row_id)


@router.get("/{row_id}/enum", response_model=AsEnum)
async def as_enum(row_id: int, db: DbDep):
    return await _row(db, row_id)


@router.get("/{row_id}/use", response_model=AsUse)
async def as_use(row_id: int, db: DbDep):
    return await _row(db, row_id)


@router.get("/{row_id}/literal", response_model=AsLiteral)
async def as_literal(row_id: int, db: DbDep):
    return await _row(db, row_id)


@router.get("/{row_id}/numbers", response_model=AsNumbers)
async def as_numbers(row_id: int, db: DbDep):
    row = await _row(db, row_id)
    return {"lvl": row.lvl, "lvl_f": row.lvl, "lvl_b": row.lvl, "state_i": row.state}


@router.get("/{row_id}/short", response_model=Short)
async def as_short(row_id: int, db: DbDep):
    return await _row(db, row_id)


@router.get("/{row_id}/validate")
async def validate_direct(row_id: int, db: DbDep):
    """model_validate / constructor with the members, and the errors' input (the member, not its value)."""
    row = await _row(db, row_id)
    out = {name: cls.model_validate(row).model_dump(mode="json") for name, cls in VIEWS.items()}
    out["dump_python"] = {k: repr(v) for k, v in AsStr.model_validate(row).model_dump().items()}
    for cls, data in ((AsNumbers, {"lvl": Level.ONE, "lvl_f": Priority.LOW, "lvl_b": Status.OPEN, "state_i": Status.DONE}),
                      (Short, {"pri": row.pri})):
        try:
            out[cls.__name__] = cls(**data).model_dump(mode="json")
        except Exception as e:
            out[cls.__name__] = [[x["type"], x["loc"], x["msg"], repr(x["input"])] for x in e.errors()]
    return out


@router.get("/raw")
async def raw(db: DbDep):
    """What PostgreSQL holds: names or values."""
    from sqlalchemy import text
    rows = (await db.execute(text("SELECT pri::text, state::text, chan, lvl::text, pri_val::text, maybe::text FROM enum_rows ORDER BY id"))).all()
    return [list(r) for r in rows]

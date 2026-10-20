"""SQLModel classes without a table, the base of gear.py's table (fields inherited from another module), one of
them an Enum (SQLModel's sa_Enum column)."""
from enum import Enum

from sqlmodel import Field, SQLModel


class Kind(str, Enum):
    tool = "tool"
    weapon = "weapon"


class Named(SQLModel):
    name: str = Field(index=True, max_length=40)


class GearBase(Named):
    kind: Kind = Field(default=Kind.tool)
    weight: float | None = None

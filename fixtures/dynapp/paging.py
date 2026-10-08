"""A Pydantic model used as a class dependency (`p: PageParams = Depends()`), in a module that imports neither
Annotated nor Query."""
import enum

from pydantic import BaseModel, Field, field_validator


class PageParams(BaseModel):
    """Pagination parameters for list endpoints."""

    page: int = Field(default=1, ge=1, description="Page number")
    page_size: int = Field(20, ge=1, le=100)
    q: str | None = Field(None, max_length=5)
    exact: bool = False


class Tagged(BaseModel):
    """model_validate overridden, then BaseModel's through `super(cls, cls)` (a common pattern)."""

    model_config = {"from_attributes": True}

    id: int
    label: str

    @classmethod
    def model_validate(cls, obj, **kwargs):
        data = {"id": obj["id"], "label": f"#{obj['id']} {obj['name']}"} if isinstance(obj, dict) else obj
        return super(cls, cls).model_validate(data, **kwargs)


class Named(BaseModel):
    id: int
    name: str

    @classmethod
    def model_validate(cls, obj, **kwargs):
        return super().model_validate({**obj, "name": obj["name"].strip()}, **kwargs)


class LoudNamed(Named):
    """A project parent's override through `super()`: `cls` stays LoudNamed."""

    @classmethod
    def model_validate(cls, obj, **kwargs):
        return super().model_validate({**obj, "name": obj["name"].upper()}, **kwargs)


class Cond(enum.Enum):
    SENDER = "SENDER_CONTAINS"
    SIZE = 1
    NONE = None


class AsText(BaseModel):
    """A plain Enum member given to a str field: lax mode keeps str(member.value) (a response schema typed str for an Enum column)."""

    kind: str
    short: str = Field("x", max_length=3)


class ArchiveOut(BaseModel):
    """Its own __init__ and a `mode="before"` validator: validated from an ORM object (from_attributes),
    pydantic-core reads the attributes and does not call __init__."""

    model_config = {"from_attributes": True}

    id: int
    name: str
    tags: list[str] = []
    note: str = "set by validation"

    def __init__(self, **data):
        super().__init__(**data)
        self.note = "set by __init__"

    @field_validator("name", mode="before")
    @classmethod
    def strip_name(cls, v):
        return v.strip() if isinstance(v, str) else v

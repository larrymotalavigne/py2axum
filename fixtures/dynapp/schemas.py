from datetime import date, datetime
from typing import Annotated, Literal, Optional

from fastapi import HTTPException
from pydantic import BaseModel, ConfigDict, EmailStr, Field, ValidationInfo, field_validator, model_validator, validator

from .enums import Channel, Level, Priority, Status


class TaskIn(BaseModel):
    title: str = Field(min_length=1, max_length=100)
    priority: Priority = Priority.MEDIUM
    tags: list[str] = Field(default_factory=list)
    channel: Channel | None = None
    level: Level = Level.ONE
    price: float | None = None


class TaskOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    title: str
    priority: Priority
    status: Status
    tags: list
    created_at: datetime
    revision: str
    updated_at: datetime | None
    price: float | None


class TaskSummary(BaseModel):
    model_config = ConfigDict(use_enum_values=True)

    title: str
    priority: Priority
    received_at: datetime = Field(default_factory=datetime.now)


class StatusChange(BaseModel):
    status: Status


class Strict(BaseModel):
    model_config = {"from_attributes": True, "str_strip_whitespace": True, "validate_assignment": True}


class Contact(Strict):
    name: str = Field(min_length=1)
    email: EmailStr


class ContactV1(BaseModel):
    name: str

    class Config:
        orm_mode = True  # v1 keys: Pydantic v2 warns and ignores them
        anystr_strip_whitespace = True
        str_to_upper = True  # v2 key in a v1-style class Config: applied


class TaskBrief(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    title: str
    project_id: int | None


class OwnerOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    name: str
    projects: list[str] = Field(default_factory=list)


class ProjectOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    name: str
    owner: OwnerOut | None
    tasks: list[TaskBrief]


class ProjectIn(BaseModel):
    name: str
    owner: str | None = None
    tasks: list[str] = Field(default_factory=list)


class Person(BaseModel):
    model_config = ConfigDict(populate_by_name=True, validate_assignment=True)

    full_name: str = Field(alias="fullName", min_length=2)
    tags: list[str] = Field(default_factory=list)
    nick: str | None = None


class TaskTitle(BaseModel):
    title: str


class Slot(BaseModel):
    """@field_validator: a ValueError/AssertionError is a 422 at the field, in pydantic-core's order."""
    start: int
    label: str
    room: str = "A"
    seats: int = 0  # the default is never validated (7 would be refused)
    stamp: datetime = Field(default_factory=datetime.now)

    @field_validator("label", "seats")
    @classmethod
    def not_reserved(cls, v):
        if v in ("admin", 7):
            raise ValueError(f"{v} is reserved")
        return v

    @field_validator("label")
    @classmethod
    def shout(cls, v):
        assert v != "quiet", "too quiet"
        assert v != "hush"
        return v.upper()

    @field_validator("room")
    @classmethod
    def room_known(cls, v):
        if v == "teapot":
            raise HTTPException(status_code=418, detail="teapot")
        return v.lower()


class NamedSlot(Slot):
    name: str

    @field_validator("name")
    @classmethod
    def no_digits(cls, v):
        if any(c.isdigit() for c in v):
            raise ValueError("digits")
        return v.title()


class Span(BaseModel):
    """`info.data` / v1 `values`: the earlier fields that passed (after their validators), defaults included."""
    start: int
    kind: str = "day"
    end: int
    seen: str = ""
    note: str | None = None

    @field_validator("start")
    @classmethod
    def start_positive(cls, v):
        if v < 0:
            raise ValueError("negative start")
        return v * 10

    @field_validator("end")
    @classmethod
    def after_start(cls, v, info: ValidationInfo):
        start = info.data.get("start")
        if start is not None and v * 10 < start:
            raise ValueError(f"{info.field_name} before start")
        return v

    @validator("seen")
    def seen_values(cls, v, values):
        return v + ":" + ",".join(f"{k}={values[k]}" for k in values)

    @field_validator("note")
    def note_info(cls, v, info):
        return f"{v}|{sorted(info.data)}|{info.field_name}|{type(info.data).__name__}"


class Agenda(BaseModel):
    spans: list[Span]
    title: str


class Booking(BaseModel):
    slots: list[NamedSlot]
    total: int


class Point(BaseModel):
    x: int


class Point3(BaseModel):
    x: int
    z: int = 0


class Label(BaseModel):
    x: str


class Mixed(BaseModel):
    """Smart unions: exact type first, then most fields set, then most exact, then leftmost."""
    a: int | str
    b: float | int = 0
    c: bool | int | None = None
    d: Point | Point3 | Label | None = None
    e: list[int] | list[str] = []
    f: Literal["1", 2] | int = 0
    g: Level | Priority | str = "x"
    h: date | int = 0
    i: dict[str, int] | list[Point] = {}
    j: Annotated[str, Field(min_length=2)] | float = 0.0
    k: int | Optional[Level] = 0  # flattened by typing: int | Level | None


class Leg(BaseModel):
    """@model_validator(mode="after"): errors at the model's loc with the raw input; fields set on self."""
    a: int
    b: int = 0

    @model_validator(mode="after")
    def check(self):
        if self.a > 10:
            raise ValueError("a too big")
        if self.a == 5:
            assert False, "five"
        self.b = self.a * 2
        return self


class Swap(BaseModel):
    a: int

    @model_validator(mode="after")
    def swap(self):
        return Swap(a=self.a + 100) if self.a < 0 else self


class Trip(BaseModel):
    legs: list[Leg]
    swap: Swap | None = None


class Amount(BaseModel):
    """mode="before" validators: raw input, reverse definition order, errors at the field."""
    value: int
    note: str | None = None
    unit: str = "eur"

    @field_validator("value", mode="before")
    @classmethod
    def strip_spaces(cls, v):
        if v == "bad":
            raise ValueError("unreadable amount")
        return v.replace(" ", "") if isinstance(v, str) else v

    @field_validator("value", mode="before")
    @classmethod
    def euros(cls, v):
        return v.removesuffix("€") if isinstance(v, str) else v

    _blank_note = field_validator("note", mode="before")(lambda v: None if isinstance(v, str) and not v.strip() else v)

    @model_validator(mode="before")
    @classmethod
    def from_text(cls, data):
        if isinstance(data, str):
            if data == "X":
                raise ValueError("not an amount")
            return {"value": data}
        return data


class Invoice(BaseModel):
    lines: list[Amount]
    total: Amount | None = None


class TaskLoose(BaseModel):
    """A response model whose model_validator(mode="before") receives the ORM object."""
    model_config = ConfigDict(from_attributes=True)

    id: int
    title: str
    source: str = "orm"

    @model_validator(mode="before")
    @classmethod
    def flag(cls, data):
        if isinstance(data, dict):
            return data
        return {"id": data.id, "title": data.title.upper(), "source": "converted"}

"""Pydantic v1's methods on v2 models (deprecated, still shipped): parse_obj, parse_raw, validate, construct,
from_orm, schema, update_forward_refs, .json(), .copy()."""
import datetime as dt
from decimal import Decimal

from fastapi import APIRouter
from pydantic import BaseModel, ConfigDict, Field, ValidationError

router = APIRouter(prefix="/pydv1")


class Owner(BaseModel):
    name: str


class Pet(BaseModel):
    id: int
    name: str = Field("rex", alias="petName")
    tags: list[str] = []
    owner: Owner | None = None
    model_config = ConfigDict(populate_by_name=True)


class Row(BaseModel):
    model_config = ConfigDict(from_attributes=True)
    id: int


class Rec:
    def __init__(self, id):
        self.id = id


def errs(e: ValidationError):
    return [{"type": x["type"], "loc": list(x["loc"]), "msg": x["msg"]} for x in e.errors()]


@router.post("/parse_obj")
async def parse_obj(body: dict):
    try:
        p = Pet.parse_obj(body)
    except ValidationError as e:
        return {"errors": errs(e), "title": e.title}
    return {"pet": p.model_dump(), "set": sorted(p.model_fields_set), "v": Pet.validate(body).id}


@router.post("/parse_raw")
async def parse_raw(body: dict):
    raw = body["raw"]
    try:
        p = Pet.parse_raw(raw.encode() if body.get("bytes") else raw)
    except ValidationError as e:
        return {"errors": errs(e), "str": str(e).splitlines()[:3]}
    return {"pet": p.model_dump(by_alias=True)}


@router.get("/misc")
async def misc():
    p = Pet(id=1, petName="a", tags=["x"], owner=Owner(name="o"))
    c = Pet.construct(id="not-validated", extra=1)
    c2 = Pet.construct({"id"}, id=2)
    cp = p.copy(update={"tags": ["y"]})
    dp = p.copy(deep=True)
    dp.owner.name = "changed"
    try:
        Pet.from_orm(Rec(1))
        from_orm_err = None
    except Exception as e:  # PydanticUserError: a TypeError until 2.12, a RuntimeError from 2.13
        from_orm_err = [type(e).__name__, str(e), isinstance(e, (TypeError, RuntimeError))]
    return {"json": p.json(), "json_alias": p.json(by_alias=True, exclude={"tags"}), "json_none": Pet(id=2).json(exclude_none=True),
            "construct": [c.id, c.model_fields_set == {"id", "extra"}, sorted(c2.model_fields_set)],
            "copy": [cp.tags, p.tags, sorted(cp.model_fields_set)], "deep": [p.owner.name, dp.owner.name],
            "from_orm": Row.from_orm(Rec(7)).id, "from_orm_err": from_orm_err, "schema": [Owner.schema(), Owner.schema() == Owner.model_json_schema()],
            "forward": Pet.update_forward_refs()}


class Defaults(BaseModel):
    model_config = ConfigDict(validate_default=True)
    n: int = "3"
    tags: list[int] = ["1", 2]
    raw: str = Field(default=5, validate_default=False)
    when: float | None = None


class BadDefault(BaseModel):
    model_config = ConfigDict(validate_default=True)
    n: int = "x"


class LooseChild(Defaults):
    model_config = ConfigDict(validate_default=False)


@router.get("/validate_default")
async def validate_default():
    d = Defaults()
    try:
        BadDefault()
        bad = None
    except ValidationError as e:
        bad = errs(e)
    c = LooseChild()
    return {"d": [d.n, d.tags, d.raw, d.when], "given": Defaults(n=4).n, "bad": bad, "child": [c.n, c.tags]}


class Encoded(BaseModel):
    """model_config json_encoders (pydantic 2 still applies them, in JSON mode only)"""
    model_config = ConfigDict(json_encoders={dt.datetime: lambda v: v.strftime("%Y/%m/%d"), Decimal: float})
    at: dt.datetime
    amount: Decimal
    day: dt.date


class EncodedChild(Encoded):
    extra: Decimal = Decimal("2.50")


@router.get("/json_encoders", response_model=EncodedChild)
async def json_encoders():
    e = EncodedChild(at=dt.datetime(2026, 1, 2, 3, 4), amount=Decimal("1.50"), day=dt.date(2026, 1, 2))
    return e


@router.get("/json_encoders/modes")
async def json_encoders_modes():
    e = Encoded(at=dt.datetime(2026, 1, 2, 3, 4), amount=Decimal("1.50"), day=dt.date(2026, 1, 2))
    return {"json": e.model_dump_json(), "py": str(e.model_dump()), "json_mode": e.model_dump(mode="json")}

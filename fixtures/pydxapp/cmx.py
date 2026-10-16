"""`Model = create_model('Model', ...)` at module level: compiled as the class it creates (fields in order,
`(type, default)` and `(type, Field(...))`, `__base__`, `__doc__`, validators and computed fields of
`__namespace__` / `__validators__` bound as pydantic binds them)."""
from fastapi import APIRouter
from pydantic import BaseModel, Field, ValidationError, computed_field, create_model, field_validator, model_validator

router = APIRouter(prefix="/cmx")


def alphanum(cls, v: str) -> str:
    assert v.isalnum(), "must be alphanumeric"
    return v


def full_name(self) -> str:
    return f"{self.first_name} {self.last_name}"


def check_total(self):
    if self.qty * self.price > 100:
        raise ValueError("too expensive")
    return self


def tagged(cls, v, info):
    return f"{info.field_name}:{v}"


UserModel = create_model(
    "UserModel",
    first_name=str,
    last_name=str,
    __namespace__={
        "check_alphanum": field_validator("first_name", "last_name")(alphanum),
        "full_name": computed_field(property(full_name)),
    },
)


class Priced(BaseModel):
    price: float = Field(ge=0)


Line = create_model(
    "Line",
    __base__=Priced,
    __doc__="an order line",
    qty=(int, 1),
    sku=(str, Field(min_length=2, max_length=5)),
    note=(str | None, None),
    __validators__={"total": model_validator(mode="after")(check_total), "tag": field_validator("sku")(tagged)},
)


@router.post("/user")
def user(item: UserModel) -> UserModel:
    return item


@router.post("/user/dump")
def user_dump(item: UserModel):
    return {"dump": item.model_dump(mode="json"), "json": item.model_dump_json(), "repr": repr(item)}


@router.post("/line")
def line(item: Line) -> Line:
    return item


@router.post("/direct")
def direct(body: dict):
    try:
        return {"line": repr(Line(**body)), "fields": list(Line.model_fields)}
    except ValidationError as e:
        return {"errors": e.errors(include_url=False), "title": e.title}

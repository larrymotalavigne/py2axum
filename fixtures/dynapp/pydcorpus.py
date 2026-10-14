"""Pydantic constructions of its documentation lifted for the 0.7 (corpus/): validation and serialization aliases
(AliasPath, AliasChoices, alias generators, validate_by_alias/validate_by_name, serialize_by_alias), Field(exclude=,
repr=, frozen=, multiple_of=, union_mode=, strict=), str_min_length/str_max_length and the str config applied to
container items, strict mode, discriminated unions."""
from collections import Counter, OrderedDict, deque
from collections.abc import Callable, Sequence
from fractions import Fraction
from types import EllipsisType
from typing import Annotated, Generic, Literal, TypeVar

from pydantic import (UUID4, AfterValidator, AliasChoices, AliasGenerator, AliasPath, BaseModel, BeforeValidator, ConfigDict,
                      Discriminator, Field, SecretStr, SkipValidation, StringConstraints, Tag,
                      PlainSerializer, PlainValidator, ValidationError, ValidationInfo, WrapSerializer, WrapValidator,
                      field_serializer, field_validator, model_serializer)
from pydantic import HttpUrl, RootModel
from pydantic.alias_generators import to_camel, to_snake
from pydantic_core import PydanticCustomError, PydanticUseDefault
from fastapi import APIRouter
from datetime import date, datetime

router = APIRouter(prefix="/pydc")


def shout(name: str) -> str:
    """an alias generator of the project: evaluated at translation time"""
    return "".join(w.capitalize() for w in name.split("_")) + "_"


class Paths(BaseModel):
    model_config = ConfigDict(extra="forbid")

    first: str = Field(validation_alias=AliasPath("names", 0))
    last: str = Field("?", validation_alias=AliasPath("names", -1))
    city: str = Field("", validation_alias=AliasChoices("city", AliasPath("contact", "address", "city"), "town"))
    zip_code: int = Field(0, validation_alias="zip", serialization_alias="postcode")


class Gen(BaseModel):
    model_config = ConfigDict(alias_generator=to_camel, populate_by_name=True, extra="allow")

    first_name: str
    last_name: str = "x"
    language_code: str = Field("fr", alias="lang")


class Gen2(BaseModel):
    model_config = ConfigDict(alias_generator=AliasGenerator(validation_alias=lambda n: n.upper(),
                                                             serialization_alias=shout))

    field_one: int
    other: str = Field("o", serialization_alias="OTHER")


class Snake(BaseModel):
    model_config = ConfigDict(alias_generator=to_snake, serialize_by_alias=True)

    myField: int = 0
    HTTPCode: int = 0


class ByName(BaseModel):
    model_config = ConfigDict(validate_by_alias=False, validate_by_name=True)

    my_field: str = Field(validation_alias="my_alias")


class Opts(BaseModel):
    model_config = ConfigDict(str_max_length=6, str_min_length=1, str_strip_whitespace=True, str_to_upper=True)

    name: str
    tags: list[str] = []
    labels: dict[str, str] = {}
    code: str = Field("ok", max_length=8)
    hidden: int = Field(0, exclude=True)
    secret: str = Field("s", repr=False)
    fixed: str = Field("f", frozen=True)
    step: int = Field(0, multiple_of=5, le=100)
    ratio: float = Field(0.0, multiple_of=0.1)
    even: float = Field(0, multiple_of=2)
    either: str | int = Field(0, union_mode="left_to_right")
    other: int | str = Field(0, union_mode="left_to_right")


class Inner(BaseModel):
    v: int = 0


class Strict(BaseModel):
    model_config = ConfigDict(strict=True)

    when: date | None = None
    at: datetime | None = None
    n: int = 0
    f: float = 0.0
    s: str = ""
    b: bool = False
    pair: tuple[int, ...] | None = None
    items: list[int] = []
    inner: Inner = Inner()
    lax: int = Field(0, strict=False)


class FieldStrict(BaseModel):
    site: HttpUrl | None = Field(None, strict=True)
    n: int = Field(0, strict=True)
    nums: list[int] = Field([], strict=True)
    day: date | None = Field(None, strict=True)
    loose: int = 0


class Cat(BaseModel):
    pet_type: Literal["cat"]
    meows: int


class Dog(BaseModel):
    pet_type: Literal["dog"]
    barks: float


class Lizard(BaseModel):
    pet_type: Literal["reptile", "lizard"]
    scales: bool


class BlackCat(BaseModel):
    pet_type: Literal["kitty"]
    color: Literal["black"]
    black_name: str


class WhiteCat(BaseModel):
    pet_type: Literal["kitty"]
    color: Literal["white"]
    white_name: str


Kitty = Annotated[BlackCat | WhiteCat, Field(discriminator="color")]


class Owner(BaseModel):
    pet: Cat | Dog | Lizard | None = Field(discriminator="pet_type")
    nested: Annotated[Kitty | Dog, Field(discriminator="pet_type")] | None = None
    n: int = 0


def is_even(value: int) -> int:
    if value % 2 == 1:
        raise ValueError(f"{value} is not an even number")
    return value


def tagged(value: int, info: ValidationInfo) -> str:
    return f"<{value} {info.field_name!r} {sorted(info.data)}>"


def ensure_list(value):
    return value if isinstance(value, list) else [value]


def doubled(value):
    return value * 2 if isinstance(value, int) else value


Even = Annotated[int, AfterValidator(is_even)]


class Checked(BaseModel):
    number: Annotated[int, AfterValidator(is_even), AfterValidator(lambda v: v + 1)] = 0
    alias_even: Even = 0
    label: Annotated[int, Field(ge=0), AfterValidator(tagged)] = 0
    numbers: Annotated[list[int], BeforeValidator(ensure_list)] = []
    plain: Annotated[int, Field(ge=10), PlainValidator(doubled)] = 0
    stamp: int = 0

    @field_validator("number")
    @classmethod
    def outer(cls, v):
        return v * 10

    @field_validator("stamp", mode="plain")
    @classmethod
    def stamp_plain(cls, v):
        if v == "now":
            return 42
        if not isinstance(v, int):
            raise ValueError("not a stamp")
        return v


def truncate(value, handler):
    try:
        return handler(value)
    except ValidationError as err:
        if err.errors()[0]["type"] == "string_too_long":
            return handler(value[:5])
        raise


def refuse_wrapped(value, handler):
    if value == "boom":
        raise ValueError("no boom")
    return handler(value)


class Wrapped(BaseModel):
    short: Annotated[str, Field(max_length=5), WrapValidator(truncate)] = ""
    nums: Annotated[list[int], WrapValidator(refuse_wrapped), AfterValidator(lambda v: v[::-1])] = []
    when: datetime | None = None
    word: Annotated[str, Field(max_length=3)] = ""

    @field_validator("when", mode="wrap")
    @classmethod
    def when_default(cls, value, handler):
        if value == "epoch":
            return datetime(1970, 1, 1)
        when = handler(value)
        return when.replace(microsecond=0) if when is not None else None

    @field_validator("word", mode="wrap")
    @classmethod
    def word_info(cls, value, handler, info):
        return f"{info.field_name}:{handler(value)}"


@router.post("/wrapped")
def wrapped(item: Wrapped) -> Wrapped:
    return item


class Shapes(BaseModel):
    t: tuple[int, float, bool] = (0, 0, False)
    one: tuple[int] = (0,)
    empty: tuple[()] = ()
    pair: tuple[Inner, str] | None = None
    kind: type[Inner] = Inner
    model: type[BaseModel] = Inner
    note: str = ""

    @field_validator("note")
    @classmethod
    def ctx(cls, v, info: ValidationInfo):
        return f"{v}:{info.context}"


@router.post("/shapes")
def shapes(item: Shapes):
    return {"t": item.t, "one": item.one, "empty": item.empty, "pair": item.pair, "kind": item.kind.__name__,
            "model": item.model.__name__, "note": item.note, "json": item.model_dump_json(exclude={"kind", "model"})}


def plus_one(value, handler):
    return handler(value) + 1


class Sered(BaseModel):
    number: Annotated[int, WrapSerializer(plus_one)] = 0
    when: Annotated[datetime | None, PlainSerializer(lambda v: v.year if v else None, when_used="json")] = None
    text: str = ""
    tags: list[str] = []
    ratio: float = Field(0, exclude_if=lambda v: v == 0)
    kept: int = 1

    @field_serializer("text", mode="plain")
    @classmethod
    def upper(cls, v: str, info) -> str:
        return f"{v.upper()}:{info.mode}:{info.context}"

    @field_serializer("tags", mode="wrap")
    def joined(self, value, handler):
        return ",".join(handler(value)) + f"/{self.kept}"


class Flat(BaseModel):
    username: str
    password: str

    @model_serializer(mode="plain")
    def as_text(self) -> str:
        return f"{self.username} - {self.password}"


class Listed(BaseModel):
    username: str
    inner: Inner = Inner()

    @model_serializer(mode="wrap")
    def with_fields(self, handler):
        out = handler(self)
        out["fields"] = list(out)
        return out


class Holder(BaseModel):
    flat: Flat | None = None
    listed: list[Listed] = []


@router.post("/sered")
def sered(item: Sered):
    return {"model": item.model_dump(), "json": item.model_dump(mode="json"), "text": item.model_dump_json(),
            "defaults": item.model_dump(exclude_defaults=True)}


@router.post("/sered/model")
def sered_model(item: Sered) -> Sered:
    return item


@router.post("/holder")
def holder(item: Holder) -> Holder:
    return item


@router.post("/holder/dump")
def holder_dump(item: Holder):
    return {"py": item.model_dump(), "json": item.model_dump_json()}


class Colls(BaseModel):
    dq: deque[int] = deque()
    fs: frozenset[int] = frozenset()
    anyfs: frozenset = frozenset()
    counts: Counter[str] = Counter()
    ordered: OrderedDict[str, int] = {}
    seq: Sequence[str] = []


@router.post("/colls")
def colls(item: Colls) -> Colls:
    return item


@router.post("/colls/use")
def colls_use(item: Colls):
    item.dq.appendleft(0)
    return {"dq": list(item.dq), "most": item.counts.most_common(1), "missing": item.counts["zz"], "seq": type(item.seq).__name__,
            "fs": sorted(item.fs), "ordered": list(item.ordered.items())}


DataT = TypeVar("DataT")
ItemT = TypeVar("ItemT", bound="Inner")


class Envelope(BaseModel, Generic[DataT]):
    data: DataT
    note: str = ""


class Paged(Envelope[list[DataT]], Generic[DataT]):
    total: int = 0


class Holding(BaseModel, Generic[ItemT]):
    item: ItemT


class Pair[K, V](BaseModel):
    key: K
    value: V


@router.post("/env/int")
def env_int(item: Envelope[int]) -> Envelope[int]:
    return item


@router.post("/env/inner")
def env_inner(item: Envelope[Inner]) -> Envelope[Inner]:
    return item


@router.post("/env/raw")
def env_raw(item: Envelope) -> Envelope:
    return item


@router.post("/paged")
def paged(item: Paged[int]):
    return {"dump": item.model_dump(), "name": type(item).__name__}


@router.post("/holding")
def holding(item: Holding) -> Holding:
    return item


@router.post("/pair")
def pair(item: Pair[str, int]) -> Pair[str, int]:
    return item


@router.post("/checked")
def checked(item: Checked) -> Checked:
    return item


@router.post("/paths")
def paths(item: Paths) -> Paths:
    return item


@router.post("/paths/dump")
def paths_dump(item: Paths):
    return {"plain": item.model_dump(), "alias": item.model_dump(by_alias=True), "json": item.model_dump_json(by_alias=True)}


@router.post("/gen")
def gen(item: Gen) -> Gen:
    return item


@router.post("/gen/dump")
def gen_dump(item: Gen):
    return {"plain": item.model_dump(), "alias": item.model_dump(by_alias=True), "set": sorted(item.model_fields_set)}


@router.post("/gen2")
def gen2(item: Gen2) -> Gen2:
    return item


@router.post("/snake")
def snake(item: Snake):
    return {"default": item.model_dump(), "no": item.model_dump(by_alias=False), "json": item.model_dump_json()}


@router.post("/byname")
def byname(item: ByName) -> ByName:
    return item


@router.post("/opts")
def opts(item: Opts):
    out = {"dump": item.model_dump(), "repr": repr(item)}
    try:
        item.fixed = "g"
        out["frozen"] = "assigned"
    except ValidationError as e:
        out["frozen"] = e.errors(include_url=False)
    item.name = "renamed"
    out["name"] = item.name
    return out


@router.post("/opts/model")
def opts_model(item: Opts) -> Opts:
    return item


@router.post("/strict")
def strict(item: Strict) -> Strict:
    return item


@router.post("/fieldstrict")
def fieldstrict(item: FieldStrict) -> FieldStrict:
    return item


@router.post("/strict/python")
def strict_python(data: dict):
    """strict validation of Python values (the date a route builds is accepted, its text is not)"""
    out = []
    for value in (date(2020, 1, 2), "2020-01-02", data.get("when")):
        try:
            out.append(Strict(when=value).model_dump(mode="json"))
        except ValidationError as e:
            out.append(e.errors(include_url=False))
    return out


def pie_kind(v):
    if isinstance(v, dict):
        return v.get("fruit", v.get("filling"))
    return getattr(v, "fruit", getattr(v, "filling", None))


class Apple(BaseModel):
    fruit: Literal["apple"] = "apple"
    n: int = 0


class Pumpkin(BaseModel):
    filling: Literal["pumpkin"] = "pumpkin"
    n: int = 0


class Dinner(BaseModel):
    dessert: Annotated[Annotated[Apple, Tag("apple")] | Annotated[Pumpkin, Tag("pumpkin")], Discriminator(pie_kind)] | None = None
    value: Annotated[Annotated[str, Tag("str")] | Annotated[int, Tag("int")],
                     Discriminator(lambda v: "str" if isinstance(v, str) else ("int" if isinstance(v, int) else None),
                                   custom_error_type="invalid_union_member", custom_error_message="Invalid union member",
                                   custom_error_context={"discriminator": "str_or_int"})] = 0
    pie: Annotated[Apple, Tag("apple")] | Annotated[Pumpkin, Tag("pumpkin")] = Field(Apple(), discriminator=Discriminator(pie_kind))


class Secrets(BaseModel):
    password: SecretStr
    other: SecretStr | None = None
    empty: SecretStr = SecretStr("")
    uid: UUID4 | None = None


class Defaults(BaseModel):
    a: int = Field(gt=10)
    c: int = 5
    b: int = Field(default_factory=lambda data: data["a"] + data["c"])
    d: dict = Field(default_factory=lambda data: dict(data))

    @field_validator("a")
    @classmethod
    def double(cls, x):
        return x * 2


@router.post("/secrets")
def secrets(item: Secrets) -> Secrets:
    return item


@router.post("/secrets/use")
def secrets_use(item: Secrets):
    return {"d": item.model_dump(), "r": repr(item), "v": item.password.get_secret_value(), "s": str(item.password),
            "eq": item.password == SecretStr("abc"), "j": item.model_dump_json()}


@router.post("/defaults")
def defaults(item: Defaults) -> Defaults:
    return item


class Plain:
    """an arbitrary type (arbitrary_types_allowed): validated by isinstance"""


Ident = int


class Misc(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True)

    # (StringConstraints(ascii_only=) exists since pydantic 2.14: not in this fixture, which runs on 2.12 too)
    code: Annotated[str, StringConstraints(max_length=3, pattern="^[a-z]+$", to_upper=True)] = "x"
    low: Annotated[str, StringConstraints(pattern="^[A-Z]+$", to_lower=True)] = "X"
    names: list[SkipValidation[str]] = []
    hook: Callable | None = None
    dots: EllipsisType | None = None
    plain: Plain | None = None
    ident: Ident = 0


class Shout(BaseModel):
    model_config = ConfigDict(str_to_upper=True)

    v: str = Field("A", pattern="^[a-z]+$")


class Cplx(BaseModel):
    n: complex = 0
    m: complex | None = None


@router.post("/cplx")
def cplx(item: Cplx) -> Cplx:
    return item


@router.post("/cplx/use")
def cplx_use(item: Cplx):
    return {"r": repr(item.n), "s": str(item.n), "j": item.model_dump_json(), "m": repr(item)}


class Ratio(BaseModel):
    x: Fraction = Fraction(1, 2)
    y: Fraction | None = None


@router.post("/ratio")
def ratio(item: Ratio) -> Ratio:
    return item


@router.post("/ratio/use")
def ratio_use(item: Ratio):
    return {"r": repr(item.x), "s": str(item.x), "d": item.model_dump(), "j": item.model_dump_json(), "m": repr(item)}


class Pets(RootModel[list[str]]):
    def describe(self) -> str:
        return f"Pets: {', '.join(self.root)}"


class Counts(RootModel):
    root: dict[str, int] = {}


class Kennel(BaseModel):
    pets: Pets
    counts: Counts = Counts()


@router.post("/pets")
def pets(item: Pets) -> Pets:
    return item


@router.post("/pets/use")
def pets_use(item: Pets):
    return {"d": item.model_dump(), "r": repr(item), "desc": item.describe(), "root": item.root, "j": item.model_dump_json(),
            "s": sorted(item.model_fields_set)}


@router.post("/kennel")
def kennel(item: Kennel) -> Kennel:
    return item


@router.post("/misc")
def misc(item: Misc) -> Misc:
    return item


@router.post("/shout")
def shout_route(item: Shout) -> Shout:
    return item


def none_default(v):
    if v is None:
        raise PydanticUseDefault()
    return v


def default_on_error(v, handler):
    try:
        return handler(v)
    except ValidationError:
        raise PydanticUseDefault()


class Custom(BaseModel):
    name: Annotated[str, BeforeValidator(none_default)] = "default_name"
    w: Annotated[int, WrapValidator(default_on_error)] = 7
    x: int = 0
    note: str = ""

    @field_validator("x")
    @classmethod
    def answer(cls, v):
        if v and v % 42 == 0:
            raise PydanticCustomError("the_answer_error", "{number} is the answer! {missing} {flag}", {"number": v, "flag": True, "l": [1]})
        return v

    @field_serializer("note")
    def ctx(self, v, info):
        return f"{v}:{info.context}"


@router.post("/custom")
def custom(item: Custom):
    return {"d": item.model_dump(), "ctx": item.model_dump(context={"k": 1}), "set": sorted(item.model_fields_set)}


@router.post("/dinner")
def dinner(item: Dinner) -> Dinner:
    return item


@router.post("/owner")
def owner(item: Owner) -> Owner:
    return item


@router.post("/owner/python")
def owner_python():
    """a discriminated union given a model instance (its attribute is read)"""
    return Owner(pet=Dog(pet_type="dog", barks=2)).model_dump()

"""`__get_pydantic_core_schema__` (a class's, an Annotated metadata's) and `GetPydanticSchema`, read statically as the
equivalent annotation: after/before/plain validators, serializers, constraints set on the handler's schema."""
from collections.abc import Sequence
from dataclasses import dataclass
from typing import Annotated, Any

from fastapi import APIRouter
from pydantic import BaseModel, GetCoreSchemaHandler, GetPydanticSchema, ValidationError, ValidationInfo
from pydantic_core import core_schema

router = APIRouter(prefix="/coreschx")


class CustomType:
    def __init__(self, value: int, field_name: str):
        self.value = value
        self.field_name = field_name

    @classmethod
    def validate(cls, value: int, info: ValidationInfo):
        return cls(value, info.field_name)

    @classmethod
    def __get_pydantic_core_schema__(cls, source_type: Any, handler: GetCoreSchemaHandler) -> core_schema.CoreSchema:
        return core_schema.with_info_after_validator_function(cls.validate, handler(int))


class WithCustom(BaseModel):
    my_field: CustomType


class Doubled(BaseModel):
    y: Annotated[str, GetPydanticSchema(lambda tp, handler: core_schema.no_info_after_validator_function(lambda x: x * 2, handler(tp)))]


@dataclass
class CompressedString:
    dictionary: dict[int, str]
    text: list[int]

    def build(self) -> str:
        return " ".join([self.dictionary[key] for key in self.text])

    @classmethod
    def __get_pydantic_core_schema__(cls, source: type[Any], handler: GetCoreSchemaHandler) -> core_schema.CoreSchema:
        assert source is CompressedString
        return core_schema.no_info_after_validator_function(
            cls._validate,
            core_schema.str_schema(),
            serialization=core_schema.plain_serializer_function_ser_schema(
                cls._serialize, info_arg=False, return_schema=core_schema.str_schema()),
        )

    @staticmethod
    def _validate(value: str) -> "CompressedString":
        inverse: dict[str, int] = {}
        text: list[int] = []
        for word in value.split(" "):
            if word not in inverse:
                inverse[word] = len(inverse)
            text.append(inverse[word])
        return CompressedString({v: k for k, v in inverse.items()}, text)

    @staticmethod
    def _serialize(value: "CompressedString") -> str:
        return value.build()


class Compressed(BaseModel):
    value: CompressedString


@dataclass
class RestrictCharacters:
    alphabet: Sequence[str]

    def __get_pydantic_core_schema__(self, source: type[Any], handler: GetCoreSchemaHandler) -> core_schema.CoreSchema:
        if not self.alphabet:
            raise ValueError("Alphabet may not be empty")
        schema = handler(source)
        if schema["type"] != "str":
            raise TypeError("RestrictCharacters can only be applied to strings")
        return core_schema.no_info_after_validator_function(self.validate, schema)

    def validate(self, value: str) -> str:
        if any(c not in self.alphabet for c in value):
            raise ValueError(f"{value!r} is not restricted to {self.alphabet!r}")
        return value


class Restricted(BaseModel):
    value: Annotated[str, RestrictCharacters("ABC")]


class SmallString:
    def __get_pydantic_core_schema__(self, source: type[Any], handler: GetCoreSchemaHandler) -> core_schema.CoreSchema:
        schema = handler(source)
        assert schema["type"] == "str"
        schema["max_length"] = 10
        return schema


class Small(BaseModel):
    value: Annotated[str, SmallString()]
    other: Annotated[int, GetPydanticSchema(lambda tp, h: core_schema.int_schema(ge=0, le=5))] = 1


class Before(BaseModel):
    n: Annotated[int, GetPydanticSchema(lambda tp, h: core_schema.no_info_before_validator_function(
        lambda v: v.strip() if isinstance(v, str) else v, h(tp)))]


class Chained(BaseModel):
    n: Annotated[int, GetPydanticSchema(lambda tp, h: core_schema.chain_schema(
        [core_schema.int_schema(), core_schema.no_info_plain_validator_function(lambda v: v * 10)]))]


@router.post("/custom")
def custom(item: WithCustom):
    return {"value": item.my_field.value, "field": item.my_field.field_name}


@router.post("/doubled")
def doubled(item: Doubled) -> Doubled:
    return item


@router.post("/compressed")
def compressed(item: Compressed) -> Compressed:
    return item


@router.post("/compressed/dump")
def compressed_dump(item: Compressed):
    return {"json": item.model_dump(mode="json"), "text": item.model_dump_json(), "words": item.value.text}


@router.post("/restricted")
def restricted(item: Restricted) -> Restricted:
    return item


@router.post("/small")
def small(item: Small) -> Small:
    return item


@router.post("/before")
def before(item: Before) -> Before:
    return item


@router.post("/chained")
def chained(item: Chained) -> Chained:
    return item


@router.post("/direct")
def direct(body: dict):
    try:
        return {"ok": Restricted(value=body["v"]).value}
    except ValidationError as e:
        return {"title": e.title, "errors": e.errors(include_url=False), "str": str(e)}

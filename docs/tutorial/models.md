# Models (Pydantic)

FastAPI validates request data and serializes responses with Pydantic v2 models. The binary embeds its own
implementation of Pydantic v2's lax mode, with pydantic-core's error types, messages, locations and contexts, so
that a 422 is the same byte for byte. It also covers validators, `model_config`, computed fields,
`TypeAdapter` and pydantic-settings.

A Pydantic v2 model with `EmailStr`, `Literal`, nested models, `Decimal` constraints, an alias with
`populate_by_name`, `str_strip_whitespace`, a field validator, a model validator and a computed field; then
`model_dump` with its options. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/tutorial/models.py"
--8<-- "docs_src/tutorial/models.py"
```

## What is native

- Lax-mode validation with pydantic-core's error types, messages, locations and contexts (speedate for
  dates), smart unions (exactness, fields set), enums, literals, nested models, lists/sets/tuples/dicts,
  `Optional`, `Field` constraints (and pydantic 1's `min_items`/`max_items`, which `Field()` still maps to
  `min_length`/`max_length`), aliases (`alias=`, and `validation_alias=`/`serialization_alias=` naming a
  different input and output key, `populate_by_name`; `AliasChoices`/`AliasPath` refused), defaults and `default_factory`,
  `validate_assignment` (an assignment runs the field's `before` validators, its type, its `after` validators
  with `info.data` holding every other field, then the model's `after` validators: one of those that raises
  leaves the value assigned, error at loc `()`, as pydantic-core does; refused with a `mode="before"` model
  validator; an `after` validator's error reports the raw input; a `mode="before"` field validator defined after
  a `mode="after"` one of the same field is refused), `extra=`, `from_attributes`, `str_strip_whitespace`/`to_lower`/`to_upper`,
  `use_enum_values`, `model_config` as a dict or `ConfigDict`, v1 `class Config` (v1-only keys ignored like
  Pydantic v2 does). The `str_*` settings also apply to the string an `EmailStr` validates. Refused: a
  `default_factory` taking the validated data (`lambda data: ...`), `Iterable[T]`, `Sequence[T]`,
  `Collection[T]` as a field, parameter, `TypeAdapter` or response type (pydantic validates an `Iterable` lazily,
  a one-shot iterator whose item errors come at iteration, and a `Sequence` keeps the input's type: annotate
  `list[...]`; the `AsyncIterable[T]`/`Iterable[T]` return annotation of a generator endpoint is its stream item
  type, see [Responses](responses.md)).
- An Enum member given to a scalar field (an ORM enum column read into `status: str`...) as pydantic-core takes
  it: a `str`/`int` subclass member (`class X(str, Enum)`, `StrEnum`, `IntEnum`) is its value for `str`, `int`,
  `float`, `bool` and `Literal` (an `int` one is `str(value)` in a `str`), a plain member is `str(value)` in a
  `str` and its value, unchecked, in an unconstrained `int`.
- `uuid.UUID` fields and parameters: a UUID instance, a str in the simple, hyphenated, `{braced}` or
  `urn:uuid:` form, or bytes, with pydantic-core's `uuid_type`/`uuid_parsing` errors (the messages of the
  `uuid` crate pinned by the project's pydantic-core); dumped as the hyphenated str.
- `@field_validator` / `@validator` (after and before, including `_x = field_validator(...)(lambda v: ...)`),
  `info: ValidationInfo` in after validators (`info.data`: the earlier fields that passed, after their own
  validators, defaults included; `info.field_name`; other attributes, and `info` in before validators, are
  refused), v1 `values` (pydantic's own signature rules: `field`/`config` parameters are refused),
  `@model_validator` (before/after); validators that raise produce the 422 at their place. Before
  validators run on the raw input in reverse definition order, like pydantic-core.
- `model_dump(mode=, by_alias=, exclude_none=, exclude_unset=, exclude=, include=)` (top-level field names
  for `exclude`/`include`), `model_dump_json`, `model_validate(_json)`, `model_copy(update=, deep=)`,
  `model_fields_set`, `model_fields`, `ValidationError.errors()` (URL with the major.minor of the pydantic the
  project locks, else the one installed next to py2axum; `ctx.error` is the exception raised by the validator,
  rendered as its attributes by `jsonable_encoder`; `include_*` options) and `error_count()`.
- `Model.model_json_schema()` (default arguments only), computed at translation time like the
  `GenerateJsonSchema` of pydantic 2.13 and 2.14 (identical on this subset) (key order included; same subset as MCP tool schemas, plus `model_config extra=`).
  The receiver must be a model class by name, or `expr.attr` where every value the project binds to an
  attribute or keyword `attr` is a model class (a tool registry); anything else is refused.
- `model_config frozen=True` (assignment raises `frozen_instance`; frozen models hash by value),
  `Field(validate_default=True)`, field options given in `Annotated[T, Field(...)]`.
- `TypeAdapter(T)`: `validate_python` (ORM objects with `from_attributes`, iterables), `validate_json`,
  `dump_python`, `dump_json`, for types written in the source or known at run time.
- `EmailStr` (rule-by-rule port of email-validator, same messages, except IDNA encoding and NFC normalization
  of internationalized domains, which are accepted and lowercased), `AnyUrl`/`AnyHttpUrl`/`HttpUrl`/`RedisDsn`
  (parsed with the `url` crate like pydantic-core, type defaults, attributes, same errors).
- pydantic-settings `BaseSettings`: environment variables (not `.env` files), `validate_default=True`,
  `env_prefix`, `case_sensitive` (names compared case-insensitively by default, the last of two names that
  differ only in case wins), `env_parse_none_str` (an environment value equal to it, case included, is
  `None`; init keywords are not parsed), JSON values for container/model fields (a union with such a
  member keeps the raw string when the JSON does not parse); other `env_*` options and `_env_*` init keywords are refused;
  a class body run like a script (class-level `if`, attributes reading earlier ones) is evaluated once.
- `decimal.Decimal` fields: lax validation like pydantic-core (a float through its `repr`, a string through
  `Decimal(str)`, finite values only), `max_digits`/`decimal_places` on the normalized value, then
  `le`/`lt`/`ge`/`gt` (int or float literals); dumped as a string in JSON mode.
- `@computed_field` (bare, over `@property` or alone): serialized after the fields and the extras,
  `exclude_none` applies, `exclude_unset` does not, `repr()` shows them. The property must not await.
- A `@classmethod` override of `model_validate` (called by name, `Model.model_validate(x)`) calling
  `super().model_validate(...)` or `super(Model, cls)`: the parent's (a project model's override, else
  BaseModel's), `cls` still the class called. `super(cls, cls)` is accepted when no project model subclasses
  the class (else its target depends on the class called: refused).
- A model's own `def __init__(self, **data)` calling `super().__init__(**data)`: run by `Model(...)`;
  validation from attributes (`from_attributes`, ORM objects) or of an existing instance skips it, as in
  pydantic-core.
- Private attributes (`_name: T = PrivateAttr(default=/default_factory=)`, any `_name`): per instance, never
  validated nor dumped; a `_name` assigned on an instance is not dumped either (unless `extra="allow"`).
  An instance of the response model's own class is serialized as returned, private attributes included.

The binary does not read `.env` files: see [Environment variables](../reference/environment.md).
- `model_config` `val_json_bytes=` / `ser_json_bytes=` (`"utf8"`, `"base64"`, `"hex"`): a `str` input of a
  `bytes` field of the model (inside containers too) decoded as pydantic-core does (URL-safe base64, the
  standard alphabet when the input holds `+` or `/`, padding optional; its error messages, which differ
  between pydantic-core 2.46 and 2.50), the JSON dump encoded (URL-safe base64 with padding, lowercase hex).
  A nested model uses its own config.

## What stays in Python

- Not supported: `@computed_field(...)` options, `@field_serializer`/`@model_serializer`, nested
  `exclude`/`include` dicts, strict mode.
- A union with a member that has validators (directly or in a nested model) is refused: Pydantic would try the
  next member when one raises.

## Differences

- A plain Enum member whose value is not an integer, given to a *constrained* `int`, reports `int_parsing`
  where pydantic-core reports `int_parsing_size`.
- With `extra="allow"`, an extra key named like a computed field shadows it on attribute access and in
  `model_dump_json` (Pydantic writes both keys).
- `jsonable_encoder` of an integral `Decimal` beyond 64 bits gives a float (Python: an int).
- Validating a model that defines its own `__init__` from a dict (request body, `model_validate(dict)`,
  nested), where pydantic-core calls the `__init__`, raises a py2axum RuntimeError (500) instead.

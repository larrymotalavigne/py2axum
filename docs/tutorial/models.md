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
  `min_length`/`max_length`), aliases (below), defaults and `default_factory`,
  `validate_assignment` (an assignment runs the field's `before` validators, its type, its `after` validators
  with `info.data` holding every other field, then the model's `after` validators: one of those that raises
  leaves the value assigned, error at loc `()`, as pydantic-core does; refused with a `mode="before"` model
  validator; an `after` validator's error reports the raw input; a `mode="before"` field validator defined after
  a `mode="after"` one of the same field is refused), `extra=`, `from_attributes`, `str_strip_whitespace`/`to_lower`/`to_upper`,
  `use_enum_values`, `model_config` as a dict or `ConfigDict`, v1 `class Config` (v1-only keys ignored like
  Pydantic v2 does). The `str_*` settings also apply to the string an `EmailStr` validates. Refused: a
  `MutableSequence[T]` or `Collection[T]` as a field, parameter, `TypeAdapter` or response type (the
  `AsyncIterable[T]`/`Iterable[T]` return annotation of a generator endpoint is its stream item type, see
  [Responses](responses.md)).
- `Iterable[T]` fields as pydantic-core's `ValidatorIterator`: any iterable is accepted (a str gives its
  characters, a dict its keys; anything else `iterable_type`), the items are validated when reached, once:
  `next()`, a loop, `list()`, `model_dump(mode="json")` (ValidationError titled `ValidatorIterator`, loc the
  item's index), `model_dump_json()` and the response (`PydanticSerializationError`, a 500). The items are taken
  when the field is validated (CPython reads a list as it is iterated: a list changed in between differs).
  Refused: a default other than None (pydantic keeps it unvalidated and warns when dumping it), `Field()`
  constraints. `model_dump()` in python mode (pydantic's `SerializationIterator`) raises.
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
- `Model.model_construct(_fields_set=None, **values)` (also read on an instance), as pydantic 2.12 to 2.14 builds
  it without validation: each field takes its `alias`, then its validation aliases (a key of `AliasChoices` is
  consumed, an `AliasPath` only read), then its name, else its default (a `default_factory` taking data gets the
  fields taken so far); the fields set are those taken, or exactly `_fields_set`; the keys left are the extras
  with `extra="allow"` (not counted in `model_fields_set`, as in pydantic), set the private attributes they
  name, and are ignored otherwise. Computed fields, `model_dump`, `model_copy` and a `response_model` work on the
  instance. Refused at translation: a RootModel, a model with `model_post_init` or a private attribute with a computed
  default, a field with both `alias=` and
  a different `validation_alias=`, a required field missing from literal keywords. Differences, raised at run
  time (`TypeError`): a required field left out through `**values` (pydantic leaves it unset, reading it
  raises `AttributeError`), a `_fields_set` that is not a set of field names, and dumping a value whose type
  is not the field's own (a `str` in an `int` field, a dict in a model field: pydantic serializes it as it is,
  with a `PydanticSerializationUnexpectedValue` warning; an `int` in a `float` field is dumped as pydantic does).
- `model_extra` (`__pydantic_extra__`: the extras with `extra="allow"`, else None).
- `model_config frozen=True` (assignment raises `frozen_instance`; frozen models hash by value),
  `Field(validate_default=True)`, field options given in `Annotated[T, Field(...)]`.
- Aliases: `alias=`, `validation_alias=` (a key, `AliasPath`, `AliasChoices`: the first path found wins, a
  missing field is reported at the first one, the input keys a field did not consume are its extras) and
  `serialization_alias=` apart; `validate_by_alias`/`validate_by_name` (`populate_by_name`), `serialize_by_alias`
  (each model's own setting when `by_alias=` is not given). `alias_generator=`: `pydantic.alias_generators`
  (`to_camel`, `to_pascal`, `to_snake`), a lambda or a function of the name made of string operations (`if`,
  `for`, assignments, `return`), an `AliasGenerator(alias=, validation_alias=, serialization_alias=)` of those, or
  `Other.model_config["alias_generator"]`, evaluated at translation time on the field names (a field that declares
  an alias keeps it, the generator fills the others, as pydantic does); anything else is refused. A configuration
  may spread another model's: `ConfigDict(**Base.model_config, ...)`, `{**Base.model_config, ...}`.
- `allow_inf_nan=False` in `model_config` (every float of the model, in its containers too: `finite_number`);
  `validate_default=True`/`False` (every field without its own `Field(validate_default=)`, inherited by
  subclasses; with `True`, a computed default — a lambda or function `default_factory` — is refused).
  `defer_build`, `cache_strings`, `validate_return` and `use_attribute_docstrings` are accepted: they change
  nothing the binary does (`model_json_schema()` refuses such a model).
- Pydantic v1's methods, deprecated but still on v2 models: `Model.parse_obj(obj)`, `Model.validate(value)`,
  `Model.from_orm(obj)` (`PydanticUserError` without `from_attributes=True`), `Model.construct(_fields_set,
  **values)`, `Model.schema()` (no arguments), `Model.parse_raw(str | bytes)` (JSON; a decoding error is a
  `ValidationError` at `__root__`, as pydantic reports it), `Model.update_forward_refs()`, `.dict(...)`,
  `.json(...)` (`model_dump_json` with `by_alias=False`) and `.copy(update=, deep=)`. `parse_file`,
  `parse_raw(content_type=, proto=)` and `.copy(include=, exclude=)` are refused.
- `hide_input_in_errors=True`: `str()` of the model's `ValidationError` without `input_value`/`input_type`
  (`errors()` and the 422 body keep the input, as in pydantic).
- `Field(exclude=True)` (never dumped), `repr=False`, `frozen=True` (an assignment raises `frozen_field`),
  `multiple_of=` (on `int`/`float`, checked before the bounds; a float within `|x| / 1e9`, like pydantic-core),
  `union_mode="left_to_right"`, `discriminator="field"` (a discriminated union, possibly nested in another one:
  the tag picks the member, `union_tag_not_found`/`union_tag_invalid` otherwise; string `Literal` tags only;
  `Discriminator(fn)` with members tagged `Annotated[T, Tag('x')]`, the function called on the input, and its
  `custom_error_type`/`_message`/`_context` of strings),
  `model_config str_min_length=`/`str_max_length=`, `StringConstraints(ascii_only=True)` (pydantic 2.14).
  A str is checked as pydantic-core does: stripped, then `ascii_only`, the lengths and the pattern, and only then
  lowered or uppercased (`str_to_upper` with a lowercase `pattern` accepts lowercase input). The `str_*` and `use_enum_values` settings of a model apply to
  the items and keys of its containers too.
- A bound read from the settings (`Field(max_length=settings.NAME_LEN)`, `Query(le=settings.MAX_PAGE)`, on an
  `int`, `float` or `str`): evaluated when the application starts, where Python evaluates it at import.
- `datetime` and `date` bounds (`Field(gt=datetime(2000, 1, 1))`, `annotated_types.Gt(...)`, a module constant;
  `tzinfo=timezone.utc` or `timezone(timedelta(...))`), `AwareDatetime`, `NaiveDatetime`: checked after the type in
  pydantic-core's order (`le`, `lt`, `ge`, `gt`, then the timezone), compared as speedate does (two aware values
  by their instants, otherwise by their wall times), written in the error as speedate writes them. A bound of
  another type, `PastDate`/`FutureDatetime` and zone-aware bounds (`ZoneInfo`) are refused.
- Strict mode (`model_config strict=True`, `Field(strict=)`), with pydantic-core's python-mode rules (FastAPI
  validates a decoded JSON body as Python data): only the exact type passes, with the type's own error
  (`int_type`, `date_type`, `tuple_type`, `is_instance_of` for `Decimal`/`UUID`/enums...); an `int` is a valid
  `float`. A model's strict config covers its fields' items, not the fields of another model it holds;
  `Field(strict=True)` covers the field's own type (a list's items keep the model's mode). Refused:
  `Field(strict=True)` on a union, `Field(strict=False)` on a container in a strict model. A strict URL refuses
  what the `url` crate reports as a syntax violation (`url_syntax_violation`), as pydantic-core does.
- `Field(default_factory=lambda data: ...)`: called with the fields validated before it; after an error of an
  earlier field it is not called (`default_factory_not_called`; before pydantic 2.14 a `missing` field does not
  count: the factory is called with what was validated, and what it raises is a 500).
- Validators in a field's annotation (`Annotated[T, AfterValidator(f)]`, `BeforeValidator`, `PlainValidator`,
  `WrapValidator`, directly or through a type alias), run like `@field_validator`s defined before the class's:
  a function, a lambda, `info` (`data`, `field_name`, `context`: `None`) in an after validator.
  `@field_validator(mode="plain")` and `PlainValidator` replace the type (and the validators before them);
  `mode="wrap"` and `WrapValidator` get a `handler` that validates against the type (a `ValidationError` with
  locations from the value, title `ValidatorCallable`; re-raised, its errors land under the field). Refused: a
  wrap validator after other validators of the field, anything but `AfterValidator` after a `WrapValidator` in
  the annotation, a validator of an item inside a container.
- `PydanticCustomError(type, template, context)` raised by a validator: its own error (the template filled from the
  context: a str as is, a bool as an int, other values by `repr`); `PydanticUseDefault` raised by a before or
  wrap validator: the field's default (still in `model_fields_set`; no default is pydantic-core's schema error, a
  500). `model_dump(context=)` reaches the serializers' `info.context`.
- Serializers: `@field_serializer(*fields, mode="plain"|"wrap", when_used=)` (instance, class or static method,
  `info` with `mode`, `field_name`, `context`: `None`), `PlainSerializer`/`WrapSerializer` in the annotation,
  `@model_serializer(mode="plain"|"wrap")`, `Field(exclude_if=)`, `model_dump(exclude_defaults=True)` (a field
  with a `@field_serializer` is never left out by it, as in pydantic-core). Serializers run while dumping and
  must not await; their result is dumped by its value's type (the return annotation is not used).
- Types: `tuple[A, B, C]`, `tuple[()]` (`missing` per position, `too_long` alone), `collections.deque[T]`,
  `pydantic.SecretStr` (shown `**********` in `str`, `repr` and JSON dumps, `get_secret_value()`),
  `UUID1`...`UUID8` (`uuid_version`), `fractions.Fraction` (CPython 3.12+'s grammar, exact from a float; dumped as
  its `str()` in both modes), `complex` (CPython's `complex()` grammar; `repr()` like CPython, JSON like
  pydantic-core: `1e20` is written `100000000000000000000`), `Callable` (`callable()`), `SkipValidation[T]` (the
  value as given), `types.EllipsisType` and `MISSING` without a default (no JSON value is one), a plain class of
  the project (`arbitrary_types_allowed`: an `isinstance` check; one defining `__get_pydantic_core_schema__`: see
  below), a type alias assigned after the class that uses it (`MyInt = int` under
  `from __future__ import annotations`), `RootModel[T]` / `RootModel` with `root: T` (the input and the dump are
  the root value; validators, serializers and aliases on it are refused),
  `frozenset[T]`, `collections.Counter[K]`, `collections.OrderedDict[K, V]`, `collections.abc.Sequence[T]` (a
  str refused with `sequence_str`, a tuple stays a tuple), `type[Model]`, `type[BaseModel]`, `type[PlainClass]`
  (`is_subclass_of`). A mutable default evaluated once (`deque()`, a model instance) is copied for each
  instance, like pydantic's `smart_deepcopy`.
- Dataclasses (`dataclasses.dataclass`, `pydantic.dataclasses.dataclass`) as a request body, a parameter, a
  `response_model` or a model's field: validated like pydantic validates a dataclass (typed fields, extra inputs
  ignored, `dataclass_type` for anything but a dict or an instance, an instance kept as it is), into an instance
  of the class (its repr and methods). A stdlib dataclass called in the code does not validate, as in Python;
  calling a pydantic dataclass (which validates its arguments) and its decorator options are refused. A stdlib
  dataclass inside a model is validated with that model's config, as pydantic does (`str_to_lower`,
  `str_strip_whitespace`, `str_to_upper`, `str_min_length`, `str_max_length`, `use_enum_values` reach its
  fields, nested ones included), unless it has its own `@with_config(...)` (those keys only); under a model
  with `strict=` or `extra=` it is refused (pydantic would validate the dataclass itself differently). A
  `typing.NamedTuple` as a validated type is refused (pydantic validates it as call arguments).
- `pydantic.Json` and `Json[T]`: a str or bytes input parsed by jiter, pydantic-core's own parser (its error
  messages and positions: `json_invalid`, `Invalid JSON: expected ident at line 1 column 2`; `inf`/`nan`
  accepted), anything else `json_type`; the parsed value is then validated as `T`. An integer beyond 64 bits in
  it is the runtime's usual difference.
- `__get_pydantic_core_schema__` on a field's type (a classmethod) or on an `Annotated` metadata (an instance of
  the class, a dataclass whose fields are constants, or the class), and `GetPydanticSchema(lambda tp, handler:
  ...)`, on a field's top-level annotation: the hook is read at translation, never run, and the schema it returns
  becomes the equivalent annotation. Supported: `handler(source)`, `handler(int)` (a builtin scalar), the scalar
  schemas with their constraints (`str_schema(max_length=)`, `int_schema(ge=)`...), the after, before, wrap and
  plain validator functions (`no_info_*`/`with_info_*`, with `serialization=`), `chain_schema([schema, plain
  validator...])`, `plain_serializer_function_ser_schema(f)`, assignments, `schema['max_length'] = 10`, and
  `if`/`assert` known at translation (`schema['type'] != 'str'`, a field of the metadata dataclass). The
  functions may be lambdas, functions of the same module, and the class's methods (`cls._validate`,
  `self.validate`, called through a lambda: a traceback shows that `<lambda>` frame first). Refused: unions,
  `is_instance_schema`, `json_or_python_schema`, the hook in a nested type (`list[T]`), `model_json_schema()` of
  such a model, `info_arg=True`, any other statement or constructor. A plain object left in a model field (an
  after validator's result under a scalar type, an `arbitrary_types_allowed` value) fails a JSON dump as in
  pydantic-core (`PydanticSerializationError: Unable to serialize unknown type`, a 500); the serializer warning
  pydantic prints before it is not reproduced.
- Generic models (`class Response(BaseModel, Generic[T])`, `class Response[T](BaseModel)`, a subclass of
  `Parent[T]`): each `Response[int]` used is specialized at translation time, named like pydantic does
  (`Response[int]`, in errors and `type(m).__name__`); the generic class used as such validates a parameter as
  its default, else its bound, else `Any`. The arguments may be written in another module than the model's
  (`Response[Role]` with `Role` imported where it is used); TypeVar constraints are refused.
- JSON dumps (responses, `model_dump(mode="json")`, `model_dump_json`) write a `float` field holding an int
  (a default `0`) as a float, like pydantic-core's typed serializer; `model_fields_set` includes the extras of
  an `extra="allow"` model.
- `TypeAdapter(T)`: `validate_python` (ORM objects with `from_attributes`, iterables), `validate_json`,
  `dump_python`, `dump_json`, for types written in the source or known at run time.
- `EmailStr` (rule-by-rule port of email-validator, same messages, except IDNA encoding and NFC normalization
  of internationalized domains, which are accepted and lowercased), `AnyUrl`/`AnyHttpUrl`/`HttpUrl`/`RedisDsn`/
  `AmqpDsn`/`KafkaDsn`/`MySQLDsn`/`MariaDBDsn`/`CockroachDsn` and the multi-host `PostgresDsn`/`MongoDsn` (parsed with
  the `url` crate like pydantic-core, type defaults, attributes, same errors). A multi-host DSN type holding more than
  one host (`postgresql://h1:5432,h2:5432/db`) is not supported: validating one answers 500, and the multi-host types
  have `hosts()` but no `host`/`port`/`username`/`password`, as in pydantic.
- Strict and constrained aliases: `StrictStr`/`StrictInt`/`StrictFloat`/`StrictBool`/`StrictBytes`,
  `PositiveInt`/`NegativeInt`/`NonNegativeInt`/`NonPositiveInt` (and the `*Float` ones), and the call forms
  `constr(...)`, `conint(...)`, `confloat(...)`, `condecimal(...)`, `conlist(T, ...)`, `conset(T, ...)` translate as the
  `Annotated[...]` they stand for (`unique_items=`, `regex=`, `allow_inf_nan=` are refused). `pathlib.Path` fields
  accept a `str` or a `Path` (`path_type` otherwise) and dump as a `str` in JSON.
- pydantic-settings `BaseSettings`: environment variables (not `.env` files), `validate_default=True`,
  `env_prefix`, `case_sensitive` (names compared case-insensitively by default, the last of two names that
  differ only in case wins), `env_parse_none_str` (an environment value equal to it, case included, is
  `None`; init keywords are not parsed), `env_ignore_empty` (an empty variable is as if unset), JSON values for container/model fields (a union with such a
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
  validated nor dumped; a `_name` assigned on an instance is not dumped either (unless `extra="allow"`). A
  computed default (`PrivateAttr(default_factory=datetime.now)`) is set once the fields are valid.
- `Model = create_model("Model", ...)` at module level: compiled as the class it creates. Fields as `name=type`,
  `name=(type, default)` or `name=(type, Field(...))`, `__base__=` a class, `__doc__=`, and in `__namespace__=`
  (pydantic 2.14+: before, pydantic ignores it, and so does the binary) / `__validators__=` the entries `field_validator(...)(f)`, `model_validator(mode=...)(f)` and
  `computed_field(property(f))` (`f` a module-level function), bound as pydantic binds them. Other options
  (`__config__`, a computed name...) are refused.
- `def model_post_init(self, context)`: called once the fields are valid, after the private attributes' defaults
  and before the after validators, with `context` None; a `ValueError` or an `AssertionError` it raises is a
  validation error of the model (loc `()` of the model, its input), anything else propagates. Inherited.
  An instance of the response model's own class is serialized as returned, private attributes included.

The binary does not read `.env` files: see [Environment variables](../reference/environment.md).
- `model_config` `val_json_bytes=` / `ser_json_bytes=` (`"utf8"`, `"base64"`, `"hex"`): a `str` input of a
  `bytes` field of the model (inside containers too) decoded as pydantic-core does (URL-safe base64, the
  standard alphabet when the input holds `+` or `/`, padding optional; its error messages, which differ
  between pydantic-core 2.46 and 2.50), the JSON dump encoded (URL-safe base64 with padding, lowercase hex).
  A nested model uses its own config.

## What stays in Python

- Not supported: `@computed_field(...)` options, nested `exclude`/`include` dicts,
  core schemas outside the subset above, `create_model`, `ImportString`.
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
- A `Fraction` or `complex` written with non-ASCII digits (CPython reads any Unicode decimal digit) is a 500.
- Arithmetic on a `Fraction` or `complex` value in the application's own code is not translated (a TypeError).

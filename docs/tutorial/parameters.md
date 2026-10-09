# Parameters

FastAPI reads path, query and header parameters from the function signature, converts them to the declared
types and validates them (`Query()`, `Path()`, `Header()` constraints and aliases). The binary does the same,
with FastAPI's 422 error bodies.

The example's router, `docs_src/tutorial/parameters.py`, uses path parameters (an `int`, a `str`
enum), query parameters with defaults, constraints and an alias, and header parameters (a list of values for
a repeated header). Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/tutorial/parameters.py"
--8<-- "docs_src/tutorial/parameters.py"
```

Every route of this file is native: `py2axum check docs_src --root .` lists them as `native`, and a bad value
gets FastAPI's 422, `loc`, `msg` and `input` included.

## What is native

- Path, query, header and cookie parameters (`int/str/bool/float/Enum/UUID/datetime/bytes`, `X | None`, lists —
  a list header parameter takes every occurrence of the header, like `headers.getlist` —,
  `Query/Path/Header/Cookie(...)` constraints and aliases, and `Annotated` constraints from `annotated_types`
  or `StringConstraints`). Errors are FastAPI's 422 bodies, byte for byte.
- `Cookie()` parameters read `request.cookies` (Starlette's lenient parser, first `Cookie` header, last value
  wins), under the parameter's name or `alias`, after the path, query and header parameters as FastAPI does
  (`loc` `["cookie", name]`).
- Models of parameters (`Annotated[Model, Query()]`, `Header()`, `Cookie()`), as FastAPI's
  `request_params_to_args`: each field read under its alias (a header's underscores turned into hyphens unless
  `Header(convert_underscores=False)`; a list field gets every value; an absent field its default when it has
  one), every other received key added (one value, or the list of them), then the dict validated as the model
  at `["query"]`, `["header"]` or `["cookie"]` (`extra="forbid"` then rejects unknown keys).
- `datetime.time` and `datetime.timedelta` read from strings and numbers as Pydantic does (speedate, python
  mode): `HH:MM[:SS[.ffffff]]` times; ISO 8601 durations (`P1Y2M3W4DT5H6M7.5S`, a year is 365 days and a month
  30), `[D day[s][,] ]H:MM[:SS[.f]]`, `D d`, numbers (and booleans) as seconds; the `time_parsing` and
  `time_delta_parsing` messages. Durations are serialised in JSON as Pydantic's ISO 8601 (`PT5M`, `P1Y38D`,
  `-PT1S`), and as seconds through `jsonable_encoder`.
- Timestamps as Pydantic reads them (speedate): numbers and integer strings within years 0000-9999, milliseconds
  above 2e10.

UUID parameters follow the rules of [UUID fields](models.md#what-is-native); the decoded path is used for
routing, as in Starlette ([Bigger applications](bigger-applications.md)).
- An unannotated parameter (`def read_item(item_id)`) is `Any`, as in FastAPI: the raw string from the path or
  the query (the last value of a repeated query parameter). Constraints (`Query(max_length=...)`) without an
  annotation are refused.

## What stays in Python

- Refused at compile time: a model of parameters next to another parameter of the same source, in a route with
  dependencies (FastAPI merges the dependencies' parameters, which changes how the model is read), in a
  dependency, or with a default; an `Annotated` metadata that changes validation or serialisation
  (`AfterValidator`, `BeforeValidator`, `WrapSerializer`, `Discriminator`, a class with
  `__get_pydantic_core_schema__`...).

## Differences

- **Known difference:** a timezone-aware `time` (a `Z` or `±HH:MM` suffix, or a number of seconds, which
  Pydantic reads as UTC) is a 500 (`TypeError`): the runtime's times are naive.
- **Known difference:** a numeric *string* with a fraction or an exponent above 2e10 seconds
  (`"6.958e+16"`, `"253402300800000.0"`) is read by speedate's float-string path with its own scaling; the binary
  applies the number's rules (an error past 9999).

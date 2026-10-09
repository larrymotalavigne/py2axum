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

- Path, query and header parameters (`int/str/bool/float/Enum/UUID/datetime`, `X | None`, lists — a list
  header parameter takes every occurrence of the header, like `headers.getlist` —,
  `Query/Path/Header(...)` constraints and aliases). Errors are FastAPI's 422 bodies, byte for byte.
- Timestamps as Pydantic reads them (speedate): numbers and integer strings within years 0000-9999, milliseconds
  above 2e10.

UUID parameters follow the rules of [UUID fields](models.md#what-is-native); the decoded path is used for
routing, as in Starlette ([Bigger applications](bigger-applications.md)).

## What stays in Python

- `Cookie()` parameters are not supported yet. `request.cookies` is ([Middleware](middleware.md)).

## Differences

- **Known difference:** a numeric *string* with a fraction or an exponent above 2e10 seconds
  (`"6.958e+16"`, `"253402300800000.0"`) is read by speedate's float-string path with its own scaling; the binary
  applies the number's rules (an error past 9999).

# Request body

FastAPI decodes a JSON request body and validates it against one or several Pydantic models (`Body(embed=)`
wraps a single model in a key). The binary decodes the body as CPython's `json` module does, then validates it
like Pydantic, with the same 422 errors.

A JSON body read into one model, into several (`item` and `user` become keys of the body), with a
singular value (`Body(gt=0)`) and with `Body(embed=True)`. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/tutorial/body.py"
--8<-- "docs_src/tutorial/body.py"
```

Invalid JSON, a trailing comma, a missing body or a wrong type give FastAPI's exact error bodies.

## What is native

- JSON bodies (one or several models, `Body(embed=)`).
- JSON bodies (and `json.loads`) are decoded as CPython's `json` does: `NaN`/`Infinity`, encodings detected
  from the bytes (BOM, UTF-16/32), the 422 `json_invalid` with CPython's `ctx.error` and position in code
  points (the trailing-comma messages of 3.13+), undecodable bytes as FastAPI's 400, NaN/inf in a 422's raw
  `input` as Starlette's 500. A `\uXXXX` escape ending the text follows the newest CPython patches
  (unterminated string; before 3.13.13/3.14.4 or so, and on 3.12, "Invalid \uXXXX escape"):
  `PY2AXUM_PYTHON_VERSION=3.14.0` selects the older message.
- A JSON `null` body is no body (missing, or the parameter's default). A body without `Content-Type` is not
  decoded as JSON from FastAPI 0.132 on (`strict_content_type`, its default).
- `FastAPI(strict_content_type=True | False)` (a literal): without it, the default of the locked FastAPI.

How the models themselves validate is described in [Models (Pydantic)](models.md). Request size limits (none by
default, like uvicorn) are in [Security § Request input](../advanced/security.md#request-input).

## What stays in Python

Nothing specific: a body model is refused only for what its fields or validators use
([Models](models.md#what-stays-in-python)).

## Differences

!!! warning "Known differences"
    A lone surrogate escape (`"\ud83d"`) becomes U+FFFD;
    an integer beyond 64 bits raises `OverflowError` (500); nesting deeper than 10 000 levels is FastAPI's 400
    (CPython's limit depends on its C stack, and FastAPI may answer 500 when it encodes a deeply nested `input`).

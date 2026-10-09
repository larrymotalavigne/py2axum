# Handling errors

FastAPI answers `HTTPException` and validation errors with its default handlers, and lets you register your
own with `@app.exception_handler`. The binary keeps Starlette's error-handling layers in the same order, so a
handler catches the same exceptions and an unhandled one ends in the same 500.

`HTTPException` with a detail and headers, a custom exception with its handler, and an unhandled
exception. The handler is registered on the application:

```python title="docs_src/tutorial/errors.py"
--8<-- "docs_src/tutorial/errors.py"
```

```python title="docs_src/main.py (excerpt)"
--8<-- "docs_src/main.py:errors"
```

Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

## What is native

- Starlette's stack: `ServerErrorMiddleware` (the `Exception`/500 handler, whose response bypasses user
  middleware; the exception is still logged) → user middleware (last added runs first) →
  `ExceptionMiddleware` (handlers by status code, then by the exception's MRO; FastAPI's defaults for
  `HTTPException` and `RequestValidationError`) → router (404/405 raised as `HTTPException`, hence catchable).
- `@app.exception_handler(class | code)`, `app.add_exception_handler(class | code, handler)`, also when they are
  registered by a `configure(app)` function ([Middleware](middleware.md#what-is-native)).
- `HTTPException(code)` without `detail`: the `http.HTTPStatus` phrase of CPython ≥ 3.13 ("Content Too
  Large", "Unprocessable Content"), like Starlette on those versions.
- `traceback.format_exception(...)` / `format_exception_only(exc)`: the exception's own line
  (`module.Class: message`), as CPython formats it for a traceback of `None`; `exc.__traceback__` is None
  (the binary has no Python frames), so frames and chained exceptions are not rendered.

Validation errors (`RequestValidationError`, `ValidationError.errors()`) are described in
[Models](models.md); exception classes of the project (class attributes, methods, `super().__init__` of
`HTTPException`) in [Python semantics](../reference/python.md). What the body of a 500 contains is in
[Security § Errors and logs](../advanced/security.md#errors-and-logs).

## What stays in Python

Nothing specific.

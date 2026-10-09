# CORS

FastAPI applications answer cross-origin requests with Starlette's `CORSMiddleware`. The binary runs the same
middleware logic, following the Starlette version your project locks.

`CORSMiddleware` with an allowed origin, credentials, methods and headers; preflight requests from
allowed and other origins are part of the example's conformance check. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/main.py (excerpt)"
--8<-- "docs_src/main.py:cors"
```

## What is native

- `CORSMiddleware` with the behaviour of the Starlette version locked by the project (`uv.lock`: 1.7 adds
  `Vary: Origin` to every response).

It sits in the middleware stack like any other middleware ([Middleware](middleware.md)); the Starlette
versions accepted are listed in [Versions](../reference/versions.md).

## What stays in Python

Nothing specific.

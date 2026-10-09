# Lifespan events

FastAPI runs startup and shutdown code through `FastAPI(lifespan=...)`: an async generator whose code before
`yield` runs before the server accepts requests, and whose code after it runs at shutdown. The binary compiles
the lifespan and runs it at the same points.

The example application creates its tables before the server listens and prints a line once it has
stopped. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/main.py (excerpt)"
--8<-- "docs_src/main.py:lifespan"
```

## What is native

- `FastAPI(lifespan=...)` with an `async def` generator of the project (decorated with
  `@contextlib.asynccontextmanager` or not, like Starlette): the code before `yield` runs before the server
  listens (a failure prints `Application startup failed. Exiting.` and exits with code 3, like uvicorn),
  the code after it once the server has stopped on SIGTERM/SIGINT (see [Graceful shutdown](../advanced/shutdown.md)).

Creating the tables at startup with `await conn.run_sync(Base.metadata.create_all)` is supported: see
[SQL databases](sql.md#what-is-native).

## What stays in Python

- Lifespan state (`yield {...}`) is refused.
- `--python-side lifespan` leaves the whole lifespan to the Python process of a hybrid deployment, where it
  runs instead of in the binary ([Hybrid mode](../getting-started/hybrid.md)).

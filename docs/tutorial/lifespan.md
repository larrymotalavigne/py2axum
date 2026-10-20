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
- A lifespan the app factory defines (`def create_app(): ... async def lifespan(app): ...`, under an `if` even):
  compiled with the factory's locals, used only when the `if`s around `FastAPI(lifespan=...)` hold at startup.

Creating the tables at startup with `await conn.run_sync(Base.metadata.create_all)` is supported: see
[SQL databases](sql.md#what-is-native).

## What stays in Python

- Lifespan state (`yield {...}`) is refused.
- `--python-side lifespan` leaves the whole lifespan to the Python process of a hybrid deployment, where it
  runs instead of in the binary ([Hybrid mode](../getting-started/hybrid.md)).
- With `--python-side auto`, a lifespan that does not translate (an APScheduler scheduler, a library the binary
  does not map) runs in the Python process alone; a translated route or middleware reading what it changes goes
  to Python with it, or the whole application when a middleware does. A `state` attribute the lifespan only
  initialises before `yield` (`app.state.count = 0`) and that translated code reads with that value as
  `getattr`'s default does not count: the binary's process reads the same.

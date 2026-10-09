# Dependencies

FastAPI's dependency injection (`Depends`) builds what an endpoint needs (a database session, the current
user, common parameters) before calling it, caches each dependency for the request, and runs the cleanup code
of `yield` dependencies afterwards. The binary resolves the same graph, in the same order.

A function dependency shared through an `Annotated` alias, a dependency required by every route of
a router (`dependencies=[...]`), a class dependency and a `yield` dependency whose cleanup runs once the
request is over. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/tutorial/dependencies.py"
--8<-- "docs_src/tutorial/dependencies.py"
```

!!! note "Left to Python in this example"
    `GET /dependencies/fruits` depends on a plain class (`Depends(Pagination)`, or `Depends()` with the class
    as annotation): py2axum only accepts project functions, Pydantic models (`Depends()`) and the session
    dependency there, so `py2axum check` reports it and `--python-side auto` relays it. Write the dependency
    as a function returning the object to keep it native.

## What is native

- `Depends(...)` with project functions, sub-dependencies, the per-request cache,
  `Annotated[T, Depends(...)]` aliases, `dependencies=[...]` on routes and routers.
- `p: Model = Depends()` with a Pydantic model (FastAPI calls the class): one query parameter per field, with its
  type, default and `Field` constraints (`ge`, `le`, `max_length`, `pattern`...), then `Model(**fields)`.
- `yield` dependencies: one `yield`, in the body or alone in a `try/finally`. The code after `yield` runs at
  the end of the request, most recent dependency first, before the session commit (FastAPI runs it after
  sending the response); on error, only `finally` blocks run.
- The session dependency: `async with maker() as s: yield s` with optional `await s.commit()`,
  `except: await s.rollback(); raise`, `finally: await s.close()`; the `async_sessionmaker` options
  (`expire_on_commit`, `autoflush`) are read from it, or from a project class wrapping it. The commit after
  `yield` runs after the endpoint, like FastAPI ≥ 0.121 (a failure is only logged); the binary commits just
  before sending a complete body (FastAPI just after): same response, without the read-after-write race.

Synchronous session dependencies are described in [SQL databases](sql.md#what-is-native), security schemes
used as dependencies in [Security](security.md).

## What stays in Python

- A plain class as a dependency (`Depends(MyClass)`, or `Depends()` with the class as annotation) is refused:
  only project functions, Pydantic models and the session dependency are accepted.
- `p: Model = Depends()`: a base other than `BaseModel`, validators or other decorated methods, `model_config`,
  aliases, container fields (FastAPI reads those from the body) are refused.
- `yield` inside `try/except` is refused (FastAPI raises the endpoint's exception there).

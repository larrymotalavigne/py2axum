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
- `yield` dependencies: one `yield`, in the body or in a top-level `try` (`except`, `else`, `finally`). The
  code after `yield` runs at the end of the request, most recent dependency first, before the session commit.
  An exception of the request (the endpoint's, a later dependency's, a validation error) is raised at the
  `yield`, as FastAPI does: an `except` clause sees it, what it raises replaces it (an `HTTPException` becomes
  the response), and an exception caught and not raised again is FastAPI's `FastAPIError` ("Response not
  awaited...", a 500).
- `Depends(dep, scope="function")` (FastAPI ≥ 0.121): the exit code runs when the endpoint returns, before the
  response is sent, and an exception there is the request's; the default scope, `"request"`, runs it once the
  response is sent (for a streamed response, at the end of the stream). The cache key includes the scope.
- An instance of a project class with `__call__` as a dependency (`checker = Checker("bar")`,
  `Depends(checker)`): its parameters are those of `__call__`, sync or async.
- `app.dependency_overrides[dep] = other` written at module level (a project function overridden by a project
  function): `other` is solved and called wherever `dep` is depended on, with its own parameters.
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
- A request-scoped `yield` dependency depending on a `scope="function"` one is refused (FastAPI's
  `DependencyScopeError`), as is a non-literal `scope=`.
- `app.dependency_overrides` with anything else than a project function on both sides, or changed anywhere
  else than at module level, is refused (an override set by a test, in-process, does not reach the binary).

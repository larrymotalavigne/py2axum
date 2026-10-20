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

## What is native

- `Depends(...)` with project functions, sub-dependencies, the per-request cache,
  `Annotated[T, Depends(...)]` aliases, `dependencies=[...]` on routes and routers.
- `p: Model = Depends()` with a Pydantic model (FastAPI calls the class): one query parameter per field, with its
  type, default and `Field` constraints (`ge`, `le`, `max_length`, `pattern`...), then `Model(**fields)`.
- `yield` dependencies: the `yield` is a statement, in the body or under `if`, `try` (any clause) and
  `with`/`async with` blocks; several only in exclusive branches of an `if` (a path without one is contextlib's
  `RuntimeError: generator didn't yield`). Refused in a loop or twice on one path (contextlib's "generator didn't
  stop", raised outside the generator). The
  code after `yield` runs at the end of the request, most recent dependency first, before the session commit.
  An exception of the request (the endpoint's, a later dependency's, a validation error) is raised at the
  `yield`, as FastAPI does: an `except` clause sees it, what it raises replaces it (an `HTTPException` becomes
  the response), and an exception caught and not raised again is FastAPI's `FastAPIError` ("Response not
  awaited...", a 500).
- `Depends(dep, scope="function")` (FastAPI ≥ 0.121): the exit code runs when the endpoint returns, before the
  response is sent, and an exception there is the request's; the default scope, `"request"`, runs it once the
  response is sent (for a streamed response, at the end of the stream). The cache key includes the scope.
- A plain project class, `Depends(Pagination)` or `p: Pagination = Depends()`: its parameters are those of the
  first `__init__` along its bases (`ABC`, `object` and at most one plain project class each, `metaclass=ABCMeta`
  allowed), as `inspect.signature` reads them (in the module of the class defining it); a `__new__` or a base of a
  library is refused.
- `Depends(Service.async_init)`: a `@classmethod` (its parameters without `cls`) or a `@staticmethod` of a plain
  project class, found along its bases (`Sub.async_init` of a classmethod of `Base` gets `Sub` as `cls`).
- `Depends(lambda: manager)`: the lambda's parameters, one function per lambda (its own cache key).
- In an application factory (`def create_app(): ...`), a dependency function or an `Annotated` alias
  (`Auth = Annotated[Claims, Depends(claims)]`) bound once at the factory's top level, before the endpoints that
  read it: compiled with the factory's locals, like the endpoints defined there.
- An instance of a project class with `__call__` as a dependency (`checker = Checker("bar")`,
  `Depends(checker)`): its parameters are those of `__call__`, sync or async.
- A dependency built by a factory, `Depends(make_dep(Query([], alias="id"), int))`: FastAPI reads the signature of
  the function the factory returns, whose defaults and annotations hold the factory's arguments. The factory may
  only define functions and return one of them (undecorated), or only return another factory's call
  (`def require_admin(): return require_role("admin")`); a `*roles` parameter holds the tuple of the extra positional
  arguments. Its arguments are constants, builtin types and
  FastAPI markers of constants, naming the same objects where the call is and where the factory is; inside, a
  marker is only read as a parameter default or through its `.default`, and no factory parameter is reassigned.
  Anything else is refused at its line.
- `app.dependency_overrides[dep] = other` written at module level (a project function overridden by a project
  function): `other` is solved and called wherever `dep` is depended on, with its own parameters.
- The session dependency: `async with maker() as s: yield s` with optional `await s.commit()`,
  `except: await s.rollback(); raise`, `finally: await s.close()`; the `async_sessionmaker` options
  (`expire_on_commit`, `autoflush`) are read from it, or from a project class wrapping it (its module instance
  can also be called outside a dependency, `get_worker_session()` -> `WRAPPER()`, for a session of its own: the
  `connect_args` options of the engine it builds, `-c statement_timeout=60s`, are set in each of that session's
  transactions; a TimeZone there, or `connect_args=` on the request session's maker, is refused). The commit after
  `yield` runs after the endpoint, like FastAPI ≥ 0.121 (a failure is only logged); the binary commits just
  before sending a complete body (FastAPI just after): same response, without the read-after-write race.
  Also accepted: `s = maker()` (annotated or not, possibly the first statement of the `try`) then
  `try: yield s ... finally: [await] s.close()`; `raise exc` of the caught exception instead of `raise`; the maker
  read into a local first (`factory = get_session_factory()`, `async with factory() as s:`); a maker returned by a
  project function (`SessionLocal = make_sessionmaker(engine)`, literal options); the maker an instance of a
  project class keeps (`async with db_manager.session_maker() as s:`), set once by a statement of its `__init__`
  or of a method the application's lifespan calls before its `yield` (`await db_manager.initialize()`). A maker
  set conditionally, twice, from outside its class or by a method nothing calls at startup is refused at its line.

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

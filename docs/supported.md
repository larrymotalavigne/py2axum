# Supported subset and known differences

py2axum translates a closed subset of Python and of a list of libraries. Anything outside it is refused at
compile time with `file:line` (and counted by `--report`), so a construct is either translated with the
behaviour described here or not translated at all. Each item below is covered by conformance cases that
compare the binary with the Python application, response by response.

"Differences" are the places where the binary is knowingly not identical to CPython; they are listed so you
can decide whether they matter for your application.

Two backends exist: the **dyn** backend (`--backend dyn`, the general one, described here) and a small
**typed** backend that emits statically-typed Rust for simple CRUD handlers (see the end of this page).
`--backend auto` uses the typed backend when it covers the whole application, else dyn.

## Applications, routes, parameters

- `FastAPI()` apps, `APIRouter(prefix=, tags=, dependencies=)`, `include_router`, routers split across
  modules, apps built by a factory function (`create_app()`), including endpoints defined inside it; a
  local of the factory assigned once is evaluated once. `if` statements around registrations are evaluated
  at startup in the factory's scope (e.g. `if not settings.TESTING:`).
- Imports are resolved statically, without executing code: relative imports, re-exports from `__init__`,
  lazy imports inside functions (visible from nested functions and lambdas), `--root` as `sys.path`.
- Routing like Starlette: declaration order, decoded path, first match on path and method wins, then 405
  with the methods of the first path match, then a 307 redirect with/without the trailing slash, then 404.
  No implicit HEAD. The redirect uses `http://` (as uvicorn does without trusted proxy headers).
- Path, query and header parameters (`int/str/bool/float/Enum/UUID/datetime`, `X | None`, lists,
  `Query/Path/Header(...)` constraints and aliases), JSON bodies (one or several models, `Body(embed=)`),
  `Form()`/`File()`/`UploadFile` (single, optional or list; multipart via `multer` or urlencoded; an empty
  string counts as absent like FastAPI; `UploadFile` in memory: `read`, `seek`, `filename`,
  `content_type`, `size`, `.file`). Errors are FastAPI's 422 bodies, byte for byte.
- An invalid JSON body gives the same 422 `json_invalid` (type, message, position) but `ctx.error` is
  serde_json's message rather than CPython's `json` module message (which itself varies across versions).
- `Cookie()` parameters are not supported yet.

## Dependencies and security

- `Depends(...)` with project functions and classes, sub-dependencies, the per-request cache,
  `Annotated[T, Depends(...)]` aliases, `dependencies=[...]` on routes and routers.
- `yield` dependencies: one `yield`, in the body or alone in a `try/finally`. The code after `yield` runs at
  the end of the request, most recent dependency first, before the session commit (FastAPI runs it after
  sending the response); on error, only `finally` blocks run. `yield` inside `try/except` is refused (FastAPI
  raises the endpoint's exception there).
- The session dependency: `async with maker() as s: yield s` with optional `await s.commit()`,
  `except: await s.rollback(); raise`, `finally: await s.close()`; the `async_sessionmaker` options
  (`expire_on_commit`, `autoflush`) are read from it, or from a project class wrapping it. The commit after
  `yield` runs after the endpoint, like FastAPI ≥ 0.121 (a failure is only logged); the binary commits just
  before sending a complete body (FastAPI just after): same response, without the read-after-write race.
- `OAuth2PasswordBearer`, `HTTPBearer` (behaviour of FastAPI 0.142: 401 `Not authenticated`,
  `WWW-Authenticate: Bearer`), `OAuth2PasswordRequestForm`. Other schemes (`APIKeyHeader`, `HTTPBasic`...)
  and a security scheme in `dependencies=[...]` are refused.

## Responses

- `response_model` (filtering, aliases, `exclude_unset/none`), status codes, returned dicts, lists, models,
  ORM objects (`from_attributes`).
- Returned `Response`, `JSONResponse`, `PlainTextResponse`, `HTMLResponse`, `RedirectResponse`,
  `FileResponse` are sent like Starlette 1.7 (header order, `ETag`/`Last-Modified` of `FileResponse`;
  no `Range`/partial `HEAD`). `JSONResponse(content=...)` serializes like Starlette (strict `json.dumps`:
  a model, a datetime or NaN raise). `set_cookie`/`delete_cookie` in `http.cookies` format, headers set on
  the injected `Response`.
- `jsonable_encoder` semantics for returned values (models by alias, `bytes.decode()`, `Decimal` via
  FastAPI's `decimal_encoder`).
- `StreamingResponse` and async generators (SSE). `await request.is_disconnected()` always returns `False`:
  a disconnected client is noticed at the next send, then the generator's `finally` runs.
- `BackgroundTasks`: run after the response (an error is logged), not after an error response.
- `HTTPException(code)` without `detail`: the `http.HTTPStatus` phrase of CPython ≥ 3.13 ("Content Too
  Large", "Unprocessable Content"), like Starlette on those versions.

## Middleware and exception handlers

- Starlette's stack: `ServerErrorMiddleware` (the `Exception`/500 handler, whose response bypasses user
  middleware; the exception is still logged) → user middleware (last added runs first) →
  `ExceptionMiddleware` (handlers by status code, then by the exception's MRO; FastAPI's defaults for
  `HTTPException` and `RequestValidationError`) → router (404/405 raised as `HTTPException`, hence catchable).
- `CORSMiddleware` with the behaviour of the Starlette version locked by the project (`uv.lock`: 1.7 adds
  `Vary: Origin` to every response), `GZipMiddleware` (`Vary: Accept-Encoding` like Starlette),
  `BaseHTTPMiddleware` subclasses (one instance per app, built on the first request; `call_next`, mutable
  `response.headers`), `@app.middleware("http")`, `@app.exception_handler(class | code)`.
- `request.cookies` (Starlette's parser), `request.client` (TCP peer), `request.url`, `request.headers`,
  `request.state`, `request.body()`/`json()`.

## Pydantic v2

- Lax-mode validation with pydantic-core's error types, messages, locations and contexts (speedate for
  dates), smart unions (exactness, fields set), enums, literals, nested models, lists/sets/tuples/dicts,
  `Optional`, `Field` constraints, aliases (`populate_by_name`), defaults and `default_factory`,
  `validate_assignment`, `extra=`, `from_attributes`, `str_strip_whitespace`/`to_lower`/`to_upper`,
  `use_enum_values`, `model_config` as a dict or `ConfigDict`, v1 `class Config` (v1-only keys ignored like
  Pydantic v2 does).
- `@field_validator` / `@validator` (after and before, including `_x = field_validator(...)(lambda v: ...)`),
  `@model_validator` (before/after); validators that raise produce the 422 at their place. Before
  validators run on the raw input in reverse definition order, like pydantic-core; a union containing such a
  model is refused. A union whose branch has a `@field_validator` picks the branch on types only, then runs
  its validators (Pydantic would try the next branch if they raise).
- `model_dump(mode=, by_alias=, exclude_none=, exclude_unset=, exclude=, include=)` (top-level field names
  for `exclude`/`include`), `model_dump_json`, `model_validate(_json)`, `model_copy(update=, deep=)`,
  `model_fields_set`, `model_fields`, `ValidationError.errors()` (URL with the locked pydantic version,
  `include_*` options) and `error_count()`.
- `model_config frozen=True` (assignment raises `frozen_instance`; frozen models hash by value),
  `Field(validate_default=True)`, field options given in `Annotated[T, Field(...)]`.
- `TypeAdapter(T)`: `validate_python` (ORM objects with `from_attributes`, iterables), `validate_json`,
  `dump_python`, `dump_json`, for types written in the source or known at run time.
- `EmailStr` (rule-by-rule port of email-validator, same messages, except IDNA encoding and NFC normalization
  of internationalized domains, which are accepted and lowercased), `AnyUrl`/`AnyHttpUrl`/`HttpUrl`/`RedisDsn`
  (parsed with the `url` crate like pydantic-core, type defaults, attributes, same errors).
- pydantic-settings `BaseSettings`: environment variables (not `.env` files), `validate_default=True`;
  a class body run like a script (class-level `if`, attributes reading earlier ones) is evaluated once.
- Not supported: `@computed_field`, `@field_serializer`/`@model_serializer`, `PrivateAttr`, nested
  `exclude`/`include` dicts, strict mode.

## SQLAlchemy 2.0 (async, PostgreSQL)

- Declarative models (`Mapped[...]`, `mapped_column`, types, `default=`/`server_default=`/`onupdate=`
  (value, callable or SQL), `unique`, `nullable`, `ForeignKey` (a foreign key without a type takes the
  referenced column's), composite primary keys, `Identity()`, `JSON`/`JSONB` (`none_as_null=`),
  `Numeric` (`Decimal`, or float with `asdecimal=False`), `Enum` columns, project `TypeDecorator`s
  (`process_bind_param`/`process_result_value` without `self`/`dialect`).
- Session: identity map (weak, like SQLAlchemy), autoflush, implicit transaction, `get` (scalar, tuple,
  list or dict identities), `add`/`add_all`/`delete`/`flush`/`commit`/`rollback`/`refresh`/`close`,
  `expire_on_commit`, savepoints (`begin_nested()` then `commit()`/`rollback()`), `session.bind`,
  `get_bind()`. Server-generated values (identity, `server_default`, SQL defaults) are fetched with
  `RETURNING` at insert, like `eager_defaults="auto"`.
- Relationships (many-to-one, one-to-many), `lazy=` select/selectin/joined/noload/raise, `selectinload()`
  chains, `back_populates`/`backref`, cascades (save-update, delete, delete-orphan), `passive_deletes`.
  Difference: `parent.children.append(x)` sets the foreign key at flush but not `x.parent` before it
  (SQLAlchemy does it immediately through the backref event).
- Core: `select` (entities, columns, labels, `*cols`), `where`/`filter_by`, joins (explicit, inferred from
  the single foreign key, relationship), `aliased`, subqueries, `exists`, `in_` (lists, selects, `tuple_`),
  `like/ilike/startswith/contains`, `is_/is_not`, `is_distinct_from`, `case`, `literal`, `cast`, `extract`,
  `func.*` (with `FILTER`), `group_by/having/order_by/limit/offset/distinct(on)`, `with_for_update`,
  `update()`/`delete()` (with `synchronize_session`), `insert()` (core and postgresql dialect: several rows,
  Python column defaults, `on_conflict_do_update(index_elements=, set_=, where=)`, `on_conflict_do_nothing`,
  `excluded`, `returning`), `text()` with `:named` parameters, `Result.scalars/all/first/one/scalar/unique/
  mappings`, rows with attribute access (`row.total`, `_mapping`, `_asdict()`).
- SQL typing like SQLAlchemy: arithmetic and `FILTER` keep the column type, `func.round`/`avg` untyped
  (Decimal); untyped integers are bound as int2/int4/int8 like psycopg; NUMERIC results are `Decimal`.
- The PostgreSQL session time zone: sqlx forces UTC, the runtime applies the one psycopg would see
  (role/database setting, then server config, or `PY2AXUM_DB_TIMEZONE`).
- `obj.__dict__` of a mapped object: `_sa_instance_state` then the loaded attributes (a snapshot).
- `create_async_engine(...)` is the binary's pool (one database, `DATABASE_URL`; its options are ignored);
  `async with engine.connect() as conn` (rolled back on exit).

## Python semantics

- Values and operators with CPython's semantics: int (64-bit: beyond is an `OverflowError`), float formatting
  and `repr`, str methods, `%`/`format`/f-strings, slicing, comparisons, `**`, bit operators, truthiness,
  `hash()` rules (unhashable Pydantic models and dataclasses unless frozen, `__hash__ = None`, `__eq__`
  without `__hash__`); `hash(int)` is CPython's, other hashes are stable but not CPython's (CPython
  randomizes str hashes anyway).
- Functions: keyword/default/`*args`/`**kwargs` binding with CPython's `TypeError`s, closures, lambdas,
  nested functions and decorators (`functools.wraps`, decorator factories; a decorated function is built
  once at startup), recursion, `global` (one cell per process), generators, `match`.
  Refused: `nonlocal`, a project decorator on a method.
- Classes: plain classes (`__init__`, methods, properties, static/class methods, class attributes,
  `__slots__`), `@dataclass` (incl. `frozen=True`, `__post_init__`), exceptions (class attributes,
  methods, `super().__init__` of `HTTPException`), Enums (methods, `_missing_`). Special methods:
  `__str__`, `__repr__`, `__eq__` (used by `str()`, f-strings, `==`, `in`, `index`, `count`, `remove`),
  `__enter__/__exit__`, `__aenter__/__aexit__`; they must not perform I/O (`def`); other special methods
  are refused. In containers, an exception raised by `__eq__` counts as "not equal" (CPython propagates it).
- Types as values: `list[X]`, `X | None`, library classes (`BaseModel`, `AsyncSession`...),
  `isinstance`/`issubclass` with run-time types, `inspect.isclass`, `typing.get_args/get_origin/
  get_type_hints` (annotations kept on decorated functions).
- Module globals are evaluated at startup in import order, like importing the app; module-level calls too.
- `map`/`filter` return lists (materialized); `frozenset` behaves as `set`.

## asyncio and threading

- A call of an `async def` that is not awaited is a coroutine object (arguments evaluated at the call,
  body run at the first `await`; a second `await` raises like CPython). `asyncio.gather` (concurrent tasks,
  argument order, `return_exceptions`), `create_task` + `await task`, `add_done_callback`, `Semaphore`,
  `Lock`, `Queue`, `wait_for`, `sleep`, `to_thread`. The runtime is multi-threaded: two tasks finishing at
  the same instant have no guaranteed order (asyncio follows creation order).
- `async with` / `with` follow CPython's protocol (the exception is passed to `__exit__`, a true result
  suppresses it).
- `threading.Lock/RLock/Event/Thread/get_ident`, `asyncio.new_event_loop()` + `run_forever()` in a
  thread, `call_soon`, `run_coroutine_threadsafe` + `wrap_future`, `loop.run_in_executor(None, f)`: emulated
  on tokio. The event loop is one "thread" (all coroutines share its ident); `Thread` and executor calls run
  on their own OS threads. Locks block like CPython's.
- `asyncio.run()` raises CPython's `RuntimeError` (always inside a running loop). Library calls that are not
  awaited (a bare `asyncio.sleep(1)`) run immediately.

## Standard library

`datetime` (with time zones, `zoneinfo`, Pydantic and `isoformat` formats, `strptime` without `%f`/`%Z`,
`combine`, `fromtimestamp`), `decimal` (`_pydecimal` rules: 28 digits, ROUND_HALF_EVEN, exact
add/mul then rounding, CPython's division, `quantize`, `round`, formatting; mixing with float refused),
`uuid`, `json`, `re` (fancy-regex with Python syntax translated; consecutive empty matches follow Rust, not
Python 3.7+), `csv` (simplified `Sniffer`), `io.StringIO/BytesIO`, `pathlib`/`open()` (UTF-8, POSIX),
`os.environ`/`os.getenv` (read-only), `os.path`, `math`, `random` (OS-seeded), `secrets`, `hashlib`,
`hmac`, `base64`, `urllib.parse`, `string` constants, `time.time/monotonic/perf_counter`,
`statistics.median`, `logging` (stderr, `LEVEL:logger:message`, level from `PY2AXUM_LOG_LEVEL`),
`email.mime`/`email.utils`, `pickle` (see below), `functools.wraps`, `inspect.iscoroutinefunction`,
`typing.get_args/get_origin/get_type_hints`. Not yet: `collections.defaultdict`/`Counter`, `string.Formatter`.

`pickle.dumps/loads` use CPython's format (protocol 5 when the project targets Python ≥ 3.14, else 4):
bytes written by the binary are read by CPython and the other way round (e.g. a Redis cache shared with
Python workers). Supported: scalars, str/bytes, containers (shared references kept), datetime types,
time zones, `Decimal`, `UUID`, enums, project class instances (`__dict__` or `__slots__`), dataclasses,
Pydantic models. Refused: mapped (ORM) objects, other globals, out-of-band buffers. Integers beyond 64 bits
read back as integral `Decimal`s.

## Libraries

| Library | Scope |
|---|---|
| httpx (0.28) | `AsyncClient`, requests, timeouts, `raise_for_status`, exceptions; no redirects followed, no `files=`, cookies or custom transports |
| aiohttp (3.14) | `ClientSession` (`timeout=None` = default timeouts), `async with session.post(...) as resp`, `ssl=False`, `proxy=None`, `status/reason/text()/json()`, `ClientTimeout`; `reason` is the standard phrase; a per-request `timeout=None` keeps the session's timeout |
| yarl | `URL(str)`: `host`, `port` (scheme default), `scheme`, `path`, `query_string`, `fragment`, `user`, `password`, `str()` |
| redis.asyncio (redis-py 5+) | `from_url`/`Redis(...)`, get/set (ex, px, nx, xx, get)/setex/delete/exists/incr/decr/mget/expire/ttl/keys/scan_iter/hash commands/ping, `Retry(backoff, n)` (retries without the backoff delay), redis-py's encoding and exceptions |
| python-jose (3.5) | `jwt.encode/decode` with HMAC algorithms and decode options; identical tokens, same exceptions |
| bcrypt (5.0), pyotp (2.9) | same hashes and codes |
| itsdangerous (2.2) | `URLSafeTimedSerializer` with the default signer and serializer |
| cryptography | `Fernet` (tokens readable both ways) |
| jinja2, aiosmtplib, email | templates (minijinja with Jinja2's output), MIME messages, SMTP sending |
| pywebpush (2.3) | aes128gcm encryption, VAPID (py-vapid rules), `WebPushException` |
| google-auth (2.49) | `id_token.verify_oauth2_token` / `verify_token` |
| alembic | `alembic.config.Config` ini reading only |
| psutil (7) | `cpu_percent`, `virtual_memory`, `disk_usage`, `pids` |
| sentry-sdk | behaves like a never-initialized SDK (calls are no-ops) |

## Not supported

WebSockets, `lifespan` and raw ASGI apps (declare them `--python-side`), libraries not listed, C extensions,
`eval`/`exec`, metaclasses, multiple inheritance of project classes, OpenAPI `/docs` in the binary.

## The typed backend

For applications made only of simple CRUD handlers, `--backend typed` emits statically-typed Rust (structs,
`FromRow`, static SQL) and can stream large list responses straight from PostgreSQL in 64 KiB chunks with
constant memory (`--no-stream` disables it; `PY2AXUM_STREAM_CHUNK`, `PY2AXUM_STREAM_MIN_ROWS`). Its
subset is narrower: no relationships, no `datetime`/`UUID`/`Decimal`, `AsyncSession` and
`aiohttp.ClientSession` dependencies only, `GZipMiddleware` as the only middleware.

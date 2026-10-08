# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.0] — 2026-10-08

- `py2axum check <package>`: every route marked native, python-side or refused with the reason at `file:line`,
  the blockers ranked by routes touched, without generating a crate; `--json`, and an exit status for CI
  (1 when generation would fail, `--fail-under PCT` on the share of native routes).
- Supported version ranges (FastAPI 0.137–0.142, Starlette 1.0–1.7 (1.7 for WebSocket routes), Pydantic 2.12–2.13, pydantic-settings
  2.11–2.15, SQLAlchemy 2.0.44–2.1, psycopg 3.2.12–3.3, httpx 0.28, aiohttp 3.13–3.14, mcp 2.2, Python
  3.12–3.14), tested at both ends in CI. A project locking or pinning a version outside them (`uv.lock`,
  `requirements*.txt`, `pyproject.toml`) is refused with `file:line`; `--allow-untested-versions` overrides.

- `await conn.run_sync(Base.metadata.create_all)` (dyn backend), e.g. in the lifespan: the DDL is compiled at
  translation time by SQLAlchemy from the mapped classes rebuilt statically (no project code runs), then replayed
  with SQLAlchemy's checkfirst (enum types, tables, indexes, comments, foreign keys of cycles).
  `async with engine.begin() as conn` commits on exit. `--python-side lifespan` is no longer needed for it.
- WebSocket routes (dyn backend): `@app.websocket` / `@router.websocket` with dependencies and path/query/header
  parameters, Starlette's `WebSocket` (accept, receive/send in every form, close, `iter_*`, denial responses,
  states), uvicorn's handshake outcomes (403, 500, 1006), exception handlers called with the WebSocket;
  `async for` over async iterators. The typed backend refuses `@router.websocket` (it was skipped silently).
- `x: Annotated[T, Query()/Header()] = default` is optional (it was required); `request.url.path` is
  percent-decoded, as Starlette builds it from `scope["path"]`.

- `sentry_sdk` reports to Sentry (it was a no-op): a native client on the Rust SDK sends the same events as the
  Python SDK (scopes per request, FastAPI/Starlette and logging integrations, transactions, scrubbing,
  `before_send`), each tagged `py2axum.source` with the route's `file.py:line`. Options the binary cannot
  reproduce (`transport=`, `before_breadcrumb=`, profiling...) are refused. An app factory's `init_sentry()`
  runs at startup. `tests/sentry_check.py` compares what a fake Sentry server receives.

- `dateutil.relativedelta` (relative fields), `xmltodict.parse` (default options, namespaces) with
  `ExpatError`, Pydantic private attributes (`PrivateAttr`, `_name`),
  `session.connection()` and its `close()` (readiness probes), `__getattr__`/`__setattr__` on plain classes
  (`object.__setattr__`), enum members with list/tuple values (`str` mixin: `("A",)` is `"A"`), a module-level
  endpoint shadowed by a later function of the same name.

- Applications served next to Starlette: `response_class=` wraps the returned value (HTML, plain text, redirect,
  file); `uuid.UUID` validated as pydantic-core does and `Uuid` columns read and written as `uuid.UUID`;
  `HTTPBasic`; `Jinja2Templates(directory=)` and `TemplateResponse`; `importlib.metadata.version("literal")`
  resolved at compile time from `pyproject.toml` and `uv.lock`. An app wrapped in a project ASGI class
  (`app = Middleware(api)`) is refused (it was served without the wrapper).
- `app.mount(...)` registered last can stay on the Python side (`--python-side mount`, or `auto`).
- Synchronous SQLAlchemy (already in 0.2.0, not listed there): `sessionmaker`, a synchronous generator
  dependency, real lazy loads (also while a `response_model` reads the object), the legacy `session.query` API.
  Now also: self-referential relationships (`remote_side=`), bound parameters shared by an expression reused in
  `GROUP BY`, ORM `@property`s read by a response model, `await` on a synchronous `Session` (CPython's
  `TypeError`).
- `model_json_schema()` computed at translation time; `sqlalchemy.inspect(x, raiseerr=False)`;
  `unicodedata.normalize/combining`; `json.dumps(sort_keys=, indent=)`; `bytes.decode` and bytes methods;
  `BaseModel.dict()`; `request.base_url`; `join(subquery, on)`; `zip(strict=)`; `date/datetime.replace` and their
  methods read as values; f-string format specs through `__format__`; class attributes read on a plain or
  Pydantic class; `Annotated[T, Header()] = default`; python-side routes under a prefix read from the settings.
- `collections.deque` and `defaultdict(deque)`; single inheritance of plain classes, `abc.ABC` and
  `@abstractmethod`; `nh3` (`clean`, `clean_text`, `is_html`) on the `ammonia` crate; `socket.getaddrinfo`,
  `ipaddress.ip_address`/`ip_network`; `email.mime.base.MIMEBase` and `email.encoders.encode_base64`;
  `Fernet.generate_key()`; `string.Template` (`substitute`, `safe_substitute`).
- `break`/`continue` leaving a `try/finally` or a `with`: the `finally` (or `__exit__`) runs first, as in CPython.

### Fixed

- Length constraints of `list`/`set`/`tuple`/`dict` fields and keyword arguments of builtins were ignored
  silently: applied as pydantic-core does, refused otherwise. A validator's error carries the raw input.
- `email.encoders.encode_base64()` of a message without payload raises CPython's `TypeError` (it encoded nothing).
- An app factory's `init_sentry()` is refused only for the functions it reaches (functions queued by other code
  were blamed on it).

### Changed

- `sentry_sdk` is no longer a no-op (see above): a project whose `init_sentry()` passes `transport=` (a Python
  transport class) is refused at translation time; it used to be skipped silently.

## [0.2.0] — 2026-10-07

- `FastAPI(lifespan=...)` is translated: the code before `yield` runs before the server listens, the rest after a
  graceful shutdown on SIGTERM/SIGINT (`PY2AXUM_SHUTDOWN_TIMEOUT`). Async generators run in lockstep with their
  consumer (`anext`, `asend`, `athrow`, `aclose`), `@asynccontextmanager`, `contextlib.suppress`,
  `contextvars.ContextVar`, `Task.cancel()`.
- Raw ASGI routes: `app.add_route(path, obj)` with an object whose class defines `async __call__(scope, receive,
  send)`; `Request(scope, receive)`, response objects called as ASGI apps.
- MCP servers (`mcp` 2.2): `MCPServer`, `@server.tool` (async, structured `dict[str, Any]` output), `ToolError`,
  streamable HTTP without state and with JSON responses, the JSON-RPC envelope validated like the SDK. Tool
  argument schemas are computed at compile time (pydantic 2.13 JSON schema, `py2axum/jsonschema.py`).
- `str(ValidationError)` in pydantic-core's format where the model is known.

- Replacing `aiohttp.ClientSession._request` / `httpx.AsyncClient.request` with a wrapper (timing outgoing calls):
  the clients call it; other assignments to library attributes are refused.

- A project function given the app in the factory (`configure(app)`) runs while the middleware stack is built:
  `app.add_middleware` of project `BaseHTTPMiddleware` subclasses, `app.add_route` (it was skipped).
  Routing objects as values: `request.app`, `app.router.routes`, `route.matches(scope)`, `Match`,
  `request.scope`. `timeit.default_timer`.

- `--python-side auto`: every route that does not translate (and raw `add_route` routes with a literal path) is
  left to the Python application, each printed with the error that blocked it.

- Module-level `try` / `if` / `for` / `while` / `with` statements run at startup (they were skipped), their
  bindings becoming module variables; import fallbacks (`try: import x except ImportError: ...`).

- `sys.settrace` / `threading.settrace_all_threads`: the project's functions report call/return/exception
  events to the trace function (function call metrics); `traceback.extract_tb`. Module-level functions are
  single objects; nested functions may name themselves.
- prometheus_client: `start_http_server`, OpenMetrics output, exemplars, name escapings, restricted registries.
- httpx / aiohttp responses keep their `Content-Encoding` header (the body is decoded by the runtime, not reqwest).
- prometheus_client multiprocess mode: the per-process files (shared with Python workers) and
  `MultiProcessCollector`, `mark_process_dead`.
- prometheus_client 0.26: `Counter`, `Gauge`, `Summary`, `Histogram`, `Info`, `Enum`, labels, `time()`,
  `count_exceptions()`, `track_inprogress()`, `CollectorRegistry`, `REGISTRY`, `generate_latest` (text format byte
  for byte).

- Typed backend: request bodies above 2 MB are read like Starlette does (axum answered 413).

## [0.1.0] — 2026-10-07, first public release

- Ahead-of-time compilation of FastAPI + SQLAlchemy 2.0 (async, PostgreSQL) + Pydantic v2 applications to
  Rust (axum, tokio, sqlx): the **dyn** backend (general) and the **typed** backend (simple CRUD).
- Runtime reproducing CPython, Pydantic v2, SQLAlchemy and Starlette semantics, plus a closed list of
  standard and third-party libraries ([docs/supported.md](docs/supported.md)).
- `--report`: per-route coverage report with the first blocking construct (`file:line`).
- `--python-side`: paths and `lifespan` left to the Python application, relayed by the binary when
  `PY2AXUM_PYTHON_URL` is set.
- Differential conformance harness (`tests/conformance.py`) and reference applications.
- `pip install`-able package with a `py2axum` command; example application with a multi-stage Dockerfile.
- aio-pika 10: connect, channels, durable queues, publishing with confirms, `queue.get()`.
- `json.loads` accepts bytes (UTF-8/16/32 detection, like CPython).
- `importlib.import_module` of a literal project module (module objects), `callable()`.
- `collections.defaultdict` (builtin type factories), `string.Formatter().vformat/format`; `str.format`
  gains `!r`/`!s` and CPython's error messages.
- tenacity 9 (`@retry` and its strategies). `g`/`e`/`G`/`E`/`X`/`o`/`b` presentations and `#` in `format()`
  and `%`; `ConnectionError` and its subclasses; `logging` level constants and `Logger.log`.

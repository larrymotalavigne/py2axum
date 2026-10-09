# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.6.0] — 2026-10-09

FastAPI documentation corpus at 90.6 %, raw ASGI middlewares three times cheaper, error codes and `py2axum
watch`, a builder Docker image, and the constructs the remaining routes of real applications needed.

### Performance

- Raw ASGI middlewares: the cost per layer drops from ~2.7 µs to ~0.85 µs. The binary advances the middleware's
  future itself instead of spawning a task per layer (code left after the response, or a client gone, still
  finishes in its own task, never aborted), the ASGI channels are a queue plus a waker, and a known-length body is
  forwarded without a oneshot. On the reference app with 11 layers, `POST /describe` goes from ~24k to ~40k req/s.

### Added

- Error codes: every refusal has a stable code (`P2A0201`: library call not supported, ...) and the command line
  prints it with the `file:line`, why the construct is refused, what to do and a link to its entry in the new
  [error code reference](https://larrymotalavigne.github.io/py2axum/reference/errors/). `py2axum check --explain
  CODE` prints the same in a terminal.
- `py2axum check`: a suggestion per refused route (the alternative py2axum knows, the `--python-side` flag that
  leaves it to Python), a summary of what would make the most routes native, `--format text|json|markdown`
  (`--json` stays a synonym).
- `py2axum watch`: regenerate, rebuild (debug profile, only the generated crate) and restart the binary on each
  change of the project, with debounce; a failed cycle keeps the previous binary running.
- Guide: [Migrating an existing application](https://larrymotalavigne.github.io/py2axum/getting-started/migration/),
  from `check` to a reversible switch in production.

- Coverage of the FastAPI documentation corpus: 79.6 % → 90.6 % of the exercised examples identical, none
  differing ([coverage](https://larrymotalavigne.github.io/py2axum/coverage/)). Newly native:
  `yield` dependencies with `try/except` around the `yield` (the request's exception is raised there, as FastAPI
  does; one swallowed is `FastAPIError`), `Depends(scope="function")`, an instance with `__call__` as a
  dependency, `app.dependency_overrides[f] = g` at module level, generator endpoints (JSON Lines,
  `EventSourceResponse` with `ServerSentEvent`, `StreamingResponse`), `response_model_include=` /
  `_exclude=` / `_exclude_unset=` / `_exclude_none=`, a `Response` class as the return annotation,
  `FastAPI(default_response_class=)`, `FastAPI(root_path=)`, unannotated parameters (`Any`),
  `fastapi.exception_handlers` defaults called from an application's handler, `TrustedHostMiddleware`,
  `HTTPSRedirectMiddleware`, `model_config` `val_json_bytes`/`ser_json_bytes`, pwdlib's Argon2
  `PasswordHash.recommended()`, `anyio.sleep`.

- SQLAlchemy's JSON index and its typed accessors, on `JSON` and `JSONB` columns: `col["k"]` / `col[0]` with
  `as_string()`, `as_integer()`, `as_float()`, `as_boolean()`, `as_numeric(precision, scale)`, `as_json()`
  (`CAST(col ->> 'k' AS VARCHAR)`, as SQLAlchemy's PostgreSQL compiler renders it); `sqlalchemy.true()` and
  `false()`. A JSON path index, `col[("a", "b")]`, is refused at translation.

- Constructs of real applications: Pydantic `serialization_alias=` / `validation_alias=` (distinct input and
  output names; `AliasChoices`/`AliasPath` refused), closures with cells as CPython builds them (late binding,
  comprehensions included) and `nonlocal`, `TypedDict` called as a `dict` (refused as a `response_model`),
  `Model.__table__` (columns and their real SQLAlchemy types, computed at translation), `StaleDataError` when a
  flushed `UPDATE` matches no row, `html.unescape` with CPython's HTML5 tables, icalendar 7.0
  (`Calendar`/`Event`/`Alarm`, `add`, `add_component`, `to_ical`, byte-identical output), `os.remove` /
  `os.unlink`; the runtime's `OSError`s carry `errno`, `strerror`, `filename` and the subclass of their errno.
- SQLAlchemy: `with engine.connect()` / `engine.begin()` on a synchronous `create_engine`, `executemany` through
  `execute(text(...), [dicts])`, `||` for a string `+` on SQL expressions; `execute/scalars/scalar(stmt, params)`
  outside `text()`, which failed at run time, is refused at translation with `file:line`.
- asyncpg 0.32 in the supported range.
- Docker: `ghcr.io/larrymotalavigne/py2axum`, a builder image (Python 3.14, py2axum, a pinned Rust toolchain and the
  runtime's crates precompiled with the release profile), multi-arch, with SBOM and provenance; a multi-stage
  example whose final image holds the binary alone ([Docker image](https://larrymotalavigne.github.io/py2axum/advanced/docker/)).
- SQLAlchemy documentation corpus: the doctests of the 2.1.4 documentation replayed as routes against the binary
  (480 examples, none differing; the starting point of the 0.7 work).

### Fixed

- A value stored in a `JSON` column (not `JSONB`, which PostgreSQL normalizes) was written as compact JSON,
  `{"a":1}`, where SQLAlchemy writes `json.dumps`' text, `{"a": 1}`; `->>` on an object or a list returned the
  other text.
- A request-scoped `yield` dependency of a streamed response closed before the stream instead of after it
  (without a session dependency).
- An `HTTPException` with a no-body status (204, 304) lost its `headers=`.
- Generator endpoints under FastAPI < 0.141 follow that version: JSON Lines and Server-Sent Events responses
  ignore `status_code=` (200), and the items of a route declared on an `APIRouter` are neither validated nor
  filtered by their `AsyncIterable[T]` type (`include_router` drops it), as FastAPI does.
- `x in y` on a value that is no container raises Python 3.14's message ("is not a container or iterable")
  when the project runs on 3.14.

## [0.5.1] — 2026-10-09

### Fixed

- **itsdangerous `loads(..., return_timestamp=True)` answered 500.** The keyword was accepted at transpile time and
  refused only at run time, so magic-link and 2FA logins built on it failed in production. `URLSafeTimedSerializer.loads`
  now takes `s, max_age, return_timestamp, salt` positionally or by keyword and returns `(payload, datetime)` with an
  aware UTC datetime, like itsdangerous 2.2; `BadSignature.payload`, `BadTimeSignature.date_signed` and
  `SignatureExpired.date_signed` are set as the library sets them. Any other argument is refused at transpile time
  with `file:line`.
- `isinstance(x, int | float)` (and `X | None`, `Optional[X]`, `Union[X, Y]`) raised "cannot be a parameterized
  generic" in the binary: a 2FA login of a production app answered 500. A union is now tested member by member, as
  CPython 3.10+ does.
- Arguments the runtime dropped or ignored are now implemented or refused at transpile time (an audit of the
  library map after the bug above):
  - `str.encode(encoding, errors)` always produced UTF-8; it now implements utf-8, utf-8-sig, latin-1 and ascii with
    `errors=` strict, ignore or replace (`UnicodeEncodeError` as CPython). `str(b, encoding)` decodes like
    `bytes.decode` instead of lossy UTF-8.
  - `str.find/rfind/index/rindex/count/startswith/endswith(sub, start, end)` and `list/tuple.index(x, start, stop)`
    ignored their bounds; `set.update(*others)` read only the first; `Match.groups/groupdict(default=)` ignored the
    keyword.
  - Starlette responses given positional arguments read `FileResponse`'s `filename` and `content_disposition_type` at
    wrong indexes and dropped a positional `background`; `background=`, `stat_result=` and `method=` are refused.
  - `MIMEApplication(data, Name=...)`, `MIMEMultipart(..., **params)` dropped their Content-Type parameters;
    `policy=`, `boundary=`, `_subparts=`, `_encoder=` and arguments of `as_string()`/`as_bytes()` are refused.
  - `Path.read_text/write_text(encoding=, errors=, newline=)` were ignored (and `read_text` decoded lossily).
  - csv: `DictReader(restval=, restkey=)` were ignored; a positional dialect is refused.
  - redis: `scan_iter("pattern:*")` ignored a positional pattern and scanned every key; `_type=` and
    `set(px=timedelta)` were dropped.
  - `model_validate_json(strict=...)` ignored its keywords; class methods such as `Model.model_validate(...)` now go
    through the transpile-time keyword check.
  - HTTP client responses (`resp.text(encoding=)`, `resp.json(loads=)`...), SQL `col.like(pattern, escape)` given
    positionally, a second positional of `deque` methods and `Pattern.search(s, pos)` raise instead of ignoring the
    argument.

## [0.5.0] — 2026-10-09

### Security

- **Denial of service fixed (all earlier versions):** a small request carrying a deeply nested JSON body could
  overflow the stack of a generated binary and abort the process, without authentication. The decoder is now
  iterative (400 past 10 000 levels, as CPython), the runtime threads get a larger stack (`PY2AXUM_STACK_SIZE`),
  and recursive walks raise `RecursionError` instead of overflowing, so the worst outcome is a 500, never an
  abort. Upgrading is strongly recommended.
- Security review of the runtime before 1.0: threat model, guarantees and differences with uvicorn/Starlette in
  the new [docs/security.md](https://larrymotalavigne.github.io/py2axum/advanced/security/). Hardening of SQL identifiers given as strings, request input
  handling (nesting, sizes, multipart limits, cookies), `Path.resolve()`, response headers, bcrypt off the event
  loop, the `--python-side` relay and the multiprocess Prometheus files. Upgrading is recommended.
- New settings: `PY2AXUM_MAX_BODY` (optional 413 on large bodies, off by default) and `PY2AXUM_STACK_SIZE`.
- CI audits the runtime's `Cargo.lock` with `cargo audit` (`tools/cargo_audit.sh`).

### Added

- Documentation site (MkDocs Material, <https://larrymotalavigne.github.io/py2axum/>), organised like FastAPI's:
  getting started, tutorial, advanced, reference, release notes. Its examples (`docs_src/`) are tested against the
  binary in CI.
- pydantic 2.14 / pydantic-core 2.50 supported (range `pydantic>=2.12.0,<2.15`, `pydantic-core>=2.41.1,<2.51`). The
  binary reproduces the minor the project locks: error URLs `…/2.14/v/…`; UUID messages of the uuid crate 1.23.4
  (0-based positions, `invalid length: found N`, the requested form); `EmailStr` refusing CR/LF; `Decimal`
  `max_digits`/`decimal_places` counted without `Decimal.normalize()`'s 28-digit rounding; an Enum `_missing_`
  that raises something other than a `ValueError` propagates. Projects on 2.12/2.13 keep their messages. See
  docs/supported.md, "Pydantic behaviours that follow the project's minor version".
- The project's library version is also read from an `==` pin in `requirements*.txt` / `pyproject.toml` (only
  `uv.lock` was read), or chosen inside the project's specifier when the installed one is outside it.
- CI: the `versions` matrix runs pydantic 2.12 (min), 2.13 (`python -m py2axum.versions pydantic2.13`) and 2.14 (max).

### Fixed

- `x is Enum.MEMBER` between Enum members was always `False`, and a header parameter `x: list[str] = Header()`
  answered 422 instead of collecting every occurrence: two silent wrong answers found by the documentation's
  examples.
- `model_config`'s `str_strip_whitespace` / `str_to_lower` / `str_to_upper` were ignored on `EmailStr` fields.
- An Enum `_missing_` raising a `ValueError` gave a `value_error` instead of pydantic's `enum` error; returning a
  value that is neither `None` nor a member gave the `enum` error instead of Enum's `TypeError`.
- `Decimal` digit bounds compared only the normalized value: a value whose written form fits passes, as in pydantic.
- Refused instead of mistranslated: a `default_factory` taking the validated data (`lambda data: ...`, called
  without arguments before), length constraints on `Iterable[T]`.

## [0.4.0] — 2026-10-08

### Breaking

- **The typed backend is removed** (`--backend typed`, `codegen.py`, `body.py`, `runtime/rt.rs`). It emitted
  statically-typed Rust for simple CRUD handlers only and covered none of the real applications measured; every
  application already went through the general backend. `--backend typed` now exits with an error
  ("removed in 0.4: use the default backend"); `--backend auto` and `--backend dyn` are accepted until 1.0 and
  change nothing. `py2axum check` and `--report` no longer try it first; `check --json` keeps `"backend": "dyn"`.

### Added

- Large list responses streamed at constant memory (what only the typed backend did): an endpoint with
  `response_model=list[Schema]` returning `(await session.execute(stmt)).scalars().all()` (or `session.scalars`,
  or through a local) reads the rows with a cursor and sends the JSON array in 64 KiB blocks — 57 MB served with
  13–16 MiB of RSS instead of 1.6 GiB, first byte after 16 ms instead of 2.9 s, identical bytes. Only when
  nothing observable changes (the session wrote nothing, one mapped class without loader options, a schema of
  plain columns...); `PY2AXUM_STREAM_CHUNK`, `PY2AXUM_STREAM_MIN_ROWS` and `--no-stream` keep their meaning. See
  docs/supported.md, "Large list responses".
- `app.state` (set in the lifespan, read through `request.app.state`).
- Raw ASGI middleware: a project class with `__init__(self, app, ...)` and `async def __call__(self, scope, receive,
  send)`, registered with `app.add_middleware(...)` or `FastAPI(middleware=[Middleware(...)])`, runs natively in
  Starlette's stack around the router (shared scope, uvicorn's `receive`, `send` chunked without a
  `content-length`, rewritten scope as a new request, wrapped `receive`/`send`, short-circuit answers, exceptions
  raised through the stack). `starlette_context`'s `RawContextMiddleware` with `RequestIdPlugin` and
  `CorrelationIdPlugin`, `FastAPI(strict_content_type=)` and `traceback.format_exception(...)` /
  `format_exception_only(exc)` are translated too. An application without raw middleware gets no extra layer;
  each one costs about 2.5 µs per request. See docs/supported.md.
- `create_engine(connect_args=...)` / `create_async_engine(connect_args=...)` applied to every pooled connection:
  libpq `options` (`-c name=value`, `-cname=value`, `--name=value`, split like libpq, `\` escapes) and asyncpg's
  `server_settings`. A `TimeZone` setting overrides the time zone discovered from the server, so timestamps decode
  in the session's zone as psycopg does (they kept the server's zone before). Any other literal libpq option is
  refused at transpile time.
- PyJWT 2.15 (`import jwt`) translated natively, like python-jose: `jwt.encode` (`algorithm=`, `headers=`,
  `sort_headers=`; `exp`/`iat`/`nbf` datetimes encoded from a copy), `jwt.decode` / `decode_complete` (every
  `options=` key, `audience=`, `issuer=`, `subject=`, `leeway=` as a number or a timedelta),
  `get_unverified_header`, with HS256/HS384/HS512 and `none`: identical tokens, the same checks in the same order,
  the `jwt.exceptions` hierarchy and messages. An asymmetric algorithm written as a literal, `json_encoder=`,
  `detached_payload=`, `verify=` and `PyJWK`/`PyJWKClient` are refused at transpile time.

### Changed

- Assigning another module's attribute (`config.LIMIT = 2`) is refused at transpile time with `file:line`; the
  binary used to fail at startup.
- `json.loads(bytes)` with invalid UTF-8 raises CPython's exact `UnicodeDecodeError` message.
- `JSONB` columns: `contains` (`@>`), `contained_by` (`<@`), `has_key` (`?`), `has_any` (`?|`) and `has_all`
  (`?&`) are compiled to PostgreSQL's operators, their argument typed as SQLAlchemy types it. They raised a
  `TypeError` at run time (a 500 where Python answered).
- A path left to Python (`--python-side`) answers FastAPI's 404 when `PY2AXUM_PYTHON_URL` is unset, as
  documented; it used to fall through to a translated route whose pattern also matched (`/books/export.zip`
  reached `/books/{book_id}` and answered 422).
- An exact pin equal to the lowest version a feature needs is accepted: `starlette==1.7.0` was refused for
  WebSocket routes ("need starlette>=1.7.0").
- `tests/conformance.py --scenario path/to/scenario.py` (and `tests/difftest.py`) load a scenario file from
  anywhere, so an application can keep its scenario in its own repository.
- New example, `examples/bookshelf`: users, JWT Bearer authentication (PyJWT), CRUD, relationships, 422s, a
  WebSocket and one route left to Python; its conformance scenario (75 steps) and `compare.sh` run in CI.
  `examples/docker`: a reference multi-stage Dockerfile, the Python sidecar and a Compose file for the hybrid
  mode. `bench/bench.py --target bookshelf` measures it against FastAPI.
- Documentation: a getting-started guide (`docs/getting-started.md`), the conformance guide rewritten for
  users, the README, `docs/supported.md` and `docs/how-it-works.md` reviewed for first-time readers.
- Translated natively: `validate_assignment=True` with field and model
  validators (before, type, after with `info.data`, model `after`, as pydantic-core; refused with a model
  `before` validator), `Field(min_items=, max_items=)`, `File(default=[])`, a SQL column name other than the
  attribute (`Column("metadata", JSON)`), the `undefer()` loader option (top level, `session.get(options=)`,
  `selectinload(...).options(undefer(...))`), `exists().where(...)` with SQLAlchemy's auto-correlation,
  `regexp_match`, `delete().returning(columns)`, `...` as a sentinel value, `collections.Counter`,
  `asyncio.open_connection` (asyncio's or uvloop's messages, after the project's lock),
  `socket.create_connection`, `cryptography` RSA key generation and serialization (DKIM), and
  `zipfile.ZipFile(io.BytesIO(), "w")` with CPython's bytes.
- A pydantic `ValidationError` raised by an assignment carries the model's `title` (and `str(e)` names it).
- A crate generated from an application with `GZipMiddleware` failed `cargo build --locked`: the shipped
  `Cargo.lock` did not list `tower-http` among the root crate's dependencies. It is now always declared.
- Fixed: a list of `UploadFile` holding an empty string was treated as absent; FastAPI reports it (422).
  `str.splitlines()` split on `\n` and `\r\n` only (CPython's line boundaries now: `\r`, `\v`, `\f`,
  `\x1c`-`\x1e`, `\x85`, U+2028/2029); iterating an `io.BytesIO` (a `StreamingResponse` over one) raised.

## [0.3.1] — 2026-10-08

- An Enum member given to a scalar field is validated as pydantic-core does: an ORM enum column read into
  `status: str` answered 500 (`string_type`) where Python returned the member's value (found in production). A
  `str`/`int` subclass member is its value for `str`, `int`, `float`, `bool` and `Literal` (an `int` one is
  `str(value)` in a `str`); a plain member is `str(value)` in a `str`, its value unchecked in an unconstrained `int`.
- Graceful shutdown in every generated binary (it only existed with a lifespan or `sentry_sdk`; otherwise
  SIGTERM killed in-flight requests): as uvicorn 0.54, new connections refused, idle keep-alive connections
  closed, in-flight requests finished with `connection: close`, WebSocket sessions closed with 1012 on both
  sides, a second signal forces the exit, and the process ends by the signal it received (status 143, uvicorn
  re-raises it; it was 0). The pool is closed before the exit.
- Memory leaked at every call (dyn backend), found by the endurance test: an MCP `tools/call` (its arguments'
  validator), `column.op("...")(value)` (the operator) and `json.dumps(separators=...)` (the separators). Each
  is now built once per distinct value; a test forbids `Box::leak` outside those caches.
- `bench/bench.py --target dynapp --compare`: a performance guard comparing a generated binary with a reference.
- Differential testing (`tests/difftest.py`, extra `difftest`): `replay` replays traffic recorded on the Python app
  (`py2axum.record`, an optional ASGI recorder writing anonymised JSONL) against both servers from the same
  database snapshot, with ddmin minimisation; `gen` generates valid and invalid requests from the OpenAPI schema
  (schemathesis 4 + hypothesis, fixed seed) plus a corpus of edge cases per operation.
- asyncpg (0.31) as the database driver, read from `DATABASE_URL`; `on_conflict_do_nothing/update(constraint=)`.
- `BaseHTTPMiddleware` without its own `__init__`; `deferred()` columns; Jinja2 `env.filters[name] = f`;
  `p: Model = Depends()` (one query parameter per field); `text(...).bindparams()`; `super()` in a model's
  classmethod; `aiosmtplib.send(sender=, recipients=)`; `desc("label")` and labels in GROUP BY / ORDER BY;
  `update(...).returning(Model)` refreshes the session's objects; `session.refresh()` keeps loaded relationships;
  `StreamingResponse` over a synchronous iterable.

### Fixed

- A route reading a module global filled by a function that does not translate (a registry filled in place by
  module-level statements: a loop, `REG[k] = lambda: f()`) was reported native and the error lost: it is now
  blocked (`--python-side auto` moves it). Module-level item and attribute assignments run at startup; an
  assignment to an unmapped library's attribute (`stripe.api_key = ...`) stays on the Python side.
- Request bodies decoded as CPython's `json.loads` does (BOM, UTF-16/32, NaN, exact positions and messages,
  too deep nesting); a `null` body is absent; FastAPI's `strict_content_type`; Starlette's form limits and
  multipart error message follow the locked Starlette version (1.4, 1.7); Starlette's redirect slashes.
- Pydantic: bool from float/Decimal/bytes, int from str, `int_parsing_size` and `bool_type` beyond 64 bits,
  timestamps parsed as speedate does, empty URL; `exclude_none` on extras; `response_model` serialised through
  `dump_json` (NaN and infinities become `null`).
- Dates: years 1–9999 (`OverflowError`), `timedelta.days`, `date()`/`datetime()` checks and messages of CPython
  3.12/3.14; `format()` of `-0.0`, `nan`, `inf`; Python whitespace U+001C–U+001F; `str.is*()` from the
  translating Python's Unicode data; paths containing NUL.
- SQL: an integer out of the column's range raises `DataError` as psycopg does, the exact psycopg class per
  SQLSTATE, `tuple_(...).in_(...)` without a cast; JSON columns keep float round-trips.
- `request.url.path` without tabs and newlines; AMQP names validated as pamqp does (no panic); a status code
  outside 100–599 drops the connection as uvicorn does.

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
  standard and third-party libraries ([docs/supported.md](https://larrymotalavigne.github.io/py2axum/supported/)).
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

# Supported subset and known differences

py2axum translates a closed subset of Python and of a list of libraries. Anything outside it is refused at
compile time with `file:line` (and counted by `--report`), so a construct is either translated with the
behaviour described in these pages or not translated at all. Method calls and attribute reads on values only
known at run time are checked by name: a name that no runtime type implements and that the project never
defines is refused (a name that exists on another type than the actual receiver is not detected). Each item of
these pages is covered by conformance cases that compare the binary with the Python application, response by
response.

"Differences" are the places where the binary is knowingly not identical to CPython; they are listed so you
can decide whether they matter for your application.

New to py2axum? Start with the [getting-started guide](getting-started/index.md);
[`py2axum check`](getting-started/check.md) tells you what these pages mean for your application, route by
route. To check the binary against your application, see [Conformance and replay](advanced/conformance.md).

There is one backend since 0.4 (the statically-typed one was removed: it covered only simple CRUD handlers;
its list streaming moved to [Large list responses](advanced/streaming.md)). `--backend auto` and
`--backend dyn` are still accepted until 1.0 and change nothing; `--backend typed` is an error.

## Where each part is described

| Topic | Page |
|---|---|
| Supported versions | [Versions](reference/versions.md) |
| Python 3.12 to 3.15: what each version changes, `sys.version_info` | [Python versions](python-versions.md) |
| Applications, routers, factories, imports, routing, mounts, raw ASGI routes | [Bigger applications](tutorial/bigger-applications.md) |
| Path, query and header parameters, timestamps | [Parameters](tutorial/parameters.md) |
| JSON bodies, JSON decoding, `strict_content_type` | [Request body](tutorial/body.md) |
| `Form()`, `File()`, `UploadFile` | [Forms and files](tutorial/files.md) |
| Dependencies, the session dependency | [Dependencies](tutorial/dependencies.md) |
| Security schemes (OAuth2, HTTP Bearer, HTTP Basic) | [Security](tutorial/security.md) |
| Responses, response classes, status codes, `StreamingResponse`, generator endpoints (JSON Lines, Server-Sent Events) | [Responses](tutorial/responses.md) |
| `BackgroundTasks` | [Background tasks](tutorial/background-tasks.md) |
| Exception handlers, `HTTPException`, Starlette's error layers | [Handling errors](tutorial/errors.md) |
| Middleware (`BaseHTTPMiddleware`, raw ASGI, GZip, starlette_context), request attributes, `configure(app)` | [Middleware](tutorial/middleware.md) |
| `CORSMiddleware` | [CORS](tutorial/cors.md) |
| Shutdown (SIGTERM, SIGINT) | [Graceful shutdown](advanced/shutdown.md) |
| `logging`, python-json-logger, uvicorn's own lines (startup, access log, errors), tracebacks, uvicorn's command-line options | [Logging](advanced/logging.md) |
| WebSockets | [WebSockets](tutorial/websockets.md) |
| Pydantic v2 | [Models (Pydantic)](tutorial/models.md) |
| SQLAlchemy 2.0 (async and sync, PostgreSQL) | [SQL databases](tutorial/sql.md) |
| Python semantics, asyncio and threading | [Python semantics](reference/python.md) |
| Lifespan | [Lifespan events](tutorial/lifespan.md) |
| Sentry (`sentry-sdk` 2.x) | [Sentry](advanced/sentry.md) |
| Prometheus (`prometheus_client` 0.26) | [Prometheus metrics](advanced/metrics.md) |
| MCP servers (`mcp` 2.2) | [MCP servers](advanced/mcp.md) |
| Standard library, `pickle` | [Standard library](reference/stdlib.md) |
| Libraries | [Libraries](reference/libraries.md) |
| Hybrid deployments | [Hybrid mode](getting-started/hybrid.md) |
| Large list responses | [Large list responses](advanced/streaming.md) |

## Runtime environment

The binary takes uvicorn's command-line options and prints uvicorn's lines and the application's `logging`
records as Python does ([Logging](advanced/logging.md); a traceback holds the project's frames only).
It reads its configuration from the environment ([the list](reference/environment.md));
it does not read `.env` files (pydantic-settings reads environment variables only, see
[Models](tutorial/models.md)): export the variables, as an orchestrator does.
Limits on untrusted input (an optional body cap, the stack size) and the differences they imply are in
[Security § Request input](advanced/security.md#request-input).
Object addresses in reprs (`<X object at 0x...>`) are the binary's own; their width matches CPython's on 64-bit
Linux, not on macOS, where a truncated repr in the logs is cut elsewhere ([Python](reference/python.md)).

## Deeply nested JSON on Python 3.14+

From Python 3.14, `json.loads` is bounded by the C stack instead of the recursion limit: a document nested
beyond 10 000 levels decodes in CPython (up to roughly 50 000 levels on an 8 MiB stack, then
`RecursionError: Stack overflow (used ... kB) while decoding a JSON array from a unicode string`). The binary
keeps the 10 000-level limit of 3.12 and 3.13 on every Python (`RecursionError: maximum recursion depth exceeded
while decoding a JSON array from a unicode string`), a bound on untrusted input
([Security](advanced/security.md)). Such a document usually exceeds the recursion limit later in Python
(FastAPI's `jsonable_encoder` when it is returned): the response is the same 500, the logged traceback differs
(where the exception is raised, and its message).

## PyYAML

`yaml.safe_load` and the dump functions follow PyYAML 6.0's pure-Python implementation (its `SafeLoader`, which
`safe_load` always uses, even when libyaml is installed). Where the binary cannot follow it:

- An integer beyond 64 bits (`9223372036854775808`, `!!int` of a long digit string) raises
  `OverflowError: py2axum: the YAML integer ... does not fit in 64 bits`, as `json.loads` does; CPython returns a
  Python int. A digit string past 4 300 digits raises CPython's own `ValueError` (the int-conversion limit).
- A `\uD800`-`\uDFFF` escape in a double-quoted scalar (a lone surrogate) becomes U+FFFD: a Rust string cannot
  hold it (CPython keeps it in the str, which FastAPI then fails to encode as JSON: a 500).
- The nesting depth at which the recursive steps raise `RecursionError: maximum recursion depth exceeded` is fixed:
  a collection nested 471 levels deep loads, one 472 deep does not; a list nested 316 deep dumps, one 317 deep
  does not. CPython's limit depends on the stack of the caller (those are its depths from an async endpoint under
  uvicorn with Python 3.12; a sync endpoint, another Python or a deeper call chain moves them).
- `yaml.dump` (the full `Representer`) writes the builtin types as PyYAML does (`!!python/tuple`,
  `!!python/complex`, `!!set`, `!!binary`...); a value it would write as `!!python/object...`,
  `!!python/name:...` or `!!python/object/apply:...` (a project class, a Pydantic model, an ORM object, an Enum
  member, `Decimal`, `UUID`, `time`, `timedelta`...) raises `TypeError: py2axum: yaml.dump() of a ... object is not
  supported`. `yaml.safe_dump` raises PyYAML's own `RepresenterError('cannot represent an object', data)` for them.
- Anchors follow object identity: a list, dict, set or tuple met twice is written once with an anchor, as in
  CPython; a `date` or `datetime` object met twice is not (the binary's dates are values, without identity), and
  an equal tuple written twice in the source is two objects in the binary where CPython may fold the constants
  into one.
- The order of a `set` (`!!set`, `sort_keys=False`) is its insertion order, not CPython's hash order (which varies
  from one process to the other for strings).
- `encoding=` accepts UTF-8, UTF-16, UTF-32 (with or without `-le`/`-be`), ASCII and Latin-1 (`TypeError` for
  another codec); a text file read by `safe_load` is decoded as UTF-8 with universal newlines.
## croniter

croniter 6.2 is ported line by line ([Libraries](reference/libraries.md)); what the binary does not reproduce:

- `R` (random) fields: Python draws a random number. A literal expression containing one is refused at compile
  time; one built at run time raises `NotImplementedError` (py2axum) where Python would draw the number (after
  the errors croniter raises before it), `is_valid` included.
- `for x in croniter(...)` and `for x in cron.all_next()` / `all_prev()` (or `list()` of them) raise a py2axum
  `TypeError`: these iterators are endless and iteration is materialised; read them with `next()`.
  `croniter_range()` is a lazy generator (`next()`, `for`, `list()`), but a `for` loop computes every value of the
  range before its first iteration.
- `croniter.expand()` of an expression holding an integer of more than 4300 digits: Python's
  `CroniterBadCronError` carries the formatted traceback of the `ValueError`, the binary its message only.
- The `CroniterUnsupportedSyntaxError` message prints each nth set with its numbers first, then `'l'`; Python's
  order of a set mixing both depends on the string hash seed (`PYTHONHASHSEED`).
- Time zones: `datetime.timezone` and `zoneinfo.ZoneInfo` (pytz and dateutil zones are not translated); the
  zone rules are the runtime's tz database, Python's those of the system or of the `tzdata` package.
- A start time that `gmtime` cannot represent raises the platform's `OSError` (`[Errno 75]` on Linux).
- `croniter_range(_croniter=)`, `croniter.expand(from_timestamp=)` and `is_valid(encoding=)` other than UTF-8
  are refused at compile time; a start time other than a `datetime`, an `int` or a `float`, and `croniter_range`
  bounds other than datetimes and numbers raise a py2axum `TypeError`.

## email.message.EmailMessage

The header parser and folder of `email.policy.default` are ported with the behaviour of each supported CPython
release ([standard library](reference/stdlib.md)); what the binary does not reproduce:

- `msg[name]` returns the header's text as a `str`, not a header object (`.addresses`, `.defects`, `.name`
  are not available; assigning one message's header to another parses its text again).
- An encoded word (`=?charset?q?...?=`) in a header value whose charset is not UTF-8, Latin-1 or ASCII, or whose
  bytes do not decode with it, raises a py2axum `TypeError`: Python keeps undecodable bytes as surrogate escapes.
- Header values are `str`, `None`, or for address headers a list of `str`; other values (header and `Address`
  objects, numbers) raise a py2axum `TypeError`.

## dnspython

dnspython 2.8's stub resolver is ported ([Libraries](reference/libraries.md)); what the binary does not reproduce:

- Names are ASCII: a non-ASCII name (dnspython encodes it with IDNA 2008, or 2003 without the `idna` package)
  raises a py2axum `TypeError`.
- Records of other types than MX, TXT, A, AAAA, CNAME and NS are skipped when a response is read: a response
  holding a malformed record of such a type is accepted where dnspython drops it (and waits for another one);
  an answer of such a type raises a py2axum `TypeError`.
- The exceptions carry their message only (`NXDOMAIN.qnames()`, `NoAnswer.response()`, `kwargs`... are not
  available). An answer's `rrset`, `response` and `expiration` are not available.
- Durations in `LifetimeTimeout`'s message and the order of `rotate` follow the clock and the random generator,
  as in Python, not Python's own draws.

## OpenTelemetry (FastAPI 0.142+)

FastAPI 0.142 traces requests, records metrics and logs through OpenTelemetry when its SDK is configured (a global
provider, or exporters added from the `OTEL_*` environment variables). **Known difference:** the binary emits no
OpenTelemetry data and does not read `OTEL_*`. `FastAPI(telemetry={...})` is accepted with its switches
(`tracing`, `metrics`, `logs`, `operation_spans`, `auto_configure`) as literals; a provider (`tracer_provider=`...),
`exclude=` and the `opentelemetry` SDK calls are refused.

## SQLModel

SQLModel classes are rewritten into SQLAlchemy mapped classes and Pydantic models at translation (see
[Libraries](reference/libraries.md)). **Known differences:**

- A table instance dumped as itself (a table class as the return type or `response_model=`, or returned without
  one): CPython lists its keys in the order of its `__dict__`, which after a load from the database depends on
  Python's string hashing (it changes from one process to the next); the binary lists the fields in their
  declaration order. A response model without a table (`HeroPublic`) has the same output on both sides.
- A table class as a route's body, a required field left out: SQLModel leaves the attribute unset (absent from the
  dumped response), the binary reads it as `None`.

## Not supported

An application wrapped in a project ASGI class at module level (`app = Wrapper(api)`, refused: the
binary would serve `api` without it), mounted ASGI apps and routers of libraries (left to Python with
`--python-side mount` when registered last), libraries not listed, C extensions, `eval`/`exec`, metaclasses, multiple inheritance of
project classes, OpenAPI `/docs` in the binary, and assigning an attribute of the application at module level
other than `app.state`, `app.dependency_overrides[f] = g` and `app.router.route_class = R` (`app.openapi = ...`
is refused with `file:line`). Not native, counted apart in the
[documentation coverage](coverage.md): what only changes the OpenAPI schema or the documentation pages
(`/openapi.json`, `/docs`, custom docs assets), OpenTelemetry providers in `FastAPI(telemetry=...)`, GraphQL
routers, `UJSONResponse`.

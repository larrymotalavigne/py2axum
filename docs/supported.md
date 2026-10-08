# Supported subset and known differences

py2axum translates a closed subset of Python and of a list of libraries. Anything outside it is refused at
compile time with `file:line` (and counted by `--report`), so a construct is either translated with the
behaviour described here or not translated at all. Method calls and attribute reads on values only known at run
time are checked by name: a name that no runtime type implements and that the project never defines is
refused (a name that exists on another type than the actual receiver is not detected). Each item below is covered by conformance cases that
compare the binary with the Python application, response by response.

"Differences" are the places where the binary is knowingly not identical to CPython; they are listed so you
can decide whether they matter for your application.

New to py2axum? Start with the [getting-started guide](getting-started.md); `py2axum check` tells you what this
page means for your application, route by route. To check the binary against your application, see
[conformance.md](conformance.md).

**Contents:** [Supported versions](#supported-versions) · [Applications, routes, parameters](#applications-routes-parameters)
· [Dependencies and security](#dependencies-and-security) · [Responses](#responses)
· [Middleware and exception handlers](#middleware-and-exception-handlers) · [Shutdown](#shutdown-sigterm-sigint)
· [WebSockets](#websockets) · [Pydantic v2](#pydantic-v2) · [SQLAlchemy 2.0](#sqlalchemy-20-async-postgresql)
· [Python semantics](#python-semantics) · [asyncio and threading](#asyncio-and-threading) · [Sentry](#sentry-sentry-sdk-2x)
· [MCP servers](#mcp-servers-mcp-22) · [Standard library](#standard-library) · [Libraries](#libraries)
· [Hybrid deployments](#hybrid-deployments) · [Runtime environment](#runtime-environment) · [Not supported](#not-supported) · [Large list responses](#large-list-responses)

There is one backend since 0.4 (the statically-typed one was removed: it covered only simple CRUD handlers;
its list streaming moved to [Large list responses](#large-list-responses)). `--backend auto` and
`--backend dyn` are still accepted until 1.0 and change nothing; `--backend typed` is an error.

## Supported versions

The runtime reproduces the behaviour of precise library versions (Pydantic's error messages and URLs,
Starlette's routing and middlewares, SQLAlchemy's session), so py2axum only accepts the ranges its
conformance suites run on. CI runs the test suite and the full `fixtures/dynapp` conformance (normal and
forced-streaming passes) at both ends of every range: lowest versions on Python 3.12, highest on 3.13 and 3.14.

| Library | Lowest tested | Highest tested | Accepted |
|---|---|---|---|
| fastapi | 0.137.0 | 0.142.3 | `>=0.137.0,<0.143` |
| starlette | 1.0.0 | 1.7.0 | `>=1.0.0,<1.8` |
| pydantic | 2.12.0 | 2.13.5 | `>=2.12.0,<2.14` |
| pydantic-core | 2.41.1 | 2.46.5 | `>=2.41.1,<2.47` |
| pydantic-settings | 2.11.0 | 2.15.0 | `>=2.11.0,<2.16` |
| sqlalchemy | 2.0.44 | 2.1.4 | `>=2.0.44,<2.2` |
| psycopg | 3.2.12 | 3.3.6 | `>=3.2.12,<3.4` |
| httpx | 0.28.1 | 0.28.1 | `>=0.28.1,<0.29` |
| aiohttp | 3.13.0 | 3.14.4 | `>=3.13.0,<3.15` |
| mcp | 2.2.0 | 2.2.0 | `>=2.2.0,<2.3` |
| asyncpg | 0.31.0 | 0.31.0 | `>=0.31.0,<0.32` |
| Python | 3.12 | 3.14 | `>=3.12,<3.15` |

`mcp` is covered by the conformance of a real MCP server (tested internally), rather than by the matrix;
`asyncpg` was verified the same way on a real application at 0.31.0, which has since moved to psycopg: that
check is no longer run. Patch releases inside a range are accepted without being tested one by one. Behaviours that change
inside a range follow the project's version: CPython's messages (3.14 names the role of an unhashable dict key
or set element and the expected input of a `math` domain error), Pydantic's (2.12 lists the expected
characters of an invalid UUID), Starlette's `CORSMiddleware`. WebSocket routes reproduce Starlette 1.7
(`WebSocketDisconnected`): a project declaring them needs `starlette>=1.7.0`. Below the lowest versions, the
differences found were FastAPI's security schemes (401 vs 403, credential stripping), empty `Form` strings and
included-router objects (< 0.137), and Jinja2 autoescape of `.txt` templates (Starlette < 1.0).

The analysed project's versions are read from the first of these found at `--root` or above (up to the
repository's root): `uv.lock` (exact versions), else `requirements*.txt` and the `pyproject.toml`
dependencies. A locked or pinned version outside its range, or a specifier that excludes the whole range
(`sqlalchemy<2`), is refused with `file:line`, by generation, `check` and `--report` alike. A specifier that
overlaps the range (`fastapi>=0.100`) is accepted. `requires-python` must overlap 3.12–3.14, and its lower bound
must not be newer than the Python running py2axum (the parser only knows its own syntax). Libraries the project
does not mention are not checked. `--allow-untested-versions` translates anyway, at your own risk.

## Applications, routes, parameters

- `FastAPI()` apps, `APIRouter(prefix=, tags=, dependencies=)`, `include_router`, routers split across
  modules, apps built by a factory function (`create_app()`), including endpoints defined inside it; a
  local of the factory assigned once is evaluated once. `if` statements around registrations are evaluated
  at startup in the factory's scope (e.g. `if not settings.TESTING:`); the factory's parameters have their
  default values (the server calls it without arguments).
- `include_router(router, prefix=<expression>)` at module level (e.g. `prefix=settings.API_V1_PREFIX`): the
  prefix is evaluated at startup, after the module globals, so the environment still overrides the
  settings; it is checked like FastAPI (must start with `/`, must not end with `/`; a failure stops the
  binary as an import error stops uvicorn). Differences: a runtime prefix containing a path parameter
  (`{...}`) stops the binary; a non-literal prefix inside a function (app factory) is refused at compile
  time; `--python-side` cannot name a route under a runtime prefix (it is compiled).
- Imports are resolved statically, without executing code: relative imports, re-exports from `__init__`,
  lazy imports inside functions (visible from nested functions and lambdas), `--root` as `sys.path`.
- Routing like Starlette: declaration order, decoded path, first match on path and method wins, then 405
  with the methods of the first path match, then a 307 redirect with/without the trailing slash, then 404.
  No implicit HEAD. The redirect uses `http://` (as uvicorn does without trusted proxy headers).
- `--python-side` routes (and `auto`'s) are relayed to `PY2AXUM_PYTHON_URL` before the binary's middleware
  stack: the Python application's own middlewares answer for them. Difference: state a middleware keeps in
  memory (a rate limiter's counters...) is per process, as with several uvicorn workers: a counter shared by
  translated and Python-side routes counts each request in one process only (e.g. `X-RateLimit-Remaining`).
- `app.mount(path, X)` (a sub-application, `StaticFiles`, FastMCP's `streamable_http_app()`...) is not
  translated, but the app's last registrations may be mounts left to the Python side (`--python-side mount`,
  implied by `--python-side auto`): with `PY2AXUM_PYTHON_URL` set, a request under the mount's path (or the
  path itself, for Starlette's 307 to `path/`) that no translated route fully matches (path and method) is
  relayed to the Python application. As in Starlette, a mount's full match beats an earlier partial one: a
  HEAD, or another method, on a translated GET route under the mount reaches the mounted application, not a
  405. A non-literal mount path relays every unmatched request. Refused with `file:line`: a mount followed by
  another registration (`include_router`, `@app.get`, `add_api_route`, `add_route`...: a route after the mount
  is shadowed by it in Python but would be served by the binary), or registrations of the app in another
  module or function than the mount (their order depends on imports or calls). WebSocket handshakes under
  the mount are not relayed (see WebSockets).
- Path, query and header parameters (`int/str/bool/float/Enum/UUID/datetime`, `X | None`, lists,
  `Query/Path/Header(...)` constraints and aliases), JSON bodies (one or several models, `Body(embed=)`),
  `Form()`/`File()`/`UploadFile` (single, optional or list, a literal default such as `File(default=[])`, a
  fresh copy per request; multipart via `multer` or urlencoded; an empty
  string counts as absent like FastAPI; `UploadFile` in memory: `read`, `seek`, `filename`,
  `content_type`, `size`, `.file`). Errors are FastAPI's 422 bodies, byte for byte.
- JSON bodies (and `json.loads`) are decoded as CPython's `json` does: `NaN`/`Infinity`, encodings detected
  from the bytes (BOM, UTF-16/32), the 422 `json_invalid` with CPython's `ctx.error` and position in code
  points (the trailing-comma messages of 3.13+), undecodable bytes as FastAPI's 400, NaN/inf in a 422's raw
  `input` as Starlette's 500. A `\uXXXX` escape ending the text follows the newest CPython patches
  (unterminated string; before 3.13.13/3.14.4 or so, and on 3.12, "Invalid \uXXXX escape"):
  `PY2AXUM_PYTHON_VERSION=3.14.0` selects the older message. **Known differences:** a lone surrogate escape (`"\ud83d"`) becomes U+FFFD;
  an integer beyond 64 bits raises `OverflowError` (500); nesting deeper than 10 000 levels is FastAPI's 400
  (CPython's limit depends on its C stack, and FastAPI may answer 500 when it encodes a deeply nested `input`).
- A JSON `null` body is no body (missing, or the parameter's default). A body without `Content-Type` is not
  decoded as JSON from FastAPI 0.132 on (`strict_content_type`, its default).
- Timestamps as Pydantic reads them (speedate): numbers and integer strings within years 0000-9999, milliseconds
  above 2e10. **Known difference:** a numeric *string* with a fraction or an exponent above 2e10 seconds
  (`"6.958e+16"`, `"253402300800000.0"`) is read by speedate's float-string path with its own scaling; the binary
  applies the number's rules (an error past 9999).
- Response status codes: uvicorn has a status line for 100..599 only and drops the connection, unanswered, for
  any other `status_code`; the binary does the same. **Known difference:** a final 1xx status is a 500 (hyper
  does not send an informational status as the final response).
- `Cookie()` parameters are not supported yet.

- Raw ASGI routes: `app.add_route(path, obj, methods=...)` / `app.router.add_route(...)` at module level, with
  an instance of a project class defining `async def __call__(self, scope, receive, send)` (Starlette runs
  anything that is not a function or a method as an ASGI app). The app gets an ASGI 3 `scope` dict (the keys
  uvicorn and Starlette's router set, `scope["app"]` with an empty `dependency_overrides`), `receive` (the
  body in one `http.request` message, then nothing until the client leaves) and `send`
  (`http.response.start`/`http.response.body`, streamed when `more_body` is true). `Request(scope, receive)`
  (the request being served only), a response object called as an ASGI app
  (`await JSONResponse(...)(scope, receive, send)`), a generator dependency called by hand
  (`gen = get_session(); s = await anext(gen); ...; await gen.aclose()`). Refused: a class as endpoint
  (`HTTPEndpoint`). Differences: `allow` of a 405 lists the methods in declaration order (Starlette
  iterates a set, so its order varies between processes); mutations of `dependency_overrides` are lost.

## Dependencies and security

- `Depends(...)` with project functions and classes, sub-dependencies, the per-request cache,
  `Annotated[T, Depends(...)]` aliases, `dependencies=[...]` on routes and routers.
- `p: Model = Depends()` with a Pydantic model (FastAPI calls the class): one query parameter per field, with its
  type, default and `Field` constraints (`ge`, `le`, `max_length`, `pattern`...), then `Model(**fields)`.
  Refused: a base other than `BaseModel`, validators or other decorated methods, `model_config`, aliases,
  container fields (FastAPI reads those from the body).
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
  `WWW-Authenticate: Bearer`), `HTTPBasic` (`realm=` a literal; `HTTPBasicCredentials`; a payload that is not
  base64 of ASCII `user:password` is a 401 even with `auto_error=False`, as in FastAPI),
  `OAuth2PasswordRequestForm`. Other schemes (`APIKeyHeader`...) and a security scheme in
  `dependencies=[...]` are refused.

## Responses

- `response_model` (filtering, aliases, `exclude_unset/none`), status codes, returned dicts, lists, models,
  ORM objects (`from_attributes`).
- Returned `Response`, `JSONResponse`, `PlainTextResponse`, `HTMLResponse`, `RedirectResponse`,
  `FileResponse` are sent like Starlette 1.7 (header order, `ETag`/`Last-Modified` of `FileResponse`;
  no `Range`/partial `HEAD`). `JSONResponse(content=...)` serializes like Starlette (strict `json.dumps`:
  a model, a datetime or NaN raise). `set_cookie`/`delete_cookie` in `http.cookies` format, headers set on
  the injected `Response`.
- `response_class=` on a route (`HTMLResponse`, `PlainTextResponse`, `Response`, `RedirectResponse`,
  `FileResponse`, `JSONResponse`): a returned non-response value is encoded (`response_model` or
  `jsonable_encoder`), then wrapped like FastAPI (`str`/`bytes`/`None` body, a returned URL redirects with 307
  unless `status_code=` is given, a returned path is served as a file, empty body for 204/304, the injected
  `Response`'s headers appended). `response_class=StreamingResponse` with a non-response return value raises
  (500); other classes are refused at transpile time.
- `jsonable_encoder` semantics for returned values (models by alias, `bytes.decode()`, `Decimal` via
  FastAPI's `decimal_encoder`).
- **Known difference:** a mapped object returned without `response_model` is encoded like FastAPI does
  (`vars(obj)` without SQLAlchemy's `_sa_*` keys: its loaded attributes), but its keys come in column order.
  CPython's order follows SQLAlchemy's iteration over a set of columns, which depends on memory addresses:
  it changes from one process to the next, so no order can be reproduced. Same keys and values.
- Large lists returned straight from the session are streamed: see [Large list responses](#large-list-responses).
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
  `response.headers`; with or without their own `__init__`), `@app.middleware("http")`, `@app.exception_handler(class | code)`,
  `app.add_exception_handler(class | code, handler)`.
- Raw ASGI middleware: a project class with `__init__(self, app, ...)` and `async def __call__(self, scope,
  receive, send)`, registered with `app.add_middleware(Cls, ...)` or `FastAPI(middleware=[Middleware(Cls,
  ...)])` (those entries stay inside every `add_middleware`, in list order, as in Starlette). The instance is
  built with the rest of the stack as its `app`. One `scope` dict per request, shared by every layer (and
  by `request.scope` below them); `receive()` returns the whole body in one `http.request` message, then
  `http.disconnect` once the response is complete (uvicorn); `send()` takes `http.response.start` /
  `http.response.body` (`more_body` streams; without `content-length` the response is sent chunked, as
  uvicorn does). `await self.app(scope, receive, send)` runs the rest of the stack: a rewritten scope
  (`headers`, `method`, `path`, `query_string`) is a new request for it, `scope["state"]` is
  `request.state`, a wrapped `receive` is read to the end of the body and becomes the request body, the
  response goes through a wrapped `send` as Starlette's messages (`http.response.start` with the raw
  headers, one body message for a body of known size, one per part plus an empty last one when
  streamed), and an exception the stack lets through is raised in the middleware. The middleware can
  answer itself (`send()` messages, or `await response(scope, receive, send)`); its code after
  `await self.app(...)` runs once the body is complete (`finally` blocks included). An exception before the
  response starts reaches the outer middlewares, then `ServerErrorMiddleware`; after it started, the
  response is kept and the exception logged. ContextVars set around `await self.app(...)` are seen by the
  endpoint and back. `app.state` is a process-wide `State` (`add_middleware(Cls, state=app.state)`).
  `http.response.trailers`/`pathsend`/`zerocopy` messages and a rewritten `root_path` raise at run time.
- `starlette_context` 0.5's `RawContextMiddleware(plugins=(RequestIdPlugin(), CorrelationIdPlugin()))`
  (plugin options `force_new_uuid`, `validate`, `version=4` as literals): the id read from its header or a new
  `uuid4().hex`, validated like `uuid.UUID(value)` (400 with an empty body otherwise), appended to the
  response headers. The `context` object itself is not available.
- `FastAPI(strict_content_type=True | False)` (a literal): without it, the default of the locked FastAPI.
- `traceback.format_exception(...)` / `format_exception_only(exc)`: the exception's own line
  (`module.Class: message`), as CPython formats it for a traceback of `None`; `exc.__traceback__` is None
  (the binary has no Python frames), so frames and chained exceptions are not rendered.
- Differences (raw ASGI middleware): `GZipMiddleware` always runs outermost, so a middleware added after it
  sees the response before compression; raw middlewares run for `http` scopes only (`websocket` and `lifespan`
  pass as if the middleware let them through, its usual first line); a wrapped `receive` is read to the end of
  the body before the rest of the stack runs, and an original `receive` already consumed by an outer layer
  still yields the body (uvicorn would wait for the client to disconnect); the 500 for a middleware returning
  without a response is produced by the layer, so outer middlewares see it (uvicorn produces it itself).
- `request.cookies` (Starlette's parser), `request.client` (TCP peer), `request.url`, `request.headers`,
  `request.state`, `request.body()`/`json()`.
- A project function given the application from the factory or the app's module (`configure(app)`) runs
  while the middleware stack is built (on the first request, when Starlette instantiates its middlewares):
  in it, `app.add_middleware(ProjectMiddleware, ...)` with a project `BaseHTTPMiddleware` subclass or raw
  ASGI class, `@app.exception_handler(...)` on its nested functions, `app.add_exception_handler(...)`,
  `@app.middleware("http")`, `app.add_route(path, endpoint, methods=)` (a Starlette `Route`: GET implies
  HEAD; such routes are tried after the declared ones, as when added last), `app.routes`. Other
  registrations there are refused.
- Routing objects as values: `request.app`, `app.router.routes` as FastAPI 0.141+ lists them (its docs
  routes, an `_IncludedRouter` per `include_router` with its `original_router`, `APIRoute`s, the added
  `Route`s), their `path`, `path_format`, `methods`, `name`, `matches(scope)` returning
  `starlette.routing.Match` and the child scope, `isinstance(r, Route | APIRoute)`. `request.scope` is a
  snapshot dict with `type`, `http_version`, `scheme`, `method`, `root_path`, `path`, `raw_path`,
  `query_string`, `headers`, `client`, `app`, and, once the router has run, `path_params` and `route` (the
  `APIRoute`, as Starlette sets it); the `endpoint` of scopes is None. A 405 on an added `Route` lists
  `GET, HEAD` in that order (CPython: set order). `FastAPI(docs_url=...)` and the other `*_url` options must
  be literals for `app.routes` to be read.

## Shutdown (SIGTERM, SIGINT)

Measured against uvicorn 0.54 (both servers, same clients), for every generated
binary, with or without a lifespan:
- As uvicorn: the listening socket is closed at once (new connections are refused), idle keep-alive
  connections are closed, in-flight requests finish and their response carries `connection: close`,
  WebSocket sessions get a close frame 1012 and the application receives `websocket.disconnect` with code
  1012, then the lifespan's code after `yield` runs, and the process ends by the signal it received (exit
  status 143 for SIGTERM, uvicorn re-raises it). A second signal skips the wait (uvicorn's force exit).
- Differences: the wait for in-flight requests and streams is bounded by `PY2AXUM_SHUTDOWN_TIMEOUT`
  seconds (default 25; uvicorn waits without a limit unless `--timeout-graceful-shutdown`, so an endless
  `StreamingResponse` such as an SSE feed holds it until the orchestrator's SIGKILL), after which the
  remaining connections are dropped; the database pool is closed before the exit (PostgreSQL gets a
  Terminate message; the Python process leaves its connections to be dropped with it).

## WebSockets

Measured against Starlette 1.7, FastAPI 0.142 and uvicorn 0.54 (its default `websockets-sansio` protocol);
the conformance suite compares the handshake, the messages and the close codes with a WebSocket client.

- `@app.websocket(path, name=, dependencies=)` and `@router.websocket(...)` (router prefixes, includes and
  their dependencies). Parameters: `WebSocket`, `HTTPConnection`, path/query/header parameters,
  `Depends` (generator dependencies run their exit code once the endpoint returns, the session dependency
  commits there). A validation error closes with 1008 before `accept`: the client gets HTTP 403.
- `WebSocket`: `accept(subprotocol=, headers=)`, `receive()`/`send(message)` (ASGI dicts),
  `receive_text/bytes/json(mode=)`, `send_text/bytes/json(mode=)` (`json.dumps` with compact separators
  and `ensure_ascii=False`), `close(code, reason)`, `iter_text/bytes/json()` (in `async for`),
  `send_denial_response(response)`, `client_state`/`application_state` (`WebSocketState`),
  `headers`, `query_params`, `path_params`, `cookies`, `client`, `url`, `state`, `scope`, `app`. Starlette's
  state checks and messages (`RuntimeError`, `WebSocketDisconnected`); `WebSocketDisconnect(code, reason)`
  when the client closes, `WebSocketDisconnect(1006)` on a send after it left.
- What the client sees, as uvicorn does it: `close()` before `accept()` (also `WebSocketException`, a
  failed validation, a path without WebSocket route) → HTTP 403 with an empty body; an exception or a
  return before `accept()` → HTTP 500; `HTTPException` before `accept()` → the JSON error response
  (FastAPI's handler); an exception or a return after `accept()` without `close()` → the connection is
  dropped without close frame (1006 for the client, not 1011); `WebSocketException` after `accept()` →
  a close frame with its code and reason.
- Exception handlers: `@app.exception_handler(...)` handlers are called with the `WebSocket` (by status
  code, then MRO); the `Exception`/500 handler is not (ServerErrorMiddleware lets WebSockets through, like
  CORS, GZip and `BaseHTTPMiddleware`).
- `async for` over async iterators in general (async generators, objects defining `__aiter__`/`__anext__`).
- Differences: no `permessage-deflate` compression (the handshake answer has no
  `Sec-WebSocket-Extensions`; messages are the same); no server keepalive pings (uvicorn pings every 20 s
  and closes with 1011 after 20 s without pong); messages are limited to 16 MiB like uvicorn, but the
  error past it may differ; an invalid handshake request (missing key, version ≠ 13) is answered by axum
  (400/426, other text); uvicorn's access log lines are not printed. WebSocket requests are never relayed
  to `PY2AXUM_PYTHON_URL`, and a WebSocket route cannot be declared `--python-side`.
- Refused: `Request`, `Response`, `BackgroundTasks`, body/`Form`/`File` parameters and security schemes
  on a WebSocket route or in one of its dependencies (FastAPI does not provide them there),
  `app.add_websocket_route`, `add_api_websocket_route`, `websocket_route` decorators, sync endpoints.

## Pydantic v2

- Lax-mode validation with pydantic-core's error types, messages, locations and contexts (speedate for
  dates), smart unions (exactness, fields set), enums, literals, nested models, lists/sets/tuples/dicts,
  `Optional`, `Field` constraints (and pydantic 1's `min_items`/`max_items`, which `Field()` still maps to
  `min_length`/`max_length`), aliases (`populate_by_name`), defaults and `default_factory`,
  `validate_assignment` (an assignment runs the field's `before` validators, its type, its `after` validators
  with `info.data` holding every other field, then the model's `after` validators: one of those that raises
  leaves the value assigned, error at loc `()`, as pydantic-core does; refused with a `mode="before"` model
  validator; an `after` validator's error reports the raw input; a `mode="before"` field validator defined after
  a `mode="after"` one of the same field is refused), `extra=`, `from_attributes`, `str_strip_whitespace`/`to_lower`/`to_upper`,
  `use_enum_values`, `model_config` as a dict or `ConfigDict`, v1 `class Config` (v1-only keys ignored like
  Pydantic v2 does).
- An Enum member given to a scalar field (an ORM enum column read into `status: str`...) as pydantic-core takes
  it: a `str`/`int` subclass member (`class X(str, Enum)`, `StrEnum`, `IntEnum`) is its value for `str`, `int`,
  `float`, `bool` and `Literal` (an `int` one is `str(value)` in a `str`), a plain member is `str(value)` in a
  `str` and its value, unchecked, in an unconstrained `int`. Difference: a plain member whose value is not an
  integer, given to a *constrained* `int`, reports `int_parsing` where pydantic-core reports `int_parsing_size`.
- `uuid.UUID` fields and parameters: a UUID instance, a str in the simple, hyphenated, `{braced}` or
  `urn:uuid:` form, or bytes, with pydantic-core's `uuid_type`/`uuid_parsing` errors (the messages of the
  `uuid` crate it pins); dumped as the hyphenated str.
- `@field_validator` / `@validator` (after and before, including `_x = field_validator(...)(lambda v: ...)`),
  `info: ValidationInfo` in after validators (`info.data`: the earlier fields that passed, after their own
  validators, defaults included; `info.field_name`; other attributes, and `info` in before validators, are
  refused), v1 `values` (pydantic's own signature rules: `field`/`config` parameters are refused),
  `@model_validator` (before/after); validators that raise produce the 422 at their place. Before
  validators run on the raw input in reverse definition order, like pydantic-core. A union with a member
  that has validators (directly or in a nested model) is refused: Pydantic would try the next member when one
  raises.
- `model_dump(mode=, by_alias=, exclude_none=, exclude_unset=, exclude=, include=)` (top-level field names
  for `exclude`/`include`), `model_dump_json`, `model_validate(_json)`, `model_copy(update=, deep=)`,
  `model_fields_set`, `model_fields`, `ValidationError.errors()` (URL with the major.minor of the pydantic the
  project locks, else the one installed next to py2axum; `ctx.error` is the exception raised by the validator,
  rendered as its attributes by `jsonable_encoder`; `include_*` options) and `error_count()`.
- `Model.model_json_schema()` (default arguments only), computed at translation time like pydantic 2.13's
  `GenerateJsonSchema` (key order included; same subset as MCP tool schemas, plus `model_config extra=`).
  The receiver must be a model class by name, or `expr.attr` where every value the project binds to an
  attribute or keyword `attr` is a model class (a tool registry); anything else is refused.
- `model_config frozen=True` (assignment raises `frozen_instance`; frozen models hash by value),
  `Field(validate_default=True)`, field options given in `Annotated[T, Field(...)]`.
- `TypeAdapter(T)`: `validate_python` (ORM objects with `from_attributes`, iterables), `validate_json`,
  `dump_python`, `dump_json`, for types written in the source or known at run time.
- `EmailStr` (rule-by-rule port of email-validator, same messages, except IDNA encoding and NFC normalization
  of internationalized domains, which are accepted and lowercased), `AnyUrl`/`AnyHttpUrl`/`HttpUrl`/`RedisDsn`
  (parsed with the `url` crate like pydantic-core, type defaults, attributes, same errors).
- pydantic-settings `BaseSettings`: environment variables (not `.env` files), `validate_default=True`,
  `env_prefix`, `case_sensitive` (names compared case-insensitively by default, the last of two names that
  differ only in case wins), `env_parse_none_str` (an environment value equal to it, case included, is
  `None`; init keywords are not parsed), JSON values for container/model fields (a union with such a
  member keeps the raw string when the JSON does not parse); other `env_*` options and `_env_*` init keywords are refused;
  a class body run like a script (class-level `if`, attributes reading earlier ones) is evaluated once.
- `decimal.Decimal` fields: lax validation like pydantic-core (a float through its `repr`, a string through
  `Decimal(str)`, finite values only), `max_digits`/`decimal_places` on the normalized value, then
  `le`/`lt`/`ge`/`gt` (int or float literals); dumped as a string in JSON mode.
- `@computed_field` (bare, over `@property` or alone): serialized after the fields and the extras,
  `exclude_none` applies, `exclude_unset` does not, `repr()` shows them. The property must not await.
  Difference: with `extra="allow"`, an extra key named like a computed field shadows it on attribute access and in `model_dump_json` (Pydantic writes both keys).
  `jsonable_encoder` of an integral `Decimal` beyond 64 bits gives a float (Python: an int).
- A `@classmethod` override of `model_validate` (called by name, `Model.model_validate(x)`) calling
  `super().model_validate(...)` or `super(Model, cls)`: the parent's (a project model's override, else
  BaseModel's), `cls` still the class called. `super(cls, cls)` is accepted when no project model subclasses
  the class (else its target depends on the class called: refused).
- A model's own `def __init__(self, **data)` calling `super().__init__(**data)`: run by `Model(...)`;
  validation from attributes (`from_attributes`, ORM objects) or of an existing instance skips it, as in
  pydantic-core. Validating such a model from a dict (request body, `model_validate(dict)`, nested), where
  pydantic-core calls the `__init__`, raises a py2axum RuntimeError (500) instead.
- Private attributes (`_name: T = PrivateAttr(default=/default_factory=)`, any `_name`): per instance, never
  validated nor dumped; a `_name` assigned on an instance is not dumped either (unless `extra="allow"`).
  An instance of the response model's own class is serialized as returned, private attributes included.
- Not supported: `@computed_field(...)` options, `@field_serializer`/`@model_serializer`, nested
  `exclude`/`include` dicts, strict mode.

## SQLAlchemy 2.0 (async, PostgreSQL)

- Declarative models (`Mapped[...]`, `mapped_column`, types, `default=`/`server_default=`/`onupdate=`
  (value, callable or SQL), `unique`, `nullable`, `ForeignKey` (a foreign key without a type takes the
  referenced column's), composite primary keys, `Identity()`, `JSON`/`JSONB` (`none_as_null=`; on `JSONB`, the
  operators `contains` (`@>`), `contained_by` (`<@`), `has_key` (`?`), `has_any` (`?|`), `has_all` (`?&`), their
  argument typed like SQLAlchemy types it: a list given to `has_any`/`has_all` is bound as JSONB, which PostgreSQL
  rejects in both implementations),
  `Uuid`/`UUID` and `Mapped[uuid.UUID]` (read as `uuid.UUID`, a str bound to it is cast by PostgreSQL as with
  psycopg; `as_uuid=False` is refused),
  `Numeric` (`Decimal`, or float with `asdecimal=False`), `Enum` columns, `LargeBinary` (bytes), `ARRAY(String)`
  (lists; `contains`/`contained_by`/`overlap`/`any`), `deferred(Column(...))` and `mapped_column(deferred=True)`
  (left out of what a query loads: reading it then is a lazy load, MissingGreenlet in an async session;
  `select(Model.col)` and `session.refresh(obj, ["col"])` read it, and so does the loader option
  `undefer(Model.col)`: in `select(...).options()`, `session.get(..., options=[...])` or after a relationship
  loader, `selectinload(A.b).options(undefer(B.col))`, also into an object of the identity map that has not
  loaded it),
  a SQL column name other than the attribute (`extra_data = Column("metadata", JSON)`: SQL uses the column
  name, rows and `session.get` the attribute key, `Model.attr.name` is the column's name), `T.with_variant(V, "postgresql")` (V; other dialects' variants
  are ignored), `col.op("...")(value)` (the value typed like the column, as SQLAlchemy does), a column type returned by a project function
  (`def _enum(cls, name): return Enum(cls, name=name, ...)`, inlined; its arguments must be literals or names),
  project `TypeDecorator`s
  (`process_bind_param`/`process_result_value` without `self`/`dialect`). Methods of mapped classes:
  plain, `@property`, `@staticmethod` and `@classmethod` (`cls(...)` builds an instance); other decorators
  (`@hybrid_property`, `@validates`...) are refused.
- Session: identity map (weak, like SQLAlchemy), autoflush, implicit transaction, `get` (scalar, tuple,
  list or dict identities), `add`/`add_all`/`delete`/`flush`/`commit`/`rollback`/`refresh`/`close`,
  `connection()` (a readiness probe: `close()` on it ends the transaction, then statements raise
  `ResourceClosedError` and `commit()` "This transaction is inactive" until `rollback()`/`close()`),
  `expire_on_commit`, savepoints (`begin_nested()` then `commit()`/`rollback()`), `session.bind`,
  `get_bind()`. Server-generated values (identity, `server_default`, SQL defaults) are fetched with
  `RETURNING` at insert, like `eager_defaults="auto"`.
- Relationships (many-to-one, one-to-many), `lazy=` select/selectin/joined/noload/raise, `selectinload()`
  chains, `back_populates`/`backref`, cascades (save-update, delete, delete-orphan), `passive_deletes`,
  `order_by=` (target columns, `.desc()`, or a string SQLAlchemy evaluates such as `"[Child.a, Child.b.desc()]"`),
  self-referential relationships (adjacency list: one-to-many by default, many-to-one with `remote_side=` naming
  the referenced column; a row made its own parent raises `CircularDependencyError` at flush, as without
  `post_update`).
  Difference: `parent.children.append(x)` sets the foreign key at flush but not `x.parent` before it
  (SQLAlchemy does it immediately through the backref event).
- Core: `select` (entities, columns, labels, `*cols`), `where`/`filter_by`, joins (explicit, inferred from
  the single foreign key, relationship), `aliased`, subqueries, `exists` (`exists(select)`, `select.exists()`,
  and `exists().where(...)`, whose FROM is the tables of its criteria less those of the enclosing statement:
  SQLAlchemy's auto-correlation), `in_` (lists, selects, `tuple_`),
  `like/ilike/startswith/contains`, `regexp_match` (`~`, `~*` with `flags="i"`; other flags raise), `is_/is_not`, `is_distinct_from`, `case`, `literal`, `cast`, `extract`,
  `func.*` (with `FILTER`), `group_by/having/order_by/limit/offset/distinct(on)` (an expression built once and
  used in the columns and in `GROUP BY` shares its bound parameters, like SQLAlchemy's bind objects; two
  identical expressions built apart do not, and PostgreSQL rejects the grouping, as it does for SQLAlchemy),
  `with_for_update`,
  `update()`/`delete()` (with `synchronize_session`; `returning`, of columns only for a DELETE), `insert()` (core and postgresql dialect: several rows,
  Python column defaults, `on_conflict_do_update(index_elements= | constraint=, set_=, where=)`,
  `on_conflict_do_nothing` (a `constraint=` name SQLAlchemy would not quote),
  `excluded`, `returning`), `text()` with `:named` parameters (and `text(...).bindparams(name=value)`, also inside
  a `where()`), `Result.scalars/all/first/one/scalar/unique/
  mappings`, rows with attribute access (`row.total`, `_mapping`, `_asdict()`).
- SQL typing like SQLAlchemy: arithmetic and `FILTER` keep the column type, `func.round`/`avg` untyped
  (Decimal); untyped integers are bound as int2/int4/int8 like psycopg; NUMERIC results are `Decimal`.
- Database errors are SQLAlchemy's classes over psycopg's (`IntegrityError`, `DataError`...), their message
  `(psycopg.errors.<class of the SQLSTATE>) <server message>` (`NumericValueOutOfRange`, `UniqueViolation`...).
  An integer bound to an `Integer`/`SmallInteger` column is cast like SQLAlchemy's psycopg dialect does
  (`::INTEGER`): out of range, a `DataError`. **Known difference:** `str()` of such an error stops there, without
  SQLAlchemy's `[SQL: ...]`, `[parameters: ...]` and background-link lines (the binary's SQL is not SQLAlchemy's
  text).
- The PostgreSQL session time zone: sqlx forces UTC, the runtime applies the one psycopg would see
  (role/database setting, then server config, or `PY2AXUM_DB_TIMEZONE`), unless the engine sets one.
- Session parameters of `create_async_engine`/`create_engine(connect_args=...)`: psycopg's libpq
  `options` (`-c name=value`, `-cname=value`, `--name=value`, `\` escapes) and asyncpg's
  `server_settings` dict are set on every pool connection; a `TimeZone` among them is the zone timestamptz
  are decoded in (psycopg). Other `options` switches are refused (a literal string, at translation; a
  computed one raises `ValueError` when the engine is created); other
  `connect_args` keys (timeouts, `sslmode`, ...) have no effect.
- The driver named by `DATABASE_URL` (`postgresql+psycopg://` or `postgresql+asyncpg://`): psycopg returns
  timestamptz in the session's zone, asyncpg as `datetime.timezone.utc` (Pydantic writes `Z`); an ORM-enabled
  `insert(Model)` without `returning` reports `rowcount` -1 over psycopg, the rows inserted over asyncpg.
- `obj.__dict__` of a mapped object: `_sa_instance_state` then the loaded attributes (a snapshot).
- `create_async_engine(...)` is the binary's pool (one database, `DATABASE_URL`; its options are ignored
  but `connect_args`, above);
  `async with engine.connect() as conn` (rolled back on exit), `async with engine.begin() as conn` (committed on
  exit, rolled back when the block raises).
- `await conn.run_sync(Base.metadata.create_all)` (e.g. in the lifespan): the DDL is compiled at translation
  time by SQLAlchemy itself (it must be installed next to py2axum; 2.x). The mapped classes of the base, from
  the modules the application imports (plus those imported in the calling function), are rebuilt as real
  SQLAlchemy classes from their source by a static evaluator that only calls SQLAlchemy: column types and
  options, `Mapped[...]` annotations and the base's `type_annotation_map`, mixins, `__table_args__`, the
  base's `metadata = MetaData(naming_convention=...)`, module-level `Table(...)` and `Index(...)`, project
  enums, project `TypeDecorator`s (their `impl`), project functions whose body is `return <type>`.
  Python-side defaults never run (only their presence matters: a primary key with a default is not SERIAL).
  At run time the statements are replayed with `checkfirst=True` like SQLAlchemy's PostgreSQL dialect:
  every named enum type absent from `pg_type` is created (even when its table exists), then each table
  absent from `pg_class` with its indexes and comments, then the foreign keys of cycles (`use_alter`) of the
  tables created. Refused at their line: `@declared_attr`, `Sequence`, DDL event listeners
  (`before_create`/`after_create`...), tables in another schema, `Enum(metadata=...)`, TypeDecorators that
  override `load_dialect_impl`/`__init__`, `values_callable` other than `lambda x: [e.value for e in x]`,
  any value the evaluator cannot build, a model module only imported inside another function, any other
  `run_sync` function. **Known difference:** a cycle of foreign keys partly created already is closed by
  `ALTER TABLE` where SQLAlchemy, which only sorts the tables it creates, may inline the constraint: same
  resulting schema. The DDL is the one of the SQLAlchemy that ran the translation.
- Synchronous sessions (`create_engine`, `sessionmaker`, `Session` parameters of `def` endpoints, a
  generator dependency `s = maker()` / `try: yield s` / `finally: s.close()` or `with maker() as s:
  yield s`): the same session semantics, run on the async pool (FastAPI's threadpool is not modelled:
  only the observable behaviour is). Reading an expired column or a relationship that is not loaded emits
  the SQL like SQLAlchemy's lazy loader (autoflush first; `ObjectDeletedError` when the row is gone),
  including while a `response_model` or a model built from ORM objects (`Page(items=rows)`) reads the
  attributes, `@property`s of the model included. Awaiting a synchronous session's method
  (`await db.execute(...)`) runs it, then raises CPython's `TypeError`. An application uses one kind of
  session dependency (sync or async), not both.

## Python semantics

- Values and operators with CPython's semantics: int (64-bit: beyond is an `OverflowError`), float formatting
  and `repr`, str methods (`isdigit`, `isdecimal`, `isnumeric`, `isalpha`, `isalnum`, `isspace` from the Unicode
  database of the Python that transpiles), `%`/`format`/f-strings (presentations `d f % e g x o b` and their upper-case forms, `#`;
  not `n`, `c`, `=` with non-numbers), slicing, comparisons, `**`, bit operators, truthiness,
  `hash()` rules (unhashable Pydantic models and dataclasses unless frozen, `__hash__ = None`, `__eq__`
  without `__hash__`); `hash(int)` is CPython's, other hashes are stable but not CPython's (CPython
  randomizes str hashes anyway). **Known difference:** a set iterates in insertion order; CPython's order
  for strings changes from one process to the next (randomized hashes), so text built from a set of
  strings (`", ".join(ALLOWED)`) cannot be reproduced.
- Builtins take CPython's keyword arguments where the runtime implements them (`sorted(key=, reverse=)`,
  `min/max(key=, default=)`, `enumerate(start=)`, `sum(start=)`, `round(ndigits=)`, `int(base=)`,
  `zip(strict=)`); any other is refused. **Difference:** `zip(strict=True)` over iterables of different
  lengths raises at the call, where CPython raises after yielding the common prefix.
- Functions: keyword/default/`*args`/`**kwargs` binding with CPython's `TypeError`s, closures, lambdas,
  nested functions and decorators (`functools.wraps`, decorator factories; a decorated function is built
  once at startup), recursion, `global` (one cell per process), generators, `match`.
  Refused: `nonlocal`, a project decorator on a method.
- Classes: plain classes (`__init__`, methods, properties, static/class methods, class attributes,
  `__slots__`; single inheritance from another plain class and/or `abc.ABC`, `@abstractmethod` (instantiating
  a class left abstract raises CPython's `TypeError` when called by name), `super().__init__(...)`), `@dataclass` (incl. `frozen=True`, `__post_init__`), exceptions (class attributes,
  methods, `super().__init__` of `HTTPException`), Enums (methods, `_missing_`). Special methods:
  `__str__`, `__repr__`, `__eq__` (used by `str()`, f-strings, `==`, `in`, `index`, `count`, `remove`),
  `__enter__/__exit__`, `__aenter__/__aexit__`; they must not perform I/O (`def`); other special methods
  are refused. In containers, an exception raised by `__eq__` counts as "not equal" (CPython propagates it).
- Types as values: `list[X]`, `X | None`, library classes (`BaseModel`, `AsyncSession`...),
  `isinstance`/`issubclass` with run-time types, `inspect.isclass`, `typing.get_args/get_origin/
  get_type_hints` (annotations kept on decorated functions).
- `type(x)`: compared with `==`/`is`/`in`, called (`type(x)(...)` for builtin types), `__name__`/`__qualname__`
  (CPython's names: `Pattern`, `UUID`, `builtin_function_or_method`; a Pydantic model class is a
  `ModelMetaclass`, an Enum class an `EnumType`). Differences: values the runtime does not tell apart report
  the type they are stored as (`frozenset` → `set`, iterators and `range` → `list`), a mapped class reports
  `type`. A project class obtained as a value (`cls(**data)` in a classmethod, `type(m)(a=1)`, a class passed as
  an argument) is called like the class named in the source: Pydantic model, settings (environment read again),
  dataclass, plain class, Enum, `SimpleNamespace`, with CPython's `TypeError`s. Difference: pydantic-settings
  init options (positional arguments, `_env_prefix=`…) on a settings class held as a value raise a `TypeError`.
- Module globals are evaluated at startup in import order, like importing the app; module-level calls too, and
  module-level `try`, `if`, `for`, `while` and `with` statements, whose bindings are module variables
  (`except E as e` names are deleted as in CPython). A `try` whose body only imports and assigns constants is an
  import fallback: its imports were resolved at compile time, so its `except` branches never run.
  `if __name__ == "__main__":` and `if TYPE_CHECKING:` blocks are skipped. Refused: a module variable bound
  by several module-level statements when one of them is compound (`X = 1` then `try: X = f()`).
  Attributes and methods of a library object such as `prometheus_client.REGISTRY` are resolved at run time.
  A module-level assignment to an attribute of a library the binary knows nothing of (`stripe.api_key = ...`)
  is left to the Python side: every read or call of that library is refused already.
  Known difference: a module-level call of a project function that does not translate (typically a logging
  setup with handlers, formatters and filters, which the binary does not reproduce: it logs in its own
  format) fails at startup with an `ERROR:py2axum:module global` line on stderr and the binary goes on
  without its effects; the routes that read a global it would have set raise that error.
- `importlib.import_module("pkg.mod")` with a literal name of a project module returns a module object
  (`getattr`/`hasattr` with run-time names, attribute calls, `__name__`). The module is compiled into the
  binary and its globals are evaluated at startup with the others (CPython: at the first import), so a
  "lazy import" saves neither memory nor startup time. A top-level name that does not translate raises
  when read. Refused: computed module names, library
  modules, modules with `import *`.
- `sys.settrace(f)`, `threading.settrace(f)`, `threading.settrace_all_threads(f)`, `sys.gettrace()`: when a
  project calls one of them, its compiled functions (methods, nested functions, lambdas) report `call`, then
  `return` or `exception` (raised there, coming from a callee, or caught there) followed by `return` with
  None, to a process-wide trace function that is not traced itself. Frames expose `f_globals["__name__"]`,
  `f_code.co_qualname`/`co_name`/`co_filename`/`co_firstlineno` and `f_lineno`; tracebacks `tb_lineno`,
  `tb_frame`, `tb_next`, and `traceback.extract_tb`. Differences: one `call`/`return` pair per invocation (a
  coroutine suspended by an `await` does not report each suspension and resumption), no `line`/`opcode`
  events, only project frames exist (library frames are absent from tracebacks, so depths count project
  frames), a frame's line is the line where its current statement starts. Without such a call the
  functions are compiled without any of this.
- A module-level function is one object (`is`, attributes set on it); a nested function naming itself reads
  its name when called, as CPython's closure cell.
- `map`/`filter` return lists (materialized); `frozenset` behaves as `set`; `callable()`; a builtin exception
  class held in a variable can be called.

## asyncio and threading

- A call of an `async def` that is not awaited is a coroutine object (arguments evaluated at the call,
  body run at the first `await`; a second `await` raises like CPython). `asyncio.gather` (concurrent tasks,
  argument order, `return_exceptions`), `create_task` + `await task`, `add_done_callback`, `Semaphore`,
  `Lock`, `Queue`, `wait_for`, `sleep`, `to_thread`, `open_connection(host, port)` (the addresses of
  `getaddrinfo` tried in order like asyncio, its `ConnectionRefusedError`/`OSError("Multiple exceptions: ...")`
  messages: `[Errno 61] Connect call failed ('127.0.0.1', 9)` under asyncio, `[Errno 61] Connection refused`
  under uvloop, which uvicorn uses when the project's `uv.lock` installs it (`uvicorn[standard]`); `StreamWriter.write/drain/close/is_closing/wait_closed/get_extra_info("peername"|"sockname")`,
  `StreamReader.read/readline/at_eof`; `ssl=`, `limit=` and the other keywords are refused, and so are
  `readexactly`/`readuntil`). The runtime is multi-threaded: two tasks finishing at
  the same instant have no guaranteed order (asyncio follows creation order).
- `async with` / `with` follow CPython's protocol (the exception is passed to `__exit__`, a true result
  suppresses it).
- `threading.Lock/RLock/Event/Thread/get_ident`, `asyncio.new_event_loop()` + `run_forever()` in a
  thread, `call_soon`, `run_coroutine_threadsafe` + `wrap_future`, `loop.run_in_executor(None, f)`: emulated
  on tokio. The event loop is one "thread" (all coroutines share its ident); `Thread` and executor calls run
  on their own OS threads. Locks block like CPython's.
- `asyncio.run()` raises CPython's `RuntimeError` (always inside a running loop). Library calls that are not
  awaited (a bare `asyncio.sleep(1)`) run immediately.
- `FastAPI(lifespan=...)` with an `async def` generator of the project (decorated with
  `@contextlib.asynccontextmanager` or not, like Starlette): the code before `yield` runs before the server
  listens (a failure prints `Application startup failed. Exiting.` and exits with code 3, like uvicorn),
  the code after it once the server has stopped on SIGTERM/SIGINT (see "Shutdown"); lifespan state
  (`yield {...}`) is refused.
- Async generators run in lockstep with their consumer like CPython: the body starts at the first
  `__anext__`, `anext()`, `asend`, `athrow`, `aclose` (`GeneratorExit` at the `yield`, `finally` blocks run).
  `@asynccontextmanager` follows `contextlib` (exception thrown in at the `yield`, `generator didn't yield`,
  `generator didn't stop`). `contextlib.suppress(*excs)`.
- `task.cancel()`, `task.cancelled()`: `await task` then raises `CancelledError`. Difference: the task's
  coroutine is stopped at its current `await` without `CancelledError` being raised inside it (its own
  `except CancelledError` / `finally` blocks do not run).
- `contextvars.ContextVar` (`get` with or without default, `set`, `reset(token)`): values belong to the request
  being served (or to the lifespan). Difference: a task created with `create_task` shares its creator's
  values instead of a copy (a value it sets is seen by the creator).

## Sentry (`sentry-sdk` 2.x)

The binary reports to Sentry through the Rust SDK (crate `sentry` 0.46: DSN, HTTP transport, rate limits,
envelopes). What the Python SDK decides is reproduced by the runtime, so a Sentry project receives the same
events from the binary as from FastAPI: same count, level, message or log entry, logger, tags, user,
contexts, extra, breadcrumbs, exception type/value/module/mechanism, request data, transaction name and
status, `_meta` annotations. `tests/sentry_check.py` compares both against a fake Sentry server
(`fixtures/sentryapp`, with and without `send_default_pii`).

- `sentry_sdk.init(...)`: `dsn` (else `SENTRY_DSN`; empty or missing = inactive SDK, every capture returns
  None, as in Python), `environment` (else `SENTRY_ENVIRONMENT`, else `production`), `release` (else
  `SENTRY_RELEASE` and the CI variables the SDK reads; `git rev-parse` is not run), `server_name` (else the
  host name), `dist`, `sample_rate`, `traces_sample_rate`, `traces_sampler` (called with
  `transaction_context`, `parent_sampled` and `asgi_scope`), `enable_tracing`, `send_default_pii`,
  `max_value_length`, `max_breadcrumbs`, `max_request_body_size`, `before_send`, `before_send_transaction`
  (the project's functions, called with the serialized event and a hint holding `exc_info`), `integrations`,
  `default_integrations`, `auto_enabling_integrations`, `shutdown_timeout`. Accepted without effect, as they
  describe Python stack frames: `attach_stacktrace`, `include_local_variables`, `include_source_context`,
  `in_app_include`, `in_app_exclude`, `project_root`, `debug`, `send_client_reports`.
- Refused at transpile time, with the reason: `transport` (a Python `Transport` class cannot run; any Sentry
  server since 20.6 accepts envelopes), `before_breadcrumb`, `event_scrubber`, `error_sampler`,
  `ignore_errors`, profiling, Sentry Logs, `trace_propagation_targets`/`propagate_traces`,
  `functions_to_trace`, proxy and CA options, `_experiments`, and any other option.
- API: `capture_message(message, level=, tags=, extras=, contexts=, user=, fingerprint=)`,
  `capture_exception(error=None, ...)` (without an argument: the exception of the enclosing `except` block),
  `set_tag`, `set_tags`, `set_user`, `set_context`, `set_extra`, `set_level`, `add_breadcrumb(crumb=None,
  hint=None, **kwargs)`, `new_scope()` / `push_scope()` as context managers (the yielded scope's `set_*`,
  `remove_*`, `add_breadcrumb`, `clear*`, `capture_*`), `get_isolation_scope()`, `get_current_scope()`,
  `flush(timeout=)`, `last_event_id()`, `is_initialized()`. `capture_*(scope=...)`, `push_scope(callback)`
  and the rest of the API (`start_transaction`, `start_span`, `continue_trace`...) are refused.
- Scopes: the import (module globals, an app factory's `init_sentry()`, which runs at startup) has its own
  isolation scope; each request forks it (breadcrumbs cleared), like `SentryAsgiMiddleware`, so a tag set in
  one request never reaches another. Code FastAPI runs in a copy of the context (a `def` endpoint, the rest
  of the stack under a `BaseHTTPMiddleware`) shares the scope, as in Python; the dedupe state does not leak
  back.
- `FastApiIntegration` / `StarletteIntegration` (also enabled automatically, as in Python): unhandled
  exceptions are captured with `handled: false`, exceptions answered by a handler when their `status_code`
  is 5xx with `handled: true`; events carry the request (`method`, headers filtered by the default
  denylist unless `send_default_pii`, `query_string`, `url`, the JSON or form body within
  `max_request_body_size`, cookies and `REMOTE_ADDR` with PII). A transaction (`op: http.server`, named
  after the route path, or the URL when no route matched) is sent per sampled request, except HEAD and
  OPTIONS; an incoming `sentry-trace` header continues its trace. Only `transaction_style="url"` and the
  default `failed_request_status_codes` / `http_methods_to_capture` are supported.
- `LoggingIntegration(level=, event_level=)` (also a default integration): records at or above `level`
  become breadcrumbs and at or above `event_level` events (`logentry`, `logger`, `exc_info` as an
  exception with mechanism `logging`), from every logger. The binary does not know the project's logging
  configuration: the records that reach Sentry are those passing `PY2AXUM_LOG_LEVEL` (default `INFO`), so
  set it to the level of the project's root logger (Python's default is `WARNING`) for the same
  breadcrumbs.
- `SqlalchemyIntegration`: accepted; it adds no child span in the binary. `AsyncioIntegration`: only when
  `init()` runs at import time (Python then has no running loop to patch and leaves tasks alone); an
  exception never retrieved from a task is reported through the `asyncio` logger, as in Python.
- The default `EventScrubber`, the dedupe of the same exception object and the serializer's limits (depth 5,
  breadth 10 in databags, `max_value_length`, `_meta`) are applied.
- Differences, by nature of the binary: `sdk` is `sentry.rust` (its `integrations` list starts with
  `py2axum`); events carry no `modules`, `sys.argv` extra, `runtime` context or thread data; an exception
  has a single stack frame, its route handler (none outside a request), and `attach_stacktrace` adds no
  stack to messages; there are no child spans (database, middleware) in transactions, and no release-health sessions or client reports (the SDK's own counters). Every event and
  transaction carries the tag `py2axum.source`: the route handler's `file.py:line`, or the call site
  outside a request. The `Task exception was never retrieved` message does not show the task's repr.
  Events are sent from the Rust SDK's background thread; on SIGTERM the queue is flushed for
  `shutdown_timeout` (2 s by default), like the Python SDK's atexit hook.

## MCP servers (`mcp` 2.2)

A FastMCP-style server served from a raw ASGI route: `MCPServer(name, title=,
instructions=, version=)`, `@server.tool(name=, title=, description=)` on `async def` tools returning
`dict[str, Any]`, `ToolError`, `server.streamable_http_app(stateless_http=True, json_response=True,
transport_security=TransportSecuritySettings(enable_dns_rebinding_protection=False))`, `async with
server.session_manager.run()`, `await server.session_manager.handle_request(scope, receive, send)`.

- Transport like `mcp/server/streamable_http.py` without sessions: 413 above 4 MiB, `Invalid Content-Type
  header`, 406 (Accept), 415 (strict content type), `Parse error` (-32700), the JSON-RPC envelope validated
  like pydantic's union of the four message models (same `Validation error: ...` text), 202 for
  notifications and posted responses, GET = an SSE stream that never sends anything, DELETE/HEAD 405.
- `initialize` (version negotiation, capabilities, `serverInfo`, `instructions`), `ping`, `tools/list`,
  `tools/call`, empty `resources/list`, `resources/templates/list`, `prompts/list`, `resources/read` and
  `prompts/get` errors, -32601 for the rest; -32602 `Invalid request parameters` for malformed params.
- Tools: the `<function>Arguments` model and its `inputSchema` are computed at compile time (pydantic 2.13's
  JSON schema, a closed subset of types: `str/int/float/bool/date/datetime/time/timedelta/UUID`, `Literal`,
  `Optional`/unions, `list`, `dict[str, T]`, `Any`, project `BaseModel`s; `Field(description=, title=,
  default=, examples=)` and constraints), FastMCP's JSON pre-parsing of string arguments, validation errors
  as `Error executing tool <name>: <pydantic message>`, `ToolError` text, other exceptions as
  `Error executing tool <name>`, structured output (`structuredContent` + indented JSON text).
- Refused at compile time: sync tools, other return annotations, other `tool()` options, parameters
  starting with `_` or shadowing a `BaseModel` attribute, resources, prompts, any other `MCPServer`
  attribute, sessions (`stateless_http=False`), SSE responses (`json_response=False`), Host/Origin checks.
- Differences: the 2026-07-28 transport (a `mcp-protocol-version` outside the handshake list) answers its
  envelope and header errors like the SDK, but a well-formed request of that protocol gets -32022
  `Unsupported protocol version`; `capabilities` of `initialize` is only checked to be an object; JSON
  parse-error messages come from serde_json (the same wording as pydantic-core's jiter for the usual
  cases); floats in tool results use Python's repr rather than pydantic-core's formatting.

## Standard library

`datetime` (with time zones, `zoneinfo`, Pydantic and `isoformat` formats, `strptime` without `%f`/`%Z`,
`combine`, `fromtimestamp`), `decimal` (`_pydecimal` rules: 28 digits, ROUND_HALF_EVEN, exact
add/mul then rounding, CPython's division, `quantize`, `round`, formatting; mixing with float refused),
`uuid`, `json`, `re` (fancy-regex with Python syntax translated; consecutive empty matches follow Rust, not
Python 3.7+), `csv` (simplified `Sniffer`), `io.StringIO/BytesIO`, `pathlib`/`open()` (UTF-8, POSIX),
`os.environ`/`os.getenv` (read-only), `os.path`, `math`, `random` (OS-seeded), `secrets`, `hashlib`,
`hmac`, `base64`, `urllib.parse`, `string` constants, `time.time/monotonic/perf_counter`,
`statistics.median`, `logging` (stderr, `LEVEL:logger:message`, level from `PY2AXUM_LOG_LEVEL`; level constants,
`Logger.log` with a standard level),
`email.mime` (incl. `MIMEBase` with `set_payload` and `email.encoders.encode_base64`)/`email.utils` (`formataddr`, `formatdate`, `make_msgid`), `html.escape`, `ipaddress.ip_address`/`ip_network` (prefix length, `strict=`; membership, str/repr; no netmask form, no IPv6 scope id), `socket.getaddrinfo` (the C library's answer; family and kind are plain ints, not `AddressFamily`/`SocketKind` members; `gaierror` carries only its message),
`unicodedata.normalize/combining` (the Unicode version of the Python that ran the translation: code points it
leaves unassigned are left alone, as CPython does), `bytes()`, `pickle` (see below), `functools.wraps`, `inspect.iscoroutinefunction`,
`importlib.metadata.version("literal")` (resolved at compile time from what `uv sync --frozen` installs: the project of the `pyproject.toml` next to a `uv.lock` at `--root` or above, or a package of that lock; any other name raises `PackageNotFoundError`; refused without such a lock), `typing.get_args/get_origin/get_type_hints`, `collections.defaultdict` with a builtin type factory (`int`, `list`, `str`...) or `deque` (`type()` of it reports `dict`), `zipfile.ZipFile(io.BytesIO(), "w", ZIP_STORED | ZIP_DEFLATED, compresslevel=)` with `writestr(name, str | bytes,
compress_type=, compresslevel=)`, `namelist`, `close`/`with` (CPython's bytes: same headers, `0o600` permissions,
local time of the call, zlib raw deflate; a mode other than a literal `"w"` is refused; `write(path)`,
`ZipInfo`, ZIP64 and a file object other than `io.BytesIO` raise),
`socket.create_connection((host, port), timeout=)` (blocking connect, the last failure raised as CPython does
without `all_errors`; the socket supports `with`, `close`, `getpeername`, `getsockname`),
`cryptography`'s RSA keys (`rsa.generate_private_key(65537 or 3, key_size >= 1024)`, `private_bytes` in PEM or
DER, PKCS8 or TraditionalOpenSSL, `NoEncryption()` only; `public_key().public_bytes` as SubjectPublicKeyInfo or
PKCS1; `key_size`), `...` as a value (a "not given" sentinel default compared with `is`),
`collections.Counter` (from an iterable, a mapping or keywords; a missing key reads 0 and is not stored;
`most_common`, `update`/`subtract`, `elements`, `total`, `copy`, `+ - | &` between Counters, CPython's `repr`;
`type()` of it reports `dict`, and `==` compares like a dict, where CPython 3.10+ ignores zero counts),
`collections.deque` (`maxlen`, append/pop on both ends, `extend(left)`, `rotate`, `remove`, `index`, `count`, indexing; JSON-encoded as a list by FastAPI), `string.Formatter().vformat/format`, `string.Template` (`substitute`/`safe_substitute`, CPython's KeyError and invalid-placeholder ValueError). `str.format` and `Formatter` support `{}`/`{0}`/`{name}`, `!r`/`!s` and format specs; attribute/index fields (`{a.b}`, `{a[0]}`), nested specs and `!a` raise. Not yet: other `Formatter` methods.

`pickle.dumps/loads` use CPython's format (protocol 5 when the project targets Python ≥ 3.14, else 4):
bytes written by the binary are read by CPython and the other way round (e.g. a Redis cache shared with
Python workers). Supported: scalars, str/bytes, containers (shared references kept), datetime types,
time zones, `Decimal`, `UUID`, enums, project class instances (`__dict__` or `__slots__`), dataclasses,
Pydantic models. Refused: mapped (ORM) objects, other globals, out-of-band buffers. Integers beyond 64 bits
read back as integral `Decimal`s.

## Libraries

| Library | Scope |
|---|---|
| nh3 (0.3) | `clean` (the same ammonia crate, configured like nh3, same `ValueError`s), `clean_text`/`escape` without `tags`, `is_html`; `attribute_filter`, `url_relative` and `id_prefix` refused |
| httpx (0.28) | `AsyncClient` (`auth=` a `BasicAuth` or a `(user, password)` tuple), requests, timeouts, `raise_for_status`, exceptions; no redirects followed, no `files=`, cookies or custom transports |
| aiohttp (3.14) | `ClientSession` (`timeout=None` = default timeouts), `async with session.post(...) as resp`, `ssl=False`, `proxy=None`, `status/reason/text()/json()`, `ClientTimeout`; `reason` is the standard phrase; a per-request `timeout=None` keeps the session's timeout Replacing the request method (`aiohttp.ClientSession._request = wrap(aiohttp.ClientSession._request)`, `httpx.AsyncClient.request = ...`, e.g. to time outgoing calls) is supported: the clients call the replacement with the session, the method, the URL and the keyword arguments aiohttp passes (`allow_redirects=` for get/options/head, `data=` for post/put/patch); httpx's replacement receives only the keyword arguments given by the caller (httpx passes all its defaults). Other assignments to library attributes are refused. |
| yarl | `URL(str)`: `host`, `port` (scheme default), `scheme`, `path`, `query_string`, `fragment`, `user`, `password`, `str()` |
| redis.asyncio (redis-py 5+) | `from_url`/`Redis(...)`, get/set (ex, px, nx, xx, get)/setex/delete/exists/incr/decr/mget/expire/ttl/keys/scan_iter/hash commands/ping, `Retry(backoff, n)` (retries without the backoff delay), redis-py's encoding and exceptions |
| aio-pika 10 | `connect`/`connect_robust` (`async with`), `channel()`, `declare_queue(name, durable=...)`, `default_exchange.publish(Message(...), routing_key=)` with publisher confirms, `queue.get(no_ack=, fail=)` and the received message's properties, `ack()`; `AMQPConnectionError`, `QueueEmpty`. `connect_robust` does not reconnect after a connection loss; `DeliveryMode` members are plain ints (`2`, not `<DeliveryMode.PERSISTENT: 2>`); consumers (`queue.iterator()`, `consume`) are not supported |
| tenacity 9 | `@retry(stop=, wait=, retry=, before=, after=, before_sleep=, reraise=, retry_error_callback=)` on async and sync functions (bare `@retry` too), `stop_after_attempt/after_delay/never/any/all`, `wait_fixed/none/random/exponential/exponential_jitter/incrementing/combine/chain`, `retry_if_exception_type/not_exception_type/exception/result`, `retry_always/never/any/all`, `|`/`&`/`+` combinations, `before_sleep_log`, `RetryError` (`last_attempt`), the retry state seen by callbacks (`attempt_number`, `outcome`, `fn`, `args`...). `str(RetryError)` shows `0x0` instead of CPython's object address. Refused: `sleep=`, `retry_error_cls=`, `before_sleep_log(exc_info=True)`, `Retrying`/`AsyncRetrying` objects |
| prometheus_client 0.26 | `Counter`, `Gauge`, `Summary`, `Histogram`, `Info`, `Enum` (namespace/subsystem/unit, buckets, multiprocess_mode, states, `registry=`), `.labels()` by position or keyword, `inc`/`dec`/`set`/`set_to_current_time`/`set_function`/`observe`/`info`/`state`/`reset`/`remove`/`remove_by_labels`/`clear`, `time()`, `count_exceptions()`, `track_inprogress()` as context managers and decorators (a plain function, like the library: on an `async def` it times the creation of the coroutine), exemplar validation, `CollectorRegistry(target_info=)`, `register`/`unregister`/`get_sample_value`/`get_target_info`/`set_target_info`, `REGISTRY`, `generate_latest(registry, escaping=)` (the text format byte for byte, the four name escapings), `openmetrics.exposition.generate_latest` (OpenMetrics 1.0 with units and exemplars), `restricted_registry(names)` (its collectors in registration order: CPython iterates a set), `start_http_server(port, addr=, registry=)` (the exporter on its own port: content negotiation, gzip, `name[]`, OPTIONS/405, `/favicon.ico`; its `Server`/`Date` headers differ; TLS options refused), `CONTENT_TYPE_LATEST`, `disable_created_metrics()`, `PROMETHEUS_DISABLE_CREATED_SERIES`, CPython's messages. `REGISTRY` holds `GC_COLLECTOR`, `PLATFORM_COLLECTOR` and `PROCESS_COLLECTOR` (unregistering them works, their names stay reserved) but they produce no samples: the binary is not a CPython process, so `python_gc_*`, `python_info` and `process_*` are absent from its output. When several names collide, `DuplicateTimeseries` lists them in the collector's order (CPython prints a set, in hash order). Multiprocess mode (`PROMETHEUS_MULTIPROC_DIR` set at startup): values are written to the library's per-process files (same names, keys and binary layout, so Python workers can share the directory) and `multiprocess.MultiProcessCollector(registry, path=)` merges every file of the directory like prometheus_client (gauge modes `all`/`live*`/`min`/`max`/`sum`/`mostrecent`, histogram accumulation, `pid` labels), `mark_process_dead(pid)`. Refused: custom collectors, `make_asgi_app`/`make_wsgi_app`, the push gateway |
| python-jose (3.5) | `jwt.encode/decode` with HMAC algorithms and decode options; identical tokens, same exceptions |
| PyJWT (2.15) | `jwt.encode` (`algorithm=`, `headers=`, `sort_headers=`; datetimes of `exp`/`iat`/`nbf` encoded from a copy), `jwt.decode` / `decode_complete` (`algorithms=`, `options=` with every `verify_*`, `require`, `strict_aud`, `enforce_minimum_key_length`; `audience=`, `issuer=`, `subject=`, `leeway=` as a number or a timedelta), `get_unverified_header`, with HS256/HS384/HS512 and `none`: identical tokens, the same checks in the same order, the `jwt.exceptions` hierarchy and messages (`MissingRequiredClaimError.claim` included). Refused at transpile time: an asymmetric algorithm (RS*, ES*, PS*, EdDSA) written as a literal, `json_encoder=`, `detached_payload=`, `verify=`, extra keyword arguments, `PyJWK`/`PyJWKClient`. At run time an asymmetric algorithm raises a py2axum RuntimeError (500), and so does an HMAC key shaped like a DER structure (PyJWT tries it as a public key). `InsecureKeyLengthWarning` is not emitted. |
| bcrypt (5.0), pyotp (2.9) | same hashes and codes |
| itsdangerous (2.2) | `URLSafeTimedSerializer` with the default signer and serializer |
| cryptography | `Fernet` (tokens readable both ways), `Fernet.generate_key()` |
| jinja2, aiosmtplib, email | templates (minijinja with Jinja2's output), MIME messages, SMTP sending; `Jinja2Templates(directory=)` and `TemplateResponse(request, name, context, status_code, headers, media_type)` (`request` added to the context, date `strftime`/`isoformat` callable from a template; `env.filters[name] = f` with a project function or lambda, called synchronously during the render; `context_processors`, `env=` and `url_for` are not supported) |
| pywebpush (2.3) | aes128gcm encryption, VAPID (py-vapid rules), `WebPushException` |
| google-auth (2.49) | `id_token.verify_oauth2_token` / `verify_token` |
| alembic | `alembic.config.Config` ini reading only |
| psutil (7) | `cpu_percent`, `virtual_memory`, `disk_usage`, `pids` |
| sentry-sdk 2.x | see [Sentry](#sentry-sentry-sdk-2x) |
| python-dateutil (2.9) | `relativedelta(years=, months=, weeks=, days=, hours=, minutes=, seconds=, microseconds=)` with integers: normalization, `repr`, attributes, `date`/`datetime` `+`/`-` (month-end clamping), `+`/`-`/`*`/`==` between deltas. Refused: absolute fields (`year=`, `day=`, `weekday=`...), `relativedelta(dt1, dt2)`; fractional days/hours raise a py2axum `TypeError` |
| xmltodict (1.0) | `parse(str or UTF-8 bytes, process_namespaces=, namespaces=)` with the default options (`@` attributes, `#text`, lists for repeated elements, whitespace stripped, comments and processing instructions ignored); `dict_constructor=` is accepted, the result is always made of dicts. Malformed XML raises `xml.parsers.expat.ExpatError` with expat's message and position for the usual errors (others may word or place it differently). A DOCTYPE is refused at run time; other options are refused |

## Hybrid deployments

Routes left to Python (`--python-side PATH`, `--python-side auto`, `--python-side mount`) are relayed to
`PY2AXUM_PYTHON_URL` (method, path, query, headers and body; the response streamed back). Without that
variable, a request for one of their paths answers FastAPI's 404 (`{"detail":"Not Found"}`): it never falls
through to a translated route whose pattern also matches. Relayed requests go through the Python
application's own middleware stack, not the binary's; WebSockets are never relayed. See
[how-it-works.md § Hybrid deployments](how-it-works.md#4-hybrid-deployments).

## Runtime environment

The binary reads its configuration from the environment ([the list](getting-started.md#10-runtime-configuration));
it does not read `.env` files (pydantic-settings reads environment variables only, see [Pydantic v2](#pydantic-v2)): export the
variables, as an orchestrator does.

## Not supported

An application wrapped in a project ASGI class at module level (`app = Wrapper(api)`, refused: the
binary would serve `api` without it) and mounted ASGI apps (left to Python with `--python-side mount` when registered last), libraries not listed, C extensions,
`eval`/`exec`, metaclasses, multiple inheritance of project classes, OpenAPI `/docs` in the binary.

## Large list responses

An endpoint whose `response_model` is `list[Schema]` and that returns the session's rows directly —
`return (await session.execute(stmt)).scalars().all()`, `return (await session.scalars(stmt)).all()`, or
`result = await session.execute(stmt)` followed by `return result.scalars().all()` — is answered as a stream:
the rows are read from PostgreSQL with a cursor on a connection of the pool, each one validated by the
schema and serialized, and the JSON array is sent in 64 KiB blocks. Memory stays constant whatever the size
(a 57 MB response: 13–16 MiB of RSS instead of 1.6 GiB buffered) and the first byte leaves after the first
rows. The bytes are the ones FastAPI sends.

Streaming applies only when nothing observable changes: the session has written nothing in its open
transaction (no flushed or pending change, no `begin_nested()`), the statement selects one mapped class
without loader options (`selectinload`...) nor `with_for_update()`, the schema reads only columns of that
class (no relationship, property or `mode="before"` model validator), the return is not inside
`try`/`with`, the status has a body, and its literal `LIMIT`, if any, is above `PY2AXUM_STREAM_MIN_ROWS`
(default 1000, so paginated lists keep the usual path). Otherwise the statement runs as written.
`PY2AXUM_STREAM_CHUNK` sets the block size (bytes); `--no-stream` turns streaming off at generation.

Differences: a streamed response has no `content-length` (chunked transfer) — a response that fits in the
first block keeps it — and is therefore compressed by `GZipMiddleware` even under `minimum_size`. The
statement runs when the response starts, on its own connection (READ COMMITTED: the same rows as in the
session, which wrote nothing). An error before the first block is a 500 like FastAPI's; after it (a row
failing response validation, a lost connection) the connection is closed mid-body, where FastAPI, which
serializes everything first, would answer 500.

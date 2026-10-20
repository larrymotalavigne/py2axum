# Bigger applications

Real FastAPI applications are split across modules: `APIRouter`s included with prefixes, an app built by a
factory function, sub-applications mounted under a path. py2axum resolves the imports statically and
registers the routes in the order Python would, then routes requests like Starlette.

The documentation's examples are one application split in modules: each page of this tutorial has
its `APIRouter` in `docs_src/tutorial/`, and `docs_src/main.py` includes them all. Imports between them are
relative (`from ..db import SessionDep`) and resolved statically. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/main.py"
--8<-- "docs_src/main.py"
```

## What is native

- Custom route classes (FastAPI's "Custom Request and APIRoute class"): `APIRouter(route_class=R)` and
  `app.router.route_class = R` (for the app's routes declared after it in the same module), where `R` subclasses
  `APIRoute` and overrides only `get_route_handler()`: `super().get_route_handler()` is the route's own handler,
  called with the request and returning the Response object (`status_code`, mutable `headers`, its class in
  `type()` and `repr()`), its exceptions raised as they are (`RequestValidationError`, `HTTPException`). The
  handler is made once per route, at its first request (FastAPI: when the route is created). A project subclass
  of `Request` built as `GzipRequest(request.scope, request.receive)` wraps the request: its attributes and
  methods fall back to it, `super().body()` reads the raw body, and its own `body()` is what the route parses
  when it has a body or form parameter (an error there: FastAPI's 400 `There was an error parsing the body`); the
  endpoint's `request` parameter is that instance. Refused: reading `self` (the APIRoute) in `get_route_handler`,
  other overridden methods, `route_class` set twice, an app route in another module than the assignment.
- `FastAPI()` apps, `APIRouter(prefix=, tags=, dependencies=, default_response_class=)`, `include_router`,
  routers split across modules (also outside the translated directory, a `src/` layout: the modules the app
  imports), apps built by a factory function (`create_app()`), including endpoints defined inside it; a
  local of the factory assigned once is evaluated once. `if` statements around registrations are evaluated
  at startup in the factory's scope (e.g. `if not settings.TESTING:`); the factory's parameters have their
  default values (the server calls it without arguments).
- `include_router(router, prefix=<expression>)` at module level (e.g. `prefix=settings.API_V1_PREFIX`): the
  prefix is evaluated at startup, after the module globals, so the environment still overrides the
  settings; it is checked like FastAPI (must start with `/`, must not end with `/`; a failure stops the
  binary as an import error stops uvicorn).
- A project function given the app that includes routers (`def add_routers(app): app.include_router(...)`),
  called once with the app in the factory or at module level: its routers are registered where it is called.
  Its `prefix=` may read its parameters (as the call passes them, or their literal defaults) or module globals
  (evaluated at startup). `for r in ROUTERS: app.include_router(r)` over a literal tuple of the module (or a
  list it never changes) is unrolled. A function without parameters that only builds a router from routers
  (`def build(): r = APIRouter(prefix="/v1"); r.include_router(...); return r`) is included by its call
  (`app.include_router(build())`).
- `include_router` under an `if` at module level (`if settings.FASTAPI_ENV == "development":`, `if
  os.getenv("ENABLE_X"):`, `else` branches, nested `if`s, in the app or in a router included later): each test
  is read once at startup, after the module globals, and the routes of a branch that is off are not served (a
  404, as if never registered). A test that calls anything but `os.getenv`/`os.environ.get`, `bool`,
  `isinstance` or `str.lower/upper/strip`, or a registration under a `try`, a loop or a `with`, is refused.
- An import guard on the project's own code is decided like CPython when that code is absent (an optional,
  gitignored module): after `try: from app.api import extra / except ImportError: extra = None`,
  `if extra is not None: app.include_router(extra.router)` never runs and `if HAS_EXTRA:` keeps one branch.
- Imports are resolved statically, without executing code: relative imports, re-exports from `__init__`,
  lazy imports inside functions (visible from nested functions and lambdas), `--root` as `sys.path`.
- Routing like Starlette: registration order (each `include_router` at its place among the routes of the app
  or router it is called on), decoded path, first match on path and method wins, then 405
  with the methods of the first path match, then a 307 redirect with/without the trailing slash, then 404.
  No implicit HEAD. The redirect uses `http://` (as uvicorn does without trusted proxy headers).
- Raw ASGI routes: `app.add_route(path, obj, methods=...)` / `app.router.add_route(...)` at module level, with
  an instance of a project class defining `async def __call__(self, scope, receive, send)` (Starlette runs
  anything that is not a function or a method as an ASGI app). The app gets an ASGI 3 `scope` dict (the keys
  uvicorn and Starlette's router set, `scope["app"]` with an empty `dependency_overrides`), `receive` (the
  body in one `http.request` message, then nothing until the client leaves) and `send`
  (`http.response.start`/`http.response.body`, streamed when `more_body` is true). `Request(scope, receive)`
  (the request being served only), a response object called as an ASGI app
  (`await JSONResponse(...)(scope, receive, send)`), a generator dependency called by hand
  (`gen = get_session(); s = await anext(gen); ...; await gen.aclose()`).
- Routing objects as values: `request.app`, `app.router.routes` as FastAPI 0.141+ lists them (its docs
  routes, an `_IncludedRouter` per `include_router` with its `original_router`, `APIRoute`s, the added
  `Route`s), their `path`, `path_format`, `methods`, `name`, `matches(scope)` returning
  `starlette.routing.Match` and the child scope, `isinstance(r, Route | APIRoute)`. `request.scope` is a
  snapshot dict with `type`, `http_version`, `scheme`, `method`, `root_path`, `path`, `raw_path`,
  `query_string`, `headers`, `client`, `app`, and, once the router has run, `path_params` and `route` (the
  `APIRoute`, as Starlette sets it); the `endpoint` of scopes is None. `FastAPI(docs_url=...)` and the other
  `*_url` options must be literals for `app.routes` to be read.

Routes added by a `configure(app)` function are described in [Middleware](middleware.md#what-is-native).

### Mounted applications

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
  the mount are not relayed (see [WebSockets](websockets.md)).

### Routers of libraries

- `app.include_router(router)` where `router` comes from a library (imported from it, or returned by one of its
  functions: `app.include_router(lib.make_router(...), prefix="/x")`) is not translated: py2axum reads only the
  project, so the library's routes are unknown. Included among the app's last registrations, its routes stay on
  the Python side (`--python-side mount`, implied by `--python-side auto`), like a final mount: with
  `PY2AXUM_PYTHON_URL` set, a request that no translated route fully matches (path and method) is relayed to the
  Python application, under the literal `prefix=` of the call, or every such request without one (the Python
  application then answers the 404s and 405s itself, the library's routes included: a POST on a translated GET
  route reaches the library's POST route of the same path, as in Starlette). Refused with `file:line`
  (P2A0315): a registration after it (a route, an `include_router` of the project, a function given the app
  that registers routes, even through another function: Starlette would try the library's routes first), a
  conditional include (under `if`, `try`, a loop, `with`), a prefix that is not a literal string, an include on
  a router instead of the app, and registrations of the app in another module or function.
- `FastAPI(root_path="/api/v1")` (a literal; `root_path_in_servers=` only changes the OpenAPI schema): the scope's
  `root_path`, stripped from the path before routing when the path starts with it (Starlette's
  `get_route_path`, so `/app` and `/api/v1/app` both match), part of `request.base_url`. Refused together with
  WebSocket routes, `app.mount()` or Python-side routes.

## What stays in Python

- Mounted applications (above), left to Python with `--python-side mount`.
- Routers of libraries (above), left to Python with `--python-side mount`.
- Raw ASGI routes: a class as endpoint (`HTTPEndpoint`) is refused.

## Differences

- `include_router` under an `if` that is off at startup: its routes are not served, but `app.routes` still
  lists them.
- `include_router`: a runtime prefix containing a path parameter (`{...}`) stops the binary; a non-literal
  prefix inside a function (app factory) is refused at compile time, except in a function given the app (above); `--python-side` cannot name a route under
  a runtime prefix (it is compiled).
- Raw ASGI routes: `allow` of a 405 lists the methods in declaration order (Starlette iterates a set, so its
  order varies between processes); mutations of `dependency_overrides` are lost.
- A 405 on an added `Route` lists `GET, HEAD` in that order (CPython: set order).

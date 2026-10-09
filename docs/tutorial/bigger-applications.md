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

- `FastAPI()` apps, `APIRouter(prefix=, tags=, dependencies=)`, `include_router`, routers split across
  modules, apps built by a factory function (`create_app()`), including endpoints defined inside it; a
  local of the factory assigned once is evaluated once. `if` statements around registrations are evaluated
  at startup in the factory's scope (e.g. `if not settings.TESTING:`); the factory's parameters have their
  default values (the server calls it without arguments).
- `include_router(router, prefix=<expression>)` at module level (e.g. `prefix=settings.API_V1_PREFIX`): the
  prefix is evaluated at startup, after the module globals, so the environment still overrides the
  settings; it is checked like FastAPI (must start with `/`, must not end with `/`; a failure stops the
  binary as an import error stops uvicorn).
- Imports are resolved statically, without executing code: relative imports, re-exports from `__init__`,
  lazy imports inside functions (visible from nested functions and lambdas), `--root` as `sys.path`.
- Routing like Starlette: declaration order, decoded path, first match on path and method wins, then 405
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

## What stays in Python

- Mounted applications (above), left to Python with `--python-side mount`.
- Raw ASGI routes: a class as endpoint (`HTTPEndpoint`) is refused.

## Differences

- `include_router`: a runtime prefix containing a path parameter (`{...}`) stops the binary; a non-literal
  prefix inside a function (app factory) is refused at compile time; `--python-side` cannot name a route under
  a runtime prefix (it is compiled).
- Raw ASGI routes: `allow` of a 405 lists the methods in declaration order (Starlette iterates a set, so its
  order varies between processes); mutations of `dependency_overrides` are lost.
- A 405 on an added `Route` lists `GET, HEAD` in that order (CPython: set order).

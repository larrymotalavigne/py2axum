# Middleware

FastAPI applications add middleware with `app.add_middleware(...)` or `@app.middleware("http")`: code that
runs around every request. The binary rebuilds Starlette's middleware stack in the same order (see
[Handling errors](errors.md#what-is-native) for the layers around your middleware) and runs
`BaseHTTPMiddleware` subclasses, `@app.middleware("http")` functions and raw ASGI middleware classes of the
project.

A function middleware (`@app.middleware("http")`) and a `BaseHTTPMiddleware` subclass, both
adding a header to every response. Like every example on this site, it is compiled and compared with FastAPI in CI ([how](testing.md)).

```python title="docs_src/main.py (excerpt)"
--8<-- "docs_src/main.py:middleware"
```

## What is native

- `GZipMiddleware` (`Vary: Accept-Encoding` like Starlette),
  `BaseHTTPMiddleware` subclasses (one instance per app, built on the first request; `call_next`, mutable
  `response.headers`; with or without their own `__init__`), `@app.middleware("http")`. `CORSMiddleware` has
  [its own page](cors.md).
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
- `starlette_context` 0.5's `RawContextMiddleware(plugins=(RequestIdPlugin(), CorrelationIdPlugin()))`
  (plugin options `force_new_uuid`, `validate`, `version=4` as literals): the id read from its header or a new
  `uuid4().hex`, validated like `uuid.UUID(value)` (400 with an empty body otherwise), appended to the
  response headers.
- `request.cookies` (Starlette's parser), `request.client` (TCP peer), `request.url`, `request.headers`,
  `request.state`, `request.body()`/`json()`.
- A project function given the application from the factory or the app's module (`configure(app)`) runs
  while the middleware stack is built (on the first request, when Starlette instantiates its middlewares):
  in it, `app.add_middleware(ProjectMiddleware, ...)` with a project `BaseHTTPMiddleware` subclass or raw
  ASGI class, `@app.exception_handler(...)` on its nested functions, `app.add_exception_handler(...)`,
  `@app.middleware("http")`, `app.add_route(path, endpoint, methods=)` (a Starlette `Route`: GET implies
  HEAD; such routes are tried after the declared ones, as when added last), `app.routes`.
- Starlette's `TrustedHostMiddleware(allowed_hosts=[...], www_redirect=...)` (Host header parsed like
  Starlette 1.7: `Invalid host header` 400, `www.` redirect) and `HTTPSRedirectMiddleware` (307 to the https URL,
  port dropped when 80 or 443), with literal options. HTTP requests only: with WebSocket routes they are
  refused (a WebSocket handshake does not cross the binary's middleware stack).

## What stays in Python

- Raw ASGI middleware: `http.response.trailers`/`pathsend`/`zerocopy` messages and a rewritten `root_path`
  raise at run time.
- The `context` object of `starlette_context` itself is not available.
- In a `configure(app)` function, other registrations than those above are refused.
- An application wrapped in a project ASGI class at module level (`app = Wrapper(api)`) is refused: see
  [Not supported](../supported.md#not-supported).

## Differences

- **Known difference:** `HTTPSRedirectMiddleware` with no `Host` header or one Starlette cannot parse (or a port
  above 65535): Starlette redirects to the server's own address (`scope["server"]`), which the binary does not
  know; it answers 500 instead.
- Differences (raw ASGI middleware): `GZipMiddleware` always runs outermost, so a middleware added after it
  sees the response before compression; raw middlewares run for `http` scopes only (`websocket` and `lifespan`
  pass as if the middleware let them through, its usual first line); a wrapped `receive` is read to the end of
  the body before the rest of the stack runs, and an original `receive` already consumed by an outer layer
  still yields the body (uvicorn would wait for the client to disconnect); the 500 for a middleware returning
  without a response is produced by the layer, so outer middlewares see it (uvicorn produces it itself).

GZip and streamed responses: see [Large list responses](../advanced/streaming.md#differences).

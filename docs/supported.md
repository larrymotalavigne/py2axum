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

The binary reads its configuration from the environment ([the list](reference/environment.md));
it does not read `.env` files (pydantic-settings reads environment variables only, see
[Models](tutorial/models.md)): export the variables, as an orchestrator does.
Limits on untrusted input (an optional body cap, the stack size) and the differences they imply are in
[Security § Request input](advanced/security.md#request-input).

## Not supported

An application wrapped in a project ASGI class at module level (`app = Wrapper(api)`, refused: the
binary would serve `api` without it) and mounted ASGI apps (left to Python with `--python-side mount` when
registered last), libraries not listed, C extensions, `eval`/`exec`, metaclasses, multiple inheritance of
project classes, OpenAPI `/docs` in the binary, and assigning an attribute of the application at module level
other than `app.state` and `app.dependency_overrides[f] = g` (`app.router.route_class = ...`, a custom
`APIRoute`, `app.openapi = ...` are refused with `file:line`). Not native, counted apart in the
[documentation coverage](coverage.md): what only changes the OpenAPI schema or the documentation pages
(`/openapi.json`, `/docs`, custom docs assets), SQLModel, OpenTelemetry's `FastAPI(telemetry=...)`, GraphQL
routers, `ORJSONResponse`/`UJSONResponse`, stdlib dataclasses as request or response types, security scopes
(`SecurityScopes`, `Security(scopes=)`), `AfterValidator` and other functional validators in `Annotated`.

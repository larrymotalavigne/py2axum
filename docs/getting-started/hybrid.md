# Hybrid mode: routes left to Python

A route that does not translate does not have to block the whole application: it can stay in Python, served by
your application under uvicorn, while the binary serves everything else and relays those paths to it. How the
routes are chosen and relayed is explained in [How it works § Hybrid deployments](../advanced/how-it-works.md#4-hybrid-deployments).

## Run the two processes

Run the same application with uvicorn and point the binary at it:

```bash
(cd examples/bookshelf && pip install -r requirements.txt && \
  DATABASE_URL=postgresql+psycopg://localhost/bookshelf uvicorn app.main:app --port 8000) &
DATABASE_URL=postgresql://localhost/bookshelf PY2AXUM_PYTHON_URL=http://127.0.0.1:8000 \
  ./build/bookshelf/target/release/bookshelf
```

The binary is the only entry point. A request for a python-side path (`GET /books/export.zip`) is relayed to
`PY2AXUM_PYTHON_URL` (method, headers and body; the response is streamed back unchanged); everything else
never touches Python. In production the Python process only needs to be reachable from the binary.

- `--python-side auto` moves every route that does not translate, and prints each with its reason. Pin the
  list with explicit flags (`--python-side '/books/export.zip'`) if you want deployments to change only when you
  decide: with `auto`, a route that becomes translatable after a py2axum upgrade moves to the binary.
- `--python-side lifespan` leaves `FastAPI(lifespan=...)` to Python (otherwise it is compiled, or refused;
  `auto` leaves it there when it does not translate, see [What auto leaves to Python](#what-auto-leaves-to-python)).
- A path moves as a whole (all its methods), and the relayed request goes through the Python app's own
  middleware stack. State the two processes share must be external: database, Redis, a broker.
- A WebSocket route moves like an HTTP one (`--python-side '/ws/{room}'`, or `auto`). WebSocket connections are relayed as well: the binary makes the handshake with the Python process first, so the client gets its decision (101 with its subprotocol and headers, or its rejection), then messages and close frames (code, reason) go through unchanged.

## What auto leaves to Python

`--python-side auto` moves the smallest part of the application that Python can serve without any observable
difference, and prints each part with the error that moved it:

| What does not translate | What goes to Python |
|---|---|
| A route (its endpoint, dependencies, models, the functions it reaches) | That route's path |
| Module code of a library translated from source (`--translate-libs`) | The routes using the library |
| The lifespan, or an `@app.on_event` handler | The lifespan, and each route that reads what it sets |
| A middleware, an exception handler, module code of the project, an app option | The whole application |

**The lifespan alone.** It runs in the Python process only, so what it sets in the process (an `app.state`
attribute, a lifespan state key, a module global it rebinds or fills, a class attribute) is never set in the
binary: a translated route that reads such a value is moved to Python too (`reads app.main.CACHE, which the
lifespan (left to Python) sets`). The whole application moves instead when a middleware reads it, or when the
lifespan changes something py2axum cannot bound: an object it was handed or that it aliases, a value of a
library the binary maps, the logging or Sentry configuration, the environment, or a process-wide setting
(`random.seed()`, `locale.setlocale()`...). Calls on an engine, a session factory or a logger change nothing the
binary reads.

**The whole application.** A middleware or an exception handler applies to every request, 404s included, so
no route can be served without it: the binary keeps no route and relays every request to
`PY2AXUM_PYTHON_URL` (`python-side (auto): the whole application — ...`), and the Python application's own
stack, handlers and lifespan answer each one. Fix the reported error to bring the routes back to the binary.

!!! note "Differences"
    - The Python process starts on its own: a request can reach the binary before the Python lifespan has
      finished (tables it creates, a cache it fills in Redis). Make the binary's readiness depend on the Python
      process's, as for any hybrid deployment.
    - The lifespan's own output (its `print` and log lines) comes from the Python process.
    - With the whole application relayed, the binary still prints uvicorn's startup lines and an access line
      per request in uvicorn's default format, next to the Python process's: run it with `--no-access-log`
      if you want one access log.

## What is relayed

Routes left to Python (`--python-side PATH`, `--python-side auto`, `--python-side mount`) are relayed to
`PY2AXUM_PYTHON_URL` (method, path, query, headers and body; the response streamed back). Without that
variable, a request for one of their paths answers FastAPI's 404 (`{"detail":"Not Found"}`): it never falls
through to a translated route whose pattern also matches. Relayed requests go through the Python
application's own middleware stack, not the binary's. WebSocket connections are relayed as well: the binary makes the handshake with the Python process first, so the client gets its decision (101 with its subprotocol and headers, or its rejection), then messages and close frames (code, reason) go through unchanged; pings are answered on each side, as
uvicorn does, and a Python application ending without closing drops the client's connection too (1006).
Hop-by-hop headers are not relayed; `X-Forwarded-*` are, and the relay connects from `127.0.0.1`: see
[Security § Hybrid deployments](../advanced/security.md#hybrid-deployments-the-python-side-relay) for the sidecar's
`--forwarded-allow-ips`.

`--python-side` routes (and `auto`'s) are relayed to `PY2AXUM_PYTHON_URL` before the binary's middleware
stack: the Python application's own middlewares answer for them.

!!! note "Difference"
    State a middleware keeps in memory (a rate limiter's counters...) is per process, as with several uvicorn
    workers: a counter shared by translated and Python-side routes counts each request in one process only
    (e.g. `X-RateLimit-Remaining`).

Mounted applications and routers of libraries left to Python (`--python-side mount`) are described in
[Bigger applications](../tutorial/bigger-applications.md#mounted-applications). For containers, see
[Deployment § Hybrid](../advanced/deployment.md#hybrid-binary-python-for-the-rest).

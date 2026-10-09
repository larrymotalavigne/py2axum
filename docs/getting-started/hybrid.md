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
- `--python-side lifespan` leaves `FastAPI(lifespan=...)` to Python (otherwise it is compiled, or refused).
- A path moves as a whole (all its methods), and the relayed request goes through the Python app's own
  middleware stack. State the two processes share must be external: database, Redis, a broker.
- WebSockets are never relayed: a WebSocket route must translate, or be routed to Python by your ingress.

## What is relayed

Routes left to Python (`--python-side PATH`, `--python-side auto`, `--python-side mount`) are relayed to
`PY2AXUM_PYTHON_URL` (method, path, query, headers and body; the response streamed back). Without that
variable, a request for one of their paths answers FastAPI's 404 (`{"detail":"Not Found"}`): it never falls
through to a translated route whose pattern also matches. Relayed requests go through the Python
application's own middleware stack, not the binary's; WebSockets are never relayed.
Hop-by-hop headers are not relayed; `X-Forwarded-*` are, and the relay connects from `127.0.0.1`: see
[Security § Hybrid deployments](../advanced/security.md#hybrid-deployments-the-python-side-relay) for the sidecar's
`--forwarded-allow-ips`.

`--python-side` routes (and `auto`'s) are relayed to `PY2AXUM_PYTHON_URL` before the binary's middleware
stack: the Python application's own middlewares answer for them.

!!! note "Difference"
    State a middleware keeps in memory (a rate limiter's counters...) is per process, as with several uvicorn
    workers: a counter shared by translated and Python-side routes counts each request in one process only
    (e.g. `X-RateLimit-Remaining`).

Mounted applications left to Python (`--python-side mount`) are described in
[Bigger applications](../tutorial/bigger-applications.md#mounted-applications). For containers, see
[Deployment § Hybrid](../advanced/deployment.md#hybrid-binary-python-for-the-rest).

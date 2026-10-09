# Deployment

The binary is one executable with no Python at run time: it reads its configuration from the environment
([the list](../reference/environment.md)), listens on `HOST`:`PORT`, and talks to PostgreSQL at `DATABASE_URL`.
This page shows how to build it in a container image and how to run it, alone or next to a Python process for
the routes left to Python ([hybrid mode](../getting-started/hybrid.md)).

## Building with Docker

A multi-stage build keeps the Python and Rust toolchains out of the final image: transpile, compile, then ship
the binary alone (an image of a few tens of MB on `debian:bookworm-slim`). The reference files are in
[`examples/docker`](https://github.com/larrymotalavigne/py2axum/tree/main/examples/docker), set up for the [bookshelf example](https://github.com/larrymotalavigne/py2axum/tree/main/examples/bookshelf) in hybrid mode:

| File | |
|---|---|
| [`Dockerfile`](https://github.com/larrymotalavigne/py2axum/blob/main/examples/docker/Dockerfile) | transpile (`python:3.13-slim`), compile (`rust:1-bookworm`, cargo caches as BuildKit cache mounts), run: the binary alone on `debian:bookworm-slim`, as `nobody` |
| [`Dockerfile.python`](https://github.com/larrymotalavigne/py2axum/blob/main/examples/docker/Dockerfile.python) | the same application under uvicorn, for the routes left to Python |
| [`compose.yaml`](https://github.com/larrymotalavigne/py2axum/blob/main/examples/docker/compose.yaml) | PostgreSQL, the Python sidecar (not published) and the binary (port 8080, the only entry point) |

```bash
docker compose -f examples/docker/compose.yaml up --build     # from the repository root
curl -s localhost:8080/health
```

### Adapting the Dockerfile to your application

The `Dockerfile` takes build arguments, so it can often be used as is:

```bash
docker build -f Dockerfile \
  --build-arg APP_DIR=. --build-arg PACKAGE=api --build-arg NAME=api \
  --build-arg PY2AXUM_FLAGS="--python-side auto" -t myapp .
```

- `APP_DIR` is the directory you run uvicorn from (the import root, `--root`), `PACKAGE` the package holding
  the FastAPI application, `NAME` the binary's name.
- `PY2AXUM_FLAGS`: `--python-side auto` leaves every route that does not translate to Python; list them
  explicitly (`--python-side '/reports/{id}.pdf'`) to decide yourself when a route moves to the binary; leave
  it empty if everything translates (the build then fails on any refused construct, which is what you want
  in CI).
- Replace the lines that install py2axum from this repository with `RUN pip install --no-cache-dir
  "py2axum==<version>"`, and pin that version: a new py2axum can translate more routes, or the same ones
  differently.
- Copy your `uv.lock` (or requirements file) with the application: library behaviours follow its versions.
- The first stage runs `py2axum check` before generating, so the build log lists every route's verdict.

The release profile uses fat LTO and one codegen unit: expect several minutes for the first `cargo build`. The
BuildKit cache mounts keep the cargo registry and the target directory between builds, so later builds only
recompile the generated code. The generated crate ships with the `Cargo.lock` py2axum is tested with
(`cargo build --locked`).

## Running it

- Environment: `DATABASE_URL`, `HOST`, `PORT`, `PY2AXUM_PYTHON_URL` (hybrid mode), `PY2AXUM_LOG_LEVEL`,
  `PY2AXUM_SHUTDOWN_TIMEOUT`, plus your application's own settings; the full list is in
  [Environment variables](../reference/environment.md). The binary does not
  read `.env` files.
- Run database migrations as a separate step (a Kubernetes Job, an init container, `alembic upgrade head` in
  your Python image): the binary does not create tables.
- The binary handles SIGTERM like uvicorn (stops accepting, finishes in-flight requests, exits with 143);
  keep `PY2AXUM_SHUTDOWN_TIMEOUT` (25 s by default) below the orchestrator's grace period.

## Hybrid: binary + Python for the rest

The binary is the public entry point; the Python container only needs to be reachable from it:

```yaml
services:
  api:                       # the binary
    build: {context: ., dockerfile: Dockerfile}
    environment:
      DATABASE_URL: postgresql+psycopg://app@db/app
      PY2AXUM_PYTHON_URL: http://python:8000
    ports: ["8080:8080"]
  python:                    # the same application, for the --python-side paths
    build: {context: ., dockerfile: Dockerfile.python}
    environment:
      DATABASE_URL: postgresql+psycopg://app@db/app
```

The binary relays matching requests (method, headers, body) and streams the response back. Both processes
must share their settings (a token secret, for instance) and keep shared state outside themselves (database,
Redis). WebSockets are not relayed: a WebSocket route must translate, or be routed to the Python service by
your ingress. If you leave the lifespan to Python (`--python-side lifespan`), its startup and shutdown code
runs in the Python container only. More in [How it works § Hybrid deployments](how-it-works.md#4-hybrid-deployments).

## Kubernetes

The same rules apply in a cluster:

- One container runs the binary; it needs `DATABASE_URL` and your application's own settings as environment
  variables (it does not read `.env` files).
- Run migrations as a Kubernetes Job or an init container: the binary does not create tables.
- Keep `PY2AXUM_SHUTDOWN_TIMEOUT` (25 s by default) below `terminationGracePeriodSeconds` (30 s by default),
  so that the binary closes its remaining streams itself before the SIGKILL ([Graceful shutdown](shutdown.md)).
- In hybrid mode, run the Python application as a sidecar container reachable from the binary only, and set
  `PY2AXUM_PYTHON_URL` to it (for instance `http://127.0.0.1:8000` in the same pod). See
  [Security § Hybrid deployments](security.md#hybrid-deployments-the-python-side-relay) for the sidecar's
  `--forwarded-allow-ips`.
- Request bodies are not capped by default: cap them at the ingress or with `PY2AXUM_MAX_BODY`
  ([Security § Request input](security.md#request-input)).

A smaller, fully native example with its own Dockerfile is in [`examples/notes`](https://github.com/larrymotalavigne/py2axum/tree/main/examples/notes).

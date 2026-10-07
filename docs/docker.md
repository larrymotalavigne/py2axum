# Building with Docker

A multi-stage build keeps Python and Rust toolchains out of the final image: transpile, compile, then ship
the binary alone (≈ 20–40 MB image on `debian:bookworm-slim`).

## Dockerfile

```dockerfile
# syntax=docker/dockerfile:1

# 1. transpile: the same Python version as the application targets (or newer)
FROM python:3.13-slim AS transpile
RUN pip install --no-cache-dir git+https://github.com/larrymotalavigne/py2axum
WORKDIR /src
COPY . .
# fails (with file:line) if a route does not translate and is not declared --python-side
RUN py2axum app --root . --backend dyn -o /crate --name api

# 2. compile (the cargo caches survive between builds with BuildKit)
FROM rust:1-bookworm AS build
WORKDIR /crate
COPY --from=transpile /crate .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/crate/target \
    cargo build --release && cp target/release/api /api

# 3. run
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /api /usr/local/bin/api
ENV HOST=0.0.0.0 PORT=8080
EXPOSE 8080
USER nobody
CMD ["api"]
```

Notes:

- `--root .` is the import root (what would be on `sys.path`); `app` is the package containing the FastAPI
  application. Copy your `uv.lock`: library behaviours follow its versions.
- The release profile uses fat LTO and one codegen unit: expect several minutes for the first `cargo build`;
  the cache mounts make rebuilds incremental.
- The generated crate ships with the `Cargo.lock` py2axum is tested with.
- Environment: `DATABASE_URL` (`postgresql://...`), `HOST`, `PORT`, `PY2AXUM_LOG_LEVEL`, plus your own
  settings. Run database migrations as a separate step: the binary does not create tables.

A complete example is in [`examples/notes`](../examples/notes) (`Dockerfile`, `docker-compose.yml`).

## Hybrid: binary + Python for the rest

Keep the routes py2axum cannot translate (or a `lifespan` with background jobs) in Python and let the
binary relay them:

```bash
py2axum app --root . --backend dyn --python-side lifespan --python-side '/admin/{path:path}' -o /crate
```

Or let py2axum choose: `--python-side lifespan --python-side auto` leaves every route that does not translate
to Python and prints each one with its reason ([details](how-it-works.md#4-hybrid-deployments)).

```yaml
services:
  api:                       # the binary: public entry point
    image: myapp-binary
    environment:
      DATABASE_URL: postgresql://app@db/app
      PY2AXUM_PYTHON_URL: http://python:8000
    ports: ["8080:8080"]
  python:                    # the same application, for the --python-side paths and the lifespan
    image: myapp-python
    command: uvicorn app.main:app --host 0.0.0.0 --port 8000
    environment:
      DATABASE_URL: postgresql+psycopg://app@db/app
```

The binary relays matching requests (method, headers, body) and streams the response back. WebSockets are
not relayed: route them to the Python service at your ingress.

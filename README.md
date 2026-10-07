# py2axum

**Compile a FastAPI + SQLAlchemy + Pydantic application ahead of time into a Rust ([axum](https://github.com/tokio-rs/axum)) binary — and prove, response by response, that it behaves like the Python app.**

You keep writing, testing and debugging your API in idiomatic Python. `py2axum` reads the source (it never imports or runs it), compiles every route and the project code it reaches into Rust over a small runtime that reproduces CPython, Pydantic v2, SQLAlchemy 2.0 (async) and Starlette semantics, and produces a self-contained binary for production.

```
app/ (FastAPI)  ──py2axum──▶  Rust crate (axum + tokio + sqlx)  ──cargo──▶  one static-ish binary
```

On the bundled example ([`examples/notes`](examples/notes)), same machine (Apple M1 Pro), same PostgreSQL, 4 uvicorn workers vs 4 tokio threads, 64 connections:

| Endpoint | FastAPI + uvicorn | py2axum binary | |
|---|---:|---:|---:|
| `GET /notes/1` (1 SELECT + relationship) | 4 276 req/s, p99 40 ms | **17 674 req/s, p99 4.1 ms** | ×4.1 |
| `GET /notes?limit=20` | 3 149 req/s, p99 47 ms | **11 822 req/s, p99 6.9 ms** | ×3.8 |
| `GET /health` | 76 712 req/s | **130 105 req/s** | ×1.7 |

The binary used 19 MB of RSS under load. Gains depend on how much of your request time is Python (validation, serialization, ORM) versus the database: measure your own app with `bench/`.

> **Status: alpha.** py2axum translates a *closed, growing subset* of Python and of a list of libraries. Anything outside it is **refused at compile time with `file:line`** — never approximated silently. A per-route coverage report tells you what blocks and what each fix would unlock.

## Why

- **Equivalence you can check.** Every supported behaviour is verified by differential testing: the same requests are sent to FastAPI and to the binary, and status, headers, content type and bodies must match **byte for byte** (JSON key order, Pydantic 422 error details and messages included). The project's own suites run 350+ such requests.
- **No rewrite, no fork.** The Python app stays the source of truth: unit tests, debugging, local development and the OpenAPI docs keep working as before.
- **Incremental.** Routes or ASGI apps that cannot be translated can stay in Python (`--python-side`): the binary relays them to a Python process, so one deployment serves both.

## Quick start

```bash
pip install git+https://github.com/larrymotalavigne/py2axum     # Python ≥ 3.12, no dependencies (PyPI soon)

# what would translate, route by route (nothing is generated)
py2axum examples/notes/app --root examples/notes --report coverage.md

# generate the Rust crate, build it, run it
py2axum examples/notes/app --root examples/notes --backend dyn -o build/notes --name notes
cargo build --release --manifest-path build/notes/Cargo.toml
DATABASE_URL=postgresql://postgres@localhost/notes PORT=8080 ./build/notes/target/release/notes
```

The binary reads `DATABASE_URL`, `HOST` (`0.0.0.0`), `PORT` (`8080`), plus whatever your code reads from the environment (`os.environ`, pydantic-settings). It does not create tables: run your migrations as usual.

Run the transpiler on a Python **at least as recent** as the one your app targets (the parser only knows its own syntax). Library behaviours that changed between versions (Starlette's CORS, Pydantic error URLs, CPython messages) follow the versions pinned in your project's `uv.lock`.

## In a Dockerfile

```dockerfile
FROM python:3.13-slim AS transpile
RUN pip install --no-cache-dir git+https://github.com/larrymotalavigne/py2axum
WORKDIR /src
COPY . .
RUN py2axum app --root . --backend dyn -o /crate --name api

FROM rust:1-bookworm AS build
WORKDIR /crate
COPY --from=transpile /crate .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /crate/target/release/api /usr/local/bin/api
EXPOSE 8080
CMD ["api"]
```

See [docs/docker.md](docs/docker.md) for caching the cargo build, the hybrid setup (binary + Python sidecar for `--python-side` routes) and a docker-compose file.

## Verify your own app

```bash
# reference and candidate on the same database, reset between runs
python tests/conformance.py http://127.0.0.1:8000 http://127.0.0.1:8080 --scenario myapp
```

A scenario is a list of requests plus a `reset()` function ([docs/conformance.md](docs/conformance.md), [`tests/scenarios/notes.py`](tests/scenarios/notes.py)). The harness prints every difference with both responses.

## What is supported

A summary — the full list, with every documented difference from CPython, is in [docs/supported.md](docs/supported.md).

- **FastAPI / Starlette**: routes and routers (prefixes, factories, conditional registration), path/query/header/body/form/file parameters with FastAPI's exact 422 errors, dependencies (`Depends`, `yield`, per-request cache, `Annotated` aliases, OAuth2/HTTPBearer), `response_model` filtering, returned `Response` objects, `StreamingResponse`/SSE, `BackgroundTasks`, exception handlers, the middleware stack (`CORSMiddleware`, `GZipMiddleware`, `BaseHTTPMiddleware`, `@app.middleware("http")`), Starlette routing (405, trailing-slash redirects).
- **Pydantic v2**: lax-mode validation with pydantic-core's error types and messages, unions, enums, `Field` constraints and aliases, validators (`field_validator`/`model_validator`, before/after), computed defaults, `model_dump`/`model_validate`/`model_copy`, `TypeAdapter`, frozen models, URL and email types, `BaseSettings`.
- **SQLAlchemy 2.0 async**: declarative models (composite keys, `Identity`, JSON/JSONB, enums, `Numeric`, `TypeDecorator`), sessions (identity map, autoflush, `expire_on_commit`, savepoints, refresh, rollback), relationships and loader strategies, `select`/`update`/`delete`/`insert ... on conflict`, joins, aliases, subqueries, aggregates, `case`, `text()`.
- **Python**: functions, closures, decorators, classes (with `__eq__`/`__str__`/`__aenter__`...), dataclasses, exceptions, comprehensions, generators, `match`, coroutines and `asyncio` (`gather`, tasks, locks), `threading`, `pickle` (CPython-compatible bytes), `datetime`/`decimal`/`uuid`/`re`/`json`/`csv`/`base64`/`hashlib`/`hmac`/`math`/`urllib.parse`...
- **Libraries**: httpx and aiohttp clients, redis.asyncio, python-jose (HMAC), bcrypt, pyotp, itsdangerous, cryptography's Fernet, jinja2 + `email` + aiosmtplib, pywebpush, google-auth id tokens, alembic config, psutil.

Not translated (stay in Python with `--python-side`, or refused): WebSockets, `lifespan`, other libraries, arbitrary `eval`/reflection, C extensions.

## How it works

1. **Frontend** — indexes the package's modules and imports statically (`ast`), finds the FastAPI app(s), routers, routes, models and schemas.
2. **Backends** — a *typed* backend emits idiomatic statically-typed Rust for simple CRUD handlers; the *dyn* backend (the general one) compiles every reachable project function to Rust over a dynamic value type with CPython semantics. Library calls go through a closed map (`py2axum/libmap.py`): anything unlisted is refused.
3. **Runtime** — `py2axum/runtime/dynrt/` (Rust, copied into each generated crate): values and operators, Pydantic validation and serialization, the SQLAlchemy session and SQL compiler (on sqlx/PostgreSQL), Starlette's middleware and routing, and the supported libraries.

Details: [docs/how-it-works.md](docs/how-it-works.md).

## Project layout

```
py2axum/            the transpiler (Python, stdlib only) and the Rust runtime it embeds
examples/notes/     a small API + Dockerfile, translated in full
fixtures/           reference apps exercising the supported subset (used by the test suites)
tests/              conformance harness and scenarios, rejection tests
bench/              throughput / latency / memory benchmarks
docs/               documentation
```

## Contributing

Contributions are welcome — especially failing constructs from real applications. See [CONTRIBUTING.md](CONTRIBUTING.md). The rule that keeps the project honest: every new behaviour comes with a conformance case comparing it to the Python reference, and every unsupported case is refused with a test.

## License

Apache License 2.0 — see [LICENSE](LICENSE).

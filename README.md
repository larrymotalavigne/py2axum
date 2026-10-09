# py2axum

**Compile a FastAPI + SQLAlchemy + Pydantic application ahead of time into a Rust ([axum](https://github.com/tokio-rs/axum)) binary — and prove, response by response, that it behaves like the Python app.**

py2axum reads your FastAPI package (it never imports or runs it) and compiles every route, and the project code
those routes reach, into a Rust crate. The crate runs on a small runtime that reproduces CPython, Pydantic v2,
SQLAlchemy 2.0 and Starlette semantics, so that status codes, headers, JSON bodies (key order included) and
422 validation errors come out byte for byte the same. What it cannot reproduce exactly, it refuses at compile
time with `file:line`, or leaves to a Python process that the binary relays to.

```
your FastAPI app ──py2axum──▶ Rust crate (axum + tokio + sqlx) ──cargo──▶ one binary, no Python at run time
```

**Who it is for:** teams running a FastAPI + PostgreSQL API in production who want lower latency, higher
throughput per core or a smaller memory footprint, without rewriting the service or giving up Python for
development, tests and debugging. The Python application stays the source of truth; the binary is a build
artifact.

## What you get

Measured on the example application, [`examples/bookshelf`](examples/bookshelf) (users, JWT auth, books,
reviews, a WebSocket), its database seeded with 1 000 books and 3 000 reviews: FastAPI on uvicorn (4 workers)
against the binary (4 threads), same PostgreSQL, same machine (Apple M1 Pro, 10 cores), the two servers
alternated run by run. Latency seen by one client, median of 3 × 5 s:

| Endpoint | FastAPI + uvicorn | py2axum binary | |
|---|---:|---:|---:|
| `GET /books/{id}`: book, owner, reviews (2 queries) | 1.94 ms (p99 16.0) | **0.50 ms** (p99 1.6) | ×3.9 |
| `GET /books?limit=20` | 1.84 ms (p99 14.6) | **0.67 ms** (p99 3.2) | ×2.7 |
| `GET /books?tag=sf&limit=20` (JSONB `@>`) | 1.85 ms (p99 12.8) | **0.79 ms** (p99 5.4) | ×2.4 |
| `GET /books/stats` (`GROUP BY`, `avg`) | 2.02 ms (p99 14.0) | **0.65 ms** (p99 2.3) | ×3.1 |
| `GET /me` (JWT check + 1 query) | 1.02 ms (p99 10.3) | **0.23 ms** (p99 0.8) | ×4.4 |
| `POST /books` (validation + INSERT) | 2.72 ms (p99 15.5) | **0.54 ms** (p99 1.7) | ×5.1 |
| `POST /auth/register`, invalid (FastAPI's 422) | 0.44 ms (p99 1.5) | **0.08 ms** (p99 0.2) | ×5.6 |
| Memory (RSS at rest) | 470 MB (4 processes) | **12 MB** | |

With 64 concurrent clients, the binary served about 2 to 4 times as many requests on `GET /books/{id}` and the
lists, and 5 to 6 times as many invalid ones; on the endpoints dominated by PostgreSQL (the aggregate, the
INSERT) the ratio moved with the load of the machine, which other jobs shared during these runs. Reproduce them
with `python bench/bench.py --target bookshelf --interleave 3`, ideally on a dedicated machine.

Gains depend on how much of your request time is Python (validation, serialization, the ORM) rather than the
database. Measure your own application: the bench script and the [conformance harness](https://larrymotalavigne.github.io/py2axum/advanced/conformance/) are
in this repository.

## What it does not do

- **It is not a Python interpreter.** It compiles a closed, growing [subset](https://larrymotalavigne.github.io/py2axum/supported/) of Python and
  of a [list of libraries](https://larrymotalavigne.github.io/py2axum/reference/libraries/). `eval`, metaclasses, C extensions, an
  application wrapped in a project ASGI class and unlisted libraries are refused, never approximated.
- **It does not run your migrations** nor serve the OpenAPI `/docs` pages: run Alembic as usual, and keep the
  docs on your Python deployment.
- **PostgreSQL only** (through sqlx). FastAPI, Pydantic, SQLAlchemy and Python must be within the
  [tested version ranges](https://larrymotalavigne.github.io/py2axum/reference/versions/), read from your lock file.
- **It is alpha.** Each supported behaviour is verified by differential tests, but your application combines
  them its own way: run a [conformance check](https://larrymotalavigne.github.io/py2axum/advanced/conformance/) on it before production.

## Quick start

```bash
pip install py2axum                  # Python ≥ 3.12, no dependencies; you also need Rust (rustup) and PostgreSQL

py2axum check app --root .           # route by route: native, python-side or refused (with file:line)
py2axum app --root . --python-side auto -o build/api --name api
cargo build --release --manifest-path build/api/Cargo.toml
DATABASE_URL=postgresql://localhost/mydb PY2AXUM_PYTHON_URL=http://127.0.0.1:8000 ./build/api/target/release/api
```

`app` is the package holding the FastAPI application and `--root` the directory you run uvicorn from.
`--python-side auto` leaves the routes that do not translate to your Python application, which the binary
relays to at `PY2AXUM_PYTHON_URL`; without it, generation fails on the first refused route.

```
$ py2axum check examples/bookshelf/app --root examples/bookshelf
native       POST /auth/login               examples/bookshelf/app/routers/auth.py:26
native       GET /books                     examples/bookshelf/app/routers/books.py:39
refused      GET /books/export.zip          examples/bookshelf/app/routers/export.py:18
             └ examples/bookshelf/app/routers/export.py:26: library call `zipfile.ZipFile()` is not supported (not in the py2axum library map)
native       WEBSOCKET /ws/books/{book_id}  examples/bookshelf/app/routers/live.py:43
...
12/13 routes native (92.3 %), 0 python-side, 1 refused — generation would fail
hint: --python-side auto leaves the refused routes to a Python process next to the binary
```

Or with Docker, nothing to install: the [builder image](https://larrymotalavigne.github.io/py2axum/advanced/docker/)
(`linux/amd64`, `linux/arm64`) holds py2axum, Rust and the runtime's precompiled dependencies, and writes the
binary to `dist/`; [`examples/docker`](examples/docker) uses it in a multi-stage build that ships the binary
alone on a distroless image.

```bash
docker run --rm -v "$PWD:/app" ghcr.io/larrymotalavigne/py2axum build app --python-side auto
```

The [getting-started guide](https://larrymotalavigne.github.io/py2axum/getting-started/) walks through the example end to end: check, build, run,
hybrid mode, comparison with FastAPI, Docker, runtime configuration, graceful shutdown and Sentry.

## Why trust the binary

- **Differential testing.** The same requests go to FastAPI and to the binary; status, headers and bodies must
  match byte for byte. The repository's suites run about 1 300 such requests, the bookshelf example 75 of its
  own, and [`tests/conformance.py`](https://larrymotalavigne.github.io/py2axum/advanced/conformance/) runs yours, from a scenario you write, recorded
  traffic or requests generated from your OpenAPI schema.
- **Refuse rather than guess.** A construct outside the subset stops the translation with `file:line`; it is
  never translated approximately. Every known difference from CPython is [documented](https://larrymotalavigne.github.io/py2axum/supported/).
- **No rewrite, no fork.** Unit tests, local development, debugging and the OpenAPI docs keep running on
  Python. A route py2axum cannot handle stays in Python (`--python-side`) and the binary relays it, so one
  deployment serves both.

## What is supported

A summary; the full list, with every documented difference, is in [the documentation](https://larrymotalavigne.github.io/py2axum/supported/).

- **FastAPI / Starlette**: routes and routers (prefixes, factories, conditional registration), path, query,
  header, body, form and file parameters with FastAPI's exact 422 errors, dependencies (`Depends`,
  `yield`, per-request cache, `Annotated`, OAuth2/HTTPBearer/HTTPBasic), `response_model`, `Response`
  objects, `StreamingResponse`/SSE, `BackgroundTasks`, exception handlers, middlewares (`CORSMiddleware`,
  `GZipMiddleware`, `BaseHTTPMiddleware`, `@app.middleware("http")`), lifespan, WebSockets, graceful shutdown.
- **Pydantic v2**: lax-mode validation with pydantic-core's error types and messages, unions, enums, `Field`
  constraints and aliases, `field_validator`/`model_validator`, `computed_field`, `model_dump`/`model_validate`,
  `TypeAdapter`, `EmailStr` and URL types, pydantic-settings.
- **SQLAlchemy 2.0** (async and sync sessions, PostgreSQL): declarative models, JSON/JSONB, enums, `Numeric`,
  `TypeDecorator`, the identity map, autoflush, savepoints, relationships and loader strategies,
  `select`/`update`/`delete`/`insert ... on conflict`, joins, subqueries, aggregates, `create_all`.
- **Python**: functions, closures, decorators, classes, dataclasses, exceptions, comprehensions, generators,
  `match`, `asyncio`, and much of the standard library (`datetime`, `decimal`, `uuid`, `re`, `json`, `csv`,
  `hashlib`, `hmac`, `base64`, `urllib.parse`...).
- **Libraries**: httpx, aiohttp, redis.asyncio, aio-pika, PyJWT and python-jose (HMAC), bcrypt, pyotp,
  itsdangerous, cryptography's Fernet, jinja2, aiosmtplib, sentry-sdk, prometheus_client, tenacity, nh3,
  pywebpush, google-auth id tokens, python-dateutil, xmltodict.

## How it works

1. **Frontend**: indexes the package's modules and imports statically (`ast`), finds the FastAPI app,
   routers, routes, models and schemas.
2. **Compiler**: every reachable project function is compiled to Rust over a dynamic value type with CPython
   semantics; library calls go through a closed map, anything unlisted is refused.
3. **Runtime** (Rust, copied into each generated crate): values and operators, Pydantic validation and
   serialization, the SQLAlchemy session and SQL compiler on sqlx, Starlette's middleware and routing, the
   supported libraries.

Details, and how this differs from RustPython: [How it works](https://larrymotalavigne.github.io/py2axum/advanced/how-it-works/).

## Documentation

| | |
|---|---|
| [Getting started](https://larrymotalavigne.github.io/py2axum/getting-started/) | from `pip install` to a deployed binary, step by step |
| [Supported subset](https://larrymotalavigne.github.io/py2axum/supported/) | what translates, version ranges, every known difference |
| [Conformance](https://larrymotalavigne.github.io/py2axum/advanced/conformance/) | check that the binary answers like your application |
| [How it works](https://larrymotalavigne.github.io/py2axum/advanced/how-it-works/) | the compiler, the runtime, hybrid deployments |
| [Deployment](https://larrymotalavigne.github.io/py2axum/advanced/deployment/) | multi-stage builds, caching, the hybrid setup |
| [Changelog](CHANGELOG.md) | what changed in each release |

## Project layout

```
py2axum/            the transpiler (Python, standard library only) and the Rust runtime it embeds
examples/           bookshelf (the example of this page), notes (a smaller one), docker (reference build)
fixtures/           reference applications exercising the supported subset (used by the test suites)
tests/              conformance harness and scenarios, rejection tests
bench/              throughput, latency and memory benchmarks
docs/               documentation
```

## Contributing

Contributions are welcome, especially constructs from real applications that py2axum refuses. See
[CONTRIBUTING.md](CONTRIBUTING.md). The rule that keeps the project honest: every new behaviour comes with
conformance cases comparing it with the Python reference, and every unsupported case is refused with a test.

## License

Apache License 2.0, see [LICENSE](LICENSE).

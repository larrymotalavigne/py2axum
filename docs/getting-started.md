# Getting started

This guide takes you from `pip install py2axum` to a binary serving a FastAPI application, using the example in
[`examples/bookshelf`](../examples/bookshelf): a reading-list API with users, JWT authentication, books,
reviews and a WebSocket. Then it shows how to do the same with your own application, deploy it, and configure
the binary.

- [1. What you need](#1-what-you-need)
- [2. Install](#2-install)
- [3. Check what translates](#3-check-what-translates)
- [4. Generate and build](#4-generate-and-build)
- [5. Run the binary](#5-run-the-binary)
- [6. Hybrid mode: routes left to Python](#6-hybrid-mode-routes-left-to-python)
- [7. Compare the binary with the Python application](#7-compare-the-binary-with-the-python-application)
- [8. Your own application](#8-your-own-application)
- [9. Docker](#9-docker)
- [10. Runtime configuration](#10-runtime-configuration)
- [11. Graceful shutdown](#11-graceful-shutdown)
- [12. Sentry](#12-sentry)
- [13. When something is refused](#13-when-something-is-refused)

## 1. What you need

- **Python ≥ 3.12**, at least as recent as the one your application targets: the transpiler parses your code
  with its own `ast` module, which only knows its own syntax.
- **Rust** (stable, via [rustup](https://rustup.rs)) to compile the generated crate. The first release build
  takes a few minutes; later builds only recompile your code.
- **PostgreSQL**: the runtime's database layer targets PostgreSQL (through sqlx).
- An application built on **FastAPI, Pydantic v2 and SQLAlchemy 2.0** (async or sync sessions), within the
  [tested version ranges](supported.md#supported-versions).

## 2. Install

```bash
pip install py2axum
```

py2axum has no dependencies: it reads your code, it never imports it. Install it next to your application
(same virtual environment, same Python) or in a separate one: either works.

The examples are not part of the wheel. To follow this guide, clone the repository:

```bash
git clone https://github.com/larrymotalavigne/py2axum && cd py2axum
```

## 3. Check what translates

```bash
py2axum check examples/bookshelf/app --root examples/bookshelf
```

`app` is the package that holds the FastAPI application; `--root` is the directory that would be on
`sys.path` when you run it (`uvicorn app.main:app` is run from `examples/bookshelf`). `check` runs the whole
translation without writing anything and answers route by route:

```
py2axum check examples/bookshelf/app

native       POST /auth/login               examples/bookshelf/app/routers/auth.py:26
native       POST /auth/register            examples/bookshelf/app/routers/auth.py:14
native       GET /books                     examples/bookshelf/app/routers/books.py:39
native       POST /books                    examples/bookshelf/app/routers/books.py:59
refused      GET /books/export.zip          examples/bookshelf/app/routers/export.py:18
             └ examples/bookshelf/app/routers/export.py:26: library call `zipfile.ZipFile()` is not supported (not in the py2axum library map)
native       GET /books/stats               examples/bookshelf/app/routers/books.py:68
...
native       WEBSOCKET /ws/books/{book_id}  examples/bookshelf/app/routers/live.py:43

Blockers (routes touched, routes for which it is the only blocker):
     1    1  library zipfile  (examples/bookshelf/app/routers/export.py:26)

12/13 routes native (92.3 %), 0 python-side, 1 refused — generation would fail
hint: --python-side auto leaves the refused routes to a Python process next to the binary
```

- **native**: compiled into the binary.
- **refused**: something this route reaches is outside the [supported subset](supported.md), here the
  `zipfile` module used by the CSV export. py2axum never guesses: a construct it cannot reproduce exactly
  stops the translation, with the `file:line` that causes it.
- **python-side**: left to a Python process running next to the binary (section 6).

The blockers table ranks what to fix (or to leave to Python) by how many routes each one blocks.

## 4. Generate and build

The export route stays in Python; everything else is compiled:

```bash
py2axum examples/bookshelf/app --root examples/bookshelf --python-side auto \
  -o build/bookshelf --name bookshelf
cargo build --release --manifest-path build/bookshelf/Cargo.toml
```

```
python-side (auto): /books/export.zip (GET export_books) — examples/bookshelf/app/routers/export.py:26: library call `zipfile.ZipFile()` is not supported (not in the py2axum library map)
generated build/bookshelf: 28 functions, 3 models, 12 schemas
```

`build/bookshelf` is an ordinary Cargo project: `src/gen.rs` is your application compiled to Rust (each
handler starts with a `/// METHOD path (from file:line)` comment), `src/dynrt/` is py2axum's runtime, and the
`Cargo.lock` is the one py2axum is tested with. Do not edit it: change the Python and generate again. The
binary is `build/bookshelf/target/release/bookshelf`.

## 5. Run the binary

The binary does not create or migrate tables: run your migrations (Alembic, or here `schema.sql`) first.

```bash
createdb bookshelf
psql bookshelf -f examples/bookshelf/schema.sql
DATABASE_URL=postgresql://localhost/bookshelf PORT=8080 ./build/bookshelf/target/release/bookshelf
```

```bash
curl -s localhost:8080/auth/register -H 'content-type: application/json' \
  -d '{"email": "ada@example.org", "password": "correct horse battery", "display_name": "Ada"}'
curl -s localhost:8080/auth/register -H 'content-type: application/json' -d '{"email": "nope"}'   # FastAPI's 422
```

The application reads its own settings as it would in Python (`os.environ`, pydantic-settings): here
`BOOKSHELF_SECRET` signs the tokens. Without `PY2AXUM_PYTHON_URL`, the route left to Python answers 404.

## 6. Hybrid mode: routes left to Python

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

Details: [how-it-works.md § Hybrid deployments](how-it-works.md#4-hybrid-deployments).

## 7. Compare the binary with the Python application

Translating is not enough: py2axum's promise is that the binary answers like FastAPI, byte for byte. The
example ships its proof:

```bash
pip install -r examples/bookshelf/requirements.txt httpx websockets
DATABASE_URL=postgresql://localhost/bookshelf examples/bookshelf/compare.sh
```

It translates and builds the example, starts FastAPI on port 9050 and the binary on port 9090 (relaying the
export route to FastAPI) against the same database, and plays [`scenario.py`](../examples/bookshelf/scenario.py)
on both: registrations, logins, valid and invalid tokens, CRUD, 422s, reviews, the export, WebSocket sessions.

```
ok   201 POST /auth/register
ok   422 POST /auth/register
...
ok   101 WS /ws/books/1?token=eyJhbGciOiJIUzI1NiIs...
ok   403 WS /ws/books/1?token=garbage
...
75/75 identical responses
```

A `DIFF` line prints both responses. To write such a scenario for your own application, and to compare on
recorded production traffic or generated requests, see [conformance.md](conformance.md).

## 8. Your own application

```bash
cd my-project                                   # where you run `uvicorn api.main:app`
py2axum check api --root .
py2axum check api --root . --python-side auto --fail-under 80     # in CI: exit 1 below 80 % native
py2axum api --root . --python-side auto -o build/api --name api
```

- **Versions.** py2axum reads your `uv.lock` (else `requirements*.txt`, else `pyproject.toml`) to reproduce
  the behaviour of the versions you run: Starlette's CORS, Pydantic's error URLs, CPython's messages. A library
  you import whose locked version is outside the [tested ranges](supported.md#supported-versions) is refused
  (`--allow-untested-versions` overrides, at your own risk).
- **The database URL.** The binary accepts SQLAlchemy URLs (`postgresql+psycopg://`, `postgresql+asyncpg://`):
  the driver named there decides details that differ between drivers (asyncpg prints UTC instants with `Z`,
  psycopg with `+00:00`), so give the binary the URL your Python app uses.
- **Configuration.** The binary reads the environment, not `.env` files: export the variables (your
  orchestrator does), or `set -a; . ./.env; set +a` locally.
- **JSON output** of `check` (`--json`) gives the same verdicts to scripts; `--report coverage.md` writes a
  detailed Markdown/JSON report.

## 9. Docker

[`examples/docker`](../examples/docker) holds a reference multi-stage build and a Compose file for the
example in hybrid mode:

```bash
docker compose -f examples/docker/compose.yaml up --build     # from the repository root
curl -s localhost:8080/health
```

- `Dockerfile`: transpile (`python:3.13-slim`), compile (`rust:1-bookworm`, cargo caches as BuildKit cache
  mounts), ship the binary alone on `debian:bookworm-slim` as `nobody` (an image of a few tens of MB).
- `Dockerfile.python`: the same application for the python-side routes, run by uvicorn.
- `compose.yaml`: PostgreSQL, the Python sidecar (not published), the binary (port 8080, the only entry point).

More on caching and on adapting it to your project: [docker.md](docker.md).

## 10. Runtime configuration

The binary reads these environment variables; your application's own settings are read as in Python.

| Variable | Default | |
|---|---|---|
| `DATABASE_URL` | `postgresql://postgres@127.0.0.1/postgres` | SQLAlchemy-style URLs accepted (see section 8) |
| `DB_POOL_SIZE` | `32` | connections in the pool (the binary's own pool replaces the engine's) |
| `HOST`, `PORT` | `0.0.0.0`, `8080` | listening address |
| `PY2AXUM_PYTHON_URL` | unset | the Python process serving python-side paths (section 6); unset: they answer 404 |
| `PY2AXUM_LOG_LEVEL` | `INFO` | level of the `logging` records the application emits (printed to stderr like Python's default handler) |
| `PY2AXUM_SHUTDOWN_TIMEOUT` | `25` | seconds to wait for in-flight requests and streams after SIGTERM (section 11) |
| `PY2AXUM_SQL_DEBUG` | unset | set: a database error is logged with its SQL statement |
| `PY2AXUM_DB_TIMEZONE` | discovered | the session `TimeZone` psycopg would get (sqlx forces UTC at connect, the runtime restores the server's); set it to skip the discovery |
| `PY2AXUM_PYTHON_VERSION` | the transpiler's | the patch release of CPython whose messages to reproduce where they changed (e.g. `3.14.0`) |
| `SENTRY_DSN`, `SENTRY_ENVIRONMENT`, `SENTRY_RELEASE` | | read as the Python SDK reads them (section 12) |
| `PROMETHEUS_MULTIPROC_DIR` | | `prometheus_client`'s multiprocess mode, with the same files as CPython |
| `PY2AXUM_STREAM_CHUNK`, `PY2AXUM_STREAM_MIN_ROWS` | `65536`, `1000` | block size (bytes) and row threshold of [large list responses](supported.md#large-list-responses) streamed from the session |

On the Python side, `py2axum.record` (an ASGI wrapper that records anonymized traffic for replay) is configured
by `PY2AXUM_RECORD*` variables: see [conformance.md](conformance.md#2-replay-recorded-traffic).

## 11. Graceful shutdown

On SIGTERM or SIGINT the binary does what uvicorn does: it stops accepting connections, closes idle
keep-alive connections, lets in-flight requests finish (their response carries `connection: close`), closes
WebSockets with code 1012, runs the lifespan's shutdown code, closes the database pool and exits with the
signal's status (143 for SIGTERM). A second signal exits at once.

The one difference: the wait is bounded by `PY2AXUM_SHUTDOWN_TIMEOUT` (25 s by default), where uvicorn waits
without limit, so an endless stream (an SSE feed) no longer holds the process until the orchestrator's
SIGKILL. Keep it below your orchestrator's grace period (Kubernetes: `terminationGracePeriodSeconds`, 30 s by
default). Details: [supported.md § Shutdown](supported.md#shutdown-sigterm-sigint).

## 12. Sentry

If your application calls `sentry_sdk.init(...)`, the binary reports to the same Sentry project through the
Rust SDK, and reproduces what the Python SDK decides: same events (errors, log records, transactions), same
tags, user, request data, breadcrumbs, scrubbing, `before_send`. Nothing to change in your code; the DSN
comes from `init(dsn=...)` or `SENTRY_DSN` as in Python. Events sent by the binary carry a `py2axum.source`
tag (the Python `file.py:line` of the route or call site), so you can tell them from the events of the Python
process in a hybrid deployment, and find the source line behind compiled code. The supported options and the
differences are listed in [supported.md § Sentry](supported.md#sentry-sentry-sdk-2x).

## 13. When something is refused

Every refusal names the construct and its `file:line`. In order of preference:

1. **Leave it to Python**: `--python-side auto`, or `--python-side PATH` for chosen routes. Correct by
   construction, at the cost of a Python process for those paths.
2. **Rewrite the line** with something the [supported subset](supported.md) covers, when it is incidental
   (a library call that the standard library or a supported library does as well).
3. **Report it**: a minimal reproduction of a construct real applications use is the most useful
   contribution ([CONTRIBUTING.md](../CONTRIBUTING.md)).

A library version outside the tested ranges is refused for the whole application: use a version in range, or
`--allow-untested-versions` after checking the behaviour with a conformance run.

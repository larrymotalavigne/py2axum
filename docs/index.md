---
hide:
  - navigation
---

# py2axum

**Compile a FastAPI + SQLAlchemy + Pydantic application ahead of time into a Rust ([axum](https://github.com/tokio-rs/axum))
binary — and prove, response by response, that it behaves like the Python application.**

<p>
<a href="https://pypi.org/project/py2axum/"><img alt="PyPI" src="https://img.shields.io/pypi/v/py2axum"></a>
<a href="https://github.com/larrymotalavigne/py2axum/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/larrymotalavigne/py2axum/actions/workflows/ci.yml/badge.svg"></a>
<a href="https://github.com/larrymotalavigne/py2axum/blob/main/LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache%202.0-blue"></a>
</p>

This documentation describes py2axum <!-- py2axum:version -->.

py2axum reads your FastAPI package (it never imports or runs it) and compiles every route, and the project code
those routes reach, into a Rust crate. The crate runs on a small runtime that reproduces CPython, Pydantic v2,
SQLAlchemy 2.0 and Starlette semantics, so that status codes, headers, JSON bodies (key order included) and
422 validation errors come out byte for byte the same. What it cannot reproduce exactly, it refuses at compile
time with `file:line`, or leaves to a Python process that the binary relays to.

```text
your FastAPI app ──py2axum──▶ Rust crate (axum + tokio + sqlx) ──cargo──▶ one binary, no Python at run time
```

The Python application stays the source of truth: you keep writing, testing and debugging it in Python. The
binary is a build artifact, like a wheel or a Docker image.

## Why

Measured on the example application, [`examples/bookshelf`](https://github.com/larrymotalavigne/py2axum/tree/main/examples/bookshelf)
(users, JWT auth, books, reviews, a WebSocket), its database seeded with 1 000 books and 3 000 reviews: FastAPI
on uvicorn (4 workers) against the binary (4 threads), same PostgreSQL, same machine (Apple M1 Pro, 10 cores),
the two servers alternated run by run. Latency seen by one client, median of 3 × 5 s:

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

Gains depend on how much of your request time is Python (validation, serialization, the ORM) rather than the
database. Measure your own application: see [Performance](advanced/performance.md).

## Why trust the binary

<div class="grid cards" markdown>

-   **Differential testing**

    The same requests go to FastAPI and to the binary; status, headers and bodies must match byte for byte.
    [Conformance](advanced/conformance.md) runs yours: a scenario you write, recorded traffic, or requests
    generated from your OpenAPI schema.

-   **Refuse rather than guess**

    A construct outside the [supported subset](supported.md) stops the translation with `file:line`; it is
    never translated approximately. Every known difference from CPython is documented on the page of its
    topic.

-   **No rewrite, no fork**

    Unit tests, local development, debugging and the OpenAPI docs keep running on Python. A route py2axum
    cannot handle stays in Python ([hybrid mode](getting-started/hybrid.md)) and the binary relays it.

-   **Every example on this site is tested**

    The code of the [tutorial](tutorial/index.md) is one application, compiled and compared with FastAPI on
    every change, in both directions: what the pages call native is native, and identical.

</div>

## When not to use it

- **Your application is not FastAPI + Pydantic v2 + SQLAlchemy 2.0 on PostgreSQL**, within the
  [tested version ranges](reference/versions.md). The database layer targets PostgreSQL only.
- **Most of your request time is in the database or in other services.** A 50 ms query stays a 50 ms query.
- **Your application leans on what py2axum does not translate**: unlisted libraries, C extensions, `eval`,
  metaclasses, a project ASGI class wrapping the app. `py2axum check` tells you, route by route, in seconds:
  start [there](getting-started/check.md).
- **You need the OpenAPI `/docs` or migrations from the binary**: it serves neither. Keep the docs on your
  Python deployment and run Alembic as usual.
- **You cannot run a conformance check before production.** py2axum is alpha: each supported behaviour is
  verified by differential tests, but your application combines them its own way.

## Where to go next

- [Getting started](getting-started/index.md): install, translate a first application, `py2axum check`, the
  hybrid mode.
- [Tutorial - User Guide](tutorial/index.md): FastAPI's features one by one, what is native, what stays in
  Python, and the differences.
- [Advanced](advanced/index.md): the runtime (shutdown, streaming, Sentry, metrics), conformance, deployment,
  security.
- [Reference](reference/index.md): the command line, environment variables, Python semantics, libraries,
  versions.

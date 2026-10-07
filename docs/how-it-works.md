# How it works

```
your package (.py)                py2axum (Python, stdlib only)                     generated crate
──────────────────    ┌──────────────────────────────────────────────┐    ─────────────────────────────
 app/main.py          │ modules.py   static module index & imports   │     Cargo.toml (+ tested Cargo.lock)
 app/models.py   ───▶ │ frontend.py  apps, routers, routes, models,  │ ──▶ src/main.rs   tokio + axum server
 app/views/...        │              schemas, middleware             │     src/gen.rs    your code, compiled
 uv.lock (versions)   │ dyn.py       compiler of project code        │     src/dynrt/    the runtime (copied)
                      │ libmap.py    closed map of library APIs      │
                      │ report.py    per-route coverage report       │
                      └──────────────────────────────────────────────┘
```

## 1. Reading the project without running it

`modules.py` parses every module of the package with `ast` and indexes its definitions and imports
(relative, `from x import *`, re-exports, function-level imports). A name is resolved to a project symbol,
a module, or an external dotted name (`fastapi.APIRouter`), never by importing anything. `frontend.py`
finds the FastAPI app(s), routers and their prefixes, routes, SQLAlchemy models and Pydantic schemas.

Library versions matter (Starlette changed CORS, Pydantic error URLs carry its version, CPython changed
some messages): they are read from the project's `uv.lock` (falling back to installed packages), and the
target Python version from `requires-python`.

## 2. Compiling project code

The **dyn** backend (`dyn.py`) compiles, for each route, its whole closure: dependencies, schemas, models
and every project function it can reach (calls on objects of unknown class depend on all project methods of
that name). Each Python function becomes a Rust `async fn` over a dynamic value type `V` (None, bool, int,
float, str, bytes, list, tuple, dict, set, datetime types, Decimal, model and schema instances, enum
members, exceptions, functions, coroutines, library objects...). Control flow is translated directly
(`if`/`for`/`while`/`try`/`with`/`match`, comprehensions, generators); exceptions are Rust `Result`s
routed to the enclosing `try` with labelled blocks.

Library calls go through `libmap.py`, a closed map from dotted names (`datetime.datetime.now`,
`sqlalchemy.select`, `redis.asyncio.from_url`) to runtime functions. An unknown name is a compile error with
`file:line`. Some constructs are compiled specially: SQLAlchemy models and Pydantic schemas become static
descriptors, `TypeAdapter(list[X])` a static validator, decorators a startup-time value `d1(d2(f))`.

A function that cannot be translated does not stop the build unless a translated route can reach it; it is
replaced by a stub that raises at run time (`py2axum: ...`). `--report` uses the same machinery to tell, per
route, the first blocking construct and what fixing it would unlock.

## 3. The runtime

`py2axum/runtime/dynrt/` is Rust, copied into every generated crate:

| Module | Reproduces |
|---|---|
| `v.rs`, `ops.rs`, `methods.rs` | values, operators, builtins and methods with CPython semantics (`repr`, float formatting, hashing, comparisons) |
| `pyd.rs` | Pydantic v2 validation (pydantic-core's order, error types and messages), serialization, `TypeAdapter` |
| `orm.rs` | the SQLAlchemy session (identity map, unit of work, relationships, loaders) and a SQL compiler emitting SQLAlchemy's SQL, on sqlx/PostgreSQL |
| `web.rs`, `asgi.rs`, `resp.rs` | FastAPI request handling (parameters, dependencies, `response_model`) and Starlette's routing, middleware and responses |
| `aio.rs`, `thread.rs` | coroutine objects, `asyncio`, `threading` and background loops on tokio |
| `dt.rs`, `decimal.rs`, `pickle.rs`, `stdlib.rs`, `libs.rs` | standard library |
| `http.rs`, `rds.rs`, `jose.rs`, `auth.rs`, `mail.rs`, `webpush.rs`, ... | supported third-party libraries |

## 4. Hybrid deployments

Paths declared with `--python-side PATH` (a route pattern like `/items/{id}` or a mount prefix) and
`--python-side lifespan` are left to the Python application. With `PY2AXUM_PYTHON_URL` set, the binary
relays those paths to it (method, headers, body; streamed response), so one entry point serves both;
otherwise it answers 404 and your ingress routes them.

## 5. Why differential testing

The compiler aims at observable equivalence: status, headers, content type and body bytes. The only reliable
way to know is to run both implementations on the same inputs, which `tests/conformance.py` does (see
[conformance.md](conformance.md)). Every supported construct in this repository has such a test, and
every refused construct has a rejection test.

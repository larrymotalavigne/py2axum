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

`--python-side auto` finds those routes for you: a first pass compiles every route without stopping, then
every path whose route does not translate (its own code or a function it reaches) is left to Python, along
with raw `add_route` routes with a literal path and the app's last `app.mount()`s (`--python-side mount`: the
requests under the mount that no translated route fully matches). Each moved path is printed with the error that blocked it:

```
python-side (auto): /assistant/chat (POST chat) — app/tools.py:61: method .model_json_schema() is not
    implemented by the runtime for any type (it would raise AttributeError at run time)
```

Rules:

- A path moves as a whole: if `GET /items/{id}` translates but `DELETE /items/{id}` does not, both are served
  by Python. The binary relays by path, and Python serving a route is always correct.
- Errors about the whole application (an unsupported middleware, `FastAPI(lifespan=...)`, an app option) are
  not moved: generation stops on them, with `file:line`. Declare `--python-side lifespan` explicitly: a
  lifespan left to Python does not run in the binary, which is a decision only you can make.
- The list follows the code: a route that becomes translatable after a py2axum upgrade moves to the binary on
  the next build. Pin the list with explicit `--python-side PATH` flags (or check `--report` in CI) if you
  want deployments to change only when you decide.
- Moved paths go through the Python app's own middleware stack, not the translated one (the relay happens
  before it). State shared between the two processes must be external (database, Redis, broker).

## 5. Why differential testing

The compiler aims at observable equivalence: status, headers, content type and body bytes. The only reliable
way to know is to run both implementations on the same inputs, which `tests/conformance.py` does (see
[conformance.md](conformance.md)). Every supported construct in this repository has such a test, and
every refused construct has a rejection test.

## 6. How it differs from RustPython

[RustPython](https://github.com/RustPython/RustPython) and py2axum are both "Python in Rust", but they solve
different problems.

| | RustPython | py2axum |
|---|---|---|
| What it is | A Python 3 **interpreter** written in Rust: it replaces CPython and runs any program at run time | An ahead-of-time **compiler** for one stack (FastAPI, SQLAlchemy, Pydantic): it emits Rust, then no Python runs |
| Scope | The whole language and standard library (in progress) | A closed subset of Python and a closed list of libraries |
| Outside the scope | Fails at run time | Refused at compile time with `file:line`, or left to Python (`--python-side`) |
| C and PyO3 extensions | Not loadable (no CPython C API): pydantic-core, psycopg's binary build, uvloop... | Not needed: Pydantic validation, the ORM session and asyncio are reimplemented natively in the runtime |
| Speed | An interpreter, generally slower than CPython | Native code on axum/tokio/sqlx; measured against uvicorn and granian in `bench/` |
| Correctness check | CPython's test suite, partially passing | Differential testing against the Python app itself, byte for byte |
| Best at | Python in the browser (WebAssembly), scripting embedded in a Rust program | Serving an existing FastAPI API from a single binary |

In practice a FastAPI application does not run on RustPython today, since Pydantic v2 depends on pydantic-core,
a compiled extension; and if it did, it would not be faster than on CPython. py2axum gets its speed and its
guarantees from its narrow scope: because it only targets one framework stack, it can reproduce that stack's
observable behaviour exactly and compile everything else away.

The two projects share one concern: reproducing CPython's semantics in Rust (`repr`, float formatting,
hashing, integer and string methods). RustPython's implementation is a useful reference for those edge cases.

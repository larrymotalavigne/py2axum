# Corpus bench: the FastAPI, Pydantic and SQLAlchemy documentation

py2axum is measured on the examples of the official documentation, not only on the applications it was
built for. The result is [docs/coverage.md](../docs/coverage.md), page by page.

## Sources and licences

Nothing is vendored: the bench clones the sources at run time into `corpus/.cache/` (ignored by git).

- **FastAPI** — <https://github.com/fastapi/fastapi>, tag = the highest FastAPI version py2axum supports
  (`py2axum/versions.py`). Used: `docs_src/` (the documentation's example apps), `tests/test_tutorial/` (their
  official tests) and `docs/en/` (the pages that show them, for the per-page table). Copyright (c) 2018
  Sebastián Ramírez, **MIT License**.
- **Pydantic** — <https://github.com/pydantic/pydantic>, tag `v<highest supported Pydantic version>`. Used: the
  Python code blocks of `docs/**/*.md`. Copyright (c) 2017 to present Pydantic Services Inc. and individual
  contributors, **MIT License**.
- **SQLAlchemy** — <https://github.com/sqlalchemy/sqlalchemy>, tag `rel_<highest supported SQLAlchemy version>`
  (`rel_2_1_4`). Used: the doctests of `doc/build/` (the Unified Tutorial, the ORM Quick Start, the ORM Querying
  Guide, ORM, extension and Core pages) and the module docstring of `lib/sqlalchemy/ext/hybrid.py`. Copyright (c)
  2005-2026 the SQLAlchemy authors and contributors, **MIT License**.

The examples are copied (unchanged for FastAPI; reduced to their definitions plus a generated route for
Pydantic) into the bench's working directory only, never into this repository.

## What it does

`python corpus/run.py` (needs Rust, PostgreSQL and the test dependencies below):

1. **Discover.** Every `docs_src` module that defines `app` is an example (a package with a `main.py` is one
   example). FastAPI's test suite is run once with a stub client to learn which official test files build a
   client for which example. Each example gets the documentation page that includes it.
2. **Check.** `py2axum check --json` on a copy of each example. An example with a route or a construction py2axum
   refuses is **refused**, with the reasons at file:line (mapped back to `docs_src/...`).
3. **Generate and compile at scale.** The examples that translate are generated, then compiled in a few shared
   crates (`corpus/build.py`): one crate holds the runtime once and each example's generated code as a module,
   its `main` starts the example named by `CORPUS_APP`. One target directory for all of them, an
   incremental profile, and files rewritten only when they change, so a second run only recompiles what
   moved. A crate that fails to compile is rebuilt without the examples the errors point at.
4. **Replay.** For each official test file, the Python reference (uvicorn) and the binary of each of its
   examples are started, and the file runs under pytest with `corpus/replay.py`: `TestClient` is replaced by a
   client that sends every request to both servers in lockstep, compares the two responses with the logic of
   `tests/conformance.py` (status, content type and encoding, cookies, middleware headers, JSON body byte for
   byte with key order), and hands the binary's response to the test. Requests to `/openapi.json`, `/docs` and
   `/redoc` are sent but not compared: the binary does not serve the OpenAPI documentation. A JWT in a body is
   compared claim by claim, its `exp`/`iat`/`nbf` within 2 seconds: the two servers are called one after the
   other, and a token minted after hashing a password can fall on the next second.
5. **Pydantic.** Each runnable documentation block (or group of blocks) is run with `BaseModel` instrumented to
   record the inputs it validates and the `model_dump` / `model_dump_json` calls it makes. Its definitions become
   a FastAPI module with a `POST /v/<Model>` route per model (validate the body, return it) and a
   `POST /d/<Model>/<n>` route per recorded dump call; the recorded inputs that a JSON body can carry are its
   requests (`corpus/pydantic_docs.py`).
6. **SQLAlchemy** (`corpus/sqlalchemy_docs.py`, `corpus/sqlalchemy_probe.py`), described below.
7. **Classify** each example (`identical`, `differs`, `refused`, `error`, `untested`, `docs-only`, defined in
   `corpus/run.py`) into `<work>/results.json`, then `python corpus/coverage.py <work>/results.json -o
   docs/coverage.md` writes the table.

Duration on the development Mac (10 cores, shared with other jobs): about 14 minutes from scratch (7 crates of
up to 60 examples compile in about 7 minutes, one crate per example would take hours), about 4 minutes when
nothing in the translator changed (checks and generated crates are cached by the translator's hash, cargo
recompiles nothing), most of it replaying the tests.

Ports 10000–10999, database `py2axum_corpus` (`DATABASE_URL` overrides; only the examples that use a
database need it).

```bash
uv venv -p 3.13 .venv-corpus && . .venv-corpus/bin/activate
uv pip install $(python -m py2axum.versions max) "uvicorn[standard]" python-multipart jinja2 email-validator \
  pyjwt "pwdlib[argon2]" sqlmodel dirty-equals inline-snapshot pytest pytest-xdist \
  pytest-codspeed pytest-timeout anyio pyyaml websockets a2wsgi flask importlib_metadata \
  "opentelemetry-instrumentation-fastapi>=0.65b0" "opentelemetry-sdk>=1.44.0" "opentelemetry-exporter-otlp-proto-http>=1.44.0" \
  logfire "sentry-sdk>=2.70.0" "strawberry-graphql>=0.200.0,<1.0.0" typer "anyio[trio]" pytest-cov
python corpus/run.py                 # --only body (a subset), --jobs N, --bundle N, --skip-pydantic
python corpus/coverage.py corpus/out/results.json -o docs/coverage.md --baseline corpus/baseline.json
```

## The SQLAlchemy corpus

The SQLAlchemy documentation is not a set of applications but of doctests: the pages of a unit share one
namespace, one long-lived session and the rows the earlier examples wrote (`test/base/test_tutorials.py` lists the
units SQLAlchemy runs). The bench:

1. **Parses** each unit as that runner does: `{execsql}`-style markers removed, `.. doctest-include` setup pages,
   `.. doctest-disable` regions. A doctest example is a statement; a code block (examples with no prose between
   them) is a corpus example. Pages SQLAlchemy does not run as doctests (cascades, composites, association
   proxy, hybrid attributes, Core pages...) are run as written: the definitions of their plain code blocks are
   run too, `engine` and `session` are provided (their fragments assume them), with the usual imports, and the
   tables of the mappings defined so far are created before each statement. What still fails is not in the
   corpus.
2. **Probes** the unit in Python, on PostgreSQL (every `create_engine("sqlite://")` becomes a fresh PostgreSQL
   database): per statement, the SQL it emits, whether it changes a session's pending state, what it prints and
   displays, the types it binds, and after a setup page the committed rows (the seed of what follows).
3. **Slices.** A block is an example when it runs SQL, or prints a `SELECT` PostgreSQL accepts (many queries are
   only shown as rendered SQL; the bench executes them and returns the rows). Its route runs, in a fresh
   session, the earlier statements it depends on (those binding the names it reads, transitively, and every
   earlier statement that wrote to the database or to a session), then the block; what the block prints or
   displays is returned as a JSON list of strings, an exception the documentation shows is caught and reported
   by its class name. Imports, classes, functions and tables are module level. A setup page in the middle of a
   unit, or a class defined again, starts a new app. A connection hook for SQLite (`PRAGMA`) is left out; a
   table reflected from one an example creates (`autoload_with=`) takes its dependents out of the corpus.
4. **Two variants** per example: `sync` (a `Session` from a `sessionmaker` dependency, `def` route) and `async`
   (an `AsyncSession` with `expire_on_commit=False`, `async def` route, `await` on the session and connection
   methods that are coroutines, `async with` for `Session(engine)`, `engine.connect()`/`begin()`). A sync
   example that lazy-loads raises `MissingGreenlet` under asyncio in Python too: `reference-fails`, not counted.
5. **Checks, generates and builds** one app per (unit, segment, variant): routes `py2axum check` refuses are
   left out (and refused, with their reason at the documentation's file:line), a refusal at generation takes
   its route (or, in a definition, all of them) out; the apps are compiled in shared crates (`corpus_sqla*`, same
   target directory as the other corpora).
6. **Replays** each route in order, on a worker slot's database (`py2axum_sqla_s<slot>`): before every request
   the database is put back to the seed (tables truncated, seed rows copied back, sequences restored, tables an
   example created dropped); the request goes twice to the Python reference (unstable output, or an object
   repr with its address: `nondeterministic`) and once to the binary, compared with tests/conformance.py's logic.

`python corpus/sqlalchemy_docs.py [--only UNIT] [--jobs N]` runs it alone (`<work>/sqla/results.json`, about 4
minutes from scratch); `python corpus/coverage.py corpus/out/sqla/results.json --only-sqlalchemy -o
docs/coverage.md` rewrites only the SQLAlchemy section. Ports 10300-10399, databases `py2axum_sqla_prep` and
`py2axum_sqla_s<slot>` on the server of `DATABASE_URL`, which must allow `session_replication_role` (a
superuser: the seed is copied back with foreign keys unchecked).

The weekly CI job `corpus` runs the same and fails when the share of identical examples falls below
`corpus/baseline.json` (FastAPI, Pydantic or SQLAlchemy), or when an example identical in the baseline is not
any more.

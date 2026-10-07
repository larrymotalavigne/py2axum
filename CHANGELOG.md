# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] — 2026-10-07, first public release

- Ahead-of-time compilation of FastAPI + SQLAlchemy 2.0 (async, PostgreSQL) + Pydantic v2 applications to
  Rust (axum, tokio, sqlx): the **dyn** backend (general) and the **typed** backend (simple CRUD).
- Runtime reproducing CPython, Pydantic v2, SQLAlchemy and Starlette semantics, plus a closed list of
  standard and third-party libraries ([docs/supported.md](docs/supported.md)).
- `--report`: per-route coverage report with the first blocking construct (`file:line`).
- `--python-side`: paths and `lifespan` left to the Python application, relayed by the binary when
  `PY2AXUM_PYTHON_URL` is set.
- Differential conformance harness (`tests/conformance.py`) and reference applications.
- `pip install`-able package with a `py2axum` command; example application with a multi-stage Dockerfile.
- aio-pika 10: connect, channels, durable queues, publishing with confirms, `queue.get()`.
- `json.loads` accepts bytes (UTF-8/16/32 detection, like CPython).
- `importlib.import_module` of a literal project module (module objects), `callable()`.
- `collections.defaultdict` (builtin type factories), `string.Formatter().vformat/format`; `str.format`
  gains `!r`/`!s` and CPython's error messages.
- tenacity 9 (`@retry` and its strategies). `g`/`e`/`G`/`E`/`X`/`o`/`b` presentations and `#` in `format()`
  and `%`; `ConnectionError` and its subclasses; `logging` level constants and `Logger.log`.

# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

- Replacing `aiohttp.ClientSession._request` / `httpx.AsyncClient.request` with a wrapper (timing outgoing calls):
  the clients call it; other assignments to library attributes are refused.

- A project function given the app in the factory (`configure(app)`) runs while the middleware stack is built:
  `app.add_middleware` of project `BaseHTTPMiddleware` subclasses, `app.add_route` (it was skipped).
  Routing objects as values: `request.app`, `app.router.routes`, `route.matches(scope)`, `Match`,
  `request.scope`. `timeit.default_timer`.

- `--python-side auto`: every route that does not translate (and raw `add_route` routes with a literal path) is
  left to the Python application, each printed with the error that blocked it.

- Module-level `try` / `if` / `for` / `while` / `with` statements run at startup (they were skipped), their
  bindings becoming module variables; import fallbacks (`try: import x except ImportError: ...`).

- `sys.settrace` / `threading.settrace_all_threads`: the project's functions report call/return/exception
  events to the trace function (function call metrics); `traceback.extract_tb`. Module-level functions are
  single objects; nested functions may name themselves.
- prometheus_client: `start_http_server`, OpenMetrics output, exemplars, name escapings, restricted registries.
- httpx / aiohttp responses keep their `Content-Encoding` header (the body is decoded by the runtime, not reqwest).
- prometheus_client multiprocess mode: the per-process files (shared with Python workers) and
  `MultiProcessCollector`, `mark_process_dead`.
- prometheus_client 0.26: `Counter`, `Gauge`, `Summary`, `Histogram`, `Info`, `Enum`, labels, `time()`,
  `count_exceptions()`, `track_inprogress()`, `CollectorRegistry`, `REGISTRY`, `generate_latest` (text format byte
  for byte).

- Typed backend: request bodies above 2 MB are read like Starlette does (axum answered 413).

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

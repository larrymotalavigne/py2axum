# Environment variables

The binary reads its configuration from the environment; your application's own settings are read as in
Python (`os.environ`, pydantic-settings). It does not read `.env` files (pydantic-settings reads environment
variables only, see [Models](../tutorial/models.md)): export the variables, as an orchestrator does, or
`set -a; . ./.env; set +a` locally.

These variables and their defaults are a stable interface ([Versioning and deprecation](versioning.md)).

## The binary

| Variable | Default | Effect |
|---|---|---|
| `DATABASE_URL` | `postgresql://postgres@127.0.0.1/postgres` | also the database of `<binary> migrate`; SQLAlchemy-style URLs accepted (`postgresql+psycopg://`, `postgresql+asyncpg://`): the driver named there decides details that differ between drivers ([SQL databases](../tutorial/sql.md#what-is-native)) |
| `DB_POOL_SIZE` | `32` | connections in the pool (the binary's own pool replaces the engine's) |
| `HOST`, `PORT` | `0.0.0.0`, `8080` | listening address when the binary is run without command-line options (with options: uvicorn's, see [Logging](../advanced/logging.md#running-the-binary-like-uvicorn)) |
| `UVICORN_*`, `FORWARDED_ALLOW_IPS` | | uvicorn's options as variables (`UVICORN_PORT`, `UVICORN_LOG_LEVEL`, `UVICORN_ACCESS_LOG`...), as uvicorn reads them |
| `PY2AXUM_SOURCE_ROOT` | the project's directory at translation | the directory the records' `pathname` and the tracebacks' file paths start with ([Logging](../advanced/logging.md)) |
| `PY2AXUM_PYTHON_URL` | unset | the Python process serving python-side paths ([Hybrid mode](../getting-started/hybrid.md)); unset: they answer 404 |
| `PY2AXUM_LOG_LEVEL` | unset | uvicorn's `--log-level` (kept for older deployments); the application's own records follow its `logging` configuration, as in Python |
| `PY2AXUM_SHUTDOWN_TIMEOUT` | `25` | seconds to wait for in-flight requests and streams after SIGTERM ([Graceful shutdown](../advanced/shutdown.md)) |
| `PY2AXUM_SQL_DEBUG` | unset | set: a database error is logged with its SQL statement (not its bound values) |
| `PY2AXUM_MAX_BODY` | unset | largest request body in bytes, 413 past it ([Security § Request input](../advanced/security.md#request-input)); unset: no limit, like uvicorn |
| `PY2AXUM_STACK_SIZE` | `268435456` (256 MiB) | stack of the runtime threads, in bytes (reserved address space; [Security § Request input](../advanced/security.md#request-input)) |
| `PY2AXUM_DB_TIMEZONE` | discovered | the session `TimeZone` psycopg would get (sqlx forces UTC at connect, the runtime restores the server's); set it to skip the discovery |
| `PY2AXUM_PYTHON_VERSION` | the patch recorded at generation | the patch release of CPython whose messages to reproduce where they changed inside a minor (e.g. `3.13.5`; see [Python versions](../python-versions.md)); generation records the one of `--python` or `.python-version`, else the transpiler's when it runs the project's minor, else the newest |
| `PY2AXUM_STREAM_CHUNK` | `65536` | block size (bytes) of [large list responses](../advanced/streaming.md) streamed from the session |
| `PY2AXUM_STREAM_MIN_ROWS` | `1000` | a literal `LIMIT` at or below it keeps a list response buffered ([Large list responses](../advanced/streaming.md)) |
| `SENTRY_DSN`, `SENTRY_ENVIRONMENT`, `SENTRY_RELEASE` | | read as the Python SDK reads them ([Sentry](../advanced/sentry.md)) |
| `PROMETHEUS_MULTIPROC_DIR` | | `prometheus_client`'s multiprocess mode, with the same files as CPython ([Prometheus metrics](../advanced/metrics.md#multiprocess-mode)) |
| `PROMETHEUS_DISABLE_CREATED_SERIES` | | as in `prometheus_client` ([Prometheus metrics](../advanced/metrics.md)) |

## The recorder (Python side)

On the Python side, `py2axum.record` (an ASGI wrapper that records anonymized traffic for replay, run as
`uvicorn py2axum.record:app`) is configured by these variables: see
[Conformance and replay § Replay](../advanced/conformance.md#2-replay-recorded-traffic).

| Variable | Default | Effect |
|---|---|---|
| `PY2AXUM_RECORD` | unset | the JSONL file to write; unset, the application is served untouched |
| `PY2AXUM_RECORD_APP` | (required) | the application to wrap, as `module:attribute` (`api.main:app`) |
| `PY2AXUM_RECORD_KEY` | random per process | secret of the pseudonyms (HMAC): set it to the same value in every worker so that one e-mail gets one pseudonym |
| `PY2AXUM_RECORD_FIELDS` | none | extra JSON/form/query keys to mask, comma separated (`first_name,iban,phone`) |
| `PY2AXUM_RECORD_HEADERS` | none | extra request headers to keep (`x-tenant`); `x-*` and a few standard ones are kept |
| `PY2AXUM_RECORD_SKIP` | `^/(health\|metrics\|docs\|openapi.json)` | regex of paths not recorded |
| `PY2AXUM_RECORD_MAX_BODY` | 1 MiB | bodies above this size (bytes) are not recorded |
| `PY2AXUM_RECORD_RAW` | unset | `1` keeps multipart and binary bodies as they are (base64): not anonymized, tests only |

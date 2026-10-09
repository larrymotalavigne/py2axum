# Sentry

If your application calls `sentry_sdk.init(...)`, the binary reports to the same Sentry project through the
Rust SDK, and reproduces what the Python SDK decides: same events (errors, log records, transactions), same
tags, user, request data, breadcrumbs, scrubbing, `before_send`. Nothing to change in your code; the DSN
comes from `init(dsn=...)` or `SENTRY_DSN` as in Python. Events sent by the binary carry a `py2axum.source`
tag (the Python `file.py:line` of the route or call site), so you can tell them from the events of the Python
process in a hybrid deployment, and find the source line behind compiled code.

## `sentry-sdk` 2.x

The binary reports to Sentry through the Rust SDK (crate `sentry` 0.46: DSN, HTTP transport, rate limits,
envelopes). What the Python SDK decides is reproduced by the runtime, so a Sentry project receives the same
events from the binary as from FastAPI: same count, level, message or log entry, logger, tags, user,
contexts, extra, breadcrumbs, exception type/value/module/mechanism, request data, transaction name and
status, `_meta` annotations. [`tests/sentry_check.py`](https://github.com/larrymotalavigne/py2axum/blob/main/tests/sentry_check.py)
compares both against a fake Sentry server
([`fixtures/sentryapp`](https://github.com/larrymotalavigne/py2axum/tree/main/fixtures/sentryapp), with and
without `send_default_pii`).

- `sentry_sdk.init(...)`: `dsn` (else `SENTRY_DSN`; empty or missing = inactive SDK, every capture returns
  None, as in Python), `environment` (else `SENTRY_ENVIRONMENT`, else `production`), `release` (else
  `SENTRY_RELEASE` and the CI variables the SDK reads; `git rev-parse` is not run), `server_name` (else the
  host name), `dist`, `sample_rate`, `traces_sample_rate`, `traces_sampler` (called with
  `transaction_context`, `parent_sampled` and `asgi_scope`), `enable_tracing`, `send_default_pii`,
  `max_value_length`, `max_breadcrumbs`, `max_request_body_size`, `before_send`, `before_send_transaction`
  (the project's functions, called with the serialized event and a hint holding `exc_info`), `integrations`,
  `default_integrations`, `auto_enabling_integrations`, `shutdown_timeout`. Accepted without effect, as they
  describe Python stack frames: `attach_stacktrace`, `include_local_variables`, `include_source_context`,
  `in_app_include`, `in_app_exclude`, `project_root`, `debug`, `send_client_reports`.
- API: `capture_message(message, level=, tags=, extras=, contexts=, user=, fingerprint=)`,
  `capture_exception(error=None, ...)` (without an argument: the exception of the enclosing `except` block),
  `set_tag`, `set_tags`, `set_user`, `set_context`, `set_extra`, `set_level`, `add_breadcrumb(crumb=None,
  hint=None, **kwargs)`, `new_scope()` / `push_scope()` as context managers (the yielded scope's `set_*`,
  `remove_*`, `add_breadcrumb`, `clear*`, `capture_*`), `get_isolation_scope()`, `get_current_scope()`,
  `flush(timeout=)`, `last_event_id()`, `is_initialized()`.
- Scopes: the import (module globals, an app factory's `init_sentry()`, which runs at startup) has its own
  isolation scope; each request forks it (breadcrumbs cleared), like `SentryAsgiMiddleware`, so a tag set in
  one request never reaches another. Code FastAPI runs in a copy of the context (a `def` endpoint, the rest
  of the stack under a `BaseHTTPMiddleware`) shares the scope, as in Python; the dedupe state does not leak
  back.
- `FastApiIntegration` / `StarletteIntegration` (also enabled automatically, as in Python): unhandled
  exceptions are captured with `handled: false`, exceptions answered by a handler when their `status_code`
  is 5xx with `handled: true`; events carry the request (`method`, headers filtered by the default
  denylist unless `send_default_pii`, `query_string`, `url`, the JSON or form body within
  `max_request_body_size`, cookies and `REMOTE_ADDR` with PII). A transaction (`op: http.server`, named
  after the route path, or the URL when no route matched) is sent per sampled request, except HEAD and
  OPTIONS; an incoming `sentry-trace` header continues its trace. Only `transaction_style="url"` and the
  default `failed_request_status_codes` / `http_methods_to_capture` are supported.
- `LoggingIntegration(level=, event_level=)` (also a default integration): records at or above `level`
  become breadcrumbs and at or above `event_level` events (`logentry`, `logger`, `exc_info` as an
  exception with mechanism `logging`), from every logger. The binary does not know the project's logging
  configuration: the records that reach Sentry are those passing `PY2AXUM_LOG_LEVEL` (default `INFO`), so
  set it to the level of the project's root logger (Python's default is `WARNING`) for the same
  breadcrumbs.
- `SqlalchemyIntegration`: accepted; it adds no child span in the binary. `AsyncioIntegration`: only when
  `init()` runs at import time (Python then has no running loop to patch and leaves tasks alone); an
  exception never retrieved from a task is reported through the `asyncio` logger, as in Python.
- The default `EventScrubber`, the dedupe of the same exception object and the serializer's limits (depth 5,
  breadth 10 in databags, `max_value_length`, `_meta`) are applied.

## What stays in Python

- Refused at transpile time, with the reason: `transport` (a Python `Transport` class cannot run; any Sentry
  server since 20.6 accepts envelopes), `before_breadcrumb`, `event_scrubber`, `error_sampler`,
  `ignore_errors`, profiling, Sentry Logs, `trace_propagation_targets`/`propagate_traces`,
  `functions_to_trace`, proxy and CA options, `_experiments`, and any other option.
- `capture_*(scope=...)`, `push_scope(callback)` and the rest of the API (`start_transaction`, `start_span`,
  `continue_trace`...) are refused.

## Differences

Differences, by nature of the binary: `sdk` is `sentry.rust` (its `integrations` list starts with
`py2axum`); events carry no `modules`, `sys.argv` extra, `runtime` context or thread data; an exception
has a single stack frame, its route handler (none outside a request), and `attach_stacktrace` adds no
stack to messages; there are no child spans (database, middleware) in transactions, and no release-health
sessions or client reports (the SDK's own counters). Every event and
transaction carries the tag `py2axum.source`: the route handler's `file.py:line`, or the call site
outside a request. The `Task exception was never retrieved` message does not show the task's repr.
Events are sent from the Rust SDK's background thread; on SIGTERM the queue is flushed for
`shutdown_timeout` (2 s by default), like the Python SDK's atexit hook.

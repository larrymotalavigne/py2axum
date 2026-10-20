# Logging

The binary prints what your application prints under uvicorn: the server's own lines (startup, access log,
errors, shutdown) and your application's `logging` records, formatted by the handlers and formatters your
code configures, on the same streams. The log conformance (`tests/conformance.py --logs`) checks it line for
line against the Python server.

```text
INFO:     Started server process [1]
INFO:     Waiting for application startup.
INFO:     Application startup complete.
INFO:     Uvicorn running on http://0.0.0.0:8000 (Press CTRL+C to quit)
2026-10-09 22:31:04,326 - app.views - WARNING - quota reached for tenant 7
INFO:     10.0.0.4:0 - "GET /items?limit=2 HTTP/1.1" 200 OK
ERROR:    Exception in ASGI application
Traceback (most recent call last):
  File "/app/app/views.py", line 42, in export
    rows = build(items)
           ~~~~~^^^^^^^
  File "/app/app/export.py", line 18, in build
    return {r.id: r for r in rows}[0]
           ~~~~~~~~~~~~~~~~~~~~~~~^^^
KeyError: 0
```

## Running the binary like uvicorn

The binary takes uvicorn's command-line options, so a container command can stay as it was:

```bash
./server --host 0.0.0.0 --port 8000 --proxy-headers --forwarded-allow-ips='*'
```

| Option | Effect, as in uvicorn 0.54 |
|---|---|
| `--host`, `--port` | listening address (default `127.0.0.1:8000`; without any option, the binary keeps `HOST`/`PORT`, default `0.0.0.0:8080`) |
| `--log-level critical\|error\|warning\|info\|debug\|trace` | level of the `uvicorn.error`, `uvicorn.access` and `uvicorn.asgi` loggers |
| `--access-log` / `--no-access-log` | the access log (on by default: one line per response, on stdout) |
| `--use-colors` / `--no-use-colors` | colours of uvicorn's formatters (default: when stdout is a terminal) |
| `--log-config FILE.json` | a `dictConfig` file instead of uvicorn's `LOGGING_CONFIG` |
| `--proxy-headers` / `--no-proxy-headers`, `--forwarded-allow-ips` | client address and scheme from `X-Forwarded-For` / `X-Forwarded-Proto` when the peer is trusted (default `127.0.0.1,::1`, or `FORWARDED_ALLOW_IPS`) |
| `--timeout-graceful-shutdown SECONDS` | bound of the wait at shutdown (the binary's default is 25 s, see [Graceful shutdown](shutdown.md)) |
| `--workers`, `--factory`, the application (`api.main:app`) | accepted and ignored: the application is compiled in, one process serves |

The same options are read from `UVICORN_*` variables (`UVICORN_PORT`, `UVICORN_LOG_LEVEL`...), as click
reads them for uvicorn.

## What is native

- **The server's lines**: `Started server process [pid]`, the lifespan lines (`Waiting for application
  startup.`, `Application startup complete.`, and their failures with the traceback), `Uvicorn running on
  ...`, `Shutting down`, `Waiting for application shutdown.`, `Application shutdown complete.`, `Finished
  server process [pid]`; the access line `client - "METHOD path?query HTTP/1.1" status phrase` when the
  response starts; `Exception in ASGI application` with the traceback of an unhandled exception. They are
  records of the `uvicorn.error` and `uvicorn.access` loggers, configured as uvicorn configures them before
  importing your application: your code can reconfigure them (`logging.getLogger("uvicorn.access").addFilter(...)`,
  `setLevel`, `handlers = []`) with the same effect.
- **Loggers**: `logging.getLogger(name)`, the hierarchy (a dotted name's ancestors), `setLevel` (number or
  name), effective levels, `propagate`, `disabled`, `handlers` and `filters` as lists your code can assign or
  modify, `hasHandlers`, `getChild`, `isEnabledFor`, `logging.disable`, `addLevelName`, `getLevelName`; the
  root logger and the module functions (`logging.info(...)` configures the root with `basicConfig()` when it
  has no handler, as in Python); `lastResort` (WARNING and above, the message only, on stderr) when no
  handler is found.
- **Records**: every `LogRecord` attribute (`asctime`, `created`, `msecs`, `relativeCreated`, `name`,
  `levelname`, `levelno`, `pathname`, `filename`, `module`, `funcName`, `lineno` of the Python call site,
  `process`, `processName`, `thread`, `threadName`, `taskName`), `extra=` (a key overwriting an attribute is
  Python's `KeyError`), `exc_info=` (True, an exception, a tuple), `logger.exception`, `stack_info=True`,
  `%`-style arguments and a single mapping, messages that are not strings.
- **Handlers**: `StreamHandler` (`sys.stdout`, `sys.stderr`), `FileHandler`, `NullHandler`, their level,
  formatter, filters, name and terminator; a formatting error is reported as Python reports it (`--- Logging
  error ---`, then the program goes on).
- **Formatters**: `logging.Formatter(fmt, datefmt, style, validate, defaults=)` in the three styles (`%`, `{`,
  `$`), Python's validation errors, `asctime` from `time.localtime` (`default_time_format`,
  `default_msec_format`), exceptions and stacks appended as Python appends them.
- **Filters**: `logging.Filter(name)`, a project subclass of `logging.Filter` whose `filter(record)` reads or
  sets record attributes (a correlation id), a plain function; returning a record replaces it (3.12).
- **Configuration**: `logging.basicConfig(...)` (every argument), `logging.config.dictConfig(...)` (version
  1: formatters, filters, StreamHandler/FileHandler/NullHandler, loggers, root, `disable_existing_loggers`),
  `LoggerAdapter` (`extra`, `merge_extra`).
- **python-json-logger** 4.0: `pythonjsonlogger.json.JsonFormatter` and the old
  `pythonjsonlogger.jsonlogger.JsonFormatter`, with `rename_fields`, `static_fields`, `reserved_attrs`,
  `timestamp`, `defaults`, `prefix`, `json_indent`, `json_ensure_ascii`; extra attributes and those set by
  filters are added in the order Python adds them.
- **Tracebacks** (`logger.exception`, `exc_info`, unhandled exceptions): `Traceback (most recent call
  last):`, a `File "...", line N, in function` entry for each project frame the exception went through, with
  its source line(s) and carets exactly as CPython prints them (the rules of the Python version running py2axum:
  3.12 and 3.13+ differ), `raise ... from ...` and implicit chaining ("During handling of the above
  exception, another exception occurred:").
- **Sentry**: `LoggingIntegration` receives the records a logger lets through, as in Python
  ([Sentry](sentry.md)).

## loguru

loguru 0.7 (`from loguru import logger`) is native for what applications do with it:

- **The default handler** that `import loguru` adds on `sys.stderr` (unless `LOGURU_AUTOINIT` is false), with
  `LOGURU_LEVEL` and `LOGURU_COLORIZE`: its format (`2026-10-19 21:04:05.123 | INFO     | app.users:create:42 -
  message`), the module (`__name__`), function and line of the call site, the local time to the millisecond.
- **Colours** as loguru decides them: when the stream is a terminal, or when `CI` and a CI's own variable
  (`GITLAB_CI`, `GITHUB_ACTIONS`, `TRAVIS`, `CIRCLECI`, `APPVEYOR`) are set, or `PYCHARM_HOSTED`; `colorize=`
  forces them on or off.
- **Logging**: `logger.trace/debug/info/success/warning/error/critical(message, *args, **kwargs)` and
  `logger.log(level, message, ...)` (a level name, or a number: `Level 25`), the message formatted by
  `str.format` only when there are arguments, a message that is not a string (`str()`), loguru's errors (an
  unknown level, a negative number, a missing argument of the format).
- **Sinks**: `logger.add(sys.stderr | sys.stdout, level=, format=, colorize=, backtrace=, diagnose=, catch=)`
  returning its id, `logger.remove()` and `logger.remove(id)`, at import (module level), in the app factory or in
  a route. A literal `format=` is compiled at translation with loguru's colour markups (`<green>`, `<level>`,
  `<fg #ff8800>`, `</>`, `\<` ...) and these fields: `time` (loguru's tokens `YYYY`, `MM`, `DD`, `HH`, `mm`,
  `ss`, `SSS`..., `Z`, `!UTC`, or strftime's `%Y %m %d %H %M %S %f %z %Z %j %a %A %b %B %p %I %y`), `level`,
  `level.name`, `level.no`, `level.icon`, `name`, `function`, `line`, `message`, `module`, `file`, `file.name`,
  `file.path`, `exception`, with a format spec and `!r`/`!s`.

Refused at translation, with the file and line: `logger.exception()` and `opt(exception=...)` (loguru prints the
exception with its `backtrace`/`diagnose` formatter, the values of the variables included by default),
`opt`, `bind`, `contextualize`, `patch`, `catch`, `configure`, `level`, `enable`/`disable`, the logger used as a
value (handed to a function, stored), file sinks (`rotation`, `retention`, `compression`), callables, coroutines
and `logging` handlers as sinks, `serialize=True`, `enqueue=True`, `filter=`, a format that is not a literal text
or that reads `extra`, `elapsed`, `process` or `thread`. A lifespan left to Python (`--python-side lifespan`) that
adds or removes a sink keeps the whole application in Python, since the binary's logger would not have it.
The binary stops at startup when another `LOGURU_*` variable is set (`LOGURU_FORMAT`, the levels' colours...),
or `LOGURU_ENQUEUE` / `LOGURU_SERIALIZE` is true.

## Differences

- A traceback holds the project's frames only: the frames of libraries (FastAPI, Starlette, uvicorn,
  SQLAlchemy, the standard library) do not exist in the binary. The log conformance drops them on both
  sides before comparing.
- A database error does not quote its statement: SQLAlchemy's `[SQL: ...]`, `[parameters: ...]` and
  `(Background on this error at: ...)` lines and psycopg's `LINE n:` excerpt are absent (the binary's SQL
  is its own rendering). The message, psycopg's `DETAIL`/`HINT` lines and the chained psycopg exception
  ("The above exception was the direct cause...") are as in Python.
- An exception raised once the response has started (while the body streams, or by a raw ASGI middleware
  after the response) has no frame of the raw ASGI middlewares around it in its traceback: in the binary it
  does not go back through them ([Middleware](../tutorial/middleware.md#differences)). An exception of a
  Server-Sent Events stream is printed as Python prints it (an `ExceptionGroup` of anyio's TaskGroup holding
  it). Without raw ASGI middlewares, the output is the same.
- `stack_info=True` prints the logging call's own frame under `Stack (most recent call last):`; Python prints
  every frame of the call stack (uvicorn's, the middlewares' included).
- The `warnings` module is not part of the subset: the warnings libraries print on stderr (pydantic's
  serializer warnings, PyJWT's `InsecureKeyLengthWarning`, a deprecation, a coroutine never awaited) are
  not printed by the binary. The log conformance drops them on both sides.
- The records some libraries log themselves are reproduced where the binary reimplements the library with
  its logging (aiormq's `error when creating transport`); python-multipart's parser warnings on a malformed
  body (`python_multipart.multipart`) are not.
- `pathname` is built from `PY2AXUM_SOURCE_ROOT` (default: the project's directory at translation), so set
  it to the directory the Python process imported the project from if your formats print paths.
- A hybrid application (`--python-side`): what runs in the Python process (the python-side routes, a
  python-side lifespan, the modules only they import) prints its records on the Python process's streams,
  not on the binary's.
- One process: `--workers N` prints what uvicorn prints with one worker. `thread` (an id), `taskName`
  (`Task-N`) and the process id differ between any two runs, Python's included.
- `threadName` is `AnyIO worker thread` for the whole request of a `def` endpoint (in Python, its async
  dependencies run on `MainThread`).
- Refused at translation, with the file and line: `stacklevel=` other than 1, a project subclass of
  `logging.Handler` or `logging.Formatter`, `logging.config.fileConfig`, handlers outside the list above
  (`RotatingFileHandler`, `QueueHandler`, `SysLogHandler`...). `--log-config` takes JSON only (no YAML,
  no INI).
- `tenacity.before_sleep_log` and the other records made by libraries the binary reimplements carry no Python
  call site (`pathname` is `(unknown file)`).

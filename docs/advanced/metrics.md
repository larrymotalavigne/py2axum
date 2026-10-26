# Prometheus metrics (`prometheus_client` 0.26)

An application that exposes Prometheus metrics with `prometheus_client` keeps doing so in the binary: the
metrics, their exposition format and the multiprocess files are reproduced, with CPython's messages.

## What is native

- Metric types: `Counter`, `Gauge`, `Summary`, `Histogram`, `Info`, `Enum` (namespace/subsystem/unit, buckets,
  multiprocess_mode, states, `registry=`).
- `.labels()` by position or keyword, `inc`/`dec`/`set`/`set_to_current_time`/`set_function`/`observe`/`info`/
  `state`/`reset`/`remove`/`remove_by_labels`/`clear`.
- `time()`, `count_exceptions()`, `track_inprogress()` as context managers and decorators (a plain function,
  like the library: on an `async def` it times the creation of the coroutine).
- Exemplar validation.
- Registries: `CollectorRegistry(target_info=)`, `register`/`unregister`/`get_sample_value`/`get_target_info`/
  `set_target_info`, `REGISTRY`, `restricted_registry(names)` (its collectors in registration order: CPython
  iterates a set).
- Exposition: `generate_latest(registry, escaping=)` (the text format byte for byte, the four name escapings),
  `openmetrics.exposition.generate_latest` (OpenMetrics 1.0 with units and exemplars), `CONTENT_TYPE_LATEST`.
- `start_http_server(port, addr=, registry=)`: the exporter on its own port (content negotiation, gzip,
  `name[]`, OPTIONS/405, `/favicon.ico`; its `Server`/`Date` headers differ; TLS options refused).
- `disable_created_metrics()`, `PROMETHEUS_DISABLE_CREATED_SERIES`.
- CPython's messages.

### Multiprocess mode

With `PROMETHEUS_MULTIPROC_DIR` set at startup, values are written to the library's per-process files (same
names, keys and binary layout, so Python workers can share the directory) and
`multiprocess.MultiProcessCollector(registry, path=)` merges every file of the directory like
prometheus_client (gauge modes `all`/`live*`/`min`/`max`/`sum`/`mostrecent`, histogram accumulation, `pid`
labels), `mark_process_dead(pid)`.

### prometheus_fastapi_instrumentator 8.x

`Instrumentator(...).instrument(app, ...).expose(app, ...)` (or an `instrumentator` global, with `expose` called
in a startup handler) is native:

- The middleware is placed in the stack where `instrument()` is called, as `app.add_middleware` places it, and
  its metrics are created when the stack is built: `metrics.default()`'s `http_requests_total`,
  `http_request_size_bytes`, `http_response_size_bytes`, `http_request_duration_highr_seconds` and
  `http_request_duration_seconds` (`metric_namespace`, `metric_subsystem`, buckets,
  `should_only_respect_2xx_for_highr`), and the in-progress gauge.
- The handler label is the template of the route matching the request (a full match, else a path-only one),
  the request path when it only matches with or without its trailing slash, else the path (or `none`,
  grouped). Options: `should_group_status_codes`, `should_ignore_untemplated`, `should_group_untemplated`,
  `excluded_handlers`, `should_round_latency_decimals`, `round_latency_decimals`, `should_respect_env_var`,
  `env_var_name`, `should_exclude_streaming_duration`, `should_instrument_requests_inprogress`,
  `inprogress_name`, `inprogress_labels`, `registry`.
- The values are recorded once the response body is complete (the end of a stream).
- `expose(app, endpoint=, should_gzip=, include_in_schema=, tags=)`: a GET route after the declared ones, its
  body `generate_latest` of the registry (a `MultiProcessCollector` registry with `PROMETHEUS_MULTIPROC_DIR`).

Refused: `should_include_root_path=True`, `.add(...)` (custom instrumentation functions) and 7.x releases (they
name partial and trailing-slash matches differently). In hybrid mode, a request relayed to the Python
process is not counted by the binary: the Python application counts it if it instruments itself. A
`Content-Length` request header that `int()` cannot read stops the recording for that request, as the library's
exception does, but nothing is logged.

## What stays in Python

- Refused: custom collectors, `make_asgi_app`/`make_wsgi_app`, the push gateway.

## Differences

- `REGISTRY` holds `GC_COLLECTOR`, `PLATFORM_COLLECTOR` and `PROCESS_COLLECTOR` (unregistering them works,
  their names stay reserved) but they produce no samples: the binary is not a CPython process, so
  `python_gc_*`, `python_info` and `process_*` are absent from its output.
- When several names collide, `DuplicateTimeseries` lists them in the collector's order (CPython prints a set,
  in hash order).

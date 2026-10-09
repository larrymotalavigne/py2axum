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

## What stays in Python

- Refused: custom collectors, `make_asgi_app`/`make_wsgi_app`, the push gateway.

## Differences

- `REGISTRY` holds `GC_COLLECTOR`, `PLATFORM_COLLECTOR` and `PROCESS_COLLECTOR` (unregistering them works,
  their names stay reserved) but they produce no samples: the binary is not a CPython process, so
  `python_gc_*`, `python_info` and `process_*` are absent from its output.
- When several names collide, `DuplicateTimeseries` lists them in the collector's order (CPython prints a set,
  in hash order).

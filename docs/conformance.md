# Checking that the binary behaves like your application

py2axum's contract is observable equivalence: for the same request on the same data, the binary must answer
what FastAPI answers. Its own test suites check that construct by construct, but your application combines
them in its own way, so check it too. This page shows three ways, from the simplest to the broadest:

1. [**A scenario**](#1-write-a-scenario): requests you write, played on both servers ([`tests/conformance.py`](../tests/conformance.py)).
2. [**Replay**](#2-replay-recorded-traffic): traffic recorded in front of your Python application, replayed on both.
3. [**Generation**](#3-generate-requests): requests derived from your OpenAPI schema, valid and invalid.

All three compare, response by response: the status; the content type and content encoding; the `Allow`,
`Location` and `WWW-Authenticate` headers; cookies; the headers middlewares add (CORS `access-control-*`,
`x-*`, `vary`, security headers, `retry-after`); download headers (`content-disposition`, `etag`...); and the
body: JSON is compared after parsing **with key order kept** (so `{"a":1,"b":2}` and `{"b":2,"a":1}` differ, as
they do for a client), any other body byte for byte. Instants generated at request time are masked by their
shape (digits become 9), so the format (`Z` or `+00:00`, the number of decimals) is still compared.

## Get the harness

The harness is not part of the `py2axum` wheel: it lives in the repository, next to the suites that use it.
Use the tag of your py2axum version:

```bash
git clone --depth 1 --branch v$(pip show py2axum | sed -n 's/^Version: //p') https://github.com/larrymotalavigne/py2axum
pip install httpx websockets "psycopg[binary]"     # websockets: WS steps; psycopg: SQL steps and reset()
```

Run it with a Python where your application's dependencies are installed if your scenario imports them (the
bookshelf scenario uses PyJWT to sign its tokens).

## 1. Write a scenario

A scenario is a Python file, which can live in your own repository. A complete one:
[`examples/bookshelf/scenario.py`](../examples/bookshelf/scenario.py).

```python
# my_project/conformance/scenario.py
import psycopg

SETTLE = 0.05      # optional: seconds to wait after a write (see "Pitfalls")

STEPS = [
    ("GET", "/health", None),
    ("POST", "/items", {"name": "a"}),                                  # a JSON body
    ("POST", "/items", {"name": ""}),                                   # FastAPI's 422
    ("POST", "/items", b'{"name": ', {"content-type": "application/json"}),   # raw bytes + headers
    ("GET", "/me", None, {"authorization": "Bearer eyJ..."}),           # headers
    ("SQL", "UPDATE items SET archived = true WHERE id = 1", None),     # a fixture the API cannot create
    ("WS", "/ws/feed?token=eyJ...", [("recv", 1), ("send", '{"type": "ping"}'), ("recv", 1), ("close",)]),
]


def reset(db: str) -> None:
    """Bring the database back to its initial state; called before each server's run."""
    with psycopg.connect(db.replace("postgresql+psycopg://", "postgresql://")) as conn:
        conn.execute("TRUNCATE items, users RESTART IDENTITY CASCADE")


def normalize(body):          # optional: mask what legitimately differs (random tokens...)
    if isinstance(body, dict) and "access_token" in body:
        body = {**body, "access_token": "<token>"}
    return body
```

What a scenario can define:

| Name | |
|---|---|
| `STEPS` | the requests, in order: `(METHOD, path, payload[, headers])`. `payload` is `None`, `bytes` (sent as is), or any JSON value (sent as JSON). |
| `("SQL", statement, None)` | a statement run on `DATABASE_URL` between two requests; nothing is compared. |
| `("WS", path, script[, headers])` | a WebSocket session; the script is a list of `("send", str or bytes)`, `("recv", n)`, `("close"[, code, reason])`, `("sleep", seconds)`. Compared: the handshake status (and the body of a refusal), the subprotocol, `x-` headers, the messages received, the close code and reason. A `subprotocols` entry in the headers offers subprotocols. |
| `reset(db)` | required: restores the initial state before each run. |
| `normalize(body)` or `normalize(body, path)` | applied to every parsed JSON body before comparison. |
| `normalize_text(text)` | the same for bodies that are not JSON. |
| `SETTLE` | seconds to wait after each request that is not a GET or HEAD. |
| `HEADER_MASKS` | names of compared headers whose value is random on both sides (`x-request-id`). |
| `COOKIE_MASKS`, `FILE_MASKS` | `(regex, replacement)` pairs applied to `Set-Cookie` values and download headers. |

## Run it

Start your application with uvicorn and the binary, both on the **same database** (each run starts with
`reset()`), then play the scenario on each:

```bash
export DATABASE_URL=postgresql://localhost/myapp_conformance      # a database you can empty
(cd my_project && DATABASE_URL=postgresql+psycopg://localhost/myapp_conformance uvicorn api.main:app --port 9050) &
PORT=9090 PY2AXUM_PYTHON_URL=http://127.0.0.1:9050 ./build/api/target/release/api &
python tests/conformance.py http://127.0.0.1:9050 http://127.0.0.1:9090 --scenario my_project/conformance/scenario.py
```

`PY2AXUM_PYTHON_URL` is only needed in hybrid mode (routes left to Python). The output lists every step,
`ok` or `DIFF` with both responses, and ends with `N/N identical responses`; the exit status is 1 if any
differs. [`examples/bookshelf/compare.sh`](../examples/bookshelf/compare.sh) does all of this in one script:
copy it as a starting point.

`--ignore-encoding` compares bodies regardless of their content encoding (for lists streamed from the session,
which have no `content-length` and are therefore compressed even under `GZipMiddleware`'s minimum size).

### In CI

```yaml
# .github/workflows/conformance.yml (a job with a PostgreSQL service, Rust and Python)
- run: pip install py2axum -r requirements.txt httpx websockets
- run: git clone --depth 1 --branch v$(pip show py2axum | sed -n 's/^Version: //p') https://github.com/larrymotalavigne/py2axum /tmp/py2axum
- run: py2axum api --root . --python-side auto -o build/api --name api
- run: cargo build --release --manifest-path build/api/Cargo.toml
- run: ./conformance/compare.sh       # starts both servers, then: python /tmp/py2axum/tests/conformance.py ...
```

### Pitfalls

- **Read the reference's statuses** the first time a scenario passes. A 500 on both sides is "identical": a
  bug of your application (or a scenario that never reaches the code it is meant to test) looks like success.
- **Make data deterministic.** Ids come from sequences: `TRUNCATE ... RESTART IDENTITY` in `reset()`. Tokens
  that embed the time of issue differ between the servers: mask them with `normalize`, and authenticate the
  following steps with tokens the scenario signs itself with fixed dates (as the bookshelf scenario does).
- **Empty tables, do not recreate them** while the servers run: the binary keeps prepared statements, and
  PostgreSQL refuses a cached plan whose result type changed.
- **`SETTLE`**: a FastAPI session dependency with `yield` commits *after* the response is sent, so the next
  request can read the database before the commit lands (the reference is racy; the binary is not).
- **Order without `ORDER BY`** is up to PostgreSQL, on both sides: give your queries an order, or normalize.
- **Keep-alive after a 500**: uvicorn closes the connection after an unhandled exception; the harness resends
  the next request once.
- Differences py2axum knows about and documents ([supported.md](supported.md)) are not bugs of your
  application or of the binary: mask them only after reading their entry.

## 2. Replay recorded traffic

`tests/difftest.py` drives the same comparison (same masks: `--scenario` borrows a scenario's `normalize`,
`COOKIE_MASKS`, `HEADER_MASKS`, `SETTLE`) with requests nobody wrote by hand. Each server gets its own
database, restored from the same snapshot (a database URL or a `pg_dump -Fc` file; target databases must have
`_replay` in their name, they are emptied and refilled with data only, so running servers keep their
prepared statements). Install the extra dependencies with `pip install "py2axum[difftest]"`.

```bash
python tests/difftest.py prepare --snapshot SNAP_URL --db REF_DB_URL --db CAND_DB_URL     # servers stopped
# start the reference on REF_DB, the binary on CAND_DB, then:
python tests/difftest.py replay requests.jsonl --ref http://127.0.0.1:9050 --cand http://127.0.0.1:9090 \
  --ref-db REF_DB_URL --cand-db CAND_DB_URL --snapshot SNAP_URL --scenario myapp/scenario.py --out reports/replay
```

Both commands of this section and the next write `divergences.jsonl` and `summary.md` (grouped by route and
first differing field; each divergence carries the request as a scenario step, ready to paste into `STEPS`).
Divergences [supported.md](supported.md) documents as known (integers beyond 64 bits, lone surrogates) are
counted apart.

The traffic is recorded on the Python side by `py2axum.record`, an ASGI wrapper that leaves the application
untouched:

```bash
PY2AXUM_RECORD=/data/requests.jsonl PY2AXUM_RECORD_APP=api.main:app PY2AXUM_RECORD_KEY=<secret> \
  uvicorn py2axum.record:app
```

The recording is anonymized as it is written: e-mail addresses, passwords, tokens, API keys, JWTs, the
`Authorization` header, every cookie value, forwarding IPs and the fields listed in `PY2AXUM_RECORD_FIELDS`
become deterministic pseudonyms (the same password at sign-up and at log-in stays the same, so the sequence still
logs in); multipart and binary bodies are not kept (`PY2AXUM_RECORD_RAW=1` keeps them, unanonymized: tests
only); response bodies are never written. Credentials the server hands out (Set-Cookie, a JSON field named like
a token) are recorded by pseudonym only: at replay, each server's own value takes their place in the requests
that follow. A static credential that exists in the snapshot (an API token) is given with `--secret VALUE
--record-key KEY`. Each server plays the whole sequence on its restored database (`--flush-redis URL` empties a
Redis database before each pass); `--minimize N` delta-debugs the prefix of the first divergence (N trials).

Other recorder settings: `PY2AXUM_RECORD_SKIP` (a regex of paths not to record, default
`^/(health|metrics|docs|openapi.json)`), `PY2AXUM_RECORD_HEADERS` (extra request headers to keep; `x-*` and a
few standard ones are kept), `PY2AXUM_RECORD_MAX_BODY` (bodies above it, 1 MiB by default, are not recorded). Record on a staging environment, or in
production only with the agreement of whoever is responsible for that data: even anonymized, a recording
describes how your users use the application.

## 3. Generate requests

`gen` reads the reference's OpenAPI and sends, for every operation, requests generated by
[schemathesis](https://schemathesis.readthedocs.io) under a fixed hypothesis seed (`--modes positive,negative`,
`--max-examples`), then a deterministic corpus of edge cases built around one valid request: missing, extra,
null and wrongly typed fields, numeric bounds, unicode (combining marks, RTL, astral characters), empty, invalid
and BOM-prefixed bodies, NaN and `1e309`, lone surrogates, a 6 MB body, odd path and query parameters. Each
request goes to the reference, then the candidate; a divergence is shrunk by hypothesis (`--shrink-time`) and
both databases are restored before the next request.

```bash
python tests/difftest.py gen --ref http://127.0.0.1:9050 --cand http://127.0.0.1:9090 \
  --ref-db REF_DB_URL --cand-db CAND_DB_URL --snapshot SNAP_URL --scenario myapp/scenario.py --seed 1 --out reports/gen
```

`--header` adds a header to every request (a token), `--setup N` replays the scenario's first N steps (a
log-in) after each restore, `--include`/`--exclude` filter on `METHOD /path`: exclude routes that call out to a
URL taken from the request.

## The repository's own suites

| Scenario | Application | Steps |
|---|---|---:|
| `dynapp` | `fixtures/dynapp`: the reference app (every supported construct) | ~1 000 |
| `syncapp` | `fixtures/syncapp`: synchronous SQLAlchemy sessions | 57 |
| `factoryapp` | `fixtures/factoryapp`: app factory, middleware stack, exception handlers | 37 |
| `examples/bookshelf/scenario.py` | [`examples/bookshelf`](../examples/bookshelf), in hybrid mode | 75 |
| `notes` | [`examples/notes`](../examples/notes) | 24 |
| `app` | `app/`: CRUD, GZip, aiohttp, large lists streamed from the session (also with forced streaming) | 80 |

`dynapp` needs PostgreSQL, Redis (database 13 is flushed), RabbitMQ (`BROKER_DSN`, default
`amqp://guest:guest@localhost:5672/`; its `py2axum_test_*` queues are deleted) and the web-push sink
(`tests/push_sink.py`), all started by `scripts_start_dyn.sh`.

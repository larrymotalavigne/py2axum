# Checking that the binary behaves like your application

`tests/conformance.py` sends the same requests to two servers — the FastAPI application (reference) and the
binary (candidate) — on a database reset before each run, and compares, for every request: status, content
type, encoding, `Allow`/`Location`/`WWW-Authenticate`, cookies, middleware headers (CORS, `Vary`...) and the
body (JSON compared after parsing with key order kept, other bodies byte for byte).

## Write a scenario

A scenario is a module in `tests/scenarios/`:

```python
# tests/scenarios/myapp.py
SETTLE = 0.2          # optional: pause after writes (FastAPI commits after the response)

STEPS = [
    ("GET", "/health", None),
    ("POST", "/items", {"name": "a"}),                       # JSON body
    ("POST", "/upload", b"raw", {"content-type": "text/plain"}),   # bytes body + headers
    ("GET", "/items?limit=0", None),                         # a 422
]

def reset(db: str) -> None:
    """Bring the database (DATABASE_URL) back to the initial state, e.g. TRUNCATE ... RESTART IDENTITY."""

def normalize(body):  # optional: mask values that legitimately differ (random tokens, timestamps)
    return body
```

`HEADER_MASKS` and `COOKIE_MASKS` mask header or cookie values that are random on both sides.

## Run it

```bash
export DATABASE_URL=postgresql://postgres@127.0.0.1/myapp_conformance   # a dedicated database
uvicorn app.main:app --port 8000 &                                      # reference
PORT=8080 ./target/release/api &                                        # candidate
python tests/conformance.py http://127.0.0.1:8000 http://127.0.0.1:8080 --scenario myapp
```

The output lists every step (`ok` / `DIFF` with both responses) and ends with `N/N identical responses`.
Options: `--ignore-encoding` (compare bodies regardless of gzip). Datetimes generated at request time are
masked automatically.

## The repository's own suites

| Scenario | Application | Requests |
|---|---|---:|
| `dynapp` | `fixtures/dynapp`: the dyn backend's reference app (every supported construct) | 556 |
| `factoryapp` | `fixtures/factoryapp`: app factory, middleware stack, exception handlers | 25 |
| `notes` | `examples/notes` | 24 |
| `app` | `app/`: the typed backend's reference app | 40 |

`dynapp` needs PostgreSQL, Redis (database 13 is flushed), RabbitMQ (`BROKER_DSN`, default
`amqp://guest:guest@localhost:5672/`; its `py2axum_test_*` queues are deleted) and the web-push sink (`tests/push_sink.py`,
started by `scripts_start_dyn.sh`).

## Beyond hand-written scenarios: replay and generation

`tests/difftest.py` drives the same comparison (same masks: `--scenario NAME` borrows a scenario's `normalize`,
`COOKIE_MASKS`, `HEADER_MASKS`, `SETTLE`) with requests nobody wrote by hand. Each server gets its own
database, restored from the same snapshot (a database URL or a `pg_dump -Fc` file; target databases must have
`_replay` in their name, they are emptied and refilled with data only, so running servers keep their
prepared statements). Install the extra dependencies with `pip install "py2axum[difftest]"`.

```bash
python tests/difftest.py prepare --snapshot SNAP_URL --db REF_DB_URL --db CAND_DB_URL     # servers stopped
# start the reference on REF_DB, the binary on CAND_DB, then:
python tests/difftest.py replay requests.jsonl --ref http://127.0.0.1:8000 --cand http://127.0.0.1:8080 \
  --ref-db REF_DB_URL --cand-db CAND_DB_URL --snapshot SNAP_URL --scenario myapp --out reports/replay
python tests/difftest.py gen --ref http://127.0.0.1:8000 --cand http://127.0.0.1:8080 \
  --ref-db REF_DB_URL --cand-db CAND_DB_URL --snapshot SNAP_URL --scenario myapp --seed 1 --out reports/gen
```

Both write `divergences.jsonl` and `summary.md` (grouped by route and first differing field; each divergence
carries the request as a scenario step, ready to paste into `STEPS`). Divergences this page documents as known
(integers beyond 64 bits, lone surrogates) are counted apart.

**Replay** plays traffic recorded on the Python side by `py2axum.record`, an ASGI wrapper that leaves the
application untouched:

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

**Generation** reads the reference's OpenAPI and sends, for every operation, requests generated by
[schemathesis](https://schemathesis.readthedocs.io) under a fixed hypothesis seed (`--modes positive,negative`,
`--max-examples`), then a deterministic corpus of edge cases built around one valid request: missing, extra,
null and wrongly typed fields, numeric bounds, unicode (combining marks, RTL, astral characters), empty, invalid
and BOM-prefixed bodies, NaN and `1e309`, lone surrogates, a 6 MB body, odd path and query parameters. Each
request goes to the reference, then the candidate; a divergence is shrunk by hypothesis (`--shrink-time`) and
both databases are restored before the next request. `--header` adds a header to every request, `--setup N`
replays the scenario's first N steps (a log-in) after each restore, `--include`/`--exclude` filter on
`METHOD /path` — exclude routes that call out to a URL taken from the request.

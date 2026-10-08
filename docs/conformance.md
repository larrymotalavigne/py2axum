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

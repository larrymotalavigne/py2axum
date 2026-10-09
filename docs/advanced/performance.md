# Performance

## Measured numbers

Measured on the example application,
[`examples/bookshelf`](https://github.com/larrymotalavigne/py2axum/tree/main/examples/bookshelf) (users, JWT
auth, books, reviews, a WebSocket), its database seeded with 1 000 books and 3 000 reviews: FastAPI on uvicorn
(4 workers) against the binary (4 threads), same PostgreSQL, same machine (Apple M1 Pro, 10 cores), the two
servers alternated run by run. Latency seen by one client, median of 3 × 5 s:

| Endpoint | FastAPI + uvicorn | py2axum binary | |
|---|---:|---:|---:|
| `GET /books/{id}`: book, owner, reviews (2 queries) | 1.94 ms (p99 16.0) | **0.50 ms** (p99 1.6) | ×3.9 |
| `GET /books?limit=20` | 1.84 ms (p99 14.6) | **0.67 ms** (p99 3.2) | ×2.7 |
| `GET /books?tag=sf&limit=20` (JSONB `@>`) | 1.85 ms (p99 12.8) | **0.79 ms** (p99 5.4) | ×2.4 |
| `GET /books/stats` (`GROUP BY`, `avg`) | 2.02 ms (p99 14.0) | **0.65 ms** (p99 2.3) | ×3.1 |
| `GET /me` (JWT check + 1 query) | 1.02 ms (p99 10.3) | **0.23 ms** (p99 0.8) | ×4.4 |
| `POST /books` (validation + INSERT) | 2.72 ms (p99 15.5) | **0.54 ms** (p99 1.7) | ×5.1 |
| `POST /auth/register`, invalid (FastAPI's 422) | 0.44 ms (p99 1.5) | **0.08 ms** (p99 0.2) | ×5.6 |
| Memory (RSS at rest) | 470 MB (4 processes) | **12 MB** | |

With 64 concurrent clients, the binary served about 2 to 4 times as many requests on `GET /books/{id}` and the
lists, and 5 to 6 times as many invalid ones; on the endpoints dominated by PostgreSQL (the aggregate, the
INSERT) the ratio moved with the load of the machine, which other jobs shared during these runs.

Gains depend on how much of your request time is Python (validation, serialization, the ORM) rather than the
database. Measure your own application.

## Reproducing them

```bash
python bench/bench.py --target bookshelf --interleave 3
```

Run it ideally on a dedicated machine. The script,
[`bench/bench.py`](https://github.com/larrymotalavigne/py2axum/blob/main/bench/bench.py), is in the repository,
next to the [conformance harness](conformance.md). `bench/bench.py --target dynapp --compare` is a performance
guard comparing a generated binary with a reference.

## Memory of large responses

Large lists returned straight from the session are streamed: a 57 MB response takes 13–16 MiB of RSS instead of
1.6 GiB buffered ([Large list responses](streaming.md)).

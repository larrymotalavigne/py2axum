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

## Memory allocator

The generated binary uses [mimalloc](https://github.com/microsoft/mimalloc) as its global allocator, not the C
library's `malloc`, which spent about 30 % of the instructions of an ORM list and contends between worker
threads. Measured on the `dynapp` test application (Linux, 16 cores, the same binary built with each allocator,
interleaved, median of 7 runs):

| | 4 worker threads | 16 worker threads |
|---|---:|---:|
| Throughput, 20-row ORM list | +21 % | +8 % |
| Throughput, one row by key | +10 % | +2 % |
| Throughput, validation only (`POST`) | +11 % | +11 % |
| Throughput, 404 | +22 % | 0 % |
| Server CPU per request | −9 to −17 % | −7 to −14 % |
| RSS at rest | 45 MiB instead of 30 | 66 MiB instead of 44 |
| RSS peak under load | 49 MiB instead of 31 | 77 MiB instead of 49 |

With 16 threads the database bounds the routes that query it, so the CPU saved shows less in the throughput.

mimalloc keeps per-thread heaps: the binary takes 16 to 28 MiB more memory. It is C code compiled into the binary
(statically: at run time the binary still needs only the C library, `distroless/cc` included); building needs a C
compiler, as zlib already did (the official Rust images and the py2axum builder image have one).

To keep the C library's allocator, build the generated crate without its default feature:

```bash
cargo build --release --no-default-features
```

## Build profile

The generated crate is a Cargo workspace of two crates: the application (`src/`) and py2axum's runtime
(`dynrt/`, a library crate, compiled once and reused by every build that shares a target directory). The release
profile links them with thin LTO and one codegen unit. Measured on the `dynapp` test application (Linux, 16
cores, the binaries of 0.8 and 0.9 measured in the same pod, interleaved, median of 7 runs), against the runtime
compiled inside the application's crate with fat LTO (the profile before 0.9):

| | 4 worker threads | 16 worker threads |
|---|---:|---:|
| Throughput (one row, 20-row list, `selectinload`, validation, 404) | −2.5 to +0.3 % | −3.0 to +7.9 % |
| Server CPU per request | +0.1 to +2.4 % | +0.5 to +3.4 % |
| RSS at rest | 44.4 MiB instead of 43.9 | 68.9 MiB instead of 68.2 |

The library crate alone costs nothing (with fat LTO: −2.4 to +1.3 % CPU per request); thin LTO costs up to 3 %
CPU per request, within the run-to-run noise of the throughput, in exchange for a lighter and faster build: the
compilation peak of a 10 MB generated module goes from 6.94 to 4.48 GiB. Without LTO the binary is 4 to 18 %
slower: keep LTO for a binary that serves traffic. To build with fat LTO (one whole-program optimisation, slower and
heavier to compile):

```bash
CARGO_PROFILE_RELEASE_LTO=fat cargo build --release
```

## Memory of large responses

Large lists returned straight from the session are streamed: a 57 MB response takes 13–16 MiB of RSS instead of
1.6 GiB buffered ([Large list responses](streaming.md)).

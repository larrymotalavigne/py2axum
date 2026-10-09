# Large list responses

An endpoint whose `response_model` is `list[Schema]` and that returns the session's rows directly —
`return (await session.execute(stmt)).scalars().all()`, `return (await session.scalars(stmt)).all()`, or
`result = await session.execute(stmt)` followed by `return result.scalars().all()` — is answered as a stream:
the rows are read from PostgreSQL with a cursor on a connection of the pool, each one validated by the
schema and serialized, and the JSON array is sent in 64 KiB blocks. Memory stays constant whatever the size
(a 57 MB response: 13–16 MiB of RSS instead of 1.6 GiB buffered) and the first byte leaves after the first
rows. The bytes are the ones FastAPI sends.

## When it applies

Streaming applies only when nothing observable changes: the session has written nothing in its open
transaction (no flushed or pending change, no `begin_nested()`), the statement selects one mapped class
without loader options (`selectinload`...) nor `with_for_update()`, the schema reads only columns of that
class (no relationship, property or `mode="before"` model validator), the return is not inside
`try`/`with`, the status has a body, and its literal `LIMIT`, if any, is above `PY2AXUM_STREAM_MIN_ROWS`
(default 1000, so paginated lists keep the usual path). Otherwise the statement runs as written.
`PY2AXUM_STREAM_CHUNK` sets the block size (bytes); `--no-stream` turns streaming off at generation.

## Differences

A streamed response has no `content-length` (chunked transfer) — a response that fits in the
first block keeps it — and is therefore compressed by `GZipMiddleware` even under `minimum_size`. The
statement runs when the response starts, on its own connection (READ COMMITTED: the same rows as in the
session, which wrote nothing). An error before the first block is a 500 like FastAPI's; after it (a row
failing response validation, a lost connection) the connection is closed mid-body, where FastAPI, which
serializes everything first, would answer 500.

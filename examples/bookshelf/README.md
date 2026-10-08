# Example: bookshelf

A small but realistic FastAPI + SQLAlchemy 2.0 (async) + Pydantic v2 application, written for this repository and
translated by py2axum: 12 of its 13 routes, the WebSocket included, are compiled into the binary; the last one
(a zipped CSV export, `zipfile` is not supported) is left to Python with `--python-side auto`, and the binary
relays it.

- users who register and log in: bcrypt password hashes, **JWT Bearer tokens** (PyJWT, HS256) read by an
  `HTTPBearer` dependency;
- **CRUD** on books, with filters, pagination, partial updates (`exclude_unset`) and ownership checks;
- **relationships**: a book's owner (joined), its reviews (`selectinload`, cascade delete), a unique constraint
  turned into a 409;
- **validation**: `Field` constraints, an enum, `EmailStr`, `field_validator`s that clean input, a
  `model_validator`, so every bad request gets FastAPI's exact 422 body;
- an aggregate endpoint (`GROUP BY`, `avg`), `GZipMiddleware`;
- a **WebSocket** per book, authenticated by a token in the query string, with JSON commands and close codes;
- a route that does **not** translate (`GET /books/export.zip`), to show the hybrid mode.

```
app/main.py            the FastAPI application
app/db.py              engine, session dependency (commit after the endpoint, rollback on error)
app/models.py          User, Book, Review
app/schemas.py         request/response models
app/security.py        passwords, tokens, the current-user dependency
app/routers/           auth.py, books.py, live.py (the WebSocket), export.py (left to Python)
schema.sql             the tables (the binary does not run migrations)
requirements.txt       pinned versions: py2axum follows them
scenario.py            the conformance scenario (75 requests and WebSocket sessions)
compare.sh             translate, build, run both servers, compare
```

## Run the comparison

```bash
createdb bookshelf
pip install "py2axum>=0.3.2" -r examples/bookshelf/requirements.txt httpx websockets
DATABASE_URL=postgresql://postgres@127.0.0.1/bookshelf examples/bookshelf/compare.sh
```

It prints `py2axum check`'s verdict per route, builds the binary, starts FastAPI on port 9050 and the binary on
port 9090 against the same database (the binary relays the export route to FastAPI), plays
[`scenario.py`](scenario.py) on each (the database is emptied before each run) and lists every response: `ok`,
or `DIFF` with both responses. It exits with 1 if any response differs.

## Run the binary alone

```bash
py2axum examples/bookshelf/app --root examples/bookshelf --python-side auto -o build/bookshelf --name bookshelf
cargo build --release --manifest-path build/bookshelf/Cargo.toml
psql bookshelf -f examples/bookshelf/schema.sql
DATABASE_URL=postgresql://localhost/bookshelf BOOKSHELF_SECRET=$(openssl rand -hex 32) ./build/bookshelf/target/release/bookshelf
```

```bash
curl -s localhost:8080/auth/register -H 'content-type: application/json' \
  -d '{"email": "ada@example.org", "password": "correct horse battery", "display_name": "Ada"}'
TOKEN=$(curl -s localhost:8080/auth/login -H 'content-type: application/json' \
  -d '{"email": "ada@example.org", "password": "correct horse battery"}' | python -c 'import json,sys; print(json.load(sys.stdin)["access_token"])')
curl -s localhost:8080/books -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"title": "Dune", "author": "Frank Herbert", "year": 1965, "tags": ["SF"]}'
curl -s 'localhost:8080/books?tag=sf'
```

Without `PY2AXUM_PYTHON_URL`, the export route answers 404; to serve it, run the same application with uvicorn
(`cd examples/bookshelf && uvicorn app.main:app --port 8000`) and start the binary with
`PY2AXUM_PYTHON_URL=http://127.0.0.1:8000`.
A Docker build of it is in [`examples/docker`](../docker). The [getting-started guide](../../docs/getting-started.md)
walks through all of this step by step.

# Example: a notes API

A small FastAPI + SQLAlchemy (async) + Pydantic application — authors, notes, validation, filters,
pagination, a relationship and an aggregate — translated in full by py2axum (8/8 routes) and checked against
FastAPI by `tests/scenarios/notes.py` (24 requests, identical responses).

```bash
# run it with Docker (from the repository root)
docker compose -f examples/notes/docker-compose.yml up --build
curl -s localhost:8080/notes

# or by hand
py2axum examples/notes/app --root examples/notes --backend dyn -o build/notes --name notes
cargo build --release --manifest-path build/notes/Cargo.toml
psql "$DB" -f examples/notes/schema.sql
DATABASE_URL=$DB ./build/notes/target/release/notes

# the same application in Python, for comparison
cd examples/notes && DATABASE_URL=postgresql+psycopg://... uvicorn app.main:app
```

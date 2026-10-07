"""Scenario of examples/notes: CRUD, validation errors, 404/409, filters, relationships, aggregates."""
from pathlib import Path

SETTLE = 0.2  # the reference commits after the response (FastAPI >= 0.121)

STEPS: list = [
    ("GET", "/health", None),
    ("POST", "/authors", {"name": "ada"}),
    ("POST", "/authors", {"name": "grace"}),
    ("POST", "/authors", {"name": "ada"}),
    ("POST", "/authors", {"name": ""}),
    ("POST", "/notes", {"title": "  first  ", "body": "hello", "author_id": 1}),
    ("POST", "/notes", {"title": "second", "pinned": True, "author_id": 2}),
    ("POST", "/notes", {"title": "   ", "author_id": 1}),
    ("POST", "/notes", {"title": "x", "author_id": 99}),
    ("POST", "/notes", {"author_id": "one"}),
    ("GET", "/notes", None),
    ("GET", "/notes?pinned=false", None),
    ("GET", "/notes?q=SEC", None),
    ("GET", "/notes?limit=0", None),
    ("GET", "/notes?limit=1&offset=1", None),
    ("GET", "/notes/1", None),
    ("GET", "/notes/42", None),
    ("GET", "/notes/abc", None),
    ("PATCH", "/notes/1", {"pinned": True, "body": "edited"}),
    ("PATCH", "/notes/1", {"title": ""}),
    ("GET", "/notes", None),
    ("DELETE", "/notes/2", None),
    ("DELETE", "/notes/2", None),
    ("GET", "/stats", None),
]


def reset(db: str) -> None:
    from sqlalchemy import create_engine, text

    engine = create_engine(db.replace("postgresql://", "postgresql+psycopg://", 1))
    schema = (Path(__file__).resolve().parents[2] / "examples" / "notes" / "schema.sql").read_text()
    with engine.begin() as conn:
        conn.exec_driver_sql(schema)
        conn.execute(text("TRUNCATE notes, authors RESTART IDENTITY CASCADE"))
    engine.dispose()

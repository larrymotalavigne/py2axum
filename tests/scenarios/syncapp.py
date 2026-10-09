"""Scenario of fixtures/syncapp: synchronous SQLAlchemy sessions (sessionmaker, generator dependency,
lazy loading, expiry on commit). Tables are created by SQLAlchemy from the fixture models, then emptied."""

STEPS: list = [
    ("GET", "/health", None),
    ("POST", "/authors", {"name": "Ada"}),
    ("POST", "/authors", {"name": "Brian"}),
    ("POST", "/authors", {}),
    ("POST", "/authors/1/books", {"title": "Notes", "pages": 12}),
    ("POST", "/authors/1/books", {"title": "Engines"}),
    ("POST", "/authors/2/books", {"title": "C"}),
    ("POST", "/authors/9/books", {"title": "Nope"}),
    ("POST", "/books/1/reviews/5", None),
    ("POST", "/books/1/reviews/3", None),
    ("GET", "/authors/1", None),
    ("GET", "/authors/2", None),
    ("GET", "/authors/9", None),
    ("GET", "/books/1", None),
    ("GET", "/books/3", None),
    ("GET", "/books/1/lazy", None),
    ("GET", "/books/3/lazy", None),
    ("PATCH", "/authors/1", {"name": "Ada L."}),
    ("POST", "/authors/2/deactivate", None),
    ("GET", "/authors/2", None),
    ("GET", "/q/authors", None),
    ("GET", "/q/first", None),
    ("GET", "/q/count", None),
    ("GET", "/q/scalar", None),
    ("GET", "/q/rows", None),
    ("GET", "/q/one/Brian", None),
    ("GET", "/q/one/Nobody", None),
    ("GET", "/q/more", None),
    ("POST", "/q/update", None),
    ("POST", "/q/delete", None),
    ("PUT", "/books/1/labels", {"labels": ["red", "blue"], "cover": "hé"}),
    ("PUT", "/books/2/labels", {"labels": ["blue", None]}),
    ("PUT", "/books/3/labels", {"labels": [], "cover": ""}),
    ("PUT", "/books/3/labels", {"labels": [1]}),
    ("GET", "/labels/blue", None),
    ("GET", "/labels/red", None),
    ("GET", "/labels/none", None),
    ("PUT", "/authors/2/mentor/1", None),
    ("PUT", "/authors/1/mentor/1", None),
    ("PUT", "/authors/9/mentor/1", None),
    ("GET", "/authors/1/mentor", None),
    ("GET", "/authors/2/mentor", None),
    ("PUT", "/authors/2/mentor/9", None),
    ("GET", "/authors/1/mentor", None),
    ("GET", "/authors/2/mentor", None),
    ("GET", "/books/1/titled", None),
    ("GET", "/books/3/titled", None),
    ("GET", "/booklist", None),
    ("GET", "/q/initials", None),
    ("GET", "/books/1/awaited", None),
    ("GET", "/when", None),
    ("GET", "/classattr", None),
    ("POST", "/authors", {"name": "Temp"}),
    ("PUT", "/authors/3/mentor/1", None),
    ("GET", "/authors/1/mentor", None),
    ("DELETE", "/authors/3", None),
    ("DELETE", "/authors/3", None),
    ("POST", "/core/many", None),
    ("GET", "/q/authors", None),
]


def reset(db: str) -> None:
    from sqlalchemy import create_engine

    from fixtures.syncapp.db import Base
    from fixtures.syncapp import models  # noqa: F401

    engine = create_engine(db.replace("postgresql://", "postgresql+psycopg://", 1))
    Base.metadata.create_all(engine)  # once; afterwards only emptied (servers cache their plans)
    with engine.begin() as conn:
        # added to an existing fixture table after its creation (create_all leaves existing tables alone)
        conn.exec_driver_sql("ALTER TABLE authors ADD COLUMN IF NOT EXISTS mentor_id INTEGER "
                             "REFERENCES authors(id) ON DELETE SET NULL")
        conn.exec_driver_sql(f"TRUNCATE {', '.join(t.name for t in Base.metadata.sorted_tables)} RESTART IDENTITY CASCADE")
    engine.dispose()

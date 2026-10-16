"""Scenario of fixtures/sqlmodelapp: SQLModel CRUD (FastAPI's sql_databases tutorials and around). Its own table,
emptied before each pass (ids restart at 1)."""

REFERENCE_APP = "fixtures.sqlmodelapp.main:app"

P = "/pilots"
STEPS: list = [
    ("GET", "/health", None),
    ("GET", f"{P}/", None),
    ("POST", f"{P}/", {"id": 9000, "name": "Dead Pond", "age": 30, "secret_name": "Dive Wilson"}),
    ("GET", f"{P}/1", None),
    ("POST", f"{P}/", {"name": "Spider-Boy", "age": 18, "secret_name": "Pedro Parqueador"}),
    ("POST", f"{P}/", {"name": "Rusty-Man", "secret_name": "Tommy Sharp", "rank": "major"}),
    ("POST", f"{P}/", {"name": "Bad", "age": -1, "secret_name": "x"}),
    ("POST", f"{P}/", {"name": "Bad", "secret_name": "x", "rank": "r" * 21}),
    ("POST", f"{P}/", {"age": "x"}),
    ("POST", f"{P}/", [1]),
    ("GET", f"{P}/", None),
    ("GET", f"{P}/?offset=1&limit=1", None),
    ("GET", f"{P}/?limit=101", None),
    ("GET", f"{P}/?offset=x", None),
    ("POST", f"{P}/table", {"name": "Table", "secret_name": "T", "age": 40, "id": 77}),
    ("POST", f"{P}/table", {"name": "NoId", "secret_name": "N"}),
    ("POST", f"{P}/table", {"name": 5, "secret_name": None}),
    ("POST", f"{P}/direct", {"name": "Direct"}),
    ("POST", f"{P}/validate", {"name": "V", "secret_name": "s", "age": "7"}),
    ("POST", f"{P}/validate", {"name": "V", "secret_name": "s", "age": -3}),
    ("POST", f"{P}/validate", {"name": "V"}),
    ("GET", f"{P}/full", None),
    ("GET", f"{P}/raw", None),
    ("GET", f"{P}/search?name=Spider-Boy&min_age=18", None),
    ("GET", f"{P}/search?name=nobody", None),
    ("PATCH", f"{P}/1", {"name": "Dog Pond", "age": None}),
    ("GET", f"{P}/1", None),
    ("PATCH", f"{P}/2", {"secret_name": "Peter"}),
    ("PATCH", f"{P}/2", {"age": -5}),
    ("PUT", f"{P}/2", {"name": "Spidey"}),
    ("PUT", f"{P}/3", {"name": "Rusty", "age": 50, "secret_name": "TS"}),
    ("GET", f"{P}/full", None),
    ("GET", f"{P}/", None),
    ("DELETE", f"{P}/1", None),
    ("GET", f"{P}/1", None),
    ("DELETE", f"{P}/1", None),
    ("PATCH", f"{P}/1", {"name": "Dog Pond"}),
    ("PUT", f"{P}/1", {"name": "Dog Pond"}),
    ("GET", f"{P}/x", None),
    ("GET", f"{P}/raw", None),
]


def reset(db: str) -> None:
    import psycopg

    with psycopg.connect(db, autocommit=True) as c:
        if c.execute("SELECT to_regclass('pilot')").fetchone()[0] is not None:
            c.execute("TRUNCATE pilot RESTART IDENTITY")

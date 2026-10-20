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

# a table model used by the routes of another module (fixtures/sqlmodelapp/songs.py)
STEPS += [
    ("GET", "/songs", None),
    ("POST", "/songs", {"name": "Midnight Fit", "artist": "Mogwai", "year": 2021}),
    ("POST", "/songs", {"name": "Nothing", "artist": "x", "year": "y"}),
    ("POST", "/songs/table", {"name": "Raw", "artist": "R", "id": 40}),
    ("POST", "/songs/table", {"name": 5}),
    ("POST", "/songs/validated", {"name": "V", "artist": "A", "year": "1999"}),
    ("POST", "/songs/validated", {"name": "V"}),
    ("GET", "/songs", None),
    ("GET", "/songs/1", None),
    ("GET", "/songs/999", None),
]

# SQLModel relationships (fixtures/sqlmodelapp/heroes.py)
H = "/heroes"
STEPS += [
    ("POST", f"{H}/teams", {"name": "Preventers", "headquarters": "Sharp Tower"}),
    ("POST", f"{H}/teams", {"name": "Z-Force", "headquarters": "Sister Margaret's Bar"}),
    ("POST", H, {"name": "Deadpond", "secret_name": "Dive Wilson", "team_id": 2}),
    ("POST", H, {"name": "Rusty-Man", "secret_name": "Tommy Sharp", "age": 48, "team_id": 1}),
    ("POST", H, {"name": "Spider-Boy", "secret_name": "Pedro Parqueador"}),
    ("GET", f"{H}/1", None), ("GET", f"{H}/3", None), ("GET", f"{H}/9", None),
    ("GET", f"{H}/teams/1", None), ("GET", f"{H}/teams/2", None), ("GET", f"{H}/teams/9", None),
    ("GET", H, None), ("GET", f"{H}?min_age=40", None), ("GET", f"{H}?team_id=2", None), ("GET", f"{H}?min_age=x", None),
    ("POST", f"{H}/teams/1/recruit?name=Tarantula", None),
    ("GET", f"{H}/teams/1", None),
    ("DELETE", f"{H}/teams/1", None),
    ("GET", f"{H}/2", None), ("GET", f"{H}/teams/1", None),
    # columns given by sa_column=Column(...)
    ("POST", f"{H}/missions", {"title": "Rescue", "notes": "night", "meta": {"risk": 3}}),
    ("POST", f"{H}/missions", {"title": "Patrol"}),
    ("POST", f"{H}/missions", {"title": "Rescue"}),
    ("GET", f"{H}/missions", None),
]

# a table whose base is in another module, an Enum column, exec() of update()/text() (fixtures/sqlmodelapp/gear.py)
STEPS += [
    ("POST", "/gear", {"name": "rope"}),
    ("POST", "/gear", {"name": "sword", "kind": "weapon", "weight": 2.5}),
    ("POST", "/gear", {"name": "bad", "kind": "spoon"}),
    ("POST", "/gear", {"name": "x" * 41}),
    ("POST", "/gear/heavier", None),
    ("GET", "/gear", None),
]


def reset(db: str) -> None:
    """the tables created once, by SQLModel itself (as the startup handler would: both servers starting together on a
    fresh database would race on CREATE TYPE / CREATE TABLE), then emptied"""
    import os

    import psycopg

    os.environ["DATABASE_URL"] = db
    from sqlmodel import SQLModel

    from fixtures.sqlmodelapp import main

    SQLModel.metadata.create_all(main.engine)
    main.engine.dispose()
    with psycopg.connect(db, autocommit=True) as c:
        c.execute("TRUNCATE pilot, song, sm_hero, sm_team, sm_mission, sm_gear RESTART IDENTITY")

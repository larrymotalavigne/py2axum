"""Loaded by the Python reference of a corpus example that opens a SQLite database (corpus/run.py): its engines go to
the PostgreSQL database of `CORPUS_SQLITE_TO` instead, as the binary's do (it serves PostgreSQL only). SQLite's
`connect_args` (check_same_thread) and pool class are dropped."""
import os

_TO = os.environ.get("CORPUS_SQLITE_TO")
if _TO:
    import sqlalchemy
    import sqlalchemy.engine

    _orig = sqlalchemy.engine.create_engine

    def create_engine(url, *args, **kwargs):
        if str(url).startswith("sqlite"):
            url = _TO
            kwargs.pop("connect_args", None)
            kwargs.pop("poolclass", None)
        return _orig(url, *args, **kwargs)

    sqlalchemy.create_engine = create_engine
    sqlalchemy.engine.create_engine = create_engine

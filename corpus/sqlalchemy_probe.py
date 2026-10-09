"""Run one unit of the SQLAlchemy documentation's doctests in Python, on PostgreSQL, and record what each
statement does (corpus/sqlalchemy_docs.py turns the record into routes).

    python sqlalchemy_probe.py <in.json> <out.json>

in.json: {"db": "postgresql+psycopg://...", "stmts": [{"src": ..., "exc": bool}], "seed_after": [i, ...]}.
The statements run in order in one namespace, as the doctest runner runs them (mode "single", so that an
expression statement's value is displayed). Every engine the documentation creates (SQLite in memory) is
created on the database of `db` instead, wiped first: a new engine is a new, empty database, as with SQLite.
Per statement: whether it raised, the kinds of SQL it emitted, whether it changed a session's pending state,
the value it displayed, what it printed (a SELECT construct printed for its SQL is executed on a separate
connection to learn whether PostgreSQL accepts it), and the types of the names it bound. After each statement of
`seed_after`, the committed rows of every table and the sequences are snapshotted (the seed of a segment).
"""
from __future__ import annotations

import __future__
import builtins
import io
import json
import re
import sys
import traceback
import types

import sqlalchemy
import sqlalchemy.orm
from sqlalchemy import event
from sqlalchemy.ext import asyncio as sa_asyncio
from sqlalchemy.ext.asyncio import engine as sa_async_engine
from sqlalchemy.orm import session as orm_session
from sqlalchemy.sql import ClauseElement
from sqlalchemy.sql.selectable import CompoundSelect, Select, TextualSelect

spec = json.load(open(sys.argv[1]))
DB = spec["db"]
_orig_create = sqlalchemy.create_engine
_orig_async = sa_asyncio.create_async_engine
_engines: list = []
current = {"i": -1}
records: list[dict] = []


def _wipe() -> None:
    import psycopg

    with psycopg.connect(DB.replace("+psycopg", ""), autocommit=True) as c:
        c.execute("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() "
                  "AND pid <> pg_backend_pid()")
        c.execute("DROP SCHEMA public CASCADE")
        c.execute("CREATE SCHEMA public")


def _classify(sql: str) -> str:
    s = re.sub(r"^\s*(--[^\n]*\n\s*)*", "", sql).lstrip("( ").upper()
    word = (re.match(r"[A-Z]+", s) or [""])[0]
    if word in ("SELECT", "WITH", "VALUES"):
        if "PG_CATALOG" in s or "INFORMATION_SCHEMA" in s or "CURRENT_SCHEMA()" in s or s.startswith("SELECT PG_"):
            return "catalog"
        if word == "WITH" and re.search(r"\)\s*(INSERT|UPDATE|DELETE|MERGE)\b", s):
            return "dml"
        return "select"
    if word in ("INSERT", "UPDATE", "DELETE", "MERGE"):
        return "dml"
    if word in ("CREATE", "DROP", "ALTER", "COMMENT", "TRUNCATE"):
        return "ddl"
    return "other"


def _tables(sql: str) -> tuple[set[str], set[str]]:
    """(tables a statement names, tables it writes), from its text."""
    name = r'"?([A-Za-z_][\w$]*)"?(?:\."?([A-Za-z_][\w$]*)"?)?'
    read = {(m.group(2) or m.group(1)).lower() for m in re.finditer(r"\b(?:FROM|JOIN|INTO|UPDATE|TABLE)\s+" + name, sql, re.I)}
    written = {(m.group(2) or m.group(1)).lower() for m in re.finditer(r"\b(?:INTO|UPDATE|DELETE\s+FROM|TABLE)\s+" + name, sql, re.I)}
    return read, written


def _listen(engine) -> None:
    sync = getattr(engine, "sync_engine", engine)

    @event.listens_for(sync, "before_cursor_execute")
    def _bce(conn, cursor, statement, parameters, context, executemany):  # noqa: ARG001
        if 0 <= current["i"] < len(records):
            rec = records[current["i"]]
            kind = _classify(statement)
            rec.setdefault("sql", []).append(kind)
            if kind != "catalog":
                read, written = _tables(statement)
                rec["tr"] = sorted(set(rec.get("tr", [])) | read)
                if kind in ("dml", "ddl"):
                    rec["tw"] = sorted(set(rec.get("tw", [])) | written)


def _url_kw(kw: dict) -> dict:
    kw = {k: v for k, v in kw.items() if k not in ("echo", "echo_pool", "future")}
    ca = kw.get("connect_args")
    if isinstance(ca, dict):
        kw["connect_args"] = {k: v for k, v in ca.items() if k not in ("check_same_thread",)}
    if kw.get("poolclass") is not None and kw["poolclass"].__name__ in ("StaticPool", "SingletonThreadPool"):
        kw.pop("poolclass")
    return kw


def create_engine(url, *a, **kw):  # noqa: ARG001
    for e in _engines + _exec_engine:
        e.dispose() if not hasattr(e, "sync_engine") else e.sync_engine.dispose()
    _wipe()
    e = _orig_create(DB, **_url_kw(kw))
    _engines.append(e)
    _listen(e)
    return e


def create_async_engine(url, *a, **kw):  # noqa: ARG001
    for e in _engines + _exec_engine:
        e.dispose() if not hasattr(e, "sync_engine") else e.sync_engine.dispose()
    _wipe()
    e = _orig_async(DB, **_url_kw(kw))
    _engines.append(e)
    _listen(e)
    return e


for mod, name, fn in ((sqlalchemy, "create_engine", create_engine), (sqlalchemy.engine, "create_engine", create_engine),
                      (sqlalchemy.engine.create, "create_engine", create_engine),
                      (sa_asyncio, "create_async_engine", create_async_engine),
                      (sa_async_engine, "create_async_engine", create_async_engine)):
    setattr(mod, name, fn)

_exec_engine = []


def _try_execute(stmt) -> str | None:
    """None when PostgreSQL runs the statement (on a connection of its own, rolled back), else the error."""
    if not _exec_engine:
        _exec_engine.append(_orig_create(DB, pool_pre_ping=True))
    try:
        with _exec_engine[0].connect() as c:
            c.exec_driver_sql("SET statement_timeout = 5000")
            c.execute(stmt).all()
            c.rollback()
        return None
    except Exception as e:  # noqa: BLE001
        return f"{type(e).__name__}: {str(e).splitlines()[0][:200]}"


def doc_print(*args, **kw):
    rec = records[current["i"]]
    info = []
    for a in args:
        sel = isinstance(a, (Select, CompoundSelect, TextualSelect))
        info.append({"sql": isinstance(a, ClauseElement), "select": sel,
                     "exec_error": _try_execute(a) if sel and len(args) == 1 else None})
        if sel:
            try:
                from sqlalchemy.dialects import postgresql

                rec["tr"] = sorted(set(rec.get("tr", [])) | _tables(str(a.compile(dialect=postgresql.dialect())))[0])
            except Exception:  # noqa: BLE001
                pass
    rec.setdefault("prints", []).append(info)
    kw.pop("file", None)
    builtins.print(*args, file=io.StringIO(), **kw)


def _display(value) -> None:
    if value is None:
        return
    try:
        text = repr(value)
    except Exception as e:  # noqa: BLE001
        text = f"<repr failed: {type(e).__name__}>"
    records[current["i"]]["display"] = text[:2000]
    builtins._ = value


def _pending() -> list:
    out = []
    for s in list(orm_session._sessions.values()):
        try:
            state = (sorted(map(id, s.new)), sorted(map(id, s.dirty)), sorted(map(id, s.deleted)))
            if any(state):  # a session opened and closed by the statement leaves nothing pending
                out.append((id(s), *state))
        except Exception:  # noqa: BLE001
            pass
    return sorted(out)


def _seed() -> dict:
    import psycopg

    tables, seqs = {}, []
    with psycopg.connect(DB.replace("+psycopg", ""), autocommit=True) as c:
        names = [r[0] for r in c.execute("SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY 1")]
        for t in names:
            cols = [r[0] for r in c.execute("SELECT attname FROM pg_attribute WHERE attrelid = %s::regclass "
                                            "AND attnum > 0 AND NOT attisdropped AND attgenerated = '' ORDER BY attnum",
                                            (f'"{t}"',))]
            buf = io.BytesIO()
            collist = ", ".join(f'"{x}"' for x in cols)
            with c.cursor().copy(f'COPY "{t}" ({collist}) TO STDOUT') as cp:
                for chunk in cp:
                    buf.write(chunk)
            tables[t] = {"columns": cols, "copy": buf.getvalue().decode()}
        for name, last in c.execute("SELECT sequencename, last_value FROM pg_sequences WHERE schemaname = 'public'"):
            seqs.append([name, last])
    return {"tables": tables, "sequences": seqs}


_mod = types.ModuleType("doc_unit")
sys.modules["doc_unit"] = _mod
ns = _mod.__dict__
ns["print"] = doc_print
if spec.get("inject"):
    # a page SQLAlchemy does not run as a doctest assumes an engine and a session
    ns["engine"] = create_engine("postgresql://")
    ns["session"] = sqlalchemy.orm.Session(ns["engine"])


def _create_tables() -> None:
    """The tables of the mappings defined so far (a page run as written creates none)."""
    from sqlalchemy import MetaData
    from sqlalchemy.orm import registry

    saved, current["i"] = current["i"], -1
    try:
        for v in list(ns.values()):
            m = v if isinstance(v, MetaData) else getattr(v, "metadata", None) if isinstance(v, (type, registry)) else None
            if isinstance(m, MetaData):
                try:
                    m.create_all(ns["engine"])
                except Exception:  # noqa: BLE001 - a fragment's mapping may not be complete
                    pass
    finally:
        current["i"] = saved
seeds = {}
sys.displayhook = _display
seed_after = set(spec.get("seed_after", []))
for i, st in enumerate(spec["stmts"]):
    rec = {"i": i}
    records.append(rec)
    current["i"] = i
    before_ids = {k: id(v) for k, v in ns.items()}
    before = _pending()
    try:
        if spec.get("inject"):
            _create_tables()
        flags = __future__.annotations.compiler_flag if st.get("future") else 0
        code = compile(st["src"], f"<doc{i}>", st.get("mode", "single"), flags=flags, dont_inherit=True)
        exec(code, ns)
        rec["ok"] = True
    except BaseException as e:  # noqa: BLE001
        rec["ok"] = False
        rec["exc"] = type(e).__name__
        rec["exc_qual"] = f"{type(e).__module__}.{type(e).__qualname__}"
        rec["exc_msg"] = (str(e).splitlines() or [""])[0][:300]
        if not st.get("exc"):
            rec["tb"] = "".join(traceback.format_exception(e))[-1500:]
    try:
        rec["state"] = _pending() != before
    except Exception:  # noqa: BLE001
        rec["state"] = True
    rec["types"] = {k: f"{type(v).__module__}.{type(v).__qualname__}" for k, v in ns.items()
                    if not k.startswith("__") and before_ids.get(k) != id(v)}
    if i in seed_after:
        current["i"] = -1
        try:
            seeds[str(i)] = _seed()
        except Exception as e:  # noqa: BLE001
            seeds[str(i)] = {"error": repr(e)}
current["i"] = -1
sys.displayhook = sys.__displayhook__
json.dump({"records": records, "seeds": seeds}, open(sys.argv[2], "w"))
for e in _engines + _exec_engine:
    try:
        (e.sync_engine if hasattr(e, "sync_engine") else e).dispose()
    except Exception:  # noqa: BLE001
        pass

"""The SQLAlchemy corpus: the documentation's doctests, each code block that runs SQL wrapped in a FastAPI route.

The documentation runs its examples as doctests (`test/base/test_tutorials.py` of SQLAlchemy): the pages of a
unit share one namespace, one long-lived session, and the data the earlier examples wrote. Here:

1. **Parse** each unit as that runner does (`{execsql}`-style markers removed, `.. doctest-include` setup pages,
   `.. doctest-disable` regions). A doctest example is one statement; a code block (consecutive examples with
   no prose between them) is one corpus example.
2. **Probe** (corpus/sqlalchemy_probe.py): run the unit in Python on PostgreSQL and record, per statement, the
   SQL it emits, whether it changes a session's pending state, what it displays and prints, and the types it
   binds. The committed rows after a setup page are the seed of what follows it.
3. **Slice.** A block is in the corpus when it runs SQL (or prints a SELECT, which the bench then executes:
   the documentation shows many queries only as rendered SQL). Its route runs, in a fresh session, the
   earlier statements it depends on (those binding the names it reads, transitively, and every earlier
   statement that wrote to the database or to a session), then the block itself; what the block prints or
   displays is returned as a JSON list of strings. Definitions (imports, classes, functions, tables) are
   module level. Each route is generated twice: `sync` (a `Session` dependency, `def` route) and `async`
   (an `AsyncSession` with `expire_on_commit=False`, `async def` route, `await` on the session and
   connection methods that are coroutines there).
4. **Replay.** Before every request the database is put back to the seed (tables truncated, seed rows copied
   back, sequences restored, tables an example created dropped), then the request goes to the Python reference
   and, after another reset, to the binary; the responses are compared with tests/conformance.py's logic.

A statement Python itself cannot run on PostgreSQL (SQLite-only SQL, a fragment of a page that is not
runnable as written) takes the blocks that depend on it out of the corpus; they are counted, by reason.
"""
from __future__ import annotations

import ast
import doctest
import json
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent

# (unit, section, files): the files of a unit run in one namespace, in order. The units SQLAlchemy's own
# doctest suite runs (test/base/test_tutorials.py) come first; then pages it does not run, as written.
UNITS = [
    ("tutorial", "SQLAlchemy Unified Tutorial", [
        "doc/build/tutorial/index.rst", "doc/build/tutorial/engine.rst", "doc/build/tutorial/dbapi_transactions.rst",
        "doc/build/tutorial/metadata.rst", "doc/build/tutorial/data.rst", "doc/build/tutorial/data_insert.rst",
        "doc/build/tutorial/data_select.rst", "doc/build/tutorial/data_update.rst",
        "doc/build/tutorial/orm_data_manipulation.rst", "doc/build/tutorial/orm_related_objects.rst"]),
    ("quickstart", "SQLAlchemy ORM", ["doc/build/orm/quickstart.rst"]),
    ("qg_select", "SQLAlchemy ORM Querying Guide", ["doc/build/orm/queryguide/_plain_setup.rst",
                                                    "doc/build/orm/queryguide/select.rst",
                                                    "doc/build/orm/queryguide/api.rst"]),
    ("qg_inheritance", "SQLAlchemy ORM Querying Guide", ["doc/build/orm/queryguide/inheritance.rst"]),
    ("qg_dml", "SQLAlchemy ORM Querying Guide", ["doc/build/orm/queryguide/dml.rst"]),
    ("qg_columns", "SQLAlchemy ORM Querying Guide", ["doc/build/orm/queryguide/columns.rst"]),
    ("qg_relationships", "SQLAlchemy ORM Querying Guide", ["doc/build/orm/queryguide/_plain_setup.rst",
                                                           "doc/build/orm/queryguide/relationships.rst"]),
    ("large_collections", "SQLAlchemy ORM", ["doc/build/orm/large_collections.rst"]),
    ("asyncio", "SQLAlchemy ORM", ["doc/build/orm/extensions/asyncio.rst"]),
    # not run by SQLAlchemy's doctest suite (AS_WRITTEN below): what runs as written is in the corpus
    ("cascades", "SQLAlchemy ORM", ["doc/build/orm/cascades.rst"]),
    ("backref", "SQLAlchemy ORM", ["doc/build/orm/backref.rst"]),
    ("session_state", "SQLAlchemy ORM", ["doc/build/orm/session_state_management.rst"]),
    ("composites", "SQLAlchemy ORM", ["doc/build/orm/composites.rst"]),
    ("collection_api", "SQLAlchemy ORM", ["doc/build/orm/collection_api.rst"]),
    ("join_conditions", "SQLAlchemy ORM", ["doc/build/orm/join_conditions.rst"]),
    ("declarative_tables", "SQLAlchemy ORM", ["doc/build/orm/declarative_tables.rst"]),
    ("mapping_styles", "SQLAlchemy ORM", ["doc/build/orm/mapping_styles.rst"]),
    ("mapped_attributes", "SQLAlchemy ORM", ["doc/build/orm/mapped_attributes.rst"]),
    ("relationship_persistence", "SQLAlchemy ORM", ["doc/build/orm/relationship_persistence.rst"]),
    ("associationproxy", "SQLAlchemy ORM Extensions", ["doc/build/orm/extensions/associationproxy.rst"]),
    ("hybrid", "SQLAlchemy ORM Extensions", ["lib/sqlalchemy/ext/hybrid.py"]),
    ("connections", "SQLAlchemy Core", ["doc/build/core/connections.rst"]),
    ("custom_types", "SQLAlchemy Core", ["doc/build/core/custom_types.rst"]),
    ("core_metadata", "SQLAlchemy Core", ["doc/build/core/metadata.rst"]),
    ("core_defaults", "SQLAlchemy Core", ["doc/build/core/defaults.rst"]),
]
MARKERS = re.compile(r"{(?:stop|sql|opensql|execsql|printsql)}")
# the units SQLAlchemy's doctest suite does not run: their mappings are in plain code blocks (no `>>>`), and their
# examples assume an engine and a session. The probe runs those definitions too, provides `engine` and `session`
# (PostgreSQL) and creates the tables of the mappings defined so far before each statement.
AS_WRITTEN = {u for u, _, _ in UNITS[UNITS.index(next(x for x in UNITS if x[0] == "cascades")):]}


@dataclass
class Stmt:
    page: str                     # doc/build/orm/queryguide/select.rst
    line: int                     # its first line in the page
    src: str
    want: str
    exc: bool                     # the documentation shows it raising
    sqlite_only: bool             # a SQLite connection hook (PRAGMA): not run, not in any route
    setup: bool                   # from a setup page (`_plain_setup.rst`, a doctest-include)
    block: int                    # code block number in the unit
    rec: dict = field(default_factory=dict)  # what the probe recorded
    mode: str = "single"          # "exec": a definition from a plain code block (no prompt) of a page run as written
    future: bool = False          # its block starts with `from __future__ import annotations`


def _page_lines(repo: Path, rel: str) -> list[tuple[int, str]]:
    """The page's (line number, text); for a Python module, its module docstring."""
    path = repo / rel
    text = path.read_text(encoding="utf-8")
    if rel.endswith(".py"):
        tree = ast.parse(text)
        node = tree.body[0]
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant)):
            return []
        doc = node.value.value
        first = node.lineno  # the opening quotes; the docstring's text starts on that line
        return [(first + i, line + "\n") for i, line in enumerate(doc.split("\n"))]
    return [(i, line) for i, line in enumerate(text.splitlines(keepends=True), 1)]


def _plain_blocks(lines: list[tuple[int, str]]) -> list[tuple[int, str]]:
    """The literal code blocks of a page that hold no doctest prompt: (first line number, dedented code)."""
    out, k = [], 0
    while k < len(lines):
        text = lines[k][1].rstrip("\n")
        intro = re.match(r"^(\s*)(?:\.\. (?:sourcecode|code-block)::\s*python3?\s*$|(?!\.\.).*::\s*$)", text)
        if not intro:
            k += 1
            continue
        indent = len(intro.group(1))
        j, body = k + 1, []
        while j < len(lines):
            t = lines[j][1].rstrip("\n")
            if t.strip() and len(t) - len(t.lstrip()) <= indent:
                break
            body.append((lines[j][0], t))
            j += 1
        while body and (not body[0][1].strip() or body[0][1].strip().startswith(":")):
            body.pop(0)
        code = "\n".join(t for _, t in body)
        if body and ">>>" not in code:
            import textwrap

            out.append((body[0][0], textwrap.dedent(code)))
        k = max(j, k + 1)
    return out


def _plain_stmts(page: str, lines, block_base: int) -> list[Stmt]:
    """The definitions of a page's plain code blocks, one statement per top-level node."""
    out = []
    for n, (first, code) in enumerate(_plain_blocks(lines)):
        try:
            tree = ast.parse(code)
        except SyntaxError:
            continue
        future = any(isinstance(x, ast.ImportFrom) and x.module == "__future__" for x in tree.body)
        src = code.split("\n")
        for node in tree.body:
            if isinstance(node, ast.ImportFrom) and node.module == "__future__":
                continue
            if not isinstance(node, (ast.Import, ast.ImportFrom, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef,
                                     ast.Assign, ast.AnnAssign)):
                continue
            start = node.decorator_list[0].lineno if getattr(node, "decorator_list", None) else node.lineno
            text = "\n".join(src[start - 1:node.end_lineno]) + "\n"
            out.append(Stmt(page, first + start - 1, text, "", False, bool(re.search(r"\bPRAGMA\b", text)), False,
                            block_base + n, mode="exec", future=future))
    return out


def parse_unit(repo: Path, files: list[str], plain: bool = False) -> list[Stmt]:
    """The unit's statements, as test_tutorials.py's runner sees them (with `plain`, the definitions of the plain
    code blocks too, in page order)."""
    parts: list[tuple[str, bool, list[tuple[int, str]]]] = []  # (page, setup, lines)
    for rel in files:
        setup_page = Path(rel).name.startswith("_")
        buf: list[tuple[int, str]] = []
        enabled = True
        for n, line in _page_lines(repo, rel):
            line = MARKERS.sub("", line)
            inc = re.match(r"\.\. doctest-include (.+\.rst)", line)
            if inc:
                parts.append((rel, setup_page, buf))
                buf = []
                sub = str(Path(rel).parent / inc.group(1))
                parts.append((sub, True, _page_lines(repo, sub)))
            dis = re.match(r"\.\. doctest-(enable|disable)", line)
            if dis:
                enabled = dis.group(1) == "enable"
            buf.append((n, line if enabled else "\n"))
        parts.append((rel, setup_page, buf))
    out: list[Stmt] = []
    parser = doctest.DocTestParser()
    block = -1
    for page, setup, lines in parts:
        if not lines:
            continue
        text = "".join(line for _, line in lines)
        examples = parser.get_examples(text)
        prev = None
        mine = []
        for ex in examples:
            if prev is None or _prose_between(lines, prev, ex):
                block += 1
            mine.append(Stmt(page, lines[ex.lineno][0], ex.source, ex.want, ex.exc_msg is not None,
                             bool(re.search(r"\bPRAGMA\b", ex.source)), setup, block))
            prev = ex
        if plain:
            extra = _plain_stmts(page, lines, 100000 + block * 100)
            mine = sorted(mine + extra, key=lambda st: st.line)
        out += mine
    if plain and out:
        # the imports such a page takes for granted (its fragments use `select`, `Session`... without importing them)
        out.insert(0, Stmt(out[0].page, 0, USUAL_IMPORTS, "", False, False, False, -1, mode="exec"))
    return out


USUAL_IMPORTS = ("from sqlalchemy import Column, ForeignKey, Integer, String, Table, func, select\n"
                 "from sqlalchemy.orm import DeclarativeBase, Mapped, Session, mapped_column, relationship\n")


def _prose_between(lines, a, b) -> bool:
    """Whether a line less indented than example `a`'s prompt separates it from `b` (another code block)."""
    for _, line in lines[a.lineno + 1:b.lineno]:
        if line.strip() and len(line) - len(line.lstrip()) < a.indent:
            return True
    return False


def probe(work: Path, unit: str, stmts: list[Stmt], db: str) -> dict:
    """Run the unit (corpus/sqlalchemy_probe.py); cached by its statements and the installed SQLAlchemy."""
    import hashlib

    import sqlalchemy

    d = work.resolve() / "probe"
    d.mkdir(parents=True, exist_ok=True)
    seed_after = [i for i, s in enumerate(stmts) if s.setup and (i + 1 == len(stmts) or not stmts[i + 1].setup)]
    spec = {"db": db, "stmts": [{"src": "pass\n" if s.sqlite_only else s.src, "exc": s.exc, "mode": s.mode,
                                 "future": s.future} for s in stmts],
            "seed_after": seed_after, "inject": unit in AS_WRITTEN}
    key = hashlib.sha256(json.dumps(spec).encode() + (HERE / "sqlalchemy_probe.py").read_bytes()
                         + sqlalchemy.__version__.encode()).hexdigest()[:16]
    out = d / f"{unit}.{key}.json"
    if not out.exists():
        for old in d.glob(f"{unit}.*.json"):
            old.unlink()
        (d / f"{unit}.in.json").write_text(json.dumps(spec))
        r = subprocess.run([sys.executable, str(HERE / "sqlalchemy_probe.py"), str(d / f"{unit}.in.json"), str(out)],
                           cwd=d, capture_output=True, text=True, timeout=900)
        if not out.exists():
            raise RuntimeError(f"probe of {unit} failed: {r.stderr[-2000:]}")
    data = json.loads(out.read_text())
    for s, rec in zip(stmts, data["records"]):
        s.rec = rec
    return data


# --- slicing: which statements a block's route runs ------------------------------------------------------------

SCHEMA_CALLS = {"Table", "MetaData", "registry", "declarative_base", "Column", "Sequence", "Enum", "Index",
                "UniqueConstraint", "CheckConstraint", "ForeignKeyConstraint", "Identity", "Computed",
                "sessionmaker", "async_sessionmaker", "scoped_session", "TypeVar", "NewType"}
SESSION_TYPES = ("sqlalchemy.orm.session.Session", "sqlalchemy.ext.asyncio.session.AsyncSession")
CONN_TYPES = ("sqlalchemy.engine.base.Connection", "sqlalchemy.ext.asyncio.engine.AsyncConnection")
ENGINE_TYPES = ("sqlalchemy.engine.base.Engine", "sqlalchemy.ext.asyncio.engine.AsyncEngine")
CONTROL = {"commit", "rollback", "flush", "close", "expire", "expire_all", "expunge", "expunge_all", "refresh",
           "merge", "delete", "add", "add_all", "begin", "begin_nested", "reset", "invalidate", "close_all"}
# methods that are coroutines on AsyncSession / AsyncConnection / AsyncEngine
SESSION_AWAIT = {"execute", "scalars", "scalar", "get", "get_one", "flush", "commit", "rollback", "refresh",
                 "delete", "merge", "close", "connection", "run_sync", "stream", "stream_scalars", "invalidate",
                 "reset", "begin", "begin_nested"}
CONN_AWAIT = {"execute", "scalars", "scalar", "exec_driver_sql", "commit", "rollback", "close", "begin",
              "begin_nested", "run_sync", "stream", "stream_scalars", "invalidate", "get_raw_connection", "start"}
ENGINE_AWAIT = {"dispose"}


@dataclass
class Example:
    id: str                       # tutorial__data_select__l123
    unit: str
    page: str
    line: int
    section: str
    stmts: list[int]              # the route's statements, in order (indexes in the unit)
    block: list[int]              # the block's own statements
    handles: dict                 # names the route provides: {"session": "session", "conn": "conn", ...}
    excluded: str | None = None   # why it is not in the corpus
    segment: int = 0


def _names(node: ast.AST) -> tuple[set[str], set[str]]:
    """(names read from before the statement, names it binds). A name the statement binds before reading it (a
    loop variable, a `with ... as` target, an earlier assignment in its body) is not read from before."""
    stores: dict[str, list[tuple[int, int]]] = {}
    writes = set()
    value_spans = []  # the value of an assignment is evaluated before its targets are bound
    for n in ast.walk(node):
        if isinstance(n, ast.Name) and isinstance(n.ctx, (ast.Store, ast.Del)):
            writes.add(n.id)
            stores.setdefault(n.id, []).append((n.lineno, n.col_offset))
        elif isinstance(n, (ast.Import, ast.ImportFrom)):
            for a in n.names:
                writes.add((a.asname or a.name).split(".")[0])
        elif isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            writes.add(n.name)
        elif isinstance(n, (ast.Assign, ast.AugAssign, ast.AnnAssign)) and n.value is not None:
            value_spans.append(((n.value.lineno, n.value.col_offset), (n.value.end_lineno, n.value.end_col_offset),
                                [t for t in (n.targets if isinstance(n, ast.Assign) else [n.target])]))
    reads = set()
    for n in ast.walk(node):
        if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load):
            pos = (n.lineno, n.col_offset)
            before = [q for q in stores.get(n.id, []) if q < pos]
            # a store that is the target of the assignment whose value holds this read does not count
            for start, end, targets in value_spans:
                if start <= pos <= end:
                    tpos = {(t.lineno, t.col_offset) for t in targets for t in ast.walk(t) if isinstance(t, ast.Name)}
                    before = [q for q in before if q not in tpos]
            if not before:
                reads.add(n.id)
    return reads, writes


def _call_name(node) -> str | None:
    if isinstance(node, ast.Call):
        f = node.func
        return f.id if isinstance(f, ast.Name) else f.attr if isinstance(f, ast.Attribute) else None
    return None


def _kind(s: Stmt, tree: ast.Module) -> str:
    """def (module level), engine / handle (provided by the route), ddl / setup (dropped), stmt (route body)."""
    body = tree.body
    if s.sqlite_only:
        return "ddl"
    sql = [k for k in s.rec.get("sql", []) if k != "catalog"]
    types = s.rec.get("types", {})
    if all(isinstance(n, (ast.Import, ast.ImportFrom, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef,
                          ast.TypeAlias)) for n in body):
        return "def"
    if any(isinstance(n, ast.keyword) and n.arg == "autoload_with" for n in ast.walk(tree)):
        return "reflect"
    if len(body) == 1 and isinstance(body[0], (ast.Assign, ast.AnnAssign)):
        v = body[0].value
        if any(t in ENGINE_TYPES for t in types.values()):
            return "engine"
        if any(t in SESSION_TYPES + CONN_TYPES for t in types.values()) and isinstance(v, ast.Call):
            return "handle"
        calls = {_call_name(c) for c in ast.walk(v)} if v is not None else set()
        if not sql and calls & SCHEMA_CALLS:
            return "def"
    if len(body) == 1 and isinstance(body[0], ast.Expr) and _call_name(body[0].value) in (
            "create_all", "drop_all", "create", "drop", "reflect") and not set(sql) - {"ddl"}:
        return "ddl"
    return "stmt"


def _control(tree: ast.Module) -> bool:
    return any(isinstance(n, ast.Expr) and isinstance(n.value, ast.Call) and isinstance(n.value.func, ast.Attribute)
               and n.value.func.attr in CONTROL for n in tree.body)


def _failed(s: Stmt) -> str | None:
    r = s.rec
    if r.get("ok") or s.exc or not r:
        return None
    if "InFailedSqlTransaction" in r.get("exc_msg", "") or "current transaction is aborted" in r.get("exc_msg", ""):
        return None  # a consequence of an earlier failure in the documentation's long transaction
    return f"{r.get('exc')}: {r.get('exc_msg', '')}"


def plan(unit: str, section: str, stmts: list[Stmt]) -> tuple[list[Example], list[dict]]:
    """The unit's examples and its segments ({"start", "defs": [i]}): a segment starts at a setup page that is not
    the unit's first statement, or at a class or function defined again."""
    trees, kinds, rw = [], [], []
    for s in stmts:
        try:
            t = ast.parse(textwrap_dedent(s.src))
        except SyntaxError:
            t = ast.parse("pass")
        trees.append(t)
        kinds.append(_kind(s, t))
        rw.append(_names(t))
    # segments: a setup page that is not the unit's first statement starts a new world (its definitions only,
    # with the imports before it); a class or function defined again starts a new segment
    # where the latest definition of each name holds
    segs: list[dict] = [{"start": 0, "world": 0}]
    defined: dict[str, str] = {}
    for i, s in enumerate(stmts):
        new_setup = s.setup and i > 0 and not stmts[i - 1].setup
        # (even with the same body: a new class object, which the later definitions build on)
        redefined = kinds[i] == "def" and any(
            isinstance(n, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)) and n.name in defined
            for n in trees[i].body)
        if new_setup or redefined:
            segs.append({"start": i, "world": i if new_setup else segs[-1]["world"]})
            if new_setup:
                defined = {}
        if kinds[i] == "def":
            for n in trees[i].body:
                if isinstance(n, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
                    defined[n.name] = ast.dump(n)
    for k, g in enumerate(segs):
        end = segs[k + 1]["start"] if k + 1 < len(segs) else len(stmts)
        latest: dict[str, int] = {}
        keep = []
        for i in range(end):
            if kinds[i] != "def":
                continue
            if i < g["world"]:
                if all(isinstance(n, (ast.Import, ast.ImportFrom)) for n in trees[i].body):
                    keep.append(i)
                continue
            for name in rw[i][1]:
                latest[name] = i
            keep.append(i)
        g["defs"] = [i for i in keep if all(isinstance(n, (ast.Import, ast.ImportFrom)) for n in trees[i].body)
                     or any(latest.get(name) == i for name in rw[i][1])]
    seg_of = []
    for i in range(len(stmts)):
        seg_of.append(max(k for k, g in enumerate(segs) if g["start"] <= i))

    blocks: dict[int, list[int]] = {}
    for i, s in enumerate(stmts):
        if not s.setup:
            blocks.setdefault(s.block, []).append(i)
    out = []
    for b, idx in blocks.items():
        runs = any(kinds[i] == "stmt" and set(stmts[i].rec.get("sql", [])) & {"select", "dml"} for i in idx)
        shows = any(kinds[i] == "stmt" and _printed_select(stmts[i], trees[i]) for i in idx)
        if not (runs or shows):
            continue
        seg = seg_of[idx[0]]
        start = segs[seg]["world"]  # what an example can depend on: its world, across redefinitions
        s0 = stmts[idx[0]]
        ex = Example(f"{unit}__{Path(s0.page).stem.lstrip('_')}__l{s0.line}", unit, s0.page, s0.line, section, [],
                     idx, {}, segment=seg)
        needed = {i for i in idx if kinds[i] == "stmt"}
        first = min(idx)
        # earlier statements that change a session (pending objects, commit, rollback...) are always replayed;
        # those that write to the database, when they write a table the example (or what it replays) uses
        writers = []
        for j in range(start, first):
            if kinds[j] != "stmt" or stmts[j].setup or _failed(stmts[j]):
                continue
            if stmts[j].rec.get("state") or _control(trees[j]):
                needed.add(j)
            elif "dml" in stmts[j].rec.get("sql", []):
                writers.append(j)
        work = sorted(needed)
        while True:
            _closure(work, needed, ex, stmts, kinds, rw, start)
            tables = set().union(*(set(stmts[i].rec.get("tr", [])) | set(stmts[i].rec.get("tw", [])) for i in needed))
            more = [j for j in writers if j not in needed and (not stmts[j].rec.get("tw") or set(stmts[j].rec["tw"]) & tables)]
            if not more:
                break
            needed.update(more)
            work = sorted(more)
        ex.stmts = sorted(needed)
        if "other" in ex.handles.values():
            ex.excluded = ex.excluded or "uses a connection object the bench cannot provide"
        for i in ex.stmts:
            f = _failed(stmts[i])
            if f:
                where = "" if i in idx else f" (at {stmts[i].page}:{stmts[i].line})"
                ex.excluded = ex.excluded or f"Python fails on PostgreSQL{where}: {f[:160]}"
        out.append(ex)
    return out, segs, kinds, trees


def _closure(work: list[int], needed: set[int], ex: Example, stmts: list[Stmt], kinds, rw, start: int) -> None:
    """Add to `needed` the statements binding the names that `work`'s statements read, transitively; the names the
    route provides (session, connection, engine) go to `ex.handles`."""
    while work:
        i = work.pop()
        for name in rw[i][0]:
            # a statement that failed in Python bound nothing: the name is the earlier binding
            w = next((j for j in range(i - 1, -1, -1) if name in rw[j][1] and not _failed(stmts[j])), None)
            if w is None or kinds[w] == "def" or w in needed:
                continue
            if kinds[w] in ("engine", "handle"):
                types = stmts[w].rec.get("types", {})
                t = types.get(name, "")
                ex.handles[name] = ("session" if t in SESSION_TYPES else "conn" if t in CONN_TYPES else
                                    "engine" if t in ENGINE_TYPES else "other")
                continue
            if w < start:
                ex.excluded = ex.excluded or f"depends on `{name}` from an earlier setup of the page"
                continue
            if stmts[w].setup:
                ex.excluded = ex.excluded or f"depends on `{name}` bound by the setup page"
                continue
            if kinds[w] == "ddl":
                continue
            if kinds[w] == "reflect":
                ex.excluded = ex.excluded or "reflects a table an example creates (autoload_with=)"
                continue
            needed.add(w)
            work.append(w)


def textwrap_dedent(src: str) -> str:
    import textwrap

    return textwrap.dedent(src)


def _printed_select(s: Stmt, tree: ast.Module) -> bool:
    """A top-level `print(stmt)` of a SELECT PostgreSQL accepts: the bench executes it instead."""
    prints = s.rec.get("prints") or []
    return (len(tree.body) == 1 and isinstance(tree.body[0], ast.Expr) and _call_name(tree.body[0].value) == "print"
            and len(prints) == 1 and len(prints[0]) == 1 and prints[0][0]["select"] and prints[0][0]["exec_error"] is None)


# --- the generated app: one module per (unit, segment, variant) ------------------------------------------------

PRELUDE = {
    "sync": ["import os", "from fastapi import Depends, FastAPI",
             "from sqlalchemy import create_engine",
             "from sqlalchemy.orm import Session, sessionmaker", "",
             'engine = create_engine(os.environ["DATABASE_URL"])', "_SessionLocal = sessionmaker(engine)", "",
             "", "def _session():", "    with _SessionLocal() as session:", "        yield session"],
    "async": ["import os", "from fastapi import Depends, FastAPI",
              "from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine", "",
              'engine = create_async_engine(os.environ["DATABASE_URL"])',
              "_SessionLocal = async_sessionmaker(engine, expire_on_commit=False)", "", "",
              "async def _session():", "    async with _SessionLocal() as session:", "        yield session"],
}


class _Prints(ast.NodeTransformer):
    """`print(a, b)` -> `<target>.append(" ".join([str(a), str(b)]))` (what the documentation shows)."""

    def __init__(self, target: str):
        self.target = target

    def visit_Expr(self, node):
        self.generic_visit(node)
        v = node.value
        if isinstance(v, ast.Call) and isinstance(v.func, ast.Name) and v.func.id == "print" and not any(
                k.arg not in ("sep",) for k in v.keywords):
            sep = next((k.value for k in v.keywords if k.arg == "sep"), ast.Constant(" "))
            parts = [ast.Call(ast.Name("str", ast.Load()), [a], []) for a in v.args]
            text = parts[0] if len(parts) == 1 else ast.Constant("") if not parts else ast.Call(
                ast.Attribute(sep, "join", ast.Load()), [ast.List(parts, ast.Load())], [])
            return ast.copy_location(ast.Expr(ast.Call(ast.Attribute(ast.Name(self.target, ast.Load()), "append",
                                                                      ast.Load()), [text], [])), node)
        return node


class _Async(ast.NodeTransformer):
    """The sync documentation code as AsyncSession / AsyncConnection code."""

    def __init__(self, sessions: set[str], conns: set[str], engines: set[str]):
        self.sessions, self.conns, self.engines = set(sessions), set(conns), set(engines)

    def _role(self, node) -> str | None:
        if isinstance(node, ast.Name):
            return ("session" if node.id in self.sessions else "conn" if node.id in self.conns else
                    "engine" if node.id in self.engines else None)
        return None

    def _ctx(self, expr) -> str | None:
        """What a `with` item's context manager opens: session / conn / tx, or None."""
        if isinstance(expr, ast.Call):
            f = expr.func
            if isinstance(f, ast.Name) and f.id == "Session":
                return "session"
            if isinstance(f, ast.Attribute):
                role = self._role(f.value)
                if role == "engine" and f.attr in ("connect", "begin"):
                    return "conn"
                if role in ("session", "conn") and f.attr in ("begin", "begin_nested"):
                    return "tx"
        return None

    def visit_With(self, node):
        kinds = [self._ctx(i.context_expr) for i in node.items]
        for item, k in zip(node.items, kinds):
            if k and isinstance(item.optional_vars, ast.Name):
                (self.sessions if k == "session" else self.conns if k == "conn" else set()).add(item.optional_vars.id)
        self.generic_visit(node)
        if any(kinds):
            for item, k in zip(node.items, kinds):
                if k and isinstance(item.context_expr, ast.Await):
                    item.context_expr = item.context_expr.value  # `async with session.begin()`, not awaited
            return ast.copy_location(ast.AsyncWith(node.items, node.body), node)
        return node

    def visit_Assign(self, node):
        self.generic_visit(node)
        return node

    def visit_Call(self, node):
        self.generic_visit(node)
        f = node.func
        if isinstance(f, ast.Name) and f.id == "Session":
            node.func = ast.Name("AsyncSession", ast.Load())
            if not any(k.arg == "expire_on_commit" for k in node.keywords):
                node.keywords.append(ast.keyword("expire_on_commit", ast.Constant(False)))
            return node
        if isinstance(f, ast.Attribute) and f.attr == "run" and isinstance(f.value, ast.Name) and f.value.id == "asyncio":
            return ast.Await(node.args[0])
        if isinstance(f, ast.Attribute):
            role = self._role(f.value)
            if (role == "session" and f.attr in SESSION_AWAIT) or (role == "conn" and f.attr in CONN_AWAIT) or (
                    role == "engine" and f.attr in ENGINE_AWAIT):
                return ast.Await(node)
        return node


def _engine_urls(tree: ast.AST) -> ast.AST:
    """`create_engine("sqlite://...")` inside the documentation's code -> the bench's database."""
    for n in ast.walk(tree):
        if isinstance(n, ast.Call) and _call_name(n) in ("create_engine", "create_async_engine") and n.args:
            n.args[0] = ast.parse('os.environ["DATABASE_URL"]', mode="eval").body
            n.keywords = [k for k in n.keywords if k.arg not in ("echo", "future")]
    return tree


def render(unit: str, seg: dict, variant: str, examples: list[Example], stmts: list[Stmt], kinds, trees,
           async_native: bool) -> tuple[str, list[str], dict[str, str]]:
    """The module's source, the documentation origin of each of its lines ("page:line" or ""), and the route of
    each example."""
    lines: list[str] = []
    origin: list[str] = []

    def emit(text: str, where: str = "") -> None:
        for t in text.split("\n"):
            lines.append(t)
            origin.append(where)

    if any(stmts[i].future for i in seg["defs"]):
        emit("from __future__ import annotations")
    emit("# Generated by corpus/sqlalchemy_docs.py from the SQLAlchemy documentation (MIT): do not edit.")
    for t in PRELUDE[variant]:
        emit(t)
    emit("")
    printing_defs = False
    for i in seg["defs"]:
        s = stmts[i]
        tree = _engine_urls(ast.parse(textwrap_dedent(s.src)))
        if any(isinstance(n, ast.Call) and isinstance(n.func, ast.Name) and n.func.id == "print"
               for n in ast.walk(tree)):
            tree = _Prints("_OUT").visit(tree)
            printing_defs = True
            src = ast.unparse(tree)
            emit("")
            emit(src, f"{s.page}:{s.line}")
        else:
            src = textwrap_dedent(s.src).rstrip("\n")
            if "create_engine(" in src or "create_async_engine(" in src:
                src = ast.unparse(tree)
                emit(src, f"{s.page}:{s.line}")
            else:
                for k, t in enumerate(src.split("\n")):
                    emit(t, f"{s.page}:{s.line + k}")
        if isinstance(tree.body[-1], (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            emit("")
    emit("")
    emit("# --- corpus: one route per documentation block (corpus/sqlalchemy_docs.py) ---")
    if printing_defs:
        emit("_OUT: list = []")
    emit("app = FastAPI()")
    routes = {}
    for n, ex in enumerate(examples):
        path = f"/e/{n}"
        routes[ex.id] = path
        emit("")
        emit("")
        emit(f'@app.post("{path}")', f"{ex.page}:{ex.line}")
        sess_t = "Session" if variant == "sync" else "AsyncSession"
        emit(f"{'async def' if variant == 'async' else 'def'} _e{n}(session: {sess_t} = Depends(_session)) -> list:",
             f"{ex.page}:{ex.line}")
        if printing_defs:
            emit("    _OUT.clear()")
            emit("    _out = _OUT")
        else:
            emit("    _out = []")
        sessions = {"session"} | {k for k, v in ex.handles.items() if v == "session"}
        conns = {k for k, v in ex.handles.items() if v == "conn"}
        engines = {"engine"} | {k for k, v in ex.handles.items() if v == "engine"}
        for name, role in ex.handles.items():
            if role == "session" and name != "session":
                emit(f"    {name} = session")
            elif role == "conn":
                emit(f"    {name} = {'await ' if variant == 'async' else ''}session.connection()")
            elif role == "engine" and name != "engine":
                emit(f"    {name} = engine")
        for i in ex.stmts:
            s = stmts[i]
            body = _route_stmt(s, trees[i], i in ex.block)
            mod = _engine_urls(ast.Module(body, []))
            mod = _Prints("_out").visit(mod)
            if variant == "async":
                mod = _Async(sessions, conns, engines).visit(mod)
            ast.fix_missing_locations(mod)
            for k, t in enumerate(ast.unparse(mod).split("\n")):
                emit("    " + t, f"{s.page}:{s.line}")
        emit("    return _out")
    emit("")
    return "\n".join(lines), origin, routes


def _route_stmt(s: Stmt, tree: ast.Module, own: bool) -> list[ast.stmt]:
    """A statement as the route runs it: a printed SELECT executed, a displayed value appended (the block's
    own statements only), an exception the documentation shows caught and reported by its class name."""
    import copy

    body = copy.deepcopy(tree.body)
    if own and _printed_select(s, tree):
        arg = body[0].value.args[0]
        body = [ast.Expr(ast.Call(ast.Name("print", ast.Load()), [ast.Call(ast.Name("repr", ast.Load()), [
            ast.Call(ast.Attribute(ast.Call(ast.Attribute(ast.Name("session", ast.Load()), "execute", ast.Load()),
                                            [arg], []), "all", ast.Load()), [], [])], [])], []))]
    elif len(body) == 1 and isinstance(body[0], ast.Expr) and _call_name(body[0].value) == "print" and (
            s.rec.get("prints") and any(p["sql"] for p in s.rec["prints"][0])):
        return [ast.Pass()]  # a statement printed for its SQL text (not executable here): rendering only
    elif own and s.rec.get("display") is not None and len(body) == 1 and isinstance(body[0], ast.Expr) \
            and not re.search(r" at 0x[0-9a-f]+", s.rec["display"]):  # an object without a repr shows nothing
        body = [ast.Expr(ast.Call(ast.Name("print", ast.Load()), [ast.Call(ast.Name("repr", ast.Load()),
                                                                           [body[0].value], [])], []))]
    if s.exc:
        handler = [ast.Expr(ast.Call(ast.Name("print", ast.Load()), [ast.Attribute(ast.Call(
            ast.Name("type", ast.Load()), [ast.Name("_e", ast.Load())], []), "__name__", ast.Load())], []))] if own \
            else [ast.Pass()]
        body = [ast.Try(body, [ast.ExceptHandler(ast.Name("Exception", ast.Load()), "_e", handler)], [], [])]
    return body


# --- the bench: prepare, check, generate, build, replay -------------------------------------------------------

DOCS_URL = "https://docs.sqlalchemy.org/en/21/"
ASYNC_NATIVE = {"asyncio"}  # pages written for asyncio: only the async variant applies
PORT_BASE = 10300           # ports 10300-10399: two per worker slot
SCHEMA = r'''
import importlib, json, os, sys
sys.path.insert(0, sys.argv[1])
mod = importlib.import_module(sys.argv[2])
from sqlalchemy import MetaData, create_engine
from sqlalchemy.orm import DeclarativeBase, registry
metas = []
for v in list(vars(mod).values()):
    m = v if isinstance(v, MetaData) else getattr(v, "metadata", None) if isinstance(v, (type, registry)) else None
    if isinstance(m, MetaData) and all(m is not x for x in metas):
        metas.append(m)
e = create_engine(sys.argv[3])
for m in metas:
    m.create_all(e)
e.dispose()
'''


@dataclass
class App:
    id: str                       # sqla_qg_select_s0_sync
    unit: str
    variant: str
    seg: int
    examples: list[Example]
    routes: dict[str, str]        # example id -> route path
    spans: dict[str, tuple[int, int]]  # example id -> lines of its route in main.py
    origin: list[str]
    seed: dict | None
    native: set = field(default_factory=set)  # the examples whose route is generated


def _title(repo: Path, rel: str) -> str:
    if rel.endswith(".py"):
        return {"hybrid.py": "Hybrid Attributes", "associationproxy.py": "Association Proxy"}.get(Path(rel).name, rel)
    lines = (repo / rel).read_text(encoding="utf-8").splitlines()
    for a, b in zip(lines, lines[1:]):
        if a.strip() and not a.startswith((" ", ".", ":")) and re.fullmatch(r"([=\-^~*#])\1{2,}", b.strip()) \
                and len(b.strip()) >= len(a.strip()):
            return a.strip()
    return Path(rel).stem


def page_url(page: str) -> str:
    if page.endswith(".py"):
        return DOCS_URL + "orm/extensions/" + Path(page).stem + ".html"
    return DOCS_URL + page.removeprefix("doc/build/").removesuffix(".rst") + ".html"


def prepare(repo: Path, work: Path, db: str, log, only: str | None = None) -> tuple[list[App], dict[str, int], dict]:
    """Parse, probe and plan every unit, and write each app's package under <work>/pkgs. Returns the apps, the
    count of blocks left out of the corpus by reason, and the page titles."""
    pk = work / "pkgs"
    pk.mkdir(parents=True, exist_ok=True)
    (pk / "__init__.py").write_text("")
    apps, skipped, titles = [], {}, {}
    for unit, section, files in UNITS:
        if only and only not in unit:
            continue
        stmts = parse_unit(repo, files, plain=unit in AS_WRITTEN)
        data = probe(work, unit, stmts, db)
        exs, segs, kinds, trees = plan(unit, section, stmts)
        for ex in exs:
            titles.setdefault(ex.page, _title(repo, ex.page))
        variants = ["async"] if unit in ASYNC_NATIVE else ["sync", "async"]
        for ex in exs:
            if ex.excluded:
                key = re.sub(r" \(at [^)]*\)", "", ex.excluded.split(":")[0])
                skipped[key] = skipped.get(key, 0) + len(variants)
        for k, seg in enumerate(segs):
            mine = [e for e in exs if e.segment == k and not e.excluded]
            if not mine:
                continue
            first = min(i for e in mine for i in e.block)
            seeds = [int(i) for i in data["seeds"] if int(i) < first]
            seed = data["seeds"][str(max(seeds))] if seeds else None
            for v in variants:
                app_id = f"sqla_{unit}_s{k}_{v}"
                src, origin, routes = render(unit, seg, v, mine, stmts, kinds, trees, unit in ASYNC_NATIVE)
                spans = _spans(src, routes)
                d = pk / app_id
                d.mkdir(exist_ok=True)
                _write(d / "__init__.py", "")
                _write(d / "main.py", src)
                _write(d / "lines.json", json.dumps(origin))
                apps.append(App(app_id, unit, v, k, mine, routes, spans, origin, seed))
    log(f"sqlalchemy: {sum(len(a.examples) for a in apps)} examples in {len(apps)} apps; "
        f"not in the corpus: {skipped}")
    return apps, skipped, titles


def _write(path: Path, text: str) -> None:
    if not path.exists() or path.read_text() != text:
        path.write_text(text)


def _spans(src: str, routes: dict[str, str]) -> dict[str, tuple[int, int]]:
    lines = src.split("\n")
    starts = {}
    for n, line in enumerate(lines, 1):
        m = re.match(r'@app\.post\("(/e/\d+)"\)', line)
        if m:
            starts[m.group(1)] = n
    ordered = sorted(starts.items(), key=lambda kv: kv[1])
    ends = {p: (ordered[k + 1][1] - 1 if k + 1 < len(ordered) else len(lines)) for k, (p, _) in enumerate(ordered)}
    return {ex: (starts[p], ends[p]) for ex, p in routes.items() if p in starts}


def unstage(text: str, app: App, pkgs: Path) -> str:
    """file:line in a generated app -> the documentation's page:line."""
    base = re.escape(str((pkgs / app.id).resolve()) + "/main.py") + "|" + re.escape(str(pkgs / app.id) + "/main.py") \
        + "|" + re.escape(str((pkgs / (app.id + "_n")).resolve()) + "/main.py") + "|" + \
        re.escape(str(pkgs / (app.id + "_n")) + "/main.py")

    def sub(m):
        n = int(m.group(1))
        o = app.origin[n - 1] if 0 < n <= len(app.origin) else ""
        return o or f"(generated route, line {n})"

    return re.sub(rf"(?:{base}):(\d+)", sub, text)


def _label(construction: str, error: str) -> str:
    """A refusal's construction as the SQLAlchemy table shows it: the library call or the option refused, without
    the names of the page's classes and columns (`check` groups them by kind: "library sqlalchemy")."""
    m = re.search(r"library call `([^`]+)` is not supported", error)
    if m:
        return f"`{m.group(1)}`"
    if construction.startswith("method ."):
        return construction
    msg = re.sub(r"^\S+:\d+: ", "", error.strip().splitlines()[0] if error.strip() else construction)
    msg = re.sub(r"^(model|column|relationship|dataclass|class|session dependency|decorator) [\w.]+: ", "", msg)
    msg = re.sub(r"^(model|column|relationship) [\w.]+: ", "", msg)
    msg = re.sub(r"\b(model|column|class) [A-Z_]\w*(\.\w+)?\b", r"\1 …", msg)
    return msg[:100] or construction


def _route_at(app: App, where: str) -> str | None:
    m = re.search(r"main\.py:(\d+)", where)
    if not m:
        return None
    n = int(m.group(1))
    return next((ex for ex, (a, b) in app.spans.items() if a <= n <= b), None)


def _subset(app: App, pkgs: Path, keep: set[str]) -> Path:
    """The app with only the routes of `keep` (the others refused): `<id>_n`, same paths and line numbers."""
    src = (pkgs / app.id / "main.py").read_text().split("\n")
    for ex, (a, b) in app.spans.items():
        if ex not in keep:
            for n in range(a - 1, b):
                src[n] = ""
    d = pkgs / (app.id + "_n")
    d.mkdir(exist_ok=True)
    _write(d / "__init__.py", "")
    _write(d / "main.py", "\n".join(src))
    return d


def translate(apps: list[App], work: Path, stamp: str, jobs: int, log) -> tuple[dict, dict[str, Path]]:
    """`py2axum check` each app, then generate the routes it translates (refused routes left out; a refusal at
    generation takes its route out too). Returns {example key: result fields} for the refused and failing
    examples, and the generated crate of each app that has routes left."""
    import build

    pkgs = work / "pkgs"
    results: dict[str, dict] = {}
    gens: dict[str, Path] = {}

    def one(app: App):
        out: dict[str, dict] = {}
        full = build.check(pkgs / app.id, work / "checks", stamp)
        if "crash" in full:
            return {ex.id: {"status": "error", "error_kind": "check", "error": unstage(full["crash"], app, pkgs)}
                    for ex in app.examples}, None, set()
        refused: dict[str, list] = {}
        for g in full.get("global", []):
            for ex in app.examples:
                refused.setdefault(ex.id, []).append({"construction": g["construction"], "where": g["where"],
                                                      "error": g["error"]})
        for r in full.get("routes", []):
            ex = next((e for e, p in app.routes.items() if p == r.get("path")), None)
            if ex is None or r["status"] == "native":
                continue
            for label, wheres in (r.get("blockers") or {}).items():
                refused.setdefault(ex, []).append({"construction": label, "where": wheres[0] if wheres else r["where"],
                                                   "error": r.get("reason") or ""})
        keep = {ex.id for ex in app.examples} - set(refused)
        while keep:
            sub = _subset(app, pkgs, keep)
            err = build.generate(sub, work / "gen" / app.id, f"{stamp}-{build._tree_hash(sub)}")
            if not err:
                break
            m = re.search(r"error: (\S+?main\.py:\d+): (.*)", err)
            ex = _route_at(app, m.group(1)) if m and "Traceback" not in err else None
            if m and "Traceback" not in err and ex is None:
                # a refusal of the definitions that `check` did not foresee: every route is refused
                for e in keep:
                    refused[e] = [{"construction": "at generation: " + m.group(2).split(":")[0][:60],
                                   "where": m.group(1), "error": m.group(2)}]
                keep = set()
                break
            if ex is None or ex not in keep:
                for e in keep:
                    out[e] = {"status": "error", "error_kind": "generate", "error": unstage(err, app, pkgs)}
                keep = set()
                break
            refused[ex] = [{"construction": "at generation: " + m.group(2).split(":")[0][:60], "where": m.group(1),
                            "error": m.group(2)}]
            keep.discard(ex)
        for ex, reasons in refused.items():
            for r in reasons:
                r["where"], r["error"] = unstage(r["where"], app, pkgs), unstage(r["error"], app, pkgs)
                r["construction"] = _label(r["construction"], r["error"])
            seen, uniq = set(), []
            for r in reasons:
                if (r["construction"], r["where"]) not in seen:
                    seen.add((r["construction"], r["where"]))
                    uniq.append(r)
            out[ex] = {"status": "refused", "reason": uniq[0]["construction"], "reasons": uniq}
        return out, (work / "gen" / app.id if keep else None), keep

    for app, res in zip(apps, build.parallel(one, apps, jobs)):
        out, gen = res[0], res[1]
        for ex, r in out.items():
            results[f"{app.variant}:{app.unit}:{ex}"] = r
        if gen is not None:
            gens[app.id] = gen
            app.native = res[2]
    log(f"sqlalchemy: {len(gens)} apps generate, "
        f"{sum(r['status'] == 'refused' for r in results.values())} examples refused")
    return results, gens


class Db:
    """A worker slot's database: the app's schema, and the seed put back before every request."""

    def __init__(self, server: str, slot: int):
        import psycopg

        self.name = f"py2axum_sqla_s{slot}"
        self.plain = f"{server}/{self.name}"
        self.url = self.plain.replace("postgresql://", "postgresql+psycopg://", 1)
        with psycopg.connect(f"{server}/postgres", autocommit=True) as c:
            if not c.execute("SELECT 1 FROM pg_database WHERE datname = %s", (self.name,)).fetchone():
                c.execute(f'CREATE DATABASE "{self.name}"')
        self.conn = psycopg.connect(self.plain, autocommit=True)
        self.base: list[str] = []
        self.seed: dict = {}

    def setup(self, app_root: Path, module: str, seed: dict | None) -> str | None:
        """A fresh schema from the app's metadata; the error when Python cannot create it."""
        c = self.conn
        c.execute("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() "
                  "AND pid <> pg_backend_pid()")
        c.execute("DROP SCHEMA public CASCADE")
        c.execute("CREATE SCHEMA public")
        r = subprocess.run([sys.executable, "-c", SCHEMA, str(app_root), module, self.url], capture_output=True,
                           text=True, timeout=120, env={**__import__("os").environ, "DATABASE_URL": self.url})
        if r.returncode:
            return (r.stderr.strip().splitlines() or ["?"])[-1]
        self.base = [t for (t,) in c.execute("SELECT tablename FROM pg_tables WHERE schemaname = 'public'")]
        self.seed = seed or {"tables": {}, "sequences": []}
        self.reset()
        return None

    def reset(self) -> None:
        c = self.conn
        with c.transaction():
            c.execute("SET LOCAL lock_timeout = '10s'")
            extra = [t for (t,) in c.execute("SELECT tablename FROM pg_tables WHERE schemaname = 'public'")
                     if t not in self.base]
            if extra:
                c.execute("DROP TABLE " + ", ".join(f'"{t}"' for t in extra) + " CASCADE")
            if self.base:
                c.execute("TRUNCATE " + ", ".join(f'"{t}"' for t in self.base) + " RESTART IDENTITY CASCADE")
            c.execute("SET LOCAL session_replication_role = replica")
            for t, data in self.seed["tables"].items():
                if t in self.base and data["copy"]:
                    cols = ", ".join(f'"{x}"' for x in data["columns"])
                    with c.cursor().copy(f'COPY "{t}" ({cols}) FROM STDIN') as cp:
                        cp.write(data["copy"])
            for name, last in self.seed["sequences"]:
                if last is not None and c.execute("SELECT 1 FROM pg_sequences WHERE schemaname = 'public' "
                                                  "AND sequencename = %s", (name,)).fetchone():
                    c.execute("SELECT setval(%s, %s, true)", (f'"{name}"', last))

    def close(self) -> None:
        self.conn.close()


def _wait(port: int, proc, timeout: float = 60) -> bool:
    import socket
    import time

    end = time.time() + timeout
    while time.time() < end:
        if proc.poll() is not None:
            return False
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def _last_error(log_text: str) -> str:
    """The exception a uvicorn log ends with (the reason the reference failed)."""
    lines = [x for x in log_text.splitlines() if re.match(r"^[A-Za-z_][\w.]*(Error|Exception|Greenlet|Warning)\b", x)
             or re.match(r"^[a-z_.]+\.[A-Z]\w+: ", x)]
    return (lines[-1] if lines else (log_text.strip().splitlines() or ["?"])[-1])[:300]


def replay(app: App, work: Path, slot: int, binary: str, server: str) -> dict[str, dict]:
    """Every generated route of the app, on the reference and on the binary, the database reset before each."""
    import signal
    import time

    import httpx
    from replay import _conf

    out: dict[str, dict] = {}
    db = Db(server, slot)
    procs = []
    logs = work / "logs"
    logs.mkdir(exist_ok=True)
    try:
        err = db.setup(work, f"pkgs.{app.id}_n.main", app.seed)
        if err:
            return {ex: {"status": "reference-fails", "error": f"schema: {err}"} for ex in app.native}
        ref_port, cand_port = PORT_BASE + 2 * slot, PORT_BASE + 2 * slot + 1
        env = {**__import__("os").environ, "DATABASE_URL": db.url}
        ref_log, cand_log = logs / f"{app.id}.ref.log", logs / f"{app.id}.cand.log"
        ref = subprocess.Popen([sys.executable, "-m", "uvicorn", f"pkgs.{app.id}_n.main:app", "--port", str(ref_port),
                                "--log-level", "warning", "--no-access-log"], cwd=work, env={**env, "PYTHONPATH": str(work)},
                               stdout=open(ref_log, "w"), stderr=subprocess.STDOUT)
        procs.append(ref)
        cand = subprocess.Popen([str(work.parent / "target" / "release" / binary)], cwd=work,
                                env={**env, "CORPUS_APP": app.id, "PORT": str(cand_port), "HOST": "127.0.0.1",
                                     "DB_POOL_SIZE": "2"}, stdout=open(cand_log, "w"), stderr=subprocess.STDOUT)
        procs.append(cand)
        if not _wait(ref_port, ref):
            return {ex: {"status": "reference-fails", "error": "reference did not start: " + _last_error(ref_log.read_text())}
                    for ex in app.native}
        if not _wait(cand_port, cand):
            return {ex: {"status": "error", "error_kind": "startup",
                         "error": "binary did not start: " + cand_log.read_text()[-1500:]} for ex in app.native}
        urls = {"ref": f"http://127.0.0.1:{ref_port}", "cand": f"http://127.0.0.1:{cand_port}"}
        with httpx.Client(timeout=60) as client:
            for ex in [e for e in app.examples if e.id in app.native]:
                path = app.routes[ex.id]
                obs = {}
                for which in ("ref", "ref2", "cand"):
                    db.reset()
                    mark = len(ref_log.read_text()) if which == "ref" else 0
                    base = urls["ref" if which == "ref2" else which]
                    try:
                        r = client.post(base + path)
                        obs[which] = _conf.observe(r, base, None, "POST", path, set())
                    except httpx.TransportError as e:
                        obs[which] = {"req": f"POST {path}", "error": type(e).__name__}
                    if which == "ref" and obs["ref"].get("status") != 200:
                        time.sleep(0.2)
                        obs["ref_error"] = _last_error(ref_log.read_text()[mark:])
                        break
                if "ref_error" in obs:
                    out[ex.id] = {"status": "reference-fails", "error": obs["ref_error"]}
                    continue
                same = json.dumps(obs["ref"]) == json.dumps(obs["cand"])
                stable = json.dumps(obs["ref"]) == json.dumps(obs["ref2"])
                status = ("nondeterministic" if not stable or re.search(r" at 0x[0-9a-f]{6,}", json.dumps(obs["ref"]))
                          else "identical" if same else "differs")
                res = {"status": status, "compared": 1, "same": int(same), "n_diffs": int(not same)}
                if not same:
                    res["diffs"] = [{"test": path, "ref": obs["ref"], "cand": obs["cand"]}]
                out[ex.id] = res
        return out
    finally:
        for p in procs:
            if p.poll() is None:
                p.send_signal(signal.SIGTERM)
        for p in procs:
            try:
                p.wait(5)
            except subprocess.TimeoutExpired:
                p.kill()
        db.close()


def bench(work: Path, server: str, jobs: int, bundle: int, stamp: str, log, only: str | None = None) -> dict:
    """The whole SQLAlchemy corpus: {"version", "skipped", "titles", "examples": {key: result}}."""
    import build
    import sources
    from concurrent.futures import ThreadPoolExecutor

    repo = sources.fetch("sqlalchemy")
    work.mkdir(parents=True, exist_ok=True)
    for d in ("checks", "gen", "logs"):
        (work / d).mkdir(exist_ok=True)
    for d in ("logs", "bundles"):  # the shared crates live next to the other corpora's
        (work.parent / d).mkdir(exist_ok=True)
    import psycopg

    with psycopg.connect(f"{server}/postgres", autocommit=True) as c:
        if not c.execute("SELECT 1 FROM pg_database WHERE datname = 'py2axum_sqla_prep'").fetchone():
            c.execute('CREATE DATABASE "py2axum_sqla_prep"')
    prep = f"{server}/py2axum_sqla_prep".replace("postgresql://", "postgresql+psycopg://", 1)
    apps, skipped, titles = prepare(repo, work, prep, log, only)
    results, gens = translate(apps, work, stamp, jobs, log)
    binaries, failed = build.build_all(gens, work.parent, bundle, log, prefix="corpus_sqla") if gens else ({}, {})
    by_id = {a.id: a for a in apps}
    for app_id, err in failed.items():
        app = by_id[app_id]
        for ex in app.native:
            results[f"{app.variant}:{app.unit}:{ex}"] = {"status": "error", "error_kind": "build", "error": err}
    todo = [a for a in apps if a.id in binaries]
    slots = list(range(jobs))

    def unit(app):
        slot = slots.pop()
        try:
            return app, replay(app, work, slot, binaries[app.id], server)
        except Exception as e:  # noqa: BLE001 - a bench failure is recorded, the others go on
            return app, {ex: {"status": "error", "error_kind": "bench", "error": repr(e)} for ex in app.native}
        finally:
            slots.append(slot)

    with ThreadPoolExecutor(jobs) as pool:
        for app, res in pool.map(unit, todo):
            for ex, r in res.items():
                results[f"{app.variant}:{app.unit}:{ex}"] = r
    out = {}
    for app in apps:
        for ex in app.examples:
            key = f"{app.variant}:{app.unit}:{ex.id}"
            r = results.get(key, {"status": "error", "error_kind": "bench", "error": "not replayed"})
            out[f"sqla_{app.variant}__{ex.id}"] = {
                "module": f"{app.id}{app.routes[ex.id]}", "source": f"{ex.page}:{ex.line}", "page": ex.page,
                "title": titles.get(ex.page, ex.page), "section": ex.section, "kind": "sqlalchemy",
                "variant": app.variant, "tests": ["replay"], **r}
    # no machine path in the results (docs/coverage.md is public)
    roots = sorted({str(p) + "/" for p in (work, work.resolve(), repo, repo.resolve(), HERE.parent, Path(sys.prefix))},
                   key=len, reverse=True)

    def relative(v):
        if isinstance(v, str):
            for r in roots:
                v = v.replace(r, "")
            return v
        if isinstance(v, list):
            return [relative(x) for x in v]
        if isinstance(v, dict):
            return {k: relative(x) for k, x in v.items()}
        return v

    return {"version": sources.version("sqlalchemy"), "skipped": skipped, "examples": relative(out)}


def main() -> int:
    """Run the SQLAlchemy corpus alone: <work>/sqla/results.json (corpus/run.py runs it with the others)."""
    import argparse
    import os
    import time

    sys.path.insert(0, str(HERE))
    sys.path.insert(0, str(HERE.parent))
    import build

    ap = argparse.ArgumentParser(description=main.__doc__)
    ap.add_argument("--work", type=Path, default=HERE / "out")
    ap.add_argument("--only", default=None, help="only the units whose name contains this text")
    ap.add_argument("--jobs", type=int, default=max(2, min(8, (os.cpu_count() or 4) - 2)))
    ap.add_argument("--bundle", type=int, default=12, help="apps per shared crate")
    args = ap.parse_args()
    t0 = time.time()
    db = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_corpus")
    server = db.rsplit("/", 1)[0].replace("postgresql+psycopg://", "postgresql://", 1)
    log = lambda m: print(m, file=sys.stderr, flush=True)  # noqa: E731
    data = bench(args.work.resolve() / "sqla", server, args.jobs, args.bundle, build.py2axum_hash(), log, args.only)
    data["seconds"] = round(time.time() - t0, 1)
    (args.work / "sqla" / "results.json").write_text(json.dumps(data, ensure_ascii=False, indent=1))
    counts: dict[str, int] = {}
    for r in data["examples"].values():
        counts[r["status"]] = counts.get(r["status"], 0) + 1
    log(f"{counts}  {data['seconds']} s")
    return 0


if __name__ == "__main__":
    sys.path.insert(0, str(HERE))
    sys.exit(main())

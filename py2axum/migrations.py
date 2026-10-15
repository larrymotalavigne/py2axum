"""Alembic migrations applied by the binary itself (`<binary> migrate`), without Python at run time.

At translation time, the project's alembic configuration (`alembic.ini` at the package or above, up to --root)
is read and every revision is rendered by alembic's own offline mode (`alembic upgrade head --sql`: a
MigrationContext on the PostgreSQL dialect writing to a buffer), one revision after the other, so the
statements are those alembic writes by construction. env.py is not run: its `context.configure(...)` options
that change the SQL (`version_table`, `version_table_schema`, `transaction_per_migration`) are read from its
source, as literals. The binary then runs, from the revision the database is at, the statements of each
revision, the version table's updates included, in alembic's transactions.

A revision that offline mode cannot render faithfully is refused, with its file:line, and the binary refuses to
apply it (the revisions before it still apply; a database already past it never needs it): one that reads the
database (`op.get_bind()`, `inspect()`, `op.get_context().bind`...), or imports anything but alembic, SQLAlchemy and
a few standard modules (its other imports would run project code at translation time).
"""
from __future__ import annotations

import ast
import configparser
import functools
import io
import logging
import shutil
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

from .ir import TranspileError


def literal_differs() -> tuple:
    """The types whose literal (offline) differs from their bound value (online). SQLAlchemy and alembic are imported
    only for a project with an alembic.ini: every generation imports this module (dyn.migrations_item)."""
    import sqlalchemy
    from sqlalchemy.dialects import postgresql

    return (sqlalchemy.JSON, postgresql.JSONB, sqlalchemy.ARRAY, sqlalchemy.PickleType, sqlalchemy.LargeBinary,
            sqlalchemy.Interval, sqlalchemy.types.TypeDecorator)

# what a revision may import: the rest would run the project at translation time
ALLOWED_IMPORTS = ("alembic", "sqlalchemy", "typing", "typing_extensions", "__future__", "datetime", "enum", "uuid",
                   "decimal", "json", "re", "collections", "textwrap", "string", "math")
# calls that read the database: offline mode would render something else than what runs online
READS_DB = {"get_bind", "inspect", "run_async", "get_x_argument", "reflect", "has_table", "get_table_names",
            "get_columns", "get_indexes"}
# env.py's context.configure() options that change the statements or their transactions
ENV_OPTIONS = {"version_table": str, "version_table_schema": (str, type(None)), "transaction_per_migration": bool,
               "transactional_ddl": bool}


@dataclass
class Step:
    revision: str
    log: str  # what alembic logs: `upgrade a -> b, message`
    heads_after: tuple[str, ...]
    stmts: list[str] = field(default_factory=list)
    refused: str | None = None  # "file:line: why"


@dataclass
class Plan:
    ini: str
    version_table: str
    version_schema: str | None
    per_migration: bool
    create_version: str
    steps: list[Step]
    sql: str  # the whole `upgrade head --sql` output
    logging: dict | None  # alembic.ini's logging sections, as a dictConfig


def find_ini(package: Path, root: Path) -> Path | None:
    d = package.resolve()
    root = root.resolve()
    while True:
        if (d / "alembic.ini").is_file():
            return d / "alembic.ini"
        if d == root or d.parent == d:
            return None
        d = d.parent


def _env_options(env: Path) -> dict:
    """The literal keyword arguments of env.py's `context.configure(...)` calls that change the statements."""
    opts: dict = {}
    if not env.is_file():
        return opts
    tree = ast.parse(env.read_text(), str(env))
    for node in ast.walk(tree):
        if not (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == "configure"):
            continue
        for kw in node.keywords:
            if kw.arg not in ENV_OPTIONS:
                continue
            try:
                value = ast.literal_eval(kw.value)
            except ValueError:
                raise TranspileError(f"env.py: context.configure({kw.arg}=...) must be a literal for the migrations to be "
                                     "translated", kw, str(env)) from None
            if not isinstance(value, ENV_OPTIONS[kw.arg]):
                raise TranspileError(f"env.py: context.configure({kw.arg}={value!r}) is not supported", kw, str(env))
            if kw.arg in opts and opts[kw.arg] != value:
                raise TranspileError(f"env.py: context.configure({kw.arg}=) differs between the online and offline "
                                     "configurations", kw, str(env))
            opts[kw.arg] = value
    if opts.get("transactional_ddl") is False:
        raise TranspileError("env.py: context.configure(transactional_ddl=False) is not supported", None, str(env))
    return opts


def _refusal(path: Path) -> str | None:
    """Why a revision cannot be rendered offline (`file:line: why`), or None."""
    src = path.read_text()
    tree = ast.parse(src, str(path))
    for node in tree.body:
        if isinstance(node, (ast.Import, ast.ImportFrom)):
            names = [a.name for a in node.names] if isinstance(node, ast.Import) else [node.module or ""]
            for n in names:
                if node.__class__ is ast.ImportFrom and node.level:
                    n = "." + n
                if n.split(".")[0] not in ALLOWED_IMPORTS:
                    return f"{path}:{node.lineno}: a revision importing `{n}` (only alembic, SQLAlchemy and standard modules)"
    # upgrade() and the helpers it may call (downgrade() never runs)
    for fn in tree.body:
        if not isinstance(fn, (ast.FunctionDef, ast.AsyncFunctionDef)) or fn.name == "downgrade":
            continue
        for node in ast.walk(fn):
            name = None
            if isinstance(node, ast.Call):
                f = node.func
                name = f.attr if isinstance(f, ast.Attribute) else f.id if isinstance(f, ast.Name) else None
            elif isinstance(node, ast.Attribute) and node.attr == "bind":
                name = "bind"
            if name in READS_DB or name == "bind":
                return (f"{path}:{node.lineno}: `{ast.unparse(node)[:60]}` reads the database: alembic's offline "
                        "mode cannot render this revision")
    return None


def _stub(path: Path, why: str) -> str:
    """A revision file reduced to its identifiers and docstring (never executed as written)."""
    tree = ast.parse(path.read_text(), str(path))
    keep = []
    if tree.body and isinstance(tree.body[0], ast.Expr) and isinstance(getattr(tree.body[0], "value", None), ast.Constant):
        keep.append(tree.body[0])
    for node in tree.body:
        targets = [t.id for t in getattr(node, "targets", []) if isinstance(t, ast.Name)]
        if isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
            targets = [node.target.id]
        if set(targets) & {"revision", "down_revision", "branch_labels", "depends_on"}:
            value = node.value
            try:
                ast.literal_eval(value)
            except ValueError:
                raise TranspileError(f"{path}: revision identifiers must be literals", node, str(path)) from None
            keep.append(ast.Assign(targets=[ast.Name(targets[0], ast.Store())], value=value, lineno=0))
    body = ast.unparse(ast.Module(body=keep, type_ignores=[]))
    del why
    return body + "\n\n\ndef upgrade():\n    pass\n\n\ndef downgrade():\n    pass\n"


def _logging(cp: configparser.ConfigParser, ini: Path) -> dict | None:
    """alembic.ini's [loggers]/[handlers]/[formatters] (what env.py's `fileConfig` installs) as a dictConfig;
    None when it has none. Only StreamHandlers on sys.stderr/sys.stdout are translated."""
    if not cp.has_section("loggers"):
        return None
    out: dict = {"version": 1, "disable_existing_loggers": True, "formatters": {}, "handlers": {}, "loggers": {}}
    names = lambda sec, key: [x.strip() for x in cp.get(sec, key, fallback="").split(",") if x.strip()]  # noqa: E731
    for f in names("formatters", "keys"):
        sec = f"formatter_{f}"
        out["formatters"][f] = {"format": cp.get(sec, "format", raw=True, fallback=None),
                                "datefmt": cp.get(sec, "datefmt", raw=True, fallback=None)}
    for h in names("handlers", "keys"):
        sec = f"handler_{h}"
        cls = cp.get(sec, "class", fallback="")
        args = cp.get(sec, "args", fallback="()").replace(" ", "")
        if cls not in ("StreamHandler", "logging.StreamHandler") or args not in ("(sys.stderr,)", "(sys.stdout,)", "()"):
            raise TranspileError(f"alembic.ini: handler {h} ({cls}{args}) is not supported for the migrations' logging",
                                 None, str(ini))
        hd = {"class": "logging.StreamHandler", "stream": "ext://sys.stdout" if "stdout" in args else "ext://sys.stderr",
              "level": cp.get(sec, "level", fallback="NOTSET")}
        if cp.get(sec, "formatter", fallback=""):
            hd["formatter"] = cp.get(sec, "formatter")
        out["handlers"][h] = hd
    for lg in names("loggers", "keys"):
        sec = f"logger_{lg}"
        entry = {"level": cp.get(sec, "level", fallback="NOTSET"), "handlers": names(sec, "handlers"),
                 "propagate": cp.get(sec, "propagate", fallback="1") not in ("0", "False", "false")}
        if lg == "root":
            entry.pop("propagate")
            out["root"] = entry
        else:
            out["loggers"][cp.get(sec, "qualname", fallback=lg)] = entry
    return out


def collect(package: Path, root: Path) -> Plan | None:
    """The migrations of the project, rendered; None without an alembic.ini."""
    ini = find_ini(package, root)
    if ini is None:
        return None
    try:
        from alembic.operations import Operations
        from alembic.runtime.migration import MigrationContext
        from alembic.script import ScriptDirectory
    except ImportError:
        raise TranspileError(f"{ini}: translating the migrations needs alembic next to py2axum (pip install alembic)",
                             None, str(ini)) from None
    differs = literal_differs()
    cp = configparser.ConfigParser(defaults={"here": str(ini.parent)})
    cp.read(ini)
    loc = cp.get("alembic", "script_location", fallback=None)
    if loc is None:
        return None
    script_dir = Path(loc) if Path(loc).is_absolute() else ini.parent / loc
    if not (script_dir / "versions").is_dir():
        raise TranspileError(f"alembic.ini: script_location {loc!r} has no versions/ directory", None, str(ini))
    if cp.get("alembic", "version_locations", fallback=None):
        raise TranspileError("alembic.ini: version_locations is not supported for the migrations", None, str(ini))
    env = _env_options(script_dir / "env.py")
    refused: dict[str, str] = {}
    with tempfile.TemporaryDirectory() as tmp:
        tdir = Path(tmp) / "scripts"
        (tdir / "versions").mkdir(parents=True)
        for f in sorted((script_dir / "versions").glob("*.py")):
            why = _refusal(f)
            if why is None:
                shutil.copy(f, tdir / "versions" / f.name)
            else:
                (tdir / "versions" / f.name).write_text(_stub(f, why))
                refused[f.name] = why
        script = ScriptDirectory(str(tdir))
        out: list[str] = []
        events: list[tuple[int, str, tuple[str, ...]]] = []

        def on_apply(ctx, step, heads, run_args):
            events.append((len(out), step.up_revision_id, tuple(sorted(heads))))

        # what alembic logs for each revision (`Running upgrade a -> b, message`): str() of its step
        running: list[str] = []

        class Logged(logging.Handler):
            def emit(self, record):
                if record.msg == "Running %s":
                    running.append(record.getMessage()[len("Running "):])
        mlog = logging.getLogger("alembic.runtime.migration")
        handler, level = Logged(), mlog.level
        mlog.addHandler(handler)
        mlog.setLevel(logging.INFO)
        buf = io.StringIO()
        ctx = MigrationContext.configure(
            dialect_name="postgresql",
            opts={"as_sql": True, "output_buffer": buf, "literal_binds": True,
                  "fn": lambda rev, _ctx: script._upgrade_revs("head", rev),
                  "transaction_per_migration": env.get("transaction_per_migration", False),
                  "version_table": env.get("version_table", "alembic_version"),
                  "version_table_schema": env.get("version_table_schema"),
                  "on_version_apply": [on_apply]})
        failed: dict[str, str] = {}

        def guarded(revs):
            # a revision whose offline rendering raises (`bulk_insert` of a JSON value...) is refused, not fatal
            for st in revs:
                fn, path = st.migration_fn, Path(st.revision.path)

                def run(*a, _fn=fn, _path=path, **k):
                    current[0] = _path
                    try:
                        return _fn(*a, **k)
                    except Exception as e:  # noqa: BLE001 - alembic's or SQLAlchemy's own error, reported as is
                        failed.setdefault(_path.name, f"{_path}: alembic's offline mode fails on it ({type(e).__name__}: {e})")
                # alembic names the step after the function (`upgrade a -> b`)
                functools.update_wrapper(run, fn)
                st.migration_fn = run
                yield st
        ctx._migrations_fn = (lambda f: lambda heads, c: guarded(f(heads, c)))(ctx._migrations_fn)
        current: list = [None]
        bulk = ctx.impl.bulk_insert

        def checked_bulk_insert(table, rows, multiinsert=True):
            # offline, the rows are written as literals; online they are bound: the types whose bound value differs from
            # their literal (JSON's None is the JSON null online, SQL NULL offline...) make the revision unrenderable
            for col in getattr(table, "c", []):
                t = col.type
                if isinstance(t, differs):
                    failed.setdefault(current[0].name, f"{current[0]}: op.bulk_insert into {table.name}.{col.name} "
                                      f"({type(t).__name__}): alembic's offline mode writes its values differently "
                                      "from an online run")
            return bulk(table, rows, multiinsert=multiinsert)
        ctx.impl.bulk_insert = checked_bulk_insert
        written = ctx.impl.static_output

        def capture(text: str) -> None:
            out.append(text)
            written(text)
        ctx.impl.static_output = capture
        try:
            with Operations.context(ctx):
                with ctx.begin_transaction():
                    ctx.run_migrations()
        finally:
            mlog.removeHandler(handler)
            mlog.setLevel(level)
        sql = buf.getvalue()
        revmap = {s.revision: s for s in script.walk_revisions()}
    vt = env.get("version_table", "alembic_version")
    starts = [i for i, t in enumerate(out) if t.startswith("-- Running ")]
    create = next((t for t in out[:starts[0]] if t.startswith("CREATE TABLE ")), "") if starts else ""
    steps: list[Step] = []
    # a revision: from alembic's `-- Running upgrade a -> b, message` comment to its version stamp (on_version_apply)
    for begin, (end, rev, heads), log in zip(starts, events, running):
        sc = revmap[rev]
        name = Path(sc.path).name
        steps.append(Step(rev, log, heads, out[begin + 1:end], refused.get(name) or failed.get(name)))
    return Plan(str(ini), vt, env.get("version_table_schema"), env.get("transaction_per_migration", False), create, steps,
                sql, _logging(cp, ini))

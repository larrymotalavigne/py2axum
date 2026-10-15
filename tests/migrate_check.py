"""`<binary> migrate` against alembic itself, on the project's own migrations (py2axum/migrations.py).

    python tests/migrate_check.py PROJECT_DIR BINARY POSTGRES_URL [--from REV[,REV...]] [--to REV]

(PG_DUMP: the pg_dump of the server's major version.) PROJECT_DIR holds the project's alembic.ini (alembic runs there, with this interpreter: the project's
dependencies must be importable, as for its conformance). POSTGRES_URL is a server URL whose database part is
replaced by two scratch databases, created and dropped here. Checked:
1. `<binary> migrate --sql` is `alembic upgrade head --sql` (when env.py's offline mode writes plain SQL);
2. from an empty database (or from REV[,REV...], reached with alembic on both sides: a revision the binary refuses comes
   before it), `alembic upgrade head` on one database and `<binary> migrate` on the other leave the same schema
   (`pg_dump --schema-only`), the same rows (`pg_dump --data-only`) and the same `alembic_version`, and write
   the same alembic log lines;
3. a second `<binary> migrate` on the migrated database does nothing (and says nothing but alembic's first lines).
Exit status 0 when everything is identical.
"""
from __future__ import annotations

import difflib
import os
import re
import subprocess
import sys
from urllib.parse import urlsplit, urlunsplit


def with_db(url: str, name: str) -> str:
    u = urlsplit(url)
    return urlunsplit((u.scheme, u.netloc, "/" + name, u.query, u.fragment))


def run(cmd: list[str], cwd: str, url: str) -> subprocess.CompletedProcess:
    env = dict(os.environ, DATABASE_URL=url)
    return subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True)


def plain(url: str) -> str:
    """the URL without SQLAlchemy's driver (`postgresql+psycopg://`), for psql and pg_dump"""
    return re.sub(r"^postgresql\+\w+://", "postgresql://", url)


def psql(url: str, sql: str) -> str:
    return subprocess.run(["psql", plain(url), "-Atc", sql], capture_output=True, text=True, check=True).stdout


def dump(url: str, what: str) -> list[str]:
    out = subprocess.run([os.environ.get("PG_DUMP", "pg_dump"), "--no-owner", "--no-privileges", f"--{what}", plain(url)], capture_output=True, text=True,
                         check=True).stdout
    # pg_dump's per-run restrict key (PostgreSQL 17.6+) changes each time
    return [line for line in out.splitlines() if not line.startswith(("\\restrict", "\\unrestrict", "--"))]


def diff(a: list[str], b: list[str], label: str) -> bool:
    if a == b:
        print(f"ok   {label}")
        return True
    print(f"DIFF {label}")
    for line in list(difflib.unified_diff(a, b, "alembic", "binary", lineterm=""))[:60]:
        print("     " + line)
    return False


TIMESTAMP = re.compile(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(\.\d+)?([+-]\d{2}(:\d{2})?)?")


# a password hashed by a revision (alembic on both sides: a random salt)
BCRYPT = re.compile(r"\$2[aby]\$\d\d\$[./A-Za-z0-9]{53}")


def rows(url: str) -> list[str]:
    """the rows, with the instants the migrations stamp (`now()` defaults) masked: the two runs differ by seconds"""
    return [BCRYPT.sub("<bcrypt>", TIMESTAMP.sub("<now>", line)) for line in dump(url, "data-only")]


def log_lines(stderr: str) -> list[str]:
    """alembic's log lines (the INFO/WARNING records of alembic.runtime.migration)"""
    return [line for line in stderr.splitlines() if "[alembic." in line]


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    start = sys.argv[sys.argv.index("--from") + 1] if "--from" in sys.argv else None
    target = sys.argv[sys.argv.index("--to") + 1] if "--to" in sys.argv else "head"
    for opt in (start, target):
        if opt in args:
            args.remove(opt)
    project, binary, server = args
    binary = os.path.abspath(binary)
    # ALEMBIC: the command running the project's alembic (default: this interpreter's)
    alembic = os.environ["ALEMBIC"].split() if os.environ.get("ALEMBIC") else [sys.executable, "-m", "alembic"]
    ok = True
    py_sql = run(alembic + ["upgrade", "head", "--sql"], project, with_db(server, "postgres"))
    rs_sql = run([binary, "migrate", "--sql"], project, with_db(server, "postgres"))
    if py_sql.returncode == 0:
        ok &= diff(py_sql.stdout.splitlines(), rs_sql.stdout.splitlines(), "upgrade head --sql")
    else:
        print(f"skip upgrade head --sql (alembic's offline mode failed: {py_sql.stderr.strip().splitlines()[-1:]})")
    names = ("py2axum_mig_ref", "py2axum_mig_cand")
    admin = with_db(server, "postgres")
    for n in names:
        psql(admin, f"DROP DATABASE IF EXISTS {n}")
        psql(admin, f"CREATE DATABASE {n}")
    ref, cand = (with_db(server, n) for n in names)
    try:
        if start:
            # REV,REV,...: applied one after the other with alembic on both sides (the binary's order of a branched
            # history: `migrate` must then find the database on its upgrade path)
            for rev in start.split(","):
                for u in (ref, cand):
                    r = run(alembic + ["upgrade", rev], project, u)
                    if r.returncode:
                        print(r.stderr[-2000:])
                        return 1
        a = run(alembic + ["upgrade", target], project, ref)
        b = run([binary, "migrate", "upgrade", target], project, cand)
        if a.returncode or b.returncode:
            print(f"alembic exit {a.returncode}: {a.stderr[-1500:]}\nbinary exit {b.returncode}: {b.stderr[-1500:]}")
            return 1
        ok &= diff(log_lines(a.stderr), log_lines(b.stderr), "logs of upgrade head")
        ok &= diff(dump(ref, "schema-only"), dump(cand, "schema-only"), "schema")
        ok &= diff(rows(ref), rows(cand), "rows")
        again = run([binary, "migrate", "upgrade", target], project, cand)
        ok &= diff(log_lines(run(alembic + ["upgrade", target], project, ref).stderr), log_lines(again.stderr),
                   "second upgrade (nothing to do)")
        ok &= diff(["exit 0"], [f"exit {again.returncode}"], "second upgrade exit status")
        cur_a = run(alembic + ["current"], project, ref)
        cur_b = run([binary, "migrate", "current"], project, cand)
        ok &= diff([re.sub(r"\s+$", "", x) for x in cur_a.stdout.splitlines()], cur_b.stdout.splitlines(), "current")
    finally:
        for n in names:
            psql(admin, f"DROP DATABASE IF EXISTS {n}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())

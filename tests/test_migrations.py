"""py2axum/migrations.py: alembic revisions rendered for `<binary> migrate`, refusals with file:line (no database:
tests/migrate_check.py compares the binary with alembic itself)."""
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

from py2axum import migrations
from py2axum.ir import TranspileError

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "fixtures" / "migrapp"


def project(tmp_path: Path, extra: dict[str, str] | None = None, env: str | None = None) -> Path:
    root = tmp_path / "proj"
    shutil.copytree(FIXTURE, root)
    for name, text in (extra or {}).items():
        (root / "migrations" / "versions" / name).write_text(text)
    if env is not None:
        (root / "migrations" / "env.py").write_text(env)
    return root


def test_fixture_rendered(tmp_path):
    root = project(tmp_path)
    plan = migrations.collect(root / "app", root)
    assert [s.revision for s in plan.steps] == ["0001", "0002", "0003", "0004", "0005"]
    assert plan.version_table == "schema_version" and not plan.per_migration
    assert plan.create_version.startswith("CREATE TABLE schema_version")
    first = plan.steps[0]
    assert first.log == "upgrade  -> 0001, tables, an enum created with its table, indexes, seed rows"
    assert first.stmts[0].startswith("CREATE TYPE status AS ENUM ('open', 'closed')")
    assert first.stmts[-1] == "INSERT INTO schema_version (version_num) VALUES ('0001') RETURNING schema_version.version_num;"
    # autocommit_block: alembic's own COMMIT / BEGIN around the statements
    assert plan.steps[2].stmts[:2] == ["COMMIT;", "ALTER TYPE status ADD VALUE IF NOT EXISTS 'archived';"]
    assert plan.steps[3].refused.endswith("`op.get_bind()` reads the database: alembic's offline mode cannot render this revision")
    assert ":14:" in plan.steps[3].refused and plan.steps[4].refused is None
    assert plan.logging["loggers"]["alembic"] == {"level": "INFO", "handlers": [], "propagate": True}
    assert plan.sql.startswith("BEGIN;\n\nCREATE TABLE schema_version")


def test_no_alembic_ini(tmp_path):
    (tmp_path / "app").mkdir()
    assert migrations.collect(tmp_path / "app", tmp_path) is None


@pytest.mark.parametrize("text, why", [
    ('revision = "0006"\ndown_revision = "0005"\nimport bcrypt\n\n\ndef upgrade():\n    pass\n',
     "0006_x.py:3: a revision importing `bcrypt`"),
    ('import sqlalchemy as sa\nfrom alembic import op\nrevision = "0006"\ndown_revision = "0005"\n\n\ndef upgrade():\n'
     '    t = sa.table("t", sa.column("d", sa.JSON))\n    op.bulk_insert(t, [{"d": None}])\n',
     "op.bulk_insert into t.d (JSON)"),
    ('import sqlalchemy as sa\nfrom alembic import op\nrevision = "0006"\ndown_revision = "0005"\n\n\ndef upgrade():\n'
     '    if op.get_context().bind.dialect.name == "postgresql":\n        pass\n',
     "0006_x.py:8: `op.get_context().bind` reads the database"),
    ('import sqlalchemy as sa\nfrom alembic import op\nrevision = "0006"\ndown_revision = "0005"\n\n\ndef upgrade():\n'
     '    t = sa.table("t", sa.column("d", sa.Integer))\n    op.execute(t.update().values(d=sa.func.now() + object()))\n',
     "alembic's offline mode fails on it"),
])
def test_revision_refused(tmp_path, text, why):
    root = project(tmp_path, {"0006_x.py": text})
    plan = migrations.collect(root / "app", root)
    assert plan.steps[-1].revision == "0006"
    assert why in plan.steps[-1].refused
    assert plan.steps[-2].refused is None  # the others are rendered


def test_env_option_must_be_literal(tmp_path):
    env = (FIXTURE / "migrations" / "env.py").read_text().replace(
        'version_table="schema_version")\n        with context.begin_transaction():\n            context.run_migrations()\n\n\nif',
        'version_table=TABLE)\n        with context.begin_transaction():\n            context.run_migrations()\n\n\nif')
    root = project(tmp_path, env=env)
    with pytest.raises(TranspileError, match=r"context.configure\(version_table=...\) must be a literal"):
        migrations.collect(root / "app", root)


def test_transaction_per_migration(tmp_path):
    env = (FIXTURE / "migrations" / "env.py").read_text().replace(
        'version_table="schema_version")', 'version_table="schema_version", transaction_per_migration=True)')
    root = project(tmp_path, env=env)
    plan = migrations.collect(root / "app", root)
    assert plan.per_migration
    # each revision in its own transaction: BEGIN/COMMIT around it in the whole script
    assert plan.sql.count("BEGIN;") == 5 + 1  # each revision, and the autocommit block's restart


def test_generation_without_sqlalchemy_nor_alembic(tmp_path):
    """every generation imports py2axum.migrations: an app without SQLAlchemy (and without alembic) in the
    translator's environment must still generate (0.8.0 imported SQLAlchemy at the top of the module)"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get('/x')\n"
                                 "async def x():\n    return {'a': 1}\n")
    # None in sys.modules: `import sqlalchemy` raises ImportError, as when it is not installed
    code = ("import sys\nfor m in ('sqlalchemy', 'alembic'):\n    sys.modules[m] = None\n"
            "from py2axum.__main__ import main\nsys.exit(main(sys.argv[1:]))")
    r = subprocess.run([sys.executable, "-c", code, str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")],
                       cwd=ROOT, capture_output=True, text=True)
    assert r.returncode == 0, r.stderr[-3000:]
    assert "MIGRATIONS: Option<&'static" in (tmp_path / "out" / "src" / "gen.rs").read_text()

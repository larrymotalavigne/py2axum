"""SQLModel rewritten at load (py2axum/sqlmodel_rewrite.py): the tables' DDL is SQLModel's, what is left as written
is refused by the translator with file:line."""
import ast
import sys
import textwrap
import types

import pytest

from py2axum.__main__ import main
from py2axum.sqlmodel_rewrite import rewrite

MODELS = '''
import uuid
from datetime import date
from decimal import Decimal

from sqlmodel import Field, SQLModel


class TeamBase(SQLModel):
    name: str = Field(index=True)
    headquarters: str = Field(max_length=40)


class Team(TeamBase, table=True):
    id: int | None = Field(default=None, primary_key=True)


class Hero(SQLModel, table=True):
    id: int | None = Field(default=None, primary_key=True)
    name: str = Field(index=True, unique=True)
    secret_name: str
    age: int | None = Field(default=None, index=True)
    power: float = 1.5
    active: bool = True
    born: date | None = None
    seen: str | None = Field(default=None, nullable=True)
    code: uuid.UUID | None = None
    price: Decimal = Field(default=Decimal("1.5"), max_digits=8, decimal_places=2)
    team_id: int | None = Field(default=None, foreign_key="team.id", ondelete="SET NULL")
    blob: bytes | None = None
'''


def _ddl(md) -> list[str]:
    from sqlalchemy import create_mock_engine

    out = []
    engine = create_mock_engine("postgresql+psycopg://", lambda sql, *a, **k: out.append(str(sql.compile(dialect=engine.dialect)).strip()))
    md.create_all(engine, checkfirst=False)
    return out


_REAL = """
import json, sys
from sqlalchemy import create_mock_engine
import sqlmodel
exec(sys.stdin.read())
out = []
engine = create_mock_engine("postgresql+psycopg://", lambda sql, *a, **k: out.append(str(sql.compile(dialect=engine.dialect)).strip()))
sqlmodel.SQLModel.metadata.create_all(engine, checkfirst=False)
print(json.dumps(out))
"""


def test_tables_ddl_is_sqlmodels():
    """the DDL of the rewritten classes, against SQLModel's own (in a process of its own: SQLModel's metadata is
    global)"""
    import json
    import subprocess

    pytest.importorskip("sqlmodel")
    run = subprocess.run([sys.executable, "-c", _REAL], input=MODELS, capture_output=True, text=True, check=True)
    expected = json.loads(run.stdout)
    # the rewritten module, its `SQLModel` the shim's declarative base
    from sqlalchemy.orm import DeclarativeBase

    class SQLModel(DeclarativeBase):
        pass

    fake = types.ModuleType("sqlmodel")
    fake.SQLModel = SQLModel
    from pydantic import Field
    fake.Field = Field
    tree = rewrite(ast.parse(MODELS))
    saved = sys.modules.get("sqlmodel")
    sys.modules["sqlmodel"] = fake
    try:
        mod = types.ModuleType("rewritten")
        exec(compile(tree, "rewritten", "exec"), mod.__dict__)
    finally:
        sys.modules["sqlmodel"] = saved
    # tables in dependency order; a table's indexes come from a set in SQLAlchemy (their order varies per process)
    assert [x for x in _ddl(SQLModel.metadata) if x.startswith("CREATE TABLE")] == [x for x in expected if x.startswith("CREATE TABLE")]
    assert sorted(_ddl(SQLModel.metadata)) == sorted(expected)


APP = '''
from typing import Annotated

from fastapi import Depends, FastAPI
from sqlmodel import Field, Session, SQLModel, create_engine, select
{imports}

{defs}

engine = create_engine("postgresql+psycopg://x/y")


def get_session():
    with Session(engine) as session:
        yield session


app = FastAPI()


@app.get("/x")
def x(session: Annotated[Session, Depends(get_session)]):
{body}
'''


def _refused(tmp_path, capsys, imports="", defs="", body="    return 1", other=None) -> str:
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(APP.format(imports=imports, defs=textwrap.dedent(defs), body=body))
    if other:
        (pkg / "models.py").write_text(textwrap.dedent(other))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    return capsys.readouterr().err


T = "class T(SQLModel, table=True):\n    id: int | None = Field(default=None, primary_key=True)\n"


@pytest.mark.parametrize("imports, defs, body, msg", [
    ("from sqlmodel import Relationship", T + "    others: list['T'] = Relationship()\n", "    return session.get(T, 1).id", "sqlmodel"),
    ("from sqlalchemy import Column, String", T + "    name: str = Field(sa_column=Column('label', String))\n", "    return session.get(T, 1).id",
     "annotate with Mapped"),
    ("", T + "\n    def label(self):\n        return 'x'\n", "    return session.get(T, 1).id", "annotate with Mapped"),
    ("", T + "    tags: list[str] = []\n", "    return session.get(T, 1).id", "annotate with Mapped"),
    # a statement given two widths (one entity, then two columns): scalars or rows cannot be told apart
    ("", T, "    statement = select(T)\n    statement = select(T.id, T.id)\n    return session.exec(statement).all()", "exec"),
])
def test_sqlmodel_outside_subset_rejected(tmp_path, capsys, imports, defs, body, msg):
    err = _refused(tmp_path, capsys, imports, defs, body)
    assert msg in err, err
    assert "main.py:" in err, err


def test_table_model_from_another_module(tmp_path, capsys):
    """a table class imported by the routes of another module: its view and helpers are imported with it (body
    turned into the table's instance, return type dumped by the view), as in its own module"""
    other = '''
        from sqlmodel import Field, SQLModel


        class T(SQLModel, table=True):
            id: int | None = Field(default=None, primary_key=True)
    '''
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(other))
    (pkg / "main.py").write_text("from fastapi import FastAPI\n\nfrom .models import T\n\napp = FastAPI()\n\n\n"
                                 "@app.post('/x')\ndef x(t: T) -> T:\n    return t\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0, capsys.readouterr().err


DATED = '''
from datetime import datetime

from pydantic import EmailStr
from sqlmodel import Field, SQLModel


class Account(SQLModel, table=True):
    id: int | None = Field(default=None, primary_key=True)
    email: EmailStr = Field(index=True, max_length=80)
    created_at: datetime = Field(default_factory=datetime.utcnow)
'''


@pytest.mark.parametrize("version, column", [((0, 0, 44), "_p2a_sm_sa.DateTime"), ((0, 0, 24), "_p2a_sm_sa.DateTime"),
                                             ((0, 0, 45), "_p2a_sm_sqltypes.UTCDateTime()")])
def test_datetime_column_follows_the_locked_sqlmodel(version, column):
    """SQLModel's get_sqlalchemy_type: datetime -> DateTime (naive) before 0.0.45, its UTCDateTime decorator since
    (TIMESTAMPTZ, read in UTC); EmailStr -> AutoString(max_length)"""
    out = ast.unparse(rewrite(ast.parse(DATED), version))
    assert f"created_at: _p2a_sm_orm.Mapped[datetime] = _p2a_sm_orm.mapped_column({column}," in out
    assert "email: _p2a_sm_orm.Mapped[EmailStr] = _p2a_sm_orm.mapped_column(_p2a_sm_sa.String(80)," in out


def test_base_from_another_module_reading_a_name_bound_otherwise(tmp_path, capsys):
    """a table whose SQLModel base is in another module: its fields are taken only when every name they read means
    the same here (here `Kind` is another class): else the table is left as written (refused)"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "base.py").write_text("from enum import Enum\n\nfrom sqlmodel import SQLModel\n\n\nclass Kind(str, Enum):\n"
                                 "    a = 'a'\n\n\nclass B(SQLModel):\n    kind: Kind = Kind.a\n")
    (pkg / "main.py").write_text("from enum import Enum\n\nfrom fastapi import FastAPI\nfrom sqlmodel import Field\n\n"
                                 "from .base import B\n\n\nclass Kind(str, Enum):\n    z = 'z'\n\n\n"
                                 "class T(B, table=True):\n    id: int | None = Field(default=None, primary_key=True)\n\n\n"
                                 "app = FastAPI()\n\n\n@app.get('/x')\ndef x() -> list[T]:\n    return []\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert "main.py:" in capsys.readouterr().err

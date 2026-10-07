"""dyn backend: constructions outside the subset are refused with file:line, never approximated."""
import textwrap

import pytest

from py2axum.__main__ import main

MODELS = '''
from sqlalchemy import ForeignKey, String
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, relationship


class Base(DeclarativeBase):
    pass


class Team(Base):
    __tablename__ = "teams"
    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(50))
    parent_id: Mapped[int | None] = mapped_column(ForeignKey("teams.id"))
    owner_id: Mapped[int | None] = mapped_column(ForeignKey("users.id"))
    backup_id: Mapped[int | None] = mapped_column(ForeignKey("users.id"))
    {rel}


class User(Base):
    __tablename__ = "users"
    id: Mapped[int] = mapped_column(primary_key=True)
'''

MAIN = '''
from fastapi import FastAPI
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine
from fastapi import Depends
from .models import Team

engine = create_async_engine("postgresql+psycopg://x/y")
Session = async_sessionmaker(engine)
app = FastAPI()


async def db():
    async with Session() as s:
        yield s


@app.get("/teams/{team_id}")
async def team(team_id: int, s: AsyncSession = Depends(db)):
    t = await s.get(Team, team_id)
    return {"name": t.name}
'''


@pytest.mark.parametrize(
    "rel, message",
    [
        ('parent: Mapped["Team"] = relationship(remote_side=[id])', "option remote_side= is not supported"),
        ('owner: Mapped["User"] = relationship()', "cannot pick the foreign key between teams and users"),
        ('owner: Mapped["User"] = relationship(foreign_keys=[owner_id], lazy="dynamic")', "lazy='dynamic' is not supported"),
        ('members: Mapped[list["User"]] = relationship(secondary="team_users")', "option secondary= is not supported"),
        ('ghost: Mapped["Nope"] = relationship("Nope")', "relationship target 'Nope': unknown mapped class"),
    ],
)
def test_relationship_rejected(tmp_path, capsys, rel, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", rel))
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "models.py:" in err


SESSION_MAIN = '''
from fastapi import Depends, FastAPI
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine

from .models import Team

engine = create_async_engine("postgresql+psycopg://x/y")
Session = async_sessionmaker(engine{opts})
app = FastAPI()


async def db():
{body}


@app.get("/teams/{{team_id}}")
async def team(team_id: int, s: AsyncSession = Depends(db)):
    t = await s.get(Team, team_id)
    return {{"name": t.name}}
'''


@pytest.mark.parametrize(
    "opts, body, message",
    [
        ("", "    async with Session() as s:\n        yield s\n        await s.execute('SET x')",
         "session dependency db: only `async with maker() as s: yield s`"),
        ("", "    async with Session() as s:\n        try:\n            yield s\n        except Exception:\n            pass",
         "session dependency db: only"),
        (", info={'a': 1}", "    async with Session() as s:\n        yield s", "async_sessionmaker(info=) is not supported"),
    ],
)
def test_session_dependency_rejected(tmp_path, capsys, opts, body, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    (pkg / "main.py").write_text(SESSION_MAIN.format(opts=opts, body=body))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "decl, message",
    [
        ("total = column_property(id + 1)", "only columns and relationships are supported"),
        ('__mapper_args__ = {"version_id_col": id}', "__mapper_args__ is not supported"),
        ('label: Mapped[str] = mapped_column("lbl", String(20))', "a SQL column name different from the attribute"),
        ("data: Mapped[dict] = mapped_column(JSON(astext_type=None, foo=1))", "JSON(foo=) is not supported"),
        ("@hybrid_property\n    def double(self):\n        return self.id * 2", "decorator @hybrid_property is not supported"),
        ("@validates('name')\n    def check(self, key, value):\n        return value", "decorator @validates('name') is not supported"),
    ],
)
def test_model_declaration_rejected(tmp_path, capsys, decl, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    models = textwrap.dedent(MODELS).replace("{rel}", decl).replace(
        "from sqlalchemy import ForeignKey, String",
        "from sqlalchemy import JSON, ForeignKey, String\nfrom sqlalchemy.orm import column_property, validates\n"
        "from sqlalchemy.ext.hybrid import hybrid_property")
    (pkg / "models.py").write_text(models)
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "models.py:" in err


@pytest.mark.parametrize(
    "body, message",
    [
        ("    it = iter([1, 2])\n    return {'a': next(it)}", "iter() stored in a variable is not supported"),
        ("    xs = [3, 1]\n    return {'a': xs.frobnicate()}", "method .frobnicate() is not implemented by the runtime"),
        ("    d = {}\n    return {'a': d.model_dump(context={'x'})}", ".model_dump(context=) is not supported"),
        ("    import os\n    os.environ['X'] = '1'\n    return {}", "writing into os.environ is not supported"),
    ],
)
def test_python_semantics_rejected(tmp_path, capsys, body, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x():\n" + body + "\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


def test_validate_assignment_with_validators_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent('''
        from fastapi import FastAPI
        from pydantic import BaseModel, ConfigDict, field_validator

        app = FastAPI()


        class In(BaseModel):
            model_config = ConfigDict(validate_assignment=True)
            name: str

            @field_validator("name")
            @classmethod
            def up(cls, v):
                return v.upper()


        @app.post("/x")
        async def x(body: In):
            return body
    '''))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "validate_assignment=True with @field_validator is not supported" in err
    assert "main.py:" in err


def test_imported_rebound_global_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "state.py").write_text("count = 0\n\n\ndef bump():\n    global count\n    count += 1\n")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\nfrom .state import bump, count\n\napp = FastAPI()\n\n\n"
        "@app.get('/x')\nasync def x():\n    bump()\n    return {'n': count}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "copies a variable that a function rebinds" in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "decl, message",
    [
        ("@dataclass(slots=True)\nclass C:\n    a: int\n", "@dataclass(slots=) is not supported"),
        ("@dataclass\nclass C:\n    a: int\n\n    def __lt__(self, other):\n        return True\n", "method __lt__ is not supported"),
        ("@dataclass\nclass C:\n    a: int = field(default=1, repr=False)\n", "field() supports default= and default_factory= only"),
    ],
)
def test_dataclass_rejected(tmp_path, capsys, decl, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from dataclasses import dataclass, field\nfrom fastapi import FastAPI\n\napp = FastAPI()\n\n\n" + decl
        + "\n\n@app.get('/x')\nasync def x():\n    return {'c': C(1)}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "body, message",
    [
        ("    impl = Integer\n", "impl `Integer` is not supported"),
        ("    impl = String\n\n    def process_bind_param(self, value, dialect):\n        return self.x\n",
         "process_bind_param: using `self` is not supported"),
        ("    impl = String\n\n    def coerce_compared_value(self, op, value):\n        return self\n",
         "`coerce_compared_value` is not supported"),
    ],
)
def test_type_decorator_rejected(tmp_path, capsys, body, message):
    models = MODELS.replace("{rel}", "").replace(
        "class User(Base):",
        "class Weird(TypeDecorator):\n" + body + "\n\nclass User(Base):")
    models = "from sqlalchemy import Integer\nfrom sqlalchemy.types import TypeDecorator\n" + models
    models += "    secret: Mapped[str] = mapped_column(Weird(10))\n"
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(models)
    (pkg / "main.py").write_text(textwrap.dedent(MAIN).replace("from .models import Team", "from .models import Team, User")
                                 + "\n\n@app.get('/users/{uid}')\nasync def user(uid: int, s: AsyncSession = Depends(db)):\n"
                                   "    u = await s.get(User, uid)\n    return {'secret': u.secret}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "models.py:" in err


def test_generator_dependency_with_except_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import Depends, FastAPI\n\napp = FastAPI()\n\n\nasync def dep():\n    try:\n        yield 1\n"
        "    except ValueError:\n        pass\n\n\n@app.get('/x')\nasync def x(v: int = Depends(dep)):\n    return {'v': v}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "`yield` inside try/except is not supported" in err
    assert "main.py:" in err


def test_form_in_dependency_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import Depends, FastAPI, Form\n\napp = FastAPI()\n\n\nasync def dep(x: str = Form(...)):\n    return x\n\n\n"
        "@app.post('/x')\nasync def x(v: str = Depends(dep)):\n    return {'v': v}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "Form()/File() parameters of a dependency are not supported" in err


@pytest.mark.parametrize(
    "code, message",
    [
        ("for _ in range(2):\n    app.add_middleware(CORSMiddleware)\n", "inside `For` is not supported"),
        ("opts = {}\napp.add_middleware(CORSMiddleware, **opts)\n", "add_middleware(*args/**kwargs) is not supported"),
        ("@app.middleware('websocket')\nasync def mw(request, call_next):\n    return await call_next(request)\n",
         'only @app.middleware("http") is supported'),
    ],
)
def test_middleware_stack_rejected(tmp_path, capsys, code, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\nfrom fastapi.middleware.cors import CORSMiddleware\n\napp = FastAPI()\n"
        + code + "\n\n@app.get('/x')\nasync def x():\n    return {}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


def test_unknown_async_with_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "import contextlib\nfrom fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x():\n"
        "    async with contextlib.AsyncExitStack() as stack:\n        return {'ok': True}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "contextlib.AsyncExitStack()" in err and "not supported" in err
    assert "main.py:" in err


def test_mixed_decorators_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from functools import lru_cache\nfrom fastapi import FastAPI\n\napp = FastAPI()\n\n\n"
        "def deco(f):\n    return f\n\n\n@deco\n@lru_cache\ndef conf():\n    return 1\n\n\n"
        "@app.get('/x')\nasync def x():\n    return {'v': conf()}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "@lru_cache combined with @deco is not supported" in err
    assert "main.py:" in err


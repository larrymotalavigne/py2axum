"""dyn backend: constructions outside the subset are refused with file:line, never approximated."""
import json
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
        ('owner: Mapped["User"] = relationship(foreign_keys=[owner_id], remote_side=[id])',
         "remote_side= is only supported on a self-referential"),
        ('owner: Mapped["User"] = relationship()', "cannot pick the foreign key between teams and users"),
        ('owner: Mapped["User"] = relationship(foreign_keys=[owner_id], lazy="dynamic")', "lazy='dynamic' is not supported"),
        ('owner: Mapped["User"] = relationship(foreign_keys=[owner_id], order_by="User.nope")', "order_by= must name columns of User"),
        ('owner: Mapped["User"] = relationship(foreign_keys=[owner_id], order_by=["User.id"])', "order_by= must name columns of User"),
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


SYNC_MAIN = '''
from fastapi import Depends, FastAPI
from sqlalchemy import create_engine
from sqlalchemy.ext.asyncio import async_sessionmaker
from sqlalchemy.orm import Session, sessionmaker

from .models import Team

engine = create_engine("postgresql+psycopg://x/y")
Maker = {maker}
app = FastAPI()


{dep}


@app.get("/teams/{{team_id}}")
def team(team_id: int, {param}):
    return {{"name": s.get(Team, team_id).name}}
'''


@pytest.mark.parametrize(
    "maker, dep, param, message",
    [
        ("sessionmaker(bind=engine)", "def db():\n    s = Maker()\n    yield s", "s: Session = Depends(db)",
         "session dependency db: only"),
        ("sessionmaker(bind=engine)", "def db():\n    s = Maker()\n    try:\n        yield s\n    finally:\n        s.expunge_all()",
         "s: Session = Depends(db)", "session dependency db: only"),
        ("async_sessionmaker(engine)", "def db():\n    with Maker() as s:\n        yield s", "s: Session = Depends(db)",
         "a synchronous dependency needs sessionmaker(...)"),
        ("sessionmaker(bind=engine)", "async def db():\n    async with Maker() as s:\n        yield s", "s: Session = Depends(db)",
         "a coroutine dependency needs async_sessionmaker(...)"),
        ("sessionmaker(bind=engine, info={})", "def db():\n    with Maker() as s:\n        yield s", "s: Session = Depends(db)",
         "sessionmaker(info=) is not supported"),
        ("sessionmaker(bind=engine)", "def db():\n    with Maker() as s:\n        yield s", "s: Session",
         "s: a synchronous Session parameter needs Depends(<session dependency>)"),
    ],
)
def test_sync_session_dependency_rejected(tmp_path, capsys, maker, dep, param, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    (pkg / "main.py").write_text(SYNC_MAIN.format(maker=maker, dep=dep, param=param))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "decl, message",
    [
        ("total = column_property(id + 1)", "only columns and relationships are supported"),
        ('__mapper_args__ = {"version_id_col": id}', "__mapper_args__ is not supported"),
        ("data: Mapped[dict] = mapped_column(JSON(astext_type=None, foo=1))", "JSON(foo=) is not supported"),
        ("codes: Mapped[list] = mapped_column(ARRAY(Integer))", "only ARRAY(String) is supported"),
        ("codes: Mapped[list] = mapped_column(ARRAY(String, as_tuple=True))", "ARRAY(as_tuple=) is not supported"),
        ("codes: Mapped[list] = mapped_column(JSON().with_variant(ARRAY(Integer), 'postgresql'))", "only ARRAY(String) is supported"),
        ("@hybrid_property\n    def double(self):\n        return self.id * 2", "decorator @hybrid_property is not supported"),
        ("@validates('name')\n    def check(self, key, value):\n        return value", "decorator @validates('name') is not supported"),
        ("blob: Mapped[bytes] = deferred(Column(LargeBinary), group='g')", "only deferred(Column(...)) is supported"),
    ],
)
def test_model_declaration_rejected(tmp_path, capsys, decl, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    models = textwrap.dedent(MODELS).replace("{rel}", decl).replace(
        "from sqlalchemy import ForeignKey, String",
        "from sqlalchemy import JSON, ForeignKey, Integer, String\nfrom sqlalchemy.dialects.postgresql import ARRAY\n"
        "from sqlalchemy import Column, LargeBinary\nfrom sqlalchemy.orm import column_property, deferred, validates\n"
        "from sqlalchemy.ext.hybrid import hybrid_property")
    (pkg / "models.py").write_text(models)
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "models.py:" in err


@pytest.mark.parametrize(
    "factory, use, message",
    [
        ("def kind(n):\n    if n:\n        return String(n)\n    return String(5)", "kind(3)", "body is `return <column type>`"),
        ("def kind(*a):\n    return String(*a)", "kind(3)", "only plain parameters are supported"),
        ("def kind(n):\n    return String(n)", "kind(3, 4)", "too many arguments to kind()"),
        ("def kind(n):\n    return String(n)", "kind(m=3)", "kind() has no parameter `m`"),
        ("def kind(n):\n    return String(n)", "kind()", "kind() missing argument `n`"),
        ("def kind(n):\n    return String(n)", "kind(len('ab'))", "must be a literal or a name"),
        ("def kind(n):\n    return String(len([n for n in 'ab']))", "kind(2)", "rebinds one of its parameters"),
    ],
)
def test_column_type_factory_rejected(tmp_path, capsys, factory, use, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    models = textwrap.dedent(MODELS).replace("{rel}", f"code: Mapped[str] = mapped_column({use})")
    (pkg / "models.py").write_text(models + "\n\n" + factory + "\n")
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "models.py:" in err


def test_column_type_factory_other_module(tmp_path, capsys):
    """the factory's body is read where the column is declared: a name it uses must mean the same there"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "kinds.py").write_text("from sqlalchemy import Text as String\n\n\ndef kind(n):\n    return String(n)\n")
    models = textwrap.dedent(MODELS).replace("{rel}", "code: Mapped[str] = mapped_column(kind(3))")
    (pkg / "models.py").write_text(models.replace("from sqlalchemy import ForeignKey, String",
                                                  "from sqlalchemy import ForeignKey, String\nfrom .kinds import kind"))
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "`String` in kind() does not mean the same" in err
    assert "kinds.py:5" in err


@pytest.mark.parametrize(
    "body, message",
    [
        ("    it = iter([1, 2])\n    return {'a': next(it)}", "iter() stored in a variable is not supported"),
        ("    xs = [3, 1]\n    return {'a': xs.frobnicate()}", "method .frobnicate() is not implemented by the runtime"),
        ("    d = {}\n    return {'a': d.model_dump(context={'x'})}", ".model_dump(context=) is not supported"),
        ("    xs = [3, 1]\n    return {'a': xs.frobnicated}", "attribute .frobnicated is not provided by the runtime"),
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


def test_validate_assignment_with_model_before_rejected(tmp_path, capsys):
    """Field and model `after` validators run on assignment; a model `before` validator would get the whole data."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent('''
        from fastapi import FastAPI
        from pydantic import BaseModel, ConfigDict, model_validator

        app = FastAPI()


        class Base(BaseModel):
            model_config = ConfigDict(validate_assignment=True)


        class In(Base):
            name: str

            @model_validator(mode="before")
            @classmethod
            def up(cls, data):
                return data


        @app.post("/x")
        async def x(body: In):
            return body
    '''))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert 'validate_assignment=True with @model_validator(mode="before") is not supported' in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "body, message",
    [
        ("    n: int = Field(max_digits=3)", "max_digits= applies to Decimal only"),
        ("    d: Decimal = Field(gt=Decimal('1'))", "`Decimal('1')` is not a constant"),
        ("    n: int\n\n    @computed_field(alias='N')\n    @property\n    def m(self) -> int:\n        return 1",
         "@computed_field(...) options are not supported"),
        ("    n: int\n\n    @computed_field\n    @cached_property\n    def m(self) -> int:\n        return 1",
         "is not supported with @computed_field"),
        ("    n: int\n\n    def __init__(self, n):\n        super().__init__(n=n)", "only `def __init__(self, **data)` is supported"),
    ],
)
def test_schema_declaration_rejected(tmp_path, capsys, body, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from decimal import Decimal\nfrom functools import cached_property\n"
        "from fastapi import FastAPI\nfrom pydantic import BaseModel, Field, computed_field\n\napp = FastAPI()\n\n\n"
        f"class In(BaseModel):\n{body}\n\n\n@app.post('/x')\nasync def x(b: In):\n    return b\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "classes, message",
    [
        ("class A:\n    pass\n\n\nclass B:\n    pass\n\n\nclass C(A, B):\n    pass\n", "only exception classes are supported"),
        ("class A:\n    def m(self):\n        return 1\n\n\nclass C(A):\n    def m(self):\n        return super().m()\n",
         "only `super().__init__(...)` inside a method is supported"),
        ("from abc import ABC\n\n\nclass A(ABC):\n    pass\n\n\nclass C(A):\n    def __init__(self):\n        super().__init__(1)\n",
         "object.__init__() takes exactly one argument"),
    ],
)
def test_plain_class_inheritance_rejected(tmp_path, capsys, classes, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("from fastapi import FastAPI\n\napp = FastAPI()\n\n\n" + classes
                                 + "\n\n@app.get('/x')\nasync def x():\n    c = C()\n    return {'m': c.m() if hasattr(c, 'm') else 0}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
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
        # a raw ASGI middleware needs `async def __call__(self, scope, receive, send)`
        ("class Sync:\n    def __init__(self, app):\n        self.app = app\n\n    def __call__(self, scope, receive, send):\n"
         "        return self.app(scope, receive, send)\n\n\napp.add_middleware(Sync)\n",
         "middleware Sync is not supported (CORSMiddleware, GZipMiddleware, BaseHTTPMiddleware"),
        ("flag = False\napp2 = FastAPI(strict_content_type=flag)\n",
         "FastAPI(strict_content_type=...) must be a literal True or False"),
        ("from fastapi.middleware import Middleware\nfrom fastapi.middleware.gzip import GZipMiddleware\n"
         "app2 = FastAPI(middleware=[Middleware(GZipMiddleware)])\n",
         "Middleware(GZipMiddleware) in FastAPI(middleware=[...]) is not supported"),
        ("mws = []\napp2 = FastAPI(middleware=mws)\n", "FastAPI(middleware=...) must be a literal list of Middleware(...)"),
        ("from starlette_context import plugins\nfrom starlette_context.middleware import RawContextMiddleware\n"
         "app.add_middleware(RawContextMiddleware, plugins=(plugins.UserAgentPlugin(),))\n",
         "RawContextMiddleware: plugin plugins.UserAgentPlugin() is not supported"),
        ("from starlette_context import plugins\nfrom starlette_context.middleware import RawContextMiddleware\n"
         "app.add_middleware(RawContextMiddleware, plugins=(plugins.RequestIdPlugin(force_new_uuid=FORCE),))\nFORCE = True\n",
         "RawContextMiddleware: plugin option force_new_uuid= is not supported"),
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



IMPORT_MAIN = '''
import importlib

from fastapi import FastAPI

app = FastAPI()


@app.get("/m/{{name}}")
async def m(name: str):
    return {{"v": getattr(importlib.import_module({arg}), "X")}}
'''


@pytest.mark.parametrize(
    "arg, message",
    [
        ("name", "importlib.import_module() of a computed name is not supported"),
        ('"json"', "importlib.import_module('json'): only modules of the project are supported"),
    ],
)
def test_import_module_rejected(tmp_path, capsys, arg, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(IMPORT_MAIN.format(arg=arg))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:11" in err


COLLS_MAIN = '''
import string
from collections import defaultdict

from fastapi import FastAPI

app = FastAPI()


@app.get("/c")
async def c():
    return {{"v": {expr}}}
'''


@pytest.mark.parametrize(
    "expr, message",
    [
        ("defaultdict(lambda: 0)", "defaultdict(lambda: 0): only a builtin type factory is supported"),
        ("defaultdict()", "defaultdict(factory[, mapping]) with a builtin type as factory is the only supported form"),
        ('list(string.Formatter().parse("{a}"))', "string.Formatter().parse() is not supported"),
    ],
)
def test_collections_rejected(tmp_path, capsys, expr, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(COLLS_MAIN.format(expr=expr))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:12" in err


TENACITY_MAIN = '''
import asyncio

from fastapi import FastAPI
from tenacity import retry, stop_after_attempt

app = FastAPI()


@retry(stop=stop_after_attempt(2), {opt})
async def flaky():
    return 1


@app.get("/t")
async def t():
    return {{"v": await flaky()}}
'''


@pytest.mark.parametrize("opt", ["sleep=asyncio.sleep", "retry_error_cls=ValueError"])
def test_tenacity_options_rejected(tmp_path, capsys, opt):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(TENACITY_MAIN.format(opt=opt))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert f"tenacity.retry({opt.split('=')[0]}=) is not supported" in err
    assert "main.py:10" in err


PROM_MAIN = """
from fastapi import FastAPI, Response
from prometheus_client import Counter, generate_latest, make_asgi_app, push_to_gateway

app = FastAPI()
C = Counter("c", "doc")


@app.get("/m")
async def m():
    {stmt}
    return Response(b"")
"""


@pytest.mark.parametrize("stmt, msg", [
    ("push_to_gateway('g', job='j', registry=None)", "library call `prometheus_client.push_to_gateway()` is not supported"),
    ("make_asgi_app()", "library call `prometheus_client.make_asgi_app()` is not supported"),
    ("Counter('d', 'doc', _labelvalues=('x',))", "prometheus_client.Counter(_labelvalues=) is not supported"),
])
def test_prometheus_outside_subset_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PROM_MAIN.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:11" in err


PYJWT_MAIN = """
import jwt
from fastapi import FastAPI

app = FastAPI()
KEY = "k" * 32


@app.get("/t")
async def t(token: str):
    return {stmt}
"""


@pytest.mark.parametrize("stmt, msg", [
    ("jwt.encode({'a': 1}, KEY, algorithm='RS256')", "jwt.encode(): algorithm 'RS256' is not supported"),
    ("jwt.encode({'a': 1}, KEY, headers={'alg': 'ES256'})", "jwt.encode(): algorithm 'ES256' is not supported"),
    ("jwt.decode(token, KEY, algorithms=['HS256', 'EdDSA'])", "jwt.decode(): algorithm 'EdDSA' is not supported"),
    ("jwt.decode(token, KEY, ['PS256'])", "jwt.decode(): algorithm 'PS256' is not supported"),
    ("jwt.encode({'a': 1}, KEY, json_encoder=None)", "jwt.encode(json_encoder=) is not supported"),
    ("jwt.decode(token, KEY, algorithms=['HS256'], detached_payload=b'x')", "jwt.decode(detached_payload=) is not supported"),
    ("jwt.decode(token, KEY, algorithms=['HS256'], verify=True)", "jwt.decode(verify=) is not supported"),
    ("jwt.decode(token, KEY, ['HS256'], None, True)", "jwt.decode(): verify is not supported"),
    ("jwt.decode(token, KEY, algorithms=['HS256'], foo=1)", "jwt.decode(foo=) is not supported"),
    ("jwt.PyJWKClient('https://idp/jwks').get_signing_key_from_jwt(token)", "is not supported"),
])
def test_pyjwt_outside_subset_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PYJWT_MAIN.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:11" in err


def test_module_variable_bound_twice_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("""
from fastapi import FastAPI

app = FastAPI()
LIMIT = 10
try:
    LIMIT = int("20")
except ValueError:
    pass


@app.get("/l")
async def l():
    return {"limit": LIMIT}
""")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "the module variable `LIMIT` is bound by several module-level statements (lines 5, 6): not supported" in err
    assert "main.py:6" in err


CONFIGURE_MAIN = """
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from .router import r


def configure(app):
    {stmt}


def create_app():
    app = FastAPI()
    configure(app)
    return app


app = create_app()
"""


@pytest.mark.parametrize("stmt, msg", [
    ("app.include_router(r)", "configure(): `app.include_router(...)` on the application passed to a function is not supported"),
    ("app.add_middleware(CORSMiddleware, allow_origins=['*'])",
     "add_middleware(CORSMiddleware) outside the app factory is not supported"),
])
def test_configure_function_registrations_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "router.py").write_text("from fastapi import APIRouter\n\nr = APIRouter()\n\n\n@r.get('/x')\nasync def x():\n    return {}\n")
    (pkg / "main.py").write_text(CONFIGURE_MAIN.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:9" in err


def test_library_attribute_assignment_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("""
import aiohttp
from fastapi import FastAPI

app = FastAPI()


def patch():
    aiohttp.ClientSession.get = None


@app.get("/p")
async def p():
    patch()
    return {}
""")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "assigning `aiohttp.ClientSession.get` (a library attribute) is not supported" in err
    assert "main.py:9" in err


def test_runtime_prefix_in_factory_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("""
import os

from fastapi import APIRouter, FastAPI

router = APIRouter()


@router.get("/x")
async def x():
    return {}


def create_app() -> FastAPI:
    app = FastAPI()
    app.include_router(router, prefix=os.environ.get("PREFIX", "/api"))
    return app


app = create_app()
""")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "include_router(prefix=<non-literal>) inside a function (app factory) is not supported" in err
    assert "main.py:16" in err


def test_model_method_decorator_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    models = textwrap.dedent(MODELS).replace("{rel}", "@staticmethod\n    def ok() -> int:\n        return 1\n\n"
                                             "    @functools.cache\n    def cached(self):\n        return 2")
    (pkg / "models.py").write_text("import functools\n" + models)
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "decorator @functools.cache is not supported (only @property, @staticmethod and @classmethod)" in err
    assert "models.py:" in err


def test_attribute_read_allowed_when_project_computes_attributes(tmp_path):
    """setattr with a computed name in the project: no attribute read is refused."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\n\napp = FastAPI()\n\n\nclass Bag:\n    def __init__(self, **kw):\n"
        "        for k, v in kw.items():\n            setattr(self, k, v)\n\n\n@app.get('/x')\nasync def x():\n"
        "    return {'a': Bag(frobnicated=1).frobnicated}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 0


@pytest.mark.parametrize(
    "config, call, message",
    [
        ('env_nested_delimiter="__"', "S()", "model_config env_nested_delimiter= is not supported"),
        ('env_ignore_empty=True', "S()", "model_config env_ignore_empty= is not supported"),
        ('env_parse_none_str=5', "S()", "env_parse_none_str= must be a string or None"),
        ('env_parse_none_str="null"', 'S(_env_parse_none_str="none")',
         "S(_env_parse_none_str=): pydantic-settings init options are not supported"),
    ],
)
def test_settings_options_rejected(tmp_path, capsys, config, call, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent(f'''
        from fastapi import FastAPI
        from pydantic_settings import BaseSettings, SettingsConfigDict

        app = FastAPI()


        class S(BaseSettings):
            model_config = SettingsConfigDict({config})
            A: int | None = 1


        @app.get("/x")
        async def x():
            return {{"a": {call}.A}}
    '''))


@pytest.mark.parametrize(
    "decl, message",
    [
        ("lifespan = None\n\n\ndef make():\n    return None\n\n\napp = FastAPI(lifespan=make())",
         "FastAPI(lifespan=...): only an `async def` generator of the project"),
        ("async def lifespan(app):\n    return None\n\n\napp = FastAPI(lifespan=lifespan)",
         "FastAPI(lifespan=...): only an `async def` generator of the project"),
        ("from contextlib import asynccontextmanager\n\n\n@asynccontextmanager\nasync def lifespan(app):\n"
         "    yield {'pool': 1}\n\n\napp = FastAPI(lifespan=lifespan)",
         "lifespan state (`yield <value>` in the lifespan) is not supported"),
    ],
)
def test_lifespan_rejected(tmp_path, capsys, decl, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(f"from fastapi import FastAPI\n\n{decl}\n\n\n@app.get('/x')\nasync def x():\n    return {{}}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "validator, message",
    [
        ('@field_validator("name")\n    @classmethod\n    def v(cls, v, info):\n        return info.context',
         "ValidationInfo.context is not supported"),
        ('@field_validator("name", mode="before")\n    @classmethod\n    def v(cls, v, info):\n        return v',
         "`info`/`values` in a mode='before' validator is not supported"),
        ('@validator("name")\n    def v(cls, v, values, field):\n        return v',
         "the `field` and `config` validator parameters do not exist in Pydantic v2"),
        ('@field_validator("name")\n    @classmethod\n    def v(cls, v, info, extra):\n        return v',
         "unrecognized field validator signature"),
        ('_v = field_validator("name")(lambda v, info: v)', "a lambda validator taking `info` is not supported"),
    ],
)
def test_validator_info_rejected(tmp_path, capsys, validator, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\nfrom pydantic import BaseModel, field_validator, validator\n\napp = FastAPI()\n\n\n"
        "class In(BaseModel):\n    name: str\n\n    " + validator + "\n\n\n"
        "@app.post('/x')\nasync def x(body: In):\n    return body\n")


MCP_APP = '''
from typing import Annotated, Any

from fastapi import FastAPI
from mcp.server.mcpserver import MCPServer
from mcp.server.transport_security import TransportSecuritySettings
from pydantic import Field

server = MCPServer(name="t"{server_kw})
{tool}


async def lifespan_like():
    server.streamable_http_app({app_kw})
    async with server.session_manager.run():
        pass


class Endpoint:
    async def __call__(self, scope, receive, send):
        await server.session_manager.handle_request(scope, receive, send)


app = FastAPI()
app.router.add_route("/mcp", Endpoint(), methods=["GET", "POST", "DELETE"])


@app.get("/x")
async def x():
    await lifespan_like()
    return {{}}
'''
TOOL = '''

@server.tool(name="t1", description="d")
async def t1(n: Annotated[int, Field(ge=0)] = 1) -> dict[str, Any]:
    return {"n": n}
'''
OK_APP_KW = "stateless_http=True, json_response=True, transport_security=TransportSecuritySettings(enable_dns_rebinding_protection=False)"


@pytest.mark.parametrize(
    "server_kw, tool, app_kw, message",
    [
        (", website_url='x'", TOOL, OK_APP_KW, "MCPServer(website_url=...) is not supported"),
        ("", TOOL.replace("async def t1", "def t1"), OK_APP_KW, "only `async def` functions are supported"),
        ("", TOOL.replace("-> dict[str, Any]", "-> str"), OK_APP_KW, "only the return annotation dict[str, Any]"),
        ("", TOOL.replace('description="d"', 'description="d", annotations=None'), OK_APP_KW,
         "@server.tool(annotations=...) is not supported"),
        ("", TOOL.replace("@server.tool(name=\"t1\", description=\"d\")", "@server.tool"), OK_APP_KW,
         "@server.tool without parentheses"),
        ("", TOOL.replace("n: Annotated[int, Field(ge=0)] = 1", "_n: int = 1"), OK_APP_KW, "cannot start with '_'"),
        ("", TOOL.replace("n: Annotated[int, Field(ge=0)] = 1", "n: tuple[int, int]"), OK_APP_KW,
         "type `tuple[int, int]` is not supported"),
        ("", TOOL + "\n\n@server.resource('x://y')\nasync def r():\n    return 'x'\n", OK_APP_KW, "MCPServer.resource is not supported"),
        ("", TOOL, OK_APP_KW.replace("stateless_http=True", "stateless_http=False"), "streamable_http_app(stateless_http=True) is required"),
        ("", TOOL, OK_APP_KW.replace("json_response=True, ", ""), "streamable_http_app(json_response=True) is required"),
        ("", TOOL, "stateless_http=True, json_response=True", "transport_security=TransportSecuritySettings"),
        ("", TOOL, OK_APP_KW.replace("False", "True"), "only enable_dns_rebinding_protection=False is supported"),
    ],
)
def test_mcp_rejected(tmp_path, capsys, server_kw, tool, app_kw, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(MCP_APP.format(server_kw=server_kw, tool=tool, app_kw=app_kw))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


def test_union_member_with_validators_rejected(tmp_path, capsys):
    """Pydantic tries the next member when a member's validator raises: not emulated, refused."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent('''
        from fastapi import FastAPI
        from pydantic import BaseModel, field_validator

        app = FastAPI()


        class A(BaseModel):
            x: int

            @field_validator("x")
            @classmethod
            def small(cls, v):
                if v > 9:
                    raise ValueError("big")
                return v


        class B(BaseModel):
            x: int


        class In(BaseModel):
            item: A | B


        @app.post("/x")
        async def x(body: In):
            return body
    '''))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "a Union containing A (validators, which would make Pydantic try the next member" in err
    assert "main.py:24" in err


def test_mcp_accepted(tmp_path):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(MCP_APP.format(server_kw=", title='T', instructions='i'", tool=TOOL, app_kw=OK_APP_KW))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "MCP_TOOL_" in gen and '\\"title\\":\\"t1Arguments\\"' in gen


def test_raw_asgi_class_endpoint_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\n\n\nclass Ep:\n    async def __call__(self, scope, receive, send):\n        pass\n\n\n"
        "app = FastAPI()\napp.add_route('/raw', Ep)\n\n\n@app.get('/x')\nasync def x():\n    return {}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "add_route() with a class endpoint" in err and "main.py:10" in err


WS_APP = '''
from typing import Annotated

from fastapi import Depends, FastAPI, Request, WebSocket
from pydantic import BaseModel

app = FastAPI()


class Msg(BaseModel):
    text: str


async def needs_request(request: Request):
    return request.url.path


async def outer(v: Annotated[str, Depends(needs_request)]):
    return v
{extra}
'''


@pytest.mark.parametrize(
    "extra, message, line",
    [
        ('@app.websocket("/w")\nasync def w(websocket: WebSocket, request: Request):\n    pass\n',
         "parameter `request` (a Request) is not provided by FastAPI on a WebSocket route", 23),
        ('@app.websocket("/w")\nasync def w(websocket: WebSocket, msg: Msg):\n    pass\n',
         "parameter `msg` (a body) is not provided", 23),
        ('@app.websocket("/w")\nasync def w(websocket: WebSocket, v: Annotated[str, Depends(outer)]):\n    pass\n',
         "dependency needs_request (main.py:14) takes a Request, which FastAPI does not provide on a WebSocket route", 23),
        ('@app.websocket("/w", dependencies=[Depends(needs_request)])\nasync def w(websocket: WebSocket):\n    pass\n',
         "dependency needs_request (main.py:14) takes a Request", 23),
        ('@app.websocket("/w")\ndef w(websocket: WebSocket):\n    pass\n', "WebSocket endpoint w must be an `async def`", 23),
        ('@app.websocket("/w", response_class=None)\nasync def w(websocket: WebSocket):\n    pass\n',
         "unsupported WebSocket route option response_class=", 22),
        ('async def w(websocket: WebSocket):\n    pass\n\napp.add_websocket_route("/w", w)\n',
         "app.add_websocket_route(...) is not supported", 25),
        ('async def w(websocket: WebSocket):\n    pass\n\napp.add_api_websocket_route("/w", w)\n',
         "app.add_api_websocket_route(...) is not supported", 25),
        ('@app.websocket_route("/w")\nasync def w(websocket: WebSocket):\n    pass\n', "@app.websocket_route(...) is not supported", 22),
        ('from fastapi import APIRouter\nr = APIRouter()\n@r.websocket_route("/w")\nasync def w(websocket: WebSocket):\n    pass\n'
         'app.include_router(r)\n', "@router.websocket_route(...) is not supported: use @router.websocket(...)", 24),
    ],
)
def test_websocket_rejected(tmp_path, capsys, extra, message, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(WS_APP.format(extra="\n\n" + extra))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err and f"main.py:{line}" in err, err


def test_websocket_python_side_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(WS_APP.format(extra='\n\n@app.websocket("/w")\nasync def w(websocket: WebSocket):\n    await websocket.accept()\n'))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "--python-side", "/w", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "WebSocket route /w cannot be declared --python-side" in err and "main.py:22" in err, err


def test_websocket_accepted(tmp_path):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(WS_APP.format(extra='''

from fastapi import APIRouter, WebSocketDisconnect
router = APIRouter(prefix="/r")


@router.websocket("/w/{room}", name="room")
async def w(websocket: WebSocket, room: str, n: int = 1):
    await websocket.accept()
    try:
        async for t in websocket.iter_text():
            await websocket.send_text(room + t * n)
    except WebSocketDisconnect:
        pass

app.include_router(router)
'''))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "/// WEBSOCKET /r/w/{room}" in gen and "WsRouteDef" in gen and "Node::Ws" in gen


JSON_SCHEMA_APP = '''
from dataclasses import dataclass

from fastapi import FastAPI
from pydantic import BaseModel, field_validator


class A(BaseModel):
    x: int = 1


class B(BaseModel):
    y: str

    @field_validator("y")
    @classmethod
    def strip(cls, v):
        return v.strip()


@dataclass
class Tool:
    params_model: type


TOOLS = [Tool(params_model=A), Tool(params_model={other})]
app = FastAPI()


@app.get("/x")
async def x():
    {body}
'''


@pytest.mark.parametrize(
    "other, body, message, line",
    [
        ("A", "return [t.params_model.model_json_schema() for t in TOOLS]", None, None),
        ("B", "return [t.params_model.model_json_schema() for t in TOOLS]",
         ".model_json_schema() of B: ", 32),
        ("A", "return B.model_json_schema()", ".model_json_schema() of B: ", 32),
        ("A", "m = A\n    return m.model_json_schema()",
         "on a value that is not a model class or an attribute holding one", 33),
        ("A", "return A(x=2).model_json_schema()", "not a model class or an attribute holding one", 32),
        ("A", "return A.model_json_schema(mode='serialization')", "with arguments is not supported", 32),
        ("A()", "return [t.params_model.model_json_schema() for t in TOOLS]", "which is also bound to `A()`", 32),
    ],
)
def test_model_json_schema_rejected(tmp_path, capsys, other, body, message, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(JSON_SCHEMA_APP.format(other=other, body=body))
    code = main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")])
    err = capsys.readouterr().err
    if message is None:
        assert code == 0, err
        assert 'title\\":\\"A' in (tmp_path / "out" / "src" / "gen.rs").read_text()
        return
    assert code == 1
    assert message in err and f"main.py:{line}" in err


@pytest.mark.parametrize(
    "body, message",
    [
        ("return inspect(x)", "only inspect(x, raiseerr=False) is supported"),
        ("return inspect(x, raiseerr=True)", "only inspect(x, raiseerr=False) is supported"),
        ("return json.dumps(x, cls=None)", "json.dumps(cls=) is not supported"),
        ("return unicodedata.normalize(form='NFC', unistr=x)", "two positional arguments"),
    ],
)
def test_library_options_rejected(tmp_path, capsys, body, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "import json\nimport unicodedata\n\nfrom fastapi import FastAPI\nfrom sqlalchemy import inspect\n\n"
        f"app = FastAPI()\n\n\n@app.get('/x')\nasync def x(x: str = 'a'):\n    {body}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err and "main.py:12" in err


def test_response_model_validator_blocks_its_routes(tmp_path):
    """A model validator outside the subset is reached by every route that validates or serializes the model:
    `--python-side auto` moves those routes (and only them), instead of a 500 at run time."""
    from py2axum.report import collect_dyn

    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent('''
        from fastapi import FastAPI
        from pydantic import BaseModel, model_validator
        import tempfile


        class Out(BaseModel):
            a: int = 1

            @model_validator(mode="before")
            @classmethod
            def odd(cls, data):
                tempfile.mkdtemp()
                return data


        class Nested(BaseModel):
            inner: Out


        app = FastAPI()


        @app.get("/direct", response_model=Out)
        async def direct():
            return {"a": 2}


        @app.get("/nested", response_model=Nested)
        async def nested():
            return {"inner": {"a": 2}}


        @app.post("/body")
        async def body(o: Out):
            return {}


        @app.get("/plain")
        async def plain():
            return {"a": 2}
    '''))
    _, per_route = collect_dyn(pkg, tmp_path, set())
    blocked = {info["path"] for info, errs in per_route if errs}
    assert blocked == {"/direct", "/nested", "/body"}


@pytest.mark.parametrize(
    "expr, message",
    [
        ("enumerate(x, begin=1)", "enumerate(begin=) is not supported (only start=, in that order)"),
        ("round(1.5, 1, ndigits=2)", "round(ndigits=) is not supported"),
        ("str(b'x', encoding='utf-8')", "str(encoding=) is not supported"),
    ],
)
def test_builtin_keywords_rejected(tmp_path, capsys, expr, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(f"from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x(x: str = 'ab'):\n    return {expr}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err and "main.py:8" in err


def test_custom_response_class_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\nfrom fastapi.responses import ORJSONResponse\n\napp = FastAPI()\n\n\n"
        "@app.get('/x', response_class=ORJSONResponse)\nasync def x():\n    return {}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "response_class=ORJSONResponse is not supported" in err and "main.py:7" in err


def test_uuid_as_str_column_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(
        "from sqlalchemy import Uuid\nfrom sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column\n\n\n"
        "class Base(DeclarativeBase):\n    pass\n\n\nclass T(Base):\n    __tablename__ = 't'\n"
        "    id: Mapped[str] = mapped_column(Uuid(as_uuid=False), primary_key=True)\n")
    (pkg / "main.py").write_text(MAIN.replace("from .models import Team", "from .models import T as Team"))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "Uuid(as_uuid=False) is not supported" in err and "models.py:11" in err


def test_http_basic_realm_must_be_literal(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import Depends, FastAPI\nfrom fastapi.security import HTTPBasic\n\nREALM = 'x'\n"
        "basic = HTTPBasic(realm=REALM)\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x(c=Depends(basic)):\n    return {}\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "HTTPBasic(realm=) must be a literal string" in err and "main.py:5" in err


VERSION_APP = """
from importlib.metadata import PackageNotFoundError, version

from fastapi import FastAPI

app = FastAPI()
try:
    V = version("My_App")
except PackageNotFoundError:
    V = "?"


@app.get("/v")
async def v():
    try:
        other = version({name})
    except PackageNotFoundError as e:
        other = str(e)
    return {{"v": V, "other": other}}
"""


def _version_project(tmp_path, name):
    (tmp_path / "pyproject.toml").write_text('[project]\nname = "my-app"\nversion = "1.2.3"\n')
    (tmp_path / "uv.lock").write_text('version = 1\n\n[[package]]\nname = "my-app"\nversion = "1.2.3"\n\n'
                                      '[[package]]\nname = "fastapi"\nversion = "0.142.2"\n')
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(VERSION_APP.format(name=name))
    return pkg


@pytest.mark.parametrize("name, expected", [('"FastAPI"', '"0.142.2"'), ('"requests"', "PACKAGE_NOT_FOUND")])
def test_distribution_version_from_pyproject_and_lock(tmp_path, name, expected):
    pkg = _version_project(tmp_path, name)
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert 'V::str("1.2.3")' in gen and expected in gen


def test_distribution_version_needs_literal(tmp_path, capsys):
    pkg = _version_project(tmp_path, "__name__")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "importlib.metadata.version() needs one literal distribution name" in err and "main.py:16" in err


def test_app_wrapped_in_asgi_class_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\n\n\nclass Quota:\n    def __init__(self, app):\n        self.app = app\n\n"
        "    async def __call__(self, scope, receive, send):\n        await self.app(scope, receive, send)\n\n\n"
        "api = FastAPI()\n\n\n@api.get('/x')\nasync def x():\n    return {}\n\n\napp = Quota(api)\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "the application wrapped in an ASGI class (`Quota(api)`) is not supported" in err and "main.py:20" in err


SENTRY_APP = '''
import sentry_sdk
from fastapi import FastAPI
from sentry_sdk.integrations.fastapi import FastApiIntegration

app = FastAPI()


@app.get("/x")
async def x():
    {stmt}
    return {{}}
'''


@pytest.mark.parametrize("stmt, msg", [
    ("sentry_sdk.init(dsn='http://k@h/1', transport=object)", "sentry_sdk.init(transport=...) is not supported: the binary sends envelopes"),
    ("sentry_sdk.init(before_breadcrumb=print)", "sentry_sdk.init(before_breadcrumb=...) is not supported"),
    ("sentry_sdk.init(profiles_sample_rate=1.0)", "profiling has no equivalent"),
    ("sentry_sdk.init(nope=1)", "sentry_sdk.init(nope=...) is not supported"),
    ("sentry_sdk.capture_message('m', scope=None)", "capture_message(scope=...) is not supported"),
    ("sentry_sdk.start_transaction(name='t')", "`sentry_sdk.start_transaction()` is not supported"),
    ("sentry_sdk.push_scope(lambda s: None)", "positional arguments, at most 0"),
    ("sentry_sdk.init(integrations=[FastApiIntegration(failed_request_status_codes={500})])",
     "FastApiIntegration(failed_request_status_codes=...) is not supported: 5xx only"),
    ("sentry_sdk.set_tag('k')", "missing required argument: 'value'"),
])
def test_sentry_outside_subset_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(SENTRY_APP.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err and "main.py:11" in err


def test_sentry_init_in_factory_not_translatable_rejected(tmp_path, capsys):
    """The app factory's `init_sentry()` runs at startup: a function it calls that does not translate is
    refused at transpile time, not left to fail when the binary starts."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent('''
        import sentry_sdk
        from fastapi import FastAPI


        def init_sentry():
            n = 0
            def bump():
                nonlocal n
            bump()
            sentry_sdk.init(dsn="")


        def create_app():
            init_sentry()
            app = FastAPI()

            @app.get("/x")
            async def x():
                return {}

            return app


        app = create_app()
    '''))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "`nonlocal` is not supported" in err and "main.py:9" in err



LIBS_MAIN = """
import enum

import xmltodict
from dateutil.relativedelta import relativedelta
from fastapi import FastAPI
from pydantic import BaseModel, PrivateAttr

app = FastAPI()


{body}


@app.get("/x")
async def x():
    return use()
"""


@pytest.mark.parametrize("body, msg, line", [
    ("def use():\n    return str(relativedelta(day=1))", "relativedelta(day=) is not supported", 13),
    ("def use():\n    return str(relativedelta(1, 2))", "relativedelta(dt1, dt2) is not supported", 13),
    ("def use():\n    return xmltodict.parse('<a/>', attr_prefix='$')", "xmltodict.parse(attr_prefix=) is not supported", 13),
    ("class M(BaseModel):\n    _p: list = PrivateAttr(default=[], init=False)\n\n\ndef use():\n    return M().model_dump()",
     "PrivateAttr() supports default= and default_factory= only", 13),
    ("class E(str, enum.Enum):\n    A = ('a', 'utf-8')\n\n\ndef use():\n    return E.A.value", "member value ('a', 'utf-8') is not supported", 12),
])
def test_library_ports_rejected(tmp_path, capsys, body, msg, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(LIBS_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert f"main.py:{line}" in err



MOUNT_APP = '''
from fastapi import FastAPI
from starlette.applications import Starlette

from . import other

sub = Starlette()
app = FastAPI()


@app.get("/x")
async def x():
    return {{}}


{tail}
'''


@pytest.mark.parametrize(
    "tail, other, flags, message, line",
    [
        # registered last, left to Python: relayed under its prefix
        ('app.mount("/sub/", sub)', "", ["--python-side", "mount"], None, None),
        ('app.mount("/sub", sub)\napp.mount("/", sub)', "", ["--python-side", "auto"], None, None),
        ('app.mount("/sub", sub)', "", [], "app.mount(...) is not translated: as the app's last registration "
         "it can stay on the Python side (--python-side mount, or --python-side auto)", 16),
        # a route after it would be shadowed in Python but served by the binary
        ('app.mount("/sub", sub)\n\n\n@app.get("/y")\nasync def y():\n    return {}',
         "", ["--python-side", "mount"], "`app.get(...)` at ", 16),
        ('app.mount("/sub", sub)\napp.add_api_route("/y", x)', "", ["--python-side", "auto"], "`app.add_api_route(...)` at ", 16),
        ('def setup():\n    app.mount("/sub", sub)', "", ["--python-side", "mount"], "is in another function", 17),
        ('app.mount("/sub", sub)', "from fastapi import APIRouter\n\nfrom .main import app\n\n\n@app.get('/z')\nasync def z():\n"
         "    return {}\n", ["--python-side", "mount"], "is in another module", 16),
    ],
)
def test_mount_left_to_python(tmp_path, capsys, tail, other, flags, message, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "other.py").write_text(other)
    (pkg / "main.py").write_text(MOUNT_APP.format(tail=tail))
    code = main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", *flags, "-o", str(tmp_path / "out")])
    err = capsys.readouterr().err
    if message is None:
        assert code == 0, err
        main_rs = (tmp_path / "out" / "src" / "main.rs").read_text()
        want = '&["", "/sub"]' if 'mount("/",' in tail else '&["/sub"]'
        assert f"set_python_side(&[], {want})" in " ".join(main_rs.split()), main_rs
        return
    assert code == 1
    assert message in err and f"main.py:{line}" in err, err



DDL_MODELS = '''
import enum
from sqlalchemy import Enum, ForeignKey, Index, Sequence, String, event
from sqlalchemy.orm import DeclarativeBase, Mapped, declared_attr, mapped_column
from sqlalchemy.types import TypeDecorator


class Base(DeclarativeBase):
    pass


class Color(enum.Enum):
    red = "r"


class Item(Base):
    __tablename__ = "items"
    id: Mapped[int] = mapped_column(primary_key=True)
    {extra}
'''

DDL_MAIN = '''
from contextlib import asynccontextmanager

from fastapi import FastAPI
from sqlalchemy.ext.asyncio import create_async_engine

from .models import Base

engine = create_async_engine("postgresql+psycopg://x/y")


@asynccontextmanager
async def lifespan(app):
    async with engine.begin() as conn:
        {call}
    yield


app = FastAPI(lifespan=lifespan)


@app.get("/x")
async def x():
    return {{}}
'''


def _ddl_project(tmp_path, extra="", call="await conn.run_sync(Base.metadata.create_all)", files=None):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(DDL_MODELS.format(extra=textwrap.indent(textwrap.dedent(extra), "    ").strip()))
    (pkg / "main.py").write_text(DDL_MAIN.format(call=call))
    for name, src in (files or {}).items():
        (pkg / name).write_text(src)
    return main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")])


def test_create_all_compiles_sqlalchemy_ddl(tmp_path):
    extra = '''
    name: Mapped[str] = mapped_column(String(20), index=True, comment="n")
    color: Mapped[Color]
    parent_id: Mapped[int | None] = mapped_column(ForeignKey("items.id", ondelete="CASCADE"))
    '''
    assert _ddl_project(tmp_path, extra) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert 'types: &[("color", "CREATE TYPE color AS ENUM (\'red\')")]' in gen
    assert "name VARCHAR(20) NOT NULL" in gen and "FOREIGN KEY(parent_id) REFERENCES items (id) ON DELETE CASCADE" in gen
    assert '"CREATE INDEX ix_items_name ON items (name)", "COMMENT ON COLUMN items.name IS \'n\'"' in gen


@pytest.mark.parametrize("extra, call, message, where", [
    ("", "await conn.run_sync(lambda c: None)", "only `run_sync(Base.metadata.create_all)` is supported", "main.py:15"),
    ("", "await conn.run_sync(Base.metadata.drop_all)", "only `run_sync(Base.metadata.create_all)` is supported", "main.py:15"),
    ("", "conn.run_sync(Base.metadata.create_all)", "without `await` does nothing", "main.py:15"),
    ("@declared_attr\ndef code(cls):\n    return mapped_column(String(5))", None, "@declared_attr is computed at class creation",
     "models.py:19"),
    ("n: Mapped[int] = mapped_column(Sequence('n_seq'))", None, "a Sequence (CREATE SEQUENCE) is not supported", "models.py:19"),
    ("c: Mapped[Color] = mapped_column(Enum(Color, values_callable=lambda x: [str(e) for e in x]))", None,
     "only `lambda x: [e.value for e in x]` is evaluated", "models.py:19"),
    ("v: Mapped[object]", None, "SQLAlchemy raised MappedAnnotationError", "models.py:16"),
    ("__table_args__ = {'schema': 'other'}", None, "schema='other'", "main.py:15"),
    ("n: Mapped[str] = mapped_column(server_default=str(5))", None, "is not evaluated at translation time", "models.py:19"),
])
def test_create_all_rejects(tmp_path, capsys, extra, call, message, where):
    code = _ddl_project(tmp_path, extra, call or "await conn.run_sync(Base.metadata.create_all)")
    err = capsys.readouterr().err
    assert code == 1 and message in err and where in err, err


@pytest.mark.parametrize("locked, virtual", [("2.1.3", True), ("2.0.46", False)])
def test_create_all_computed_column_follows_server_and_project_versions(tmp_path, capsys, locked, virtual):
    """A computed column without `persisted=`: SQLAlchemy 2.1 renders it bare (VIRTUAL) on PostgreSQL 18+ and
    STORED before, chosen by the binary from the server's version; a project on SQLAlchemy 2.0 always gets STORED."""
    (tmp_path / "uv.lock").write_text(f'[[package]]\nname = "sqlalchemy"\nversion = "{locked}"\n')
    src = DDL_MODELS.format(extra="d: Mapped[int] = mapped_column(Computed('id * 2'))").replace(
        "from sqlalchemy import ", "from sqlalchemy import Computed, ", 1)
    import sqlalchemy
    if virtual and tuple(int(x) for x in sqlalchemy.__version__.split(".")[:2]) < (2, 1):
        # translated by SQLAlchemy 2.0 (the CI's lowest end): it cannot render the project's 2.1 DDL
        assert _ddl_project(tmp_path, files={"models.py": src}) == 1
        assert "translate with SQLAlchemy >= 2.1" in capsys.readouterr().err
        return
    assert _ddl_project(tmp_path, files={"models.py": src}) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    if virtual:
        assert "d INTEGER GENERATED ALWAYS AS (id * 2) NOT NULL" in gen
        assert "create_pre18: Some(" in gen and "d INTEGER GENERATED ALWAYS AS (id * 2) STORED NOT NULL" in gen
    else:
        assert "create_pre18: None" in gen and "GENERATED ALWAYS AS (id * 2) STORED" in gen
        assert "GENERATED ALWAYS AS (id * 2) NOT NULL" not in gen


def test_create_all_fk_cycle_added_by_alter(tmp_path):
    extra = 'other_id: Mapped[int | None] = mapped_column(ForeignKey("others.id"))'
    other = '\n\nclass Other(Base):\n    __tablename__ = "others"\n    id: Mapped[int] = mapped_column(primary_key=True)\n' \
            '    item_id: Mapped[int | None] = mapped_column(ForeignKey("items.id"))\n'
    assert _ddl_project(tmp_path, files={"models.py": DDL_MODELS.format(extra=extra) + other}) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert 'alters: &["ALTER TABLE items ADD FOREIGN KEY(other_id) REFERENCES others (id)"]' in gen
    assert 'alters: &["ALTER TABLE others ADD FOREIGN KEY(item_id) REFERENCES items (id)"]' in gen


def test_create_all_rejects_ddl_listener(tmp_path, capsys):
    listener = '\n\n@event.listens_for(Base.metadata, "after_create")\ndef seed(target, conn, **kw):\n    pass\n'
    code = _ddl_project(tmp_path, files={"models.py": DDL_MODELS.format(extra="") + listener})
    err = capsys.readouterr().err
    assert code == 1 and "a DDL event listener is not reproduced" in err and "models.py:22" in err, err


def test_create_all_rejects_type_decorator_dialect_impl(tmp_path, capsys):
    td = ('\n\nclass Up(TypeDecorator):\n    impl = String\n    cache_ok = True\n\n'
          '    def load_dialect_impl(self, dialect):\n        return String(5)\n')
    src = DDL_MODELS.format(extra="u: Mapped[str] = mapped_column(Up)").replace("\n\nclass Item(Base)", td + "\n\nclass Item(Base)")
    code = _ddl_project(tmp_path, files={"models.py": src})
    err = capsys.readouterr().err
    assert code == 1 and "load_dialect_impl() changes the column type" in err, err


def test_create_all_rejects_model_imported_late(tmp_path, capsys):
    late = ('from sqlalchemy.orm import Mapped, mapped_column\n\nfrom .models import Base\n\n\n'
            'class Late(Base):\n    __tablename__ = "late"\n    id: Mapped[int] = mapped_column(primary_key=True)\n')
    lazy = 'async def later():\n    from . import late  # noqa: F401\n'
    code = _ddl_project(tmp_path, files={"late.py": late, "lazy.py": lazy})
    err = capsys.readouterr().err
    assert code == 1 and "only imported inside a function" in err and "late.py:6" in err, err


def test_factory_sentry_init_not_blamed_for_route_errors(tmp_path, capsys):
    """A function queued earlier (here by a module statement) that does not translate is not blamed on the
    factory's `init_sentry()`: only the functions that statement reaches count."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "letters.py").write_text(textwrap.dedent('''
        import reportlab.lib.styles


        def styled():
            return reportlab.lib.styles.ParagraphStyle("x")


        # module statements: their lambda queues styled() while the module is compiled
        GENERATORS = {}
        for _k in ("pdf",):
            GENERATORS[_k] = lambda: styled()
    '''))
    (pkg / "views.py").write_text(textwrap.dedent('''
        from fastapi import APIRouter

        from .letters import GENERATORS

        router = APIRouter()


        @router.get("/pdf")
        async def pdf():
            return {"s": str(GENERATORS["pdf"]())}
    '''))
    (pkg / "main.py").write_text(textwrap.dedent('''
        import sentry_sdk
        from fastapi import FastAPI

        from .views import router


        def init_sentry():
            sentry_sdk.init(dsn="")


        def create_app():
            init_sentry()
            app = FastAPI()
            app.include_router(router)
            return app


        app = create_app()
    '''))
    code = main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "--python-side", "auto", "-o", str(tmp_path / "out")])
    err = capsys.readouterr().err
    assert code == 0, err
    assert "app factory statement" not in err


def _letters_project(tmp_path, letters: str, views: str):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "letters.py").write_text(letters)
    (pkg / "views.py").write_text(textwrap.dedent(views))
    (pkg / "main.py").write_text(textwrap.dedent('''
        from fastapi import FastAPI

        from .views import router


        def create_app():
            app = FastAPI()
            app.include_router(router)
            return app


        app = create_app()
    '''))
    return pkg


_STYLED = 'import reportlab.lib.styles\n\n\ndef styled():\n    return reportlab.lib.styles.ParagraphStyle("x")\n\n\n'


@pytest.mark.parametrize("fill", [
    # filled in place by a module-level loop (a letter-generator registry)
    'GENERATORS = {}\nfor _k in ("pdf",):\n    GENERATORS[_k] = lambda: styled()\n',
    # filled by a module-level method call
    'GENERATORS = {}\nGENERATORS.update({"pdf": lambda: styled()})\n',
    # a module-level item assignment (a registry entry assigned by key)
    'GENERATORS = {}\nGENERATORS["pdf"] = lambda: styled()\n',
    # its own value: the second route reading it reaches styled() too
    'GENERATORS = {"pdf": lambda: styled()}\n',
])
def test_global_filled_with_untranslatable_function_blocks_readers(tmp_path, capsys, fill):
    """A function that does not translate, reachable only through a lambda stored in a module global,
    blocks every route reading that global (refused with file:line, moved by --python-side auto)."""
    pkg = _letters_project(tmp_path, _STYLED + fill, '''
        from fastapi import APIRouter

        from .letters import GENERATORS

        router = APIRouter()


        @router.get("/ok")
        async def ok():
            return {"n": len(GENERATORS)}


        @router.get("/pdf")
        async def pdf():
            return {"s": str(GENERATORS["pdf"]())}
    ''')
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "letters.py:5: library call `reportlab.lib.styles.ParagraphStyle()` is not supported" in err
    rep = tmp_path / "rep.md"
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "--report", str(rep)]) == 0
    data = json.loads(rep.with_suffix(".json").read_text())
    status = {r["path"]: r["status"] for r in data["routes"]}
    # reading the global may call what it holds: both readers are blocked, as the build refuses them
    assert status == {"/ok": "bloquée", "/pdf": "bloquée"}, status


def test_module_level_class_attribute_assignment_rejected(tmp_path, capsys):
    """`C.x = 7` at module level: class attributes are read as declared, the new value would be ignored."""
    pkg = _letters_project(tmp_path, "class C:\n    x = 1\n\n\nC.x = 7\nGENERATORS = {'pdf': C.x}\n", '''
        from fastapi import APIRouter

        from .letters import GENERATORS

        router = APIRouter()


        @router.get("/pdf")
        async def pdf():
            return GENERATORS
    ''')
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    assert "letters.py:5: assigning the class attribute `C.x` at module level is not supported" in capsys.readouterr().err


def test_module_level_assignment_to_an_unmapped_library_is_left_to_python(tmp_path, capsys):
    """`stripe.api_key = ...` at module level: the runtime maps nothing of stripe, no native code can read it, the
    statement is not run at startup (the routes using stripe are refused or --python-side)."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent('''
        import os

        import stripe
        from fastapi import FastAPI

        stripe.api_key = os.environ.get("STRIPE_API_KEY", "")
        app = FastAPI()


        @app.get("/x")
        async def x():
            return {"ok": True}
    '''))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 0, capsys.readouterr().err


@pytest.mark.parametrize(
    "classes, param, message",
    [
        ("class P(BaseModel):\n    page: int = 1\n\n    @field_validator('page')\n    @classmethod\n"
         "    def v(cls, x):\n        return x\n", "p: P = Depends()", "a class dependency with validators"),
        ("class P(BaseModel):\n    ids: list[int] = []\n", "p: P = Depends()", "a container field of a class dependency"),
        ("class P(BaseModel):\n    page: int = Field(1, alias='p')\n", "p: P = Depends()", "Field(alias=) is not supported in a class dependency"),
        ("class B(BaseModel):\n    page: int = 1\n\n\nclass P(B):\n    size: int = 2\n", "p: P = Depends()",
         "a class dependency must subclass BaseModel directly"),
        ("class P(BaseModel):\n    page: int = 1\n", "p: P = Depends(use_cache=False)", "Depends() options are not supported on a class dependency"),
    ],
)
def test_class_dependency_rejected(tmp_path, capsys, classes, param, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import Depends, FastAPI\nfrom pydantic import BaseModel, Field, field_validator\n\napp = FastAPI()\n\n\n"
        f"{classes}\n\n@app.get('/x')\nasync def x({param}):\n    return p\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


def test_super_cls_cls_with_subclass_rejected(tmp_path, capsys):
    """`super(cls, cls)` targets the parent of the class called: with a subclass, not the defining class's."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\nfrom pydantic import BaseModel\n\napp = FastAPI()\n\n\n"
        "class A(BaseModel):\n    n: int\n\n    @classmethod\n    def model_validate(cls, obj, **kw):\n"
        "        return super(cls, cls).model_validate(obj, **kw)\n\n\nclass B(A):\n    pass\n\n\n"
        "@app.post('/x')\nasync def x(body: dict):\n    return A.model_validate(body)\n")
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "`super(cls, cls)` in A, which B subclass" in err
    assert "main.py:" in err


def test_module_attribute_assignment_rejected(tmp_path, capsys):
    """`mod.ATTR = v` rebinds another module's global: refused (it used to fail at startup, read-only)."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "conf.py").write_text("LIMIT = 1\n")
    (pkg / "main.py").write_text(textwrap.dedent('''
        from fastapi import FastAPI
        from . import conf

        app = FastAPI()


        @app.get("/limit")
        async def limit():
            conf.LIMIT = 2
            return {"limit": conf.LIMIT}
    '''))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "assigning `conf.LIMIT` (an attribute of module proj.conf) is not supported" in err
    assert "main.py:" in err


NET_MAIN = """
import asyncio
import io
import socket
import zipfile

from fastapi import FastAPI

app = FastAPI()


async def use():
    {body}


@app.get("/x")
async def x():
    return await use()
"""


@pytest.mark.parametrize("body, msg", [
    ("return zipfile.ZipFile(io.BytesIO(), 'r')", 'only zipfile.ZipFile(file, "w", ...) is supported'),
    ("return zipfile.ZipFile(io.BytesIO(), 'w', strict_timestamps=False)", "zipfile.ZipFile(strict_timestamps=) is not supported"),
    ("return await asyncio.open_connection('h', 1, ssl=True)", "asyncio.open_connection(ssl=) is not supported"),
    ("return socket.create_connection(('h', 1), 5, ('', 0))", "socket.create_connection() takes at most 2 positional arguments"),
    ("return socket.create_connection(('h', 1), all_errors=True)", "socket.create_connection(all_errors=) is not supported"),
])
def test_network_and_archive_options_rejected(tmp_path, capsys, body, msg):
    """Socket, stream and zip calls: options with no native equivalent are refused, with their line."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(NET_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:13" in err


def test_before_validator_after_an_after_validator_rejected(tmp_path, capsys):
    """pydantic nests a field's validators in definition order; the runtime runs every `before` first."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent('''
        from fastapi import FastAPI
        from pydantic import BaseModel, field_validator

        app = FastAPI()


        class In(BaseModel):
            name: str

            @field_validator("name")
            @classmethod
            def after(cls, v):
                return v

            @field_validator("name", mode="before")
            @classmethod
            def before(cls, v):
                return v


        @app.post("/x")
        async def x(body: In):
            return body
    '''))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert 'a mode="before" validator defined after a mode="after" validator of the same field is not supported' in err
    assert "main.py:" in err


@pytest.mark.parametrize("opts, found", [("-B 10", "'-B'"), ("-c", "'-c'"), ("-c TimeZone=UTC --nope", "'--nope'")])
def test_engine_libpq_options_rejected(tmp_path, capsys, opts, found):
    """`connect_args={"options": ...}`: only the `-c`/`--` switches are applied to the pool's connections."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    engine = f'engine = create_async_engine("postgresql+psycopg://x/y", connect_args={{"options": {opts!r}}})'
    (pkg / "main.py").write_text(textwrap.dedent(MAIN).replace('engine = create_async_engine("postgresql+psycopg://x/y")', engine))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert f"main.py:7: connect_args options: only `-c name=value` and `--name=value` switches are supported (found {found})" in err


def test_engine_connect_args_generated(tmp_path):
    """A supported `options` string: the engine global runs at startup (session parameters for the pool)."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    engine = 'engine = create_async_engine("postgresql+psycopg://x/y", connect_args={"options": "-c TimeZone=UTC"})'
    (pkg / "main.py").write_text(textwrap.dedent(MAIN).replace('engine = create_async_engine("postgresql+psycopg://x/y")', engine))
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "engine_connect_args" in gen
    init = gen[gen.index("pub async fn init_globals"):]
    assert "g_proj_main__engine(cx)" in init[:init.index("\n}")]

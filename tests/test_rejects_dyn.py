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
        ('label: Mapped[str] = mapped_column("lbl", String(20))', "a SQL column name different from the attribute"),
        ("data: Mapped[dict] = mapped_column(JSON(astext_type=None, foo=1))", "JSON(foo=) is not supported"),
        ("codes: Mapped[list] = mapped_column(ARRAY(Integer))", "only ARRAY(String) is supported"),
        ("codes: Mapped[list] = mapped_column(ARRAY(String, as_tuple=True))", "ARRAY(as_tuple=) is not supported"),
        ("codes: Mapped[list] = mapped_column(JSON().with_variant(ARRAY(Integer), 'postgresql'))", "only ARRAY(String) is supported"),
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
        "from sqlalchemy import JSON, ForeignKey, Integer, String\nfrom sqlalchemy.dialects.postgresql import ARRAY\n"
        "from sqlalchemy.orm import column_property, validates\n"
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

"""Patterns found across a fleet of production FastAPI applications (anonymised): each is accepted with the
behaviour of the Python application, or refused with file:line when it would not be."""
import textwrap

import pytest

from py2axum.__main__ import main

MODELS = '''
from sqlalchemy import String
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


class Base(DeclarativeBase):
    pass


class Team(Base):
    __tablename__ = "teams"
    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(50))
'''

SESSION_MAIN = '''
from fastapi import Depends, FastAPI
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine

from .models import Team

engine = create_async_engine("postgresql+psycopg://x/y")
Session = async_sessionmaker(autocommit=False, autoflush=False, expire_on_commit=False, bind=engine)
app = FastAPI()


async def db():
{body}


@app.get("/teams/{{team_id}}")
async def team(team_id: int, s: AsyncSession = Depends(db)):
    t = await s.get(Team, team_id)
    return {{"name": t.name}}
'''


def project(tmp_path, main_src: str, models: str = MODELS):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(models))
    (pkg / "main.py").write_text(main_src)
    return pkg


def generate(tmp_path, pkg, *extra) -> str:
    out = tmp_path / "out"
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(out), *extra]) == 0
    return (out / "src" / "main.rs").read_text()


@pytest.mark.parametrize(
    "body, commit",
    [
        # outer try: the handler and the finally run after the `with` closed the session (no-ops on a closed one)
        ("    try:\n        async with Session() as db:\n            yield db\n    except:\n        await db.rollback()\n"
         "        raise\n    finally:\n        db.expunge_all()\n        await db.aclose()", False),
        ("    try:\n        async with Session() as db:\n            yield db\n    except:\n        await db.rollback()\n"
         "        raise\n    finally:\n        await db.aclose()", False),
        # commit after the yield, directly in the `with`
        ("    try:\n        async with Session() as db:\n            yield db\n            await db.commit()\n    except:\n"
         "        await db.rollback()\n        raise\n    finally:\n        db.expunge_all()\n        await db.close()", True),
        # commit in the try's else
        ("    async with Session() as db:\n        try:\n            yield db\n        except Exception:\n"
         "            await db.rollback()\n            raise\n        else:\n            await db.commit()", True),
        ("    async with Session() as db:\n        try:\n            yield db\n        finally:\n            db.expunge_all()", False),
    ],
)
def test_session_dependency_variants_accepted(tmp_path, body, commit):
    pkg = project(tmp_path, SESSION_MAIN.format(body=body))
    rs = generate(tmp_path, pkg)
    assert f"commit_after: {str(commit).lower()}" in rs


@pytest.mark.parametrize(
    "body",
    [
        # the outer handler swallows the error
        "    try:\n        async with Session() as db:\n            yield db\n    except:\n        await db.rollback()",
        # something else than a cleanup in the finally
        "    try:\n        async with Session() as db:\n            yield db\n    finally:\n        await db.commit()",
        # the else does more than commit
        "    async with Session() as db:\n        try:\n            yield db\n        except Exception:\n"
        "            await db.rollback()\n            raise\n        else:\n            await db.commit()\n            await db.refresh(1)",
        # expunge_all awaited (it is not a coroutine)
        "    async with Session() as db:\n        try:\n            yield db\n        finally:\n            await db.expunge_all()",
    ],
)
def test_session_dependency_variants_refused(tmp_path, capsys, body):
    pkg = project(tmp_path, SESSION_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "session dependency db" in err and "main.py:" in err


DEPS_MAIN = '''
from fastapi import APIRouter, Depends, FastAPI, HTTPException


async def auth():
    raise HTTPException(status_code=401)


async def admin():
    return True

{decl}
router = APIRouter()


@router.post("/x", dependencies={use})
async def x():
    return {{"ok": 1}}


app = FastAPI()
app.include_router(router)
'''


@pytest.mark.parametrize("decl, use", [
    ("ADMIN = [Depends(auth), Depends(admin)]", "ADMIN"),
    ("BASE = [Depends(auth)]\nADMIN = BASE + [Depends(admin)]", "ADMIN"),
    ("BASE = (Depends(auth),)", "[*BASE]"),
])
def test_route_dependencies_from_a_constant_are_kept(tmp_path, decl, use):
    """`dependencies=ADMIN_ONLY` (a module constant): its dependencies run, as in FastAPI; they were dropped."""
    pkg = project(tmp_path, DEPS_MAIN.format(decl=decl, use=use))
    if use.startswith("[*"):
        assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
        return
    generate(tmp_path, pkg)
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "f_proj_main__auth" in gen and "f_proj_main__admin" in gen


@pytest.mark.parametrize("decl", [
    "ADMIN = [Depends(auth)]\nADMIN.append(Depends(admin))",
    "def make():\n    return [Depends(auth)]\nADMIN = make()",
])
def test_route_dependencies_not_static_refused(tmp_path, capsys, decl):
    """Never served without them: a list changed after its definition, or built by a call, is refused."""
    pkg = project(tmp_path, DEPS_MAIN.format(decl=decl, use="ADMIN"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert "must be a list of Depends(...) read statically" in capsys.readouterr().err


def test_object_getattribute_skips_getattr(tmp_path):
    """`object.__getattribute__(obj, "_x")` (a proxy check): compiled, without the class's __getattr__ fallback."""
    src = '''
from fastapi import FastAPI


class Proxy:
    def __init__(self):
        self._x = 1

    def __getattr__(self, name):
        return 42


def has_x(o) -> bool:
    try:
        object.__getattribute__(o, "_y")
    except AttributeError:
        return False
    return True


app = FastAPI()


@app.get("/p")
async def p():
    return {"y": has_x(Proxy()), "x": object.__getattribute__(Proxy(), "_x")}
'''
    pkg = project(tmp_path, src)
    generate(tmp_path, pkg)
    assert "methods::getattribute(cx" in (tmp_path / "out" / "src" / "gen.rs").read_text()


def test_except_a_module_tuple_of_classes(tmp_path):
    """`except FAILURES as err:` with `FAILURES = (MyError, KeyError, TimeoutError)` a module constant."""
    src = '''
from fastapi import FastAPI


class MyError(Exception):
    pass


FAILURES = (MyError, KeyError, TimeoutError)
app = FastAPI()


@app.get("/e")
async def e(k: str):
    try:
        {}[k]
    except FAILURES as err:
        return {"caught": type(err).__name__}
'''
    pkg = project(tmp_path, src)
    generate(tmp_path, pkg)
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "isinstance(&dynrt::v::KEY_ERROR)" in gen and "TIMEOUT_ERROR" in gen


M2M = '''
from sqlalchemy import BigInteger, ForeignKey
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, relationship


class Base(DeclarativeBase):
    pass


class Link(Base):
    __tablename__ = "link"
    a_id = mapped_column(ForeignKey("a.id"), primary_key=True, type_=BigInteger)
    b_id = mapped_column(ForeignKey("b.id"), primary_key=True, type_=BigInteger)


class A(Base):
    __tablename__ = "a"
    id: Mapped[int] = mapped_column(BigInteger, primary_key=True)
    bs = relationship("B", secondary="link", back_populates="as_")


class B(Base):
    __tablename__ = "b"
    id: Mapped[int] = mapped_column(BigInteger, primary_key=True)
    as_ = relationship("A", secondary="link", back_populates="bs")
'''

M2M_MAIN = '''
from fastapi import Depends, FastAPI
from sqlalchemy import select
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine

from .models import A, B

engine = create_async_engine("postgresql+psycopg://x/y")
Session = async_sessionmaker(engine)
app = FastAPI()


async def db():
    async with Session() as s:
        yield s


@app.get("/orphans")
async def orphans(s: AsyncSession = Depends(db)):
    rows = (await s.execute(select(A.id).where(~A.bs.any()))).scalars().all()
    {extra}
    return list(rows)
'''


@pytest.mark.parametrize("extra, ok", [("", True), ("(await s.get(A, 1)).bs.append(B(id=2))", False)])
def test_many_to_many_back_populates_through_a_mapped_table(tmp_path, capsys, extra, ok):
    """`secondary="link"` naming a mapped class's table, back_populates on both sides: accepted while no code
    changes the collections in memory (SQLAlchemy would keep the other side in step)."""
    pkg = project(tmp_path, M2M_MAIN.format(extra=extra), models=M2M)
    rc = main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")])
    assert rc == (0 if ok else 1)
    if not ok:
        assert "changed in memory" in capsys.readouterr().err


CONTEXT_MAIN = '''
from fastapi import FastAPI
from starlette_context import context, plugins
from starlette_context.errors import ContextDoesNotExistError
from starlette_context.middleware import RawContextMiddleware

app = FastAPI()
app.add_middleware(RawContextMiddleware, plugins=(plugins.RequestIdPlugin(),))


def user_id():
    try:
        return context.get("user_id")
    except ContextDoesNotExistError:
        return None


@app.get("/c")
async def c():
    {body}
'''


@pytest.mark.parametrize("body", [
    'context["user_id"] = 7\n    return {"rid": context["X-Request-ID"], "user": user_id(), "n": len(context)}',
    'return {"has": "user_id" in context, "keys": sorted(context), "data": context.data, "exists": context.exists()}',
    'context.update(a=1)\n    context.setdefault("b", 2)\n    del context["a"]\n    return {**context, "b": context.pop("b")}',
    'return [k for k in context if context]',
])
def test_starlette_context_as_a_mapping(tmp_path, body):
    """starlette_context's `context`: the request's context dict, read and written by the endpoints."""
    pkg = project(tmp_path, CONTEXT_MAIN.format(body=body))
    generate(tmp_path, pkg)
    assert "ctxmw::data(cx)" in (tmp_path / "out" / "src" / "gen.rs").read_text()


@pytest.mark.parametrize("body, msg", [
    ("return context", "not handed on as the UserDict object itself"),
    ("return repr(context)", "not handed on as the UserDict object itself"),
    ("return context.popitem()", "`starlette_context.context.popitem()` is not supported"),
])
def test_starlette_context_refused(tmp_path, capsys, body, msg):
    pkg = project(tmp_path, CONTEXT_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err and "main.py:" in err and ("P2A0201" in err or "P2A0202" in err)


COPY_MAIN = '''
import copy
from copy import deepcopy

from fastapi import FastAPI

app = FastAPI()


class Cfg:
    def __init__(self):
        self.opts = {{"a": [1]}}
{extra}

DEFAULTS = {{"limits": [1, 2]}}


@app.get("/c")
async def c():
    d = deepcopy(DEFAULTS)
    d["limits"].append(3)
    return {{"d": d, "s": copy.copy(Cfg()).opts, "x": copy.deepcopy(x=[1])}}
'''


def test_copy_module(tmp_path):
    """copy.copy / copy.deepcopy (a module default copied per request): dynrt/copymod.rs."""
    pkg = project(tmp_path, COPY_MAIN.format(extra=""))
    generate(tmp_path, pkg)
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "copymod::deepcopy(" in gen and "copymod::copy(" in gen


def test_copy_with_a_class_copy_method_refused(tmp_path, capsys):
    """A class of its own `__deepcopy__`: copy would call it; refused, never ignored."""
    pkg = project(tmp_path, COPY_MAIN.format(extra="\n    def __deepcopy__(self, memo):\n        return self\n"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert "__deepcopy__" in capsys.readouterr().err


SPAN_MAIN = '''
import os

from fastapi import FastAPI

HOST = os.getenv("HOST")
SEEN = HOST
{between}
if not HOST:
    HOST = "localhost"

app = FastAPI()


@app.get("/h")
async def h():
    return {{"host": HOST, "seen": SEEN}}
'''


def test_module_variable_bound_twice(tmp_path):
    """`X = os.getenv(...)` then `if not X: X = ...` at module level: the statements from the first binding to
    the last run as one block at startup (SEEN keeps the value read in between)."""
    pkg = project(tmp_path, SPAN_MAIN.format(between=""))
    generate(tmp_path, pkg)
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "ms_proj_main__6" in gen


def test_module_variable_bound_twice_with_a_call_between_refused(tmp_path, capsys):
    pkg = project(tmp_path, SPAN_MAIN.format(between="print(HOST)"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert "bound by several module-level statements" in capsys.readouterr().err


def test_sync_and_async_session_dependencies_in_one_app(tmp_path):
    """A sync `Session` dependency next to an `AsyncSession` one: each route makes its session with its own
    dependency's configuration (the first one seen is the application's default)."""
    src = SESSION_MAIN.format(body="    async with Session() as s:\n        yield s") + '''

from sqlalchemy import create_engine
from sqlalchemy.orm import Session as SyncSession, sessionmaker

sync_maker = sessionmaker(bind=create_engine("postgresql+psycopg://x/y"))


def sync_db():
    s = sync_maker()
    try:
        yield s
    finally:
        s.close()


@app.get("/sync/{team_id}")
def sync_team(team_id: int, s: SyncSession = Depends(sync_db)):
    return {"name": s.get(Team, team_id).name}
'''
    pkg = project(tmp_path, src)
    generate(tmp_path, pkg)
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "dynrt::session(cx)" in gen and "dynrt::session_with(cx, false, true, true, true)" in gen


INSTR_MAIN = '''
from fastapi import FastAPI
from prometheus_fastapi_instrumentator import Instrumentator

app = FastAPI()
{setup}


@app.get("/items/{{item_id}}")
async def item(item_id: int):
    return {{"id": item_id}}
'''


@pytest.mark.parametrize("setup", [
    "Instrumentator().instrument(app).expose(app)",
    "instrumentator = Instrumentator(excluded_handlers=['/metrics']).instrument(app, metric_namespace='x')\n\n\n"
    "@app.on_event('startup')\nasync def _startup():\n    instrumentator.expose(app, include_in_schema=False)",
])
def test_prometheus_fastapi_instrumentator(tmp_path, setup):
    """`Instrumentator().instrument(app).expose(app)`: its middleware in the stack, `/metrics` added (dynrt/instr.rs)."""
    pkg = project(tmp_path, INSTR_MAIN.format(setup=setup))
    generate(tmp_path, pkg)
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "dynrt::instr::instrument(" in gen and "dynrt::instr::take_pending()" in gen and "dynrt::instr::expose(" in gen


def test_several_route_decorators(tmp_path):
    """`@app.get("/a")` over `@app.post("/b")`: one route each, the lowest decorator registered first."""
    src = '''
from fastapi import FastAPI

app = FastAPI()


@app.get("/a")
@app.post("/b", status_code=201)
async def x():
    return {"ok": 1}
'''
    pkg = project(tmp_path, src)
    generate(tmp_path, pkg)
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert gen.index('pattern: "^/b$"') < gen.index('pattern: "^/a$"')


def test_settings_value_as_a_constraint(tmp_path):
    """`Query(le=settings.MAX_PAGE)`, `Field(max_length=settings.NAME_LEN)`: read when the application starts."""
    (tmp_path / "proj").mkdir()
    (tmp_path / "proj" / "settings.py").write_text(
        "from pydantic_settings import BaseSettings\n\n\nclass Settings(BaseSettings):\n    max_page: int = 100\n"
        "    name_len: int = 20\n\n\nSETTINGS = Settings()\n")
    src = '''
from fastapi import FastAPI, Query
from pydantic import BaseModel, Field

from .settings import SETTINGS

app = FastAPI()


class M(BaseModel):
    name: str = Field(max_length=SETTINGS.name_len)


@app.get("/x")
async def x(limit: int = Query(10, le=SETTINGS.max_page)):
    return {"limit": limit}


@app.post("/y")
async def y(m: M):
    return m
'''
    (tmp_path / "proj" / "__init__.py").write_text("")
    (tmp_path / "proj" / "models.py").write_text("")
    (tmp_path / "proj" / "main.py").write_text(src)
    generate(tmp_path, tmp_path / "proj")
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "late: Some(&LB_1)" in gen and "pub async fn lb_2" in gen


@pytest.mark.parametrize("field, ok", [("at: datetime", True), ("at: datetime | None = None", False)])
def test_json_encoders(tmp_path, capsys, field, ok):
    """`model_config json_encoders={datetime: fn}`: the JSON-mode serializer of a datetime field; inside Optional,
    refused (pydantic applies it to the datetime member)."""
    src = f'''
from datetime import datetime

from fastapi import FastAPI
from pydantic import BaseModel, ConfigDict

app = FastAPI()


class M(BaseModel):
    model_config = ConfigDict(json_encoders={{datetime: lambda v: v.isoformat()}})
    {field}


@app.get("/m", response_model=M)
async def m():
    return M(at=datetime(2026, 1, 2))
'''
    pkg = project(tmp_path, src)
    rc = main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")])
    assert rc == (0 if ok else 1)
    if ok:
        assert "json_enc_at" in (tmp_path / "out" / "src" / "gen.rs").read_text()
    else:
        assert "json_encoders for `datetime` inside" in capsys.readouterr().err


def test_printed_warning(tmp_path):
    """`warnings.warn(msg)` (UserWarning) and a builtin category CPython prints: `file:line: Category: message` and
    the source line on stderr, once per message, category and line (the default filters' "default" action)."""
    src = '''
import warnings

from fastapi import FastAPI

app = FastAPI()


@app.get("/w")
async def w(n: int = 0):
    warnings.warn(f"careful {n}")
    warnings.warn("later", FutureWarning, stacklevel=1)
    return {"ok": 1}
'''
    pkg = project(tmp_path, src)
    generate(tmp_path, pkg)
    gen = "".join((tmp_path / "out" / "src" / "gen.rs").read_text().split())
    assert 'warn_print("UserWarning"' in gen and '"proj/main.py",11,"warnings.warn(f\\"careful{n}\\")"' in gen
    assert 'warn_print("FutureWarning"' in gen


def test_sqlmodel_template_patterns(tmp_path):
    """The official full-stack template: a table model deriving from a SQLModel base with `datetime` columns
    (SQLModel's UTCDateTime), `sqlmodel_update(data, update=...)` and `Table.model_validate(x, update=...)` in another
    module, `col()`, `session.exec(statement)` of a `delete(...)`, pwdlib with Argon2 and bcrypt hashers."""
    models = '''
import uuid
from datetime import datetime, timezone

from sqlmodel import Field, SQLModel


def now() -> datetime:
    return datetime.now(timezone.utc)


class ItemBase(SQLModel):
    title: str = Field(min_length=1, max_length=255)


class ItemCreate(ItemBase):
    pass


class Item(ItemBase, table=True):
    id: uuid.UUID = Field(default_factory=uuid.uuid4, primary_key=True)
    created_at: datetime | None = Field(default_factory=now)
    secret: str = ""
'''
    src = '''
import uuid

from fastapi import FastAPI
from pwdlib import PasswordHash
from pwdlib.hashers.argon2 import Argon2Hasher
from pwdlib.hashers.bcrypt import BcryptHasher
from sqlmodel import Session, col, create_engine, delete

from .models import Item, ItemBase, ItemCreate

engine = create_engine("postgresql+psycopg://u@h/db")
password_hash = PasswordHash((Argon2Hasher(), BcryptHasher()))
app = FastAPI()


@app.post("/items")
def create(item_in: ItemCreate) -> ItemBase:
    with Session(engine) as session:
        item = Item.model_validate(item_in, update={"secret": password_hash.hash("x")})
        session.add(item)
        session.commit()
        return item


@app.patch("/items/{item_id}")
def update(item_id: uuid.UUID, item_in: ItemCreate) -> ItemBase:
    with Session(engine) as session:
        item = session.get(Item, item_id)
        item.sqlmodel_update(item_in.model_dump(exclude_unset=True), update={"secret": "s"})
        ok, _ = password_hash.verify_and_update("x", item.secret)
        session.add(item)
        session.commit()
        return item


@app.delete("/items/{item_id}")
def remove(item_id: uuid.UUID) -> dict:
    with Session(engine) as session:
        statement = delete(Item).where(col(Item.id) == item_id)
        session.exec(statement)
        session.commit()
        return {"ok": True}
'''
    pkg = project(tmp_path, src, models)
    generate(tmp_path, pkg, "--allow-untested-versions")
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "ColTy::UtcDateTime" in gen and "pwd_new(" in gen and 'pwd_hasher("bcrypt"' in gen


def test_enum_base_without_members_and_field_extras(tmp_path):
    """`class AppEnum(StrEnum): pass` as a base of the app's enums (Python allows deriving from an enum without
    members), and pydantic's `Field(True, nullable=True)` (an extra keyword: json_schema_extra, the schema only)."""
    src = '''
from enum import StrEnum

from fastapi import FastAPI
from pydantic import BaseModel, Field


class AppEnum(StrEnum):
    """common base"""

    pass


class Status(AppEnum):
    active = "Active"
    closed = "Closed"


class Item(BaseModel):
    status: Status = Status.active
    enabled: bool | None = Field(True, nullable=True)


app = FastAPI()


@app.post("/items")
async def items(item: Item):
    return {"status": item.status, "upper": item.status.upper(), "enabled": item.enabled}
'''
    pkg = project(tmp_path, src)
    generate(tmp_path, pkg)


def test_enum_base_with_members_still_refused(tmp_path, capsys):
    src = '''
from enum import StrEnum

from fastapi import FastAPI


class AppEnum(StrEnum):
    a = "A"


class Status(AppEnum):
    b = "B"


app = FastAPI()


@app.get("/s")
async def s(x: Status):
    return {"x": x}
'''
    pkg = project(tmp_path, src)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert "subclassing an enum is not supported" in capsys.readouterr().err


def test_sentry_asgi_middleware_is_transparent(tmp_path):
    """`app.add_middleware(SentryAsgiMiddleware)` over sentry-sdk 2's automatic FastAPI integration: applied once,
    the binary reporting as that integration does."""
    src = '''
import sentry_sdk
from fastapi import FastAPI
from sentry_sdk.integrations.asgi import SentryAsgiMiddleware

sentry_sdk.init(dsn=None)
app = FastAPI()
app.add_middleware(SentryAsgiMiddleware)


@app.get("/x")
async def x():
    return {"ok": True}
'''
    pkg = project(tmp_path, src)
    generate(tmp_path, pkg)


def test_middleware_list_from_a_factory(tmp_path):
    """`FastAPI(middleware=make_middleware())`: a function of the module returning a list of Middleware(...)."""
    src = '''
from fastapi import FastAPI
from fastapi.middleware import Middleware
from fastapi.middleware.cors import CORSMiddleware


def make_middleware() -> list[Middleware]:
    middleware = [
        Middleware(CORSMiddleware, allow_origins=["*"], allow_methods=["*"]),
    ]
    return middleware


app = FastAPI(middleware=make_middleware())


@app.get("/x")
async def x():
    return {"ok": True}
'''
    pkg = project(tmp_path, src)
    main_rs = generate(tmp_path, pkg)
    assert "Cors::new" in main_rs or "Cors::new" in (tmp_path / "out" / "src" / "gen.rs").read_text()


@pytest.mark.parametrize("name, alias", [("created_at", "createdAt"), ("recipe_ingredient_id", "recipeIngredientId"),
                                         ("id", "id"), ("HTTPCode", "HTTPCode")])
def test_humps_alias_generator(name, alias):
    """pyhumps's camelize as an alias generator (mealie): ported, computed at translation"""
    from py2axum import pydstatic

    assert pydstatic.LIBRARY["humps.main.camelize"](name) == alias
    assert pydstatic.LIBRARY["humps.camelize"] is pydstatic.LIBRARY["humps.main.camelize"]


_MW_MODULE = '''
from fastapi import FastAPI
from fastapi.middleware import Middleware
from fastapi.middleware.cors import CORSMiddleware

middleware = [
    Middleware(CORSMiddleware, allow_origins=["*"], allow_methods=["*"]),
]
{extra}

def create_app() -> FastAPI:
    app = FastAPI(middleware=middleware)

    @app.get("/x")
    async def x():
        return {{"ok": True}}

    return app


app = create_app()
'''


def test_middleware_list_of_the_module_in_a_factory(tmp_path):
    """`middleware = [...]` at module level, `FastAPI(middleware=middleware)` in the app factory (grillazz)"""
    pkg = project(tmp_path, _MW_MODULE.format(extra=""))
    generate(tmp_path, pkg)
    assert "Cors::new" in (tmp_path / "out" / "src" / "gen.rs").read_text() + (tmp_path / "out" / "src" / "main.rs").read_text()


@pytest.mark.parametrize("extra", ["def g():\n    middleware.append(None)\n", "middleware = []", "def f(middleware):\n    return middleware\n"])
def test_middleware_list_of_the_module_changed_refused(tmp_path, capsys, extra):
    pkg = project(tmp_path, _MW_MODULE.format(extra=extra))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert "middleware=" in capsys.readouterr().err


_SHARED_CONFIG = '''
from fastapi import FastAPI
from pydantic import BaseModel, ConfigDict

config = ConfigDict(str_strip_whitespace=True, extra="forbid")
{extra}

class A(BaseModel):
    model_config = config
    name: str


app = FastAPI()


@app.post("/a")
async def a(x: A) -> A:
    return x
'''


def test_model_config_shared_by_name(tmp_path):
    """`config = ConfigDict(...)` then `model_config = config` in several models (grillazz)"""
    pkg = project(tmp_path, _SHARED_CONFIG.format(extra=""))
    generate(tmp_path, pkg)


@pytest.mark.parametrize("extra", ['config["extra"] = "allow"', "config.update(frozen=True)", "config = ConfigDict()"])
def test_model_config_shared_but_changed_refused(tmp_path, capsys, extra):
    pkg = project(tmp_path, _SHARED_CONFIG.format(extra=extra))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert "model_config" in capsys.readouterr().err


_FACTORY_PREFIX = '''
from fastapi import APIRouter, FastAPI

from .settings import settings

router = APIRouter()


@router.get("/x")
async def x():
    return {{"ok": True}}


def create_app({param}) -> FastAPI:
    app = FastAPI()
    app.include_router(router, prefix={prefix})
    return app


app = create_app({arg})
'''


@pytest.mark.parametrize("param, prefix, arg, ok", [
    ("", "settings.API_PREFIX", "", True),
    ("", "settings.API_PREFIX + '/v1'", "", True),
    # a parameter, a local: what they hold is known only when the factory runs
    ("prefix: str", "prefix", "settings.API_PREFIX", False),
    ("", "p", "", False),
])
def test_include_router_prefix_from_settings_in_the_app_factory(tmp_path, capsys, param, prefix, arg, ok):
    """`include_router(r, prefix=settings.API_PREFIX)` in the app factory, `settings` a module global: read when the
    application starts, as at module level (FastAPI-Backend-Template, the realworld example app)"""
    src = _FACTORY_PREFIX.format(param=param, prefix=prefix, arg=arg)
    if prefix == "p":
        src = src.replace("    app = FastAPI()\n", "    app = FastAPI()\n    p = settings.API_PREFIX\n")
    pkg = project(tmp_path, src)
    (pkg / "settings.py").write_text("from pydantic_settings import BaseSettings\n\n\nclass Settings(BaseSettings):\n"
                                     "    API_PREFIX: str = '/api'\n\n\nsettings = Settings()\n")
    rc = main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")])
    err = capsys.readouterr().err
    assert rc == (0 if ok else 1), err
    if not ok:
        assert "prefix=<non-literal>" in err


_AUTH_APP = '''
from fastapi import FastAPI, Request{ws_import}
from starlette.authentication import AuthCredentials, AuthenticationBackend, SimpleUser
from starlette.middleware.authentication import AuthenticationMiddleware


class Backend(AuthenticationBackend):
    async def authenticate(self, conn):
        token = conn.headers.get("x-user")
        return (AuthCredentials(["read"]), SimpleUser(token)) if token else None


app = FastAPI()
app.add_middleware(AuthenticationMiddleware, backend=Backend())


@app.get("/me")
async def me(request: Request):
    return {{"name": request.user.display_name, "scopes": request.auth.scopes}}
{ws}'''


def test_starlette_authentication_middleware(tmp_path):
    """Starlette's AuthenticationMiddleware: a raw ASGI class translated from Starlette's source (shim), the
    backend's user and credentials read through request.user / request.auth"""
    pkg = project(tmp_path, _AUTH_APP.format(ws_import="", ws=""))
    generate(tmp_path, pkg)
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "_py2axum_shim_starlette_auth" in gen or "AuthenticationMiddleware" in gen


def test_starlette_authentication_middleware_with_websockets_refused(tmp_path, capsys):
    ws = '\n\n@app.websocket("/ws")\nasync def ws(websocket: WebSocket):\n    await websocket.accept()\n'
    pkg = project(tmp_path, _AUTH_APP.format(ws_import=", WebSocket", ws=ws))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert "WebSocket routes" in capsys.readouterr().err

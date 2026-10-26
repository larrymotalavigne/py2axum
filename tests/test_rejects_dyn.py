"""dyn backend: constructions outside the subset are refused with file:line, never approximated."""
import json
import sys
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
        ('members: Mapped[list["User"]] = relationship(secondary="team_users")', "secondary= must name a module-level Table"),
        ('ghost: Mapped["Nope"] = relationship("Nope")', "relationship target 'Nope': unknown mapped class"),
        ('members: DynamicMapped["User"] = relationship(foreign_keys=[owner_id])', "DynamicMapped[...] is not supported"),
        ('owner: WriteOnlyMapped["User"] = relationship(foreign_keys=[owner_id], passive_deletes=True)',
         "a write-only many-to-one relationship is not supported"),
        ('owner: Mapped["User"] = relationship(foreign_keys=[owner_id], lazy="write_only", back_populates="x")',
         "back_populates= on a write-only collection is not supported"),
        # primaryjoin=: a foreign key equal to the column it references, nothing else
        ('owner: Mapped["User"] = relationship(primaryjoin="Team.owner_id == User.id")', "primaryjoin= must be `column == column`"),
        ('owner: Mapped["User"] = relationship(primaryjoin=lambda: Team.owner_id == User.id)', "primaryjoin= must be `column == column`"),
        ('owner: Mapped["User"] = relationship(primaryjoin=name == User.id)', "must equate a foreign key with the column it references"),
        ('owner: Mapped["User"] = relationship(primaryjoin=owner_id != User.id)', "primaryjoin= must be `column == column`"),
        ('owner: Mapped["User"] = relationship(primaryjoin=owner_id == User.id, foreign_keys=[backup_id])',
         "foreign_keys= names another column than primaryjoin="),
        ('parent: Mapped["Team"] = relationship(primaryjoin=parent_id == id)', "primaryjoin= is only supported between two classes"),
        # post_update=: the many-to-one side only (SQLAlchemy's advice)
        ('children: Mapped[list["Team"]] = relationship(post_update=True)', "post_update=True is only supported on a many-to-one"),
    ],
)
def test_relationship_rejected(tmp_path, capsys, rel, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", rel))
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "models.py:" in err


M2M_TABLES = '''

from sqlalchemy import Column, Table  # noqa: E402

team_users = Table("team_users", Base.metadata, Column("team_id", ForeignKey("teams.id"), primary_key=True),
                   Column("user_id", ForeignKey("users.id"), primary_key=True))
bad_users = Table("bad_users", Base.metadata, Column("team_id", ForeignKey("teams.id"), primary_key=True))
'''


@pytest.mark.parametrize(
    "rel, message",
    [
        # the pair's other side missing (SQLAlchemy raises at configuration)
        ('members: Mapped[list["User"]] = relationship(secondary=team_users, back_populates="teams")',
         "back_populates='teams' must name a many-to-many relationship of User"),
        ('members: Mapped[list["User"]] = relationship(secondary="team_users", backref="teams")',
         "backref= on a many-to-many relationship is not supported"),
        ('members: Mapped[list["User"]] = relationship(secondary=team_users, cascade="all, delete-orphan")',
         "cascade delete / delete-orphan / passive_deletes on a many-to-many"),
        ('members: Mapped[list["User"]] = relationship(secondary=bad_users)', "needs one foreign key to teams and one to users"),
        ('members: Mapped[list["User"]] = relationship(secondary=team_users, order_by="User.id")',
         "secondary= with order_by= is not supported"),
    ],
)
def test_many_to_many_rejected(tmp_path, capsys, rel, message):
    """relationship(secondary=): what is not modelled is refused (instrumented collections kept in step, cascades)."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", rel) + M2M_TABLES)
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err


def test_many_to_many_translates(tmp_path):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace(
        "{rel}", 'members: Mapped[list["User"]] = relationship(secondary=team_users)') + M2M_TABLES)
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


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
        # `raise` of another exception than the caught one: not the endpoint's error any more
        ("", "    s = Session()\n    try:\n        yield s\n    except Exception as exc:\n        await s.rollback()\n"
             "        raise RuntimeError(exc)\n    finally:\n        await s.close()", "session dependency db: only"),
        # the session assigned, its close forgotten
        ("", "    s: AsyncSession = Session()\n    try:\n        yield s\n    finally:\n        pass",
         "session dependency db: only"),
        # a factory called with arguments
        ("", "    factory = Session\n    async with factory(bind=engine) as s:\n        yield s", "session dependency db: only"),
    ],
)
def test_session_dependency_rejected(tmp_path, capsys, opts, body, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    (pkg / "main.py").write_text(SESSION_MAIN.format(opts=opts, body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


MANAGER_MAIN = '''
from contextlib import asynccontextmanager

from fastapi import Depends, FastAPI
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine

from .models import Team

engine = create_async_engine("postgresql+psycopg://x/y")


class Manager:
    def __init__(self):
        self.maker = None

{methods}


manager = Manager()


@asynccontextmanager
async def lifespan(_app):
{life}
    yield


app = FastAPI(lifespan=lifespan)


async def db():
    async with manager.maker() as s:
        yield s


@app.get("/teams/{{team_id}}")
async def team(team_id: int, s: AsyncSession = Depends(db)):
    return {{"name": (await s.get(Team, team_id)).name}}
'''

INIT = "    async def init(self):\n        self.maker = async_sessionmaker(engine)"


@pytest.mark.parametrize(
    "methods, life, message",
    [
        (INIT, "    pass", "set by Manager.init(), which the application's lifespan does not call before its yield"),
        ("    async def init(self, on=True):\n        if on:\n            self.maker = async_sessionmaker(engine)",
         "    await manager.init()", "set conditionally in Manager.init"),
        (INIT + "\n\n    def other(self):\n        self.maker = async_sessionmaker(engine, expire_on_commit=False)",
         "    await manager.init()", "set more than once in its class"),
        (INIT, "    await manager.init()\n    manager.maker = None", "assigned outside its class"),
    ],
)
def test_instance_session_maker_rejected(tmp_path, capsys, methods, life, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    (pkg / "main.py").write_text(MANAGER_MAIN.format(methods=methods, life=life))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
        # the exception swallowed (FastAPI then answers otherwise)
        ("sessionmaker(bind=engine)", "def db():\n    try:\n        s = Maker()\n        yield s\n    except Exception as e:\n"
         "        print(e)\n    finally:\n        s.close()", "s: Session = Depends(db)", "session dependency db: only"),
        # a maker built by a project function from its parameter: its options are not known
        ("make(False)\n\n\ndef make(flush):\n    return sessionmaker(bind=engine, autoflush=flush)",
         "def db():\n    with Maker() as s:\n        yield s", "s: Session = Depends(db)", "expected a literal value here"),
    ],
)
def test_sync_session_dependency_rejected(tmp_path, capsys, maker, dep, param, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    (pkg / "main.py").write_text(SYNC_MAIN.format(maker=maker, dep=dep, param=param))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "decl, message",
    [
        ("total = column_property(id + 1)", "only columns and relationships are supported"),
        ('__mapper_args__ = {"version_id_col": id}', "__mapper_args__ key 'version_id_col' is not supported"),
        ('__mapper_args__ = {"polymorphic_on": "nope"}', "polymorphic_on='nope' is not a column of the class"),
        ('__mapper_args__ = {"polymorphic_identity": 3}', "polymorphic_identity=3 is not supported"),
        ('__mapper_args__ = {"eager_defaults": 1.5}', "eager_defaults=1.5 is not supported"),
        ("data: Mapped[dict] = mapped_column(JSON(astext_type=None, foo=1))", "JSON(foo=) is not supported"),
        ("codes: Mapped[list] = mapped_column(ARRAY(Integer))", "only ARRAY(String) is supported"),
        ("codes: Mapped[list] = mapped_column(ARRAY(String, as_tuple=True))", "ARRAY(as_tuple=) is not supported"),
        ("codes: Mapped[list] = mapped_column(JSON().with_variant(ARRAY(Integer), 'postgresql'))", "only ARRAY(String) is supported"),
        ("@functools.cache\n    def double(self):\n        return self.id * 2", "decorator @functools.cache is not supported"),
        ("@validates('name')\n    def check(self, key, value):\n        return value", "decorator @validates('name') is not supported"),
        ("blob: Mapped[bytes] = deferred(Column(LargeBinary), active_history=True)",
         "only deferred(Column(...), group=, raiseload=) is supported"),
        ("pos: Mapped[int] = composite(mapped_column('a'), return_none_on=None)", "option return_none_on= is not supported"),
        ("pos = composite('name')", "the class is needed"),
        ("pos: Mapped[User] = composite(mapped_column('a'))", "the column a needs a type"),
        ("pos: Mapped[User] = composite(User, name.op('+'))", "is not supported (columns, their names or attribute names)"),
    ],
)
def test_model_declaration_rejected(tmp_path, capsys, decl, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    models = textwrap.dedent(MODELS).replace("{rel}", decl).replace(
        "from sqlalchemy import ForeignKey, String",
        "from sqlalchemy import JSON, ForeignKey, Integer, String\nfrom sqlalchemy.dialects.postgresql import ARRAY\n"
        "from sqlalchemy import Column, LargeBinary\nfrom sqlalchemy.orm import column_property, composite, deferred, validates\n"
        "from sqlalchemy.ext.hybrid import hybrid_property")
    (pkg / "models.py").write_text(models)
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "models.py:" in err


JOINED_SUB = ('kind: Mapped[str]\n    __mapper_args__ = {"polymorphic_on": "kind"}',
              'class Squad(Team):\n    __tablename__ = "squads"\n    id: Mapped[int] = mapped_column(ForeignKey("teams.id"), '
              'primary_key=True)\n    __mapper_args__ = {"polymorphic_identity": "s"}')


@pytest.mark.parametrize("stmt, message", [
    ('await s.execute(pg_insert(Squad).on_conflict_do_nothing())', "insert(Squad) of a joined table inheritance subclass"),
    ("await s.scalars(select(aliased(Squad)))", "aliased(Squad) of a joined table inheritance subclass"),
    ("await s.scalars(select(with_polymorphic(Team, [Squad], aliased=True, selectable=None)))",
     "selectable= and polymorphic_on= are not supported"),
    ("await s.scalars(select(Team).options(selectin_polymorphic(Team, [Squad], extra=1)))",
     "selectin_polymorphic(Base, [Sub, ...]) only"),
])
def test_joined_inheritance_construct_rejected(tmp_path, capsys, stmt, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", JOINED_SUB[0]) + "\n\n" + JOINED_SUB[1] + "\n")
    main_src = textwrap.dedent(MAIN).replace("from .models import Team", "from sqlalchemy import insert, select\n"
                                             "from sqlalchemy.dialects.postgresql import insert as pg_insert\n"
                                             "from sqlalchemy.orm import aliased, selectin_polymorphic, with_polymorphic\nfrom .models import Team, Squad")
    main_src = main_src.replace("    t = await s.get(Team, team_id)\n", f"    {stmt}\n    t = await s.get(Team, team_id)\n")
    (pkg / "main.py").write_text(main_src)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "base_extra, sub, message",
    [
        ("", 'class Squad(Team):\n    __mapper_args__ = {"polymorphic_identity": "s"}',
         "inheritance needs polymorphic_on in the __mapper_args__ of Team"),
        ('kind: Mapped[str]\n    __mapper_args__ = {"polymorphic_on": "kind"}',
         'class Squad(Team):\n    __tablename__ = "squads"\n    __mapper_args__ = {"polymorphic_identity": "s"}',
         "a joined subclass needs its primary key, a foreign key to the parent's key"),
        ('kind: Mapped[str]\n    __mapper_args__ = {"polymorphic_on": "kind"}',
         'class Squad(Team):\n    __tablename__ = "squads"\n    id: Mapped[int] = mapped_column(primary_key=True)\n'
         '    __mapper_args__ = {"polymorphic_identity": "s"}',
         "column id redefined in a subclass is not supported (only the key, as a foreign key to teams.id)"),
        ('kind: Mapped[str]\n    __mapper_args__ = {"polymorphic_on": "kind"}', "class Squad(Team):\n    size: Mapped[int]",
         "a mapped subclass needs a polymorphic_identity"),
        ('kind: Mapped[str]\n    __mapper_args__ = {"polymorphic_on": "kind"}',
         'class Squad(Team):\n    __mapper_args__ = {"polymorphic_identity": "s", "polymorphic_on": "kind"}',
         "polymorphic_on on a subclass is not supported"),
        ('kind: Mapped[str]\n    __mapper_args__ = {"polymorphic_on": "kind"}',
         'class Squad(Team):\n    __mapper_args__ = {"polymorphic_identity": "s", "polymorphic_load": "joined"}',
         "polymorphic_load='joined' is not supported"),
    ],
)
def test_inheritance_rejected(tmp_path, capsys, base_extra, sub, message):
    """Mapped class inheritance: single table only, with a discriminator and an identity per subclass."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", base_extra) + "\n\n" + sub + "\n")
    (pkg / "main.py").write_text(textwrap.dedent(MAIN).replace("from .models import Team", "from .models import Team, Squad")
                                 .replace("s.get(Team, team_id)", "s.get(Squad, team_id)"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "`String` in kind() does not mean the same" in err
    assert "kinds.py:5" in err


@pytest.mark.parametrize(
    "body, message",
    [
        ("    it = iter([1, 2])\n    return {'a': next(it)}", "iter() stored in a variable is not supported"),
        ("    xs = [3, 1]\n    return {'a': xs.frobnicate()}", "method .frobnicate() is not implemented by the runtime"),
        ("    d = {}\n    return {'a': d.model_dump(round_trip=True)}", ".model_dump(round_trip=) is not supported"),
        ("    xs = [3, 1]\n    return {'a': xs.frobnicated}", "attribute .frobnicated is not provided by the runtime"),
        ("    import os\n    os.environ['X'] = '1'\n    return {}", "writing into os.environ is not supported"),
        # integer constants beyond 64 bits: CPython makes a big integer, the runtime raises OverflowError
        ("    return {'a': 10**100}", "integer expression out of 64-bit range"),
        ("    return {'a': (1 << 70) - 1}", "integer expression out of 64-bit range"),
        ("    return {'a': 2**62 * 4}", "integer expression out of 64-bit range"),
        ("    return {'a': [10**x for x in (3, 6, 100)]}", "integer expression out of 64-bit range (with x = 100)"),
        # strftime directives glibc and macOS render differently (or %s, the local zone): refused, never copied
        ("    import datetime\n    return {'a': datetime.date(2026, 1, 2).strftime('%-d/%m')}", "strftime directive %-d in strftime()"),
        ("    import datetime\n    return {'a': datetime.datetime(2026, 1, 2).strftime('%s')}", "strftime directive %s in strftime()"),
        ("    import datetime\n    return {'a': datetime.datetime(2026, 1, 2).strftime('%Ey')}", "strftime directive %Ey in"),
        ("    import datetime\n    return {'a': datetime.datetime(2026, 1, 2).strftime('%q')}", "strftime directive %q in"),
        ("    import datetime\n    return {'a': f'{datetime.date(2026, 1, 2):%v}'}", "strftime directive %v in an f-string format spec"),
        ("    import datetime\n    return {'a': datetime.time(1, 2).strftime('%:M')}", "strftime directive %:M in"),
    ],
)
def test_python_semantics_rejected(tmp_path, capsys, body, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x():\n" + body + "\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize("body", [
    "    import orjson\n    try:\n        orjson.loads('[')\n    except orjson.JSONDecodeError as e:\n"
    "        return {'a': [e.pos, e.lineno, e.colno, e.msg]}\n    return {}",
    "    from unidecode import UnidecodeError, unidecode\n    try:\n        unidecode('\\ue000', errors='strict')\n"
    "    except UnidecodeError as e:\n        return {'a': e.index}\n    return {}",
])
def test_exception_attributes_set_by_the_runtime_accepted(tmp_path, body):
    """attributes the runtime sets on an exception instance (`attrs.insert("colno".into(), ..)`) are known to the
    attribute check: libapp alone was refused on `.colno` (another fixture of dynapp used to mask it)"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x():\n" + body + "\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


@pytest.mark.parametrize(
    "call, message",
    [
        ("await s.execute(insert(Team).values(name='x').on_conflict_do_nothing(), [{'name': 'a'}])",
         "execute(insert(...).on_conflict_do_nothing(...), parameters)"),
        ("stmt = update(Team).returning(Team.id)\n    await s.execute(stmt, [{'id': 1, 'name': 'a'}])",
         "execute(update(...).returning(...), parameters) is not supported"),
        ("await s.scalars(select(Team), [{'name': 'a'}])", ".scalars(statement, parameters) is not supported"),
        ("await s.scalar(text('SELECT :x'), {'x': 1})", ".scalar(statement, parameters) is not supported"),
    ],
)
def test_execute_with_parameters_rejected(tmp_path, capsys, call, message):
    """executemany / ORM bulk INSERT and UPDATE: refused at translation (the runtime takes parameters for text()
    only; it used to fail at run time with a 500)."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    main_py = textwrap.dedent(MAIN).replace("from .models import Team", "from sqlalchemy import delete, insert, select, text, update\n"
                                            "from .models import Team")
    main_py = main_py.replace("    t = await s.get(Team, team_id)\n", f"    {call}\n    t = await s.get(Team, team_id)\n")
    (pkg / "main.py").write_text(main_py)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "call, message",
    [
        ("await s.scalars(select(Team).options(load_only(Team.name, foo=1)))", "load_only(): only the raiseload= keyword"),
        ("await s.scalars(select(Team).options(defer(Team.name, Team.id)))", "defer() takes exactly one positional argument"),
        ("await s.scalars(select(Team).options(selectinload(Team.owner, recursion_depth=2)))",
         "selectinload() takes exactly one positional argument"),
        ("await s.scalars(select(Team).options(undefer_group('g', 'h')))", "undefer_group() takes exactly one positional"),
        ("await s.execute(select(Team).where(Team.name == bindparam('n', type_=None)), {'n': 'a'})",
         "bindparam(type_=) is supported with ARRAY(String()) only"),
        ("await s.execute(select(Team).where(Team.name == bindparam('n', required=True)), {'n': 'a'})",
         "bindparam(key, value=None, type_=ARRAY(String())): other arguments are not supported"),
        ("await s.scalars(select(aliased(Team, adapt_on_names=True)))", "aliased(Model, subquery) and flat= only"),
        ("await s.scalars(select(Team).where(with_parent(Team.owner)))", "with_parent(instance, Parent.relationship) only"),
    ],
)
def test_loader_option_arguments_rejected(tmp_path, capsys, call, message):
    """Column and relationship loader options: an argument the runtime would not honour is refused, never ignored."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", 'owner: Mapped["User"] = relationship(foreign_keys=[owner_id])'))
    main_py = textwrap.dedent(MAIN).replace("from .models import Team", "from sqlalchemy import bindparam, select\n"
                                            "from sqlalchemy.orm import aliased, defer, load_only, selectinload, undefer_group, with_parent\n"
                                            "from .models import Team")
    main_py = main_py.replace("    t = await s.get(Team, team_id)\n", f"    {call}\n    t = await s.get(Team, team_id)\n")
    (pkg / "main.py").write_text(main_py)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize("body, message", [
    # one value both iterated (introspection) and joined (Core): two different runtime values
    ("    t = Team.__table__\n    names = [c.name for c in t.columns]\n    return {'x': str(select(t)), 'n': names}",
     "Model.__table__ used both as a Core table and for its introspection"),
    ("    t = Team.__table__\n    helper(t)\n    return {'x': str(t.c.id)}",
     "Model.__table__ used both as a Core table and for its introspection (or given to other code)"),
    ("    return {'x': str(Squad.__table__.join(Team.__table__))}",
     "Squad.__table__: the Core use of the table of an inheriting class is not supported"),
])
def test_model_table_core_use_rejected(tmp_path, capsys, body, message):
    """Model.__table__ is introspection, or a Core table when the code uses it as one; never both for one value."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", JOINED_SUB[0]) + "\n\n" + JOINED_SUB[1] + "\n")
    (pkg / "main.py").write_text("from fastapi import FastAPI\nfrom sqlalchemy import select\nfrom .models import Squad, Team\n\n"
                                 "app = FastAPI()\n\n\ndef helper(t):\n    return t\n\n\n"
                                 "@app.get('/x')\nasync def x():\n" + body + "\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert message in capsys.readouterr().err


def test_session_in_endpoint_options_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    main_py = textwrap.dedent(MAIN).replace("    t = await s.get(Team, team_id)\n",
                                            "    async with AsyncSession(engine, info={}) as s2:\n        pass\n"
                                            "    t = await s.get(Team, team_id)\n")
    (pkg / "main.py").write_text(main_py)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "AsyncSession() is supported with the engine" in err and "P2A0505" in err


HYBRID = '''
    @hybrid_property
    def label(self):
        return self.name
'''


@pytest.mark.parametrize("extra, message", [
    ("    @label.inplace.update_expression\n    def _u(cls, value):\n        return []\n", "@label.inplace.update_expression is not supported"),
    ("    @label.inplace.expression\n    def _e(cls):\n        return cls.name\n\n    @label.inplace.comparator\n    def _c(cls):\n"
     "        return None\n", "a hybrid takes one @label.expression or @label.comparator"),
    ("    @label.inplace.getter\n    def _g(self):\n        return self.name\n", "@label.inplace.getter is not supported"),
    ("    @label.expression\n    def other(cls):\n        return cls.name\n", "needs `.inplace`"),
])
def test_hybrid_rejected(tmp_path, capsys, extra, message):
    """sqlalchemy.ext.hybrid: expression, comparator and setter only."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    models = textwrap.dedent(MODELS).replace("{rel}", "").replace(
        "from sqlalchemy.orm import", "from sqlalchemy.ext.hybrid import hybrid_property\nfrom sqlalchemy.orm import", 1)
    models = models.replace('    name: Mapped[str] = mapped_column(String(50))\n', '    name: Mapped[str] = mapped_column(String(50))\n'
                            + HYBRID + extra, 1)
    (pkg / "models.py").write_text(models)
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert message in capsys.readouterr().err


def test_execute_text_with_parameters_translates(tmp_path):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    main_py = textwrap.dedent(MAIN).replace("from .models import Team", "from sqlalchemy import text\nfrom .models import Team")
    main_py = main_py.replace("    t = await s.get(Team, team_id)\n",
                              "    stmt = text('SELECT 1 WHERE 1 = :x')\n    await s.execute(stmt, {'x': 1})\n"
                              "    t = await s.get(Team, team_id)\n")
    (pkg / "main.py").write_text(main_py)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


@pytest.mark.parametrize("imp, base", [("from sqlalchemy.ext import hybrid", "hybrid.Comparator"),
                                        ("import sqlalchemy.ext.hybrid as h", "h.Comparator[str]")])
def test_hybrid_comparator_translates(tmp_path, capsys, imp, base):
    """sqlalchemy.ext.hybrid.Comparator reached through its module: the class translated from py2axum's source
    (its methods, `self.__clause_element__()`, known to the attribute check)"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    models = textwrap.dedent(MODELS).replace("{rel}", "").replace("from sqlalchemy.orm import", imp + "\nfrom sqlalchemy import func\nfrom sqlalchemy.ext.hybrid import hybrid_property\nfrom sqlalchemy.orm import", 1)
    models = models.replace('    name: Mapped[str] = mapped_column(String(50))\n', '    name: Mapped[str] = mapped_column(String(50))\n'
                            + HYBRID + "    @label.inplace.comparator\n    @classmethod\n    def _c(cls):\n        return CI(cls.name)\n", 1)
    models += f"\n\nclass CI({base}):\n    def __eq__(self, other):\n        return func.lower(self.__clause_element__()) == func.lower(other)\n"
    (pkg / "models.py").write_text(models)
    main_py = textwrap.dedent(MAIN).replace("from .models import Team", "from sqlalchemy import select\nfrom .models import Team")
    main_py = main_py.replace("    t = await s.get(Team, team_id)\n", "    await s.execute(select(Team).filter_by(label='x'))\n"
                              "    t = await s.get(Team, team_id)\n")
    (pkg / "main.py").write_text(main_py)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0, capsys.readouterr().err
    assert "not implemented by the runtime" not in (tmp_path / "out" / "src" / "gen.rs").read_text()


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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
        ("    model_config = ConfigDict(alias_generator=lambda n: n.encode().hex())\n    a: int",
         "alias generator: `n.encode().hex()` cannot be evaluated at translation time"),
        ("    model_config = ConfigDict(alias_generator=str.upper)\n    a: int", "alias generator `str.upper` is not supported"),
        ("    model_config = ConfigDict(validate_by_alias=False)\n    a: int = Field(alias='A')",
         "validate_by_alias=False requires validate_by_name=True"),
        ("    a: int = Field(validation_alias=AliasPath(0, 'x'))", "AliasPath(...): a key first"),
        ("    a: int | str = Field(strict=True)", "Field(strict=True) on `int | str` is not supported"),
        ("    model_config = ConfigDict(strict=True)\n    a: list[int] = Field(strict=False)",
         "Field(strict=False) on `list[int]` in a strict model is not supported"),
        ("    a: str = Field(multiple_of=2)", "multiple_of= does not apply to `str`"),
        ("    a: Decimal = Field(multiple_of=2)", "Field(multiple_of=) on a Decimal is not supported"),
        ("    a: int = Field(union_mode='random')", "Field(union_mode=) must be 'smart' or 'left_to_right'"),
        ("    p: Cat | Dog = Field(discriminator=Discriminator(lambda v: 'c'))", "needs a Tag('...')"),
        ("    p: Cat | Plain = Field(discriminator='kind')", "Plain.kind must be a Literal[...] of strings"),
        ("    p: Cat | Num = Field(discriminator='kind')", "Num.kind tags other than strings are not supported"),
        ("    p: Cat | Cat2 = Field(discriminator='kind')", "tag 'c' is in two members"),
        ("    model_config = ConfigDict(alias_generator=Dog.model_config['alias_generator'])\n    a: int",
         "`Dog.model_config['alias_generator']`: that model has no alias generator"),
        ("    model_config = ConfigDict(**{'strict': True})\n    a: int", "model_config: `{'strict': True}` is not supported"),
        ("    model_config = {**dict(strict=True)}\n    a: int", "model_config: `dict(strict=True)` is not supported"),
        ("    model_config = ConfigDict(allow_inf_nan=True)\n    a: float", "allow_inf_nan= other than False is not supported"),
        ("    model_config = ConfigDict(validate_default=True)\n    a: float = Field(default_factory=lambda: 1)",
         "validate_default=True with a computed default is not supported"),
    ],
)
def test_schema_declaration_rejected(tmp_path, capsys, body, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from decimal import Decimal\nfrom functools import cached_property\nfrom typing import Literal\n"
        "from fastapi import FastAPI\nfrom pydantic import AliasPath, BaseModel, ConfigDict, Discriminator, Field, computed_field\n\n"
        "app = FastAPI()\n\n\n"
        "class Cat(BaseModel):\n    kind: Literal['c']\n\n\nclass Cat2(BaseModel):\n    kind: Literal['c', 'd']\n\n\n"
        "class Dog(BaseModel):\n    kind: Literal['d']\n\n\nclass Plain(BaseModel):\n    kind: str\n\n\n"
        "class Num(BaseModel):\n    kind: Literal[1]\n\n\n"
        f"class In(BaseModel):\n{body}\n\n\n@app.post('/x')\nasync def x(b: In):\n    return b\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "prelude, config, message",
    [
        ("", "ConfigDict(**Base.model_config, strict=False)",
         "ConfigDict() got multiple values for keyword argument 'strict'"),
        ("def gen(name):\n    while name:\n        return name\n    return 'x'\n", "ConfigDict(alias_generator=gen)",
         "cannot be evaluated at translation time"),
        ("def gen(name):\n    return helper(name)\n", "ConfigDict(alias_generator=gen)", "cannot be evaluated at translation time"),
        ("G = 'g'\n\n\ndef gen(name):\n    global G\n    return G + name\n", "ConfigDict(alias_generator=gen)",
         "cannot be evaluated at translation time"),
    ],
)
def test_config_from_project_rejected(tmp_path, capsys, prelude, config, message):
    """`**Other.model_config`, generators of several statements: what cannot be exact is refused"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\nfrom pydantic import BaseModel, ConfigDict\n\napp = FastAPI()\n\n\n" + prelude
        + "\n\nclass Base(BaseModel):\n    model_config = ConfigDict(strict=True)\n\n\n"
        f"class In(BaseModel):\n    model_config = {config}\n    some_field: int\n\n\n"
        "@app.post('/x')\nasync def x(b: In):\n    return b\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "copies a variable that a function rebinds" in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "decl, message",
    [
        ("@dataclass(slots=True)\nclass C:\n    a: int\n", "@dataclass(slots=) is not supported"),
        ("@dataclass\nclass C:\n    a: int\n\n    def __index__(self):\n        return 1\n", "method __index__ is not supported"),
        # a base class: one plain class (SQLAlchemy's hybrid Comparator), without class attributes
        ("@dataclass\nclass C(Exception):\n    a: int\n", "base classes are not supported (a plain class only)"),
        ("class P:\n    k = 1\n\n\n@dataclass(eq=False)\nclass C(P):\n    a: int\n", "a base class with class attributes"),
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "models.py:" in err


@pytest.mark.parametrize("dep_kw, message", [
    ('scope="function"', 'dependency outer has a scope of "request", it cannot depend on dependencies with scope "function"'),
    ("scope=SCOPE", 'Depends(scope=) must be the literal "function" or "request"'),
])
def test_generator_dependency_scope_rejected(tmp_path, capsys, dep_kw, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import Depends, FastAPI\n\napp = FastAPI()\nSCOPE = 'function'\n\n\nasync def inner():\n"
        "    yield 1\n\n\n"
        f"async def outer(i: int = Depends(inner, {dep_kw})):\n    yield i\n\n\n"
        "@app.get('/x')\nasync def x(v: int = Depends(outer)):\n    return {'v': v}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


def test_form_in_dependency_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import Depends, FastAPI, Form\n\napp = FastAPI()\n\n\nasync def dep(x: str = Form(...)):\n    return x\n\n\n"
        "@app.post('/x')\nasync def x(v: str = Depends(dep)):\n    return {'v': v}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
        # Middleware(GZipMiddleware) in the list is the binary's outermost layer, as add_middleware's: once only
        ("from fastapi.middleware import Middleware\nfrom fastapi.middleware.gzip import GZipMiddleware\n"
         "app.add_middleware(GZipMiddleware)\napp2 = FastAPI(middleware=[Middleware(GZipMiddleware)])\n",
         "GZipMiddleware registered twice is not supported"),
        # a list changed after its definition is not read statically (an unchanged one is: test_fleet_patterns)
        ("mws = []\nmws.append(None)\napp2 = FastAPI(middleware=mws)\n",
         "FastAPI(middleware=...) must be a literal list of Middleware(...)"),
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "@lru_cache combined with @deco is not supported" in err
    assert "main.py:" in err


METHOD_DECOS = [
    # a property's deleter, a setter without its property, a decorator class (descriptor), the plain decorators beside a value one
    ("    @property\n    def x(self):\n        return 1\n\n    @x.deleter\n    def x(self):\n        pass\n",
     "main.py:12: C.x: decorator @x.deleter is not supported"),
    ("    @x.setter\n    def x(self, v):\n        pass\n", "decorator @x.setter is not supported without a `@property def x`"),
    ("    @Descr\n    def x(self):\n        return 1\n", "decorator @Descr is not supported (it is a class: a descriptor"),
    ("    @classmethod\n    @deco\n    def x(cls):\n        return 1\n", "@classmethod combined with @deco is not supported"),
    ("    @functools.cached_property\n    @deco\n    def x(self):\n        return 1\n", "decorator @cached_property is not supported beside another decorator"),
]


@pytest.mark.parametrize("body,message", METHOD_DECOS)
def test_method_decorators_rejected(tmp_path, capsys, body, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "import functools\nfrom fastapi import FastAPI\n\napp = FastAPI()\n\n\n"
        "def deco(f):\n    return f\n\n\nclass Descr:\n    def __init__(self, f):\n        pass\n\n\n"
        "class C:\n" + body + "\n\n@app.get('/x')\nasync def x():\n    return {'v': C().x}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message.replace("main.py:12: ", "") in err
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
        ('"json"', "importlib.import_module('json'): the module is neither a module of the project nor installed"),
    ],
)
def test_import_module_rejected(tmp_path, capsys, arg, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(IMPORT_MAIN.format(arg=arg))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
        ('next(string.Formatter().parse("{a}"))', "parse() inside list() or tuple()"),
    ],
)
def test_collections_rejected(tmp_path, capsys, expr, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(COLLS_MAIN.format(expr=expr))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    ("jwt.decode(token, KEY, ['ES256'])", "jwt.decode(): algorithm 'ES256' is not supported"),
    ("jwt.encode({'a': 1}, KEY, json_encoder=None)", "jwt.encode(json_encoder=) is not supported"),
    ("jwt.decode(token, KEY, algorithms=['HS256'], detached_payload=b'x')", "jwt.decode(detached_payload=) is not supported"),
    ("jwt.decode(token, KEY, algorithms=['HS256'], verify=True)", "jwt.decode(verify=) is not supported"),
    ("jwt.decode(token, KEY, ['HS256'], None, True)", "jwt.decode(): verify is not supported"),
    ("jwt.decode(token, KEY, algorithms=['HS256'], foo=1)", "jwt.decode(foo=) is not supported"),
])
def test_pyjwt_outside_subset_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PYJWT_MAIN.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    # (a single `configure(app)` that includes routers is native: test_router_configure_function)
    ("app.add_middleware(CORSMiddleware, allow_origins=['*'])",
     "add_middleware(CORSMiddleware) outside the app factory is not supported"),
])
def test_configure_function_registrations_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "router.py").write_text("from fastapi import APIRouter\n\nr = APIRouter()\n\n\n@r.get('/x')\nasync def x():\n    return {}\n")
    (pkg / "main.py").write_text(CONFIGURE_MAIN.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "include_router(prefix=<non-literal>) inside a function (app factory) is not supported" in err
    assert "main.py:16" in err


ROUTER_GUARD = """
import os

from fastapi import APIRouter, FastAPI

try:
    from proj import opt
except ImportError:
    opt = None

app = FastAPI()
r = APIRouter()


@r.get("/x")
async def x():
    return {}


if %s:
    app.include_router(%s)
"""


@pytest.mark.parametrize("present,test,arg,message,line", [
    # the project's own module, absent: ImportError decided statically, the guarded registration never runs
    (False, "opt is not None", "opt.router", None, None),
    (False, "opt is None", "r", None, None),
    (True, "opt is not None", "opt.router", None, None),
    (True, "opt", "opt.router", None, None),
    # read once at startup (module level, the environment and the settings)
    (False, 'os.environ.get("X")', "r", None, None),
    (False, 'os.environ.get("X", "a").lower() == "b" and opt is None', "r", None, None),
    # a test that calls the project's code (or anything else): refused
    (False, "x()", "r", "include_router(...) under an `if` is not supported", 16),
])
def test_router_import_guard(tmp_path, capsys, present, test, arg, message, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    if present:
        (pkg / "opt.py").write_text('from fastapi import APIRouter\n\nrouter = APIRouter()\n\n\n@router.get("/o")\n'
                                    'async def o():\n    return {}\n')
    (pkg / "main.py").write_text(ROUTER_GUARD % (test, arg))
    rc = main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")])
    err = capsys.readouterr().err
    if message is None:
        assert rc == 0, err
        return
    assert rc == 1
    assert message in err and f"main.py:{line}" in err


ROUTER_CONFIGURE = """
from fastapi import APIRouter, FastAPI

a = APIRouter()
b = APIRouter()


@a.get("/a")
async def ra():
    return {}


@b.get("/b")
async def rb():
    return {}


ROUTERS = %s


def add_routers(app):
    for r in ROUTERS:
        app.include_router(r)


app = FastAPI()
%s
"""


@pytest.mark.parametrize("routers,calls,message,line", [
    ("(a, b)", "add_routers(app)", None, None),
    ("[a, b]", "add_routers(app)", None, None),
    ("[a, b]\nROUTERS.append(a)", "add_routers(app)", "include_router: cannot resolve `r` to an APIRouter", 24),
    ("(a, b)", "add_routers(app)\nadd_routers(app)", "on the application passed to a function is not supported", 23),
    ("(a, b)", "add_routers(app=app)\nother = FastAPI()\nadd_routers(other)", "on the application passed to a function is not supported", 23),
])
def test_router_configure_function(tmp_path, capsys, routers, calls, message, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(ROUTER_CONFIGURE % (routers, calls))
    rc = main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")])
    err = capsys.readouterr().err
    if message is None:
        assert rc == 0, err
        return
    assert rc == 1
    assert message in err and f"main.py:{line}" in err, err


def test_router_default_response_class_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("""
from fastapi import APIRouter, FastAPI
from fastapi.responses import RedirectResponse

r = APIRouter(default_response_class=RedirectResponse)


@r.get("/x")
async def x():
    return "/y"


app = FastAPI()
app.include_router(r)
""")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "APIRouter(default_response_class=RedirectResponse) is not supported" in err
    assert "main.py:5" in err


@pytest.mark.parametrize("body,message,line", [
    ("    r = APIRouter()\n    r.include_router(a)\n    return r\n", None, None),
    # a route declared on the local router, or anything else built in the function: not a router factory
    ("    r = APIRouter()\n\n    @r.get('/z')\n    async def z():\n        return {}\n    return r\n",
     "include_router: cannot resolve `build()` to an APIRouter", 18),
    ("    r = APIRouter()\n    r.include_router(a)\n    print(r)\n    return r\n",
     "include_router: cannot resolve `build()` to an APIRouter", 18),
])
def test_router_factory_function(tmp_path, capsys, body, message, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("""
from fastapi import APIRouter, FastAPI

a = APIRouter()


@a.get("/a")
async def ra():
    return {}


def build():
%s

app = FastAPI()
app.include_router(build())
""" % body)
    rc = main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")])
    err = capsys.readouterr().err
    if message is None:
        assert rc == 0, err
        return
    assert rc == 1
    assert message in err, err


def test_router_as_keyword(tmp_path, capsys):
    """`include_router(prefix=..., router=r)`: its routes are registered (they were skipped without an error)."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("""
from fastapi import APIRouter, FastAPI

a = APIRouter()


@a.get("/a")
async def ra():
    return {}


app = FastAPI()
app.include_router(prefix="/kw", router=a)
""")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0, capsys.readouterr().err
    assert 'pattern: "^/kw/a$"' in (tmp_path / "out" / "src" / "gen.rs").read_text()


def test_model_method_decorator_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    models = textwrap.dedent(MODELS).replace("{rel}", "@staticmethod\n    def ok() -> int:\n        return 1\n\n"
                                             "    @functools.cache\n    def cached(self):\n        return 2")
    (pkg / "models.py").write_text("import functools\n" + models)
    (pkg / "main.py").write_text(textwrap.dedent(MAIN))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "a Union containing A (validators, which would make Pydantic try the next member" in err
    assert "main.py:24" in err


def test_mcp_accepted(tmp_path):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(MCP_APP.format(server_kw=", title='T', instructions='i'", tool=TOOL, app_kw=OK_APP_KW))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "MCP_TOOL_" in gen and '\\"title\\":\\"t1Arguments\\"' in gen


def test_raw_asgi_class_endpoint_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\n\n\nclass Ep:\n    async def __call__(self, scope, receive, send):\n        pass\n\n\n"
        "app = FastAPI()\napp.add_route('/raw', Ep)\n\n\n@app.get('/x')\nasync def x():\n    return {}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err and f"main.py:{line}" in err, err


def test_websocket_python_side_relayed(tmp_path, capsys):
    """A WebSocket route declared --python-side: not compiled, its path relayed to the Python process (ws::relay)"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(WS_APP.format(extra='\n\n@app.websocket("/w")\nasync def w(websocket: WebSocket):\n    import ujson\n    await websocket.accept()\n'))
    assert main([str(pkg), "--root", str(tmp_path), "--python-side", "/w", "-o", str(tmp_path / "out")]) == 0
    err = capsys.readouterr().err
    assert "WEBSOCKET /w declared Python-side" in err, err
    main_rs = "".join((tmp_path / "out" / "src" / "main.rs").read_text().split())
    assert 'set_python_side(&["/w"],' in main_rs, main_rs


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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
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
    code = main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")])
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err and "main.py:8" in err


def test_custom_response_class_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\nfrom fastapi.responses import UJSONResponse\n\napp = FastAPI()\n\n\n"
        "@app.get('/x', response_class=UJSONResponse)\nasync def x():\n    return {}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "response_class=UJSONResponse is not supported" in err and "main.py:7" in err


def test_uuid_as_str_column_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(
        "from sqlalchemy import Uuid\nfrom sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column\n\n\n"
        "class Base(DeclarativeBase):\n    pass\n\n\nclass T(Base):\n    __tablename__ = 't'\n"
        "    id: Mapped[str] = mapped_column(Uuid(as_uuid=False), primary_key=True)\n")
    (pkg / "main.py").write_text(MAIN.replace("from .models import Team", "from .models import T as Team"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "Uuid(as_uuid=False) is not supported" in err and "models.py:11" in err


def test_http_basic_realm_must_be_literal(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import Depends, FastAPI\nfrom fastapi.security import HTTPBasic\n\nREALM = 'x'\n"
        "basic = HTTPBasic(realm=REALM)\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x(c=Depends(basic)):\n    return {}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert 'V::str("1.2.3")' in gen and expected in gen


def test_distribution_version_needs_literal(tmp_path, capsys):
    pkg = _version_project(tmp_path, "__name__")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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


        def init_sentry(n=0):
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "`nonlocal n` naming a parameter of the enclosing function is not supported" in err and "main.py:8" in err



LIBS_MAIN = """
import enum

import xmltodict
from dateutil.relativedelta import relativedelta
from fastapi import FastAPI
from pydantic import BaseModel, Field, PrivateAttr

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
    ("from typing import Iterable\n\n\nclass M(BaseModel):\n    xs: Iterable[int] = Field(max_length=1)\n\n\ndef use():\n"
     "    return M(xs=[1]).model_dump()", "`Iterable[int]` with Field(max_length=) is not supported", 16),
])
def test_library_ports_rejected(tmp_path, capsys, body, msg, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(LIBS_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert f"main.py:{line}" in err


CRON_MAIN = """
from datetime import datetime

from croniter import croniter, croniter_range
from fastapi import FastAPI

app = FastAPI()


@app.get("/x")
async def x():
    return {body}
"""


@pytest.mark.parametrize("body, msg", [
    ('croniter("R * * * *", 0).get_next()', "croniter.croniter(): R (random) fields in 'R * * * *' are not supported"),
    ('croniter.is_valid("0 r(0-5)/2 * * *")', "croniter.croniter.is_valid(): R (random) fields"),
    ('croniter.match("H R * * *", datetime(2026, 1, 1))', "croniter.croniter.match(): R (random) fields"),
    ('list(croniter_range(0, 60, "* * * * *", _croniter=croniter))', "croniter.croniter_range(): _croniter= is not supported"),
    ('croniter.expand("* * * * *", from_timestamp=0)', "croniter.croniter.expand(): from_timestamp= is not supported"),
    ('croniter.is_valid("* * * * *", encoding="latin-1")', "encoding= other than a literal UTF-8 is not supported"),
    ('croniter("* * * * *", 0, zone="UTC")', "croniter.croniter(): unexpected keyword argument 'zone'"),
    ('croniter("* * * * *", 0, float, True, 50, False, None, False, False, False, 1)', "takes at most 10 positional arguments"),
])
def test_croniter_outside_subset_rejected(tmp_path, capsys, body, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(CRON_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:12" in err



REDIS_MAIN = """
import redis
import redis.asyncio as aredis
from fastapi import FastAPI

app = FastAPI()
OPTS = {{"decode_responses": True}}


@app.get("/x")
async def x():
    r = redis.Redis.from_url("redis://127.0.0.1:6379/0")
    return {body}
"""


@pytest.mark.parametrize("body, msg", [
    ('redis.Redis.from_url("redis://h", ssl_cert_reqs=None)', "redis.Redis.from_url(): option ssl_cert_reqs= is not supported"),
    ('redis.Redis("localhost", 6379)', "redis.Redis(): pass the address as keywords"),
    ('aredis.Redis.from_url("redis://h", encoding="latin-1")', "encoding= other than a literal 'utf-8' is not supported"),
    ('redis.ConnectionPool.from_url("redis://h", **OPTS)', "redis.ConnectionPool.from_url(): `**options` is not supported"),
    ('aredis.Redis(connection_pool=None, unix_socket_path="/x")', "option unix_socket_path= is not supported"),
    ('r.xread({"s": 0})', "method .xread() is not implemented by the runtime"),
])
def test_redis_outside_subset_rejected(tmp_path, capsys, body, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(REDIS_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:13" in err


POPULAR_MAIN = """
import ecs_logging
import orjson
from fastapi import FastAPI
from unidecode import unidecode

app = FastAPI()


{body}


@app.get("/x")
async def x():
    return use()
"""


@pytest.mark.parametrize("body, msg, line", [
    ("def use():\n    return orjson.dumps(orjson.Fragment(b'1'))", "library call `orjson.Fragment()` is not supported", 11),
    ("def use():\n    return orjson.loads(b'1', 2)", "orjson.loads() takes exactly one positional argument", 11),
    ("def use():\n    return orjson.dumps(1, opts=1)", "orjson.dumps(opts=) is not supported", 11),
    ("def use():\n    return unidecode('é', errors='ignore', strict=True)", "unidecode.unidecode(strict=) is not supported", 11),
    ("def use():\n    return str(ecs_logging.StdlibFormatter(validate=False, colors=1))",
     "ecs_logging.StdlibFormatter(colors=) is not supported", 11),
])
def test_popular_libraries_rejected(tmp_path, capsys, body, msg, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(POPULAR_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
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
        ('app.mount("/sub", sub)\napp.add_api_route("/y", x)', "", ["--python-side", "mount"], "`app.add_api_route(...)` at ", 16),
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
    code = main([str(pkg), "--root", str(tmp_path), *flags, "-o", str(tmp_path / "out")])
    err = capsys.readouterr().err
    if message is None:
        assert code == 0, err
        main_rs = (tmp_path / "out" / "src" / "main.rs").read_text()
        want = '&["", "/sub"]' if 'mount("/",' in tail else '&["/sub"]'
        assert f"set_python_side(&[], {want})" in " ".join(main_rs.split()), main_rs
        return
    assert code == 1
    assert message in err and f"main.py:{line}" in err, err


LIB_ROUTER_APP = """
import os

from fastapi import APIRouter, FastAPI
from extlib.views import make_router, router as ext_router

app = FastAPI()


@app.get("/x")
async def x():
    return {{}}


{tail}
"""


@pytest.mark.parametrize(
    "tail, flags, message, line",
    [
        # a router of a library, included last: its routes stay in Python (relayed when nothing native matches)
        ("app.include_router(ext_router)", ["--python-side", "mount"], None, ['""']),
        ('app.include_router(ext_router, prefix="/lib/")\napp.include_router(make_router("a"), prefix="/b")',
         ["--python-side", "auto"], None, ['"/b"', '"/lib"']),
        ('app.include_router(ext_router)\napp.mount("/sub", x)', ["--python-side", "mount"], None, ['""', '"/sub"']),
        ("app.include_router(ext_router)", [], "it can stay on the Python side (--python-side mount, or --python-side auto)", 15),
        # Starlette tries the routes in order and the library's are unknown: nothing may register after it
        ('app.include_router(ext_router)\n\n\n@app.get("/y")\nasync def y():\n    return {}', ["--python-side", "mount"],
         "`app.get(...)` at ", 15),
        # a route added after it at startup: relayed to Python, which tries the library's first (routing::lib_barrier)
        ("async def m(request):\n    return None\n\n\ndef setup(a):\n    a.add_route('/m', m)\n\n\n"
         "app.include_router(ext_router)\nsetup(app)", ["--python-side", "mount"], None, ['""']),
        ("if os.environ.get('LIB'):\n    app.include_router(ext_router)", ["--python-side", "mount"], "registered conditionally", 16),
        ("app.include_router(ext_router, prefix=os.environ['P'])", ["--python-side", "mount"], "must be a literal string", 15),
        ("r = APIRouter()\nr.include_router(ext_router)\napp.include_router(r)", ["--python-side", "mount"],
         "only `app.include_router(...)` on the application itself", 16),
        ("def setup():\n    app.include_router(ext_router)", ["--python-side", "mount"], "is in another function", 16),
    ],
)
def test_library_router_left_to_python(tmp_path, capsys, tail, flags, message, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(LIB_ROUTER_APP.format(tail=tail))
    code = main([str(pkg), "--root", str(tmp_path), *flags, "-o", str(tmp_path / "out")])
    err = capsys.readouterr().err
    if message is None:
        assert code == 0, err
        main_rs = " ".join((tmp_path / "out" / "src" / "main.rs").read_text().split())
        assert f"set_python_side(&[], &[{', '.join(line)}])" in main_rs, main_rs
        return
    assert code == 1
    assert message in err and f"main.py:{line}" in err and "P2A0315" in err, err


CONSTRUCT_APP = """
from fastapi import FastAPI
from pydantic import BaseModel, Field, RootModel

app = FastAPI()


{model}


@app.get("/c")
async def c():
    return {call}
"""


@pytest.mark.parametrize(
    "model, call, message",
    [
        ("class M(BaseModel):\n    a: int\n    b: int = 0", "M.model_construct(b=1).model_dump()",
         "M.model_construct() without the required field `a`"),
        ("class M(BaseModel):\n    a: int\n\n    def model_post_init(self, ctx):\n        pass", "M.model_construct(a=1)",
         "model_post_init is not supported"),
        ("class M(RootModel[int]):\n    pass", "M.model_construct(1)", "a RootModel is not supported"),
        ("class M(BaseModel):\n    a: int = Field(0, alias='A', validation_alias='aa')", "M.model_construct(A=1)",
         "has both alias= and validation_alias="),
        ("class M(BaseModel):\n    a: int = 0", "M.model_construct(set(), set(), a=1)", "`_fields_set` as its only positional"),
    ],
)
def test_model_construct_refusals(tmp_path, capsys, model, call, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(CONSTRUCT_APP.format(model=model, call=call))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err and "main.py:" in err, err


CLASS_ATTR_APP = """
import json

from fastapi import FastAPI

from .names import *

app = FastAPI()


def double(x):
    return x * 2


class Holder:
    {attr}

    def run(self):
        return {use}


H = Holder()


@app.get("/h")
async def h():
    return {{"v": H.run()}}
"""


@pytest.mark.parametrize(
    "attr, use, message, line",
    [
        # Python binds a function held as a class attribute to the instance it is read through
        ("f = double", "self.f(2)", "the function `f = double` as a class attribute is not supported", 16),
        ("f = lambda self, x: x", "self.f(2)", "the function `f = lambda self, x: x` as a class attribute", 16),
        ("f = classmethod(double)", "self.f(2)", "(only staticmethod(f))", 16),
        ("f = staticmethod(double, 1)", "self.f(2)", "(only staticmethod(f))", 16),
        # `from m import *`: the names without a leading `_`, or those of a literal __all__
        ("f = staticmethod(double)", "_HIDDEN", "unknown name `_HIDDEN`", 19),
        ("f = staticmethod(double)", "LISTED + UNLISTED", "unknown name `UNLISTED`", 19),
    ],
)
def test_class_attribute_functions(tmp_path, capsys, attr, use, message, line):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "names.py").write_text("__all__ = ['LISTED', 'OK']\nLISTED = 1\nUNLISTED = 2\n_HIDDEN = 3\nOK = 4\n")
    (pkg / "main.py").write_text(CLASS_ATTR_APP.format(attr=attr, use=use))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err and f"main.py:{line}" in err, err


MARKER_APP = """
from fastapi import FastAPI, Query

app = FastAPI()
{globals}


@app.get("/a")
async def a({a}):
    return {{}}


@app.get("/b")
async def b({b}):
    return {{}}
"""


@pytest.mark.parametrize(
    "globals_, a, b, message",
    [
        # a marker kept in a module variable stands for its call; shared without alias= by parameters of other
        # names, FastAPI gives it the first one's name
        ("Limit = Query(10, ge=1)", "limit: int = Limit", "limit: int = Limit", None),
        ("Limit = Query(10, alias='n')", "limit: int = Limit", "size: int = Limit", None),
        ("Limit = Query(10, ge=1)", "limit: int = Limit", "size: int = Limit", "is the default of parameters `limit` and `size`"),
        ("Limit = Query(10)\n\n\ndef reset():\n    global Limit\n    Limit = Query(5)", "limit: int = Limit", "x: int = 1",
         "rebinds with `global`"),
    ],
)
def test_parameter_markers_in_module_variables(tmp_path, capsys, globals_, a, b, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(MARKER_APP.format(globals=globals_, a=a, b=b))
    code = main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")])
    err = capsys.readouterr().err
    if message is None:
        assert code == 0, err
        return
    assert code == 1 and message in err and "main.py:" in err, err



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
    return main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")])


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
    code = main([str(pkg), "--root", str(tmp_path), "--python-side", "auto", "-o", str(tmp_path / "out")])
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


_STYLED = 'import reportlab.platypus\n\n\ndef styled():\n    return reportlab.platypus.KeepTogether([])\n\n\n'


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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "letters.py:5: library call `reportlab.platypus.KeepTogether()` is not supported" in err
    rep = tmp_path / "rep.md"
    assert main([str(pkg), "--root", str(tmp_path), "--report", str(rep)]) == 0
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0, capsys.readouterr().err


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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "`super(cls, cls)` in A, which B subclass" in err
    assert "main.py:" in err


def test_module_attribute_assignment_accepted(tmp_path, capsys):
    """`mod.ATTR = v` rebinds another module's variable, as `global` in that module would (`stripe.api_key = ...`);
    it used to be refused (and before, to fail at startup, read-only)."""
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
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
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "engine_connect_args" in gen
    init = gen[gen.index("pub async fn init_globals"):]
    assert "g_proj_main__engine(cx)" in init[:init.index("\n}")]


# constructions of the FastAPI documentation's examples (corpus/, docs/coverage.md) that were translated
# without them before (a silent difference): each is now refused at its line of main.py
DOCS_REFUSED = [
    ('''
from fastapi import FastAPI
from fastapi.routing import APIRoute


class LoggingRoute(APIRoute):
    pass


app = FastAPI()
app.openapi = LoggingRoute


@app.get("/")
async def root():
    return {}
''', "main.py:10: assigning `app.openapi` at module level is not supported (only `app.state` attributes)"),
]


@pytest.mark.parametrize("source, message", DOCS_REFUSED)
def test_docs_corpus_refusals(tmp_path, capsys, source, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(source.lstrip("\n"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert message in capsys.readouterr().err


@pytest.mark.parametrize("route, message", [
    ("async def r(f: Annotated[F, Query()], q: int = 0):",
     "r: a model of query parameters is supported as the route's only query parameter, without dependencies"),
    ("async def r(f: Annotated[F, Query()], u: int = Depends(dep)):",
     "r: a model of query parameters is supported as the route's only query parameter, without dependencies"),
    ("async def r(f: Annotated[F, Form()], g: Annotated[str, Form()]):",
     "r: a model of form parameters is supported as the route's only form parameter, without dependencies"),
    ("async def r(f: Annotated[F | None, Cookie()] = None):", "f: an optional or union model of cookies is not supported"),
    ("async def r(f: Annotated[F, Header()] = F()):", "f: a default for a model of header parameters is not supported"),
])
def test_param_models_refused(tmp_path, capsys, route, message):
    """A model of parameters is read field by field only alone in its source (FastAPI's `len(fields) == 1`)."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from typing import Annotated\n\nfrom fastapi import Cookie, Depends, FastAPI, Form, Header, Query\n"
        "from pydantic import BaseModel\n\n\nclass F(BaseModel):\n    a: int = 1\n\n\nasync def dep():\n    return 1\n\n\n"
        f"app = FastAPI()\n\n\n@app.get('/')\n{route}\n    return 1\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert message in capsys.readouterr().err


@pytest.mark.parametrize("extra, message", [
    ("@app.on_event(EVENT)\nasync def s():\n    pass\n", '@app.on_event(...) needs a literal "startup" or "shutdown"'),
    ("@app.on_event('startup')\nasync def s(x=1):\n    pass\n", "@app.on_event(...) handler s must take no parameters"),
])
def test_on_event_refused(tmp_path, capsys, extra, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("from fastapi import FastAPI\n\nEVENT = 'startup'\napp = FastAPI()\n\n\n" + extra)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    assert message in capsys.readouterr().err


def test_on_event_lifespan(tmp_path):
    """FastAPI's default lifespan: startup handlers in order before serving, shutdown ones after."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\n\nitems = {}\napp = FastAPI()\n\n\n@app.on_event('startup')\nasync def a():\n"
        "    items['a'] = 1\n\n\n@app.on_event('shutdown')\ndef b():\n    items.clear()\n\n\n@app.on_event('startup')\n"
        "def c():\n    items['c'] = 2\n\n\n@app.get('/')\nasync def r():\n    return items\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    body = gen[gen.index("fn f_proj_main____py2axum_on_event_lifespan"):]
    body = body[:body.index("\n}\n")]
    assert body.index("__py2axum_on_event_a") < body.index("__py2axum_on_event_c") < body.index("__py2axum_on_event_b")


def test_app_dependencies_first(tmp_path):
    """`FastAPI(dependencies=[...])`: solved first for every route, then the include's, then the route's own."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import APIRouter, Depends, FastAPI\n\n\nasync def app_dep():\n    return 1\n\n\n"
        "async def inc_dep():\n    return 2\n\n\nasync def own_dep():\n    return 3\n\n\n"
        "app = FastAPI(dependencies=[Depends(app_dep)])\nrouter = APIRouter()\n\n\n"
        "@router.get('/r', dependencies=[Depends(own_dep)])\nasync def r():\n    return 1\n\n\n"
        "@app.get('/a')\nasync def a():\n    return 1\n\n\n"
        "app.include_router(router, dependencies=[Depends(inc_dep)])\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    r = gen[gen.index("/// GET /r"):]
    r = r[:r.index("\n}\n")]
    assert r.index("dep_proj_main__app_dep") < r.index("dep_proj_main__inc_dep") < r.index("dep_proj_main__own_dep")
    a = gen[gen.index("/// GET /a"):]
    assert "dep_proj_main__app_dep" in a[:a.index("\n}\n")]


def test_request_validation_error_body(tmp_path):
    """`exc.body` of a route's RequestValidationError: the decoded body (FastAPI's `body=`), not for a form route."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI, Form\nfrom pydantic import BaseModel\n\napp = FastAPI()\n\n\nclass I(BaseModel):\n"
        "    a: int\n\n\n@app.post('/j')\nasync def j(i: I):\n    return i\n\n\n@app.post('/f')\nasync def f(a: int = Form()):\n"
        "    return a\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    j, f = gen[gen.index("/// POST /j"):], gen[gen.index("/// POST /f"):]
    assert "web::check_body(cx, __errv, &__body)" in j[:j.index("\n}\n")]
    assert "web::check(cx, __errv)" in f[:f.index("\n}\n")]


@pytest.mark.parametrize("src, message", [
    # generator endpoints (FastAPI >= 0.134): JSON Lines, EventSourceResponse, StreamingResponse only
    ("from fastapi.responses import HTMLResponse\n@app.get('/x', response_class=HTMLResponse)\nasync def x():\n    yield 'a'\n",
     "a generator endpoint with response_class=HTMLResponse is not supported"),
    ("from fastapi.sse import EventSourceResponse\n@app.get('/x', response_class=EventSourceResponse)\nasync def x():\n"
     "    return {'a': 1}\n", "response_class=EventSourceResponse is only supported on a generator endpoint"),
    ("@app.get('/x', response_model_exclude_none=True)\nasync def x():\n    yield 1\n",
     "response_model_exclude_none= on a generator endpoint is not supported"),
    # response_model_include/exclude: literal top-level field names of a model response_model
    ("class M(BaseModel):\n    a: int\n@app.get('/x', response_model=list[M], response_model_include={'a'})\nasync def x():\n"
     "    return []\n", "response_model_include= is only supported with a Pydantic model as the response model"),
    ("class M(BaseModel):\n    a: int\n@app.get('/x', response_model=M, response_model_exclude={'b'})\nasync def x():\n"
     "    return {'a': 1}\n", "response_model_exclude=: `b` is not a field of M"),
    ("class M(BaseModel):\n    a: int\n@app.get('/x', response_model=M, response_model_include={'a': True})\nasync def x():\n"
     "    return {'a': 1}\n", "response_model_include= must be a literal set or list of field names"),
    # app.dependency_overrides at module level: project function to project function
    ("def dep():\n    return 1\napp.dependency_overrides[dep] = lambda: 2\n@app.get('/x')\nasync def x(v: int = Depends(dep)):\n"
     "    return v\n", "only a project function overridden by a project function is supported"),
    # model_config bytes modes
    ("class M(BaseModel):\n    b: bytes\n    model_config = {'val_json_bytes': 'base32'}\n@app.post('/x')\nasync def x(m: M):\n"
     "    return 1\n", 'model_config val_json_bytes= must be "utf8", "base64" or "hex"'),
    # TrustedHostMiddleware's own assertion, root_path with a WebSocket route
    ("from fastapi.middleware.trustedhost import TrustedHostMiddleware\napp.add_middleware(TrustedHostMiddleware, "
     "allowed_hosts=['ex*.com'])\n@app.get('/x')\nasync def x():\n    return 1\n", "domain wildcard patterns must be like"),
    ("app2 = None\n@app.websocket('/ws')\nasync def ws(w: WebSocket):\n    await w.accept()\n",
     "FastAPI(root_path=...) together with WebSocket routes"),
    ("def __getattr__(n):\n    return n\n@app.get('/x')\nasync def x(q: Annotated[str, Query(min_length=2)] = None, u=Query(max_length=3)):\n"
     "    return 1\n", "constraints without a type annotation are not supported"),
    # a raw ASGI middleware that also looks at WebSocket connections: the binary would skip it for them
    ("class Guard:\n    def __init__(self, app):\n        self.app = app\n    async def __call__(self, scope, receive, send):\n"
     "        if scope['type'] == 'websocket' and scope['path'] != '/ok':\n            return\n        await self.app(scope, receive, send)\n"
     "app.add_middleware(Guard)\n@app.websocket('/ws')\nasync def ws(w: WebSocket):\n    await w.accept()\n",
     "raw ASGI middleware Guard with WebSocket routes"),
])
def test_corpus06_rejected(tmp_path, capsys, src, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    app = "FastAPI(root_path='/r')" if "root_path" in message else "FastAPI()"
    (pkg / "main.py").write_text(
        "from typing import Annotated\nfrom fastapi import Depends, FastAPI, Query, WebSocket\nfrom pydantic import BaseModel\n"
        f"app = {app}\n" + src)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err, err
    assert "main.py:" in err


ITS_MAIN = """
from fastapi import FastAPI
from itsdangerous import URLSafeTimedSerializer

app = FastAPI()
S = URLSafeTimedSerializer("k", salt="s")


@app.get("/x")
async def x(t: str):
    return {stmt}
"""


@pytest.mark.parametrize("stmt, msg", [
    # incident of 09/10/2026 (a production app): an argument the runtime did not implement was a TypeError (a 500) at run time
    ("S.loads(t, max_age=5, serializer=None)", ".loads(serializer=) is not supported (only max_age, return_timestamp, salt)"),
    ("S.loads(t, 5, True, 's', 'x')", ".loads() takes at most 4 positional arguments here"),
    ("S.loads(*[t])", ".loads() takes at most 4 positional arguments here (no * or ** unpacking)"),
    ("S.loads(t, **{'max_age': 5})", ".loads() takes at most 4 positional arguments here (no * or ** unpacking)"),
    ("S.dumps({'a': 1}, header_fields={})", ".dumps(header_fields=) is not supported (only salt)"),
    ("S.dumps({'a': 1}, 's', 'x')", ".dumps() takes at most 2 positional arguments here"),
    ("S.loads_unsafe(t)", "method .loads_unsafe() is not implemented by the runtime"),
])
def test_itsdangerous_outside_subset_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(ITS_MAIN.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:11" in err


KW_MAIN = """
import re
from email.mime.application import MIMEApplication
from email.mime.multipart import MIMEMultipart
from email.mime.text import MIMEText

from fastapi import FastAPI, Response
from fastapi.responses import FileResponse, PlainTextResponse
from pydantic import BaseModel

app = FastAPI()


class M(BaseModel):
    a: int = 1


@app.get("/x")
async def x(t: str, response: Response):
    return {stmt}
"""


@pytest.mark.parametrize("stmt, msg", [
    # arguments the runtime dropped or ignored, refused at generation since 09/10/2026 (same class as return_timestamp=)
    ("t.encode('utf-8', 'strict', 1)", ".encode() takes at most 2 positional arguments here"),
    ("t.encode(encoding='utf-8', bom=True)", ".encode(bom=) is not supported (only encoding, errors)"),
    ("t.find('a', 1, 2, 3)", ".find() takes at most 3 positional arguments here"),
    ("M.model_validate_json('{}', strict=True)", ".model_validate_json(strict=) is not supported"),
    ("M.model_validate({}, context={})", ".model_validate(context=) is not supported (only from_attributes)"),
    ("response.set_cookie('a', 'b', partitioned=True)", ".set_cookie(partitioned=) is not supported"),
    ("PlainTextResponse('x', background=t)", "PlainTextResponse(background=) is not supported"),
    ("FileResponse('p', 200, None, None, t)", "FileResponse(background=) is not supported"),
    ("FileResponse('p', stat_result=t)", "FileResponse(stat_result=) is not supported"),
    ("MIMEApplication(b'x', _encoder=t)", "MIMEApplication(_encoder=) is not supported"),
    ("MIMEText('x', policy=t)", "MIMEText(policy=) is not supported"),
    ("MIMEMultipart(boundary='b')", "MIMEMultipart(boundary=) is not supported"),
    ("re.compile('a').search(t, 1, 2, 3)", ".search() takes at most 3 positional arguments here"),
    ("re.compile('a').fullmatch(t, 1)", ".fullmatch() takes at most 1 positional argument here"),
    ("re.match('(a)', t).groupdict(fallback=1)", ".groupdict(fallback=) is not supported (only default)"),
])
def test_dropped_arguments_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(KW_MAIN.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:20" in err


def test_json_path_index_rejected(tmp_path, capsys):
    """`Model.json_col[("a", "b")]` (JSON path, #>>) is refused; `col["k"].as_string()` translates."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", "data: Mapped[dict | None] = mapped_column(JSON)")
                                   .replace("from sqlalchemy import ForeignKey", "from sqlalchemy import JSON, ForeignKey"))
    (pkg / "main.py").write_text(textwrap.dedent(MAIN) + textwrap.dedent('''
        from sqlalchemy import select


        @app.get("/one")
        async def one(s: AsyncSession = Depends(db)):
            return (await s.execute(select(Team.data["k"].as_string()))).scalars().all()


        @app.get("/path")
        async def path(s: AsyncSession = Depends(db)):
            return (await s.execute(select(Team.data[("a", "b")].as_string()))).scalars().all()
    '''))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "a JSON path index" in err and "main.py:" in err
    assert "/one" not in err


def _reject(tmp_path, capsys, src: str) -> str:
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent(src))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    return capsys.readouterr().err


def test_nonlocal_outside_enclosing_function_rejected(tmp_path, capsys):
    """`nonlocal` of a name the enclosing function does not bind (a global, a class attribute)."""
    err = _reject(tmp_path, capsys, '''
        from fastapi import FastAPI

        app = FastAPI()


        @app.get("/n")
        async def n():
            def inner():
                def deepest():
                    nonlocal total
                    total = 1
                deepest()
            inner()
            return {}
    ''')
    assert "`nonlocal total`: only a local variable of the enclosing function is supported" in err and "main.py:11" in err



@pytest.mark.parametrize("call", ["await request.form()", "[c async for c in request.stream()]"])
def test_request_form_and_stream_rejected(tmp_path, capsys, call):
    """Starlette's `request.form()` / `request.stream()` have no runtime counterpart: an AttributeError at run
    time otherwise (the names are in the runtime's catalog for other values)."""
    err = _reject(tmp_path, capsys, f'''
        from fastapi import FastAPI, Request

        app = FastAPI()


        @app.post("/f")
        async def f(request: Request):
            got = {call}
            return {{"n": len(got)}}
    ''')
    name = "form" if "form" in call else "stream"
    assert f"`request.{name}()` is not supported" in err and "main.py:9" in err


def test_typeddict_response_model_rejected(tmp_path, capsys):
    """Calling a TypedDict builds a dict; as a response model Pydantic would validate it: refused."""
    err = _reject(tmp_path, capsys, '''
        from typing import TypedDict

        from fastapi import FastAPI

        app = FastAPI()


        class Q(TypedDict):
            ok: bool


        @app.get("/q")
        async def q() -> Q:
            return Q(ok=1)
    ''')
    assert "is not a Pydantic model or an enum" in err and "main.py:14" in err
PYD_CORPUS_MAIN = """
from collections.abc import Iterable, MutableSequence
from typing import Annotated, Any, Generic, TypeVar

from fastapi import FastAPI
from pydantic import (AfterValidator, BaseModel, Discriminator, Field, PlainSerializer, Tag, WrapValidator,
                      field_serializer, field_validator, model_serializer)

app = FastAPI()
T = TypeVar("T")
C = TypeVar("C", int, str)


class Base(BaseModel):
    n: int = 0


class Holder(BaseModel, Generic[T]):
    item: T


def w(v, handler):
    return handler(v)


{body}


@app.post("/x")
async def x(b: In):
    return b
"""


@pytest.mark.parametrize("body, msg", [
    ("class In(BaseModel):\n    a: Annotated[int, AfterValidator(lambda v: v), WrapValidator(w)] = 0",
     "a WrapValidator after other validators in Annotated[...] is not supported"),
    ("class In(BaseModel):\n    a: Annotated[int, WrapValidator(w), Field(ge=0)] = 0",
     "after a WrapValidator in Annotated[...] is not supported"),
    ("class In(BaseModel):\n    a: int = 0\n\n    @field_validator('a')\n    @classmethod\n    def f(cls, v):\n        return v\n\n"
     "    @field_validator('a', mode='wrap')\n    @classmethod\n    def g(cls, v, handler):\n        return handler(v)",
     "a mode='wrap' validator of 'a' after other validators of the field is not supported"),
    ("class In(BaseModel):\n    a: Annotated[int, PlainSerializer(str, when_used='sometimes')] = 0", "when_used='sometimes' is not supported"),
    ("class In(BaseModel):\n    a: int = 0\n\n    @field_serializer('*')\n    def s(self, v):\n        return v",
     "@field_serializer('*') or without field names is not supported"),
    ("class In(BaseModel):\n    a: int = 0\n\n    @model_serializer\n    async def s(self):\n        return {}",
     "an async serializer is not supported"),
    ("class In(BaseModel):\n    h: Holder[int, str]", "1 type parameter(s) expected"),
    ("class Box(BaseModel, Generic[C]):\n    c: C\n\n\nclass In(BaseModel):\n    b: Box[int]",
     "constraints and options other than bound=/default= are not supported"),
    ("class In(BaseModel):\n    b: Base[int]", "Base is not a generic model"),
    ("class In(BaseModel):\n    k: type[ValueError]", "unsupported type annotation `type[ValueError]`"),
    ("class In(BaseModel):\n    t: tuple[int, str, ...]", "a variable tuple has one type"),
    ("class In(BaseModel):\n    xs: MutableSequence[int]", "`MutableSequence[int]` is not supported"),
    ("class In(BaseModel):\n    xs: Iterable[int] = [1]", "an Iterable field with a default other than None is not supported"),
    ("class In(BaseModel):\n    p: Base | Holder[int] = Field(discriminator=Discriminator(lambda v: 'x'))",
     "needs a Tag('...')"),
    ("class In(BaseModel):\n    p: Annotated[Base, Tag('b')] | Annotated[Holder[int], Tag('h')] = "
     "Field(discriminator=Discriminator(lambda v: 'b', custom_error_type='t'))",
     "Discriminator custom errors: a type, a message and a dict of strings are supported"),
    ("from pydantic import MISSING\n\n\nclass In(BaseModel):\n    f: MISSING = MISSING", "library value `pydantic.MISSING` is not supported"),
    ("class Odd:\n    @classmethod\n    def __get_pydantic_core_schema__(cls, s, h):\n        return h(int)\n\n\n"
     "class In(BaseModel):\n    o: list[Odd]", "customizes its Pydantic schema"),
    ("from pydantic import RootModel\n\n\nclass In(RootModel[list[int]]):\n    @field_validator('root')\n    @classmethod\n"
     "    def v(cls, x):\n        return x", "validators, serializers, extra= or aliases on a RootModel are not supported"),
    ("class In(BaseModel):\n    c: complex = Field(0, ge=1)", "constraints on `complex` are not supported"),
    ("from pydantic import conlist\n\n\nclass In(BaseModel):\n    xs: conlist(int, unique_items=True)", "unique_items= is not supported"),
    ("from pydantic import constr\n\n\nclass In(BaseModel):\n    s: constr(regex='a')", "regex= is not supported"),
    ("from pydantic import confloat\n\n\nclass In(BaseModel):\n    f: confloat(allow_inf_nan=True)", "allow_inf_nan= is not supported"),
    ("from pydantic import conlist\n\n\nclass In(BaseModel):\n    xs: conlist(int, 1)", "the item type is the one positional argument"),
])
def test_pydantic_corpus_constructions_rejected(tmp_path, capsys, body, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PYD_CORPUS_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:" in err


# ------------------------------------------------------------------ Python versions (docs/python-versions.md)

PYVER_APP = """
import base64
import re
import sys
from urllib.parse import urlsplit

from fastapi import FastAPI

app = FastAPI()


@app.get("/x")
async def x(t: str):
    return {stmt}
"""


@pytest.mark.parametrize("stmt, msg", [
    ("re.prefixmatch('a', t)", "re.prefixmatch() needs Python 3.15 (the project runs on 3.12"),
    ("urlsplit(t, missing_as_none=True)", "urlsplit(missing_as_none=) is not supported"),
    ("base64.b64encode(t.encode(), wrapcol=76)", "b64encode(wrapcol=) is not supported"),
    ("sys.version_info.micro", "library value `sys.version_info.micro` is not supported"),
    ("sys.version_info >= (3, 12, 4)", "library value `sys.version_info` is not supported"),
])
def test_python_version_dependent_rejected(tmp_path, capsys, monkeypatch, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PYVER_APP.format(stmt=stmt))
    monkeypatch.setenv("PY2AXUM_TARGET_PYTHON", "3.12")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:14" in err


PYVER_EXC_APP = """
import asyncio
import re

from fastapi import FastAPI

app = FastAPI()


@app.get("/x")
async def x(t: str):
    try:
        {stmt}
    except {exc}:
        return None
"""


@pytest.mark.parametrize("stmt, exc, msg", [
    # re.error is named PatternError from 3.13 only
    ("return re.compile(t).pattern", "re.PatternError", "re.PatternError needs Python 3.13 (the project runs on 3.12"),
    ("return await asyncio.sleep()", "ValueError", "asyncio.sleep() missing required argument 'delay'"),
    ("return await asyncio.sleep(1, t, 2)", "ValueError", "asyncio.sleep() takes at most 2 positional arguments"),
    ("return await asyncio.sleep(1, loop=t)", "ValueError", "asyncio.sleep(loop=) is not supported"),
])
def test_python_version_dependent_names_rejected(tmp_path, capsys, monkeypatch, stmt, exc, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PYVER_EXC_APP.format(stmt=stmt, exc=exc))
    monkeypatch.setenv("PY2AXUM_TARGET_PYTHON", "3.12")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert ("main.py:14" if "PatternError" in msg else "main.py:13") in err, err


@pytest.mark.skipif(sys.version_info < (3, 14), reason="t-strings need the 3.14 parser")
def test_t_string_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PYVER_APP.format(stmt="t'hello {t}'"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "t-strings (template strings, PEP 750, Python 3.14) are not supported" in err and "main.py:14" in err, err


@pytest.mark.skipif(sys.version_info < (3, 15), reason="`lazy import` needs the 3.15 parser")
def test_lazy_import_of_a_project_module_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "helper.py").write_text("X = 1\n")
    (pkg / "main.py").write_text("lazy import json\nlazy from proj import helper\n" + PYVER_APP.format(stmt="helper.X"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "`lazy` import of the project module `proj.helper` is not supported" in err and "main.py:2" in err, err

@pytest.mark.parametrize(
    "head, body, message",
    [
        ("", "    logging.getLogger(__name__).info('x', stacklevel=2)\n    return {}", "logging `stacklevel=2` is not supported"),
        ("", "    logging.warning('x', stacklevel=n)\n    return {}", "logging `stacklevel=n` is not supported"),
        ("", "    logging.config.fileConfig('logging.ini')\n    return {}", "library call `logging.config.fileConfig()` is not supported"),
        ("import logging.handlers\n", "    logging.handlers.RotatingFileHandler('x.log')\n    return {}",
         "library call `logging.handlers.RotatingFileHandler()` is not supported"),
        ("class H(logging.Handler):\n    def emit(self, record):\n        pass\n\n\n", "    logging.getLogger().addHandler(H())\n    return {}",
         "base `logging.Handler`"),
    ],
)
def test_logging_rejected(tmp_path, capsys, head, body, message):
    """docs/advanced/logging.md: what the runtime does not reproduce is refused at translation, with file:line"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "import logging\nimport logging.config\nfrom fastapi import FastAPI\n\n" + head + "app = FastAPI()\nn = 2\n\n\n"
        "@app.get('/x')\nasync def x():\n" + body + "\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


@pytest.mark.parametrize(
    "body, message",
    [
        ("    logger.exception('x')", "loguru `logger.exception()` is not supported"),
        ("    logger.opt(exception=True).debug('x')", "loguru `logger.opt()` is not supported"),
        ("    logger.bind(user=1).info('x')", "loguru `logger.bind()` is not supported"),
        ("    with logger.contextualize(user=1):\n        pass", "loguru `logger.contextualize()` is not supported"),
        ("    logger.add('app.log', rotation='00:00')", "loguru `logger.add('app.log')` is not supported"),
        ("    logger.add(lambda m: None)", "is not supported: the binary writes to sys.stderr or sys.stdout only"),
        ("    logger.add(sys.stderr, serialize=True)", "loguru `logger.add(serialize=...)` is not supported"),
        ("    logger.add(sys.stderr, enqueue=True)", "loguru `logger.add(enqueue=...)` is not supported"),
        ("    logger.add(sys.stderr, filter='app')", "loguru `logger.add(filter=...)` is not supported"),
        ("    logger.add(sys.stderr, format=FMT)", "supported with a literal text only"),
        ("    logger.add(sys.stderr, format='{extra[user]} {message}')", "the record field `{extra[user]}` is not reproduced"),
        ("    logger.add(sys.stderr, format='<foo>{message}</foo>')", "color markups could not be parsed correctly"),
        ("    logger.add(sys.stderr, format='{time:%G} {message}')", "the time directive `%G` is not reproduced"),
        ("    logger.add(sys.stderr, format='{line:q}')", "the format spec of `{line:q}` fails"),
        ("    logger.add(sys.stderr, format='{level!r}')", "`{level!r}` is not reproduced"),
        ("    logger.add(sys.stderr, colour=True)", "loguru `logger.add(colour=...)` is not supported"),
        ("    logger.remove(1, 2)", "loguru `logger.remove()` takes one argument"),
        ("    logger.level('NOTICE', no=25)", "loguru `logger.level()` is not supported"),
        ("    print(logger)", "library value `loguru.logger` is not supported"),
        ("    f = logger.info\n    f('x')", "library value `loguru.logger.info` is not supported"),
    ],
)
def test_loguru_rejected(tmp_path, capsys, body, message):
    """docs/advanced/logging.md#loguru: what dynrt/loguru.rs does not reproduce is refused at translation, with
    file:line (never at run time, where an application's `except Exception` would hide it)"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "import sys\nfrom fastapi import FastAPI\nfrom loguru import logger\n\nFMT = '{message}'\napp = FastAPI()\n\n\n"
        "@app.get('/x')\nasync def x():\n" + body + "\n    return {}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


def test_raw_middleware_passing_websockets_accepted(tmp_path):
    """the usual guard (non-HTTP scopes handed on unchanged) keeps a raw middleware next to WebSocket routes"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI, WebSocket\napp = FastAPI()\n"
        "class Stamp:\n    def __init__(self, app):\n        self.app = app\n    async def __call__(self, scope, receive, send):\n"
        "        if scope['type'] != 'http' or scope['path'] == '/x':\n            await self.app(scope, receive, send)\n            return\n"
        "        await self.app(scope, receive, send)\n"
        "app.add_middleware(Stamp)\n@app.websocket('/ws')\nasync def ws(w: WebSocket):\n    await w.accept()\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


SYNC_GEN_APP = """
from fastapi import FastAPI
from fastapi.responses import StreamingResponse

app = FastAPI()


def top(n):
    for i in range(n):
        yield str(i)


@app.get("/stream")
async def stream():
    return StreamingResponse(top(3), media_type="text/plain")


@app.get("/x")
async def x():
{body}
"""


@pytest.mark.parametrize("body, line", [
    ("    return list(top(3))", 20),
    ("    return [v for v in top(3)]", 20),
    ("    def inner(n):\n        yield n\n    out = []\n    for v in inner(2):\n        out.append(v)\n    return out", 23),
])
def test_sync_generator_consumed_synchronously_rejected(tmp_path, capsys, body, line):
    # the runtime runs a generator function as an async generator: only a streamed response consumes it
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(SYNC_GEN_APP.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "calls a synchronous generator function: only `StreamingResponse(gen())` is supported" in err, err
    assert f"main.py:{line}" in err, err
    assert "main.py:15" not in err, err


YAML_MAIN = """
import yaml

from fastapi import FastAPI

app = FastAPI()


def use(s):
    return {body}


@app.post("/x")
async def x(s: str):
    return use(s)
"""


@pytest.mark.parametrize("body, msg", [
    ("yaml.load(s)", "yaml.load() is supported with Loader=yaml.SafeLoader only"),
    ("yaml.load(s, Loader=yaml.Loader)", "yaml.Loader"),
    ("yaml.load(s, Loader=yaml.FullLoader)", "yaml.FullLoader"),
    ("yaml.load(s, yaml.CSafeLoader)", "yaml.CSafeLoader"),
    ("yaml.full_load(s)", "yaml.full_load()"),
    ("yaml.unsafe_load(s)", "yaml.unsafe_load()"),
    ("yaml.safe_load_all(s)", "yaml.safe_load_all()"),
    ("yaml.load_all(s, Loader=yaml.SafeLoader)", "yaml.load_all()"),
    ("yaml.safe_load(stream=s, Loader=yaml.SafeLoader)", "yaml.safe_load(Loader=) is not supported"),
    ("yaml.safe_dump(s, None, True)", "yaml.safe_dump() takes at most 2 positional arguments"),
    ("yaml.safe_dump(s, indnt=2)", "yaml.safe_dump(indnt=) is not supported"),
    ("yaml.dump(s, Dumper=yaml.CDumper)", "yaml.CDumper"),
    ("yaml.dump(s, Dumper=yaml.BaseDumper)", "yaml.BaseDumper"),
    ("yaml.add_constructor('!x', None)", "yaml.add_constructor()"),
    ("yaml.compose(s)", "yaml.compose()"),
    ("yaml.ScannerError", "yaml.ScannerError"),
])
def test_pyyaml_outside_subset_rejected(tmp_path, capsys, body, msg):
    """PyYAML: only the pure-Python SafeLoader and the dump functions are native; the other loaders (Python
    objects, libyaml's own messages) and the extension API are refused with their line."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(YAML_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:10" in err


@pytest.mark.parametrize(
    "source",
    [
        # match arms of a tuple pattern: `(Kind::Httpx, "reason_phrase") | (..)`, `(Sql::Insert(..), "excluded") if ..`
        "import httpx\nfrom fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x():\n"
        "    async with httpx.AsyncClient() as c:\n        r = await c.get('http://h')\n    return {'a': r.reason_phrase}\n",
        # `methods::setattr(wrapper, "__wrapped__", ..)`: functools.wraps
        "import functools\nfrom fastapi import FastAPI\n\napp = FastAPI()\n\n\ndef deco(f):\n    @functools.wraps(f)\n"
        "    def w(*a):\n        return f(*a)\n    return w\n\n\n@deco\ndef g():\n    return 1\n\n\n"
        "@app.get('/x')\nasync def x():\n    return {'a': g.__wrapped__.__name__}\n",
        # OAuth2PasswordRequestForm's fields, built by the generator
        "from fastapi import Depends, FastAPI\nfrom fastapi.security import OAuth2PasswordRequestForm\n\napp = FastAPI()\n\n\n"
        "@app.post('/x')\nasync def x(form: OAuth2PasswordRequestForm = Depends()):\n"
        "    return {'a': [form.grant_type, form.scopes, form.client_id]}\n",
    ],
)
def test_runtime_attributes_accepted(tmp_path, source):
    """Attributes some runtime value provides are not refused as certain AttributeErrors (they were only accepted
    when another module of the project made every attribute open, as fixtures/dynapp did with a __getattr__)."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(source)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


def test_scope_state_keys_are_request_state_attributes(tmp_path):
    """A raw ASGI middleware's `scope.setdefault("state", {})["rid"] = v` is Starlette's `request.state.rid`."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI, Request\n\napp = FastAPI()\n\n\n"
        "class Stamp:\n    def __init__(self, app):\n        self.app = app\n\n"
        "    async def __call__(self, scope, receive, send):\n"
        "        scope.setdefault('state', {})['rid'] = 'r1'\n        await self.app(scope, receive, send)\n\n\n"
        "app.add_middleware(Stamp)\n\n\n"
        "@app.get('/x')\nasync def x(request: Request):\n    return {'a': request.state.rid}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


LIBS9_APP = """
import mimetypes

from fastapi import FastAPI
from itsdangerous import URLSafeSerializer

app = FastAPI()
S = URLSafeSerializer("k", salt="s")


@app.get("/x")
async def x(t: str):
    return {stmt}
"""


@pytest.mark.parametrize("stmt, msg", [
    ("mimetypes.add_type('a/b', '.ab')", "library call `mimetypes.add_type()` is not supported"),
    ("mimetypes.guess_type(t, strict=False, x=1)", "mimetypes.guess_type(x=) is not supported"),
    ("mimetypes.guess_type(t, False, 1)", "mimetypes.guess_type() takes at most 2 positional arguments here"),
    ("mimetypes.guess_type()", "mimetypes.guess_type() needs its url argument"),
    ("mimetypes.guess_file_type(t)", "mimetypes.guess_file_type() needs Python 3.13 (the project runs on 3.12"),
    ("mimetypes.types_map[t]", "mimetypes.types_map"),
    ("URLSafeSerializer('k', serializer=None).dumps(1)", "URLSafeSerializer(serializer=) is not supported"),
    ("S.loads_unsafe(t)", "method .loads_unsafe() is not implemented by the runtime"),
])
def test_libs_09_outside_subset_rejected(tmp_path, capsys, monkeypatch, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(LIBS9_APP.format(stmt=stmt))
    monkeypatch.setenv("PY2AXUM_TARGET_PYTHON", "3.12")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:13" in err


QUERY_MAIN = """
import itertools
import string

from fastapi import Depends, FastAPI, Query
from sqlalchemy import ARRAY, Integer, bindparam, case, func, literal_column, select, text, union_all
from sqlalchemy.dialects import postgresql
from sqlalchemy.ext.asyncio import AsyncSession

from .models import Team
from .deps import make

app = FastAPI()


def use(s, x):
    return {body}


@app.get("/x")
async def x(s: str, d=Depends({dep})):
    return use(s, d)
"""

QUERY_DEPS = """
from fastapi import Query

LIMIT = 3


def make(query: Query = Query(None), kind: type = str):
{factory}
"""

GOOD_FACTORY = "    def inner(v: list[str] = query):\n        return [kind(x) for x in v]\n\n    return inner\n"


@pytest.mark.parametrize("body, dep, factory, msg, where", [
    # window frames, recursive CTEs, LATERAL/CTE options: not rendered
    ("func.rank().over(order_by=Team.id, rows=(None, 0))", "make()", GOOD_FACTORY, ".over(rows=) is not supported", "main.py:17"),
    ("func.rank().over(Team.id)", "make()", GOOD_FACTORY, ".over() takes at most 0 positional arguments", "main.py:17"),
    ("select(Team.id).cte('c', recursive=True)", "make()", GOOD_FACTORY, ".cte(recursive=) is not supported", "main.py:17"),
    ("select(Team.id).cte('c', True)", "make()", GOOD_FACTORY, ".cte() takes at most 1 positional argument", "main.py:17"),
    ("select(Team.id).lateral('l', flat=True)", "make()", GOOD_FACTORY, ".lateral(flat=) is not supported", "main.py:17"),
    ("postgresql.array(s, type_=int)", "make()", GOOD_FACTORY, "array(items) only: type_= is not supported", "main.py:17"),
    ("literal_column('x', type_=int)", "make()", GOOD_FACTORY, "literal_column(text) only", "main.py:17"),
    ("union_all(select(Team.id), select(Team.id), x=1)", "make()", GOOD_FACTORY, "keyword arguments are not supported", "main.py:17"),
    ("select(Team.id).where(Team.name == func.any(bindparam('b', type_=ARRAY(Integer))))", "make()", GOOD_FACTORY,
     "bindparam(type_=) is supported with ARRAY(String()) only", "main.py:17"),
    ("list(itertools.chain([1], [2]))[0] + next(itertools.chain([s]))", "make()", GOOD_FACTORY,
     "itertools.chain() is supported consumed whole only", "main.py:17"),
    ("select(Team).subquery(text('SELECT 1').columns(Team.id))", "make()", GOOD_FACTORY,
     "method .columns() (text().columns(), a TextualSelect) is not supported", "main.py:17"),
    # a library function's name is no method of a runtime value: `re.compile` does not make `stmt.compile()` exist
    ("str(select(Team.id).compile())", "make()", GOOD_FACTORY, "method .compile() is not implemented by the runtime", "main.py:17"),
    # Formatter().parse() only consumed whole: CPython's iterator raises on the failing item, after the others
    ("[p for p in string.Formatter().parse(s)]", "make()", GOOD_FACTORY, "parse() inside list() or tuple()", "main.py:17"),
    # dependency factories: only functions defined then one returned; constant arguments named alike in both modules
    ("x", "make(Query([]), int)", "    n = 1\n\n    def inner(v: list[str] = query):\n        return v\n\n    return inner\n",
     "a dependency factory must only define functions and return one of them", "deps.py:7"),
    ("x", "make(Query([]), int)", "    def inner(v: list[str] = query):\n        kind = int\n        return v\n\n    return inner\n",
     "the factory parameter `kind` is reassigned in inner()", "deps.py:9"),
    ("x", "make(Query([]), int)", "    def inner(v: list[str] = query):\n        return [query, v]\n\n    return inner\n",
     "a FastAPI marker argument is only supported as a parameter default", "deps.py:9"),
    ("x", "make(Query([]), lambda v: v)", GOOD_FACTORY, "unsupported argument `lambda v: v`", "main.py:21"),
    ("x", "make(Query(LIMIT))", GOOD_FACTORY, "`LIMIT` does not name the same object", "main.py:21"),
    ("x", "make(Query(), str)", "    def inner(v: list[str] = query):\n        return query.default\n\n    return inner\n",
     "`query.default`: the marker has no default", "deps.py:9"),
])
def test_query_constructs_outside_subset_rejected(tmp_path, capsys, body, dep, factory, msg, where):
    """Window functions, CTEs, LATERAL, Formatter().parse() and dependency factories: what the runtime does not
    reproduce is refused with its line."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(MODELS.format(rel=""))
    (pkg / "deps.py").write_text(QUERY_DEPS.format(factory=factory))
    (pkg / "main.py").write_text(QUERY_MAIN.format(body=body, dep=dep))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert where in err, err


def test_column_type_keyword_with_positional_type_rejected(tmp_path, capsys):
    """mapped_column(type_=T) is the column's type; with a positional type too, SQLAlchemy raises: refused"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(MODELS.format(rel="size = mapped_column(String(5), type_=String(9))"))
    (pkg / "main.py").write_text(MAIN)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "column size: type_= with a positional type is not supported" in err, err
    assert "models.py:17" in err, err


NT_MAIN = """
from typing import NamedTuple

from fastapi import FastAPI

app = FastAPI()


class P(NamedTuple):
    a: int
{body}


@app.get("/x")
async def x():
    return P(1)
"""


@pytest.mark.parametrize("body, msg", [
    ("    _b: int = 0", "field names cannot start with an underscore: '_b'"),
    ("    b: list = []", "only a literal immutable default"),
    ("    b: int = len('x')", "only a literal immutable default"),
    ("    def __repr__(self):\n        return 'p'", "NamedTuple P: method __repr__ is not supported"),
    ("    def _hidden(self):\n        return 1", "NamedTuple P: method _hidden is not supported"),
    ("    def a(self):\n        return 1", "NamedTuple P: method a is not supported"),
])
def test_namedtuple_outside_subset_rejected(tmp_path, capsys, body, msg):
    """typing.NamedTuple: what CPython refuses at class creation, a default shared by every instance that is not a
    literal immutable value, and the tuple's own dunders or _methods overridden are refused with their line."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(NT_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:1" in err, err
ROUTE_CLASS_APP = """
from collections.abc import Callable

from fastapi import APIRouter, FastAPI, Request, Response
from fastapi.routing import APIRoute


class R(APIRoute):
{body}


app = FastAPI()
router = APIRouter(route_class=R)


@router.get("/x")
async def x():
    return {{}}


app.include_router(router)
{tail}
"""

GRH = """    def get_route_handler(self) -> Callable:
        original = super().get_route_handler()

        async def h(request: Request) -> Response:
            return await original(request)

        return h"""


@pytest.mark.parametrize("body, tail, msg", [
    (GRH.replace("original = super().get_route_handler()", "original = super().get_route_handler()\n        print(self.path)"), "",
     "R.get_route_handler(): `self` (the APIRoute: path, endpoint, dependant...) is not supported"),
    (GRH + "\n\n    def matches(self, scope):\n        return super().matches(scope)", "",
     "R.matches(): a custom route class may only override get_route_handler()"),
    (GRH, "app.router.route_class = R\napp.router.route_class = APIRoute",
     "`app.router.route_class` set more than once is not supported"),
    (GRH, "app.openapi = None", "assigning `app.openapi` at module level is not supported"),
])
def test_route_class_outside_subset_rejected(tmp_path, capsys, body, tail, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(ROUTE_CLASS_APP.format(body=body, tail=tail))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:" in err


CORE_SCHEMA_APP = """
from dataclasses import dataclass
from typing import Annotated, Any

from fastapi import FastAPI
from pydantic import BaseModel, GetPydanticSchema
from pydantic_core import core_schema


class T:
    @classmethod
    def __get_pydantic_core_schema__(cls, source: Any, handler) -> core_schema.CoreSchema:
{hook}


class M(BaseModel):
    x: {ann}


app = FastAPI()


@app.post("/x")
def x(m: M):
    return {use}
"""


@pytest.mark.parametrize("hook, ann, use, msg", [
    ("        return core_schema.union_schema([core_schema.int_schema(), core_schema.str_schema()])", "T", "1",
     "core_schema.union_schema() is not supported"),
    ("        raise ValueError('no')", "T", "1", "this hook raises (pydantic refuses the model)"),
    ("        return handler(source)", "T", "1", "handler(source) on the class that defines the hook is not supported"),
    ("        return core_schema.int_schema(multiple_of=2, foo=1)", "T", "1", "core_schema.int_schema(foo=) is not supported"),
    ("        return core_schema.no_info_after_validator_function(lambda v: cls, handler(int))", "T", "1",
     "a lambda using the hook's names is not supported"),
    ("        return core_schema.int_schema()", "Annotated[int, GetPydanticSchema(lambda tp, h: h(tp)), 'doc']", "1",
     "after a schema hook in Annotated[...] is not supported"),
    ("        print(source)\n        return core_schema.int_schema()", "T", "1", "`print(source)` is not supported"),
    ("        return core_schema.int_schema()", "T", "M.model_json_schema()", "model_json_schema"),
])
def test_core_schema_outside_subset_rejected(tmp_path, capsys, hook, ann, use, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(CORE_SCHEMA_APP.format(hook=hook, ann=ann, use=use))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:" in err


DC_APP = """
from fastapi import FastAPI
import pydantic.dataclasses as pdc


@pdc.dataclass{opts}
class P:
    name: str


app = FastAPI()


@app.post("/x")
def x(p: P):
    return {use}
"""


@pytest.mark.parametrize("opts, use, msg", [
    ("", "P(name='a')", "calling the pydantic dataclass P(...) is not supported"),
    ("(frozen=True)", "p", "@pydantic.dataclasses.dataclass(...) options are not supported (P)"),
])
def test_pydantic_dataclass_outside_subset_rejected(tmp_path, capsys, opts, use, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(DC_APP.format(opts=opts, use=use))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:" in err


@pytest.mark.parametrize("expr", ["2**62 - 1 + 2**62", "-(2**62) * 2", "[10**x for x in (3, 6, 18)]", "1 << 62", "2**-1",
                                  "[x * 2**40 for x in range(10**6)]", "10**18 // 3"])
def test_integer_expressions_in_range_accepted(tmp_path, expr):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(f"from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x():\n"
                                 f"    return {{'a': {expr}}}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


@pytest.mark.parametrize("use", ["await H.am(H(), 1)", "await (H.am)(H(), 1)", "len([H.am])"])
def test_async_method_read_on_its_class_rejected(tmp_path, capsys, use):
    """the runtime reads a method on its class as a function it runs at once: an `async def` would not make a
    coroutine (refused at run time too, through a class held as a value)"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\n\napp = FastAPI()\n\n\nclass H:\n    async def am(self, n):\n        return n\n\n\n"
        f"@app.get('/x')\nasync def x():\n    return {{'a': {use}, 'b': await H().am(2)}}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "the async method H.am read on its class is not supported" in err
    assert "P2A0101" in err and "main.py:13" in err


MAIL_DNS_APP = """
import email.policy
from email.message import EmailMessage

import dns.resolver
from fastapi import FastAPI

app = FastAPI()
R = dns.resolver.Resolver(configure=False)


@app.get("/x")
async def x(t: str):
    return {stmt}
"""


@pytest.mark.parametrize("stmt, msg", [
    ("EmailMessage(policy=email.policy.compat32)", "email.policy.compat32"),
    ("EmailMessage(email.policy.SMTP)", "EmailMessage() takes at most 0 positional arguments here"),
    ("EmailMessage().set_content(t, disposition='attachment')", ".set_content(disposition=) is not supported"),
    ("EmailMessage().set_content(b'x', maintype='application', subtype='pdf')", ".set_content(maintype=) is not supported"),
    ("EmailMessage().get_content()", "method .get_content() is not implemented by the runtime"),
    ("EmailMessage().add_attachment(b'x')", "method .add_attachment() is not implemented by the runtime"),
    ("dns.resolver.resolve(t, 'MX', source='10.0.0.1')", "resolve(source=) is not supported"),
    ("R.resolve(t, 'MX', source_port=5353)", ".resolve(source_port=) is not supported"),
    ("dns.resolver.Resolver(filename='/x', configure=True, extra=1)", "Resolver(extra=) is not supported"),
    ("dns.resolver.query(t, 'MX')", "dns.resolver.query"),
    ("R.resolve_name(t)", "method .resolve_name() is not implemented by the runtime"),
])
def test_email_dns_outside_subset_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(MAIL_DNS_APP.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:14" in err, err


@pytest.mark.parametrize("ann", ["P", "list[P] = []", "P | None = None"])
def test_namedtuple_as_pydantic_type_rejected(tmp_path, capsys, ann):
    """pydantic-core validates a NamedTuple field as call arguments (tuple, list or dict; named_tuple_type in 2.14,
    arguments_type before): validated like a model it gave other errors (corpus validation_errors.md:1664)"""
    err = _reject(tmp_path, capsys, f'''
        from typing import NamedTuple

        from fastapi import FastAPI
        from pydantic import BaseModel

        app = FastAPI()


        class P(NamedTuple):
            x: int


        class M(BaseModel):
            p: {ann}


        @app.post("/m")
        def m(item: M):
            return 1
    ''')
    assert "type `P` is not supported as a Pydantic type: a NamedTuple" in err, err
    assert "main.py:15" in err, err
PYDX_APP = """
import dataclasses
from datetime import date, datetime, timezone
from typing import Annotated, Any, NamedTuple

from fastapi import FastAPI
from pydantic import AwareDatetime, BaseModel, ConfigDict, Field, PrivateAttr, with_config
from zoneinfo import ZoneInfo


class P(NamedTuple):
    x: int


@dataclasses.dataclass
class D:
    name: str


{defs}


app = FastAPI()


@app.post("/x")
def x(m: M):
    return {use}
"""


@pytest.mark.parametrize("defs, use, msg", [
    ("class M(BaseModel):\n    p: P", "1", "type `P` is not supported as a Pydantic type: a NamedTuple"),
    ("class M(BaseModel):\n    p: list[P] = []", "1", "type `P` is not supported as a Pydantic type: a NamedTuple"),
    ("class M(BaseModel):\n    d: datetime = Field(gt=date(2000, 1, 1))", "1", "a `datetime` bound must be a datetime(...) value"),
    ("class M(BaseModel):\n    d: date = Field(lt=datetime(2000, 1, 1))", "1", "a `date` bound must be a date(...) value"),
    ("class M(BaseModel):\n    d: datetime = Field(gt=5)", "1", "a `datetime` bound must be a datetime(...) value"),
    ("class M(BaseModel):\n    d: AwareDatetime = Field(max_length=3)", "1", "max_length= does not apply to `datetime`"),
    ("class M(BaseModel):\n    n: int = Field(gt=datetime(2000, 1, 1))", "1", "a datetime bound does not apply to `int`"),
    ("class M(BaseModel):\n    d: datetime = Field(gt=datetime(2000, 13, 1))", "1", "raises ValueError: month must be in 1..12"),
    ("class M(BaseModel):\n    d: datetime = Field(gt=datetime(2000, 1, 1, tzinfo=ZoneInfo('Europe/Paris')))", "1",
     "`ZoneInfo('Europe/Paris')` is not a constant"),
    ("class M(BaseModel):\n    model_config = ConfigDict(strict=True)\n    d: D", "1",
     "dataclass D in a model with strict= or extra= in its model_config is not supported"),
    ("class M(BaseModel):\n    model_config = ConfigDict(extra='forbid')\n    d: list[D] = []", "1",
     "dataclass D in a model with strict= or extra= in its model_config is not supported"),
    ("@dataclasses.dataclass\n@with_config(extra='forbid')\nclass W:\n    a: str\n\n\nclass M(BaseModel):\n    w: W", "1",
     "@with_config(extra=) is not supported"),
    ("class M(BaseModel):\n    x: int\n    def model_post_init(self, context, other):\n        pass", "1",
     "only `def model_post_init(self, context)` is supported"),
    ("class M(BaseModel):\n    x: int\n    def model_post_init(self, context: Any) -> None:\n        pass", "M.model_construct(x=1)",
     "model_construct(): model_post_init is not supported, nor a private attribute with a computed default"),
    ("class M(BaseModel):\n    x: int\n    _at: datetime = PrivateAttr(default_factory=datetime.now)", "M.model_construct(x=1)",
     "model_construct(): model_post_init is not supported, nor a private attribute with a computed default"),
])
def test_pydantic_corpus_constructions_outside_subset_rejected(tmp_path, capsys, defs, use, msg):
    """NamedTuple fields (validated as call arguments, differently by pydantic version), datetime bounds of another
    type, a stdlib dataclass under a model whose strict=/extra= would apply to it, model_post_init's signature"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PYDX_APP.format(defs=defs, use=use))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:" in err


FAPI_APP = """
from typing import Annotated, Any

from fastapi import Body, Depends, FastAPI, HTTPException, Response, Security
from fastapi.responses import StreamingResponse
from fastapi.security import HTTPBearer, SecurityScopes
from pydantic import AfterValidator

app = FastAPI()
SCOPES = ["a"]


def gen():
    yield 1


def dep(s: SecurityScopes):
    return s.scopes


{defs}
"""


@pytest.mark.parametrize("defs, msg", [
    ("class B(HTTPBearer):\n    def make_not_authenticated_error(self):\n        return HTTPException(403, self.scheme_name)\n\n\n"
     "@app.get('/x')\ndef x(c=Depends(B())):\n    return 1", "reading `self` is not supported"),
    ("class B(HTTPBearer):\n    realm = 'x'\n\n\n@app.get('/x')\ndef x(c=Depends(B())):\n    return 1",
     "only `def make_not_authenticated_error(self)` and `def make_authenticate_headers(self)` overrides are supported"),
    ("@app.get('/x')\ndef x(s=Security(dep, scopes=SCOPES)):\n    return 1", "Security(scopes=) must be a literal list of strings"),
    ("@app.get('/x')\ndef x(s=Depends(dep, scopes=['a'])):\n    return 1", "Depends(scopes=) is not supported"),
    ("@app.get('/x')\ndef x(q: Annotated[list[int], AfterValidator(sorted)] = []):\n    return 1",
     "AfterValidator(sorted) on a parameter: a lambda or a function of the project is supported"),
    ("@app.post('/x')\ndef x(b: Annotated[dict, Body(), AfterValidator(lambda v: v)]):\n    return 1",
     "AfterValidator on a body parameter is not supported"),
    ("@app.get('/x')\ndef x(q: Annotated[str, AfterValidator(lambda v, info: v)] = 'a'):\n    return 1",
     "an AfterValidator taking `info` on a parameter is not supported"),
    ("@app.get('/x')\ndef x():\n    def g():\n        yield from gen()\n    return StreamingResponse(g())",
     "`yield from` is supported as a statement over a file"),
    ("class R(Response):\n    charset = 'latin-1'\n\n\n@app.get('/x', response_class=R)\ndef x():\n    return 1",
     "only `media_type = \"...\"` and `def render(self, content)` are supported"),
    ("class R(Response):\n    def render(self, content: Any) -> bytes:\n        return self.media_type.encode()\n\n\n"
     "@app.get('/x', response_class=R)\ndef x():\n    return 1", "response class R.render: reading `self` is not supported"),
    ("class R:\n    pass\n\n\n@app.get('/x', response_class=R)\ndef x():\n    return 1",
     "response_class=R: a subclass of one fastapi/starlette response class"),
    ("class R(StreamingResponse):\n    def render(self, content: Any) -> bytes:\n        return b''\n\n\n"
     "@app.get('/x', response_class=R)\ndef x():\n    yield b'a'", "a generator endpoint with a response class overriding render()"),
])
def test_fastapi_corpus_constructions_outside_subset_rejected(tmp_path, capsys, defs, msg):
    """security scheme subclasses and response class subclasses beyond their overridable hooks, Security scopes not
    literal, AfterValidator on parameters beyond functions of the project, `yield from` a generator"""
    err = _reject(tmp_path, capsys, FAPI_APP.format(defs=defs))
    assert msg in err, err
    assert "main.py:" in err


@pytest.mark.parametrize("call", [
    "create_model('M', x=int, __config__={'strict': True})",
    "create_model('Other', x=int)",
    "create_model('M', x=(int,))",
    "create_model('M', __validators__={'v': field_validator('x')(lambda cls, v: v)}, x=int)",
    "create_model(NAME, x=int)",
])
def test_create_model_outside_subset_rejected(tmp_path, capsys, call):
    """`create_model` is rewritten into a class only in the forms listed in py2axum/pydrewrite.py; any other call is
    left as written and refused"""
    err = _reject(tmp_path, capsys, f'''
        from fastapi import FastAPI
        from pydantic import create_model, field_validator

        NAME = "M"
        M = {call}
        app = FastAPI()


        @app.post("/x")
        def x(m: M):
            return 1
    ''')
    assert "create_model" in err or "is not a Pydantic model" in err, err
    assert "main.py:" in err, err


@pytest.mark.parametrize("opt", ["{'tracer_provider': None}", "{'tracing': 1}", "CFG", "{'exclude': lambda scope: True}"])
def test_fastapi_telemetry_outside_subset_rejected(tmp_path, capsys, opt):
    """FastAPI(telemetry=): only the literal on/off switches (the binary emits no OpenTelemetry data)"""
    err = _reject(tmp_path, capsys, f'''
        from fastapi import FastAPI

        CFG = {{"tracing": False}}
        app = FastAPI(telemetry={opt})


        @app.get("/x")
        def x():
            return 1
    ''')
    # FastAPI(telemetry=) appeared in FastAPI 0.142: before, any use is refused for that reason
    from importlib.metadata import version

    old = tuple(int(x) for x in version("fastapi").split(".")[:2]) < (0, 142)
    msg = "FastAPI(telemetry=...) appeared in FastAPI 0.142" if old else "FastAPI(telemetry=...) is supported as a dict literal"
    assert msg in err, err
    assert "main.py:" in err, err


def test_graphql_router_rejected(tmp_path, capsys):
    """strawberry's GraphQLRouter held in a module variable: a router of a library (refused, decision of 16/10),
    with the way to leave it to Python"""
    err = _reject(tmp_path, capsys, '''
        import strawberry
        from fastapi import FastAPI
        from strawberry.fastapi import GraphQLRouter


        @strawberry.type
        class Query:
            @strawberry.field
            def hello(self) -> str:
                return "x"


        graphql_app = GraphQLRouter(strawberry.Schema(query=Query))
        app = FastAPI()
        app.include_router(graphql_app, prefix="/graphql")
    ''')
    assert "`graphql_app` is a router of the library `strawberry`" in err, err
    assert "--python-side mount" in err and "main.py:16" in err, err


PROTO_APP = """
from fastapi import FastAPI
{imports}


class K:
    LIMIT = 3
{body}


app = FastAPI()


@app.get('/x')
async def x():
    return {{'a': {use}}}
"""


@pytest.mark.parametrize("imports, body, use, msg", [
    ("", "    def __hash__(self):\n        return 1", "len({K(): 1})", "class K: method __hash__ is not supported"),
    ("import pickle", "    def __deepcopy__(self, memo):\n        return self", "len(pickle.dumps(K()))",
     "class K: method __deepcopy__ is not supported"),
    ("", "    def __getstate__(self):\n        return {}", "K().__getstate__()", "class K: method __getstate__ is not supported"),
    # pickle reached after the class was compiled: refused once everything is compiled
    ("import pickle", "    def __getstate__(self):\n        return {}", "[K(), pickle.loads(pickle.dumps(1))]",
     "method __getstate__ is not supported (the application uses copy or pickle"),
    # a class keyword other than metaclass=ABCMeta, a class decorator, a dunder an exception class does not dispatch
    ("class M(type):\n    pass\n\n\nclass KM(metaclass=M):\n    pass", "", "KM()",
     "class KM: the class keyword `metaclass=M` is not supported"),
    ("import functools\n\n\n@functools.total_ordering\nclass KD:\n    pass", "", "KD()",
     "class KD: decorator @functools.total_ordering is not supported"),
    ("class E(Exception):\n    def __eq__(self, o):\n        return True", "", "E() == E()",
     "exception class E: method __eq__ is not supported"),
    ("", "    def f(self, n=LIMIT):\n        return n\n    LIMIT = 4", "K().f()", "LIMIT__rebound_later_in_class_K"),
    ("", "    def __index__(self):\n        return 1", "[1, 2][K()]", "class K: method __index__ is not supported"),
])
def test_protocol_outside_subset_rejected(tmp_path, capsys, imports, body, use, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PROTO_APP.format(imports=imports, body=body, use=use))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:" in err


def test_copy_methods_kept_when_nothing_copies(tmp_path):
    """`import copy` alone (a library pickling for multiprocessing): a class's copy methods are never called"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PROTO_APP.format(
        imports="import copy", body="    def __deepcopy__(self, memo):\n        return self", use="K() is not None"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


def test_module_function_attribute_rebound_rejected(tmp_path, capsys):
    """`mod.f = other` replaces a function of a module (monkeypatching): refused (only variables are rebound)"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "conf.py").write_text("def f():\n    return 1\n")
    (pkg / "main.py").write_text("from fastapi import FastAPI\nfrom . import conf\n\napp = FastAPI()\n\n\n@app.get('/x')\n"
                                 "async def x():\n    conf.f = len\n    return {'a': 1}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "assigning `conf.f` (an attribute of module proj.conf) is not supported" in err, err


DATELIB_MAIN = """
import time
import threading
from datetime import datetime

import cachetools
from cachetools import LRUCache, TLRUCache, TTLCache, cached, cachedmethod, keys
from cachetools.func import lru_cache
from dateutil import parser, tz
from fastapi import FastAPI

app = FastAPI()


@app.get("/x")
def x():
    return {body}
"""


@pytest.mark.parametrize("body, msg", [
    # dateutil: a custom parserinfo, the parser class
    ('parser.parse("2026", parserinfo=parser.parserinfo(dayfirst=True))', "dateutil.parser.parse(): a parserinfo is not supported"),
    ('parser.parse("2026", parser.parserinfo())', "dateutil.parser.parse(): a parserinfo is not supported"),
    ('parser.parser().parse("2026")', "library call `dateutil.parser.parser()` is not supported"),
    ('tz.tzrange("EST", -18000)', "library call `dateutil.tz.tzrange()` is not supported"),
    # cachetools: timers, sizes, keys and conditions the runtime does not reproduce; caches it does not have
    ('TTLCache(10, 60, timer=lambda: 0)', "cachetools.TTLCache(timer=...) is not supported (time.monotonic or time.time only)"),
    ('LRUCache(10, getsizeof=lambda v: 2)', "cachetools.LRUCache(getsizeof=...) is not supported (len only)"),
    ('cached(LRUCache(2), key=keys.typedkey)', "cachetools.cached(key=...) is not supported with cachetools.keys.typedkey"),
    ('cached(LRUCache(2), condition=threading.Condition())', "cachetools.cached(condition=...) is not supported"),
    ('TLRUCache(10, lambda k, v, t: t + 1)', "library call `cachetools.TLRUCache()` is not supported"),
    ('cachetools.RRCache(10)', "library call `cachetools.RRCache()` is not supported"),
    ('cachetools.LFUCache(10)', "library call `cachetools.LFUCache()` is not supported"),
    ('lru_cache(maxsize=3, typed=True)', "cachetools.func.lru_cache(typed=...) is not supported"),
])
def test_datelibs_outside_subset_rejected(tmp_path, capsys, body, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(DATELIB_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:17" in err, err


def test_cachetools_decorators_outside_subset_rejected(tmp_path, capsys):
    """@cached on an async def (the coroutine object would be cached), @cachedmethod with info=, its cache_clear(),
    on a function, through its class"""
    cases = [
        ('''
        @cached(cache=TTLCache(maxsize=4, ttl=60))
        async def f(x):
            return x


        @app.get("/x")
        async def x():
            return await f(1)
        ''', "@cached on an async function is not supported", 6),
        ('''
        class Repo:
            def __init__(self):
                self.c = LRUCache(4)

            @cachedmethod(lambda self: self.c, info=True)
            def find(self, x):
                return x


        @app.get("/x")
        def x():
            return Repo().find(1)
        ''', "cachetools.cachedmethod(info=...) is not supported", 10),
        ('''
        class Repo:
            def __init__(self):
                self.c = LRUCache(4)

            @cachedmethod(lambda self: self.c)
            def find(self, x):
                return x


        @app.get("/x")
        def x():
            r = Repo()
            r.find.cache_clear()
            return r.find(1)
        ''', "`.find.cache_clear` of a @cachedmethod is not supported (only its calls)", 18),
        ('''
        @cachedmethod(lambda self: self.c)
        def find(self, x):
            return x


        @app.get("/x")
        def x():
            return find(None, 1)
        ''', "@cachedmethod on the function find (not a method) is not supported", 6),
        ('''
        class Repo:
            def __init__(self):
                self.c = LRUCache(4)

            @cachedmethod(lambda self: self.c)
            async def find(self, x):
                return x


        @app.get("/x")
        async def x():
            return await Repo().find(1)
        ''', "@cachedmethod on an async method is not supported", 10),
    ]
    for i, (src, msg, line) in enumerate(cases):
        d = tmp_path / f"c{i}"
        d.mkdir()
        head = "from cachetools import LRUCache, TTLCache, cached, cachedmethod\nfrom fastapi import FastAPI\n\napp = FastAPI()\n"
        err = _reject(d, capsys, head + textwrap.dedent(src))
        assert msg in err, err
        assert f"main.py:{line}" in err, err


REQUEST_APP = '''
from fastapi import FastAPI, Request

app = FastAPI()


@app.get('/x')
async def x(request: Request):
    {body}
    return {{'a': 1}}
'''


@pytest.mark.parametrize("body, msg", [
    ("request.session", "attribute .session of a Request is not supported"),
    ("request.user", "attribute .user of a Request is not supported"),
    ("request.auth", "attribute .auth of a Request is not supported"),
    ("await request.close()", "attribute .close of a Request is not supported"),
    ("request.keys()", "attribute .keys of a Request is not supported"),
    ("request.nope", "attribute .nope of a Request is not supported"),
    ("request['type']", "attribute .__getitem__ of a Request"),
    ("'type' in request", "attribute .__contains__ of a Request"),
    ("sorted(request)", "attribute .__iter__ of a Request"),
    ("request.custom = 1", "attribute .custom of a Request cannot be assigned"),
    ("request.url_for('x')", "attribute .url_for of a Request is not supported"),
    ("await request.send_push_promise('/a')", "attribute .send_push_promise of a Request is not supported"),
])
def test_request_outside_subset_rejected(tmp_path, capsys, body, msg):
    """What Starlette's Request offers beyond the runtime's attributes: refused where the variable is a parameter
    annotated Request (the runtime would fail with AttributeError, or answer another value)"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(REQUEST_APP.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:" in err


CODEC_APP = '''
from fastapi import FastAPI

app = FastAPI()


@app.get('/x')
async def x():
    {body}
    return {{'a': 1}}
'''


@pytest.mark.parametrize("body, msg", [
    ("b'a'.decode('utf-8', 'surrogateescape')", "got 'surrogateescape'"),
    ("b'a'.decode(errors='xmlcharrefreplace')", "got 'xmlcharrefreplace'"),
    ("b'a'.decode('ascii', errors='namereplace')", "got 'namereplace'"),
    ("b'a'.decode(errors='nothandler')", "got 'nothandler'"),
    ("'a'.encode('ascii', 'namereplace')", "got 'namereplace'"),
])
def test_codec_error_handlers_outside_subset_rejected(tmp_path, capsys, body, msg):
    """errors= of decode / encode written as a literal: only the handlers the runtime renders as CPython does"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(CODEC_APP.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:" in err


TIMESTAMP_MAIN = """
import datetime
import time
from fastapi import FastAPI

app = FastAPI()


@app.get("/t")
async def t(x: float):
    return {stmt}
"""


@pytest.mark.parametrize("stmt, msg", [
    ("time.localtime(x)", "library call `time.localtime()` is not supported"),
    ("time.gmtime(x)", "library call `time.gmtime()` is not supported"),
    ("datetime.date.fromtimestamp(x).isoformat()", "library call `datetime.date.fromtimestamp()` is not supported"),
])
def test_real_timestamp_functions_rejected(tmp_path, capsys, stmt, msg):
    # 3.15: les fonctions C à horodatage acceptent tout réel ; celles qu'on ne traduit pas restent refusées
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(TIMESTAMP_MAIN.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:11" in err


@pytest.mark.parametrize("body, message", [
    # stripe-python 14 (dynrt/stripe.rs): the legacy resource API only
    ("    import stripe\n    return await stripe.Customer.create_async(email='a@b.c')",
     "library call `stripe.Customer.create_async()` is not supported"),
    ("    import stripe\n    client = stripe.StripeClient('sk_test_x')\n    return client.v1.customers.list()",
     "library call `stripe.StripeClient()` is not supported"),
    ("    import stripe\n    stripe.default_http_client = None\n    return {}", "assigning `stripe.default_http_client`"),
    ("    import stripe\n    stripe.proxy = 'http://proxy:3128'\n    return {}", "assigning `stripe.proxy`"),
    ("    import stripe\n    return stripe.Customer.construct_from({'id': 'cus_1'}, 'sk_test_x')",
     "library call `stripe.Customer.construct_from()` is not supported"),
    # a resource's instance methods (refresh, save, instance_url...) are not translated
    ("    import stripe\n    c = stripe.Customer.retrieve('cus_1')\n    return {'url': c.instance_url()}",
     "method .instance_url() is not implemented by the runtime"),
])
def test_stripe_outside_the_legacy_api_rejected(tmp_path, capsys, body, message):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x():\n" + body + "\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    assert "main.py:" in err


def test_stripe_error_module_before_14_0_1(tmp_path, capsys):
    """`stripe.error` appeared in 14.0.1: with 14.0.0 locked, `except stripe.error.StripeError` raises a bare
    AttributeError when an exception is tested against it (fixtures/stripeapp /errors-legacy): translated as that
    raise at `stripe.error`, never as the alias."""
    (tmp_path / "uv.lock").write_text('version = 1\n\n[[package]]\nname = "stripe"\nversion = "14.0.0"\n')
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "import stripe\nfrom fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get('/x')\nasync def x():\n"
        "    try:\n        return stripe.Customer.retrieve('cus_1')\n    except stripe.error.StripeError:\n        return {}\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "stripe::no_error_module(cx, Some(" in gen and "STRIPE_ERROR" not in gen.split("no_error_module")[1][:200]
    # from 14.0.1 on, stripe.error is stripe._error (`except stripe.error.CardError`)
    (tmp_path / "uv.lock").write_text('version = 1\n\n[[package]]\nname = "stripe"\nversion = "14.4.1"\n')
    (pkg / "main.py").write_text((pkg / "main.py").read_text().replace("stripe.error.StripeError", "stripe.error.CardError"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert "STRIPE_CARD_ERROR" in gen and "no_error_module" not in gen


@pytest.mark.parametrize("body, message", [
    ("    import secrets\n    return {'c': secrets.choice(x for x in 'ab')}", "library call `secrets.choice()` is not supported on a generator expression"),
    ("    import random\n    return {'c': random.choice(x for x in 'ab')}", "library call `random.choice()` is not supported on a generator expression"),
    ("    import tempfile\n    f = tempfile.NamedTemporaryFile(buffering=0)\n    return {}", "tempfile.NamedTemporaryFile(buffering=) is not supported"),
    ("    import tempfile\n    f = tempfile.NamedTemporaryFile('wb')\n    return {}", "tempfile.NamedTemporaryFile() with positional arguments"),
    ("    import tempfile\n    d = tempfile.TemporaryDirectory(mode=0o700)\n    return {}", "tempfile.TemporaryDirectory(mode=) is not supported"),
    ("    import tempfile\n    d = tempfile.TemporaryDirectory('', 'p', None, False, 1)\n    return {}",
     "tempfile.TemporaryDirectory() takes at most 4 positional arguments"),
    ("    import tempfile\n    return {'a': tempfile.gettempdir(1)}", "tempfile.gettempdir() takes no argument"),
])
def test_tempfile_outside_surface_rejected(tmp_path, capsys, body, message):
    test_python_semantics_rejected(tmp_path, capsys, body, message)


@pytest.mark.parametrize("body, message", [
    ("    from openpyxl import Workbook\n    wb = Workbook(write_only=True)\n    return {}", "openpyxl Workbook(write_only=) is not supported"),
    ("    from openpyxl.styles import Font\n    f = Font(underline='single')\n    return {}", "openpyxl Font(underline=) is not supported"),
    ("    from openpyxl.styles import Font\n    f = Font('Arial')\n    return {}", "openpyxl Font() with positional arguments"),
    ("    from openpyxl.styles import PatternFill\n    f = PatternFill(fgColor='FF0000', patternType='solid', bogus=1)\n    return {}",
     "openpyxl PatternFill(bogus=) is not supported"),
    ("    from openpyxl.utils import get_column_letter\n    return {'a': get_column_letter(col_idx=3)}", "get_column_letter() takes the column index"),
])
def test_openpyxl_outside_surface_rejected(tmp_path, capsys, body, message):
    test_python_semantics_rejected(tmp_path, capsys, body, message)


@pytest.mark.parametrize("body, message", [
    ("    from reportlab.platypus import Paragraph\n    p = Paragraph('x', None, bulletText='-')\n    return {}",
     "reportlab Paragraph(bulletText=) is not supported"),
    ("    from reportlab.lib.styles import ParagraphStyle\n    s = ParagraphStyle('a', underlineWidth=2)\n    return {}",
     "reportlab ParagraphStyle(underlineWidth=) is not supported"),
    ("    import io\n    from reportlab.platypus import SimpleDocTemplate\n    d = SimpleDocTemplate(io.BytesIO(), showBoundary=1)\n"
     "    return {}", "reportlab SimpleDocTemplate(showBoundary=) is not supported"),
    ("    from reportlab.platypus import Spacer\n    s = Spacer(1, 2, True)\n    return {}",
     "reportlab Spacer() takes at most 2 positional arguments"),
    ("    from reportlab.platypus import PageBreak\n    b = PageBreak('Later')\n    return {}",
     "reportlab PageBreak() takes at most 0 positional arguments"),
    ("    from reportlab.graphics.shapes import Rect\n    r = Rect(0, 0, 1, 1, fontName='x')\n    return {}",
     "reportlab Rect(fontName=) is not supported"),
    ("    from reportlab.platypus import KeepTogether\n    k = KeepTogether([])\n    return {}",
     "library call `reportlab.platypus.KeepTogether()` is not supported"),
    ("    from reportlab.pdfgen import canvas\n    c = canvas.Canvas('x.pdf')\n    return {}",
     "library call `reportlab.pdfgen.canvas.Canvas()` is not supported"),
    ("    from reportlab.lib.colors import CMYKColor\n    c = CMYKColor(0, 0, 0, 1)\n    return {}",
     "library call `reportlab.lib.colors.CMYKColor()` is not supported"),
])
def test_reportlab_outside_surface_rejected(tmp_path, capsys, body, message):
    test_python_semantics_rejected(tmp_path, capsys, body, message)


def rl_accel_project(tmp_path):
    (tmp_path / "requirements.txt").write_text("reportlab==4.4.5\nrl_accel==0.9.0\n")
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("import io\n\nfrom fastapi import FastAPI\nfrom reportlab.lib import colors\n"
                                 "from reportlab.platypus import SimpleDocTemplate\n\napp = FastAPI()\n\n\n"
                                 "@app.get('/x')\nasync def x():\n    SimpleDocTemplate(io.BytesIO())\n    return {}\n\n\n"
                                 "@app.get('/y')\nasync def y():\n    return {'r': colors.red.red}\n")
    return pkg


def test_reportlab_with_rl_accel_rejected(tmp_path, capsys):
    # rl_accel replaces reportlab's pure-Python fp_str, escapes and widths: the routes making documents are refused
    pkg = rl_accel_project(tmp_path)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "reportlab SimpleDocTemplate() with rl-accel 0.9.0 installed is not supported" in err and "main.py:12" in err


def test_reportlab_with_rl_accel_left_to_python(tmp_path, capsys):
    pkg = rl_accel_project(tmp_path)
    from py2axum.__main__ import main as cli
    assert cli(["check", str(pkg), "--root", str(tmp_path), "--python-side", "auto", "--json"]) == 0
    routes = {r["path"]: r for r in json.loads(capsys.readouterr().out)["routes"]}
    assert routes["/x"]["status"] == "python-side" and "rl-accel 0.9.0" in routes["/x"]["reason"]
    assert routes["/y"]["status"] == "native"


def lxml_project(tmp_path):
    (tmp_path / "requirements.txt").write_text("openpyxl==3.1.5\net-xmlfile==2.0.0\nlxml==6.0.2\n")
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("from fastapi import FastAPI\nfrom openpyxl import Workbook\nfrom openpyxl.styles import Font\n\n"
                                 "app = FastAPI()\n\n\n@app.get('/x')\nasync def x():\n    Workbook()\n    return {}\n\n\n"
                                 "@app.get('/y')\nasync def y():\n    Font(bold=True)\n    return {}\n")
    return pkg


def test_openpyxl_with_lxml_rejected(tmp_path, capsys):
    # openpyxl writes through lxml when it is installed: other bytes than the ElementTree writer dynrt/xlsx.rs
    # follows. The routes building a workbook are refused (or left to Python), not the whole application.
    pkg = lxml_project(tmp_path)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "openpyxl Workbook() with lxml 6.0.2 installed is not supported" in err and "main.py:10" in err


def test_openpyxl_with_lxml_left_to_python(tmp_path, capsys):
    pkg = lxml_project(tmp_path)
    assert main([str(pkg), "--root", str(tmp_path), "--python-side", "auto", "-o", str(tmp_path / "out")]) == 0
    capsys.readouterr()
    from py2axum.__main__ import main as cli
    assert cli(["check", str(pkg), "--root", str(tmp_path), "--python-side", "auto", "--json"]) == 0
    routes = {r["path"]: r for r in json.loads(capsys.readouterr().out)["routes"]}
    assert routes["/x"]["status"] == "python-side" and "lxml 6.0.2" in routes["/x"]["reason"]
    assert routes["/y"]["status"] == "native"


MAKER_WRAP_APP = """
import asyncio
from collections import defaultdict

from fastapi import Depends, FastAPI
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine


class Wrap:
    make_engine = staticmethod(create_async_engine)

    def __init__(self, **kwargs):
        self.params = {{"autoflush": kwargs.pop("autoflush", False),
                       "expire_on_commit": kwargs.pop("expire_on_commit", False)}}
        self.engine_params = kwargs
        self.per = defaultdict(self.new)

    def __call__(self):
        return self.per[asyncio.get_running_loop()]()

    def new(self):
        return async_sessionmaker(bind=self.make_engine(**self.engine_params), **self.params)


MAKER = Wrap(url="postgresql+psycopg://x/y", connect_args={connect_args})


async def get_db():
    async with MAKER() as db:
        yield db


app = FastAPI()


@app.get("/x")
async def x({param}):
    {body}
    return {{}}
"""


@pytest.mark.parametrize("connect_args, param, body, msg", [
    # psycopg decodes timestamptz in each connection's zone: not a per-transaction setting
    ('{"options": "-c TimeZone=UTC"}', "", "s = MAKER()", "a TimeZone in the connect_args of a session maker wrapper"),
    # the request's session (a dependency) runs on the process pool as is
    ('{"options": "-c statement_timeout=5s"}', "db: AsyncSession = Depends(get_db)", "pass",
     "connect_args= of the session maker it calls is not supported"),
])
def test_session_maker_wrapper_options_rejected(tmp_path, capsys, connect_args, param, body, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(MAKER_WRAP_APP.format(connect_args=connect_args, param=param, body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err and "main.py:" in err, err


@pytest.mark.parametrize("body, message", [
    # pgpy: PGPKey.from_blob() only (dynrt/pgpy.rs)
    ("    import pgpy\n    return {'a': str(pgpy.PGPMessage.from_blob('x'))}",
     "library call `pgpy.PGPMessage.from_blob()` is not supported: of pgpy, only PGPKey.from_blob()"),
    ("    import pgpy\n    key = pgpy.PGPKey()\n    return {}", "library call `pgpy.PGPKey()` is not supported"),
    ("    import pgpy\n    key, _ = pgpy.PGPKey.new(1, 2048)\n    return {}", "library call `pgpy.PGPKey.new()` is not supported"),
    ("    import pgpy\n    key, _ = pgpy.PGPKey.from_blob('x')\n    return {'a': key.frobnicate_pgp()}",
     "method .frobnicate_pgp() is not implemented by the runtime"),
    # sys.modules: only the key of a module the project's Python cannot import
    ("    import sys\n    return {'a': 'json' in sys.modules}", "sys.modules['json'] is not supported"),
    ("    import sys\n    return {'a': sys.modules.get('os')}", "sys.modules['os'] is not supported"),
    ("    import sys\n    m = sys.modules\n    return {}", "sys.modules is only supported with a literal key"),
])
def test_pgpy_and_sys_modules_outside_surface_rejected(tmp_path, capsys, body, message):
    test_python_semantics_rejected(tmp_path, capsys, body, message)


@pytest.mark.parametrize("stmt, message", [
    ("async with s.execute(select(Team)):\n        pass",
     "`async with s.execute(select(Team))`: the value of .execute() of a session, connection or result is not an "
     "asynchronous context manager"),
    ("with s.scalar(select(Team)):\n        pass", "`with s.scalar(select(Team))`: the value of .scalar() of a session"),
])
def test_with_on_a_session_value_rejected(tmp_path, capsys, stmt, message):
    """A `with` / `async with` on what a session method returns (a coroutine, a result): CPython's TypeError at run
    time, refused at generation (a 500 otherwise); `session.begin()` / `begin_nested()` stay context managers."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "models.py").write_text(textwrap.dedent(MODELS).replace("{rel}", ""))
    body = SESSION_MAIN.format(opts="", body="    async with Session() as s:\n        yield s")
    body = body.replace("from fastapi import", "from sqlalchemy import select\nfrom fastapi import").replace(
        "    t = await s.get(Team, team_id)\n", f"    {stmt}\n    async with s.begin_nested():\n        pass\n"
        "    t = await s.get(Team, team_id)\n")
    (pkg / "main.py").write_text(body)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err and "main.py:" in err, err


STACK_APP = '''
from fastapi import FastAPI
from starlette.middleware.base import BaseHTTPMiddleware
{imports}


class Mw(BaseHTTPMiddleware):
    async def dispatch(self, request, call_next):
        return await call_next(request)


def setup(a):
    a.state.ready = True
    return 1


app = FastAPI()
app.add_middleware(Mw)
{tail}


@app.get("/x")
async def x():
    return {{"v": {read}}}
'''


@pytest.mark.parametrize("imports, tail, read, message", [
    # a function given the application, its result kept (the transpiler raised StopIteration)
    ("", "done = setup(app)", "1", "setup(app) inside `Assign` is not supported"),
    ("", "def make_plugin():\n    def build():\n        sub = FastAPI()\n        setup(sub)\n        return sub\n"
         "    return build", "1", "setup(sub) inside `FunctionDef` is not supported"),
    # a router prefix read at startup that does not translate, next to a project middleware class
    # (AttributeError: 'str' object has no attribute 'module' on its ("prefix", i) node)
    ("import mysterylib_p2a\nfrom fastapi import APIRouter",
     "router = APIRouter()\n\n\n@router.get('/y')\nasync def y():\n    return 1\n\n\n"
     "app.include_router(router, prefix=mysterylib_p2a.prefix())", "1", "mysterylib_p2a"),
])
def test_stack_statements_refused_not_crashed(tmp_path, capsys, imports, tail, read, message):
    """`py2axum check` and the generation refuse with file:line, never with a Python exception."""
    from py2axum.check import main as check_main

    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(STACK_APP.format(imports=imports, tail=tail, read=read))
    (tmp_path / ".git").mkdir()
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err and "main.py:" in err, err
    assert check_main([str(pkg), "--root", str(tmp_path)]) == 1
    out = capsys.readouterr()
    assert message in out.out + out.err and "Traceback" not in out.err


def _bench_project(tmp_path, main_py: str, files: dict[str, bytes] | None = None):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_bytes(main_py.encode() if isinstance(main_py, str) else main_py)
    for name, data in (files or {}).items():
        (tmp_path / name).write_bytes(data)
    (tmp_path / ".git").mkdir()
    return pkg


def test_route_path_given_as_keyword(tmp_path, capsys):
    """`@router.post(path="/x")` (IndexError before): the path is read; a decorator without any path is refused."""
    app = "from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.post(path=\"/x\", status_code=201)\nasync def x():\n    return 1\n"
    pkg = _bench_project(tmp_path, app)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0, capsys.readouterr().err
    assert "/// POST /x" in (tmp_path / "out" / "src" / "gen.rs").read_text()
    (pkg / "main.py").write_text(app.replace('path="/x", ', ""))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out2")]) == 1
    assert "has no path" in capsys.readouterr().err


def test_requirements_in_utf16_read_as_pip_does(tmp_path, capsys):
    """`pip freeze > requirements.txt` under PowerShell writes UTF-16 with a BOM (UnicodeDecodeError before)."""
    app = "from fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get(\"/\")\nasync def r():\n    return 1\n"
    pkg = _bench_project(tmp_path, app, {"requirements.txt": "fastapi==0.1.0\r\n".encode("utf-16")})
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "fastapi" in err and "0.1.0" in err and "Traceback" not in err, err  # the pin, read, is out of range
    (tmp_path / "requirements.txt").write_bytes(b"\xff\xfe\x00\xd8")  # a BOM then a lone surrogate
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out2")]) == 1
    assert "cannot be read as text" in capsys.readouterr().err


def test_module_with_a_pep263_coding_line(tmp_path, capsys):
    """A module declared `# -*- coding: latin-1 -*-` is decoded as Python does (UnicodeDecodeError before)."""
    app = ("# -*- coding: latin-1 -*-\nfrom fastapi import FastAPI\n\napp = FastAPI()\n\n\n@app.get(\"/\")\n"
           "async def r():\n    return {\"v\": \"\xe9t\xe9\"}\n").encode("latin-1")
    pkg = _bench_project(tmp_path, app)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0, capsys.readouterr().err


def test_global_declared_by_its_annotation_only(tmp_path, capsys):
    """`X: int` then `global X; X = ...` (AttributeError: 'NoneType' object has no attribute '_fields' before)."""
    app = ("from fastapi import FastAPI\n\napp = FastAPI()\nCOUNT: int\n\n\ndef bump():\n    global COUNT\n    COUNT = 1\n\n\n"
           "@app.get(\"/\")\nasync def r():\n    bump()\n    return COUNT\n")
    pkg = _bench_project(tmp_path, app)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "declares the module global `COUNT` without a value" in err and "main.py:4" in err, err




QR_MAIN = """
import qrcode
from qrcode.image.pure import PyPNGImage

from fastapi import FastAPI

app = FastAPI()


def use():
    {body}


@app.get("/x")
async def x():
    return use()
"""


@pytest.mark.parametrize("body, msg", [
    ("return qrcode.QRCode(image_factory=PyPNGImage)", "library value `qrcode.image.pure.PyPNGImage` is not supported"),
    ("return qrcode.QRCode(image_factory=dict)", "qrcode.QRCode(image_factory=) is not supported"),
    ("return qrcode.QRCode(1, 0, 10, 4, dict)", "qrcode.QRCode(image_factory=) is not supported"),
    ("return qrcode.make('x', image_factory=dict)", "qrcode.make(image_factory=) is not supported"),
    ("return qrcode.QRCode(size=3)", "qrcode.QRCode(size=) is not supported"),
    ("return qrcode.make('x', 'y')", "qrcode.make(data, **kwargs): the QRCode arguments by keyword"),
    ("return qrcode.make('x', fit=True)", "qrcode.make(fit=) is not supported"),
])
def test_qrcode_options_rejected(tmp_path, capsys, body, msg):
    """qrcode: another image factory than Pillow's PilImage (its own PNG, SVG...) is refused, with its line."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(QR_MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:11" in err


def test_qrcode_without_pillow_rejected(tmp_path, capsys):
    """A uv.lock with qrcode but not Pillow: qrcode draws with PyPNGImage, whose PNG is not reproduced."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(QR_MAIN.format(body="return qrcode.QRCode()"))
    (tmp_path / "uv.lock").write_text('version = 1\n\n[[package]]\nname = "qrcode"\nversion = "8.2"\n')
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out"),
                 "--allow-untested-versions"]) == 1
    err = capsys.readouterr().err
    assert "qrcode without Pillow" in err
    assert "main.py:11" in err


@pytest.mark.parametrize("body, message", [
    ("    from pathlib import Path\n    return {'a': str(Path.cwd('x'))}", "Path.cwd() takes no arguments here"),
    ("    import os\n    return {'a': os.getcwd(1)}", "os.getcwd() takes no arguments here"),
])
def test_cwd_outside_surface_rejected(tmp_path, capsys, body, message):
    test_python_semantics_rejected(tmp_path, capsys, body, message)




PASSLIB_MAIN = """
from fastapi import FastAPI
from passlib.context import CryptContext

app = FastAPI()
ctx = {stmt}


@app.get("/h")
async def h(pw: str):
    return {{"h": ctx.hash(pw)}}
"""


@pytest.mark.parametrize("stmt, msg", [
    ("CryptContext(schemes=['argon2'])", "schemes must be the literal [\"bcrypt\"]"),
    ("CryptContext(schemes=['bcrypt', 'sha256_crypt'], deprecated='auto')", "schemes must be the literal [\"bcrypt\"]"),
    ("CryptContext()", "schemes must be the literal [\"bcrypt\"]"),
    ("CryptContext(schemes=['bcrypt'], bcrypt__rounds=10)", "bcrypt__rounds= is not supported"),
    ("CryptContext(schemes=['bcrypt'], deprecated=['bcrypt'])", "deprecated must be \"auto\""),
    ("CryptContext(schemes=list(('bcrypt',)))", "schemes must be the literal [\"bcrypt\"]"),
    ("CryptContext(**{'schemes': ['bcrypt']})", "`*`/`**` arguments are not supported"),
])
def test_passlib_outside_subset_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PASSLIB_MAIN.format(stmt=stmt))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:6" in err


@pytest.mark.parametrize("stmt, msg", [
    ("ctx.using(rounds=5).hash(pw)", "CryptContext.using is not supported"),
    ("ctx.hash(pw, scheme='bcrypt')", "CryptContext.hash(scheme=) is not supported"),
    ("ctx.verify(pw, pw, 'bcrypt')", "CryptContext.verify() with 3 positional arguments is not supported"),
    ("ctx.handler()", "CryptContext.handler is not supported"),
    ("ctx.schemes", "CryptContext.schemes is not supported"),
])
def test_passlib_methods_outside_subset_rejected(tmp_path, capsys, stmt, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(PASSLIB_MAIN.format(stmt="CryptContext(schemes=['bcrypt'])").replace("ctx.hash(pw)", stmt))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err
    assert "main.py:11" in err


def test_httpx_request_attribute_outside_subset_rejected(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("""
import httpx
from fastapi import FastAPI

app = FastAPI()


@app.get("/c")
async def c():
    async with httpx.AsyncClient() as client:
        try:
            await client.get("http://127.0.0.1:9/")
        except httpx.RequestError as e:
            return {"h": dict(e.request.headers)}
""")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "httpx.Request.headers is not supported" in err
    assert "main.py:14" in err




DEP_FORMS_MAIN = """from fastapi import Depends, FastAPI

from .other import Other

app = FastAPI()


{defs}


@app.get('/x')
def x(v=Depends({dep})):
    return 1
"""


@pytest.mark.parametrize("defs, dep, msg, line", [
    # generator dependencies: yields reached at most once, as statements
    ("def d():\n    for i in range(2):\n        yield i", "d", "a `yield` in a loop is not supported", 10),
    ("def d():\n    yield 1\n    yield 2", "d", "two `yield`s on one path are not supported", 10),
    ("def d():\n    try:\n        yield 1\n    except ValueError:\n        yield 2", "d", "two `yield`s on one path", 12),
    ("def d(k: int = 0):\n    if k:\n        yield 1\n    yield 2", "d", "two `yield`s on one path", 11),
    ("def d():\n    v = yield 1", "d", "the `yield` must be a statement, under `if`, `try` or `with` blocks only", 9),
    ("def d():\n    yield from [1]", "d", "`yield from` is not supported", 9),
    # class dependencies: the signature FastAPI reads must be one the translator reads too
    ("class P(dict):\n    pass", "P", "a class dependency's bases may be `ABC`, `object` and one plain class", 8),
    ("class B:\n    def __new__(cls, n: int = 1):\n        return object.__new__(cls)\n\n\nclass P(B):\n    pass", "P",
     "B.__new__: a class dependency defining __new__ is not supported", 9),
    ("class P:\n    __init__ = object.__init__", "P", "assigned in the class body of a class dependency", 9),
    # methods: a classmethod or a staticmethod, found along the bases
    ("class S:\n    def make(self, n: int = 1):\n        return n", "S.make", "must be a @classmethod or a @staticmethod", 9),
    ("class S:\n    pass", "S.make", "S has no method `make`", 13),
    ("class S:\n    @classmethod\n    def make(cls, *ns: int):\n        return ns", "S.make",
     "*args, **kwargs, positional-only parameters or `yield` are not supported in a method dependency", 10),
    ("class S:\n    @classmethod\n    def make(cls):\n        yield cls", "S.make", "in a method dependency", 10),
    # a library's object (or a global built by calling one): FastAPI reads the signature of what it made
    ("import secrets\n\nmake_token = secrets.token_hex", "make_token", "`secrets.token_hex`, of the library secrets: FastAPI "
     "reads the signature of what the library makes", "main.py:"),
    ("class Svc:\n    def current(self):\n        return 1\n\n\nsvc = Svc()", "svc.current",
     "a method bound to an instance is not supported", "main.py:"),
    # lambdas, factories
    ("", "lambda *a: a", "Depends(lambda ...): *args, **kwargs or positional-only parameters", 12),
    ("def f(**kw):\n    def g():\n        return kw\n    return g", "f(a=1)",
     "a dependency factory must only define functions and return one of them", 8),
])
def test_dependency_forms_rejected(tmp_path, capsys, defs, dep, msg, line):
    """Forms of Depends() met in public applications: what FastAPI would read differently is refused at its line."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "other.py").write_text("from fastapi import Query as Q\n\n\nclass Other:\n    def __init__(self, n: int = Q(1)):\n"
                                  "        self.n = n\n\n    @classmethod\n    def build(cls, n: 'Q' = 1):\n        return cls(n)\n")
    (pkg / "main.py").write_text(DEP_FORMS_MAIN.format(defs=defs, dep=dep))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert (line if isinstance(line, str) else f"main.py:{line}") in err, err
@pytest.mark.parametrize("body, msg", [
    # bound after the endpoint reading it: Python's NameError when the factory runs
    ("    @app.get('/x')\n    def x(v=Depends(dep)):\n        return v\n\n    def dep():\n        return 1\n",
     "dependency `dep` must be a project function or class"),
    # bound twice in the factory: which one an endpoint reads depends on the order
    ("    def dep():\n        return 1\n\n    if flag:\n        def dep():\n            return 2\n\n"
     "    @app.get('/x')\n    def x(v=Depends(dep)):\n        return v\n", "dependency `dep` must be a project function or class"),
    ("    Auth = Annotated[int, Depends(lambda: 1)]\n    Auth = int\n\n    @app.get('/x')\n    def x(v: Auth):\n        return v\n",
     "unsupported type annotation `Auth`"),
])
def test_factory_dependency_rejected(tmp_path, capsys, body, msg):
    """A dependency or an Annotated alias of the application factory: bound once, before the endpoint reading it."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text("from typing import Annotated\n\nfrom fastapi import Depends, FastAPI\n\n\n"
                                 "def create_app(flag: bool = False):\n    app = FastAPI()\n\n" + body + "\n    return app\n\n\n"
                                 "app = create_app()\n")
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err and "main.py:" in err, err




TAM_MAIN = '''
import enum

from fastapi import Depends, FastAPI
from sqlalchemy import String
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


class Status(str, enum.Enum):
    ON = "on"


class Base(DeclarativeBase):
    type_annotation_map = {{Status: String(16), str: String(80)}}


class Item(Base):
    __tablename__ = "items"
    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str]
{extra}

engine = create_async_engine("postgresql+psycopg://x/y")
Session = async_sessionmaker(engine)
app = FastAPI()


async def db():
    async with Session() as s:
        yield s


@app.get("/items/{{item_id}}")
async def item(item_id: int, s: AsyncSession = Depends(db)):
    return {{"name": (await s.get(Item, item_id)).name}}
'''


@pytest.mark.parametrize("extra, code", [("", 0), ("    status: Mapped[Status]", 1)])
def test_type_annotation_map_project_class_key(tmp_path, capsys, extra, code):
    """a project class (an Enum) as a key of the base's type_annotation_map: refused at the column annotated
    with it, the other columns and classes of the base unaffected"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(TAM_MAIN.format(extra=extra))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == code
    if code:
        err = capsys.readouterr().err
        assert "column status: the type_annotation_map of Base maps `Status`" in err
        assert "main.py:" in err


DECO_MAIN = '''
import functools

from fastapi import APIRouter, FastAPI, HTTPException, WebSocket

app = FastAPI()
router = APIRouter()


def require_admin(fn):
    @functools.wraps(fn)
    async def wrapper(*args, **kwargs):
        raise HTTPException(403)
    return wrapper


@app.get("/ok")
async def ok():
    return {{"ok": True}}


{endpoint}


app.include_router(router)
'''


@pytest.mark.parametrize("endpoint, offset, message", [
    # below the route decorator: FastAPI serves the wrapper (here, always 403)
    ('@app.get("/x")\n@require_admin\nasync def x():\n    return {"x": 1}', 1,
     "endpoint x: decorator @require_admin on an endpoint is not supported: FastAPI registers the decorated function"),
    ('@router.post("/x")\n@functools.lru_cache(maxsize=8)\ndef x():\n    return {"x": 1}', 1,
     "endpoint x: decorator @functools.lru_cache(maxsize=8) on an endpoint is not supported"),
    # above: runs at import on the registered function
    ('@require_admin\n@app.get("/x")\nasync def x():\n    return {"x": 1}', 0,
     "decorator @require_admin on an endpoint is not supported: it runs at import"),
    # a second route decorator registers the function once more
    ('@app.get("/x")\n@app.post("/y")\nasync def x():\n    return {"x": 1}', 0, "several route decorators on one function"),
    ('@router.get("/x")\n@router.put("/x")\nasync def x():\n    return {"x": 1}', 0, "several route decorators on one function"),
    ('@app.websocket("/ws")\n@require_admin\nasync def x(ws: WebSocket):\n    await ws.accept()', 1,
     "endpoint x: decorator @require_admin on an endpoint is not supported"),
])
def test_endpoint_decorator_rejected(tmp_path, capsys, endpoint, offset, message):
    """a decorator on an endpoint besides its route decorator was ignored: the binary served the bare function
    (an authentication decorator skipped in production). Refused at the decorator's line, P2A0318."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    src = DECO_MAIN.format(endpoint=endpoint)
    (pkg / "main.py").write_text(src)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert message in err
    first = src.splitlines().index(endpoint.splitlines()[0]) + 1  # the endpoint's first decorator
    assert f"main.py:{first + offset}:" in err
    assert "P2A0318" in err


def test_endpoint_decorator_left_to_python(tmp_path):
    """with --python-side auto the decorated route goes to Python, the others stay native"""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(DECO_MAIN.format(endpoint='@app.get("/x")\n@require_admin\nasync def x():\n    return {"x": 1}'))
    assert main([str(pkg), "--root", str(tmp_path), "--python-side", "auto", "-o", str(tmp_path / "out")]) == 0


@pytest.mark.parametrize("decorator", ['@app.api_route("/x", methods=["GET", "POST"])', '@app.head("/x")',
                                       '@router.options("/x")', '@app.trace("/x")', '@app.route("/x")'])
def test_untranslated_route_decorator_rejected(tmp_path, capsys, decorator):
    """route decorators outside get/post/put/patch/delete/websocket left the function unregistered: the binary
    answered 404/405 where FastAPI served the route. Refused (P2A0318)."""
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(DECO_MAIN.format(endpoint=f"{decorator}\nasync def x():\n    return {{\"x\": 1}}"))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert f"route decorator {decorator.split('(')[0]}(...) is not supported" in err
    assert "main.py:22:" in err and "P2A0318" in err

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

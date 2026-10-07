"""The transpiler must refuse code outside the subset, with a precise file:line message."""
import textwrap
from pathlib import Path

import pytest

from py2axum.codegen import TranspileErrors, generate
from py2axum.frontend import Frontend
from py2axum.ir import TranspileError

HEADER = '''
from fastapi import Depends, FastAPI
from pydantic import BaseModel
from sqlalchemy import String, select
from sqlalchemy.ext.asyncio import AsyncSession
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column
import requests

class Base(DeclarativeBase):
    pass

class Item(Base):
    __tablename__ = "items"
    id: Mapped[int] = mapped_column(primary_key=True)
    label: Mapped[str] = mapped_column(String(50))

app = FastAPI()
'''


def transpile(tmp_path: Path, handler: str) -> None:
    pkg = tmp_path / "pkg"
    pkg.mkdir()
    (pkg / "main.py").write_text(HEADER + textwrap.dedent(handler))
    app = Frontend(pkg).run()
    generate(app, tmp_path / "out", "pkg", "pkg_axum")


@pytest.mark.parametrize(
    "handler, message",
    [
        (
            '''
            from fastapi import APIRouter
            async def auth():
                return None
            router = APIRouter(prefix="/r", dependencies=[Depends(auth)])
            app.include_router(router)

            @router.get("/x")
            async def x():
                return {}
            ''',
            "router-level dependencies",
        ),
        (
            '''
            from fastapi import APIRouter
            PREFIX = "/api"
            router = APIRouter()
            app.include_router(router, prefix=PREFIX)

            @router.get("/x")
            async def x():
                return {}
            ''',
            "include_router(prefix=...) from a runtime value (settings, env) is only supported by the dyn backend",
        ),
        (
            '''
            @app.exception_handler(ValueError)
            async def on_value_error(request, exc):
                return None

            @app.get("/x")
            async def x():
                return {}
            ''',
            "@app.exception_handler(...) is not supported",
        ),
        (
            '''
            class Stamped:
                pass

            class Thing(Base, Stamped):
                __tablename__ = "things"
                id: Mapped[int] = mapped_column(primary_key=True)

            @app.get("/x")
            async def x():
                return {}
            ''',
            "base class Stamped is not supported",
        ),
        (
            '''
            @app.get("/x")
            async def x():
                return requests.get("http://example.com").json()
            ''',
            "unsupported call",
        ),
        (
            '''
            @app.get("/x")
            async def x(session: AsyncSession = Depends(lambda: None)):
                items = await session.execute(select(Item).where(Item.label.startswith("a")))
                return []
            ''',
            "unsupported SQL filter",
        ),
        (
            '''
            @app.get("/x")
            async def x():
                for i in range(3):
                    pass
            ''',
            "only the partial-update idiom",
        ),
        (
            '''
            @app.get("/x")
            def x():
                return {}
            ''',
            "only `async def` handlers",
        ),
        (
            '''
            @app.get("/x/{item_id}")
            async def x(item_id: int, session: AsyncSession = Depends(lambda: None)):
                item = await session.get(Item, item_id)
                return item
            ''',
            "needs a response_model",
        ),
    ],
)
def test_rejected_with_location(tmp_path, handler, message):
    with pytest.raises((TranspileError, TranspileErrors)) as exc:
        transpile(tmp_path, handler)
    text = str(exc.value)
    assert message in text
    assert "main.py:" in text  # points at the offending line


def test_supported_subset_generates(tmp_path):
    transpile(
        tmp_path,
        '''
        class ItemOut(BaseModel):
            id: int
            label: str

        @app.get("/items", response_model=list[ItemOut])
        async def items(q: str | None = None, session: AsyncSession = Depends(lambda: None)):
            query = select(Item).order_by(Item.label.desc())
            if q is not None:
                query = query.where(Item.label == q)
            result = await session.execute(query)
            return result.scalars().all()
        ''',
    )
    handlers = (tmp_path / "out" / "src" / "handlers.rs").read_text()
    assert 'order_by("label DESC")' in handlers
    assert 'rt::Cond::Cmp("label", "="' in handlers

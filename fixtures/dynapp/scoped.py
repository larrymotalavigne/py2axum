"""async_scoped_session with a ContextVar as its scope, set by a raw ASGI middleware per request and removed at its
end (teamhide/fastapi-boilerplate's SQLAlchemyMiddleware): the module's `session` proxy is the request's own session,
`session()` too; the next request gets a new one (an unflushed change does not leak through the identity map)."""
from contextvars import ContextVar, Token
from uuid import uuid4

from fastapi import APIRouter, HTTPException
from sqlalchemy import String, select
from sqlalchemy.ext.asyncio import AsyncSession, async_scoped_session, async_sessionmaker
from sqlalchemy.orm import Mapped, mapped_column

from .db import engine
from .models import Base

router = APIRouter(prefix="/scoped")
session_context: ContextVar[str] = ContextVar("session_context")


def get_session_context() -> str:
    return session_context.get()


def set_session_context(session_id: str) -> Token:
    return session_context.set(session_id)


def reset_session_context(context: Token) -> None:
    session_context.reset(context)


_factory = async_sessionmaker(engine, class_=AsyncSession, expire_on_commit=False)
session = async_scoped_session(session_factory=_factory, scopefunc=get_session_context)


class ScopedMiddleware:
    def __init__(self, app) -> None:
        self.app = app

    async def __call__(self, scope, receive, send) -> None:
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        context = set_session_context(session_id=str(uuid4()))
        try:
            await self.app(scope, receive, send)
        finally:
            await session.remove()
            reset_session_context(context=context)


class ScItem(Base):
    __tablename__ = "sc_items"
    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(20))


@router.post("/items/{name}")
async def add(name: str):
    item = ScItem(name=name)
    session.add(item)
    await session.commit()
    return {"id": item.id, "same": session() is session(), "type": type(session).__name__}


@router.get("/items")
async def items():
    rows = (await session.scalars(select(ScItem).order_by(ScItem.id))).all()
    return [[r.id, r.name] for r in rows]


@router.post("/dirty/{item_id}")
async def dirty(item_id: int):
    """changed, never flushed nor committed: gone with the request's session"""
    item = await session.get(ScItem, item_id)
    if item is None:
        raise HTTPException(status_code=404)
    item.name = "dirty"
    return {"name": item.name, "in": item in session()}


@router.get("/items/{item_id}")
async def get(item_id: int):
    item = await session.get(ScItem, item_id)
    if item is None:
        raise HTTPException(status_code=404)
    return {"name": item.name, "in": item in session()}

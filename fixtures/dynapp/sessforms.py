"""Other shapes of the session dependency met in real applications, same behaviour as db.get_db (commit after the
endpoint, rollback on an exception): the session assigned then closed in `finally` (`s = Maker()`), the maker read
from a cached factory into a local (`factory = get_factory()`), the maker returned by a project function
(`Maker = make_maker(engine)`), `raise exc` of the caught exception, the maker an instance keeps (set by its
`__init__`, or by a method the lifespan calls: life.lifespan). A write then an exception must leave nothing."""
from functools import lru_cache
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException
from sqlalchemy import func, select
from sqlalchemy.ext.asyncio import AsyncEngine, AsyncSession, async_sessionmaker

from .db import engine
from .models import Owner

router = APIRouter(prefix="/sessf")


def make_maker(bind: AsyncEngine) -> async_sessionmaker:
    return async_sessionmaker(bind, expire_on_commit=False, autoflush=False)


Maker = make_maker(engine)


@lru_cache(maxsize=1)
def get_factory() -> async_sessionmaker[AsyncSession]:
    return async_sessionmaker(bind=engine, expire_on_commit=False, autoflush=False, autocommit=False)


class SessionManager:
    def __init__(self, bind: AsyncEngine):
        self.engine = bind
        self.session_factory = async_sessionmaker(bind, class_=AsyncSession, expire_on_commit=False, autoflush=False)


class LateManager:
    def __init__(self):
        self.engine: AsyncEngine | None = None
        self.maker: async_sessionmaker | None = None

    async def initialize(self):
        self.engine = engine
        self.maker = async_sessionmaker(self.engine, expire_on_commit=False, autoflush=False)


manager = SessionManager(engine)
late_manager = LateManager()


async def manager_session():
    async with manager.session_factory() as session:
        try:
            yield session
            await session.commit()
        except Exception:
            await session.rollback()
            raise


async def late_session():
    async with late_manager.maker() as session:
        try:
            yield session
            await session.commit()
        except Exception:
            await session.rollback()
            raise
        finally:
            await session.close()


async def assigned_session():
    session: AsyncSession = Maker()
    try:
        yield session
        await session.commit()
    except Exception as exc:
        await session.rollback()
        raise exc
    finally:
        await session.close()


async def factory_session():
    factory = get_factory()
    async with factory() as session:
        try:
            yield session
            await session.commit()
        except Exception:
            await session.rollback()
            raise


Assigned = Annotated[AsyncSession, Depends(assigned_session)]
Factory = Annotated[AsyncSession, Depends(factory_session)]
Managed = Annotated[AsyncSession, Depends(manager_session)]
Late = Annotated[AsyncSession, Depends(late_session)]


async def count(db: AsyncSession, name: str) -> int:
    return await db.scalar(select(func.count()).select_from(Owner).where(Owner.name == name))


@router.post("/assigned/{name}")
async def assigned_add(name: str, db: Assigned, fail: int = 0):
    db.add(Owner(name=name))
    await db.flush()
    if fail == 1:
        raise HTTPException(409, "rolled back")
    if fail == 2:
        raise ValueError("rolled back too")
    return {"seen": await count(db, name)}


@router.post("/factory/{name}")
async def factory_add(name: str, db: Factory, fail: int = 0):
    db.add(Owner(name=name))
    await db.flush()
    if fail:
        raise HTTPException(409, "rolled back")
    return {"seen": await count(db, name)}


@router.get("/count/{name}")
async def owners(name: str, db: Factory):
    return {"name": name, "count": await count(db, name)}


@router.post("/manager/{name}")
async def manager_add(name: str, db: Managed, fail: int = 0):
    db.add(Owner(name=name))
    await db.flush()
    if fail:
        raise HTTPException(409, "rolled back")
    return {"seen": await count(db, name)}


@router.post("/late/{name}")
async def late_add(name: str, db: Late, fail: int = 0):
    db.add(Owner(name=name))
    await db.flush()
    if fail:
        raise HTTPException(409, "rolled back")
    return {"seen": await count(db, name)}

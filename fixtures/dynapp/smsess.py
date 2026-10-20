"""SQLModel's AsyncSession (sqlmodel.ext.asyncio.session) as the session type, its maker created inside the dependency
(testdriven.io's SQLModel tutorial): SQLAlchemy's AsyncSession plus `exec()`; same behaviour as db.get_db."""
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException
from sqlalchemy import func, select
from sqlalchemy.ext.asyncio import async_sessionmaker
from sqlmodel.ext.asyncio.session import AsyncSession

from .db import engine
from .models import Owner

router = APIRouter(prefix="/smsess")


async def get_session():
    maker = async_sessionmaker(engine, class_=AsyncSession, expire_on_commit=False, autoflush=False)
    async with maker() as session:
        try:
            yield session
            await session.commit()
        except Exception:
            await session.rollback()
            raise


SessionDep = Annotated[AsyncSession, Depends(get_session)]


@router.post("/owners/{name}")
async def add(name: str, session: SessionDep, fail: int = 0):
    session.add(Owner(name=name))
    await session.flush()
    if fail:
        raise HTTPException(409, "rolled back")
    return {"seen": await session.scalar(select(func.count()).select_from(Owner).where(Owner.name == name))}

from contextlib import asynccontextmanager

import aiohttp
from fastapi import Depends, FastAPI, HTTPException, Query
from fastapi.middleware.gzip import GZipMiddleware
from sqlalchemy import select
from sqlalchemy.ext.asyncio import AsyncSession

from .db import engine, get_session
from .models import Base, User
from .routers import upstream
from .schemas import UserCreate, UserOut, UserUpdate

@asynccontextmanager
async def lifespan(app: FastAPI):
    async with engine.begin() as conn:
        await conn.run_sync(Base.metadata.create_all)
    app.state.http = aiohttp.ClientSession()
    yield
    await app.state.http.close()
    await engine.dispose()


app = FastAPI(lifespan=lifespan)
app.add_middleware(GZipMiddleware, minimum_size=1000, compresslevel=6)
app.include_router(upstream.router)


@app.get("/health")
async def health():
    return {"status": "ok"}


@app.get("/users", response_model=list[UserOut])
async def list_users(
    skip: int = Query(default=0, ge=0),
    limit: int = Query(default=20, ge=1, le=100),
    active_only: bool = False,
    session: AsyncSession = Depends(get_session),
):
    query = select(User).order_by(User.id).offset(skip).limit(limit)
    if active_only:
        query = query.where(User.is_active == True)  # noqa: E712
    result = await session.execute(query)
    return result.scalars().all()


@app.get("/users/export", response_model=list[UserOut])
async def export_users(
    limit: int = Query(default=1_000_000, ge=1), session: AsyncSession = Depends(get_session)
):
    result = await session.execute(select(User).order_by(User.id).limit(limit))
    return result.scalars().all()


@app.get("/users/{user_id}", response_model=UserOut)
async def get_user(user_id: int, session: AsyncSession = Depends(get_session)):
    user = await session.get(User, user_id)
    if user is None:
        raise HTTPException(status_code=404, detail="User not found")
    return user


@app.post("/users", response_model=UserOut, status_code=201)
async def create_user(payload: UserCreate, session: AsyncSession = Depends(get_session)):
    existing = await session.scalar(select(User).where(User.email == payload.email))
    if existing is not None:
        raise HTTPException(status_code=409, detail="Email already registered")
    user = User(**payload.model_dump())
    session.add(user)
    await session.commit()
    await session.refresh(user)
    return user


@app.patch("/users/{user_id}", response_model=UserOut)
async def update_user(
    user_id: int, payload: UserUpdate, session: AsyncSession = Depends(get_session)
):
    user = await session.get(User, user_id)
    if user is None:
        raise HTTPException(status_code=404, detail="User not found")
    for field, value in payload.model_dump(exclude_unset=True).items():
        setattr(user, field, value)
    await session.commit()
    await session.refresh(user)
    return user


@app.delete("/users/{user_id}", status_code=204)
async def delete_user(user_id: int, session: AsyncSession = Depends(get_session)):
    user = await session.get(User, user_id)
    if user is None:
        raise HTTPException(status_code=404, detail="User not found")
    await session.delete(user)
    await session.commit()


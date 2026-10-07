import os

import aiohttp
from fastapi import Request
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine

DATABASE_URL = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/poc")

engine = create_async_engine(
    DATABASE_URL.replace("postgresql://", "postgresql+psycopg://", 1),
    pool_size=int(os.environ.get("DB_POOL_SIZE", "10")),
)
SessionLocal = async_sessionmaker(engine, expire_on_commit=False)


async def get_session():
    async with SessionLocal() as session:
        yield session


async def get_http(request: Request) -> aiohttp.ClientSession:
    return request.app.state.http

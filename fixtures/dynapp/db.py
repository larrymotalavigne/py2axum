import os
from typing import Annotated

from fastapi import Depends
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine

DATABASE_URL = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_dyn")

# connect_args: session parameters sent at connect (libpq options); psycopg decodes timestamptz in
# the session's TimeZone, whatever the server's (a fixed +05:30 zone, never the host's)
engine = create_async_engine(
    DATABASE_URL.replace("postgresql://", "postgresql+psycopg://", 1),
    connect_args={"options": r"-c TimeZone=Asia/Kolkata --application-name=dyn\ app -cstatement_timeout=60s"},
)
SessionLocal = async_sessionmaker(engine, expire_on_commit=False, autoflush=False)


async def get_db():
    """A common shape: the request commits after the endpoint, rolls back on an exception."""
    async with SessionLocal() as session:
        try:
            yield session
            await session.commit()
        except Exception:
            await session.rollback()
            raise
        finally:
            await session.close()


DbDep = Annotated[AsyncSession, Depends(get_db)]

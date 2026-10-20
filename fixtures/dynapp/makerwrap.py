"""A project class wrapping engine and sessionmaker, one instance per pool (one engine per event loop), called
for a session outside any dependency (a worker's `get_worker_session()`): its engine's connect_args options hold
in its sessions only."""
import asyncio
import os
from collections import defaultdict

from fastapi import APIRouter
from sqlalchemy import text
from sqlalchemy.ext.asyncio import async_sessionmaker, create_async_engine

from .db import DbDep

router = APIRouter(prefix="/mkw")
URL = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_dyn").replace(
    "postgresql://", "postgresql+psycopg://", 1)


class LoopSessionMaker:
    create_engine_impl = staticmethod(create_async_engine)
    session_maker_impl = staticmethod(async_sessionmaker)
    get_context_key = staticmethod(asyncio.get_running_loop)

    def __init__(self, **kwargs):
        self.maker_params = {
            "autocommit": kwargs.pop("autocommit", False),
            "autoflush": kwargs.pop("autoflush", False),
            "expire_on_commit": kwargs.pop("expire_on_commit", False),
        }
        self.engine_params = kwargs
        self.per_context = defaultdict(self.new_engine_and_maker)

    def __call__(self):
        return self.per_context[self.get_context_key()][1]()

    def new_engine_and_maker(self):
        engine = self.create_engine_impl(**self.engine_params)
        return engine, self.session_maker_impl(bind=engine, **self.maker_params)


WORKER = LoopSessionMaker(
    url=URL,
    pool_size=2,
    connect_args={"options": "-c statement_timeout=7s -c lock_timeout=3s"},
)


async def get_worker_session():
    return WORKER()


async def settings(db) -> list:
    r = await db.execute(text("SELECT current_setting('statement_timeout'), current_setting('lock_timeout')"))
    return list(r.one())


@router.get("/settings")
async def both(db: DbDep):
    worker = await get_worker_session()
    try:
        inside = await settings(worker)
        await worker.commit()
        again = await settings(worker)
    finally:
        await worker.close()
    return {"worker": inside, "worker_after_commit": again, "request": await settings(db)}

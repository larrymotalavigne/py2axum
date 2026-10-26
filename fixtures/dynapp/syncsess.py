"""A synchronous `Session` dependency next to the application's async one (db.get_db): each route gets the session
its own dependency makes (sync, expire_on_commit=True, no commit after the endpoint)."""
from fastapi import APIRouter, Depends, HTTPException
from sqlalchemy import create_engine, func, select
from sqlalchemy.orm import Session, sessionmaker

from .db import DATABASE_URL
from .models import Owner

router = APIRouter(prefix="/syncsess")
sync_engine = create_engine(DATABASE_URL.replace("postgresql://", "postgresql+psycopg://", 1))
SyncSession = sessionmaker(bind=sync_engine, autoflush=False)


def get_sync_db():
    db = SyncSession()
    try:
        yield db
    finally:
        db.close()


@router.post("/owners/{name}")
def add_owner(name: str, fail: int = 0, db: Session = Depends(get_sync_db)):
    o = Owner(name=name)
    db.add(o)
    db.flush()
    if fail:
        raise HTTPException(409, "not committed")
    db.commit()
    return {"id_set": o.id is not None, "name": o.name}


@router.get("/owners/{name}")
def count_owners(name: str, db: Session = Depends(get_sync_db)):
    return {"n": db.scalar(select(func.count()).select_from(Owner).where(Owner.name == name))}

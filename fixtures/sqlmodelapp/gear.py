"""A table whose SQLModel base is in another module (gearbase.py), without SQLModel imported here; `session.exec()`
of `text(...)` and `update(...)` (SQLModel's exec() of a non-select: the Result). Table `sm_gear`."""
import os
from typing import Annotated

from fastapi import APIRouter, Depends
from sqlmodel import Field, Session, create_engine, select, text, update

from .gearbase import GearBase, Kind

url = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_dyn").replace("postgresql://", "postgresql+psycopg://", 1)
engine = create_engine(url)
router = APIRouter(prefix="/gear")


class Gear(GearBase, table=True):
    __tablename__ = "sm_gear"
    id: int | None = Field(default=None, primary_key=True)


def get_session():
    with Session(engine) as session:
        yield session


SessionDep = Annotated[Session, Depends(get_session)]


@router.post("", response_model=Gear)
def add(gear: GearBase, session: SessionDep):
    row = Gear.model_validate(gear)
    session.add(row)
    session.commit()
    session.refresh(row)
    return {"id": row.id, "name": row.name, "kind": row.kind, "weight": row.weight}


@router.get("")
def all_gear(session: SessionDep):
    return [{"id": g.id, "name": g.name, "kind": g.kind, "weight": g.weight}
            for g in session.exec(select(Gear).order_by(Gear.id)).all()]


@router.post("/heavier")
def heavier(session: SessionDep):
    res = session.exec(update(Gear).where(Gear.kind == Kind.weapon).values(weight=Gear.weight + 1))
    session.commit()
    total = session.exec(text("SELECT count(*) FROM sm_gear")).scalar()
    return {"updated": res.rowcount, "total": total}

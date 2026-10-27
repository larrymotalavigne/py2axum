"""SQLModel as FastAPI's documentation uses it (sql_databases tutorials): models without table (Pydantic models),
a table model (a mapped class, validated and dumped as a Pydantic model), `Session(engine)` in a dependency,
`session.exec(select(...))`, `Model.model_validate(other)`, `sqlmodel_update()`, `SQLModel.metadata.create_all(engine)`
at startup. Its own table (`pilot`), emptied by the scenario."""
import os
from typing import Annotated

from fastapi import Depends, FastAPI, HTTPException, Query
from sqlmodel import Field, Session, SQLModel, create_engine, select


class PilotBase(SQLModel):
    name: str = Field(index=True)
    age: int | None = Field(default=None, index=True, ge=0)


class Pilot(PilotBase, table=True):
    id: int | None = Field(default=None, primary_key=True)
    secret_name: str
    rank: str = Field(default="cadet", max_length=20)


class PilotPublic(PilotBase):
    id: int


class PilotFull(PilotBase):
    """a table instance's fields, in a fixed order: dumped as itself, a table instance's keys follow its __dict__,
    whose order after a load depends on Python's string hashing (a documented difference)"""
    id: int | None
    secret_name: str | None = None
    rank: str


class PilotCreate(PilotBase):
    secret_name: str
    rank: str = "cadet"


class PilotUpdate(PilotBase):
    name: str | None = None
    age: int | None = None
    secret_name: str | None = None


url = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_dyn").replace("postgresql://", "postgresql+psycopg://", 1)
engine = create_engine(url)


def create_db_and_tables():
    SQLModel.metadata.create_all(engine)


def get_session():
    with Session(engine) as session:
        yield session


SessionDep = Annotated[Session, Depends(get_session)]
app = FastAPI()


@app.on_event("startup")
def on_startup():
    create_db_and_tables()


@app.get("/health")
def health():
    return {"ok": True}


@app.post("/pilots/", response_model=PilotPublic)
def create_pilot(pilot: PilotCreate, session: SessionDep):
    db_pilot = Pilot.model_validate(pilot)
    session.add(db_pilot)
    session.commit()
    session.refresh(db_pilot)
    return db_pilot


@app.post("/pilots/table", response_model=PilotFull)
def create_pilot_table(pilot: Pilot, session: SessionDep):
    """the table model as the body (tutorial001): not validated by SQLModel, the values as given"""
    session.add(pilot)
    session.commit()
    session.refresh(pilot)
    return pilot


@app.post("/pilots/direct")
def create_direct(body: dict, session: SessionDep):
    """a table model built by its constructor (no validation), returned without a response model"""
    p = Pilot(name=body["name"], secret_name=body.get("secret_name", "x"))
    session.add(p)
    session.commit()
    session.refresh(p)
    return {"id": p.id, "name": p.name, "age": p.age, "rank": p.rank}


@app.post("/pilots/validate")
def validate_dict(body: dict):
    p = Pilot.model_validate(body)
    return {"name": p.name, "age": p.age, "id": p.id, "rank": p.rank}


@app.get("/pilots/", response_model=list[PilotPublic])
def read_pilots(session: SessionDep, offset: int = 0, limit: Annotated[int, Query(le=100)] = 100):
    return session.exec(select(Pilot).offset(offset).limit(limit)).all()


@app.get("/pilots/full", response_model=list[PilotFull])
def read_full(session: SessionDep):
    return session.exec(select(Pilot).order_by(Pilot.id)).all()


@app.get("/pilots/raw")
def read_raw(session: SessionDep):
    """table instances read by attribute"""
    return [{"id": p.id, "name": p.name, "rank": p.rank} for p in session.exec(select(Pilot).order_by(Pilot.id.desc())).all()]


@app.get("/pilots/search")
def search(session: SessionDep, name: str = "", min_age: int = 0):
    statement = select(Pilot).where(Pilot.name == name)
    first = session.exec(statement).first()
    older = session.exec(select(Pilot).where(Pilot.age >= min_age).order_by(Pilot.age, Pilot.id)).all()
    pairs = session.exec(select(Pilot.name, Pilot.age).order_by(Pilot.id)).all()
    names = session.exec(select(Pilot.name).order_by(Pilot.name)).all()
    return {"first": first and first.secret_name, "older": [p.id for p in older], "pairs": [list(r) for r in pairs], "names": names}


@app.get("/pilots/{pilot_id}", response_model=PilotPublic)
def read_pilot(pilot_id: int, session: SessionDep):
    pilot = session.get(Pilot, pilot_id)
    if not pilot:
        raise HTTPException(status_code=404, detail="Pilot not found")
    return pilot


@app.patch("/pilots/{pilot_id}", response_model=PilotPublic)
def update_pilot(pilot_id: int, pilot: PilotUpdate, session: SessionDep):
    pilot_db = session.get(Pilot, pilot_id)
    if not pilot_db:
        raise HTTPException(status_code=404, detail="Pilot not found")
    pilot_data = pilot.model_dump(exclude_unset=True)
    pilot_db.sqlmodel_update(pilot_data)
    session.add(pilot_db)
    session.commit()
    session.refresh(pilot_db)
    return pilot_db


@app.put("/pilots/{pilot_id}", response_model=PilotFull)
def replace_pilot(pilot_id: int, pilot: PilotUpdate, session: SessionDep):
    """sqlmodel_update() with a model: each of its fields, `update=` winning"""
    pilot_db = session.get(Pilot, pilot_id)
    if not pilot_db:
        raise HTTPException(status_code=404, detail="Pilot not found")
    pilot_db.sqlmodel_update(pilot, update={"rank": "captain", "nope": 1})
    session.add(pilot_db)
    session.commit()
    session.refresh(pilot_db)
    return pilot_db


@app.delete("/pilots/{pilot_id}")
def delete_pilot(pilot_id: int, session: SessionDep):
    pilot = session.get(Pilot, pilot_id)
    if not pilot:
        raise HTTPException(status_code=404, detail="Pilot not found")
    session.delete(pilot)
    session.commit()
    return {"ok": True}


from . import songs  # noqa: E402  (its table is created by the startup handler above)

app.include_router(songs.router)

from . import heroes  # noqa: E402

app.include_router(heroes.router)

from . import gear  # noqa: E402

app.include_router(gear.router)

from . import tmpl  # noqa: E402

app.include_router(tmpl.router)

from . import authx  # noqa: E402

app.add_middleware(authx.AuthMiddleware, backend=authx.Backend(), on_error=authx.on_error)
app.include_router(authx.router)

"""SQLModel relationships as FastAPI's tutorial writes them (sql-databases, relationships): `Relationship(back_populates=)`
on both sides, `cascade_delete=True`, a response model nesting the related rows (read through the relationship while
the response is serialised). Tables `sm_team` and `sm_hero`, emptied by the scenario."""
import os
from datetime import datetime
from typing import Annotated, Optional

from fastapi import APIRouter, Depends, HTTPException
from sqlmodel import JSON, TEXT, Column, DateTime, Field, Relationship, Session, SQLModel, String, create_engine, func, select

url = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_dyn").replace("postgresql://", "postgresql+psycopg://", 1)
engine = create_engine(url)
router = APIRouter(prefix="/heroes")


class TeamBase(SQLModel):
    name: str = Field(index=True)
    headquarters: str


class Team(TeamBase, table=True):
    __tablename__ = "sm_team"
    id: int | None = Field(default=None, primary_key=True)
    heroes: list["Hero"] = Relationship(back_populates="team", cascade_delete=True)


class HeroBase(SQLModel):
    name: str = Field(index=True)
    secret_name: str
    age: int | None = Field(default=None, index=True)
    team_id: int | None = Field(default=None, foreign_key="sm_team.id", ondelete="CASCADE")


class Hero(HeroBase, table=True):
    __tablename__ = "sm_hero"
    id: int | None = Field(default=None, primary_key=True)
    team: Optional[Team] = Relationship(back_populates="heroes")


class TeamPublic(TeamBase):
    id: int


class HeroPublic(HeroBase):
    id: int


class HeroPublicWithTeam(HeroPublic):
    team: TeamPublic | None = None


class TeamPublicWithHeroes(TeamPublic):
    heroes: list[HeroPublic] = []


def get_session():
    with Session(engine) as session:
        yield session


SessionDep = Annotated[Session, Depends(get_session)]


@router.post("/teams", response_model=TeamPublic)
def create_team(team: TeamBase, session: SessionDep):
    db_team = Team.model_validate(team)
    session.add(db_team)
    session.commit()
    session.refresh(db_team)
    return db_team


@router.post("", response_model=HeroPublic)
def create_hero(hero: HeroBase, session: SessionDep):
    db_hero = Hero.model_validate(hero)
    session.add(db_hero)
    session.commit()
    session.refresh(db_hero)
    return db_hero


@router.get("/{hero_id}", response_model=HeroPublicWithTeam)
def read_hero(hero_id: int, session: SessionDep):
    hero = session.get(Hero, hero_id)
    if not hero:
        raise HTTPException(status_code=404, detail="Hero not found")
    return hero


@router.get("/teams/{team_id}", response_model=TeamPublicWithHeroes)
def read_team(team_id: int, session: SessionDep):
    team = session.get(Team, team_id)
    if not team:
        raise HTTPException(status_code=404, detail="Team not found")
    return team


@router.post("/teams/{team_id}/recruit")
def recruit(team_id: int, name: str, session: SessionDep):
    """the relationship written from the parent side: back_populates sets the child's team"""
    team = session.get(Team, team_id)
    if not team:
        raise HTTPException(status_code=404, detail="Team not found")
    hero = Hero(name=name, secret_name=name.lower())
    team.heroes.append(hero)
    session.commit()
    return {"team": hero.team.name, "team_id": hero.team_id, "heroes": sorted(h.name for h in team.heroes)}


@router.delete("/teams/{team_id}")
def delete_team(team_id: int, session: SessionDep):
    """cascade_delete=True: the team's heroes go with it"""
    team = session.get(Team, team_id)
    if not team:
        raise HTTPException(status_code=404, detail="Team not found")
    session.delete(team)
    session.commit()
    return {"left": session.exec(select(Hero.name).order_by(Hero.name)).all()}


@router.get("", response_model=list[HeroPublic])
def list_heroes(session: SessionDep, min_age: int | None = None, team_id: int | None = None):
    """the statement refined by optional filters before session.exec()"""
    stmt = select(Hero).order_by(Hero.id)
    if min_age is not None:
        stmt = stmt.where(Hero.age >= min_age)
    if team_id is not None:
        stmt = stmt.where(Hero.team_id == team_id)
    return session.exec(stmt).all()


class Mission(SQLModel, table=True):
    """columns given by `sa_column=Column(...)` (taken as they are by SQLModel), types imported from sqlmodel"""
    __tablename__ = "sm_mission"
    id: int | None = Field(default=None, primary_key=True)
    title: str = Field(sa_column=Column(String(30), nullable=False, unique=True))
    notes: str | None = Field(default=None, sa_column=Column(TEXT))
    meta: dict = Field(default_factory=dict, sa_column=Column(JSON))
    started_at: datetime | None = Field(default=None, sa_column=Column(DateTime(timezone=True), server_default=func.now()))


@router.post("/missions")
def create_mission(body: dict, session: SessionDep):
    m = Mission(title=body["title"], notes=body.get("notes"), meta=body.get("meta", {}))
    session.add(m)
    session.commit()
    session.refresh(m)
    return {"id": m.id, "title": m.title, "notes": m.notes, "meta": m.meta, "started": m.started_at is not None}


@router.get("/missions")
def list_missions(session: SessionDep):
    return [{"id": m.id, "title": m.title, "notes": m.notes, "meta": m.meta}
            for m in session.exec(select(Mission).order_by(Mission.id)).all()]

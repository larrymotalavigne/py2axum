from typing import Annotated

from fastapi import APIRouter, HTTPException, Query, status
from pydantic import BaseModel, ConfigDict, Field
from sqlalchemy import ForeignKey, String, func, select
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import Mapped, mapped_column, relationship, selectinload

from ..db import Base, SessionDep

router = APIRouter(prefix="/sql", tags=["sql"])


# --8<-- [start:models]
class Hero(Base):
    __tablename__ = "heroes"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(50), index=True)
    age: Mapped[int | None]
    team_id: Mapped[int | None] = mapped_column(ForeignKey("teams.id"))
    team: Mapped["Team | None"] = relationship(back_populates="heroes")


class Team(Base):
    __tablename__ = "teams"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(50), unique=True)
    headquarters: Mapped[str]
    heroes: Mapped[list[Hero]] = relationship(back_populates="team", order_by=Hero.id)
# --8<-- [end:models]


# --8<-- [start:schemas]
class TeamIn(BaseModel):
    name: str = Field(min_length=1, max_length=50)
    headquarters: str


class TeamOut(TeamIn):
    model_config = ConfigDict(from_attributes=True)
    id: int


class HeroIn(BaseModel):
    name: str = Field(min_length=1, max_length=50)
    age: int | None = Field(default=None, ge=0)
    team_id: int | None = None


class HeroUpdate(BaseModel):
    name: str | None = Field(default=None, min_length=1, max_length=50)
    age: int | None = Field(default=None, ge=0)
    team_id: int | None = None


class HeroOut(HeroIn):
    model_config = ConfigDict(from_attributes=True)
    id: int


class HeroWithTeam(HeroOut):
    team: TeamOut | None


class TeamWithHeroes(TeamOut):
    heroes: list[HeroOut]
# --8<-- [end:schemas]


# --8<-- [start:routes]
@router.post("/teams", response_model=TeamOut, status_code=status.HTTP_201_CREATED)
async def create_team(data: TeamIn, session: SessionDep):
    team = Team(**data.model_dump())
    session.add(team)
    try:
        await session.flush()
    except IntegrityError:
        raise HTTPException(status_code=409, detail="A team with this name already exists")
    return team


@router.post("/heroes", response_model=HeroOut, status_code=status.HTTP_201_CREATED)
async def create_hero(data: HeroIn, session: SessionDep):
    if data.team_id is not None and await session.get(Team, data.team_id) is None:
        raise HTTPException(status_code=422, detail="Unknown team")
    hero = Hero(**data.model_dump())
    session.add(hero)
    await session.flush()
    return hero


@router.get("/heroes", response_model=list[HeroOut])
async def list_heroes(
    session: SessionDep,
    offset: int = 0,
    limit: Annotated[int, Query(le=100)] = 100,
    name: str | None = None,
):
    stmt = select(Hero).order_by(Hero.id).offset(offset).limit(limit)
    if name:
        stmt = stmt.where(Hero.name.ilike(f"%{name}%"))
    return (await session.scalars(stmt)).all()


@router.get("/heroes/{hero_id}", response_model=HeroWithTeam)
async def read_hero(hero_id: int, session: SessionDep):
    hero = await session.get(Hero, hero_id, options=[selectinload(Hero.team)])
    if hero is None:
        raise HTTPException(status_code=404, detail="Hero not found")
    return hero


@router.patch("/heroes/{hero_id}", response_model=HeroOut)
async def update_hero(hero_id: int, data: HeroUpdate, session: SessionDep):
    hero = await session.get(Hero, hero_id)
    if hero is None:
        raise HTTPException(status_code=404, detail="Hero not found")
    for key, value in data.model_dump(exclude_unset=True).items():
        setattr(hero, key, value)
    await session.flush()
    return hero


@router.delete("/heroes/{hero_id}", status_code=status.HTTP_204_NO_CONTENT)
async def delete_hero(hero_id: int, session: SessionDep):
    hero = await session.get(Hero, hero_id)
    if hero is None:
        raise HTTPException(status_code=404, detail="Hero not found")
    await session.delete(hero)


@router.get("/teams/{team_id}", response_model=TeamWithHeroes)
async def read_team(team_id: int, session: SessionDep):
    team = await session.get(Team, team_id, options=[selectinload(Team.heroes)])
    if team is None:
        raise HTTPException(status_code=404, detail="Team not found")
    return team


@router.get("/stats")
async def team_stats(session: SessionDep):
    stmt = (
        select(Team.name, func.count(Hero.id).label("heroes"), func.avg(Hero.age).label("average_age"))
        .outerjoin(Hero, Hero.team_id == Team.id)
        .group_by(Team.name)
        .order_by(Team.name)
    )
    rows = (await session.execute(stmt)).all()
    return [{"team": row.name, "heroes": row.heroes, "average_age": row.average_age} for row in rows]
# --8<-- [end:routes]

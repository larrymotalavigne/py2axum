"""datetime and date bounds (`Field(gt=datetime(...))`, annotated_types) and AwareDatetime / NaiveDatetime: the
bound is built at translation, compared like speedate (two aware values by their instants, otherwise by their wall
times), checked in pydantic-core's order (le, lt, ge, gt, then the timezone), written in the error like speedate."""
from datetime import date, datetime, timedelta, timezone
from typing import Annotated

import annotated_types
from fastapi import APIRouter, Query
from pydantic import AwareDatetime, BaseModel, ConfigDict, Field, NaiveDatetime

router = APIRouter(prefix="/dtx")

START = datetime(2000, 1, 1)
EAST = timezone(timedelta(hours=-3, minutes=-30))


class Event(BaseModel):
    dt: Annotated[AwareDatetime, Field(gt=datetime(2000, 1, 1))]


class Window(BaseModel):
    at: datetime = Field(ge=START, lt=datetime(2030, 1, 1, tzinfo=timezone.utc))
    east: Annotated[datetime, annotated_types.Gt(datetime(2000, 1, 1, 5, 6, 7, 8, tzinfo=EAST))] | None = None


class Naive(BaseModel):
    dt: NaiveDatetime
    plain: Annotated[NaiveDatetime, Field(le=datetime(2020, 2, 29, 23, 59, 59, 999999))] | None = None


class Day(BaseModel):
    d: Annotated[date, Field(gt=date(2000, 1, 1), le=date(2000, 12, 31))]


class StrictAware(BaseModel):
    model_config = ConfigDict(strict=True)
    dt: Annotated[AwareDatetime, Field(lt=datetime(2010, 6, 15, 12, tzinfo=timezone.utc))]


@router.post("/event")
def event(item: Event) -> Event:
    return item


@router.post("/window")
def window(item: Window) -> Window:
    return item


@router.post("/naive")
def naive(item: Naive) -> Naive:
    return item


@router.post("/day")
def day(item: Day):
    return {"dump": item.model_dump(mode="json"), "json": item.model_dump_json()}


@router.post("/strict")
def strict(item: StrictAware) -> StrictAware:
    return item


@router.get("/since")
def since(after: Annotated[datetime, Query(gt=datetime(2001, 1, 1))], before: Annotated[date, Query(lt=date(2030, 1, 1))] | None = None):
    return {"after": after, "before": before}

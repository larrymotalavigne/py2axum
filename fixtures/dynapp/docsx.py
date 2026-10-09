"""Constructions of the FastAPI documentation's examples (corpus/, docs/coverage.md) lifted from the refusals:
Cookie() parameters, models of query/header/cookie/form parameters, File() as bytes, a plain class as
dependency, jsonable_encoder, time and timedelta read from JSON, `is` on Enum members."""
from datetime import datetime, time, timedelta
from decimal import Decimal
from enum import Enum
from typing import Annotated, Literal

from fastapi import APIRouter, Body, Cookie, Depends, File, Form, Header, Query
from fastapi.encoders import jsonable_encoder
from pydantic import BaseModel, Field

router = APIRouter(prefix="/docsx")


@router.get("/cookies")
async def cookies(ads_id: Annotated[str | None, Cookie()] = None, n: Annotated[int, Cookie()] = 1,
                  sess: str | None = Cookie(default=None, alias="session-id")):
    return {"ads_id": ads_id, "n": n, "sess": sess}


@router.get("/cookie-required")
async def cookie_required(token: Annotated[str, Cookie(min_length=3)], x_a: Annotated[int, Header()],
                          q: int = 0):
    # FastAPI's order of errors: path, query, header, then cookie
    return {"token": token, "x_a": x_a, "q": q}


# models of parameters (FastAPI docs: query/header/cookie param models, form models)
class FilterParams(BaseModel):
    model_config = {"extra": "forbid"}

    limit: int = Field(100, gt=0, le=100)
    offset: int = Field(0, ge=0)
    order_by: Literal["created_at", "updated_at"] = "created_at"
    tags: list[str] = []


class LooseParams(BaseModel):
    q: str | None = None
    page: int = 1


class CommonHeaders(BaseModel):
    host: str
    save_data: bool
    if_modified_since: str | None = None
    traceparent: str | None = None
    x_tag: list[str] = []


class StrictHeaders(CommonHeaders):
    model_config = {"extra": "forbid"}


class Cookies(BaseModel):
    session_id: str
    fatebook_tracker: str | None = None
    googall_tracker: str | None = None


class FormData(BaseModel):
    username: str
    password: str
    remember: bool = False
    tags: list[str] = []


@router.get("/pm/query")
async def pm_query(filter_query: Annotated[FilterParams, Query()]):
    return filter_query


@router.get("/pm/loose")
async def pm_loose(params: Annotated[LooseParams, Query()]):
    return params.model_dump()


@router.get("/pm/headers")
async def pm_headers(headers: Annotated[CommonHeaders, Header()]):
    return headers


@router.get("/pm/strict-headers")
async def pm_strict_headers(headers: Annotated[StrictHeaders, Header()]):
    return headers


@router.get("/pm/raw-headers")
async def pm_raw_headers(headers: Annotated[CommonHeaders, Header(convert_underscores=False)]):
    return headers


@router.get("/pm/cookies")
async def pm_cookies(cookies: Annotated[Cookies, Cookie()]):
    return cookies


@router.post("/pm/form")
async def pm_form(data: Annotated[FormData, Form()]):
    return data


# File() read as bytes (FastAPI docs: request files, forms and files)
@router.post("/files/one")
async def file_one(file: bytes = File(), token: str = Form()):
    return {"size": len(file), "head": file[:6].decode(), "token": token}


@router.post("/files/opt")
async def file_opt(file: Annotated[bytes | None, File(min_length=2)] = None):
    return {"size": None if file is None else len(file)}


@router.post("/files/many")
async def file_many(files: list[bytes] = File()):
    return {"sizes": [len(f) for f in files]}


class Blob(BaseModel):
    data: bytes


@router.post("/files/blob")
async def blob(b: Blob) -> Blob:
    return b


# a plain class as dependency (its __init__ signature), jsonable_encoder
class CommonQueryParams:
    def __init__(self, q: str | None = None, skip: int = 0, limit: Annotated[int, Query(le=50)] = 10):
        self.q = q
        self.skip = skip
        self.limit = limit


class NoInit:
    pass


@router.get("/classdep")
async def classdep(commons: CommonQueryParams = Depends(CommonQueryParams),
                   other: Annotated[CommonQueryParams, Depends()] = None, n: Annotated[NoInit, Depends()] = None):
    return {"q": commons.q, "skip": commons.skip, "limit": commons.limit, "same": other.limit,
            "noinit": type(n).__name__}


class Stamp(BaseModel):
    title: str
    when: datetime
    tags: set[str] = set()
    price: Decimal = Decimal("1.50")


@router.post("/encoder")
async def encoder(s: Stamp):
    out = jsonable_encoder(s)
    return {"type": type(out).__name__, "encoded": out, "plain": jsonable_encoder({"d": s.when, "b": b"xy", "n": None})}


# time and timedelta from JSON (FastAPI docs: extra data types)
class Schedule(BaseModel):
    every: timedelta
    at: time | None = None


@router.put("/when")
async def when(process_after: timedelta = Body(), repeat_at: time | None = Body(default=None)):
    return {"process_after": process_after, "repeat_at": repeat_at, "doubled": process_after * 2}


@router.post("/schedule")
async def schedule(s: Schedule) -> Schedule:
    return s


class ModelName(str, Enum):
    alexnet = "alexnet"
    resnet = "resnet"
    lenet = "lenet"


@router.get("/models/{model_name}")
async def get_model(model_name: ModelName):
    # an Enum member is a singleton: `is` compares identities
    if model_name is ModelName.alexnet:
        return {"model_name": model_name, "message": "Deep Learning FTW!"}
    if model_name is not ModelName.lenet:
        return {"model_name": model_name, "message": "Have some residuals"}
    return {"model_name": model_name, "message": "LeCNN all the images", "same": ModelName("lenet") is model_name}

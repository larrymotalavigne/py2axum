"""Annotations of the real-application bench (P2A0401): pydantic's strict and constrained aliases, `constr`/`conint`/
`conlist`..., and a generic model specialized in another module than the one that declares it."""
from pathlib import Path
from typing import List, Optional

from fastapi import APIRouter
from pydantic import (AmqpDsn, BaseModel, KafkaDsn, MongoDsn, MySQLDsn, PostgresDsn, Field, NegativeInt, NonNegativeFloat, NonNegativeInt, NonPositiveInt, PositiveFloat,
                      PositiveInt, StrictBool, StrictBytes, StrictFloat, StrictInt, StrictStr, condecimal, confloat,
                      conint, conlist, conset, constr)

from . import annx_resp
from .annx_resp import Page, Response
from .annx_schemas import Role

router = APIRouter(prefix="/annx")


class Strict(BaseModel):
    s: StrictStr
    i: StrictInt = 1
    f: StrictFloat = 1.0
    b: StrictBool | None = None
    y: StrictBytes = b""
    o: Optional[StrictStr] = None
    ls: List[StrictInt] = Field(default_factory=list)
    sm: StrictStr = Field("ab", min_length=2, max_length=4)


class Signs(BaseModel):
    p: PositiveInt = 1
    n: NegativeInt = -1
    nn: NonNegativeInt = 0
    np: NonPositiveInt = 0
    pf: PositiveFloat = 1.5
    nf: NonNegativeFloat = 0.0
    tight: PositiveInt = Field(5, ge=3, lt=10)


class Con(BaseModel):
    s: constr(min_length=2, max_length=5) = "ab"
    up: constr(strip_whitespace=True, to_upper=True, min_length=1) = "x"
    pat: constr(pattern=r"^[a-z]+$") = "abc"
    strict_s: constr(strict=True, min_length=1) = "s"
    maybe: Optional[constr(min_length=5, max_length=20)] = None
    i: conint(ge=1, le=10) = 1
    mult: conint(multiple_of=3, strict=True) = 3
    f: confloat(gt=0, lt=1) = 0.5
    d: condecimal(max_digits=5, decimal_places=2) = 1
    lst: conlist(int, min_length=1, max_length=3) = [1]
    tags: conlist(constr(min_length=2), min_length=0) = []
    st: conset(int, max_length=2) = set()


class Paths(BaseModel):
    p: Path
    o: Optional[Path] = None
    u: Path | int = 0
    s: Path = Field(Path("s"), strict=True)
    d: Path = Path("/var") / "log"
    ls: List[Path] = []


@router.post("/paths")
async def paths(m: Paths):
    return {"model": m, "kinds": [type(m.p).__name__, m.p.name, str(m.p.parent)], "dump": m.model_dump(),
            "json": m.model_dump_json()}


class Dsns(BaseModel):
    pg: PostgresDsn
    mongo: MongoDsn | None = None
    amqp: Optional[AmqpDsn] = None
    kafka: Optional[KafkaDsn] = None
    mysql: Optional[MySQLDsn] = None


@router.post("/dsns")
async def dsns(m: Dsns):
    out = {"model": m, "str": str(m.pg), "repr": repr(m.pg), "scheme": m.pg.scheme, "path": m.pg.path, "query": m.pg.query,
           "fragment": m.pg.fragment, "hosts": m.pg.hosts(), "unicode": m.pg.unicode_string()}
    for attr in ("host", "port", "username", "password"):
        try:
            out[attr] = getattr(m.pg, attr)
        except AttributeError as exc:
            out[attr] = str(exc)
    return out


@router.post("/strict")
async def strict(m: Strict):
    return m


@router.post("/signs")
async def signs(m: Signs):
    return m


@router.post("/con")
async def con(m: Con):
    return m


@router.post("/con/users")
async def con_users(users: conlist(int, min_length=1, max_length=2)):
    return {"users": users}


@router.get("/generic", response_model=Response[Role])
async def generic():
    return Response(data=Role(id=1, name="a"))


@router.get("/generic/list", response_model=Response[List[Role]])
async def generic_list(n: int = 2):
    return {"code": 0, "data": [{"id": i, "name": f"r{i}"} for i in range(n)]}


@router.get("/generic/bad", response_model=Response[Role])
async def generic_bad():
    return {"data": {"id": "x"}}


@router.get("/generic/page")
async def generic_page() -> Page[Role]:
    return Page(items=[Role(id=1, name="a")], total=1)


@router.get("/generic/module")
async def generic_module() -> annx_resp.Response[Role]:
    return annx_resp.Response(msg="m", data=Role(id=2, name="b"))


@router.post("/generic/in")
async def generic_in(body: Response[Role]):
    return body

"""FastAPI parameter markers kept in module variables and given as defaults or `Annotated` metadata
(`RadiusQuery = Query(50.0, ge=5)`, then `radius: float = RadiusQuery`), also from another module."""
from typing import Annotated

from fastapi import APIRouter, Depends, Header, Query

from .markers_shared import LimitQuery, TagsQuery

router = APIRouter(prefix="/markers")

RadiusQuery = Query(50.0, ge=5, le=500, description="cluster radius in meters")
CountQuery = Query(2, ge=1, le=1000)
RatioQuery: float = Query(0.5, ge=0, le=1)
SortQuery = Query(alias="sort-by", pattern="^(name|size)$")
RadiusMeta = Query(ge=5, le=500)
TraceHeader = Header(None, alias="x-trace")
PageSizeQuery = Query(20, ge=1, le=50)


def paging(page: int = Query(1, ge=1), size: int = PageSizeQuery):
    return {"page": page, "size": size}


PagingDep = Depends(paging)


@router.get("/search")
async def search(radius: float = RadiusQuery, count: int = CountQuery, ratio: float = RatioQuery):
    return {"radius": radius, "count": count, "ratio": ratio}


@router.post("/search/{key}")
async def search_post(key: str, radius: float = RadiusQuery, count: int = CountQuery):
    return {"key": key, "radius": radius, "count": count}


@router.get("/annotated")
async def annotated(radius: Annotated[float, RadiusMeta], sort: Annotated[str, SortQuery] = "name"):
    return {"radius": radius, "sort": sort}


@router.get("/shared")
async def shared(limit: int = LimitQuery, tags: list[str] = TagsQuery, trace: str | None = TraceHeader):
    return {"limit": limit, "tags": tags, "trace": trace}


@router.get("/paged")
async def paged(p: dict = PagingDep):
    return p

from typing import Annotated

from fastapi import APIRouter, Depends, Header, HTTPException


async def verify_key(x_key: Annotated[str, Header()]):
    if x_key != "fake-super-secret-key":
        raise HTTPException(status_code=400, detail="X-Key header invalid")


# every route of this router requires the X-Key header
router = APIRouter(prefix="/dependencies", tags=["dependencies"], dependencies=[Depends(verify_key)])


async def common_parameters(q: str | None = None, skip: int = 0, limit: int = 100):
    return {"q": q, "skip": skip, "limit": limit}


CommonsDep = Annotated[dict, Depends(common_parameters)]


@router.get("/items")
async def read_items(commons: CommonsDep):
    return commons


class Pagination:
    def __init__(self, skip: int = 0, limit: int = 3):
        self.skip = skip
        self.limit = limit


FRUITS = ["apple", "banana", "cherry", "date", "elderberry", "fig", "grape"]


@router.get("/fruits")
async def read_fruits(page: Annotated[Pagination, Depends(Pagination)]):
    return FRUITS[page.skip : page.skip + page.limit]


CLOSED: list[str] = []


async def get_connection():
    connection = {"name": f"connection-{len(CLOSED) + 1}"}
    try:
        yield connection
    finally:
        CLOSED.append(connection["name"])  # runs once the request is over


@router.get("/connection")
async def use_connection(connection: Annotated[dict, Depends(get_connection)]):
    return {"using": connection["name"], "closed_so_far": CLOSED}

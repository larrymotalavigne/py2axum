from datetime import date
from enum import Enum
from typing import Annotated
from uuid import UUID

from fastapi import APIRouter, Header, Path, Query

router = APIRouter(prefix="/parameters", tags=["parameters"])


class Shelf(str, Enum):
    to_read = "to-read"
    reading = "reading"
    done = "done"


@router.get("/items/{item_id}")
async def read_item(item_id: int, q: str | None = None, short: bool = False):
    item = {"item_id": item_id}
    if q:
        item["q"] = q
    if not short:
        item["description"] = "A long description of the item."
    return item


@router.get("/shelves/{shelf}")
async def read_shelf(shelf: Shelf):
    if shelf is Shelf.done:
        return {"shelf": shelf, "message": "Finished books"}
    return {"shelf": shelf, "message": f"Books on the {shelf.value} shelf"}


@router.get("/search")
async def search(
    q: Annotated[str, Query(min_length=2, max_length=50, pattern="^[a-z ]+$")],
    tags: Annotated[list[str], Query(alias="tag")] = [],
    limit: Annotated[int, Query(ge=1, le=100)] = 10,
    since: date | None = None,
):
    return {"q": q, "tags": tags, "limit": limit, "since": since}


@router.get("/files/{file_id}")
async def read_file(
    file_id: Annotated[UUID, Path(title="The file's id")],
    user_agent: Annotated[str | None, Header()] = None,
    x_token: Annotated[list[str], Header()] = [],
):
    return {"file_id": file_id, "user_agent": user_agent, "x_token": x_token}

from typing import Annotated

from fastapi import APIRouter, Body
from pydantic import BaseModel

router = APIRouter(prefix="/body", tags=["body"])


class Item(BaseModel):
    name: str
    description: str | None = None
    price: float
    tax: float | None = None


class User(BaseModel):
    username: str
    full_name: str | None = None


@router.post("/items")
async def create_item(item: Item):
    data = item.model_dump()
    if item.tax is not None:
        data["price_with_tax"] = item.price + item.tax
    return data


@router.put("/items/{item_id}")
async def update_item(
    item_id: int,
    item: Item,
    user: User,
    importance: Annotated[int, Body(gt=0)],
    q: str | None = None,
):
    result = {"item_id": item_id, "item": item, "user": user, "importance": importance}
    if q:
        result["q"] = q
    return result


@router.put("/embedded/{item_id}")
async def update_embedded(item_id: int, item: Annotated[Item, Body(embed=True)]):
    return {"item_id": item_id, "item": item}

"""Dataclasses as FastAPI request and response types (stdlib and pydantic.dataclasses): validated like pydantic
dataclasses, into instances of the class (its repr, its methods), `dataclass_type` for anything else."""
from dataclasses import dataclass, field

from fastapi import APIRouter
import pydantic.dataclasses as pdc

router = APIRouter(prefix="/dcx")


@dataclass
class Item:
    name: str
    price: float
    tags: list[str] = field(default_factory=list)
    description: str | None = None
    tax: float | None = None

    def total(self) -> float:
        return self.price + (self.tax or 0)


@pdc.dataclass
class PItem:
    name: str
    description: str | None = None


@pdc.dataclass
class Author:
    name: str
    items: list[PItem] = field(default_factory=list)


@router.post("/items/")
async def create_item(item: Item):
    return {"type": type(item).__name__, "repr": repr(item), "total": item.total(), "item": item}


@router.post("/echo/", response_model=Item)
async def echo(item: Item):
    return item


@router.get("/next", response_model=Item)
async def read_next_item(bad: int = 0):
    if bad == 2:
        return Item("built", 2.0)
    return {"name": "Island In The Moon", "price": "12.99" if not bad else "nope",
            "description": "A place to be playin' and havin' fun", "tags": ["breater"]}


@router.post("/authors/{author_id}/items/", response_model=Author)
async def create_author_items(author_id: str, items: list[PItem]):
    return {"name": author_id, "items": items}


@router.get("/authors/", response_model=list[Author])
def get_authors():
    return [{"name": "Breaters", "items": [{"name": "Island In The Moon", "description": "fun"}, {"name": "Holy Buddies"}]},
            {"name": "System of an Up", "items": []}]

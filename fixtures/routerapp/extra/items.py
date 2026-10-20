from fastapi import APIRouter, HTTPException

from fixtures.routerapp.extra.nested import router as nested

router = APIRouter(prefix="/items", tags=["items"])
router.include_router(nested, prefix="/nested")

ITEMS = {1: "one", 2: "two"}


@router.get("/{item_id}")
async def get_item(item_id: int):
    if item_id not in ITEMS:
        raise HTTPException(404, "no such item")
    return {"id": item_id, "name": ITEMS[item_id]}


@router.get("")
async def list_items(limit: int = 10):
    return [{"id": k, "name": v} for k, v in list(ITEMS.items())[:limit]]

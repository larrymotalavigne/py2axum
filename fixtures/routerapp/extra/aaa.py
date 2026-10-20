from fastapi import APIRouter

# overlaps /items/{item_id} of items.py: included after it, so Starlette never reaches it for /items/<int>
router = APIRouter(prefix="/items")


@router.get("/{name}")
async def by_name(name: str):
    return {"aaa": name}

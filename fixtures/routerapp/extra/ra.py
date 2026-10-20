from fastapi import APIRouter

router = APIRouter(prefix="/a")


@router.get("/{x}")
async def a_any(x: str):
    return {"ra": x}

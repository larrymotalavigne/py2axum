from fastapi import APIRouter

router = APIRouter()


@router.get("/a/fixed")
async def a_fixed():
    return {"rb": "fixed"}  # after ra's /a/{x} in ROUTERS: never reached


@router.get("/b")
async def b():
    return {"rb": "b"}

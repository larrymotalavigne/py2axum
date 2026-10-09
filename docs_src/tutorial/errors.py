from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import JSONResponse

router = APIRouter(prefix="/errors", tags=["errors"])

ITEMS = {"foo": "The Foo Wrestlers"}


@router.get("/items/{item_id}")
async def read_item(item_id: str):
    if item_id not in ITEMS:
        raise HTTPException(status_code=404, detail="Item not found", headers={"X-Error": "missing item"})
    return {"item": ITEMS[item_id]}


class UnicornException(Exception):
    def __init__(self, name: str):
        self.name = name


# registered on the application: app.add_exception_handler(UnicornException, unicorn_handler)
async def unicorn_handler(request: Request, exc: UnicornException):
    return JSONResponse(status_code=418, content={"message": f"Oops! {exc.name} did something."})


@router.get("/unicorns/{name}")
async def read_unicorn(name: str):
    if name == "yolo":
        raise UnicornException(name)
    return {"unicorn": name}


@router.get("/divide")
async def divide(a: int, b: int):
    return {"result": a / b}  # b=0: an unhandled exception, FastAPI's plain 500

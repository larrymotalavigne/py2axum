from fastapi import APIRouter, Response, status
from fastapi.responses import HTMLResponse, JSONResponse, PlainTextResponse, RedirectResponse, StreamingResponse
from pydantic import BaseModel

router = APIRouter(prefix="/responses", tags=["responses"])


class UserIn(BaseModel):
    username: str
    password: str
    email: str | None = None


class UserOut(BaseModel):
    username: str
    email: str | None = None


@router.post("/users", response_model=UserOut, status_code=status.HTTP_201_CREATED)
async def create_user(user: UserIn):
    return user  # the response model leaves the password out


class Item(BaseModel):
    name: str
    description: str | None = None
    price: float
    tags: list[str] = []


ITEMS = {
    "foo": {"name": "Foo", "price": 50.2},
    "bar": {"name": "Bar", "description": "The bartenders", "price": 62, "tags": []},
}


@router.get("/items/{item_id}", response_model=Item, response_model_exclude_unset=True)
async def read_item(item_id: str):
    return ITEMS[item_id]


@router.get("/hello", response_class=PlainTextResponse)
async def hello():
    return "Hello, world"


@router.get("/page", response_class=HTMLResponse)
async def page():
    return "<h1>Compiled by py2axum</h1>"


@router.get("/old-hello")
async def old_hello():
    return RedirectResponse("/responses/hello")


@router.get("/cookie")
async def set_cookie(response: Response):
    response.set_cookie("session", "abc123", httponly=True, max_age=3600)
    response.headers["X-Cache"] = "miss"
    return {"ok": True}


@router.get("/teapot")
async def teapot():
    return JSONResponse({"detail": "I'm a teapot"}, status_code=418)


async def countdown(n: int):
    for i in range(n, 0, -1):
        yield f"data: {i}\n\n"
    yield "data: liftoff\n\n"


@router.get("/countdown")
async def events(n: int = 3):
    return StreamingResponse(countdown(n), media_type="text/event-stream")

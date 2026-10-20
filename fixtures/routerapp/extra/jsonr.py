from fastapi import APIRouter
from fastapi.responses import HTMLResponse
from pydantic import BaseModel
from starlette.responses import JSONResponse


class Num(BaseModel):
    x: float
    label: str = "é"


# dispatch form: the router's routes use the class explicitly (no dump_json fast path of FastAPI >= 0.130)
router = APIRouter(default_response_class=JSONResponse)
html = APIRouter(default_response_class=HTMLResponse)
plain = APIRouter()


@router.get("/num", response_model=Num)
async def num():
    return {"x": 1e16}


@router.get("/raw")
async def raw():
    return {"x": 1e16, "y": [1.5, 2e-7]}


@plain.get("/num", response_model=Num)
async def plain_num():
    return {"x": 1e16}


@plain.get("/explicit", response_model=Num, response_class=JSONResponse)
async def explicit_num():
    return {"x": 1e16}


@plain.get("/text")
async def text():
    return "<i>x</i>"


@html.get("/page")
async def page():
    return "<b>hi</b>"


html.include_router(plain, prefix="/inner")  # included into an HTMLResponse router: its routes become HTML

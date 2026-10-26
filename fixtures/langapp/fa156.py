"""Request handling that changed in FastAPI 0.144 to 0.156 (dynrt `fastapi_at_least`): missing booleans of a form
(checkboxes), empty form values of a form model, defaults applied at validation, a JSON `null` body, non-finite
floats in JSON Lines, empty file inputs, text items of a `list[bytes]` file field."""
from collections.abc import AsyncIterable
from typing import Annotated

from fastapi import APIRouter, Body, File, Form, Query, Request, Response, UploadFile
from pydantic import AfterValidator, BaseModel

router = APIRouter(prefix="/fa156")


@router.post("/checkbox")
async def checkbox(remember: bool = Form(True), agree: bool | None = Form(None), name: str = Form("x")):
    return {"remember": remember, "agree": agree, "name": name}


class Prefs(BaseModel):
    dark: bool = True
    label: str = "none"
    tags: list[str] = []
    size: int = 3


@router.post("/prefs")
async def prefs(p: Annotated[Prefs, Form()]):
    return p


def doubled(v: int) -> int:
    return v * 2


@router.get("/dflt")
async def dflt(n: Annotated[int, AfterValidator(doubled)] = 2, m: Annotated[int, Query(ge=0)] = 1):
    return {"n": n, "m": m}


class Item(BaseModel):
    name: str


@router.post("/nullbody")
async def nullbody(item: Item = Body(Item(name="default"))):
    return item


@router.post("/nullopt")
async def nullopt(item: Item | None = None):
    return {"item": item}


@router.post("/nullembed")
async def nullembed(item: Item | None = Body(None, embed=True), k: int | None = Body(None)):
    return {"item": item, "k": k}


@router.get("/nan")
async def nan(n: int = 2) -> AsyncIterable[dict]:
    yield {"v": float("nan"), "w": [1.5, float("inf")]}
    for i in range(n):
        yield {"i": i, "neg": -float("inf")}


@router.post("/upload")
async def upload(file: UploadFile | None = File(None), files: list[UploadFile] = File([]), note: str = Form("")):
    return {"file": file.filename if file else None, "files": [f.filename for f in files], "note": note}


@router.post("/blobs")
async def blobs(data: list[bytes] = File([])):
    return {"sizes": [len(d) for d in data], "first": data[0].decode() if data else None}


@router.head("/probe")
async def probe_head(response: Response):
    response.headers["x-probe"] = "head"
    return {"ok": True}


@router.get("/probe")
async def probe_get():
    return {"ok": "get"}


@router.options("/probe")
async def probe_options():
    return Response(status_code=204, headers={"allow": "GET, HEAD, OPTIONS"})


@router.api_route("/multi/{x}", methods=["GET", "POST"])
async def multi(x: int, request: Request):
    return {"x": x, "method": request.method}


@router.api_route("/onlyget")
async def onlyget():
    return {"default": "GET"}

"""mimetypes: guess_type (paths, URLs, data: URLs, suffix and encoding maps, case, strict=False), guess_file_type
(3.13+), guess_extension / guess_all_extensions (order of types_map_inv, the host's mime.types files included),
the ValueErrors of urlsplit's netloc checks, and FileResponse's guess."""
import mimetypes
import sys
from pathlib import Path

from fastapi import APIRouter
from fastapi.responses import FileResponse

router = APIRouter(prefix="/mimex")


@router.post("/guess")
async def guess(body: dict):
    out = []
    for name in body["names"]:
        try:
            if body.get("as") == "path":
                r = mimetypes.guess_type(Path(name), strict=body.get("strict", True))
            elif body.get("as") == "bytes":
                r = mimetypes.guess_type(name.encode())
            elif "strict" in body:
                r = mimetypes.guess_type(name, body["strict"])
            else:
                r = mimetypes.guess_type(name)
            out.append(list(r))
        except ValueError as e:
            out.append({"err": type(e).__name__, "msg": str(e)})
    return out


@router.post("/file")
async def guess_file(body: dict):
    if sys.version_info >= (3, 13):
        return [list(mimetypes.guess_file_type(n, strict=body.get("strict", True))) for n in body["names"]]
    else:
        return {"skip": True}


@router.post("/ext")
async def ext(body: dict):
    out = []
    for t in body["types"]:
        if body.get("all"):
            out.append(mimetypes.guess_all_extensions(t, strict=body.get("strict", True)))
        elif "strict" in body:
            out.append(mimetypes.guess_extension(t, body["strict"]))
        else:
            out.append(mimetypes.guess_extension(t))
    return out


@router.get("/upload")
async def upload(filename: str | None = None, content_type: str | None = None):
    """the pattern of a real app: the client's type, else a guess, else a default"""
    mime = content_type or mimetypes.guess_type(filename or "")[0] or "application/octet-stream"
    return {"mime": mime, "ext": mimetypes.guess_extension(mime) or ""}


@router.get("/bad")
async def bad(what: str):
    try:
        if what == "int":
            return mimetypes.guess_type(5)
        if what == "none_ext":
            return mimetypes.guess_extension(None)
        if what == "int_all":
            return mimetypes.guess_all_extensions(3)
    except (TypeError, AttributeError) as e:
        return {"err": type(e).__name__, "msg": str(e)}


@router.get("/file_response")
async def file_response(name: str):
    try:
        return FileResponse(Path(__file__), filename=name)
    except ValueError as e:
        return {"err": type(e).__name__, "msg": str(e)}

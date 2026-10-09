from typing import Annotated

from fastapi import APIRouter, File, Form, UploadFile

router = APIRouter(prefix="/files", tags=["files"])


@router.post("/login")
async def login(username: Annotated[str, Form()], password: Annotated[str, Form(min_length=8)]):
    return {"username": username}


@router.post("/upload")
async def upload(file: UploadFile, note: Annotated[str | None, Form()] = None):
    content = await file.read()
    return {
        "filename": file.filename,
        "content_type": file.content_type,
        "size": len(content),
        "lines": content.decode().count("\n"),
        "note": note,
    }


@router.post("/uploads")
async def upload_many(files: list[UploadFile]):
    return [{"filename": f.filename, "size": f.size} for f in files]


@router.post("/raw")
async def raw_bytes(data: Annotated[bytes, File()]):
    return {"size": len(data)}

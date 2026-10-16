"""The routes of the library: py2axum never reads them (fixtures/integapp/main.py includes `router` last)."""
from fastapi import APIRouter, HTTPException, Request
from pydantic import BaseModel

router = APIRouter(prefix="/ext", tags=["ext"])
NOTES: dict[int, str] = {}


class Note(BaseModel):
    text: str
    level: int = 1


@router.get("/notes")
async def list_notes():
    return {"notes": [{"id": k, "text": v} for k, v in sorted(NOTES.items())]}


@router.post("/notes", status_code=201)
async def add_note(note: Note):
    nid = len(NOTES) + 1
    NOTES[nid] = note.text
    return {"id": nid, "text": note.text, "level": note.level}


@router.get("/notes/{nid}")
async def get_note(nid: int):
    if nid not in NOTES:
        raise HTTPException(status_code=404, detail="no such note")
    return {"id": nid, "text": NOTES[nid]}


@router.delete("/notes")
async def clear_notes():
    NOTES.clear()
    return {"cleared": True}


@router.get("/echo/{rest:path}")
async def echo(rest: str, request: Request):
    return {"rest": rest, "query": dict(request.query_params), "agent": request.headers.get("x-probe")}


# a second router without a prefix: POST /health, a path the application serves with GET only. Starlette tries the
# application's GET /health first (a partial match), then this full match
extra = APIRouter()


@extra.post("/health")
async def post_health():
    return {"from": "library"}


def make_router(label: str) -> APIRouter:
    """A router the library builds on demand (`app.include_router(make_router(...), prefix=...)`)."""
    r = APIRouter()

    @r.get("/whoami")
    async def whoami():
        return {"label": label}

    return r

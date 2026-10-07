"""A small notes API: the example translated by py2axum (see ../README.md)."""
from fastapi import FastAPI, HTTPException, Query
from sqlalchemy import func, select
from sqlalchemy.exc import IntegrityError

from .db import DbDep
from .models import Author, Note
from .schemas import AuthorIn, AuthorOut, NoteIn, NoteOut, NotePatch

app = FastAPI(title="notes")


@app.get("/health")
async def health() -> dict:
    return {"status": "ok"}


@app.post("/authors", response_model=AuthorOut, status_code=201)
async def create_author(payload: AuthorIn, db: DbDep):
    author = Author(name=payload.name)
    db.add(author)
    try:
        await db.flush()
    except IntegrityError:
        raise HTTPException(status_code=409, detail=f"author {payload.name!r} already exists")
    return author


@app.post("/notes", response_model=NoteOut, status_code=201)
async def create_note(payload: NoteIn, db: DbDep):
    if await db.get(Author, payload.author_id) is None:
        raise HTTPException(status_code=404, detail="author not found")
    note = Note(**payload.model_dump())
    db.add(note)
    await db.flush()
    await db.refresh(note)
    return note


@app.get("/notes", response_model=list[NoteOut])
async def list_notes(
    db: DbDep,
    q: str | None = Query(default=None, description="text searched in titles"),
    pinned: bool | None = None,
    limit: int = Query(default=20, ge=1, le=100),
    offset: int = Query(default=0, ge=0),
):
    stmt = select(Note).order_by(Note.pinned.desc(), Note.id)
    if q:
        stmt = stmt.where(Note.title.ilike(f"%{q}%"))
    if pinned is not None:
        stmt = stmt.where(Note.pinned == pinned)
    return (await db.execute(stmt.limit(limit).offset(offset))).scalars().all()


@app.get("/notes/{note_id}", response_model=NoteOut)
async def get_note(note_id: int, db: DbDep):
    note = await db.get(Note, note_id)
    if note is None:
        raise HTTPException(status_code=404, detail="note not found")
    return note


@app.patch("/notes/{note_id}", response_model=NoteOut)
async def update_note(note_id: int, payload: NotePatch, db: DbDep):
    note = await db.get(Note, note_id)
    if note is None:
        raise HTTPException(status_code=404, detail="note not found")
    for field, value in payload.model_dump(exclude_unset=True).items():
        setattr(note, field, value)
    await db.flush()
    return note


@app.delete("/notes/{note_id}", status_code=204)
async def delete_note(note_id: int, db: DbDep):
    note = await db.get(Note, note_id)
    if note is None:
        raise HTTPException(status_code=404, detail="note not found")
    await db.delete(note)


@app.get("/stats")
async def stats(db: DbDep) -> dict:
    rows = (await db.execute(
        select(Author.name, func.count(Note.id).label("notes")).join(Note, isouter=True).group_by(Author.name).order_by(Author.name)
    )).all()
    return {"authors": [{"name": r.name, "notes": r.notes} for r in rows]}

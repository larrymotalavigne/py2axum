"""AsyncSession.stream() / stream_scalars() (an AsyncResult read by `async for`, its methods awaited) and joinedload()
with innerjoin=True on a many-to-one whose foreign key is NOT NULL: the patterns of rhoboro/async-fastapi-sqlalchemy
(classmethods on the model yielding rows, a hybrid property reading the joined object)."""
from __future__ import annotations

from typing import AsyncIterator

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, ConfigDict
from sqlalchemy import ForeignKey, String, select
from sqlalchemy.ext.asyncio import AsyncSession
from sqlalchemy.ext.hybrid import hybrid_property
from sqlalchemy.orm import Mapped, joinedload, mapped_column, relationship, selectinload

from .db import DbDep
from .models import Base

router = APIRouter(prefix="/streamx")


class SxBook(Base):
    __tablename__ = "sx_books"
    id: Mapped[int] = mapped_column("id", autoincrement=True, nullable=False, unique=True, primary_key=True)
    title: Mapped[str] = mapped_column("title", String(length=64), nullable=False)
    notes: Mapped[list[SxNote]] = relationship("SxNote", back_populates="book", order_by="SxNote.id")

    @classmethod
    async def read_all(cls, session: AsyncSession, include_notes: bool) -> AsyncIterator[SxBook]:
        stmt = select(cls)
        if include_notes:
            stmt = stmt.options(selectinload(cls.notes))
        stream = await session.stream_scalars(stmt.order_by(cls.id))
        async for row in stream:
            yield row


class SxNote(Base):
    __tablename__ = "sx_notes"
    id: Mapped[int] = mapped_column("id", autoincrement=True, nullable=False, unique=True, primary_key=True)
    title: Mapped[str] = mapped_column("title", String(length=64), nullable=False)
    content: Mapped[str] = mapped_column("content", nullable=False, default="")
    book_id: Mapped[int] = mapped_column("book_id", ForeignKey("sx_books.id"), nullable=False)
    book: Mapped[SxBook] = relationship("SxBook", back_populates="notes")

    @hybrid_property
    def book_title(self) -> str:
        return self.book.title

    @classmethod
    async def read_all(cls, session: AsyncSession) -> AsyncIterator[SxNote]:
        stmt = select(cls).options(joinedload(cls.book, innerjoin=True))
        stream = await session.stream_scalars(stmt.order_by(cls.id))
        async for row in stream:
            yield row

    @classmethod
    async def read_by_id(cls, session: AsyncSession, note_id: int) -> SxNote | None:
        stmt = select(cls).where(cls.id == note_id).options(joinedload(cls.book))
        return await session.scalar(stmt.order_by(cls.id))

    @classmethod
    async def read_by_ids(cls, session: AsyncSession, note_ids: list[int]) -> AsyncIterator[SxNote]:
        stmt = select(cls).where(cls.id.in_(note_ids)).options(joinedload(cls.book))
        stream = await session.stream_scalars(stmt.order_by(cls.id))
        async for row in stream:
            yield row


class NoteOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)
    id: int
    title: str
    content: str
    book_id: int
    book_title: str


class BookOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)
    id: int
    title: str
    notes: list[NoteOut]


@router.post("/seed")
async def seed(db: DbDep):
    db.add_all([SxBook(title="first", notes=[SxNote(title="a", content="x"), SxNote(title="b")]),
                SxBook(title="second", notes=[SxNote(title="c", content="z")]), SxBook(title="empty")])
    await db.commit()
    return {"ok": True}


@router.get("/notes", response_model=list[NoteOut])
async def notes(db: DbDep):
    return [NoteOut.model_validate(n) async for n in SxNote.read_all(db)]


@router.get("/notes/{note_id}", response_model=NoteOut)
async def note(note_id: int, db: DbDep):
    n = await SxNote.read_by_id(db, note_id)
    if n is None:
        raise HTTPException(status_code=404)
    return NoteOut.model_validate(n)


@router.get("/byids", response_model=list[NoteOut])
async def by_ids(db: DbDep, ids: str = "1,3,9"):
    return [NoteOut.model_validate(n) async for n in SxNote.read_by_ids(db, [int(x) for x in ids.split(",")])]


@router.get("/books")
async def books(db: DbDep):
    out = []
    async for b in SxBook.read_all(db, True):
        out.append(BookOut.model_validate(b).model_dump())
    return out


@router.get("/rows")
async def rows(db: DbDep):
    """stream(): rows, then each method of the AsyncResult awaited"""
    stmt = select(SxNote.id, SxNote.title).order_by(SxNote.id)
    res = await db.stream(stmt)
    tuples = [[r.id, r.title, list(r)] async for r in res]
    everything = await (await db.stream(stmt)).all()
    first = await (await db.stream(stmt)).first()
    scalars = await (await db.stream(stmt)).scalars().all()
    maps = await (await db.stream(stmt)).mappings().all()
    one = await (await db.stream(select(SxNote.title).where(SxNote.id == 2))).scalar_one()
    none = await (await db.stream(select(SxNote.title).where(SxNote.id == 99))).one_or_none()
    return {"tuples": tuples, "all": [list(r) for r in everything], "first": list(first), "scalars": scalars,
            "maps": [dict(m) for m in maps], "one": one, "none": none}


@router.get("/scalars")
async def scalars(db: DbDep):
    """stream_scalars(): its methods awaited, a joined chain with innerjoin=True"""
    stmt = select(SxNote).order_by(SxNote.id)
    first = await (await db.stream_scalars(stmt)).first()
    every = await (await db.stream_scalars(stmt)).all()
    uniq = await (await db.stream_scalars(select(SxNote.book_id).order_by(SxNote.book_id))).unique().all()
    chained = select(SxBook).where(SxBook.id < 3).order_by(SxBook.id).options(
        selectinload(SxBook.notes).joinedload(SxNote.book, innerjoin=True))
    nested = [[b.title, [[n.title, n.book.title] for n in b.notes]] async for b in await db.stream_scalars(chained)]
    unnested = (await db.scalars(select(SxNote).order_by(SxNote.id).options(joinedload(SxNote.book, innerjoin="unnested")))).all()
    plain = (await db.scalars(select(SxNote).order_by(SxNote.id).options(joinedload(SxNote.book, innerjoin=False)))).all()
    return {"first": first.title, "all": [n.title for n in every], "unique": uniq, "nested": nested,
            "unnested": [n.book_title for n in unnested], "plain": [n.book.title for n in plain],
            "type": type(await db.stream_scalars(stmt)).__name__, "rtype": type(await db.stream(stmt)).__name__}

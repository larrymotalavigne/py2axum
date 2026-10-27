"""fastapi-beyond-CRUD's shapes: a many-to-many through a link model (`Relationship(link_model=...)`, back_populates on
both sides, lazy="selectin") whose collections change in memory (append, extend, remove, assignment: the other side
follows), table classes with methods, `sqlmodel.desc`, and a `register(app)` function adding CORSMiddleware and a
TrustedHostMiddleware whose hosts come from the settings."""
import uuid
from typing import List

import sqlalchemy.dialects.postgresql as pg
from fastapi import APIRouter, FastAPI, HTTPException
from fastapi.middleware.cors import CORSMiddleware
from fastapi.middleware.trustedhost import TrustedHostMiddleware
from pydantic_settings import BaseSettings
from sqlmodel import Column, Field, Relationship, Session, SQLModel, desc, select

from .main import SessionDep

router = APIRouter(prefix="/bookly")


class Settings(BaseSettings):
    bookly_hosts: list[str] = ["127.0.0.1", "localhost", "testserver"]
    bookly_origins: list[str] = ["https://bookly.example/"]


settings = Settings()


def register(app: FastAPI):
    app.add_middleware(CORSMiddleware, allow_origins=[o.rstrip("/") for o in settings.bookly_origins],
                       allow_methods=["*"], allow_headers=["*"], allow_credentials=True)
    app.add_middleware(TrustedHostMiddleware, allowed_hosts=settings.bookly_hosts)


class BkBookTag(SQLModel, table=True):
    __tablename__ = "bk_booktag"
    book_id: uuid.UUID = Field(default=None, foreign_key="bk_book.uid", primary_key=True)
    tag_id: uuid.UUID = Field(default=None, foreign_key="bk_tag.uid", primary_key=True)


class BkTag(SQLModel, table=True):
    __tablename__ = "bk_tag"
    uid: uuid.UUID = Field(sa_column=Column(pg.UUID, nullable=False, primary_key=True, default=uuid.uuid4))
    name: str = Field(sa_column=Column(pg.VARCHAR, nullable=False))
    books: List["BkBook"] = Relationship(link_model=BkBookTag, back_populates="tags",
                                         sa_relationship_kwargs={"lazy": "selectin"})

    def __repr__(self) -> str:
        return f"<Tag {self.name}>"


class BkBook(SQLModel, table=True):
    __tablename__ = "bk_book"
    uid: uuid.UUID = Field(sa_column=Column(pg.UUID, nullable=False, primary_key=True, default=uuid.uuid4))
    title: str
    tags: List[BkTag] = Relationship(link_model=BkBookTag, back_populates="books",
                                     sa_relationship_kwargs={"lazy": "selectin"})

    def __repr__(self):
        return f"<Book {self.title}>"

    def label(self) -> str:
        return self.title.upper()




def names(xs) -> list[str]:
    return sorted(x.title if isinstance(x, BkBook) else x.name for x in xs)


@router.post("/seed")
def seed(session: SessionDep):
    py, rs, web = BkTag(name="python"), BkTag(name="rust"), BkTag(name="web")
    one, two = BkBook(title="one"), BkBook(title="two")
    one.tags.append(py)
    one.tags.extend([rs, web])
    two.tags = [py]
    seen = {"python": names(py.books), "rust": names(rs.books), "web": names(web.books)}
    session.add_all([one, two])
    session.commit()
    return {"in_memory": seen, "reprs": [repr(one), repr(py)], "label": one.label()}


def book(session: Session, title: str) -> BkBook:
    b = session.exec(select(BkBook).where(BkBook.title == title)).first()
    if b is None:
        raise HTTPException(status_code=404)
    return b


def tag(session: Session, name: str) -> BkTag:
    t = session.exec(select(BkTag).where(BkTag.name == name)).first()
    if t is None:
        raise HTTPException(status_code=404)
    return t


@router.post("/untag/{title}/{name}")
def untag(title: str, name: str, session: SessionDep):
    b, t = book(session, title), tag(session, name)
    before = names(t.books)
    b.tags.remove(t)
    after = names(t.books)
    session.commit()
    return {"before": before, "after": after}


@router.post("/tag/{title}/{name}")
def add_tag(title: str, name: str, session: SessionDep):
    b, t = book(session, title), tag(session, name)
    b.tags.append(t)
    mirrored = names(t.books)
    session.commit()
    return {"mirrored": mirrored}


@router.get("/books")
def books(session: SessionDep):
    rows = session.exec(select(BkBook).order_by(desc(BkBook.title))).all()
    return [[b.title, names(b.tags)] for b in rows]


@router.get("/tags")
def tags(session: SessionDep):
    rows = session.exec(select(BkTag).order_by(BkTag.name)).all()
    return [[t.name, names(t.books)] for t in rows]

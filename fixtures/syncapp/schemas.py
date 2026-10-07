from datetime import datetime

from pydantic import BaseModel, ConfigDict


class AuthorCreate(BaseModel):
    name: str


class BookCreate(BaseModel):
    title: str
    pages: int | None = None


class BookRead(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    title: str
    pages: int | None
    author_id: int


class AuthorRead(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    name: str
    active: bool
    created_at: datetime


class AuthorWithBooks(AuthorRead):
    books: list[BookRead]


class ReviewRead(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    stars: int


class BookFull(BookRead):
    author: AuthorRead
    reviews: list[ReviewRead]

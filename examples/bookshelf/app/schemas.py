from datetime import datetime
from enum import Enum

from pydantic import BaseModel, ConfigDict, EmailStr, Field, field_validator, model_validator


class Status(str, Enum):
    to_read = "to_read"
    reading = "reading"
    done = "done"


class UserCreate(BaseModel):
    email: EmailStr
    password: str = Field(min_length=8, max_length=72)
    display_name: str = Field(min_length=1, max_length=50)


class LoginIn(BaseModel):
    email: EmailStr
    password: str


class Token(BaseModel):
    access_token: str
    token_type: str = "bearer"
    expires_in: int


class UserOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    email: str
    display_name: str
    created_at: datetime


class Author(BaseModel):
    """A user as shown next to a book or a review: no e-mail address."""

    model_config = ConfigDict(from_attributes=True)

    id: int
    display_name: str


def _clean_tags(tags: list[str]) -> list[str]:
    out: list[str] = []
    for tag in tags:
        tag = tag.strip().lower()
        if tag and tag not in out:
            out.append(tag)
    return out


class BookIn(BaseModel):
    title: str = Field(min_length=1, max_length=200)
    author: str = Field(min_length=1, max_length=120)
    year: int | None = Field(default=None, ge=0, le=2100)
    status: Status = Status.to_read
    tags: list[str] = Field(default_factory=list, max_length=10)

    @field_validator("title", "author")
    @classmethod
    def strip(cls, value: str) -> str:
        value = value.strip()
        if not value:
            raise ValueError("must not be blank")
        return value

    @field_validator("tags")
    @classmethod
    def normalize_tags(cls, tags: list[str]) -> list[str]:
        return _clean_tags(tags)


class BookPatch(BaseModel):
    title: str | None = Field(default=None, min_length=1, max_length=200)
    author: str | None = Field(default=None, min_length=1, max_length=120)
    year: int | None = Field(default=None, ge=0, le=2100)
    status: Status | None = None
    tags: list[str] | None = Field(default=None, max_length=10)

    @field_validator("tags")
    @classmethod
    def normalize_tags(cls, tags: list[str] | None) -> list[str] | None:
        return None if tags is None else _clean_tags(tags)

    @model_validator(mode="after")
    def not_empty(self) -> "BookPatch":
        if not self.model_fields_set:
            raise ValueError("nothing to update")
        return self


class ReviewIn(BaseModel):
    rating: int = Field(ge=1, le=5)
    body: str = Field(default="", max_length=2000)


class ReviewOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    rating: int
    body: str
    user: Author
    created_at: datetime


class BookOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    title: str
    author: str
    year: int | None
    status: Status
    tags: list[str]
    owner: Author
    created_at: datetime


class BookDetail(BookOut):
    reviews: list[ReviewOut]
    average_rating: float | None = None


class Stats(BaseModel):
    books: int
    by_status: dict[str, int]
    reviews: int
    average_rating: float | None

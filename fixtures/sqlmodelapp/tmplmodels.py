"""Tables shaped like the official full-stack template's (app/models.py): a table deriving from a SQLModel base,
uuid primary keys, `datetime` columns (SQLModel's UTCDateTime), relationships with cascade_delete. Tables
`sm_owner`, `sm_note`."""
import uuid
from datetime import UTC, datetime

from pydantic import EmailStr
from sqlmodel import Field, Relationship, SQLModel


def get_datetime_utc() -> datetime:
    return datetime.now(UTC)


class OwnerBase(SQLModel):
    email: EmailStr = Field(unique=True, index=True, max_length=255)
    full_name: str | None = Field(default=None, max_length=255)


class OwnerCreate(OwnerBase):
    password: str = Field(min_length=8, max_length=128)


class OwnerUpdate(SQLModel):
    full_name: str | None = Field(default=None, max_length=255)
    password: str | None = Field(default=None, min_length=8, max_length=128)


class Owner(OwnerBase, table=True):
    __tablename__ = "sm_owner"
    id: uuid.UUID = Field(default_factory=uuid.uuid4, primary_key=True)
    hashed_password: str
    created_at: datetime | None = Field(default_factory=get_datetime_utc)
    notes: list["Note"] = Relationship(back_populates="owner", cascade_delete=True)


class NoteBase(SQLModel):
    title: str = Field(min_length=1, max_length=255)


class Note(NoteBase, table=True):
    __tablename__ = "sm_note"
    id: uuid.UUID = Field(default_factory=uuid.uuid4, primary_key=True)
    created_at: datetime | None = Field(default_factory=get_datetime_utc)
    owner_id: uuid.UUID = Field(foreign_key="sm_owner.id", nullable=False, ondelete="CASCADE")
    owner: Owner | None = Relationship(back_populates="notes")

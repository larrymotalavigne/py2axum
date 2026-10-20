"""A table model and its schemas in their own module, used by the routes of songs.py (testdriven.io's SQLModel
tutorial, FastAPI's full-stack template): the table class is a type there too. Its own table (`song`)."""
from typing import Optional

from sqlmodel import Field, SQLModel


class SongBase(SQLModel):
    name: str
    artist: str
    year: Optional[int] = None


class Song(SongBase, table=True):
    id: int = Field(default=None, nullable=False, primary_key=True)


class SongCreate(SongBase):
    pass


class SongRead(SongBase):
    """a loaded table instance dumped in a fixed order (its own keys follow its __dict__: a documented difference)"""
    id: int

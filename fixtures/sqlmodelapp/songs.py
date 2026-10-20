"""Routes of a table model defined in another module (song.py): `response_model=list[Song]` (new instances), a `Song`
body (not validated, as SQLModel's table models), `Song.model_validate(...)`, `select(Song)` through the session."""
import os
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException
from sqlmodel import Session, create_engine, select

from .song import Song, SongCreate, SongRead

url = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_dyn").replace("postgresql://", "postgresql+psycopg://", 1)
engine = create_engine(url)
router = APIRouter(prefix="/songs")


def get_session():
    with Session(engine) as session:
        yield session


SessionDep = Annotated[Session, Depends(get_session)]


@router.get("", response_model=list[Song])
def get_songs(session: SessionDep):
    songs = session.exec(select(Song).order_by(Song.id)).all()
    return [Song(name=song.name, artist=song.artist, year=song.year, id=song.id) for song in songs]


@router.post("")
def add_song(song: SongCreate, session: SessionDep):
    song = Song(name=song.name, artist=song.artist, year=song.year)
    session.add(song)
    session.commit()
    session.refresh(song)
    return {"id": song.id, "name": song.name, "year": song.year}


@router.post("/table", response_model=SongRead)
def add_table(song: Song, session: SessionDep):
    session.add(song)
    session.commit()
    session.refresh(song)
    return song


@router.post("/validated")
def add_validated(body: dict, session: SessionDep) -> SongRead:
    song = Song.model_validate(body)
    session.add(song)
    session.commit()
    session.refresh(song)
    return song


@router.get("/{song_id}", response_model=SongRead)
def get_song(song_id: int, session: SessionDep):
    song = session.get(Song, song_id)
    if song is None:
        raise HTTPException(404, "Song not found")
    return song

"""Routes shaped like the official full-stack template's (crud.py, api/routes): `Table.model_validate(x, update=...)`
and `obj.sqlmodel_update(data, update=...)` on tables of another module, pwdlib with Argon2 and bcrypt hashers
(`verify_and_update` rehashing with the first), `col()`, `session.exec(statement)` of a `delete(...)`, a naive
datetime refused by UTCDateTime. Random values (uuids, hashes, times) are not returned."""
import os
from datetime import datetime
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException
from pwdlib import PasswordHash
from pwdlib.exceptions import UnknownHashError
from pwdlib.hashers.argon2 import Argon2Hasher
from pwdlib.hashers.bcrypt import BcryptHasher
from pydantic import BaseModel
from sqlalchemy.exc import StatementError
from sqlmodel import Session, col, create_engine, delete, func, select

from .tmplmodels import Note, NoteBase, Owner, OwnerCreate, OwnerUpdate

url = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_dyn").replace("postgresql://", "postgresql+psycopg://", 1)
engine = create_engine(url)
router = APIRouter(prefix="/tmpl")
password_hash = PasswordHash((Argon2Hasher(), BcryptHasher(rounds=4)))
# bcrypt.hashpw(b"secret", b"$2b$04$abcdefghijklmnopqrstuu")
OLD_BCRYPT = "$2b$04$abcdefghijklmnopqrstuu2r9OfJnfCsdneAXAGHnS4UpFFP8WIrW"


def get_session():
    with Session(engine) as session:
        yield session


SessionDep = Annotated[Session, Depends(get_session)]


def create_owner(session: Session, owner_create: OwnerCreate) -> Owner:
    db_obj = Owner.model_validate(owner_create, update={"hashed_password": password_hash.hash(owner_create.password)})
    session.add(db_obj)
    session.commit()
    session.refresh(db_obj)
    return db_obj


def update_owner(session: Session, db_owner: Owner, owner_in: OwnerUpdate) -> Owner:
    owner_data = owner_in.model_dump(exclude_unset=True)
    extra_data = {}
    if "password" in owner_data:
        extra_data["hashed_password"] = password_hash.hash(owner_data["password"])
    db_owner.sqlmodel_update(owner_data, update=extra_data)
    session.add(db_owner)
    session.commit()
    session.refresh(db_owner)
    return db_owner


def by_email(session: Session, email: str) -> Owner:
    owner = session.exec(select(Owner).where(Owner.email == email)).first()
    if owner is None:
        raise HTTPException(status_code=404, detail="Owner not found")
    return owner


def shown(owner: Owner) -> dict:
    return {"email": owner.email, "full_name": owner.full_name, "tz": str(owner.created_at.tzinfo),
            "offset": owner.created_at.utcoffset().total_seconds(), "hash": owner.hashed_password[:10]}


@router.post("/owners")
def add_owner(owner_in: OwnerCreate, session: SessionDep):
    return shown(create_owner(session, owner_in))


@router.patch("/owners/{email}")
def patch_owner(email: str, owner_in: OwnerUpdate, session: SessionDep):
    owner = update_owner(session, by_email(session, email), owner_in)
    return shown(owner)


class Login(BaseModel):
    email: str
    password: str


@router.post("/login")
def login(body: Login, session: SessionDep):
    owner = by_email(session, body.email)
    ok, updated = password_hash.verify_and_update(body.password, owner.hashed_password)
    return {"ok": ok, "rehash": updated is not None, "plain": password_hash.verify(body.password, owner.hashed_password)}


@router.get("/legacy")
def legacy(password: str = "secret"):
    ok, updated = password_hash.verify_and_update(password, OLD_BCRYPT)
    try:
        password_hash.verify(password, "plain-text")
        unknown = None
    except UnknownHashError as e:
        unknown = type(e).__name__
    return {"ok": ok, "rehashed_with": updated[:10] if updated else None, "unknown": unknown}


@router.post("/owners/{email}/notes")
def add_note(email: str, note_in: NoteBase, session: SessionDep):
    owner = by_email(session, email)
    note = Note.model_validate(note_in, update={"owner_id": owner.id})
    session.add(note)
    session.commit()
    count = session.exec(select(func.count()).select_from(Note).where(Note.owner_id == owner.id)).one()
    return {"title": note.title, "count": count, "tz": str(note.created_at.tzinfo)}


@router.delete("/owners/{email}/notes")
def clear_notes(email: str, session: SessionDep):
    owner = by_email(session, email)
    statement = delete(Note).where(col(Note.owner_id) == owner.id)
    session.exec(statement)
    session.commit()
    count = session.exec(select(func.count()).select_from(Note).where(Note.owner_id == owner.id)).one()
    return {"count": count}


@router.post("/owners/{email}/naive")
def naive_note(email: str, session: SessionDep):
    owner = by_email(session, email)
    session.add(Note(title="naive", owner_id=owner.id, created_at=datetime(2024, 1, 1, 12, 0)))
    try:
        session.commit()
        return {"stored": True}
    except StatementError as e:
        session.rollback()
        return {"error": type(e).__name__, "orig": type(e.orig).__name__, "msg": str(e.orig)}

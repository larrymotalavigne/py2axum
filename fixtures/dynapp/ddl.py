"""`await conn.run_sync(Base.metadata.create_all)` on its own declarative base: each route drops part of the schema,
runs create_all (checkfirst) in `engine.begin()` and returns what PostgreSQL's catalog then holds."""
import datetime as dt
import decimal
import enum
import uuid
from typing import Any, Literal, Optional

from fastapi import APIRouter
from sqlalchemy import (JSON, BigInteger, CheckConstraint, Column, Computed, DateTime, Enum, ForeignKey, Identity, Index,
                        Integer, LargeBinary, MetaData, Numeric, SmallInteger, String, Table, Text, Time,
                        UniqueConstraint, func, text)
from sqlalchemy.dialects.postgresql import ARRAY, JSONB
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, relationship
from sqlalchemy.types import TypeDecorator

from .db import engine

router = APIRouter(prefix="/ddl")


class Mood(enum.Enum):
    happy = "h"
    sad = "s"


class Tier(str, enum.Enum):
    FREE = "free"
    PRO = "pro"


class Upper(TypeDecorator):
    """A TypeDecorator: its DDL is its impl's."""
    impl = String(30)
    cache_ok = True

    def process_bind_param(self, value, dialect):
        return value.upper() if value else value


class DdlBase(DeclarativeBase):
    metadata = MetaData(naming_convention={
        "ix": "ix_%(column_0_label)s", "uq": "uq_%(table_name)s_%(column_0_name)s",
        "ck": "ck_%(table_name)s_%(constraint_name)s", "fk": "fk_%(table_name)s_%(column_0_name)s_%(referred_table_name)s",
        "pk": "pk_%(table_name)s"})
    type_annotation_map = {dict[str, Any]: JSONB, dt.datetime: DateTime(timezone=True)}


class Stamped:
    """A mixin: its columns come first in SQLAlchemy's order rules."""
    created_at: Mapped[dt.datetime] = mapped_column(server_default=func.now())
    updated_at: Mapped[Optional[dt.datetime]] = mapped_column(onupdate=func.now())


def _enum(cls, name):
    return Enum(cls, name=name, values_callable=lambda x: [e.value for e in x])


ddl_tags = Table(
    "ddl_tags", DdlBase.metadata,
    Column("item_id", ForeignKey("ddl_items.id", ondelete="CASCADE"), primary_key=True),
    Column("tag", String(20), primary_key=True),
)


class DdlOwner(Stamped, DdlBase):
    __tablename__ = "ddl_owners"
    id: Mapped[int] = mapped_column(primary_key=True)
    email: Mapped[str] = mapped_column(String(120), unique=True, index=True, comment="l'adresse")
    tier: Mapped[Tier] = mapped_column(default=Tier.FREE, server_default="FREE")
    mood: Mapped[Optional[Mood]]
    # a foreign key cycle with ddl_items: added by ALTER TABLE once both tables exist
    best_item_id: Mapped[Optional[int]] = mapped_column(ForeignKey("ddl_items.id", use_alter=True, ondelete="SET NULL"))
    items: Mapped[list["DdlItem"]] = relationship(back_populates="owner", foreign_keys="DdlItem.owner_id")
    MAX_ITEMS = 10


class DdlItem(DdlBase):
    __tablename__ = "ddl_items"
    __table_args__ = (
        UniqueConstraint("owner_id", "slug"),
        CheckConstraint("qty >= 0", name="qty_pos"),
        Index("ix_items_lower_slug", func.lower(text("slug"))),
        {"comment": "les objets"},
    )
    id: Mapped[int] = mapped_column(BigInteger, Identity(always=True, start=100), primary_key=True)
    owner_id: Mapped[int] = mapped_column(ForeignKey("ddl_owners.id", ondelete="CASCADE"), index=True)
    slug: Mapped[str] = mapped_column(String(40))
    title: Mapped[str | None] = mapped_column(Text)
    qty: Mapped[int] = mapped_column(SmallInteger, default=0, server_default=text("0"))
    price: Mapped[decimal.Decimal] = mapped_column(Numeric(10, 2))
    ratio: Mapped[float]
    ok: Mapped[bool] = mapped_column(server_default=text("true"))
    day: Mapped[Optional[dt.date]]
    at: Mapped[Optional[dt.time]] = mapped_column(Time)
    uid: Mapped[uuid.UUID] = mapped_column(default=uuid.uuid4, unique=True)
    data: Mapped[dict[str, Any]] = mapped_column(default=dict)
    raw: Mapped[Optional[dict]] = mapped_column(JSON)
    blob: Mapped[Optional[bytes]] = mapped_column(LargeBinary)
    labels: Mapped[list[str]] = mapped_column(ARRAY(String(10)), server_default="{}")
    level: Mapped[str] = mapped_column(_enum(Mood, "item_level"))
    kind: Mapped[Literal["a", "b"]]
    code: Mapped[str] = mapped_column(Upper)
    total: Mapped[int] = mapped_column(Computed("qty * 2"))
    owner: Mapped[DdlOwner] = relationship(back_populates="items", foreign_keys=[owner_id])


Index("ix_items_owner_day", DdlItem.owner_id, DdlItem.day.desc())


class DdlNote(DdlBase):
    """Legacy Column() style; a Python default on the key: INTEGER, not SERIAL."""
    __tablename__ = "ddl_notes"
    id = Column(Integer, primary_key=True, default=7)
    body = Column(String(50), nullable=False, server_default="")
    item_id = Column(BigInteger, ForeignKey("ddl_items.id", ondelete="SET NULL"))


TABLES = "ddl_tags, ddl_notes, ddl_items, ddl_owners"
TYPES = "tier, mood, item_level, kind"
CATALOG = {
    "columns": "SELECT table_name, column_name, CAST(ordinal_position AS text), data_type, udt_name, "
               "CAST(character_maximum_length AS text), CAST(numeric_precision AS text), CAST(numeric_scale AS text), "
               "is_nullable, column_default, is_identity, identity_generation, identity_start, is_generated, "
               "generation_expression FROM information_schema.columns WHERE table_name LIKE 'ddl_%' "
               "ORDER BY table_name, ordinal_position",
    "constraints": "SELECT CAST(conrelid AS regclass) || '', conname, pg_get_constraintdef(oid) FROM pg_constraint "
                   "WHERE CAST(conrelid AS regclass) || '' LIKE 'ddl_%' ORDER BY 1, 2",
    "indexes": "SELECT tablename, indexname, indexdef FROM pg_indexes WHERE tablename LIKE 'ddl_%' ORDER BY 1, 2",
    "comments": "SELECT c.relname, COALESCE(a.attname, ''), d.description FROM pg_description d "
                "JOIN pg_class c ON c.oid = d.objoid LEFT JOIN pg_attribute a ON a.attrelid = c.oid "
                "AND a.attnum = d.objsubid WHERE c.relname LIKE 'ddl_%' ORDER BY 1, 2",
    "enums": "SELECT t.typname, string_agg(e.enumlabel, ',' ORDER BY e.enumsortorder) FROM pg_type t "
             "JOIN pg_enum e ON e.enumtypid = t.oid WHERE t.typname IN ('tier', 'mood', 'item_level', 'kind') "
             "GROUP BY t.typname ORDER BY 1",
}


async def create_all() -> None:
    async with engine.begin() as conn:
        await conn.run_sync(DdlBase.metadata.create_all)


async def rebuild(setup: list[str]) -> dict:
    async with engine.begin() as conn:
        for q in setup:
            await conn.execute(text(q))
    error = None
    try:
        await create_all()
    except Exception as e:
        error = type(e).__name__
    out = {"error": error}
    async with engine.connect() as conn:
        for key, q in CATALOG.items():
            out[key] = [list(r) for r in (await conn.execute(text(q))).all()]
    return out


@router.post("/fresh")
async def fresh():
    return await rebuild([f"DROP TABLE IF EXISTS {TABLES} CASCADE", f"DROP TYPE IF EXISTS {TYPES}"])


@router.post("/again")
async def again():
    """Everything exists: nothing is created."""
    return await rebuild([])


@router.post("/partial")
async def partial():
    """One table and two enum types missing; the tables referencing the owners lose their foreign keys."""
    return await rebuild(["DROP TABLE ddl_owners CASCADE", "DROP TYPE tier", "DROP TYPE mood"])


@router.post("/clash")
async def clash():
    """The index name SQLAlchemy creates is taken: the whole block rolls back."""
    return await rebuild(["DROP TABLE ddl_items CASCADE", "CREATE INDEX ix_items_lower_slug ON ddl_notes (body)"])

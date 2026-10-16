"""Mappings of fixtures/ormapp: the hybrid attributes of SQLAlchemy's documentation (`sqlalchemy.ext.hybrid`) with a
custom Comparator, hybrid value objects (a plain class and dataclasses deriving from Comparator), a composite value
object (`Point`), and two classes referencing each other through `relationship(primaryjoin=..., post_update=True)`.
Tables of their own (tests/scenarios/ormapp.py creates them)."""
import os
from dataclasses import dataclass
from typing import Any

from sqlalchemy import ColumnElement, ForeignKey, Integer, SQLColumnExpression, String, func, tuple_
from sqlalchemy.ext.asyncio import async_sessionmaker, create_async_engine
from sqlalchemy.ext.hybrid import Comparator, hybrid_property
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, relationship

DATABASE_URL = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_orm")
engine = create_async_engine(DATABASE_URL.replace("postgresql://", "postgresql+psycopg://", 1))
SessionLocal = async_sessionmaker(engine, expire_on_commit=False)


class Base(DeclarativeBase):
    pass


class CaseInsensitiveComparator(Comparator[str]):
    """the documentation's comparator: `__eq__` only (no `__hash__`: unhashable); other operators go to
    Comparator.operate, which raises NotImplementedError"""

    def __eq__(self, other: Any) -> ColumnElement[bool]:  # type: ignore[override]
        return func.lower(self.__clause_element__()) == func.lower(other)


class LowerComparator(Comparator):
    """every comparison through operate(): LOWER() on both sides"""

    def operate(self, op, other, **kwargs):
        return op(func.lower(self.__clause_element__()), func.lower(other), **kwargs)


class CaseInsensitiveWord(Comparator):
    "Hybrid value representing a lower case representation of a word."

    def __init__(self, word):
        if isinstance(word, str):
            self.word = word.lower()
        else:
            self.word = func.lower(word)

    def operate(self, op, other, **kwargs):
        if not isinstance(other, CaseInsensitiveWord):
            other = CaseInsensitiveWord(other)
        return op(self.word, other.word, **kwargs)

    def __clause_element__(self):
        return self.word

    def __str__(self):
        return self.word

    key = "word"
    "Label to apply to Query tuple results"


@dataclass(eq=False)
class CaseInsensitiveDc(Comparator):
    word: str | SQLColumnExpression[str]

    def __post_init__(self):
        if isinstance(self.word, str):
            self.word = self.word.lower()
        else:
            self.word = func.lower(self.word)

    def operate(self, op, other, **kwargs):
        if not isinstance(other, CaseInsensitiveDc):
            other = CaseInsensitiveDc(other)
        return op(self.word, other.word, **kwargs)

    def __clause_element__(self):
        return self.word


@dataclass(eq=False)
class Point(Comparator):
    x: int | SQLColumnExpression[int]
    y: int | SQLColumnExpression[int]

    def operate(self, op, other, **kwargs):
        return op(self.x, other.x) & op(self.y, other.y)

    def __clause_element__(self):
        return tuple_(self.x, self.y)


@dataclass(eq=False)
class Tag:
    """no base: compared and hashed by identity"""
    name: str


@dataclass
class Label:
    """eq=True (the default): compared by value, unhashable"""
    name: str


class SearchWord(Base):
    __tablename__ = "orm_searchword"

    id: Mapped[int] = mapped_column(primary_key=True)
    word: Mapped[str]

    @hybrid_property
    def word_insensitive(self) -> str:
        return self.word.lower()

    @word_insensitive.inplace.comparator
    @classmethod
    def _word_insensitive_comparator(cls) -> CaseInsensitiveComparator:
        return CaseInsensitiveComparator(cls.word)

    @hybrid_property
    def word_lower(self) -> str:
        return self.word.lower()

    @word_lower.comparator
    def word_lower(cls):
        return LowerComparator(cls.word)

    @hybrid_property
    def word_ci(self) -> CaseInsensitiveWord:
        return CaseInsensitiveWord(self.word)

    @hybrid_property
    def word_dc(self) -> CaseInsensitiveDc:
        return CaseInsensitiveDc(self.word)


class Vertex(Base):
    __tablename__ = "orm_vertices"

    id: Mapped[int] = mapped_column(primary_key=True)

    x1: Mapped[int]
    y1: Mapped[int]
    x2: Mapped[int]
    y2: Mapped[int]

    @hybrid_property
    def start(self) -> Point:
        return Point(self.x1, self.y1)

    @start.inplace.setter
    def _set_start(self, value: Point) -> None:
        self.x1 = value.x
        self.y1 = value.y

    @hybrid_property
    def end(self) -> Point:
        return Point(self.x2, self.y2)

    @end.inplace.setter
    def _set_end(self, value: Point) -> None:
        self.x2 = value.x
        self.y2 = value.y

    @hybrid_property
    def width(self) -> int:
        return self.x2 - self.x1

    def __repr__(self) -> str:
        return f"Vertex(start={self.start}, end={self.end})"


class Entry(Base):
    __tablename__ = "orm_entry"
    entry_id = mapped_column(Integer, primary_key=True)
    widget_id = mapped_column(Integer, ForeignKey("orm_widget.widget_id"))
    name = mapped_column(String(50))


class Widget(Base):
    __tablename__ = "orm_widget"

    widget_id = mapped_column(Integer, primary_key=True)
    favorite_entry_id = mapped_column(Integer, ForeignKey("orm_entry.entry_id", name="fk_orm_favorite_entry"))
    name = mapped_column(String(50))

    entries = relationship(Entry, primaryjoin=widget_id == Entry.widget_id)
    favorite_entry = relationship(Entry, primaryjoin=Entry.entry_id == favorite_entry_id, post_update=True)

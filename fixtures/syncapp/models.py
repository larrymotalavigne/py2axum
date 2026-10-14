from datetime import datetime
from typing import Optional

from sqlalchemy import JSON, Boolean, Column, DateTime, ForeignKey, Integer, LargeBinary, String, Table, Text
from sqlalchemy.dialects.postgresql import ARRAY as PG_ARRAY
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy import func
from sqlalchemy.ext.hybrid import hybrid_method, hybrid_property
from sqlalchemy.orm import Mapped, deferred, mapped_column, relationship

from .db import Base


class Author(Base):
    __tablename__ = "authors"

    id = Column(Integer, primary_key=True, index=True)
    name = Column(String(100), nullable=False)
    active = Column(Boolean, default=True, nullable=False)
    created_at = Column(DateTime, default=datetime(2026, 1, 2, 3, 4, 5), nullable=False)

    books = relationship("Book", back_populates="author", order_by="[Book.title, Book.id.desc()]")

    # adjacency list (a section and its parent section): many-to-one through remote_side=, its backref
    # one-to-many, and a one-to-many declared without remote_side=
    mentor_id = Column(Integer, ForeignKey("authors.id", ondelete="SET NULL"), nullable=True)
    mentor = relationship("Author", remote_side=[id], foreign_keys=[mentor_id], backref="mentees")
    pupils = relationship("Author", order_by="Author.name", overlaps="mentor,mentees")


# many-to-many: books <-> tags through an association table (relationship(secondary=))
book_tags = Table(
    "book_tags",
    Base.metadata,
    Column("book_id", ForeignKey("books.id"), primary_key=True),
    Column("tag_id", ForeignKey("tags.id"), primary_key=True),
)


class Tag(Base):
    __tablename__ = "tags"

    id = Column(Integer, primary_key=True)
    name = Column(String(30), nullable=False)


class Book(Base):
    __tablename__ = "books"

    id = Column(Integer, primary_key=True, index=True)
    title = Column(String(200), nullable=False)
    pages = Column(Integer, nullable=True)
    summary = Column(Text, nullable=True)
    # JSONB on PostgreSQL, JSON elsewhere; a variant for another dialect leaves the base type
    meta = Column(JSON().with_variant(JSONB(), "postgresql"), nullable=True)
    notes = Column(Text().with_variant(String(10), "sqlite"), nullable=True)
    cover = Column(LargeBinary, nullable=True)
    labels = Column(PG_ARRAY(String).with_variant(JSON(), "sqlite"), nullable=True)
    author_id = Column(Integer, ForeignKey("authors.id", ondelete="CASCADE"), nullable=False, index=True)

    author = relationship("Author", back_populates="books")
    tags = relationship("Tag", secondary="book_tags")

    # sqlalchemy.ext.hybrid: Python on an instance, SQL on the class
    @hybrid_property
    def double(self):
        return self.pages * 2

    @hybrid_method
    def longer_than(self, n):
        return self.pages > n

    @hybrid_property
    def label(self):
        return self.title + "!"

    @label.inplace.setter
    def _label_setter(self, value):
        self.title = value.rstrip("!")

    @label.inplace.expression
    @classmethod
    def _label_expression(cls):
        return func.upper(cls.title) + "!"

    # read by a response model (from_attributes): the first one lazy-loads
    @property
    def author_name(self) -> str | None:
        return self.author.name if self.author else None

    @property
    def shout(self) -> str:
        return self.title.upper()


class Review(Base):
    __tablename__ = "reviews"

    id = Column(Integer, primary_key=True)
    book_id = Column(Integer, ForeignKey("books.id", ondelete="CASCADE"), nullable=False)
    stars = Column(Integer, nullable=False)

    book = relationship("Book", backref="reviews")


class Order(Base):
    """Reserved words as table and column names: quoted in SQL like SQLAlchemy does."""

    __tablename__ = "order"

    id = Column(Integer, primary_key=True)
    end = Column(Integer, nullable=False)
    user = Column(String(20), nullable=True)
    author_id = Column(Integer, ForeignKey("authors.id"), nullable=True)

    author = relationship("Author")


class Stamp(Base):
    """`onupdate=` computed by the database: fetched by the UPDATE (eager_defaults) or expired."""

    __tablename__ = "stamps"
    __mapper_args__ = {"eager_defaults": True}

    id = Column(Integer, primary_key=True)
    name = Column(String(20), nullable=False)
    rev = Column(Integer, nullable=True, onupdate=func.abs(-7))


class LazyStamp(Base):
    __tablename__ = "lazy_stamps"
    __mapper_args__ = {"eager_defaults": "auto"}

    id = Column(Integer, primary_key=True)
    name = Column(String(20), nullable=False)
    rev = Column(Integer, nullable=True, onupdate=func.abs(-7))


# write-only collections (WriteOnlyMapped / lazy="write_only"): never loaded, changed through add/remove and
# bulk statements bound to the parent
from sqlalchemy.orm import WriteOnlyMapped  # noqa: E402


class Wallet(Base):
    __tablename__ = "wallets"

    id = Column(Integer, primary_key=True)
    name = Column(String(20), nullable=False)

    entries: WriteOnlyMapped["Entry"] = relationship(
        cascade="all, delete-orphan", passive_deletes=True, order_by="Entry.amount")


class Entry(Base):
    __tablename__ = "entries"

    id = Column(Integer, primary_key=True)
    wallet_id = Column(Integer, ForeignKey("wallets.id", ondelete="CASCADE"), nullable=True)
    purse_id = Column(Integer, ForeignKey("purses.id", ondelete="SET NULL"), nullable=True)
    label = Column(String(30), nullable=False)
    amount = Column(Integer, nullable=False)


audit_entries = Table(
    "audit_entries",
    Base.metadata,
    Column("audit_id", ForeignKey("audits.id", ondelete="CASCADE"), primary_key=True),
    Column("entry_id", ForeignKey("entries.id", ondelete="CASCADE"), primary_key=True),
)


class Audit(Base):
    __tablename__ = "audits"

    id = Column(Integer, primary_key=True)
    entries = relationship("Entry", secondary=audit_entries, lazy="write_only", passive_deletes=True)


class Purse(Base):
    """A write-only collection without delete-orphan: removed members keep their row, foreign key NULL."""

    __tablename__ = "purses"

    id = Column(Integer, primary_key=True)
    entries: WriteOnlyMapped["Entry"] = relationship(passive_deletes=True, foreign_keys="Entry.purse_id")


# single table inheritance: one table, the class of a row by its discriminator
class Staff(Base):
    __tablename__ = "staff"

    id = Column(Integer, primary_key=True)
    name = Column(String(30), nullable=False)
    kind = Column(String(20), nullable=False)
    wallet_id = Column(Integer, ForeignKey("wallets.id"), nullable=True)

    wallet = relationship("Wallet")

    __mapper_args__ = {"polymorphic_identity": "staff", "polymorphic_on": kind}

    def __repr__(self):
        return f"{self.__class__.__name__}({self.name!r})"


class Boss(Staff):
    budget = Column(Integer, nullable=True)
    __mapper_args__ = {"polymorphic_identity": "boss"}


class Coder(Staff):
    language = Column(String(20), nullable=True)
    __mapper_args__ = {"polymorphic_identity": "coder"}


class Lead(Coder):
    team = Column(String(20), nullable=True)
    __mapper_args__ = {"polymorphic_identity": "lead"}


class Manuscript(Base):
    """Deferred columns: a group, a raiseload one, deferred() around a Column."""

    __tablename__ = "manuscripts"

    id: Mapped[int] = mapped_column(primary_key=True)
    title: Mapped[str] = mapped_column(String(50))
    body: Mapped[str] = mapped_column(Text, deferred_group="content")
    draft: Mapped[Optional[str]] = mapped_column(Text, deferred=True, deferred_group="content")
    secret: Mapped[Optional[str]] = mapped_column(Text, deferred_raiseload=True)
    notes = deferred(Column(Text, nullable=True), group="extra")
    author_id: Mapped[Optional[int]] = mapped_column(ForeignKey("authors.id"))

    author = relationship("Author")


# a Core table used as a value (insert/select/update/delete on it, `t.c.col`), not mapped
events = Table(
    "events",
    Base.metadata,
    Column("id", Integer, primary_key=True),
    Column("kind", String(20), nullable=False),
    Column("note", Text, nullable=True),
    Column("author_id", ForeignKey("authors.id"), nullable=True),
)


# composite(): a value built from several columns
import dataclasses  # noqa: E402

from sqlalchemy.orm import composite  # noqa: E402


@dataclasses.dataclass
class Point:
    x: int
    y: int


class Pair:
    """A composite class that is not a dataclass: __composite_values__ gives its columns."""

    def __init__(self, a, b):
        self.a = a
        self.b = b

    def __composite_values__(self):
        return self.a, self.b

    def __repr__(self):
        return f"Pair({self.a!r}, {self.b!r})"

    def __eq__(self, other):
        return isinstance(other, Pair) and other.a == self.a and other.b == self.b


class Segment(Base):
    __tablename__ = "segments"

    id: Mapped[int] = mapped_column(primary_key=True)
    start: Mapped[Point] = composite(mapped_column("x1"), mapped_column("y1"))
    end: Mapped[Optional[Point]] = composite(mapped_column("x2"), mapped_column("y2"))
    lo: Mapped[int] = mapped_column(default=0)
    hi: Mapped[int] = mapped_column(default=0)
    span: Mapped[Pair] = composite(Pair, "lo", "hi")

    def __repr__(self):
        return f"Segment(start={self.start}, end={self.end})"


class Place(Base):
    __tablename__ = "places"

    id = Column(Integer, primary_key=True)
    lat = Column(Integer, nullable=True)
    lon = Column(Integer, nullable=True)
    spot = composite(Point, lat, lon)

# joined table inheritance: a table per class, joined on the key
class Member(Base):
    __tablename__ = "members"

    id = Column(Integer, primary_key=True)
    name = Column(String(30), nullable=False)
    kind = Column(String(20), nullable=False)
    stamp = Column(Integer, nullable=True, onupdate=func.abs(-1))

    __mapper_args__ = {"polymorphic_identity": "member", "polymorphic_on": kind}

    def __repr__(self):
        return f"{self.__class__.__name__}({self.name!r})"


class Admin(Member):
    __tablename__ = "admins"

    id = Column(Integer, ForeignKey("members.id"), primary_key=True)
    level = Column(Integer, nullable=False)
    tickets = relationship("Ticket", order_by="Ticket.id")

    __mapper_args__ = {"polymorphic_identity": "admin"}


class Root(Admin):
    __tablename__ = "roots"

    id = Column(Integer, ForeignKey("admins.id"), primary_key=True)
    shell = Column(String(20), nullable=True)

    __mapper_args__ = {"polymorphic_identity": "root"}


class Guest(Member):
    __tablename__ = "guests"

    id = Column(Integer, ForeignKey("members.id"), primary_key=True)
    note = Column(String(30), nullable=True)

    __mapper_args__ = {"polymorphic_identity": "guest"}


class Ticket(Base):
    __tablename__ = "tickets"

    id = Column(Integer, primary_key=True)
    admin_id = Column(Integer, ForeignKey("admins.id"), nullable=False)
    title = Column(String(30), nullable=False)

    admin = relationship("Admin", overlaps="tickets")


class Interval(Base):
    """A mapped class with its own __init__ (positional arguments, a default)."""

    __tablename__ = "intervals"

    id: Mapped[int] = mapped_column(primary_key=True)
    start: Mapped[int]
    end: Mapped[int]

    def __init__(self, start, end=None):
        self.start = start
        self.end = end if end is not None else start

    @hybrid_property
    def length(self):
        return self.end - self.start


@dataclasses.dataclass
class Box:
    """A composite built by a classmethod, its columns from __composite_values__ (dataclasses.astuple)."""

    lo: Point
    hi: Point

    @classmethod
    def _generate(cls, x1, y1, x2, y2):
        return Box(Point(x1, y1), Point(x2, y2))

    def __composite_values__(self):
        return dataclasses.astuple(self.lo) + dataclasses.astuple(self.hi)


class Framed(Base):
    __tablename__ = "framed"

    id: Mapped[int] = mapped_column(primary_key=True)
    x1: Mapped[int]
    y1: Mapped[int]
    x2: Mapped[int]
    y2: Mapped[int]
    box: Mapped[Box] = composite(Box._generate, "x1", "y1", "x2", "y2")

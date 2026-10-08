import uuid
from datetime import UTC, datetime
from decimal import Decimal

from sqlalchemy import (
    JSON, BigInteger, DateTime, Enum as SQLEnum, ForeignKey, Identity, Integer, LargeBinary, Numeric, String, func,
)
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy import Column
from sqlalchemy.orm import DeclarativeBase, Mapped, deferred, mapped_column, relationship

from .coltypes import EncryptedString, Money, enum_type
from .enums import Channel, Level, Priority, Status


def utcnow() -> datetime:
    return datetime.now(UTC)


class Base(DeclarativeBase):
    pass


class Owner(Base):
    __tablename__ = "owners"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(50))
    projects: Mapped[list["Project"]] = relationship(lazy="noload", overlaps="owner")

    @staticmethod
    def slug(name: str) -> str:
        return "-".join(name.strip().lower().split())

    @classmethod
    def named(cls, name: str, shout: bool = False) -> "Owner":
        return cls(name=cls.slug(name).upper() if shout else cls.slug(name))

    @classmethod
    def label(cls) -> str:
        return f"{cls.__name__}s"

    def describe(self) -> str:
        return f"{self.label()}:{self.slug(self.name)}"


class Project(Base):
    __tablename__ = "projects"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(50))
    owner_id: Mapped[int | None] = mapped_column(ForeignKey("owners.id"))
    budget: Mapped[Decimal | None] = mapped_column(Numeric(10, 2))
    owner: Mapped[Owner | None] = relationship(lazy="selectin")
    tasks: Mapped[list["Task"]] = relationship(back_populates="project", cascade="all, delete-orphan")


class Task(Base):
    __tablename__ = "tasks"

    id: Mapped[int] = mapped_column(Integer, primary_key=True)
    title: Mapped[str] = mapped_column(String(100))
    priority: Mapped[Priority] = mapped_column(SQLEnum(Priority), default=Priority.MEDIUM)
    status: Mapped[Status] = mapped_column(
        SQLEnum(Status, values_callable=lambda e: [m.value for m in e], native_enum=False, length=8), default=Status.OPEN
    )
    tags: Mapped[list] = mapped_column(JSON, default=list)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)
    touched_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=func.now())
    revision: Mapped[str] = mapped_column(String(10), default="new", onupdate="edited")
    updated_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), onupdate=utcnow)
    price: Mapped[float | None] = mapped_column(Money)
    project_id: Mapped[int | None] = mapped_column(ForeignKey("projects.id"))
    project: Mapped[Project | None] = relationship(back_populates="tasks")


class Secret(Base):
    __tablename__ = "secrets"

    id: Mapped[int] = mapped_column(primary_key=True)
    label: Mapped[str] = mapped_column(String(50))
    token: Mapped[str | None] = mapped_column(EncryptedString(300))


class Archive(Base):
    """A deferred column: left out of what a query loads."""
    __tablename__ = "archives"

    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(40))
    blob: Mapped[bytes | None] = deferred(Column(LargeBinary, nullable=True))

    @property
    def short(self) -> bool:
        return len(self.name) < 3


class Asset(Base):
    """Column types from a project factory function."""
    __tablename__ = "assets"

    id: Mapped[int] = mapped_column(primary_key=True)
    channel: Mapped[Channel] = mapped_column(enum_type(Channel, "asset_channel"), default=Channel.MAIL, index=True)
    data: Mapped[bytes | None] = mapped_column(LargeBinary)
    thumb: Mapped[bytes | None] = mapped_column(nullable=True)


class Membership(Base):
    """Composite primary key (configuration tables)."""
    __tablename__ = "memberships"

    owner_id: Mapped[int] = mapped_column(ForeignKey("owners.id"), primary_key=True)
    role: Mapped[str] = mapped_column(String(20), primary_key=True)
    note: Mapped[str | None] = mapped_column(String(50))
    owner: Mapped[Owner] = relationship(lazy="selectin")


class Ticket(Base):
    """JSONB(none_as_null=True), JSON, an Identity column that is not the key, an untyped foreign key."""
    __tablename__ = "tickets"

    code: Mapped[str] = mapped_column(String(20), primary_key=True)
    seq: Mapped[int] = mapped_column(BigInteger, Identity(always=False), unique=True, nullable=False)
    data = mapped_column(JSONB(none_as_null=True), nullable=True)
    raw = mapped_column(JSON, nullable=True)
    owner_id = mapped_column(ForeignKey("owners.id"), nullable=True)


# coproscan's shape: Uuid primary keys drawn by uuid.uuid4, a Uuid foreign key, JSONB with a sqlite variant
from sqlalchemy import Uuid  # noqa: E402
from sqlalchemy.types import JSON as SAJSON  # noqa: E402

UUIDVariant = Uuid(as_uuid=True)
JSONVariant = JSONB().with_variant(SAJSON(), "sqlite")


class Analysis(Base):
    __tablename__ = "analyses"

    id: Mapped[uuid.UUID] = mapped_column(UUIDVariant, primary_key=True, default=uuid.uuid4)
    address: Mapped[str] = mapped_column(String(100))
    findings: Mapped[dict | None] = mapped_column(JSONVariant, nullable=True)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)


class AnalysisDoc(Base):
    __tablename__ = "analysis_docs"

    id: Mapped[uuid.UUID] = mapped_column(UUIDVariant, primary_key=True, default=uuid.uuid4)
    analysis_id: Mapped[uuid.UUID] = mapped_column(UUIDVariant, ForeignKey("analyses.id", ondelete="CASCADE"), index=True)
    ref: Mapped[uuid.UUID | None] = mapped_column(nullable=True)
    name: Mapped[str] = mapped_column(String(50))


class EnumRow(Base):
    """Enum columns in every SQLAlchemy flavour, read back into schemas of every kind (fixtures/dynapp/enumcols.py)."""
    __tablename__ = "enum_rows"

    id: Mapped[int] = mapped_column(Integer, primary_key=True)
    # a (str, Enum) stored by NAME in a native PostgreSQL enum: SQLAlchemy's default
    pri: Mapped[Priority] = mapped_column(SQLEnum(Priority, name="enumrow_pri"))
    # a plain Enum, by name
    state: Mapped[Status] = mapped_column(SQLEnum(Status, name="enumrow_state"))
    # a StrEnum in a VARCHAR (native_enum=False, length=)
    chan: Mapped[Channel] = mapped_column(SQLEnum(Channel, native_enum=False, length=20))
    # an IntEnum, by name
    lvl: Mapped[Level] = mapped_column(SQLEnum(Level, name="enumrow_lvl"))
    # stored by value (values_callable=)
    pri_val: Mapped[Priority] = mapped_column(
        SQLEnum(Priority, name="enumrow_pri_val", values_callable=lambda e: [m.value for m in e]))
    maybe: Mapped[Priority | None] = mapped_column(SQLEnum(Priority, name="enumrow_maybe"), nullable=True)


class NoticeBlob(Base):
    """a shared attachment content: a deferred blob, loaded by `undefer()` (fixtures/dynapp/pydmore.py)."""
    __tablename__ = "notice_blobs"

    key: Mapped[str] = mapped_column("hash", String(64), primary_key=True)
    content: Mapped[bytes | None] = deferred(Column(LargeBinary, nullable=True))
    refs: Mapped[int] = mapped_column(default=1)


class Notice(Base):
    """SQL column names other than the attributes (`Column("metadata", JSON)` as a notification log)."""
    __tablename__ = "notices"

    id: Mapped[int] = mapped_column("nid", Integer, primary_key=True)
    extra_data: Mapped[dict | None] = Column("metadata", JSON, nullable=True)
    label: Mapped[str] = mapped_column("lbl", String(20), default="none")
    blob_key: Mapped[str | None] = mapped_column("blob_hash", ForeignKey("notice_blobs.hash"), nullable=True)
    blob: Mapped[NoticeBlob | None] = relationship(lazy="noload")

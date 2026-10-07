from datetime import datetime

from sqlalchemy import JSON, Boolean, Column, DateTime, ForeignKey, Integer, LargeBinary, String, Text
from sqlalchemy.dialects.postgresql import ARRAY as PG_ARRAY
from sqlalchemy.dialects.postgresql import JSONB
from sqlalchemy.orm import relationship

from .db import Base


class Author(Base):
    __tablename__ = "authors"

    id = Column(Integer, primary_key=True, index=True)
    name = Column(String(100), nullable=False)
    active = Column(Boolean, default=True, nullable=False)
    created_at = Column(DateTime, default=datetime(2026, 1, 2, 3, 4, 5), nullable=False)

    books = relationship("Book", back_populates="author", order_by="[Book.title, Book.id.desc()]")


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


class Review(Base):
    __tablename__ = "reviews"

    id = Column(Integer, primary_key=True)
    book_id = Column(Integer, ForeignKey("books.id", ondelete="CASCADE"), nullable=False)
    stars = Column(Integer, nullable=False)

    book = relationship("Book", backref="reviews")

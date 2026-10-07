"""Synchronous SQLAlchemy, as real-world projects use it: create_engine, sessionmaker, a generator dependency."""
import os

from sqlalchemy import create_engine
from sqlalchemy.ext.declarative import declarative_base
from sqlalchemy.orm import sessionmaker
from sqlalchemy.pool import StaticPool

DATABASE_URL = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_sync")

engine = create_engine(DATABASE_URL.replace("postgresql://", "postgresql+psycopg://", 1), pool_pre_ping=True,
                       poolclass=StaticPool if os.environ.get("TESTING") == "1" else None, pool_size=10, max_overflow=20)
SessionLocal = sessionmaker(autocommit=False, autoflush=False, bind=engine)
Base = declarative_base()


def get_db():
    """A session per request, closed after the response."""
    db = SessionLocal()
    try:
        yield db
    finally:
        db.close()

"""Synchronous SQLAlchemy, as real-world projects use it: create_engine, sessionmaker, a generator dependency."""
import os

from sqlalchemy import create_engine
from sqlalchemy.ext.declarative import declarative_base
from sqlalchemy.orm import Session, sessionmaker
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


def _prepare_sessionmaker(bind) -> sessionmaker:
    return sessionmaker(autocommit=False, autoflush=False, bind=bind)


OtherSessionLocal = _prepare_sessionmaker(engine)


def get_other_db():
    """Another shape seen in real projects: an annotated session, `raise exc` of the caught exception."""
    db: Session = OtherSessionLocal()
    try:
        yield db
    except Exception as exc:
        db.rollback()
        raise exc
    finally:
        db.close()

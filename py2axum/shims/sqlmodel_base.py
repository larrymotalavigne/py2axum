"""SQLModel's shared declarative base, as py2axum compiles SQLModel (py2axum/sqlmodel_rewrite.py): the table classes
of every module are mapped classes of it, `SQLModel.metadata` is theirs."""
from sqlalchemy.orm import DeclarativeBase


class SQLModel(DeclarativeBase):
    pass

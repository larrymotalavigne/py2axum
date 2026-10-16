"""queryapp's own tables (dynapp's are shared with the other fixtures): a column typed by `type_=`."""
from sqlalchemy import BigInteger, ForeignKey, String
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


class QBase(DeclarativeBase):
    pass


class Node(QBase):
    __tablename__ = "qry_nodes"

    id = mapped_column(BigInteger, primary_key=True)
    name: Mapped[str] = mapped_column(String(20))


class Link(QBase):
    __tablename__ = "qry_links"

    node_id = mapped_column(ForeignKey("qry_nodes.id", ondelete="CASCADE"), primary_key=True, type_=BigInteger)
    tag = mapped_column(primary_key=True, type_=String(10))

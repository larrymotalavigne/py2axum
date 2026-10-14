"""Inheritance in an async session: what a query loads of the subclasses decides what can be read without IO
(MissingGreenlet otherwise). with_polymorphic(), selectin_polymorphic() (also chained after selectinload()),
of_type(with_polymorphic(...)), polymorphic_load="selectin", options on a subclass's relationship."""
from fastapi import APIRouter
from sqlalchemy import ForeignKey, String, select
from sqlalchemy.orm import Mapped, mapped_column, relationship, selectin_polymorphic, selectinload, with_polymorphic

from .db import DbDep
from .models import Base

router = APIRouter(prefix="/inh")


class Firm(Base):
    __tablename__ = "inh_firms"
    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(20))
    people: Mapped[list["Person"]] = relationship(back_populates="firm", order_by="Person.id")


class Person(Base):
    __tablename__ = "inh_people"
    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(20))
    kind: Mapped[str] = mapped_column(String(10))
    firm_id: Mapped[int | None] = mapped_column(ForeignKey("inh_firms.id"))
    firm: Mapped[Firm | None] = relationship(back_populates="people")

    __mapper_args__ = {"polymorphic_identity": "person", "polymorphic_on": "kind"}


class Boss(Person):
    __tablename__ = "inh_bosses"
    id: Mapped[int] = mapped_column(ForeignKey("inh_people.id"), primary_key=True)
    budget: Mapped[int]
    notes: Mapped[list["Note"]] = relationship(order_by="Note.id")

    __mapper_args__ = {"polymorphic_identity": "boss"}


class Dev(Person):
    __tablename__ = "inh_devs"
    id: Mapped[int] = mapped_column(ForeignKey("inh_people.id"), primary_key=True)
    lang: Mapped[str] = mapped_column(String(10))

    __mapper_args__ = {"polymorphic_identity": "dev"}


class Note(Base):
    __tablename__ = "inh_notes"
    id: Mapped[int] = mapped_column(primary_key=True)
    boss_id: Mapped[int] = mapped_column(ForeignKey("inh_bosses.id"))
    text: Mapped[str] = mapped_column(String(20))


# single table: Hammer's columns come with any query on Tool (polymorphic_load), Saw's are read on access
class Tool(Base):
    __tablename__ = "inh_tools"
    id: Mapped[int] = mapped_column(primary_key=True)
    label: Mapped[str] = mapped_column(String(20))
    kind: Mapped[str] = mapped_column(String(10))

    __mapper_args__ = {"polymorphic_identity": "tool", "polymorphic_on": "kind"}


class Hammer(Tool):
    weight: Mapped[int | None]
    __mapper_args__ = {"polymorphic_identity": "hammer", "polymorphic_load": "selectin"}


class Saw(Tool):
    teeth: Mapped[int | None]
    __mapper_args__ = {"polymorphic_identity": "saw"}


async def seed(db) -> None:
    f = Firm(name="acme")
    db.add_all([
        Boss(name="b", budget=10, firm=f, notes=[Note(text="n1"), Note(text="n2")]),
        Dev(name="d1", lang="rs", firm=f), Dev(name="d2", lang="py", firm=f), Person(name="p", firm=f),
        Hammer(label="h", weight=3), Saw(label="s", teeth=40), Tool(label="t"),
    ])
    await db.commit()
    db.expunge_all()


def read(o, attr):
    """an attribute read without IO, or the class of what an async session raises when it would need IO
    (MissingGreenlet in SQLAlchemy 2.0, StatementError around it since 2.1)"""
    try:
        return getattr(o, attr)
    except Exception as e:
        return type(e).__name__


@router.post("/lazy")
async def lazy(db: DbDep):
    await seed(db)
    rows = (await db.scalars(select(Person).order_by(Person.id))).all()
    return [[type(r).__name__, r.name, read(r, "budget" if isinstance(r, Boss) else "lang")] for r in rows if type(r) is not Person]


@router.post("/selectin")
async def selectin(db: DbDep):
    await seed(db)
    stmt = select(Person).order_by(Person.id).options(selectin_polymorphic(Person, [Boss, Dev]), selectinload(Boss.notes))
    rows = (await db.scalars(stmt)).all()
    return [[type(r).__name__, read(r, "budget") if isinstance(r, Boss) else read(r, "lang") if isinstance(r, Dev) else None,
             [n.text for n in r.notes] if isinstance(r, Boss) else None] for r in rows]


@router.post("/selectin/only")
async def selectin_only(db: DbDep):
    """only Dev listed: a Boss's columns stay unloaded"""
    await seed(db)
    rows = (await db.scalars(select(Person).order_by(Person.id).options(selectin_polymorphic(Person, [Dev])))).all()
    return [[r.name, read(r, "lang") if isinstance(r, Dev) else read(r, "budget") if isinstance(r, Boss) else None] for r in rows]


@router.post("/withpoly")
async def withpoly(db: DbDep):
    await seed(db)
    poly = with_polymorphic(Person, "*")
    rows = (await db.scalars(select(poly).where((poly.Boss.budget > 5) | (poly.Dev.lang == "py")).order_by(poly.id))).all()
    out = [[type(r).__name__, r.name, read(r, "budget") if isinstance(r, Boss) else read(r, "lang")] for r in rows]
    devs = with_polymorphic(Person, [Dev])
    names = (await db.execute(select(Firm.name, devs.name, devs.Dev.lang).join(Firm.people.of_type(devs))
                              .order_by(devs.id))).all()
    return {"rows": out, "join": [list(x) for x in names]}


@router.post("/chained")
async def chained(db: DbDep):
    await seed(db)
    firm = (await db.scalars(select(Firm).options(selectinload(Firm.people).selectin_polymorphic([Boss, Dev])))).one()
    out = [[type(p).__name__, read(p, "budget") if isinstance(p, Boss) else read(p, "lang") if isinstance(p, Dev) else None]
           for p in firm.people]
    db.expunge_all()
    allp = with_polymorphic(Person, "*")
    firm = (await db.scalars(select(Firm).options(selectinload(Firm.people.of_type(allp))))).one()
    return {"chained": out, "of_type": [read(p, "lang") for p in firm.people if isinstance(p, Dev)]}


@router.post("/load")
async def load(db: DbDep):
    """polymorphic_load="selectin" on Hammer: its columns come with the query on Tool; Saw's do not"""
    await seed(db)
    rows = (await db.scalars(select(Tool).order_by(Tool.id))).all()
    return [[type(r).__name__, read(r, "weight") if isinstance(r, Hammer) else read(r, "teeth") if isinstance(r, Saw) else None]
            for r in rows]

"""The ORM corpus's remaining shapes: a mapped class's table used as a Core table (`Model.__table__.join(...)`,
`outerjoin`, `full=`, chained joins, `select_from(table).select_from(join)`, `t.c.col`, `select(table)`), and aliased
with_polymorphic() (`aliased=True`, `flat=`: two aliases of one hierarchy joined to each other, a subclass's columns
in the criteria under the alias, and loaded with the objects: read after the query without IO). Every route rolls
back what it wrote. Tables are dynapp's (fixtures/dynapp/models.py, fixtures/dynapp/inherit.py)."""
from fastapi import APIRouter
from sqlalchemy import or_, select
from sqlalchemy.orm import with_polymorphic

from fixtures.dynapp.db import DbDep
from fixtures.dynapp.inherit import Boss, Dev, Person, seed
from fixtures.dynapp.models import Owner, Project, Task

router = APIRouter(prefix="/ormx")


async def _owners(db):
    """explicit keys: other steps of the scenario insert owners with theirs"""
    db.add_all([Owner(id=1001, name="ann"), Owner(id=1002, name="bob"), Owner(id=1003, name="cid")])
    await db.flush()
    db.add_all([Project(id=1001, name="p1", owner_id=1001), Project(id=1002, name="p2", owner_id=1001),
                Project(id=1003, name="p3", owner_id=1002), Project(id=1004, name="p4")])
    await db.flush()
    db.add_all([Task(id=1001, title="t1", project_id=1001), Task(id=1002, title="t2", project_id=1001),
                Task(id=1003, title="t3")])
    await db.flush()


async def _rows(db, stmt) -> list:
    out = [list(r) for r in (await db.execute(stmt)).all()]
    await db.rollback()
    return out


@router.get("/table/join")
async def table_join(db: DbDep):
    """the ORM query guide's form: the join supersedes the earlier select_from(owners)"""
    await _owners(db)
    owners = Owner.__table__
    projects = Project.__table__
    j = projects.join(owners, owners.c.id == projects.c.owner_id)
    stmt = (select(projects.c.name, owners.c.name).select_from(owners).select_from(j)
            .where(owners.c.name == "ann").order_by(projects.c.name))
    return await _rows(db, stmt)


@router.get("/table/outerjoin")
async def table_outerjoin(db: DbDep, how: str = "outerjoin"):
    """ON from the foreign key; outerjoin(), join(isouter=True), join(full=True)"""
    await _owners(db)
    owners = Owner.__table__
    projects = Project.__table__
    if how == "outerjoin":
        j = owners.outerjoin(projects)
    elif how == "isouter":
        j = owners.join(projects, isouter=True)
    elif how == "full":
        j = owners.join(projects, owners.c.id == projects.c.owner_id, full=True)
    else:
        j = owners.join(projects)
    stmt = select(owners.c.name, projects.c.name).select_from(j).order_by(owners.c.name, projects.c.name)
    return await _rows(db, stmt)


@router.get("/table/chain")
async def table_chain(db: DbDep):
    """a join of a join, read straight from the attribute"""
    await _owners(db)
    tasks = Task.__table__
    j = Project.__table__.join(Owner.__table__).outerjoin(tasks, tasks.c.project_id == Project.__table__.c.id)
    stmt = select(Project.__table__.c.name, Owner.__table__.c.name, tasks.c.title).select_from(j).order_by(tasks.c.title)
    return await _rows(db, stmt)


@router.get("/table/rows")
async def table_rows(db: DbDep):
    """select(table): rows of every column, not objects"""
    await _owners(db)
    owners = Owner.__table__
    return await _rows(db, select(owners).where(owners.c.name != "cid").order_by(owners.c.id.desc()))


@router.get("/table/name")
async def table_name(db: DbDep):
    """the table's name next to a Core use of it"""
    owners = Owner.__table__
    await _owners(db)
    rows = await _rows(db, select(owners.c.name).select_from(owners.join(Project.__table__)).distinct().order_by(owners.c.name))
    return {"table": owners.name, "rows": rows}


def _people(rows) -> list:
    return [[type(a).__name__, a.name, a.budget if isinstance(a, Boss) else None, type(b).__name__, b.name,
             b.lang if isinstance(b, Dev) else None] for a, b in rows]


@router.get("/poly/aliased")
async def poly_aliased(db: DbDep, flat: bool = True):
    """the query guide's form: two aliased with_polymorphic() of Person joined, a subclass column in the WHERE"""
    await seed(db)
    bosses = with_polymorphic(Person, [Boss], aliased=True, flat=flat)
    devs = with_polymorphic(Person, [Dev], aliased=True, flat=flat)
    stmt = (select(bosses, devs).join(devs, devs.firm_id == bosses.firm_id)
            .where(or_(bosses.name == "nobody", bosses.Boss.budget > 5))
            .order_by(devs.name, bosses.name))
    out = _people((await db.execute(stmt)).all())
    await db.rollback()
    return out


@router.get("/poly/filter")
async def poly_filter(db: DbDep):
    """a subclass column under the alias in WHERE and ORDER BY, the base's id from the subclass namespace"""
    await seed(db)
    devs = with_polymorphic(Person, [Dev], aliased=True)
    rows = (await db.scalars(select(devs).where(devs.Dev.lang != "go", devs.Dev.id > 0).order_by(devs.Dev.lang.desc()))).all()
    out = [[type(p).__name__, p.name, p.lang] for p in rows]
    await db.rollback()
    return out


@router.get("/poly/star")
async def poly_star(db: DbDep):
    """every subclass ("*"), aliased: each object's own columns loaded with it"""
    await seed(db)
    every = with_polymorphic(Person, "*", aliased=True, flat=True)
    rows = (await db.scalars(select(every).order_by(every.id))).all()
    out = [[type(p).__name__, p.name, p.budget if isinstance(p, Boss) else p.lang if isinstance(p, Dev) else None]
           for p in rows]
    await db.rollback()
    return out

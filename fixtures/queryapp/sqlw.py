"""SQL constructs of reporting queries, on dynapp's tables: window functions (`.over()`), CTEs (`.cte()`), LATERAL
subqueries and explicit correlation (`.correlate()`), `case(*whens)`. Every route rolls back what it wrote."""
from fastapi import APIRouter
from sqlalchemy import ARRAY, Numeric, String, bindparam, case, cast, desc, func, literal, literal_column, select, true, union_all
from sqlalchemy.dialects.postgresql import array
from sqlalchemy.orm import aliased
from sqlalchemy.sql.expression import any_

from fixtures.dynapp.db import DbDep
from fixtures.dynapp.models import Owner, Project, Task

from .models import Link, Node

router = APIRouter(prefix="/sqlw")


async def _seed(db):
    """explicit keys, apart from the other fixtures' ranges"""
    db.add_all([Owner(id=2001, name="ann"), Owner(id=2002, name="bob"), Owner(id=2003, name="cid")])
    await db.flush()
    db.add_all([Project(id=2001, name="p1", owner_id=2001, budget=10), Project(id=2002, name="p2", owner_id=2001, budget=30),
                Project(id=2003, name="p3", owner_id=2002, budget=30), Project(id=2004, name="p4", budget=None),
                Project(id=2005, name="p5", owner_id=2002, budget=5)])
    await db.flush()
    db.add_all([Task(id=2001, title="t1", project_id=2001), Task(id=2002, title="t2", project_id=2001),
                Task(id=2003, title="t3", project_id=2002), Task(id=2004, title="t4")])
    await db.flush()


async def _rows(db, stmt) -> list:
    out = [list(r) for r in (await db.execute(stmt)).all()]
    await db.rollback()
    return out


@router.get("/over/rank")
async def over_rank(db: DbDep, how: str = "row_number"):
    """row_number / rank / dense_rank over a partition, ordered by a tuple of clauses"""
    await _seed(db)
    f = func.rank() if how == "rank" else func.dense_rank() if how == "dense_rank" else func.row_number()
    stmt = (select(Project.name, f.over(partition_by=Project.owner_id, order_by=(Project.budget.desc(), Project.id.desc()))
                   .label("rk"))
            .where(Project.id >= 2001).order_by(Project.id))
    return await _rows(db, stmt)


@router.get("/over/empty")
async def over_empty(db: DbDep):
    """`OVER ()`: an aggregate over the whole result"""
    await _seed(db)
    stmt = select(Project.name, func.count().over().label("n"), func.sum(Project.budget).over().label("total")) \
        .where(Project.id >= 2001).order_by(Project.id)
    return await _rows(db, stmt)


@router.get("/over/lists")
async def over_lists(db: DbDep):
    """partition_by and order_by given as lists; a running sum; nulls_last in the window"""
    await _seed(db)
    stmt = (select(Project.name,
                   func.sum(Project.budget).over(partition_by=[Project.owner_id], order_by=[Project.id]).label("run"),
                   func.lag(Project.name).over(order_by=desc(Project.budget).nulls_last()).label("prev"))
            .where(Project.id >= 2001).order_by(Project.id))
    return await _rows(db, stmt)


@router.get("/over/sub")
async def over_sub(db: DbDep):
    """the common shape: rank in a subquery, keep the first row of each partition"""
    await _seed(db)
    ranked = (select(Project.owner_id, Project.name,
                     func.row_number().over(partition_by=Project.owner_id, order_by=Project.budget.desc()).label("rk"))
              .where(Project.id >= 2001).subquery())
    stmt = select(ranked.c.owner_id, ranked.c.name).where(ranked.c.rk == 1).order_by(ranked.c.owner_id.nulls_first())
    return await _rows(db, stmt)


@router.get("/over/filter")
async def over_filter(db: DbDep):
    """an aggregate's FILTER, then its window; the window function's own type in the result"""
    await _seed(db)
    stmt = (select(Task.title,
                   func.count(Task.id).filter(Task.project_id.is_not(None)).over(partition_by=Task.project_id).label("c"))
            .where(Task.id >= 2001).order_by(Task.id))
    return await _rows(db, stmt)


@router.get("/cte/named")
async def cte_named(db: DbDep):
    """a named CTE as the first FROM, joined to a table; DISTINCT ON in the CTE"""
    await _seed(db)
    best = (select(Project.id, Project.owner_id).where(Project.budget > 6).where(Project.id >= 2001)
            .distinct(Project.owner_id).order_by(Project.owner_id, Project.budget.desc(), Project.id).cte("best"))
    stmt = select(Owner.name, best.c.id).select_from(best).join(Owner, Owner.id == best.c.owner_id).order_by(Owner.id)
    return await _rows(db, stmt)


@router.get("/cte/params")
async def cte_params(db: DbDep, low: int = 6, high: int = 2003):
    """an anonymous CTE: parameters in the CTE and in the statement, the CTE read in a WHERE subquery too"""
    await _seed(db)
    c = select(Project.id.label("pid"), Project.owner_id.label("oid")).where(Project.budget > low).cte()
    stmt = (select(c.c.pid, Owner.name).join(Owner, Owner.id == c.c.oid)
            .where(c.c.pid <= high).where(Owner.id.in_(select(c.c.oid))).order_by(c.c.pid))
    return await _rows(db, stmt)


@router.get("/cte/chain")
async def cte_chain(db: DbDep):
    """a CTE read by another CTE: both in the WITH clause, the first one first"""
    await _seed(db)
    a = select(Project.id, Project.budget).where(Project.id >= 2001).cte("a")
    b = select(a.c.id, (a.c.budget * 2).label("dbl")).where(a.c.budget.is_not(None)).cte("b")
    stmt = select(b.c.id, b.c.dbl).order_by(b.c.dbl.desc(), b.c.id)
    return await _rows(db, stmt)


@router.get("/cte/scalar")
async def cte_scalar(db: DbDep):
    """a CTE read only in a scalar subquery of the columns"""
    await _seed(db)
    c = select(Task.project_id).where(Task.id >= 2001).cte("tp")
    n = select(func.count()).select_from(c).where(c.c.project_id == Project.id).scalar_subquery()
    stmt = select(Project.name, n.label("n")).where(Project.id >= 2001).order_by(Project.id)
    return await _rows(db, stmt)


@router.get("/lateral")
async def lateral(db: DbDep, how: str = "literal"):
    """a correlated LATERAL ... LIMIT 1 joined ON TRUE (literal_column / true()), outer or inner"""
    await _seed(db)
    last = (select(Task.title.label("title")).where(Task.project_id == Project.id).order_by(Task.id.desc()).limit(1)
            .correlate(Project).lateral())
    stmt = select(Project.name, func.coalesce(last.c.title, "-").label("last")).select_from(Project)
    if how == "literal":
        stmt = stmt.outerjoin(last, literal_column("TRUE"))
    elif how == "true":
        stmt = stmt.outerjoin(last, true())
    else:
        stmt = stmt.join(last, true())
    return await _rows(db, stmt.where(Project.id >= 2001).order_by(Project.id))


@router.get("/lateral/auto")
async def lateral_auto(db: DbDep):
    """a named LATERAL without correlate(): correlated anyway (two FROMs)"""
    await _seed(db)
    top = (select(Project.name.label("pname")).where(Project.owner_id == Owner.id).order_by(Project.budget.desc().nulls_last())
           .limit(1).lateral("top"))
    stmt = select(Owner.name, top.c.pname).join(top, true()).where(Owner.id >= 2001).order_by(Owner.id)
    return await _rows(db, stmt)


@router.get("/correlate")
async def correlate(db: DbDep, how: str = "owner"):
    """explicit correlation of a scalar subquery; correlate(None): not correlated (every project counted)"""
    await _seed(db)
    sub = select(func.count(Project.id)).where(Project.owner_id == Owner.id).where(Project.id >= 2001)
    sub = sub.correlate(Owner) if how == "owner" else sub.correlate(None)
    stmt = select(Owner.name, sub.scalar_subquery().label("n")).where(Owner.id >= 2001).order_by(Owner.id)
    return await _rows(db, stmt)


THRESHOLDS = (8, 20, 40)


def _bucket(col):
    whens = []
    for t in THRESHOLDS:
        whens.append((col < t, literal(t)))
    return case(*whens, else_=literal(9999))


@router.get("/case/star")
async def case_star(db: DbDep, how: str = "list"):
    """case(*whens) from a list built in a loop, grouped by its value; a generator and a tuple of whens too"""
    await _seed(db)
    if how == "list":
        bucket = _bucket(Project.budget)
    elif how == "gen":
        bucket = case(*((Project.budget < t, t) for t in THRESHOLDS))
    else:
        bucket = case(*[(Project.budget.is_(None), "none")], *((Project.budget < t, str(t)) for t in THRESHOLDS[:1]),
                      else_="big")
    stmt = (select(bucket.label("b"), func.count().label("n")).where(Project.id >= 2001)
            .group_by(bucket).order_by(bucket.nulls_first()))
    return await _rows(db, stmt)


@router.get("/case/empty")
async def case_empty(db: DbDep, how: str = "empty"):
    """case(*[]): built (CASE ELSE ... END), refused by PostgreSQL; the 1.x list of whens: ArgumentError"""
    try:
        if how == "empty":
            stmt = select(case(*[], else_=1).label("x"))
            return await _rows(db, stmt)
        case([(Project.id == 1, 2)])
    except Exception as e:  # noqa: BLE001
        await db.rollback()
        return {"error": type(e).__name__, "msg": str(e).split("\n")[0]}
    return {"error": None}


@router.get("/icontains")
async def icontains(db: DbDep, q: str = "P", how: str = "icontains"):
    """case-insensitive LIKE (lower() on both sides), the value's % and _ not escaped"""
    await _seed(db)
    col = Project.name
    cond = col.icontains(q) if how == "icontains" else col.istartswith(q) if how == "istartswith" else col.iendswith(q)
    return await _rows(db, select(col).where(cond).where(Project.id >= 2001).order_by(Project.id))


@router.get("/numeric")
async def numeric(db: DbDep):
    """cast(..., Numeric) so that round(x, n) exists in PostgreSQL; Decimal results, float() of them"""
    await _seed(db)
    stmt = select(func.round(cast(func.avg(Project.budget), Numeric), 1).label("avg"),
                  func.round(cast(100.0 * func.sum(case((Project.budget >= 10, 1), else_=0)) / func.nullif(func.count(), 0),
                                  Numeric), 2).label("rate"),
                  cast(Project.id, Numeric).label("as_num")) \
        .where(Project.id >= 2001).group_by(Project.id).order_by(Project.id)
    rows = (await db.execute(stmt)).all()
    await db.rollback()
    return [{"avg": r.avg, "rate": r.rate, "f": float(r.rate or 0), "n": r.as_num, "t": type(r.avg).__name__} for r in rows]


@router.get("/union")
async def union(db: DbDep, how: str = "all"):
    """union_all() of two aliased selects made a subquery, grouped; select().union(); executed as is"""
    await _seed(db)
    p1, p2 = aliased(Project), aliased(Project)
    a = (select(p1.owner_id.label("oid"), func.count(p1.id).label("n")).where(p1.budget > 6, p1.id >= 2001)
         .group_by(p1.owner_id))
    b = (select(p2.owner_id.label("oid"), func.count(p2.id).label("n")).where(p2.budget.is_(None), p2.id >= 2001)
         .group_by(p2.owner_id))
    if how == "all":
        u = union_all(a, b).subquery("res")
        stmt = (select(u.c.oid, func.sum(u.c.n)).select_from(u).group_by(u.c.oid)
                .having(func.sum(u.c.n) >= 1).order_by(u.c.oid.nulls_first()))
    elif how == "method":
        u = a.union(b, select(Owner.id, func.count()).where(Owner.id == 2003).group_by(Owner.id)).subquery()
        stmt = select(u.c.oid, u.c.n).order_by(u.c.oid.nulls_last(), u.c.n)
    else:
        rows = (await db.execute(union_all(a, b))).all()
        await db.rollback()
        return sorted([list(r) for r in rows], key=lambda r: (r[0] is None, r[0] or 0, r[1]))
    return await _rows(db, stmt)


@router.post("/any")
async def any_array(db: DbDep, body: dict):
    """col == any_(array([...])) in two branches of a union_all(); an empty list: PostgreSQL's error"""
    await _seed(db)
    ids = array(body["names"])
    a = select(Project.name.label("k"), Project.id.label("v")).where(Project.name == any_(ids))
    b = select(Owner.name.label("k"), Owner.id.label("v")).where(Owner.name == any_(ids))
    u = a.union_all(b).subquery()
    try:
        return await _rows(db, select(u.c.k, u.c.v).order_by(u.c.v, u.c.k))
    except Exception as e:  # noqa: BLE001
        await db.rollback()
        return {"error": type(e).__name__}


@router.post("/typed")
async def typed(db: DbDep, body: dict):
    """a column typed by mapped_column(type_=...); bindparam(type_=ARRAY(String())) compared with any_();
    result.fetchone() row by row"""
    db.add_all([Node(id=5_000_000_000, name="big"), Node(id=7, name="small")])
    await db.flush()
    db.add_all([Link(node_id=5_000_000_000, tag="a"), Link(node_id=5_000_000_000, tag="b"), Link(node_id=7, tag="a")])
    await db.flush()
    p = bindparam("tags", type_=ARRAY(String()))
    res = await db.execute(select(Link.node_id, Link.tag).where(Link.tag == any_(p)).order_by(Link.node_id, Link.tag),
                           {"tags": body["tags"]})
    first, second = res.fetchone(), res.fetchone()
    rest = res.fetchall()
    after = res.fetchone()
    await db.rollback()
    return {"first": list(first) if first else None, "second": list(second) if second else None,
            "rest": [list(r) for r in rest], "after": after}


@router.get("/fetchone/closed")
async def fetchone_closed(db: DbDep):
    res = await db.execute(select(Owner.id).where(Owner.id == -1))
    res.first()
    try:
        res.fetchone()
    except Exception as e:  # noqa: BLE001
        return {"error": type(e).__name__, "msg": str(e)}
    return {"error": None}


@router.get("/aliased/sub")
async def aliased_sub(db: DbDep):
    """aliased(Model, select(Model)....subquery()): the entity read from the subquery (ordered, limited), joined;
    aliased(Model, flat=True) on a plain table"""
    await _seed(db)
    inner = select(Project).where(Project.budget > 6).order_by(Project.budget.desc(), Project.id).limit(3)
    p = aliased(Project, inner.subquery())
    stmt = select(p, Owner.name).join(Owner, Owner.id == p.owner_id, isouter=True).where(p.id >= 2001).order_by(p.id)
    rows = (await db.execute(stmt)).all()
    out = [[r[0].id, r[0].name, str(r[0].budget), r[1]] for r in rows]
    flat = aliased(Owner, flat=True)
    out.append([list(x) for x in (await db.execute(select(flat.id, flat.name).where(flat.id >= 2001).order_by(flat.id))).all()])
    await db.rollback()
    return out


@router.get("/aliased/more")
async def aliased_more(db: DbDep, how: str = "cte"):
    """aliased(Model, cte) joined (ON from the foreign key), aliased(Model, union_all(...).subquery()), and
    select(...).scalar_subquery().correlate(Model)"""
    await _seed(db)
    if how == "cte":
        c = select(Project).where(Project.budget > 6).cte()
        p = aliased(Project, c)
        stmt = select(Owner.name, p.name).join_from(Owner, p).where(Owner.id >= 2001).order_by(Owner.id, p.id)
    elif how == "union":
        u = union_all(select(Project).where(Project.id == 2001), select(Project).where(Project.id == 2003))
        p = aliased(Project, u.subquery())
        stmt = select(p.id, p.name).order_by(p.id)
    else:
        n = (select(func.count(Project.id)).where(Project.owner_id == Owner.id).scalar_subquery().correlate(Owner))
        stmt = select(Owner.name, n.label("n")).where(Owner.id >= 2001).order_by(Owner.id)
    return await _rows(db, stmt)

"""Composite primary keys: identity map, session.get forms, UPDATE/DELETE by key, outer joins."""
from fastapi import APIRouter
from pydantic import BaseModel, ConfigDict, TypeAdapter, ValidationError
from sqlalchemy import case, func, insert, literal, select, text, update
from sqlalchemy.dialects.postgresql import insert as pg_insert
from sqlalchemy.exc import InvalidRequestError

from .db import DbDep
from .models import Membership, Owner, Ticket

router = APIRouter(prefix="/composite")


@router.post("/run")
async def run(db: DbDep):
    o = Owner(name="o1")
    lonely = Owner(name="o2")
    db.add_all([o, lonely])
    await db.flush()
    db.add_all([Membership(owner_id=o.id, role="admin", note="a"), Membership(owner_id=o.id, role="dev")])
    await db.commit()
    m = await db.get(Membership, (o.id, "admin"))
    same = await db.get(Membership, [o.id, "admin"])
    m2 = await db.get(Membership, {"owner_id": o.id, "role": "dev"})
    miss = await db.get(Membership, (o.id, "nope"))
    errs = []
    for ident in (o.id, (o.id,), {"owner_id": o.id}):
        try:
            await db.get(Membership, ident)
        except InvalidRequestError as e:
            errs.append(str(e))
    m.note = "changed"
    await db.commit()
    await db.refresh(m)
    rows = (await db.execute(select(Membership).order_by(Membership.role))).scalars().all()
    pairs = (await db.execute(
        select(Owner.name, Membership)
        .outerjoin(Membership, Membership.owner_id == Owner.id)
        .order_by(Owner.id, Membership.role)
    )).all()
    await db.delete(m2)
    await db.commit()
    left = await db.scalar(select(func.count()).select_from(Membership))
    return {
        "note": m.note,
        "identity": same is m,
        "dev": [m2.owner_id == o.id, m2.role, m2.note],
        "miss": miss,
        "errs": errs,
        "rows": [[r.role, r.note, r.owner.name] for r in rows],
        "pairs": [[n, mm.role if mm is not None else None] for n, mm in pairs],
        "left": left,
    }


@router.get("/list")
async def listing(db: DbDep):
    rows = (await db.execute(select(Membership).order_by(Membership.role))).scalars().all()
    return [{"role": r.role, "note": r.note, "owner": r.owner.name} for r in rows]


@router.post("/tickets")
async def tickets(db: DbDep):
    o = Owner(name="t")
    db.add(o)
    await db.flush()
    a, b = Ticket(code="a", data=None, raw=None), Ticket(code="b", data={"x": 1}, raw=[1], owner_id=o.id)
    db.add_all([a, b])
    await db.flush()
    seqs = [a.seq > 0, b.seq > a.seq]
    await db.commit()
    q = "SELECT code, data IS NULL, raw IS NULL, jsonb_typeof(data), json_typeof(raw) FROM tickets ORDER BY code"
    before = [list(r) for r in (await db.execute(text(q))).all()]
    await db.execute(update(Ticket).where(Ticket.code == "b").values(data=None, raw=None))
    await db.commit()
    after = [list(r) for r in (await db.execute(text(q))).all()]
    got = await db.get(Ticket, "b")
    return {"seqs": seqs, "before": before, "after": after, "b": [got.data, got.raw, got.owner_id == o.id]}


@router.post("/upsert")
async def upsert(db: DbDep):
    """Core insert (several rows, column defaults), postgresql ON CONFLICT DO UPDATE / NOTHING with
    `excluded`, case(), is_distinct_from, RETURNING entities and columns, literal()."""
    o = Owner(name="u")
    db.add(o)
    await db.flush()
    r1 = await db.execute(insert(Membership).values([{"owner_id": o.id, "role": "a", "note": "n1"}, {"owner_id": o.id, "role": "b", "note": None}]))
    try:
        await db.execute(insert(Membership).values([{"owner_id": o.id, "role": "c", "note": "x"}, {"owner_id": o.id, "role": "d"}]))
        bad = None
    except Exception as e:  # noqa: BLE001
        bad = f"{type(e).__name__}: {e}"
    stmt = pg_insert(Membership).values(owner_id=o.id, role="a", note="n2")
    stmt = stmt.on_conflict_do_update(
        index_elements=["owner_id", "role"],
        set_={"note": case((Membership.note.is_distinct_from(stmt.excluded.note), stmt.excluded.note), else_=Membership.note)},
    ).returning(Membership)
    m = (await db.execute(stmt)).scalar()
    same = pg_insert(Membership).values(owner_id=o.id, role="a", note="n2")
    same = same.on_conflict_do_update(
        index_elements=[Membership.owner_id, Membership.role],
        set_={Membership.note: case((Membership.note.is_distinct_from(same.excluded.note), literal("changed")), else_=literal("kept"))},
    ).returning(Membership.note)
    kept = (await db.execute(same)).scalar()
    r3 = await db.execute(pg_insert(Membership).values(owner_id=o.id, role="b", note="zzz").on_conflict_do_nothing(index_elements=["owner_id", "role"]))
    # a named constraint (ON CONFLICT ON CONSTRAINT), several rows, one of them new
    r4 = await db.execute(pg_insert(Membership).values([{"owner_id": o.id, "role": "b"}, {"owner_id": o.id, "role": "e"}])
                          .on_conflict_do_nothing(constraint="memberships_pkey"))
    try:
        pg_insert(Membership).values(owner_id=o.id, role="f").on_conflict_do_nothing(constraint="memberships_pkey", index_elements=["role"])
        both = None
    except ValueError as e:
        both = str(e)
    rows = (await db.execute(pg_insert(Ticket).values(code="u1", data={"k": [1]}, raw=None).returning(Ticket.code, Ticket.data))).all()
    sel = (await db.execute(
        select(Membership.role, case((Membership.note == None, literal("none")), else_=Membership.note).label("n"))  # noqa: E711
        .where(Membership.owner_id == o.id).order_by(Membership.role)
    )).all()
    await db.commit()
    check = (await db.execute(text("SELECT raw IS NULL, json_typeof(raw) FROM tickets WHERE code = 'u1'"))).all()
    return {"r1": r1.rowcount, "bad": bad, "m": [m.role, m.note], "kept": kept, "r3": r3.rowcount, "r4": r4.rowcount, "both": both, "rows": [list(r) for r in rows],
            "sel": [list(r) for r in sel], "check": [list(r) for r in check]}


class OwnerRow(BaseModel):
    model_config = ConfigDict(from_attributes=True)
    id: int
    name: str


OWNERS = TypeAdapter(list[OwnerRow])
INTS = TypeAdapter(list[int])


@router.get("/adapter")
async def adapter(db: DbDep):
    rows = (await db.execute(select(Owner).order_by(Owner.id))).scalars().all()
    a = OWNERS.validate_python(rows)
    b = OWNERS.validate_python(list(map(lambda x: x.__dict__, rows)))
    err = None
    try:
        INTS.validate_python(["1", "x"])
    except ValidationError as e:
        err = e.errors(include_url=False)
    return {"a": a, "same": a == b, "keys": sorted(rows[0].__dict__) if rows else [], "c": INTS.validate_python(map(int, ["1", "2"])),
            "dump": OWNERS.dump_python(a[:1]), "json": INTS.dump_json([1, 2]).decode(), "err": err,
            "vjson": INTS.validate_json("[3, 4]")}


@router.get("/mappings")
async def mappings(db: DbDep):
    cols = [Owner.id, Owner.name.label("who")]
    rows = (await db.execute(select(*cols).order_by(Owner.id).limit(2))).mappings().all()
    first = (await db.execute(select(Owner.name).order_by(Owner.id))).mappings().first()
    return {"rows": [dict(r) for r in rows], "first": dict(first), "who": rows[0]["who"]}

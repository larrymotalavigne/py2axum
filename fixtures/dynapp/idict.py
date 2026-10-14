"""`obj.__dict__` around an INSERT (AsyncSession, expire_on_commit=False): a column never set and without an
insert default stays out of it (it reads None without IO), client and server defaults come back in it (RETURNING,
eager_defaults="auto"), a foreign key enters it when its relationship was set, the many-to-one a collection
append back-populates is there; then what a later read, SELECT, refresh or UPDATE adds."""
from fastapi import APIRouter
from sqlalchemy import select, text

from .db import DbDep
from .models import IdBack, IdEager, IdIdent, IdKid, IdParent, Owner

router = APIRouter(prefix="/idict")


def keys(o) -> list[str]:
    return sorted(k for k in o.__dict__ if not k.startswith("_"))


def build(case: str):
    """(named objects, objects to add)"""
    if case == "append":
        p, k, b = IdParent(name="p"), IdKid(title="k"), IdBack()
        p.kids.append(k)
        p.bkids.append(b)
        return {"p": p, "k": k, "b": b}, [p]
    if case == "m2o_set":
        p = IdParent(name="p")
        k = IdKid(title="k", parent=p)
        return {"p": p, "k": k}, [k]
    if case == "explicit_none":
        p = IdParent(name="p", owner=None, plain=None)
        return {"p": p}, [p]
    if case == "m2o_obj":
        o = Owner(name="o")
        p = IdParent(name="p", owner=o)
        return {"p": p, "o": o}, [p]
    if case == "identity":
        i = IdIdent()
        return {"i": i}, [i]
    if case == "eager":
        e = IdEager()
        return {"e": e}, [e]
    if case == "ctor_list":
        p = IdParent(name="p", kids=[IdKid(title="a")])
        return {"p": p, "k": p.kids[0]}, [p]
    if case == "backref_m2o":
        b, p = IdBack(), IdParent(name="p")
        b.bparent = p
        return {"p": p, "b": b}, [b]
    if case == "fk_none":
        p, k = IdParent(name="p", owner_id=None), IdKid(parent_id=None)
        return {"p": p, "k": k}, [p, k]
    if case == "batch":
        a, b = IdParent(name="a"), IdParent(name="b", plain="x")
        return {"a": a, "b": b}, [a, b]
    raise ValueError(case)


@router.post("/insert/{case}/{mode}")
async def insert(case: str, mode: str, db: DbDep):
    objs, add = build(case)
    # (an append to a collection sets the member's back reference only at flush in the runtime: docs/tutorial/sql.md)
    out = {"before": {n: keys(o) for n, o in objs.items() if not (case == "append" and n in ("k", "b"))}}
    db.add_all(add)
    if mode == "flush":
        await db.flush()
    else:
        await db.commit()
    out["after"] = {n: keys(o) for n, o in objs.items()}
    if "k" in objs and case != "fk_none":
        out["parent"] = objs["k"].parent is objs["p"]
    if case == "m2o_obj":
        # lazy="noload": not populated by the INSERT, an empty list once read
        out["noload"] = [objs["o"].projects, keys(objs["o"])]
    if "b" in objs and case in ("append", "backref_m2o"):
        out["bparent"] = objs["b"].bparent is objs["p"]
    return out


@router.post("/later")
async def later(db: DbDep):
    out = {}
    p = IdParent(name="p")
    db.add(p)
    await db.commit()
    # a never-set column reads None without loading, and stays out of the dict
    out["read"] = [p.plain, p.dnone, p.owner_id, keys(p)]
    p.name = "q"
    await db.commit()
    out["update"] = [keys(p), p.onupd]
    # a SELECT that returns the object fills what is not in its dict from the row
    await db.execute(text("UPDATE idict_parents SET plain = 'zz' WHERE id = :i"), {"i": p.id})
    await db.scalars(select(IdParent).where(IdParent.id == p.id))
    out["select"] = [keys(p), p.plain]
    q = IdParent(name="r")
    db.add(q)
    await db.commit()
    await db.execute(text("UPDATE idict_parents SET plain = 'yy' WHERE id = :i"), {"i": q.id})
    await db.get(IdParent, q.id)  # from the identity map: nothing loaded
    out["get"] = [keys(q), q.plain]
    await db.refresh(q)
    out["refresh"] = [keys(q), q.plain]
    # a relationship read on a just-inserted object without a key to follow
    r = IdParent(name="s")
    db.add(r)
    await db.flush()
    out["rel"] = [r.owner, keys(r)]
    # assigning None later puts it in the dict
    r.plain = None
    out["set_none"] = keys(r)
    return out


@router.post("/remove")
async def remove(db: DbDep):
    p, k = IdParent(name="p"), IdKid(title="k")
    p.kids.append(k)
    db.add(p)
    await db.flush()
    p.kids.remove(k)
    await db.flush()
    return {"k": keys(k), "parent": k.parent, "fk": k.parent_id, "p": keys(p)}

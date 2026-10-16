"""Reference app of the SQLAlchemy ORM corpus's last shapes, apart from fixtures/langapp (whose crate is near the CI
build containers' memory): hybrid attributes with a custom Comparator (`@x.inplace.comparator`, `@x.comparator`),
hybrid value objects (Comparator subclasses, plain or dataclasses: `operate()`, `__clause_element__()`, used in
Python and in SQL, on aliased() classes too), a composite value object with setters, `@dataclass(eq=False)`, and
`relationship(primaryjoin=..., post_update=True)` between two tables referencing each other. Every route rolls back
what it wrote."""
import operator
from typing import Annotated

from fastapi import Depends, FastAPI
from sqlalchemy import select, text
from sqlalchemy.exc import CompileError
from sqlalchemy.ext.asyncio import AsyncSession
from sqlalchemy.ext.hybrid import Comparator
from sqlalchemy.orm import aliased

from .models import (CaseInsensitiveDc, CaseInsensitiveWord, Entry, Label, Point, SearchWord, SessionLocal, Tag, Vertex,
                     Widget)

app = FastAPI()


async def get_db():
    async with SessionLocal() as session:
        yield session


DbDep = Annotated[AsyncSession, Depends(get_db)]

WORDS = ["Trucks", "pickup", "TRUCKS", "Zebra", "apple", "trucks!"]


async def _words(db) -> list:
    out = [SearchWord(id=i + 1, word=w) for i, w in enumerate(WORDS)]
    db.add_all(out)
    await db.flush()
    return out


async def _ids(db, stmt) -> list:
    out = [r[0] for r in (await db.execute(stmt)).all()]
    await db.rollback()
    return out


@app.get("/health")
async def health():
    return {"ok": True}


@app.get("/cmp/filter_by")
async def cmp_filter_by(db: DbDep, word: str = "Trucks"):
    """the documentation's custom comparator: LOWER() on both sides of =="""
    await _words(db)
    return await _ids(db, select(SearchWord.id).filter_by(word_insensitive=word).order_by(SearchWord.id))


@app.get("/cmp/where")
async def cmp_where(db: DbDep, word: str = "TRUCKS", op: str = "eq"):
    """== through the comparator's __eq__; the other operators go to Comparator.operate (NotImplementedError)"""
    await _words(db)
    col = SearchWord.word_insensitive
    try:
        cond = {"eq": lambda: col == word, "ne": lambda: col != word, "lt": lambda: col < word,
                "ge": lambda: col >= word}[op]()
    except NotImplementedError as e:
        await db.rollback()
        return {"exc": type(e).__name__}
    return await _ids(db, select(SearchWord.id).where(cond).order_by(SearchWord.id))


@app.get("/cmp/operate")
async def cmp_operate(db: DbDep, word: str = "trucks", op: str = "eq"):
    """a comparator whose operate() lowers both sides: every comparison operator"""
    await _words(db)
    col = SearchWord.word_lower
    cond = {"eq": lambda: col == word, "ne": lambda: col != word, "lt": lambda: col < word, "le": lambda: col <= word,
            "gt": lambda: col > word, "ge": lambda: col >= word, "rev": lambda: word == col,
            "rlt": lambda: word < col}[op]()
    return await _ids(db, select(SearchWord.id).where(cond).order_by(SearchWord.id))


@app.get("/cmp/objects")
async def cmp_objects(db: DbDep):
    """the instance's value (the getter), select() of the class-level attribute (the comparator's
    __clause_element__: the column)"""
    words = await _words(db)
    out = {"instance": words[0].word_insensitive, "lower": words[2].word_lower,
           "selected": [r[0] for r in (await db.execute(select(SearchWord.word_insensitive).order_by(SearchWord.id))).all()],
           "selected2": [r[0] for r in (await db.execute(select(SearchWord.word_lower).order_by(SearchWord.id))).all()]}
    await db.rollback()
    return out


@app.get("/value/sql")
async def value_sql(db: DbDep, word: str = "Trucks", kind: str = "ci", op: str = "eq"):
    """a hybrid value object (plain class or dataclass) in SQL: every comparison operator, the value lowered in Python"""
    await _words(db)
    col = SearchWord.word_ci if kind == "ci" else SearchWord.word_dc
    cond = {"eq": lambda: col == word, "ne": lambda: col != word, "lt": lambda: col < word, "le": lambda: col <= word,
            "gt": lambda: col > word, "ge": lambda: col >= word, "rev": lambda: word == col,
            "rgt": lambda: word > col}[op]()
    return await _ids(db, select(SearchWord.id).where(cond).order_by(SearchWord.id))


@app.get("/value/aliased")
async def value_aliased(db: DbDep, kind: str = "ci"):
    """the documentation's two aliases: the hybrid selected (coerced by __clause_element__) and compared"""
    await _words(db)
    sw1, sw2 = aliased(SearchWord), aliased(SearchWord)
    a, b = (sw1.word_ci, sw2.word_ci) if kind == "ci" else (sw1.word_dc, sw2.word_dc)
    stmt = select(sw1.id, sw2.id, a, b).filter(a > b).order_by(sw1.id, sw2.id)
    out = [list(r) for r in (await db.execute(stmt)).all()]
    await db.rollback()
    return out


@app.get("/value/order")
async def value_order(db: DbDep, kind: str = "ci"):
    """order_by() and a scalar select of the value object"""
    await _words(db)
    col = SearchWord.word_ci if kind == "ci" else SearchWord.word_dc
    out = [list(r) for r in (await db.execute(select(SearchWord.id, col).order_by(col, SearchWord.id.desc()))).all()]
    await db.rollback()
    return out


@app.get("/value/python")
async def value_python(word: str = "SomeWord", other: str = "sOmEwOrD"):
    """the same objects in Python: comparisons (reflected ones too), str(), repr() of the dataclass, sorting, `in`"""
    sw = SearchWord(word=word)
    ci, dc = sw.word_ci, sw.word_dc
    return {
        "ci": [ci == other, ci != other, ci < other, ci >= other, other == ci, other < ci, ci == ci],
        "dc": [dc == other, dc != other, dc > other, dc <= other, other == dc, other > dc],
        "str": str(ci), "key": ci.key, "word": ci.word, "repr_dc": repr(dc), "dc_word": dc.word,
        "sorted": [str(x) for x in sorted([CaseInsensitiveWord(w) for w in ["b", "C", "a"]])],
        "sorted_dc": [x.word for x in sorted([CaseInsensitiveDc(w) for w in ["b", "C", "a"]])],
        "max": str(max([CaseInsensitiveWord(w) for w in ["b", "C", "a"]])),
        "in": [other in [ci], "x" in [ci], other in [dc]],
        "isinstance": [isinstance(ci, Comparator), isinstance(dc, Comparator), isinstance(dc, CaseInsensitiveDc)],
        "hash": [isinstance(hash(ci), int), isinstance(hash(dc), int)],
    }


@app.get("/dataclass/eq")
async def dataclass_eq():
    """@dataclass(eq=False): identity; the default eq=True: by value and unhashable"""
    t1, t2 = Tag("a"), Tag("a")
    l1, l2 = Label("a"), Label("a")
    try:
        hash(l1)
        lh = "hashable"
    except TypeError as e:
        lh = str(e)
    return {"tag": [t1 == t2, t1 != t2, t1 == t1, t1 in [t2], t1 in [t1], len({t1, t2, t1}), repr(t1)],
            "label": [l1 == l2, l1 != l2, l1 in [l2], lh]}


async def _vertices(db) -> list:
    out = [Vertex(id=1, start=Point(3, 4), end=Point(15, 10)), Vertex(id=2, start=Point(3, 4), end=Point(5, 6)),
           Vertex(id=3, start=Point(1, 1), end=Point(2, 9))]
    db.add_all(out)
    await db.flush()
    return out


@app.get("/point/where")
async def point_where(db: DbDep, op: str = "doc"):
    """the documentation's query: a composite value object compared column by column (& is AND)"""
    await _vertices(db)
    cond = {"doc": lambda: (Vertex.start == Point(3, 4)) & (Vertex.end < Point(7, 8)),
            "ne": lambda: Vertex.start != Point(3, 4), "ge": lambda: Vertex.end >= Point(5, 6),
            "le": lambda: Vertex.end <= Point(5, 9), "gt": lambda: Vertex.end > Point(1, 5)}[op]()
    stmt = select(Vertex.id).where(cond).order_by(Vertex.id)
    if op == "doc":
        stmt = select(Vertex.id).where(Vertex.start == Point(3, 4)).where(Vertex.end < Point(7, 8))
    return await _ids(db, stmt)


@app.get("/point/python")
async def point_python(db: DbDep):
    """the constructor's hybrid setters, a setter by assignment, Python comparisons, repr() through the hybrids"""
    vs = await _vertices(db)
    v1 = vs[0]
    out = {"eq": [v1.end == Point(15, 10), v1.end == Point(15, 11), v1.start != Point(3, 4), v1.end > Point(1, 1)],
           "repr": repr(v1), "width": v1.width}
    v1.start = Point(7, 8)
    await db.flush()
    out["after"] = [list(r) for r in (await db.execute(select(Vertex.x1, Vertex.y1, Vertex.x2).where(Vertex.id == 1))).all()]
    out["by_width"] = [r[0] for r in (await db.execute(select(Vertex.id).where(Vertex.width > 1).order_by(Vertex.id))).all()]
    await db.rollback()
    return out


@app.get("/point/select")
async def point_select(db: DbDep):
    """selecting the value object: its __clause_element__ is a tuple_(), which SQLAlchemy refuses to SELECT"""
    await _vertices(db)
    try:
        await db.execute(select(Vertex.start))
        out = {"ok": True}
    except CompileError as e:
        out = {"exc": type(e).__name__, "msg": str(e)}
    await db.rollback()
    return out


@app.get("/point/nosetter")
async def point_nosetter(db: DbDep):
    """a hybrid without a setter given to the constructor"""
    try:
        Vertex(id=9, width=3)
        return {"ok": True}
    except AttributeError as e:
        return {"exc": type(e).__name__, "msg": str(e)}


async def _cycle(db) -> tuple:
    w1 = Widget(widget_id=1, name="somewidget")
    e1 = Entry(entry_id=1, name="someentry")
    w1.favorite_entry = e1
    w1.entries = [e1]
    db.add_all([w1, e1])
    await db.flush()
    return w1, e1


async def _state(db) -> dict:
    w = (await db.execute(text("SELECT widget_id, favorite_entry_id, name FROM orm_widget ORDER BY widget_id"))).all()
    e = (await db.execute(text("SELECT entry_id, widget_id, name FROM orm_entry ORDER BY entry_id"))).all()
    return {"widgets": [list(r) for r in w], "entries": [list(r) for r in e]}


@app.get("/post/insert")
async def post_insert(db: DbDep):
    """the documentation's flush: widget INSERTed without its favorite, the entries, then the UPDATE"""
    w1, e1 = await _cycle(db)
    out = await _state(db)
    out["fav"] = [w1.favorite_entry_id, w1.favorite_entry is e1, e1.widget_id]
    got = (await db.execute(select(Widget).where(Widget.favorite_entry_id == 1))).scalars().all()
    out["query"] = [w.name for w in got]
    await db.rollback()
    return out


@app.get("/post/change")
async def post_change(db: DbDep, to: str = "new"):
    """a persistent widget's favorite changed (to a new entry, an existing one, or None)"""
    w1, e1 = await _cycle(db)
    if to == "new":
        e2 = Entry(entry_id=2, name="other")
        w1.entries.append(e2)
        w1.favorite_entry = e2
        db.add(e2)
    elif to == "none":
        w1.favorite_entry = None
    else:
        e2 = Entry(entry_id=2, name="plain", widget_id=1)
        db.add(e2)
        await db.flush()
        w1.favorite_entry = e2
    await db.flush()
    out = await _state(db)
    await db.rollback()
    return out


@app.get("/post/delete")
async def post_delete(db: DbDep, what: str = "both"):
    """deleting rows referencing each other: the favorite set to NULL by an UPDATE first"""
    w1, e1 = await _cycle(db)
    if what == "both":
        await db.delete(w1)
        await db.delete(e1)
    elif what == "entry_first":
        await db.delete(e1)
        await db.delete(w1)
    else:
        e1.widget_id = None
        await db.delete(w1)
    await db.flush()
    out = await _state(db)
    await db.rollback()
    return out


@app.get("/post/many")
async def post_many(db: DbDep):
    """several widgets and entries in one flush, favorites crossing"""
    ws = [Widget(widget_id=i, name=f"w{i}") for i in (1, 2, 3)]
    es = [Entry(entry_id=i, name=f"e{i}") for i in (1, 2, 3, 4)]
    ws[0].entries = [es[0], es[1]]
    ws[1].entries = [es[2]]
    ws[0].favorite_entry = es[1]
    ws[1].favorite_entry = es[2]
    ws[2].favorite_entry = es[3]
    db.add_all(ws + es)
    await db.flush()
    out = await _state(db)
    out["names"] = sorted(e.name for e in ws[0].entries)
    await db.rollback()
    return out


@app.get("/post/commit")
async def post_commit():
    """the documentation's example as written: commit, then read back in a new session, then clean up"""
    async with SessionLocal() as s:
        w1 = Widget(name="somewidget")
        e1 = Entry(name="someentry")
        w1.favorite_entry = e1
        w1.entries = [e1]
        s.add_all([w1, e1])
        await s.commit()
        ids = [w1.widget_id is not None, e1.entry_id is not None, w1.favorite_entry_id == e1.entry_id,
               e1.widget_id == w1.widget_id]
    async with SessionLocal() as s:
        w = (await s.execute(select(Widget))).scalars().one()
        fav = (await s.execute(select(Entry).where(Entry.entry_id == w.favorite_entry_id))).scalars().one()
        out = {"ids": ids, "fav": fav.name}
        await s.delete(w)
        await s.delete(fav)
        await s.commit()
    async with SessionLocal() as s:
        out["left"] = await _state(s)
    return out


@app.get("/cmp/raise")
async def cmp_raise(op: str = "lt"):
    """Comparator.operate's NotImplementedError not caught: a 500, its traceback in the log"""
    if op == "lt":
        return {"cond": str(SearchWord.word_insensitive < "x")}
    try:
        _ = SearchWord.word_insensitive >= "x"
    except NotImplementedError as e:
        return {"msg": str(e), "callable": callable(operator.ge), "repr": repr(operator.ge)}

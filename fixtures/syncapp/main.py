"""fixtures/syncapp: synchronous SQLAlchemy sessions (lazy loading, expiry on commit) in sync endpoints."""
from typing import Annotated

from fastapi import Depends, FastAPI, HTTPException
from pydantic import BaseModel
from sqlalchemy.orm import Session

from .db import get_db, get_other_db
from .models import Author, Book, Review
from .schemas import AuthorCreate, AuthorRead, AuthorWithBooks, BookCreate, BookFull, BookRead

app = FastAPI()

Db = Annotated[Session, Depends(get_db)]


@app.get("/health")
def health():
    return {"ok": True}


@app.post("/authors", response_model=AuthorRead, status_code=201)
def create_author(payload: AuthorCreate, db: Db):
    author = Author(name=payload.name)
    db.add(author)
    db.commit()
    db.refresh(author)
    return author


@app.post("/authors/{author_id}/books", response_model=BookRead, status_code=201)
def add_book(author_id: int, payload: BookCreate, db: Session = Depends(get_db)):
    author = db.get(Author, author_id)
    if author is None:
        raise HTTPException(status_code=404, detail="Author not found")
    book = Book(title=payload.title, pages=payload.pages, author=author, meta={"pages": payload.pages, "tags": ["x"]},
                notes="n" * 20)
    db.add(book)
    db.commit()
    # expired by the commit: reading it reloads the row
    return book


@app.post("/books/{book_id}/reviews/{stars}")
def add_review(book_id: int, stars: int, db: Db):
    review = Review(book_id=book_id, stars=stars)
    db.add(review)
    db.commit()
    return {"id": review.id, "stars": review.stars, "book": review.book.title}


@app.get("/authors/{author_id}", response_model=AuthorWithBooks)
def read_author(author_id: int, db: Db):
    author = db.get(Author, author_id)
    if author is None:
        raise HTTPException(status_code=404, detail="Author not found")
    return author


@app.get("/books/{book_id}", response_model=BookFull)
def read_book(book_id: int, db: Db):
    book = db.get(Book, book_id)
    if book is None:
        raise HTTPException(status_code=404, detail="Book not found")
    return book


@app.get("/books/{book_id}/lazy")
def lazy_book(book_id: int, db: Db):
    book = db.get(Book, book_id)
    # many-to-one then one-to-many, each loaded on access
    return {"author": book.author.name, "siblings": [b.title for b in book.author.books],
            "reviews": len(book.reviews), "meta": book.meta, "notes": book.notes}


@app.patch("/authors/{author_id}")
def rename(author_id: int, payload: AuthorCreate, db: Db):
    author = db.get(Author, author_id)
    author.name = payload.name
    before = len(author.books)
    db.commit()
    # every attribute expired: name reloaded, books reloaded on access
    return {"name": author.name, "books": before, "after": [b.title for b in author.books]}


@app.post("/authors/{author_id}/deactivate")
def deactivate(author_id: int, db: Db):
    author = db.get(Author, author_id)
    author.active = False
    db.flush()
    db.rollback()
    # rolled back: reloaded from the row
    return {"active": author.active}


@app.delete("/authors/{author_id}")
def delete_author(author_id: int, db: Db):
    author = db.get(Author, author_id)
    if author is None:
        raise HTTPException(status_code=404, detail="Author not found")
    db.delete(author)
    db.commit()
    return {"deleted": author_id}


# ---- self-referential relationship (remote_side=)
@app.put("/authors/{author_id}/mentor/{mentor_id}")
def set_mentor(author_id: int, mentor_id: int, db: Db):
    author = db.get(Author, author_id)
    if author is None:
        raise HTTPException(status_code=404, detail="Author not found")
    author.mentor = db.get(Author, mentor_id)
    db.commit()
    return {"id": author.id, "mentor_id": author.mentor_id}


@app.get("/authors/{author_id}/mentor")
def read_mentor(author_id: int, db: Db):
    author = db.get(Author, author_id)
    if author is None:
        raise HTTPException(status_code=404, detail="Author not found")
    return {"mentor": author.mentor.name if author.mentor else None,
            "mentees": sorted(a.name for a in author.mentees),
            "pupils": [a.name for a in author.pupils]}


# ---- the legacy Query API (session.query)
from sqlalchemy import func  # noqa: E402
from sqlalchemy.exc import MultipleResultsFound, NoResultFound  # noqa: E402


@app.get("/q/authors", response_model=list[AuthorRead])
def q_authors(db: Db):
    return db.query(Author).order_by(Author.id).all()


@app.get("/q/first")
def q_first(db: Db):
    a = db.query(Author).filter(Author.name.ilike("%a%")).order_by(Author.id.desc()).first()
    none = db.query(Author).filter(Author.name == "nobody").first()
    return {"first": a.name if a else None, "none": none, "row": list(db.query(Book.id, Book.title).order_by(Book.id).first())}


@app.get("/q/count")
def q_count(db: Db):
    return {"all": db.query(Author).count(), "pages": db.query(Book).filter(Book.pages.isnot(None)).count(),
            "join": db.query(Book).join(Author).filter(Author.active.is_(True)).count(),
            "by": db.query(Book).filter_by(author_id=1).count(), "grouped": db.query(Book.author_id).group_by(Book.author_id).count()}


@app.get("/q/scalar")
def q_scalar(db: Db):
    out = {"count": db.query(func.count(Book.id)).scalar(), "title": db.query(Book.title).filter(Book.id == 1).scalar(),
           "none": db.query(Author).filter_by(name="nobody").scalar(),
           "entity": db.query(Author).filter(Author.id == 1).scalar().name}
    try:
        db.query(Book.title).scalar()
    except MultipleResultsFound as e:
        out["many"] = str(e)
    return out


@app.get("/q/rows")
def q_rows(db: Db):
    counts = db.query(Author.name, func.count(Book.id).label("n")).outerjoin(Book).group_by(Author.name).order_by(Author.name).all()
    titles = db.query(Book.title).order_by(Book.id).all()
    return {"counts": [[r.name, r.n] for r in counts], "titles": [r[0] for r in titles], "attr": [r.title for r in titles],
            "with": [list(r) for r in db.query(Book).with_entities(Book.id, Book.pages).order_by(Book.id).all()],
            "distinct": sorted(r[0] for r in db.query(Book.author_id).distinct().all())}


@app.get("/q/one/{name}")
def q_one(name: str, db: Db):
    try:
        a = db.query(Author).filter(Author.name == name).one()
        return {"one": a.id, "or_none": db.query(Author).filter(Author.name == name).one_or_none().id}
    except NoResultFound as e:
        return {"error": str(e), "or_none": db.query(Author).filter(Author.name == name).one_or_none()}


@app.get("/q/more")
def q_more(db: Db):
    sub = db.query(Author.id).filter(Author.active.is_(True))
    return {"get": db.query(Author).get(1).name, "missing": db.query(Author).get(99),
            "in": db.query(Book).filter(Book.author_id.in_(sub)).count(),
            "exists": db.query(db.query(Book).filter(Book.pages > 10).exists()).scalar(),
            "page": [b.title for b in db.query(Book).order_by(Book.id).offset(1).limit(1).all()],
            "iter": [a.name for a in db.query(Author).order_by(Author.id)],
            "lazy": [len(a.books) for a in db.query(Author).order_by(Author.id).all()]}


@app.post("/q/update")
def q_update(db: Db):
    n = db.query(Book).filter(Book.pages.is_(None)).update({Book.pages: 1}, synchronize_session=False)
    m = db.query(Book).filter(Book.id == 1).update({"title": "Notes 2"})
    db.commit()
    return {"n": n, "m": m, "pages": [b.pages for b in db.query(Book).order_by(Book.id).all()]}


@app.post("/q/delete")
def q_delete(db: Db):
    n = db.query(Review).filter(Review.stars < 4).delete()
    db.commit()
    return {"n": n, "left": db.query(Review).count()}


# ---- LargeBinary and ARRAY(String) columns
class LabelsIn(BaseModel):
    labels: list[str | None] | None
    cover: str | None = None


@app.put("/books/{book_id}/labels")
def set_labels(book_id: int, payload: LabelsIn, db: Db):
    book = db.get(Book, book_id)
    book.labels = payload.labels
    book.cover = payload.cover.encode() if payload.cover is not None else None
    db.commit()
    return {"labels": book.labels, "cover": list(book.cover) if book.cover is not None else None,
            "size": len(book.cover or b"")}


@app.get("/labels/{label}")
def by_label(label: str, db: Db):
    out = {"contains": [b.id for b in db.query(Book).filter(Book.labels.contains([label])).order_by(Book.id)],
           "any": [b.id for b in db.query(Book).filter(Book.labels.any(label)).order_by(Book.id)],
           "overlap": [b.id for b in db.query(Book).filter(Book.labels.overlap([label, "zz"])).order_by(Book.id)],
           "null": db.query(Book).filter(Book.labels.is_(None)).count()}
    try:
        db.query(Book).filter(Book.meta.op("?|")([label])).all()  # jsonb ?| jsonb: no such operator
    except Exception as e:  # noqa: BLE001
        out["op"] = type(e).__name__
        db.rollback()
    return out


# ---- constructions of a real application: an ORM @property in a response model, a model built from ORM
# objects, a SQL expression shared by the columns and GROUP BY, a synchronous session awaited
from sqlalchemy import select  # noqa: E402

from .schemas import BookList, BookTitled  # noqa: E402

FIRST = func.substr(Book.title, 1, 1)


@app.get("/books/{book_id}/titled", response_model=BookTitled)
def titled(book_id: int, db: Db):
    return db.get(Book, book_id)


@app.get("/booklist", response_model=BookList)
def booklist(db: Db):
    books = db.query(Book).order_by(Book.id).all()
    return BookList(items=books, total=len(books))


@app.get("/q/initials")
def initials(db: Db):
    rows = db.query(FIRST.label("i"), func.count(Book.id).label("n")).group_by(FIRST).order_by(FIRST).all()
    out = {"shared": [[r.i, r.n] for r in rows]}
    try:  # two expressions built apart: two parameters, PostgreSQL refuses the GROUP BY
        db.query(func.substr(Book.title, 1, 1), func.count(Book.id)).group_by(func.substr(Book.title, 1, 1)).all()
        out["apart"] = "ok"
    except Exception as e:  # noqa: BLE001
        out["apart"] = type(e).__name__
        db.rollback()
    return out


async def awaited_lookup(db, book_id: int):
    result = await db.execute(select(Book).where(Book.id == book_id))
    return result.scalar_one_or_none()


@app.get("/books/{book_id}/awaited")
async def awaited(book_id: int, db: Db):
    try:
        await awaited_lookup(db, book_id)
        return {"ok": True}
    except TypeError as e:
        return {"error": str(e)}


@app.get("/when")
def when():
    from datetime import date, datetime

    d = datetime(2026, 3, 4, 5, 6, 7)
    return {"fmt": f"{d:%d/%m à %H:%M}", "date": f"{date(2026, 1, 2):%Y}", "plain": f"{d}",
            "has": [hasattr(d, "isoformat"), hasattr(d, "nope"), hasattr(date(2026, 1, 2), "strftime")],
            "iso": d.isoformat() if hasattr(d, "isoformat") else str(d)}


class Patterns:
    WORDS = ["a", "b"]

    @staticmethod
    def joined() -> str:
        return "-".join(Patterns.WORDS)


@app.get("/classattr")
def classattr():
    return {"joined": Patterns.joined(), "n": len(Patterns.WORDS)}


from sqlalchemy import text  # noqa: E402

from .db import engine  # noqa: E402


@app.post("/core/many")
def core_many():
    """A sync engine's connections (`with engine.begin()` / `engine.connect()`) and text() executemany."""
    with engine.begin() as conn:
        r = conn.execute(text("INSERT INTO authors (name, active, created_at) VALUES (:name, true, '2026-01-02')"),
                         [{"name": "M1"}, {"name": "M2"}])
        inserted = r.rowcount
    with engine.connect() as conn:
        r = conn.execute(text("UPDATE authors SET name = :new WHERE name = :old"),
                         [{"new": "M1b", "old": "M1"}, {"new": "Mx", "old": "nobody"}])
        updated = r.rowcount
        conn.commit()
    with engine.connect() as conn:  # not committed: rolled back when the block ends
        conn.execute(text("UPDATE authors SET name = 'lost' WHERE name = :n"), [{"n": "M2"}])
    try:
        with engine.begin() as conn:
            conn.execute(text("INSERT INTO authors (name, active, created_at) VALUES (:name, true, '2026-01-02')"),
                         [{"name": "M3"}])
            raise ValueError("rolled back")
    except ValueError as e:
        failed = str(e)
    with engine.connect() as conn:
        names = [row.name for row in conn.execute(text("SELECT name FROM authors WHERE name LIKE 'M%' ORDER BY name"))]
        try:
            conn.execute(text("SELECT :a"), [{"b": 1}])
        except Exception as e:  # noqa: BLE001
            missing = type(e).__name__
    return {"inserted": inserted, "updated": updated, "failed": failed, "names": names, "missing": missing}


from sqlalchemy.orm.exc import DetachedInstanceError  # noqa: E402


@app.get("/expunge/{author_id}")
def expunge(author_id: int, db: Db):
    """expunge_all()/expunge(): the objects leave the session (detached), a later query loads new ones."""
    first = db.get(Author, author_id)
    if first is None:
        raise HTTPException(status_code=404, detail="Author not found")
    name = first.name
    db.expunge_all()
    out = {"name": name, "kept": first.name}
    try:
        out["books"] = len(first.books)
    except DetachedInstanceError as e:
        out["books"] = type(e).__name__
    again = db.get(Author, author_id)
    out["same"] = again is first
    again.name = again.name + "?"
    db.expunge(again)
    db.commit()  # nothing to flush: the change left with the object
    out["after"] = db.get(Author, author_id).name
    try:
        db.expunge(again)
    except Exception as e:  # noqa: BLE001
        out["twice"] = type(e).__name__
    db.expire_all()
    try:
        first.name
    except Exception as e:  # noqa: BLE001
        out["expired"] = type(e).__name__
    return out


from sqlalchemy.orm import selectinload  # noqa: E402

from .models import Tag  # noqa: E402


@app.post("/m2m")
def m2m(db: Db):
    """relationship(secondary=): lazy and selectin loading, join and any() through the association table, rows
    added and removed at flush, deleted with their owner."""
    red, blue, green = Tag(name="red"), Tag(name="blue"), Tag(name="green")
    b1, b2 = db.get(Book, 1), db.get(Book, 2)
    b1.tags = [red, blue]
    b2.tags.append(blue)
    db.commit()
    out = {"b1": [t.name for t in b1.tags], "b2": [t.name for t in b2.tags]}
    b1.tags.remove(blue)
    b2.tags.append(green)
    db.commit()
    db.expire_all()
    out["after"] = {b.title: sorted(t.name for t in b.tags) for b in db.scalars(select(Book).order_by(Book.id))}
    rows = db.execute(select(Book.title, Tag.name).join(Book.tags).order_by(Book.id, Tag.name)).all()
    out["join"] = [list(r) for r in rows]
    out["outer"] = [list(r) for r in db.execute(select(Book.title, Tag.name).outerjoin(Book.tags).order_by(Book.id, Tag.name))]
    out["any"] = db.scalars(select(Book.title).where(Book.tags.any(Tag.name == "green")).order_by(Book.id)).all()
    out["none"] = db.scalars(select(Book.title).where(~Book.tags.any()).order_by(Book.id)).all()
    db.expunge_all()
    eager = db.scalars(select(Book).options(selectinload(Book.tags)).order_by(Book.id)).all()
    out["eager"] = {b.title: sorted(t.name for t in b.tags) for b in eager}
    db.delete(db.get(Book, 2))
    db.commit()
    out["left"] = [list(r) for r in db.execute(text("SELECT book_id, tag_id FROM book_tags ORDER BY 1, 2")).all()]
    return out


from sqlalchemy.orm import aliased  # noqa: E402


@app.get("/relq")
def relq(db: Db):
    """Relationship criteria: many-to-one == / != / None, one-to-many contains(), join(Target, Model.rel), join()
    of an aliased class's relationship, select_from() then join(), execution_options of a SELECT."""
    ada, book = db.get(Author, 1), db.get(Book, 1)
    out = {
        "eq": db.scalars(select(Book.title).where(Book.author == ada).order_by(Book.id)).all(),
        "ne": db.scalars(select(Book.title).where(Book.author != ada).order_by(Book.id)).all(),
        "none": db.scalars(select(Book.title).where(Book.author == None).order_by(Book.id)).all(),  # noqa: E711
        "contains": db.scalars(select(Author.name).where(Author.books.contains(book))).all(),
        "join2": [list(r) for r in db.execute(select(Author.name, Book.title).join(Book, Author.books).order_by(Book.id))],
    }
    b2 = aliased(Book)
    out["aliasjoin"] = [list(r) for r in db.execute(
        select(Author.name, b2.title).join(b2, Author.books).where(b2.pages > 0).order_by(b2.id))]
    a2 = aliased(Author)
    out["aliasrel"] = db.scalars(select(a2.name).join(a2.books).order_by(Book.id)).all()
    out["select_from"] = db.scalars(select(Book.title).select_from(Author).join(Book.author).where(Author.id == 1)
                                    .order_by(Book.id)).all()
    ada.name = "changed, not flushed"
    out["no_autoflush"] = db.scalars(select(Author.name).where(Author.id == 1).execution_options(autoflush=False)).all()
    fresh = db.scalars(select(Author).where(Author.id == 1).execution_options(populate_existing=True)).one()
    out["populate"] = [fresh is ada, ada.name]
    out["yield_per"] = len(db.scalars(select(Book).execution_options(yield_per=2)).all())
    try:
        select(Book).where(Author.books == book)
    except Exception as e:  # noqa: BLE001
        out["collection_eq"] = type(e).__name__
    return out


from sqlalchemy import insert, update  # noqa: E402


@app.post("/bulk")
def bulk(db: Db):
    """ORM bulk INSERT (keys differing between rows, Python defaults, RETURNING entities) and bulk UPDATE by
    primary key (applied to the objects of the session)."""
    r = db.execute(insert(Author), [{"name": "B1", "active": False}, {"name": "B2"}, {"name": "B3", "active": True}])
    out = {"first": r.first()}
    try:
        r.all()
    except Exception as e:  # noqa: BLE001
        out["all"] = [type(e).__name__, str(e)]
    try:
        r.scalar()
    except Exception as e:  # noqa: BLE001
        out["scalar"] = [type(e).__name__, str(e)]
    try:
        r.rowcount
    except AttributeError as e:
        out["rowcount"] = str(e)
    s = db.execute(select(Author.name).where(Author.name.like("B%")).order_by(Author.id))
    out["twice"] = [len(s.all()), s.all()]
    s = db.execute(select(Author.name).order_by(Author.id))
    s.first()
    try:
        s.all()
    except Exception as e:  # noqa: BLE001
        out["closed"] = [type(e).__name__, str(e)]
    made = db.scalars(insert(Author).returning(Author), [{"name": "B4"}, {"name": "B5", "active": False}]).all()
    out["made"] = [[a.name, a.active, a in db] for a in made]
    out["ids"] = [list(row) for row in db.execute(insert(Author).returning(Author.id, Author.name, sort_by_parameter_order=True), [{"name": "B6"}, {"name": "B7"}])]
    # a None is left out (the column's default applies), unless render_nulls
    out["none"] = [[a.name, a.active] for a in db.scalars(insert(Author).returning(Author), [{"name": "B8", "active": None}])]
    # insert().values([...]): the columns of the first row; a SELECT value is a scalar subquery
    out["values"] = [[a.name, a.active] for a in db.scalars(insert(Author).values(
        [{"name": "V1"}, {"name": "V2", "active": False},
         {"name": select(Author.name).where(Author.id == 1).scalar_subquery()}]).returning(Author))]
    out["subq"] = [a.name for a in db.scalars(insert(Author).values(
        [{"name": select(Author.name).where(Author.id == 2)}]).returning(Author))]
    loaded = db.scalars(select(Author).where(Author.name == "B1")).one()
    u = db.execute(update(Author), [{"id": loaded.id, "name": "B1!"}, {"id": made[0].id, "active": False}])
    out["update_result"] = [u.all(), u.first()]
    out["synced"] = [loaded.name, made[0].active]
    try:
        db.execute(update(Author), [{"name": "no key"}])
    except Exception as e:  # noqa: BLE001
        out["nokey"] = type(e).__name__
    db.commit()
    out["names"] = db.scalars(select(Author.name).where(Author.name.like("B%")).order_by(Author.id)).all()
    try:
        db.scalars(insert(Author).returning(Author).execution_options(render_nulls=True), [{"name": "B9", "active": None}])
    except Exception as e:  # noqa: BLE001
        out["render_nulls"] = type(e).__name__
        db.rollback()
    return out


@app.post("/own-session")
def own_session():
    """Session(engine) in the endpoint: its `with` block closes it (rollback, objects detached)."""
    with Session(engine) as s:
        a = Author(name="Own", active=True)
        s.add(a)
        s.commit()
        out = {"name": a.name, "is": isinstance(s, Session)}
        a.name = "Own (not committed)"
    try:
        a.books
    except DetachedInstanceError as e:
        out["detached"] = type(e).__name__
    with Session(engine, expire_on_commit=False) as s:
        out["kept"] = s.scalars(select(Author.name).where(Author.name.like("Own%"))).all()
        b = s.get(Author, a.id)
        s.close()
        out["closed"] = [b.name, b in s]
        out["again"] = s.scalar(select(Author.name).where(Author.id == a.id))
    return out


@app.post("/hybrid")
def hybrid(db: Db):
    """sqlalchemy.ext.hybrid: hybrid_property (with an expression and a setter), hybrid_method."""
    books = db.scalars(select(Book).where(Book.pages.is_not(None)).order_by(Book.id)).all()
    out = {"instance": [[b.double, b.longer_than(10), b.label] for b in books]}
    out["select"] = [list(r) for r in db.execute(select(Book.id, Book.double, Book.label).where(Book.pages.is_not(None))
                                                  .order_by(Book.id))]
    out["where"] = db.scalars(select(Book.title).where(Book.longer_than(10), Book.double > 0).order_by(Book.label)).all()
    # select(...).label(): a labeled scalar subquery, the label dropped where it is an operand
    nb = select(func.count(Book.id)).where(Book.author_id == Author.id).label("nb")
    out["label"] = [list(r) for r in db.execute(select(Author.name, nb).where(nb > 0, Author.id.label("i") < 100)
                                                .order_by(Author.id))]
    if books:
        books[0].label = "Renamed!"
        db.flush()
        out["setter"] = db.scalar(select(Book.title).where(Book.id == books[0].id))
        try:
            books[0].double = 3
        except AttributeError as e:
            out["no_setter"] = str(e)
        db.rollback()
    return out


from .models import Order  # noqa: E402


@app.post("/reserved")
def reserved(db: Db):
    """A table named `order`, columns `end` and `user` (reserved words in PostgreSQL)."""
    o = Order(end=3, user="u", author=db.get(Author, 1))
    db.add_all([o, Order(end=1)])
    db.commit()
    o.end = 4
    db.flush()
    out = {"rows": [[r.end, r.user] for r in db.scalars(select(Order).where(Order.end > 0).order_by(Order.end))]}
    out["join"] = db.scalars(select(Author.name).join(Order, Order.author_id == Author.id).where(Order.user == "u")).all()
    out["eager"] = [x.author.name if x.author else None for x in db.scalars(
        select(Order).options(selectinload(Order.author)).order_by(Order.id))]
    db.execute(update(Order).where(Order.end == 1).values(user="x"))
    db.delete(o)
    db.commit()
    out["left"] = [[r.end, r.user] for r in db.scalars(select(Order))]
    return out


from .models import LazyStamp, Stamp  # noqa: E402


@app.post("/eager-defaults")
def eager_defaults(db: Db):
    """eager_defaults=True loads the SQL `onupdate=` value with the UPDATE; otherwise it is expired."""
    out = {}
    for cls in (Stamp, LazyStamp):
        s = cls(name="a")
        db.add(s)
        db.flush()
        s.name = "b"
        db.flush()
        db.expunge(s)
        try:
            out[cls.__tablename__] = [s.name, s.rev]
        except DetachedInstanceError as e:
            out[cls.__tablename__] = type(e).__name__


from sqlalchemy.exc import InvalidRequestError  # noqa: E402
from sqlalchemy.orm import defaultload, defer, load_only, undefer, undefer_group  # noqa: E402

from .models import Manuscript  # noqa: E402


def loaded(o) -> list:
    """the attributes loaded on a mapped object (its __dict__), without loading anything"""
    return sorted(k for k in o.__dict__ if not k.startswith("_"))


def read_secret(m) -> str:
    try:
        return m.secret
    except InvalidRequestError as e:
        return f"{type(e).__name__}: {e}"


@app.post("/defer")
def defer_columns(db: Db):
    """Column loading: deferred columns and groups, load_only / defer / undefer / undefer_group, raiseload."""
    db.add_all([Manuscript(title="A", body="ba", draft="da", secret="sa", notes="na", author_id=1),
                Manuscript(title="B", body="bb", secret="sb", author_id=2)])
    db.commit()
    db.close()
    out = {}
    ms = db.scalars(select(Manuscript).order_by(Manuscript.id)).all()
    out["mapping"] = loaded(ms[0])
    out["group"] = [ms[0].body, loaded(ms[0])]
    out["alone"] = [ms[0].notes, loaded(ms[0])]
    out["raise"] = read_secret(ms[0])
    db.close()
    m = db.scalars(select(Manuscript).options(load_only(Manuscript.title)).where(Manuscript.id == 1)).one()
    out["load_only"] = loaded(m)
    out["load_only_read"] = [m.author_id, loaded(m), m.draft, loaded(m)]
    db.close()
    m = db.scalar(select(Manuscript).options(defer(Manuscript.title, raiseload=True), undefer_group("content"))
                  .where(Manuscript.id == 2))
    out["defer"] = loaded(m)
    try:
        out["defer_read"] = m.title
    except InvalidRequestError as e:
        out["defer_read"] = type(e).__name__
    db.close()
    m = db.scalar(select(Manuscript).options(undefer("*")).where(Manuscript.id == 1))
    out["star"] = [loaded(m), m.secret]
    db.close()
    m = db.scalar(select(Manuscript).options(load_only(Manuscript.title, raiseload=True)).where(Manuscript.id == 1))
    for name in ("body", "author_id"):
        try:
            out[name] = getattr(m, name)
        except InvalidRequestError as e:
            out[name] = type(e).__name__
    db.close()
    m = db.scalar(select(Manuscript).options(undefer(Manuscript.secret), defer(Manuscript.author_id)).where(Manuscript.id == 2))
    out["undefer"] = [loaded(m), m.secret, m.author_id, loaded(m)]
    db.close()
    rows = db.execute(select(Manuscript, Author).join(Author, Manuscript.author_id == Author.id)
                      .options(load_only(Manuscript.title), load_only(Author.name)).order_by(Manuscript.id)).all()
    out["two"] = [[loaded(a), loaded(b)] for a, b in rows]
    db.close()
    ms = db.scalars(select(Manuscript).options(selectinload(Manuscript.author).load_only(Author.name))
                    .order_by(Manuscript.id)).all()
    out["selectin"] = [loaded(ms[0]), loaded(ms[0].author), ms[0].author.active, loaded(ms[0].author)]
    db.close()
    ms = db.scalars(select(Manuscript).options(defaultload(Manuscript.author).load_only(Author.name, raiseload=True))
                    .order_by(Manuscript.id)).all()
    out["default"] = [loaded(ms[1]), ms[1].author.name, loaded(ms[1].author)]
    try:
        out["default_raise"] = ms[1].author.active
    except InvalidRequestError as e:
        out["default_raise"] = type(e).__name__
    # a column loaded by a later query into an object already in the session
    m = db.get(Manuscript, 1)
    db.scalars(select(Manuscript).options(undefer(Manuscript.notes)).where(Manuscript.id == 1)).all()
    out["partial"] = loaded(m)
    m.title = "A2"
    db.commit()
    out["expired"] = [loaded(m), m.title, loaded(m), m.body, loaded(m), read_secret(m)]
    db.rollback()
    return out


from .models import Audit, Entry, Purse, Wallet  # noqa: E402


@app.post("/write-only")
def write_only(db: Db):
    """WriteOnlyMapped: pending members, add_all/remove, select(), bulk insert/update/delete bound to the parent."""
    out = {}
    w = Wallet(name="w", entries=[Entry(label="a", amount=5), Entry(label="b", amount=-3)])
    db.add(w)
    db.commit()
    out["type"] = type(w.entries).__name__
    w.entries.add_all([Entry(label="c", amount=10), Entry(label="d", amount=-8)])
    w.entries.add(Entry(label="e", amount=1))
    db.commit()
    neg = db.scalars(w.entries.select().where(Entry.amount < 0).limit(10)).all()
    out["neg"] = [[e.label, e.amount] for e in neg]
    w.entries.remove(neg[0])  # delete-orphan: the row goes
    db.commit()
    db.execute(w.entries.insert(), [{"label": "f", "amount": 7}, {"label": "g", "amount": -1}])
    got = db.scalars(w.entries.insert().returning(Entry), [{"label": "h", "amount": 100}]).all()
    out["returned"] = [[e.label, e.wallet_id] for e in got]
    a = Audit()
    db.add(a)
    a.entries.add_all(got + neg[1:])
    db.commit()
    db.execute(w.entries.update().values(amount=Entry.amount + 1000).where(Entry.amount == -1))
    db.execute(w.entries.delete().where(Entry.amount.between(0, 6)))
    db.execute(a.entries.update().values(label=Entry.label + " (audited)"))
    sub = a.entries.select().with_only_columns(Entry.id)
    out["sub"] = db.scalars(select(func.count()).select_from(Entry).where(Entry.id.in_(sub))).one()
    out["all"] = [[e.label, e.amount, e.wallet_id] for e in db.scalars(select(Entry).order_by(Entry.id))]
    out["ordered"] = [e.label for e in db.scalars(w.entries.select())]
    out["audited"] = [e.label for e in db.scalars(a.entries.select().order_by(Entry.id))]
    p = Purse()
    db.add(p)
    db.flush()
    x = Entry(label="p", amount=2)
    p.entries.add(x)
    db.flush()
    p.entries.remove(x)  # no delete-orphan: wallet-less row, foreign key NULL
    db.flush()
    out["purse"] = [x.purse_id, db.scalar(select(func.count()).select_from(Entry).where(Entry.purse_id == p.id))]
    db.delete(w)  # passive_deletes: ON DELETE CASCADE removes the entries
    db.commit()
    out["left"] = db.scalars(select(Entry.label).order_by(Entry.id)).all()
    w3 = Wallet(name="z")
    db.add(w3)
    db.flush()
    try:
        w3.entries = [Entry(label="q", amount=1)]
        db.flush()
        out["assign"] = db.scalars(select(Entry.label).where(Entry.wallet_id == w3.id)).all()
    except Exception as e:
        out["assign"] = [type(e).__name__, str(e)]
    try:
        db.scalars(select(Wallet).options(selectinload(Wallet.entries))).all()
    except Exception as e:
        out["eager"] = [type(e).__name__, str(e)]
    try:
        a.entries.insert()
    except Exception as e:
        out["m2m_insert"] = [type(e).__name__, str(e)]
    db.rollback()


from sqlalchemy import bindparam, delete  # noqa: E402
from sqlalchemy.exc import StatementError  # noqa: E402

from .models import events  # noqa: E402


def outcome(f):
    try:
        r = f()
        return [r.rowcount, [list(x) for x in r] if r.returns_rows else None]
    except (InvalidRequestError, StatementError) as e:
        return type(e).__name__ + ": " + str(e).split("\n")[0]


@app.post("/core")
def core_table(db: Db):
    """A Core Table(): statements on it, bindparam(), executemany with a list of dicts, aliases, t.c[...]."""
    out = {}
    with engine.connect() as conn:
        out["one"] = conn.execute(insert(events).values(kind="a", note="first")).rowcount
        out["many"] = conn.execute(insert(events), [{"kind": "b", "note": None}, {"kind": "c", "note": "x"}]).rowcount
        out["uneven"] = outcome(lambda: conn.execute(insert(events), [{"kind": "d", "note": None}, {"kind": "e"}]))
        out["extra"] = conn.execute(insert(events), [{"kind": "f"}, {"kind": "c", "note": "ignored"}]).rowcount
        conn.commit()
    with engine.begin() as conn:
        r = conn.execute(update(events).where(events.c.kind == bindparam("k")).values(note=bindparam("n")),
                         [{"k": "a", "n": "A"}, {"k": "b", "n": "B"}, {"k": "zz", "n": "Z"}])
        out["update_many"] = r.rowcount
        out["update_extra"] = conn.execute(update(events).where(events.c.kind == bindparam("k")), [{"k": "c", "note": "C"}]).rowcount
    out["select"] = [list(r) for r in db.execute(select(events).order_by(events.c.id))]
    out["cols"] = [list(r) for r in db.execute(select(events.c["kind", "note"]).order_by(events.c.kind.desc()))]
    out["name"] = [events.name, events.c.kind.name, str(events.c.note)]
    a1, a2 = events.alias(), events.alias()
    out["alias"] = [list(r) for r in db.execute(select(a1.c.kind, a2.c.kind).join_from(a1, a2, a1.c.id < a2.c.id)
                                                .order_by(a1.c.id, a2.c.id))]
    out["param"] = db.execute(select(events.c.note).where(events.c.kind == bindparam("k")), {"k": "b"}).scalars().all()
    out["default"] = db.execute(select(events.c.kind).where(events.c.note == bindparam("n", "A"))).scalars().all()
    out["missing"] = outcome(lambda: db.execute(select(events.c.kind).where(events.c.kind == bindparam("k"))))
    db.execute(update(events).where(events.c.kind == "c").values(author_id=1))
    out["join"] = [list(r) for r in db.execute(select(events.c.kind, Author.name).join(Author, events.c.author_id == Author.id)
                                               .order_by(events.c.id))]
    out["method"] = [list(r) for r in db.execute(events.select().where(events.c.kind != "a").order_by(events.c.id))]
    out["orm_list"] = outcome(lambda: db.execute(update(Author).where(Author.id == bindparam("u")),
                                                 [{"u": 1, "active": False}]))
    a = db.get(Author, 1)
    out["orm_dict"] = outcome(lambda: db.execute(update(Author).where(Author.id == bindparam("u")), {"u": 1, "active": True}))
    out["conn_list"] = outcome(lambda: db.connection().execute(update(Author).where(Author.id == bindparam("i")),
                                                               [{"i": 1, "active": True}, {"i": 2, "active": True}]))
    out["after"] = [a.name, a.active]
    # insert().values(...) with a list: Core (a scalar subquery reading a bindparam()) and ORM (RETURNING, keys differ)
    who = select(Author.id).where(Author.id == bindparam("who")).scalar_subquery()
    out["values_core"] = db.execute(insert(events).values(author_id=who, note="v"), [{"who": 2, "kind": "g"},
                                                                                    {"who": 1, "kind": "h"}]).rowcount
    out["values_orm"] = [[o.title, o.pages, o.author_id] for o in db.scalars(
        insert(Book).values(author_id=2, summary="s").returning(Book), [{"title": "V1", "pages": 3}, {"title": "V2"}])]
    out["delete"] = outcome(lambda: db.execute(delete(events).where(events.c.kind == bindparam("k")), [{"k": "a"}, {"k": "b"}]))
    out["left"] = db.execute(select(events.c.kind)).scalars().all()
    db.rollback()
    with engine.begin() as conn:
        conn.execute(delete(events))
    return out


from sqlalchemy import delete  # noqa: E402

from .models import Boss, Coder, Lead, Staff  # noqa: E402


@app.post("/inherit/single")
def inherit_single(db: Db):
    """Single table inheritance: the discriminator, polymorphic loading, subclass queries, flush and bulk DML."""
    out = {}
    w = Wallet(name="w")
    b = Boss(name="b", budget=10, wallet=w)
    out["new"] = [b.kind, repr(b), isinstance(b, Staff), isinstance(b, Coder)]
    db.add_all([b, Coder(name="c", language="py", wallet=Wallet(name="w2")), Lead(name="l", language="rs", team="t"),
                Staff(name="s")])
    db.commit()
    # a joined subclass's identities are in the ON clause: an outer join keeps the wallets without a boss
    out["outer"] = [list(x) for x in db.execute(
        select(Wallet.name, Boss.name).outerjoin(Boss, Boss.wallet_id == Wallet.id).order_by(Wallet.name))]
    db.expunge_all()
    rows = db.scalars(select(Staff).order_by(Staff.id)).all()
    out["all"] = [[type(r).__name__, r.name, r.kind] for r in rows]
    out["lazy"] = [rows[0].budget, rows[2].team, rows[1].language]  # subclass columns read on access
    out["same"] = db.get(Staff, 1) is rows[0]
    db.expunge_all()
    out["coders"] = [[type(r).__name__, r.name, r.language] for r in db.scalars(select(Coder).order_by(Coder.id))]
    out["filter"] = db.scalars(select(Coder.name).where(Coder.language == "rs")).all()
    out["mixed"] = db.scalars(select(Boss.name).where(Staff.name == "b")).all()
    out["get"] = [type(db.get(Coder, 3)).__name__, db.get(Coder, 1), db.get(Boss, 1).budget]
    out["count"] = db.scalar(select(func.count()).select_from(Coder))
    c = db.get(Coder, 2)
    c.language = "go"
    c.name = "c2"
    db.commit()
    out["updated"] = [list(r) for r in db.execute(select(Staff.name, Staff.kind).order_by(Staff.id))]
    db.execute(update(Coder).values(name=Coder.name + "!"))
    out["bulk"] = db.scalars(select(Staff.name).order_by(Staff.id)).all()
    db.execute(delete(Lead))
    out["left"] = db.scalars(select(Staff.kind).order_by(Staff.id)).all()
    out["wallet"] = db.scalars(select(Boss)).one().wallet.name
    db.rollback()
    return out


from .models import Pair, Place, Point, Segment  # noqa: E402


@app.post("/composite")
def composites(db: Db):
    """composite(): build, select, compare, set; a dataclass and a class with __composite_values__."""
    out = {"transient": [repr(Segment().start), repr(Segment().end), repr(Place().spot)]}
    s1 = Segment(start=Point(3, 4), end=Point(5, 6), span=Pair(1, 2))
    db.add_all([s1, Segment(start=Point(7, 8), end=Point(9, 1)), Place(spot=Point(1, 1)), Place(lat=2, lon=None)])
    db.commit()
    out["rows"] = [repr(r) for r in db.execute(select(Segment.start, Segment.end).order_by(Segment.id)).all()]
    out["where"] = [repr(x) for x in db.scalars(select(Segment).where(Segment.start == Point(3, 4)).where(Segment.end < Point(7, 8)))]
    out["ne"] = db.scalars(select(Segment.id).where(Segment.start != Point(3, 4)).order_by(Segment.id)).all()
    out["none"] = db.scalars(select(Place.id).where(Place.spot == None)).all()  # noqa: E711
    out["pair"] = [repr(x.span) for x in db.scalars(select(Segment).where(Segment.span == Pair(1, 2)))]
    s = db.scalars(select(Segment).order_by(Segment.id)).first()
    p = s.start
    out["cached"] = [s.start is p, s.start == Point(3, 4), sorted(k for k in s.__dict__ if not k.startswith("_"))]
    s.end = Point(10, 14)
    out["set"] = [s.x2, s.y2, repr(s.end)]
    db.commit()
    out["after"] = [sorted(k for k in s.__dict__ if not k.startswith("_")), repr(s.end), s.start is p]
    out["places"] = [repr(x.spot) for x in db.scalars(select(Place).order_by(Place.id))]
    db.rollback()
    return out


from sqlalchemy.orm import with_parent  # noqa: E402


@app.post("/joins")
def joins(db: Db):
    """Relationship joins: of_type(alias), and_(criteria), from an alias; add_columns(); with_parent(); named aliases."""
    bk, au = aliased(Book, name="bk"), aliased(Author, name="au")
    out = {"of_type": [list(r) for r in db.execute(select(Author.name, bk.title).join(Author.books.of_type(bk)).order_by(bk.id))]}
    out["and_"] = db.scalars(select(Author.name).join(Author.books.and_(Book.pages > 5)).order_by(Author.id)).all()
    out["alias_rel"] = [list(r) for r in db.execute(select(au.name, bk.title).join(au.books.of_type(bk).and_(bk.pages == None))  # noqa: E711
                                                    .order_by(bk.id))]
    out["add_columns"] = [[a.name, b.title] for a, b in db.execute(select(Author).join(Author.books).add_columns(Book)
                                                                   .order_by(Author.id, Book.id))]
    a1 = db.get(Author, 1)
    out["with_parent"] = db.scalars(select(Book.title).where(with_parent(a1, Author.books)).order_by(Book.id)).all()
    row = db.execute(select(au, bk).join(au.books.of_type(bk)).order_by(bk.id)).first()
    out["row_keys"] = [row.au.name, row.bk.title, list(row._mapping.keys())]
    b1 = db.get(Book, 1)
    out["with_parent_m2o"] = db.scalars(select(Author.name).where(with_parent(b1, Book.author))).all()
    return out

from .models import Admin, Guest, Member, Root, Ticket  # noqa: E402


@app.post("/inherit/joined")
def inherit_joined(db: Db):
    """Joined table inheritance: one INSERT/UPDATE/DELETE per table, polymorphic loading, subclass queries."""
    out = {}
    a = Admin(name="a", level=1, tickets=[Ticket(title="t1"), Ticket(title="t2")])
    db.add_all([a, Guest(name="g", note="n"), Root(name="r", level=9, shell="sh", tickets=[Ticket(title="t3")]), Member(name="m")])
    db.commit()
    out["ids"] = [a.id, a.kind]
    db.expunge_all()
    rows = db.scalars(select(Member).order_by(Member.id)).all()
    out["all"] = [[type(r).__name__, r.name] for r in rows]
    out["lazy"] = [rows[0].level, rows[1].note, rows[2].shell, rows[2].level]
    db.expunge_all()
    out["admins"] = [[type(r).__name__, r.name, r.level] for r in db.scalars(select(Admin).order_by(Admin.id))]
    out["where"] = db.scalars(select(Admin.name).where(Admin.level > 5, Member.name != "x")).all()
    out["get"] = [repr(db.get(Admin, 3)), db.get(Admin, 2), db.get(Member, 1).level]
    out["tickets"] = [t.title for t in db.get(Admin, 1).tickets]
    out["join"] = db.scalars(select(Ticket.title).join(Admin).where(Admin.name == "a").order_by(Ticket.id)).all()
    out["of_type"] = [list(x) for x in db.execute(select(Ticket.title, Root.shell).join(Ticket.admin.of_type(Root)))]
    out["of_type_admin"] = db.scalars(select(Ticket.title).join(Ticket.admin.of_type(Admin)).order_by(Ticket.id)).all()
    r = db.get(Root, 3)
    r.name, r.level, r.shell = "r2", 10, "zsh"
    db.flush()
    out["updated"] = [r.stamp, db.execute(text("SELECT name, stamp FROM members WHERE id = 3")).one()[0]]
    db.execute(update(Admin).where(Admin.id == 1).values(level=Admin.level + 100))
    db.execute(update(Admin).where(Admin.id == Member.id, Member.name == "r2").values(level=0))
    out["bulk"] = len(db.execute(text("SELECT id, level FROM admins ORDER BY id")).all())
    out["levels"] = [list(x) for x in db.execute(text("SELECT id, level FROM admins ORDER BY id"))]
    db.delete(db.get(Guest, 2))
    db.flush()
    out["guests"] = [db.scalar(text("SELECT count(*) FROM guests")), db.scalar(text("SELECT count(*) FROM members"))]
    db.execute(delete(Ticket))
    db.execute(delete(Root).where(Root.id == 3))
    out["roots"] = [db.scalar(text("SELECT count(*) FROM roots")), db.scalar(text("SELECT count(*) FROM admins"))]
    got = db.scalars(insert(Admin).returning(Admin), [{"name": "b1", "level": 4}, {"name": "b2", "level": 5, "stamp": None}]).all()
    out["bulk_ret"] = [[type(x).__name__, x.name, x.kind, x.level, x.id] for x in got]
    db.execute(insert(Guest), [{"name": "g2"}, {"name": "g3", "note": "z"}])
    out["bulk_rows"] = [list(x) for x in db.execute(text(
        "SELECT m.name, m.kind, g.note FROM members m JOIN guests g ON g.id = m.id ORDER BY m.id"))]


from typing import cast  # noqa: E402

from .models import Interval  # noqa: E402


@app.post("/init")
def own_init(db: Db):
    """A mapped class's own __init__; typing.cast()."""
    db.add_all([Interval(5, 10), Interval(3), Interval(start=1, end=4)])
    db.flush()
    out = {"rows": [[i.start, i.end, i.length] for i in db.scalars(select(Interval).order_by(Interval.id))]}
    out["long"] = db.scalars(select(Interval.start).where(cast("int", Interval.length) > 2).order_by(Interval.id)).all()
    out["filter_by"] = db.scalars(select(Interval.start).filter_by(length=5)).all()
    try:
        db.scalars(select(Interval).filter_by(width=5))
    except InvalidRequestError as e:
        out["filter_by_bad"] = str(e)
    try:
        Interval(1, 2, 3)
    except TypeError as e:
        out["error"] = type(e).__name__
    db.rollback()
    return out


import dataclasses  # noqa: E402

from .models import Box, Framed  # noqa: E402


@app.post("/composite/factory")
def composite_factory(db: Db):
    """composite(Cls.factory, ...) with __composite_values__; dataclasses.astuple / asdict."""
    b = Box(Point(1, 2), Point(3, 4))
    db.add_all([Framed(box=b), Framed(box=Box(Point(5, 6), Point(7, 8)))])
    db.commit()
    f = db.scalars(select(Framed).where(Framed.box == Box(Point(1, 2), Point(3, 4)))).one()
    out = {"box": repr(f.box), "cols": [f.x1, f.y1, f.x2, f.y2]}
    out["astuple"] = list(dataclasses.astuple(b))
    out["asdict"] = dataclasses.asdict(b)
    out["rows"] = [repr(r.box) for r in db.execute(select(Framed.box).order_by(Framed.id))]
    try:
        dataclasses.astuple(f)
    except TypeError as e:
        out["error"] = str(e)
    db.rollback()
    return out


from .models import Manuscript, Segment  # noqa: E402


@app.post("/expire")
def expire_one(db: Db):
    """session.expire(obj) and expire(obj, [names]) in a synchronous session: reloaded on access, changes of the
    expired attributes not flushed are lost."""
    k = lambda o: sorted(x for x in o.__dict__ if not x.startswith("_"))  # noqa: E731
    out = {}
    a = db.get(Author, 1)
    books = list(a.books)
    a.name = "lost"
    db.expire(a)
    out["whole"] = [k(a), a.name, k(a)]
    b = books[0]
    b.title, b.pages = "lost", 777
    db.expire(b, ["title"])
    out["partial"] = [k(b), b.title, b.pages, k(b)]
    db.flush()
    out["db"] = list(db.execute(text("SELECT title, pages FROM books WHERE id = :i"), {"i": b.id}).one())
    db.expire(a, ["books"])
    out["rel"] = [k(a), len(a.books), "books" in a.__dict__]
    db.expire(a, ["id"])
    out["pk"] = [k(a), a.id, k(a)]
    m = Manuscript(title="m", body="b", draft="d", secret="s", notes="n")
    db.add(m)
    db.flush()
    db.expunge(m)  # (from a load: __dict__ right after the INSERT is not compared)
    m = db.get(Manuscript, m.id)
    m.body, m.notes
    db.expire(m, ["notes", "body"])
    out["deferred"] = [k(m), m.title, k(m), m.notes, k(m), m.body, k(m)]
    s = Segment(start=Point(1, 2), end=Point(3, 4))
    db.add(s)
    db.flush()
    db.expunge(s)
    s = db.get(Segment, s.id)
    e = s.end
    s.x1 = 50
    db.expire(s, ["x1"])
    out["composite_col"] = [k(s), repr(s.start), k(s)]
    db.expire(s, ["start"])
    out["composite"] = [k(s), repr(s.start), k(s), s.end is e]
    gone = Author(name="gone")
    db.add(gone)
    db.flush()
    gid = gone.id
    db.expire(gone, ["id"])
    db.execute(text("DELETE FROM authors WHERE id = :i"), {"i": gid})
    try:
        gone.id
    except Exception as e:  # noqa: BLE001
        msg = str(e)
        out["gone"] = [type(e).__name__, msg[: msg.index(" at 0x")] if " at 0x" in msg else msg]
    db.rollback()
    return out


def _keys(o) -> list[str]:
    return sorted(k for k in o.__dict__ if not k.startswith("_"))


@app.post("/idict")
def idict(db: Db):
    """`__dict__` after an INSERT in a synchronous session: columns never set stay out, defaults and foreign keys
    come in, back references set; then everything expired by the commit."""
    a = Author(name="idict")
    b = Book(title="t")
    r = Review(stars=3)
    a.books = [b]
    b.reviews.append(r)
    db.add(a)
    db.flush()
    out = {"a": _keys(a), "b": _keys(b), "r": _keys(r), "back": [b.author is a, r.book is b], "read": [b.pages, a.mentor_id]}
    out["after_read"] = [_keys(a), _keys(b)]
    db.commit()
    out["committed"] = [_keys(a), _keys(b), _keys(r)]
    return out


OtherDb = Annotated[Session, Depends(get_other_db)]


@app.post("/other/authors/{name}")
def other_author(name: str, db: OtherDb, fail: int = 0):
    """sessions of `get_other_db` (fixtures/syncapp/db.py): committed by the endpoint, else rolled back"""
    db.add(Author(name=name))
    db.flush()
    if fail == 1:
        raise HTTPException(status_code=409, detail="rolled back")
    if fail == 2:
        raise ValueError("rolled back too")
    db.commit()
    return {"count": db.query(Author).filter(Author.name == name).count()}


@app.get("/other/authors/{name}")
def other_count(name: str, db: OtherDb):
    return {"name": name, "count": db.query(Author).filter(Author.name == name).count()}

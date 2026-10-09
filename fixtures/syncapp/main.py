"""fixtures/syncapp: synchronous SQLAlchemy sessions (lazy loading, expiry on commit) in sync endpoints."""
from typing import Annotated

from fastapi import Depends, FastAPI, HTTPException
from pydantic import BaseModel
from sqlalchemy.orm import Session

from .db import get_db
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

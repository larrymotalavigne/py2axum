from typing import Annotated

from fastapi import APIRouter, HTTPException, Query, Response, status
from sqlalchemy import func, select
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import selectinload

from ..db import DbDep
from ..models import Book, Review
from ..schemas import BookDetail, BookIn, BookOut, BookPatch, ReviewIn, ReviewOut, Stats, Status
from ..security import CurrentUser

router = APIRouter(prefix="/books", tags=["books"])


async def load_book(db: DbDep, book_id: int, with_reviews: bool = False) -> Book:
    stmt = select(Book).where(Book.id == book_id)
    if with_reviews:
        stmt = stmt.options(selectinload(Book.reviews).joinedload(Review.user))
    book = (await db.execute(stmt)).unique().scalar_one_or_none()
    if book is None:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=f"book {book_id} not found")
    return book


def own(book: Book, user) -> None:
    if book.owner_id != user.id:
        raise HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail="only the owner can change this book")


def detail(book: Book) -> BookDetail:
    out = BookDetail.model_validate(book)
    if book.reviews:
        out.average_rating = round(sum(r.rating for r in book.reviews) / len(book.reviews), 2)
    return out


@router.get("", response_model=list[BookOut])
async def list_books(
    db: DbDep,
    q: Annotated[str | None, Query(max_length=100, description="searched in titles and authors")] = None,
    status_: Annotated[Status | None, Query(alias="status")] = None,
    tag: str | None = None,
    limit: Annotated[int, Query(ge=1, le=100)] = 20,
    offset: Annotated[int, Query(ge=0)] = 0,
):
    stmt = select(Book).order_by(Book.id)
    if q:
        pattern = f"%{q}%"
        stmt = stmt.where(Book.title.ilike(pattern) | Book.author.ilike(pattern))
    if status_ is not None:
        stmt = stmt.where(Book.status == status_.value)
    if tag:
        stmt = stmt.where(Book.tags.contains([tag.lower()]))
    return (await db.execute(stmt.limit(limit).offset(offset))).scalars().all()


@router.post("", response_model=BookOut, status_code=status.HTTP_201_CREATED)
async def create_book(payload: BookIn, db: DbDep, user: CurrentUser):
    book = Book(**payload.model_dump(mode="json"), owner=user)
    db.add(book)
    await db.flush()
    await db.refresh(book)
    return book


@router.get("/stats", response_model=Stats)
async def stats(db: DbDep):
    rows = (await db.execute(select(Book.status, func.count()).group_by(Book.status).order_by(Book.status))).all()
    count, average = (await db.execute(select(func.count(Review.id), func.avg(Review.rating)))).one()
    return Stats(
        books=sum(n for _, n in rows),
        by_status={s.value: 0 for s in Status} | {s: n for s, n in rows},
        reviews=count,
        average_rating=None if average is None else round(float(average), 2),
    )


@router.get("/{book_id}", response_model=BookDetail)
async def get_book(book_id: int, db: DbDep):
    return detail(await load_book(db, book_id, with_reviews=True))


@router.patch("/{book_id}", response_model=BookOut)
async def update_book(book_id: int, payload: BookPatch, db: DbDep, user: CurrentUser):
    book = await load_book(db, book_id)
    own(book, user)
    for field, value in payload.model_dump(mode="json", exclude_unset=True).items():
        if value is None and field in ("title", "author", "status", "tags"):
            raise HTTPException(status_code=status.HTTP_422_UNPROCESSABLE_CONTENT, detail=f"{field} cannot be null")
        setattr(book, field, value)
    await db.flush()
    return book


@router.delete("/{book_id}", status_code=status.HTTP_204_NO_CONTENT)
async def delete_book(book_id: int, db: DbDep, user: CurrentUser):
    book = await load_book(db, book_id)
    own(book, user)
    await db.delete(book)
    return Response(status_code=status.HTTP_204_NO_CONTENT)


@router.post("/{book_id}/reviews", response_model=ReviewOut, status_code=status.HTTP_201_CREATED)
async def add_review(book_id: int, payload: ReviewIn, db: DbDep, user: CurrentUser):
    book = await load_book(db, book_id)
    if book.owner_id == user.id:
        raise HTTPException(status_code=status.HTTP_400_BAD_REQUEST, detail="you cannot review your own book")
    review = Review(book_id=book.id, user=user, **payload.model_dump())
    db.add(review)
    try:
        await db.flush()
    except IntegrityError:
        raise HTTPException(status_code=status.HTTP_409_CONFLICT, detail="you already reviewed this book")
    await db.refresh(review)
    return review

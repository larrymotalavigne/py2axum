"""A user's shelf as a zipped CSV file. `zipfile` is not in py2axum's library map: this route is the one left to
Python (`--python-side auto`), the binary relays it to the Python process."""
import csv
import io
import zipfile

from fastapi import APIRouter, Response
from sqlalchemy import select

from ..db import DbDep
from ..models import Book
from ..security import CurrentUser

router = APIRouter(tags=["books"])


@router.get("/books/export.zip")
async def export_books(db: DbDep, user: CurrentUser):
    books = (await db.execute(select(Book).where(Book.owner_id == user.id).order_by(Book.id))).scalars().all()
    text = io.StringIO()
    writer = csv.writer(text)
    writer.writerow(["id", "title", "author", "year", "status", "tags"])
    for book in books:
        writer.writerow([book.id, book.title, book.author, book.year or "", book.status, " ".join(book.tags)])
    data = io.BytesIO()
    with zipfile.ZipFile(data, "w", zipfile.ZIP_DEFLATED) as archive:
        # a fixed date keeps the archive reproducible
        archive.writestr(zipfile.ZipInfo("books.csv", date_time=(2026, 1, 1, 0, 0, 0)), text.getvalue())
    return Response(
        data.getvalue(),
        media_type="application/zip",
        headers={"content-disposition": f'attachment; filename="bookshelf-{user.id}.zip"'},
    )

"""bookshelf: a small reading-list API, the example application of py2axum (see ../README.md).

Users register and log in (bcrypt, JWT Bearer tokens), add books to their shelf, review the books of others,
and follow a book's ratings over a WebSocket.
"""
from fastapi import FastAPI
from fastapi.middleware.gzip import GZipMiddleware

from .routers import auth, books, export, live

app = FastAPI(title="bookshelf", version="1.0.0")
app.add_middleware(GZipMiddleware, minimum_size=1000)

app.include_router(auth.router)
# before books: /books/{book_id} would take /books/export.zip
app.include_router(export.router)
app.include_router(books.router)
app.include_router(live.router)


@app.get("/health")
async def health() -> dict:
    return {"status": "ok"}

"""A WebSocket per book: the client authenticates with `?token=`, gets the book's rating summary, then sends
commands (`ping`, `summary`, `review`) and gets one JSON message back for each."""
from fastapi import APIRouter, WebSocket, WebSocketDisconnect, status
from pydantic import ValidationError
from sqlalchemy import func, select
from sqlalchemy.exc import IntegrityError

from ..db import SessionLocal
from ..models import Book, Review
from ..schemas import ReviewIn
from ..security import user_id_from_token

router = APIRouter()


async def summary(book_id: int) -> dict:
    async with SessionLocal() as db:
        count, average = (await db.execute(
            select(func.count(Review.id), func.avg(Review.rating)).where(Review.book_id == book_id)
        )).one()
    return {"type": "summary", "book_id": book_id, "reviews": count,
            "average_rating": None if average is None else round(float(average), 2)}


async def add_review(book_id: int, user_id: int, data: dict) -> dict:
    try:
        review = ReviewIn.model_validate(data)
    except ValidationError as exc:
        return {"type": "error", "errors": [{"loc": list(e["loc"]), "msg": e["msg"]} for e in exc.errors()]}
    async with SessionLocal() as db:
        book = await db.get(Book, book_id)
        if book.owner_id == user_id:
            return {"type": "error", "errors": [{"loc": [], "msg": "you cannot review your own book"}]}
        db.add(Review(book_id=book_id, user_id=user_id, rating=review.rating, body=review.body))
        try:
            await db.commit()
        except IntegrityError:
            return {"type": "error", "errors": [{"loc": [], "msg": "you already reviewed this book"}]}
    return await summary(book_id)


@router.websocket("/ws/books/{book_id}")
async def book_feed(websocket: WebSocket, book_id: int, token: str = ""):
    user_id = user_id_from_token(token)
    if user_id is None:
        # before accept(): the client gets an HTTP 403
        await websocket.close(code=status.WS_1008_POLICY_VIOLATION)
        return
    async with SessionLocal() as db:
        exists = await db.get(Book, book_id) is not None
    if not exists:
        await websocket.close(code=status.WS_1008_POLICY_VIOLATION)
        return
    await websocket.accept()
    await websocket.send_json(await summary(book_id))
    try:
        while True:
            message = await websocket.receive_json()
            kind = message.get("type") if isinstance(message, dict) else None
            if kind == "ping":
                await websocket.send_json({"type": "pong"})
            elif kind == "summary":
                await websocket.send_json(await summary(book_id))
            elif kind == "review":
                await websocket.send_json(await add_review(book_id, user_id, message))
            else:
                await websocket.close(code=4400, reason=f"unknown message type: {kind!r}")
                return
    except WebSocketDisconnect:
        pass

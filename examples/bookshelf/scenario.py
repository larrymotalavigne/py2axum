"""Conformance scenario of the bookshelf example: the same requests go to FastAPI and to the binary, and every
response must be identical (status, headers that matter, body byte for byte after masking).

    python tests/conformance.py http://127.0.0.1:9050 http://127.0.0.1:9090 --scenario examples/bookshelf/scenario.py

Access tokens handed out by /auth/login carry the time of the request, so they differ between the two servers:
`normalize` masks them, and the authenticated steps use tokens signed here with fixed dates (the servers'
default secret, BOOKSHELF_SECRET unset).
"""
from datetime import UTC, datetime

import jwt
import psycopg

SECRET = "change-me-in-production-0123456789"
SETTLE = 0.05  # FastAPI commits the session after sending the response: let it land before the next request


def token(sub, exp=datetime(2100, 1, 1, tzinfo=UTC), key=SECRET, **claims) -> str:
    claims = {"sub": str(sub), "iat": datetime(2026, 1, 1, tzinfo=UTC), **claims}
    if exp is not None:
        claims["exp"] = exp
    return jwt.encode(claims, key, algorithm="HS256")


def auth(sub, **kw) -> dict:
    return {"authorization": f"Bearer {token(sub, **kw)}"}


ADA, BOB, CAROL = auth(1), auth(2), auth(3)
PW = "correct horse battery"

STEPS = [
    ("GET", "/health", None),
    # --- registration and log-in
    ("POST", "/auth/register", {"email": "ada@example.org", "password": PW, "display_name": "Ada"}),
    ("POST", "/auth/register", {"email": "bob@example.org", "password": PW, "display_name": "Bob"}),
    ("POST", "/auth/register", {"email": "carol@example.org", "password": PW, "display_name": "Carol"}),
    ("POST", "/auth/register", {"email": "ADA@example.org", "password": PW, "display_name": "Ada again"}),
    ("POST", "/auth/register", {"email": "not-an-email", "password": "short", "display_name": ""}),
    ("POST", "/auth/register", {}),
    ("POST", "/auth/register", b'{"email": "x@example.org",'),
    ("POST", "/auth/login", {"email": "Ada@Example.org", "password": PW}),
    ("POST", "/auth/login", {"email": "ada@example.org", "password": "wrong password"}),
    ("POST", "/auth/login", {"email": "nobody@example.org", "password": PW}),
    ("POST", "/auth/login", {"email": "ada@example.org"}),
    # --- Bearer authentication
    ("GET", "/me", None),
    ("GET", "/me", None, ADA),
    ("GET", "/me", None, {"authorization": "Bearer garbage"}),
    ("GET", "/me", None, {"authorization": "Basic YWRhOnB3"}),
    ("GET", "/me", None, auth(1, exp=datetime(2001, 1, 1, tzinfo=UTC))),
    ("GET", "/me", None, auth(1, key="another secret, 32 bytes or more...")),
    ("GET", "/me", None, auth(1, exp=None)),
    ("GET", "/me", None, auth(99)),
    ("GET", "/me", None, auth("ada")),
    # --- books: create, validate
    ("POST", "/books", {"title": "Dune", "author": "Frank Herbert"}),
    ("POST", "/books", {"title": "  Dune ", "author": "Frank Herbert", "year": 1965, "tags": ["SF", " classic", "sf"]}, ADA),
    ("POST", "/books", {"title": "Foundation", "author": "Isaac Asimov", "year": 1951, "status": "done", "tags": ["sf"]}, ADA),
    ("POST", "/books", {"title": "Middlemarch", "author": "George Eliot", "year": 1871, "status": "reading"}, BOB),
    ("POST", "/books", {"title": "The Left Hand of Darkness", "author": "Ursula K. Le Guin", "year": 1969,
                        "tags": ["SF", "Classic"]}, BOB),
    ("POST", "/books", {"title": "Emma", "author": "Jane Austen", "year": "1815"}, CAROL),
    ("POST", "/books", {"title": "   ", "author": "x", "year": -1, "status": "lost", "tags": [str(i) for i in range(11)]}, ADA),
    ("POST", "/books", {"title": 5, "author": None, "year": 1.5, "tags": "sf"}, ADA),
    ("POST", "/books", [], ADA),
    # --- books: read
    ("GET", "/books", None),
    ("GET", "/books?q=le", None),
    ("GET", "/books?status=done", None),
    ("GET", "/books?status=lost", None),
    ("GET", "/books?tag=SF", None),
    ("GET", "/books?limit=2&offset=1", None),
    ("GET", "/books?limit=0&offset=-1", None),
    ("GET", "/books?limit=abc", None),
    ("GET", "/books/1", None),
    ("GET", "/books/999", None),
    ("GET", "/books/abc", None),
    # --- reviews (a relationship, a unique constraint)
    ("POST", "/books/1/reviews", {"rating": 5, "body": "A classic."}, BOB),
    ("POST", "/books/1/reviews", {"rating": 4}, CAROL),
    ("POST", "/books/1/reviews", {"rating": 3}, BOB),
    ("POST", "/books/1/reviews", {"rating": 5}, ADA),
    ("POST", "/books/3/reviews", {"rating": 0}, ADA),
    ("POST", "/books/3/reviews", {"rating": 6, "body": "x" * 2001}, ADA),
    ("POST", "/books/999/reviews", {"rating": 3}, BOB),
    ("GET", "/books/1", None),
    # --- partial updates, ownership
    ("PATCH", "/books/1", {"status": "done"}, BOB),
    ("PATCH", "/books/1", {"status": "reading", "tags": ["Space", "space ", "opera"]}, ADA),
    ("PATCH", "/books/1", {}, ADA),
    ("PATCH", "/books/1", {"title": None}, ADA),
    ("PATCH", "/books/1", {"year": None}, ADA),
    ("PATCH", "/books/1", {"status": "lost", "year": 3000}, ADA),
    ("PATCH", "/books/999", {"status": "done"}, ADA),
    ("GET", "/books/stats", None),
    # --- left to Python (zipfile): relayed by the binary
    ("GET", "/books/export.zip", None),
    ("GET", "/books/export.zip", None, ADA),
    ("GET", "/books/export.zip", None, CAROL),
    ("POST", "/books/export.zip", None, ADA),
    # --- WebSocket: a token in the query, JSON messages, close codes
    ("WS", f"/ws/books/1?token={token(3)}", [("recv", 1), ("send", '{"type": "ping"}'), ("recv", 1),
                                             ("send", '{"type": "summary"}'), ("recv", 1), ("close",)]),
    ("WS", f"/ws/books/3?token={token(1)}", [("recv", 1), ("send", '{"type": "review", "rating": 9}'), ("recv", 1),
                                             ("send", '{"type": "review", "rating": 4, "body": "Slow but good."}'),
                                             ("recv", 1), ("send", '{"type": "review", "rating": 2}'), ("recv", 1),
                                             ("send", '{"type": "dance"}'), ("recv", 1)]),
    ("WS", f"/ws/books/1?token={token(1)}", [("recv", 1), ("send", '{"type": "review", "rating": 3}'), ("recv", 1),
                                             ("send", '["review"]'), ("recv", 1)]),
    ("WS", "/ws/books/1?token=garbage", []),
    ("WS", "/ws/books/1", []),
    ("WS", f"/ws/books/999?token={token(1)}", []),
    ("WS", f"/ws/books/1?token={token(2)}", [("recv", 1), ("send", "not json"), ("recv", 1)]),
    ("GET", "/books/stats", None),
    # --- delete (cascades to the reviews)
    ("DELETE", "/books/3", None, ADA),
    ("DELETE", "/books/3", None, BOB),
    ("DELETE", "/books/3", None, BOB),
    ("GET", "/books/3", None),
    ("GET", "/books", None),
    ("GET", "/books/stats", None),
]


def reset(db: str) -> None:
    with psycopg.connect(db.replace("postgresql+psycopg://", "postgresql://")) as conn:
        conn.execute("TRUNCATE users, books, reviews RESTART IDENTITY CASCADE")


def normalize(body):
    if isinstance(body, dict) and "access_token" in body:
        body = {**body, "access_token": "<token>"}
    return body

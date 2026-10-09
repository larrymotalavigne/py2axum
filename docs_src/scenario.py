"""Conformance scenario of the documentation's examples (docs_src/): the same requests go to FastAPI and to the
binary, and every response must be identical.

    python tests/conformance.py http://127.0.0.1:9150 http://127.0.0.1:9190 --scenario docs_src/scenario.py

One block of steps per page of the tutorial. The application's state that is not in the database (the
background notifications, the closed connections of the dependencies page) lives in each server's memory: both
start empty, and every pass restarts them.
"""
import base64

import psycopg

SETTLE = 0.05  # FastAPI commits the session (and runs background tasks) after sending the response


def form(**fields) -> tuple[bytes, dict]:
    body = "&".join(f"{k}={v}" for k, v in fields.items()).encode()
    return body, {"content-type": "application/x-www-form-urlencoded"}


def multipart(*parts) -> tuple[bytes, dict]:
    """parts: (name, value) for a field, (name, filename, content type, bytes) for a file."""
    out = b""
    for part in parts:
        out += b"--docs-boundary\r\n"
        if len(part) == 2:
            out += f'Content-Disposition: form-data; name="{part[0]}"\r\n\r\n{part[1]}\r\n'.encode()
        else:
            name, filename, ctype, data = part
            out += (f'Content-Disposition: form-data; name="{name}"; filename="{filename}"\r\n'
                    f"Content-Type: {ctype}\r\n\r\n").encode() + data + b"\r\n"
    return out + b"--docs-boundary--\r\n", {"content-type": "multipart/form-data; boundary=docs-boundary"}


def basic(user: str, password: str) -> dict:
    return {"authorization": "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()}


KEY = {"x-key": "fake-super-secret-key"}
ORIGIN = {"origin": "https://app.example.org"}
CUSTOMER = {"email": "ada@example.org", "name": "  Ada Lovelace ", "tags": ["Math", "math", "Engines"]}

STEPS = [
    ("GET", "/health", None),
    # --- parameters
    ("GET", "/parameters/items/42", None),
    ("GET", "/parameters/items/42?q=hello&short=true", None),
    ("GET", "/parameters/items/42?short=yes", None),
    ("GET", "/parameters/items/42?short=maybe", None),
    ("GET", "/parameters/items/forty-two", None),
    ("GET", "/parameters/shelves/done", None),
    ("GET", "/parameters/shelves/to-read", None),
    ("GET", "/parameters/shelves/lost", None),
    ("GET", "/parameters/search?q=rust", None),
    ("GET", "/parameters/search?q=rust%20books&tag=a&tag=b&limit=5&since=2026-10-09", None),
    ("GET", "/parameters/search?q=R", None),
    ("GET", "/parameters/search?q=ok&limit=0&since=yesterday", None),
    ("GET", "/parameters/search", None),
    ("GET", "/parameters/files/0d1f4a3c-7b1e-4c2a-9f0e-5a6b7c8d9e0f", None, {"x-token": "a"}),
    ("GET", "/parameters/files/0D1F4A3C7B1E4C2A9F0E5A6B7C8D9E0F", None),
    ("GET", "/parameters/files/not-a-uuid", None),
    # --- request body
    ("POST", "/body/items", {"name": "Foo", "price": 35.4}),
    ("POST", "/body/items", {"name": "Foo", "description": "A foo", "price": "35.4", "tax": 3.2}),
    ("POST", "/body/items", {"name": "Foo"}),
    ("POST", "/body/items", {"name": ["Foo"], "price": "cheap"}),
    ("POST", "/body/items", b'{"name": "Foo", "price": 1,}'),
    ("POST", "/body/items", b"not json"),
    ("POST", "/body/items", None),
    ("PUT", "/body/items/5?q=x", {"item": {"name": "Foo", "price": 1}, "user": {"username": "dave"}, "importance": 3}),
    ("PUT", "/body/items/5", {"item": {"name": "Foo", "price": 1}, "user": {"username": "dave"}, "importance": 0}),
    ("PUT", "/body/items/5", {"name": "Foo", "price": 1}),
    ("PUT", "/body/embedded/5", {"item": {"name": "Foo", "price": 1}}),
    ("PUT", "/body/embedded/5", {"name": "Foo", "price": 1}),
    # --- models
    ("POST", "/models/customers", CUSTOMER),
    ("POST", "/models/customers", {**CUSTOMER, "plan": "pro", "credit": "12.5", "referrerId": 7,
                                   "addresses": [{"city": "London", "country": "GB"}]}),
    ("POST", "/models/customers", {**CUSTOMER, "plan": "pro"}),
    ("POST", "/models/customers", {**CUSTOMER, "referrer_id": 3, "credit": 1.005}),
    ("POST", "/models/customers", {"email": "not-an-email", "name": "", "plan": "gold", "credit": "123456789",
                                   "addresses": [{"city": "Paris", "country": "FRA"}]}),
    ("POST", "/models/customers/dump", {**CUSTOMER, "referrerId": 7, "addresses": [{"city": "Oslo", "country": "NO"}]}),
    ("POST", "/models/customers/dump", {"email": "bob@example.org", "name": "Bob"}),
    # --- responses
    ("POST", "/responses/users", {"username": "ada", "password": "secret", "email": "ada@example.org"}),
    ("POST", "/responses/users", {"username": "ada"}),
    ("GET", "/responses/items/foo", None),
    ("GET", "/responses/items/bar", None),
    ("GET", "/responses/hello", None),
    ("GET", "/responses/page", None),
    ("GET", "/responses/old-hello", None),
    ("GET", "/responses/cookie", None),
    ("GET", "/responses/teapot", None),
    ("GET", "/responses/countdown", None),
    ("GET", "/responses/countdown?n=1", None),
    # --- errors
    ("GET", "/errors/items/foo", None),
    ("GET", "/errors/items/bar", None),
    ("GET", "/errors/unicorns/yolo", None),
    ("GET", "/errors/unicorns/sparkle", None),
    ("GET", "/errors/divide?a=1&b=2", None),
    ("GET", "/errors/divide?a=1&b=0", None),
    ("GET", "/errors/nowhere", None),
    ("POST", "/errors/items/foo", None),
    # --- dependencies
    ("GET", "/dependencies/items", None),
    ("GET", "/dependencies/items", None, {"x-key": "wrong"}),
    ("GET", "/dependencies/items?q=x&skip=1&limit=2", None, KEY),
    ("GET", "/dependencies/items?skip=one", None, KEY),
    ("GET", "/dependencies/fruits?skip=2", None, KEY),
    ("GET", "/dependencies/fruits?limit=-1", None, KEY),
    ("GET", "/dependencies/connection", None, KEY),
    ("GET", "/dependencies/connection", None, KEY),
    # --- security
    ("POST", "/security/token", *form(username="alice", password="secret")),
    ("POST", "/security/token", *form(username="alice", password="wrong")),
    ("POST", "/security/token", *form(username="alice")),
    ("GET", "/security/me", None),
    ("GET", "/security/me", None, {"authorization": "Bearer alice"}),
    ("GET", "/security/me", None, {"authorization": "Bearer bob"}),
    ("GET", "/security/me", None, {"authorization": "Bearer mallory"}),
    ("GET", "/security/me", None, {"authorization": "Basic YWxpY2U6c2VjcmV0"}),
    ("GET", "/security/basic", None),
    ("GET", "/security/basic", None, basic("alice", "secret")),
    ("GET", "/security/basic", None, basic("alice", "nope")),
    ("GET", "/security/basic", None, {"authorization": "Basic !!!"}),
    # --- SQL databases
    ("POST", "/sql/teams", {"name": "Preventers", "headquarters": "Sharp Tower"}),
    ("POST", "/sql/teams", {"name": "Z-Force", "headquarters": "Sister Margaret's Bar"}),
    ("POST", "/sql/teams", {"name": "Preventers", "headquarters": "Elsewhere"}),
    ("POST", "/sql/teams", {"name": "", "headquarters": 3}),
    ("POST", "/sql/heroes", {"name": "Deadpond", "team_id": 2}),
    ("POST", "/sql/heroes", {"name": "Rusty-Man", "age": 48, "team_id": 1}),
    ("POST", "/sql/heroes", {"name": "Spider-Boy", "age": 16}),
    ("POST", "/sql/heroes", {"name": "Tarantula", "age": 32, "team_id": 9}),
    ("POST", "/sql/heroes", {"name": "Black Lion", "age": -1}),
    ("GET", "/sql/heroes", None),
    ("GET", "/sql/heroes?offset=1&limit=1", None),
    ("GET", "/sql/heroes?name=MAN", None),
    ("GET", "/sql/heroes?limit=1000", None),
    ("GET", "/sql/heroes/2", None),
    ("GET", "/sql/heroes/3", None),
    ("GET", "/sql/heroes/99", None),
    ("PATCH", "/sql/heroes/3", {"age": 17, "team_id": 1}),
    ("PATCH", "/sql/heroes/3", {}),
    ("PATCH", "/sql/heroes/3", {"age": None}),
    ("PATCH", "/sql/heroes/99", {"age": 1}),
    ("GET", "/sql/teams/1", None),
    ("GET", "/sql/stats", None),
    ("DELETE", "/sql/heroes/1", None),
    ("DELETE", "/sql/heroes/1", None),
    ("GET", "/sql/teams/2", None),
    ("GET", "/sql/teams/7", None),
    ("GET", "/sql/stats", None),
    # --- forms and files
    ("POST", "/files/login", *form(username="ada", password="correct horse")),
    ("POST", "/files/login", *form(username="ada", password="short")),
    ("POST", "/files/login", {"username": "ada", "password": "correct horse"}),
    ("POST", "/files/upload", *multipart(("file", "notes.txt", "text/plain", b"one\ntwo\n"), ("note", "hi"))),
    ("POST", "/files/upload", *multipart(("file", "empty.csv", "text/csv", b""))),
    ("POST", "/files/upload", *multipart(("note", "no file"))),
    ("POST", "/files/uploads", *multipart(("files", "a.txt", "text/plain", b"a"), ("files", "b.bin", "application/octet-stream", b"\x00\x01"))),
    ("POST", "/files/raw", *multipart(("data", "x.bin", "application/octet-stream", b"12345"))),
    # --- background tasks
    ("GET", "/background/notifications", None),
    ("POST", "/background/send-notification/ada@example.org", None),
    ("POST", "/background/send-notification/bob@example.org", None),
    ("GET", "/background/notifications", None),
    # --- middleware and CORS (the headers are compared on every response)
    ("GET", "/health", None, {"x-request-id": "abc-123"}),
    ("GET", "/health", None, ORIGIN),
    ("GET", "/health", None, {"origin": "https://evil.example.com"}),
    ("OPTIONS", "/sql/heroes", None, {**ORIGIN, "access-control-request-method": "POST",
                                      "access-control-request-headers": "content-type"}),
    ("OPTIONS", "/sql/heroes", None, {**ORIGIN, "access-control-request-method": "DELETE"}),
    ("OPTIONS", "/sql/heroes", None, {"origin": "https://evil.example.com", "access-control-request-method": "GET"}),
    # --- WebSockets
    ("WS", "/ws/echo", [("send", "hello"), ("recv", 1), ("send", "again"), ("recv", 1), ("close",)]),
    ("WS", "/ws/rooms/lobby?token=letmein", [("send", '{"type": "say", "text": "hi"}'), ("recv", 1),
                                              ("send", '{"type": "bye"}'), ("recv", 1)]),
    ("WS", "/ws/rooms/lobby", []),
    ("WS", "/ws/rooms/lobby?token=letmein", [("send", "not json"), ("recv", 1)]),
]


def reset(db: str) -> None:
    with psycopg.connect(db.replace("postgresql+psycopg://", "postgresql://")) as conn:
        conn.execute("TRUNCATE heroes, teams RESTART IDENTITY CASCADE")

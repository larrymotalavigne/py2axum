"""Scenario of fixtures/bodyapp: Starlette's per-Request-object body cache (`request._body`) through two
BaseHTTPMiddlewares (one decompressing gzip bodies) and the exception handlers (route, 404/405, 500)."""
import gzip
import json

GZ = {"content-encoding": "gzip", "content-type": "application/json"}


def gz(doc) -> bytes:
    return gzip.compress(json.dumps(doc).encode(), mtime=0)


STEPS: list = [
    ("GET", "/health", None),
    ("POST", "/items", {"name": "a", "qty": 2}),
    ("POST", "/items", gz({"name": "b"}), GZ),
    ("POST", "/items", {"name": "bad"}),
    ("POST", "/items", gz({"name": "bad"}), GZ),
    ("POST", "/items", {"qty": "x"}),
    ("POST", "/items", b"not gzip", GZ),
    ("POST", "/items", {"name": "c"}, {"x-audit": "read"}),
    ("POST", "/items", gz({"name": "bad"}), {**GZ, "x-audit": "read"}),
    ("POST", "/raw", {"k": [1, 2]}),
    ("POST", "/raw?fail=1", {"k": 1}),
    ("POST", "/raw?fail=1", gz({"k": "z"}), GZ),
    ("POST", "/raw", None),
    ("POST", "/form", b"name=n1", {"content-type": "application/x-www-form-urlencoded"}),
    ("GET", "/nobody", None),
    ("GET", "/nowhere", None),
    ("POST", "/nowhere", {"a": 1}),
    ("DELETE", "/items", None),
    # gzip.compress (header per Python version, zlib's stream)
    *[("GET", f"/gz/{lv}", None) for lv in (9, 1, 6, 0, -1, 10)],
    ("GET", "/gz/9?mtime=1700000000", None), ("GET", "/gz/1?mtime=5", None), ("GET", "/gz/9?mtime=-1", None), ("GET", "/gz/9?mtime=4294967295", None), ("GET", "/gz/9?mtime=4294967296", None),
]


def reset(db: str) -> None:
    pass

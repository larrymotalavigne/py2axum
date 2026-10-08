"""Scenario of the reference app (`app/`): CRUD, validation errors, partial updates, outbound HTTP,
GZip and streaming of large lists."""
import subprocess

STEPS: list = [
    ("GET", "/health", None),
    # create
    ("POST", "/users", {"email": "ada@example.com", "name": "Ada", "age": 36}),
    ("POST", "/users", {"email": "linus@example.com", "name": "Linus", "age": None}),
    ("POST", "/users", {"email": "grace@example.com", "name": "Grace"}),
    ("POST", "/users", {"email": "zoe@example.com", "name": "Zoë 🚀", "age": "42"}),
    ("POST", "/users", {"email": "ken@example.com", "name": "Ken", "age": 3.0}),
    ("POST", "/users", {"email": "ada@example.com", "name": "Dup"}),
    # validation errors
    ("POST", "/users", {"email": "a", "age": "x"}),
    ("POST", "/users", {"email": "bad@example.com", "name": "B", "age": 200}),
    ("POST", "/users", {"email": "bad@example.com", "name": "B", "age": -1}),
    ("POST", "/users", {"email": "bad@example.com", "name": "B", "age": 3.5}),
    ("POST", "/users", {"email": "bad@example.com", "name": 123}),
    ("POST", "/users", {"email": "bad@example.com", "name": ""}),
    ("POST", "/users", {"email": "x" * 300, "name": "B"}),
    ("POST", "/users", [1, 2]),
    ("POST", "/users", b""),
    ("POST", "/users", b"{not json"),
    # reads
    ("GET", "/users", None),
    ("GET", "/users?limit=2&skip=1", None),
    ("GET", "/users?limit=0", None),
    ("GET", "/users?limit=abc&skip=-1", None),
    ("GET", "/users?active_only=maybe", None),
    ("GET", "/users/1", None),
    ("GET", "/users/999", None),
    ("GET", "/users/abc", None),
    # partial updates (exclude_unset semantics)
    ("PATCH", "/users/1", {"age": None}),
    ("PATCH", "/users/1", {"name": "Ada L."}),
    ("PATCH", "/users/1", {}),
    ("PATCH", "/users/3", {"is_active": False}),
    ("PATCH", "/users/3", {"is_active": "no"}),
    ("PATCH", "/users/3", {"name": ""}),
    ("PATCH", "/users/999", {"name": "Nobody"}),
    ("GET", "/users?active_only=true", None),
    ("GET", "/users?active_only=1&limit=100", None),
    # delete
    ("DELETE", "/users/2", None),
    ("DELETE", "/users/2", None),
    ("GET", "/users/2", None),
    # outbound HTTP with aiohttp / reqwest
    ("GET", "/upstream/health", None),
    # framework behaviour
    ("GET", "/nope", None),
    ("PUT", "/health", None),
]
# enough rows for a > 1000-byte list: exercises GZipMiddleware(minimum_size=1000) and streaming
STEPS += [("POST", "/users", {"email": f"bulk{i}@example.com", "name": f"Bulk {i}", "age": i}) for i in range(30)]
STEPS += [("GET", "/users?limit=100", None), ("GET", "/users?limit=3", None),
             ("GET", "/users/export", None), ("GET", "/users/export?limit=5", None),
             ("GET", "/users/export?limit=0", None),
             # streamed from the session's statement (dyn: orm::defer_list), or not when the session wrote
             ("GET", "/users/newest", None), ("POST", "/users/3/shout", None), ("POST", "/users/999/shout", None),
             ("GET", "/users/3", None)]
# a body above axum's 2 MB default limit (Starlette has none): unknown field, ignored by the model
STEPS += [("POST", "/users", {"email": "big@example.com", "name": "Big", "pad": "x" * 5_000_000})]



def reset(db: str) -> None:
    subprocess.run(["psql", db, "-qc", "TRUNCATE users RESTART IDENTITY"], check=True, capture_output=True)


def normalize(body):
    """Only differences that are not semantic: the JSON parser's own error text."""
    if isinstance(body, dict) and isinstance(body.get("detail"), list):
        for d in body["detail"]:
            if d.get("type") == "json_invalid":
                d.get("ctx", {}).pop("error", None)
    return body

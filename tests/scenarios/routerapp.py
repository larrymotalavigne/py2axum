"""Scenario of fixtures/routerapp: routers outside the translated directory, import guards of the project's own
modules decided statically (the guarded registrations never run), a function given the app that registers
routers through a loop over a module tuple, in registration order."""

STEPS: list = [
    ("GET", "/health", None),
    ("GET", "/api/items", None),
    ("GET", "/api/items?limit=1", None),
    ("GET", "/api/items?limit=x", None),
    ("GET", "/api/items/2", None),
    ("GET", "/api/items/9", None),
    ("GET", "/api/items/abc", None),
    ("GET", "/api/items/nested/ping", None),
    ("GET", "/api/optional", None),
    ("GET", "/api2/optional", None),
    ("GET", "/flag", None),
    ("GET", "/reg/a/fixed", None),
    ("GET", "/reg/a/zz", None),
    ("GET", "/reg/b", None),
    ("GET", "/reg2/a/fixed", None),
    ("GET", "/reg2/b", None),
    ("GET", "/reg2/a/zz", None),
    ("GET", "/more/v2/b", None),
    ("GET", "/more/v1/b", None),
    ("GET", "/env/x/a/q", None),
    ("GET", "/j/num", None),
    ("GET", "/j/raw", None),
    ("GET", "/p/num", None),
    ("GET", "/p/explicit", None),
    ("GET", "/h/page", None),
    ("GET", "/p/text", None),
    ("GET", "/h/inner/text", None),
    ("GET", "/on/b", None),
    ("GET", "/off/a/x", None),
    ("GET", "/debug/a/x", None),
    ("GET", "/debug/a/x/", None),
    ("GET", "/on/b/", None),
    ("GET", "/cond/local/b", None),
    ("GET", "/cond/prod/b", None),
    ("POST", "/cond/local/b", None),
    ("POST", "/off/a/x", None),
    ("GET", "/f/a/1", None),
    ("GET", "/f/rb/b", None),
    ("GET", "/again/f/rb/a/fixed", None),
    ("GET", "/again/f/a/z", None),
    ("GET", "/kw/ping", None),
    ("GET", "/items", None),
    ("POST", "/api/items", None),
]


def reset(db: str) -> None:
    pass

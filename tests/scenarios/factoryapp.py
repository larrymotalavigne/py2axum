"""Scenario of fixtures/factoryapp (no database): factory endpoints, middleware stack, exception handlers."""
OK = {"Origin": "http://ok.example"}
BAD = {"Origin": "http://evil.example"}
PRE = {"Origin": "http://ok.example", "Access-Control-Request-Method": "POST"}

STEPS: list = [
    ("GET", "/health", None),
    ("GET", "/", None),
    ("GET", "/nope", None),
    ("GET", "/nope", None, OK),
    ("POST", "/health", None, OK),
    ("GET", "/health/", None, OK),
    ("GET", "/health", None, BAD),
    ("GET", "/health", None, {"Origin": "https://www.ok.example", "Vary": "x"}),
    # preflights
    ("OPTIONS", "/items", None, PRE),
    ("OPTIONS", "/items", None, {**PRE, "Access-Control-Request-Headers": "content-type, x-custom"}),
    ("OPTIONS", "/items", None, {**PRE, "Access-Control-Request-Headers": "X-Other"}),
    ("OPTIONS", "/items", None, {**PRE, "Origin": "http://evil.example", "Access-Control-Request-Method": "DELETE"}),
    ("OPTIONS", "/items", None, {**PRE, "Access-Control-Request-Private-Network": "true"}),
    ("OPTIONS", "/items", None, OK),
    ("POST", "/items", {"a": 1}, OK),
    # exception handlers
    ("GET", "/conflict/flat", None, OK),
    ("GET", "/crash", None, OK),
    ("GET", "/teapot", None, OK),
    ("GET", "/missing", None),
    # the instance's state survives requests; the cookie or the client address is the key
    ("GET", "/limited", None),
    ("GET", "/limited", None, {"Cookie": 'session=abc; theme="d\\141rk"; empty='}),
    ("GET", "/limited", None),
    ("GET", "/limited", None, OK),
    ("GET", "/limited?x=1", None, {"Cookie": "session=abc"}),
    ("GET", "/limited", None, {"Cookie": "session=abc"}),
]


def reset(db: str) -> None:
    pass

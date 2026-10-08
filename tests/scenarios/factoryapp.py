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
    # request metrics installed by install_observability(app): labels from the route templates
    ("GET", "/metrics/all", None),
    ("GET", "/shop/items/3", None),
    ("GET", "/v2/shop/items/4", None),
    ("GET", "/shop/items/2", None),
    ("GET", "/shop/items/x\"y", None),
    ("POST", "/shop/items/3", None),
    ("POST", "/shop/items", None),
    ("GET", "/shop/items/", None),
    ("HEAD", "/metrics", None),
    ("GET", "/introspect", None),
    ("GET", "/metrics", None),
    ("GET", "/metrics/all", None),
    # handlers registered by a function given the app, app.add_exception_handler, strict_content_type=False
    ("POST", "/typed", {"qty": 2}),
    ("POST", "/typed", {"qty": "x"}),
    ("POST", "/typed", {}),
    ("POST", "/typed", b'{"qty": 3}', {"content-type": ""}),
    ("POST", "/typed", b'{"qty": 3}', {"content-type": "text/plain"}),
    ("POST", "/typed", b'{"qty": ', {"content-type": ""}),
    ("GET", "/keyerror", None),
    ("GET", "/gone/thing", None),
    ("GET", "/gone/thing", None, OK),
]


def normalize_text(text: str) -> str:
    """prometheus_client: the `_created` series hold creation instants, masked when they have their shape;
    multiprocess mode: `pid` labels are the servers' pids, request durations vary, and MultiProcessCollector
    lists the metrics in the order the directory lists the process files (it depends on the file names,
    hence on the pid, in CPython too): families are compared sorted."""
    import re
    if not text.startswith("# HELP"):
        return text
    text = re.sub(r"(?m)^(\S+_created(?:\{.*\})?) \d\.\d+e\+09$", r"\1 <created>", text)
    text = re.sub(r'pid="\d+"', 'pid="<pid>"', text)
    text = re.sub(r"(?m)^(http_seconds_sum(?:\{.*\})?) \S+$", r"\1 <duration>", text)
    families = re.split(r"(?m)^(?=# HELP )", text)
    return "".join(sorted(f for f in families if f))


def reset(db: str) -> None:
    pass

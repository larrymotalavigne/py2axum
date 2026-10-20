"""Scenario of fixtures/wsrelayapp: WebSocket connections the binary relays to the Python process (route
`/r/{kind}` declared --python-side, `/state` moved there by --python-side auto: it reads what the lifespan left
to Python sets), compared with uvicorn serving them directly; `/native` is translated."""

STEPS: list = [
    ("GET", "/health", None),
    ("WS", "/native", [("send", "a"), ("recv", 1)]),
    ("WS", "/state", [("recv", 1)]),
    ("WS", "/r/echo", [("send", "a"), ("recv", 1), ("send", b"\x00\x01"), ("recv", 1), ("send", "é" * 3000), ("recv", 1)]),
    ("WS", "/r/echo", [("send", "x"), ("recv", 1), ("close", 4000, "client bye")]),
    ("WS", "/r/echo", [("send", "x"), ("recv", 1), ("close",)]),
    ("WS", "/r/refuse", []),
    ("WS", "/r/deny", []),
    ("WS", "/r/proto", [("recv", 1)], {"subprotocols": ["chat", "superchat"], "x-token": "t1"}),
    ("WS", "/r/proto", [("recv", 1)]),
    ("WS", "/r/query?a=1&b=%C3%A9", [("recv", 1)]),
    ("WS", "/r/caf%C3%A9", []),
    ("WS", "/r/drop", [("recv", 2)]),
    ("WS", "/nowhere", []),
    ("GET", "/r/echo", None),
]

LOG_IGNORE = [
    # docs/getting-started/hybrid.md: a relayed connection's application lines (an error, a traceback) are written
    # by the Python process that runs it, the binary writing the transport ones (handshake, `connection open`)
    r"ASGI callable returned without completing handshake\.",
]


def reset(db: str) -> None:
    pass

"""Scenario of fixtures/hostapp: TrustedHostMiddleware (main.py) and HTTPSRedirectMiddleware (https.py) answer from
the Host header (valid, wildcard, www redirect, malformed, IPv6, ports). Both apps get the same steps."""

HOSTS = ["127.0.0.1", "a.example.com", "example.com", "x.y.example.com", "example.org", "EXAMPLE.ORG:8080",
         "evil.com", "127.0.0.1:80", "127.0.0.1:443", "127.0.0.1:0443", "[::1]", "[::1]:443", "[::g]", "a b",
         "host:99999", "Example.COM:80", ""]

STEPS: list = [
    *[("GET", "/", None, {"host": h}) for h in HOSTS],
    ("GET", "/items/caf%C3%A9?q=a%20b&r=1", None, {"host": "example.org"}),
    ("GET", "/items/x", None, {"host": "evil.com"}),
    ("POST", "/", {"a": 1}, {"host": "example.org"}),
    ("GET", "/", None),
]


def reset(db: str) -> None:
    pass

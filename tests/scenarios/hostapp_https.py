"""Scenario of fixtures/hostapp/https.py (HTTPSRedirectMiddleware): the Host headers Starlette parses. A missing
or unparsable one makes Starlette redirect to the server's own address, which the binary does not know (500,
documented): not sent here."""
from tests.scenarios.hostapp import reset  # noqa: F401

HOSTS = ["127.0.0.1", "a.example.com", "example.com", "EXAMPLE.ORG:8080", "127.0.0.1:80", "127.0.0.1:443",
         "127.0.0.1:0443", "[::1]", "[::1]:443", "[::1]:8000", "Example.COM:80", "h:65535"]

STEPS: list = [
    *[("GET", "/", None, {"host": h}) for h in HOSTS],
    ("GET", "/items/caf%C3%A9?q=a%20b&r=1", None, {"host": "example.org"}),
    ("POST", "/", {"a": 1}, {"host": "example.org"}),
]

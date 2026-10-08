"""py2axum/record.py (the anonymizing request recorder) and the replay side of tests/difftest.py."""
import json

import httpx
from fastapi import FastAPI, Request, Response
from fastapi.testclient import TestClient

from py2axum.record import RecordMiddleware
from tests import difftest


def _app() -> FastAPI:
    app = FastAPI()

    @app.post("/signup")
    async def signup(request: Request):
        return {"echo": await request.json()}

    @app.post("/login")
    async def login(request: Request, response: Response):
        body = await request.json()
        response.set_cookie("session", "real-session-" + body["email"])
        return {"access_token": "real-jwt-value-123", "user": {"email": body["email"]}}

    @app.get("/me")
    async def me(request: Request):
        return {"auth": request.headers.get("authorization"), "cookie": request.cookies.get("session")}

    @app.post("/upload")
    async def upload(request: Request):
        return {"size": len(await request.body())}

    @app.get("/health")
    async def health():
        return {"ok": True}

    return app


def _record(tmp_path, **kw):
    path = tmp_path / "rec.jsonl"
    app = RecordMiddleware(_app(), str(path), key=b"k", fields=["first_name"], **kw)
    with TestClient(app) as c:
        r = c.post("/signup", json={"email": "Alice@Corp.fr", "password": "hunter2", "first_name": "Alice",
                                    "note": "write to bob@corp.fr", "age": 31})
        assert r.json()["echo"]["email"] == "Alice@Corp.fr"  # the application sees the real body
        c.post("/login", json={"email": "alice@corp.fr", "password": "hunter2"})
        c.get("/me?email=alice@corp.fr&page=2", headers={"authorization": "Bearer real-jwt-value-123",
                                                         "x-forwarded-for": "81.2.69.160", "user-agent": "secret-ua"})
        c.post("/upload", content=b"\x00\x01binary", headers={"content-type": "application/octet-stream"})
        c.get("/health")
    lines = [json.loads(x) for x in path.read_text().splitlines()]
    return lines[0], lines[1:]


def test_header_and_skip(tmp_path):
    header, recs = _record(tmp_path)
    assert header["format"] == "py2axum-replay" and header["version"] == 1 and header["anonymized"]
    assert [r["path"] for r in recs] == ["/signup", "/login", "/me", "/upload"]  # /health skipped


def test_anonymized(tmp_path):
    _, recs = _record(tmp_path)
    text = "\n".join(json.dumps(r) for r in recs)
    for secret in ("alice@corp.fr", "Alice@Corp.fr", "bob@corp.fr", "hunter2", "real-jwt-value-123", "real-session",
                   "81.2.69.160", "secret-ua", '"Alice"'):
        assert secret.lower() not in text.lower(), secret
    signup, login, me, upload = recs
    body = signup["body"]["json"]
    assert body["age"] == 31 and body["first_name"].startswith("redacted-")
    # one e-mail (any case), one password: one pseudonym each, sign-up and log-in still agree
    assert body["email"] == login["body"]["json"]["email"] and body["email"].endswith("@example.com")
    assert body["password"] == login["body"]["json"]["password"]
    assert body["note"].startswith("write to u-")
    assert "email=u-" in me["query"] and me["query"].endswith("page=2")
    assert dict(me["headers"])["x-forwarded-for"].startswith("198.51.100.")
    assert upload["body"] == {"omitted": "binary", "size": 8}


def test_issued_credentials_link_to_later_requests(tmp_path):
    _, recs = _record(tmp_path)
    login, me = recs[1], recs[2]
    issued = {(k, w): p for k, w, p in login["issued"]}
    jwt = issued[("json", "/access_token")]
    assert ("cookie", "session") in issued
    assert dict(me["headers"])["authorization"] == f"Bearer {jwt}"
    assert me["session"] is not None


def test_raw_bodies_only_on_request(tmp_path):
    _, recs = _record(tmp_path, raw=True)
    assert recs[3]["body"] == {"b64": "AAFiaW5hcnk="}


def test_replay_substitutes_each_servers_credentials(tmp_path):
    _, recs = _record(tmp_path)
    login, me = recs[1], recs[2]
    side = difftest.Side("ref", "http://x", "postgresql:///x_replay", None, jar=False, headers={})
    side.issued = login["issued"]
    r = httpx.Response(200, json={"access_token": "other-server-jwt"}, headers={"set-cookie": "session=other; Path=/"},
                       request=httpx.Request("POST", "http://x/login"))
    side.learn(r)
    step = difftest.record_step(me)
    method, path, payload, headers = side.subst(step)
    assert headers["authorization"] == "Bearer other-server-jwt"
    assert difftest.record_step(recs[3]) is None  # body not recorded: not replayed


def test_unknown_session_cookie_dropped():
    side = difftest.Side("ref", "http://x", "postgresql:///x_replay", None, jar=False, headers={})
    step = ("GET", "/me", None, {"cookie": "session=py2axum-tok-0123456789ab; theme=dark"})
    assert difftest._unknown_cookies(side, step)[3] == {"cookie": "theme=dark"}


def test_minimize_keeps_the_cause():
    recs = list(range(20))
    # the last request diverges only when 3 and 11 came before it
    got = difftest.minimize(recs, lambda sub: 3 in sub and 11 in sub and sub[-1] == 19, 200)
    assert got == [3, 11, 19]


def test_short_credentials_not_replaced_inside_text():
    from py2axum.record import Anonymizer

    a = Anonymizer(b"k")
    short, long = a.token("42"), a.token("a-long-session-token-0123")
    assert a.text("item 42 of 420") == "item 42 of 420"
    assert a.text("42") == short  # the whole value still is
    got = a.text("x a-long-session-token-0123 y")
    assert got == f"x {long} y" and "a-long" not in got

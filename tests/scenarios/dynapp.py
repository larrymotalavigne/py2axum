"""Scenario of fixtures/dynapp: the dyn backend's own reference app (enums, computed defaults,
annotated dependency aliases...). Tables are (re)created by SQLAlchemy from the fixture models."""
import os
from importlib.metadata import version as _version

# get_db commits after the response (FastAPI >= 0.121): pause after each write so that the reference has
# committed before the next request reads (the binary commits before answering)
SETTLE = 0.2


def _cpython_pickles() -> tuple[str, str]:
    """Pickles written by CPython itself (protocols 4 and 5) for /pk/load."""
    import base64
    import pickle

    from fixtures.dynapp.pk import Entry, sample
    e = Entry(sample(), 1.5, 2.0, 3.0)
    return tuple(base64.b64encode(pickle.dumps(e, protocol=p)).decode() for p in (4, 5))


def _unpickled(b64: str):
    """A pickle written by either server, read back by CPython: compared by value, not by bytes."""
    import base64
    import pickle

    from fixtures.dynapp.pk import Bag, Entry

    def desc(o):
        if isinstance(o, Entry):
            return {"entry": [desc(getattr(o, s)) for s in Entry.__slots__]}
        if isinstance(o, Bag):
            return {"bag": sorted((k, desc(v)) for k, v in vars(o).items())}
        if isinstance(o, dict):
            return {k: desc(v) for k, v in o.items()}
        if isinstance(o, (list, tuple)):
            return [type(o).__name__, [desc(x) for x in o]]
        return repr(o)
    try:
        return desc(pickle.loads(base64.b64decode(b64)))
    except Exception as e:  # noqa: BLE001
        return f"unreadable by CPython: {type(e).__name__}: {e}"


def _jwt_cases() -> list:
    """Tokens built by python-jose itself (fixed dates: same bytes for both servers)."""
    import base64
    import json as _json
    from urllib.parse import quote

    from jose import jwt

    k = "s3cret"
    far, past = 4102444800, 946684800  # 2100-01-01, 2000-01-01

    def raw(header: dict, payload, key=k, alg="HS256"):
        import hashlib
        import hmac
        enc = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()  # noqa: E731
        h = enc(_json.dumps(header, separators=(",", ":")).encode())
        p = enc(payload if isinstance(payload, bytes) else _json.dumps(payload, separators=(",", ":")).encode())
        hm = {"HS256": hashlib.sha256, "HS384": hashlib.sha384}[alg]
        sig = enc(hmac.new(key.encode(), f"{h}.{p}".encode(), hm).digest())
        return f"{h}.{p}.{sig}"

    good = jwt.encode({"sub": "ada", "exp": far, "n": 1.5, "l": [1, "é"]}, k)
    tokens = [
        good,
        jwt.encode({"sub": "ada", "exp": past}, k),
        jwt.encode({"nbf": far}, k),
        jwt.encode({"aud": "x"}, k),
        jwt.encode({"aud": ["x", 1]}, k),
        jwt.encode({"sub": 5}, k),
        jwt.encode({"jti": 5}, k),
        jwt.encode({"iat": "abc"}, k),
        jwt.encode({"iat": "12", "exp": "4102444800"}, k),
        jwt.encode({"at_hash": "x"}, k),
        jwt.encode({"sub": "ada"}, "other"),
        jwt.encode({"sub": "ada"}, k, algorithm="HS384"),
        good[:-3] + "AAA",
        good + "!!",
        "abc", "a.b", "a.b.c", "....", good.replace(".", "..", 1),
        raw({"alg": "none"}, {"a": 1}),
        raw({"typ": "JWT"}, {"a": 1}),
        raw({"alg": "HS256"}, [1, 2]),
        raw({"alg": "HS256"}, b"not json"),
        raw({"alg": "RS256"}, {"a": 1}),
        raw({"alg": "XX1"}, {"a": 1}),
    ]
    steps = [("GET", f"/jwt/check?token={quote(t)}", None) for t in tokens]
    steps += [
        ("GET", f"/jwt/check?token={quote(good)}&algs=HS384&algs=HS256", None),
        ("GET", f"/jwt/check?token={quote(tokens[11])}&algs=HS384", None),
        ("GET", f"/jwt/check?token={quote(good)}&key=%22s3cret%22", None),
        ("GET", f"/jwt/check?token={quote(good)}&key=-----BEGIN%20PUBLIC%20KEY-----", None),
        ("POST", "/jwt/issue", {"sub": "ada", "roles": ["a"], "é": "ü"}),
        ("POST", "/jwt/issue?alg=HS512", {"sub": "ada"}),
        ("POST", "/jwt/issue?alg=XX", {"sub": "ada"}),
        ("GET", "/jwt/me", None, {"authorization": f"Bearer {good}"}),
        ("GET", "/jwt/me", None, {"authorization": "Bearer nope"}),
    ]
    return steps


def _its_cases() -> list:
    """Tokens signed by itsdangerous itself with fixed timestamps."""
    from urllib.parse import quote

    from itsdangerous import TimestampSigner, URLSafeTimedSerializer

    def ser(ts, key="s3cret", salt="itsdangerous"):
        class Signer(TimestampSigner):
            def get_timestamp(self):
                return ts

        class S(URLSafeTimedSerializer):
            default_signer = Signer

        return S(key, salt=salt)

    old = 1_000_000_000
    big = {"rows": [{"name": "same", "value": i % 3} for i in range(30)], "é": "ü"}
    raw = TimestampSigner("s3cret", salt="itsdangerous")
    tokens = [
        (ser(old).dumps({"user_id": 7, "purpose": "2fa"}), ""),
        (ser(old).dumps({"user_id": 7}), "&max_age=60"),
        (ser(4102444800).dumps({"user_id": 7}), "&max_age=60"),
        (ser(old).dumps(big), ""),
        (ser(old).dumps([1, "x", None]), "&salt=other"),
        (ser(old, key="old-key").dumps({"k": "old"}), "&old_key=true"),
        (ser(old, key="old-key").dumps({"k": "old"}), ""),
        (ser(old).dumps({"a": 1})[:-2] + "xx", ""),
        (ser(old).dumps({"a": 1})[:-2] + "xx", "&max_age=60"),
        ("nodot", ""),
        ("a.b", ""),
        (raw.sign(b"!!!").decode(), ""),
        (raw.sign(b"bm90IGpzb24").decode(), ""),
        (raw.sign(b".bm90IHpsaWI").decode(), ""),
        (raw.sign(b"e30").decode().rsplit(".", 2)[0] + "..sig", ""),
    ]
    return [("GET", f"/its/check?token={quote(t)}{q}", None) for t, q in tokens] + [
        ("POST", "/its/issue", {"user_id": 1, "purpose": "2fa", "é": "ü"}),
        ("POST", "/its/issue?salt=magic", {"mandant_id": 3}),
        ("POST", "/its/issue", big),
    ]


def _multipart(parts: list) -> tuple[bytes, dict]:
    """(name, value) text fields and (name, filename, content_type, bytes) files -> body, headers."""
    b = "py2axumBOUNDARY"
    out = b""
    for p in parts:
        out += f"--{b}\r\n".encode()
        if len(p) == 2:
            out += f'Content-Disposition: form-data; name="{p[0]}"\r\n\r\n'.encode() + p[1].encode() + b"\r\n"
        else:
            ct = f"Content-Type: {p[2]}\r\n" if p[2] else ""
            out += (f'Content-Disposition: form-data; name="{p[0]}"; filename="{p[1]}"\r\n{ct}\r\n').encode() + p[3] + b"\r\n"
    out += f"--{b}--\r\n".encode()
    return out, {"content-type": f"multipart/form-data; boundary={b}"}


def _upload_cases() -> list:
    cases = [
        [("file", "a.txt", "text/plain", "héllo wörld".encode()), ("caption", "Cap"), ("order", "3"), ("primary", "true")],
        [("file", "b.bin", None, b"\x00\x01\x02"), ("caption", ""), ("order", "")],
        [("caption", "no file"), ("order", "x")],
        [("file", "not a file"), ("primary", "maybe")],
    ]
    steps = []
    for c in cases:
        body, h = _multipart(c)
        steps.append(("POST", "/upload", body, h))
    body, h = _multipart([("files", "1.csv", "text/csv", b"a,b"), ("files", "2.csv", "text/csv", b"c,d,e"), ("note", "n.txt", "text/plain", b"x")])
    steps.append(("POST", "/uploads", body, h))
    body, h = _multipart([("files", "1.csv", "text/csv", b"a")])
    steps.append(("POST", "/uploads", body, h))
    body, h = _multipart([("other", "x")])
    steps.append(("POST", "/uploads", body, h))
    steps.append(("POST", "/upload", b"caption=url&order=2", {"content-type": "application/x-www-form-urlencoded"}))
    steps.append(("POST", "/upload", {"file": "json"}))
    steps.append(("POST", "/upload", b"--x\r\nbroken", {"content-type": "multipart/form-data; boundary=x"}))
    return steps

# the reference's Starlette (the version matrix runs the lowest supported one)
_STARLETTE = tuple(int(x) for x in _version("starlette").split(".")[:2])

STEPS: list = [
    ("POST", "/tasks", {"title": "Écrire", "priority": "high", "tags": ["a", "b"]}),
    ("POST", "/tasks", {"title": "Relire"}),
    ("POST", "/tasks", {"title": "Bas", "priority": "low"}),
    ("POST", "/tasks", {"title": "x", "priority": "urgent"}),
    ("POST", "/tasks", {"title": "x", "priority": 1}),
    ("POST", "/tasks", {"title": "x", "level": 3}),
    ("POST", "/tasks", {"title": "x", "channel": "fax"}),
    ("GET", "/tasks", None),
    ("GET", "/tasks?priority=high", None),
    ("GET", "/tasks?priority=nope", None),
    ("GET", "/tasks?status=open", None),
    ("GET", "/tasks/1", None),
    ("GET", "/tasks/99", None),
    ("POST", "/tasks/1/status", {"status": "done"}),
    ("POST", "/tasks/1/status", {"status": "done"}),
    ("POST", "/tasks/1/status", {"status": "DONE"}),
    ("GET", "/tasks?status=done", None),
    ("POST", "/tasks/2/lower", None),
    ("POST", "/tasks/99/lower", None),
    ("POST", "/describe", {"title": "t", "priority": "high", "channel": "sms", "level": 2}),
    ("POST", "/describe", {"title": "t"}),
    ("POST", "/summary", {"title": "t", "priority": "low"}),
    ("GET", "/stats", None),
    ("POST", "/contacts", {"name": " Ada ", "email": 'Ada@Example.COM'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'ada'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'ada@'}),
    ("POST", "/contacts", {"name": " Ada ", "email": '@x.fr'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'ada@localhost'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'a b@x.fr'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'ada@x..fr'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'ada@-x.fr'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'ADA.Lovelace+tag@sub.Example.org'}),
    ("POST", "/contacts", {"name": " Ada ", "email": '  ada@x.fr '}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'ada@[1.2.3.4]'}),
    ("POST", "/contacts", {"name": " Ada ", "email": '"a b"@x.fr'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'ada@x.123'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'Ada <ada@x.fr>'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'Info@x.fr'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'ada@x.test'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'ada@x.fr.'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 'x@a--b.fr'}),
    ("POST", "/contacts", {"name": " Ada ", "email": 5}),
    ("POST", "/contacts", {"name": "   ", "email": "a@x.fr"}),
    ("POST", "/contacts/bad-assign", {"name": "Ada", "email": "a@x.fr"}),
    ("POST", "/projects", {"name": "Alpha", "owner": "Ada", "tasks": ["a1", "a2", "a3"]}),
    ("POST", "/projects", {"name": "Beta", "tasks": ["b1"]}),
    ("POST", "/projects", {"name": "Gamma"}),
    ("GET", "/projects", None),
    ("GET", "/projects/1", None),
    ("GET", "/projects/9", None),
    ("GET", "/projects/1/lazy", None),
    ("GET", "/owners/1", None),
    ("GET", "/tasks/5/project", None),
    ("GET", "/tasks/5/project?preload=discard", None),
    ("GET", "/tasks/5/project?preload=keep", None),
    ("GET", "/tasks/1/project", None),
    ("POST", "/tasks/1/move/3", None),
    ("POST", "/tasks/6/move/3", None),
    ("GET", "/project-sizes", None),
    ("POST", "/projects/1/drop-first", None),
    ("DELETE", "/projects/3", None),
    ("GET", "/tasks", None),
    ("GET", "/projects", None),
    ("POST", "/tasks", {"title": "p1", "price": 12.3}),
    ("POST", "/tasks", {"title": "p2", "price": 0.1}),
    ("POST", "/tasks", {"title": "p3", "price": 19.999}),
    ("POST", "/tasks", {"title": "p4", "price": -5.555}),
    ("GET", "/prices", None),
    ("GET", "/prices?above=0.1", None),
    ("POST", "/drafts", None),
    ("POST", "/drafts?fail=true", None),
    ("GET", "/drafts/count", None),
    ("GET", "/drafts/count?pending=true", None),
    ("POST", "/drafts", None),
    ("GET", "/drafts/count", None),
    ("POST", "/projects", {"name": "Delta", "tasks": ["d1", "d2"]}),
    ("GET", "/sql-mix", None),
    ("POST", "/tasks/2/retitle", None),
    ("POST", "/tasks/3/retitle?sync=false", None),
    ("GET", "/tasks/3", None),
    ("GET", "/projects/1/via-get", None),
    ("GET", "/projects/2/via-get?how=refresh", None),
    ("GET", "/projects/9/via-get", None),
    ("GET", "/items/special", None),
    ("POST", "/items/special", None),
    ("GET", "/items/caf%C3%A9%20au%20lait", None),
    ("GET", "/tasks/%31", None),
    ("HEAD", "/tasks", None),
    ("DELETE", "/tasks", None),
    ("PUT", "/items/x", None),
    ("GET", "/tasks/?priority=high", None, {"host": "api.example:9000"}),
    ("GET", "/items/x/", None, {"host": "api.example"}),
    ("GET", "/nowhere/", None),
    ("POST", "/people", {"fullName": "Ada Lovelace", "tags": ["math"]}),
    ("POST", "/people", {"full_name": "Ada", "nick": "al"}),
    ("POST", "/people", {"full_name": "A"}),
    ("POST", "/people", {}),
    ("GET", "/tasks/2/as-title?attrs=true", None),
    ("GET", "/tasks/2/as-title", None),
    ("POST", "/sum?scale=2", [1, 2, 3]),
    ("POST", "/sum?values=1&values=2", None),
    ("POST", "/sum", b"{not json"),
    ("GET", "/stats", b"{not json"),
    ("POST", "/tasks/2/price?value=42.424", None),
    ("POST", "/tasks/2/price?value=1e12", None),
    ("GET", "/lazy", None),
    ("GET", "/lazy?n=2", None),
    ("POST", "/bookings?n=1", {"slots": [{"start": 1, "label": "talk", "name": "ada lovelace", "room": "Big"}], "total": 1}),
    ("POST", "/bookings?n=-1", {"slots": [
        {"start": "x", "label": "admin", "name": "a1", "seats": 7},
        {"start": 1, "label": "quiet", "name": "ok"},
        {"start": 1, "label": "hush", "name": "b2"},
        {"start": 2, "label": 3, "name": "c"},
    ], "total": "many"}),
    ("POST", "/bookings?n=1", {"slots": [{"start": 1, "label": "x", "name": "n", "room": "teapot"}], "total": 1}),
    ("POST", "/bookings?n=1", {"slots": [{"start": 1, "label": "x", "name": "n", "room": "teapot"}], "total": "x"}),
    # field validators taking `info` (info.data, info.field_name) or v1 `values`
    ("POST", "/agenda", {"spans": [{"start": 1, "end": 2, "seen": "s", "note": "n"}], "title": "t"}),
    ("POST", "/agenda", {"spans": [{"start": -1, "end": 0, "seen": "s", "note": "n"},
                                   {"start": "x", "kind": 3, "end": 9, "seen": "s", "note": None},
                                   {"start": 5, "end": 1, "seen": "s", "note": "n"}], "title": 2}),
    ("POST", "/agenda", {"spans": [{"start": 2, "end": 1}], "title": "t"}),
    ("POST", "/agenda/errors", {"start": -1, "end": 0}),
    ("POST", "/agenda/errors", {"start": 3, "end": "z", "seen": 1}),
    ("POST", "/agenda/errors", {"start": 1, "end": 1}),
    ("POST", "/slots/check", {"start": 1, "label": "ok"}),
    ("POST", "/slots/check", {"start": "z", "label": "admin", "seats": 7}),
    ("POST", "/slots/check", {"label": "quiet", "room": "C"}),
    ("GET", "/slots/fine", None),
    ("GET", "/slots/admin", None),
    ("POST", "/mixed", {"a": 1, "b": 1, "c": 1, "d": {"x": 1}, "e": [1], "f": "1", "g": 1, "h": "2020-01-02", "i": {"k": 1}}),
    ("POST", "/mixed", {"a": "1", "b": 1.0, "c": True, "d": {"x": 1, "z": 2}, "e": ["1"], "f": 2, "g": "low", "h": "3", "j": "ab"}),
    ("POST", "/mixed", {"a": 1.0, "b": "1", "c": "1", "d": {"x": "1"}, "e": [], "f": 2.0, "g": "nope", "h": 4, "i": [{"x": 1}], "j": 5}),
    ("POST", "/mixed", {"a": True, "b": True, "c": 1.0, "d": {"x": "a"}, "f": "2", "g": 2, "j": "a"}),
    ("POST", "/mixed", {"a": None, "b": "x", "c": "maybe", "d": {}, "e": [1, "a"], "f": [], "g": [], "h": "x", "i": 3, "j": None}),
    ("POST", "/mixed", {"a": [], "d": 1, "e": "x", "i": [{"y": 1}], "k": "q"}),
    ("POST", "/mixed", {"a": 1, "k": None}),
    ("GET", '/auth/me', None),
    ("GET", '/auth/me', None, {"authorization": 'Bearer good'}),
    ("GET", '/auth/me', None, {"authorization": 'Bearer bad'}),
    ("GET", '/auth/me?n=x', None),
    ("GET", '/auth/me?n=x', None, {"authorization": 'Bearer good'}),
    ("GET", '/auth/token', None, {"authorization": 'bearer  spaced'}),
    ("GET", '/auth/token', None, {"authorization": 'Basic abc'}),
    ("GET", '/auth/token', None, {"authorization": 'Bearer'}),
    ("GET", '/auth/token', None, {"authorization": ''}),
    ("GET", '/auth/optional', None),
    ("GET", '/auth/optional', None, {"authorization": 'Token x'}),
    ("GET", '/auth/optional', None, {"authorization": 'BEARER t'}),
    ("GET", '/auth/creds', None),
    ("GET", '/auth/creds', None, {"authorization": 'Bearer jwt.x'}),
    ("GET", '/auth/creds', None, {"authorization": 'Bearer'}),
    ("GET", '/auth/creds', None, {"authorization": 'Basic abc'}),
    ("GET", '/auth/creds', None, {"authorization": 'Bearer a b'}),
    ("GET", '/auth/maybe-creds', None),
    ("GET", '/auth/maybe-creds', None, {"authorization": 'Basic x'}),
    ("GET", '/auth/maybe-creds', None, {"authorization": 'bearer y'}),
    *_jwt_cases(),
    ("GET", "/env", None),
    ("GET", "/env?name=HOME", None),
    ("GET", "/env?name=", None),
    ("POST", "/globals", None),
    ("POST", "/globals", None),
    ("GET", "/sentry?user_id=7", None),
    ("GET", "/dataclass", None),
    ("GET", "/dataclass?role=viewer&perm=read", None),
    ("GET", "/dataclass?role=viewer", None),
    ("GET", "/dataclass?role=admin", None),
    *_its_cases(),
    ("POST", "/secrets", {"label": "totp", "token": "JBSWY3DPEHPK3PXP"}),
    ("POST", "/secrets", {"label": "empty", "token": ""}),
    ("POST", "/secrets", {"label": "none"}),
    ("GET", "/secrets/1", None),
    ("GET", "/secrets/2", None),
    ("GET", "/secrets/3", None),
    ("PUT", "/secrets/1", {"token": "été ✓"}),
    ("GET", "/secrets/1", None),
    ("POST", "/assets", {}),
    ("POST", "/assets", {"channel": "sms"}),
    ("POST", "/assets", {"channel": "fax"}),
    ("POST", "/assets", {"b64": "AAH/gGhpAA==", "text": "héllo"}),
    ("POST", "/assets", {"b64": "aGVsbG8=", "channel": "sms"}),
    ("GET", "/assets/1/data", None),
    ("GET", "/assets/3/data", None),
    ("GET", "/assets/4/data", None),
    ("GET", "/assets/4/meta", None),
    ("GET", "/assets/1/meta", None),
    ("GET", "/assets/3/meta", None),
    ("GET", "/assets", None),
    ("GET", "/assets?channel=sms", None),
    ("GET", "/assets?channel=SMS", None),
    ("GET", "/gendep/events", None),
    ("GET", "/gendep?tag=a", None),
    ("GET", "/gendep/events", None),
    ("GET", "/gendep?fail=true", None),
    ("GET", "/gendep/events", None),
    ("GET", "/gendep?fail=maybe", None),
    ("GET", "/gendep/events", None),
    ("GET", "/classes", None),
    ("POST", "/trips", {"legs": [{"a": 1}, {"a": 11}, {"a": "z"}, {"a": 5}]}),
    ("POST", "/trips", {"legs": [{"a": 2}], "swap": {"a": -1}}),
    ("POST", "/trips", {"legs": [{"a": 3}], "swap": {"a": 4}}),
    ("POST", "/invoices", {"lines": [{"value": "1 000€", "note": "  "}, "250", {"value": 3, "note": "x"}], "total": "1253"}),
    ("POST", "/invoices", {"lines": [{"value": "bad"}, "X", {"value": "z€"}, {"value": 1, "unit": 2}], "total": {"value": "bad"}}),
    ("GET", "/tasks/2/loose", None),
    *_upload_cases(),
    ("POST", "/files/note.txt", {"text": "héllo"}),
    ("POST", "/files/archive.tar.gz", {"text": ""}),
    ("GET", "/misc", None),
    ("GET", "/misc?n=12&kind=bois", None),
    ("GET", "/misc?kind=charbon", None),
    ("GET", "/resp/cookies", None),
    ("GET", "/resp/redirect", None),
    ("GET", "/resp/plain", None),
    ("GET", "/resp/json", None),
    ("GET", "/resp/empty", None),
    ("GET", "/resp/file", None),
    ("GET", "/resp/file?name=Reçu 1.pdf", None),
    ("GET", "/resp/file?name=plain.pdf", None),
    ("POST", "/resp/background", None),
    ("GET", "/gendep/events", None),
    ("GET", "/mail/render", None),
    ("GET", "/mail/build", None),
    ("POST", "/mail/send?to=nobody@example.com", None),  # no SMTP server in the scenario: the error path
    ("POST", "/stdlib", {"text": "<LOC> https://a.fr/x </loc><loc>b</loc> le 05/10/2026 et 31/12/1999"}),
    ("POST", "/stdlib", {"text": "rien"}),
    ("GET", "/sqlx", None),
    ("GET", "/sqlx?prio=low", None),
    ("POST", "/tasks/2/tag?tag=x", None),
    ("POST", "/tasks/2/tag?tag=y", None),
    ("POST", "/batch2", None),
    ("POST", "/token", b"username=ada&password=secret&scope=read+write", {"content-type": "application/x-www-form-urlencoded"}),
    ("POST", "/token", b"grant_type=password&username=ada&password=x", {"content-type": "application/x-www-form-urlencoded"}),
    ("POST", "/token", b"grant_type=implicit&password=x", {"content-type": "application/x-www-form-urlencoded"}),
    # last: a failed INSERT may or may not consume the id sequence (plan caching, on both sides)
    ("POST", "/tasks", {"title": "p5", "price": 1e9}),
    # dunder methods, hashability, frozen models (fixtures/dynapp/dunders.py)
    ("GET", "/dunders/eq", None),
    ("GET", "/dunders/hash", None),
    ("GET", "/dunders/frozen", None),
    ("POST", "/dunders/vdefault", {"bad": 1}),
    ("POST", "/dunders/vdefault", {"bad": 1, "nickName": "z", "upper": "q"}),
    ("POST", "/dunders/vdefault", {}),
    ("GET", "/dunders/jsonresp", None),
    ("GET", "/dunders/raise/unavailable", None),
    ("GET", "/dunders/raise/limited", None),
    ("GET", "/dunders/raise/provider", None),
    ("GET", "/dunders/raise/base", None),
    ("GET", "/dunders/raise/none", None),
    ("GET", "/dunders/builtins", None),
    ("GET", "/dunders/types", None),
    ("GET", "/dunders/classvalue", None),
    ("GET", "/dunders/smallbatch", None),
    # --python-side /dunders/proxied/{name}: relayed by the binary to PY2AXUM_PYTHON_URL
    ("POST", "/dunders/proxied/abc?q=1", b"raw body", {"x-probe": "yes", "content-type": "text/plain"}),
    # app.mount("/mounted", sub) registered last, --python-side mount: under /mounted, what no translated route
    # fully matches goes to the Python sub-application (HEAD/POST on a GET route included); elsewhere, native 404
    ("GET", "/mounted/native", None),
    ("HEAD", "/mounted/native", None),
    ("POST", "/mounted/native", b"to the mount", {"content-type": "text/plain"}),
    ("PUT", "/mounted/native", None),
    ("GET", "/mounted/a/b%20c?x=1", None),
    ("DELETE", "/mounted/", None),
    ("GET", "/mounted", None),
    ("GET", "/mountedx", None),
    ("POST", "/tasks/1/nope", None),
    ("GET", "/dunders/jsonresp?kind=model", None),
    ("GET", "/dunders/jsonresp?kind=dt", None),
    ("GET", "/dunders/jsonresp?kind=nan", None),
    ("POST", "/dunders/vdefault", {"bad": 2, "nick": "ignored", "rank": "x"}),
    # pickle in CPython's format (fixtures/dynapp/pk.py)
    ("GET", "/pk/dump", None),
    ("POST", "/pk/load", {"b64": _cpython_pickles()[0]}),
    ("POST", "/pk/load", {"b64": _cpython_pickles()[1]}),
    ("GET", "/pk/errors", None),
    # redis.asyncio (fixtures/dynapp/rds.py; Redis database 13, emptied by reset)
    ("POST", "/rds/basic", None),
    ("POST", "/rds/pickled", None),
    ("GET", "/rds/down", None),
    # aio_pika (fixtures/dynapp/amqp.py; RabbitMQ at BROKER_DSN)
    ("POST", "/amqp/roundtrip/alpha", None),
    ("GET", "/amqp/down", None),
    # @staticmethod / @classmethod of a mapped class (fixtures/dynapp/models.py Owner)
    ("POST", "/owners-cls/named", {"name": "  Ada  Lovelace "}),
    ("POST", "/owners-cls/named", {"name": "grace hopper", "shout": True}),
    ("GET", "/owners-cls/slug?q=Hello%20World", None),
    ("GET", "/owners-cls/slug?q=x&bad=true", None),
    # include_router(prefix=<settings>) read at startup (DYNAPP_API_PREFIX=/api/v9 in scripts_start_dyn.sh,
    # the default /api/v1 must not answer), include_router(dependencies=...) (fixtures/dynapp/apiv.py)
    ("GET", "/api/v9/things/whoami", None, {"x-user": "u"}),
    ("GET", "/api/v9/things/whoami", None),
    ("GET", "/api/v9/things/whois", None, {"remote-user": "ada"}),
    ("GET", "/api/v9/things/whois", None),
    ("GET", "/api/v9/things/box", None, {"x-user": "u"}),
    ("GET", "/api/v9/things/admin/ping", None, {"x-user": "u"}),
    ("GET", "/api/v9/things/admin/zed", None, {"x-user": "u"}),
    ("GET", "/api/v9/things/admin/ping/", None, {"x-user": "u"}),
    ("GET", "/api/v1/things/whoami", None, {"x-user": "u"}),
    ("DELETE", "/api/v9/things/whoami", None, {"x-user": "u"}),
    ("GET", "/api/v9/things/probe?path=/api/v9/things/admin/zed", None, {"x-user": "u"}),
    ("GET", "/api/v9/things/probe?path=/api/v1/things/admin/zed", None, {"x-user": "u"}),
    # importlib.import_module (fixtures/dynapp/lazy.py)
    ("GET", "/lazy/attr/BIG_TABLE", None),
    ("GET", "/lazy/attr/FACTOR", None),
    ("GET", "/lazy/attr/scale", None),
    ("GET", "/lazy/attr/missing", None),
    ("GET", "/lazy/use", None),
    # collections.defaultdict, string.Formatter, str.format errors (fixtures/dynapp/colls.py)
    ("POST", "/colls/count", ["ab", "abc", "b", "bcd", "a"]),
    ("POST", "/colls/count", []),
    # dateutil.relativedelta (fixtures/dynapp/extras.py)
    ("GET", "/extras/jumps", None),
    ("GET", "/extras/jumps?stop=0", None),
    ("GET", "/extras/jumps?stop=9", None),
    ("GET", "/extras/reldelta?day=2024-03-31&months=1", None),
    ("GET", "/extras/reldelta?day=2024-02-29&months=12", None),
    ("GET", "/extras/reldelta?day=2024-01-31&months=-13&weeks=2", None),
    ("GET", "/extras/reldelta?day=2023-12-31&hours=5", None),
    ("GET", "/extras/reldelta?day=2024-05-15", None),
    ("GET", "/extras/proxy", None),
    ("POST", "/extras/computed", {"a": 1, "b": 2}),
    ("POST", "/extras/computed", {"a": 1, "total": 5, "_seen": [3]}),
    ("POST", "/extras/computed", {"a": "x"}),
    ("POST", "/extras/computed/raw", {"a": 4, "c": "y", "label": "no"}),
    ("GET", "/extras/computed/dump", None),
    ("POST", "/extras/enums", {"group": ["street", "city"], "rel": "MASTER"}),
    ("POST", "/extras/enums", {"group": [1, "a"]}),
    ("POST", "/extras/enums", {"group": "TEXT"}),
    ("POST", "/extras/xml", {"docs": ["<a><b>1</b><b>2</b><c x='1'>t</c><d/><e x='2'/><f>  sp  </f></a>", '<a>text<b>1</b>tail</a>', '<?xml version=\'1.0\' encoding=\'utf-8\'?>\n<r xmlns=\'http://example.com/a\' xmlns:p=\'http://example.com/p\'>\n  <p:i p:k=\'v\' k="w">1</p:i><j>&amp;&lt;&#233;&#x41;</j><![CDATA[ cd ]]></r>', '<a><!-- c --><b>x</b><?pi x?></a>', '<a><b><c>1</c></b><b>2</b><b/></a>', '<a>  </a>', "<a x='1'>  </a>", "<a x='a\tb'></a>", '\n<a>\r\n x \r\n</a>\n', '<a>', '<a></b>', '', 'x', '<a>&nope;</a>', '<a/><b/>', '<a><b></a>', "<a x='1' x='2'/>", '<p:a/>', '<a>é</a>']}),
    ("POST", "/extras/xml/form", b"doc=%3Cr%3E%3Cs%3E1%3C%2Fs%3E%3C%2Fr%3E", {"content-type": "application/x-www-form-urlencoded"}),
    # the session's connection (fixtures/dynapp/sqlmore.py)
    ("GET", "/sqlm/ready", None),
    ("GET", "/sqlm/closed/none", None),
    ("GET", "/sqlm/closed/commit", None),
    ("GET", "/sqlm/closed/rollback", None),
    ("GET", "/sqlm/closed/close", None),
    ("GET", "/colls/dup/a", None),
    ("GET", "/colls/dup/two/b?n=3", None),
    ("GET", "/colls/dup/two/b?n=x", None),
    ("POST", "/colls/render", {"template": "{street} {number}, {city}", "values": {"street": "Main", "city": "X"}}),
    ("POST", "/colls/render", {"template": "{a!r} {b:>5} {c!s}", "values": {"a": "q", "b": "z"}}),
    ("POST", "/colls/render", {"template": "{}", "values": {}}),
    ("POST", "/colls/render", {"template": "a } b", "values": {}}),
    ("POST", "/colls/render", {"template": "a { b", "values": {}}),
    ("POST", "/colls/render", {"template": "{x!z}", "values": {"x": 1}}),
    ("POST", "/colls/render", {"template": "{0}{}", "values": {}}),
    ("POST", "/colls/format", {"template": "{} {}", "args": [1]}),
    ("POST", "/colls/format", {"template": "{0} {}", "args": [1, 2]}),
    ("POST", "/colls/format", {"template": "{} {0}", "args": [1, 2]}),
    ("POST", "/colls/format", {"template": "{name}", "kw": {}}),
    ("POST", "/colls/format", {"template": "{0!r:>8}|{n:05d}", "args": ["é"], "kw": {"n": 42}}),
    # float presentations g/e (str.format and %)
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0, 123456.0]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0, 1234567.0]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001, 0.0001]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [-2.5, -2.5, -2.5, -2.5, -2.5, -2.5, -2.5, -2.5, -2.5, -2.5, -2.5]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22, 1e+22]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999, 9.9999]}),
    ("POST", "/colls/format", {"template": "{:g}|{:.3g}|{:.1g}|{:e}|{:.2e}|{:G}|{:.0e}|{:10.3g}|{:<8.2g}|{:+.3g}|{:E}", "args": [0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1]}),
    ("POST", "/colls/format", {"template": "{:x}|{:X}|{:o}|{:b}|{:#x}", "args": [255, 255, 8, 5, 1]}),
    ("POST", "/colls/percent", {"fmt": "%g|%.3g|%e|%.2E|%G|%10.3g|%-8.2g|%X|%o", "args": [0.5, 0.0, 1234.5, -0.00012, 1e-07, 3.14159, 2.0, 255, 8]}),
    ("POST", "/colls/percent", {"fmt": "%.3g s", "args": [0.0]}),
    ("POST", "/colls/percent", {"fmt": "%.3g s", "args": [1.5]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [0.0, 0.0, 0.0, 0.0, 0.0]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [1.0, 1.0, 1.0, 1.0, 1.0]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [12.0, 12.0, 12.0, 12.0, 12.0]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [123.0, 123.0, 123.0, 123.0, 123.0]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [0.5, 0.5, 0.5, 0.5, 0.5]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05, 1.234e-05]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [99.95, 99.95, 99.95, 99.95, 99.95]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [1e+16, 1e+16, 1e+16, 1e+16, 1e+16]}),
    ("POST", "/colls/format", {"template": "{:.3}|{:.1}|{:.5}|{:8.2}|{:.2}", "args": [-2.5, -2.5, -2.5, -2.5, -2.5]}),
    ("POST", "/colls/format", {"template": "{:.3}", "args": [5]}),
    # tenacity (fixtures/dynapp/retrying.py)
    ("GET", "/retry/case/flaky?arg=0", None),
    ("GET", "/retry/case/flaky?arg=2", None),
    ("GET", "/retry/case/flaky?arg=3", None),
    ("GET", "/retry/case/reraise", None),
    ("GET", "/retry/case/wrapped", None),
    ("GET", "/retry/case/callback?arg=7", None),
    ("GET", "/retry/case/typed-value", None),
    ("GET", "/retry/case/typed-other", None),
    ("GET", "/retry/case/pred-again", None),
    ("GET", "/retry/case/pred-stop", None),
    ("GET", "/retry/case/result?arg=1", None),
    ("GET", "/retry/case/result?arg=5", None),
    ("GET", "/retry/case/sync", None),
    ("GET", "/retry/case/bare", None),
    # prometheus_client (fixtures/dynapp/prom.py); timer durations only in /prom/timers, last
    ("GET", "/prom/metrics", None),
    ("GET", "/prom/module", None),
    ("POST", "/prom/run", None),
    ("GET", "/prom/metrics", None),
    ("GET", "/prom/nocreated", None),
    ("GET", "/prom/default", None),
    ("GET", "/prom/targeted", None),
    ("GET", "/prom/single", None),
    ("GET", "/prom/errors", None),
    ("POST", "/prom/mutate", None),
    ("GET", "/prom/metrics", None),
    ("POST", "/prom/reset", None),
    ("GET", "/prom/exporter", None),
    ("GET", "/prom/timers", None),
    # lifespan, async generators by hand, asynccontextmanager, suppress, ContextVar, cancel (fixtures/dynapp/life.py)
    ("GET", "/life/state", None),
    ("GET", "/life/acm", None),
    ("GET", "/life/agen", None),
    ("GET", "/life/suppress", None),
    ("GET", "/life/ctxvar", None),
    ("GET", "/life/ctxvar/isolated", None),
    ("GET", "/life/cancel", None),
    # coroutine objects (fixtures/dynapp/aio.py)
    ("GET", "/aio/gather", None),
    ("GET", "/aio/coro", None),
    ("GET", "/aio/acm", None),
    ("GET", "/bg/run", None),
    ("GET", "/bg/run", None),
    ("GET", "/bg/misc", None),
    # composite primary keys (fixtures/dynapp/composite.py)
    ("POST", "/composite/run", None),
    ("GET", "/composite/list", None),
    ("POST", "/composite/tickets", None),
    ("POST", "/composite/upsert", None),
    ("GET", "/composite/adapter", None),
    ("GET", "/composite/mappings", None),
    # project decorators (fixtures/dynapp/decos.py)
    ("GET", "/decos/fetch", None),
    ("GET", "/decos/fetch?n=4&fail=true", None),
    ("GET", "/decos/attrs", None),
    ("GET", "/decos/compute", None),
    ("GET", "/decos/bare", None),
    ("GET", "/decos/step", None),
    ("GET", "/decos/step?b=0", None),
    ("GET", "/decos/nested", None),
    ("GET", "/decos/types", None),
    # third-party libraries (fixtures/dynapp/libs.py)
    ("GET", "/libs/bcrypt", None),
    ("GET", "/libs/bcrypt?pw=%C3%A9t%C3%A9%20%F0%9F%8C%9E", None),
    ("GET", "/libs/totp", None),
    ("POST", "/libs/thread", None),
    ("GET", "/libs/sent", None),
    ("POST", "/libs/alert/one", None),
    ("POST", "/libs/alert/two", None),
    ("GET", "/libs/http", None),
    ("GET", "/outbound/run", None),
    ("GET", "/traced/run", None),
    ("GET", "/traced/run", None),
    ("GET", "/libs/yarl", None),
    # pywebpush: tests/push_sink.py on port 8299 (started by scripts_start_dyn.sh)
    ("POST", "/libs/push?target=http://127.0.0.1:8299/sink", None),
    ("POST", "/libs/push?target=http://127.0.0.1:8299/sink&der=true&size=9000", None),
    ("POST", "/libs/push?target=http://127.0.0.1:8299/sink%3Fgone%3Dtrue", None),
    ("POST", "/libs/push?target=http://127.0.0.1:8299/sink&sub=admin", None),
    ("POST", "/libs/push-errors", None),
    ("POST", "/libs/savepoint", None),
    ("POST", "/libs/savepoint", None),
    ("GET", "/libs/enum", None),
    ("GET", "/libs/enum?rank=high", None),
    ("GET", "/libs/enum?rank=Estar", None),
    ("GET", "/libs/enum?rank=", None),
    ("GET", "/libs/enum?rank=zz", None),
    ("POST", "/libs/enum", {"rank": "ESTAR", "ranks": ["low", "", "estar"]}),
    ("POST", "/libs/enum", {"rank": "bad", "ranks": ["low", "worse"]}),
    ("GET", "/libs/urllib", None),
    ("GET", "/libs/b64", None),
    ("GET", "/libs/psutil", None),
    ("GET", "/libs/sqlmisc", None),
    ("GET", "/libs/google", None),
    ("GET", "/libs/aliased", None),
    ("GET", "/libs/decimal", None),
    ("POST", "/libs/decimal-db", None),
    ("GET", "/libs/formatdate", None),
    ("GET", "/libs/smalllibs", None),
    ("GET", "/libs/unicodedata", None),
    ("GET", "/libs/jsonfmt", None),
    ("GET", "/libs/datereplace", None),
    ("POST", "/libs/lastpositive", {"items": []}),
    ("POST", "/libs/lastpositive", {"items": [1, 2, 3, 4]}),
    ("POST", "/libs/lastpositive", {"items": [1, "x"]}),
    ("POST", "/libs/lastpositive", {"items": [1, -2]}),
    ("POST", "/libs/lastpositive", {"items": [3]}),
    ("POST", "/libs/lastpositive", {"items": ["3", "-2"]}),  # the error's input is the raw list
    ("POST", "/libs/lastpositive", {"items": [1], "tags": [1, 2], "meta": {"a": 1, "b": 2}}),
    ("POST", "/libs/lastpositive", {"items": [1], "tags": [1, 1], "meta": {"a": 1}, "pair": [4], "names": ["x"]}),
    ("POST", "/libs/lastpositive", {"items": [1, "x", 3, 4, 5], "pair": [1, "x", 3], "names": [1, 2, 3]}),
    ("POST", "/libs/lastpositive", {"items": ["x"], "pair": ["y"]}),
    ("POST", "/libs/lastpositive", {"items": [2], "tags": ["x", 1, 2], "pair": [1, 2, 3]}),
    ("GET", "/libs/unicodedata?s=%C3%85ngstr%C3%B6m%20%E2%84%AB%20%EA%9F%B1%20%F0%90%BB%BA%CC%A7", None),
    ("GET", "/libs/shapes", None),
    ("GET", "/libs/ipaddr", None),
    ("GET", "/libs/mimebase", None),
    ("GET", "/libs/template", None),
    ("GET", "/libs/ssrf?host=localhost", None),
    ("GET", "/libs/ssrf?host=127.0.0.1", None),
    ("GET", "/libs/ssrf?host=::1", None),
    ("GET", "/libs/ssrf?host=nonexistent.invalid", None),
    ("POST", "/libs/sanitize", {"html": "<p onclick=x>Hi <a href=\"javascript:alert(1)\">x</a> <a href=cid:abc target=_blank>y</a>"
                                        "<script>bad()</script><!-- c --><b class=\"k z\" style=\"color:red\">b</b>"
                                        "<img src=x onerror=1><style>p{}</style><table><td colspan=2>t</td></table></p>"}),
    ("POST", "/libs/sanitize", {"html": "plain & <text> \"q\" é"}),
    ("GET", "/libs/deque", None),
    ("GET", "/libs/deque", None),
    ("GET", "/libs/deque?key=b", None),
    ("GET", "/libs/deque", None),
    ("POST", "/libs/tagged", {"name": "a", "tags": None}),
    ("POST", "/libs/tagged", {"name": "", "mode": "m", "tags": ["t"]}),
    ("POST", "/libs/tagged", {"name": 5, "tags": ["t"]}),
    ("GET", "/libs/http-auth", None),
    ("POST", "/libs/bill", {"qty": 3, "unit": "12.50"}),
    ("POST", "/libs/bill", {"qty": 10, "unit": 11, "note": "rush", "extra": [1]}),
    ("POST", "/libs/bill", {"qty": 1, "unit": "1", "other": 99}),
    ("POST", "/libs/bill", {"qty": "x"}),
    ("POST", "/libs/bill-echo", {"qty": 9, "unit": "12", "note": "n"}),
    ("POST", "/libs/decimal-in", {"amount": 1}),
    ("POST", "/libs/decimal-in", {"amount": 1.1}),
    ("POST", "/libs/decimal-in", {"amount": "1.10"}),
    ("POST", "/libs/decimal-in", {"amount": " 2.5 "}),
    ("POST", "/libs/decimal-in", {"amount": "1_000"}),
    ("POST", "/libs/decimal-in", {"amount": "1e3"}),
    ("POST", "/libs/decimal-in", {"amount": 1e15}),
    ("POST", "/libs/decimal-in", {"amount": 0.30000000000000004}),
    ("POST", "/libs/decimal-in", {"amount": "-0"}),
    ("POST", "/libs/decimal-in", {"amount": 1e-7}),
    ("POST", "/libs/decimal-in", {"amount": "abc"}),
    ("POST", "/libs/decimal-in", {"amount": True}),
    ("POST", "/libs/decimal-in", {"amount": None}),
    ("POST", "/libs/decimal-in", {"amount": [1]}),
    ("POST", "/libs/decimal-in", {"amount": "NaN"}),
    ("POST", "/libs/decimal-in", {"amount": "-Infinity"}),
    ("POST", "/libs/decimal-in", {"amount": "snan"}),
    ("POST", "/libs/decimal-in", {}),
    ("POST", "/libs/decimal-in", {"amount": 1, "fee": "0.5"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "0"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": -1.5}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "600"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "500.5"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "1E+3"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "1E+2"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "123.456"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "12.345"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "100.000"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "1.2300"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "capped": "-1234.567"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "small": 0.05}),
    ("POST", "/libs/decimal-in", {"amount": 1, "small": 10}),
    ("POST", "/libs/decimal-in", {"amount": 1, "small": "9.95"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "small": "9.90"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "digits": "0.00001"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "digits": "1E+1"}),
    ("POST", "/libs/decimal-in", {"amount": 1, "digits": 0}),
    ("POST", "/libs/decimal-in", {"amount": "x", "fee": "y", "capped": 0}),
    ("POST", "/libs/decimal-echo", {"amount": "1.10", "fee": 2, "small": 0.5}),
    ("POST", "/libs/decimal-echo", {"amount": 1e20, "capped": "1.5"}),
    ("POST", "/libs/decimal-out", {"amount": 1e20, "fee": 0.1, "capped": "2"}),
    ("POST", "/libs/decimal-out", {"amount": "bad"}),
    ("GET", "/libs/sqlmore", None),
    ("GET", "/libs/sets", None),
    ("GET", "/libs/startup", None),
    ("GET", "/libs/numtypes", None),
    ("GET", "/libs/script-settings", None),
    ("GET", "/libs/none-settings", None),
    ("GET", "/libs/jose-options", None),
    ("POST", "/libs/urls", {"site": "http://Example.COM/a b?x=1#frag", "any": "x:y", "redis": "redis://h"}),
    ("POST", "/libs/urls", {"site": "https://a.b:443/"}),
    ("POST", "/libs/urls", {"site": "ftp://x", "any": "http://", "redis": "http://x"}),
    ("POST", "/libs/urls", {"site": 5, "any": "nope", "redis": "redis://"}),
    ("POST", "/libs/urls", {"site": "http://" + "a" * 2100 + ".com"}),
    ("GET", "/libs/urllib?s=%F0%9F%8C%9E%20x%3B%3A%40%26%3D%2B%24%2C", None),
    # Base.metadata.create_all compiled at translation time (fixtures/dynapp/ddl.py): the catalog after each run
    ("POST", "/ddl/fresh", None),
    ("POST", "/ddl/again", None),
    ("POST", "/ddl/partial", None),
    ("POST", "/ddl/clash", None),
    ("POST", "/ddl/fresh", None),
    # small ATOM apps (fixtures/dynapp/small.py)
    ("GET", "/small/html?name=<b>", None),
    ("GET", "/small/html-kinds/bytes", None),
    ("GET", "/small/html-kinds/none", None),
    ("GET", "/small/html-kinds/obj", None),
    ("GET", "/small/html-kinds/dict", None),
    ("GET", "/small/html-kinds/texte", None),
    ("POST", "/small/html-created", None),
    ("DELETE", "/small/html-gone", None),
    ("GET", "/small/html-status", None),
    ("GET", "/small/llms.txt", None),
    ("GET", "/small/raw", None),
    ("GET", "/small/go", None),
    ("GET", "/small/go?to=/ailleurs", None),
    ("GET", "/small/go-found", None),
    ("GET", "/small/file", None),
    ("POST", "/small/analyses", {"address": "1 rue A", "docs": [{"name": "pv"}]}),
    ("POST", "/small/analyses", {"id": "11111111-2222-4333-8444-555555555555", "address": "2 rue B", "docs": [{"name": "pv", "ref": "AAAAAAAA-BBBB-4CCC-9DDD-EEEEEEEEEEEE"}, {"name": "charges"}]}),
    ("POST", "/small/analyses", {"id": "{aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee}", "address": "3 rue C"}),
    ("POST", "/small/analyses", {"id": "urn:uuid:aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee", "address": "dup"}),
    ("POST", "/small/analyses", {"id": 5, "address": "x", "docs": [{"name": "a", "ref": "1234"}, {"name": "b", "ref": "1234567g-1234-5678-1234-567812345678"}]}),
    ("POST", "/small/analyses", {"id": "12345678-1234-5678-1234567812345678", "address": "x", "docs": [{"name": "a", "ref": ""}, {"name": "b", "ref": "12345678-1234-5678-1234-56781234567"}, {"name": "c", "ref": "ééé"}]}),
    ("GET", "/small/analyses/11111111-2222-4333-8444-555555555555", None),
    ("GET", "/small/analyses/aaaaaaaabbbb4ccc9dddeeeeeeeeeeee", None),
    ("GET", "/small/analyses/00000000-0000-0000-0000-000000000000", None),
    ("GET", "/small/analyses/nope", None),
    ("GET", "/small/analyses?ids=11111111-2222-4333-8444-555555555555&ids=aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee", None),
    ("GET", "/small/analyses?ids=11111111-2222-4333-8444-555555555555&ids=bad", None),
    ("GET", "/small/analyses", None),
    ("GET", "/small/uuid-ops/11111111-2222-4333-8444-555555555555?other=11111111222243338444555555555555", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=%7B1234567g-1234-5678-1234-567812345678%7D", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=urn:uuid:1234567-81234-5678-1234-567812345678", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=%7Baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa%7D", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=a%C3%A9", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=%7B12345678-1234-5678-1234-5678123456789%7D", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=urn:uuid:12345678-1234-5678-1234-56781234567", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=%7B1234%7D", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=------------------------------------", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=12345678-1234-5678-1234-567812345678%7D", None),
    ("GET", "/small/uuid-ops/aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee?other=urn:uuid:aaaaaaaa-bbbb-4ccc-9ddd-eeeeeeeeeeee", None),
    ("GET", "/small/basic-opt", None),
    ("GET", "/small/basic-opt", None, {"authorization": 'Bearer x'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic !!!'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic YWRtaW46czNjcmV0'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'basic YWRtaW46czNjcmV0'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic YWRtaW46bm9wZQ=='}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic bm9jb2xvbg=='}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic w6k6eA=='}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic dXNlcjpwYXN'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic\tYWRtaW46czNjcmV0'}),
    ("GET", "/small/basic-opt", None, {"authorization": 'Basic'}),
    ("GET", "/small/pages", None),
    ("GET", "/small/pages", None, {"authorization": 'Bearer x'}),
    ("GET", "/small/pages", None, {"authorization": 'Basic !!!'}),
    ("GET", "/small/pages", None, {"authorization": 'Basic YWRtaW46czNjcmV0'}),
    ("GET", "/small/pages/11111111-2222-4333-8444-555555555555", None, {"authorization": 'Basic YWRtaW46czNjcmV0'}),
    ("GET", "/small/pages/00000000-0000-0000-0000-000000000000", None, {"authorization": 'Basic YWRtaW46czNjcmV0'}),
    ("GET", "/small/pages/11111111-2222-4333-8444-555555555555", None),
    ("GET", "/small/pages-txt", None),
    ("GET", "/small/pages-txt?y=<b>", None),
    ("GET", "/small/pages-missing", None),
]


# fixtures/dynapp/wsock.py: WebSocket routes (tests/conformance.py ws_step), then what the server saw
LOG = ("GET", "/ws/log", None)
STEPS += [
    ("WS", "/ws/echo", [("send", "a"), ("recv", 1), ("send", "b"), ("recv", 1)]), LOG,
    ("WS", "/ws/echo", [("send", "x"), ("recv", 1), ("close", 4000, "client bye")]), LOG,
    ("WS", "/ws/echo", [("send", "é" * 3000), ("recv", 1), ("close",)]), LOG,
    ("WS", "/ws/json", [("send", '{"a": [1, 2.0, null], "b": "\\u00e9"}'), ("recv", 1), ("send", b'{"k": true}'), ("recv", 2)]), LOG,
    ("WS", "/ws/json", [("send", "not json"), ("recv", 1)]), LOG,
    ("WS", "/ws/bytes", [("send", b"\x01\x02\x03"), ("recv", 1), ("send", "t"), ("recv", 3)]), LOG,
    ("WS", "/ws/bytes", [("send", "text, not bytes"), ("recv", 1)]), LOG,
    ("WS", "/ws/refuse", []), LOG,
    ("WS", "/ws/boom-before", []),
    ("WS", "/ws/boom-after", [("recv", 2)]),
    ("WS", "/ws/noaccept", []),
    ("WS", "/ws/noclose", [("recv", 2)]),
    ("WS", "/ws/http-exc-before", []),
    ("WS", "/ws/http-exc-after", [("recv", 1)]),
    ("WS", "/ws/wsexc-before", []),
    ("WS", "/ws/wsexc-after", [("recv", 1)]),
    ("WS", "/ws/handled", []),
    ("WS", "/ws/handled?after=true", [("recv", 1)]),
    ("WS", "/ws/handled?after=maybe", []),
    ("WS", "/ws/denial", []), LOG,
    ("WS", "/ws/items/3?n=4", [("recv", 2)], {"x-token": "tok"}),
    ("WS", "/ws/items/3?n=4&tag=a&tag=b", [("recv", 2)]),
    ("WS", "/ws/items/x?n=4", []),
    ("WS", "/ws/items/3?n=0", []),
    ("WS", "/ws/items/3", []),
    ("WS", "/ws/deps?token=ok", [("recv", 2)]), LOG,
    ("WS", "/ws/deps?token=bad", []), LOG,
    ("WS", "/ws/deps", []), LOG,
    ("WS", "/ws/conn/caf%C3%A9?q=1&q=2&r=%20x", [("recv", 2)], {"x-token": "t", "cookie": "a=1; b=\"two\"", "subprotocols": ["other", "chat"]}),
    ("WS", "/ws/iter", [("send", "a"), ("recv", 1), ("send", "b"), ("recv", 1), ("close", 1001, "away")]), LOG,
    ("WS", "/ws/iter", [("close",)]), LOG,
    ("WS", "/ws/iter-json", [("send", '{"v": 2}'), ("recv", 1), ("send", '{"v": 3.5}'), ("recv", 1), ("close",)]), LOG,
    ("WS", "/ws/iter-bytes", [("send", b"abc"), ("send", b""), ("sleep", 0.1), ("close", 4999)]), LOG,
    *([] if _STARLETTE < (1, 7) else [  # WebSocketDisconnected: Starlette 1.7 (older ones are refused for WebSockets)
        ("WS", "/ws/late", [("recv", 1), ("close", 4002, "later")]), LOG,
        ("WS", "/ws/misuse", [("recv", 2)]), LOG,
    ]),
    ("WS", "/ws/abc/tail", [("recv", 2)]),
    ("WS", "/ws-app/kitchen", [("recv", 2)]),
    ("WS", "/ws/http-only", []),
    ("WS", "/ws/nowhere/at/all", []),
    ("WS", "/ws/echo/", []),
    ("GET", "/ws/echo", None),
    ("GET", "/ws/http-only", None),
    ("GET", "/ws/async-for", None),
    ("GET", "/ws/annotated-default", None),
    ("GET", "/ws/annotated-default?n=0", None, {"x-token": "h"}),
    ("GET", "/ws/annotated-default?n=7", None),
    ("GET", "/ws/urlpath/caf%C3%A9%20x?q=%20", None),
    ("GET", "/ws/urlpath/caf%C3%A9%20x%2Fy", None),
    LOG,
]


def _reset_broker() -> None:
    """Delete the test queues of fixtures/dynapp/amqp.py; waits up to 60 s for the broker (CI service start)."""
    import asyncio
    import time

    import aio_pika

    async def run():
        url = os.environ.get("BROKER_DSN", "amqp://guest:guest@localhost:5672/")
        for attempt in range(60):
            try:
                conn = await aio_pika.connect(url)
                break
            except (aio_pika.exceptions.AMQPConnectionError, OSError):
                if attempt == 59:
                    raise
                time.sleep(1)
        async with conn:
            ch = await conn.channel()
            for name in ("alpha",):
                await ch.queue_delete(f"py2axum_test_{name}")

    asyncio.run(run())


def reset(db: str) -> None:
    from sqlalchemy import create_engine

    from fixtures.dynapp.models import Base

    # FileResponse: a file both servers serve, with a fixed mtime (etag, last-modified)
    os.makedirs("storage-test", exist_ok=True)
    with open("storage-test/fixed.pdf", "wb") as f:
        f.write(b"%PDF-1.4 fixed")
    os.utime("storage-test/fixed.pdf", (1700000000.123456, 1700000000.123456))
    import redis as _redis
    _redis.Redis.from_url(os.environ.get("REDIS_URL", "redis://127.0.0.1:6379/13")).flushdb()
    _reset_broker()
    engine = create_engine(db.replace("postgresql://", "postgresql+psycopg://", 1))
    Base.metadata.create_all(engine)  # once; afterwards only emptied (servers cache their plans)
    with engine.begin() as conn:
        conn.exec_driver_sql(f"TRUNCATE {', '.join(t.name for t in Base.metadata.sorted_tables)} RESTART IDENTITY CASCADE")
    engine.dispose()


def normalize_text(text: str) -> str:
    """prometheus_client: the `_created` series and the exemplars hold instants, masked when they have their
    shape."""
    import re as _re
    text = _re.sub(r"(?m)^(\S+_created(?:\{.*\})?) \d\.\d+e\+09$", r"\1 <created>", text)
    return _re.sub(r"(?m)^(.* # \{.*\} \S+) \d{10}\.\d+$", r"\1 <timestamp>", text)


def normalize(body):
    """MIME boundaries are random (email.generator): masked. prometheus_client texts: see normalize_text."""
    import re as _re
    if isinstance(body, dict) and "events" in body and "metrics" in body:
        body = dict(body, metrics=normalize_text(body["metrics"]))
    if isinstance(body, dict) and "texts" in body and "om" in body:
        def walk(v):
            return normalize_text(v) if isinstance(v, str) else [walk(x) for x in v] if isinstance(v, list) else v

        def families(text):
            # a restricted registry collects a set of collectors: CPython's order follows their hashes
            return "".join(sorted(f for f in _re.split(r"(?m)^(?=# HELP )", text.removesuffix("# EOF\n")) if f))
        body = {k: walk(v) for k, v in body.items()}
        body["texts"] = [t[:4] + [families(t[4])] if "name[]" in str(t[0]) else t for t in body["texts"]]
        body["restricted"] = [families(t) for t in body["restricted"]]
    if isinstance(body, dict) and set(body) == {"slots", "proto4"}:
        body = {k: _unpickled(v) for k, v in body.items()}
    if isinstance(body, dict) and isinstance(body.get("text"), str):
        body["text"] = _re.sub(r"={15}\d{19}==", "<boundary>", body["text"])
    return _normalize(body)


def _normalize(body):
    """Known, documented difference (README): the decoder message of a `json_invalid` error comes from
    CPython's json module (and changes between Python versions); type, msg and loc are compared."""
    if isinstance(body, dict) and isinstance(body.get("detail"), list):
        for e in body["detail"]:
            if isinstance(e, dict) and e.get("type") == "json_invalid" and "ctx" in e:
                e["ctx"] = {"error": "<decoder message>"}
    return body

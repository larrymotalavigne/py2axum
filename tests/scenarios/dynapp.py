"""Scenario of fixtures/dynapp: the dyn backend's own reference app (enums, computed defaults,
annotated dependency aliases...). Tables are (re)created by SQLAlchemy from the fixture models.

The Python language, its standard library and Pydantic are in tests/scenarios/langapp.py, the libraries and the
platform in tests/scenarios/integapp.py (fixtures/langapp, fixtures/integapp: one crate no longer compiled within
the CI build containers' 6 GiB); they share these tables (`reset_tables`)."""
import os
from importlib.metadata import version as _version

# get_db commits after the response (FastAPI >= 0.121): pause after each write so that the reference has
# committed before the next request reads (the binary commits before answering)
SETTLE = 0.2
# the pause only after the writes that can do work after their response (tests/settle.py)
REFERENCE_APP = "fixtures.dynapp.main:app"


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
    # return_timestamp=, payload and date_signed of the exceptions (09/10/2026)
    future = 4102444800
    stamped = [
        (ser(old).dumps({"user_id": 7, "purpose": "2fa"}), ""),
        (ser(old).dumps({"user_id": 7, "purpose": "2fa"}), "&positional=true"),
        (ser(old).dumps({"m": 1}, salt="magic"), "&salt=magic"),
        (ser(old).dumps({"m": 1}, salt="magic"), "&salt=magic&positional=true"),
        (ser(old).dumps({"m": 1}, salt="magic"), ""),  # wrong salt: BadTimeSignature with date_signed
        (ser(old).dumps({"user_id": 7}), "&max_age=60"),  # SignatureExpired.date_signed
        (ser(old).dumps({"user_id": 7}), "&max_age=60&positional=true"),
        (ser(future).dumps({"user_id": 7}), "&max_age=60"),  # age < 0
        (ser(0).dumps({"epoch": True}), ""),
        (ser(253_402_300_799).dumps([1]), ""),  # the last second datetime takes
        (ser(old).dumps({"a": 1})[:-2] + "xx", ""),
        (ser(old).dumps({"a": 1})[:-2] + "xx", "&max_age=60"),
        ("nodot", ""),
        ("a.b", ""),  # timestamp missing (signature error first)
        (raw.sign(b"e30").decode().rsplit(".", 2)[0] + "..sig", ""),
        (raw.sign(b"e30").decode().rsplit(".", 2)[0] + ".AQAAAAAAAAAA.sig", ""),  # bad sig, a 9-byte timestamp: no date (struct.error swallowed)
        (raw.sign(b"e30").decode().rsplit(".", 2)[0] + ".AAAAAAAAAAAAAAE.sig", ""),  # 11 bytes, even with leading zeros: no date
        (raw.sign(b"e30").decode().rsplit(".", 2)[0] + ".f_________8.sig", ""),  # past year 9999: malformed timestamp
        (raw.sign(b"e30").decode().rsplit(".", 2)[0] + ".!!.sig", ""),  # undecodable timestamp: no date_signed
        (raw.sign(b"!!!").decode(), ""),
        (raw.sign(b".bm90IHpsaWI").decode(), ""),
    ]
    return [("GET", f"/its/check?token={quote(t)}{q}", None) for t, q in tokens] + [
        ("GET", f"/its/stamped?token={quote(t)}{q}", None) for t, q in stamped] + [
        ("POST", "/its/fresh", {"user_id": 1, "purpose": "2fa"}),
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
    ("GET", "/tasks/2147483648", None),  # beyond INTEGER: DataError, as psycopg casts the parameter
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
    # pydantic 2.14 refuses CR/LF before parsing (2.13: stripped by `.strip()` or a pretty-form separator)
    *[("POST", "/contacts/loud", {"email": e}) for e in (" ada@example.com\n", "Ada <ada@example.com>", "ada@ex\na.com")],
    *[("POST", "/contacts", {"name": "Ada", "email": e}) for e in ("ada@example.com\n", "Ada\n<ada@example.com>", "\r\nada@example.com", "ad\na@example.com")],
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
    ("GET", "/greenlet/read/column", None),
    ("GET", "/greenlet/read/key", None),
    ("GET", "/greenlet/read/deferred", None),
    ("GET", "/greenlet/read/m2o", None),
    ("GET", "/greenlet/read/o2m", None),
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
    ("GET", "/paged", None),
    ("GET", "/paged?page=2&page_size=5&q=abc&exact=true", None),
    ("GET", "/paged?page=0&page_size=101", None),
    ("GET", "/paged?page=abc&q=toolong&exact=maybe", None),
    ("POST", "/validated", {"id": 3, "name": " Ada "}),
    ("POST", "/validated", {"id": "x", "name": "Ada"}),
    ("POST", "/archives", {"name": "a", "blob": "deferred bytes"}),
    ("GET", "/archives-report", None),
    ("GET", "/archives/1/out", None),
    ("GET", "/projects/1/refreshed", None),
    ("GET", "/export.csv", None),
    ("GET", "/export.csv?direct=true", None),
    # malformed JSON bodies: json.JSONDecodeError's position (in characters) and message, in loc and ctx
    ("POST", "/archives", ''.encode()),
    ("POST", "/archives", ' '.encode()),
    ("POST", "/archives", 'not json'.encode()),
    ("POST", "/archives", '{'.encode()),
    ("POST", "/archives", '{"a"'.encode()),
    ("POST", "/archives", '{"a":'.encode()),
    ("POST", "/archives", '{"a":1'.encode()),
    ("POST", "/archives", '{"a":1,'.encode()),
    ("POST", "/archives", '{"a":1,}'.encode()),
    ("POST", "/archives", '{a:1}'.encode()),
    ("POST", "/archives", '{"a" 1}'.encode()),
    ("POST", "/archives", '{"a":1 "b":2}'.encode()),
    ("POST", "/archives", '['.encode()),
    ("POST", "/archives", '[1'.encode()),
    ("POST", "/archives", '[1,'.encode()),
    ("POST", "/archives", '[1,]'.encode()),
    ("POST", "/archives", '[1 2]'.encode()),
    ("POST", "/archives", '"abc'.encode()),
    ("POST", "/archives", '"a\\x"'.encode()),
    ("POST", "/archives", '"a\\u12g4"'.encode()),
    ("POST", "/archives", '"a\tb"'.encode()),
    ("POST", "/archives", '1 2'.encode()),
    ("POST", "/archives", '{"a":1}x'.encode()),
    ("POST", "/archives", '-'.encode()),
    ("POST", "/archives", '01'.encode()),
    ("POST", "/archives", '1.'.encode()),
    ("POST", "/archives", 'tru'.encode()),
    ("POST", "/archives", '[-]'.encode()),
    ("POST", "/archives", '{"a":[1,{"b":}]}'.encode()),
    ("POST", "/archives", '\ufeff{}'.encode()),
    ("POST", "/archives", '{"é":"ü",}'.encode()),
    ("POST", "/archives", '{"a":1}\n\n x'.encode()),
    ("GET", "/archives/1", None),
    ("GET", "/archives/1/lazy", None),
    ("GET", "/archives/9", None),
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
    ("GET", "/mail/filters", None),
    ("GET", "/mail/filters?name=x&amount=0.5", None),
    ("GET", "/mail/build", None),
    ("GET", "/mail/build?to=j%C3%B6hn%40ex%C3%A4mple.com", None),
    ("GET", "/mail/build?to=J%C3%B6hn%20%3Cjohn%40example.com%3E", None),
    *[("GET", f"/mail/build?subject={s}", None) for s in (
        "D%C3%A9claration%20revenus%20fonciers%202025%20%E2%80%94%20Ana%C3%AFs%20Lef%C3%A8vre-Dupr%C3%A9",
        "%E2%82%AC" * 40, "a%20b%20c%20%C3%A9" * 30, "ligne%20une%20%C3%A9%0Aligne%20deux")],
    ("GET", "/mail/build?name=" + "%C3%89quipe%20Easy%20" * 8, None),
    ("POST", "/mail/send?to=nobody@example.com", None),
    ("POST", "/mail/send?to=nobody@example.com&envelope=true", None),  # no SMTP server in the scenario: the error path
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
    # the session's connection (fixtures/dynapp/sqlmore.py)
    # EXISTS / scalar subqueries correlated in a statement that joins their tables (sqlmore.py)
    ("GET", "/sqlm/correlate/join_has", None),
    ("GET", "/sqlm/correlate/join_has_kw_crit", None),
    ("GET", "/sqlm/correlate/join_has_other", None),
    ("GET", "/sqlm/correlate/join_any", None),
    ("GET", "/sqlm/correlate/nested", None),
    ("GET", "/sqlm/correlate/nested_plain", None),
    ("GET", "/sqlm/correlate/exists_auto", None),
    ("GET", "/sqlm/correlate/exists_correlate", None),
    ("GET", "/sqlm/correlate/exists_except", None),
    ("GET", "/sqlm/correlate/exists_except_none", None),
    ("GET", "/sqlm/correlate/scalar_auto", None),
    ("GET", "/sqlm/correlate/scalar_except", None),
    ("GET", "/sqlm/correlate/scalar_except_after", None),
    ("GET", "/sqlm/ready", None),
    ("GET", "/sqlm/closed/none", None),
    ("GET", "/sqlm/closed/commit", None),
    ("GET", "/sqlm/closed/rollback", None),
    ("GET", "/sqlm/closed/close", None),
    ("GET", "/sqlm/session", None),
    ("GET", "/sqlm/concat", None),
    # lifespan, async generators by hand, asynccontextmanager, suppress, ContextVar, cancel (fixtures/dynapp/life.py)
    ("GET", "/life/state", None),
    ("GET", "/life/acm", None),
    ("GET", "/life/agen", None),
    ("GET", "/life/suppress", None),
    ("GET", "/life/ctxvar", None),
    ("GET", "/life/ctxvar/isolated", None),
    ("GET", "/life/cancel", None),
    # coroutine objects (fixtures/dynapp/aio.py)
    ("GET", "/aio/sleep/20", None),
    ("GET", "/aio/gather", None),
    ("GET", "/aio/coro", None),
    ("GET", "/aio/acm", None),
    # composite primary keys (fixtures/dynapp/composite.py)
    ("POST", "/composite/run", None),
    ("GET", "/composite/list", None),
    ("POST", "/composite/tickets", None),
    ("POST", "/composite/upsert", None),
    ("GET", "/composite/adapter", None),
    ("GET", "/composite/mappings", None),
    # Base.metadata.create_all compiled at translation time (fixtures/dynapp/ddl.py): the catalog after each run
    # Enum columns of every SQLAlchemy flavour read into str / Enum / Literal / use_enum_values / int / float / bool
    ("POST", "/enumcols", {"pri": "high", "state": "done", "chan": "sms", "lvl": 2, "pri_val": "low", "maybe": None}),
    ("POST", "/enumcols", {"pri": "low", "state": "open", "chan": "mail", "lvl": 1, "pri_val": "medium", "maybe": "high"}),
    ("GET", "/enumcols/raw", None),
    ("GET", "/enumcols/1/str", None),
    ("GET", "/enumcols/1/enum", None),
    ("GET", "/enumcols/1/use", None),
    ("GET", "/enumcols/1/literal", None),
    ("GET", "/enumcols/1/numbers", None),
    ("GET", "/enumcols/1/short", None),
    ("GET", "/enumcols/1/validate", None),
    ("GET", "/enumcols/2/str", None),
    ("GET", "/enumcols/2/enum", None),
    ("GET", "/enumcols/2/use", None),
    ("GET", "/enumcols/2/literal", None),
    ("GET", "/enumcols/2/numbers", None),
    ("GET", "/enumcols/2/short", None),
    ("GET", "/enumcols/2/validate", None),
    ("GET", "/enumcols/3/str", None),
    # a session maker wrapper's sessions take its engine's connect_args (fixtures/dynapp/makerwrap.py)
    ("GET", "/mkw/settings", None),
    ("POST", "/ddl/fresh", None),
    ("POST", "/ddl/readings", None),
    ("POST", "/ddl/again", None),
    ("POST", "/ddl/partial", None),
    ("POST", "/ddl/clash", None),
    ("POST", "/ddl/fresh", None),
]
STEPS += [    ("GET", "/owners-cls/slug?q=%1Fa%1C%20b%1E", None),  # U+001C..U+001F are Python whitespace
    ("GET", "/ws/urlpath/%0Ax%09y", None),  # request.url.path: urlsplit drops tabs and newlines
    # SQLAlchemy's psycopg dialect casts the parameter (`::INTEGER`): out of range, a DataError (500)
    ("GET", "/tasks/2147483648", None),
    # Starlette strips every trailing slash before its redirect: /tasks/%2F -> /tasks// -> /tasks
    ("GET", "/tasks/%2F", None),
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

# fixtures/dynapp/leaks.py: shapes that made reference cycles (nested function naming itself, objects left linked
# by a failed flush, a rollback, a DELETE); the same answers as before the cycles were broken
STEPS += [
    ("GET", "/leak/selfref", None),
    ("GET", "/leak/selfref?n=0", None),
    ("GET", "/leak/selfref_cell", None),
    ("GET", "/leak/selfref_escape", None),
    ("POST", "/leak/dup_flush", None),
    ("POST", "/leak/rolled_back", None),
    ("POST", "/leak/deleted", None),
    ("GET", "/leak/task_state", None),
    ("GET", "/leak/task_failed_state", None),
    ("GET", "/leak/task_failed_state?n=7", None),
    ("GET", "/leak/agen_state", None),
    ("GET", "/leak/agen_state?n=1", None),
    ("GET", "/leak/agen_state?start=false", None),
    ("POST", "/leak/rolled_back/state", None),
    ("POST", "/leak/rolled_back/state?delete=true", None),
    ("POST", "/leak/dup_flush/state", None),
    ("GET", "/leak/stream_dep?n=3&pause=0", None),
]

# last: they rename task 2
STEPS += [
    ("POST", "/tasks/2/returning", {"title": "Renamed by RETURNING"}),
    ("POST", "/tasks/999/returning", {"title": "nobody"}),
]


def fixed_file() -> None:
    """FileResponse: a file both servers serve, with a fixed mtime (etag, last-modified)"""
    os.makedirs("storage-test", exist_ok=True)
    with open("storage-test/fixed.pdf", "wb") as f:
        f.write(b"%PDF-1.4 fixed")
    os.utime("storage-test/fixed.pdf", (1700000000.123456, 1700000000.123456))


def reset_tables(db: str) -> None:
    """dynapp's tables, also those of fixtures/langapp and fixtures/integapp, emptied."""
    from sqlalchemy import create_engine

    import fixtures.dynapp.inherit  # noqa: F401  (its tables are on the same Base)
    from fixtures.dynapp.models import Base

    engine = create_engine(db.replace("postgresql://", "postgresql+psycopg://", 1))
    Base.metadata.create_all(engine)  # once; afterwards only emptied (servers cache their plans)
    with engine.begin() as conn:
        conn.exec_driver_sql(f"TRUNCATE {', '.join(t.name for t in Base.metadata.sorted_tables)} RESTART IDENTITY CASCADE")
    engine.dispose()


def reset(db: str) -> None:
    fixed_file()
    reset_tables(db)

# ---- raw ASGI middlewares (fixtures/dynapp/asgimw.py): stack order, response headers added and removed, framing
# (content-length dropped), ContextVar and request.state, short-circuits, receive() wrappers, rewritten scopes,
# exceptions around the app; starlette_context's RawContextMiddleware on every request (generated ids compared
# by shape)
HEADER_SHAPES = {"x-request-id": r"[0-9a-f]{32}", "x-correlation-id": r"[0-9a-f]{32}"}
FRAMING_PREFIXES = ("/mw/",)
_U1 = "0f4c2f2e-6f6b-4f1a-9a49-3c1d2b9e8a71"
STEPS += [
    ("GET", "/mw/plain", None),
    ("GET", "/mw/inflight", None),
    ("GET", "/mw/ctx", None),
    ("GET", "/mw/ctx", None, {"x-rid": "abc-123"}),
    ("GET", "/mw/ctx/change", None, {"x-rid": "r1"}),
    ("GET", "/mw/nolength", None),
    ("GET", "/mw/headers", None),
    ("GET", "/mw/headers", None, {"x-strip": "x-internal"}),
    ("GET", "/mw/headers", None, {"x-strip": "x-internal, x-public, content-type"}),
    ("GET", "/mw/plain", None, {"x-strip": "x-mw-order"}),
    ("GET", "/mw/plain", None, {"x-block": "send"}),
    ("GET", "/mw/plain", None, {"x-block": "response"}),
    ("GET", "/mw/plain", None, {"x-block": "text"}),
    ("GET", "/mw/plain", None, {"x-block": "stream"}),
    ("GET", "/mw/plain", None, {"x-block": "empty"}),
    ("GET", "/tasks/1", None, {"x-block": "send"}),
    ("POST", "/mw/body", b"hello body"),
    ("POST", "/mw/body", b"hello body", {"x-upper": "1"}),
    ("POST", "/mw/body", b""),
    ("POST", "/mw/body/item", {"title": "pen", "qty": 3}),
    ("POST", "/mw/body/item", {"title": "pen", "qty": 3}, {"x-upper": "1"}),
    ("POST", "/mw/body/item", b"not json"),
    ("GET", "/mw/old/thing", None),
    ("GET", "/mw/old/caf%C3%A9", None),
    ("GET", "/mw/old/", None),
    ("GET", "/mw/method", None),
    ("GET", "/mw/plain", None, {"x-upper-out": "1"}),
    ("GET", "/mw/stream", None),
    ("GET", "/mw/stream", None, {"x-upper-out": "1"}),
    ("GET", "/mw/stream", None, {"x-strip": "content-type"}),
    ("GET", "/mw/missing", None),
    ("GET", "/mw/missing", None, {"x-upper-out": "1"}),
    ("GET", "/mw/nothing-here", None),
    ("GET", "/mw/raise", None, {"x-boom": "catch"}),
    ("GET", "/mw/raise/runtime", None, {"x-boom": "catch"}),
    ("GET", "/mw/plain", None, {"x-boom": "before"}),
    ("GET", "/mw/plain", None, {"x-boom": "after"}),
    ("GET", "/mw/plain", None),
    ("GET", "/mw/raise", None),
    ("GET", "/mw/plain", None, {"x-request-id": _U1}),
    ("GET", "/mw/plain", None, {"x-request-id": _U1.replace("-", "").upper(), "x-correlation-id": "{" + _U1 + "}"}),
    ("GET", "/mw/plain", None, {"x-request-id": "urn:uuid:" + _U1}),
    ("GET", "/mw/plain", None, {"x-request-id": "not-a-uuid"}),
    ("GET", "/mw/plain", None, {"x-correlation-id": "1234"}),
    ("GET", "/mw/plain", None, {"x-request-id": ""}),
    ("GET", "/tasks/1", None, {"x-request-id": "0x" + "0" * 30}),
    ("GET", "/tasks/1", None, {"x-request-id": "+" + "a" * 31}),
    ("GET", "/tasks/1", None, {"x-request-id": "-" + "0" * 31}),
    ("GET", "/tasks/1", None, {"x-request-id": "-" + "0" * 30 + "1"}),
    ("GET", "/tasks/1", None, {"x-request-id": "a_" + "b" * 30}),
    ("GET", "/tasks/1", None, {"x-request-id": "g" * 32}),
    ("GET", "/mw/inflight", None),
]
# gzip.decompress: members, padding, header flags, truncated/corrupted inputs
_GZ = __import__("gzip").compress(b"hello gzip " * 50, mtime=0)
_GZ2 = __import__("gzip").compress("deux é".encode(), mtime=0)
_GZN = b"\x1f\x8b\x08\x08\x00\x00\x00\x00\x00\x03name.txt\x00" + _GZ2[10:]
STEPS += [("POST", "/mw/gunzip", body) for body in (
    _GZ, _GZ + _GZ2, _GZ + b"\x00\x00" + _GZ2, _GZN, b"", b"\x1f", b"plain text", _GZ[:-3], _GZ[:20],
    _GZ[:-8] + b"\x00\x00\x00\x00" + _GZ[-4:], _GZ[:-4] + b"\x01\x00\x00\x00", _GZ[:10] + b"\xff" * 12 + _GZ[-8:],
    b"\x1f\x8b\x07" + _GZ[3:], _GZ + b"x",
)]


def normalize(body):
    """MIME boundaries are random (email.generator): masked."""
    import re as _re
    if isinstance(body, dict) and isinstance(body.get("text"), str):
        body["text"] = _re.sub(r"={15}\d{19}==", "<boundary>", body["text"])
    return body


# documented differences of the log conformance (tests/logcmp.py): records left out on both sides
LOG_IGNORE = [
    # docs/tutorial/websockets.md: the raw ASGI middlewares do not see WebSocket connections (their frames are
    # missing from the traceback of a WebSocket endpoint's exception)
    r'File "[^"]*/fixtures/dynapp/wsock\.py"',
    # docs/tutorial/middleware.md: raised once the response has started (a middleware after the response, a body
    # that fails while it streams): no frame of the raw ASGI middlewares around it
    r"RuntimeError: boom after the response",
    # the WebSocket case above, the binary's side (its traceback has no frame of wsock.py: the HTTPException is
    # raised before the context chain Python prints)
    r"Expected ASGI message 'websocket.send' or 'websocket.close', but got 'websocket.http.response.start'",
]

# ---- inheritance in an async session (fixtures/dynapp/inherit.py): what each loader option loads of the subclasses
STEPS += [
    ("POST", "/inh/lazy", None),
    ("POST", "/inh/selectin", None),
    ("POST", "/inh/selectin/only", None),
    ("POST", "/inh/withpoly", None),
    ("POST", "/inh/chained", None),
    ("POST", "/inh/load", None),
]

# ---- obj.__dict__ around an INSERT (fixtures/dynapp/idict.py): columns never set, defaults, foreign keys and
# back-populated relationships, then a later read, SELECT, get, refresh, UPDATE, collection removal
STEPS += [("POST", f"/idict/insert/{case}/{mode}", None)
          for case in ("append", "m2o_set", "explicit_none", "m2o_obj", "identity", "eager", "ctor_list", "backref_m2o",
                       "fk_none", "batch")
          for mode in ("flush", "commit")]
STEPS += [("POST", "/idict/later", None), ("POST", "/idict/remove", None)]

# fixtures/dynapp/greenlet.py: an attribute needing IO in an AsyncSession is a StatementError whose .orig is the
# MissingGreenlet (expired column, expired key, lazy one-to-many and many-to-one); .orig of a database error
STEPS += [("POST", "/greenlet/run", None), ("POST", "/greenlet/dbapi", None)]

# session.expire(obj) / expire(obj, [names]) (fixtures/dynapp/expire.py); last: its rolled back rows use sequence values
STEPS += [("POST", "/expire/run", None), ("POST", "/expire/run", None)]

# the raw request body and pydantic's JSON mode on it (fixtures/dynapp/reqbody.py)
_TEXT = {"content-type": "text/plain;charset=UTF-8"}
_JSON = {"content-type": "application/json"}
_track = b'{"event":"pricing_toggle","properties":{"env":"prod","n":1.5},"session_id":"s1"}'
STEPS += [
    # sendBeacon (text/plain), HttpClient (application/json), no content-type, empty, invalid, too long
    ("POST", "/reqbody/track", _track, _TEXT),
    ("POST", "/reqbody/track", _track.replace(b"pricing", b"signup"), _JSON),
    ("POST", "/reqbody/track", b'{"event":"bare"}', {}),
    ("POST", "/reqbody/track", b"", _TEXT),
    ("POST", "/reqbody/track", b"not json", _TEXT),
    ("POST", "/reqbody/track", b'{"event":"' + b"x" * 65 + b'"}', _JSON),
    ("GET", "/reqbody/tracked", None),
]
for _body, _h in ((b"", {}), (b'{"a": 1}', _JSON), ("é€".encode(), _TEXT), (b"\xff\xfe\x00", {}),
                  (b"x" * (3 * 1024 * 1024), {"content-type": "application/octet-stream"})):
    STEPS.append(("POST", "/reqbody/raw", _body, _h))
for _body in (b'{"a": [1, 2.5, "\\u00e9"]}', b"", b"nope", b"\xef\xbb\xbf[1]", '"é"'.encode("utf-16"), b'"\xff"',
              b"[NaN, Infinity]", b'{"a": 1, "a": 2}'):
    STEPS.append(("POST", "/reqbody/json", _body, _JSON))
STEPS += [
    ("POST", "/reqbody/after-param", {"name": "a", "n": 2}),
    ("POST", "/reqbody/after-param", b'{"name": 5}', _JSON),
    ("POST", "/reqbody/after-dep", b"same bytes", _TEXT),
    ("POST", "/reqbody/after-dep", None),
    ("POST", "/reqbody/mvj-none", None),
    ("POST", "/reqbody/mvj/none", None),
]
_RICH_OK = (b'{"ints":[1,2],"pair":[1,"a"],"tags":[3,3],"frozen":[1],"mapping":{"a":1},"inner":{"x":1},'
            b'"inners":[{"x":2}],"point":{"y":3},"delay":"PT1S","amount":1.0,"day":"2020-01-01",'
            b'"when":"2020-01-01T00:00:00","raw":"x","ident":"12345678-1234-5678-1234-567812345678",'
            b'"price":"1.5","flag":true}')
for _kind in ("bytes", "str"):
    for _body in (_RICH_OK, b"", b"nope", b"[]", b"5", b"null", b'{"ints":5}', b'{"pair":5}', b'{"tags":{}}',
                  b'{"frozen":"x"}', b'{"mapping":[]}', b'{"inner":5}', b'{"inners":[5]}', b'{"point":5}',
                  b'{"delay":true}', b'{"delay":"x"}', b'{"delay":null}', b'{"delay":1e400}', b'{"amount":1e2}',
                  b'{"amount":-0.0}', b'{"amount":12345678901234567890.5}', b'{"amount":"x"}', b'{"day":"x"}',
                  b'{"raw":"\\ud800"}', b'{"raw":"\xff"}', b'{"flag":"maybe"}', b'{"ints":[1,"a"],"pair":[1]}',
                  b'{"ints":[1]', b'{"ints": [1]} x', b"[1, 2,]"):
        STEPS.append(("POST", f"/reqbody/mvj/{_kind}", _body, _JSON))
for _body in (b'{"event":"a"}', b"", b'{"event":1}', b'{"properties":5}'):
    STEPS.append(("POST", "/reqbody/mvj/track", _body, _TEXT))
for _body in (b"[1,2]", b"5", b"", b"[1,true]", b"{}"):
    STEPS.append(("POST", "/reqbody/mvj/adapter", _body, _JSON))
for _body in (b"[1,2]", b"5", b"{}", b"nope", b'[1,"x"]'):
    STEPS.append(("POST", "/reqbody/mvj/jsonfield", _body, _JSON))
# a Json[T] field inside model_validate_json: its inner type in JSON mode too
for _body in (b'{"j":"5"}', b'{"j":"[1]"}', b'{"j":5}'):
    STEPS.append(("POST", "/reqbody/mvj/jsonmode", _body, _JSON))
# the same inputs in python mode (model_validate of the parsed JSON; a nested model given a non-dict is left out:
# the runtime answers model_attributes_type, FastAPI's from_attributes=True rule, where pydantic says model_type)
for _body in (b'{"point":5}', b'{"mapping":[]}', b'{"delay":true}', b'{"amount":1.0}'):
    STEPS.append(("POST", "/reqbody/mvj/python", _body, _JSON))

# other shapes of the session dependency (fixtures/dynapp/sessforms.py): commit after the endpoint, nothing left by
# an HTTPException or an unhandled exception raised after a flush
STEPS += [
    ("POST", "/sessf/assigned/sf-a", None), ("POST", "/sessf/assigned/sf-b?fail=1", None),
    ("POST", "/sessf/assigned/sf-c?fail=2", None), ("POST", "/sessf/factory/sf-d", None),
    ("POST", "/sessf/factory/sf-e?fail=1", None), ("POST", "/sessf/manager/sf-f", None),
    ("POST", "/sessf/manager/sf-g?fail=1", None), ("POST", "/sessf/late/sf-h", None), ("POST", "/sessf/late/sf-i?fail=1", None),
    ("POST", "/smsess/owners/sf-j", None), ("POST", "/smsess/owners/sf-k?fail=1", None),
] + [("GET", f"/sessf/count/sf-{x}", None) for x in "abcdefghijk"]

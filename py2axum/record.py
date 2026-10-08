"""Request recorder for differential replay (`tests/difftest.py replay`).

An ASGI middleware that writes every HTTP request the Python application receives to a JSONL file, anonymized,
so that the same sequence can later be played against FastAPI and the generated binary. It sits on the Python
side only: the transpiler never sees it, the application's code does not change.

    PY2AXUM_RECORD=/data/requests.jsonl PY2AXUM_RECORD_APP=api.main:app \\
        uvicorn py2axum.record:app

or, in code, `app = RecordMiddleware(app, "/data/requests.jsonl")`. Without `PY2AXUM_RECORD` the factory serves
the application untouched.

Environment (all optional but PY2AXUM_RECORD):
    PY2AXUM_RECORD_KEY      secret of the pseudonyms (HMAC): set it to the same value in every worker so that
                            one e-mail gets one pseudonym; random per process otherwise (never written down)
    PY2AXUM_RECORD_FIELDS   extra JSON/form/query keys to mask, comma separated (`first_name,iban,phone`)
    PY2AXUM_RECORD_HEADERS  extra request headers to keep (`x-tenant`); x-* and a few standard ones are kept
    PY2AXUM_RECORD_SKIP     regex of paths not recorded (default `^/(health|metrics|docs|openapi.json)`)
    PY2AXUM_RECORD_MAX_BODY bodies above this size (bytes, default 1 MiB) are not recorded (`omitted`)
    PY2AXUM_RECORD_RAW=1    keep multipart and binary bodies as they are (base64): NOT anonymized, test only

What is masked: e-mail addresses anywhere (→ `u-<h>@example.com`), passwords (→ `Rd-<h>-Pw1!`, a value most
password policies accept), tokens, secrets, API keys, JWTs, the Authorization header and every cookie value
(→ `py2axum-tok-<h>`), client IPs in forwarding headers (→ 198.51.100.x), and the listed fields. A pseudonym
depends only on the value and the key: the same password at sign-up and at log-in stays the same, so the
replayed sequence still logs in.

Credentials the server hands out (Set-Cookie, a JSON field named like a token) are recorded under `issued` by
pseudonym only: at replay, each server's own value replaces the pseudonym in the later requests. Response
bodies are never written.

Line format (`py2axum-replay` version 1): a header line, then one line per request:
    {"t": ns since epoch, "pid": int, "session": "s-<h>" | null, "method", "path", "query" (raw string),
     "headers": [[name, value]...], "cookies": [[name, value]...],
     "body": null | {"json": v} | {"form": [[k, v]...]} | {"text": s} | {"b64": s} | {"omitted": why, "size": n},
     "status": int, "ms": duration, "issued": [["cookie", name, pseudonym] | ["json", "/pointer", pseudonym]...]}
"""
from __future__ import annotations

import base64
import gzip
import hashlib
import hmac
import importlib
import ipaddress
import json
import logging
import os
import re
import secrets
import threading
import time
from urllib.parse import parse_qsl, quote, unquote, urlencode

FORMAT = "py2axum-replay"
MAX_TOKENS = 10000
VERSION = 1

EMAIL = re.compile(r"[A-Za-z0-9._%+'-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}")
JWT = re.compile(r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]*")
PASSWORD_KEY = re.compile(r"pass(word|wd|phrase)?|pwd", re.I)
TOKEN_KEY = re.compile(r"token|secret|api[_-]?key|apikey|authorization|session|signature|otp|totp|csrf|nonce|code_verifier",
                       re.I)
EMAIL_KEY = re.compile(r"e?mail", re.I)
IP_KEY = re.compile(r"(^|_)ip(_address)?$|remote_addr", re.I)
DEFAULT_FIELDS = ("phone", "telephone", "mobile", "iban", "bic", "card_number", "cvv", "ssn")
KEPT_HEADERS = {"content-type", "accept", "accept-language", "accept-encoding", "content-encoding", "origin",
                "if-none-match", "if-modified-since", "if-match", "range", "authorization"}
IP_HEADERS = {"x-forwarded-for", "x-real-ip", "forwarded", "cf-connecting-ip", "true-client-ip"}
DROPPED_HEADERS = {"cookie", "host", "content-length", "connection", "user-agent", "transfer-encoding", "referer"}


class Anonymizer:
    """Deterministic pseudonyms (keyed HMAC): the same value under the same rule always maps to the same
    pseudonym, so relations inside the recording survive (sign-up then log-in, a token reused)."""

    def __init__(self, key: bytes, fields=(), raw: bool = False):
        self.key = key
        self.fields = {f.lower() for f in (*DEFAULT_FIELDS, *fields)}
        self.raw = raw
        self.tokens: dict[str, str] = {}  # real credential issued by the server -> pseudonym

    def h(self, kind: str, value: str, n: int = 10) -> str:
        return hmac.new(self.key, f"{kind}\0{value}".encode(), hashlib.sha256).hexdigest()[:n]

    def token(self, value: str) -> str:
        p = self.tokens.get(value)
        if p is None:
            if len(self.tokens) >= MAX_TOKENS:  # a long recording: forget the oldest credentials
                for old in list(self.tokens)[:MAX_TOKENS // 2]:
                    del self.tokens[old]
            p = self.tokens[value] = f"py2axum-tok-{self.h('tok', value, 12)}"
        return p

    def email(self, value: str) -> str:
        return f"u-{self.h('mail', value.lower())}@example.com"

    def password(self, value: str) -> str:
        return f"Rd-{self.h('pw', value, 8)}-Pw1!"

    def ip(self, value: str) -> str:
        try:
            ipaddress.ip_address(value.strip())
        except ValueError:
            return value
        return f"198.51.100.{int(self.h('ip', value.strip(), 4), 16) % 254 + 1}"

    def text(self, s: str) -> str:
        """Free text: known credentials (16 characters or more: a short value would match inside unrelated
        text), JWTs and e-mail addresses inside it."""
        if s in self.tokens:
            return self.tokens[s]
        long = [real for real in self.tokens if len(real) >= 16 and real in s]
        if long:
            pat = re.compile("|".join(re.escape(r) for r in sorted(long, key=len, reverse=True)))
            s = pat.sub(lambda m: self.tokens[m.group(0)], s)  # one pass: a pseudonym is never rewritten
        s = JWT.sub(lambda m: self.token(m.group(0)), s)
        return EMAIL.sub(lambda m: self.email(m.group(0)), s)

    def keyed(self, key: str, value):
        """A value under a key (JSON field, form field, query parameter)."""
        k = key.lower()
        if isinstance(value, (dict, list)):
            return self.value(value)
        if value is None or isinstance(value, bool):
            return value
        if PASSWORD_KEY.search(k):
            return self.password(str(value)) if isinstance(value, str) else value
        if TOKEN_KEY.search(k) and isinstance(value, str):
            return self.token(value) if value else value
        if k in self.fields or any(k.endswith("_" + f) for f in self.fields):
            if isinstance(value, str):
                return f"redacted-{self.h(k, value, 8)}"
            if isinstance(value, int):
                return int(self.h(k, str(value), 6), 16)
            return value
        if IP_KEY.search(k) and isinstance(value, str):
            return self.ip(value)
        if EMAIL_KEY.fullmatch(k) and isinstance(value, str) and "@" in value:
            return self.email(value)
        return self.text(value) if isinstance(value, str) else value

    def value(self, v):
        if isinstance(v, dict):
            return {k: self.keyed(k, x) for k, x in v.items()}
        if isinstance(v, list):
            return [self.value(x) for x in v]
        if isinstance(v, str):
            return self.text(v)
        return v

    def pairs(self, pairs):
        return [[k, self.keyed(k, v)] for k, v in pairs]

    def query(self, raw: str) -> str:
        if not raw:
            return raw
        pairs = parse_qsl(raw, keep_blank_values=True)
        masked = self.pairs(pairs)
        if masked == [list(p) for p in pairs]:
            return raw  # untouched: keep the exact bytes (encoding, order, `+`)
        return urlencode([tuple(p) for p in masked])

    def path(self, raw: str) -> str:
        """A raw (percent-encoded) path: masked on its decoded form, re-encoded only when something changed."""
        decoded = unquote(raw)
        masked = self.text(decoded)
        return raw if masked == decoded else quote(masked, safe="/:@!$&'()*+,;=~")

    def header(self, name: str, value: str) -> str:
        if name == "authorization":
            scheme, _, cred = value.partition(" ")
            return f"{scheme} {self.token(cred)}" if cred else self.token(value)
        if name in IP_HEADERS:
            return ", ".join(self.ip(p) for p in value.split(","))
        if TOKEN_KEY.search(name):
            return self.token(value)
        return self.text(value)

    def body(self, ctype: str, data: bytes):
        if not data:
            return None
        if "json" in ctype:
            try:
                return {"json": self.value(json.loads(data))}
            except ValueError:
                pass  # invalid JSON is worth replaying (a 422): kept as text, masked
        if ctype.startswith("application/x-www-form-urlencoded"):
            try:
                return {"form": self.pairs(parse_qsl(data.decode(), keep_blank_values=True))}
            except UnicodeDecodeError:
                pass
        if ctype.startswith("multipart/") or not _is_text(ctype, data):
            if self.raw:
                return {"b64": base64.b64encode(data).decode()}
            return {"omitted": "multipart" if ctype.startswith("multipart/") else "binary", "size": len(data)}
        return {"text": self.text(data.decode())}


def _is_text(ctype: str, data: bytes) -> bool:
    if ctype.startswith("text/") or "json" in ctype or "xml" in ctype or not ctype:
        try:
            data.decode()
            return True
        except UnicodeDecodeError:
            return False
    return False


def json_tokens(value, pointer: str = ""):
    """(pointer, value) of the credentials in a response body: string fields named like a token."""
    if isinstance(value, dict):
        for k, v in value.items():
            p = f"{pointer}/{str(k).replace('~', '~0').replace('/', '~1')}"
            if isinstance(v, str) and v and TOKEN_KEY.search(str(k)) and not PASSWORD_KEY.search(str(k)):
                yield p, v
            else:
                yield from json_tokens(v, p)
    elif isinstance(value, list):
        for i, v in enumerate(value):
            yield from json_tokens(v, f"{pointer}/{i}")


class Writer:
    def __init__(self, path: str, header: dict):
        self.fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        self.lock = threading.Lock()
        if os.fstat(self.fd).st_size == 0:
            self.write(header)

    def write(self, line: dict) -> None:
        data = (json.dumps(line, ensure_ascii=False, separators=(",", ":")) + "\n").encode()
        with self.lock:
            os.write(self.fd, data)  # one write per line on O_APPEND: workers do not interleave lines


class RecordMiddleware:
    def __init__(self, app, path: str | None = None, *, key: bytes | None = None, fields=None, headers=None,
                 skip: str | None = None, max_body: int | None = None, raw: bool | None = None):
        env = os.environ
        self.app = app
        path = path or env["PY2AXUM_RECORD"]
        key = key or (env["PY2AXUM_RECORD_KEY"].encode() if env.get("PY2AXUM_RECORD_KEY") else secrets.token_bytes(32))
        fields = fields if fields is not None else [f.strip() for f in env.get("PY2AXUM_RECORD_FIELDS", "").split(",") if f.strip()]
        extra = headers if headers is not None else [h.strip() for h in env.get("PY2AXUM_RECORD_HEADERS", "").split(",") if h.strip()]
        self.kept = KEPT_HEADERS | {h.lower() for h in extra}
        self.skip = re.compile(skip if skip is not None else env.get("PY2AXUM_RECORD_SKIP", r"^/(health|metrics|docs|openapi\.json)"))
        self.max_body = max_body if max_body is not None else int(env.get("PY2AXUM_RECORD_MAX_BODY", 1 << 20))
        raw = raw if raw is not None else env.get("PY2AXUM_RECORD_RAW") == "1"
        self.anon = Anonymizer(key, fields, raw)
        self.out = Writer(path, {"format": FORMAT, "version": VERSION, "anonymized": not raw,
                                 "fields": sorted(self.anon.fields), "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http" or self.skip.search(scope["path"]):
            return await self.app(scope, receive, send)
        t0 = time.time_ns()
        headers = [(k.decode("latin-1").lower(), v.decode("latin-1")) for k, v in scope["headers"]]
        hmap = dict(headers)
        size = int(hmap.get("content-length") or 0)
        chunks: list[bytes] = []
        if size <= self.max_body:
            # read the whole body first, hand it to the application in one message
            more = True
            while more:
                msg = await receive()
                if msg["type"] != "http.request":
                    return await self.app(scope, _replay([msg], receive), send)
                chunks.append(msg.get("body", b""))
                more = msg.get("more_body", False)
            body = b"".join(chunks)
            app_receive = _replay([{"type": "http.request", "body": body, "more_body": False}], receive)
        else:
            body, app_receive = None, receive
        status = [0]
        resp_headers: list = []
        resp_body: list[bytes] = []

        async def tee(msg):
            if msg["type"] == "http.response.start":
                status[0] = msg["status"]
                resp_headers.extend(msg.get("headers", []))
            elif msg["type"] == "http.response.body" and sum(map(len, resp_body)) < 65536:
                resp_body.append(msg.get("body", b""))
            await send(msg)

        try:
            await self.app(scope, app_receive, tee)
        finally:
            try:
                self.record(scope, headers, body, size, status[0], resp_headers, b"".join(resp_body), t0)
            except Exception:  # noqa: BLE001 - recording must never break the application
                logging.getLogger("py2axum.record").exception("request not recorded")

    def record(self, scope, headers, body, size, status, resp_headers, resp_body, t0) -> None:
        a = self.anon
        issued = []
        # credentials handed out first: the pseudonyms of this very request already know them
        for k, v in resp_headers:
            if k.lower() == b"set-cookie":
                name, _, rest = v.decode("latin-1").partition("=")
                value = rest.split(";", 1)[0].strip().strip('"')
                if value:
                    issued.append(["cookie", name.strip(), a.token(value)])
        ctype = dict((k.lower(), v) for k, v in resp_headers).get(b"content-type", b"").decode("latin-1")
        enc = dict((k.lower(), v) for k, v in resp_headers).get(b"content-encoding")
        if "json" in ctype and enc in (None, b"gzip") and resp_body:
            try:
                for pointer, value in json_tokens(json.loads(gzip.decompress(resp_body) if enc else resp_body)):
                    issued.append(["json", pointer, a.token(value)])
            except (ValueError, OSError, EOFError):
                pass  # truncated (over 64 KiB) or not JSON after all
        cookies, kept = [], []
        for k, v in headers:
            if k == "cookie":
                for part in v.split(";"):
                    name, _, value = part.strip().partition("=")
                    if name:
                        cookies.append([name, a.token(value) if value else value])
            elif k in self.kept or (k.startswith("x-") and k not in IP_HEADERS) or k in IP_HEADERS:
                if k not in DROPPED_HEADERS:
                    kept.append([k, a.header(k, v)])
        sess = next((v for n, v in cookies if TOKEN_KEY.search(n)), None) or \
            next((v for n, v in kept if n == "authorization"), None)
        ctype_req = dict(headers).get("content-type", "")
        if body is None:
            rec_body = {"omitted": "too_large", "size": size}
        else:
            rec_body = a.body(ctype_req, body)
        self.out.write({
            "t": t0, "pid": os.getpid(), "session": f"s-{a.h('sess', sess, 8)}" if sess else None,
            "method": scope["method"], "path": a.path(scope["raw_path"].decode("latin-1") if scope.get("raw_path") else quote(scope["path"])),
            "query": a.query(scope.get("query_string", b"").decode("latin-1")),
            "headers": kept, "cookies": cookies, "body": rec_body,
            "status": status, "ms": round((time.time_ns() - t0) / 1e6, 2), "issued": issued})


def _replay(messages: list, receive):
    queue = list(messages)

    async def inner():
        if queue:
            return queue.pop(0)
        return await receive()
    return inner


def _load(target: str):
    mod, _, attr = target.partition(":")
    obj = importlib.import_module(mod)
    for part in (attr or "app").split("."):
        obj = getattr(obj, part)
    return obj


def __getattr__(name: str):
    """`uvicorn py2axum.record:app`: the application named by PY2AXUM_RECORD_APP, recorded when
    PY2AXUM_RECORD is set."""
    if name != "app":
        raise AttributeError(name)
    inner = _load(os.environ["PY2AXUM_RECORD_APP"])
    return RecordMiddleware(inner) if os.environ.get("PY2AXUM_RECORD") else inner

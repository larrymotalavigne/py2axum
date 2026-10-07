"""Third-party libraries: bcrypt, pyotp, asyncio.to_thread, function values."""
import asyncio
from datetime import UTC, datetime
from decimal import Decimal
from enum import Enum
from uuid import UUID

import bcrypt
import pyotp
from fastapi import APIRouter, BackgroundTasks

router = APIRouter(prefix="/libs")

SALT = b"$2b$04$abcdefghijklmnopqrstuu"
SECRET = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP"
T0 = datetime(2026, 10, 6, 12, 0, 7, tzinfo=UTC)


def _try(f):
    try:
        return f()
    except Exception as e:  # noqa: BLE001
        return f"{type(e).__name__}: {e}"


@router.get("/bcrypt")
async def bcrypt_cases(pw: str = "s3cret"):
    hashed = bcrypt.hashpw(pw.encode("utf-8"), SALT)
    fresh = bcrypt.hashpw(pw.encode(), bcrypt.gensalt(rounds=4))
    return {
        "hash": hashed.decode("utf-8"),
        "ok": bcrypt.checkpw(pw.encode("utf-8"), hashed),
        "bad": bcrypt.checkpw(b"other", hashed),
        "fresh_ok": bcrypt.checkpw(pw.encode(), fresh),
        "fresh_prefix": fresh[:7].decode(),
        "salt_len": len(bcrypt.gensalt()),
        "2y": bcrypt.hashpw(b"x", b"$2y$04$abcdefghijklmnopqrstuu").decode(),
        "long_salt": bcrypt.hashpw(b"x", SALT + b"EXTRA").decode(),
        "errors": [
            _try(lambda: bcrypt.hashpw(b"x" * 73, SALT)),
            _try(lambda: bcrypt.checkpw(b"x", b"nothash")),
            _try(lambda: bcrypt.hashpw("x", SALT)),
            _try(lambda: bcrypt.hashpw(b"x", b"$2b$03$abcdefghijklmnopqrstuu")),
            _try(lambda: bcrypt.hashpw(b"x", b"$2b$04$abcdefghijklmnopqrst")),
            _try(lambda: bcrypt.gensalt(3)),
            _try(lambda: bcrypt.gensalt(12, b"2y")),
        ],
    }


@router.get("/totp")
async def totp_cases(code: str = "000000"):
    totp = pyotp.TOTP(SECRET)
    at = totp.at(T0)
    return {
        "at": at,
        "prev": totp.at(T0, -1),
        "int_time": totp.at(1759752007),
        "verify": totp.verify(at, for_time=T0),
        "window": [totp.verify(totp.at(T0, i), for_time=T0, valid_window=1) for i in (-2, -1, 0, 1, 2)],
        "bad": totp.verify(code, for_time=T0),
        "fullwidth": totp.verify("".join(chr(ord(c) + 0xFEE0) for c in at), for_time=T0),
        "now_len": len(totp.now()),
        "uri": pyotp.totp.TOTP(SECRET).provisioning_uri(name="ana+test@example.com", issuer_name="Easy Location"),
        "uri2": pyotp.TOTP(SECRET, digits=8, interval=60, name="bob").provisioning_uri(),
        "eight": pyotp.TOTP(SECRET, digits=8).at(T0),
        "rand": [len(pyotp.random_base32()), _try(lambda: pyotp.random_base32(16))],
        "errors": [_try(lambda: pyotp.TOTP("not base32!").now()), _try(lambda: pyotp.TOTP("A").now()),
                   _try(lambda: pyotp.TOTP(SECRET, digits=11))],
    }


def render(data: dict, landlord: str, *, upper: bool = False) -> str:
    text = f"{landlord}: {sorted(data)}"
    return text.upper() if upper else text


class Kind(Enum):
    A = "a"


async def _send_quietly(sender, *args, **kwargs) -> None:
    try:
        await sender(*args, **kwargs)
    except Exception:  # noqa: BLE001
        pass


SENT: list = []


async def send(to: str, subject: str = "hi", user_id: int | None = None) -> None:
    SENT.append([to, subject, user_id])


@router.post("/thread")
async def thread_cases(tasks: BackgroundTasks):
    tasks.add_task(_send_quietly, send, "ana@x.fr", user_id=4)
    tasks.add_task(_send_quietly, send, "bob@x.fr", "yo")
    tasks.add_task(_send_quietly, send)  # TypeError swallowed
    rendered = await asyncio.to_thread(render, {"b": 1, "a": 2}, "Ana", upper=True)
    values = [None, Kind.A, UUID("12345678-1234-5678-1234-567812345678"), T0.time(), 1.5, "s"]
    kinds = [[isinstance(v, Decimal), isinstance(v, Enum), isinstance(v, UUID), isinstance(v, (str, Decimal))] for v in values]
    return {"rendered": rendered, "kinds": kinds, "sent": SENT[-3:]}


@router.get("/sent")
async def sent():
    return SENT


# ---- admin_alerts: fire-and-forget tasks kept in a set, rate window rebuilt in place
_pending: set = set()
_stamps: list = [1, 5, 9]
DELIVERED: list = []


async def _deliver(title: str, lines: list, delay: float = 0.01) -> None:
    await asyncio.sleep(delay)
    DELIVERED.append([title, lines])


def notify(title: str, lines: list) -> None:
    _stamps[:] = [t for t in _stamps if t > 2]
    _stamps.append(len(_stamps) * 10)

    async def _run() -> None:
        try:
            await _deliver(title, lines)
        except Exception:  # noqa: BLE001
            pass

    try:
        task = asyncio.get_running_loop().create_task(_run())
        _pending.add(task)
        task.add_done_callback(_pending.discard)
    except RuntimeError:
        asyncio.run(_run())


@router.post("/alert/{title}")
async def alert(title: str):
    notify(title, ["a", 1])
    t = asyncio.create_task(_deliver(title + "!", [], delay=0.03))
    await asyncio.sleep(0.05)
    nums = [0, 1, 2, 3, 4, 5]
    nums[1:3] = ["x"]
    nums[::2] = ["e", "e", "e"]
    errs = []
    try:
        nums[::2] = [1]
    except ValueError as e:
        errs.append(str(e))
    try:
        asyncio.run(_deliver("never", []))
    except RuntimeError as e:
        errs.append(str(e))
    return {"stamps": _stamps, "pending": len(_pending), "done": t.done(), "delivered": DELIVERED[-2:],
            "nums": nums, "errs": errs}


# ---- outgoing HTTP (httpx, aiohttp): each server calls itself back
import aiohttp  # noqa: E402
import httpx  # noqa: E402
from fastapi import Request  # noqa: E402
from fastapi.responses import PlainTextResponse  # noqa: E402

_TIMEOUT = httpx.Timeout(30.0, connect=10.0)


@router.post("/echo")
async def echo(request: Request):
    body = await request.body()
    return {"method": request.method, "query": request.url.query, "ctype": request.headers.get("content-type"),
            "body": body.decode(), "x": request.headers.get("x-test")}


@router.get("/whoami")
async def whoami(request: Request):
    return {"auth": request.headers.get("authorization")}


@router.get("/http-auth")
async def http_auth(request: Request):
    """AsyncClient(auth=...): BasicAuth or a (user, password) tuple, replacing the request's own header."""
    url = f"http://{request.headers['host']}/libs/whoami"
    out = []
    for auth in (httpx.BasicAuth("é", "p:w"), ("bob", b"secret"), httpx.BasicAuth(username="u", password=""), None):
        async with httpx.AsyncClient(timeout=_TIMEOUT, auth=auth) as client:
            out.append((await client.get(url)).json())
            out.append((await client.get(url, headers={"Authorization": "Bearer x"})).json())
    return out


@router.get("/status/{code}")
async def status_code(code: int):
    return PlainTextResponse("nope é", status_code=code)


@router.get("/http")
async def http_cases(request: Request):
    base = f"http://{request.headers['host']}/libs"
    out = {"host": httpx.URL(base).host}
    async with httpx.AsyncClient(timeout=_TIMEOUT) as client:
        r = await client.post(f"{base}/echo", json={"é": [1, 2.5, None]}, params={"a": 1, "b": True},
                              headers={"X-Test": "yes"})
        out["httpx"] = [r.status_code, r.reason_phrase, r.json(), r.headers["content-type"], r.is_success]
        r = await client.post(f"{base}/echo", data={"k": "v w", "n": 2})
        out["form"] = r.json()["body"]
        r = await client.get(f"{base}/status/404")
        out["text"] = [r.status_code, r.text, r.reason_phrase]
        try:
            r.raise_for_status()
        except httpx.HTTPStatusError as e:
            out["raise"] = str(e).replace(base, "<base>")
        except httpx.HTTPError:
            out["raise"] = "other"
        try:
            await client.get("http://127.0.0.1:9/x")
        except httpx.HTTPError as e:
            out["refused"] = type(e).__name__
    try:
        await client.get(f"{base}/echo")
    except RuntimeError as e:
        out["closed"] = str(e)
    timeout = aiohttp.ClientTimeout(total=30)
    async with aiohttp.ClientSession(timeout=timeout) as session:
        async with session.post(f"{base}/echo", json={"é": [1, 2.5, None]}) as resp:
            out["aiohttp"] = [resp.status, resp.reason, await resp.json(), resp.ok]
        async with session.get(f"{base}/status/503") as resp:
            out["aio_text"] = [resp.status, await resp.text()]
            try:
                await resp.json()
            except aiohttp.ClientResponseError as e:
                out["aio_ctype"] = [type(e).__name__, e.status, e.message]
            try:
                resp.raise_for_status()
            except aiohttp.ClientError as e:
                out["aio_raise"] = str(e).replace(base, "<base>")
        try:
            async with session.get("http://127.0.0.1:9/x") as resp:
                pass
        except (aiohttp.ClientError, TimeoutError) as e:
            out["aio_refused"] = isinstance(e, aiohttp.ClientConnectorError)
    return out


# ---- pywebpush: both servers push to tests/push_sink.py (port 8299), which decrypts the message and
# checks the VAPID token (keys and signatures are random; webpush blocks the loop: not the same process)
import json  # noqa: E402

from pywebpush import WebPushException, webpush  # noqa: E402

RECEIVER_PRIV = "p0X3nDVd54RiCpnSbmMx9JONqeREDLCVUKjeAf1AQUA"
RECEIVER_PUB = "BOXn1DgAbdzkwDwVa7vqMJrI3we_FUizlZnoSmwuvLJSwr20dyTdPmRZTWVn5fIDFTEwlXB8rYZpxSkB1UfDDdA"
AUTH = "T_7KKXwOodfugDhp9lyyoA"
VAPID_RAW = "5-NcwjAzuSiIy3aUuPt37Ti-WenZl3P-sTWDSgqPfLE"
VAPID_DER = ("MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg5-NcwjAzuSiIy3aUuPt37Ti-WenZl3P-sTWDSgqPfLGhRANCAAQBsvC5crmc65xi9"
             "vgW81Tsh-ctwTOVJIVwSaMxVE-kXdcK-ndQHIHearMmzLv1iulYYqrr3uki1Mxuw38UVOpK")
VAPID_PUB = "BAGy8LlyuZzrnGL2-BbzVOyH5y3BM5UkhXBJozFUT6Rd1wr6d1Acgd5qsybMu_WK6Vhiquve6SLUzG7DfxRU6ko"


@router.post("/push")
async def push(target: str, der: bool = False, size: int = 1, sub: str = "mailto:admin@example.com"):
    claims = {"sub": sub}
    info = {"endpoint": target, "keys": {"p256dh": RECEIVER_PUB, "auth": AUTH}}
    payload = {"notification": {"title": "Loyer reçu ✓", "body": "x" * size}}
    try:
        resp = webpush(subscription_info=info, data=json.dumps(payload), vapid_private_key=VAPID_DER if der else VAPID_RAW,
                       vapid_claims=claims)
        return {"status": resp.status_code, "sink": resp.json(), "claims": sorted(claims),
                "aud": claims["aud"].split(":")[0]}
    except WebPushException as e:
        return {"error": str(e), "status": e.response.status_code if e.response is not None else None,
                "message": e.message}
    except Exception as e:  # noqa: BLE001
        return {"other": type(e).__name__, "msg": str(e)}


@router.post("/push-errors")
async def push_errors():
    out = []
    for info in ({}, {"endpoint": "http://x", "keys": {"auth": AUTH}}, {"endpoint": "http://x", "keys": {"p256dh": "AAAA", "auth": AUTH}}):
        try:
            webpush(subscription_info=info, data="hi", vapid_private_key=VAPID_RAW, vapid_claims={"sub": "mailto:a@b.c"})
        except WebPushException as e:
            out.append([str(e), e.response is None])
        except Exception as e:  # noqa: BLE001
            out.append([type(e).__name__])
    return out


# ---- session.begin_nested(): savepoints (de-duplication inside a transaction)
from sqlalchemy import func, select  # noqa: E402
from sqlalchemy.exc import IntegrityError  # noqa: E402

from .db import DbDep  # noqa: E402
from .models import Owner, Project  # noqa: E402


@router.post("/savepoint")
async def savepoint(db: DbDep):
    owner = Owner(name="sp")
    db.add(owner)
    await db.flush()
    sp = await db.begin_nested()
    try:
        db.add(Project(name="bad", owner_id=999999))
        await db.flush()
        await sp.commit()
        out = "committed"
    except IntegrityError as e:
        await sp.rollback()
        out = "violates foreign key constraint" in str(e)
    sp2 = await db.begin_nested()
    owner.name = "renamed"
    await db.flush()
    await sp2.rollback()
    await db.refresh(owner)
    good = Project(name="ok", owner_id=owner.id)
    sp3 = await db.begin_nested()
    db.add(good)
    await sp3.commit()
    n = (await db.execute(select(func.count()).select_from(Project).where(Project.owner_id == owner.id))).scalar()
    return {"out": out, "name": owner.name, "projects": n, "good": good.id is not None}


# ---- Enum methods, _missing_ (also used by Pydantic validation), match statements
from pydantic import BaseModel  # noqa: E402


class Rank(str, Enum):
    HIGH = "high"
    LOW = "low"
    UNKNOWN = "unknown"

    @classmethod
    def _missing_(cls, value):
        if isinstance(value, str) and value.upper() == "ESTAR":
            return cls.HIGH
        if value in ("", None):
            return cls.UNKNOWN
        return super()._missing_(value)

    @staticmethod
    def is_top(r) -> bool:
        return r == Rank.HIGH

    @classmethod
    def from_score(cls, score: int, country: str = "US"):
        match score:
            case 0 | 1:
                return cls.LOW
            case 9 if country == "FR":
                return cls.UNKNOWN
            case int() if False:
                return None
            case _ if score >= 5:
                return cls.HIGH
            case other:
                return other

    def to_score(self) -> int:
        match self:
            case Rank.HIGH:
                return 10
            case Rank.LOW as low:
                return len(low.value)
            case None:
                return -2
            case _:
                return -1

    @property
    def shout(self) -> str:
        return self.value.upper() + "!"


class RankIn(BaseModel):
    rank: Rank
    ranks: list[Rank] = []


@router.get("/enum")
async def enum_cases(rank: Rank = Rank.LOW):
    out = {"rank": rank, "score": rank.to_score(), "shout": rank.shout, "top": Rank.is_top(rank),
           "estar": Rank("estar"), "empty": Rank(""), "member": Rank.HIGH.to_score(),
           "from": [Rank.from_score(s) for s in (0, 1, 7, 3)] + [Rank.from_score(9, "FR"), Rank.from_score(9)]}
    try:
        Rank("nope")
    except ValueError as e:
        out["err"] = str(e)
    return out


@router.post("/enum")
async def enum_body(body: RankIn):
    return {"rank": body.rank, "ranks": body.ranks, "scores": [r.to_score() for r in body.ranks]}


# ---- urllib.parse
from urllib.parse import quote, quote_plus, unquote, unquote_plus, urlencode, urlparse, urlsplit  # noqa: E402


@router.get("/urllib")
async def urllib_cases(s: str = "a b/c?d=é&e+f~"):
    p = urlparse("HTTPS://user:pw@Example.COM:8443/p/a;x=1?q=1&r=2#frag")
    sp = urlsplit("amqp://guest:guest@rabbit:5672/vhost")
    scheme, netloc, *_ = sp
    return {
        "quote": [quote(s), quote(s, safe=""), quote_plus(s), quote_plus(s, safe="/"), quote(s.encode())],
        "unquote": [unquote("a%20b%C3%A9+c%zz"), unquote_plus("a+b%2Bc")],
        "urlencode": [urlencode({"a": 1, "b": "x y", "c": [1, 2]}), urlencode({"c": [1, 2], "d": "é"}, True),
                      urlencode([("ids", "1"), ("ids", 2)]), urlencode({"t": (1, "a")}, doseq=True)],
        "parse": [p.scheme, p.netloc, p.path, p.params, p.query, p.fragment, p.hostname, p.port, p.username,
                  p.password, p.geturl(), p[1], list(p)],
        "split": [sp.hostname, sp.port, sp.path, scheme, netloc, urlparse("/rel/path?x").netloc],
        "divmod": [divmod(7, 2), divmod(-7, 2), divmod(7.5, 2), divmod(3725, 60)],
    }


# ---- base64, psutil, tuple_, text(), engine.connect() (a readiness probe)
import base64  # noqa: E402

import psutil  # noqa: E402
from sqlalchemy import text, tuple_  # noqa: E402

from .db import engine  # noqa: E402
from .models import Task  # noqa: E402


@router.get("/b64")
async def b64_cases(s: str = "héllo wörld ~~ ??"):
    raw = s.encode()
    out = {"enc": [base64.b64encode(raw).decode(), base64.urlsafe_b64encode(raw).decode(), base64.b16encode(raw).decode(),
                   base64.b32encode(raw).decode(), base64.b64encode(raw, altchars=b"*!").decode()],
           "dec": [base64.b64decode(base64.b64encode(raw)).decode(), base64.urlsafe_b64decode(base64.urlsafe_b64encode(raw)),
                   base64.b64decode("aGk=\n  aGk="), base64.b64decode(b"a G k"), base64.b16decode("6869"),
                   base64.b32decode(base64.b32encode(b"x"))]}
    errs = []
    for bad in ("abc", "a", "aGk=x", "é"):
        try:
            base64.b64decode(bad)
        except ValueError as e:
            errs.append([type(e).__name__, str(e)])
    try:
        base64.b64decode("aGk=x", validate=True)
    except ValueError as e:
        errs.append(str(e))
    try:
        base64.b64encode("str")
    except TypeError as e:
        errs.append(str(e))
    out["errs"] = errs
    return out


@router.get("/psutil")
async def psutil_cases():
    vm = psutil.virtual_memory()
    du = psutil.disk_usage("/")
    cpu = psutil.cpu_percent(interval=0.1)
    return {"cpu": isinstance(cpu, float) and 0 <= cpu <= 100, "vm": [vm.total > 0, 0 <= vm.percent <= 100, vm.available > 0],
            "du": [du.total > 0, du.free <= du.total, round(du.used / (du.used + du.free) * 100, 1) == du.percent,
                   du[0] == du.total], "pids": len(psutil.pids()) > 1}


@router.get("/sqlmisc")
async def sql_misc(db: DbDep):
    db.add_all([Task(title="tp-a"), Task(title="tp-b")])
    await db.flush()
    rows = (await db.execute(select(Task.title).where(tuple_(Task.title, Task.revision).in_([("tp-a", "new"), ("zz", "new")])))).all()
    one = (await db.execute(text("SELECT 1 AS x, 'é'::text AS y"))).all()
    from sqlalchemy import text as local_text

    async with engine.connect() as conn:
        ping = (await asyncio.wait_for(conn.execute(local_text("SELECT 1")), timeout=3)).scalar()
    bind = (await db.execute(text("SELECT CAST(:x AS int) + 1 AS y, :s || '\\:' AS z, '1'::int AS c, :x AS again"),
                             {"x": 41, "s": "ab"})).all()
    try:
        await db.execute(text("SELECT :missing"), {})
        missing = "no error"
    except Exception as e:  # noqa: BLE001
        missing = type(e).__name__
    bind = [list(r) for r in bind] + [missing]
    return {"rows": [list(r) for r in rows], "one": [list(r) for r in one], "ping": ping, "bind": bind}


# ---- google-auth ID tokens, against tests/push_sink.py's certificates (port 8299)
import re  # noqa: E402

from google.auth.transport import requests as google_requests  # noqa: E402
from google.oauth2 import id_token as google_id_token  # noqa: E402


@router.get("/google")
async def google_cases():
    async with httpx.AsyncClient() as c:
        toks = (await c.get("http://127.0.0.1:8299/tokens")).json()
    out = {}
    for name, tok in toks.items():
        try:
            info = google_id_token.verify_token(tok, google_requests.Request(), "client-1",
                                                certs_url="http://127.0.0.1:8299/certs")
            out[name] = ["ok", info["sub"], info.get("email"), sorted(info)]
        except Exception as e:  # noqa: BLE001
            out[name] = [type(e).__name__, re.sub(r"\d{9,}", "<t>", str(e))[:120]]
    try:
        google_id_token.verify_token(toks["valid"], google_requests.Request(), "client-1",
                                     certs_url="http://127.0.0.1:8299/nope")
    except Exception as e:  # noqa: BLE001
        out["transport"] = [type(e).__name__, str(e)]
    return out


# ---- sqlalchemy.orm.aliased (a renamed entity, joined and selected)
from sqlalchemy.orm import aliased  # noqa: E402


@router.get("/aliased")
async def aliased_cases(db: DbDep):
    o = Owner(name="al-owner")
    db.add(o)
    await db.flush()
    p1, p2 = Project(name="al-p1", owner_id=o.id), Project(name="al-p2", owner_id=o.id)
    db.add_all([p1, p2])
    await db.flush()
    db.add(Task(title="al-t", project_id=p1.id))
    await db.flush()
    pa = aliased(Project)
    rows = (await db.execute(
        select(Task, Owner, pa).join(pa, Task.project_id == pa.id).join(Owner, pa.owner_id == Owner.id)
        .where(pa.name.like("al-%"), Task.title == "al-t")
    )).all()
    other = aliased(Project)
    pairs = (await db.execute(
        select(pa.name, other.name).where(pa.owner_id == other.owner_id, pa.id < other.id, pa.owner_id == o.id)
    )).all()
    return {"rows": [[t.title, ow.name, p.name, p is p1] for t, ow, p in rows], "pairs": [list(x) for x in pairs]}


# ---- decimal.Decimal (CPython's rules), NUMERIC columns, SQL division
from decimal import ROUND_DOWN, ROUND_HALF_UP, InvalidOperation  # noqa: E402


@router.get("/decimal")
async def decimal_cases():
    a, b = Decimal("10.25"), Decimal("3")
    errs = []
    for f in (lambda: Decimal("abc"), lambda: a + 1.5, lambda: a / 0, lambda: Decimal(0) / 0):
        try:
            f()
        except (InvalidOperation, TypeError, ZeroDivisionError) as e:
            errs.append(type(e).__name__)
    return {
        "str": [str(a), str(Decimal("1.10")), str(Decimal("0.0000001")), str(Decimal("1E+3")), str(Decimal("-0")),
                repr(a), str(Decimal(0.1))[:30], str(Decimal(7)), f"{a:.1f}", f"{Decimal('1234567.891'):,.2f}"],
        "ops": [str(a + b), str(a - b), str(a * b), str(a / b), str(Decimal(10) / Decimal(4)), str(Decimal(1) / Decimal(8)),
                str(a // b), str(a % b), str(Decimal(-7) // 2), str(Decimal(-7) % 2), str(-a), str(abs(-a)), str(a + 2)],
        "round": [round(a), round(Decimal("2.5")), str(round(a, 1)), str(Decimal("2.675").quantize(Decimal("0.01"))),
                  str(Decimal("2.675").quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)),
                  str(Decimal("2.679").quantize(Decimal("0.01"), rounding=ROUND_DOWN)), str(Decimal("1.2300").normalize())],
        "cmp": [a > b, a == Decimal("10.250"), Decimal("2") == 2, Decimal("0.5") == 0.5, max(a, b) == a, sorted([b, a, 1])[0] == 1],
        "conv": [float(a), int(a), int(Decimal("-2.7")), bool(Decimal(0)), isinstance(a, Decimal), sum([a, b, 1]) == Decimal("14.25")],
        "json": [a, Decimal("12"), Decimal("1E+2"), {"x": Decimal("0.5")}],
        "errs": errs,
    }


@router.post("/decimal-db")
async def decimal_db(db: DbDep):
    o = Owner(name="dec")
    db.add(o)
    await db.flush()
    db.add_all([Project(name="d1", owner_id=o.id, budget=Decimal("10.50")), Project(name="d2", owner_id=o.id, budget=Decimal("2.255")),
                Project(name="d3", owner_id=o.id, budget=7), Project(name="d4", owner_id=o.id)])
    await db.flush()
    rows = (await db.execute(select(Project.name, Project.budget).where(Project.owner_id == o.id).order_by(Project.id))).all()
    total = (await db.execute(select(func.sum(Project.budget)).where(Project.owner_id == o.id))).scalar()
    avg = (await db.execute(select(func.avg(Project.budget)).where(Project.owner_id == o.id))).scalar()
    ratio = (await db.execute(select(func.count(Project.id) / 3).where(Project.owner_id == o.id))).scalar()
    big = (await db.execute(select(Project.name).where(Project.budget > Decimal("5")).where(Project.owner_id == o.id))).scalars().all()
    p = (await db.execute(select(Project).where(Project.name == "d2", Project.owner_id == o.id))).scalar_one()
    p.budget = p.budget * 2
    await db.flush()
    await db.refresh(p)
    return {"rows": [[n, str(x) if x is not None else None, type(x).__name__] for n, x in rows], "total": str(total),
            "avg": str(avg), "ratio": str(ratio), "big": big, "doubled": str(p.budget), "raw": [total, avg]}


# ---- html.escape, datetime.min/max, toordinal
import html  # noqa: E402
from datetime import date as _date  # noqa: E402


SKIPPED, STUCK, LIMITS = "sautée", "figée", (1, 2)


@router.get("/smalllibs")
async def small_libs():
    out = [SKIPPED, STUCK, LIMITS, html.escape("<a href='x'>&\"</a>"), html.escape("<'\">", quote=False), html.escape("plain", False),
           str(datetime.combine(_date(2024, 5, 6), datetime.min.time(), tzinfo=UTC)), str(datetime.min), str(datetime.max),
           _date(2024, 5, 6).toordinal(), datetime(2024, 5, 6, 7).toordinal(), _date.min.toordinal(),
           (_date(2024, 5, 6).toordinal() - 1) // 7]
    try:
        html.escape(5)
    except AttributeError as e:
        out.append(str(e))
    return out


# ---- email.utils.formatdate (RFC 2822 dates)
from email.utils import formatdate  # noqa: E402


@router.get("/formatdate")
async def format_date():
    return [formatdate(0), formatdate(0, usegmt=True), formatdate(1700000000.5), formatdate(1e9, localtime=True),
            formatdate(timeval=86399.9999999), formatdate(1e9, True, True), len(formatdate()),
            formatdate(localtime=True)[-5:], formatdate(None, usegmt=True)[-4:], formatdate(-1)]


# ---- Decimal fields of Pydantic models (lax validation, digits, bounds, JSON as strings)
from pydantic import BaseModel as _BaseModel, Field, computed_field  # noqa: E402


class Price(_BaseModel):
    amount: Decimal
    fee: Decimal | None = None
    capped: Decimal | None = Field(None, gt=0, le=500.5, max_digits=5, decimal_places=2)
    small: Decimal | None = Field(None, ge=0.1, lt=10, decimal_places=1)
    digits: Decimal | None = Field(None, max_digits=1)


class Bill(_BaseModel):
    model_config = {"extra": "allow"}

    qty: int
    unit: Decimal
    note: str | None = None

    @computed_field
    @property
    def total(self) -> Decimal:
        return self.qty * self.unit

    @computed_field
    @property
    def label(self) -> str | None:
        return self.note.upper() if self.note else None


class BigBill(Bill):
    @computed_field
    def big(self) -> bool:
        return self.total > 100


@router.post("/bill")
async def bill(b: BigBill):
    return {"dump": b.model_dump(), "json": b.model_dump(mode="json"), "nn": b.model_dump(exclude_none=True),
            "unset": b.model_dump(exclude_unset=True), "attr": str(b.total), "exc": b.model_dump(exclude={"total"}),
            "inc": b.model_dump(include={"qty", "big"}), "mj": b.model_dump_json(), "big": b.big}


@router.post("/bill-echo", response_model=BigBill)
async def bill_echo(b: BigBill):
    b.qty = b.qty + 1
    return b


class Tagged(_BaseModel):
    """A model's own __init__: run by Tagged(...), skipped by validation from attributes."""
    model_config = {"from_attributes": True}

    name: str
    mode: str = "a"
    tags: list[str] = []
    full: str | None = None

    def __init__(self, **data):
        val = data.get("mode")
        if val and hasattr(val, "value"):
            data["mode"] = val.value
        if data.get("tags") is None:
            data["tags"] = []
        super().__init__(**data)
        if self.name:
            self.full = f"{self.name}:{self.mode}"


class SubTagged(Tagged):
    n: int = 0


@router.post("/tagged")
async def tagged(body: dict):
    from pydantic import ValidationError
    from .enums import Channel
    from types import SimpleNamespace
    obj = SimpleNamespace(name=body.get("name"), mode=body.get("mode", "a"), tags=body.get("tags") or [])
    out = {"v": Tagged.model_validate(obj).model_dump(),
           "e": Tagged(name="x", mode=Channel.SMS, tags=None).model_dump(), "s": SubTagged(name="y", n=2).model_dump(),
           "fs": sorted(Tagged(name="z").model_fields_set)}
    try:
        out["t"] = Tagged(**body).model_dump()
    except ValidationError as e:
        out["t"] = [x["type"] for x in e.errors()]
    return out


@router.post("/decimal-in")
async def decimal_in(p: Price):
    return {"amount": str(p.amount), "dump": p.model_dump(mode="json"), "raw": p.model_dump(),
            "is_dec": isinstance(p.amount, Decimal), "plus": str(p.amount + 1)}


@router.post("/decimal-echo", response_model=Price)
async def decimal_echo(p: Price):
    return p


@router.post("/decimal-out", response_model=Price)
async def decimal_out(body: dict):
    """response_model validation of plain values (floats through their repr)."""
    return body


# ---- is_(True), join without ON, in_(select), injected Response headers, recursion
from fastapi import Response  # noqa: E402


def _coerce(v):
    if isinstance(v, dict):
        return {str(k): _coerce(x) for k, x in v.items()}
    if isinstance(v, (list, tuple)):
        return [_coerce(x) for x in v]
    return str(v) if isinstance(v, Decimal) else v


@router.get("/sqlmore")
async def sql_more(response: Response, db: DbDep):
    o = Owner(name="more")
    db.add(o)
    await db.flush()
    db.add_all([Project(name="m1", owner_id=o.id), Task(title="mt", done=True) if hasattr(Task, "done") else Task(title="mt")])
    await db.flush()
    owned = select(Owner.id).where(Owner.name == "more")
    joined = (await db.execute(select(Project.name, Owner.name).join(Owner).where(Project.owner_id.in_(owned)))).all()
    notin = (await db.execute(select(func.count(Project.id)).where(Project.owner_id.not_in(owned)))).scalar()
    flags = (await db.execute(select(func.count(Task.id)).where(Task.title.isnot(None).is_(True)))).scalar()
    try:
        await db.execute(select(Owner).join(Task))
        err = None
    except Exception as e:  # noqa: BLE001
        err = type(e).__name__
    response.headers["X-Total-Count"] = str(len(joined))
    response.headers["x-total-count"] = "again"
    response.headers.append("X-Multi", "a")
    response.headers.append("X-Multi", "b")
    return {"joined": [list(r) for r in joined], "notin": notin >= 0, "flags": flags >= 1, "join_err": err,
            "coerced": _coerce({"a": [Decimal("1.5"), {"b": (Decimal("2"),)}]})}


@router.get("/sets")
async def set_cases():
    s = {"a", "b", "c"}
    t = frozenset({"a", "x"})
    d = s.difference(["a"], ("b",))
    u = s.union(["z"], {"y"})
    i = s.intersection("abq")
    x = s.symmetric_difference(["a", "q"])
    m = set(s)
    m.difference_update(["c"])
    n = set(s)
    n.intersection_update("ab", ["b"])
    return {"d": sorted(d), "u": sorted(u), "i": sorted(i), "x": sorted(x), "m": sorted(m), "n": sorted(n),
            "t": sorted(t - set(["x"])), "sub": [s.issubset("abcd"), s.issuperset(["a"]), s.isdisjoint(["q"])]}


# ---- module globals are evaluated at import (startup), not on first use; label references
import time as _time  # noqa: E402

_STARTED = _time.time()


@router.get("/startup")
async def startup_cases(db: DbDep):
    uptime = _time.time() - _STARTED
    o = Owner(name="lbl")
    db.add(o)
    await db.flush()
    db.add_all([Project(name="g1", owner_id=o.id), Project(name="g2", owner_id=o.id)])
    await db.flush()
    rows = (await db.execute(
        select(Project.owner_id.label("own"), func.count(Project.id).label("n")).where(Project.owner_id == o.id)
        .group_by("own").order_by("own")
    )).all()
    try:
        select(Project.id).order_by("nope")
        err = None
    except Exception as e:  # noqa: BLE001
        err = type(e).__name__
    return {"uptime_ok": uptime >= 0.4, "rows": [r.n for r in rows], "err": err}


@router.get("/numtypes")
async def num_types(db: DbDep):
    db.add_all([Task(title="nt1", price=12.5), Task(title="nt2", price=7)])
    await db.flush()
    row = (await db.execute(select(
        func.sum(Task.price).filter(Task.title.like("nt%")), func.sum(Task.price * 2).filter(Task.title.like("nt%")),
        func.round(func.sum(Task.price), 1), func.avg(Task.price), func.count(Task.id) * 1.5,
    ))).one()
    return {"types": [type(x).__name__ for x in row], "sum_ok": row[0] >= 19.5}


# ---- a BaseSettings body run like a script: class-level if/elif, earlier attributes,
# alembic's Config read from an ini file
import os  # noqa: E402

from alembic.config import Config as AlembicConfig  # noqa: E402
from pydantic_settings import BaseSettings  # noqa: E402

INI = AlembicConfig(os.path.join(os.path.dirname(__file__), "test_alembic.ini"))
INI.set_main_option("script_location", os.path.realpath(os.path.join(__file__, "..", "migrations")))


class ScriptSettings(BaseSettings):
    PROJECT: str = "APP42"
    AUDIENCE: str = PROJECT
    HOST: str = os.getenv("SCRIPT_HOST", "")
    URL: str = os.getenv("SCRIPT_URL", "")
    if URL:
        URL = URL.replace("postgresql://", "postgresql+psycopg://")
        INI.set_main_option("sqlalchemy.url", URL)
    elif HOST:
        URL: str = f"postgresql+psycopg://{quote_plus('us er')}:{quote_plus('p%ss').replace('%', '%%')}@{HOST}/db"
        INI.set_main_option("sqlalchemy.url", URL)
    DB_URI: str = INI.get_main_option("sqlalchemy.url").replace("postgresql://", "postgresql+psycopg://")
    MAX: int = 3
    LIMIT: int = MAX * 10
    TOKEN: str

    def is_prod(self) -> bool:
        return self.PROJECT == "prod"


@router.get("/script-settings")
async def script_settings():
    s = ScriptSettings(TOKEN="t")
    try:
        ScriptSettings()
        missing = None
    except Exception as e:  # noqa: BLE001
        missing = type(e).__name__
    try:
        INI.set_main_option("bad", "50%")
    except ValueError as e:
        bad = str(e)
    return {"vals": [s.PROJECT, s.AUDIENCE, s.URL, s.DB_URI, s.LIMIT, s.is_prod()],
            "ini": [INI.get_main_option("script_location").endswith("/fixtures/dynapp/migrations"),
                    INI.get_main_option("version_locations").replace(os.path.dirname(os.path.abspath(__file__)), "<here>"),
                    INI.get_main_option("nope", "dflt"), INI.get_section_option("post_write_hooks", "hooks")],
            "missing": missing, "bad": bad}


# ---- pydantic URL types (pydantic-core parses with the `url` crate)
from pydantic import AnyUrl, HttpUrl, RedisDsn  # noqa: E402


class UrlIn(BaseModel):
    site: HttpUrl
    any: AnyUrl | None = None
    redis: RedisDsn | None = None


class UrlSettings(BaseSettings):
    REDIS_DSN: RedisDsn | None = "redis://"


@router.post("/urls")
async def url_cases(body: UrlIn):
    r = body.redis
    return {"body": body, "str": [str(body.site), f"{body.any}"], "redis": [str(r), r.host, r.port, r.path, r.scheme] if r else None,
            "site": [body.site.host, body.site.port, body.site.path, body.site.query, body.site.fragment],
            "dump": body.model_dump(mode="json"), "settings": f"{UrlSettings().REDIS_DSN}",
            "env": str(UrlSettings(REDIS_DSN="rediss://u:p@cache").REDIS_DSN)}


# ---- pydantic-settings env_parse_none_str and case_sensitive (env set in scripts_start_dyn.sh)
from pydantic import ValidationError as _SettingsError  # noqa: E402
from pydantic_settings import SettingsConfigDict  # noqa: E402


class NoneSettings(BaseSettings):
    model_config = SettingsConfigDict(case_sensitive=True, env_parse_none_str="none", env_prefix="DYNAPP_")
    NONE_INT: int | None = 5
    NONE_STR: str | None = "x"
    NONE_LIST: list[int] | None = [1]
    NONE_CASE: str | None = "c"
    NONE_DICT: dict[str, int | None] | None = None
    lower: str = "dflt"
    NONE_REQ: int = 3
    RAW: list[int] | str | None = None
    JSON: list[int] | None = None


class LooseNoneSettings(BaseSettings):
    model_config = SettingsConfigDict(env_parse_none_str="none", env_prefix="dynapp_")
    none_int: int | None = 5
    lower: str = "dflt"
    none_req: int = 1


class ChildNoneSettings(LooseNoneSettings):
    model_config = SettingsConfigDict(env_parse_none_str="None")
    none_int: str | None = "i"
    none_case: str | None = "c"


@router.get("/none-settings")
async def none_settings():
    out = [NoneSettings(NONE_REQ=1).model_dump(), LooseNoneSettings(none_req=2).model_dump(),
           ChildNoneSettings(none_req=2).model_dump()]
    # missing required value from the env -> None; an init keyword is not parsed
    try:
        NoneSettings()
    except _SettingsError as e:
        out.append(e.errors(include_url=False))
    try:
        LooseNoneSettings()
    except _SettingsError as e:
        out.append(e.errors(include_url=False))
    try:
        NoneSettings(NONE_REQ=1, NONE_INT="none")
    except _SettingsError as e:
        out.append(e.errors(include_url=False))
    return out


# ---- python-jose decode options (tokens of an identity provider read unverified)
from jose import jwt as _jwt  # noqa: E402
from jose.exceptions import JWTClaimsError as _ClaimsError  # noqa: E402


@router.get("/jose-options")
async def jose_options():
    now = int(_time.time())
    tok = _jwt.encode({"sub": "u1", "aud": "app", "iss": "idp", "exp": now - 30, "groups": ["a"]}, "k1", algorithm="HS256")
    out = []
    cases = [
        lambda: _jwt.decode(tok, key=None, options={"verify_signature": False, "verify_aud": False}),
        lambda: _jwt.decode(tok, "k1", algorithms=["HS256"], options={"verify_exp": False}, audience="app"),
        lambda: _jwt.decode(tok, "k1", algorithms=["HS256"], audience="app", options={"leeway": 60}),
        lambda: _jwt.decode(tok, "k1", algorithms=["HS256"], audience="other", options={"leeway": 60}),
        lambda: _jwt.decode(tok, "k1", algorithms=["HS256"], audience="app", issuer="nope", options={"leeway": 60}),
        lambda: _jwt.decode(tok, "k1", algorithms=["HS256"], audience="app", issuer=["idp"], subject="u2", options={"leeway": 60}),
        lambda: _jwt.decode(tok, "bad", options={"verify_signature": False, "verify_exp": False, "require_jti": True, "verify_aud": False}),
        lambda: _jwt.decode(tok, "bad", algorithms=["HS256"]),
        lambda: _jwt.decode(tok, None, options={"verify_signature": False, "verify_aud": False, "verify_exp": False}, audience=5),
    ]
    for f in cases:
        try:
            out.append(sorted(f()))
        except Exception as e:  # noqa: BLE001
            out.append([type(e).__name__, str(e), isinstance(e, _ClaimsError)])
    return out


# ---- yarl.URL, aiohttp options of a Prometheus-style client (ssl=, proxy=None, timeout=None)
@router.get("/yarl")
async def yarl_route():
    from yarl import URL

    u = URL("http://127.0.0.1:9090/api/v1?x=1#f")
    v = URL("https://user:pw@[::1]/p")
    w = URL("relative/path")
    s = aiohttp.ClientSession(timeout=None, auth=None)
    try:
        async with s.get("http://127.0.0.1:8299/certs", headers=None, params=None, proxy=None, ssl=False, timeout=None) as r:
            status = r.status
    finally:
        await s.close()
    return {"u": [u.host, u.port, u.scheme, u.path, u.query_string, u.fragment, str(u)],
            "v": [v.host, v.port, v.user, v.password, v.explicit_port], "w": [w.host, w.port, w.path], "status": status}

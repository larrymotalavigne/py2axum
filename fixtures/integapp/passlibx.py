"""passlib 1.7.4: CryptContext of bcrypt, as FastAPI templates write it (hash at sign-up, verify at login). With
bcrypt 4.3 the backend loads once (passlib logs "(trapped) error reading bcrypt version", bcrypt 4.1 having dropped
`__about__`); with bcrypt 5.0 its self-test fails and every hash or verify raises ValueError (reproduced). Fresh hashes
are random: the scenario checks their shape and verifies them with the bcrypt package (tests/scenarios/integapp.py)."""
from fastapi import APIRouter, HTTPException
from passlib.context import CryptContext
from passlib.exc import PasswordSizeError, PasswordValueError, UnknownHashError
from pydantic import BaseModel

router = APIRouter(prefix="/passlib")

pwd_context = CryptContext(schemes=["bcrypt"], deprecated="auto")
legacy = CryptContext(["bcrypt"], deprecated=["auto"])
plain = CryptContext(schemes=("bcrypt",))

# hashes made by passlib (rounds 4, a fixed salt): verified natively
H2A = "$2a$04$abcdefghijklmnopqrstuuLZYjhNQAOdpbzt4WxWlUHjv1wsyH5DG"
H2B = "$2b$04$abcdefghijklmnopqrstuuLZYjhNQAOdpbzt4WxWlUHjv1wsyH5DG"
H2Y = "$2y$04$abcdefghijklmnopqrstuuLZYjhNQAOdpbzt4WxWlUHjv1wsyH5DG"
H20 = "$2$04$abcdefghijklmnopqrstuuBwnQvEurWGYE7ZuLPCOGyVZxAr7zgvu"
H20_EMPTY = "$2$04$abcdefghijklmnopqrstuubyCG3zY1GIXMyxfivm.ClDiInHzxjiq"
H72 = "$2b$04$abcdefghijklmnopqrstuuNt.0ah97gHY3F7JC1Z6EXmQc0bQBhIO"
HUTF8 = "$2b$04$abcdefghijklmnopqrstuuw2MKV/2i0FmjX61.g1rM8OfW8o0Irqu"


def _try(f):
    try:
        return repr(f())
    except (UnknownHashError, PasswordValueError) as e:
        return f"passlib {type(e).__name__}: {e}"
    except Exception as e:  # noqa: BLE001
        return f"{type(e).__name__}: {e}"


class Signup(BaseModel):
    username: str
    password: str


USERS: dict[str, str] = {}


def get_password_hash(password: str) -> str:
    return pwd_context.hash(password)


def verify_password(plain_password: str, hashed_password: str) -> bool:
    return pwd_context.verify(plain_password, hashed_password)


@router.post("/signup")
async def signup(body: Signup):
    """errors escape: 500 under bcrypt 5.0 (the log conformance compares the traceback)"""
    USERS[body.username] = get_password_hash(body.password)
    h = USERS[body.username]
    return {"pw": body.password, "hash": h}


@router.post("/login")
async def login(body: Signup):
    h = USERS.get(body.username)
    if not h or not verify_password(body.password, h):
        raise HTTPException(status_code=401, detail="Incorrect username or password")
    return {"ok": True, "needs_update": pwd_context.needs_update(h)}


@router.get("/fresh")
async def fresh(pw: str = "s3cret", ctx: str = "main"):
    c = {"main": pwd_context, "legacy": legacy, "plain": plain}[ctx]
    try:
        h = c.hash(pw)
    except Exception as e:  # noqa: BLE001
        return {"error": f"{type(e).__name__}: {e}"}
    return {
        "pw": pw,
        "hash": h,
        "verify": c.verify(pw, h),
        "wrong": c.verify(pw + "!", h),
        "bytes": c.verify(pw.encode(), h.encode()),
        "identify": c.identify(h),
        "needs_update": c.needs_update(h),
        "and_update": c.verify_and_update(pw, h),
    }


@router.get("/check")
async def check(h: str, pw: str = "s3cret"):
    """a hash of the request (passlib's, or a malformed one): every method on it"""
    return {
        "verify": _try(lambda: pwd_context.verify(pw, h)),
        "verify_bytes": _try(lambda: pwd_context.verify(pw.encode(), h.encode())),
        "needs_update": _try(lambda: pwd_context.needs_update(h)),
        "identify": _try(lambda: pwd_context.identify(h)),
        "required": _try(lambda: pwd_context.identify(h, required=True)),
        "and_update": _try(lambda: (lambda r: (r[0], r[1] is not None and r[1][:7]))(pwd_context.verify_and_update(pw, h))),
    }


@router.get("/raw")
async def raw(h: str, pw: str = "s3cret"):
    """no handler: an error is a 500"""
    return {"ok": pwd_context.verify(pw, h)}


@router.get("/known")
async def known():
    """passlib's own hashes, each ident, verified natively"""
    v = pwd_context.verify
    return [
        _try(lambda: [v("s3cret", h) for h in (H2A, H2B, H2Y, H20)]),
        _try(lambda: [v("s3creT", h) for h in (H2A, H2B, H2Y, H20)]),
        _try(lambda: [v(b"s3cret", h.encode()) for h in (H2A, H2B)]),
        _try(lambda: v("", H20_EMPTY)),
        _try(lambda: v("p" * 72, H72)),
        _try(lambda: v("p" * 100, H72)),  # bcrypt reads 72 bytes
        _try(lambda: v("p" * 71, H72)),
        _try(lambda: v("été 🌞", HUTF8)),
        _try(lambda: v("été 🌞".encode(), HUTF8)),
        _try(lambda: v("ete", HUTF8)),
        _try(lambda: legacy.verify("s3cret", H2B)),
        _try(lambda: plain.verify("s3cret", H2Y)),
        _try(lambda: [pwd_context.needs_update(h) for h in (H2A, H2B, H2Y, H20)]),
    ]


@router.get("/errors")
async def errors():
    v = pwd_context.verify
    return [
        _try(lambda: v("s3cret", None)),  # dummy_verify: never verifies
        _try(lambda: pwd_context.verify_and_update("s3cret", None)),
        _try(lambda: v(None, H2B)),
        _try(lambda: v(5, H2B)),
        _try(lambda: v(None, "nothash")),
        _try(lambda: v("s3cret", 5)),
        _try(lambda: v("s3cret", ["$2b$"])),
        _try(lambda: v("s3cret", "")),
        _try(lambda: v("s3cret", b"")),
        _try(lambda: v("a\x00b", H2B)),
        _try(lambda: v("x" * 4097, H2B)),
        _try(lambda: v("é" * 3000, H2B)),  # 3000 characters, 6000 bytes
        _try(lambda: v(b"x" * 4097, H2B)),
        _try(lambda: pwd_context.hash(None)),
        _try(lambda: pwd_context.hash(3.5)),
        _try(lambda: pwd_context.hash({"a": 1})),
        _try(lambda: pwd_context.hash("x" * 4097)),
        _try(lambda: pwd_context.hash("a\x00b")),
        _try(lambda: pwd_context.identify(None)),
        _try(lambda: pwd_context.identify(b"\xff\xfe")),
        _try(lambda: pwd_context.identify(b"$2b$\xff")),
        _try(lambda: pwd_context.needs_update("nothash")),
        _try(lambda: pwd_context.needs_update(None)),
        _try(lambda: pwd_context.dummy_verify()),
    ]


@router.get("/catch")
async def catch(h: str = "nothash"):
    try:
        pwd_context.verify("s3cret", h)
    except UnknownHashError as e:
        return {"caught": "unknown", "msg": str(e), "message": e.message, "value": e.value, "args": list(e.args),
                "value_error": isinstance(e, ValueError)}
    except PasswordSizeError as e:
        return {"caught": "size", "msg": str(e), "max": e.max_size}
    except ValueError as e:
        return {"caught": "value", "msg": str(e)}
    return {"caught": None}


@router.get("/size")
async def size():
    try:
        pwd_context.hash("x" * 5000)
    except PasswordSizeError as e:
        return {"msg": str(e), "max": e.max_size, "value_error": isinstance(e, PasswordValueError)}

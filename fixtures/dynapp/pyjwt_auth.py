"""PyJWT (`import jwt`): a real application's authentication after python-jose (HS256 pinned at decode,
`jwt.InvalidTokenError`), then the decode options, claim checks, keys and exceptions."""
import time as _time
from datetime import UTC, datetime, timedelta

import jwt
from fastapi import APIRouter, Depends, HTTPException, Query
from fastapi.security import OAuth2PasswordBearer

router = APIRouter(prefix="/pyjwt")
SECRET = "s3cret-pyjwt-0123456789abcdef-0123"
ALGORITHM = "HS256"
oauth2 = OAuth2PasswordBearer(tokenUrl="/token")


def _err(e: Exception) -> list:
    return [type(e).__name__, str(e), list(e.args), isinstance(e, jwt.InvalidTokenError), isinstance(e, jwt.DecodeError),
            isinstance(e, jwt.PyJWTError), getattr(e, "claim", None)]


def create_access_token(data: dict, expires_delta: timedelta | None = None) -> str:
    to_encode = data.copy()
    expire = datetime.now(UTC) + (expires_delta or timedelta(minutes=15))
    to_encode.update({"exp": expire, "type": "access"})
    return jwt.encode(to_encode, SECRET, algorithm=ALGORITHM)


def verify_token(token: str, kind: str) -> str | None:
    try:
        payload = jwt.decode(token, SECRET, algorithms=[ALGORITHM])
        email: str = payload.get("sub")
        if email is None or payload.get("type") != kind:
            return None
        return email
    except jwt.InvalidTokenError:
        return None


@router.post("/issue")
async def issue(body: dict, alg: str = "HS256", kid: str | None = None, sort: bool = True):
    """The caller's dict keeps its datetimes (PyJWT encodes a copy)."""
    claims = dict(body)
    claims["exp"] = datetime(2100, 1, 1, 12, 30, 15, 999)
    claims["iat"] = datetime(2020, 6, 1, 8, 0, tzinfo=UTC)
    headers = {"kid": kid, "x-a": 1} if kid else None
    try:
        token = jwt.encode(claims, SECRET, algorithm=alg, headers=headers, sort_headers=sort)
    except Exception as e:  # noqa: BLE001
        return _err(e)
    return {"token": token, "claims": claims, "header": jwt.get_unverified_header(token)}


@router.get("/check")
async def check(token: str, algs: list[str] = Query(default=["HS256"]), key: str = SECRET, aud: str | None = None,
                iss: str | None = None, leeway: float = 0):
    try:
        claims = jwt.decode(token, key, algorithms=algs, audience=aud, issuer=iss, leeway=leeway)
    except jwt.ExpiredSignatureError as e:
        return {"error": "expired", "e": _err(e)}
    except jwt.InvalidAudienceError as e:
        return {"error": "aud", "e": _err(e)}
    except jwt.InvalidTokenError as e:
        return {"error": "invalid", "e": _err(e)}
    except jwt.PyJWTError as e:
        return {"error": "base", "e": _err(e)}
    return {"claims": claims}


@router.get("/me")
async def me(token: str = Depends(oauth2)):
    email = verify_token(token, "access")
    if email is None:
        raise HTTPException(status_code=401, detail="Could not validate credentials", headers={"WWW-Authenticate": "Bearer"})
    return {"sub": email}


@router.get("/roundtrip")
async def roundtrip(sub: str = "ada@example.com"):
    """A token issued now and read back (the expiry is not compared byte for byte: it is the clock)."""
    token = create_access_token({"sub": sub})
    payload = jwt.decode(token, SECRET, algorithms=[ALGORITHM])
    left = payload["exp"] - _time.time()
    return {"sub": verify_token(token, "access"), "refresh": verify_token(token, "refresh"), "keys": list(payload),
            "ttl": 890 < left <= 900}


@router.get("/cases")
async def cases():
    now = int(_time.time())
    tok = jwt.encode({"sub": "u1", "aud": "app", "iss": "idp", "exp": now - 30, "groups": ["a"]}, "k1", algorithm="HS256")
    fresh = jwt.encode({"sub": "u1", "exp": now + 3600}, "k1")
    pem = "-----BEGIN PUBLIC KEY-----\nMIIB\n-----END PUBLIC KEY-----\n"
    tests = [
        lambda: jwt.decode(tok, options={"verify_signature": False}),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"]),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], audience="app", leeway=60),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], audience="other", leeway=timedelta(seconds=60)),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], audience=["x", "app"], leeway=60.5),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], audience="app", issuer="nope", leeway=60),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], audience="app", issuer=["idp"], subject="u2", leeway=60),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], audience="app", issuer=5, leeway=60),
        lambda: jwt.decode(tok, options={"verify_signature": False, "require": ["jti"]}),
        lambda: jwt.decode(tok, options={"verify_signature": False, "require": "exp"}),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], options={"verify_exp": False}),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], options={"verify_exp": False, "strict_aud": True}, audience="app"),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], options={"verify_exp": False, "strict_aud": True}, audience=["app"]),
        lambda: jwt.decode(fresh, "k1", algorithms=["HS256"], audience="app"),
        lambda: jwt.decode(tok, "k1"),
        lambda: jwt.decode(tok, "", algorithms=["HS256"]),
        lambda: jwt.decode(tok, pem, algorithms=["HS256"]),
        lambda: jwt.decode(tok, '{"keys": [{"kty": "oct", "k": "eA"}]}', algorithms=["HS256"]),
        lambda: jwt.decode(tok, "ssh-rsa AAAAB3Nza", algorithms=["HS256"]),
        lambda: jwt.decode(tok, b"k1", algorithms=("HS256", "HS512"), options={"verify_exp": False, "enforce_minimum_key_length": True}, audience="app"),
        lambda: jwt.decode(12345, "k1", algorithms=["HS256"]),
        lambda: jwt.decode(tok, "k1", algorithms="HS256", audience=5, leeway=60),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], audience="app", leeway="x"),
        lambda: jwt.decode(tok, "k1", algorithms=["HS256"], options="x"),
        lambda: jwt.decode(tok, "k2", algorithms=["HS256"]),
        lambda: jwt.decode(tok, "k1", algorithms=["HS384"]),
        lambda: sorted(jwt.decode_complete(fresh, "k1", algorithms=["HS256"])),
        lambda: jwt.decode_complete(fresh, "k1", algorithms=["HS256"])["header"],
        lambda: len(jwt.decode_complete(fresh, "k1", algorithms=["HS256"])["signature"]),
        lambda: jwt.encode({"a": 1}, "", algorithm="none"),
        lambda: jwt.encode({"a": 1}, None, algorithm=None),
        lambda: jwt.encode({"a": 1}, "k", algorithm="none"),
        lambda: jwt.decode(jwt.encode({"a": 1}, None, algorithm="none"), None, algorithms=["none"]),
        lambda: jwt.decode(jwt.encode({"a": 1}, None, algorithm="none"), "k1", algorithms=["HS256", "none"]),
        lambda: jwt.decode(jwt.encode({"iat": now + 3600}, "k1"), "k1", algorithms=["HS256"]),
        lambda: jwt.decode(jwt.encode({"iat": now + 30}, "k1"), "k1", algorithms=["HS256"], leeway=120),
        lambda: jwt.decode(jwt.encode({"nbf": "abc"}, "k1"), "k1", algorithms=["HS256"]),
        lambda: jwt.decode(jwt.encode({"nbf": now + 3600}, "k1"), "k1", algorithms=["HS256"]),
        lambda: jwt.decode(jwt.encode({"iat": "x"}, "k1"), "k1", algorithms=["HS256"]),
        lambda: jwt.decode(jwt.encode({"iat": "12", "exp": "4102444800"}, "k1"), "k1", algorithms=["HS256"]),
        lambda: jwt.decode(jwt.encode({"sub": 5}, "k1"), "k1", algorithms=["HS256"]),
        lambda: jwt.decode(jwt.encode({"jti": 5}, "k1"), "k1", algorithms=["HS256"]),
        lambda: jwt.decode(jwt.encode({"aud": ["x", 1]}, "k1"), "k1", algorithms=["HS256"], audience="x"),
        lambda: jwt.decode(jwt.encode({"aud": ""}, "k1"), "k1", algorithms=["HS256"], audience="x"),
        lambda: jwt.decode(jwt.encode({"a": 1}, "k1"), "k1", algorithms=["HS256"], issuer="idp"),
        lambda: jwt.encode({"a": 1}, "k1", headers={"kid": 5}),
        lambda: jwt.encode({"iss": 5}, "k1"),
        lambda: jwt.encode([1], "k1"),
        lambda: jwt.encode({"a": 1}, "k1", algorithm="XX"),
        lambda: jwt.encode({"a": 1}, "k1", headers={"typ": None, "alg": "HS384", "kid": "k"}, sort_headers=False),
        lambda: jwt.get_unverified_header(jwt.encode({"a": 1}, "k1", headers={"typ": None, "alg": "HS384", "kid": "k"}, sort_headers=False)),
        lambda: jwt.decode(jwt.encode({"a": 1}, "k1", headers={"alg": "HS384"}), "k1", algorithms=["HS256"]),
        lambda: jwt.decode(jwt.encode({"a": 1}, "k1", headers={"crit": ["exp"]}), "k1", algorithms=["HS256"]),
        lambda: jwt.decode(jwt.encode({"a": 1}, "k1", headers={"crit": []}), "k1", algorithms=["HS256"]),
        lambda: jwt.encode({"a": 1}, "k1", headers={"b64": False}),
        lambda: jwt.decode(jwt.encode({"a": 1}, "k1", headers={"b64": False}), "k1", algorithms=["HS256"]),
        lambda: jwt.get_unverified_header("e30.e30.x"),
    ]
    out = []
    for f in tests:
        try:
            r = f()
            # exp/iat follow the clock
            out.append(sorted((k, v) for k, v in r.items() if k not in ("exp", "iat")) if isinstance(r, dict) else r)
        except Exception as e:  # noqa: BLE001
            out.append(_err(e))
    return out

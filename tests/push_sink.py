"""Web push sink for the dynapp conformance (port 8299): decrypts like a browser, checks the VAPID token
like a push service. Its own process: pywebpush's `webpush` is synchronous and blocks the server calling it."""
import base64
import json

import http_ece
from cryptography.hazmat.primitives.asymmetric import ec
from fastapi import FastAPI, Request
from fastapi.responses import PlainTextResponse
from py_vapid import Vapid02

from fixtures.dynapp.libs import AUTH, RECEIVER_PRIV, VAPID_PUB

app = FastAPI()


def _pad(s: str) -> str:
    return s + "=" * (-len(s) % 4)


@app.post("/sink")
async def sink(request: Request, gone: bool = False):
    if gone:
        return PlainTextResponse("push subscription has unsubscribed or expired.", status_code=410)
    priv = ec.derive_private_key(int.from_bytes(base64.urlsafe_b64decode(_pad(RECEIVER_PRIV)), "big"), ec.SECP256R1())
    body = await request.body()
    plain = http_ece.decrypt(body, private_key=priv, auth_secret=base64.urlsafe_b64decode(_pad(AUTH)), version="aes128gcm")
    auth = request.headers["authorization"]
    token = auth.split(" ", 1)[1].split(",")[0][2:]
    claims = json.loads(base64.urlsafe_b64decode(_pad(token.split(".")[1])))
    return {"plain": json.loads(plain), "vapid_ok": Vapid02.verify(auth), "k": auth.endswith("k=" + VAPID_PUB),
            "claims": sorted(claims), "sub": claims["sub"], "aud": claims["aud"],
            "headers": [request.headers.get(h) for h in ("content-encoding", "ttl", "content-type")],
            "rs": int.from_bytes(body[16:20], "big"), "idlen": body[20]}


# ---- Google-style ID tokens: certificates (x509 per key id) and tokens to verify
import datetime as _dt  # noqa: E402
import time as _time  # noqa: E402

from cryptography import x509 as _x509  # noqa: E402
from cryptography.hazmat.primitives import hashes as _hashes, serialization as _ser  # noqa: E402
from cryptography.hazmat.primitives.asymmetric import rsa as _rsa  # noqa: E402
from cryptography.x509.oid import NameOID as _NameOID  # noqa: E402
from google.auth import crypt as _crypt, jwt as _gjwt  # noqa: E402


def _cert(key):
    name = _x509.Name([_x509.NameAttribute(_NameOID.COMMON_NAME, "py2axum-test")])
    now = _dt.datetime.now(_dt.UTC)
    c = (_x509.CertificateBuilder().subject_name(name).issuer_name(name).public_key(key.public_key())
         .serial_number(1).not_valid_before(now - _dt.timedelta(days=1)).not_valid_after(now + _dt.timedelta(days=30))
         .sign(key, _hashes.SHA256()))
    return c.public_bytes(_ser.Encoding.PEM).decode()


_RSA = _rsa.generate_private_key(public_exponent=65537, key_size=2048)
_RSA2 = _rsa.generate_private_key(public_exponent=65537, key_size=2048)
_EC = ec.generate_private_key(ec.SECP256R1())
_CERTS = {"k1": _cert(_RSA), "k2": _cert(_RSA2), "e1": _cert(_EC)}


def _signer(key, kid):
    pem = key.private_bytes(_ser.Encoding.PEM, _ser.PrivateFormat.PKCS8, _ser.NoEncryption()).decode()
    if isinstance(key, ec.EllipticCurvePrivateKey):
        from google.auth.crypt import es256
        return es256.ES256Signer.from_string(pem, kid)
    return _crypt.RSASigner.from_string(pem, kid)


@app.get("/certs")
async def certs():
    return _CERTS


@app.get("/tokens")
async def tokens():
    now = int(_time.time())
    base = {"iss": "https://accounts.google.com", "aud": "client-1", "sub": "1234", "email": "ana@example.com",
            "iat": now - 10, "exp": now + 600}
    t = {
        "valid": _gjwt.encode(_signer(_RSA, "k1"), base),
        "nokid": _gjwt.encode(_signer(_RSA2, None), base),
        "es256": _gjwt.encode(_signer(_EC, "e1"), base),
        "expired": _gjwt.encode(_signer(_RSA, "k1"), {**base, "iat": now - 900, "exp": now - 300}),
        "future": _gjwt.encode(_signer(_RSA, "k1"), {**base, "iat": now + 300}),
        "aud": _gjwt.encode(_signer(_RSA, "k1"), {**base, "aud": "other"}),
        "audlist": _gjwt.encode(_signer(_RSA, "k1"), {**base, "aud": ["client-1"]}),
        "badkey": _gjwt.encode(_signer(_RSA2, "k1"), base),
        "unknownkid": _gjwt.encode(_signer(_RSA, "zz"), base),
        "noexp": _gjwt.encode(_signer(_RSA, "k1"), {k: v for k, v in base.items() if k != "exp"}),
    }
    t = {k: v.decode() for k, v in t.items()}
    t["segments"] = "a.b"
    t["garbage"] = "eyJhbGciOiJSUzI1NiJ9.bm90IGpzb24.c2ln"
    return t

"""The standard library and `cryptography` as a mail service uses them: `collections.Counter` (a log analysis),
`asyncio.open_connection` (a TCP health probe under `asyncio.wait_for`), a DKIM RSA key pair."""
import asyncio
import base64
import io
import socket as _socket
import zipfile
from collections import Counter

from cryptography.hazmat.backends import default_backend
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from fastapi import APIRouter

router = APIRouter(prefix="/stdmore")


@router.post("/counter")
async def counter(body: list[str]):
    c = Counter(x for x in body if x)
    d = Counter({"a": 2, "b": -1})
    d.update(["a", "c", "c"])
    d.update({"b": 3})
    d.subtract(["c"])
    e = Counter(a=1, z=0)
    out = {"c": dict(c), "most": c.most_common(), "top2": c.most_common(2), "missing": c["nope"],
           "stored": "nope" in c, "d": dict(d), "elements": sorted(d.elements()), "total": d.total(),
           "add": dict(c + d), "sub": dict(d - e), "or": dict(d | e), "and": dict(d & e),
           "repr": [repr(Counter()), repr(d), repr(e)], "get": c.get("x", 0), "len": len(c), "copy": repr(d.copy())}
    c["new"] += 1
    out["incr"] = [c["new"], sorted(c.items())]
    for ip, count in c.most_common(5):
        if count > 1:
            out.setdefault("often", []).append(f"{ip}:{count}")
    return out


@router.get("/tcp")
async def tcp(host: str = "127.0.0.1", port: int = 9, timeout: float = 2.0, send: str | None = None):
    try:
        reader, writer = await asyncio.wait_for(asyncio.open_connection(host, port), timeout=timeout)
    except Exception as e:  # noqa: BLE001
        return {"ok": False, "type": type(e).__name__, "error": str(e)[:200], "oserror": isinstance(e, OSError)}
    out = {"ok": True, "peer": list(writer.get_extra_info("peername"))[:2], "closing": writer.is_closing()}
    if send is not None:
        writer.write(send.encode() + b"\r\n")
        await writer.drain()
        out["line"] = (await reader.readline()).decode()
    writer.close()
    out["closing_after"] = writer.is_closing()
    await writer.wait_closed()
    return out


def generate_dkim_keypair(key_size: int, exponent: int) -> tuple[str, str]:
    private_key = rsa.generate_private_key(public_exponent=exponent, key_size=key_size, backend=default_backend())
    private_pem = private_key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    ).decode("utf-8")
    public_der = private_key.public_key().public_bytes(
        encoding=serialization.Encoding.DER, format=serialization.PublicFormat.SubjectPublicKeyInfo)
    return private_pem, base64.b64encode(public_der).decode("utf-8")


@router.get("/rsa")
async def rsa_key(bits: int = 1024, e: int = 65537):
    """Keys are random: their shape is compared (PEM armour and line widths, DER header and length)."""
    try:
        pem, pub = generate_dkim_keypair(bits, e)
    except ValueError as x:
        return {"error": str(x)}
    key = rsa.generate_private_key(e, bits)
    trad = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.TraditionalOpenSSL,
                             serialization.NoEncryption())
    der = key.private_bytes(serialization.Encoding.DER, serialization.PrivateFormat.PKCS8, serialization.NoEncryption())
    pub_pem = key.public_key().public_bytes(serialization.Encoding.PEM, serialization.PublicFormat.PKCS1)
    spki_pem = key.public_key().public_bytes(serialization.Encoding.PEM, serialization.PublicFormat.SubjectPublicKeyInfo)
    lines = pem.splitlines()
    return {"key_size": key.key_size, "pub_size": key.public_key().key_size, "type": type(key).__name__,
            "pem": [lines[0], lines[-1], sorted({len(x) for x in lines[1:-2]}), pem.endswith("\n"), pem.count("\r")],
            "trad": trad.splitlines()[0].decode(), "der": der[:4].hex()[:2] + der[4:7].hex(),
            "pub_pem": [pub_pem.splitlines()[0].decode(), len(pub_pem)], "spki": [spki_pem.splitlines()[-1].decode(), len(spki_pem)],
            "dns": f"v=DKIM1; k=rsa; p={pub}"[:24], "b64len": len(pub), "der_pub": base64.b64decode(pub)[:3].hex()}


@router.get("/sock")
async def sock(host: str = "127.0.0.1", port: int = 9, timeout: float | None = 5):
    """a fail2ban probe: a blocking `socket.create_connection` in a `with` block."""
    try:
        with _socket.create_connection((host, port), timeout=timeout) as s:
            peer = list(s.getpeername())
        return {"ok": True, "peer": peer}
    except OSError as e:
        return {"ok": False, "type": type(e).__name__, "error": str(e), "timeout": isinstance(e, TimeoutError)}


@router.post("/zip")
async def make_zip(body: dict):
    """data exports: members written into an in-memory zip (the DOS timestamps are masked by the scenario)."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zf:
        for name, content in body.items():
            zf.writestr(name, content.encode() if name.endswith(".bin") else content)
        names = zf.namelist()
    stored = io.BytesIO()
    zf2 = zipfile.ZipFile(stored, "w")
    zf2.writestr("a.txt", "plain")
    zf2.writestr("b.txt", b"x" * 300, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
    zf2.writestr("dir/", "")
    zf2.close()
    try:
        zf2.writestr("late.txt", "x")
        late = None
    except ValueError as e:
        late = str(e)
    buf.seek(0)
    return {"names": names, "zip": base64.b64encode(buf.read()).decode(), "tell": buf.tell(),
            "stored": base64.b64encode(stored.getvalue()).decode(), "late": late}


@router.post("/lines")
async def lines(body: dict):
    b = body["text"].encode()
    return {"plain": [x.decode() for x in b.splitlines()], "kept": [x.decode() for x in b.splitlines(True)],
            "str": body["text"].splitlines()}

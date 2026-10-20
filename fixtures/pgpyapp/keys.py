"""An application's PGP key upload (`_parse_pgp_key`, kept as written), its static-import twin, and a route that
reports everything `from_blob()` returns."""
from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

router = APIRouter()


class KeyIn(BaseModel):
    key: str


def _import_pgpy():
    """Lazy import pgpy to avoid import-time failures on Python 3.13+.

    pgpy uses the removed 'imghdr' module. We provide a stub if needed.
    """
    import sys

    if "imghdr" not in sys.modules:
        import types

        stub = types.ModuleType("imghdr")
        stub.what = lambda *a, **kw: None
        sys.modules["imghdr"] = stub
    import pgpy

    return pgpy


def _parse_pgp_key(public_key_text: str) -> tuple[str, str, str]:
    """Parse and validate a PGP public key.

    Returns (fingerprint, key_id, email).
    Raises ValueError if invalid.
    """
    pgpy = _import_pgpy()
    try:
        key, _ = pgpy.PGPKey.from_blob(public_key_text)
    except Exception as e:
        raise ValueError(f"Invalid PGP key: {e}")

    if not key.is_public:
        raise ValueError("Only public keys are accepted. Do not upload your private key.")

    fingerprint = str(key.fingerprint).replace(" ", "")
    key_id = fingerprint[-16:]

    # Extract primary email from the key's UIDs
    email = None
    for uid in key.userids:
        if uid.email:
            email = uid.email.lower()
            break

    if not email:
        raise ValueError("PGP key must contain an email address in its UID")

    return fingerprint, key_id, email


@router.post("/pgp-keys", status_code=201)
async def upload(data: KeyIn):
    try:
        fingerprint, key_id, email = _parse_pgp_key(data.key)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    return {"fingerprint": fingerprint, "key_id": key_id, "email": email}


def _name(e) -> str:
    return f"{type(e).__name__}: {e}"


@router.post("/pgp-keys/detail")
async def detail(data: KeyIn):
    pgpy = _import_pgpy()
    try:
        r = pgpy.PGPKey.from_blob(data.key)
    except Exception as e:
        cause = e.__cause__
        return {"error": _name(e), "cause": None if cause is None else _name(cause)}
    key, po = r
    uids = []
    uid_error = None
    try:
        for u in key.userids:
            uids.append([u.name, u.comment, u.email, u.userid, u.is_uid, f"{u}"])
    except Exception as e:
        uid_error = _name(e)
    primary = []
    for u in key.userids + key.userattributes:
        try:
            primary.append(u.is_primary)
        except Exception as e:
            primary.append(_name(e))
    fp = key.fingerprint
    return {
        "len": len(r),
        "is_public": key.is_public,
        "is_primary": key.is_primary,
        "fingerprint": str(fp),
        "fp_keyid": None if fp is None else [fp.keyid, fp.shortid, fp == fp.keyid, fp == fp.shortid, fp == "x", len(fp),
                                             fp[-16:], fp.lower(), "A" in fp, fp + "!", isinstance(fp, str),
                                             hash(fp) == hash(str(fp)), fp != fp.shortid, {fp: 1}.get(str(fp))],
        "uid_primary": primary,
        "uids": uids,
        "uid_error": uid_error,
        "po": [[k[0], k[1]] for k in po],
        "po_same": [v is key for v in po.values()],
        "po_public": [v.is_public for v in po.values()],
        "subkeys": [[k, v.is_public, v.is_primary, str(v.fingerprint)] for k, v in key.subkeys.items()],
        "n_ua": len(key.userattributes),
        "ua": [[u.is_uid, u.is_ua, u.userid, u.name, u.email] for u in key.userattributes],
    }

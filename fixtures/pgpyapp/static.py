"""pgpy imported at the top of a module (after the imghdr stub of `imghdr_stub`): `pgpy.PGPKey.from_blob` resolved
statically."""
from fastapi import APIRouter, HTTPException

from . import imghdr_stub  # noqa: F401  (before pgpy: Python 3.13 removed imghdr)
import pgpy  # noqa: E402

from .keys import KeyIn  # noqa: E402

router = APIRouter()


@router.post("/static/pgp-keys")
async def static_upload(data: KeyIn):
    try:
        key, _ = pgpy.PGPKey.from_blob(data.key)
    except Exception as e:
        raise HTTPException(status_code=400, detail=f"Invalid PGP key: {type(e).__name__}: {e}")
    return {"public": key.is_public, "fingerprint": str(key.fingerprint),
            "emails": [u.email for u in key.userids if u.email]}

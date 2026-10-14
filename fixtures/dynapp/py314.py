"""Python 3.14 syntax and semantics (imported by main.py under `sys.version_info >= (3, 14)`): `except A, B`
without brackets (PEP 758), `continue` in a `finally` (PEP 765: a SyntaxWarning, same behaviour), annotations
evaluated lazily (PEP 649: a model can name a class defined below it, unquoted)."""
import uuid

from fastapi import APIRouter
from pydantic import BaseModel

router = APIRouter()


@router.get("/py314/except")
async def except_bare(v: str = "x"):
    try:
        return {"n": int(v)}
    except ValueError, TypeError:
        return {"n": None}


@router.get("/py314/finally")
async def finally_continue(n: int = 3):
    seen = []
    for i in range(n):
        try:
            if i == 1:
                raise KeyError(i)
            seen.append(i)
        finally:
            continue  # noqa: B012 — swallows the KeyError, as in every Python
    return {"seen": seen}


class Outer(BaseModel):
    inner: Inner | None = None
    items: list[Inner] = []


class Inner(BaseModel):
    v: int


def later_helper(x: NotDefinedAnywhere) -> int:  # noqa: F821 — never evaluated (PEP 649)
    return x + 1


@router.post("/py314/forward")
async def forward(o: Outer) -> Outer:
    return o


@router.get("/py314/lazy-annotation")
async def lazy_annotation(n: int = 1):
    return {"r": later_helper(n)}


@router.get("/py314/uuid7")
async def uuid7_shape():
    # uuid.uuid7(): time-ordered (version 7, RFC 9562), monotonic within a millisecond
    a, b = uuid.uuid7(), uuid.uuid7()
    return {"version": a.version, "ordered": str(a) < str(b), "len": len(str(a)), "hex": len(a.hex)}


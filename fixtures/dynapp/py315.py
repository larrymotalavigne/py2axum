"""Python 3.15 syntax and library changes (imported by main.py under `sys.version_info >= (3, 15)`): unpacking
in comprehensions (PEP 798), `lazy import` of a library (PEP 810), a unary plus in a match pattern,
re.prefixmatch(), base64's `padded=`."""
lazy import base64
lazy from re import compile as re_compile

import re

from fastapi import APIRouter

router = APIRouter()


@router.get("/py315/unpack")
async def unpack():
    lists = [[1, 2], (3,), "ab"]
    maps = [{"a": 1, "b": 1}, {"b": 2}]
    return {"list": [*x for x in lists], "set": sorted({*x for x in [[1, 2], [2, 3]]}),
            "gen": list((*x for x in lists)), "dict": {**m for m in maps}}


@router.get("/py315/unpack-error/{kind}")
async def unpack_error(kind: str):
    try:
        if kind == "list":
            return {"r": [*x for x in [1]]}
        if kind == "set":
            return {"r": sorted({*x for x in [1]})}
        if kind == "gen":
            return {"r": list((*x for x in [1]))}
        return {"r": {**m for m in [1]}}
    except TypeError as e:
        return {"e": str(e)}


@router.get("/py315/prefixmatch")
async def prefixmatch(s: str = "aab"):
    m = re.prefixmatch(r"a+", s)
    p = re_compile(r"a+").prefixmatch(s)
    return {"m": m.group() if m else None, "p": p.group() if p else None}


@router.get("/py315/padded")
async def padded(s: str = "YWJjYQ"):
    try:
        return {"r": base64.urlsafe_b64decode(s, padded=True).decode()}
    except Exception as e:
        return {"e": str(e), "unpadded": base64.urlsafe_b64decode(s).decode()}


@router.get("/py315/match")
async def match_plus(v: int = 1):
    match v:
        case +1:
            return {"r": "plus one"}
        case -1:
            return {"r": "minus one"}
        case _:
            return {"r": "other"}


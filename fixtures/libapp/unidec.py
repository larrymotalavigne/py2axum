"""Unidecode: transliteration of every Unicode block, the `errors=` modes, UnidecodeError."""
from fastapi import APIRouter
from unidecode import UnidecodeError, unidecode, unidecode_expect_ascii, unidecode_expect_nonascii

router = APIRouter(prefix="/unidecode")


@router.post("/run")
async def run(body: dict):
    errors, replace_str = body.get("errors", "ignore"), body.get("replace_str", "?")
    try:
        if body.get("fn") == "ascii":
            out = unidecode_expect_ascii(body["text"], errors=errors, replace_str=replace_str)
        elif body.get("fn") == "nonascii":
            out = unidecode_expect_nonascii(body["text"], errors, replace_str)
        elif "errors" in body:
            out = unidecode(body["text"], errors, replace_str=replace_str)
        else:
            out = unidecode(body["text"])
    except UnidecodeError as e:
        return {"err": type(e).__name__, "msg": str(e), "index": e.index, "value": isinstance(e, ValueError)}
    return {"out": out}


@router.get("/blocks/{start}")
async def blocks(start: int, count: int = 4096):
    """a run of consecutive code points (unassigned and private ones included), transliterated one by one"""
    return [unidecode(chr(cp), errors="replace", replace_str="<?>") for cp in range(start, min(start + count, 0x110000))
            if not 0xD800 <= cp <= 0xDFFF]


@router.get("/bad")
async def bad(what: str):
    try:
        if what == "int":
            return {"out": unidecode(1)}
        if what == "int_nonascii":
            return {"out": unidecode_expect_nonascii(1)}
        if what == "replace_int":
            return {"out": unidecode("é\U000f0000", errors="replace", replace_str=1)}
    except (AttributeError, TypeError) as e:
        return {"err": type(e).__name__, "msg": str(e)}
    return {}

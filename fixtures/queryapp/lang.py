"""The Python around reporting queries: dependencies built by a factory, string.Formatter().parse(),
float.is_integer(), builtins imported by name, itertools.chain() consumed whole."""
import math
import string
from builtins import list
from itertools import chain  # noqa: A004  (shadows nothing: the builtin itself)

from fastapi import APIRouter, Depends, HTTPException, Query

from .depfactory import header_flag, parse_list

router = APIRouter(prefix="/lang")


@router.get("/depf/ids")
async def depf_ids(ids: list[int] = Depends(parse_list(query=Query([], alias="id"), class_type=int))):
    """repeated keys, one bracketed list, none (the marker's default), an invalid element (500 in both)"""
    return {"ids": ids, "types": [type(i).__name__ for i in ids]}


@router.get("/depf/required")
async def depf_required(names: list[str] = Depends(parse_list(Query(..., alias="name", description="names"), str))):
    """a required marker: 422 without the parameter"""
    return names


@router.get("/depf/flag", dependencies=[Depends(header_flag())])
async def depf_flag(on: bool = Depends(header_flag(default=True))):
    """a factory with defaults, in dependencies=[...] and as a parameter"""
    return {"on": on}


@router.post("/fmt/parse")
async def fmt_parse(body: dict):
    """list(string.Formatter().parse(s)): the items, or CPython's ValueError"""
    try:
        return {"items": list(string.Formatter().parse(body["s"]))}
    except ValueError as e:
        return {"error": str(e)}
    except TypeError as e:
        return {"type_error": str(e)}


@router.post("/fmt/fields")
async def fmt_fields(body: dict):
    """the common use: the field names of a template, validated first"""
    try:
        parts = tuple(string.Formatter().parse(body["s"]))
    except ValueError as exc:
        raise HTTPException(status_code=422, detail=f"not a valid format string: {exc}") from exc
    return sorted({name for _, name, _, _ in parts if name})


@router.post("/isint")
async def isint(body: dict):
    """float.is_integer() / int.is_integer(); a float sent as 53300.0 becomes "53300" """
    out = []
    for v in body["values"]:
        if isinstance(v, float) and v.is_integer():
            out.append(str(int(v)))
        elif isinstance(v, (int, float)):
            out.append([v.is_integer(), str(v)])
        else:
            out.append(v)
    return out


@router.get("/isint/inf")
async def isint_inf():
    return [float("inf").is_integer(), float("nan").is_integer(), (-0.0).is_integer(), True.is_integer(), (2.5).is_integer()]


@router.get("/builtins")
async def builtins_list():
    """`from builtins import list`: list() is the builtin"""
    codes = list({c for c in ("fr", "de", "fr", "*") if c != "*"})
    return sorted(codes) + list("ab") + [len(list(range(3)))]


@router.get("/chain")
async def chain_items():
    """list(chain.from_iterable(...)), sorted(chain(...)), over lists built at run time"""
    groups = [[3, 1], [], [2]]
    return [list(chain.from_iterable(groups)), sorted(chain(*groups, (9, 0))), list(chain())]


@router.get("/chain/bad")
async def chain_bad():
    try:
        return list(chain.from_iterable([[1], 2]))
    except TypeError as e:
        return {"error": str(e)}


@router.get("/float-int")
async def float_int():
    """int(), round(), math.floor/ceil/trunc of NaN and infinities: CPython's ValueError / OverflowError"""
    out = []
    for x in (float("nan"), float("inf"), float("-inf"), 2.5, -3.5, 1e15):
        for k in range(5):
            try:
                if k == 0:
                    out.append(int(x))
                elif k == 1:
                    out.append(round(x))
                elif k == 2:
                    out.append(math.floor(x))
                elif k == 3:
                    out.append(math.ceil(x))
                else:
                    out.append(math.trunc(x))
            except (ValueError, OverflowError) as e:
                out.append(f"{type(e).__name__}: {e}")
    return out

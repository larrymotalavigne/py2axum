"""Libraries translated from their source (`--translate-libs auto`): pure-Python distributions of the environment,
compiled like project code, only the functions the routes reach. No database.

Generated with `python -m py2axum fixtures/libsrcapp --root . --translate-libs auto`.
"""
from typing import Annotated

import humanize
import requests
from fastapi import FastAPI, Query
from requests.utils import quote

from .caching import router as cache_router
from .gens import router as gens_router
from .stdsrc import router as std_router

app = FastAPI()
app.include_router(std_router)
app.include_router(gens_router)
app.include_router(cache_router)


@app.get("/health")
async def health():
    return {"ok": True}


@app.get("/hum/metric/{x}")
async def metric(x: float, unit: str = "", precision: int = 3):
    return {"v": humanize.metric(x, unit, precision)}


@app.get("/hum/scientific/{x}")
async def scientific(x: str, precision: int = 2):
    return {"v": humanize.scientific(float(x) if "." in x or "e" in x or "n" in x else int(x), precision)}


@app.get("/hum/clamp/{x}")
async def clamp(x: float, floor: float | None = None, ceil: float | None = None, fmt: str = "{:}"):
    return {"v": humanize.clamp(x, format=fmt, floor=floor, ceil=ceil)}


@app.get("/hum/list")
async def natural_list(item: Annotated[list[str], Query()] = []):  # noqa: B006
    return {"v": humanize.natural_list(item)}


@app.get("/hum/size/{n}")
async def size(n: int, binary: bool = False, gnu: bool = False, fmt: str = "%.1f"):
    return {"size": humanize.naturalsize(n, binary=binary, gnu=gnu, format=fmt)}


@app.get("/hum/size-s/{s}")
async def size_str(s: str):
    return {"size": humanize.naturalsize(s)}


@app.get("/hum/words/{n}")
async def words(n: int):
    return {"comma": humanize.intcomma(n), "ord": humanize.ordinal(n), "ap": humanize.apnumber(n)}


# requests: the library map has its exceptions only, the rest comes from its source; the source's InvalidURL
# derives from the map's RequestException (caught by the `except` below, as in Python)
@app.get("/rq/requote")
async def rq_requote(u: str):
    return {"v": requests.utils.requote_uri(u), "q": quote(u, safe="")}


@app.get("/rq/unquote")
async def rq_unquote(u: str):
    try:
        return {"v": requests.utils.unquote_unreserved(u)}
    except requests.RequestException as e:
        return {"err": type(e).__name__, "msg": str(e), "base": isinstance(e, requests.exceptions.RequestException),
                "value": isinstance(e, ValueError)}

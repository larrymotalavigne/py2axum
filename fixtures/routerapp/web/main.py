import os

from fastapi import APIRouter, FastAPI

from fixtures.routerapp.extra import aaa, condr, items, jsonr, ra, rb
from fixtures.routerapp.extra.factory import build_router
from fixtures.routerapp.extra.nested import router as nested_router
from fixtures.routerapp.extra.registry import add_more, add_routers

try:
    from fixtures.routerapp.web import optional  # absent: ImportError, the fallback runs
except ImportError:
    optional = None

try:
    from fixtures.routerapp.web.optional import extra_router
    HAS_OPTIONAL = True
except ImportError:
    HAS_OPTIONAL = False

app = FastAPI()
fallback = APIRouter()


@fallback.get("/optional")
async def optional_missing():
    return {"optional": optional is None, "flag": HAS_OPTIONAL}


@app.get("/health")
async def health():
    return {"ok": True}


app.include_router(items.router, prefix="/api")
app.include_router(aaa.router, prefix="/api")
app.include_router(jsonr.router, prefix="/j")
app.include_router(jsonr.plain, prefix="/p")
app.include_router(jsonr.html, prefix="/h")

if optional is not None:
    app.include_router(optional.router, prefix="/api")
if optional is None:
    app.include_router(fallback, prefix="/api")
if not optional:
    app.include_router(fallback, prefix="/api2")
if HAS_OPTIONAL:
    app.include_router(extra_router)

if not HAS_OPTIONAL:
    @app.get("/flag")
    async def flag_off():
        return {"flag": False}
else:
    @app.get("/flag")
    async def flag_on():
        return {"flag": True}


add_routers(app)
add_more(app, version="v2")


@app.get("/reg/b")
async def reg_b_app():
    return {"app": "b"}  # registered after add_routers(app): rb's /reg/b wins


ENABLE_EXTRA = os.getenv("ROUTERAPP_EXTRA", "1") == "1"
if ENABLE_EXTRA:
    app.include_router(rb.router, prefix="/on")
else:
    app.include_router(ra.router, prefix="/off")
if os.getenv("ROUTERAPP_DEBUG"):
    if ENABLE_EXTRA:
        app.include_router(ra.router, prefix="/debug")
app.include_router(condr.api, prefix="/cond")
app.include_router(build_router())
app.include_router(build_router(), prefix="/again")
app.include_router(prefix="/kw", router=nested_router, tags=["kw"])  # Vaultexe form: the router as a keyword

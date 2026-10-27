"""Reference app of the Python language, its standard library and Pydantic (dunders, decorators, keyword
arguments, pickle, collections, Python-version messages, the FastAPI and Pydantic documentation corpora, the
security review's cases, what tests/difftest.py found, real-world shapes, the SQLAlchemy ORM corpus's remaining shapes), apart from fixtures/dynapp: with them,
dynapp's crate no longer compiled within the CI build containers' 6 GiB. Tables and enums are dynapp's
(fixtures/dynapp/models.py)."""
import sys

from fastapi import FastAPI
from fastapi.exception_handlers import http_exception_handler, request_validation_exception_handler
from fastapi.exceptions import RequestValidationError
from starlette.exceptions import HTTPException as StarletteHTTPException

from . import colls, construct, cp, corpusx, decoprops, decos, depinst, docsx, dunders, edges, extras, fa156, jwksx, kwargs, pubapps, modspan, ormx, pk, procx, pydcorpus, pydmore, pydv1, pyver, rds, realworld, sec, silent, stdmore

app = FastAPI()
app.include_router(decos.router)
app.include_router(procx.router)
app.include_router(decoprops.router)
app.include_router(dunders.router)
app.include_router(pk.router)
app.include_router(cp.router)
app.include_router(pydv1.router)
app.include_router(modspan.router)
app.include_router(rds.router)
app.include_router(colls.router)
app.include_router(sec.router)
app.include_router(extras.router)
app.include_router(corpusx.router)
app.include_router(edges.router)
app.include_router(pydmore.router)
app.include_router(pydcorpus.router)
app.include_router(stdmore.router)
app.include_router(kwargs.router)
app.include_router(docsx.router)
app.include_router(realworld.router)
app.include_router(pyver.router)
app.include_router(ormx.router)
app.include_router(construct.router)
app.include_router(silent.router)
app.include_router(fa156.router)
app.include_router(pubapps.router)
app.include_router(jwksx.router)
app.include_router(depinst.router)
# the syntax of newer Pythons: py2axum folds `sys.version_info` against the project's Python (a 3.12 project never
# parses these modules)
if sys.version_info >= (3, 14):
    from . import py314

    app.include_router(py314.router)
if sys.version_info >= (3, 15):
    from . import py315

    app.include_router(py315.router)


@app.exception_handler(StarletteHTTPException)
async def http_exc_default(request, exc):
    """FastAPI's default handlers, called by the application's own (fixtures/langapp/corpusx.py)"""
    return await http_exception_handler(request, exc)


@app.exception_handler(RequestValidationError)
async def validation_default(request, exc):
    return await request_validation_exception_handler(request, exc)


@app.get("/health")
async def health():
    return {"ok": True}

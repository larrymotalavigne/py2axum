"""Reference app of the Pydantic and FastAPI constructions of the documentation corpora (0.9): Iterable fields,
custom APIRoute classes, __get_pydantic_core_schema__, Json, dataclasses. Apart from fixtures/dynapp and
fixtures/langapp, whose crates are at the edge of the CI build containers' memory. No database."""
from fastapi import FastAPI

from . import aliasx, annx, cfgx, cmx, coreschx, dcx, dtx, iterx, jsonx, postx, respx, routex, secx

app = FastAPI()
app.include_router(iterx.router)
app.include_router(aliasx.router)
app.include_router(annx.router)
app.include_router(coreschx.router)
app.include_router(jsonx.router)
app.include_router(dcx.router)
app.include_router(dtx.router)
app.include_router(cfgx.router)
app.include_router(secx.router)
app.include_router(respx.router)
app.include_router(cmx.router)
app.include_router(postx.router)
app.include_router(routex.logging_router)
app.include_router(routex.header_router)
app.include_router(routex.plain_router)
app.include_router(routex.gzip_router)


@app.get("/health")
async def health():
    return {"ok": True}


@app.get("/routex/app/before")
async def before_route_class():
    """declared before `app.router.route_class` is set: a plain APIRoute"""
    return {"route": "plain"}


app.router.route_class = routex.HeaderRoute


@app.get("/routex/app/after")
async def after_route_class():
    return {"route": "custom"}

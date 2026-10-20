"""Reference app of the libraries and the platform around the application (bcrypt, passlib, itsdangerous, Jinja2, PyJWT,
pywebpush, alembic, prometheus_client, logging and python-json-logger, retries, tracing, aio-pika, outbound HTTP,
background loops, ATOM's small apps), apart from fixtures/dynapp: with them, dynapp's crate no longer compiled
within the CI build containers' 6 GiB. Tables and enums are dynapp's (fixtures/dynapp/models.py)."""
from fastapi import FastAPI
from fastapi.exception_handlers import http_exception_handler, request_validation_exception_handler
from fastapi.exceptions import RequestValidationError
from starlette.exceptions import HTTPException as StarletteHTTPException

from extroutes import views as ext_views

from . import addedroutes, amqp, bgloop, httpxerr, libs, logs, markers, outbound, passlibx, prom, pyjwt_auth, retrying, small, tracing

app = FastAPI()
app.include_router(libs.router)
app.include_router(bgloop.router)
app.include_router(amqp.router)
app.include_router(retrying.router)
app.include_router(prom.router)
app.include_router(outbound.router)
app.include_router(tracing.router)
app.include_router(small.router)
app.include_router(pyjwt_auth.router)
app.include_router(logs.router)
app.include_router(markers.router)
app.include_router(passlibx.router)
app.include_router(httpxerr.router)
addedroutes.add_early(app)


@app.exception_handler(StarletteHTTPException)
async def http_exc_default(request, exc):
    """FastAPI's default handlers, called by the application's own"""
    return await http_exception_handler(request, exc)


@app.exception_handler(RequestValidationError)
async def validation_default(request, exc):
    return await request_validation_exception_handler(request, exc)


@app.get("/health")
async def health():
    return {"ok": True}


# routers of a library (fixtures/extlib/extroutes, not translated): included last, they stay on the Python side
# (--python-side mount): a request no translated route fully matches is relayed to the reference server
app.include_router(ext_views.router)
app.include_router(ext_views.extra)
app.include_router(ext_views.make_router("built"), prefix="/made")
# added after the library's routers: relayed to Python, which tries the library's routes first
addedroutes.add_late(app)

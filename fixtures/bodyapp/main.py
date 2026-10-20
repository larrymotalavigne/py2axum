"""Starlette's per-Request-object body cache (`request._body`): a BaseHTTPMiddleware that decompresses a gzip body
and hands it on (`request._body = data`, `request._receive = ...`), one that reads the body to log it, and
exception handlers that look for the cache (the route's Request object, ExceptionMiddleware's own on a 404 or 405;
ServerErrorMiddleware's `Request(scope)`, without a receive channel, in crash.py). No database."""
import gzip
import logging

from fastapi import FastAPI, Form, HTTPException, Request
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from starlette.middleware.base import BaseHTTPMiddleware

log = logging.getLogger("bodyapp")


class Inflate(BaseHTTPMiddleware):
    async def dispatch(self, request: Request, call_next):
        if "gzip" in (request.headers.get("content-encoding") or "").lower():
            raw = await request.body()
            try:
                data = gzip.decompress(raw)
            except OSError:
                return JSONResponse({"detail": "bad gzip"}, status_code=400)
            request._body = data

            async def receive():
                return {"type": "http.request", "body": data, "more_body": False}

            request._receive = receive
        response = await call_next(request)
        response.headers["x-inflate-cached"] = str(hasattr(request, "_body"))
        return response


class Audit(BaseHTTPMiddleware):
    """outermost: reads the body before Inflate (the compressed bytes) when asked to"""

    async def dispatch(self, request: Request, call_next):
        before = hasattr(request, "_body")
        if request.headers.get("x-audit") == "read":
            body = await request.body()
            log.warning("audit %d bytes", len(body))
        response = await call_next(request)
        response.headers["x-audit-cached"] = f"{before}/{hasattr(request, '_body')}"
        return response


app = FastAPI()
app.add_middleware(Inflate)
app.add_middleware(Audit)


async def body_text(request: Request) -> str:
    """the body for an error message, without raising again"""
    try:
        if hasattr(request, "_body"):
            return request._body.decode("utf-8") if request._body else "<empty>"
        try:
            body = await request.body()
            return "read:" + (body.decode("utf-8") if body else "<empty>")
        except RuntimeError as e:
            return f"<runtime: {e}>"
    except Exception as e:
        return f"<error: {type(e).__name__}>"


@app.exception_handler(HTTPException)
async def http_handler(request: Request, exc: HTTPException):
    return JSONResponse({"detail": exc.detail, "body": await body_text(request)}, status_code=exc.status_code)


class Item(BaseModel):
    name: str
    qty: int = 1


@app.get("/health")
async def health():
    return {"ok": True}


@app.post("/items")
async def create(item: Item):
    if item.name == "bad":
        raise HTTPException(status_code=422, detail="bad name")
    return {"name": item.name, "qty": item.qty}


@app.post("/raw")
async def raw(request: Request):
    """the endpoint reads the body itself, then twice (the cache)"""
    first = await request.body()
    again = await request.json() if first else None
    if request.query_params.get("fail"):
        raise HTTPException(status_code=409, detail="after read")
    return {"len": len(first), "json": again, "cached": hasattr(request, "_body")}


@app.post("/form")
async def form(name: str = Form()):
    """form() does not fill the cache"""
    raise HTTPException(status_code=400, detail=f"form {name}")


@app.get("/nobody")
async def nobody():
    raise HTTPException(status_code=403, detail="no body read")


@app.get("/gz/{level}")
async def gz_out(level: int, mtime: int = 0):
    """gzip.compress: zlib's stream, the header as the Python writes it (mtime given: no clock)"""
    import base64

    from pydantic_core import PydanticSerializationError

    try:
        data = gzip.compress(b"hello hello hello " * 20, compresslevel=level, mtime=mtime)
    except Exception as e:
        return {"error": type(e).__name__, "msg": str(e),
                "serialization": isinstance(e, PydanticSerializationError)}
    return {"b64": base64.b64encode(data).decode(), "back": gzip.decompress(data)[:18].decode()}

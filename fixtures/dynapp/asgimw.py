"""Raw ASGI middlewares: classes with `__init__(self, app, ...)` and `async __call__(self, scope, receive, send)`,
registered on the application in main.py (add_middleware and FastAPI(middleware=[...])). Each acts under /mw/
(or on a request header), so every other request of the scenario goes through them unchanged."""
import contextvars
import gzip
import zlib

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import JSONResponse, PlainTextResponse, StreamingResponse
from pydantic import BaseModel

router = APIRouter(prefix="/mw")

REQUEST_ID: contextvars.ContextVar[str] = contextvars.ContextVar("request_id", default="none")


def header(scope, name: bytes) -> bytes | None:
    for k, v in scope["headers"]:
        if k == name:
            return v
    return None


class HeaderStamp:
    """appends its name to `x-mw-order` on the responses under /mw/ (shows the stack order)"""

    def __init__(self, app, name: str):
        self.app = app
        self.name = name

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http" or not scope["path"].startswith("/mw/"):
            await self.app(scope, receive, send)
            return

        async def send_wrapper(message):
            if message["type"] == "http.response.start":
                headers = list(message.get("headers", []))
                headers.append((b"x-mw-order", self.name.encode()))
                message = {**message, "headers": headers}
            await send(message)

        await self.app(scope, receive, send_wrapper)


class HeaderStrip:
    """drops from the response the headers the request names in `x-strip`, and content-length under /mw/nolength"""

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        drop = set()
        names = header(scope, b"x-strip")
        if names:
            drop.update(n.strip().lower().encode() for n in names.decode().split(","))
        if scope["path"].startswith("/mw/nolength"):
            drop.add(b"content-length")
        if not drop:
            return await self.app(scope, receive, send)

        async def send_wrapper(message):
            if message["type"] == "http.response.start":
                message["headers"] = [(k, v) for (k, v) in message["headers"] if k.lower() not in drop]
            await send(message)

        await self.app(scope, receive, send_wrapper)


class InFlight:
    """counts the requests being served in `state.in_flight` (the application's state, given at registration)"""

    def __init__(self, app, state):
        self.app = app
        self.state = state

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        self.state.in_flight = getattr(self.state, "in_flight", 0) + 1
        try:
            await self.app(scope, receive, send)
        finally:
            self.state.in_flight -= 1


class RequestContext:
    """the request id (`x-rid`, else "generated") in a ContextVar and in `request.state`; the response
    echoes the ContextVar as it is when the response starts (the endpoint may have changed it)"""

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http" or not scope["path"].startswith("/mw/ctx"):
            await self.app(scope, receive, send)
            return
        rid = header(scope, b"x-rid")
        rid = rid.decode() if rid else "generated"
        token = REQUEST_ID.set(rid)
        scope.setdefault("state", {})["rid"] = rid

        async def send_wrapper(message):
            if message["type"] == "http.response.start":
                message.setdefault("headers", []).append((b"x-rid-echo", REQUEST_ID.get().encode()))
            await send(message)

        try:
            await self.app(scope, receive, send_wrapper)
        finally:
            REQUEST_ID.reset(token)


class ShortCircuit:
    """answers itself when the request says `x-block`: by send() messages, with a response object, or streamed"""

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        mode = header(scope, b"x-block") if scope["type"] == "http" else None
        if mode == b"send":
            await send({"type": "http.response.start", "status": 403,
                        "headers": [(b"content-type", b"application/json"), (b"x-blocked", b"send")]})
            await send({"type": "http.response.body", "body": b'{"blocked": "send"}'})
        elif mode == b"response":
            await JSONResponse({"blocked": "response"}, status_code=429, headers={"x-blocked": "response"})(scope, receive, send)
        elif mode == b"text":
            await PlainTextResponse("blocked as text", status_code=401)(scope, receive, send)
        elif mode == b"stream":
            await send({"type": "http.response.start", "status": 200, "headers": [(b"content-type", b"text/plain; charset=utf-8")]})
            for part in (b"one ", b"two ", b"three"):
                await send({"type": "http.response.body", "body": part, "more_body": True})
            await send({"type": "http.response.body", "body": b"", "more_body": False})
        elif mode == b"empty":
            await send({"type": "http.response.start", "status": 204, "headers": []})
            await send({"type": "http.response.body"})
        else:
            await self.app(scope, receive, send)


class BodyReader:
    """under /mw/body: reads the body through receive(), hands the app a receive() that replays it (upper-cased
    with `x-upper`), and reports the size read in `x-body-size`"""

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http" or not scope["path"].startswith("/mw/body"):
            await self.app(scope, receive, send)
            return
        chunks = []
        while True:
            message = await receive()
            chunks.append(message.get("body", b""))
            if not message.get("more_body", False):
                break
        body = b"".join(chunks)
        if header(scope, b"x-upper"):
            body = body.upper()
        replayed = {"done": False}

        async def replay():
            if replayed["done"]:
                return {"type": "http.disconnect"}
            replayed["done"] = True
            return {"type": "http.request", "body": body, "more_body": False}

        async def send_wrapper(message):
            if message["type"] == "http.response.start":
                message["headers"] = [*message["headers"], (b"x-body-size", str(len(body)).encode())]
            await send(message)

        await self.app(scope, replay, send_wrapper)


class Rewrite:
    """serves /mw/old/... as /mw/new/... and adds an `x-injected` request header (a new scope dict)"""

    def __init__(self, app, injected: str = "rewrite"):
        self.app = app
        self.injected = injected

    async def __call__(self, scope, receive, send):
        if scope["type"] == "http" and scope["path"].startswith("/mw/old/"):
            path = "/mw/new/" + scope["path"][len("/mw/old/"):]
            scope = {**scope, "path": path, "raw_path": path.encode(),
                     "headers": [*scope["headers"], (b"x-injected", self.injected.encode())]}
        elif scope["type"] == "http" and scope["path"] == "/mw/method":
            scope = dict(scope)
            scope["method"] = "PUT"
            scope["query_string"] = b"via=rewrite"
        await self.app(scope, receive, send)


class ResponseUpper:
    """upper-cases the response body (fixed or streamed) when the request says `x-upper-out`"""

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http" or header(scope, b"x-upper-out") is None:
            await self.app(scope, receive, send)
            return
        seen = {"parts": 0}

        async def send_wrapper(message):
            if message["type"] == "http.response.body":
                seen["parts"] += 1
                message = {**message, "body": message.get("body", b"").upper()}
            elif message["type"] == "http.response.start":
                message = {**message, "headers": [*message["headers"], (b"x-upper-out", b"1")]}
            await send(message)

        await self.app(scope, receive, send_wrapper)


class Boom:
    """`x-boom: before` raises before the app, `after` once the response is sent, `catch` turns the endpoint's
    ValueError into a 418 of its own"""

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        mode = header(scope, b"x-boom") if scope["type"] == "http" else None
        if mode == b"before":
            raise RuntimeError("boom before the application")
        if mode == b"catch":
            try:
                await self.app(scope, receive, send)
            except ValueError as e:
                await send({"type": "http.response.start", "status": 418,
                            "headers": [(b"content-type", b"text/plain; charset=utf-8"), (b"x-caught", type(e).__name__.encode())]})
                await send({"type": "http.response.body", "body": f"caught: {e}".encode()})
            return
        await self.app(scope, receive, send)
        if mode == b"after":
            raise RuntimeError("boom after the response")


# ---- endpoints


class Item(BaseModel):
    title: str
    qty: int = 1


@router.get("/plain")
async def plain():
    return {"ok": True}


@router.get("/inflight")
async def inflight(request: Request):
    return {"in_flight": request.app.state.in_flight}


@router.get("/ctx")
async def ctx(request: Request):
    return {"var": REQUEST_ID.get(), "state": request.state.rid}


@router.get("/ctx/change")
async def ctx_change():
    REQUEST_ID.set(REQUEST_ID.get() + "+endpoint")
    return {"var": REQUEST_ID.get()}


@router.get("/nolength")
async def nolength():
    return {"framing": "chunked once the middleware drops content-length"}


@router.get("/headers")
async def headers_out():
    return JSONResponse({"ok": 1}, headers={"x-internal": "secret", "x-public": "shown"})


@router.post("/body")
async def body_raw(request: Request):
    raw = await request.body()
    return {"raw": raw.decode(), "len": len(raw)}


@router.post("/body/item")
async def body_item(item: Item):
    return item


@router.get("/new/{name}")
async def new(name: str, request: Request):
    return {"name": name, "injected": request.headers.get("x-injected"), "path": request.url.path}


@router.put("/method")
async def method(request: Request):
    return {"method": request.method, "via": request.query_params.get("via")}


@router.get("/stream")
async def stream():
    async def parts():
        for p in ("alpha ", "beta ", "gamma"):
            yield p
    return StreamingResponse(parts(), media_type="text/plain")


@router.get("/raise")
async def raise_value():
    raise ValueError("bad value from the endpoint")


@router.get("/raise/runtime")
async def raise_runtime():
    raise RuntimeError("unhandled in the endpoint")


@router.get("/missing")
async def missing():
    raise HTTPException(404, "nothing here", headers={"x-reason": "missing"})


@router.post("/gunzip")
async def gunzip(request: Request):
    raw = await request.body()
    try:
        return {"text": gzip.decompress(raw).decode()}
    except zlib.error as e:
        return {"error": "zlib.error", "msg": str(e)}
    except gzip.BadGzipFile as e:
        return {"error": "BadGzipFile", "msg": str(e), "os": isinstance(e, OSError)}
    except EOFError as e:
        return {"error": "EOFError", "msg": str(e)}

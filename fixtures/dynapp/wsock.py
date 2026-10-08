"""WebSocket routes (`@app.websocket`, `@router.websocket`): the handshake (accept, refusal, denial
response, failure), messages in both directions, closing by either side, Starlette's states and errors,
parameters and dependencies, `async for` over `iter_*`. What the server saw goes to LOG (GET /ws/log)."""
from typing import Annotated

from fastapi import APIRouter, Depends, Header, HTTPException, Query, Request, WebSocket, WebSocketDisconnect, WebSocketException, status
from fastapi.responses import PlainTextResponse
from fastapi.websockets import WebSocketState
from sqlalchemy import text
from starlette.requests import HTTPConnection

from .db import DbDep

router = APIRouter(prefix="/ws")
LOG: list = []


class WsBoom(Exception):
    """handled by an application exception handler (main.py), called with the WebSocket"""


@router.get("/log")
async def log():
    out = list(LOG)
    LOG.clear()
    return out


def states(ws: WebSocket) -> list:
    return [ws.client_state.name, ws.application_state.name]


@router.websocket("/echo")
async def echo(websocket: WebSocket):
    await websocket.accept()
    try:
        while True:
            t = await websocket.receive_text()
            await websocket.send_text("echo:" + t)
    except WebSocketDisconnect as e:
        LOG.append(["echo", e.code, e.reason, states(websocket), websocket.client_state == WebSocketState.DISCONNECTED])


@router.websocket("/json")
async def json_(websocket: WebSocket):
    await websocket.accept()
    data = await websocket.receive_json()
    await websocket.send_json({"got": data, "é": [1, 2.5, None, True], "s": "ü\n\"q\""})
    data = await websocket.receive_json(mode="binary")
    await websocket.send_json({"binary": data}, mode="binary")
    await websocket.close(code=4100, reason="bye")
    LOG.append(["json", states(websocket)])


@router.websocket("/bytes")
async def bytes_(websocket: WebSocket):
    await websocket.accept()
    b = await websocket.receive_bytes()
    await websocket.send_bytes(b[::-1])
    m = await websocket.receive()
    LOG.append(["raw", sorted(m), m["type"], m.get("text"), m.get("bytes")])
    await websocket.send({"type": "websocket.send", "text": "raw:" + str(m.get("text"))})
    await websocket.send({"type": "websocket.send", "bytes": b"\x00\xff"})
    await websocket.close()


@router.websocket("/refuse")
async def refuse(websocket: WebSocket):
    LOG.append(["refuse", states(websocket)])
    await websocket.close(code=4001, reason="nope")
    LOG.append(["refused", states(websocket)])


@router.websocket("/boom-before")
async def boom_before(websocket: WebSocket):
    raise ValueError("before")


@router.websocket("/boom-after")
async def boom_after(websocket: WebSocket):
    await websocket.accept()
    await websocket.send_text("hi")
    raise ValueError("after")


@router.websocket("/noaccept")
async def noaccept(websocket: WebSocket):
    return None


@router.websocket("/noclose")
async def noclose(websocket: WebSocket):
    await websocket.accept()
    await websocket.send_text("a")


@router.websocket("/http-exc-before")
async def http_exc_before(websocket: WebSocket):
    raise HTTPException(status_code=418, detail={"why": "teapot"}, headers={"x-reason": "pot"})


@router.websocket("/http-exc-after")
async def http_exc_after(websocket: WebSocket):
    await websocket.accept()
    raise HTTPException(status_code=418, detail="teapot")


@router.websocket("/wsexc-before")
async def wsexc_before(websocket: WebSocket):
    raise WebSocketException(code=status.WS_1008_POLICY_VIOLATION, reason="denied")


@router.websocket("/wsexc-after")
async def wsexc_after(websocket: WebSocket):
    await websocket.accept()
    raise WebSocketException(4003, "denied after")


@router.websocket("/handled")
async def handled(websocket: WebSocket, after: bool = False):
    if after:
        await websocket.accept()
    raise WsBoom("custom")


@router.websocket("/denial")
async def denial(websocket: WebSocket):
    await websocket.send_denial_response(PlainTextResponse("go away", status_code=401, headers={"x-why": "no"}))
    LOG.append(["denial", states(websocket)])


@router.websocket("/items/{item_id}")
async def items(websocket: WebSocket, item_id: int, n: Annotated[int, Query(ge=1)], tag: str | None = None,
                x_token: Annotated[str | None, Header()] = None):
    await websocket.accept()
    await websocket.send_json({"item": item_id, "n": n, "tag": tag, "token": x_token})
    await websocket.close()


async def teardown_dep():
    LOG.append(["dep enter"])
    try:
        yield "dep-value"
    finally:
        LOG.append(["dep exit"])


async def ws_user(websocket: WebSocket, token: str = Query()) -> str:
    if token != "ok":
        raise WebSocketException(code=status.WS_1008_POLICY_VIOLATION, reason="bad token")
    return "user:" + websocket.url.path


async def conn_kind(conn: HTTPConnection) -> str:
    return type(conn).__name__


@router.websocket("/deps", dependencies=[Depends(teardown_dep)])
async def deps(websocket: WebSocket, db: DbDep, user: Annotated[str, Depends(ws_user)],
               kind: Annotated[str, Depends(conn_kind)], value: Annotated[str, Depends(teardown_dep)]):
    answer = (await db.execute(text("SELECT 41 + 1"))).scalar_one()
    await websocket.accept()
    LOG.append(["deps", user, kind, value, answer])
    await websocket.send_json([user, kind, value, answer])
    await websocket.close()


@router.websocket("/conn/{name}")
async def conn(websocket: WebSocket, name: str):
    await websocket.accept(subprotocol="chat", headers=[(b"x-extra", b"v1"), (b"x-other", b"v2")])
    await websocket.send_json({
        "path_params": websocket.path_params, "query": dict(websocket.query_params), "cookies": websocket.cookies,
        "header": websocket.headers.get("x-token"), "host": websocket.client.host, "path": websocket.url.path,
        "scope": [websocket.scope["type"], websocket.scope["path"], websocket.scope["subprotocols"]],
        "states": states(websocket),
    })
    await websocket.close(1000, "done")


@router.websocket("/iter")
async def iter_(websocket: WebSocket):
    await websocket.accept()
    n = 0
    async for t in websocket.iter_text():
        n += 1
        await websocket.send_text(f"{n}:{t}")
    LOG.append(["iter done", n, states(websocket)])


@router.websocket("/iter-json")
async def iter_json(websocket: WebSocket):
    await websocket.accept()
    total = 0
    async for item in websocket.iter_json():
        total += item["v"]
        await websocket.send_json({"total": total})
    LOG.append(["iter-json done", total])


@router.websocket("/iter-bytes")
async def iter_bytes(websocket: WebSocket):
    await websocket.accept()
    sizes = []
    async for b in websocket.iter_bytes():
        sizes.append(len(b))
    LOG.append(["iter-bytes done", sizes])


@router.websocket("/late")
async def late(websocket: WebSocket):
    """sends after the client has closed: WebSocketDisconnect(1006), then WebSocketDisconnected"""
    await websocket.accept()
    await websocket.send_text("ready")
    try:
        await websocket.receive_text()
    except WebSocketDisconnect as e:
        LOG.append(["late closed", e.code, e.reason, states(websocket)])
    try:
        await websocket.send_text("too late")
    except WebSocketDisconnect as e:
        LOG.append(["late send", type(e).__name__, e.code, e.reason, states(websocket)])
    for op in ("send", "receive"):
        try:
            if op == "send":
                await websocket.send_text("again")
            else:
                await websocket.receive_text()
        except RuntimeError as e:  # WebSocketDisconnected on Starlette 1.7 (a RuntimeError; not imported: absent before)
            LOG.append(["late " + op, type(e).__name__, str(e), isinstance(e, RuntimeError)])


@router.websocket("/misuse")
async def misuse(websocket: WebSocket):
    out = []
    try:
        await websocket.receive_text()
    except RuntimeError as e:
        out.append([type(e).__name__, str(e)])
    try:
        await websocket.send({"type": "websocket.send", "text": "x"})
    except RuntimeError as e:
        out.append([type(e).__name__, str(e)])
    await websocket.accept()
    try:
        await websocket.accept()
    except RuntimeError as e:
        out.append([type(e).__name__, str(e)])
    try:
        await websocket.receive_json(mode="xml")
    except RuntimeError as e:
        out.append([type(e).__name__, str(e)])
    try:
        raise WebSocketDisconnect(code=4444)
    except WebSocketDisconnect as e:
        out.append([e.code, e.reason, list(e.args)])
    try:
        raise WebSocketDisconnect(4445, "why")
    except Exception as e:
        out.append([e.code, e.reason, list(e.args), isinstance(e, WebSocketDisconnect)])
    await websocket.send_json(out)
    await websocket.close()
    try:
        await websocket.close()
    except RuntimeError as e:
        LOG.append(["misuse close twice", type(e).__name__, str(e)])


@router.websocket("/{name}/tail")
async def tail(websocket: WebSocket, name: str):
    await websocket.accept()
    await websocket.send_text("tail:" + name)
    await websocket.close()


@router.get("/annotated-default")
async def annotated_default(request: Request, n: Annotated[int, Query(ge=1)] = 5, x_token: Annotated[str | None, Header()] = None):
    """`Annotated[T, Query()] = default`: optional, as in FastAPI (HTTP routes too)"""
    return {"n": n, "token": x_token, "path": request.url.path}


@router.get("/urlpath/{name}")
async def urlpath(request: Request, name: str):
    return [name, request.url.path, request.url.query]


class Countdown:
    def __init__(self, n):
        self.n = n

    def __aiter__(self):
        return self

    async def __anext__(self):
        if self.n == 0:
            raise StopAsyncIteration
        self.n -= 1
        return self.n


async def tens(n):
    for i in range(n):
        yield i * 10


@router.get("/async-for")
async def async_for():
    """`async for` outside WebSockets: a project class, an async generator, break / else"""
    out = []
    async for x in Countdown(3):
        out.append(x)
    async for y in tens(4):
        if y == 20:
            break
        out.append(y)
    else:
        out.append("no else after break")
    async for a, b in pairs():
        out.append(a + b)
    else:
        out.append("else")
    out.append([z async for z in Countdown(2)])
    return out


async def pairs():
    yield 1, 2
    yield 3, 4


@router.get("/http-only")
async def http_only():
    return {"http": True}

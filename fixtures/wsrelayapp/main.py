import http_ece
from contextlib import asynccontextmanager

from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.responses import PlainTextResponse


@asynccontextmanager
async def lifespan(app):
    # does not translate (http_ece is not in the library map): --python-side auto runs it in the Python process
    # only, and `/state`, which reads what it sets, goes there too (relayed by the binary)
    app.state.motd = "hello " + http_ece.iv(b"\0" * 12, 1).hex()
    yield


app = FastAPI(lifespan=lifespan)


@app.get("/health")
async def health():
    return {"ok": True}


@app.websocket("/native")
async def native(websocket: WebSocket):
    await websocket.accept()
    msg = await websocket.receive_text()
    await websocket.send_text(f"native {msg}")
    await websocket.close()


@app.websocket("/state")
async def state(websocket: WebSocket):
    await websocket.accept()
    await websocket.send_text(websocket.app.state.motd)
    await websocket.close()


@app.websocket("/r/{kind}")
async def relayed(websocket: WebSocket, kind: str):
    """Served by the Python process, the binary relaying it: one behaviour per `kind`."""
    if kind == "refuse":
        await websocket.close()  # before accepting: 403
        return
    if kind == "deny":
        await websocket.send_denial_response(PlainTextResponse("go away", status_code=401, headers={"x-why": "no"}))
        return
    if kind == "proto":
        offered = websocket.scope.get("subprotocols", [])
        await websocket.accept(subprotocol=offered[-1] if offered else None, headers=[(b"x-relay", b"yes")])
        await websocket.send_text(",".join(offered) + " " + websocket.headers.get("x-token", "-"))
        await websocket.close(code=4001, reason="done")
        return
    await websocket.accept()
    if kind == "echo":
        try:
            while True:
                m = await websocket.receive()
                if m["type"] == "websocket.disconnect":
                    return
                if m.get("text") is not None:
                    await websocket.send_text("echo " + m["text"])
                else:
                    await websocket.send_bytes(b"echo " + m["bytes"])
        except WebSocketDisconnect:
            return
    if kind == "query":
        await websocket.send_json({"q": dict(websocket.query_params), "path": websocket.url.path})
        await websocket.close(code=1000)
        return
    if kind == "drop":
        await websocket.send_text("bye")
        return  # without close: the connection drops (1006)
    await websocket.close(code=4404, reason=f"unknown {kind}")

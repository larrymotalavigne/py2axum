from fastapi import APIRouter, WebSocket, WebSocketDisconnect

router = APIRouter(prefix="/ws", tags=["websockets"])


@router.websocket("/echo")
async def echo(websocket: WebSocket):
    await websocket.accept()
    try:
        while True:
            text = await websocket.receive_text()
            await websocket.send_text(f"Message text was: {text}")
    except WebSocketDisconnect:
        pass


@router.websocket("/rooms/{room}")
async def room(websocket: WebSocket, room: str, token: str | None = None):
    if token != "letmein":
        await websocket.close(code=1008)  # before accept(): the client gets HTTP 403
        return
    await websocket.accept()
    async for message in websocket.iter_json():
        if message.get("type") == "bye":
            await websocket.close(code=1000, reason="see you")
            return
        await websocket.send_json({"room": room, "echo": message})

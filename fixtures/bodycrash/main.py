"""The `Exception` handler of an app like fixtures/bodyapp (one BaseHTTPMiddleware): ServerErrorMiddleware hands it a
`Request(scope)` without a receive channel, whose body() raises RuntimeError."""
from fastapi import FastAPI, Request
from fastapi.responses import PlainTextResponse

from fixtures.bodyapp.main import Inflate, Item, body_text

app = FastAPI()
app.add_middleware(Inflate)


@app.exception_handler(Exception)
async def crash_handler(request: Request, exc: Exception):
    return PlainTextResponse(f"crash: {exc}; body={await body_text(request)}", status_code=500)


@app.get("/health")
async def health():
    return {"ok": True}


@app.post("/boom")
async def boom(item: Item):
    raise ValueError(f"boom {item.name}")


@app.get("/boom")
async def boom_get():
    raise KeyError("no body")

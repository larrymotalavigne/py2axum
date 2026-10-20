"""pgpy's PGPKey.from_blob(): see __init__.py."""
from fastapi import FastAPI

from .keys import router as keys_router
from .static import router as static_router

app = FastAPI()
app.include_router(keys_router)
app.include_router(static_router)


# two BaseHTTPMiddleware layers, as in real applications: an exception escaping a route (pgpy's AttributeError on a
# user id holding a newline) reaches uvicorn with the anyio ExceptionGroup of the inner layer as its cause
@app.middleware("http")
async def inner_layer(request, call_next):
    return await call_next(request)


@app.middleware("http")
async def outer_layer(request, call_next):
    response = await call_next(request)
    response.headers["x-layers"] = "2"
    return response


@app.get("/health")
async def health():
    return {"ok": True}

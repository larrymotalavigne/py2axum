"""The application of the documentation's examples: each page of the tutorial has its router in tutorial/.

    uvicorn docs_src.main:app                      # the Python application, from the repository's root
    py2axum docs_src --root . -o build/docs_axum   # the same application, compiled
"""
from contextlib import asynccontextmanager

from fastapi import FastAPI, Request
from fastapi.middleware.cors import CORSMiddleware
from starlette.middleware.base import BaseHTTPMiddleware

from .db import Base, engine
from .tutorial import (background, body, dependencies, errors, files, models, parameters, responses, security, sql,
                       websockets)


# --8<-- [start:lifespan]
@asynccontextmanager
async def lifespan(app: FastAPI):
    # before the server listens: create the tables that do not exist yet
    async with engine.begin() as conn:
        await conn.run_sync(Base.metadata.create_all)
    yield
    # after the server has stopped (SIGTERM, Ctrl+C)
    print("shutting down")


app = FastAPI(title="py2axum documentation examples", lifespan=lifespan)
# --8<-- [end:lifespan]


# --8<-- [start:middleware]
@app.middleware("http")
async def add_served_by(request: Request, call_next):
    response = await call_next(request)
    response.headers["X-Served-By"] = "docs-example"
    return response


class RequestIdMiddleware(BaseHTTPMiddleware):
    """Echo the caller's request id, or say there was none."""

    async def dispatch(self, request: Request, call_next):
        response = await call_next(request)
        response.headers["X-Request-Id"] = request.headers.get("x-request-id", "none")
        return response


app.add_middleware(RequestIdMiddleware)
# --8<-- [end:middleware]

# --8<-- [start:cors]
app.add_middleware(
    CORSMiddleware,
    allow_origins=["https://app.example.org"],
    allow_credentials=True,
    allow_methods=["GET", "POST"],
    allow_headers=["authorization", "content-type"],
)
# --8<-- [end:cors]

# --8<-- [start:errors]
app.add_exception_handler(errors.UnicornException, errors.unicorn_handler)
# --8<-- [end:errors]

# --8<-- [start:routers]
app.include_router(parameters.router)
app.include_router(body.router)
app.include_router(models.router)
app.include_router(responses.router)
app.include_router(errors.router)
app.include_router(dependencies.router)
app.include_router(security.router)
app.include_router(sql.router)
app.include_router(files.router)
app.include_router(background.router)
app.include_router(websockets.router)
# --8<-- [end:routers]


@app.get("/health")
async def health() -> dict:
    return {"status": "ok"}

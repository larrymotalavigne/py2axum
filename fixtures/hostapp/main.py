"""Starlette's TrustedHostMiddleware (fixtures/hostapp, tests/scenarios/hostapp.py)."""
from fastapi import FastAPI
from fastapi.middleware.trustedhost import TrustedHostMiddleware

app = FastAPI()
app.add_middleware(TrustedHostMiddleware, allowed_hosts=["127.0.0.1", "*.example.com", "www.example.org", "[::1]"])


@app.get("/")
async def main():
    return {"message": "Hello World"}


@app.get("/items/{name}")
async def item(name: str, q: str | None = None):
    return {"name": name, "q": q}

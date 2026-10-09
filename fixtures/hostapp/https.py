"""Starlette's HTTPSRedirectMiddleware (fixtures/hostapp, tests/scenarios/hostapp.py)."""
from fastapi import FastAPI
from fastapi.middleware.httpsredirect import HTTPSRedirectMiddleware

app = FastAPI()
app.add_middleware(HTTPSRedirectMiddleware)


@app.get("/")
async def main():
    return {"message": "Hello World"}

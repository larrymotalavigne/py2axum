"""An application whose migrations the binary applies (`<binary> migrate`, tests/migrate_check.py)."""
from fastapi import FastAPI

app = FastAPI()


@app.get("/")
async def root():
    return {}

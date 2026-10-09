# Compiled once while the image is built, so that the runtime's dependencies are already in the image's target
# directory: an application's build then compiles only its own crate (and links).
from fastapi import FastAPI

app = FastAPI()


@app.get("/")
async def root():
    return {"ok": True}

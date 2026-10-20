"""Documents assembled from files on disk, the paths resolved against `Path.cwd()` the way an application
resolves the relative paths it stored. No database."""
import os
from pathlib import Path

from fastapi import FastAPI

app = FastAPI()


@app.get("/health")
async def health():
    return {"ok": True}


@app.get("/cwd")
async def cwd():
    here = Path.cwd()
    rel = Path("fixtures/pdfmergeapp/main.py")
    if not rel.is_absolute():
        rel = Path.cwd() / rel
    return {"cwd": str(here), "same": str(here) == os.getcwd(), "abs": here.is_absolute(), "name": here.name,
            "type": type(os.getcwd()).__name__, "joined": str(rel), "exists": rel.exists(),
            "parent": str(Path.cwd().parent), "posix": str(Path.cwd() / "x") == os.path.join(os.getcwd(), "x")}

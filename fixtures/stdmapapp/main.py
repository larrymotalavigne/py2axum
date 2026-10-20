"""Standard-library names the libraries translated from their source (`--translate-libs`) reach: errno and sys
constants, collections.abc as isinstance targets, warnings, chained comparisons of sys.version_info, itertools.chain
under str.join, importlib.import_module of an installed module, array.array. No database.

Generated with `python -m py2axum fixtures/stdmapapp --root . --translate-libs auto`."""
from fastapi import FastAPI

from .stdmore import router as more
from .stdx import router

app = FastAPI()
app.include_router(router)
app.include_router(more)


@app.get("/health")
async def health():
    return {"ok": True}

"""Protocol methods, NotImplemented, dead code, class-scope defaults, generic bases, module attributes rebound,
lazy module attributes: the constructions libraries use (marche A, lot A3). No database."""
from fastapi import FastAPI

from . import conf
from .klass import router as klass_router
from .proto import router

conf.api_key = "sk_module_level"
app = FastAPI()
app.include_router(router)
app.include_router(klass_router)


@app.get("/health")
async def health():
    return {"ok": True}

"""Reference app of the shapes common in reporting APIs, apart from the other fixtures (each crate must compile
within the CI build containers' 6 GiB): window functions, CTEs, correlated subqueries, `case(*whens)`, typed casts
and bound parameters (fixtures/queryapp/sqlw.py), and the Python around them (fixtures/queryapp/lang.py). Tables are
dynapp's (fixtures/dynapp/models.py); every route rolls back what it wrote."""
from fastapi import FastAPI

from . import lang, ntuple, sqlw

app = FastAPI()
app.include_router(sqlw.router)
app.include_router(lang.router)
app.include_router(ntuple.router)


@app.get("/health")
async def health():
    return {"ok": True}

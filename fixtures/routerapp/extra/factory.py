from fastapi import APIRouter

from fixtures.routerapp.extra import ra, rb


def build_router() -> APIRouter:
    """gymhero form: a router built from routers by a function, included by calling it."""
    api = APIRouter(prefix="/f")
    api.include_router(ra.router)
    api.include_router(rb.router, prefix="/rb", tags=["rb"])
    return api

import os

from fastapi import FastAPI

from fixtures.routerapp.extra import ra, rb

ROUTERS = (ra.router, rb.router)
PREFIX = os.environ.get("ROUTERAPP_PREFIX", "/env")


def add_routers(app: FastAPI) -> None:
    """Douyin/Vaultexe form: the routers registered by a function given the app, through a loop."""
    for router in ROUTERS:
        app.include_router(router, prefix="/reg")
    app.include_router(rb.router, prefix="/reg2", tags=["again"])


def add_more(app: FastAPI, prefix: str = "/more", version="v1") -> None:
    """fast-soy-admin / inteliver form: prefixes from the parameters (as called) and from module globals."""
    app.include_router(rb.router, prefix=f"{prefix}/{version}")
    app.include_router(ra.router, prefix=PREFIX + "/x")

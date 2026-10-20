import os

from fastapi import APIRouter

from fixtures.routerapp.extra import rb

# full-stack-fastapi-template form: a router included in another one under a setting read at import
api = APIRouter()
if os.getenv("ROUTERAPP_ENV", "local") == "local":
    api.include_router(rb.router, prefix="/local")
if os.getenv("ROUTERAPP_ENV", "local") != "local":
    api.include_router(rb.router, prefix="/prod")

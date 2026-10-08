"""Routers mounted under prefixes read from the settings at startup (overridable by the environment)."""
from typing import Annotated

from fastapi import APIRouter, Depends, Header, HTTPException, Request
from starlette.routing import Match
from pydantic_settings import BaseSettings, SettingsConfigDict


class ApiSettings(BaseSettings):
    model_config = SettingsConfigDict(env_prefix="DYNAPP_")

    API_PREFIX: str = "/api/v1"
    ADMIN_PREFIX: str = "/admin"


settings = ApiSettings()


async def require_user(x_user: str | None = Header(default=None)) -> str:
    if not x_user:
        raise HTTPException(status_code=401, detail="no user")
    return x_user


router = APIRouter(prefix="/things")
admin = APIRouter()


@router.get("/whoami")
async def whoami(user: str = Depends(require_user)):
    return {"user": user, "prefix": settings.API_PREFIX}


@router.get("/whois")
async def whois(remote: Annotated[str | None, Header(alias="Remote-User")] = None):
    """Annotated metadata with the default after `=`."""
    return {"remote": remote}


@router.get("/probe")
async def probe(request: Request, path: str):
    """The application's route tree matched at run time: the include prefixes are the runtime values."""
    scope = {"type": "http", "method": "GET", "path": path, "root_path": ""}
    out = []
    for r in request.app.router.routes:
        found, _ = r.matches(scope)
        if found != Match.NONE:
            out.append([type(r).__name__, found.name])
    return out


@router.get("/{name}")
async def thing(name: str):
    return {"thing": name}


@admin.get("/ping")
async def ping():
    return {"pong": settings.ADMIN_PREFIX}


@admin.get("/{name}")
async def admin_name(name: str):
    return {"name": name}


router.include_router(admin, prefix=settings.ADMIN_PREFIX)

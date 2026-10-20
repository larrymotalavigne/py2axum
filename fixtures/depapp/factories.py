"""Dependency factories with `*roles`, chained factories, lambdas."""
from typing import Annotated

from fastapi import APIRouter, Depends, Header, HTTPException

from .state import EVENTS

router = APIRouter(prefix="/f")

MANAGER = {"name": "manager"}


async def current_role(x_role: Annotated[str | None, Header()] = None) -> str:
    EVENTS.append(f"role {x_role}")
    if x_role is None:
        raise HTTPException(401, "Not authenticated", headers={"WWW-Authenticate": "Bearer"})
    return x_role


def require_role(*roles: str):
    """Allowed roles."""

    async def _check(role: str = Depends(current_role)) -> str:
        if role not in roles:
            raise HTTPException(403, f"Role {role!r} insufficient. Required: {list(roles)} ({', '.join(roles)})")
        return role

    return _check


def require_admin():
    return require_role("admin")


def require_staff(*extra: str):
    return require_role("admin", "operator", *extra)


Admin = Annotated[str, Depends(require_role("admin"))]


@router.get("/one")
async def one(role: str = Depends(require_role("admin", "operator"))):
    return {"role": role}


@router.get("/none")
async def none(role: str = Depends(require_role())):
    return {"role": role}


@router.get("/chained")
async def chained(role: str = Depends(require_admin())):
    return {"role": role}


@router.get("/chained/star")
async def chained_star(role: str = Depends(require_staff("guest"))):
    return {"role": role}


@router.get("/alias")
async def alias(a: Admin, b: Admin):
    return {"a": a, "b": b}


@router.get("/two", dependencies=[Depends(require_role("admin", "guest"))])
async def two(role: str = Depends(require_role("guest"))):
    return {"role": role}


@router.get("/lambda")
async def lam(m: dict = Depends(lambda: MANAGER)):
    return m


@router.get("/lambda/q")
async def lam_q(v=Depends(lambda q="d": q)):
    return {"v": v}

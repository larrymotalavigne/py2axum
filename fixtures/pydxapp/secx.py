"""Security constructions of the FastAPI documentation: a security scheme subclassed to change its error
(`make_not_authenticated_error`) or its challenge (`make_authenticate_headers`); SecurityScopes with
`Security(dep, scopes=[...])` (the scopes of the chain, a cache entry per set of scopes); AfterValidator on path,
query and header parameters; `yield from` an open file in a streamed generator."""
import os
from collections.abc import Callable
from dataclasses import dataclass
from typing import Annotated, Any

from fastapi import APIRouter, Depends, Header, HTTPException, Query, Security, status
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, GetCoreSchemaHandler
from pydantic_core import CoreSchema, core_schema
from fastapi.security import (HTTPAuthorizationCredentials, HTTPBasic, HTTPBasicCredentials, HTTPBearer,
                              OAuth2PasswordBearer, SecurityScopes)
from pydantic import AfterValidator

router = APIRouter(prefix="/secx")
CALLS: list = []


class HTTPBearer403(HTTPBearer):
    def make_not_authenticated_error(self) -> HTTPException:
        return HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail="Not authenticated")


class TeaBearer(OAuth2PasswordBearer):
    """only the challenge changes"""

    def make_authenticate_headers(self) -> dict[str, str]:
        return {"WWW-Authenticate": 'Bearer realm="tea"', "X-Tea": "yes"}


class QuietBasic(HTTPBasic):
    def make_not_authenticated_error(self) -> HTTPException:
        return HTTPException(status_code=418, detail={"why": "teapot"}, headers={"X-Basic": "no"})


bearer403 = HTTPBearer403()
optional403 = HTTPBearer403(auto_error=False)
tea = TeaBearer(tokenUrl="token", scopes={"me": "Me", "items": "Items"})
basic = QuietBasic()

CredentialsDep = Annotated[HTTPAuthorizationCredentials, Depends(HTTPBearer403())]


@router.get("/me")
def read_me(credentials: CredentialsDep):
    return {"message": "You are authenticated", "token": credentials.credentials}


@router.get("/me2")
def read_me2(credentials: Annotated[HTTPAuthorizationCredentials | None, Depends(optional403)]):
    return {"token": credentials.credentials if credentials else None}


@router.get("/tea")
def read_tea(token: Annotated[str, Depends(tea)]):
    return {"token": token}


@router.get("/basic")
def read_basic(c: Annotated[HTTPBasicCredentials, Depends(basic)]):
    return {"user": c.username, "password": c.password}


async def current_user(security_scopes: SecurityScopes, token: Annotated[str, Security(tea, scopes=["base"])]):
    CALLS.append(f"user {security_scopes.scopes}")
    authenticate = f'Bearer scope="{security_scopes.scope_str}"' if security_scopes.scopes else "Bearer"
    user, _, granted = token.partition(":")
    for scope in security_scopes.scopes:
        if scope not in granted.split(","):
            raise HTTPException(status_code=401, detail=f"Not enough permissions: {scope}",
                                headers={"WWW-Authenticate": authenticate})
    return {"user": user, "scopes": security_scopes.scopes, "str": security_scopes.scope_str}


async def active_user(current: Annotated[dict, Security(current_user, scopes=["me"])]):
    CALLS.append("active")
    return current


def plain_dep(scopes: SecurityScopes):
    return list(scopes.scopes)


@router.get("/users/me")
async def users_me(user: Annotated[dict, Depends(active_user)]):
    out = {"user": user, "calls": list(CALLS)}
    CALLS.clear()
    return out


@router.get("/users/items")
async def users_items(user: Annotated[dict, Security(active_user, scopes=["items", "me"])],
                      again: Annotated[dict, Security(current_user, scopes=["me", "items", "base"])],
                      other: Annotated[dict, Security(current_user, scopes=["admin"])]):
    out = {"user": user, "again": again, "other": other, "calls": list(CALLS)}
    CALLS.clear()
    return out


@router.get("/status", dependencies=[Security(plain_dep, scopes=["z", "a", "z"])])
async def status_(user: Annotated[dict, Depends(current_user)], s: Annotated[list, Security(plain_dep, scopes=["q"])]):
    out = {"user": user, "s": s, "calls": list(CALLS)}
    CALLS.clear()
    return out


def check_valid_id(id: str):
    if not id.startswith(("isbn-", "imdb-")):
        raise ValueError('Invalid ID format, it must start with "isbn-" or "imdb-"')
    return id


def upper(v):
    return v.upper()


def sort_list(v: list) -> list:
    return sorted(v)


def positive(n: int) -> int:
    assert n > 0, "must be positive"
    return n * 10


@dataclass(frozen=True)
class MyAfterValidator:
    func: Callable[[Any], Any]

    def __get_pydantic_core_schema__(self, source_type: Any, handler: GetCoreSchemaHandler) -> CoreSchema:
        return core_schema.no_info_after_validator_function(self.func, handler(source_type))


class Named(BaseModel):
    name: Annotated[str, MyAfterValidator(str.lower)]
    title: Annotated[str, MyAfterValidator(check_valid_id)] = "isbn-0"
    short: Annotated[str, MyAfterValidator(lambda v: v[:3])] = "abcdef"


@router.post("/named")
def named(item: Named) -> Named:
    return item


@router.get("/items/")
async def read_items(id: Annotated[str | None, AfterValidator(check_valid_id)] = None,
                     tag: Annotated[str, AfterValidator(upper), AfterValidator(lambda v: v + "!")] = "none"):
    return {"id": id, "tag": tag}


@router.get("/items/{n}")
async def read_n(n: Annotated[int, AfterValidator(positive)],
                 x_token: Annotated[str | None, Header(), AfterValidator(check_valid_id)] = None,
                 q: Annotated[list[int], Query(), AfterValidator(sort_list)] = []):  # noqa: B006
    return {"n": n, "x_token": x_token, "q": q}


some_file_path = os.path.join(os.environ.get("TMPDIR", "/tmp"), "py2axum-secx-video.mp4")


@router.get("/video")
def video():
    def iterfile():
        with open(some_file_path, mode="rb") as file_like:
            yield from file_like

    return StreamingResponse(iterfile(), media_type="video/mp4")


@router.get("/letters")
def letters():
    def gen():
        yield "<"
        yield from ["a", "b"]
        yield from "cd"
        yield from sorted(["y", "x"])
        yield from ("e", "f")
        yield ">"

    return StreamingResponse(gen(), media_type="text/plain")


@router.post("/video")
def write_video(body: dict):
    with open(some_file_path, "wb") as f:
        f.write(body["data"].encode() * body.get("n", 1))
    return {"ok": True}

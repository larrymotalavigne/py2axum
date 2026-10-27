"""Starlette's AuthenticationMiddleware (a project subclass of it, as teamhide/fastapi-boilerplate and its
descendants write it) with a project backend: `request.user` / `request.auth` as the backend returns them (a
`(bool, user)` pair as those apps do, or AuthCredentials and SimpleUser), None (UnauthenticatedUser), an
AuthenticationError answered by the default on_error (400, its text) or by the project's."""
from fastapi import APIRouter, Request
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from starlette.authentication import (AuthCredentials, AuthenticationBackend, AuthenticationError, SimpleUser,
                                      UnauthenticatedUser)
from starlette.middleware.authentication import AuthenticationMiddleware as BaseAuthenticationMiddleware
from starlette.requests import HTTPConnection

router = APIRouter(prefix="/authx")


class CurrentUser(BaseModel):
    id: int | None = None


class Backend(AuthenticationBackend):
    async def authenticate(self, conn: HTTPConnection):
        mode = conn.headers.get("x-authx")
        if mode is None:
            return None
        if mode == "bad":
            raise AuthenticationError("bad credentials")
        if mode == "custom":
            raise AuthenticationError("custom")
        if mode.startswith("simple:"):
            return AuthCredentials(["read", "write"]), SimpleUser(mode[len("simple:"):])
        if mode.startswith("pair:"):
            # teamhide's backend: a bool and the user, whatever the header
            user = CurrentUser()
            if mode == "pair:":
                return False, user
            user.id = int(mode[len("pair:"):])
            return True, user
        return AuthCredentials(), UnauthenticatedUser()


def on_error(conn: HTTPConnection, exc: AuthenticationError):
    if str(exc) == "custom":
        return JSONResponse({"error": str(exc), "path": conn.url.path}, status_code=401)
    return BaseAuthenticationMiddleware.default_on_error(conn, exc)


class AuthMiddleware(BaseAuthenticationMiddleware):
    pass


@router.get("/me")
async def me(request: Request):
    user, auth = request.user, request.auth
    if isinstance(user, CurrentUser):
        return {"kind": "pair", "auth": auth, "id": user.id}
    return {"kind": type(user).__name__, "authenticated": user.is_authenticated, "name": user.display_name,
            "identity": user.identity, "scopes": auth.scopes}

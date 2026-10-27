"""Callable instances as dependencies (FastAPI reads `__call__`'s parameters): `Depends(RoleChecker([...]))` inline and
through a module-level `Depends(...)` listed in `dependencies=`; project subclasses of HTTPBearer with a `__call__` of
their own calling `super().__call__(request)` (fastapi-beyond-CRUD's TokenBearer chain); a bare `raise` in an
endpoint (RuntimeError: no exception is being handled)."""
from fastapi import APIRouter, Depends, HTTPException, Request
from fastapi.security import HTTPBearer

router = APIRouter(prefix="/depinst")


def current_user(request: Request) -> str:
    return request.headers.get("x-user", "anon")


class RoleChecker:
    def __init__(self, allowed: list[str]) -> None:
        self.allowed = allowed

    def __call__(self, user: str = Depends(current_user)) -> str:
        if user not in self.allowed:
            raise HTTPException(status_code=403, detail=f"{user} not in {self.allowed}")
        return user


admin_only = Depends(RoleChecker(["admin"]))


class TokenBearer(HTTPBearer):
    def __init__(self, auto_error: bool = True):
        super().__init__(auto_error=auto_error)

    async def __call__(self, request: Request) -> dict:
        creds = await super().__call__(request)
        if creds is None:
            return {"anon": True, "scheme_name": self.scheme_name}
        if not self.token_valid(creds.credentials):
            raise HTTPException(status_code=401, detail="bad token")
        data = {"token": creds.credentials, "scheme": creds.scheme, "auto_error": self.auto_error}
        self.verify_token_data(data)
        return data

    def token_valid(self, token: str) -> bool:
        return token.startswith("t-")

    def verify_token_data(self, data: dict) -> None:
        raise NotImplementedError("Please override this method in child classes")


class AccessTokenBearer(TokenBearer):
    def verify_token_data(self, data: dict) -> None:
        if data["token"].endswith("-refresh"):
            raise HTTPException(status_code=403, detail="access token required")


class OptionalBearer(TokenBearer):
    def verify_token_data(self, data: dict) -> None:
        pass


@router.get("/admin", dependencies=[admin_only])
def admin():
    return {"ok": True}


@router.get("/editor")
def editor(user: str = Depends(RoleChecker(["admin", "editor"]))):
    return {"user": user}


@router.get("/token")
async def token(data: dict = Depends(AccessTokenBearer())):
    return data


@router.get("/optional")
async def optional(data: dict = Depends(OptionalBearer(auto_error=False))):
    return data


@router.get("/bare-raise")
def bare_raise():
    raise

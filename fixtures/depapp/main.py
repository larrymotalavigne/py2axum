from typing import Annotated

from fastapi import Depends, FastAPI, Header, HTTPException

from .factories import router as fact_router
from .gens import router as gens_router
from .klass import router as klass_router
from .state import EVENTS


def create_app(realm: str = "hub") -> FastAPI:
    """The application factory: dependencies and an `Annotated` alias bound in it, read by the endpoints it
    defines (closing over its locals)."""
    app = FastAPI()
    app.include_router(gens_router)
    app.include_router(klass_router)
    app.include_router(fact_router)
    store = {"realm": realm, "tokens": {"t1": "ann", "t2": "bob"}}

    def claims(authorization: Annotated[str | None, Header()] = None) -> dict:
        if not authorization or not authorization.startswith("Bearer "):
            raise HTTPException(401, "Authentication required")
        user = store["tokens"].get(authorization[7:])
        if user is None:
            raise HTTPException(403, "unknown token")
        EVENTS.append(f"claims {user}")
        return {"user": user, "realm": store["realm"]}

    def admin(c: Annotated[dict, Depends(claims)]) -> dict:
        if c["user"] != "ann":
            raise HTTPException(403, "admin only")
        return c

    Auth = Annotated[dict, Depends(claims)]
    Admin = Annotated[dict, Depends(admin)]

    @app.get("/health")
    async def health():
        return {"ok": True}

    @app.get("/events")
    async def events():
        out = list(EVENTS)
        EVENTS.clear()
        return out

    @app.get("/fac/me")
    def me(c: Auth):
        return c

    @app.get("/fac/twice")
    def twice(a: Auth, b: Auth, c: dict = Depends(claims)):
        return {"same": a is b and b is c}

    @app.get("/fac/admin", dependencies=[Depends(claims)])
    def admin_only(c: Admin):
        return {"admin": c["user"]}

    return app


app = create_app()

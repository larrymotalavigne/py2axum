"""An application built by a factory (create_app()): endpoints defined inside it read the
factory's locals; a missing optional module is an ImportError caught at run time."""
import os
from datetime import UTC, datetime

from fastapi import FastAPI, HTTPException, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse

from fixtures.factoryapp.middleware import CountingMiddleware, SecurityHeadersMiddleware


class Settings:
    def __init__(self):
        self.app_name = os.getenv("FACTORY_NAME", "factory")
        self.app_version = "1.2.3"
        self.TESTING = os.getenv("FACTORY_TESTING") == "1"


class Conflict(Exception):
    def __init__(self, what: str):
        super().__init__(f"conflict on {what}")
        self.what = what


def get_settings() -> Settings:
    return Settings()


def country_for(ip: str) -> str | None:
    try:
        from fixtures.factoryapp import geoip  # noqa: F401 — absent on purpose

        return geoip.country_for(ip)
    except ImportError:
        return None


def create_app() -> FastAPI:
    settings = get_settings()
    prefix = settings.app_name.upper() + "-"
    app = FastAPI()

    # last added = outermost: CORS, then the counter, then the security headers
    app.add_middleware(SecurityHeadersMiddleware)
    if not settings.TESTING:
        app.add_middleware(CountingMiddleware, tier="strict")
    else:
        app.add_middleware(CountingMiddleware, tier="loose")
    app.add_middleware(
        CORSMiddleware,
        allow_origins=["http://ok.example", "https://www.ok.example"],
        allow_credentials=True,
        allow_methods=["GET", "POST", "PUT"],
        allow_headers=["Content-Type", "Authorization", "X-Custom"],
        expose_headers=["X-RateLimit-Remaining", "X-Total-Count"],
    )

    @app.exception_handler(Conflict)
    async def _conflict(request: Request, exc: Conflict):
        message = str(getattr(exc, "orig", exc))
        return JSONResponse(status_code=409, content={"detail": message, "what": exc.what,
                                                      "path": request.url.path, "method": request.method})

    @app.exception_handler(404)
    async def _not_found(request: Request, exc: HTTPException):
        return JSONResponse(status_code=404, content={"detail": "nothing at " + request.url.path})

    # ServerErrorMiddleware's: its response skips the user middlewares (no CORS/security headers)
    @app.exception_handler(Exception)
    async def _unhandled(request: Request, exc: Exception):
        return JSONResponse(status_code=500, content={"detail": "Erreur interne du serveur."})

    @app.get("/conflict/{what}")
    async def conflict(what: str):
        raise Conflict(what)

    @app.get("/crash")
    async def crash():
        raise RuntimeError("boom")

    @app.get("/teapot")
    async def teapot():
        raise HTTPException(status_code=418, detail="short and stout", headers={"X-Tea": "earl grey"})

    @app.get("/missing")
    async def missing():
        raise HTTPException(status_code=404, detail="gone")

    @app.post("/items")
    async def create_item(item: dict):
        return {"created": item}

    @app.get("/limited")
    async def limited(request: Request):
        return {"cookies": request.cookies, "client": request.client.host,
                "counts": [request.client[0] == request.client.host]}

    @app.get("/health")
    async def health_check():
        return {"status": "healthy", "service": settings.app_name, "version": settings.app_version,
                "ts_ok": datetime.now(UTC).year > 2000}

    @app.get("/")
    async def root():
        return {"message": f"Welcome to {prefix}{settings.app_name}", "country": country_for("1.2.3.4")}

    return app


app = create_app()

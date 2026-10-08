"""An application built by a factory (create_app()): endpoints defined inside it read the
factory's locals; a missing optional module is an ImportError caught at run time."""
import logging
import os
import traceback
from datetime import UTC, datetime

from fastapi import FastAPI, HTTPException, Request
from fastapi.exceptions import RequestValidationError
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from pydantic import BaseModel

from fixtures.factoryapp.middleware import CountingMiddleware, SecurityHeadersMiddleware, StampMiddleware
from fixtures.factoryapp.observe import install_observability, introspect, router as shop


class Settings:
    def __init__(self):
        self.app_name = os.getenv("FACTORY_NAME", "factory")
        self.app_version = "1.2.3"
        self.TESTING = os.getenv("FACTORY_TESTING") == "1"


class Conflict(Exception):
    def __init__(self, what: str):
        super().__init__(f"conflict on {what}")
        self.what = what


class Gone(Exception):
    pass


class Typed(BaseModel):
    qty: int


async def _gone(request: Request, exc: Gone):
    # the traceback is logged as the application does; the response shows the exception's own line
    logging.getLogger("factory").info("".join(traceback.format_exception(type(exc), exc, exc.__traceback__)))
    return JSONResponse(status_code=410, content={"lines": traceback.format_exception(type(exc), exc, None),
                                                  "only": traceback.format_exception_only(exc)})


def add_error_handlers(app: FastAPI):
    """handlers registered by a function given the application (not in the factory's body)"""
    @app.exception_handler(RequestValidationError)
    async def _invalid(request: Request, exc: RequestValidationError):
        tb = "".join(traceback.format_exception(type(exc), exc, exc.__traceback__))
        logging.getLogger("factory").info(f"{request.method} {request.url} 422\n{tb}")
        return JSONResponse(status_code=422, content={"invalid": exc.errors(), "n": len(exc.errors())})

    async def _key(request: Request, exc: KeyError):
        return JSONResponse(status_code=400, content={"lines": traceback.format_exception(type(exc), exc, None), "path": request.url.path})

    app.add_exception_handler(KeyError, _key)

    @app.middleware("http")
    async def _configured(request: Request, call_next):
        response = await call_next(request)
        response.headers["x-configured"] = "yes"
        return response


def get_settings() -> Settings:
    return Settings()


def country_for(ip: str) -> str | None:
    try:
        from fixtures.factoryapp import geoip  # noqa: F401 — absent on purpose

        return geoip.country_for(ip)
    except ImportError:
        return None


def create_app(observed: bool = True) -> FastAPI:
    settings = get_settings()
    prefix = settings.app_name.upper() + "-"
    app = FastAPI(strict_content_type=False)

    # last added = outermost: CORS, then the counter, then the security headers
    app.add_middleware(StampMiddleware)
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

    add_error_handlers(app)
    app.add_exception_handler(Gone, _gone)

    @app.post("/typed")
    async def typed(item: Typed):
        return {"qty": item.qty}

    @app.get("/keyerror")
    async def keyerror():
        return {}["missing"]

    @app.get("/gone/{what}")
    async def gone(what: str):
        raise Gone(f"{what} is gone")

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

    app.include_router(shop)
    app.include_router(shop, prefix="/v2")
    app.include_router(introspect)
    if observed:  # a parameter of the factory: the server calls it without arguments
        install_observability(app)
    return app


app = create_app()

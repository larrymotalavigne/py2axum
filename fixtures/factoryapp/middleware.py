"""Middlewares written as BaseHTTPMiddleware subclasses."""
from fastapi import Request, Response
from starlette.middleware.base import BaseHTTPMiddleware
from starlette.types import ASGIApp

_LIMITS = {"strict": 2, "loose": 100}


class SecurityHeadersMiddleware(BaseHTTPMiddleware):
    def __init__(self, app: ASGIApp):
        super().__init__(app)

    async def dispatch(self, request: Request, call_next) -> Response:
        response = await call_next(request)
        response.headers["X-Content-Type-Options"] = "nosniff"
        response.headers["X-Frame-Options"] = "DENY"
        response.headers["Content-Security-Policy"] = "default-src 'self'; frame-ancestors 'none';"
        if "x-seen" in response.headers:
            response.headers["X-Seen"] = response.headers["x-seen"] + ", security"
        return response


class CountingMiddleware(BaseHTTPMiddleware):
    """Per-key counter kept on the instance (one instance for the whole app, like Starlette)."""

    def __init__(self, app: ASGIApp, tier: str = "strict"):
        super().__init__(app)
        self._windows: dict[str, list[int]] = {}
        self.limit = _LIMITS[tier]

    async def dispatch(self, request: Request, call_next) -> Response:
        if not request.url.path.startswith("/limited"):
            return await call_next(request)
        session = request.cookies.get("session")
        key = f"user:{session}" if session else f"ip:{request.client.host}"
        store = self._windows.setdefault(key, [])
        if len(store) >= self.limit:
            return Response(content="Rate limit exceeded.", status_code=429,
                            headers={"Retry-After": "60", "X-RateLimit-Limit": str(self.limit)})
        store.append(1)
        response = await call_next(request)
        response.headers["X-RateLimit-Remaining"] = str(self.limit - len(store))
        response.headers["X-Key"] = key
        response.headers["X-Seen"] = "counting"
        return response


class StampMiddleware(BaseHTTPMiddleware):
    """No __init__: BaseHTTPMiddleware's (app, dispatch=None) is the one called."""

    async def dispatch(self, request: Request, call_next) -> Response:
        response = await call_next(request)
        response.headers["X-Stamp"] = request.method.lower()
        return response

"""Request metrics installed by a function given the application: a BaseHTTPMiddleware labelling each
request with the template of the route it matches (walking `app.router.routes` with `matches()`), and a
`/metrics` route added with `app.add_route`. The servers run in prometheus_client's multiprocess mode
(`PROMETHEUS_MULTIPROC_DIR`, see scripts_start_factory.sh): `/metrics/all` merges the process files."""
import os
import time

from fastapi import APIRouter, Request, Response
from prometheus_client import CONTENT_TYPE_LATEST, CollectorRegistry, Counter, Gauge, Histogram, Summary, generate_latest
from prometheus_client.multiprocess import MultiProcessCollector
from starlette.middleware.base import BaseHTTPMiddleware
from starlette.routing import Match, Route

try:
    from prometheus_client import GC_COLLECTOR  # noqa: F401
    HAS_GC = True
except ImportError:
    HAS_GC = False

REG = CollectorRegistry()
TIMES = CollectorRegistry()
REQUESTS = Counter("http_requests", "Requests by route", ["method", "status", "route"], registry=REG)
ACTIVE = Gauge("http_active", "Requests in progress", ["route"], registry=REG)
SECONDS = Histogram("http_seconds", "Request durations", ["route"], buckets=(60,), registry=TIMES)
WORKERS = Gauge("http_workers", "Live workers", multiprocess_mode="livesum", registry=REG)
PEAK = Gauge("http_peak", "Highest item seen", multiprocess_mode="max", registry=REG)
LAST = Gauge("http_last", "Last item seen", multiprocess_mode="mostrecent", registry=REG)
SIZES = Summary("http_sizes", "Item id lengths", ["kind"], registry=REG)

router = APIRouter(prefix="/shop")


@router.get("/items/{item_id}")
async def item(item_id: str, request: Request):
    if item_id.isdigit():
        PEAK.set(int(item_id))
        LAST.set(int(item_id))
    SIZES.labels("digits" if item_id.isdigit() else "other").observe(len(item_id))
    return {"item": item_id, "route": label(request), "active": REG.get_sample_value("http_active", {"route": label(request)})}


@router.post("/items")
async def add_item():
    return {"added": True}


def template(routes, scope) -> str | None:
    """The template of the first route matching the scope, through included routers."""
    for route in routes:
        found, child = route.matches(scope)
        if found == Match.NONE:
            continue
        path = getattr(route, "path_format", None) or getattr(route, "path", None)
        if path:
            return path
        nested = getattr(getattr(route, "original_router", None), "routes", None)
        if nested:
            inner = template(nested, {**scope, **child} if child else scope)
            if inner:
                return inner
    return None


def label(request: Request) -> str:
    route = request.scope.get("route")
    known = getattr(route, "path_format", None)
    if known:
        return known
    return template(request.app.router.routes, request.scope) or request.url.path


class ObserveMiddleware(BaseHTTPMiddleware):
    def __init__(self, app, *, skip: tuple = ("/metrics",)):
        super().__init__(app)
        self.skip = set(skip)

    async def dispatch(self, request: Request, call_next):
        route = label(request)
        if route in self.skip:
            return await call_next(request)
        ACTIVE.labels(route=route).inc()
        start = time.perf_counter()
        try:
            response = await call_next(request)
            REQUESTS.labels(method=request.method, status=getattr(response, "status_code", 500), route=route).inc()
            return response
        except Exception:
            REQUESTS.labels(method=request.method, status=500, route=route).inc()
            raise
        finally:
            ACTIVE.labels(route=route).dec()
            SECONDS.labels(route).observe(time.perf_counter() - start)


async def metrics(_: Request) -> Response:
    return Response(generate_latest(REG), media_type=CONTENT_TYPE_LATEST)


async def metrics_all(_: Request) -> Response:
    registry = CollectorRegistry()
    MultiProcessCollector(registry)
    return Response(generate_latest(registry), media_type=CONTENT_TYPE_LATEST)


def install_observability(app, path: str = "/metrics") -> None:
    if os.getenv("FACTORY_NO_METRICS"):
        return
    taken = any(isinstance(r, Route) and r.path == path for r in app.routes)
    WORKERS.set(1)
    app.add_middleware(ObserveMiddleware, skip=(path, path + "/all"))
    if not taken:
        app.add_route(path, metrics, methods=["GET"])
        app.add_route(path + "/all", metrics_all)


def describe(routes, depth: int = 0) -> list:
    out = []
    for r in routes:
        out.append([depth, type(r).__name__, getattr(r, "path", None), sorted(getattr(r, "methods", None) or []),
                    isinstance(r, Route)])
        nested = getattr(getattr(r, "original_router", None), "routes", None)
        if nested:
            out += describe(nested, depth + 1)
    return out


def probe(app, method: str, path: str) -> list:
    scope = {"type": "http", "method": method, "path": path, "root_path": ""}
    out = []
    for r in app.router.routes:
        found, child = r.matches(scope)
        out.append(found.name)
    return [out, template(app.router.routes, scope)]


introspect = APIRouter()


@introspect.get("/introspect")
async def show(request: Request):
    app = request.app
    seen = request.scope
    return {"routes": describe(app.routes), "has_gc": HAS_GC,
            "scope": [seen["type"], seen["method"], seen["path"], seen["root_path"], type(seen.get("route")).__name__,
                      seen["route"].path_format, seen["path_params"], "app" in seen],
            "probes": [probe(app, m, p) for m, p in (("GET", "/shop/items/3"), ("POST", "/shop/items/3"), ("HEAD", "/metrics"),
                                                    ("GET", "/v2/shop/items/9"), ("GET", "/nope"), ("GET", "/health"))],
            "match": [repr(Match.FULL), Match.PARTIAL.value, Match.NONE == Match.NONE],
            "timed": TIMES.get_sample_value("http_seconds_count", {"route": "/health"})}

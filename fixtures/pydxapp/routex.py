"""Custom APIRoute classes (FastAPI's "Custom Request and APIRoute class"): `get_route_handler()` wrapping the route's
own handler, on an APIRouter(route_class=...): the request validation error caught and re-raised, the Response
object returned by the original handler (headers set, repr printed)."""
import gzip
from collections.abc import Callable

from fastapi import APIRouter, Body, HTTPException, Request, Response
from fastapi.exceptions import RequestValidationError
from fastapi.routing import APIRoute


class ValidationErrorLoggingRoute(APIRoute):
    def get_route_handler(self) -> Callable:
        original_route_handler = super().get_route_handler()

        async def custom_route_handler(request: Request) -> Response:
            try:
                return await original_route_handler(request)
            except RequestValidationError as exc:
                body = await request.body()
                detail = {"errors": exc.errors(), "body": body.decode()}
                raise HTTPException(status_code=422, detail=detail)

        return custom_route_handler


class HeaderRoute(APIRoute):
    def get_route_handler(self) -> Callable:
        original_route_handler = super().get_route_handler()

        async def custom_route_handler(request: Request) -> Response:
            response: Response = await original_route_handler(request)
            response.headers["X-Route"] = "custom"
            print(f"route response status: {response.status_code}")
            print(f"route response headers: {response.headers}")
            print(f"route response type: {type(response).__name__}")
            return response

        return custom_route_handler


class GzipRequest(Request):
    async def body(self) -> bytes:
        if not hasattr(self, "_body"):
            body = await super().body()
            if "gzip" in self.headers.getlist("Content-Encoding"):
                body = gzip.decompress(body)
            self._body = body
        return self._body


class GzipRoute(APIRoute):
    def get_route_handler(self) -> Callable:
        original_route_handler = super().get_route_handler()

        async def custom_route_handler(request: Request) -> Response:
            request = GzipRequest(request.scope, request.receive)
            return await original_route_handler(request)

        return custom_route_handler


gzip_router = APIRouter(prefix="/routex/gz", route_class=GzipRoute)


@gzip_router.post("/sum")
async def gz_sum(numbers: list[int] = Body()):
    return {"sum": sum(numbers)}


@gzip_router.post("/echo/{name}")
async def gz_echo(name: str, request: Request, q: int = 0):
    return {"name": name, "q": q, "type": type(request).__name__, "body": (await request.body()).decode()}


logging_router = APIRouter(prefix="/routex/log", route_class=ValidationErrorLoggingRoute)
header_router = APIRouter(prefix="/routex/hdr", route_class=HeaderRoute)
plain_router = APIRouter(prefix="/routex/plain")


@logging_router.post("/sum")
async def sum_numbers(numbers: list[int] = Body()):
    return sum(numbers)


@logging_router.get("/item/{item_id}")
def item(item_id: int, q: str | None = None):
    if item_id == 0:
        raise HTTPException(status_code=404, detail="no item 0")
    return {"item_id": item_id, "q": q}


@header_router.get("/timed")
async def timed():
    return {"message": "It's the time of my life"}


@header_router.get("/text", response_class=Response)
async def text():
    return Response("plain", media_type="text/plain", headers={"X-Own": "1"})


@header_router.post("/check")
async def check(n: int = Body(embed=True)):
    return {"n": n}


@plain_router.get("/")
async def not_timed():
    return {"message": "Not timed"}

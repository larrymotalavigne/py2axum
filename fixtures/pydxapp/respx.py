"""Project subclasses of the response classes as `response_class=`: their `media_type` and their
`render(self, content)` (given the encoded value); a StreamingResponse subclass for a generator endpoint (its
media type, the items as they are); `yield from` an io.BytesIO returned by a function."""
import base64
from collections.abc import AsyncIterable, Iterable
from io import BytesIO
from typing import Any

import orjson
from fastapi import APIRouter, Response
from fastapi.responses import HTMLResponse, JSONResponse, PlainTextResponse, StreamingResponse

router = APIRouter(prefix="/respx")

binary_image = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")


def read_image() -> BytesIO:
    return BytesIO(binary_image + b"\nline two\nend")


class CustomORJSONResponse(Response):
    media_type = "application/json"

    def render(self, content: Any) -> bytes:
        assert orjson is not None, "orjson must be installed"
        return orjson.dumps(content, option=orjson.OPT_INDENT_2)


class ApiJSON(JSONResponse):
    """only the media type changes"""

    media_type = "application/vnd.api+json"


class Shout(PlainTextResponse):
    def render(self, content: Any) -> bytes:
        return str(content).upper().encode()


class Xml(HTMLResponse):
    media_type = "text/xml"


class PNGStreamingResponse(StreamingResponse):
    media_type = "image/png"


@router.get("/orjson", response_class=CustomORJSONResponse)
async def main():
    return {"message": "Hello World", "nested": [1, {"a": None}], "é": 1.5}


@router.get("/api", response_class=ApiJSON)
def api():
    return {"items": [1, 2]}


@router.get("/api/{n}", response_class=ApiJSON, status_code=201)
def api_n(n: int):
    if n == 0:
        return Response("raw", media_type="text/plain")
    return {"n": n}


@router.get("/shout", response_class=Shout)
def shout(q: str = "hi"):
    return q


@router.get("/xml", response_class=Xml)
def xml():
    return "<a>b</a>"


@router.get("/empty", response_class=CustomORJSONResponse, status_code=204)
def empty():
    return None


@router.get("/image/stream", response_class=PNGStreamingResponse)
async def stream_image() -> AsyncIterable[bytes]:
    with read_image() as image_file:
        for chunk in image_file:
            yield chunk


@router.get("/image/stream-no-async", response_class=PNGStreamingResponse)
def stream_image_no_async() -> Iterable[bytes]:
    with read_image() as image_file:
        for chunk in image_file:
            yield chunk


@router.get("/image/stream-no-async-yield-from", response_class=PNGStreamingResponse)
def stream_image_no_async_yield_from() -> Iterable[bytes]:
    with read_image() as image_file:
        yield from image_file


@router.get("/image/stream-no-annotation", response_class=PNGStreamingResponse)
async def stream_image_no_annotation():
    with read_image() as image_file:
        for chunk in image_file:
            yield chunk

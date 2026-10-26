"""prometheus_fastapi_instrumentator: the middleware's metrics (handler templates, status codes, request and
response sizes, excluded handlers) in a registry of their own; durations are left out of the comparison."""
from fastapi import APIRouter, Response
from fastapi.responses import StreamingResponse
from prometheus_client import CollectorRegistry, generate_latest
from prometheus_fastapi_instrumentator import Instrumentator

REG = CollectorRegistry()
instrumentator = Instrumentator(registry=REG, should_group_status_codes=False, should_group_untemplated=False,
                                excluded_handlers=["^/instr/skip"])
router = APIRouter(prefix="/instr")


@router.get("/items/{item_id}")
async def item(item_id: int):
    return {"id": item_id, "pad": "x" * item_id}


@router.post("/echo")
async def echo(body: dict):
    return body


@router.get("/skip")
async def skip():
    return {"counted": False}


@router.get("/empty", status_code=204)
async def empty():
    return Response(status_code=204)


@router.get("/stream")
async def stream():
    async def parts():
        for p in ("a", "bb", "ccc"):
            yield p
    return StreamingResponse(parts(), media_type="text/plain")


@router.get("/boom")
async def boom():
    raise RuntimeError("instrumented failure")


@router.get("/check")
async def check():
    """the samples of the /instr handlers (not the durations' sums and buckets), sorted"""
    out = []
    for line in generate_latest(REG).decode().splitlines():
        if line.startswith("#"):
            if "duration" not in line:
                out.append(line)
            continue
        if 'handler="/instr' not in line or "/instr/check" in line or "_created" in line:
            continue
        if "duration" in line and not line.split("{")[0].endswith("_count"):
            continue
        out.append(line)
    return {"lines": sorted(out)}

import os

import aiohttp
from fastapi import APIRouter, Depends

from ..db import get_http

UPSTREAM_URL = os.environ.get("UPSTREAM_URL", "http://127.0.0.1:8000")

router = APIRouter(prefix="/upstream", tags=["upstream"])


@router.get("/health")
async def upstream_health(http: aiohttp.ClientSession = Depends(get_http)):
    async with http.get(f"{UPSTREAM_URL}/health") as resp:
        data = await resp.json()
    return {"upstream": data["status"], "upstream_status_code": resp.status}

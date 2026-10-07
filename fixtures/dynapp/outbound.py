"""Outgoing requests timed by replacing the HTTP clients' request method (`aiohttp.ClientSession._request`,
`httpx.AsyncClient.request`) with a wrapper, at import, if the libraries are installed."""
import functools

from fastapi import APIRouter, Request

try:
    import aiohttp
except ImportError:
    aiohttp = None

try:
    import httpx
except ImportError:
    httpx = None

router = APIRouter(prefix="/outbound")
CALLS: list = []


def traced(lib: str):
    def deco(function):
        @functools.wraps(function)
        async def wrapper(*args, **kwargs):
            method, url = args[1], args[2]
            status = ""
            try:
                result = await function(*args, **kwargs)
                status = result.status if lib == "aiohttp" else result.status_code
            except Exception as error:
                status = type(error).__name__
                raise
            finally:
                # httpx's get() passes all its keyword arguments along: only aiohttp's are compared
                CALLS.append([lib, method, str(url).split("/libs")[-1], status, sorted(kwargs) if lib == "aiohttp" else None])
            return result

        return wrapper

    return deco


_patched = False


def patch_clients() -> None:
    global _patched
    if not _patched:
        if aiohttp is not None:
            aiohttp.ClientSession._request = traced("aiohttp")(aiohttp.ClientSession._request)
        if httpx is not None:
            httpx.AsyncClient.request = traced("httpx")(httpx.AsyncClient.request)
        _patched = True


patch_clients()


@router.get("/run")
async def run(request: Request):
    CALLS.clear()
    base = f"http://{request.headers['host']}/libs"
    out = {"names": [aiohttp.ClientSession._request.__name__, httpx.AsyncClient.request.__qualname__]}
    async with aiohttp.ClientSession() as session:
        async with session.get(f"{base}/status/404") as resp:
            out["get"] = [resp.status, await resp.text()]
        async with session.post(f"{base}/echo", json={"a": 1}) as resp:
            out["post"] = (await resp.json())["body"]
        async with session.request("put", f"{base}/echo", data="x") as resp:
            out["request"] = resp.status
        async with session.get(f"{base}/status/200", allow_redirects=True, params={"q": "1"}) as resp:
            out["params"] = resp.status
        try:
            async with session.get("http://127.0.0.1:9/x") as resp:
                pass
        except (aiohttp.ClientError, TimeoutError) as error:
            out["refused"] = type(error).__name__
    async with httpx.AsyncClient() as client:
        r = await client.get(f"{base}/status/201")
        out["httpx"] = r.status_code
        r = await client.request("POST", f"{base}/echo", content=b"raw")
        out["httpx_request"] = r.json()["body"]
    out["calls"] = CALLS
    return out

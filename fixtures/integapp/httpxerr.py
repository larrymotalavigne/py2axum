"""httpx's exception classes, as the client raises them against a misbehaving server (tests/bad_http.py, port
INTEG_BAD_HTTP_PORT): the server closing before the response or in the body, a reset, a timeout, a refused
connection; caught by their class, a tuple of classes or a base, with `.request` (and `.response`)."""
import os

import httpx
from fastapi import APIRouter

router = APIRouter(prefix="/httpxerr")

BAD = f"http://127.0.0.1:{os.environ.get('INTEG_BAD_HTTP_PORT', '8028')}"

RETRYABLE = (httpx.ReadError, httpx.ConnectError, httpx.RemoteProtocolError, httpx.ReadTimeout, httpx.ConnectTimeout)


def _is_retryable(exc: Exception) -> bool:
    """as an LLM client decides whether to retry"""
    if isinstance(exc, httpx.HTTPStatusError):
        return exc.response.status_code in (429, 500, 502, 503, 504)
    if isinstance(exc, RETRYABLE):
        return True
    return False


def _bases(e: BaseException) -> dict:
    return {
        "http": isinstance(e, httpx.HTTPError), "request_error": isinstance(e, httpx.RequestError),
        "transport": isinstance(e, httpx.TransportError), "network": isinstance(e, httpx.NetworkError),
        "protocol": isinstance(e, httpx.ProtocolError), "timeout": isinstance(e, httpx.TimeoutException),
        "local": isinstance(e, httpx.LocalProtocolError), "write": isinstance(e, (httpx.WriteError, httpx.WriteTimeout)),
        "pool": isinstance(e, httpx.PoolTimeout), "close": isinstance(e, httpx.CloseError),
        "proxy": isinstance(e, httpx.ProxyError), "stream": isinstance(e, httpx.StreamError),
        "runtime": isinstance(e, RuntimeError), "retry": _is_retryable(e),
    }


@router.get("/call/{what}")
async def call(what: str, timeout: float = 2.0):
    url = f"{BAD}/{what}" if what != "refused" else "http://127.0.0.1:9/x"
    async with httpx.AsyncClient(timeout=httpx.Timeout(timeout)) as client:
        try:
            r = await client.get(url)
            r.raise_for_status()
            return {"ok": r.status_code, "text": r.text}
        except httpx.HTTPStatusError as e:
            return {"status_error": str(e).replace(BAD, "<bad>"), "code": e.response.status_code,
                    "method": e.request.method, "url": str(e.request.url).replace(BAD, "<bad>"), **_bases(e)}
        except httpx.RemoteProtocolError as e:
            return {"remote": str(e), "method": e.request.method, "url": str(e.request.url).replace(BAD, "<bad>"),
                    "host": e.request.url.host, "repr": repr(e.request).replace(BAD, "<bad>"), **_bases(e)}
        except httpx.ReadError as e:
            return {"read": str(e), "args": list(e.args), "method": e.request.method, **_bases(e)}
        except (httpx.ReadTimeout, httpx.ConnectTimeout) as e:
            return {"timeout": type(e).__name__, "msg": str(e), "method": e.request.method, **_bases(e)}
        except httpx.HTTPError as e:
            return {"other": type(e).__name__, "msg": str(e), "url": str(e.request.url), **_bases(e)}


@router.get("/raw/{what}")
async def raw(what: str):
    """not caught: a 500, its traceback in the log"""
    async with httpx.AsyncClient(timeout=httpx.Timeout(2.0)) as client:
        r = await client.get(f"{BAD}/{what}")
    return {"ok": r.status_code}


@router.get("/retry/{what}")
async def retry(what: str):
    """a retry loop on the retryable errors (the classes in a tuple)"""
    attempts = []
    async with httpx.AsyncClient(timeout=httpx.Timeout(2.0)) as client:
        for _ in range(3):
            try:
                r = await client.get(f"{BAD}/{what}")
                r.raise_for_status()
                attempts.append(r.status_code)
                break
            except Exception as e:  # noqa: BLE001
                attempts.append(type(e).__name__)
                if not _is_retryable(e):
                    break
    return attempts

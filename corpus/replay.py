"""pytest plugin: the official FastAPI tests (`tests/test_tutorial/`) played against two real servers.

`fastapi.testclient.TestClient` (and Starlette's) is replaced by a client that sends every request of a test
to the Python reference (uvicorn serving the docs_src module) and to the generated binary, in lockstep,
compares the two responses as tests/conformance.py does, and hands the binary's response to the test (its
assertions then check the binary too). The documentation routes (`/openapi.json`, `/docs`, `/redoc`) are sent
to both but not compared: the binary does not serve them (docs/supported.md, "Not supported"); so are the
requests a test sends while it overrides dependencies (`app.dependency_overrides`), which only the in-process
app sees.

    CORPUS_MODE=discover  record which docs_src module each test builds a client for, send nothing
    CORPUS_MODE=replay    CORPUS_MAP = {module: {"ref": url, "cand": url}}; a module without servers skips
    CORPUS_OUT            JSON lines written here: clients, exchanges, test outcomes

Loaded with `pytest -p replay` (corpus/ on sys.path).
"""
from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path

import httpx
import pytest



def _conformance():
    """tests/conformance.py by its path: FastAPI's own `tests` package is the one importable here."""
    import importlib.util

    path = Path(__file__).resolve().parent.parent / "tests" / "conformance.py"
    spec = importlib.util.spec_from_file_location("py2axum_conformance", path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = mod
    saved = list(sys.path)
    spec.loader.exec_module(mod)
    # it puts py2axum's root first on sys.path, whose own docs_src/ would shadow FastAPI's
    sys.path[:] = saved
    return mod


_conf = _conformance()
observe, sent_datetimes = _conf.observe, _conf.sent_datetimes

MODE = os.environ.get("CORPUS_MODE", "replay")
MAP: dict = json.loads(Path(os.environ["CORPUS_MAP"]).read_text()) if os.environ.get("CORPUS_MAP") else {}
_OUT = open(os.environ.get("CORPUS_OUT", os.devnull), "a", buffering=1)
# what the conformance runs mask too: the values a middleware computes per request (timing)
HEADER_SHAPES = {"x-process-time": r"[0-9.e-]+"}
IDLE = float(os.environ.get("CORPUS_IDLE", "3"))  # a streamed body idle this long is compared as read so far


class _Scenario:
    HEADER_SHAPES = HEADER_SHAPES


def emit(**event) -> None:
    _OUT.write(json.dumps(event, ensure_ascii=False, default=repr) + "\n")


def current_test() -> str | None:
    cur = os.environ.get("PYTEST_CURRENT_TEST")
    return cur.rsplit(" (", 1)[0] if cur else None


def caller_test_file() -> str | None:
    """The official test file building a client (a module-level `client = TestClient(app)` has no current test)."""
    f = sys._getframe(1)
    while f is not None:
        name = f.f_code.co_filename.replace("\\", "/")
        if "/tests/test_tutorial/" in name:
            return "tests/test_tutorial/" + name.split("/tests/test_tutorial/", 1)[1]
        f = f.f_back
    return None


def module_of(app) -> str | None:
    """The docs_src module an app object comes from: the one that defines `app` (its tests may import it)."""
    names = [n for n, m in list(sys.modules.items())
             if n.startswith("docs_src.") and m is not None and getattr(m, "__dict__", {}).get("app") is app]
    if not names:
        return None
    known = [n for n in names if n in MAP]
    pool = known or [n for n in names if not n.rsplit(".", 1)[-1].startswith("test_")] or names
    return min(pool, key=len)


def docs_paths(app) -> set[str]:
    # the defaults too: a test may rebuild the app with other settings (conditional OpenAPI)
    paths = {"/openapi.json", "/docs", "/docs/oauth2-redirect", "/redoc"}
    for attr in ("openapi_url", "docs_url", "redoc_url", "swagger_ui_oauth2_redirect_url"):
        v = getattr(app, attr, None)
        if v:
            paths.add(v)
    return paths


def _read(resp: httpx.Response) -> tuple[bytes, bool]:
    """The raw body, or what came before the stream went idle (an endless event stream)."""
    chunks, done = [], True
    try:
        for chunk in resp.iter_raw():
            chunks.append(chunk)
    except httpx.ReadTimeout:
        done = False
    finally:
        resp.close()
    return b"".join(chunks), done


JWT = re.compile(r"eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*")
TIME_CLAIMS = ("exp", "iat", "nbf")


def _jwt_parts(token: str):
    import base64

    def dec(x: str):
        return json.loads(base64.urlsafe_b64decode(x + "=" * (-len(x) % 4)))
    head, payload, _ = token.split(".")
    return dec(head), dec(payload)


def same_but_clock(a, b, slack: int = 2):
    """Two bodies equal except for JWTs minted a second apart (the reference and the binary are called one
    after the other: `exp = now + delta` crosses a second when hashing a password takes long): same header,
    same claims, `exp`/`iat`/`nbf` within `slack` seconds. Anything else is a difference."""
    if isinstance(a, dict) and isinstance(b, dict):
        return list(a) == list(b) and all(same_but_clock(a[k], b[k], slack) for k in a)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(same_but_clock(x, y, slack) for x, y in zip(a, b))
    if isinstance(a, str) and isinstance(b, str) and a != b and JWT.fullmatch(a) and JWT.fullmatch(b):
        try:
            (ha, pa), (hb, pb) = _jwt_parts(a), _jwt_parts(b)
        except (ValueError, json.JSONDecodeError):
            return False
        if ha != hb or list(pa) != list(pb):
            return False
        return all(pa[k] == pb[k] if k not in TIME_CLAIMS else
                   isinstance(pa[k], int) and isinstance(pb[k], int) and abs(pa[k] - pb[k]) <= slack for k in pa)
    return a == b


class LockstepTransport(httpx.BaseTransport):
    def __init__(self, module: str, docs: set[str], app=None):
        self.module = module
        self.app = app
        self.docs = docs
        self.urls = MAP[module]
        self.sent: set[str] = set()
        timeout = httpx.Timeout(10, read=IDLE)
        self.servers = {k: httpx.HTTPTransport(retries=1) for k in ("ref", "cand")}
        self.timeout = timeout

    def _send(self, which: str, request: httpx.Request, body: bytes):
        base = self.urls[which]
        url = httpx.URL(base).copy_with(raw_path=request.url.raw_path)
        req = httpx.Request(request.method, url, headers=request.headers, content=body,
                            extensions={"timeout": self.timeout.as_dict()})
        try:
            resp = self.servers[which].handle_request(req)
        except (httpx.ReadError, httpx.RemoteProtocolError, httpx.ConnectError):
            # uvicorn closes a keep-alive connection after an unhandled 500: send it again once
            try:
                resp = self.servers[which].handle_request(req)
            except httpx.TransportError as e:
                return None, type(e).__name__
        except httpx.TransportError as e:
            return None, type(e).__name__
        raw, done = _read(resp)
        return httpx.Response(resp.status_code, headers=resp.headers.raw, content=raw, request=req), done

    def close(self) -> None:
        for t in self.servers.values():
            t.close()

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        body = request.read()
        path = request.url.path
        target = path + (f"?{request.url.query.decode()}" if request.url.query else "")
        self.sent |= sent_datetimes([(request.method, target, body)])
        out = {}
        for which in ("ref", "cand"):
            out[which] = self._send(which, request, body)
        obs = {}
        for which, (resp, done) in out.items():
            if resp is None:
                obs[which] = {"req": f"{request.method} {target}", "error": done}
                continue
            try:
                obs[which] = observe(resp, "http://testserver", _Scenario, request.method, target, self.sent)
            except Exception as e:  # noqa: BLE001 - a body the observation cannot read is itself the result
                obs[which] = {"req": f"{request.method} {target}", "observe_error": repr(e)}
            if not done:
                obs[which]["stream"] = "idle"
        # a test that overrides dependencies changes the in-process app only: the servers cannot see it
        overridden = bool(getattr(self.app, "dependency_overrides", None))
        kind = "docs" if path in self.docs else "overridden" if overridden else "http"
        same = json.dumps(obs["ref"], ensure_ascii=False) == json.dumps(obs["cand"], ensure_ascii=False)
        if not same and {k: v for k, v in obs["ref"].items() if k != "body"} == \
                {k: v for k, v in obs["cand"].items() if k != "body"}:
            same = same_but_clock(obs["ref"].get("body"), obs["cand"].get("body"))
        emit(ev="exchange", module=self.module, test=current_test(), kind=kind, same=same,
             ref=obs["ref"], cand=obs["cand"])
        resp = out["cand"][0]
        if resp is None:
            raise httpx.ConnectError(f"binary: {out['cand'][1]}", request=request)
        return httpx.Response(resp.status_code, headers=resp.headers.raw, content=resp.content, request=request)


class _Skip(httpx.BaseTransport):
    def __init__(self, reason: str):
        self.reason = reason

    def handle_request(self, request):
        pytest.skip(self.reason)


class ProxyClient(httpx.Client):
    """TestClient's constructor and defaults (base URL http://testserver, user agent, redirects followed)."""

    __test__ = False  # imported as `TestClient` into the test modules

    def __init__(self, app, base_url: str = "http://testserver", raise_server_exceptions: bool = True,
                 root_path: str = "", backend: str = "asyncio", backend_options=None, cookies=None,
                 headers=None, follow_redirects: bool = True, client=("testclient", 50000)):
        self.app = app
        self.module = module_of(app)
        emit(ev="client", module=self.module, test=current_test(), file=caller_test_file(), base_url=base_url,
             root_path=root_path,
             raise_server_exceptions=raise_server_exceptions)
        if MODE == "discover":
            transport = _Skip("corpus discovery")
        elif self.module not in MAP:
            transport = _Skip(f"corpus: no servers for {self.module}")
        else:
            transport = LockstepTransport(self.module, docs_paths(app), app)
        super().__init__(base_url=base_url, headers={"user-agent": "testclient", **(headers or {})},
                         transport=transport, follow_redirects=follow_redirects, cookies=cookies)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    def websocket_connect(self, url: str, subprotocols=None, **kwargs):
        if MODE == "discover" or self.module not in MAP:
            pytest.skip("corpus discovery" if MODE == "discover" else f"corpus: no servers for {self.module}")
        return LockstepWebSocket(self.module, url, subprotocols, kwargs.get("headers") or {})


class LockstepWebSocket:
    """TestClient's WebSocket session on both servers: every action on both, what they send compared."""

    def __init__(self, module: str, url: str, subprotocols, headers):
        self.module, self.url = module, url
        self.subprotocols, self.headers = subprotocols, headers
        self.ws = {}

    def __enter__(self):
        from starlette.websockets import WebSocketDisconnect
        from websockets.exceptions import InvalidStatus
        from websockets.sync.client import connect

        path = httpx.URL(self.url).raw_path.decode()
        res = {}
        for which in ("ref", "cand"):
            base = MAP[self.module][which].replace("http://", "ws://", 1)
            try:
                self.ws[which] = connect(base + path, additional_headers=self.headers, subprotocols=self.subprotocols,
                                         compression=None, open_timeout=5, close_timeout=3)
                res[which] = {"status": 101, "subprotocol": self.ws[which].subprotocol}
            except InvalidStatus as e:
                res[which] = {"status": e.response.status_code, "body": e.response.body.decode(errors="replace")}
        self._record(f"WS {path} connect", res)
        if "cand" not in self.ws:
            raise WebSocketDisconnect(1000)
        return self

    def __exit__(self, *exc):
        for ws in self.ws.values():
            try:
                ws.close()
            except Exception:  # noqa: BLE001
                pass

    def _record(self, req: str, res: dict) -> None:
        emit(ev="exchange", module=self.module, test=current_test(), kind="ws",
             same=json.dumps(res.get("ref"), sort_keys=True) == json.dumps(res.get("cand"), sort_keys=True),
             ref={"req": req, **(res.get("ref") or {})}, cand={"req": req, **(res.get("cand") or {})})

    def _each(self, fn):
        for ws in self.ws.values():
            try:
                fn(ws)
            except Exception:  # noqa: BLE001 - a closed socket: seen at the next receive
                pass

    def send_text(self, data: str) -> None:
        self._each(lambda ws: ws.send(data))

    def send_bytes(self, data: bytes) -> None:
        self._each(lambda ws: ws.send(data))

    def send_json(self, data, mode: str = "text") -> None:
        text = json.dumps(data, separators=(",", ":"), ensure_ascii=False)
        self._each(lambda ws: ws.send(text if mode == "text" else text.encode()))

    def _receive(self):
        from starlette.websockets import WebSocketDisconnect
        from websockets.exceptions import ConnectionClosed

        res = {}
        for which, ws in self.ws.items():
            try:
                m = ws.recv(timeout=5)
                res[which] = {"bytes": m.hex()} if isinstance(m, bytes) else {"text": m}
            except ConnectionClosed:
                res[which] = {"closed": [ws.close_code, ws.close_reason]}
            except TimeoutError:
                res[which] = {"timeout": True}
        self._record("WS receive", res)
        got = res.get("cand", {})
        if "closed" in got:
            raise WebSocketDisconnect(got["closed"][0] or 1000, got["closed"][1] or None)
        if "timeout" in got:
            raise TimeoutError("binary: nothing received")
        return got

    def receive_text(self) -> str:
        got = self._receive()
        return got["text"] if "text" in got else bytes.fromhex(got["bytes"]).decode()

    def receive_bytes(self) -> bytes:
        got = self._receive()
        return bytes.fromhex(got["bytes"]) if "bytes" in got else got["text"].encode()

    def receive_json(self, mode: str = "text"):
        got = self._receive()
        return json.loads(got["text"] if "text" in got else bytes.fromhex(got["bytes"]))

    def receive(self):
        got = self._receive()
        return {"type": "websocket.send", **({"text": got["text"]} if "text" in got else {"bytes": bytes.fromhex(got["bytes"])})}

    def close(self, code: int = 1000, reason: str | None = None) -> None:
        self._each(lambda ws: ws.close(code, reason or ""))


def _patch() -> None:
    import fastapi.testclient
    import starlette.testclient

    fastapi.testclient.TestClient = ProxyClient
    starlette.testclient.TestClient = ProxyClient


_patch()


def pytest_runtest_logreport(report) -> None:
    if report.when == "call" or (report.when == "setup" and report.outcome != "passed"):
        emit(ev="test", test=report.nodeid, when=report.when, outcome=report.outcome,
             skip=str(report.longrepr[-1]) if report.skipped and isinstance(report.longrepr, tuple) else None,
             error=report.longreprtext[-1500:] if report.failed else None)


"""Differential conformance test: the same request sequence against a reference server (FastAPI) and
a candidate (the generated axum server), on a freshly reset database each time. Status, content type,
content-encoding and JSON bodies must match byte for byte, key order included.

usage: python tests/conformance.py REF_URL CAND_URL [--scenario app] [--ignore-encoding]

Scenarios live in tests/scenarios/<name>.py and define:
  STEPS       list of (method, path, payload[, headers]); payload = None, bytes or a JSON value;
              method "WS": a WebSocket connection, payload = its script (see ws_step)
  reset(db)   bring the database (DATABASE_URL) back to the scenario's initial state
  normalize   optional: body -> body, for differences proven not to be semantic
  normalize_text  optional: text -> text, the same for bodies that are not JSON

Generated instants (`created_at`, ...) cannot match between two runs: every ISO-8601 datetime the
client did not send is replaced by its *shape* (digits -> 9), so the format (Z vs offset, fraction
length) is still compared, only the value is not.

--ignore-encoding: forced-streaming pass. A streamed response has no content-length, so axum
compresses it even under GZipMiddleware's minimum_size; everything else must still match.
"""
from __future__ import annotations

import importlib
import json
import os
import re
import sys
import time
from pathlib import Path

import httpx

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

DB = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/poc")
J = {"content-type": "application/json"}
DATETIME = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d{1,6})?)?(?:Z|[+-]\d{2}:\d{2})?")


def mask_datetimes(value, sent: set[str]):
    """Replace generated instants by their shape, keep the ones the client sent verbatim."""
    if isinstance(value, str):
        return DATETIME.sub(lambda m: m.group(0) if m.group(0) in sent else
                            "<dt " + re.sub(r"\d", "9", m.group(0)) + ">", value)
    if isinstance(value, list):
        return [mask_datetimes(v, sent) for v in value]
    if isinstance(value, dict):
        return {k: mask_datetimes(v, sent) for k, v in value.items()}
    return value


def sent_datetimes(steps) -> set[str]:
    found: set[str] = set()
    for step in steps:
        payload = step[2]
        text = payload.decode(errors="replace") if isinstance(payload, bytes) else json.dumps(payload, default=repr)
        found.update(DATETIME.findall(text + " " + step[1]))
    return found


MW_HEADER_PREFIXES = ("access-control-", "x-", "vary", "content-security-policy", "strict-transport-security",
                      "referrer-policy", "permissions-policy", "retry-after")


def mask_cookie(scenario, cookie: str) -> str:
    for pat, repl in getattr(scenario, "COOKIE_MASKS", []):
        cookie = re.sub(pat, repl, cookie)
    return cookie


def ws_step(base: str, step) -> dict:
    """A WebSocket connection played from a script: ("send", str | bytes), ("recv", n), ("close"[, code,
    reason]), ("sleep", s). Compared: the handshake status (with the body and content type of a refusal),
    the accepted subprotocol, the `x-` headers, the messages received, the close code and reason received
    (1006: the connection dropped without a close frame). `compression=None`: the binary does not
    implement permessage-deflate (docs/supported.md), the client asks neither server for it."""
    from websockets.exceptions import ConnectionClosed, InvalidStatus
    from websockets.sync.client import connect

    path, script = step[1], step[2]
    headers = dict(step[3]) if len(step) > 3 else {}
    subprotocols = headers.pop("subprotocols", None)
    out: dict = {"req": f"WS {path}"}
    got: list = []
    try:
        with connect(base.replace("http://", "ws://", 1) + path, additional_headers=headers, subprotocols=subprotocols,
                     compression=None, open_timeout=5, close_timeout=3) as ws:
            out.update(status=101, subprotocol=ws.subprotocol,
                       headers={k.lower(): v for k, v in ws.response.headers.raw_items() if k.lower().startswith("x-")})
            try:
                for act in script:
                    if act[0] == "send":
                        ws.send(act[1])
                    elif act[0] == "recv":
                        for _ in range(act[1]):
                            m = ws.recv(timeout=3)
                            got.append(["bytes", m.hex()] if isinstance(m, bytes) else ["text", m])
                    elif act[0] == "close":
                        ws.close(*act[1:])
                    elif act[0] == "sleep":
                        time.sleep(act[1])
            except ConnectionClosed:
                pass
            except TimeoutError:
                got.append(["timeout"])
    except InvalidStatus as e:
        r = e.response
        out.update(status=r.status_code, ctype=r.headers.get("content-type"), body=r.body.decode(errors="replace"),
                   headers={k.lower(): v for k, v in r.headers.raw_items() if k.lower().startswith("x-")})
        return out
    out.update(msgs=got, closed=[ws.close_code, ws.close_reason])
    time.sleep(0.15)  # what the server does after the connection (its log) lands before the next step
    return out


def mask_file(scenario, value: str | None) -> str | None:
    """FILE_MASKS: (pattern, replacement) on the file headers (a timestamp in a download's file name...)."""
    for pat, repl in getattr(scenario, "FILE_MASKS", []) if value else []:
        value = re.sub(pat, repl, value)
    return value


def exchange(c: httpx.Client, base: str, scenario, step, sent: set[str], db: str = DB, on_response=None) -> dict | None:
    """Play one step on one server and return what is compared (None for a SQL fixture step, run on `db`).
    `on_response(r)` sees the raw response first (tests/difftest.py learns the credentials it hands out)."""
    method, path, payload = step[:3]
    if method == "WS":
        return ws_step(base, step)
    if method == "SQL":
        # a fixture the HTTP API cannot create (a paid plan, a tenant's password...): the same
        # statement on the shared database before the next request, nothing compared
        import psycopg

        with psycopg.connect(db.replace("postgresql+psycopg://", "postgresql://")) as conn:
            conn.execute(path)
        return None
    normalize = getattr(scenario, "normalize", lambda body: body)
    headers = dict(step[3]) if len(step) > 3 else {}
    if isinstance(payload, bytes):
        kw = {"content": payload, "headers": {**J, **headers}}
    elif payload is None:
        kw = {"headers": headers}
    else:
        kw = {"content": json.dumps(payload), "headers": {**J, **headers}}
    try:
        r = c.request(method, path, **kw)
    except (httpx.ReadError, httpx.RemoteProtocolError):
        # uvicorn closes a keep-alive connection after an unhandled 500: the request was
        # sent on a dead connection and never reached the server, send it again
        try:
            r = c.request(method, path, **kw)
        except (httpx.ReadError, httpx.RemoteProtocolError) as e:
            # the server drops this very request without answering (a status uvicorn has no line for)
            return {"req": f"{method} {path}", "error": type(e).__name__}
    if on_response is not None:
        on_response(r)
    ctype = r.headers.get("content-type", "").split(";")[0]
    try:
        body = (normalize(r.json(), path) if normalize.__code__.co_argcount == 2 else normalize(r.json())) if r.content else None
    except ValueError:
        body = getattr(scenario, "normalize_text", lambda text: text)(r.text)
    if method not in ("GET", "HEAD") and getattr(scenario, "SETTLE", 0):
        # FastAPI commits a `yield` session dependency after sending the response: let it land
        # before the next request reads (the reference is racy otherwise, the binary is not)
        time.sleep(scenario.SETTLE)
    return {"req": f"{method} {path}", "status": r.status_code, "ctype": ctype,
            "encoding": r.headers.get("content-encoding"), "allow": r.headers.get("allow"),
            # a redirect to the server itself: its own address differs between the two
            "location": (r.headers.get("location") or "").replace(base, "<base>") or None,
            "www_authenticate": r.headers.get("www-authenticate"),
            "cookies": [mask_cookie(scenario, re.sub(r"expires=[^;]+", "expires=<date>", c)) for c in r.headers.get_list("set-cookie")],
            "file": [mask_file(scenario, r.headers.get(h)) for h in ("content-disposition", "etag", "last-modified", "accept-ranges")],
            # middlewares: CORS, security headers, rate limiting...
            "mw": {k: "<masked>" if k in getattr(scenario, "HEADER_MASKS", ()) else ", ".join(r.headers.get_list(k))
                   for k in sorted(set(r.headers.keys())) if k.startswith(MW_HEADER_PREFIXES)},
            "body": mask_datetimes(body, sent)}


def client(base: str) -> httpx.Client:
    return httpx.Client(base_url=base, timeout=10, headers={"accept-encoding": "gzip"})


def run(base: str, scenario) -> list[dict]:
    scenario.reset(DB)
    sent = sent_datetimes(scenario.STEPS)
    out = []
    with client(base) as c:
        for step in scenario.STEPS:
            got = exchange(c, base, scenario, step, sent)
            if got is not None:
                out.append(got)
    return out


def no_encoding(r: dict) -> dict:
    """--ignore-encoding: the content coding and its `Vary: Accept-Encoding` are not compared."""
    mw = dict(r.get("mw", {}))
    if "vary" in mw:
        rest = [t for t in (x.strip() for x in mw["vary"].split(",")) if t.lower() != "accept-encoding"]
        if rest:
            mw["vary"] = ", ".join(rest)
        else:
            del mw["vary"]
    return dict(r, encoding=None, mw=mw)


def identical(a: dict, b: dict, ignore_encoding: bool = False) -> bool:
    ka, kb = (no_encoding(a), no_encoding(b)) if ignore_encoding else (a, b)
    return json.dumps(ka, ensure_ascii=False) == json.dumps(kb, ensure_ascii=False)  # key order too


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    flags = [a for a in sys.argv[1:] if a.startswith("--")]
    name = next((a.split("=", 1)[1] for a in flags if a.startswith("--scenario=")), None)
    if name is None and "--scenario" in sys.argv:
        name = sys.argv[sys.argv.index("--scenario") + 1]
        args.remove(name)
    scenario = importlib.import_module(f"tests.scenarios.{name or 'app'}")
    ignore_encoding = "--ignore-encoding" in flags
    ref_url, cand_url = args[0], args[1]
    ref, cand = run(ref_url, scenario), run(cand_url, scenario)
    failures = 0
    for a, b in zip(ref, cand):
        same = identical(a, b, ignore_encoding)
        failures += not same
        mark = "ok  " if same else "DIFF"
        print(f"{mark} {a.get('status', '---'):>3} {a['req']}")
        if not same:
            print(f"     ref:  {json.dumps(a, ensure_ascii=False)}")
            print(f"     cand: {json.dumps(b, ensure_ascii=False)}")
    print(f"\n{len(ref) - failures}/{len(ref)} identical responses")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())

"""Differential conformance test: the same request sequence against a reference server (FastAPI) and
a candidate (the generated axum server), on a freshly reset database each time. Status, content type,
content-encoding and JSON bodies must match byte for byte, key order included.

usage: python tests/conformance.py REF_URL CAND_URL [--scenario app] [--ignore-encoding]

Scenarios live in tests/scenarios/<name>.py and define:
  STEPS       list of (method, path, payload[, headers]); payload = None, bytes or a JSON value
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
        text = payload.decode(errors="replace") if isinstance(payload, bytes) else json.dumps(payload)
        found.update(DATETIME.findall(text + " " + step[1]))
    return found


MW_HEADER_PREFIXES = ("access-control-", "x-", "vary", "content-security-policy", "strict-transport-security",
                      "referrer-policy", "permissions-policy", "retry-after")


def mask_cookie(scenario, cookie: str) -> str:
    for pat, repl in getattr(scenario, "COOKIE_MASKS", []):
        cookie = re.sub(pat, repl, cookie)
    return cookie


def run(base: str, scenario) -> list[dict]:
    scenario.reset(DB)
    sent = sent_datetimes(scenario.STEPS)
    normalize = getattr(scenario, "normalize", lambda body: body)
    out = []
    with httpx.Client(base_url=base, timeout=10, headers={"accept-encoding": "gzip"}) as c:
        for step in scenario.STEPS:
            method, path, payload = step[:3]
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
                r = c.request(method, path, **kw)
            ctype = r.headers.get("content-type", "").split(";")[0]
            try:
                body = (normalize(r.json(), path) if normalize.__code__.co_argcount == 2 else normalize(r.json())) if r.content else None
            except ValueError:
                body = getattr(scenario, "normalize_text", lambda text: text)(r.text)
            if method not in ("GET", "HEAD") and getattr(scenario, "SETTLE", 0):
                # FastAPI commits a `yield` session dependency after sending the response: let it land
                # before the next request reads (the reference is racy otherwise, the binary is not)
                time.sleep(scenario.SETTLE)
            out.append({"req": f"{method} {path}", "status": r.status_code, "ctype": ctype,
                        "encoding": r.headers.get("content-encoding"), "allow": r.headers.get("allow"),
                        # a redirect to the server itself: its own address differs between the two
                        "location": (r.headers.get("location") or "").replace(base, "<base>") or None,
                        "www_authenticate": r.headers.get("www-authenticate"),
                        "cookies": [mask_cookie(scenario, re.sub(r"expires=[^;]+", "expires=<date>", c)) for c in r.headers.get_list("set-cookie")],
                        "file": [r.headers.get(h) for h in ("content-disposition", "etag", "last-modified", "accept-ranges")],
                        # middlewares: CORS, security headers, rate limiting...
                        "mw": {k: "<masked>" if k in getattr(scenario, "HEADER_MASKS", ()) else ", ".join(r.headers.get_list(k))
                               for k in sorted(set(r.headers.keys())) if k.startswith(MW_HEADER_PREFIXES)},
                        "body": mask_datetimes(body, sent)})
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
        ka, kb = (no_encoding(a), no_encoding(b)) if ignore_encoding else (a, b)
        same = json.dumps(ka, ensure_ascii=False) == json.dumps(kb, ensure_ascii=False)  # key order too
        failures += not same
        mark = "ok  " if same else "DIFF"
        print(f"{mark} {a['status']:>3} {a['req']}")
        if not same:
            print(f"     ref:  {json.dumps(a, ensure_ascii=False)}")
            print(f"     cand: {json.dumps(b, ensure_ascii=False)}")
    print(f"\n{len(ref) - failures}/{len(ref)} identical responses")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())

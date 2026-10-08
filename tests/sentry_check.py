"""What Sentry receives from FastAPI and from the binary for the same requests (fixtures/sentryapp).

Both servers report to the fake Sentry server (tests/sentry_sink.py), the reference as project 1 and the
binary as project 2 (scripts_start_sentry.sh starts all three). Each request goes to both servers, then the
items each project received are compared once normalized: count and kind (event / transaction), level,
message or log entry, logger, tags, user, contexts, extra, breadcrumbs, exception type/value/module/
mechanism, request data, transaction name, status and tags, `_meta`.

Not compared (the platform, documented in docs/supported.md): ids and timestamps, `sdk`, `modules`,
`sys.argv`, the `runtime` context, thread data, stack frames (the binary sends one frame: the route
handler). The binary's items must carry the tag `py2axum.source`; in both, the events of a request share
the trace id of its transaction.

    python tests/sentry_check.py http://127.0.0.1:8710 http://127.0.0.1:8790 [--sink http://127.0.0.1:8799]
"""
from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.request

BIG = "a" * 12000

# (method, path, headers, body or None, content-type)
REQUESTS = [
    ("GET", "/health", {}, None, None),
    ("GET", "/msg?text=a&level=warning", {}, None, None),
    ("GET", "/msg?text=%C3%A9t%C3%A9+x&level=fatal", {}, None, None),
    ("GET", "/msg-default", {}, None, None),
    ("GET", "/exc", {}, None, None),
    ("GET", "/exc-implicit", {}, None, None),
    ("GET", "/user/5?email=a@atom.fr", {}, None, None),
    ("GET", "/leak", {}, None, None),
    ("GET", "/after-leak", {}, None, None),
    ("GET", "/boom?user_id=4", {"Authorization": "Bearer xyz", "Cookie": "sid=1; b=2", "X-Forwarded-For": "1.2.3.4",
                                "X-Api-Key": "k", "X-Custom": "c"}, None, None),
    ("GET", "/boom-sync", {}, None, None),
    ("GET", "/http/500", {}, None, None),
    ("GET", "/http/503", {}, None, None),
    ("GET", "/http/404", {}, None, None),
    ("GET", "/log", {}, None, None),
    ("GET", "/log?what=exception", {}, None, None),
    ("GET", "/log?what=root", {}, None, None),
    ("GET", "/log?what=critical", {}, None, None),
    ("GET", "/scope?source=x", {}, None, None),
    ("GET", "/push-scope", {}, None, None),
    ("GET", "/filtered", {}, None, None),
    ("GET", "/task?n=2", {}, None, None),
    ("GET", "/task?n=0", {}, None, None),
    ("GET", "/notrace/boom", {}, None, None),
    ("GET", "/stats", {"sentry-trace": "0123456789abcdef0123456789abcdef-0123456789abcdef-1"}, None, None),
    ("GET", "/stats", {"sentry-trace": "0123456789abcdef0123456789abcdef-0123456789abcdef-0"}, None, None),
    ("HEAD", "/stats", {}, None, None),
    ("GET", "/bare-capture", {}, None, None),
    ("GET", "/nope", {}, None, None),
    ("GET", "/health/", {}, None, None),
    ("POST", "/msg", {}, None, None),
    ("POST", "/items", {}, json.dumps({"name": "x", "price": -1}), "application/json"),
    ("POST", "/items", {}, json.dumps({"name": BIG, "price": -1}), "application/json"),
    ("POST", "/items", {}, "raw text", "text/plain"),
    ("POST", "/items", {}, json.dumps({"name": "ok", "price": 2}), "application/json"),
    ("POST", "/form", {}, "a=1&b=2&a=3&token=s", "application/x-www-form-urlencoded"),
    ("GET", "/extras", {}, None, None),
    ("GET", "/handled-custom", {}, None, None),
    ("GET", "/mw-boom", {}, None, None),
    ("GET", "/dedupe", {}, None, None),
    ("GET", "/capture-kwargs", {}, None, None),
]


def fetch(url: str, method: str = "GET", data: bytes | None = None, headers: dict | None = None):
    req = urllib.request.Request(url, data=data, method=method, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def items(sink: str, project: int) -> list:
    """The events and transactions a project received. Not compared: the SDK's own telemetry the binary does
    not send (client reports, release-health session aggregates flushed every minute)."""
    return [i for i in json.loads(fetch(f"{sink}/_items/{project}")[1]) if i["type"] not in ("client_report", "sessions")]


def settle(sink: str, projects: tuple[int, ...], before: dict[int, int]) -> dict[int, list]:
    """Waits until both projects stop receiving (the SDKs send from background threads)."""
    last, stable = None, 0
    deadline = time.time() + 8
    while time.time() < deadline:
        cur = {p: items(sink, p) for p in projects}
        sig = tuple(len(cur[p]) for p in projects)
        if sig == last:
            stable += 1
            if stable >= 4:
                break
        else:
            stable = 0
        last = sig
        time.sleep(0.08)
    return {p: cur[p][before[p]:] for p in projects}


DROP_EXTRA = {"sys.argv"}


def norm(it: dict, host: str) -> dict:
    p = json.loads(json.dumps(it["payload"]).replace(host, "HOST"))
    out = {"item": it["type"]}
    for k in ("level", "message", "logger", "transaction", "transaction_info", "user", "environment", "release",
              "server_name", "platform", "fingerprint", "dist", "measurements"):
        if k in p:
            out[k] = p[k]
    if "logentry" in p:
        le = dict(p["logentry"])
        if p.get("logger") == "asyncio":  # the task's repr: name, coroutine and file differ
            for k in ("message", "formatted"):
                le[k] = le[k].split("\n")[0]
        out["logentry"] = le
    tags = dict(p.get("tags") or {})
    tags.pop("py2axum.source", None)
    out["tags"] = tags
    ctx = dict(p.get("contexts") or {})
    ctx.pop("runtime", None)
    tr = ctx.pop("trace", None) or {}
    out["trace"] = {k: tr.get(k) for k in ("op", "status", "origin", "description") if k in tr}
    if "data" in tr:
        out["trace"]["status_code"] = tr["data"].get("http.response.status_code")
    out["trace"]["parent"] = tr.get("parent_span_id")
    out["contexts"] = ctx
    extra = {k: v for k, v in (p.get("extra") or {}).items() if k not in DROP_EXTRA}
    out["extra"] = extra
    if "breadcrumbs" in p:
        out["breadcrumbs"] = [{k: v for k, v in b.items() if k != "timestamp"} for b in p["breadcrumbs"].get("values", [])]
    if "exception" in p:
        out["exception"] = [{k: v for k, v in e.items() if k != "stacktrace"} for e in p["exception"]["values"]]
    if "request" in p:
        out["request"] = p["request"]
    meta = dict(p.get("_meta") or {})
    if "exception" in meta:  # Python's frame variables are annotated too
        ex = {i: {k: v for k, v in e.items() if k != "stacktrace"} for i, e in meta["exception"].get("values", {}).items()}
        ex = {i: e for i, e in ex.items() if e}
        meta["exception"] = {"values": ex} if ex else None
        if meta["exception"] is None:
            del meta["exception"]
    if meta:
        out["_meta"] = meta
    if it["type"] == "transaction":
        out["spans"] = len(p.get("spans", []))
    return out


def key(n: dict) -> str:
    return json.dumps([n["item"], n.get("transaction"), n.get("message"), (n.get("logentry") or {}).get("message"),
                       [e.get("type") for e in n.get("exception", [])]], sort_keys=True)


def check_trace(raw: list) -> list[str]:
    """Events of a request share the trace id of its transaction."""
    txs = [i["payload"]["contexts"]["trace"]["trace_id"] for i in raw if i["type"] == "transaction"]
    if not txs:
        return []
    evs = [i["payload"]["contexts"]["trace"]["trace_id"] for i in raw if i["type"] == "event"]
    return [f"event trace_id {t} != transaction {txs[0]}" for t in evs if t != txs[0]]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("ref")
    ap.add_argument("cand")
    ap.add_argument("--sink", default="http://127.0.0.1:8799")
    ap.add_argument("--show", action="store_true", help="print the normalized items")
    a = ap.parse_args()
    projects = (1, 2)
    fails = 0
    total = 0
    for method, path, headers, body, ctype in REQUESTS:
        before = {p: len(items(a.sink, p)) for p in projects}
        h = dict(headers)
        if ctype:
            h["Content-Type"] = ctype
        data = body.encode() if body is not None else None
        st1, _ = fetch(a.ref + path, method, data, h)
        st2, _ = fetch(a.cand + path, method, data, h)
        got = settle(a.sink, projects, before)
        ref = sorted((norm(i, a.ref.split("//")[1]) for i in got[1]), key=key)
        cand = sorted((norm(i, a.cand.split("//")[1]) for i in got[2]), key=key)
        problems = []
        if st1 != st2:
            problems.append(f"HTTP status {st1} != {st2}")
        problems += [f"python: {m}" for m in check_trace(got[1])]
        problems += [f"binary: {m}" for m in check_trace(got[2])]
        for i in got[2]:
            if "py2axum.source" not in (i["payload"].get("tags") or {}) and i["payload"].get("transaction_info", {}).get("source") != "url":
                problems.append(f"binary {i['type']} without py2axum.source tag")
        if len(ref) != len(cand):
            problems.append(f"{len(ref)} items from Python, {len(cand)} from the binary: "
                            f"{[key(n) for n in ref]} vs {[key(n) for n in cand]}")
        else:
            for r, c in zip(ref, cand):
                for k in sorted(set(r) | set(c)):
                    if r.get(k) != c.get(k) and not (k == "extra" and not r.get(k) and not c.get(k)):
                        problems.append(f"{r['item']} {k}: {json.dumps(r.get(k), ensure_ascii=False)} != {json.dumps(c.get(k), ensure_ascii=False)}")
        total += 1
        label = f"{method} {path[:60]}"
        if a.show:
            for n in ref:
                print("   py ", json.dumps(n, ensure_ascii=False)[:400])
            for n in cand:
                print("   rs ", json.dumps(n, ensure_ascii=False)[:400])
        if problems:
            fails += 1
            print(f"DIFF {label} ({len(ref)} items)")
            for pb in problems:
                print("    " + pb[:700])
        else:
            print(f"ok   {label} ({len(ref)} items)")
    print(f"{total - fails}/{total} requests: same items in Sentry")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())

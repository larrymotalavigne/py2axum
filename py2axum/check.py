"""`py2axum check`: what would translate, route by route, without generating a crate.

    py2axum check <package> [--root DIR] [--python-side PATH|auto|lifespan] [--format text|json|markdown]
                  [--fail-under PCT]
    py2axum check --explain P2A0201

Each route is *native* (compiled into the binary), *python-side* (declared with --python-side, or moved by
--python-side auto) or *refused* (generation would stop on it), with the reason at file:line. Errors that
concern the whole application (and versions outside the tested ranges) are listed apart: they refuse the
generation whatever the routes. Every refusal carries its stable error code (errors.py) and a suggestion; the
summary orders the constructs by how many routes fixing them would make native. Built on the coverage report's collect mode (report.py), so `check`,
`--report` and `--python-side auto` agree.

Exit status: 0 when generation would succeed (and native routes ≥ --fail-under), 1 otherwise.
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

from . import errors, versions
from .report import RouteReport, _where, aggregate, classify


_DECLARED = re.compile(r"^(?P<where>.+?:\d+): (?P<method>[A-Z]+) (?P<path>\S+) declared Python-side$")
_MOUNT = re.compile(r"^(?P<where>.+?:\d+): app\.mount\(\.\.\.\) stays on the Python side")
_DECLARED_RAW = re.compile(r"^(?P<where>.+?:\d+): .* — (?P<path>\S+) declared Python-side$")


# libraries without a runtime equivalent, and what does the same job natively
ALTERNATIVES = {
    "requests": "httpx.AsyncClient or aiohttp.ClientSession (both native)",
    "urllib3": "httpx.AsyncClient or aiohttp.ClientSession (both native)",
    "passlib": "bcrypt or pwdlib (both native, same hashes)",
    "orjson": "json (native; same output with separators=(',', ':'))",
    "ujson": "json (native)",
    "simplejson": "json (native)",
    "aioredis": "redis.asyncio (native)",
    "loguru": "logging (native)",
    "structlog": "logging (native)",
    "pendulum": "datetime and dateutil.relativedelta (native)",
    "arrow": "datetime and dateutil.relativedelta (native)",
    "pytz": "zoneinfo (native)",
    "jwcrypto": "PyJWT or python-jose with an HMAC algorithm (native)",
    "authlib": "PyJWT or python-jose with an HMAC algorithm (native)",
    "smtplib": "aiosmtplib (native)",
}


def suggestion(e, path: str | None) -> str:
    """What to do about a refusal: the error class's advice, specialised when py2axum knows better (a native
    library for the same job), and, for a route, the --python-side flag that leaves it to Python."""
    c = errors.BY_CODE[e.code]
    fix = c.fix.replace(" " + errors.PY_SIDE, "")
    m = re.search(r"library (?:call|value|function) `([a-zA-Z0-9_]+)", e.msg)
    if m and m[1] in ALTERNATIVES:
        fix = f"Use {ALTERNATIVES[m[1]]} instead of {m[1]}."
    if path and not c.whole_app:
        fix += f" Or leave the route to Python: --python-side '{path}'."
    return fix


def _global(result: dict, e) -> None:
    result["global"].append({"construction": classify(e, english=True)[0], "error": e.render(), "where": _where(e),
                             "code": e.code, "suggestion": suggestion(e, None)})


def run(package: Path, root: Path | None, python_side: list[str], backend: str = "auto",
        allow_untested: bool = False) -> dict:
    side = set(python_side)
    auto = "auto" in side
    side.discard("auto")
    result = {"package": str(package), "backend": "dyn", "python_side_auto": auto, "routes": [], "global": [],
              "notes": []}
    codes: dict[str, str] = {}  # construction label -> error code
    if not allow_untested:
        c = errors.BY_CODE["P2A0601"]
        for msg, file, line in versions.check_project(root or package.resolve().parent, package):
            result["global"].append({"construction": "library version", "error": f"{file}:{line}: {msg}",
                                     "where": f"{file}:{line}", "code": c.code, "suggestion": c.fix})
    from .ir import TranspileError
    from .report import collect_dyn

    try:
        fe, per_route = collect_dyn(package, root, side, auto=auto)
    except TranspileError as e:
        # an error found while discovering the application (before any route is compiled): it refuses
        # the whole application, at its file:line, as the generation does
        _global(result, e)
        return _summarise(result, codes)
    for e in fe.global_errors:
        _global(result, e)
    moving = auto and not fe.global_errors
    for info, errs in per_route:
        rr = RouteReport(info["method"].upper(), info["path"], info["func"], f"{info['file']}:{info['line']}",
                         info["conditional"], "native")
        rr.code = rr.suggestion = None
        for e in errs:
            label = classify(e, english=True)[0]
            codes.setdefault(label, e.code)
            rr.blockers.setdefault(label, [])
            if _where(e) not in rr.blockers[label]:
                rr.blockers[label].append(_where(e))
        if errs:
            rr.status = "python-side" if moving else "refused"
            rr.error = errs[0].render()
            rr.code, rr.suggestion = errs[0].code, suggestion(errs[0], info["path"])
        result["routes"].append(rr)
    for path, e in getattr(fe, "auto_side", {}).items():
        rr = RouteReport("RAW", path, "?", _where(e), False, "python-side" if moving else "refused", e.render())
        rr.code, rr.suggestion = e.code, suggestion(e, path)
        label = classify(e, english=True)[0]
        codes.setdefault(label, e.code)
        rr.blockers[label] = [_where(e)]
        result["routes"].append(rr)
    for n in fe.notes:
        mnt = _MOUNT.match(n)
        if mnt and "mount" not in side and not auto:
            # collect mode keeps a final app.mount() Python-side; generation refuses it unless asked to
            c = errors.BY_CODE["P2A0310"]
            result["global"].append({"construction": "app.mount", "where": mnt["where"], "code": c.code,
                                     "suggestion": c.fix,
                                     "error": f"{mnt['where']}: app.mount(...) is not translated: as the app's last "
                                              "registration it can stay on the Python side (--python-side mount, or "
                                              "--python-side auto)"})
            continue
        m = _DECLARED.match(n) or _DECLARED_RAW.match(n)
        if m:
            rr = RouteReport(m.groupdict().get("method") or "RAW", m["path"], "?", m["where"], False, "python-side",
                             "declared --python-side")
            rr.code = rr.suggestion = None
            result["routes"].append(rr)
        else:
            result["notes"].append(n)
    result["routes"].sort(key=lambda r: (r.path, r.method))
    return _summarise(result, codes)


def _summarise(result: dict, codes: dict[str, str]) -> dict:
    routes = result["routes"]
    count = {s: sum(r.status == s for r in routes) for s in ("native", "python-side", "refused")}
    total = len(routes)
    result["summary"] = {"total": total, **count,
                         "native_pct": round(100 * count["native"] / total, 1) if total else 100.0,
                         "generates": not result["global"] and not count["refused"]}
    # blockers of the routes that are not native, most decisive first (the report's greedy order)
    agg = aggregate([RouteReport(r.method, r.path, r.func, r.where, r.conditional,
                                 "traduite" if r.status == "native" or not r.blockers else "bloquée", r.error,
                                 r.blockers) for r in routes])

    def advice(label: str) -> str:
        c = errors.BY_CODE.get(codes.get(label, ""), errors.OTHER)
        return c.fix.replace(" " + errors.PY_SIDE, "")

    result["blockers"] = [{"construction": c["construction"], "routes": c["routes_touchées"],
                           "only_blocker": c["débloquées_seule"], "code": codes.get(c["construction"]),
                           "where": sorted({w for r in routes for w in r.blockers.get(c["construction"], [])})}
                          for c in agg["constructions"]]
    # what would make the most routes native: support (or rewrite) these constructs in this order
    result["unblock"] = [{"construction": st["construction"], "code": codes.get(st["construction"]),
                          "new_native": st["nouvelles"], "native_after": st["cumul"],
                          "suggestion": advice(st["construction"])}
                         for st in agg["glouton"] if st["nouvelles"]]
    return result


def to_json(result: dict) -> str:
    data = dict(result)
    data["routes"] = [{"method": r.method, "path": r.path, "handler": r.func, "where": r.where,
                       "status": r.status, "reason": r.error, "code": getattr(r, "code", None),
                       "suggestion": getattr(r, "suggestion", None), "blockers": r.blockers}
                      for r in result["routes"]]
    return json.dumps(data, ensure_ascii=False, indent=1)


def _verdict(s: dict) -> str:
    return (f"{s['native']}/{s['total']} routes native ({s['native_pct']} %), {s['python-side']} python-side, "
            f"{s['refused']} refused — ")


def to_text(result: dict, color: bool = False) -> str:
    def c(code: str, s: str) -> str:
        return f"\x1b[{code}m{s}\x1b[0m" if color else s

    mark = {"native": c("32", "native     "), "python-side": c("33", "python-side"), "refused": c("31", "refused    ")}
    s = result["summary"]
    o = [f"py2axum check {result['package']}", ""]
    if result["global"]:
        o.append(c("31", "Whole application — generation refused whatever the routes:"))
        for g in result["global"]:
            o.append(f"  [{g['code']}] {g['error']}")
            o.append(f"           help: {g['suggestion']}")
        o.append("")
    width = max((len(r.method) + 1 + len(r.path) for r in result["routes"]), default=0)
    for r in result["routes"]:
        o.append(f"{mark[r.status]}  {(r.method + ' ' + r.path).ljust(width)}  {r.where}")
        if r.status != "native" and r.error:
            code = getattr(r, "code", None)
            o.append(f"             └ {r.error}" + (f" [{code}]" if code else ""))
            if getattr(r, "suggestion", None) and r.status == "refused":
                o.append(f"               help: {r.suggestion}")
    if result["unblock"]:
        o += ["", "What would make the most routes native (in this order):"]
        for u in result["unblock"]:
            o.append(f"  +{u['new_native']:<3} → {u['native_after']:>4}/{s['total']}  {u['construction']}"
                     + (f" [{u['code']}]" if u["code"] else ""))
    if result["blockers"]:
        o += ["", "Blockers (routes touched, routes for which it is the only blocker):"]
        for b in result["blockers"]:
            where = ", ".join(b["where"][:3]) + (f", +{len(b['where']) - 3}" if len(b["where"]) > 3 else "")
            o.append(f"  {b['routes']:>4} {b['only_blocker']:>4}  {b['construction']}  ({where})")
    if result["notes"]:
        o += ["", "Notes:"] + [f"  {n}" for n in result["notes"]]
    o += ["", _verdict(s) + ("generation would succeed" if s["generates"] else c("31", "generation would fail"))]
    codes = sorted({r.code for r in result["routes"] if getattr(r, "code", None) and r.status == "refused"}
                   | {g["code"] for g in result["global"]})
    if codes:
        o.append(f"explain a code: py2axum check --explain {codes[0]}")
    if not s["generates"] and s["refused"] and not result["global"] and not result["python_side_auto"]:
        o.append("hint: --python-side auto leaves the refused routes to a Python process next to the binary")
    return "\n".join(o)


def _md(text: str) -> str:
    return text.replace("|", "\\|")


def to_markdown(result: dict) -> str:
    """For a pull request comment or a CI job summary."""
    s = result["summary"]
    link = errors.DOCS + "/reference/errors/#"
    o = [f"## py2axum check: `{result['package']}`", "",
         f"**{_verdict(s)}{'generation would succeed' if s['generates'] else 'generation would fail'}**", ""]
    if result["global"]:
        o += ["### Whole application", "", "Generation is refused whatever the routes:", ""]
        for g in result["global"]:
            o.append(f"- [{g['code']}]({link}{g['code'].lower()}) `{g['where']}`: {_md(g['error'])}  ")
            o.append(f"  *{_md(g['suggestion'])}*")
        o.append("")
    o += ["### Routes", "", "| Status | Route | Where | Reason | Suggestion |", "|---|---|---|---|---|"]
    icon = {"native": "✅ native", "python-side": "🐍 python-side", "refused": "❌ refused"}
    for r in result["routes"]:
        code = getattr(r, "code", None)
        reason = (f"[{code}]({link}{code.lower()}) " if code else "") + _md(r.error or "")
        sugg = _md(getattr(r, "suggestion", None) or "") if r.status == "refused" else ""
        o.append(f"| {icon[r.status]} | `{r.method} {r.path}` | `{r.where}` | {reason} | {sugg} |")
    if result["unblock"]:
        o += ["", "### What would make the most routes native", "",
              "| + routes | Native after | Construct | What to do |", "|---|---|---|---|"]
        for u in result["unblock"]:
            code = f" [{u['code']}]({link}{u['code'].lower()})" if u["code"] else ""
            o.append(f"| +{u['new_native']} | {u['native_after']}/{s['total']} | {_md(u['construction'])}{code} | "
                     f"{_md(u['suggestion'])} |")
    if result["notes"]:
        o += ["", "### Notes", ""] + [f"- {_md(n)}" for n in result["notes"]]
    return "\n".join(o) + "\n"


def _relative(text: str) -> str:
    """Paths under the current directory shown relative to it (the translation works on resolved paths)."""
    return text.replace(str(Path.cwd()) + "/", "")


def main(argv: list[str]) -> int:
    import argparse

    ap = argparse.ArgumentParser(prog="py2axum check", description=__doc__.splitlines()[0])
    ap.add_argument("package", type=Path, nargs="?", help="directory of the FastAPI application package")
    ap.add_argument("--root", type=Path, default=None, help="import root, like sys.path (default: parent of the package)")
    ap.add_argument("--backend", choices=["auto", "dyn"], default="auto", help="deprecated, no effect (one backend since 0.4)")
    ap.add_argument("--python-side", action="append", default=[], metavar="PATH|lifespan|auto",
                    help="as for generation: routes or the lifespan left to Python; auto: every route that does not translate")
    ap.add_argument("--format", choices=["text", "json", "markdown"], default=None,
                    help="text (default), json (machine-readable, on stdout) or markdown (for a pull request or a CI summary)")
    ap.add_argument("--json", action="store_true", help="same as --format json")
    ap.add_argument("--explain", metavar="CODE", default=None,
                    help="explain an error code (P2A0201...): why it is refused and what to do, then exit")
    ap.add_argument("--fail-under", type=float, default=None, metavar="PCT",
                    help="also exit 1 when fewer than PCT %% of the routes are native")
    ap.add_argument("--allow-untested-versions", action="store_true",
                    help="do not refuse library versions outside the tested ranges")
    args = ap.parse_args(argv)
    if args.explain is not None:
        text = errors.explain(args.explain)
        if text is None:
            print(f"unknown error code {args.explain!r}: see {errors.DOCS}/reference/errors/", file=sys.stderr)
            return 2
        print(text)
        return 0
    if args.package is None:
        ap.error("the package directory is required (or --explain CODE)")
    if not args.package.is_dir():
        ap.error(f"{args.package} is not a directory")
    fmt = args.format or ("json" if args.json else "text")
    result = run(args.package, args.root, args.python_side, args.backend, args.allow_untested_versions)
    s = result["summary"]
    below = args.fail_under is not None and s["native_pct"] < args.fail_under
    if fmt == "json":
        print(to_json(result))
    else:
        print(_relative(to_markdown(result) if fmt == "markdown" else to_text(result, color=sys.stdout.isatty())))
        if below:
            print(f"native routes {s['native_pct']} % < --fail-under {args.fail_under:g} %")
    return 0 if s["generates"] and not below else 1

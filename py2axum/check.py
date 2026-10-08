"""`py2axum check`: what would translate, route by route, without generating a crate.

    py2axum check <package> [--root DIR] [--python-side PATH|auto|lifespan] [--json] [--fail-under PCT]

Each route is *native* (compiled into the binary), *python-side* (declared with --python-side, or moved by
--python-side auto) or *refused* (generation would stop on it), with the reason at file:line. Errors that
concern the whole application (and versions outside the tested ranges) are listed apart: they refuse the
generation whatever the routes. Built on the coverage report's collect mode (report.py), so `check`,
`--report` and `--python-side auto` agree.

Exit status: 0 when generation would succeed (and native routes ≥ --fail-under), 1 otherwise.
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

from . import versions
from .report import RouteReport, _where, aggregate, classify


_DECLARED = re.compile(r"^(?P<where>.+?:\d+): (?P<method>[A-Z]+) (?P<path>\S+) declared Python-side$")
_DECLARED_RAW = re.compile(r"^(?P<where>.+?:\d+): .* — (?P<path>\S+) declared Python-side$")


def run(package: Path, root: Path | None, python_side: list[str], backend: str = "auto",
        allow_untested: bool = False) -> dict:
    side = set(python_side)
    auto = "auto" in side
    side.discard("auto")
    result = {"package": str(package), "backend": "dyn", "python_side_auto": auto, "routes": [], "global": [],
              "notes": []}
    if not allow_untested:
        for msg, file, line in versions.check_project(root or package.resolve().parent, package):
            result["global"].append({"construction": "library version", "error": f"{file}:{line}: {msg}",
                                     "where": f"{file}:{line}"})
    if backend == "auto" and _typed_covers(package, root, result):
        return _summarise(result)
    from .report import collect_dyn

    fe, per_route = collect_dyn(package, root, side, auto=auto)
    for e in fe.global_errors:
        result["global"].append({"construction": classify(e, english=True)[0], "error": e.render(), "where": _where(e)})
    moving = auto and not fe.global_errors
    for info, errs in per_route:
        rr = RouteReport(info["method"].upper(), info["path"], info["func"], f"{info['file']}:{info['line']}",
                         info["conditional"], "native")
        for e in errs:
            label = classify(e, english=True)[0]
            rr.blockers.setdefault(label, [])
            if _where(e) not in rr.blockers[label]:
                rr.blockers[label].append(_where(e))
        if errs:
            rr.status = "python-side" if moving else "refused"
            rr.error = errs[0].render()
        result["routes"].append(rr)
    for path, e in getattr(fe, "auto_side", {}).items():
        rr = RouteReport("RAW", path, "?", _where(e), False, "python-side" if moving else "refused", e.render())
        rr.blockers[classify(e, english=True)[0]] = [_where(e)]
        result["routes"].append(rr)
    for n in fe.notes:
        m = _DECLARED.match(n) or _DECLARED_RAW.match(n)
        if m:
            rr = RouteReport(m.groupdict().get("method") or "RAW", m["path"], "?", m["where"], False, "python-side",
                             "declared --python-side")
            result["routes"].append(rr)
        else:
            result["notes"].append(n)
    result["routes"].sort(key=lambda r: (r.path, r.method))
    return _summarise(result)


def _typed_covers(package: Path, root: Path | None, result: dict) -> bool:
    """`--backend auto` generates with the typed backend when it covers the whole application."""
    import tempfile

    from . import codegen
    from .frontend import Frontend

    try:
        fe = Frontend(package, root)
        app = fe.run()
        with tempfile.TemporaryDirectory() as tmp:
            codegen.generate(app, Path(tmp), str(package), "check")
    except Exception:
        return False
    result["backend"] = "typed"
    result["routes"] = sorted((RouteReport(info["method"].upper(), info["path"], info["func"],
                                           f"{info['file']}:{info['line']}", info["conditional"], "native")
                               for info, _ in fe.route_infos), key=lambda r: (r.path, r.method))
    result["notes"] = list(fe.notes)
    return True


def _summarise(result: dict) -> dict:
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
    result["blockers"] = [{"construction": c["construction"], "routes": c["routes_touchées"],
                           "only_blocker": c["débloquées_seule"],
                           "where": sorted({w for r in routes for w in r.blockers.get(c["construction"], [])})}
                          for c in agg["constructions"]]
    return result


def to_json(result: dict) -> str:
    data = dict(result)
    data["routes"] = [{"method": r.method, "path": r.path, "handler": r.func, "where": r.where,
                       "status": r.status, "reason": r.error, "blockers": r.blockers} for r in result["routes"]]
    return json.dumps(data, ensure_ascii=False, indent=1)


def to_text(result: dict, color: bool = False) -> str:
    def c(code: str, s: str) -> str:
        return f"\x1b[{code}m{s}\x1b[0m" if color else s

    mark = {"native": c("32", "native     "), "python-side": c("33", "python-side"), "refused": c("31", "refused    ")}
    s = result["summary"]
    o = [f"py2axum check {result['package']} ({result['backend']} backend)", ""]
    if result["global"]:
        o.append(c("31", "Whole application — generation refused whatever the routes:"))
        o += [f"  {g['error']}" for g in result["global"]]
        o.append("")
    width = max((len(r.method) + 1 + len(r.path) for r in result["routes"]), default=0)
    for r in result["routes"]:
        o.append(f"{mark[r.status]}  {(r.method + ' ' + r.path).ljust(width)}  {r.where}")
        if r.status != "native" and r.error:
            o.append(f"             └ {r.error}")
    if result["blockers"]:
        o += ["", "Blockers (routes touched, routes for which it is the only blocker):"]
        for b in result["blockers"]:
            where = ", ".join(b["where"][:3]) + (f", +{len(b['where']) - 3}" if len(b["where"]) > 3 else "")
            o.append(f"  {b['routes']:>4} {b['only_blocker']:>4}  {b['construction']}  ({where})")
    if result["notes"]:
        o += ["", "Notes:"] + [f"  {n}" for n in result["notes"]]
    o += ["", f"{s['native']}/{s['total']} routes native ({s['native_pct']} %), {s['python-side']} python-side, "
              f"{s['refused']} refused — " + ("generation would succeed" if s["generates"] else
                                               c("31", "generation would fail"))]
    if not s["generates"] and s["refused"] and not result["global"] and not result["python_side_auto"]:
        o.append("hint: --python-side auto leaves the refused routes to a Python process next to the binary")
    return "\n".join(o)


def _relative(text: str) -> str:
    """Paths under the current directory shown relative to it (the translation works on resolved paths)."""
    return text.replace(str(Path.cwd()) + "/", "")


def main(argv: list[str]) -> int:
    import argparse

    ap = argparse.ArgumentParser(prog="py2axum check", description=__doc__.splitlines()[0])
    ap.add_argument("package", type=Path, help="directory of the FastAPI application package")
    ap.add_argument("--root", type=Path, default=None, help="import root, like sys.path (default: parent of the package)")
    ap.add_argument("--backend", choices=["auto", "dyn"], default="auto")
    ap.add_argument("--python-side", action="append", default=[], metavar="PATH|lifespan|auto",
                    help="as for generation: routes or the lifespan left to Python; auto: every route that does not translate")
    ap.add_argument("--json", action="store_true", help="machine-readable output on stdout")
    ap.add_argument("--fail-under", type=float, default=None, metavar="PCT",
                    help="also exit 1 when fewer than PCT %% of the routes are native")
    ap.add_argument("--allow-untested-versions", action="store_true",
                    help="do not refuse library versions outside the tested ranges")
    args = ap.parse_args(argv)
    if not args.package.is_dir():
        ap.error(f"{args.package} is not a directory")
    result = run(args.package, args.root, args.python_side, args.backend, args.allow_untested_versions)
    s = result["summary"]
    below = args.fail_under is not None and s["native_pct"] < args.fail_under
    if args.json:
        print(to_json(result))
    else:
        print(_relative(to_text(result, color=sys.stdout.isatty())))
        if below:
            print(f"native routes {s['native_pct']} % < --fail-under {args.fail_under:g} %")
    return 0 if s["generates"] and not below else 1

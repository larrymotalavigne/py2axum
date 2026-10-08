"""py2axum: transpile a FastAPI + SQLAlchemy + Pydantic + aiohttp package to a Rust/axum project.

usage: python -m py2axum <python package dir> -o <output dir> [--name crate_name] [--root DIR]
       python -m py2axum check <python package dir> [--root DIR] [--json] [--fail-under PCT]
       python -m py2axum <python package dir> --report report.md [--root DIR]   (+ report.json)
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

from .frontend import Frontend
from .ir import TranspileError


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] == ["check"]:
        from .check import main as check

        return check(argv[1:])
    ap = argparse.ArgumentParser(prog="py2axum", description=__doc__.splitlines()[0])
    ap.add_argument("package", type=Path, help="directory of the FastAPI application package")
    ap.add_argument("-o", "--out", type=Path, help="output directory for the Rust project")
    ap.add_argument("--root", type=Path, default=None,
                    help="import root, like sys.path (default: parent of the package)")
    ap.add_argument("--backend", choices=["auto", "typed", "dyn"], default="auto",
                    help="deprecated, no effect: there is one backend (auto and dyn are accepted until 1.0; "
                         "typed was removed in 0.4)")
    ap.add_argument("--python-side", action="append", default=[], metavar="PATH|lifespan|mount|auto",
                    help="what stays in a Python process next to the binary (raw routes such as /api/v1/mcp, lifespan; "
                         "mount: the app's last app.mount(), which gets every request no translated route fully matches); "
                         "auto: every route that does not translate, and such mounts")
    ap.add_argument("--report", type=Path, default=None,
                    help="coverage report (Markdown + JSON) instead of generating: never stops at the first error")
    ap.add_argument("--name", default=None, help="crate name (default: <package>_axum)")
    ap.add_argument("--no-stream", action="store_true", help="buffer list responses instead of streaming them")
    ap.add_argument("--allow-untested-versions", action="store_true",
                    help="translate even if the project locks library versions outside the tested ranges")
    args = ap.parse_args(argv)
    if args.backend == "typed":
        print("error: --backend typed was removed in 0.4: use the default backend (drop --backend)", file=sys.stderr)
        return 2
    from .versions import check_project

    bad = [] if args.allow_untested_versions else check_project(args.root or args.package.resolve().parent, args.package)
    if bad and not args.report:
        for msg, file, line in bad:
            print(f"error: {file}:{line}: {msg}", file=sys.stderr)
        return 1
    if args.report:
        from .report import aggregate, build, write

        result = build(args.package, args.root, stream=not args.no_stream,
                       python_side=tuple(args.python_side) or ("lifespan",))
        result["global"] += [("version hors fourchette", f"{file}:{line}: {msg}") for msg, file, line in bad]
        write(result, args.report)
        agg = aggregate(result["routes"])
        print(f"report {args.report} (+ .json): {agg['traduites']}/{agg['total']} routes translated", file=sys.stderr)
        return 0
    if args.out is None:
        ap.error("-o/--out is required (or --report)")

    crate = args.name or f"{args.package.resolve().name}_axum"
    python_side = set(args.python_side)
    auto = "auto" in python_side
    python_side.discard("auto")
    from . import dyn

    if auto:
        moved = auto_python_side(args.package, args.root, python_side)
        if moved is None:
            return 1
        python_side |= moved
    try:
        fe = Frontend(args.package, args.root)
        gzip = dyn.prepare(fe, python_side)
        proj = dyn.generate_project(fe, args.out, str(args.package), crate, gzip=gzip, stream=not args.no_stream)
    except TranspileError as e:
        print(f"error: {e.render()}", file=sys.stderr)
        return 1
    for n in fe.notes:
        print(f"note: {n}", file=sys.stderr)
    print(
        f"generated {args.out}: {len(proj.fns)} functions, {len(proj.models)} models, {len(proj.schemas)} schemas",
        file=sys.stderr,
    )
    return 0


def auto_python_side(package: Path, root: Path | None, python_side: set[str]) -> set[str] | None:
    """The paths of the routes that do not translate (`--python-side auto`), each reported with its first
    error; None when an error concerns the whole application (a route cannot be moved around it)."""
    from .report import collect_dyn

    fe, per_route = collect_dyn(package, root, python_side, auto=True)
    if fe.global_errors:
        for e in fe.global_errors:
            print(f"error: {e.render()}", file=sys.stderr)
        print("error: --python-side auto only moves routes; the errors above concern the whole application",
              file=sys.stderr)
        return None
    moved: dict[str, tuple[str, TranspileError]] = {p: ("raw route", e) for p, e in fe.auto_side.items()}
    if fe.__dict__.get("mount_fallback"):
        moved["mount"] = ("app.mount", None)
    for info, errs in per_route:
        if errs and info["path"] not in moved:
            moved[info["path"]] = (f"{info['method'].upper()} {info['func']}", errs[0])
    for path, (what, e) in sorted(moved.items()):
        if e is None:
            print(f"python-side (auto): {what} — requests no translated route fully matches", file=sys.stderr)
        else:
            print(f"python-side (auto): {path} ({what}) — {e.render()}", file=sys.stderr)
    return set(moved)


if __name__ == "__main__":
    sys.exit(main())

"""py2axum: transpile a FastAPI + SQLAlchemy + Pydantic + aiohttp package to a Rust/axum project.

usage: python -m py2axum <python package dir> -o <output dir> [--name crate_name] [--root DIR]
       python -m py2axum check <python package dir> [--root DIR] [--format text|json|markdown] [--fail-under PCT]
       python -m py2axum check --explain P2A0201
       python -m py2axum watch <python package dir> -o <output dir> [--run] [--python-side ...]
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
    from .config import ConfigError

    try:
        return _main(argv)
    except ConfigError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2


def _main(argv: list[str]) -> int:
    from .config import apply as with_config

    if argv[:1] == ["check"]:
        from .check import main as check
        from .check import parser as check_parser

        return check(with_config(argv[1:], check_parser))
    if argv[:1] == ["watch"]:
        from .watch import main as watch
        from .watch import parser as watch_parser

        return watch(with_config(argv[1:], watch_parser))
    from .versions import apply_python_flag

    if removed_backend(argv):
        return 2
    argv = with_config(argv, parser)
    ap = parser()
    args = ap.parse_args(argv)
    apply_python_flag(args)
    from .frontend import set_app_target
    from .modules import set_extra_roots

    set_extra_roots(args.path)
    set_app_target(args.app)
    from .libsrc import apply_flags

    try:
        apply_flags(args)
    except ValueError as e:
        ap.error(str(e))
    from .versions import check_project

    bad = [] if args.allow_untested_versions else check_project(args.root or args.package.resolve().parent, args.package)
    if bad and not args.report:
        from .errors import BY_CODE

        c = BY_CODE["P2A0601"]
        for msg, file, line in bad:
            print(f"error[{c.code}]: {file}:{line}: {msg}", file=sys.stderr)
        print(f"  = help: {c.fix}\n  = see: {c.anchor()}", file=sys.stderr)
        return 1
    if args.report:
        print("warning[deprecated]: --report is an internal tool, outside the stable interface from 1.0: use "
              "`py2axum check --format json` (or markdown)", file=sys.stderr)
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

    whole = False
    if auto:
        moved = auto_python_side(args.package, args.root, python_side)
        if moved is None:
            return 1
        whole = "*" in moved
        python_side |= moved - {"*"}
    try:
        fe = Frontend(args.package, args.root)
        if whole:
            dyn.prepare_relay(fe)
            gzip = None
        else:
            gzip = dyn.prepare(fe, python_side)
        proj = dyn.generate_project(fe, args.out, str(args.package), crate, gzip=gzip, stream=not args.no_stream)
    except TranspileError as e:
        print(e.explain(), file=sys.stderr)
        if not auto:
            print("  = note: `py2axum check` lists every refusal at once; `--python-side auto` leaves the routes "
                  "that do not translate to Python", file=sys.stderr)
        return 1
    for n in fe.notes:
        print(f"note: {n}", file=sys.stderr)
    print(
        f"generated {args.out}: {len(proj.fns)} functions, {len(proj.models)} models, {len(proj.schemas)} schemas",
        file=sys.stderr,
    )
    return 0


def parser() -> argparse.ArgumentParser:
    """The generation command line (frozen by tests/test_contract.py: see docs/reference/versioning.md)."""
    ap = argparse.ArgumentParser(prog="py2axum", description=__doc__.splitlines()[0])
    ap.add_argument("package", type=Path, help="directory of the FastAPI application package")
    ap.add_argument("-o", "--out", type=Path, help="output directory for the Rust project")
    ap.add_argument("--root", type=Path, default=None,
                    help="import root, like sys.path (default: parent of the package)")
    ap.add_argument("--path", type=Path, action="append", default=[], metavar="DIR",
                    help="experimental: one more import root, like an extra sys.path entry (repeatable): the sibling packages of a monorepo the application installs from its own checkout")
    ap.add_argument("--app", default=None, metavar="MODULE[:ATTR]",
                    help="the served application, as uvicorn's import string (`api.main:app`, `api.main:create_app`): only that module's FastAPI() is analysed when the package has several entry points (default: every FastAPI() of the package, analysed together)")
    ap.add_argument("--python-side", action="append", default=[], metavar="PATH|lifespan|mount|auto",
                    help="what stays in a Python process next to the binary (raw routes such as /api/v1/mcp, lifespan; "
                         "mount: the app's last app.mount() and include_router() of a library's router, which get every "
                         "request no translated route fully matches); "
                         "auto: every route that does not translate, and such mounts")
    ap.add_argument("--report", type=Path, default=None, help=argparse.SUPPRESS)  # internal tool, outside the contract
    ap.add_argument("--name", default=None, help="crate name (default: <package>_axum)")
    ap.add_argument("--no-stream", action="store_true", help="buffer list responses instead of streaming them")
    ap.add_argument("--allow-untested-versions", action="store_true",
                    help="translate even if the project locks library versions outside the tested ranges")
    from .libsrc import add_flags as add_libs_flags
    from .versions import add_python_flag

    add_python_flag(ap)
    add_libs_flags(ap)
    return ap


def removed_backend(argv: list[str]) -> bool:
    """`--backend` is gone (1.0): an error that says so, rather than argparse's `unrecognized arguments`."""
    if any(a == "--backend" or a.startswith("--backend=") for a in argv):
        print("error: --backend was removed in 1.0 (it has had no effect since 0.4: there is one backend): drop it",
              file=sys.stderr)
        return True
    return False


def auto_python_side(package: Path, root: Path | None, python_side: set[str]) -> set[str] | None:
    """What --python-side auto leaves to Python (fallback.py), each part printed with its first error: the paths of
    the routes that do not translate, `lifespan`, or `*` for the whole application; None when an error cannot be
    left to Python (the generation fails)."""
    from .fallback import plan
    from .report import collect_dyn

    fe, per_route = collect_dyn(package, root, python_side, auto=True)
    pl = plan(fe, per_route)
    if pl.refused:
        for e in pl.refused:
            print(e.explain(), file=sys.stderr)
        print("error: --python-side auto cannot leave the errors above to Python", file=sys.stderr)
        return None
    if pl.whole is not None:
        e = pl.whole
        print(f"python-side (auto): the whole application — {e.render()} [{e.code}]: the binary relays every request "
              "to PY2AXUM_PYTHON_URL", file=sys.stderr)
        return {"*"}
    for path, (what, e) in sorted(pl.routes.items()):
        if e is None:
            print(f"python-side (auto): {what} — requests no translated route fully matches", file=sys.stderr)
        else:
            print(f"python-side (auto): {path} ({what}) — {e.render()} [{e.code}]", file=sys.stderr)
    if pl.lifespan is not None:
        e = pl.lifespan
        print(f"python-side (auto): lifespan — {e.render()} [{e.code}]", file=sys.stderr)
    return set(pl.routes) | ({"lifespan"} if pl.lifespan is not None else set())


if __name__ == "__main__":
    sys.exit(main())

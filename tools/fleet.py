"""Fleet scoreboard: `py2axum check --json` over many applications, blockers aggregated across them.

    python tools/fleet.py APP[:ROOT[:VENV]][@MODULE:ATTR] ... [--python-side auto] [--allow-untested-versions] [--format text|markdown|json]
                                         [--save DIR]

APP is the application package (the directory `py2axum check` takes), ROOT its import root (default: its parent),
VENV the project's environment when it is not a `.venv` next to it (`--venv`: the installed libraries' sources),
MODULE:ATTR the served application (`--app`, uvicorn's import string) when the package has several.
The output ranks the constructs by the routes they block across all applications, so the next runtime feature is
the one that unlocks the most routes of the fleet, not of one application. Reports stay local: nothing is sent.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path


def run_check(app: str, extra: list[str]) -> dict:
    app, _, target = app.partition("@")
    pkg, _, rest = app.partition(":")
    root, _, venv = rest.partition(":")
    pkg_path = Path(pkg).resolve()
    root_path = Path(root).resolve() if root else pkg_path.parent
    cmd = [sys.executable, "-m", "py2axum", "check", str(pkg_path), "--root", str(root_path), "--json", *extra]
    if venv:
        cmd += ["--venv", str(Path(venv).resolve())]
    if target:
        cmd += ["--app", target]
    p = subprocess.run(cmd, capture_output=True, text=True, cwd=root_path.parent)
    try:
        data = json.loads(p.stdout)
    except json.JSONDecodeError:
        return {"name": name_of(pkg_path), "crash": (p.stderr or p.stdout).strip().splitlines()[-1:] or ["no output"]}
    data["name"] = name_of(pkg_path)
    return data


def name_of(pkg: Path) -> str:
    for part in reversed(pkg.parts):
        if part not in {"api", "app", "back", "src"}:
            return part
    return pkg.name


def aggregate(results: list[dict]) -> list[dict]:
    rows: dict[tuple[str, str], dict] = {}
    for r in results:
        for b in r.get("blockers", []):
            row = rows.setdefault((b["code"], b["construction"]),
                                  {"code": b["code"], "construction": b["construction"], "routes": 0, "only": 0, "apps": []})
            row["routes"] += b["routes"]
            row["only"] += b["only_blocker"]
            row["apps"].append(r["name"])
    return sorted(rows.values(), key=lambda x: (-x["only"], -x["routes"], x["code"]))


def global_errors(results: list[dict]) -> list[dict]:
    seen: dict[tuple[str, str], dict] = {}
    for r in results:
        for g in r.get("global", []):
            row = seen.setdefault((g["code"], g["construction"]), {"code": g["code"], "construction": g["construction"],
                                                                   "apps": set(), "example": g["error"]})
            row["apps"].add(r["name"])
    out = [dict(v, apps=sorted(v["apps"])) for v in seen.values()]
    return sorted(out, key=lambda x: (-len(x["apps"]), x["code"]))


def render(results: list[dict], fmt: str) -> str:
    if fmt == "json":
        return json.dumps({"apps": [{k: r.get(k) for k in ("name", "summary", "crash")} for r in results],
                           "blockers": aggregate(results), "global": global_errors(results)}, indent=2)
    md = fmt == "markdown"
    lines = []
    tot = nat = gen = 0
    lines.append("| app | routes | native | python-side | refused | native % | generates |" if md else
                 f"{'app':40} {'routes':>6} {'native':>6} {'py':>5} {'refused':>7} {'native%':>8}  generates")
    if md:
        lines.append("|---|---:|---:|---:|---:|---:|---|")
    for r in results:
        if "crash" in r:
            lines.append(f"| {r['name']} | crash: {r['crash'][0]} |||||" if md else f"{r['name']:40} CRASH {r['crash'][0]}")
            continue
        s = r["summary"]
        tot += s["total"]
        nat += s["native"]
        gen += bool(s["generates"])
        cells = (r["name"], s["total"], s["native"], s["python-side"], s["refused"], f"{s['native_pct']:.1f}",
                 "yes" if s["generates"] else "no")
        lines.append("| " + " | ".join(map(str, cells)) + " |" if md else
                     f"{cells[0]:40} {cells[1]:>6} {cells[2]:>6} {cells[3]:>5} {cells[4]:>7} {cells[5]:>8}  {cells[6]}")
    pct = 100 * nat / tot if tot else 0.0
    lines.append(f"\nfleet: {nat}/{tot} routes native ({pct:.1f} %), {gen}/{len(results)} applications generate\n")
    lines.append("Whole-application errors (refuse the generation whatever the routes):")
    for g in global_errors(results):
        lines.append(f"  {len(g['apps']):2} apps  {g['code']}  {g['construction']}  — {g['example'][:160]}")
    lines.append("\nRoute blockers across the fleet (routes touched, routes for which it is the only blocker):")
    for b in aggregate(results):
        apps = ", ".join(sorted(set(b["apps"])))
        lines.append(f"  {b['routes']:5} {b['only']:5}  {b['code']}  {b['construction'][:50]:50}  {apps}")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("apps", nargs="+", metavar="APP[:ROOT[:VENV]][@MODULE:ATTR]")
    ap.add_argument("--python-side", action="append", default=[])
    ap.add_argument("--allow-untested-versions", action="store_true")
    ap.add_argument("--format", choices=("text", "markdown", "json"), default="text")
    ap.add_argument("--save", type=Path, help="also write each application's check report there")
    ap.add_argument("-j", "--jobs", type=int, default=4)
    a = ap.parse_args(argv)
    extra = [f"--python-side={p}" for p in a.python_side]
    if a.allow_untested_versions:
        extra.append("--allow-untested-versions")
    with ThreadPoolExecutor(a.jobs) as ex:
        results = list(ex.map(lambda app: run_check(app, extra), a.apps))
    if a.save:
        a.save.mkdir(parents=True, exist_ok=True)
        for r in results:
            (a.save / f"{r['name']}.json").write_text(json.dumps(r, indent=2))
    print(render(results, a.format))
    return 0 if all(r.get("summary", {}).get("generates") for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())

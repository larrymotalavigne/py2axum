"""The corpus bench: the FastAPI documentation's examples (and the Pydantic documentation's models, and the
SQLAlchemy documentation's doctests), translated by py2axum and compared with Python on their official tests.

    python corpus/run.py [--work DIR] [--only SUBSTR] [--jobs N] [--bundle N] [--skip-pydantic]
                         [--skip-sqlalchemy]

Steps: fetch the sources (corpus/sources.py), discover the examples and their tests (corpus/examples.py),
`py2axum check` each one, generate and compile those that translate (corpus/build.py), then for each official
test file start the Python reference (uvicorn) and the binary of each of its examples and replay the tests
against both (corpus/replay.py). Each example ends up:

    identical   every compared response is the same (status, headers, body byte for byte, key order)
    differs     at least one response differs: a correctness bug, unless documented
    refused     py2axum refuses it (whole application or one of its routes), with the reason at file:line
    error       generation or compilation failed (a translator bug), or the bench could not run it
    documented  the only differences are documented ones (docs/supported.md: the order of a set's elements,
                an integer beyond 64 bits answered 500)
    nondeterministic  Python's own output varies from run to run (random): listed, never counted identical
    untested    translated, but no official test exercises it (no request to compare)
    docs-only   its tests only request the OpenAPI documentation, which the binary does not serve
    reference-fails  (SQLAlchemy) the Python reference itself fails on the generated route: never counted

The SQLAlchemy corpus has its own pipeline (corpus/sqlalchemy_docs.py: ports 10300-10399, databases
`py2axum_sqla_*`); its examples land in the same results.json (kind "sqlalchemy").

Writes <work>/results.json; corpus/coverage.py turns it into docs/coverage.md. Ports 10000-10999, database
`py2axum_corpus` (DATABASE_URL overrides).
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(ROOT))

import build  # noqa: E402
import examples as exs  # noqa: E402
import sources  # noqa: E402

DB = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/py2axum_corpus")
# the environment an official test sets with monkeypatch.setenv before it imports the example (both servers get it)
EXAMPLE_ENV = {"settings__": {"ADMIN_EMAIL": "admin@example.com"}}
PORT_BASE = int(os.environ.get("CORPUS_PORT_BASE", "10000"))


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def ensure_db() -> None:
    try:
        import psycopg

        base, name = DB.rsplit("/", 1)
        with psycopg.connect(base + "/postgres", autocommit=True) as c:
            if not c.execute("SELECT 1 FROM pg_database WHERE datname = %s", (name,)).fetchone():
                c.execute(f'CREATE DATABASE "{name}"')
    except Exception as e:  # noqa: BLE001 - only the examples that use a database need it
        log(f"note: database {DB} not available ({e})")


def wait_port(port: int, proc: subprocess.Popen, timeout: float = 30) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        if proc.poll() is not None:
            return False
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.1)
    return False


def free(port: int) -> bool:
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) != 0


class Servers:
    """The reference and the binary of the examples of one test file, on a worker's ports."""

    def __init__(self, work: Path, repo: Path, slot: int, examples: list[exs.Example], binaries: dict[str, str]):
        self.procs: list[subprocess.Popen] = []
        self.map: dict[str, dict] = {}
        self.startup: dict[str, str] = {}
        port = PORT_BASE + slot * 40
        for ex in examples:
            ref_port, cand_port = port, port + 1
            port += 2
            for p in (ref_port, cand_port):
                if not free(p):
                    raise RuntimeError(f"port {p} busy")
            cwd = work / "run" / ex.id
            if cwd.exists():
                shutil.rmtree(cwd)
            cwd.mkdir(parents=True)
            logs = work / "logs"
            app_root = repo if ex.kind == "fastapi" else work / "pydantic_apps"
            extra = {k: v for prefix, env in EXAMPLE_ENV.items() if ex.id.startswith(prefix) for k, v in env.items()}
            ref = subprocess.Popen([sys.executable, "-m", "uvicorn", f"{ex.module}:app", "--port", str(ref_port),
                                    "--log-level", "warning", "--no-access-log"], cwd=cwd,
                                   env={**os.environ, **extra, "PYTHONPATH": str(app_root), "DATABASE_URL": DB},
                                   stdout=open(logs / f"{ex.id}.ref.log", "w"), stderr=subprocess.STDOUT)
            self.procs.append(ref)
            cand = subprocess.Popen([str(work / "target" / "release" / binaries[ex.id])], cwd=cwd,
                                    env={**os.environ, **extra, "CORPUS_APP": ex.id, "PORT": str(cand_port), "HOST": "127.0.0.1",
                                         "DATABASE_URL": DB, "DB_POOL_SIZE": "2"},
                                    stdout=open(logs / f"{ex.id}.cand.log", "w"), stderr=subprocess.STDOUT)
            self.procs.append(cand)
            ok_ref, ok_cand = wait_port(ref_port, ref), wait_port(cand_port, cand)
            if not ok_ref:
                self.startup[ex.id] = "reference did not start: " + (logs / f"{ex.id}.ref.log").read_text()[-1500:]
            elif not ok_cand:
                self.startup[ex.id] = "binary did not start: " + (logs / f"{ex.id}.cand.log").read_text()[-1500:]
            else:
                self.map[ex.module] = {"ref": f"http://127.0.0.1:{ref_port}", "cand": f"http://127.0.0.1:{cand_port}"}

    def stop(self) -> None:
        for p in self.procs:
            if p.poll() is None:
                p.send_signal(signal.SIGTERM)
        for p in self.procs:
            try:
                p.wait(5)
            except subprocess.TimeoutExpired:
                p.kill()


def replay_file(work: Path, repo: Path, slot: int, test_file: str, examples: list[exs.Example],
                binaries: dict[str, str]) -> tuple[list[dict], dict[str, str]]:
    servers = Servers(work, repo, slot, examples, binaries)
    try:
        out = work / "events" / (test_file.replace("/", "__") + ".jsonl")
        out.parent.mkdir(parents=True, exist_ok=True)
        if out.exists():
            out.unlink()
        mp = work / "events" / (test_file.replace("/", "__") + ".map.json")
        mp.write_text(json.dumps(servers.map))
        if servers.map:
            env = {**os.environ, "CORPUS_MODE": "replay", "CORPUS_MAP": str(mp), "CORPUS_OUT": str(out),
                   "PYTHONPATH": f"{HERE}{os.pathsep}{repo}"}
            cwd = work / "run" / f"pytest{slot}"
            cwd.mkdir(parents=True, exist_ok=True)
            try:
                subprocess.run([sys.executable, "-W", "ignore", "-m", "pytest", "-p", "replay", "-q", "--no-header",
                                "-p", "no:cacheprovider", f"--rootdir={repo}", "-c", str(repo / "pyproject.toml"),
                                "-o", "timeout=60", "-W", "ignore::ResourceWarning",
                                "-W", "ignore::pytest.PytestUnraisableExceptionWarning", str(repo / test_file)],
                               cwd=cwd, env=env, capture_output=True, text=True, timeout=900)
            except subprocess.TimeoutExpired:
                servers.startup.update({ex.id: "bench: the test file timed out" for ex in examples
                                        if ex.module in servers.map})
        events = [json.loads(line) for line in out.read_text().splitlines()] if out.exists() else []
        return events, servers.startup
    finally:
        servers.stop()


def first_reason(check: dict) -> tuple[str, list[dict]]:
    """The reasons py2axum refuses an example: (summary, [{construction, where, error}])."""
    reasons = [{"construction": g["construction"], "where": g["where"], "error": g["error"]} for g in check.get("global", [])]
    for r in check.get("routes", []):
        if r["status"] != "native":
            for label, wheres in (r.get("blockers") or {}).items():
                reasons.append({"construction": label, "where": wheres[0] if wheres else r["where"],
                                "error": r.get("reason") or ""})
    seen, uniq = set(), []
    for r in reasons:
        k = (r["construction"], r["where"])
        if k not in seen:
            seen.add(k)
            uniq.append(r)
    return (uniq[0]["construction"] if uniq else "refused"), uniq


def _permuted_lists_only(a, b) -> bool:
    """Two values equal up to the order of the elements of some lists (a `set` serialised: its order follows
    CPython's string hashing, randomised per process — docs/supported.md)."""
    if isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            return False
        if all(_permuted_lists_only(x, y) for x, y in zip(a, b)):
            return True
        key = lambda v: json.dumps(v, sort_keys=True)  # noqa: E731
        return sorted(map(key, a)) == sorted(map(key, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return list(a) == list(b) and all(_permuted_lists_only(a[k], b[k]) for k in a)
    return a == b


def documented(src: Path, d: dict) -> str | None:
    """The documented difference a response differs by, if any."""
    r, c = d["ref"], d["cand"]
    if {k for k in set(r) | set(c) if r.get(k) != c.get(k)} == {"body"} and _permuted_lists_only(r["body"], c["body"]):
        text = "".join(p.read_text() for p in ([src] if src.is_file() else src.rglob("*.py")))
        if re.search(r"\b(set|frozenset|Set|FrozenSet)\[", text):
            return "order of a set's elements"
    big = [int(n) for n in re.findall(r"(?<![\d.\w])-?\d{19,}(?![\d.])", d.get("test") or "")]
    if c.get("status") == 500 and r.get("status") != 500 and any(not -2**63 <= n < 2**63 for n in big):
        return "an integer beyond 64 bits in the request"
    return None


# examples whose Python output is not reproducible from one run to the next: compared, never counted identical
NONDETERMINISTIC = {
    "query_params_str_validations__tutorial015_an_py310": "the endpoint returns random.choice(...)",
    "query_params_str_validations__tutorial015_py310": "the endpoint returns random.choice(...)",
}


def classify(ex: exs.Example, events: list[dict], src: Path) -> dict:
    """src: the example's sources (a file or a directory)."""
    exch = [e for e in events if e["ev"] == "exchange" and e["module"] == ex.module]
    compared = [e for e in exch if e["kind"] == "http"]
    tests = {}
    for e in events:
        if e["ev"] == "test":
            tests[e["test"]] = e["outcome"]
    diffs = [{"test": e["test"], "ref": e["ref"], "cand": e["cand"]} for e in compared if not e["same"]]
    notes = sorted({n for d in diffs if (n := documented(src, d))})
    if notes and all(documented(src, d) for d in diffs):
        known = notes
    else:
        known = []
    mine = {t: o for t, o in tests.items() if any(e["test"] == t for e in exch)}
    if not exch:
        status = "untested"
    elif not compared:
        status = "docs-only"
    elif ex.id in NONDETERMINISTIC:
        # identical or not, by chance: never counted
        status, known = "nondeterministic", [NONDETERMINISTIC[ex.id]]
    elif not diffs:
        status = "identical"
    elif known:
        status = "documented"
    else:
        status = "differs"
    return {"status": status, "compared": len(compared), "same": len(compared) - len(diffs), "known": known,
            "docs_excluded": sum(e["kind"] == "docs" for e in exch),
            "overridden": sum(e["kind"] == "overridden" for e in exch), "diffs": diffs[:20], "n_diffs": len(diffs),
            # the official assertions, run on the binary's responses (informative: a test can fail on both)
            "tests_passed": sum(o == "passed" for o in mine.values()), "tests_run": len(mine)}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--work", type=Path, default=HERE / "out", help="working directory (cache, crates, results)")
    ap.add_argument("--only", default=None, help="only the examples whose module contains this text")
    ap.add_argument("--jobs", type=int, default=max(2, min(8, (os.cpu_count() or 4) - 2)))
    ap.add_argument("--bundle", type=int, default=60, help="examples per shared crate")
    ap.add_argument("--skip-pydantic", action="store_true")
    ap.add_argument("--skip-fastapi", action="store_true")
    ap.add_argument("--skip-sqlalchemy", action="store_true")
    ap.add_argument("--sqla-bundle", type=int, default=12, help="SQLAlchemy apps per shared crate")
    args = ap.parse_args()
    t0 = time.time()
    work = args.work.resolve()
    for d in ("pkgs", "gen", "checks", "logs", "bundles"):
        (work / d).mkdir(parents=True, exist_ok=True)
    timings: dict[str, float] = {}

    def lap(name: str, since: float) -> float:
        timings[name] = round(time.time() - since, 1)
        log(f"[{name}] {timings[name]} s")
        return time.time()

    t = time.time()
    repo = sources.fetch("fastapi")
    all_ex: list[exs.Example] = []
    if not args.skip_fastapi:
        fx = exs.discover(repo)
        nav = exs.assign_pages(repo, fx)
        tests = exs.discover_tests(repo, HERE, work / "discover.jsonl")
        for ex in fx:
            ex.tests = sorted(tests.get(ex.module, ()))
        all_ex += fx
    else:
        nav = []
    if not args.skip_pydantic:
        import pydantic_docs

        all_ex += pydantic_docs.examples(sources.fetch("pydantic"), work / "pydantic_apps", log, args.jobs)
    if args.only:
        all_ex = [e for e in all_ex if args.only in e.module]
    t = lap("discover", t)
    log(f"{len(all_ex)} examples ({sum(e.kind == 'fastapi' for e in all_ex)} FastAPI, "
        f"{sum(e.kind == 'pydantic' for e in all_ex)} Pydantic)")

    stamp = build.py2axum_hash()
    results: dict[str, dict] = {}

    def unstage(text: str, ex) -> str:
        if ex.kind == "fastapi":
            return exs.unstage(text, ex, work / "pkgs")
        import pydantic_docs

        return pydantic_docs.unstage(text, ex, work / "pydantic_apps" / "pkgs")

    def staged(ex):
        if ex.kind == "fastapi":
            return exs.stage(repo, ex, work / "pkgs")
        return work / "pydantic_apps" / "pkgs" / ex.id

    pkgs = {ex.id: staged(ex) for ex in all_ex}
    checks = dict(zip([e.id for e in all_ex], build.parallel(lambda e: build.check(pkgs[e.id], work / "checks", stamp),
                                                             all_ex, args.jobs)))
    t = lap("check", t)
    to_gen = []
    for ex in all_ex:
        c = checks[ex.id]
        base = {"module": ex.module, "source": ex.source, "page": ex.page, "title": ex.title, "section": ex.section,
                "kind": ex.kind, "tests": ex.tests}
        if "crash" in c:
            results[ex.id] = {**base, "status": "error", "error_kind": "check", "error": unstage(c["crash"], ex)}
        elif not c["summary"]["generates"]:
            summary, reasons = first_reason(c)
            for r in reasons:
                r["where"] = unstage(r["where"], ex)
                r["error"] = unstage(r["error"], ex)
            results[ex.id] = {**base, "status": "refused", "reason": summary, "reasons": reasons,
                              "routes": c["summary"]}
        else:
            results[ex.id] = {**base, "routes": c["summary"]}
            to_gen.append(ex)
    log(f"{len(to_gen)} translate, {sum(r.get('status') == 'refused' for r in results.values())} refused")
    errs = build.parallel(lambda e: build.generate(pkgs[e.id], work / "gen" / e.id, stamp), to_gen, args.jobs)
    gens = {}
    for ex, err in zip(to_gen, errs):
        if err and "Traceback" not in err and (m := re.search(r"error: (\S+?:\d+): (.*)", err)):
            # a refusal `check` did not foresee (itself a bug of check, but the example is refused)
            where, msg = unstage(m[1], ex), unstage(m[2], ex)
            results[ex.id].update(status="refused", reason="at generation", reasons=[
                {"construction": "at generation: " + msg.split(":")[0][:60], "where": where, "error": msg}])
        elif err:
            results[ex.id].update(status="error", error_kind="generate", error=unstage(err, ex))
        else:
            gens[ex.id] = work / "gen" / ex.id
    t = lap("generate", t)
    binaries, failed = build.build_all(gens, work, args.bundle, log) if gens else ({}, {})
    for i, err in failed.items():
        results[i].update(status="error", error_kind="build", error=err)
    t = lap("build", t)

    # replay: one unit per official test file (FastAPI) or per generated app (Pydantic)
    ensure_db()
    by_file: dict[str, list[exs.Example]] = {}
    for ex in all_ex:
        if ex.id not in binaries:
            continue
        if ex.kind == "pydantic":
            by_file.setdefault(f"pydantic::{ex.id}", []).append(ex)
        for f in ex.tests:
            by_file.setdefault(f, []).append(ex)
    events_of: dict[str, list[dict]] = {ex.id: [] for ex in all_ex}
    startup: dict[str, str] = {}
    slots = list(range(args.jobs))

    def unit(item):
        name, exl = item
        slot = slots.pop()
        try:
            if name.startswith("pydantic::"):
                import pydantic_docs

                return name, *pydantic_docs.replay(work, slot, exl[0], binaries, Servers)
            return name, *replay_file(work, repo, slot, name, exl, binaries)
        except Exception as e:  # noqa: BLE001 - a bench failure is recorded, the others go on
            return name, [], {ex.id: f"bench: {e!r}" for ex in exl}
        finally:
            slots.append(slot)

    with ThreadPoolExecutor(args.jobs) as pool:
        for name, events, st in pool.map(unit, sorted(by_file.items())):
            startup.update(st)
            for ex in by_file[name]:
                events_of[ex.id] += [e for e in events if e.get("module") in (ex.module, None)]
    t = lap("replay", t)

    for ex in all_ex:
        r = results[ex.id]
        if "status" in r:
            continue
        if ex.id in startup:
            r.update(status="error", error_kind="startup", error=startup[ex.id])
            continue
        r.update(classify(ex, events_of[ex.id], repo / ex.source if ex.kind == "fastapi" else pkgs[ex.id]))
    sqla = None
    if not args.skip_sqlalchemy:
        import sqlalchemy_docs

        server = DB.rsplit("/", 1)[0].replace("postgresql+psycopg://", "postgresql://", 1)
        sqla = sqlalchemy_docs.bench(work / "sqla", server, args.jobs, args.sqla_bundle, stamp, log, args.only)
        t = lap("sqlalchemy", t)
    timings["total"] = round(time.time() - t0, 1)
    # no machine path in the results (docs/coverage.md is public): the work and source trees become relative
    roots = sorted({str(work) + "/", str(work.resolve()) + "/", str(repo) + "/", str(repo.resolve()) + "/"}, key=len,
                   reverse=True)

    def relative(v):
        if isinstance(v, str):
            for r in roots:
                v = v.replace(r, "")
            return v
        if isinstance(v, list):
            return [relative(x) for x in v]
        if isinstance(v, dict):
            return {k: relative(x) for k, x in v.items()}
        return v

    if sqla:
        results.update(sqla["examples"])
    results = relative(results)
    out = {"fastapi": sources.version("fastapi"), "pydantic": sources.version("pydantic"), "nav": nav,
           "timings": timings, "examples": results}
    if sqla:
        out["sqlalchemy"], out["sqlalchemy_skipped"] = sqla["version"], sqla["skipped"]
    (work / "results.json").write_text(json.dumps(out, ensure_ascii=False, indent=1))
    counts: dict[str, int] = {}
    for r in results.values():
        counts[r["status"]] = counts.get(r["status"], 0) + 1
    log(f"{counts}  total {timings['total']} s  -> {work / 'results.json'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

"""Throughput/latency/memory benchmark: FastAPI (Uvicorn), FastAPI (Granian), generated axum.

Fairness: every server gets the same CPU budget. Python servers run one worker per core and
axum one Tokio thread per core. On a large machine, set SERVER_CPUS / PG_CPUS / LOAD_CPUS
(e.g. 0-7 / 8-11 / 12-15) to isolate server, Postgres and load generator; without them nothing
is pinned and all three share the machine.

usage: python bench/bench.py [--duration 10s] [--conns 128]
       python bench/bench.py --target dynapp --duration 5s --compare bench/perf_reference.json --table perf.md
         the generated fixtures/dynapp binary alone (DYNAPP_BIN, default generated/dynapp_axum/...), compared
         with a reference measured on the same machine: a drop of more than --tolerance (30 %) on any endpoint
         makes the exit status 1 (CI: a warning, the runners are shared and noisy)
       python bench/bench.py --target bookshelf [--interleave 3]
         examples/bookshelf: FastAPI + Uvicorn against the binary (BOOKSHELF_BIN, default
         generated/bookshelf/target/release/bookshelf), on a database seeded with 1 000 books and 3 000 reviews
"""
from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DB = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/poc")
SERVER_CPUS = os.environ.get("SERVER_CPUS")
PG_CPUS = os.environ.get("PG_CPUS")
LOAD_CPUS = os.environ.get("LOAD_CPUS")


def cpu_count(spec: str | None) -> int:
    if not spec:
        return len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count()
    n = 0
    for part in spec.split(","):
        a, _, b = part.partition("-")
        n += int(b or a) - int(a) + 1
    return n


WORKERS = int(os.environ.get("WORKERS", cpu_count(SERVER_CPUS)))


def pinned(cpus: str | None, cmd: list[str]) -> list[str]:
    return ["taskset", "-c", cpus, *cmd] if cpus else cmd

SERVERS = {
    "FastAPI + Uvicorn": {
        "port": 8000,
        "cmd": f"uvicorn app.main:app --port 8000 --workers {WORKERS} --log-level warning --no-access-log",
        "env": {"DB_POOL_SIZE": "10"},
    },
    "FastAPI + Granian": {
        "port": 8001,
        "cmd": f"granian --interface asgi --workers {WORKERS} --port 8001 --log-level warning --no-ws app.main:app",
        "env": {"DB_POOL_SIZE": "10"},
    },
    "axum (généré)": {
        "port": 8080,
        "cmd": os.environ.get("APP_BIN", "generated/app_axum/target/release/app_axum"),
        "env": {"PORT": "8080", "TOKIO_WORKER_THREADS": str(WORKERS), "DB_POOL_SIZE": "32"},
    },
}

# --target dynapp: the reference app of real-project constructions, seeded by seed_dynapp (60 tasks, 10 projects of 5 tasks)
DYNAPP_PORT = int(os.environ.get("DYNAPP_PORT", "8090"))
DYNAPP = {
    "axum dynapp": {
        "port": DYNAPP_PORT,
        "cmd": os.environ.get("DYNAPP_BIN", "generated/dynapp_axum/target/release/dynapp_axum"),
        "env": {"PORT": str(DYNAPP_PORT), "TOKIO_WORKER_THREADS": str(WORKERS), "DB_POOL_SIZE": "32"},
    },
}
DYNAPP_ENDPOINTS = {
    "GET /tasks/{id} (1 SELECT)": ["--rand-regex-url", "/tasks/([1-5][0-9]|[1-9])"],
    "GET /tasks?priority=low (20 rows)": ["/tasks?priority=low"],
    "GET /projects/{id} (selectinload)": ["--rand-regex-url", "/projects/[1-9]"],
    "POST /describe (validation)": [
        "/describe", "-m", "POST", "-H", "content-type: application/json",
        "-d", '{"title": "t", "priority": "high", "tags": ["a", "b"], "channel": "mail"}',
    ],
    "GET /tasks/0 (404)": ["/tasks/0"],
}

# --target bookshelf: the example application, seeded by seed_bookshelf
BOOKSHELF_SECRET = "change-me-in-production-0123456789"  # the example's default
BOOKSHELF = {
    "FastAPI + Uvicorn": {
        "port": 9050,
        "cmd": f"uvicorn --app-dir examples/bookshelf app.main:app --port 9050 --workers {WORKERS} --log-level warning "
               "--no-access-log",
        "env": {"DATABASE_URL": DB.replace("postgresql://", "postgresql+psycopg://", 1)},
    },
    "py2axum binary": {
        "port": 9090,
        "cmd": os.environ.get("BOOKSHELF_BIN", "generated/bookshelf/target/release/bookshelf"),
        "env": {"PORT": "9090", "TOKIO_WORKER_THREADS": str(WORKERS)},
    },
}


def hs256(claims: dict, key: str) -> str:
    """a JWT signed with HS256, without PyJWT (the bench's only dependencies are oha and the servers')"""
    import base64
    import hashlib
    import hmac

    b64 = lambda raw: base64.urlsafe_b64encode(raw).rstrip(b"=").decode()  # noqa: E731
    head = b64(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(",", ":")).encode())
    body = b64(json.dumps(claims, separators=(",", ":")).encode())
    sig = hmac.new(key.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest()
    return f"{head}.{body}.{b64(sig)}"


BOOKSHELF_AUTH = f"authorization: Bearer {hs256({'sub': '1', 'exp': 4102444800}, BOOKSHELF_SECRET)}"
BOOKSHELF_ENDPOINTS = {
    "GET /health": ["/health"],
    "GET /books/{id} (book, owner, reviews)": ["--rand-regex-url", "/books/([1-9][0-9]{0,2}|1000)"],
    "GET /books?limit=20 (20 rows)": ["/books?limit=20"],
    "GET /books?tag=sf&limit=20 (JSONB @>)": ["/books?tag=sf&limit=20"],
    "GET /books/stats (aggregates)": ["/books/stats"],
    "GET /me (JWT + 1 SELECT)": ["/me", "-H", BOOKSHELF_AUTH],
    "POST /books (validation + INSERT)": [
        "/books", "-m", "POST", "-H", BOOKSHELF_AUTH, "-H", "content-type: application/json",
        "-d", '{"title": " Dune ", "author": "Frank Herbert", "year": 1965, "tags": ["SF", "classic"]}',
    ],
    "POST /auth/register (422)": [
        "/auth/register", "-m", "POST", "-H", "content-type: application/json",
        "-d", '{"email": "not-an-email", "password": "short", "display_name": ""}',
    ],
}

ENDPOINTS = {
    "GET /health (JSON)": ["/health"],
    "GET /users/{id} (1 SELECT)": ["--rand-regex-url", "/users/[1-9][0-9]{0,3}"],
    "GET /users?limit=20 (liste)": ["/users?limit=20"],
    "PATCH /users/{id} (UPDATE)": [
        "--rand-regex-url", "/users/[1-9][0-9]{0,3}",
        "-m", "PATCH", "-H", "content-type: application/json", "-d", '{"age": 30}',
    ],
}


def sh(cmd: str) -> str:
    return subprocess.run(cmd, shell=True, check=True, capture_output=True, text=True).stdout


def seed() -> None:
    sh(f"psql {DB} -qc 'TRUNCATE users RESTART IDENTITY'")
    sh(
        f"psql {DB} -qc \"INSERT INTO users (email, name, age, is_active) "
        "SELECT 'user' || g || '@example.com', 'User ' || g, 20 + g % 50, g % 7 <> 0 "
        "FROM generate_series(1, 10000) g\""
    )
    sh(f"psql {DB} -qc 'VACUUM ANALYZE users'")


def seed_dynapp(port: int) -> None:
    """the schema by SQLAlchemy (as tests/scenarios/dynapp.py), the rows through the API"""
    from sqlalchemy import create_engine, text

    import sys
    sys.path.insert(0, str(ROOT))
    from fixtures.dynapp.models import Base

    engine = create_engine(DB.replace("postgresql://", "postgresql+psycopg://", 1))
    Base.metadata.create_all(engine)
    with engine.begin() as conn:
        names = ", ".join(t.name for t in Base.metadata.sorted_tables)
        conn.execute(text(f"TRUNCATE {names} RESTART IDENTITY CASCADE"))
    engine.dispose()

    def post(path: str, body: dict) -> None:
        req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", json.dumps(body).encode(),
                                     {"content-type": "application/json"})
        urllib.request.urlopen(req, timeout=10).read()

    for i in range(60):
        post("/tasks", {"title": f"task {i}", "priority": "low" if i % 3 == 0 else "high", "tags": ["a"] * (i % 3)})
    for i in range(10):
        post("/projects", {"name": f"project {i}", "owner": f"owner {i}", "tasks": [f"pt {i}.{j}" for j in range(5)]})


def seed_bookshelf() -> None:
    """50 users, 1 000 books (a third tagged sf), 3 000 reviews; the schema from examples/bookshelf/schema.sql"""
    sql = (ROOT / "examples/bookshelf/schema.sql").read_text() + """
    TRUNCATE users, books, reviews RESTART IDENTITY CASCADE;
    INSERT INTO users (email, display_name, password_hash)
        SELECT 'user' || g || '@example.org', 'User ' || g, repeat('x', 60) FROM generate_series(1, 50) g;
    INSERT INTO books (owner_id, title, author, year, status, tags)
        SELECT 1 + g % 50, 'Book ' || g, 'Author ' || (g % 97), 1900 + g % 120,
               (ARRAY['to_read', 'reading', 'done'])[1 + g % 3],
               CASE WHEN g % 3 = 0 THEN '["sf", "classic"]'::jsonb ELSE '["novel"]'::jsonb END
        FROM generate_series(1, 1000) g;
    INSERT INTO reviews (book_id, user_id, rating, body)
        SELECT 1 + g % 1000, 1 + (g / 1000 + 1 + g % 1000) % 50, 1 + g % 5, 'review ' || g
        FROM generate_series(0, 2999) g ON CONFLICT DO NOTHING;
    VACUUM ANALYZE users, books, reviews;
    """
    subprocess.run(["psql", DB, "-q", "-v", "ON_ERROR_STOP=1", "-c", "SET client_min_messages = warning", "-f", "-"],
                   input=sql, text=True, check=True, capture_output=True)


def pin_postgres() -> None:
    if not PG_CPUS:
        return
    pid = sh("head -1 /var/lib/pgpoc/postmaster.pid").strip()
    for p in [pid] + sh(f"pgrep -P {pid} || true").split():
        subprocess.run(["taskset", "-apc", PG_CPUS, p], capture_output=True)


def wait_up(port: int, path: str = "/health") -> None:
    for _ in range(150):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=1)
            return
        except urllib.error.HTTPError:
            return  # it answers
        except Exception:
            time.sleep(0.2)
    raise RuntimeError(f"server on :{port} did not start")


def tree_pids(pid: int) -> list[int]:
    out = [pid]
    for child in sh(f"pgrep -P {pid} || true").split():
        out += tree_pids(int(child))
    return out


def mem_mb(pid: int, field: str) -> float:
    """Sum a /proc status field (VmRSS = current, VmHWM = peak) over the whole process tree."""
    if not Path("/proc").is_dir():  # macOS: current RSS only
        import psutil
        return sum(psutil.Process(p).memory_info().rss for p in tree_pids(pid)) / 2**20
    total = 0
    for p in tree_pids(pid):
        try:
            for line in Path(f"/proc/{p}/status").read_text().splitlines():
                if line.startswith(field + ":"):
                    total += int(line.split()[1])
        except FileNotFoundError:
            pass
    return total / 1024


def oha(port: int, args: list[str], duration: str, conns: int) -> dict:
    target_args = list(args)
    # the URL is the first arg not starting with '-' and not an option value
    if target_args[0] == "--rand-regex-url":
        target_args[1] = f"http://127.0.0.1:{port}{target_args[1]}"
    else:
        target_args[0] = f"http://127.0.0.1:{port}{target_args[0]}"
    cmd = pinned(LOAD_CPUS, ["oha", "-z", duration, "-c", str(conns),
                             "--no-tui", "--output-format", "json", *target_args])
    r = subprocess.run(cmd, capture_output=True, text=True, check=True)
    data = json.loads(r.stdout)
    codes = data.get("statusCodeDistribution", {})
    ok = sum(v for k, v in codes.items() if k.startswith("2"))
    total = sum(codes.values()) or 1
    return {
        "rps": data["summary"]["requestsPerSec"],
        "p50_ms": data["latencyPercentiles"]["p50"] * 1000,
        "p99_ms": data["latencyPercentiles"]["p99"] * 1000,
        "ok_ratio": ok / total,
        "codes": codes,
    }


def compare(results: dict, ref_path: Path, tolerance: float) -> tuple[str, bool]:
    """markdown table of each endpoint against the reference; True when one dropped beyond the tolerance"""
    ref = json.loads(ref_path.read_text()) if ref_path.exists() else {"servers": {}}
    rows = ["| server | endpoint | req/s | reference | change | p99 ms | reference p99 | |",
            "|---|---|---:|---:|---:|---:|---:|---|"]
    bad = False
    for name, srv in results["servers"].items():
        refs = ref["servers"].get(name, {}).get("endpoints", {})
        for ep, m in srv["endpoints"].items():
            r = refs.get(ep)
            if r is None:
                rows.append(f"| {name} | {ep} | {m['rps']:.0f} | – | – | {m['p99_ms']:.2f} | – | no reference |")
                continue
            change = m["rps"] / r["rps"] - 1
            flag = "ok" if change >= -tolerance else f"**below -{tolerance:.0%}**"
            if m["ok_ratio"] < 0.999:
                flag += f" (only {m['ok_ratio']:.1%} 2xx/expected)"
            bad |= change < -tolerance
            rows.append(f"| {name} | {ep} | {m['rps']:.0f} | {r['rps']:.0f} | {change:+.1%} | {m['p99_ms']:.2f} | "
                        f"{r['p99_ms']:.2f} | {flag} |")
        rows.append(f"| {name} | memory | {srv['rss_idle_mb']:.0f} MB idle, {srv['rss_peak_mb']:.0f} MB peak | | | | | |")
    return "\n".join(rows) + "\n", bad


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", default="10s")
    ap.add_argument("--conns", type=int, default=64)
    ap.add_argument("--target", choices=["app", "dynapp", "bookshelf"], default="app",
                    help="app: app/ against FastAPI (APP_BIN); dynapp: fixtures/dynapp's binary alone; "
                         "bookshelf: examples/bookshelf against FastAPI (BOOKSHELF_BIN)")
    ap.add_argument("--out", default=str(ROOT / "bench" / "results.json"))
    ap.add_argument("--compare", help="reference results (same format as --out)")
    ap.add_argument("--tolerance", type=float, default=0.30)
    ap.add_argument("--table", help="write the comparison as a markdown table")
    ap.add_argument("--interleave", type=int, metavar="ROUNDS",
                    help="start every server at once and alternate them, endpoint by endpoint, ROUNDS times (median "
                         "kept): the fair comparison on a machine whose load varies (needs distinct ports)")
    args = ap.parse_args()
    if args.interleave:
        return interleaved(args)

    pin_postgres()
    results: dict = {"config": {"target": args.target, "server_cpus": SERVER_CPUS, "workers": WORKERS,
                                "duration": args.duration, "conns": args.conns,
                                "cpus_available": cpu_count(None)}, "servers": {}}
    only = os.environ.get("ONLY")
    servers, endpoints = {"dynapp": (DYNAPP, DYNAPP_ENDPOINTS), "bookshelf": (BOOKSHELF, BOOKSHELF_ENDPOINTS)}.get(
        args.target, (SERVERS, ENDPOINTS))
    probe = "/tasks/0" if args.target == "dynapp" else "/health"
    for name, cfg in servers.items():
        if only and only.lower() not in name.lower():
            continue
        if args.target == "app":
            seed()
        elif args.target == "bookshelf":
            seed_bookshelf()
        env = {**os.environ, **cfg["env"]}
        proc = subprocess.Popen(
            pinned(SERVER_CPUS, ["sh", "-c", f"exec {cfg['cmd']}"]),
            cwd=ROOT, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            wait_up(cfg["port"], probe)
            if args.target == "dynapp":
                seed_dynapp(cfg["port"])
            time.sleep(1)
            idle = mem_mb(proc.pid, "VmRSS")
            oha(cfg["port"], [probe], "3s", args.conns)  # warm-up
            per = {}
            for ep, ep_args in endpoints.items():
                per[ep] = oha(cfg["port"], ep_args, args.duration, args.conns)
                expected = "404" if "(404)" in ep else "422" if "(422)" in ep else None
                if expected:
                    codes = per[ep]["codes"]
                    per[ep]["ok_ratio"] = codes.get(expected, 0) / (sum(codes.values()) or 1)
                print(f"{name:20} {ep:30} {per[ep]['rps']:>10.0f} req/s  p99 {per[ep]['p99_ms']:7.2f} ms  "
                      f"ok {per[ep]['ok_ratio']:.3f}", flush=True)
            peak = mem_mb(proc.pid, "VmHWM")
            results["servers"][name] = {"endpoints": per, "rss_idle_mb": idle, "rss_peak_mb": peak}
            print(f"{name:20} mémoire: {idle:.0f} Mo au repos, {peak:.0f} Mo en pic", flush=True)
        finally:
            os.killpg(proc.pid, signal.SIGTERM)
            proc.wait(timeout=40)
            time.sleep(1)
    out = Path(args.out)
    out.write_text(json.dumps(results, indent=2, ensure_ascii=False))
    print(f"\nwrote {out}")
    if args.compare:
        table, bad = compare(results, Path(args.compare), args.tolerance)
        print("\n" + table)
        if args.table:
            Path(args.table).write_text(table)
        if bad:
            print(f"throughput below the reference by more than {args.tolerance:.0%} (see the table)")
            raise SystemExit(1)


def interleaved(args) -> None:
    """--interleave: every server up at once, runs alternated per endpoint, the median of ROUNDS runs kept."""
    import statistics

    servers, endpoints = {"dynapp": (DYNAPP, DYNAPP_ENDPOINTS), "bookshelf": (BOOKSHELF, BOOKSHELF_ENDPOINTS)}.get(
        args.target, (SERVERS, ENDPOINTS))
    probe = "/tasks/0" if args.target == "dynapp" else "/health"
    {"app": seed, "bookshelf": seed_bookshelf}.get(args.target, lambda: None)()
    procs = {}
    try:
        for name, cfg in servers.items():
            procs[name] = subprocess.Popen(pinned(SERVER_CPUS, ["sh", "-c", f"exec {cfg['cmd']}"]), cwd=ROOT,
                                           env={**os.environ, **cfg["env"]}, stdout=subprocess.DEVNULL,
                                           stderr=subprocess.DEVNULL, start_new_session=True)
        for cfg in servers.values():
            wait_up(cfg["port"], probe)
        if args.target == "dynapp":
            seed_dynapp(next(iter(servers.values()))["port"])
        time.sleep(1)
        results: dict = {"config": {"target": args.target, "workers": WORKERS, "duration": args.duration,
                                    "conns": args.conns, "rounds": args.interleave, "interleaved": True,
                                    "cpus_available": cpu_count(None)},
                         "servers": {n: {"endpoints": {}, "rss_idle_mb": mem_mb(p.pid, "VmRSS")} for n, p in procs.items()}}
        for cfg in servers.values():
            oha(cfg["port"], [probe], "2s", args.conns)  # warm-up
        for ep, ep_args in endpoints.items():
            runs: dict = {n: [] for n in servers}
            for _ in range(args.interleave):
                for name, cfg in servers.items():
                    runs[name].append(oha(cfg["port"], ep_args, args.duration, args.conns))
            for name, rs in runs.items():
                m = {k: statistics.median(r[k] for r in rs) for k in ("rps", "p50_ms", "p99_ms", "ok_ratio")}
                m["codes"] = rs[-1]["codes"]
                expected = "404" if "(404)" in ep else "422" if "(422)" in ep else None
                if expected:
                    m["ok_ratio"] = m["codes"].get(expected, 0) / (sum(m["codes"].values()) or 1)
                results["servers"][name]["endpoints"][ep] = m
                print(f"{name:20} {ep:40} {m['rps']:>10.0f} req/s  p99 {m['p99_ms']:7.2f} ms  ok {m['ok_ratio']:.3f}",
                      flush=True)
        for name, p in procs.items():
            results["servers"][name]["rss_peak_mb"] = mem_mb(p.pid, "VmHWM")
            print(f"{name:20} memory: {results['servers'][name]['rss_idle_mb']:.0f} MB idle, "
                  f"{results['servers'][name]['rss_peak_mb']:.0f} MB after the runs", flush=True)
    finally:
        for p in procs.values():
            os.killpg(p.pid, signal.SIGTERM)
            p.wait(timeout=40)
    Path(args.out).write_text(json.dumps(results, indent=2, ensure_ascii=False))
    print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()

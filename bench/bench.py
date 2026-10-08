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
        "cmd": "generated/app_axum/target/release/app_axum",
        "env": {"PORT": "8080", "TOKIO_WORKER_THREADS": str(WORKERS), "DB_POOL_SIZE": "32"},
    },
}

# --target dynapp: the dyn backend's reference app, seeded by seed_dynapp (60 tasks, 10 projects of 5 tasks)
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
    ap.add_argument("--target", choices=["app", "dynapp"], default="app",
                    help="app: the typed backend's app against FastAPI; dynapp: the dyn backend's binary alone")
    ap.add_argument("--out", default=str(ROOT / "bench" / "results.json"))
    ap.add_argument("--compare", help="reference results (same format as --out)")
    ap.add_argument("--tolerance", type=float, default=0.30)
    ap.add_argument("--table", help="write the comparison as a markdown table")
    args = ap.parse_args()

    pin_postgres()
    results: dict = {"config": {"target": args.target, "server_cpus": SERVER_CPUS, "workers": WORKERS,
                                "duration": args.duration, "conns": args.conns,
                                "cpus_available": cpu_count(None)}, "servers": {}}
    only = os.environ.get("ONLY")
    servers, endpoints = (DYNAPP, DYNAPP_ENDPOINTS) if args.target == "dynapp" else (SERVERS, ENDPOINTS)
    probe = "/tasks/0" if args.target == "dynapp" else "/health"
    for name, cfg in servers.items():
        if only and only.lower() not in name.lower():
            continue
        if args.target == "app":
            seed()
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
                if args.target == "dynapp" and "(404)" in ep:
                    codes = per[ep]["codes"]
                    per[ep]["ok_ratio"] = codes.get("404", 0) / (sum(codes.values()) or 1)
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


if __name__ == "__main__":
    main()

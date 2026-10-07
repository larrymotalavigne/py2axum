"""Throughput/latency/memory benchmark: FastAPI (Uvicorn), FastAPI (Granian), generated axum.

Fairness: every server gets the same CPU budget. Python servers run one worker per core and
axum one Tokio thread per core. On a large machine, set SERVER_CPUS / PG_CPUS / LOAD_CPUS
(e.g. 0-7 / 8-11 / 12-15) to isolate server, Postgres and load generator; without them nothing
is pinned and all three share the machine.

usage: python bench/bench.py [--duration 10s] [--conns 128]
"""
from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DB = os.environ.get("DATABASE_URL", "postgresql://postgres@127.0.0.1/poc")
SERVER_CPUS = os.environ.get("SERVER_CPUS")
PG_CPUS = os.environ.get("PG_CPUS")
LOAD_CPUS = os.environ.get("LOAD_CPUS")


def cpu_count(spec: str | None) -> int:
    if not spec:
        return len(os.sched_getaffinity(0))
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


def pin_postgres() -> None:
    if not PG_CPUS:
        return
    pid = sh("head -1 /var/lib/pgpoc/postmaster.pid").strip()
    for p in [pid] + sh(f"pgrep -P {pid} || true").split():
        subprocess.run(["taskset", "-apc", PG_CPUS, p], capture_output=True)


def wait_up(port: int) -> None:
    for _ in range(150):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1)
            return
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


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", default="10s")
    ap.add_argument("--conns", type=int, default=64)
    args = ap.parse_args()

    pin_postgres()
    results: dict = {"config": {"server_cpus": SERVER_CPUS, "workers": WORKERS, "duration": args.duration,
                                "conns": args.conns, "cpus_available": len(os.sched_getaffinity(0))}, "servers": {}}
    only = os.environ.get("ONLY")
    for name, cfg in SERVERS.items():
        if only and only.lower() not in name.lower():
            continue
        seed()
        env = {**os.environ, **cfg["env"]}
        proc = subprocess.Popen(
            pinned(SERVER_CPUS, ["sh", "-c", f"exec {cfg['cmd']}"]),
            cwd=ROOT, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            wait_up(cfg["port"])
            time.sleep(1)
            idle = mem_mb(proc.pid, "VmRSS")
            oha(cfg["port"], ["/health"], "3s", args.conns)  # warm-up
            per = {}
            for ep, ep_args in ENDPOINTS.items():
                per[ep] = oha(cfg["port"], ep_args, args.duration, args.conns)
                print(f"{name:20} {ep:30} {per[ep]['rps']:>10.0f} req/s  p99 {per[ep]['p99_ms']:7.2f} ms  "
                      f"ok {per[ep]['ok_ratio']:.3f}", flush=True)
            peak = mem_mb(proc.pid, "VmHWM")
            results["servers"][name] = {"endpoints": per, "rss_idle_mb": idle, "rss_peak_mb": peak}
            print(f"{name:20} mémoire: {idle:.0f} Mo au repos, {peak:.0f} Mo en pic", flush=True)
        finally:
            os.killpg(proc.pid, signal.SIGTERM)
            proc.wait(timeout=20)
            time.sleep(1)
    out = ROOT / "bench" / "results.json"
    out.write_text(json.dumps(results, indent=2, ensure_ascii=False))
    print(f"\nwrote {out}")


if __name__ == "__main__":
    main()

"""Large-response test: GET /users/export?limit=N sized to 5, 10, ... 50 MB of JSON (app/: the list is
streamed from the session's statement, at constant memory; PY2AXUM_STREAM_MIN_ROWS=1e12 buffers it).

For each size and each server (restarted per size so peak memory is per size):
- latency of 3 sequential requests (median),
- throughput with 4 concurrent clients for a few seconds,
- peak RSS of the whole process tree (VmHWM),
- byte-for-byte equality of the two responses (sha256).

usage: python bench/big_responses.py <python package dir> <axum binary>
"""
from __future__ import annotations

import gzip
import hashlib
import json
import os
import signal
import statistics
import subprocess
import sys
import threading
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from bench import mem_mb, wait_up  # noqa: E402

SIZES_MB = [5, 10, 15, 20, 25, 30, 35, 40, 45, 50]
THROUGHPUT_SECONDS = float(os.environ.get("THROUGHPUT_SECONDS", "6"))
CONCURRENCY = 4
GZIP = os.environ.get("GZIP") == "1"
HEADERS = {"Accept-Encoding": "gzip"} if GZIP else {}


def get_timed(url: str, timeout: float = 180) -> tuple[int, bytes, float, float, str | None]:
    """status, body as sent on the wire, time to first body byte, total time, content-encoding."""
    t = time.perf_counter()
    req = urllib.request.Request(url, headers=HEADERS)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        first = r.read(1)
        ttfb = time.perf_counter() - t
        body = first + r.read()
        return r.status, body, ttfb, time.perf_counter() - t, r.headers.get("content-encoding")


def get(url: str, timeout: float = 180) -> tuple[int, bytes, float]:
    status, body, _, total, _ = get_timed(url, timeout)
    return status, body, total


def throughput(url: str) -> tuple[float, int]:
    stop = time.perf_counter() + THROUGHPUT_SECONDS
    done, errors = [0], [0]
    lock = threading.Lock()

    def worker():
        while time.perf_counter() < stop:
            try:
                status, _, _ = get(url)
                with lock:
                    done[0] += status == 200
                    errors[0] += status != 200
            except Exception:
                with lock:
                    errors[0] += 1

    t0 = time.perf_counter()
    threads = [threading.Thread(target=worker) for _ in range(CONCURRENCY)]
    for th in threads:
        th.start()
    for th in threads:
        th.join()
    return done[0] / (time.perf_counter() - t0), errors[0]


def start(cmd: str, cwd: Path, env: dict) -> subprocess.Popen:
    return subprocess.Popen(
        ["sh", "-c", f"exec {cmd}"], cwd=cwd, env={**os.environ, **env},
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
    )


def stop(p: subprocess.Popen) -> None:
    os.killpg(p.pid, signal.SIGTERM)
    p.wait(timeout=30)
    time.sleep(0.5)


def main() -> None:
    pkg, binary = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    servers = {
        "FastAPI + Uvicorn": (8100, f"uvicorn {pkg.name}.main:app --port 8100 --workers 2 --log-level warning --no-access-log",
                              pkg.parent, {"DB_POOL_SIZE": "10"}),
        "axum (généré)": (8180, str(binary), pkg.parent,
                          {"PORT": "8180", "TOKIO_WORKER_THREADS": "2", "DB_POOL_SIZE": "32"}),
    }
    # bytes per row, measured on the axum server
    p = start(servers["axum (généré)"][1], servers["axum (généré)"][2], servers["axum (généré)"][3])
    wait_up(8180)
    _, sample, _, _, enc = get_timed("http://127.0.0.1:8180/users/export?limit=10000")
    if enc == "gzip":
        sample = gzip.decompress(sample)  # calibrate on JSON bytes, not on wire bytes
    stop(p)
    per_row = len(sample) / 10000
    print(f"~{per_row:.0f} octets JSON par ligne", flush=True)

    results = []
    for mb in SIZES_MB:
        limit = int(mb * 1024 * 1024 / per_row)
        row = {"target_mb": mb, "rows": limit, "servers": {}}
        hashes = {}
        for name, (port, cmd, cwd, env) in servers.items():
            proc = start(cmd, cwd, env)
            try:
                wait_up(port)
                url = f"http://127.0.0.1:{port}/users/export?limit={limit}"
                lat, ttfbs, size, wire = [], [], 0, 0
                for _ in range(3):
                    status, body, ttfb, dt, enc = get_timed(url)
                    assert status == 200, status
                    lat.append(dt)
                    ttfbs.append(ttfb)
                    wire = len(body)
                    if enc == "gzip":
                        body = gzip.decompress(body)
                    size = len(body)
                    hashes[name] = hashlib.sha256(body).hexdigest()
                rps, errors = throughput(url)
                peak = mem_mb(proc.pid, "VmHWM")
                row["servers"][name] = {
                    "size_mb": size / 1024 / 1024, "wire_mb": wire / 1024 / 1024,
                    "ttfb_ms": statistics.median(ttfbs) * 1000,
                    "latency_ms": statistics.median(lat) * 1000,
                    "rps": rps, "errors": errors, "peak_rss_mb": peak,
                }
                print(f"{mb:>3} Mo {name:18} {size/1048576:6.1f} Mo (réseau {wire/1048576:5.1f})  "
                      f"1er octet {statistics.median(ttfbs)*1000:6.0f} ms  latence {statistics.median(lat)*1000:7.0f} ms  "
                      f"{rps:6.2f} req/s  erreurs {errors}  pic mémoire {peak:6.0f} Mo", flush=True)
            finally:
                stop(proc)
        row["identical"] = len(set(hashes.values())) == 1
        print(f"{mb:>3} Mo réponses identiques : {row['identical']}", flush=True)
        results.append(row)
    suffix = os.environ.get("OUT_SUFFIX", "_gzip" if GZIP else "")
    out = Path(__file__).parent / f"big_responses{suffix}.json"
    out.write_text(json.dumps({"per_row_bytes": per_row, "results": results}, indent=2, ensure_ascii=False))
    print("DONE", flush=True)


if __name__ == "__main__":
    main()

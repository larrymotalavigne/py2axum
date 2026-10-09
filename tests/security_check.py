"""Hostile inputs against a running binary (fixtures/dynapp, docs/advanced/security.md): the process must survive each
one, answer within bounds and keep serving the others. Not a conformance run: FastAPI is not involved, the
expected behaviours are the binary's own guarantees.

    python tests/security_check.py http://127.0.0.1:8280 [--max-body N]   # N: the server's PY2AXUM_MAX_BODY
"""
import argparse
import concurrent.futures as cf
import json
import socket
import sys
import time
import urllib.error
import urllib.request

FAILS: list[str] = []


def req(base, method, path, body=None, headers=None, timeout=60):
    r = urllib.request.Request(base + path, data=body, method=method, headers=headers or {})
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(r, timeout=timeout) as x:
            return x.status, x.read(), time.monotonic() - t0
    except urllib.error.HTTPError as e:
        return e.code, e.read(), time.monotonic() - t0
    except Exception as e:  # noqa: BLE001
        return type(e).__name__, b"", time.monotonic() - t0


def raw(base, head: bytes, body_parts) -> int | str:
    """A raw HTTP/1.1 exchange (urllib forces `Connection: close` and needs the whole body sent): the head,
    then body parts until the server answers or closes; the status of its answer."""
    host, port = base.split("//", 1)[1].split(":")
    with socket.create_connection((host, int(port)), timeout=30) as s:
        s.sendall(head)
        try:
            for part in body_parts:
                s.sendall(part)
        except OSError:
            pass  # answered and closed before the end of the body: read that answer
        data = b""
        try:
            while b"\r\n" not in data:
                chunk = s.recv(4096)
                if not chunk:
                    break
                data += chunk
        except OSError as e:
            return type(e).__name__
    line = data.split(b"\r\n", 1)[0].split()
    return int(line[1]) if len(line) > 1 else "no answer"


def check(name, ok, detail=""):
    print(("ok  " if ok else "FAIL") + f" {name} {detail}")
    if not ok:
        FAILS.append(name)


def alive(base):
    return req(base, "GET", "/tasks/0", timeout=10)[0] in (200, 404)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("base")
    ap.add_argument("--max-body", type=int, default=0)
    a = ap.parse_args()
    base = a.base.rstrip("/")
    js = {"content-type": "application/json"}

    # 1. nesting chosen by the request: never a stack overflow (it would abort the process)
    for depth in (1000, 5000, 9999, 50000):
        for path in ("/sec/echo", "/sec/model", "/colls/render"):
            for body in ("[" * depth + "]" * depth, '{"a":' * depth + "1" + "}" * depth):
                st, _, _ = req(base, "POST", path, body.encode(), js)
                check(f"deep json {path} depth={depth}", st in (200, 400, 422, 500) and alive(base), str(st))

    # 2. a Cookie header of many short cookies: parsed in linear time
    cookie = "; ".join(f"c{i}=v" for i in range(30000))
    st, b, dt = req(base, "GET", "/sec/cookies", headers={"cookie": cookie})
    check("many cookies", st in (200, 431) and dt < 5 and alive(base), f"{st} {dt:.2f}s")

    # 3. multipart part counts above Starlette's limits: refused, not parsed to the end
    bd = "b0undary"
    parts = b"".join(f'--{bd}\r\nContent-Disposition: form-data; name="files"; filename="{i}"\r\n\r\nx\r\n'.encode() for i in range(5000))
    st, b, _ = req(base, "POST", "/sec/upload", parts + f"--{bd}--\r\n".encode(), {"content-type": f"multipart/form-data; boundary={bd}"})
    check("5000 files", st == 400 and b"Too many files" in b, str(st))

    # 4. bcrypt at cost 12 on every worker at once: other requests still answered promptly
    with cf.ThreadPoolExecutor(8) as ex:
        hashes = [ex.submit(req, base, "GET", "/sec/hash") for _ in range(8)]
        time.sleep(0.2)
        lat = []
        for _ in range(5):
            st, _, dt = req(base, "GET", "/tasks/0", timeout=10)
            lat.append(dt)
        worst = max(lat)
        check("bcrypt off the loop", all(h.result()[0] == 200 for h in hashes) and worst < 0.5, f"worst {worst:.3f}s")

    # 5. sizes chosen by the request
    for n in (2**62, 10**15, -1):
        st, b, _ = req(base, "GET", f"/sec/repeat?n={n}")
        check(f"repeat n={n}", st == 200 and alive(base), b[:60].decode(errors="replace"))
    st, b, _ = req(base, "GET", "/sec/token?n=-1")
    check("token_urlsafe(-1)", st == 200 and b"ValueError" in b)

    # 6. the python-side relay: hop-by-hop headers and those named by Connection are not forwarded
    import http.client
    host, port = base.split("//", 1)[1].split(":")
    c = http.client.HTTPConnection(host, int(port), timeout=30)
    c.putrequest("POST", "/dunders/proxied/x", skip_accept_encoding=True)
    for k, v in (("connection", "keep-alive, x-probe"), ("x-probe", "1"), ("content-length", "0")):
        c.putheader(k, v)
    c.endheaders()
    b = c.getresponse().read()
    c.close()
    check("proxy drops Connection-named headers", json.loads(b).get("ua") is None, b[:80].decode())
    st, b, _ = req(base, "POST", "/dunders/proxied/x", b"", {"x-probe": "1"})
    check("proxy relays other headers", st == 200 and json.loads(b).get("ua") == "1", b[:80].decode())

    # 7. CR/LF from the request in a response header: a plain 500, nothing injected
    r = urllib.request.Request(base + "/sec/header?v=a%0d%0aSet-Cookie:%20x=1")
    try:
        with urllib.request.urlopen(r, timeout=10) as x:
            st, hs = x.status, x.headers
    except urllib.error.HTTPError as e:
        st, hs = e.code, e.headers
    check("header injection", st == 500 and "set-cookie" not in {k.lower() for k in hs.keys()}, str(st))

    # 8. a 500 says nothing
    st, b, _ = req(base, "GET", "/sec/boom")
    check("500 body", st == 500 and b == b"Internal Server Error", b[:60].decode())

    # 9. PY2AXUM_MAX_BODY, when the server runs with it
    if a.max_body:
        big = b"[" + b"1," * a.max_body + b"1]"
        parts = [big[i:i + 65536] for i in range(0, len(big), 65536)]
        head = b"POST /sec/echo HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\n"
        st = raw(base, head + f"content-length: {len(big)}\r\n\r\n".encode(), parts)
        check("max body (content-length)", st == 413 and alive(base), str(st))
        st = raw(base, head + b"transfer-encoding: chunked\r\n\r\n", [b"%x\r\n%s\r\n" % (len(p), p) for p in parts] + [b"0\r\n\r\n"])
        check("max body (chunked)", st == 413 and alive(base), str(st))
        st, _, _ = req(base, "POST", "/sec/echo", b"[1]", js)
        check("under max body", st == 200, str(st))

    check("still serving", alive(base))
    print(f"{len(FAILS)} failure(s)")
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()

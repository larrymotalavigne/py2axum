"""A fake Sentry server for the Sentry comparison (tests/sentry_check.py): it accepts what the SDKs send
(`/api/<project>/envelope/`, the legacy `/api/<project>/store/`) and keeps the items by project, so the
reference (project 1) and the binary (project 2) can be read back and compared.

    python tests/sentry_sink.py 8799
    GET    /_items/<project>   -> [{"type": ..., "headers": {...}, "payload": {...}, "auth": ...}, ...]
    DELETE /_items/<project>
"""
import gzip
import json
import re
import sys
import threading
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ITEMS: dict[str, list] = {}
LOCK = threading.Lock()


def parse_envelope(raw: bytes) -> list[dict]:
    """Sentry envelope: a header line, then (item header line, payload) pairs; the payload length is
    the item header's `length` when present, else up to the next newline."""
    lines = raw.split(b"\n", 1)
    rest = lines[1] if len(lines) > 1 else b""
    out = []
    while rest.strip():
        head, _, rest = rest.partition(b"\n")
        if not head.strip():
            continue
        h = json.loads(head)
        if "length" in h:
            payload, rest = rest[: h["length"]], rest[h["length"]:]
            rest = rest[1:] if rest.startswith(b"\n") else rest
        else:
            payload, _, rest = rest.partition(b"\n")
        try:
            body = json.loads(payload) if payload else None
        except ValueError:
            body = payload.decode("utf-8", "replace")
        out.append({"type": h.get("type"), "headers": h, "payload": body})
    return out


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, code: int, body: bytes = b"{}", ctype: str = "application/json"):
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length") or 0))
        enc = (self.headers.get("content-encoding") or "").lower()
        if enc == "gzip":
            raw = gzip.decompress(raw)
        elif enc == "deflate":
            raw = zlib.decompress(raw)
        elif enc == "br":
            import brotli

            raw = brotli.decompress(raw)
        m = re.fullmatch(r"/api/(\d+)/(envelope|store)/", self.path.split("?")[0])
        if not m:
            return self._send(404)
        auth = self.headers.get("x-sentry-auth") or self.path.partition("?")[2]
        if m[2] == "envelope":
            items = parse_envelope(raw)
        else:
            items = [{"type": "event", "headers": {}, "payload": json.loads(raw)}]
        for it in items:
            it["auth"] = auth
            it["endpoint"] = m[2]
        with LOCK:
            ITEMS.setdefault(m[1], []).extend(items)
        self._send(200, b'{"id":"0"}')

    def do_GET(self):
        m = re.fullmatch(r"/_items/(\d+)", self.path)
        if not m:
            return self._send(404)
        with LOCK:
            body = json.dumps(ITEMS.get(m[1], [])).encode()
        self._send(200, body)

    def do_DELETE(self):
        m = re.fullmatch(r"/_items/(\d+)", self.path)
        if not m:
            return self._send(404)
        with LOCK:
            ITEMS.pop(m[1], None)
        self._send(200)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8799
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()

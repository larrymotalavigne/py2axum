"""Differential testing beyond the hand-written scenarios: the same requests against FastAPI (REF) and the
generated binary (CAND), each on its own database restored from the same snapshot, compared with the rules of
tests/conformance.py (masked instants, key order, 422 bodies, cookies, middleware headers).

    python tests/difftest.py prepare  --snapshot SRC --db REF_DB --db CAND_DB
    python tests/difftest.py replay   REC.jsonl --ref URL --cand URL --ref-db DB --cand-db DB --snapshot SRC
    python tests/difftest.py gen      --ref URL --cand URL --ref-db DB --cand-db DB --snapshot SRC --seed 1

SRC is a database URL or a `pg_dump -Fc` file (client tools from PG_BIN when set). Target databases must have `_replay` in their name: they are
emptied and refilled from the snapshot (data only: the servers may keep running, their prepared plans stay valid).
The dump is written to a temporary file removed at exit.

replay: a recording of py2axum/record.py (anonymized JSONL). Each server plays the whole sequence on its freshly
restored database (Redis flushed with --flush-redis before each pass); the credentials a server hands out
(cookies, JSON tokens) replace their pseudonyms in the requests that follow, per server. Static credentials
that exist in the snapshot (an API token) are given with --secret VALUE --record-key KEY: the pseudonym the
recorder made of VALUE is replaced by it. --minimize: delta debugging of the prefix of the first divergence,
printed as scenario steps.

gen: requests generated from the reference's OpenAPI by schemathesis (positive and negative modes) under a fixed
hypothesis seed, plus a deterministic corpus of edge cases per operation (bounds, wrong types, missing and extra
fields, unicode, NaN, lone surrogates, a 6 MB body, odd path parameters). Each request goes to REF then CAND; a
divergence is shrunk by hypothesis to a minimal request and both databases are restored before the next one.

Common options: --scenario NAME borrows the masks of tests/scenarios/NAME.py, or of a scenario file given by its
path (normalize, COOKIE_MASKS, HEADER_MASKS, SETTLE); --out DIR writes divergences.jsonl and summary.md there; --ignore-encoding as in
conformance.py.
"""
from __future__ import annotations

import argparse
import atexit
import base64
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import types
from pathlib import Path
from urllib.parse import urlencode, urlsplit

import httpx

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from tests.conformance import client, exchange, identical, sent_datetimes  # noqa: E402
from tests.conformance import load_scenario as _scenario_module  # noqa: E402

TOKEN = re.compile(r"py2axum-tok-[0-9a-f]{12}")


# --------------------------------------------------------------------------------------------- databases

def _pg(url: str) -> str:
    return url.replace("postgresql+psycopg://", "postgresql://", 1).replace("postgresql+asyncpg://", "postgresql://", 1)


def _dbname(url: str) -> str:
    return urlsplit(_pg(url)).path.lstrip("/")


def _bin(name: str) -> str:
    """PostgreSQL client tools: PG_BIN when the default ones are older than the server (pg_dump refuses)."""
    return os.path.join(os.environ.get("PG_BIN", ""), name)


def _guard(url: str) -> None:
    if "_replay" not in _dbname(url):
        sys.exit(f"refusing to overwrite {_dbname(url)!r}: replay databases must have `_replay` in their name")


class Snapshot:
    """A data snapshot (pg_dump -Fc, temporary) restored into replay databases."""

    def __init__(self, src: str):
        self.dir = tempfile.mkdtemp(prefix="py2axum-snap-")
        atexit.register(shutil.rmtree, self.dir, True)
        if os.path.isfile(src):
            self.dump = src
        else:
            self.dump = os.path.join(self.dir, "snap.dump")
            subprocess.run([_bin("pg_dump"), "-Fc", "-f", self.dump, _pg(src)], check=True)

    def create(self, target: str) -> None:
        """(Re)create `target` with the snapshot's schema and data. No server may be connected to it."""
        _guard(target)
        name = _dbname(target)
        admin = _pg(target).rsplit("/", 1)[0] + "/postgres"
        subprocess.run([_bin("psql"), "-q", "-v", "ON_ERROR_STOP=1", admin, "-c", f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)',
                        "-c", f'CREATE DATABASE "{name}"'], check=True, capture_output=True)
        subprocess.run([_bin("pg_restore"), "--no-owner", "--no-acl", "-d", _pg(target), self.dump], check=True)

    def restore(self, target: str) -> None:
        """Empty every table of `target` and reload the snapshot's rows and sequences (schema untouched)."""
        import psycopg

        _guard(target)
        with psycopg.connect(_pg(target), autocommit=True) as conn:
            tables = [f'"{s}"."{t}"' for s, t in conn.execute(
                "SELECT schemaname, tablename FROM pg_tables WHERE schemaname NOT IN ('pg_catalog', 'information_schema')")]
            if tables:
                conn.execute(f"TRUNCATE {', '.join(tables)} RESTART IDENTITY CASCADE")
        subprocess.run([_bin("pg_restore"), "--data-only", "--disable-triggers", "--single-transaction", "--no-owner",
                        "-d", _pg(target), self.dump], check=True, capture_output=True)


def flush_redis(url: str | None) -> None:
    if url:
        import redis

        redis.Redis.from_url(url).flushdb()


# --------------------------------------------------------------------------------------------- sides

def load_scenario(name: str | None, settle: float | None = None):
    scenario = _scenario_module(name) if name else types.SimpleNamespace()
    if settle is not None:
        scenario = types.SimpleNamespace(**{k: getattr(scenario, k) for k in dir(scenario) if not k.startswith("__")})
        scenario.SETTLE = settle
    return scenario


class ServerDown(Exception):
    pass


class Side:
    """One server: its base URL, database, and the credentials it handed out (pseudonym -> its own value)."""

    def __init__(self, name: str, base: str, db: str, scenario, jar: bool, headers: dict):
        self.name, self.base, self.db, self.scenario, self.jar = name, base, db, scenario, jar
        self.headers = headers
        self.c = client(base)
        self.secrets: dict[str, str] = {}  # static credentials valid in the snapshot (--secret)
        self.tokens: dict[str, str] = {}
        self.issued: list = []

    def reset_session(self) -> None:
        self.c.cookies.clear()
        self.tokens = dict(self.secrets)

    def learn(self, r: httpx.Response) -> None:
        for kind, where, pseudo in self.issued:
            if kind == "cookie":
                value = r.cookies.get(where)
                if value is None:  # a Set-Cookie httpx did not keep (domain, path): read it raw
                    for raw in r.headers.get_list("set-cookie"):
                        name, _, rest = raw.partition("=")
                        if name.strip() == where:
                            value = rest.split(";", 1)[0].strip().strip('"')
            else:
                try:
                    value = _pointer(r.json(), where)
                except (ValueError, LookupError, TypeError):
                    value = None  # not handed out this time (an error answer): the pseudonym stays unknown
            if isinstance(value, str):
                self.tokens[pseudo] = value

    def subst(self, v):
        if isinstance(v, str):
            return TOKEN.sub(lambda m: self.tokens.get(m.group(0), m.group(0)), v)
        if isinstance(v, (list, tuple)):
            return type(v)(self.subst(x) for x in v)
        if isinstance(v, dict):
            return {self.subst(k): self.subst(x) for k, x in v.items()}
        return v

    def play(self, step, sent: set[str], issued=()) -> dict | None:
        self.issued = list(issued)
        method, path, payload = step[:3]
        headers = {**self.headers, **(dict(step[3]) if len(step) > 3 else {})}
        if isinstance(payload, bytes) and TOKEN.search(payload.decode("utf-8", "replace")):
            payload = self.subst(payload.decode()).encode()
        step = (method, self.subst(path), payload if isinstance(payload, bytes) else self.subst(payload), self.subst(headers))
        if not self.jar:
            self.c.cookies.clear()
        try:
            out = exchange(self.c, self.base, self.scenario, step, sent, db=self.db, on_response=self.learn)
        except httpx.ConnectError as e:
            # the server is gone: every later answer would be "identical" connection errors
            raise ServerDown(f"{self.name} ({self.base}) refused the connection on {method} {step[1][:100]}: {e}") from e
        except httpx.HTTPError as e:  # a stream that never ends, a dropped connection: compared as such
            self.c.close()
            self.c = client(self.base)
            out = {"req": f"{method} {step[1]}", "error": type(e).__name__}
        if not self.jar:
            self.c.cookies.clear()
        return out


def _pointer(doc, pointer: str):
    for part in pointer.split("/")[1:]:
        part = part.replace("~1", "/").replace("~0", "~")
        doc = doc[int(part)] if isinstance(doc, list) else doc[part]
    return doc


# --------------------------------------------------------------------------------------------- report

BIG_INT = re.compile(r"(?<![\d.eE])-?\d{19,}(?![\d.eE])")
LONE_SURROGATE = re.compile(r"\\u[dD][89a-fA-F][0-9a-fA-F]{2}")


def known_difference(step, a: dict | None, b: dict | None) -> str | None:
    """A divergence docs/supported.md documents: counted apart, not reported as a failure."""
    if a and b and a.get("status") == b.get("status"):
        # str() of a SQLAlchemy DBAPIError: the binary stops before `[SQL: ...]` (docs/supported.md)
        ra = re.sub(r"\\n\[SQL: .*?(?=\")", "", json.dumps(a.get("body"), ensure_ascii=False))
        if "[SQL: " in json.dumps(a.get("body"), ensure_ascii=False) and ra == json.dumps(b.get("body"), ensure_ascii=False):
            return "database error message without SQLAlchemy's [SQL: ...] lines"
    payload = step[2].decode("utf-8", "replace") if isinstance(step[2], bytes) else json.dumps(step[2])
    if a and b and LONE_SURROGATE.search(payload) and a.get("status") != b.get("status") \
            and not re.search(r"\\u[dD][89abAB][0-9a-fA-F]{2}\\u[dD][c-fC-F]", payload):
        return "lone surrogate in a JSON string"  # U+FFFD in the binary, an error later in CPython
    if not (a and b) or b.get("status") != 500 or a.get("status") == 500 and a.get("body") == b.get("body"):
        return None
    text = step[1] + " " + payload
    ref_body = json.dumps(a.get("body"))
    if any(abs(int(m)) > 2**63 - 1 for m in BIG_INT.findall(text + " " + ref_body)):
        return "integer outside the signed 64-bit range"
    if LONE_SURROGATE.search(payload) and not re.search(r"\\u[dD][89abAB][0-9a-fA-F]{2}\\u[dD][c-fC-F]", payload):
        return "lone surrogate in a JSON string"
    return None


class Report:
    def __init__(self, out: str | None, ignore_encoding: bool):
        self.out = Path(out) if out else None
        self.ignore_encoding = ignore_encoding
        self.items: list[dict] = []
        self.known: dict[str, int] = {}
        self.total = 0
        self.mismatch = False
        if self.out:
            self.out.mkdir(parents=True, exist_ok=True)
            (self.out / "divergences.jsonl").write_text("")

    def diff(self, label: str, step, a: dict | None, b: dict | None) -> dict | None:
        """The divergence between two observations, None when they match (counted either way)."""
        if a is None and b is None:
            return None
        self.total += 1
        self.mismatch = False
        if a is not None and b is not None and identical(a, b, self.ignore_encoding):
            return None
        self.mismatch = True  # documented or not, the two databases may now differ
        why = known_difference(step, a, b)
        if why:
            self.known[why] = self.known.get(why, 0) + 1
            return None
        return {"label": label, "step": _printable(step), "ref": a, "cand": b}

    def add(self, item: dict) -> None:
        self.items.append(item)
        if self.out:
            with open(self.out / "divergences.jsonl", "a") as f:
                f.write(json.dumps(item, ensure_ascii=False, default=repr) + "\n")

    def compare(self, label: str, step, a: dict | None, b: dict | None) -> bool:
        item = self.diff(label, step, a, b)
        if item:
            self.add(item)
        return item is None

    def finish(self, title: str) -> int:
        same = self.total - len(self.items) - sum(self.known.values())
        lines = [f"# {title}", "", f"{same}/{self.total} identical responses", ""]
        for why, n in sorted(self.known.items()):
            lines.append(f"- documented difference ({why}): {n}")
        lines.append("")
        groups: dict[tuple, list] = {}
        for it in self.items:
            a, b = it["ref"] or {}, it["cand"] or {}
            key = (it["label"].split(" #")[0], a.get("status"), b.get("status"), _first_diff(a, b))
            groups.setdefault(key, []).append(it)
        for (label, sa, sb, where), its in sorted(groups.items(), key=lambda kv: -len(kv[1])):
            lines.append(f"- **{label}** ref {sa} / cand {sb}, differs in `{where}` ({len(its)}x)")
            lines.append(f"  - step: `{its[0]['step']}`")
            for side in ("ref", "cand"):
                lines.append(f"  - {side}: `{json.dumps(its[0][side], ensure_ascii=False, default=repr)[:600]}`")
        text = "\n".join(lines) + "\n"
        if self.out:
            (self.out / "summary.md").write_text(text)
        print(text)
        return 1 if self.items else 0


def _first_diff(a: dict, b: dict) -> str:
    for k in dict.fromkeys([*a, *b]):
        if json.dumps(a.get(k), default=repr) != json.dumps(b.get(k), default=repr):
            return k
    return "-"


def _printable(step) -> str:
    """The step as a Python literal to paste into tests/scenarios/<name>.py (bodies over 2 KiB abbreviated)."""
    method, path, payload = step[:3]
    if isinstance(payload, bytes) and len(payload) > 2048:
        payload = f"<{len(payload)} bytes: {payload[:200]!r}...>"
    text = repr((method, path, payload, *step[3:]))
    return text if len(text) < 8000 else text[:8000] + "..."


# --------------------------------------------------------------------------------------------- replay

def load_records(path: str) -> tuple[dict, list[dict]]:
    with open(path) as f:
        lines = [json.loads(line) for line in f if line.strip()]
    header = lines[0] if lines and "format" in lines[0] else {}
    if header.get("format") != "py2axum-replay" or header.get("version") != 1:
        sys.exit(f"{path}: not a py2axum-replay v1 recording")
    return header, sorted((r for r in lines[1:] if "method" in r), key=lambda r: (r["t"], r["pid"]))


def record_step(rec: dict):
    """A recorded request as a conformance step, or None when its body was not kept."""
    body, payload = rec.get("body"), None
    headers = {k: v for k, v in rec["headers"]}
    if body:
        if "omitted" in body:
            return None
        if "json" in body:
            payload = json.dumps(body["json"]).encode()
        elif "form" in body:
            payload = urlencode([tuple(kv) for kv in body["form"]]).encode()
        elif "text" in body:
            payload = body["text"].encode()
        elif "b64" in body:
            payload = base64.b64decode(body["b64"])
    if rec["cookies"]:
        headers["cookie"] = "; ".join(f"{n}={v}" for n, v in rec["cookies"])
    path = rec["path"] + ("?" + rec["query"] if rec["query"] else "")
    return (rec["method"], path, payload, headers)


def _unknown_cookies(side: Side, step):
    """Cookies whose pseudonym this server never handed out (a session opened before the recording) are not
    sent: their real value is not in the recording."""
    headers = dict(step[3])
    if "cookie" in headers:
        kept = [p for p in headers["cookie"].split("; ") if not (TOKEN.search(p) and TOKEN.search(p).group(0) not in side.tokens)]
        if kept:
            headers["cookie"] = "; ".join(kept)
        else:
            del headers["cookie"]
    return (*step[:3], headers)


def play_sequence(side: Side, recs: list[dict], sent: set[str], snapshot: Snapshot | None, redis_url: str | None) -> list:
    if snapshot:
        snapshot.restore(side.db)
    flush_redis(redis_url)
    side.reset_session()
    out = []
    for rec in recs:
        step = record_step(rec)
        if step is None:
            out.append(None)
            continue
        out.append(side.play(_unknown_cookies(side, step), sent, rec.get("issued", ())))
    return out


def cmd_replay(a) -> int:
    header, recs = load_records(a.recording)
    scenario = load_scenario(a.scenario, a.settle)
    snapshot = Snapshot(a.snapshot) if a.snapshot else None
    ref = Side("ref", a.ref, a.ref_db, scenario, jar=False, headers={})
    cand = Side("cand", a.cand, a.cand_db, scenario, jar=False, headers={})
    if a.secret and not a.record_key:
        sys.exit("--secret needs --record-key (the PY2AXUM_RECORD_KEY of the recording)")
    for value in a.secret:
        from py2axum.record import Anonymizer

        pseudo = Anonymizer(a.record_key.encode()).token(value)
        ref.secrets[pseudo] = cand.secrets[pseudo] = value
    if a.limit:
        recs = recs[:a.limit]
    steps = [s for s in map(record_step, recs) if s]
    sent = sent_datetimes(steps)
    print(f"{len(recs)} requests ({len(recs) - len(steps)} without a recorded body, skipped)", file=sys.stderr)
    ra = play_sequence(ref, recs, sent, snapshot, a.flush_redis)
    rb = play_sequence(cand, recs, sent, snapshot, a.flush_redis)
    report = Report(a.out, a.ignore_encoding)
    first = None
    for i, (rec, x, y) in enumerate(zip(recs, ra, rb)):
        if not report.compare(f"{rec['method']} {rec['path']} #{i}", record_step(rec) or (rec["method"], rec["path"], None), x, y) and first is None:
            first = i
    code = report.finish(f"replay of {a.recording}")
    if first is not None and a.minimize:
        if not snapshot:
            sys.exit("--minimize needs --snapshot (each trial starts from it)")
        minimal = minimize(recs[:first + 1], lambda sub: _diverges(sub, ref, cand, sent, snapshot, a), a.minimize)
        print("minimal sequence reproducing the first divergence (scenario steps):")
        for rec in minimal:
            print("    " + _printable(record_step(rec)) + ",")
    return code


def _diverges(recs, ref: Side, cand: Side, sent, snapshot, a) -> bool:
    x = play_sequence(ref, recs, sent, snapshot, a.flush_redis)[-1]
    y = play_sequence(cand, recs, sent, snapshot, a.flush_redis)[-1]
    return not (x is None and y is None) and not (x is not None and y is not None and identical(x, y, a.ignore_encoding))


def minimize(recs: list, diverges, budget: int) -> list:
    """ddmin over the prefix: the last request (the divergent one) is always kept."""
    head, last = recs[:-1], recs[-1]
    n, trials = 2, 0
    if diverges([last]):
        return [last]
    while len(head) >= 1 and trials < budget:
        chunk = max(1, len(head) // n)
        parts = [head[i:i + chunk] for i in range(0, len(head), chunk)]
        reduced = False
        for i in range(len(parts)):
            trials += 1
            rest = [r for j, p in enumerate(parts) if j != i for r in p]
            if diverges(rest + [last]):
                head, n, reduced = rest, max(n - 1, 2), True
                break
            if trials >= budget:
                break
        if not reduced:
            if chunk == 1:
                break
            n = min(len(head), n * 2)
    return head + [last]


# --------------------------------------------------------------------------------------------- generation

HUGE = 6 * 1024 * 1024
ODD_PATH = ["0", "-1", "2147483648", "9223372036854775808", "1.0", "1e3", "abc", "%2F", "%C3%A9", "%00", " ", "x" * 5000]
ODD_INT = [0, -1, 2**31, 2**63, 2**64, -(2**63) - 1, 1.0, 1.5, "1", True, None]
ODD_STR = ["", " ", "\u0000", "é", "é", "😀", "‮abc", "ǅ", "ß", "İ", "a" * 10000, 1, None, True, []]


def edge_cases(op: dict, example) -> list[tuple[str, tuple]]:
    """A deterministic corpus for one operation, built around one valid example (method, path, query, body)."""
    method, path, query, body, headers = example
    out: list[tuple[str, tuple]] = []

    def step(p=path, q=query, b=body, h=headers, raw: bytes | None = None):
        url = p + ("?" + urlencode(q, doseq=True) if q else "")
        return (method, url, raw if raw is not None else b, h)

    for name, value in (op.get("path_params") or {}).items():
        for odd in ODD_PATH:
            out.append((f"path {name}={odd[:20]}", step(p=path.replace(value, odd, 1))))
    for name, sch in (op.get("query_params") or {}).items():
        for odd in (ODD_INT if sch.get("type") in ("integer", "number") else ODD_STR[:10]):
            q = dict(query or {})
            q[name] = json.dumps(odd) if not isinstance(odd, str) else odd
            out.append((f"query {name}={str(odd)[:20]}", step(q=q)))
        q = list((query or {}).items()) + [(name, "1"), (name, "2")]
        out.append((f"query {name} repeated", step(q=q)))
    out.append(("query unknown", step(q={**(query or {}), "zz_unknown": "1"})))
    if op.get("has_body"):
        out += [("body empty", step(raw=b"")), ("body null", step(raw=b"null")), ("body []", step(raw=b"[]")),
                ("body invalid", step(raw=b"{")), ("body trailing", step(raw=b"{} x")),
                ("body bom", step(raw=b"\xef\xbb\xbf" + json.dumps(body).encode())),
                ("body NaN", step(raw=b'{"x": NaN}')), ("body 1e309", step(raw=b'{"x": 1e309}')),
                ("body lone surrogate", step(raw=b'{"x": "\\ud800"}')),
                ("body latin-1", step(raw=json.dumps(body, ensure_ascii=False).encode("latin-1", "replace"))),
                ("body text/plain", step(h={**headers, "content-type": "text/plain"})),
                ("body no content-type", step(h={**headers, "content-type": ""})),
                ("body 6 MB", step(raw=b'{"x": "' + b"a" * HUGE + b'"}'))]
        if isinstance(body, dict):
            out.append(("body dup key", step(raw=(json.dumps(body)[:-1] + (", " if body else "") + json.dumps(next(iter(body), "k")) + ": 1}").encode())))
            out.append(("body extra field", step(b={**body, "zz_extra": 1})))
            for k, v in body.items():
                rest = {x: y for x, y in body.items() if x != k}
                out.append((f"body -{k}", step(b=rest)))
                pool = ODD_INT if isinstance(v, (int, float)) and not isinstance(v, bool) else ODD_STR
                for odd in pool:
                    out.append((f"body {k}={str(odd)[:20]!s}", step(b={**body, k: odd})))
                if isinstance(v, (int, float)) and not isinstance(v, bool):
                    for raw in (b"-0.0", b"1E2", b"0.1e1", b"00", b"1_000"):
                        out.append((f"body {k}={raw.decode()}", step(raw=json.dumps({**body, k: "@@"}).replace('"@@"', raw.decode()).encode())))
    return out


def _case_step(case) -> tuple | None:
    """A schemathesis Case as a conformance step (method, path?query, payload, headers)."""
    path = case.formatted_path
    q = case.query or {}
    if q:
        path += "?" + urlencode([(k, v if isinstance(v, str) else json.dumps(v) if not isinstance(v, list) else v)
                                 for k, v in q.items()], doseq=True)
    headers = {k: v for k, v in (case.headers or {}).items() if isinstance(v, str)}
    body = case.body
    media = case.media_type
    payload = None
    if body is not None and not (type(body).__name__ == "NotSet"):
        if media and "json" not in media:
            if media == "application/x-www-form-urlencoded" and isinstance(body, dict):
                payload = urlencode(body, doseq=True).encode()
            elif isinstance(body, (str, bytes)):
                payload = body.encode() if isinstance(body, str) else body
            else:
                return None  # multipart: not generated (files)
            headers["content-type"] = media
        elif isinstance(body, (bytes, bytearray)):
            payload = bytes(body)
        else:
            payload = json.dumps(body).encode()
    if case.cookies:
        headers["cookie"] = "; ".join(f"{k}={v}" for k, v in case.cookies.items())
    try:
        httpx.Headers(headers)
        for v in headers.values():
            v.encode("ascii")
    except (UnicodeEncodeError, ValueError, TypeError):
        return None
    return (case.method.upper(), path, payload, headers)


def _op_info(op) -> dict:
    d = op.definition.raw if hasattr(op.definition, "raw") else op.definition
    params = d.get("parameters", [])
    return {"path_params": {}, "query_params": {p["name"]: p.get("schema", {}) for p in params if p.get("in") == "query"},
            "path_names": [p["name"] for p in params if p.get("in") == "path"], "has_body": "requestBody" in d}


def cmd_gen(a) -> int:
    import schemathesis
    from hypothesis import HealthCheck, Phase, given, reject, seed, settings
    from hypothesis.errors import Unsatisfiable

    scenario = load_scenario(a.scenario, a.settle)
    snapshot = Snapshot(a.snapshot) if a.snapshot else None
    extra = dict(h.split(":", 1) for h in a.header)
    extra = {k.strip(): v.strip() for k, v in extra.items()}
    ref = Side("ref", a.ref, a.ref_db, scenario, jar=True, headers=extra)
    cand = Side("cand", a.cand, a.cand_db, scenario, jar=True, headers=extra)
    report = Report(a.out, a.ignore_encoding)

    def restore():
        if snapshot:
            snapshot.restore(ref.db)
            snapshot.restore(cand.db)
        flush_redis(a.flush_redis)
        for side in (ref, cand):
            side.reset_session()
            for s in getattr(scenario, "STEPS", [])[:a.setup]:
                side.play(s, set())

    restore()
    spec = httpx.get(a.ref + a.openapi, headers=extra, timeout=30).json()
    schema = schemathesis.openapi.from_dict(spec)
    inc, exc = re.compile(a.include), re.compile(a.exclude) if a.exclude else None
    ops = []
    for res in schema.get_all_operations():
        op = res.ok()
        label = f"{op.method.upper()} {op.path}"
        if inc.search(label) and not (exc and exc.search(label)):
            ops.append(op)
    print(f"{len(ops)} operations", file=sys.stderr)
    modes = [schemathesis.GenerationMode(m) for m in a.modes.split(",") if m in ("positive", "negative")]

    def send(label: str, step, keep: bool = True) -> dict | None:
        sent = sent_datetimes([step])
        item = report.diff(label, step, ref.play(step, sent), cand.play(step, sent))
        if item and keep:
            report.add(item)
        if report.mismatch:
            restore()  # the databases may have diverged: start the next request from the snapshot again
        return item

    for k, op in enumerate(ops):
        label = f"{op.method.upper()} {op.path}"
        info = _op_info(op)
        first_valid: list = []
        for mode in modes:
            failing: list = []
            since: list = []  # when the first divergence of this mode was found

            @seed(a.seed * 1000003 + k * 31 + modes.index(mode))
            @settings(max_examples=a.max_examples, database=None, deadline=None, derandomize=False,
                      suppress_health_check=list(HealthCheck), phases=[Phase.generate, Phase.shrink], report_multiple_bugs=False)
            @given(op.as_strategy(generation_mode=mode))
            def check(case):
                if since and time.monotonic() - since[0] > a.shrink_time:
                    return  # shrinking budget spent: keep the smallest divergence found so far
                step = _case_step(case)
                if step is None:
                    reject()
                if mode.value == "positive" and not first_valid:
                    first_valid.append((case, step))
                item = send(f"{label} [{mode.value}]", step, keep=False)
                if item:
                    failing[:] = [item]  # the last one is the smallest (hypothesis shrinks towards it)
                    since[:] = since or [time.monotonic()]
                    raise AssertionError("divergence")

            try:
                check()
            except AssertionError:
                pass
            except Unsatisfiable:
                pass  # nothing to vary (no parameter, no body): the positive mode covered it
            except ServerDown:
                raise
            except Exception as e:  # noqa: BLE001 - a schema schemathesis cannot handle must not stop the run
                if not failing:  # with a divergence: hypothesis's Flaky once the shrinking budget is spent
                    print(f"  {label} [{mode.value}]: {type(e).__name__}: {str(e)[:200]}", file=sys.stderr)
            if failing:
                report.add(failing[0])
        if a.edge and first_valid:
            case, step = first_valid[0]
            info["path_params"] = {n: str(v) for n, v in (case.path_parameters or {}).items()}
            path_only, _, _ = step[1].partition("?")
            example = (step[0], path_only, case.query or {}, case.body if step[2] is not None and isinstance(case.body, (dict, list)) else None,
                       {**step[3]})
            if example[3] is None and info["has_body"]:
                info["has_body"] = False  # a non-JSON body: the JSON corpus does not apply
            for name, s in edge_cases(info, example):
                if not isinstance(s[2], bytes) and s[2] is not None:
                    s = (s[0], s[1], json.dumps(s[2]).encode(), s[3])
                send(f"{label} [edge: {name}]", s)
        print(f"  [{k + 1}/{len(ops)}] {label}: {len(report.items)} divergences so far", file=sys.stderr)
    return report.finish(f"generation (seed {a.seed}, {a.max_examples} examples/mode)")


def cmd_prepare(a) -> int:
    snap = Snapshot(a.snapshot)
    for db in a.db:
        snap.create(db)
        print(f"created {_dbname(db)} from the snapshot")
    return 0


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    pr = sub.add_parser("prepare", help="create replay databases from a snapshot (servers stopped)")
    pr.add_argument("--snapshot", required=True)
    pr.add_argument("--db", action="append", required=True)
    for name in ("replay", "gen"):
        s = sub.add_parser(name)
        if name == "replay":
            s.add_argument("recording")
            s.add_argument("--limit", type=int, default=0)
            s.add_argument("--minimize", type=int, default=0, metavar="TRIALS", help="ddmin budget (0: off)")
            s.add_argument("--record-key", help="PY2AXUM_RECORD_KEY of the recording, for --secret")
            s.add_argument("--secret", action="append", default=[],
                           help="a static credential valid in the snapshot (API token...): its pseudonym is replaced by it")
        else:
            s.add_argument("--seed", type=int, default=1)
            s.add_argument("--max-examples", type=int, default=25)
            s.add_argument("--modes", default="positive,negative")
            s.add_argument("--no-edge", dest="edge", action="store_false")
            s.add_argument("--include", default="", help="regex on `METHOD /path`")
            s.add_argument("--exclude", default="")
            s.add_argument("--openapi", default="/openapi.json")
            s.add_argument("--header", action="append", default=[], help="`Name: value` sent with every request")
            s.add_argument("--setup", type=int, default=0, help="play the scenario's first N steps (log-in) after each restore")
            s.add_argument("--shrink-time", type=float, default=30, help="seconds spent shrinking one divergence")
        s.add_argument("--ref", required=True)
        s.add_argument("--cand", required=True)
        s.add_argument("--ref-db", required=True)
        s.add_argument("--cand-db", required=True)
        s.add_argument("--snapshot")
        s.add_argument("--flush-redis")
        s.add_argument("--scenario")
        s.add_argument("--settle", type=float, help="pause after a write (default: the scenario's SETTLE)")
        s.add_argument("--out")
        s.add_argument("--ignore-encoding", action="store_true")
    a = p.parse_args(argv)
    try:
        return run(a)
    except ServerDown as e:
        print(f"ABORTED: {e}", file=sys.stderr)
        return 2


def run(a) -> int:
    for attr in ("ref_db", "cand_db"):
        if getattr(a, attr, None):
            _guard(getattr(a, attr))
    return {"prepare": cmd_prepare, "replay": cmd_replay, "gen": cmd_gen}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())

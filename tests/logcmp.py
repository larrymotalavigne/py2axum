"""Log conformance: what the reference and the candidate wrote on stdout and stderr, line for line.

Each server writes its two streams to `<prefix>.out` and `<prefix>.err` (scripts_start_*.sh); `<prefix>.pid`
holds its process id when the shutdown is to be compared too. Before comparing, what legitimately differs
between two runs is masked:

- instants: `asctime` (`2026-10-09 22:31:04,202`, any `datefmt` made of these digits), ISO-8601 values;
- the process id (`Started server process [123]`), client ports (`127.0.0.1:54321`), the server's port in
  "Uvicorn running on", asyncio task numbers (`Task-12`), thread ids;
- in a JSON line (python-json-logger), the values of `created`, `msecs`, `relativeCreated`, `thread`,
  `process` (under their name or a `rename_fields` alias given in LOG_JSON_VARYING);
- the library frames of a traceback (paths outside the project, LOG_PROJECT_ROOT or the current directory,
  under `site-packages`, or in the interpreter's standard library wherever it is installed): the binary has
  only the project's frames (docs/advanced/logging.md), so only those are compared;
- the statement a database error quotes (docs/advanced/logging.md, "Differences"): SQLAlchemy's `[SQL: ...]`,
  `[parameters: ...]` and `(Background on this error at: ...)` lines, psycopg's `LINE n:` excerpt and its
  caret line;
- the warnings libraries print through the `warnings` module (pydantic's serializer warnings, PyJWT's
  InsecureKeyLengthWarning, a coroutine never awaited...): the binary prints none (the `warnings` module is
  not part of the subset), so a `file:line: SomeWarning: ...` block is dropped on both sides;
- the records of the loggers of libraries the binary reimplements without their logging (LOG_LIBRARY_LOGGERS,
  default python-multipart's `python_multipart.multipart`): a line naming one is dropped.

The lines are then compared in order, per stream; the differences are reported as a unified diff. A scenario
may list documented differences in `LOG_IGNORE` (regexes): a record matching one (with its traceback) is left
out on both sides; and values of its own that vary between runs in `LOG_MASKS` (`(regex, replacement)` pairs:
a random request id, a number made from the clock).
"""
from __future__ import annotations

import difflib
import json
import os
import re
import signal
import time
from pathlib import Path

TIME = re.compile(
    r"\d{4}[-/]\d{2}[-/]\d{2}[ T]\d{2}:\d{2}(?::\d{2}(?:[.,]\d{1,9})?)?(?:Z|[+-]\d{2}:?\d{2})?"  # asctime, ISO, datefmt
    r"|\b\d{2}:\d{2}:\d{2}(?:[.,]\d{1,9})?\b"  # a time-only datefmt
)
MASKS = [
    (re.compile(r"(server process \[)\d+(\])"), r"\1<pid>\2"),
    (re.compile(r"(Uvicorn running on https?://[^ ]*?:)\d+"), r"\1<port>"),
    (re.compile(r"\b((?:\d{1,3}\.){3}\d{1,3}|\[[0-9a-f:]+\]|::1|localhost):\d{2,5}\b"), r"\1:<port>"),
    (re.compile(r"\bTask-\d+\b"), "Task-<n>"),
    (re.compile(r" at 0x[0-9a-f]+>"), " at 0x<addr>>"),  # an object's default repr
    # a database error's statement quoted inside a repr (`'msg': "...\\n[SQL: ...]..."`)
    (re.compile(r"\\n\[SQL: .*?\]\\n\[parameters: .*?\]\\n\(Background on this error at: [^)]+\)"), ""),
]
JSON_VARYING = {"created", "msecs", "relativeCreated", "thread", "process", "asctime", "timestamp"}
FRAME = re.compile(r'^((?:\s*\|)?\s*)File "([^"]+)", line \d+, in ')
REPEATED = re.compile(r"^(?:\s*\|)?\s*\[Previous line repeated \d+ more times?\]$")
WARNING = re.compile(r"^(?:\S+:\d+: \w*Warning: |RuntimeWarning: Enable tracemalloc)")
SQL_TAIL = re.compile(r"^(\[parameters: .*\]|\(Background on this error at: \S+\)|LINE \d+: .*)$")


def project_root() -> str:
    return os.path.abspath(os.environ.get("LOG_PROJECT_ROOT", os.getcwd())).rstrip("/") + "/"


def _interpreter_dirs() -> tuple[str, ...]:
    """The standard library and the package directories of this interpreter (the reference's: the conformance
    runs with it). They count as libraries wherever they are: CI's uv installs Python under the project
    (`.cache/uv-python/`), where `enum.py` would otherwise pass for a project file."""
    import sysconfig

    dirs = set()
    for key in ("stdlib", "platstdlib", "purelib", "platlib"):
        p = sysconfig.get_path(key)
        if p:
            dirs |= {os.path.abspath(p).rstrip("/") + "/", os.path.realpath(p).rstrip("/") + "/"}
    return tuple(sorted(dirs))


def library(path: str) -> bool:
    return ("/site-packages/" in path or not path.startswith(project_root())
            or path.startswith(_interpreter_dirs()))


def drop_library_frames(lines: list[str]) -> list[str]:
    """A traceback without the frames of libraries: a `File "..."` line under site-packages or the stdlib and
    the source lines indented under it."""
    out: list[str] = []
    skip_indent: int | None = None
    prefix = ""
    for line in lines:
        m = FRAME.match(line)
        if m:
            skip_indent = len(m.group(1)) if library(m.group(2)) else None
            if skip_indent is not None:
                prefix = m.group(1)
                continue
        elif skip_indent is not None:
            if len(line) > skip_indent + 2 and line[skip_indent:skip_indent + 2] == "  " and line[:skip_indent] == prefix:
                continue
            # the dropped frame's repetitions (a library's recursion): `[Previous line repeated 953 more times]`
            if REPEATED.match(line) and line[:skip_indent] == prefix:
                continue
            skip_indent = None
        if m:
            prefix = m.group(1)
        out.append(line)
    return out


def drop_library_records(lines: list[str]) -> list[str]:
    names = [n for n in os.environ.get("LOG_LIBRARY_LOGGERS", "python_multipart.multipart").split(",") if n]
    pat = re.compile(r"(?<![\w.])(?:" + "|".join(re.escape(n) for n in names) + r")(?![\w.])") if names else None
    return [l for l in lines if pat is None or not pat.search(l)]


def drop_warnings(lines: list[str]) -> list[str]:
    """without the `warnings` module's output: `path:line: Category: message`, its continuation lines, the
    source line under it"""
    out: list[str] = []
    skipping = False
    for line in lines:
        if WARNING.match(line):
            skipping = True
            continue
        if skipping and (line.startswith("  ") or line.startswith("* ")):
            continue
        skipping = False
        out.append(line)
    return out


def drop_sql_text(lines: list[str]) -> list[str]:
    """a database error without the statement it quotes: `[SQL: ...]` (one or more lines up to its `]`),
    `[parameters: ...]`, the documentation link, psycopg's `LINE n:` excerpt and the caret under it"""
    out: list[str] = []
    in_sql = False
    after_line = False
    for line in lines:
        if in_sql:
            in_sql = not line.endswith("]")
            continue
        if line.startswith("[SQL: "):
            in_sql = not line.endswith("]")
            continue
        if after_line and line.strip() == "^":
            after_line = False
            continue
        after_line = line.startswith("LINE ") and SQL_TAIL.match(line) is not None
        if SQL_TAIL.match(line):
            continue
        out.append(line)
    return out


def mask_text(s: str) -> str:
    s = TIME.sub("<time>", s)
    for pat, rep in MASKS:
        s = pat.sub(rep, s)
    if "\n" in s:
        # a multi-line value (a JSON field holding a traceback): the same rules as for the lines of a stream
        s = "\n".join(drop_warnings(drop_sql_text(drop_library_frames(s.split("\n")))))
    return s


def mask_json(v, varying: set[str]):
    if isinstance(v, dict):
        return {k: ("<varies>" if k in varying else mask_json(x, varying)) for k, x in v.items()}
    if isinstance(v, list):
        return [mask_json(x, varying) for x in v]
    if isinstance(v, str):
        return mask_text(v)
    return v


def normalize(text: str, masks: list[tuple[str, str]] | None = None) -> list[str]:
    varying = JSON_VARYING | set(filter(None, os.environ.get("LOG_JSON_VARYING", "").split(",")))
    lines: list[str] = []
    for line in text.split("\n"):
        st = line.strip()
        if st.startswith("{") and st.endswith("}"):
            try:
                obj = json.loads(st)
            except ValueError:
                obj = None
            if isinstance(obj, dict):
                lines.append(json.dumps(mask_json(obj, varying), ensure_ascii=False))
                continue
        lines.append(mask_text(line))
    lines = drop_library_records(drop_warnings(drop_sql_text(drop_library_frames(lines))))
    for pat, rep in masks or []:
        lines = [re.sub(pat, rep, l) for l in lines]
    while lines and lines[-1] == "":
        lines.pop()
    return lines


class Logs:
    """One server's output files, read by segments (byte offsets)."""

    def __init__(self, prefix: str):
        self.prefix = prefix
        self.segments: dict[str, list[tuple[int, int]]] = {"out": [], "err": []}
        self.mark_at = {s: 0 for s in ("out", "err")}

    def path(self, stream: str) -> Path:
        return Path(f"{self.prefix}.{stream}")

    def size(self, stream: str) -> int:
        try:
            return self.path(stream).stat().st_size
        except OSError:
            return 0

    def mark(self) -> None:
        """from here on, the output counts"""
        for s in self.mark_at:
            self.mark_at[s] = self.size(s)

    def take(self) -> None:
        """what was written since the mark counts"""
        for s in self.mark_at:
            self.segments[s].append((self.mark_at[s], self.size(s)))
            self.mark_at[s] = self.size(s)

    def text(self, stream: str) -> str:
        data = self.path(stream).read_bytes() if self.path(stream).exists() else b""
        return "".join(data[a:b].decode("utf-8", "replace") for a, b in self.segments[stream])

    def stop(self, timeout: float = 40.0) -> None:
        """SIGTERM to the server (its `.pid` file), then its shutdown lines count"""
        self.mark()
        try:
            pid = int(Path(f"{self.prefix}.pid").read_text().strip())
        except (OSError, ValueError):
            return
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        end = time.time() + timeout
        while time.time() < end:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.1)
        time.sleep(0.2)
        self.take()


def settle(*logs: Logs, quiet: float = 0.3, timeout: float = 5.0) -> None:
    """waits until the files stop growing (the last lines of a request are written after its response)"""
    end = time.time() + timeout
    last = None
    while time.time() < end:
        cur = tuple(lg.size(s) for lg in logs for s in ("out", "err"))
        if cur == last:
            return
        last = cur
        time.sleep(quiet)


# a record starts with uvicorn's level prefix or with its instant (an application format: `%(asctime)s - ...`)
RECORD = re.compile(r"^(?:(?:CRITICAL|ERROR|WARNING|INFO|DEBUG|TRACE)[: ]|<time>)")


def drop_events(lines: list[str], ignore: list[str]) -> list[str]:
    """without the records (a record line and the lines continuing it: a traceback) matching one of the
    scenario's documented differences (`LOG_IGNORE` regexes)"""
    if not ignore:
        return lines
    pats = [re.compile(p) for p in ignore]
    out: list[str] = []
    event: list[str] = []

    def flush() -> None:
        text = "\n".join(event)
        if not any(p.search(text) for p in pats):
            out.extend(event)
        event.clear()

    for line in lines:
        if RECORD.match(line):
            flush()
        event.append(line)
    flush()
    return out


def compare(ref: Logs, cand: Logs, context: int = 2, limit: int = 60, ignore: list[str] | None = None,
            masks: list[tuple[str, str]] | None = None) -> list[str]:
    """the differences (unified diff lines), empty when both streams match"""
    report: list[str] = []
    for stream, name in (("out", "stdout"), ("err", "stderr")):
        missing = [str(lg.path(stream)) for lg in (ref, cand) if not lg.path(stream).exists()]
        if missing:
            # a wrong prefix would compare two empty streams: never a pass
            print(f"DIFF LOG {name}: no such file {', '.join(missing)}")
            report.extend(missing)
            continue
        a = drop_events(normalize(ref.text(stream), masks), ignore or [])
        b = drop_events(normalize(cand.text(stream), masks), ignore or [])
        if os.environ.get("LOG_DUMP"):
            # the normalized streams, to study a difference (LOG_DUMP=<dir>)
            Path(os.environ["LOG_DUMP"]).mkdir(parents=True, exist_ok=True)
            for side, lines in (("ref", a), ("cand", b)):
                (Path(os.environ["LOG_DUMP"]) / f"{side}.{stream}").write_text("\n".join(lines) + "\n")
        if a == b:
            print(f"ok   LOG {name}: {len(a)} lines")
            continue
        diff = list(difflib.unified_diff(a, b, f"ref {name}", f"cand {name}", n=context, lineterm=""))
        print(f"DIFF LOG {name}: {len(a)} lines (ref), {len(b)} (cand)")
        for line in diff[:limit]:
            print(f"     {line}")
        if len(diff) > limit:
            print(f"     ... {len(diff) - limit} more diff lines")
        report.extend(diff)
    return report

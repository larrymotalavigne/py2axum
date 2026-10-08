"""The runtime must not leak memory per request: `Box::leak`, `Vec::leak`, `mem::forget` only in the caches that
make a value `&'static` once per distinct key.

Found by bench/soak.py (an MCP application, 08/10/2026): a `tools/call` of the MCP server leaked its validator, a
`col.op(...)` its operator and `json.dumps(separators=)` its separators, at every call.
"""
import re
from pathlib import Path

RUNTIME = Path(__file__).resolve().parent.parent / "py2axum" / "runtime"
LEAK = re.compile(r"Box::leak|\.leak\(\)|mem::forget")
FN = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)")
# (file, enclosing function): each one caches what it leaks, keyed by the value
ALLOWED = {("types.rs", "schema_td"), ("types.rs", "intern"), ("types.rs", "td_of")}


def leaks() -> list[tuple[str, str, int]]:
    out = []
    for f in sorted(RUNTIME.rglob("*.rs")):
        fn = "<module>"
        for i, line in enumerate(f.read_text().splitlines(), 1):
            if m := FN.match(line):
                fn = m.group(1)
            if LEAK.search(line.split("//")[0]):
                out.append((f.name, fn, i))
    return out


def test_no_leak_outside_caches():
    bad = [f"{f}:{i} in fn {fn}" for f, fn, i in leaks() if (f, fn) not in ALLOWED]
    assert not bad, "memory leaked per call (cache it, see types::schema_td / types::intern): " + ", ".join(bad)


def test_the_check_sees_the_caches():
    assert {(f, fn) for f, fn, _ in leaks()} >= {("types.rs", "schema_td"), ("types.rs", "intern")}

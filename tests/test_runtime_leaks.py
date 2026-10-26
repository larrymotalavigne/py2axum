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
ALLOWED = {("types.rs", "schema_td"), ("types.rs", "intern"), ("types.rs", "td_of"), ("v.rs", "psycopg_class"),
           # pytz's table, read once; dateutil: a tzoffset name, a zone file (per path and contents), a tzstr string
           ("pytzrt.rs", "str"), ("dutz.rs", "name_code"), ("dutz.rs", "register"),
           # FastAPI's docs routes from non-literal *_url options: built once at startup (a OnceLock)
           ("routing.rs", "set_docs")}


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


# A nested function naming itself (recursion) held itself through its cell: the cell holds the function, the
# function's closure the cell, a cycle CPython's GC frees and Arc never does (everything it captured leaked at
# each call; PR #1 of the public repository, 11/10/2026). Bound once by an undecorated `def` run once per call,
# the name always holds that function: it reads itself through a weak reference (`SelfRef`).
SELFREF = '''
from fastapi import FastAPI

app = FastAPI()


def deco(f):
    return f


@app.get("/{case}")
async def route(case: str):
    payload = "x" * 10

    def once(k):
        return len(payload) if k <= 0 else once(k - 1)

    def looked(k):
        return k if k <= 0 else looked(k - 1)

    pick = lambda: looked  # noqa: E731

    @deco
    def decorated(k):
        return k if k <= 0 else decorated(k - 1)

    def rebound(k):
        return k if k <= 0 else rebound(k - 1)

    rebound = deco(rebound)
    out = []
    for i in range(2):
        def looping(k):
            return k if k <= 0 else looping(k - 1)

        out.append(looping)
    return {"v": once(2) + pick()(1) + decorated(1) + rebound(1) + out[0](1)}
'''


def test_self_naming_nested_function_is_weak(tmp_path):
    from py2axum.__main__ import main

    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(SELFREF)
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
    src = "".join(p.read_text() for p in sorted((tmp_path / "out" / "src").glob("*.rs")))
    # `once` and `looked` (a cell variable: another closure reads it, it gets a cell of the weak reference);
    # not `decorated`, `rebound`, `looping`: the name may hold something else than this function
    assert src.count("SelfRef::default()") == 2
    assert src.count("ops::cell_of(") >= 1

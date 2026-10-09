"""Translate the corpus and compile it at scale: `py2axum check`, then generation, then a few shared crates.

Compiling one crate per example would rebuild the runtime (~40 000 lines of Rust) hundreds of times. The
examples are grouped in bundles: one crate holds the runtime once and each example's generated code as a
module (`gen.rs` and `main.rs` unchanged but for the module paths), and its `main` starts the example named
by `CORPUS_APP`. All bundles share one target directory (the dependencies compile once), with an
incremental, unoptimised-for-size profile: what is compared is behaviour, not speed. A file is rewritten only
when its content changes, so cargo's incremental cache serves the next run.
"""
from __future__ import annotations

import hashlib
import json
import re
import shutil
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROFILE = """[profile.release]
lto = false
codegen-units = 256
opt-level = 1
incremental = true
debug = false

# password hashing at opt-level 1 takes seconds: a token minted after it would lag the reference's
[profile.release.package.argon2]
opt-level = 3

[profile.release.package.blake2]
opt-level = 3

[profile.release.package.bcrypt]
opt-level = 3

[profile.release.package.blowfish]
opt-level = 3
"""


def py2axum_hash() -> str:
    """The translator's sources: a cached check or generation is reused only for the same translator."""
    h = hashlib.sha256()
    for p in sorted((ROOT / "py2axum").rglob("*")):
        if p.is_file() and "__pycache__" not in p.parts:
            h.update(str(p.relative_to(ROOT)).encode())
            h.update(p.read_bytes())
    return h.hexdigest()[:16]


def _tree_hash(d: Path) -> str:
    h = hashlib.sha256()
    for p in sorted(d.rglob("*.py")):
        h.update(str(p.relative_to(d)).encode())
        h.update(p.read_bytes())
    return h.hexdigest()[:16]


def check(pkg: Path, cache: Path, tool: str) -> dict:
    """`py2axum check --json` of a staged example (cached by translator and sources)."""
    key = cache / f"{pkg.name}.{tool}.{_tree_hash(pkg)}.json"
    if key.exists():
        return json.loads(key.read_text())
    r = subprocess.run([sys.executable, "-m", "py2axum", "check", str(pkg), "--json", "--allow-untested-versions"],
                       cwd=ROOT, capture_output=True, text=True, timeout=300)
    try:
        data = json.loads(r.stdout)
    except ValueError:
        data = {"crash": (r.stderr or r.stdout)[-3000:]}
    for old in cache.glob(f"{pkg.name}.*.json"):
        old.unlink()
    key.write_text(json.dumps(data))
    return data


def generate(pkg: Path, out: Path, stamp: str) -> str | None:
    """Generate the example's crate into `out` (skipped when the stamp matches); the error text on failure."""
    mark = out / ".corpus-stamp"
    if mark.exists() and mark.read_text() == stamp and (out / "src" / "gen.rs").exists():
        return None
    r = subprocess.run([sys.executable, "-m", "py2axum", str(pkg), "-o", str(out), "--name", pkg.name,
                        "--allow-untested-versions"], cwd=ROOT, capture_output=True, text=True, timeout=600)
    if r.returncode != 0:
        return (r.stderr or r.stdout)[-3000:]
    mark.write_text(stamp)
    return None


def _write(path: Path, text: str) -> None:
    """Only when it changes: cargo's incremental compilation keys on modification times too."""
    if not path.exists() or path.read_text() != text:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


def _as_module(main_rs: str, ident: str) -> str:
    out = re.sub(r"#\[macro_use\]\s*mod dynrt;\s*mod gen;", f"use crate::dynrt;\nuse crate::{ident}_gen as gen;", main_rs)
    if out == main_rs:
        raise ValueError("unexpected main.rs layout")
    return re.sub(r"^fn main\(\)", "pub fn main()", out, flags=re.M)


def write_bundle(bundle: Path, gens: dict[str, Path]) -> None:
    """One crate: the runtime once, then each example's generated code as modules."""
    first = next(iter(gens.values()))
    cargo = (first / "Cargo.toml").read_text()
    cargo = re.sub(r'name = "[^"]+"', f'name = "{bundle.name}"', cargo, count=1)
    cargo = cargo[:cargo.index("[profile.release]")] + PROFILE
    _write(bundle / "Cargo.toml", cargo)
    lock = ROOT / "py2axum" / "runtime" / "Cargo.lock"
    if not (bundle / "Cargo.lock").exists():
        (bundle / "Cargo.lock").write_text(lock.read_text().replace('name = "dynapp_axum"', f'name = "{bundle.name}"'))
    src = bundle / "src"
    rt = src / "dynrt"
    for p in sorted((first / "src" / "dynrt").iterdir()):
        _write(rt / p.name, p.read_text())
    for p in list(rt.iterdir()):
        if not (first / "src" / "dynrt" / p.name).exists():
            p.unlink()
    mods, arms = ["#[macro_use]", "mod dynrt;"], []
    for ident, g in sorted(gens.items()):
        _write(src / "apps" / f"{ident}_gen.rs", (g / "src" / "gen.rs").read_text())
        _write(src / "apps" / f"{ident}_main.rs", _as_module((g / "src" / "main.rs").read_text(), ident))
        mods += [f'#[path = "apps/{ident}_gen.rs"]', f"mod {ident}_gen;", f'#[path = "apps/{ident}_main.rs"]',
                 f"mod {ident}_main;"]
        arms.append(f'        "{ident}" => {ident}_main::main(),')
    keep = {f"{i}_{k}.rs" for i in gens for k in ("gen", "main")}
    for p in list((src / "apps").iterdir()):
        if p.name not in keep:
            p.unlink()
    main = "\n".join(["// corpus bundle: the runtime once, one module per example (corpus/build.py)", *mods, "",
                      "fn main() {", '    let app = std::env::var("CORPUS_APP").expect("CORPUS_APP");',
                      "    match app.as_str() {", *arms,
                      '        other => { eprintln!("unknown example {other}"); std::process::exit(2) }',
                      "    }", "}", ""])
    _write(src / "main.rs", main)


_ERR_FILE = re.compile(r"-->\s+src/apps/([a-z0-9_]+?)_(?:gen|main)\.rs:")


def cargo_build(bundle: Path, target: Path, log: Path) -> tuple[bool, set[str]]:
    """Build a bundle; on failure, the examples whose generated code the errors point at."""
    r = subprocess.run(["cargo", "build", "--release", "--message-format", "human"], cwd=bundle,
                       env={**__import__("os").environ, "CARGO_TARGET_DIR": str(target), "CARGO_TERM_COLOR": "never"},
                       capture_output=True, text=True)
    log.write_text(r.stderr[-200_000:])
    if r.returncode == 0:
        return True, set()
    bad = set()
    for block in r.stderr.split("\nerror")[1:]:
        m = _ERR_FILE.search(block)
        if m:
            bad.add(m.group(1))
    return False, bad


def build_all(gens: dict[str, Path], work: Path, size: int, log, prefix: str = "corpus_b") -> tuple[dict[str, str], dict[str, str]]:
    """Bundle and compile every generated example: (example -> bundle binary name, example -> build error).
    `prefix` names the bundles (one set per corpus; they share the target directory)."""
    target = work / "target"
    ids = sorted(gens)
    chunks = [ids[i:i + size] for i in range(0, len(ids), size)]
    binary, failed = {}, {}
    for n, chunk in enumerate(chunks):
        name = f"{prefix}{n}"
        bundle = work / "bundles" / name
        members = {i: gens[i] for i in chunk}
        while members:
            write_bundle(bundle, members)
            ok, bad = cargo_build(bundle, target, work / "logs" / f"{name}.log")
            if ok:
                for i in members:
                    binary[i] = name
                break
            text = (work / "logs" / f"{name}.log").read_text()
            if not bad:
                for i in members:
                    failed[i] = "bundle build failed: " + text[-2000:]
                break
            for i in bad:
                failed[i] = _errors_of(text, i)
                del members[i]
            log(f"  {name}: {len(bad)} example(s) do not compile, rebuilding without them")
        log(f"  {name}: {len([i for i in chunk if i in binary])}/{len(chunk)} built")
    for stale in (work / "bundles").glob(f"{prefix}*"):
        if stale.name.removeprefix(prefix).isdigit() and int(stale.name.removeprefix(prefix)) >= len(chunks):
            shutil.rmtree(stale)
    return binary, failed


def _errors_of(text: str, ident: str) -> str:
    blocks = ["error" + b for b in text.split("\nerror")[1:] if f"src/apps/{ident}_" in b]
    return "\n".join(b[:800] for b in blocks[:3])


def parallel(fn, items, workers: int):
    with ThreadPoolExecutor(workers) as ex:
        return list(ex.map(fn, items))

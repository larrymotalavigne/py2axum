"""The generated crate's layout: the runtime as a library crate next to the application's, one dependency list."""
import re
import textwrap
from pathlib import Path

from py2axum import dyn
from py2axum.__main__ import main

APP = '''
    from fastapi import FastAPI

    app = FastAPI()

    @app.get("/ping")
    async def ping():
        return {"ok": True}
'''


def deps(text: str) -> dict[str, str]:
    section = text.split("\n[dependencies]\n", 1)[1].split("\n[", 1)[0]
    return dict(re.match(r"([\w-]+) = (.*)", line).groups() for line in section.splitlines() if re.match(r"[\w-]+ = ", line))


def generate(tmp_path: Path, out: Path) -> None:
    pkg = tmp_path / "proj"
    pkg.mkdir(exist_ok=True)
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(textwrap.dedent(APP))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(out), "--name", "proj_axum"]) == 0


def test_runtime_is_a_library_crate(tmp_path):
    out = tmp_path / "out"
    generate(tmp_path, out)
    cargo = (out / "Cargo.toml").read_text()
    assert cargo.startswith("[workspace]\n")
    assert 'dynrt = { path = "dynrt" }' in cargo
    assert re.search(r'\[profile\.release\]\n(#.*\n)*lto = "thin"\ncodegen-units = 1\n', cargo)
    assert 'default = ["mimalloc"]' in cargo
    # the runtime's sources and manifest, as shipped (the same files and module directories such as rl/, nothing
    # generated in them)
    shipped = sorted(p.name for p in dyn.RUNTIME_DIR.iterdir() if p.is_file() or (p.is_dir() and p.name != "target"))
    assert sorted(p.name for p in (out / "dynrt").iterdir()) == shipped
    assert sorted(p.name for p in (out / "dynrt" / "rl").iterdir()) == sorted(p.name for p in (dyn.RUNTIME_DIR / "rl").iterdir())
    assert not (out / "src" / "dynrt").exists()
    main_rs = (out / "src" / "main.rs").read_text()
    assert "#[macro_use]\nextern crate dynrt;\nmod gen;" in main_rs
    assert "crate::dynrt" not in (out / "src" / "gen.rs").read_text()
    # every dependency of the runtime is declared by the application with the same version and features
    rt = deps((dyn.RUNTIME_DIR / "Cargo.toml").read_text())
    app = deps(cargo)
    assert {k: app.get(k) for k in rt} == rt
    lock = (out / "Cargo.lock").read_text()
    assert '\nname = "dynrt"\n' in lock and '\nname = "proj_axum"\n' in lock


def test_regeneration_replaces_the_pre_09_layout(tmp_path):
    """A crate generated before 0.9 (runtime in src/dynrt/, a lock file without the runtime's crate)."""
    out = tmp_path / "out"
    (out / "src" / "dynrt").mkdir(parents=True)
    (out / "src" / "dynrt" / "mod.rs").write_text("// old\n")
    (out / "Cargo.lock").write_text('version = 4\n\n[[package]]\nname = "proj_axum"\nversion = "0.1.0"\n')
    generate(tmp_path, out)
    assert not (out / "src" / "dynrt").exists()
    assert '\nname = "dynrt"\n' in (out / "Cargo.lock").read_text()
    # a current lock file is kept (cargo may have rewritten it)
    (out / "Cargo.lock").write_text((out / "Cargo.lock").read_text() + "# kept\n")
    generate(tmp_path, out)
    assert (out / "Cargo.lock").read_text().endswith("# kept\n")

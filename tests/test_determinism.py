"""The generated project must not depend on PYTHONHASHSEED (set iteration order): two runs, same bytes."""
import os
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent


def generate(out: Path, seed: str, *args: str) -> dict[str, bytes]:
    env = {**os.environ, "PYTHONHASHSEED": seed}
    subprocess.run([sys.executable, "-m", "py2axum", *args, "-o", str(out)],
                   cwd=ROOT, env=env, check=True, capture_output=True)
    return {str(p.relative_to(out)): p.read_bytes() for p in sorted(out.rglob("*")) if p.is_file()}


@pytest.mark.parametrize("args", [
    ("app", "--name", "app_axum"),
    ("fixtures/dynapp", "--root", ".", "--backend", "dyn", "--python-side", "mount", "--name", "dynapp_axum"),
], ids=["typed", "dyn"])
def test_same_output_whatever_the_hash_seed(tmp_path, args):
    a = generate(tmp_path / "a", "0", *args)
    b = generate(tmp_path / "b", "1", *args)
    assert a.keys() == b.keys()
    assert [f for f in a if a[f] != b[f]] == []

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
    ("fixtures/dynapp", "--root", ".", "--python-side", "mount", "--name", "dynapp_axum"),
], ids=["app", "dynapp"])
def test_same_output_whatever_the_hash_seed(tmp_path, args):
    a = generate(tmp_path / "a", "0", *args)
    b = generate(tmp_path / "b", "1", *args)
    assert a.keys() == b.keys()
    assert [f for f in a if a[f] != b[f]] == []


HELPERS = '''import boto3
import openpyxl
import reportlab.pdfgen.canvas


def use_boto():
    return boto3.client("s3")


def use_reportlab():
    return reportlab.pdfgen.canvas.Canvas("x")


def use_openpyxl():
    return openpyxl.Workbook()
'''
MAIN = '''from fastapi import FastAPI

from proj.helpers import use_boto, use_openpyxl, use_reportlab

app = FastAPI()


@app.get("/many")
async def many():
    use_reportlab()
    use_openpyxl()
    use_boto()
    return {}
'''


def test_check_json_whatever_the_hash_seed(tmp_path):
    """A route refused for several reasons: the call graph's edges are sets, and walking them as they came made
    the first reason (and the order of the blockers) of `check --json` change from run to run. Now: closest to the
    route first, then source order."""
    import json

    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "helpers.py").write_text(HELPERS)
    (pkg / "main.py").write_text(MAIN)
    outs = set()
    for seed in ("0", "1", "2", "3", "4"):
        r = subprocess.run([sys.executable, "-m", "py2axum", "check", str(pkg), "--root", str(tmp_path), "--json",
                            "--allow-untested-versions"], cwd=ROOT, env={**os.environ, "PYTHONHASHSEED": seed},
                           capture_output=True, text=True)
        outs.add(r.stdout)
    assert len(outs) == 1
    [route] = json.loads(outs.pop())["routes"]
    assert "boto3.client()" in route["reason"]  # helpers.py:7, the first in source order
    assert list(route["blockers"]) == ["library boto3", "library reportlab", "library openpyxl"]

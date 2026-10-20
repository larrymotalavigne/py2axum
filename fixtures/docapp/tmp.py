"""tempfile.NamedTemporaryFile: a document written to a temporary file for a library that wants a path (a mail
attachment), read back by its name, then removed; and the default delete=True, removed when the block ends."""
import os
import tempfile
from pathlib import Path

from fastapi import APIRouter

router = APIRouter()


@router.post("/t/attach/{year}")
async def attach(year: int, size: int = 1000):
    data = bytes(range(256)) * (size // 256) + b"%PDF-end"
    with tempfile.NamedTemporaryFile(suffix=".pdf", delete=False, prefix=f"form-2044-{year}-") as tmp:
        tmp.write(data)
        tmp_path = tmp.name
    base = os.path.basename(tmp_path)
    try:
        with open(tmp_path, "rb") as f:
            back = f.read()
    finally:
        Path(tmp_path).unlink(missing_ok=True)
    return {"same": back == data, "size": len(back), "prefix": base.startswith(f"form-2044-{year}-"),
            "suffix": base.endswith(".pdf"), "random": len(base) - len(f"form-2044-{year}-.pdf"),
            "in_tmp": os.path.dirname(tmp_path) == tempfile.gettempdir(),
            "gone": not os.path.exists(tmp_path)}


@router.post("/t/scratch")
async def scratch(text: str = "x"):
    with tempfile.NamedTemporaryFile(mode="w+") as tmp:
        tmp.write(text)
        tmp.flush()  # Python's buffer holds small writes until flush or close (docs: Generated documents)
        name = tmp.name
        inside = os.path.exists(name)
        with open(name) as f:
            seen = f.read()
    return {"inside": inside, "after": os.path.exists(name), "seen": seen, "tmp": os.path.basename(name).startswith("tmp")}


@router.post("/t/tmpdir")
async def tmpdir(name: str = "dossier.pdf", size: int = 100, nested: bool = False):
    """tempfile.TemporaryDirectory in a `with` block: a file written under it by a library that wants a path (a
    mail attachment), the directory and its content removed when the block ends."""
    data = bytes(range(256)) * (size // 256) + b"%%EOF"
    with tempfile.TemporaryDirectory() as dossier:
        chemin = Path(dossier) / name
        chemin.write_bytes(data)
        if nested:
            sub = Path(dossier) / "sous"
            sub.mkdir()
            (sub / "x.txt").write_text("x")
        back = chemin.read_bytes()
        inside = os.path.isdir(dossier)
        base = os.path.basename(dossier)
    return {"same": back == data, "inside": inside, "gone": not os.path.exists(dossier), "tmp": base.startswith("tmp"),
            "random": len(base) - 3, "in_tmp": os.path.dirname(dossier) == tempfile.gettempdir()}


@router.post("/t/tmpdir/options")
async def tmpdir_options(prefix: str = "pack-", suffix: str = "-x", explicit: bool = True):
    """Prefix and suffix, `.name`, `cleanup()` called twice, `delete=False` left to the caller."""
    td = tempfile.TemporaryDirectory(suffix=suffix, prefix=prefix)
    base = os.path.basename(td.name)
    shape = [base.startswith(prefix), base.endswith(suffix), len(base) - len(prefix) - len(suffix)]
    (Path(td.name) / "a").write_text("a")
    shown = repr(td) == f"<TemporaryDirectory {td.name!r}>"
    if explicit:
        td.cleanup()
        td.cleanup()
    gone = not os.path.exists(td.name)
    kept = tempfile.TemporaryDirectory(delete=False)
    with kept as d:
        (Path(d) / "b").write_text("b")
    still = os.path.exists(kept.name)
    kept.cleanup()
    return {"shape": shape, "shown": shown, "gone": gone, "still": still, "after": os.path.exists(kept.name)}

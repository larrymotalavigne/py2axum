"""`--path DIR` (experimental): a sibling package of a monorepo resolves like a module under `--root`."""
from py2axum.check import main
from py2axum.modules import set_extra_roots


def test_path_option_resolves_a_sibling_package(tmp_path, capsys):
    pkg = tmp_path / "app" / "pkg"
    pkg.mkdir(parents=True)
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(
        "from fastapi import FastAPI\nfrom sibling.models import Item\n\napp = FastAPI()\n\n\n"
        "@app.get('/i', response_model=Item)\nasync def i():\n    return Item(id=1, name='a')\n")
    sib = tmp_path / "shared" / "sibling"
    sib.mkdir(parents=True)
    (sib / "__init__.py").write_text("")
    (sib / "models.py").write_text("from pydantic import BaseModel\n\n\nclass Item(BaseModel):\n    id: int\n    name: str\n")
    args = [str(pkg), "--root", str(tmp_path / "app"), "--allow-untested-versions"]
    try:
        assert main(args) == 1  # `sibling` is outside the import root: the annotation is refused
        capsys.readouterr()
        assert main([*args, "--path", str(tmp_path / "shared")]) == 0
    finally:
        set_extra_roots([])

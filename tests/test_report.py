"""Coverage report on a multi-module project: import resolution, routers, continue-on-error."""
import json
import re
import textwrap
from pathlib import Path

from py2axum.__main__ import main

FILES = {
    "proj/__init__.py": "",
    "proj/main.py": '''
        from fastapi import FastAPI
        from proj.views import items
        from .views.users import router as users_router

        def create_app() -> FastAPI:
            app = FastAPI()
            app.include_router(items.router, prefix="/api")
            app.include_router(users_router)
            return app

        app = create_app()
    ''',
    "proj/models/__init__.py": "from .item import Item\n",
    "proj/models/base.py": '''
        from sqlalchemy.orm import DeclarativeBase

        class Base(DeclarativeBase):
            pass
    ''',
    "proj/models/item.py": '''
        from sqlalchemy import String
        from sqlalchemy.orm import Mapped, mapped_column
        from .base import Base

        class Item(Base):
            __tablename__ = "items"
            id: Mapped[int] = mapped_column(primary_key=True)
            label: Mapped[str] = mapped_column(String(50))
    ''',
    "proj/schemas.py": '''
        from pydantic import BaseModel, ConfigDict, Field, SecretStr

        class ItemOut(BaseModel):
            model_config = ConfigDict(from_attributes=True)
            id: int
            label: str = Field(..., min_length=1)

        class Stamped(BaseModel):
            at: SecretStr
    ''',
    "proj/deps.py": '''
        import boto3

        async def get_current_user():
            return boto3.client("s3", region_name="eu-west-3")
    ''',
    "proj/views/__init__.py": "",
    "proj/views/items.py": '''
        from fastapi import APIRouter, Depends, HTTPException
        from sqlalchemy.ext.asyncio import AsyncSession
        from proj.models import Item
        from proj.schemas import ItemOut, Stamped
        from ..deps import get_current_user

        router = APIRouter(prefix="/items", tags=["items"])

        @router.get("/{item_id}", response_model=ItemOut)
        async def get_item(item_id: int, session: AsyncSession = Depends()):
            item = await session.get(Item, item_id)
            if item is None:
                raise HTTPException(status_code=404, detail="not found")
            return item

        @router.get("/stamp/now", response_model=Stamped)
        async def stamp():
            return {"at": "2026-01-01T00:00:00"}

        @router.get("/me/info")
        async def me(user: dict = Depends(get_current_user)):
            return {"ok": True}

        @router.get("/me/again")
        async def me_again(user: dict = Depends(get_current_user)):
            return {"ok": True}

        @router.get("/me/stamp", response_model=Stamped)
        async def me_stamp(user: dict = Depends(get_current_user)):
            return {"at": "2026-01-01T00:00:00"}
    ''',
    "proj/views/users.py": '''
        from fastapi import APIRouter

        router = APIRouter(prefix="/users")

        @router.get("/count")
        async def count():
            return {"count": 0}
    ''',
}


def write_project(root: Path) -> Path:
    for rel, src in FILES.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(textwrap.dedent(src))
    return root / "proj"


def test_report(tmp_path):
    pkg = write_project(tmp_path)
    out = tmp_path / "report.md"
    assert main([str(pkg), "--root", str(tmp_path), "--report", str(out)]) == 0
    data = json.loads(out.with_suffix(".json").read_text())
    routes = {r["path"]: r for r in data["routes"]}
    assert set(routes) == {"/api/items/{item_id}", "/api/items/stamp/now", "/api/items/me/info",
                           "/api/items/me/stamp", "/api/items/me/again", "/users/count"}
    # imports: absolute, relative, __init__ re-export, factory app, router prefixes
    assert routes["/api/items/{item_id}"]["status"] == "traduite"
    assert routes["/users/count"]["status"] == "traduite"
    # dyn backend: the SecretStr field blocks the schema, the unmapped lib blocks the dependency; one
    # blocker per route (compilation of a route's closure stops at its first error)
    assert set(routes["/api/items/stamp/now"]["blockers"]) == {"type SecretStr"}
    assert set(routes["/api/items/me/info"]["blockers"]) == {"lib boto3"}
    # the second route using the same refused dependency is blocked too (no stale dependency cache)
    assert set(routes["/api/items/me/again"]["blockers"]) == {"lib boto3"}
    assert len(routes["/api/items/me/stamp"]["blockers"]) == 1
    alone = {c["construction"]: c["débloquées_seule"] for c in data["constructions"]}
    assert alone == {"type SecretStr": 2, "lib boto3": 2}
    assert data["glouton"][-1]["cumul"] == 6
    assert data["résumé"] == {"total": 6, "traduites": 2, "bloquées": 4}


def test_required_field_with_ellipsis(tmp_path):
    """`Field(..., min_length=1)` is a required field, not a default value of `...`."""
    pkg = write_project(tmp_path)
    schemas_py = textwrap.dedent(FILES["proj/schemas.py"])
    (pkg / "schemas.py").write_text(schemas_py.split("class Stamped")[0])
    items_py = textwrap.dedent(FILES["proj/views/items.py"])
    (pkg / "views" / "items.py").write_text(
        items_py.replace("from proj.schemas import ItemOut, Stamped", "from proj.schemas import ItemOut")
        .split('@router.get("/stamp')[0]
    )
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0
    gen = (tmp_path / "out" / "src" / "gen.rs").read_text()
    assert re.search(r'name: "label",\s*alias: None,\s*td: &TD_\d+,\s*default: crate::dynrt::pyd::Dflt::Required,', gen)


def test_python_side_auto(tmp_path, capsys):
    """`--python-side auto`: the routes that do not translate are left to Python, the others are generated."""
    pkg = write_project(tmp_path)
    out = tmp_path / "out"
    assert main([str(pkg), "--root", str(tmp_path), "--backend", "dyn", "-o", str(out)]) == 1
    capsys.readouterr()
    assert main([str(pkg), "--root", str(tmp_path), "--python-side", "auto", "-o", str(out)]) == 0
    err = capsys.readouterr().err
    moved = {"/api/items/stamp/now", "/api/items/me/info", "/api/items/me/again", "/api/items/me/stamp"}
    for path in moved:
        assert f"python-side (auto): {path} " in err
    assert "python-side (auto): /users/count" not in err
    main_rs = (out / "src" / "main.rs").read_text()
    for path in moved:
        assert f'"{path}"' in main_rs


def test_python_side_auto_global_error(tmp_path, capsys):
    """An error about the whole application cannot be avoided by moving routes: refused, with file:line."""
    pkg = write_project(tmp_path)
    main_py = pkg / "main.py"
    main_py.write_text(main_py.read_text().replace("app = FastAPI()", "app = FastAPI(dependencies=[])"))
    assert main([str(pkg), "--root", str(tmp_path), "--python-side", "auto", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "main.py" in err and "FastAPI(dependencies=...) is not supported" in err
    assert "only moves routes" in err


def test_python_side_auto_raw_route(tmp_path, capsys):
    """A raw `add_route` with a literal path is moved too (without auto, it stops the generation)."""
    pkg = write_project(tmp_path)
    main_py = pkg / "main.py"
    main_py.write_text(main_py.read_text().replace(
        "    return app\n",
        "    app.router.add_route(\"/raw\", raw, methods=[\"GET\"])\n    return app\n",
    ).replace("def create_app", "async def raw(request):\n    return None\n\n\ndef create_app"))
    out = tmp_path / "out"
    assert main([str(pkg), "--root", str(tmp_path), "--python-side", "auto", "-o", str(out)]) == 0
    assert "python-side (auto): /raw (raw route)" in capsys.readouterr().err
    assert '"/raw"' in (out / "src" / "main.rs").read_text()

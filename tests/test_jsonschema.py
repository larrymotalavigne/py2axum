"""py2axum/jsonschema.py against the real pydantic: the `inputSchema` of an MCP tool (FastMCP's
`<function>Arguments` model) must be the same JSON, key order included."""
import importlib
import inspect
import json
import sys
import textwrap
from typing import Annotated

import pytest
from pydantic import BaseModel, ConfigDict, Field, create_model

from py2axum import dyn
from py2axum.frontend import Frontend
from py2axum.ir import TranspileError
from py2axum.jsonschema import SchemaGen
from py2axum.modules import Sym

SCHEMAS = '''
from datetime import date, datetime
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field


class Option(BaseModel):
    """One button.

    Its `value` is recorded."""

    value: str = Field(min_length=1, max_length=64)
    severity: str | None = Field(default=None, max_length=16)
    closes: bool | None = None


class Child(Option):
    model_config = ConfigDict(from_attributes=True)
    weight: float = 1.5
    tags: list[str] = []


class Strict(BaseModel):
    """No extras."""

    model_config = ConfigDict(extra="forbid")
    kind: Literal["a", "b"] | None = Field(None, description="Kind")
    n: int = Field(10, ge=1, le=20)


class Loose(Strict):
    model_config = ConfigDict(extra="allow")


class Ignored(Option):
    model_config = ConfigDict(extra="ignore")


class Holder(BaseModel):
    child: Child
    maybe: Option | None = None
    many: list[Option] = Field(default_factory=list)
    when: datetime | None = None
'''

TOOLS = '''
from datetime import date
from typing import Annotated, Any, Literal, Optional

from pydantic import Field

from .schemas import Child, Holder, Option


async def lister(
    status: Annotated[Literal["pending", "decided"] | None, Field(description="the status")] = None,
    source: Annotated[str | None, Field(max_length=64, description="Filtre")] = None,
    limit: Annotated[int, Field(ge=1, le=500)] = 50,
) -> dict[str, Any]:
    return {}


async def lire(id: Annotated[int, Field(description="Id")]) -> dict[str, Any]:
    return {}


async def rien() -> dict[str, Any]:
    return {}


async def deposer(
    source: Annotated[str, Field(min_length=1, max_length=64, description="Qui")],
    kind: Literal["send", "publish"],
    options: Annotated[list[Option], Field(description="Boutons")],
    one: Literal["only"] = "only",
    body: Annotated[str | None, Field(description="Texte")] = None,
    priority: Annotated[int, Field(ge=0, le=3, description="3 = urgent")] = 0,
    due_date: Annotated[date | None, Field(description="Échéance")] = None,
    payload: Annotated[dict[str, Any] | None, Field(description="Opaque")] = None,
    counts: dict[str, int] | None = None,
    ack: bool = False,
    ratio: Optional[float] = None,
    nested: Holder | None = None,
    child: Child = Field(description="a child"),
    *,
    tags: list[str] = ["a"],
    level: Literal[1, 2, 3] = 2,
) -> dict[str, Any]:
    return {}
'''


@pytest.fixture(scope="module")
def project(tmp_path_factory):
    root = tmp_path_factory.mktemp("js")
    pkg = root / "jsproj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "schemas.py").write_text(textwrap.dedent(SCHEMAS))
    (pkg / "tools.py").write_text(textwrap.dedent(TOOLS))
    (pkg / "main.py").write_text("from fastapi import FastAPI\n\nfrom . import tools\n\napp = FastAPI()\n")
    fe = Frontend(pkg, root)
    dyn.prepare(fe, set())
    sys.path.insert(0, str(root))
    try:
        mod = importlib.import_module("jsproj.tools")
    finally:
        sys.path.remove(str(root))
    return fe, dyn.Project(fe), mod


class ArgModelBase(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True)


def reference(func) -> dict:
    """FastMCP 2.2's func_metadata + the wire Tool model's key order."""
    params = {}
    for p in inspect.signature(func, eval_str=True).parameters.values():
        ann = Annotated[(p.annotation, Field())]
        params[p.name] = (ann, p.default) if p.default is not inspect.Parameter.empty else ann
    s = create_model(f"{func.__name__}Arguments", __base__=ArgModelBase, **params).model_json_schema()
    head = {k: s[k] for k in ("properties", "required", "type") if k in s}
    return head | {k: v for k, v in s.items() if k not in head}


@pytest.mark.parametrize("name", ["lister", "lire", "rien", "deposer"])
def test_arguments_schema_matches_pydantic(project, name):
    fe, proj, mod = project
    module = "jsproj.tools"
    fn = proj.ix.definition(Sym(module, name))
    got = SchemaGen(proj).arguments(fn, module)
    assert json.dumps(got) == json.dumps(reference(getattr(mod, name)))


@pytest.mark.parametrize(
    "sig, message",
    [
        ("x: tuple[int, int]", "type `tuple[int, int]` is not supported"),
        ("x: Annotated[int, Field(alias='y')]", "Field(alias=...) is not supported"),
        ("x", "parameter `x` without annotation"),
        ("*args: int", "*args, **kwargs"),
        ("x: dict[int, str]", "only dict[str, T]"),
        ("x: Annotated[str | int, Field(max_length=3)]", "constraints on a union of several types"),
        ("x: Literal['a', 1]", "Literal of mixed types"),
    ],
)
def test_unsupported_rejected(tmp_path, sig, message):
    pkg = tmp_path / "bad"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "tools.py").write_text("from typing import Annotated, Literal\n\nfrom pydantic import Field\n\n\n"
                                  f"async def t({sig}):\n    return {{}}\n")
    (pkg / "main.py").write_text("from fastapi import FastAPI\n\nfrom . import tools\n\napp = FastAPI()\n")
    fe = Frontend(pkg, tmp_path)
    dyn.prepare(fe, set())
    proj = dyn.Project(fe)
    with pytest.raises(TranspileError) as e:
        SchemaGen(proj).arguments(proj.ix.definition(Sym("bad.tools", "t")), "bad.tools")
    assert message in e.value.msg
    assert "tools.py:" in e.value.render()


@pytest.mark.parametrize("name", ["Option", "Child", "Holder", "Strict", "Loose", "Ignored"])
def test_model_schema_matches_pydantic(project, name):
    """`Model.model_json_schema()`."""
    fe, proj, _ = project
    sys.path.insert(0, str(fe.ix.root) if hasattr(fe, "ix") else "")
    mod = importlib.import_module("jsproj.schemas")
    got = SchemaGen(proj).model(Sym("jsproj.schemas", name))
    assert json.dumps(got) == json.dumps(getattr(mod, name).model_json_schema())

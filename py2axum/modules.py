"""Static module index: qualified module names and import resolution, without running any code.

`root` plays the role of `sys.path`: `api/views/users.py` under `root` is module `api.views.users`.
The transpiled package is parsed eagerly; any other module under `root` it imports (e.g. a sibling
`shared/` package holding the models) is parsed on demand. Anything else is an external package.
"""
from __future__ import annotations

import ast
import os
from dataclasses import dataclass, field
from pathlib import Path

SKIP_DIRS = {"__pycache__", ".venv", "venv", "node_modules", ".git", ".claude", "site-packages"}


@dataclass(frozen=True)
class Sym:
    """A top-level definition `name` of project module `module`."""

    module: str
    name: str

    @property
    def qual(self) -> str:
        return f"{self.module}.{self.name}"


@dataclass(frozen=True)
class ModRef:
    """A project module used as a value (`from api.views import users_view`)."""

    module: str


@dataclass(frozen=True)
class Ext:
    """Something from outside the project: `fastapi.APIRouter`, `stripe`, ..."""

    dotted: str

    @property
    def package(self) -> str:
        return self.dotted.split(".")[0]


Target = Sym | ModRef | Ext


@dataclass
class Module:
    name: str
    path: Path
    tree: ast.Module
    is_pkg: bool
    in_package: bool  # part of the transpiled package (not only reached through imports)
    defs: dict[str, ast.AST] = field(default_factory=dict)  # top-level class/def/assign targets
    imports: dict[str, tuple] = field(default_factory=dict)  # local name -> import spec
    stars: list[str] = field(default_factory=list)  # modules imported with `*`

    @property
    def package(self) -> str:
        """The package relative imports are resolved against."""
        return self.name if self.is_pkg else self.name.rpartition(".")[0]


def _import_specs(stmts, package: str) -> tuple[dict[str, tuple], list[str]]:
    """Import statements directly in `stmts` -> {local: spec}, star modules.

    spec = ("module", dotted) for `import a.b as x` / ("from", module, name) for `from m import n`.
    """
    out: dict[str, tuple] = {}
    stars: list[str] = []
    for node in stmts:
        if isinstance(node, ast.Import):
            for a in node.names:
                if a.asname:
                    out[a.asname] = ("module", a.name)
                else:  # `import a.b` binds `a`
                    top = a.name.split(".")[0]
                    out[top] = ("module", top)
        elif isinstance(node, ast.ImportFrom):
            base = node.module or ""
            if node.level:
                parts = package.split(".") if package else []
                if node.level - 1 > len(parts):
                    continue
                parts = parts[: len(parts) - (node.level - 1)]
                base = ".".join(p for p in [*parts, node.module or ""] if p)
            for a in node.names:
                if a.name == "*":
                    stars.append(base)
                else:
                    out[a.asname or a.name] = ("from", base, a.name)
        elif isinstance(node, (ast.If, ast.Try)):  # `if TYPE_CHECKING:` / try-import fallbacks
            inner = list(node.body)
            if isinstance(node, ast.Try):
                for h in node.handlers:
                    inner += h.body
            o, s = _import_specs(inner, package)
            for k, v in o.items():
                out.setdefault(k, v)
            stars += s
    return out, stars


def function_imports(fn: ast.AST, package: str) -> dict[str, tuple]:
    """Imports made inside a function body (lazy imports), anywhere in it."""
    stmts = [n for n in ast.walk(fn) if isinstance(n, (ast.Import, ast.ImportFrom))]
    return _import_specs(stmts, package)[0]


class ModuleIndex:
    def __init__(self, root: Path, package: Path):
        self.root = root.resolve()
        self.package_dir = package.resolve()
        self.modules: dict[str, Module] = {}
        self.syntax_errors: list[tuple[str, SyntaxError]] = []
        self._missing: set[str] = set()
        for path in sorted(self.package_dir.rglob("*.py")):
            if SKIP_DIRS & set(path.relative_to(self.package_dir).parts):
                continue
            self._load_path(path, in_package=True)

    # ------------------------------------------------------------------ loading

    def _modname(self, path: Path) -> str:
        rel = path.resolve().relative_to(self.root).with_suffix("")
        parts = list(rel.parts)
        if parts[-1] == "__init__":
            parts = parts[:-1]
        return ".".join(parts)

    def _load_path(self, path: Path, in_package: bool) -> Module | None:
        name = self._modname(path)
        if name in self.modules:
            return self.modules[name]
        try:
            tree = ast.parse(path.read_text(), filename=str(path))
        except SyntaxError as e:
            self.syntax_errors.append((str(path), e))
            return None
        m = Module(name, path, tree, path.name == "__init__.py", in_package)
        for node in tree.body:
            if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
                m.defs[node.name] = node
            elif isinstance(node, ast.Assign):
                for t in node.targets:
                    if isinstance(t, ast.Name):
                        m.defs[t.id] = node
            elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
                m.defs[node.target.id] = node
        m.imports, m.stars = _import_specs(tree.body, m.package)
        self.modules[name] = m
        return m

    def module(self, name: str) -> Module | None:
        """A project module by dotted name, parsed on demand; None if outside the project."""
        if name in self.modules:
            return self.modules[name]
        if not name or name in self._missing:
            return None
        base = self.root.joinpath(*name.split("."))
        for path in (base.with_suffix(".py"), base / "__init__.py"):
            if path.is_file() and self._exact_case(path):
                return self._load_path(path, in_package=False)
        self._missing.add(name)
        return None

    def _exact_case(self, path: Path) -> bool:
        """On a case-insensitive file system (macOS), `models/Item.py` "exists" when only
        `models/item.py` does: `from models import Item` must not resolve to a module."""
        cur = self.root
        for part in path.relative_to(self.root).parts:
            if part not in os.listdir(cur):
                return False
            cur = cur / part
        return True

    def module_of(self, path: str | Path) -> Module:
        return self.modules[self._modname(Path(path))]

    def package_modules(self) -> list[Module]:
        return [m for m in self.modules.values() if m.in_package]

    # ------------------------------------------------------------------ resolution

    def resolve(self, module: str, name: str, scope: ast.AST | None = None, _seen=None) -> Target | None:
        """What `name` refers to inside `module` (or inside function `scope` of it)."""
        seen = _seen if _seen is not None else set()
        if (module, name) in seen:
            return None  # import cycle
        seen.add((module, name))
        m = self.module(module)
        if m is None:
            return Ext(f"{module}.{name}")
        if scope is not None:
            local = function_imports(scope, m.package)
            if name in local:
                return self._spec(local[name], seen)
        if name in m.defs:
            return Sym(module, name)
        if name in m.imports:
            return self._spec(m.imports[name], seen)
        for star in m.stars:
            t = self.resolve(star, name, _seen=seen)
            if t is not None and not (isinstance(t, Ext) and self.module(star) is not None):
                return t
        if m.is_pkg and self.module(f"{module}.{name}") is not None:
            return ModRef(f"{module}.{name}")
        return None

    def _spec(self, spec: tuple, seen) -> Target | None:
        if spec[0] == "module":
            return ModRef(spec[1]) if self.module(spec[1]) is not None else Ext(spec[1])
        _, mod, name = spec
        if self.module(f"{mod}.{name}") is not None:  # `from pkg import submodule`
            return ModRef(f"{mod}.{name}")
        if self.module(mod) is None:
            return Ext(f"{mod}.{name}")
        return self.resolve(mod, name, _seen=seen)

    def resolve_expr(self, module: str, node: ast.AST, scope: ast.AST | None = None) -> Target | None:
        """`users_view.router`, `fastapi.APIRouter`, `Base` ... -> target."""
        if isinstance(node, ast.Name):
            return self.resolve(module, node.id, scope)
        if isinstance(node, ast.Attribute):
            base = self.resolve_expr(module, node.value, scope)
            if isinstance(base, ModRef):
                return self.resolve(base.module, node.attr)
            if isinstance(base, Ext):
                return Ext(f"{base.dotted}.{node.attr}")
        return None

    def definition(self, sym: Sym) -> ast.AST | None:
        m = self.module(sym.module)
        return m.defs.get(sym.name) if m else None


def is_ext(t: Target | None, *names: str) -> bool:
    """`t` is one of the external names, compared on the last component and the package
    (`fastapi.APIRouter` matches `fastapi.routing.APIRouter`)."""
    if not isinstance(t, Ext):
        return False
    for n in names:
        pkg, _, last = n.rpartition(".")
        if t.dotted == n or (t.dotted.split(".")[-1] == last and t.package == pkg.split(".")[0]):
            return True
    return False

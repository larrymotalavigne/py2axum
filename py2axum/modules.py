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
    binders: dict[str, list[ast.stmt]] = field(default_factory=dict)  # name -> top-level statements binding it

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


def _static_value(e: ast.expr) -> bool:
    """An expression that cannot raise: constants, names, attributes of names, tuples of them."""
    if isinstance(e, ast.Constant):
        return True
    if isinstance(e, ast.Name):
        return True
    if isinstance(e, ast.Attribute):
        return _static_value(e.value)
    if isinstance(e, (ast.Tuple, ast.List)):
        return all(_static_value(x) for x in e.elts)
    return False


def stmt_bindings(st: ast.stmt, handlers: bool = False) -> list[str]:
    """Names a module-level statement binds (assignments, loop and `as` targets), in order; nested functions,
    classes, lambdas and comprehensions have their own scopes. `except E as e` names only with `handlers`:
    CPython deletes them when the handler ends."""
    out: list[str] = []

    def target(t):
        if isinstance(t, ast.Name):
            out.append(t.id)
        elif isinstance(t, (ast.Tuple, ast.List)):
            for e in t.elts:
                target(e)
        elif isinstance(t, ast.Starred):
            target(t.value)

    def visit(n):
        if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            out.append(n.name)
            return
        if isinstance(n, (ast.Lambda, ast.ListComp, ast.SetComp, ast.DictComp, ast.GeneratorExp)):
            return
        if isinstance(n, ast.Assign):
            for t in n.targets:
                target(t)
        elif isinstance(n, (ast.AnnAssign, ast.AugAssign)):
            if not isinstance(n, ast.AnnAssign) or n.value is not None:
                target(n.target)
        elif isinstance(n, (ast.For, ast.AsyncFor)):
            target(n.target)
        elif isinstance(n, ast.ExceptHandler) and n.name and handlers:
            out.append(n.name)
        elif isinstance(n, (ast.With, ast.AsyncWith)):
            for it in n.items:
                if it.optional_vars is not None:
                    target(it.optional_vars)
        elif isinstance(n, ast.NamedExpr):
            target(n.target)
        for c in ast.iter_child_nodes(n):
            visit(c)

    visit(st)
    return list(dict.fromkeys(out))


def startup_skipped(st: ast.stmt) -> bool:
    """`if __name__ == "__main__":` (never true in the binary) and `if TYPE_CHECKING:` (imports only)."""
    if not isinstance(st, ast.If):
        return False
    t = st.test
    if isinstance(t, ast.Compare) and isinstance(t.left, ast.Name) and t.left.id == "__name__":
        return True
    return (isinstance(t, ast.Name) and t.id == "TYPE_CHECKING") or (isinstance(t, ast.Attribute) and t.attr == "TYPE_CHECKING")


def import_guard(node: ast.stmt) -> bool:
    """A module-level `try` whose body only imports and binds static values (an import fallback): only a
    failed import could reach its handlers."""
    if not isinstance(node, ast.Try) or node.finalbody or not node.handlers:
        return False
    for st in [*node.body, *node.orelse]:
        if isinstance(st, (ast.Import, ast.ImportFrom, ast.Pass)):
            continue
        if isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant):
            continue
        if isinstance(st, ast.Assign) and all(isinstance(t, ast.Name) for t in st.targets) and _static_value(st.value):
            continue
        if isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name) and st.value is not None and _static_value(st.value):
            continue
        return False
    return True


def function_imports(fn: ast.AST, package: str) -> dict[str, tuple]:
    """Imports made inside a function body (lazy imports), anywhere in it."""
    stmts = [n for n in ast.walk(fn) if isinstance(n, (ast.Import, ast.ImportFrom))]
    return _import_specs(stmts, package)[0]


class _FoldVersion(ast.NodeTransformer):
    """`sys.version_info` read against the project's Python (dyn.target_python): comparisons with a tuple of
    ints, `.major`/`.minor`, `[0]`, `[1]` and `[:2]` become constants, and an `if` (or `x if c else y`) whose
    test folds keeps only the branch taken, so a version-gated import or definition the target Python never
    runs is never translated (nor needs to resolve). What depends on the patch level (`.micro`, a 3-item
    tuple of the same minor, `sys.version`) is left as written: refused where it is used."""

    OPS = {ast.Lt: "__lt__", ast.LtE: "__le__", ast.Gt: "__gt__", ast.GtE: "__ge__", ast.Eq: "__eq__", ast.NotEq: "__ne__"}

    def __init__(self, tree: ast.Module, python: tuple[int, int]):
        self.py = python
        self.dropped: list[ast.stmt] = []  # the statements of the branches not taken
        self.mods, self.names = set(), set()
        for n in ast.walk(tree):
            if isinstance(n, ast.Import):
                self.mods |= {a.asname or a.name for a in n.names if a.name == "sys"}
            elif isinstance(n, ast.ImportFrom) and n.module == "sys" and not n.level:
                self.names |= {a.asname or a.name for a in n.names if a.name == "version_info"}

    def _vi(self, e) -> bool:
        return ((isinstance(e, ast.Attribute) and e.attr == "version_info" and isinstance(e.value, ast.Name)
                 and e.value.id in self.mods) or (isinstance(e, ast.Name) and e.id in self.names))

    def _cmp(self, op, t: tuple):
        """version_info <op> t, or None when it depends on more than (major, minor)."""
        k = min(len(t), 2)
        cur, head = self.py[:k], t[:k]
        if cur != head:
            return getattr(cur, self.OPS[type(op)])(head)
        if len(t) > 2:
            return None
        # equal prefix, version_info longer: strictly greater
        return {ast.Lt: False, ast.LtE: False, ast.Gt: True, ast.GtE: True, ast.Eq: False, ast.NotEq: True}[type(op)]

    def visit_Compare(self, node):
        self.generic_visit(node)
        if len(node.ops) != 1 or type(node.ops[0]) not in self.OPS:
            return node
        left, right, op = node.left, node.comparators[0], node.ops[0]
        flip = {ast.Lt: ast.Gt(), ast.LtE: ast.GtE(), ast.Gt: ast.Lt(), ast.GtE: ast.LtE(), ast.Eq: ast.Eq(), ast.NotEq: ast.NotEq()}
        if self._vi(right) and not self._vi(left):
            left, right, op = right, left, flip[type(op)]
        if not self._vi(left) or not isinstance(right, ast.Tuple) or not right.elts or not all(
                isinstance(x, ast.Constant) and type(x.value) is int for x in right.elts):
            return node
        r = self._cmp(op, tuple(x.value for x in right.elts))
        return node if r is None else self._const(r, node)

    def visit_Attribute(self, node):
        if self._vi(node.value) and node.attr in ("major", "minor"):
            return self._const(self.py[node.attr == "minor"], node)
        return self.generic_visit(node)

    def visit_Subscript(self, node):
        if not self._vi(node.value):
            return self.generic_visit(node)
        sl = node.slice
        if isinstance(sl, ast.Constant) and sl.value in (0, 1):
            return self._const(self.py[sl.value], node)
        if (isinstance(sl, ast.Slice) and sl.step is None and (sl.lower is None or (isinstance(sl.lower, ast.Constant) and sl.lower.value == 0))
                and isinstance(sl.upper, ast.Constant) and sl.upper.value in (1, 2)):
            return ast.copy_location(ast.Tuple([self._const(v, node) for v in self.py[:sl.upper.value]], ast.Load()), node)
        return node

    def visit_UnaryOp(self, node):
        self.generic_visit(node)
        if isinstance(node.op, ast.Not) and isinstance(node.operand, ast.Constant) and type(node.operand.value) is bool:
            return self._const(not node.operand.value, node)
        return node

    def visit_IfExp(self, node):
        self.generic_visit(node)
        if isinstance(node.test, ast.Constant) and type(node.test.value) is bool and self._folded(node.test):
            return node.body if node.test.value else node.orelse
        return node

    def _folded(self, c) -> bool:
        return getattr(c, "_py2axum_folded", False)

    def _const(self, value, like):
        """A constant marked as made by this pass (a literal `if True:` is left alone)."""
        c = ast.copy_location(ast.Constant(value), like)
        c._py2axum_folded = True
        return c

    def visit_If(self, node):
        self.generic_visit(node)
        if isinstance(node.test, ast.Constant) and type(node.test.value) is bool and self._folded(node.test):
            self.dropped += node.orelse if node.test.value else node.body
            return (node.body if node.test.value else node.orelse) or []
        return node

    def generic_visit(self, node):
        super().generic_visit(node)
        # a body emptied by a folded `if` keeps a statement (`pass`, at the `if`'s line)
        for f in ("body", "orelse", "finalbody"):
            v = getattr(node, f, None)
            if isinstance(v, list) and not v and f == "body" and not isinstance(node, ast.Module):
                v.append(ast.copy_location(ast.Pass(), node))
        return node


def fold_version_info(tree: ast.Module, python: tuple[int, int]) -> tuple[ast.Module, list[ast.stmt]]:
    """`sys.version_info` read against the project's Python (_FoldVersion): the folded tree, and the
    statements of the branches dropped."""
    if not any((isinstance(n, ast.Attribute) and n.attr == "version_info")
               or (isinstance(n, ast.alias) and n.name == "version_info") for n in ast.walk(tree)):
        return tree, []
    f = _FoldVersion(tree, python)
    return ast.fix_missing_locations(f.visit(tree)), f.dropped


class ModuleIndex:
    def __init__(self, root: Path, package: Path):
        self.root = root.resolve()
        self.package_dir = package.resolve()
        self.modules: dict[str, Module] = {}
        self.syntax_errors: list[tuple[str, SyntaxError]] = []
        self.gated: set[str] = set()  # modules imported only in a branch a `sys.version_info` test dropped
        # `lazy import` (Python 3.15, PEP 810): (module name, statement, the modules it names)
        self.lazy: list[tuple[str, ast.stmt, list[str]]] = []
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
        from .dyn import target_python

        tree, dropped = fold_version_info(tree, target_python(self.root))
        m = Module(name, path, tree, path.name == "__init__.py", in_package)
        for node in tree.body:
            if import_guard(node):
                # `try: import x; FLAG = True / except ImportError: FLAG = False`: the imports were resolved at
                # compile time, so the body's bindings are the module's (its handlers never run)
                for st in [*node.body, *node.orelse]:
                    if isinstance(st, ast.Assign):
                        for t in st.targets:
                            if isinstance(t, ast.Name):
                                m.defs[t.id] = st
                    elif isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name):
                        m.defs[st.target.id] = st
            elif isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
                m.defs[node.name] = node
            elif (isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], (ast.Tuple, ast.List))
                  and isinstance(node.value, (ast.Tuple, ast.List)) and len(node.targets[0].elts) == len(node.value.elts)
                  and all(isinstance(t, ast.Name) for t in node.targets[0].elts)
                  and not any(isinstance(v, ast.Starred) for v in node.value.elts)):
                # `A, B = "a", "b"`: one plain assignment per name (literal tuple of the same length)
                for t, v in zip(node.targets[0].elts, node.value.elts):
                    one = ast.copy_location(ast.Assign(targets=[ast.Name(t.id, ast.Store())], value=v), node)
                    m.defs[t.id] = ast.fix_missing_locations(one)
                    m.binders.setdefault(t.id, []).append(one)
                continue
            elif isinstance(node, ast.Assign):
                for t in node.targets:
                    if isinstance(t, ast.Name):
                        m.defs[t.id] = node
            elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
                m.defs[node.target.id] = node
            elif isinstance(node, (ast.Try, ast.If, ast.For, ast.While, ast.With)) and not startup_skipped(node):
                # a compound statement run at startup: the names it binds are module variables
                for n in stmt_bindings(node):
                    m.defs[n] = node
            else:
                continue
            for n in ({node.name} if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef))
                      else stmt_bindings(node)):
                m.binders.setdefault(n, []).append(node)
        m.imports, m.stars = _import_specs(tree.body, m.package)
        for node in tree.body:
            if getattr(node, "is_lazy", 0):
                specs, _ = _import_specs([node], m.package)
                self.lazy.append((name, node, sorted({x for spec in specs.values()
                                                      for x in ((spec[1], f"{spec[1]}.{spec[2]}") if spec[0] == "from" else (spec[1],))})))
        gone, gone_stars = _import_specs([n for d in dropped for n in ast.walk(d) if isinstance(n, (ast.Import, ast.ImportFrom))], m.package)
        self.gated |= {x for spec in gone.values() for x in ((spec[1], f"{spec[1]}.{spec[2]}") if spec[0] == "from" else (spec[1],))}
        self.gated |= set(gone_stars)
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

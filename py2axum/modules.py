"""Static module index: qualified module names and import resolution, without running any code.

`root` plays the role of `sys.path`: `api/views/users.py` under `root` is module `api.views.users`.
The transpiled package is parsed eagerly; any other module under `root` it imports (e.g. a sibling
`shared/` package holding the models) is parsed on demand. Anything else is an external package.
"""
from __future__ import annotations

import ast
import re
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
    lib: str | None = None  # the distribution it was translated from (--translate-libs), None for the project

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


def star_exports(tree: ast.Module, name: str) -> bool:
    """`from m import *` binds `name` of m: the names of its literal `__all__`, else its names without a leading
    `_` (imported ones included, like CPython). A `__all__` that is not a literal list or tuple of strings exports
    nothing here (refused later as an unknown name, never guessed)."""
    alls = [st for st in ast.walk(tree) if isinstance(st, (ast.Assign, ast.AnnAssign, ast.AugAssign))
            and any(isinstance(t, ast.Name) and t.id == "__all__" for t in (st.targets if isinstance(st, ast.Assign) else [st.target]))]
    if not alls:
        return not name.startswith("_")
    st = alls[0]
    if len(alls) > 1 or isinstance(st, ast.AugAssign) or st not in tree.body or not isinstance(st.value, (ast.List, ast.Tuple)) \
            or not all(isinstance(e, ast.Constant) and isinstance(e.value, str) for e in st.value.elts):
        return False
    return name in {e.value for e in st.value.elts}


def dotted_name(e: ast.AST) -> str | None:
    if isinstance(e, ast.Name):
        return e.id
    if isinstance(e, ast.Attribute):
        base = dotted_name(e.value)
        return f"{base}.{e.attr}" if base else None
    return None


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
        if (c := self._folded_int_cmp(node)) is not None:
            return c
        left, right, op = node.left, node.comparators[0], node.ops[0]
        flip = {ast.Lt: ast.Gt(), ast.LtE: ast.GtE(), ast.Gt: ast.Lt(), ast.GtE: ast.LtE(), ast.Eq: ast.Eq(), ast.NotEq: ast.NotEq()}
        if self._vi(right) and not self._vi(left):
            left, right, op = right, left, flip[type(op)]
        if not self._vi(left) or not isinstance(right, ast.Tuple) or not right.elts or not all(
                isinstance(x, ast.Constant) and type(x.value) is int for x in right.elts):
            return node
        r = self._cmp(op, tuple(x.value for x in right.elts))
        return node if r is None else self._const(r, node)

    def _folded_int_cmp(self, node):
        """`sys.version_info[0] == 3` once the subscript is folded: two int constants, one of them folded"""
        left, right, op = node.left, node.comparators[0], node.ops[0]
        if (isinstance(left, ast.Constant) and isinstance(right, ast.Constant) and type(left.value) is int
                and type(right.value) is int and (self._folded(left) or self._folded(right))):
            return self._const(getattr(left.value, self.OPS[type(op)])(right.value), node)
        return None

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


def class_scope_defaults(tree: ast.Module) -> None:
    """The defaults of a method are evaluated in its class body (`def construct(p, tolerance=DEFAULT_TOLERANCE)` after
    `DEFAULT_TOLERANCE = 300` in the class): such a name becomes `Class.NAME`, the class attribute that holds the
    same value. A name the class body binds again after the method would have changed since: it becomes a name no
    scope has (refused where it is compiled, never read from the module)."""
    for cls in tree.body:
        if not isinstance(cls, ast.ClassDef):
            continue
        bound: dict[str, list[int]] = {}
        for i, st in enumerate(cls.body):
            targets = st.targets if isinstance(st, ast.Assign) else [st.target] if isinstance(st, ast.AnnAssign) and st.value else []
            for t in targets:
                if isinstance(t, ast.Name):
                    bound.setdefault(t.id, []).append(i)
        if not bound:
            continue
        for i, st in enumerate(cls.body):
            if not isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            for d in [*st.args.defaults, *(x for x in st.args.kw_defaults if x is not None)]:
                for n in ast.walk(d):
                    for field_, v in ast.iter_fields(n):
                        if isinstance(v, ast.Name) and v.id in bound and any(j < i for j in bound[v.id]):
                            setattr(n, field_, _class_ref(cls.name, v, all(j < i for j in bound[v.id])))
                        elif isinstance(v, list):
                            for k, x in enumerate(v):
                                if isinstance(x, ast.Name) and x.id in bound and any(j < i for j in bound[x.id]):
                                    v[k] = _class_ref(cls.name, x, all(j < i for j in bound[x.id]))
            for k, d in enumerate(st.args.defaults):
                if isinstance(d, ast.Name) and d.id in bound and any(j < i for j in bound[d.id]):
                    st.args.defaults[k] = _class_ref(cls.name, d, all(j < i for j in bound[d.id]))
            for k, d in enumerate(st.args.kw_defaults):
                if isinstance(d, ast.Name) and d.id in bound and any(j < i for j in bound[d.id]):
                    st.args.kw_defaults[k] = _class_ref(cls.name, d, all(j < i for j in bound[d.id]))


def _class_ref(cls: str, name: ast.Name, once: bool) -> ast.expr:
    if not once:
        return ast.copy_location(ast.Name(f"{name.id}__rebound_later_in_class_{cls}", ast.Load()), name)
    return ast.copy_location(ast.Attribute(ast.copy_location(ast.Name(cls, ast.Load()), name), name.id, ast.Load()), name)


def drop_overloads(tree: ast.Module) -> None:
    """`@typing.overload` signatures (module level and class bodies): the implementation that follows rebinds the
    name, as at run time."""
    def is_overload(d):
        return (isinstance(d, ast.Name) and d.id == "overload") or (isinstance(d, ast.Attribute) and d.attr == "overload")

    def clean(body):
        return [st for st in body if not (isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef))
                                          and any(is_overload(d) for d in st.decorator_list))]

    tree.body = clean(tree.body)
    for n in ast.walk(tree):
        if isinstance(n, ast.ClassDef):
            n.body = clean(n.body) or [ast.copy_location(ast.Pass(), n)]


def fold_flags(tree: ast.Module) -> ast.Module:
    """Module constants made of a folded version or platform test (`PY3 = sys.version_info[0] == 3`, `WINDOWS =
    sys.platform == "win32"`), bound once: an `if PY3:` (or `not PY3`, `x if PY3 else y`) keeps only its branch,
    in the module and in the functions that do not bind the name themselves."""
    from collections import Counter

    counts: Counter = Counter()
    for st in tree.body:
        for n in stmt_bindings(st):
            counts[n] += 1
    consts = {}
    for st in tree.body:
        if (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name)
                and isinstance(st.value, ast.Constant) and type(st.value.value) is bool
                and getattr(st.value, "_py2axum_folded", False) and counts[st.targets[0].id] == 1):
            consts[st.targets[0].id] = st.value.value
    for n in ast.walk(tree):
        if isinstance(n, ast.Global):
            for x in n.names:
                consts.pop(x, None)
    if not consts:
        return tree

    class Flags(ast.NodeTransformer):
        def __init__(self):
            self.shadow: list[set[str]] = []

        def value(self, t):
            if isinstance(t, ast.Name) and t.id in consts and not any(t.id in s for s in self.shadow):
                return consts[t.id]
            if isinstance(t, ast.UnaryOp) and isinstance(t.op, ast.Not):
                v = self.value(t.operand)
                return None if v is None else not v
            return None

        def visit_FunctionDef(self, node):
            local = {a.arg for a in [*node.args.posonlyargs, *node.args.args, *node.args.kwonlyargs]}
            local |= {x.id for x in ast.walk(node) if isinstance(x, ast.Name) and isinstance(x.ctx, ast.Store)}
            self.shadow.append(local)
            self.generic_visit(node)
            self.shadow.pop()
            return node

        visit_AsyncFunctionDef = visit_FunctionDef

        def visit_If(self, node):
            self.generic_visit(node)
            v = self.value(node.test)
            if v is None:
                return node
            return (node.body if v else node.orelse) or [ast.copy_location(ast.Pass(), node)]

        def visit_IfExp(self, node):
            self.generic_visit(node)
            v = self.value(node.test)
            return node if v is None else (node.body if v else node.orelse)

    return ast.fix_missing_locations(Flags().visit(tree))


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
        from . import libsrc

        self.libs = libsrc.index(self.root)  # --translate-libs: the project's site-packages (None: off)
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

    def _load_path(self, path: Path, in_package: bool, lib=None, name: str | None = None) -> Module | None:
        name = name or self._modname(path)
        if name in self.modules:
            return self.modules[name]
        from .dyn import target_python

        try:
            if lib is not None:
                from .libsrc import parse_cached

                tree, dropped = parse_cached(lib, path, target_python(self.root))
            else:
                tree = ast.parse(path.read_text(), filename=str(path))
        except SyntaxError as e:
            self.syntax_errors.append((str(path), e))
            return None
        if lib is None:
            tree, dropped = fold_version_info(tree, target_python(self.root))
            from .sqlmodel_rewrite import imports_sqlmodel, rewrite

            if imports_sqlmodel(tree):
                # SQLModel classes rewritten into mapped classes and Pydantic models; their base loaded now (the
                # classification of the project's classes reads it)
                tree = rewrite(tree)
                self.load_shim("sqlmodel.SQLModel")
            from .pydrewrite import rewrite as rewrite_create_model

            if not hasattr(self, "_pyd_namespace"):
                from .dyn import locked_version

                v = tuple(int(x) for x in re.findall(r"\d+", locked_version(self.root, "pydantic") or "2.14")[:2])
                self._pyd_namespace = v >= (2, 14)
            tree = rewrite_create_model(tree, self._pyd_namespace)  # `M = create_model('M', ...)` as the class it creates
            tree = fold_flags(tree)
        else:
            self._decide_import_guards(tree, name, path.name == "__init__.py")
        drop_overloads(tree)
        class_scope_defaults(tree)
        m = Module(name, path, tree, path.name == "__init__.py", in_package, lib=lib.name if lib is not None else None)
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

    def _lazy_attr(self, m: Module, name: str):
        """A name a module's `__getattr__` (PEP 562) imports lazily, in the two static shapes libraries use:
        a literal map of names to modules (`target, is_submodule = _import_map[name]; module =
        import_module(target)`, stripe) and `if name in __all__: return import_module("." + name, __name__)`
        (dateutil). A ModRef (a submodule), ("module", name) (the name read on that module), or None."""
        ga = m.defs.get("__getattr__")
        if not isinstance(ga, ast.FunctionDef) or name.startswith("__"):
            return None
        calls = [n for n in ast.walk(ga) if isinstance(n, ast.Call) and (dotted_name(n.func) or "").split(".")[-1] == "import_module"]
        if not calls:
            return None
        for n in ast.walk(ga):
            # `_import_map[name]` of a module-level literal dict {"Name": ("pkg.mod", is_submodule) | "pkg.mod"}
            if isinstance(n, ast.Subscript) and isinstance(n.value, ast.Name) and isinstance(n.slice, ast.Name):
                d = m.defs.get(n.value.id)
                if isinstance(d, ast.Assign) and isinstance(d.value, ast.Dict):
                    for k, v in zip(d.value.keys, d.value.values):
                        if not (isinstance(k, ast.Constant) and k.value == name):
                            continue
                        if isinstance(v, ast.Constant) and isinstance(v.value, str):
                            return (v.value, name)
                        if (isinstance(v, ast.Tuple) and len(v.elts) == 2 and all(isinstance(e, ast.Constant) for e in v.elts)
                                and isinstance(v.elts[0].value, str) and isinstance(v.elts[1].value, bool)):
                            target, sub = v.elts[0].value, v.elts[1].value
                            if sub:
                                return ModRef(target) if self.module(target) is not None else Ext(target)
                            return (target, name)
                    return None
            # `if name in __all__: return import_module("." + name, __name__)`
            if (isinstance(n, ast.If) and isinstance(n.test, ast.Compare) and len(n.test.ops) == 1
                    and isinstance(n.test.ops[0], ast.In) and isinstance(n.test.comparators[0], ast.Name)
                    and n.test.comparators[0].id == "__all__" and len(n.body) == 1 and isinstance(n.body[0], ast.Return)):
                call = n.body[0].value
                if (isinstance(call, ast.Call) and call in calls and len(call.args) == 2
                        and ast.unparse(call.args[0]).replace(" ", "") in ("'.'+name", '"."+name')
                        and isinstance(call.args[1], ast.Name) and call.args[1].id == "__name__"
                        and m.is_pkg and star_exports(m.tree, name) and "__all__" in m.defs):
                    sub = f"{m.name}.{name}"
                    return ModRef(sub) if self.module(sub) is not None else Ext(sub)
        return None

    def _decide_import_guards(self, tree: ast.Module, name: str, is_pkg: bool) -> None:
        """A library's `try: import x / except ImportError: <fallback>` at module level, decided by the environment
        as CPython decides it at import: the fallback replaces the `try` when an imported module is not installed."""
        package = name if is_pkg else name.rpartition(".")[0]
        body = []
        for node in tree.body:
            if import_guard(node) and not node.orelse:
                catch = [h for h in node.handlers if h.type is None or (dotted_name(h.type) or "") in
                         ("ImportError", "ModuleNotFoundError", "Exception", "BaseException")]
                def fails(st):
                    specs, stars = _import_specs([st], package)
                    return any(not self.importable(m) for m in {spec[1] for spec in specs.values()} | set(stars))

                i = next((k for k, st in enumerate(node.body) if fails(st)), None)
                if i is not None and catch:
                    # what ran before the failing import stays bound, then the handler runs
                    body += node.body[:i] + catch[0].body
                    continue
            body.append(node)
        tree.body = body

    def importable(self, name: str) -> bool:
        """`import name` succeeds in the project's environment: a module of the project or of a translated library,
        a module of the standard library, or a module of an installed distribution."""
        if self.module(name) is not None:
            return True
        top = name.split(".")[0]
        if self.libs is not None and top in self.libs.std:
            import importlib.util

            try:
                return importlib.util.find_spec(name) is not None
            except (ImportError, ValueError):
                return False
        if self.libs is not None and top in self.libs.by_top:
            dist = self.libs.by_top[top]
            base = dist.site.joinpath(*name.split("."))
            return any(p.exists() for p in (base.with_suffix(".py"), base / "__init__.py")) or any(
                p.parent == base.parent and p.name.startswith(base.name + ".") for p in base.parent.glob(base.name + ".*"))
        return False

    SHIMS = {"sqlalchemy.ext.hybrid.Comparator": ("_py2axum_shim_sqlalchemy_hybrid", "sqlalchemy_hybrid.py"),
             "sqlmodel.SQLModel": ("_py2axum_shim_sqlmodel", "sqlmodel_base.py")}

    def load_shim(self, dotted: str) -> Sym | None:
        """A library class py2axum translates from Python source of its own (`py2axum/shims/`): the class as a
        project symbol of a module outside the project, loaded once. None for any other library name."""
        if dotted not in self.SHIMS:
            return None
        name, file = self.SHIMS[dotted]
        if name not in self.modules:
            path = Path(__file__).resolve().parent / "shims" / file
            tree = ast.parse(path.read_text(), filename=str(path))
            m = Module(name, path, tree, False, False)
            for node in tree.body:
                if isinstance(node, ast.ClassDef):
                    m.defs[node.name] = node
                    m.binders.setdefault(node.name, []).append(node)
            m.imports, m.stars = _import_specs(tree.body, "")
            self.modules[name] = m
        return Sym(name, dotted.rpartition(".")[2])

    def is_shim(self, t) -> bool:
        return isinstance(t, Sym) and self.is_shim_module(t.module)

    def is_shim_module(self, module: str) -> bool:
        return any(module == m for m, _ in self.SHIMS.values())

    def _ext(self, dotted: str) -> Target:
        """A library name: the class py2axum translates from its own source when it has one (`load_shim`)"""
        return self.load_shim(dotted) or Ext(dotted)

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
        if self.libs is not None and (found := self.libs.locate(name)) is not None:
            path, _site, dist = found
            return self._load_path(path, in_package=False, lib=dist, name=name)
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

    def resolve(self, module: str, name: str, scope: ast.AST | None = None, _seen=None, _bound_only=False) -> Target | None:
        """What `name` refers to inside `module` (or inside function `scope` of it).

        `_bound_only`: only what the module itself binds (definitions, imports, stars of project modules), never a
        submodule it does not bind nor a star of an external module (which may or may not export it)."""
        if name.startswith("__p2a_"):
            # the names of an annotation the translator writes (coreschema.annotation): the library's own
            return Ext(SYNTHETIC[name]) if name in SYNTHETIC else None
        seen = _seen if _seen is not None else set()
        if (module, name) in seen:
            return None  # import cycle
        seen.add((module, name))
        m = self.module(module)
        if m is None:
            return self._ext(f"{module}.{name}")
        if scope is not None:
            local = function_imports(scope, m.package)
            if name in local:
                return self._spec(local[name], seen)
        if name in m.defs:
            return Sym(module, name)
        if name in m.imports:
            return self._spec(m.imports[name], seen)
        for star in m.stars:
            sm = self.module(star)
            if sm is not None and not star_exports(sm.tree, name):
                continue
            if sm is None and _bound_only:
                continue
            t = self.resolve(star, name, _seen=seen)
            if t is not None:
                return t
        lazy = self._lazy_attr(m, name)
        if lazy is not None:
            return lazy if not isinstance(lazy, tuple) else self.resolve(lazy[0], name, _seen=seen)
        if _bound_only:
            return None
        if m.is_pkg and self.module(f"{module}.{name}") is not None:
            return ModRef(f"{module}.{name}")
        if (m.lib is not None and m.is_pkg and self.libs.native_module(f"{module}.{name}")
                and ((m.path.parent / f"{name}.py").is_file() or (m.path.parent / name / "__init__.py").is_file())):
            return self._ext(f"{module}.{name}")  # a submodule the library map translates (dateutil.relativedelta)
        return None

    def _spec(self, spec: tuple, seen) -> Target | None:
        if spec[0] == "module":
            return ModRef(spec[1]) if self.module(spec[1]) is not None else Ext(spec[1])
        _, mod, name = spec
        if self.module(f"{mod}.{name}") is not None:  # `from pkg import submodule`
            # CPython reads the package's attribute first: a name its `__init__` binds (`from .nom import nom`,
            # `from .nom import *`) is that object, not the submodule (python-slugify, tenacity, email-validator)
            bound = self.resolve(mod, name, _seen=set(seen), _bound_only=True) if self.module(mod) else None
            return bound if bound is not None else ModRef(f"{mod}.{name}")
        if self.module(mod) is None:
            return self._ext(f"{mod}.{name}")
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
                return self._ext(f"{base.dotted}.{node.attr}")
        return None

    def definition(self, sym: Sym) -> ast.AST | None:
        m = self.module(sym.module)
        return m.defs.get(sym.name) if m else None


# the roots of annotations built by the translator itself (py2axum/coreschema.py: `__p2a_typing.Annotated[...]`,
# `__p2a_pydantic.AfterValidator(f)`), whatever the module imports
SYNTHETIC = {"__p2a_typing": "typing", "__p2a_pydantic": "pydantic"}


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

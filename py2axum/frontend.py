"""Front-end: the discovery passes over a FastAPI + SQLAlchemy + Pydantic package that `dyn.py` builds on.

Finds the SQLAlchemy models, Pydantic schemas and dataclasses, the FastAPI applications, their middlewares,
the routers and their `include_router` chains, and the route decorators. Names are resolved through the
imports of each module (`modules.py`): nothing is imported or executed.

`collect=True` (coverage report) never stops at the first error: app-level blockers are recorded.
"""
from __future__ import annotations

import ast
import re
from dataclasses import dataclass, field
from pathlib import Path

from .ir import Gzip, TranspileError
from .modules import ModuleIndex, Sym, is_ext

HTTP_METHODS = {"get", "post", "put", "patch", "delete"}
DECLARATIVE = ("sqlalchemy.orm.DeclarativeBase", "sqlalchemy.orm.DeclarativeBaseNoMeta")
DECLARATIVE_FACTORIES = ("sqlalchemy.orm.declarative_base", "sqlalchemy.ext.declarative.declarative_base")


def dotted(node: ast.AST) -> str | None:
    """`aiohttp.ClientSession` -> "aiohttp.ClientSession"."""
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        base = dotted(node.value)
        return f"{base}.{node.attr}" if base else None
    return None


def literal(node: ast.AST, file: str):
    try:
        return ast.literal_eval(node)
    except Exception:
        raise TranspileError("expected a literal value here (py2axum reads it without running the code): "
                             "write the constant itself", node, file) from None


@dataclass
class Router:
    sym: Sym
    node: ast.Call
    file: str
    prefix: str = ""
    deps: ast.AST | None = None  # dependencies=[...] node
    unsupported: list[ast.keyword] = field(default_factory=list)


@dataclass
class Mount:
    """One way a router is reached from the app: accumulated prefix and conditions."""

    prefix: str
    conditional: bool = False
    deps: list[tuple[ast.AST, str]] = field(default_factory=list)  # (node, file)
    unsupported: list[tuple[ast.AST, str]] = field(default_factory=list)


def _parents(tree: ast.AST) -> dict[ast.AST, ast.AST]:
    out = {}
    for node in ast.walk(tree):
        for child in ast.iter_child_nodes(node):
            out[child] = node
    return out


def _under_if(node: ast.AST, parents: dict) -> bool:
    cur = parents.get(node)
    while cur is not None and not isinstance(cur, ast.Module):
        if isinstance(cur, (ast.If, ast.IfExp, ast.Try)):
            return True
        cur = parents.get(cur)
    return False


def _enclosing_fn(node: ast.AST, parents: dict) -> ast.AST | None:
    cur = parents.get(node)
    while cur is not None:
        if isinstance(cur, (ast.FunctionDef, ast.AsyncFunctionDef)):
            return cur
        cur = parents.get(cur)
    return None


class Frontend:
    def __init__(self, package: Path, root: Path | None = None, collect: bool = False):
        self.package = package
        self.index = ModuleIndex(root or package.resolve().parent, package)
        self.collect = collect
        # collect mode results
        self.global_errors: list[TranspileError] = []
        self.notes: list[str] = []
        # symbol tables
        self.model_syms: dict[Sym, ast.ClassDef] = {}
        self.schema_syms: dict[Sym, ast.ClassDef] = {}
        self.app_vars: dict[str, set[str]] = {}  # module -> names bound to FastAPI()
        self.routers: dict[Sym, Router] = {}
        # non-literal `include_router(prefix=...)` of module-level calls, read at startup by the dyn backend:
        # the route paths hold the marker `\x01<index>\x01` in their place
        self.dyn_prefixes: list[tuple[ast.expr, str, str]] = []  # (value, file, module)
        self._parents_cache: dict[str, dict] = {}

    def _fail(self, e: TranspileError) -> None:
        if not self.collect:
            raise e

    def _global(self, e: TranspileError) -> None:
        self._fail(e)
        self.global_errors.append(e)

    def _parents(self, module: str) -> dict:
        if module not in self._parents_cache:
            self._parents_cache[module] = _parents(self.index.module(module).tree)
        return self._parents_cache[module]

    def _load_imports(self) -> None:
        """Parse every project module the package imports, transitively (shared model packages)."""
        done: set[str] = set()
        while True:
            todo = [m for m in self.index.modules.values() if m.name not in done]
            if not todo:
                return
            for m in todo:
                done.add(m.name)
                specs = list(m.imports.values())
                for node in ast.walk(m.tree):
                    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                        from .modules import function_imports
                        specs += function_imports(node, m.package).values()
                for spec in specs:
                    if spec[0] == "module":
                        self.index.module(spec[1])
                    else:
                        self.index.module(spec[1]) and self.index.module(f"{spec[1]}.{spec[2]}")
                for star in m.stars:
                    self.index.module(star)

    # ------------------------------------------------------------------ classes

    def _classify(self) -> None:
        """Find SQLAlchemy models and Pydantic schemas by resolving base classes."""
        classes: dict[Sym, ast.ClassDef] = {}
        bases: set[Sym] = set()
        for m in list(self.index.modules.values()):
            for node in m.tree.body:
                if isinstance(node, ast.ClassDef):
                    classes[Sym(m.name, node.name)] = node
                elif isinstance(node, ast.Assign) and isinstance(node.value, ast.Call):
                    if is_ext(self.index.resolve_expr(m.name, node.value.func), *DECLARATIVE_FACTORIES):
                        for t in node.targets:
                            if isinstance(t, ast.Name):
                                bases.add(Sym(m.name, t.id))
        resolved = {
            sym: [self.index.resolve_expr(sym.module, b) for b in node.bases] for sym, node in classes.items()
        }
        for sym, targets in resolved.items():
            if any(is_ext(t, *DECLARATIVE) for t in targets):
                bases.add(sym)
        models: set[Sym] = set()
        schemas: set[Sym] = set()
        changed = True
        while changed:
            changed = False
            for sym, targets in resolved.items():
                if sym in models or sym in schemas or sym in bases:
                    continue
                if any(t in bases or t in models for t in targets):
                    models.add(sym)
                    changed = True
                elif any(is_ext(t, "pydantic.BaseModel", "pydantic_settings.BaseSettings") or t in schemas for t in targets):
                    schemas.add(sym)
                    changed = True
        # @dataclass classes (plain ones: options are checked by the dyn backend)
        self.dataclass_syms = {
            s: n for s, n in classes.items() if s not in models and s not in schemas and any(
                is_ext(self.index.resolve_expr(s.module, d.func if isinstance(d, ast.Call) else d), "dataclasses.dataclass")
                for d in n.decorator_list)
        }
        self.model_syms = {s: classes[s] for s in classes if s in models}
        self.schema_syms = {s: classes[s] for s in classes if s in schemas}
        self._declarative = bases

    # ------------------------------------------------------------------ app object, middleware

    def _discover_apps(self) -> None:
        """`app = FastAPI(...)`, at module level or inside an app factory function."""
        for m in self.index.package_modules():
            for node in ast.walk(m.tree):
                if isinstance(node, (ast.Assign, ast.AnnAssign)) and isinstance(node.value, ast.Call):
                    if dotted(node.value.func) in {"FastAPI", "fastapi.FastAPI"} or is_ext(
                        self.index.resolve_expr(m.name, node.value.func), "fastapi.FastAPI"
                    ):
                        targets = node.targets if isinstance(node, ast.Assign) else [node.target]
                        for t in targets:
                            if isinstance(t, ast.Name):
                                self.app_vars.setdefault(m.name, set()).add(t.id)
                        if _under_if(node, self._parents(m.name)):
                            self.notes.append(
                                f"{m.path}:{node.lineno}: FastAPI() built under a condition; "
                                "every variant is analysed together"
                            )

    def _is_app(self, module: str, node: ast.AST) -> bool:
        return isinstance(node, ast.Name) and node.id in self.app_vars.get(module, ())

    def _asgi_class(self, module: str, func: ast.AST) -> bool:
        """A project class whose `__call__(self, scope, receive, send)` makes its instances ASGI applications."""
        t = self.index.resolve_expr(module, func)
        d = self.index.definition(t) if isinstance(t, Sym) else None
        if not isinstance(d, ast.ClassDef):
            return False
        for f in d.body:
            if isinstance(f, (ast.FunctionDef, ast.AsyncFunctionDef)) and f.name == "__call__":
                return [a.arg for a in f.args.args[1:]] == ["scope", "receive", "send"]
        return False

    def _middlewares(self) -> Gzip | None:
        gzip = None
        for m in self.index.package_modules():
            file = str(m.path)
            for node in m.tree.body:
                # `app = QuotaMiddleware(api)`: the server runs the wrapper, the binary would serve the bare app
                if (isinstance(node, ast.Assign) and isinstance(node.value, ast.Call)
                        and any(self._is_app(m.name, a) for a in [*node.value.args, *(k.value for k in node.value.keywords)])
                        and self._asgi_class(m.name, node.value.func)):
                    self._global(TranspileError(
                        f"the application wrapped in an ASGI class (`{ast.unparse(node.value)}`) is not supported: "
                        "the binary would serve it without the wrapper", node, file))
            for node in ast.walk(m.tree):
                if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
                    owner, attr = node.func.value, node.func.attr
                    if attr == "add_middleware" and self._is_app(m.name, owner):
                        try:
                            gzip = self._middleware(node, file)
                        except TranspileError as e:
                            self._global(e)
                    elif attr in {"add_exception_handler", "mount", "add_route", "add_api_route",
                                  "add_websocket_route", "add_api_websocket_route"} and (
                        self._is_app(m.name, owner)
                        or (isinstance(owner, ast.Attribute) and self._is_app(m.name, owner.value))
                    ):
                        self._global(TranspileError(f"app.{attr}(...) is not supported", node, file))
                elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    for d in node.decorator_list:
                        if (
                            isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute)
                            and self._is_app(m.name, d.func.value)
                            and d.func.attr in {"exception_handler", "middleware", "on_event", "websocket", "websocket_route"}
                        ):
                            # each one is handled by dyn.prepare (compiled, or refused with this message)
                            self._global(TranspileError(f"@app.{d.func.attr}(...) is not supported", d, file))
        return gzip

    def _middleware(self, call: ast.Call, file: str) -> Gzip:
        kind = dotted(call.args[0]) if call.args else None
        if kind not in {"GZipMiddleware", "fastapi.middleware.gzip.GZipMiddleware"}:
            raise TranspileError(f"middleware {kind} is not supported (only GZipMiddleware)", call, file)
        g = Gzip()
        for kw in call.keywords:
            if kw.arg == "minimum_size":
                g.minimum_size = literal(kw.value, file)
                if not 0 <= g.minimum_size <= 65535:
                    raise TranspileError("minimum_size must be between 0 and 65535", kw, file)
            elif kw.arg == "compresslevel":
                g.compresslevel = literal(kw.value, file)
            else:
                raise TranspileError(f"GZipMiddleware option {kw.arg}= is not supported", kw, file)
        return g

    # ------------------------------------------------------------------ routers

    def _discover_routers(self) -> None:
        """Module-level `router = APIRouter(prefix=..., tags=..., dependencies=...)`."""
        for m in self.index.package_modules():
            file = str(m.path)
            for node in m.tree.body:
                if not (isinstance(node, (ast.Assign, ast.AnnAssign)) and isinstance(node.value, ast.Call)):
                    continue
                call = node.value
                if not (dotted(call.func) == "APIRouter"
                        or is_ext(self.index.resolve_expr(m.name, call.func), "fastapi.APIRouter")):
                    continue
                targets = node.targets if isinstance(node, ast.Assign) else [node.target]
                for t in targets:
                    if not isinstance(t, ast.Name):
                        continue
                    r = Router(Sym(m.name, t.id), call, file)
                    for kw in call.keywords:
                        if kw.arg == "prefix":
                            try:
                                r.prefix = literal(kw.value, file)
                            except TranspileError:
                                r.unsupported.append(kw)
                        elif kw.arg == "dependencies":
                            r.deps = kw
                        elif kw.arg in {"tags", "responses", "deprecated", "include_in_schema"}:
                            pass
                        else:
                            r.unsupported.append(kw)
                    self.routers[r.sym] = r

    def _dyn_prefix(self, value: ast.expr, file: str, module: str) -> str:
        for i, (v, _, _) in enumerate(self.dyn_prefixes):
            if v is value:
                return f"\x01{i}\x01"
        self.dyn_prefixes.append((value, file, module))
        return f"\x01{len(self.dyn_prefixes) - 1}\x01"

    def shown_path(self, path: str) -> str:
        """A route path for messages: runtime prefixes shown as `<expression>`."""
        return re.sub("\x01(\\d+)\x01", lambda m: f"<{ast.unparse(self.dyn_prefixes[int(m.group(1))][0])}>", path)

    def _mounts(self) -> dict[Sym, list[Mount]]:
        """Every `include_router` chain from the app to each router, with its full prefix."""
        edges: list[tuple[object, Sym, ast.Call, str, bool, str, bool]] = []  # (owner, child, call, file, cond, module, top)
        for m in self.index.package_modules():
            file = str(m.path)
            parents = self._parents(m.name)
            for node in ast.walk(m.tree):
                if not (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
                        and node.func.attr == "include_router" and node.args):
                    continue
                scope = _enclosing_fn(node, parents)
                child = self.index.resolve_expr(m.name, node.args[0], scope)
                if not isinstance(child, Sym) or child not in self.routers:
                    self._global(TranspileError(
                        f"include_router: cannot resolve `{ast.unparse(node.args[0])}` to an APIRouter",
                        node, file))
                    continue
                owner_node = node.func.value
                if self._is_app(m.name, owner_node):
                    owner = "app"
                else:
                    owner = self.index.resolve_expr(m.name, owner_node, scope)
                    if owner not in self.routers:
                        continue
                edges.append((owner, child, node, file, _under_if(node, parents), m.name, scope is None))
        self.include_edges = edges
        memo: dict[object, list[Mount]] = {"app": [Mount("", deps=list(self.__dict__.get("app_deps", [])))]}

        def mounts_of(target, stack=()) -> list[Mount]:
            if target in memo:
                return memo[target]
            out: list[Mount] = []
            for owner, child, call, file, cond, module, top in edges:
                if child != target or owner in stack:
                    continue
                for base in mounts_of(owner, (*stack, target)):
                    mt = Mount(base.prefix, base.conditional or cond, list(base.deps), list(base.unsupported))
                    if owner in self.routers:
                        mt.prefix += self.routers[owner].prefix
                    for kw in call.keywords:
                        if kw.arg == "prefix":
                            try:
                                mt.prefix += literal(kw.value, file)
                            except TranspileError:
                                if top:
                                    mt.prefix += self._dyn_prefix(kw.value, file, module)
                                else:
                                    mt.unsupported.append((kw, file))
                        elif kw.arg == "dependencies":
                            mt.deps.append((kw, file))
                        elif kw.arg in {"tags", "responses", "deprecated", "include_in_schema"}:
                            pass
                        else:
                            mt.unsupported.append((kw, file))
                    out.append(mt)
            memo[target] = out
            return out

        result = {}
        for sym, r in self.routers.items():
            ms = mounts_of(sym)
            if not ms:
                self.notes.append(f"{r.file}:{r.node.lineno}: router `{sym.name}` is never included in the app")
            result[sym] = ms
        return result

    # ------------------------------------------------------------------ routes

    def _route_decorator(self, fn, module: str, parents) -> tuple[ast.Call, Sym | None] | None:
        """`@app.get(...)` -> (decorator, None); `@router.get(...)` -> (decorator, router)."""
        found = None
        for d in fn.decorator_list:
            if not (isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and d.func.attr in HTTP_METHODS):
                continue
            if self._is_app(module, d.func.value):
                found = (d, None)
                continue
            t = self.index.resolve_expr(module, d.func.value, _enclosing_fn(fn, parents))
            if isinstance(t, Sym) and t in self.routers:
                found = (d, t)
        return found

    def _ws_decorator(self, fn, module: str, parents) -> tuple[ast.Call, Sym | None] | None:
        """`@app.websocket(...)` -> (decorator, None); `@router.websocket(...)` -> (decorator, router)."""
        found = None
        for d in fn.decorator_list:
            if not (isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and d.func.attr == "websocket"):
                continue
            if self._is_app(module, d.func.value):
                found = (d, None)
                continue
            t = self.index.resolve_expr(module, d.func.value, _enclosing_fn(fn, parents))
            if isinstance(t, Sym) and t in self.routers:
                found = (d, t)
        return found


"""Front-end: the discovery passes over a FastAPI + SQLAlchemy + Pydantic package that `dyn.py` builds on.

Finds the SQLAlchemy models, Pydantic schemas and dataclasses, the FastAPI applications, their middlewares,
the routers and their `include_router` chains, and the route decorators. Names are resolved through the
imports of each module (`modules.py`): nothing is imported or executed.

`collect=True` (coverage report) never stops at the first error: app-level blockers are recorded.
"""
from __future__ import annotations

import ast
import copy
import re
from dataclasses import dataclass, field
from pathlib import Path

from .ir import Gzip, TranspileError
from .modules import Ext, ModuleIndex, Sym, is_ext

HTTP_METHODS = {"get", "post", "put", "patch", "delete", "head", "options"}
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
    route_class: ast.AST | None = None  # route_class=R: an APIRoute subclass of the project
    resp_cls: ast.AST | None = None  # default_response_class=X


@dataclass
class Mount:
    """One way a router is reached from the app: accumulated prefix and conditions."""

    prefix: str
    conditional: bool = False
    deps: list[tuple[ast.AST, str]] = field(default_factory=list)  # (node, file)
    unsupported: list[tuple[ast.AST, str]] = field(default_factory=list)
    chain: tuple[tuple[str, int], ...] = ()  # (module, line) of each include_router from the app: registration order
    resp_cls: tuple[ast.AST, str] | None = None  # (default_response_class=, module) of the nearest including router
    conds: tuple[tuple[int, bool], ...] = ()  # (index in Frontend.dyn_conds, branch): `if` tests read at startup



# `--app MODULE[:ATTR]` (set_app_target): the module of the application the binary serves, else every FastAPI()
APP_TARGET: str | None = None


def set_app_target(target: str | None) -> None:
    global APP_TARGET
    APP_TARGET = target or None


def route_path(deco: ast.Call, src: str) -> str:
    """The path of `@router.get("/x")` / `@router.get(path="/x")`; a route decorator without one is refused (FastAPI
    raises TypeError at import: path is required)."""
    if deco.args and not isinstance(deco.args[0], ast.Starred):
        return literal(deco.args[0], src)
    kw = next((k for k in deco.keywords if k.arg == "path"), None)
    if kw is None:
        raise TranspileError(f"`{ast.unparse(deco)[:60]}` has no path: the route decorator's path must be a literal "
                             "string (first argument or path=)", deco, src)
    return literal(kw.value, src)

def _parents(tree: ast.AST) -> dict[ast.AST, ast.AST]:
    out = {}
    for node in ast.walk(tree):
        for child in ast.iter_child_nodes(node):
            out[child] = node
    return out


def _under_if(node: ast.AST, parents: dict, decide=None) -> bool | None:
    """Under an `if`/`try`: True; False when not (or when every enclosing `if` is decided statically, `decide(test)`
    True/False, on the branch that runs); None when the node is in a branch that never runs (dead code)."""
    cur, child = parents.get(node), node
    while cur is not None and not isinstance(cur, ast.Module):
        if isinstance(cur, ast.If) and decide is not None and (truth := decide(cur.test)) is not None:
            if (child in cur.body) != truth:
                return None
        elif isinstance(cur, (ast.If, ast.IfExp, ast.Try)):
            return True
        cur, child = parents.get(cur), cur
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
        self.router_factories: dict[Sym, tuple[Sym, str, ast.AST]] = {}  # build_router -> (its router, local, def)
        # non-literal `include_router(prefix=...)` of module-level calls, read at startup by the dyn backend:
        # the route paths hold the marker `\x01<index>\x01` in their place
        self.dyn_prefixes: list[tuple[ast.expr, str, str]] = []  # (value, file, module)
        self.dyn_conds: list[tuple[ast.expr, str, str]] = []  # (if test around an include_router, file, module)
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
        # a base written `Parent[T]` (a generic Pydantic model) counts as `Parent`
        resolved = {
            sym: [self.index.resolve_expr(sym.module, b.value if isinstance(b, ast.Subscript) else b) for b in node.bases]
            for sym, node in classes.items()
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
                elif any(is_ext(t, "pydantic.BaseModel", "pydantic_settings.BaseSettings", "pydantic.RootModel") or t in schemas
                         for t in targets):
                    schemas.add(sym)
                    changed = True
        # @dataclass classes (plain ones: options are checked by the dyn backend)
        self.dataclass_syms = {
            s: n for s, n in classes.items() if s not in models and s not in schemas and any(
                is_ext(self.index.resolve_expr(s.module, d.func if isinstance(d, ast.Call) else d), "dataclasses.dataclass",
                       "pydantic.dataclasses.dataclass")
                for d in n.decorator_list)
        }
        # `class P(NamedTuple)`: compiled like a dataclass, its instances tuples (the dyn backend checks the rest)
        self.namedtuple_syms = {
            s: n for s, n in classes.items() if s not in models and s not in schemas and s not in self.dataclass_syms
            and any(is_ext(self.index.resolve_expr(s.module, b), "typing.NamedTuple", "typing_extensions.NamedTuple")
                    for b in n.bases)
        }
        self.dataclass_syms.update(self.namedtuple_syms)
        self.model_syms = {s: classes[s] for s in classes if s in models}
        self.schema_syms = {s: classes[s] for s in classes if s in schemas}
        self._declarative = bases

    # ------------------------------------------------------------------ app object, middleware

    def _discover_apps(self) -> None:
        """`app = FastAPI(...)`, at module level or inside an app factory function."""
        # an adopted module's own FastAPI() (fixtures/bodycrash imports bodyapp's middleware class, not its app) is
        # another application, not the served one: counted only when the package dir has none (src/ layouts)
        mods = self.index.package_modules()
        own = [m for m in mods if not m.adopted]
        if any("FastAPI" in ast.dump(n.func) for m in own for n in ast.walk(m.tree)
               if isinstance(n, ast.Call) and isinstance(n.func, (ast.Name, ast.Attribute))):
            mods = own
        if APP_TARGET is not None:
            # `--app api.main[:create_app]`: the one application uvicorn serves (a package with several entry
            # points: `main.py`, `publish_main.py`...); the others' registrations are not this binary's
            module, _, attr = APP_TARGET.partition(":")
            chosen = [m for m in mods if m.name == module]
            if not chosen:
                raise TranspileError(f"--app {APP_TARGET}: no module `{module}` in the package (modules are named from "
                                     f"--root, like uvicorn's import string)", None, str(self.package))
            if attr and attr not in chosen[0].defs:
                raise TranspileError(f"--app {APP_TARGET}: `{module}` defines no `{attr}`", None, str(chosen[0].path))
            mods = chosen
            self.app_target_module = module
        for m in mods:
            parents = self._parents(m.name)
            for node in ast.walk(m.tree):
                if isinstance(node, (ast.Assign, ast.AnnAssign)) and isinstance(node.value, ast.Call):
                    sub = self._fastapi_subclass(m.name, node.value.func, _enclosing_fn(node, parents))
                    if sub is not None:
                        # `app = MetricsFastAPI(...)`: an application, served by a class of the project that changes how
                        # FastAPI builds or routes it (its methods run inside Starlette): the whole application in Python
                        self._global(TranspileError(
                            f"the application is an instance of `{sub}`, a subclass of FastAPI of the project: not "
                            "supported (its overrides run inside FastAPI and Starlette)", node, str(m.path)))
                    if sub is not None or dotted(node.value.func) in {"FastAPI", "fastapi.FastAPI"} or is_ext(
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
        if APP_TARGET is not None and not self.app_vars:
            raise TranspileError(f"--app {APP_TARGET}: no `app = FastAPI(...)` in that module", None, str(self.package))

    def _fastapi_subclass(self, module: str, func: ast.AST, scope) -> str | None:
        """`MetricsFastAPI(...)` (or `app_class(...)`, a local bound once to it): the name of a project class deriving
        from fastapi.FastAPI, else None."""
        if isinstance(func, ast.Name) and scope is not None:
            binds = [n for n in ast.walk(scope) if isinstance(n, ast.Assign) and len(n.targets) == 1
                     and isinstance(n.targets[0], ast.Name) and n.targets[0].id == func.id]
            if len(binds) == 1 and isinstance(binds[0].value, (ast.Name, ast.Attribute)):
                func = binds[0].value
        t = self.index.resolve_expr(module, func, scope) if isinstance(func, (ast.Name, ast.Attribute)) else None
        seen = 0
        while isinstance(t, Sym) and seen < 8:
            d = self.index.definition(t)
            if not isinstance(d, ast.ClassDef):
                return None
            for b in d.bases:
                bt = self.index.resolve_expr(t.module, b)
                if is_ext(bt, "fastapi.FastAPI", "starlette.applications.Starlette"):
                    return t.name
            t = self.index.resolve_expr(t.module, d.bases[0]) if len(d.bases) == 1 else None
            seen += 1
        return None

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
                    call = self._router_maker_call(m.name, call)
                    if call is None:
                        continue
                targets = node.targets if isinstance(node, ast.Assign) else [node.target]
                for t in targets:
                    if isinstance(t, ast.Name):
                        self.routers[Sym(m.name, t.id)] = self._router(Sym(m.name, t.id), call, file)
            for fn in m.tree.body:
                if (made := self._router_factory(m.name, fn)) is not None:
                    local, call = made
                    sym = Sym(m.name, f"{fn.name}()")
                    self.routers[sym] = self._router(sym, call, file)
                    self.router_factories[Sym(m.name, fn.name)] = (sym, local, fn)

    def _router_maker_call(self, module: str, call: ast.Call) -> ast.Call | None:
        """`router_v2 = make_router(Impl.V2)`, `make_router` a function of the same module whose body only returns
        `APIRouter(...)`: that call with the parameters replaced by the arguments (names, attributes and
        constants: evaluated once, as the function evaluates them). A non-literal prefix is read at startup."""
        t = self.index.resolve_expr(module, call.func) if isinstance(call.func, (ast.Name, ast.Attribute)) else None
        fn = self.index.definition(t) if isinstance(t, Sym) and t.module == module else None
        if not (isinstance(fn, ast.FunctionDef) and not fn.decorator_list):
            return None
        body = fn.body[1:] if fn.body and isinstance(fn.body[0], ast.Expr) and isinstance(fn.body[0].value, ast.Constant) else fn.body
        if not (len(body) == 1 and isinstance(body[0], ast.Return) and isinstance(body[0].value, ast.Call)):
            return None
        made = body[0].value
        if not (dotted(made.func) == "APIRouter" or is_ext(self.index.resolve_expr(module, made.func), "fastapi.APIRouter")):
            return None
        a = fn.args
        if a.posonlyargs or a.vararg or a.kwarg or a.kwonlyargs:
            return None
        names = [x.arg for x in a.args]
        if len(call.args) > len(names) or any(k.arg not in names for k in call.keywords) or any(
                isinstance(x, ast.Starred) for x in call.args):
            return None
        bound = dict(zip(names, call.args)) | {k.arg: k.value for k in call.keywords}
        defaults = dict(zip(names[len(names) - len(a.defaults):], a.defaults))
        for n in names:
            if n not in bound:
                if n not in defaults:
                    return None
                bound[n] = defaults[n]
        simple = (ast.Name, ast.Attribute, ast.Constant)
        if not all(isinstance(v, simple) and all(isinstance(x, (*simple, ast.Load)) for x in ast.walk(v)) for v in bound.values()):
            return None
        # a parameter rebound or shadowed in the body: not a plain substitution
        if any(isinstance(n, (ast.Lambda, ast.comprehension, ast.NamedExpr)) for n in ast.walk(made)):
            return None

        class Subst(ast.NodeTransformer):
            def visit_Name(self, node):
                if isinstance(node.ctx, ast.Load) and node.id in bound:
                    return ast.copy_location(copy.deepcopy(bound[node.id]), node)
                return node
        out = Subst().visit(copy.deepcopy(made))
        ast.fix_missing_locations(out)
        out._p2a_made = True
        return out

    def _router_factory(self, module: str, fn: ast.AST) -> tuple[str, ast.Call] | None:
        """`def build_router(): r = APIRouter(...); r.include_router(...)...; return r` (gymhero): a function without
        parameters whose body only builds a router from routers: (local name, APIRouter call). Each call makes the
        same router, so it is compiled once (`build_router()` as its symbol)."""
        if not (isinstance(fn, (ast.FunctionDef, ast.AsyncFunctionDef)) and not fn.decorator_list
                and not isinstance(fn, ast.AsyncFunctionDef)):
            return None
        a = fn.args
        if a.posonlyargs or a.args or a.kwonlyargs or a.vararg or a.kwarg:
            return None
        body = fn.body[1:] if fn.body and isinstance(fn.body[0], ast.Expr) and isinstance(fn.body[0].value, ast.Constant) else fn.body
        if len(body) < 2 or not (isinstance(body[0], ast.Assign) and len(body[0].targets) == 1
                                 and isinstance(body[0].targets[0], ast.Name) and isinstance(body[0].value, ast.Call)):
            return None
        local, call = body[0].targets[0].id, body[0].value
        if not (dotted(call.func) == "APIRouter" or is_ext(self.index.resolve_expr(module, call.func), "fastapi.APIRouter")):
            return None
        if not (isinstance(body[-1], ast.Return) and isinstance(body[-1].value, ast.Name) and body[-1].value.id == local):
            return None
        for st in body[1:-1]:
            if not (isinstance(st, ast.Expr) and isinstance(st.value, ast.Call) and isinstance(st.value.func, ast.Attribute)
                    and st.value.func.attr == "include_router" and isinstance(st.value.func.value, ast.Name)
                    and st.value.func.value.id == local and st.value.args
                    and not any(isinstance(n, ast.Name) and n.id == local for x in [*st.value.args, *st.value.keywords]
                                for n in ast.walk(x))):
                return None
        return local, call

    def _router(self, sym: Sym, call: ast.Call, file: str) -> Router:
        """An `APIRouter(...)` call: its prefix and options."""
        r = Router(sym, call, file)
        for kw in call.keywords:
            if kw.arg == "prefix":
                try:
                    r.prefix = literal(kw.value, file)
                except TranspileError:
                    if getattr(call, "_p2a_made", False) and isinstance(kw.value, ast.JoinedStr):
                        # a router made by a function from its arguments (_router_maker_call): read at startup
                        r.prefix = self._dyn_prefix(kw.value, file, sym.module)
                    else:
                        r.unsupported.append(kw)
            elif kw.arg == "dependencies":
                r.deps = kw
            elif kw.arg == "route_class":
                r.route_class = kw.value
            elif kw.arg == "default_response_class":
                r.resp_cls = kw.value
            elif kw.arg in {"tags", "responses", "deprecated", "include_in_schema"}:
                pass
            else:
                r.unsupported.append(kw)
        return r

    def _library_router(self, module: str, arg: ast.expr, scope) -> str | None:
        """`include_router(x)` where `x` comes from a library (a name imported from it, or what one of its
        functions returns): the library's package, else None. fastapi/starlette themselves do not count."""
        node = arg.func if isinstance(arg, ast.Call) else arg
        if not isinstance(node, (ast.Name, ast.Attribute)):
            return None
        t = self.index.resolve_expr(module, node, scope)
        if isinstance(t, Sym):
            # `graphql_app = GraphQLRouter(schema)`: a module variable holding what a library call returns
            d = self.index.definition(t)
            if isinstance(d, (ast.Assign, ast.AnnAssign)) and isinstance(d.value, ast.Call):
                return self._library_router(t.module, d.value, None)
            return None
        if not isinstance(t, Ext) or t.package in {"fastapi", "starlette", "builtins"}:
            return None
        return t.package

    def _dyn_prefix(self, value: ast.expr, file: str, module: str) -> str:
        for i, (v, _, _) in enumerate(self.dyn_prefixes):
            if v is value:
                return f"\x01{i}\x01"
        self.dyn_prefixes.append((value, file, module))
        return f"\x01{len(self.dyn_prefixes) - 1}\x01"

    # calls an `if` read at startup may make (they only read the environment)
    STARTUP_CALLS = {"os.getenv", "os.environ.get", "bool", "str.lower", "isinstance"}

    def _startup_tests(self, module: str, node: ast.AST, parents) -> list[tuple[ast.expr, bool]] | None:
        """The `if` tests around a module-level registration, each with the branch it is in, when every one can be
        read once at startup (names, attributes, comparisons, `os.getenv`): None under a `try`, a conditional
        expression, a loop or a test that calls anything else."""
        out, cur, child = [], parents.get(node), node
        while cur is not None and not isinstance(cur, ast.Module):
            if isinstance(cur, ast.If):
                if (truth := self._static_truth(module, cur.test)) is None:
                    for n in ast.walk(cur.test):
                        if isinstance(n, (ast.NamedExpr, ast.Await, ast.Lambda, ast.Yield, ast.YieldFrom)):
                            return None
                        if isinstance(n, ast.Call):
                            t = self.index.resolve_expr(module, n.func)
                            name = t.dotted if hasattr(t, "dotted") else (dotted(n.func) or "")
                            if name not in self.STARTUP_CALLS and not (isinstance(n.func, ast.Attribute)
                                                                       and n.func.attr in {"lower", "upper", "strip"}):
                                return None
                    out.append((cur.test, child in cur.body))
                elif (child in cur.body) != truth:
                    return []  # dead branch: decided by the caller already
            elif isinstance(cur, (ast.IfExp, ast.Try, ast.For, ast.While, ast.With, ast.AsyncWith, ast.AsyncFor)):
                return None
            cur, child = parents.get(cur), cur
        return out[::-1]

    def _dyn_cond(self, test: ast.expr, file: str, module: str) -> int:
        for i, (v, _, _) in enumerate(self.dyn_conds):
            if v is test:
                return i
        self.dyn_conds.append((test, file, module))
        return len(self.dyn_conds) - 1

    def _static_truth(self, module: str, test: ast.AST) -> bool | None:
        """`if x is not None:` / `if x is None:` / `if x:` where `x` is a module the project imports (an import
        guard the index decided: `try: from app.api import x / except ImportError: x = None`): known statically."""
        neg = False
        if isinstance(test, ast.UnaryOp) and isinstance(test.op, ast.Not):
            test, neg = test.operand, True
        name = None
        if isinstance(test, ast.Name):
            name, truth = test, True
        elif (isinstance(test, ast.Compare) and len(test.ops) == 1 and isinstance(test.ops[0], (ast.Is, ast.IsNot))
              and isinstance(test.comparators[0], ast.Constant) and test.comparators[0].value is None
              and isinstance(test.left, ast.Name)):
            name, truth = test.left, isinstance(test.ops[0], ast.IsNot)
        if name is None:
            return None
        from .modules import ModRef, stmt_bindings

        if isinstance(self.index.resolve(module, name.id, _bound_only=True), ModRef):
            return truth != neg
        m = self.index.module(module)
        st = m.defs.get(name.id) if m is not None else None
        # `x = None` bound once at module level (the fallback of an import guard that fails): falsy
        if (isinstance(st, ast.Assign) and isinstance(st.value, ast.Constant) and st.value.value is None
                and sum(name.id in stmt_bindings(t) for t in m.tree.body) == 1
                and not any(isinstance(n, ast.Global) and name.id in n.names for n in ast.walk(m.tree))):
            return (not truth) != neg
        return None

    def _configure_site(self, module: str, owner: ast.AST, scope) -> tuple[str, ast.Call, bool | None] | None:
        """`def add_routers(app): app.include_router(...)`, a top-level function of the project called exactly once,
        with the application for that parameter: (module, call, condition of the call), else None (refused by
        dyn.check_configure)."""
        if not (isinstance(owner, ast.Name) and isinstance(scope, (ast.FunctionDef, ast.AsyncFunctionDef))):
            return None
        m = self.index.module(module)
        if m is None or m.defs.get(scope.name) is not scope:
            return None
        params = [a.arg for a in [*scope.args.posonlyargs, *scope.args.args]]
        if owner.id not in params or any(isinstance(n, ast.Name) and n.id == owner.id and isinstance(n.ctx, ast.Store)
                                         for n in ast.walk(scope)):
            return None
        pos = params.index(owner.id)
        sites = []
        for mm in self.index.package_modules():
            mparents = self._parents(mm.name)
            for c in ast.walk(mm.tree):
                if not isinstance(c, ast.Call):
                    continue
                if c.func is scope or self.index.resolve_expr(mm.name, c.func, _enclosing_fn(c, mparents)) != Sym(module, scope.name):
                    continue
                arg = c.args[pos] if pos < len(c.args) else next((k.value for k in c.keywords if k.arg == owner.id), None)
                if arg is None or not self._is_app(mm.name, arg) or any(isinstance(a, ast.Starred) for a in c.args):
                    return None  # called otherwise too: which app is not known
                sites.append((mm.name, c, _under_if(c, mparents, lambda t, mn=mm.name: self._static_truth(mn, t))))
        return sites[0] if len(sites) == 1 else None

    def _configure_prefix(self, value: ast.expr, fn, call: ast.Call) -> ast.expr | None:
        """The `prefix=` of an include_router in a function given the app: the function's parameters replaced by
        the literal the call passes (or their literal default), an f-string of literals folded; the expression
        itself when it reads only module globals (evaluated at startup); None when it reads anything else local."""
        a = fn.args
        params = [*a.posonlyargs, *a.args]
        defaults = dict(zip([p.arg for p in params][len(params) - len(a.defaults):], a.defaults))
        defaults.update({p.arg: d for p, d in zip(a.kwonlyargs, a.kw_defaults) if d is not None})
        given = {p.arg: v for p, v in zip(params, call.args)}
        given.update({k.arg: k.value for k in call.keywords if k.arg})
        local = {n.id for n in ast.walk(fn) if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Store)}
        local |= {p.arg for p in [*params, *a.kwonlyargs]} | {x.arg for x in (a.vararg, a.kwarg) if x}
        names = {n.id for n in ast.walk(value) if isinstance(n, ast.Name)}
        if not names & local:
            return value
        subst = {}
        for n in names & local:
            v = given.get(n, defaults.get(n))
            if not (isinstance(v, ast.Constant) and isinstance(v.value, str)) or n not in defaults.keys() | given.keys():
                return None
            subst[n] = v.value
        if isinstance(value, ast.Name):
            return ast.copy_location(ast.Constant(subst[value.id]), value)
        if isinstance(value, ast.JoinedStr):
            out = []
            for part in value.values:
                if isinstance(part, ast.Constant):
                    out.append(part.value)
                elif (isinstance(part, ast.FormattedValue) and part.conversion == -1 and part.format_spec is None
                      and isinstance(part.value, ast.Name) and part.value.id in subst):
                    out.append(subst[part.value.id])
                else:
                    return None
            return ast.copy_location(ast.Constant("".join(out)), value)
        return None

    def _loop_items(self, module: str, node: ast.Call, parents, scope) -> list[tuple[str, ast.expr]] | None:
        """`for r in ROUTERS: app.include_router(r)`, ROUTERS a literal tuple (or a list never changed) of the
        module, or a literal in the loop: its elements, each with the module it is resolved in. Else None."""
        arg, st = node.args[0], parents.get(node)
        loop = parents.get(st)
        if not (isinstance(arg, ast.Name) and isinstance(st, ast.Expr) and isinstance(loop, ast.For)
                and st in loop.body and isinstance(loop.target, ast.Name) and loop.target.id == arg.id and not loop.orelse):
            return None
        if any(isinstance(n, (ast.Break, ast.Continue)) for b in loop.body for n in ast.walk(b)):
            return None
        it = loop.iter
        if isinstance(it, (ast.Tuple, ast.List)):
            return None if any(isinstance(e, ast.Starred) for e in it.elts) else [(module, e) for e in it.elts]
        if isinstance(it, ast.Attribute) and isinstance(it.value, ast.Name):
            # `for r in views.ROUTERS:`: a module of the project, imported, read as an attribute
            t = self.index.resolve_expr(module, it, scope)
            if not isinstance(t, Sym):
                return None
        elif not isinstance(it, ast.Name) or (scope is not None and any(
                isinstance(n, ast.Name) and n.id == it.id and isinstance(n.ctx, ast.Store) for n in ast.walk(scope))):
            return None
        else:
            t = self.index.resolve(module, it.id)
        dm = self.index.module(t.module) if isinstance(t, Sym) else None
        d = dm.defs.get(t.name) if dm is not None else None
        if not (isinstance(d, ast.Assign) and len(d.targets) == 1 and isinstance(d.value, (ast.Tuple, ast.List))
                and not any(isinstance(e, ast.Starred) for e in d.value.elts)
                and len(dm.binders.get(t.name, [d])) <= 1):
            return None
        if isinstance(d.value, ast.List) and any(
                (isinstance(n, ast.Attribute) and isinstance(n.value, ast.Name) and n.value.id == t.name)
                or (isinstance(n, (ast.AugAssign, ast.Subscript)) and isinstance(getattr(n, "target", getattr(n, "value", None)), ast.Name)
                    and getattr(n, "target", getattr(n, "value", None)).id == t.name)
                for n in ast.walk(dm.tree)):
            return None  # `ROUTERS.append(...)`: changed after its definition
        for m in self.index.package_modules():
            for n in ast.walk(m.tree):
                if not (isinstance(n, ast.Attribute) and n.attr == t.name and m.name != t.module):
                    continue
                if isinstance(n.ctx, ast.Store):
                    return None  # rebound from another module (`views.ROUTERS = ...`)
                up = self._parents(m.name).get(n)
                if isinstance(d.value, ast.List) and (isinstance(up, ast.Attribute) or (
                        isinstance(up, ast.Subscript) and isinstance(up.ctx, (ast.Store, ast.Del)))
                        or isinstance(up, ast.AugAssign)):
                    return None  # `views.ROUTERS.append(...)` / `views.ROUTERS[0] = ...` elsewhere
        return [(t.module, e) for e in d.value.elts]

    def shown_path(self, path: str) -> str:
        """A route path for messages: runtime prefixes shown as `<expression>`."""
        return re.sub("\x01(\\d+)\x01", lambda m: f"<{ast.unparse(self.dyn_prefixes[int(m.group(1))][0])}>", path)

    def _mounts(self) -> dict[Sym, list[Mount]]:
        """Every `include_router` chain from the app to each router, with its full prefix."""
        # (owner, child, call, file, cond, module, top, (module, line...) where it registers)
        edges: list[tuple[object, Sym, ast.Call, str, bool, str, bool, tuple]] = []
        self.include_conds: dict[int, tuple[tuple[int, bool], ...]] = {}  # include_router call -> startup tests
        self.include_prefix: dict[int, str] = {}  # include_router call -> its literal prefix (folded)
        self.configure_includes: set[int] = set()  # include_router calls run by a function or a loop (not compiled)
        for m in self.index.package_modules():
            file = str(m.path)
            parents = self._parents(m.name)
            for node in ast.walk(m.tree):
                if not (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
                        and node.func.attr == "include_router" and node.args):
                    continue
                cond = _under_if(node, parents, lambda t, mn=m.name: self._static_truth(mn, t))
                if cond is None:
                    continue  # `if dianjin is not None:` after a failed import guard: never runs
                scope = _enclosing_fn(node, parents)
                lib_include = self._library_router(m.name, node.args[0], scope) is not None
                if (cond and scope is None and not lib_include
                        and (tests := self._startup_tests(m.name, node, parents)) is not None):
                    # `if settings.ENABLE_X:` at module level: read once at startup, the routes served or not
                    self.include_conds[id(node)] = tuple(
                        (self._dyn_cond(t, file, m.name), branch) for t, branch in tests)
                    cond = False
                owner_node = node.func.value
                key = (m.name, (node.lineno,))
                # (a parameter named like the app variable shadows it: `def add_routers(app: FastAPI)`)
                site = self._configure_site(m.name, owner_node, scope)
                if site is not None:
                    # `def add_routers(app): app.include_router(...)` called once with the app: registered where called
                    smod, scall, scond = site
                    if scond is None:
                        continue
                    cond = cond or scond
                    key = (smod, (scall.lineno, node.lineno))
                    self.configure_includes.add(id(node))
                items = self._loop_items(m.name, node, parents, scope)
                param = isinstance(owner_node, ast.Name) and isinstance(scope, (ast.FunctionDef, ast.AsyncFunctionDef)) and \
                    owner_node.id in [a.arg for a in [*scope.args.posonlyargs, *scope.args.args, *scope.args.kwonlyargs]]
                if items is not None and (site is not None or not param):
                    self.configure_includes.add(id(node))  # else refused by dyn.check_configure (which app is unknown)
                targets = [(m.name, node.args[0], scope, ())] if items is None else [
                    (imod, e, None, (k,)) for k, (imod, e) in enumerate(items)]
                for amod, arg, ascope, sub in targets:
                    child = self.index.resolve_expr(amod, arg, ascope)
                    if isinstance(arg, ast.Call) and not arg.args and not arg.keywords:
                        made = self.router_factories.get(self.index.resolve_expr(amod, arg.func, ascope))
                        if made is not None:
                            child = made[0]  # `include_router(build_router())`
                    lib = self._library_router(amod, arg, ascope)
                    if lib is not None:
                        # dyn.prepare leaves it to Python when it is among the app's last registrations
                        where = "the app" if self._is_app(m.name, owner_node) or site else f"`{ast.unparse(owner_node)}`"
                        self._global(TranspileError(
                            f"include_router: `{ast.unparse(arg)}` is a router of the library `{lib}` (included in "
                            f"{where}): its routes are not translated",
                            node, file))
                        continue
                    if not isinstance(child, Sym) or child not in self.routers:
                        self._global(TranspileError(
                            f"include_router: cannot resolve `{ast.unparse(arg)}` to an APIRouter",
                            node, file))
                        continue
                    factory = next((sym for sym, local, fn in self.router_factories.values()
                                    if fn is scope and isinstance(owner_node, ast.Name) and owner_node.id == local), None)
                    if factory is not None:
                        owner = factory  # `r.include_router(...)` inside `def build_router(): r = APIRouter()`
                    elif site is not None or self._is_app(m.name, owner_node):
                        owner = "app"
                    else:
                        owner = self.index.resolve_expr(m.name, owner_node, scope)
                        if owner not in self.routers:
                            continue
                    top = scope is None or (("configure", scope, site[1]) if site is not None else False)
                    edges.append((owner, child, node, file, cond, m.name, top, (key[0], key[1] + sub)))
        self.include_edges = edges
        memo: dict[object, list[Mount]] = {"app": [Mount("", deps=list(self.__dict__.get("app_deps", [])))]}

        def mounts_of(target, stack=()) -> list[Mount]:
            if target in memo:
                return memo[target]
            out: list[Mount] = []
            for owner, child, call, file, cond, module, top, key in edges:
                if child != target or owner in stack:
                    continue
                for base in mounts_of(owner, (*stack, target)):
                    mt = Mount(base.prefix, base.conditional or cond, list(base.deps), list(base.unsupported),
                               (*base.chain, key),
                               (self.routers[owner].resp_cls, self.routers[owner].sym.module)
                               if owner in self.routers and self.routers[owner].resp_cls is not None else base.resp_cls,
                               (*base.conds, *self.include_conds.get(id(call), ())))
                    if owner in self.routers:
                        mt.prefix += self.routers[owner].prefix
                    for kw in call.keywords:
                        if kw.arg == "prefix":
                            value = kw.value
                            if isinstance(top, tuple):
                                # in a function given the app: its parameters as the call passes them
                                value = self._configure_prefix(value, top[1], top[2])
                            try:
                                text = None if value is None else literal(value, file)
                            except TranspileError:
                                text = None
                            if text is not None:
                                mt.prefix += text
                                self.include_prefix[id(call)] = text
                            elif top and value is kw.value:
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

    def _expand(self, d, module: str | None = None) -> list:
        """A route decorator as the per-method decorators it stands for: `@router.api_route(path, methods=[...])`
        (a literal list of HTTP methods, GET when omitted) is one `@router.<method>(path, ...)` per method, made
        once (the same nodes on every call); any other decorator is itself."""
        if not (isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and d.func.attr == "api_route"):
            return [d]
        cache = self.__dict__.setdefault("_api_routes", {})
        if id(d) not in cache:
            kw = next((k for k in d.keywords if k.arg == "methods"), None)
            names = None
            value = kw.value if kw is not None else None
            if isinstance(value, ast.Name) and module is not None:
                value = self._module_list(module, value)
            if kw is None:
                names = ["get"]
            elif isinstance(value, (ast.List, ast.Tuple, ast.Set)) and all(
                    isinstance(e, ast.Constant) and isinstance(e.value, str) for e in value.elts):
                names = list(dict.fromkeys(e.value.lower() for e in value.elts))
            out = []
            if names and all(n in HTTP_METHODS for n in names):
                for n in names:
                    func = ast.copy_location(ast.Attribute(value=d.func.value, attr=n, ctx=ast.Load()), d.func)
                    c = ast.copy_location(ast.Call(func=func, args=d.args, keywords=[k for k in d.keywords if k.arg != "methods"]), d)
                    c.p2a_orig = d
                    c.p2a_methods = names  # one APIRoute with all these methods (the route tree's node)
                    out.append(c)
            cache[id(d)] = (d, out)
        return cache[id(d)][1]

    def _module_list(self, module: str, name: ast.Name):
        """The literal a module-level name is bound to once and never mutated in its module (`METHODS = [...]`)."""
        t = self.index.resolve_expr(module, name)
        if not isinstance(t, Sym):
            return None
        m = self.index.module(t.module)
        d = m.defs.get(t.name) if m is not None else None
        if not (isinstance(d, (ast.Assign, ast.AnnAssign)) and d.value is not None and len(m.binders.get(t.name, [d])) <= 1):
            return None
        for n in ast.walk(m.tree):
            if isinstance(n, ast.Attribute) and isinstance(n.value, ast.Name) and n.value.id == t.name \
                    and n.attr in {"append", "extend", "insert", "remove", "pop", "clear", "add", "discard"}:
                return None
            if isinstance(n, (ast.AugAssign, ast.Subscript)) and isinstance(getattr(n, "target", getattr(n, "value", None)), ast.Name) \
                    and getattr(getattr(n, "target", getattr(n, "value", None)), "id", None) == t.name \
                    and (isinstance(n, ast.AugAssign) or isinstance(n.ctx, (ast.Store, ast.Del))):
                return None
        return d.value

    def _route_decorator(self, fn, module: str, parents) -> tuple[ast.Call, Sym | None] | None:
        """`@app.get(...)` -> (decorator, None); `@router.get(...)` -> (decorator, router)."""
        found = None
        for d in [x for dd in fn.decorator_list for x in self._expand(dd, module)]:
            if not (isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and d.func.attr in HTTP_METHODS):
                continue
            if self._is_app(module, d.func.value):
                found = (d, None)
                continue
            t = self.index.resolve_expr(module, d.func.value, _enclosing_fn(fn, parents))
            if isinstance(t, Sym) and t in self.routers:
                found = (d, t)
        return found

    def _route_decorators(self, fn, module: str, parents) -> list[tuple[ast.Call, Sym | None]]:
        """Every HTTP route decorator of the function, in registration order (the lowest first: Python applies
        decorators bottom up, each registers the function once more)."""
        out = []
        for d in [x for dd in reversed(fn.decorator_list) for x in self._expand(dd, module)]:
            if not (isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and d.func.attr in HTTP_METHODS):
                continue
            if self._is_app(module, d.func.value):
                out.append((d, None))
                continue
            t = self.index.resolve_expr(module, d.func.value, _enclosing_fn(fn, parents))
            if isinstance(t, Sym) and t in self.routers:
                out.append((d, t))
        return out

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


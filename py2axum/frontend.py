"""Front-end: read a FastAPI + SQLAlchemy + Pydantic package and build the IR.

Only declarations are interpreted here (models, schemas, constants, routers, route signatures).
Handler bodies are kept as AST and translated by `body.py`. Names are resolved through the
imports of each module (`modules.py`): nothing is imported or executed.

`collect=True` (coverage report) never stops at the first error: a model/schema that fails is
*poisoned* and the routes using it are blocked; route errors and app-level blockers are recorded.
"""
from __future__ import annotations

import ast
import re
from dataclasses import dataclass, field
from pathlib import Path

from .ir import (
    MISSING,
    App,
    BlockedBy,
    Column,
    Const,
    Gzip,
    OrmModel,
    Param,
    Route,
    Schema,
    SchemaField,
    TranspileError,
    TypeRef,
)
from .modules import ModuleIndex, Sym, is_ext

SCALARS = {"int", "str", "bool", "float"}
HTTP_METHODS = {"get", "post", "put", "patch", "delete"}
NUM_CONSTRAINTS = {"ge", "gt", "le", "lt"}
STR_CONSTRAINTS = {"min_length", "max_length"}
# route decorator options with no runtime effect (documentation only)
DOC_OPTIONS = {"tags", "summary", "description", "responses", "deprecated", "operation_id",
               "include_in_schema", "name", "response_description"}
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
        raise TranspileError("expected a literal value", node, file) from None


@dataclass
class Src:
    """Where a declaration lives: file for messages, module (+ function) for name resolution."""

    file: str
    module: str
    scope: ast.AST | None = None


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
        self.poisoned: dict[Sym, TranspileError] = {}
        self.route_errors: list[tuple[dict, TranspileError]] = []  # (route info, error)
        self.global_errors: list[TranspileError] = []
        self.notes: list[str] = []
        self.route_infos: list[tuple[dict, Route]] = []  # every translated-so-far route with its info
        # symbol tables
        self.model_syms: dict[Sym, ast.ClassDef] = {}
        self.schema_syms: dict[Sym, ast.ClassDef] = {}
        self.keys: dict[Sym, str] = {}  # model/schema/const -> Rust name
        self.app_vars: dict[str, set[str]] = {}  # module -> names bound to FastAPI()
        self.routers: dict[Sym, Router] = {}
        # non-literal `include_router(prefix=...)` of module-level calls, read at startup by the dyn backend:
        # the route paths hold the marker `\x01<index>\x01` in their place
        self.dyn_prefixes: list[tuple[ast.expr, str, str]] = []  # (value, file, module)
        self._schemas_done: dict[Sym, Schema] = {}
        self._parents_cache: dict[str, dict] = {}

    # ------------------------------------------------------------------ entry

    def run(self) -> App:
        for path, e in self.index.syntax_errors:
            self._global(TranspileError(f"syntax error: {e.msg}", None, f"{path}:{e.lineno}"))
        self._load_imports()
        self._classify()
        models: dict[str, OrmModel] = {}
        schemas: dict[str, Schema] = {}
        consts: dict[str, Const] = {}
        for sym, node in self.model_syms.items():
            try:
                m = self._model(node, self._src(sym.module))
                models[m.name] = m
            except TranspileError as e:
                self._poison(sym, e)
        for sym in self.schema_syms:
            try:
                s = self._schema(sym)
                schemas[s.name] = s
            except TranspileError as e:
                self._poison(sym, e)
        for m in self.index.package_modules():
            src = self._src(m.name)
            for node in m.tree.body:
                if isinstance(node, (ast.Assign, ast.AnnAssign)):
                    c = self._const(node, src)
                    if c:
                        targets = node.targets if isinstance(node, ast.Assign) else [node.target]
                        key = self._unique(c.name, consts)
                        self.keys[Sym(m.name, targets[0].id)] = key
                        c.name = key
                        consts[key] = c
        self._discover_apps()
        gzip = self._middlewares()
        self._discover_routers()
        mounts = self._mounts()
        routes = self._routes(mounts)
        if not routes and not self.collect:
            raise TranspileError("no FastAPI routes found (expected @app.get/post/... or @router.get/... decorators)")
        return App(models, schemas, consts, routes, gzip)

    def _fail(self, e: TranspileError) -> None:
        if not self.collect:
            raise e

    def _global(self, e: TranspileError) -> None:
        self._fail(e)
        self.global_errors.append(e)

    def _poison(self, sym: Sym, e: TranspileError) -> None:
        self._fail(e)
        self.poisoned[sym] = e

    def _src(self, module: str, scope=None) -> Src:
        return Src(str(self.index.module(module).path), module, scope)

    def _parents(self, module: str) -> dict:
        if module not in self._parents_cache:
            self._parents_cache[module] = _parents(self.index.module(module).tree)
        return self._parents_cache[module]

    @staticmethod
    def _unique(name: str, taken) -> str:
        if name not in taken:
            return name
        i = 2
        while f"{name}_{i}" in taken:
            i += 1
        return f"{name}_{i}"

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
        for group in (self.model_syms, self.schema_syms):
            taken: set[str] = set()
            dup = {s.name for s in group if sum(o.name == s.name for o in group) > 1}
            for s in group:
                key = s.name if s.name not in dup else f"{s.name}_{s.module.replace('.', '_')}"
                key = self._unique(key, taken)
                taken.add(key)
                self.keys[s] = key

    def _type_sym(self, node: ast.AST, src: Src):
        t = self.index.resolve_expr(src.module, node, src.scope)
        return t if isinstance(t, Sym) else None

    # ------------------------------------------------------------------ ORM models

    def _model(self, node: ast.ClassDef, src: Src) -> OrmModel:
        file = src.file
        for b in node.bases:
            t = self.index.resolve_expr(src.module, b)
            if t not in self._declarative:
                raise TranspileError(
                    f"model {node.name}: base class {ast.unparse(b)} is not supported (inheritance/mixins)", b, file
                )
        table = None
        cols: list[Column] = []
        for stmt in node.body:
            if isinstance(stmt, ast.Assign) and any(
                isinstance(t, ast.Name) and t.id == "__tablename__" for t in stmt.targets
            ):
                table = literal(stmt.value, file)
            elif isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
                cols.append(self._column(stmt, src))
            elif isinstance(stmt, (ast.Pass, ast.Expr)):
                continue
            else:
                raise TranspileError(f"unsupported statement in model {node.name}", stmt, file)
        if not table:
            raise TranspileError(f"model {node.name} has no __tablename__", node, file)
        model = OrmModel(self.keys[Sym(src.module, node.name)], table, cols)
        try:
            _ = model.pk  # validates the primary key
        except TranspileError as e:
            raise TranspileError(e.msg, node, file) from None
        return model

    def _column(self, stmt: ast.AnnAssign, src: Src) -> Column:
        file = src.file
        name = stmt.target.id
        ann = stmt.annotation
        if not (isinstance(ann, ast.Subscript) and dotted(ann.value) == "Mapped"):
            raise TranspileError(f"column {name}: annotate with Mapped[...]", stmt, file)
        t = self._annotation(ann.slice, src)
        base = t.base()
        if base.kind not in SCALARS:
            raise TranspileError(f"column {name}: type {base.kind} not supported (int/str/bool/float)", stmt, file)
        col = Column(name=name, py_type=base.kind, nullable=t.is_optional)
        if stmt.value is not None:
            call = stmt.value
            if not (isinstance(call, ast.Call) and dotted(call.func) == "mapped_column"):
                raise TranspileError(f"column {name}: expected mapped_column(...)", stmt, file)
            for arg in call.args:
                d = dotted(arg.func) if isinstance(arg, ast.Call) else dotted(arg)
                if d == "String":
                    col.length = literal(arg.args[0], file) if isinstance(arg, ast.Call) and arg.args else None
                elif d == "BigInteger":
                    col.big = True
                elif d in {"Integer", "Boolean", "Float", "Text"}:
                    pass
                else:
                    raise TranspileError(f"column {name}: unsupported column type {d}", arg, file)
            for kw in call.keywords:
                if kw.arg == "primary_key":
                    col.primary_key = literal(kw.value, file)
                elif kw.arg == "unique":
                    col.unique = literal(kw.value, file)
                elif kw.arg == "nullable":
                    col.nullable = literal(kw.value, file)
                elif kw.arg == "default":
                    col.default = literal(kw.value, file)
                elif kw.arg == "index":
                    pass
                else:
                    raise TranspileError(f"column {name}: unsupported option {kw.arg}=", kw, file)
        if col.primary_key:
            col.nullable = False
        return col

    # ------------------------------------------------------------------ Pydantic schemas

    def _schema(self, sym: Sym, _stack=()) -> Schema:
        if sym in self._schemas_done:
            return self._schemas_done[sym]
        if sym in self.poisoned:
            raise BlockedBy(f"schema {sym.name}", self.poisoned[sym])
        node = self.schema_syms[sym]
        src = self._src(sym.module)
        file = src.file
        fields: list[SchemaField] = []
        for b in node.bases:
            parent = self._type_sym(b, src)
            if parent in self.schema_syms:
                if parent in _stack:
                    raise TranspileError(f"schema {node.name}: inheritance cycle", b, file)
                try:
                    fields.extend(self._schema(parent, (*_stack, sym)).fields)
                except TranspileError as e:
                    raise e if isinstance(e, BlockedBy) else BlockedBy(f"schema {parent.name}", e, b, file)
            elif not is_ext(self.index.resolve_expr(sym.module, b), "pydantic.BaseModel"):
                raise TranspileError(f"schema {node.name}: base class {ast.unparse(b)} is not supported", b, file)
        for stmt in node.body:
            if isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
                fields = [f for f in fields if f.name != stmt.target.id]
                fields.append(self._field(stmt, src))
            elif isinstance(stmt, ast.Assign) and any(
                isinstance(t, ast.Name) and t.id == "model_config" for t in stmt.targets
            ):
                continue  # ConfigDict(from_attributes=True) is implied by the generated From impls
            elif isinstance(stmt, (ast.Pass, ast.Expr)):
                continue
            else:
                raise TranspileError(f"unsupported statement in schema {node.name}", stmt, file)
        s = Schema(self.keys[sym], fields)
        self._schemas_done[sym] = s
        return s

    def _field(self, stmt: ast.AnnAssign, src: Src) -> SchemaField:
        file = src.file
        name = stmt.target.id
        f = SchemaField(name, self._annotation(stmt.annotation, src))
        if stmt.value is not None:
            v = stmt.value
            if isinstance(v, ast.Call) and dotted(v.func) in {"Field", "pydantic.Field"}:
                self._constraints(v, f.constraints, file)
                f.default = f.constraints.pop("default", MISSING)
                if v.args:
                    f.default = literal(v.args[0], file)
                if f.default is Ellipsis:  # Field(...) / Field(default=...) = required
                    f.default = MISSING
            else:
                f.default = literal(v, file)
        self._check_constraints(f.typ, f.constraints, stmt, file)
        return f

    def _constraints(self, call: ast.Call, out: dict, file: str) -> None:
        for kw in call.keywords:
            if kw.arg in NUM_CONSTRAINTS | STR_CONSTRAINTS | {"default"}:
                out[kw.arg] = literal(kw.value, file)
            elif kw.arg in {"description", "title", "examples"}:
                pass
            else:
                raise TranspileError(f"unsupported Field/Query option {kw.arg}=", kw, file)

    @staticmethod
    def _check_constraints(t: TypeRef, cons: dict, node, file) -> None:
        base = t.base().kind
        for k in cons:
            if k in STR_CONSTRAINTS and base != "str":
                raise TranspileError(f"{k} only applies to str fields", node, file)
            if k in NUM_CONSTRAINTS and base not in {"int", "float"}:
                raise TranspileError(f"{k} only applies to numeric fields", node, file)

    # ------------------------------------------------------------------ annotations

    def _annotation(self, node: ast.AST, src: Src) -> TypeRef:
        file = src.file
        if isinstance(node, ast.Constant) and node.value is None:
            return TypeRef("none")
        d = dotted(node)
        if d in SCALARS:
            return TypeRef(d)
        if d is not None:
            sym = self._type_sym(node, src)
            if sym in self.schema_syms:
                if sym in self.poisoned:
                    raise BlockedBy(f"schema {sym.name}", self.poisoned[sym], node, file)
                return TypeRef("schema", name=self.keys[sym])
        if d in {"dict", "Any", "typing.Any"}:
            return TypeRef("json")
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr):
            left, right = self._annotation(node.left, src), self._annotation(node.right, src)
            if right.kind == "none" and left.kind != "none":
                return TypeRef("optional", inner=left)
            if left.kind == "none" and right.kind != "none":
                return TypeRef("optional", inner=right)
            raise TranspileError("only `X | None` unions are supported", node, file)
        if isinstance(node, ast.Subscript):
            outer = dotted(node.value)
            if outer in {"Optional", "typing.Optional"}:
                return TypeRef("optional", inner=self._annotation(node.slice, src))
            if outer in {"list", "List", "typing.List"}:
                return TypeRef("list", inner=self._annotation(node.slice, src))
        raise TranspileError(f"unsupported type annotation `{ast.unparse(node)}`", node, file)

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

    def _middlewares(self) -> Gzip | None:
        gzip = None
        for m in self.index.package_modules():
            file = str(m.path)
            for node in ast.walk(m.tree):
                if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
                    owner, attr = node.func.value, node.func.attr
                    if attr == "add_middleware" and self._is_app(m.name, owner):
                        try:
                            gzip = self._middleware(node, file)
                        except TranspileError as e:
                            self._global(e)
                    elif attr in {"add_exception_handler", "mount", "add_route", "add_api_route",
                                  "add_websocket_route"} and (
                        self._is_app(m.name, owner)
                        or (isinstance(owner, ast.Attribute) and self._is_app(m.name, owner.value))
                    ):
                        self._global(TranspileError(f"app.{attr}(...) is not supported", node, file))
                elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    for d in node.decorator_list:
                        if (
                            isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute)
                            and self._is_app(m.name, d.func.value)
                            and d.func.attr in {"exception_handler", "middleware", "on_event", "websocket"}
                        ):
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

    # ------------------------------------------------------------------ constants

    def _const(self, node: ast.Assign | ast.AnnAssign, src: Src) -> Const | None:
        targets = node.targets if isinstance(node, ast.Assign) else [node.target]
        if len(targets) != 1 or not isinstance(targets[0], ast.Name):
            return None
        name = targets[0].id
        if not name.isupper():
            return None  # only UPPER_CASE module constants are carried over
        v = node.value
        if isinstance(v, ast.Constant) and isinstance(v.value, str):
            return Const(name, None, v.value)
        if (
            isinstance(v, ast.Call)
            and dotted(v.func) in {"os.environ.get", "os.getenv"}
            and len(v.args) == 2
        ):
            try:
                return Const(name, literal(v.args[0], src.file), str(literal(v.args[1], src.file)))
            except TranspileError:
                return None
        return None

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
        memo: dict[object, list[Mount]] = {"app": [Mount("")]}

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

    def _routes(self, mounts: dict[Sym, list[Mount]]) -> list[Route]:
        routes: list[Route] = []
        taken: set[str] = set()
        for m in self.index.package_modules():
            file = str(m.path)
            parents = self._parents(m.name)
            for fn in ast.walk(m.tree):
                if not isinstance(fn, (ast.AsyncFunctionDef, ast.FunctionDef)):
                    continue
                found = self._route_decorator(fn, m.name, parents)
                if found is None:
                    continue
                deco, router = found
                ms = mounts.get(router, []) if router else [Mount("")]
                for mt in ms:
                    info = {"method": deco.func.attr, "path": None, "func": fn.name, "file": file,
                            "line": fn.lineno, "conditional": mt.conditional, "node": fn,
                            "module": m.name, "router": router}
                    try:
                        r = self._route(fn, deco, Src(file, m.name, fn), router, mt)
                        info["path"] = r.path
                        self.route_infos.append((info, r))
                        r.rust_name = self._unique(r.func, taken)
                        taken.add(r.rust_name)
                        routes.append(r)
                    except TranspileError as e:
                        if not self.collect:
                            raise
                        try:
                            info["path"] = mt.prefix + (self.routers[router].prefix if router else "") + literal(
                                deco.args[0], file)
                        except (TranspileError, IndexError):
                            info["path"] = "?"
                        self.route_errors.append((info, e))
        return routes

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

    def _route(self, fn, deco: ast.Call, src: Src, router: Sym | None, mount: Mount) -> Route:
        file = src.file
        if isinstance(fn, ast.FunctionDef):
            raise TranspileError(f"route {fn.name}: only `async def` handlers are supported", fn, file)
        method = deco.func.attr
        if not deco.args:
            raise TranspileError("route decorator needs a path", deco, file)
        r = self.routers.get(router) if router else None
        if r is not None:
            if r.deps is not None:
                raise TranspileError("router-level dependencies (APIRouter(dependencies=[...])) are not supported",
                                     r.deps, r.file)
            if r.unsupported:
                raise TranspileError(f"unsupported APIRouter option {r.unsupported[0].arg}=", r.unsupported[0], r.file)
        if mount.deps:
            node, f = mount.deps[0]
            raise TranspileError("include_router(dependencies=[...]) is not supported", node, f)
        if mount.unsupported:
            node, f = mount.unsupported[0]
            raise TranspileError(f"unsupported include_router option {getattr(node, 'arg', '?')}=", node, f)
        if "\x01" in mount.prefix:
            value, f, _ = self.dyn_prefixes[int(mount.prefix.split("\x01")[1])]
            raise TranspileError("include_router(prefix=...) from a runtime value (settings, env) is only supported "
                                 "by the dyn backend (--backend dyn)", value, f)
        path = mount.prefix + (r.prefix if r else "") + literal(deco.args[0], file)
        response_model = None
        status_code = 200
        for kw in deco.keywords:
            if kw.arg == "response_model":
                response_model = self._annotation(kw.value, src)
            elif kw.arg == "status_code":
                if isinstance(kw.value, ast.Attribute) and kw.value.attr.startswith("HTTP_"):
                    raise TranspileError(f"status_code={dotted(kw.value)}: named status constants are not supported",
                                         kw, file)
                status_code = literal(kw.value, file)
            elif kw.arg in DOC_OPTIONS:
                pass
            else:
                raise TranspileError(f"unsupported route option {kw.arg}=", kw, file)
        path_names = set(re.findall(r"{(\w+)}", path))
        params = self._params(fn, path_names, src)
        if sum(p.source == "body" for p in params) > 1:
            raise TranspileError("several body parameters (embedded bodies) are not supported", fn, file)
        missing = path_names - {p.name for p in params if p.source == "path"}
        if missing:
            raise TranspileError(f"path parameters without a function argument: {missing}", fn, file)
        names = self._body_names(fn, src)
        return Route(method, path, fn.name, params, response_model, status_code, fn.body, file, fn,
                     module=src.module, names=names)

    def _body_names(self, fn, src: Src) -> dict[str, str]:
        """Identifiers of the body that denote a model, schema or constant -> their key."""
        names: dict[str, str] = {}
        for node in ast.walk(ast.Module(body=fn.body, type_ignores=[])):
            if isinstance(node, ast.Name) and node.id not in names:
                t = self.index.resolve(src.module, node.id, fn)
                if isinstance(t, Sym):
                    if t in self.poisoned:
                        kind = "model" if t in self.model_syms else "schema"
                        raise BlockedBy(f"{kind} {t.name}", self.poisoned[t], node, src.file)
                    if t in self.keys:
                        names[node.id] = self.keys[t]
        return names

    def _params(self, fn: ast.AsyncFunctionDef, path_names: set[str], src: Src) -> list[Param]:
        args = fn.args
        if args.vararg or args.kwarg or args.posonlyargs:
            raise TranspileError("*args/**kwargs are not supported in handlers", fn, src.file)
        all_args = args.args + args.kwonlyargs
        defaults: list = [MISSING] * (len(args.args) - len(args.defaults)) + list(args.defaults)
        defaults += list(args.kw_defaults)
        params = []
        for arg, default in zip(all_args, defaults):
            if default is None:
                default = MISSING
            params.append(self._param(arg, default, path_names, src))
        return params

    def _param(self, arg: ast.arg, default, path_names, src: Src) -> Param:
        file = src.file
        name = arg.arg
        ann = arg.annotation
        # Dependencies are recognised by their annotated type, not by the provider function.
        if isinstance(default, ast.Call) and dotted(default.func) == "Depends":
            kind = dotted(ann) if ann is not None else None
            dep = {"AsyncSession": "session", "aiohttp.ClientSession": "http", "ClientSession": "http"}.get(kind)
            if dep is None:
                raise TranspileError(
                    f"dependency `{name}`: only AsyncSession and aiohttp.ClientSession are supported", arg, file
                )
            return Param(name, "dep", dep=dep)
        if ann is None:
            raise TranspileError(f"parameter `{name}` needs a type annotation", arg, file)
        typ = self._annotation(ann, src)
        p = Param(name, "query", typ)
        if isinstance(default, ast.Call) and dotted(default.func) in {"Query", "Path", "Body"}:
            self._constraints(default, p.constraints, file)
            p.default = p.constraints.pop("default", MISSING)
            if default.args:
                d0 = default.args[0]
                if not (isinstance(d0, ast.Constant) and d0.value is Ellipsis):
                    p.default = literal(d0, file)
            if dotted(default.func) == "Body":
                p.source = "body"
        elif default is not MISSING:
            p.default = literal(default, file)
        self._check_constraints(typ, p.constraints, arg, file)
        if name in path_names:
            p.source = "path"
        elif typ.base().kind == "schema":
            p.source = "body"
        if p.source in {"path", "query"} and typ.base().kind not in SCALARS:
            raise TranspileError(f"{p.source} parameter `{name}` must be int/str/bool/float", arg, file)
        return p

"""The compiler: a FastAPI project to Rust code operating on dynamic Python values.

Real projects (three layers, services, dynamic attribute access...) are compiled as written:
every project function reachable from the routes becomes an `async fn(cx, V...) -> R<V>`, Python
semantics come from `runtime/dynrt` (values, Pydantic, SQLAlchemy session, FastAPI plumbing), and
library calls go through the closed list of `libmap.py`. No Python interpreter is involved.

Unsupported constructs raise `TranspileError` with file:line, as everywhere else.
"""
from __future__ import annotations

import ast
import types
import copy
import functools
import json
import re
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

from . import coreschema, libmap
from .frontend import HTTP_METHODS, Frontend, Mount, dotted, literal, route_path
from .frontend import _enclosing_fn as frontend_enclosing_fn
from .ir import TranspileError
from .modules import Ext, ModRef, Sym, import_guard, is_ext, startup_skipped, stmt_bindings

RT = "dynrt"
RUNTIME_DIR = Path(__file__).parent / "runtime" / "dynrt"

BUILTIN_FUNCS = {
    "len", "str", "repr", "int", "float", "bool", "list", "tuple", "set", "frozenset", "dict", "sorted",
    "reversed", "min", "max", "sum", "any", "all", "next", "round", "abs", "enumerate", "zip", "range",
    "print", "isinstance", "getattr", "setattr", "hasattr", "iter", "open", "type", "chr", "ord", "divmod", "callable",
    "hash", "id", "filter", "map", "issubclass", "vars", "anext", "format",
}
BUILTIN_TYPES = {"str", "int", "float", "bool", "dict", "list", "tuple", "set", "frozenset", "bytes", "object"}
# builtin types the runtime never makes a value of: usable where only their identity matters (`isinstance(x,
# (bytes, bytearray))` is false for a bytearray the binary never has); calling them is refused
TYPE_ONLY = {"bytearray", "memoryview", "complex"}


_RS_SPECIAL = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\ud800-\udfff]")


# the logging methods that make a record (their call site is passed along: dynrt/logging.rs)
LOG_METHODS = frozenset({"debug", "info", "warning", "warn", "error", "exception", "critical", "fatal", "log"})
# loguru's `logger.<level>()` methods (dynrt/loguru.rs)
LOGURU_LEVEL_METHODS = frozenset({"trace", "debug", "info", "success", "warning", "error", "critical"})


class LoneSurrogate(TranspileError):
    """A string with a lone surrogate (`re.compile('[\\ud800-\\udfff]')`): no Rust string holds one. Raised without
    a location; the expression being compiled gives it one."""


def rs(s: str) -> str:
    """A Rust string literal (JSON's escapes are Rust's, but for control characters: `\\u{1c}`, not `\\u001c`)."""
    if not _RS_SPECIAL.search(s):
        return json.dumps(s, ensure_ascii=False)
    out = []
    for ch in s:
        o = ord(ch)
        if 0xD800 <= o <= 0xDFFF:
            raise LoneSurrogate(f"unsupported constant: a lone surrogate (\\u{o:04x}) cannot be held in a Rust string: "
                                f"{s[:40]!r}")
        out.append(json.dumps(ch, ensure_ascii=False)[1:-1] if o >= 0x20 or ch in "\n\r\t" else f"\\u{{{o:x}}}")
    return '"' + "".join(out) + '"'


def rs_marked(s: str) -> str:
    """A Rust string literal that may hold runtime prefix markers (`\\x01<index>\\x01`)."""
    return rs(s).replace("\\u0001", "\\u{1}")


# a property's setter in a class's method table: entry `set:<name>`, function `Class.<name>.__set__`
SETTER_PREFIX = "set:"
RO_PREFIX = "ro:"
SETTER_SUFFIX = ".__set__"


def property_accessor(fn) -> str | None:
    """"setter"/"deleter" for `@<fn.name>.setter` / `.deleter` (the only decorator forms of a property's accessors)."""
    for d in fn.decorator_list:
        if isinstance(d, ast.Attribute) and d.attr in ("setter", "deleter") and isinstance(d.value, ast.Name) \
                and d.value.id == fn.name:
            return d.attr
    return None


def ident(s: str) -> str:
    return re.sub(r"[^0-9A-Za-z_]", "_", s)


def mod_ident(module: str) -> str:
    return ident(module.replace(".", "_"))


@dataclass
class ParamSpec:
    name: str
    kind: str  # positional | vararg | kwonly | kwarg
    default: ast.AST | None


def fn_params(node: ast.FunctionDef | ast.AsyncFunctionDef | ast.Lambda) -> list[ParamSpec]:
    a = node.args
    pos = a.posonlyargs + a.args
    defaults = [None] * (len(pos) - len(a.defaults)) + list(a.defaults)
    out = [ParamSpec(p.arg, "positional", d) for p, d in zip(pos, defaults)]
    if a.vararg:
        out.append(ParamSpec(a.vararg.arg, "vararg", None))
    out += [ParamSpec(p.arg, "kwonly", d) for p, d in zip(a.kwonlyargs, a.kw_defaults)]
    if a.kwarg:
        out.append(ParamSpec(a.kwarg.arg, "kwarg", None))
    return out


def _session_call(node, meth: str):
    """`await s.<meth>(q)` or `s.<meth>(q)` (sync Session), `s` a name: the call."""
    if isinstance(node, ast.Await):
        node = node.value
    if (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == meth
            and isinstance(node.func.value, ast.Name) and len(node.args) == 1 and not node.keywords
            and not isinstance(node.args[0], ast.Starred)):
        return node
    return None


def list_return(stmts: list, i: int):
    """A list returned straight from the session, which a route may stream (orm::defer_list):
    `return (await s.execute(q)).scalars().all()`, `return (await s.scalars(q)).all()`, or
    `r = await s.execute(q)` then `return r.scalars().all()`. (the session's call, statements used)."""
    def all_of(e):
        if (isinstance(e, ast.Call) and isinstance(e.func, ast.Attribute) and e.func.attr == "all"
                and not e.args and not e.keywords):
            return e.func.value
        return None

    def scalars_of(e):
        if (isinstance(e, ast.Call) and isinstance(e.func, ast.Attribute) and e.func.attr == "scalars"
                and not e.args and not e.keywords):
            return e.func.value
        return None

    node = stmts[i]
    if isinstance(node, ast.Return) and (x := all_of(node.value)) is not None:
        if (y := scalars_of(x)) is not None and (c := _session_call(y, "execute")):
            return c, 1
        if c := _session_call(x, "scalars"):
            return c, 1
    if (isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name)
            and (c := _session_call(node.value, "execute")) and i + 1 < len(stmts)):
        nxt = stmts[i + 1]
        if (isinstance(nxt, ast.Return) and (x := all_of(nxt.value)) is not None and (y := scalars_of(x)) is not None
                and isinstance(y, ast.Name) and y.id == node.targets[0].id and c.func.value.id != y.id):
            return c, 2
    return None


def has_list_return(fn) -> bool:
    for n in ast.walk(fn):
        for body in (getattr(n, f, None) for f in ("body", "orelse", "finalbody")):
            if isinstance(body, list) and any(isinstance(b, ast.stmt) for b in body):
                if any(list_return(body, i) for i in range(len(body))):
                    return True
    return False


def has_yield(node: ast.AST) -> bool:
    for n in walk_scope(node):
        if isinstance(n, (ast.Yield, ast.YieldFrom)):
            return True
    return False


TELEMETRY_SWITCHES = {"tracing", "metrics", "logs", "operation_spans", "auto_configure"}


def shown_name(name: str) -> str:
    """the name a class shows (repr, errors): a SQLModel table's Pydantic view is the table class
    (py2axum/sqlmodel_rewrite.py)"""
    for suffix in ("_p2a_sm_view", "_p2a_sm_raw"):
        if name.endswith(suffix):
            return name[:-len(suffix)]
    return name


def method_descriptor(ty: str, attr: str) -> bool:
    """`ty.attr` is a method of a builtin type (not a classmethod like dict.fromkeys, nor a dunder)"""
    import builtins
    import inspect
    import types
    if is_dunder(attr) or attr.startswith("_"):
        return False
    t = getattr(builtins, ty, None)
    d = inspect.getattr_static(t, attr, None) if isinstance(t, type) else None
    return isinstance(d, types.MethodDescriptorType)


def walk_scope(node: ast.AST):
    """ast.walk without entering nested functions, lambdas or classes."""
    todo = list(ast.iter_child_nodes(node))
    while todo:
        n = todo.pop()
        yield n
        if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda, ast.ClassDef)):
            continue
        todo.extend(ast.iter_child_nodes(n))


def target_names(t: ast.AST, out: set[str]) -> None:
    if isinstance(t, ast.Name):
        out.add(t.id)
    elif isinstance(t, (ast.Tuple, ast.List)):
        for e in t.elts:
            target_names(e, out)
    elif isinstance(t, ast.Starred):
        target_names(t.value, out)


def assigned_names(body: list[ast.stmt]) -> set[str]:
    out: set[str] = set()
    for stmt in body:
        # a nested def binds its name; what its body assigns is its own (walrus in defaults/decorators aside)
        nodes = [stmt] if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)) else [stmt, *walk_scope(stmt)]
        for n in nodes:
            if isinstance(n, (ast.Assign,)):
                for t in n.targets:
                    target_names(t, out)
            elif isinstance(n, (ast.AugAssign, ast.AnnAssign)) and n.value is not None or isinstance(n, ast.AugAssign):
                target_names(n.target, out)
            elif isinstance(n, (ast.For, ast.AsyncFor)):
                target_names(n.target, out)
            elif isinstance(n, ast.ExceptHandler) and n.name:
                out.add(n.name)
            elif isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n is not stmt or (
                isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n is stmt
            ):
                out.add(n.name)
            elif isinstance(n, ast.NamedExpr):
                target_names(n.target, out)
            elif isinstance(n, (ast.MatchAs, ast.MatchStar)) and n.name:
                out.add(n.name)
            elif isinstance(n, (ast.With, ast.AsyncWith)):
                for it in n.items:
                    if it.optional_vars is not None:
                        target_names(it.optional_vars, out)
            elif isinstance(n, ast.Delete):
                for t in n.targets:
                    target_names(t, out)
    return out


# ====================================================================== classes and descriptors


class _Used(list):
    """Positional arguments handed to a libmap template, recording which ones it reads."""

    def __init__(self, items):
        super().__init__(items)
        self.read: set[int] = set()
        self.all_read = False

    def __getitem__(self, i):
        if isinstance(i, slice):
            self.read.update(range(len(self))[i])
        else:
            self.read.add(i % len(self) if len(self) else i)
        return super().__getitem__(i)

    def __iter__(self):
        self.all_read = True
        return super().__iter__()

    def __len__(self):
        return super().__len__()


class _UsedKw(dict):
    """Keyword arguments handed to a libmap template, recording which ones it reads."""

    def __init__(self, items):
        super().__init__(items)
        self.read: set[str] = set()

    def __getitem__(self, k):
        self.read.add(k)
        return super().__getitem__(k)

    def get(self, k, default=None):
        self.read.add(k)
        return super().get(k, default)

    def __contains__(self, k):
        self.read.add(k)
        return super().__contains__(k)

    def items(self):
        self.read.update(self.keys())
        return super().items()

    def values(self):
        self.read.update(self.keys())
        return super().values()

    def __iter__(self):
        self.read.update(self.keys())
        return super().__iter__()


@dataclass
class ModelInfo:
    sym: Sym
    rust: str  # MODEL_x
    cls: str  # CLS_x
    table: str
    cols: list[dict]
    pk: int
    fk_tables: list[str]
    pks: list[int] = field(default_factory=list)
    methods: list[tuple[str, bool, Sym]] = field(default_factory=list)
    rels_raw: list[tuple] = field(default_factory=list)  # (name, Mapped[...] or None, relationship call, module)
    rels: list[dict] | None = None  # resolved by Project.resolve_rels (needs the target models)
    eager_defaults: bool = False  # __mapper_args__ = {"eager_defaults": True}
    # single-table inheritance: the mapped class it extends, the discriminator column (name) of the hierarchy,
    # this class's polymorphic_identity
    parent: Sym | None = None
    poly_on: str | None = None
    identity: str | None = None
    poly_load: bool = False  # polymorphic_load="selectin" / "inline"
    rels_decl: list[tuple] = field(default_factory=list)  # every relationship() declared, kept for subclasses
    # joined table inheritance: (subclass table, its key column, parent table, parent key column) from the root down
    joins: list[tuple] = field(default_factory=list)


@dataclass
class SchemaInfo:
    sym: Sym
    rust: str
    cls: str
    fields: list[dict]
    from_attributes: bool
    extra: str
    settings: bool = False
    env_prefix: str = ""
    case_sensitive: bool = False
    env_none: str | None = None  # env_parse_none_str
    validators: list[tuple[list[str], Sym]] = field(default_factory=list)
    methods: list[tuple[str, bool, Sym]] = field(default_factory=list)


class Project:
    """Everything the generated `gen.rs` needs, built on demand from the routes."""

    def __init__(self, fe: Frontend, collect: bool = False):
        self.fe = fe
        self.ix = fe.index
        self.collect = collect
        self.errors: list[TranspileError] = []
        self.items: list[str] = []  # Rust items (statics, fns)
        self.models: dict[Sym, ModelInfo] = {}
        self.schemas: dict[Sym, SchemaInfo] = {}
        self.exc_classes: dict[Sym, str] = {}
        self.fns: dict[tuple, str] = {}  # (Sym, variant) -> rust name
        self.fn_queue: list[tuple] = []
        self.globals: dict[Sym, str] = {}
        self.tds: dict[str, str] = {}  # rust init -> static name
        self.pats: dict[str, str] = {}
        self.dflts: dict[str, str] = {}
        self.deps: dict[tuple[Sym, str], str] = {}  # (dependency, scope) -> solver
        self.counter = 0
        self.cur: object = None  # node being compiled (route id, dep sym or fn key): call-graph source
        self.edges: dict[object, set] = {}
        self.fn_errors: dict[object, TranspileError] = {}
        self.class_errors: dict[Sym, TranspileError] = {}
        self.enums: dict[Sym, tuple[str, str, list[str]]] = {}  # sym -> (ENUM_x, CLS_x, member names)
        self.enum_cols: dict[str, str] = {}
        self.expire_on_commit = self._expire_on_commit()

    # ---------------------------------------------------------------- utils

    TRACE_CALLS = {"sys.settrace", "threading.settrace", "threading.settrace_all_threads"}

    def traced(self) -> bool:
        """The project installs a trace function (`sys.settrace`): its functions report their events."""
        if self.__dict__.get("_traced") is None:
            self.__dict__["_traced"] = any(
                isinstance(n, ast.Call) and is_ext(self.ix.resolve_expr(m.name, n.func), *self.TRACE_CALLS)
                for m in list(self.ix.modules.values()) for n in ast.walk(m.tree))
        return self.__dict__["_traced"]

    def uses_sentry(self) -> bool:
        """The project imports sentry_sdk: its `except` blocks record the exception being handled
        (`sys.exc_info()` of `capture_exception()` and `logger.exception`)."""
        if self.__dict__.get("_sentry") is None:
            self.__dict__["_sentry"] = any(
                (isinstance(n, ast.Import) and any(a.name.split(".")[0] == "sentry_sdk" for a in n.names))
                or (isinstance(n, ast.ImportFrom) and (n.module or "").split(".")[0] == "sentry_sdk")
                for m in list(self.ix.modules.values()) for n in ast.walk(m.tree))
        return self.__dict__["_sentry"]

    def tb_index(self, module: str, func: str, node) -> int:
        """The index in `TB` of a traceback entry: the file, the instruction's line, the function, and the text
        CPython prints under the `File` line (py2axum/tbfmt.py)."""
        key = (module, func, node.lineno, node.end_lineno, node.col_offset, node.end_col_offset)
        tbl = self.__dict__.setdefault("tb_table", {})
        i = tbl.get(key)
        if i is None:
            from . import tbfmt
            text = tbfmt.frame_text(self.src(module), node.lineno, node.end_lineno, node.col_offset, node.end_col_offset)
            i = tbl[key] = len(tbl)
            self.__dict__.setdefault("tb_rows", []).append(
                f"    {RT}::trace::Pos {{ file: {rs(self.rel_file(module))}, line: {node.lineno}, func: {rs(func)}, text: {rs(text)} }},")
        return i

    def tracks_handling(self) -> bool:
        """The project imports sentry_sdk, logging, sys or traceback: its `except` blocks record the exception
        being handled (`sys.exc_info()`, `logger.exception`, `exc_info=True`, `capture_exception()`)."""
        if self.__dict__.get("_handling") is None:
            roots = {"sentry_sdk", "logging", "sys", "traceback"}
            self.__dict__["_handling"] = any(
                (isinstance(n, ast.Import) and any(a.name.split(".")[0] in roots for a in n.names))
                or (isinstance(n, ast.ImportFrom) and (n.module or "").split(".")[0] in roots)
                for m in list(self.ix.modules.values()) for n in ast.walk(m.tree))
        return self.__dict__["_handling"]

    def rel_file(self, module: str) -> str:
        """A module's path relative to --root (what the binary reports as its file)."""
        path = Path(self.src(module)).resolve()
        try:
            return str(path.relative_to(self.ix.root))
        except ValueError:
            return str(path)

    def rebound_globals(self) -> set[Sym]:
        """Module variables some module-level function rebinds with `global`."""
        if self.__dict__.get("_rebound") is None:
            out = set()
            for m in list(self.ix.modules.values()):
                for d in m.tree.body:
                    if isinstance(d, (ast.FunctionDef, ast.AsyncFunctionDef)):
                        for n in walk_scope(d):
                            if isinstance(n, ast.Global):
                                out |= {Sym(m.name, x) for x in n.names}
                # `conf.api_key = v` anywhere: an attribute of a project (or translated library) module, rebinding
                # one of its variables (a module-level assignment, not a function or a class)
                for n in ast.walk(m.tree):
                    if isinstance(n, ast.Attribute) and isinstance(n.ctx, ast.Store) and isinstance(n.value, (ast.Name, ast.Attribute)):
                        base = self.ix.resolve_expr(m.name, n.value)
                        if isinstance(base, ModRef) and (tm := self.ix.module(base.module)) is not None:
                            d = tm.defs.get(n.attr)
                            if isinstance(d, (ast.Assign, ast.AnnAssign)):
                                out.add(Sym(base.module, n.attr))
            self.__dict__["_rebound"] = out
        return self.__dict__["_rebound"]

    # library calls whose translation reads the enclosing function's state: not usable as values
    LIB_FN_NOT_VALUES: set = set()

    def async_method_names(self) -> set[str]:
        """Names of the `async def` methods of the project's classes (a call not awaited may be a coroutine)."""
        if self.__dict__.get("_async_names") is None:
            names = set()
            for m in list(self.ix.modules.values()):
                for n in ast.walk(m.tree):
                    if isinstance(n, ast.ClassDef):
                        names |= {st.name for st in n.body if isinstance(st, ast.AsyncFunctionDef) and not has_yield(st)}
            self._async_names = names
        return self._async_names

    def crypt_context_names(self) -> tuple[set, set[str]]:
        """Where a passlib CryptContext is bound in the project: module globals (`pwd_context = CryptContext(...)`,
        as Syms) and attribute names (`self.context = CryptContext(...)`). Their attributes and calls are checked
        against the translated surface (FnCompiler.check_passlib)."""
        if self.__dict__.get("_crypt_names") is None:
            globs, attrs = set(), set()
            for m in list(self.ix.modules.values()):
                if m.lib is not None:
                    continue
                for n in ast.walk(m.tree):
                    if isinstance(n, (ast.Assign, ast.AnnAssign)) and isinstance(n.value, ast.Call):
                        if not isinstance(self.resolve(m.name, n.value.func), Ext) or \
                                self.resolve(m.name, n.value.func).dotted != "passlib.context.CryptContext":
                            continue
                        for t in (n.targets if isinstance(n, ast.Assign) else [n.target]):
                            if isinstance(t, ast.Name) and n in m.tree.body:
                                globs.add(Sym(m.name, t.id))
                            elif isinstance(t, ast.Attribute):
                                attrs.add(t.attr)
            self._crypt_names = (globs, attrs)
        return self._crypt_names

    def httpx_exc_names(self) -> set[str]:
        """Names bound by `except httpx.X as name` in the project: `name.request` is httpx's Request, of which
        only `method` and `url` are translated (FnCompiler.check_httpx_request)."""
        if self.__dict__.get("_httpx_exc") is None:
            names = set()
            for m in list(self.ix.modules.values()):
                if m.lib is not None:
                    continue
                for n in ast.walk(m.tree):
                    if isinstance(n, ast.ExceptHandler) and n.name and n.type is not None and "httpx" in ast.unparse(n.type):
                        names.add(n.name)
            self._httpx_exc = names
        return self._httpx_exc

    def cachedmethod_names(self) -> set[str]:
        """Methods of a project class decorated with `@cachedmethod` (by name: their `cache_*` attributes are
        refused)."""
        if self.__dict__.get("_cm_names") is None:
            names = set()
            for m in list(self.ix.modules.values()):
                for n in ast.walk(m.tree):
                    if isinstance(n, ast.ClassDef):
                        for st in n.body:
                            if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)) and any(
                                    (dotted(d.func if isinstance(d, ast.Call) else d) or "").split(".")[-1] == "cachedmethod"
                                    for d in st.decorator_list):
                                names.add(st.name)
            self._cm_names = names
        return self._cm_names

    def project_attrs(self) -> set[str]:
        """Names a project object may carry: methods and class attributes of every class, attributes
        assigned anywhere (`obj.cb = f`), columns and fields included."""
        if self.__dict__.get("_attrs") is None:
            names = set()
            for m in list(self.ix.modules.values()):
                # the library classes py2axum translates from its own source, when the project imports one (or its
                # module: `from sqlalchemy.ext import hybrid`, `import sqlalchemy.ext.hybrid as h`)
                for spec in m.imports.values():
                    name = f"{spec[1]}.{spec[2]}" if spec[0] == "from" else spec[1]
                    for shim in self.ix.SHIMS:
                        if name in (shim, shim.rpartition(".")[0]):
                            self.ix.load_shim(shim)
            for m in list(self.ix.modules.values()):
                for n in ast.walk(m.tree):
                    if isinstance(n, ast.ClassDef):
                        for st in n.body:
                            if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
                                names.add(st.name)
                            elif isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name):
                                names.add(st.target.id)
                            elif isinstance(st, ast.Assign):
                                names |= {t.id for t in st.targets if isinstance(t, ast.Name)}
                    elif isinstance(n, ast.Attribute) and isinstance(n.ctx, ast.Store):
                        names.add(n.attr)
                    if isinstance(n, ast.ClassDef):
                        names.add(n.name)  # a row's entity by class name (`row.User`)
                    elif isinstance(n, ast.Call) and getattr(n.func, "attr", getattr(n.func, "id", None)) == "relationship":
                        # `relationship(backref="x")` / `backref("x")`: the attribute it adds to the target class
                        for k in n.keywords:
                            b = k.value.args[0] if (k.arg == "backref" and isinstance(k.value, ast.Call)
                                                    and k.value.args) else k.value
                            if k.arg == "backref" and isinstance(b, ast.Constant) and isinstance(b.value, str):
                                names.add(b.value)
                    elif isinstance(n, ast.Call) and getattr(n.func, "attr", getattr(n.func, "id", None)) == "aliased":
                        # `aliased(User, name="u1")`: the key of its entity in a result row (`row.u1`)
                        names |= {k.value.value for k in n.keywords if k.arg == "name" and isinstance(k.value, ast.Constant)
                                  and isinstance(k.value.value, str)}
                    elif isinstance(n, ast.Call) and getattr(n.func, "attr", getattr(n.func, "id", None)) in ATTR_NAMING_CALLS:
                        # attributes named by a string or a keyword: `.label("total")`, `setattr(o, "x", v)`,
                        # `namedtuple("P", "a b")`, `SimpleNamespace(x=1)`, `getattr(o, "x")`
                        names |= {k.arg for k in n.keywords if k.arg}
                        for a in n.args:
                            if isinstance(a, ast.Constant) and isinstance(a.value, str):
                                names |= set(re.findall(r"[A-Za-z_][A-Za-z0-9_]*", a.value))
                            elif isinstance(a, (ast.List, ast.Tuple)):
                                names |= {e.value for e in a.elts if isinstance(e, ast.Constant) and isinstance(e.value, str)}
                    elif (isinstance(n, ast.Call) and len(n.args) == 1 and isinstance(n.args[0], ast.Constant)
                          and isinstance(n.args[0].value, str)
                          and (isinstance(n.func, ast.Name) and n.func.id == "import_module"
                               or isinstance(n.func, ast.Attribute) and n.func.attr == "import_module")):
                        # a module object's top-level names (importlib.import_module("pkg.mod").name)
                        mod = self.ix.module(n.args[0].value)
                        if mod is not None:
                            names |= {*mod.defs, *mod.imports}
                    elif (isinstance(n, ast.Subscript) and isinstance(n.ctx, ast.Store) and isinstance(n.slice, ast.Constant)
                          and isinstance(n.slice.value, str) and self.scope_state(n.value)):
                        # a raw ASGI middleware's `scope["state"]["rid"] = v` (or `scope.setdefault("state", {})[...]`):
                        # Starlette's `request.state.rid`
                        names.add(n.slice.value)
            self.__dict__["_attrs"] = names
        return self.__dict__["_attrs"]

    def json_schemas(self) -> tuple[dict, dict]:
        """`.model_json_schema()` call sites: ({model sym: its JSON schema text}, {call site: TranspileError}).
        A site's receiver is a model class by name, or `expr.attr` where every value the project binds to an
        attribute or keyword `attr` is one (the models then get their schema); anything else is refused."""
        if self.__dict__.get("_json_schemas") is None:
            from .jsonschema import SchemaGen

            mods = list(self.ix.modules.items())
            sites = [(name, n) for name, m in mods for n in ast.walk(m.tree)
                     if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute) and (
                         n.func.attr == "model_json_schema"
                         # pydantic v1's `Model.schema()` (deprecated): model_json_schema() on a model named here
                         or n.func.attr == "schema" and not n.args and not n.keywords
                         and isinstance(n.func.value, (ast.Name, ast.Attribute))
                         and self.resolve(name, n.func.value) in self.fe.schema_syms)]
            done, errors, bad_syms = {}, {}, {}

            def schema_of(t: Sym):
                if t not in done and t not in bad_syms:
                    try:
                        done[t] = json.dumps(SchemaGen(self).model(t), ensure_ascii=False, separators=(",", ":"))
                    except TranspileError as e:
                        bad_syms[t] = e
                return bad_syms.get(t)

            def bound_to(attr: str) -> list:
                out = []
                for name, m in mods:
                    for n in ast.walk(m.tree):
                        if isinstance(n, ast.Call):
                            out += [(name, k.value) for k in n.keywords if k.arg == attr]
                        elif isinstance(n, ast.Assign):
                            out += [(name, n.value) for t in n.targets if isinstance(t, ast.Attribute) and t.attr == attr]
                        elif isinstance(n, ast.ClassDef):
                            out += [(name, st.value) for st in n.body if isinstance(st, ast.AnnAssign)
                                    and isinstance(st.target, ast.Name) and st.target.id == attr and st.value is not None]
                return out

            for module, call in sites:
                recv = call.func.value
                key = (module, call.lineno, call.col_offset)
                src = self.src(module)
                t = self.resolve(module, recv) if isinstance(recv, (ast.Name, ast.Attribute)) else None
                if isinstance(t, Sym) and t in self.fe.schema_syms:
                    vals = [(module, recv)]
                elif isinstance(recv, ast.Attribute):
                    vals = bound_to(recv.attr)
                else:
                    errors[key] = TranspileError(".model_json_schema() on a value that is not a model class or an "
                                                 "attribute holding one is not supported", call, src)
                    continue
                for vm, v in vals:
                    if isinstance(v, ast.Constant) and v.value is None:
                        continue
                    t = self.resolve(vm, v) if isinstance(v, (ast.Name, ast.Attribute)) else None
                    if not (isinstance(t, Sym) and t in self.fe.schema_syms):
                        errors[key] = TranspileError(
                            f".model_json_schema() on `.{getattr(recv, 'attr', '')}`, which is also bound to "
                            f"`{ast.unparse(v)}` ({self.src(vm).rsplit('/', 1)[-1]}:{v.lineno}), not a model class",
                            call, src)
                        break
                    e = schema_of(t)
                    if e is not None:
                        errors[key] = TranspileError(f".model_json_schema() of {t.name}: {e.render()}", call, src)
                        break
            self.__dict__["_json_schemas"] = (done, errors)
        return self.__dict__["_json_schemas"]

    @staticmethod
    def scope_state(node) -> bool:
        """`<scope>["state"]` or `<scope>.setdefault("state", ...)`: the dict behind Starlette's `request.state`."""
        if isinstance(node, ast.Subscript):
            return isinstance(node.slice, ast.Constant) and node.slice.value == "state"
        return (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == "setdefault"
                and bool(node.args) and isinstance(node.args[0], ast.Constant) and node.args[0].value == "state")

    def stripe_fields(self) -> frozenset:
        """The field names of stripe's API objects when the project imports stripe: a StripeObject's attributes are
        its keys (`__getattr__`), so these reads may find one."""
        if self.__dict__.get("_stripe_fields") is None:
            uses = any((isinstance(n, ast.Import) and any(a.name.split(".")[0] == "stripe" for a in n.names))
                       or (isinstance(n, ast.ImportFrom) and not n.level and (n.module or "").split(".")[0] == "stripe")
                       for m in list(self.ix.modules.values()) if m.lib is None for n in ast.walk(m.tree))
            from . import stripetab
            self.__dict__["_stripe_fields"] = stripetab.FIELDS if uses else frozenset()
        return self.__dict__["_stripe_fields"]

    def open_attrs(self) -> bool:
        """Does the project give objects attributes that cannot be listed statically (`__getattr__`,
        `setattr`/`__dict__` with a computed name)? Then no attribute read is refused."""
        if self.__dict__.get("_open") is None:
            def computed(n):
                if isinstance(n, ast.FunctionDef | ast.AsyncFunctionDef):
                    return n.name in {"__getattr__", "__getattribute__"}
                if isinstance(n, ast.Call) and isinstance(n.func, ast.Name) and n.func.id == "setattr":
                    return len(n.args) < 2 or not (isinstance(n.args[1], ast.Constant) and isinstance(n.args[1].value, str))
                if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute) and n.func.attr == "update":
                    v = n.func.value
                    return (isinstance(v, ast.Attribute) and v.attr == "__dict__") or (
                        isinstance(v, ast.Call) and isinstance(v.func, ast.Name) and v.func.id == "vars")
                return False
            self.__dict__["_open"] = any(computed(n) for m in list(self.ix.modules.values()) for n in ast.walk(m.tree))
        return self.__dict__["_open"]

    def uid(self) -> int:
        self.counter += 1
        return self.counter

    def method_edges(self, name: str) -> None:
        """`obj.name(...)` on a value of unknown class: the caller may reach every project method of
        that name (their translation errors then block it, in the report as in the build)."""
        if self.cur is None:
            return
        idx = self.__dict__.get("_method_index")
        if idx is None:
            idx = {}
            for m in list(self.ix.modules.values()):
                for cls in m.tree.body:
                    if isinstance(cls, ast.ClassDef):
                        for st in cls.body:
                            if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
                                idx.setdefault(st.name, []).append(Sym(m.name, f"{cls.name}.{st.name}"))
            self.__dict__["_method_index"] = idx
        for sym in idx.get(name, ()):
            self.edges.setdefault(self.cur, set()).add((sym, "plain"))

    def src(self, module: str) -> str:
        return str(self.ix.module(module).path)

    def create_all_ddl(self, holder, node, module: str, fn) -> str:
        """`DDL_x`: the statements of `<holder>.metadata.create_all` (holder: the declarative base or one of its
        mapped classes), one static per base."""
        from . import ddl
        base = None
        if isinstance(holder, Sym):
            b = ddl.DdlBuilder(self, holder, module, fn)
            base = b.root_base(holder)
        if base is None:
            raise self.err(f"create_all: `{ast.unparse(node.value.value)}` is not a declarative base or a mapped class "
                           "of the project", node, module)
        done = self.__dict__.setdefault("ddls", {})
        if base in done:
            return done[base]
        out = ddl.compile_ddl(self, base, node, module, fn)
        name = f"DDL_{mod_ident(base.module)}__{ident(base.name)}"
        done[base] = name
        types = ", ".join(f"({rs(n)}, {rs(sql)})" for n, sql in out["types"])
        tables = ", ".join(f"{RT}::orm::DdlTable {{ name: {rs(n)}, stmts: &[{', '.join(rs(x) for x in stmts)}], "
                           f"alters: &[{', '.join(rs(x) for x in alters)}], "
                           f"create_pre18: {('Some(' + rs(pre) + ')') if pre else 'None'} }}"
                           for n, stmts, alters, pre in out["tables"])
        self.items.append(f"/// {base.name}.metadata.create_all() ({self.rel_file(base.module)}), DDL compiled by SQLAlchemy\n"
                          f"pub static {name}: {RT}::orm::Ddl = {RT}::orm::Ddl {{ types: &[{types}], tables: &[{tables}] }};")
        return name

    def err(self, msg: str, node, module: str) -> TranspileError:
        return TranspileError(msg, node, self.src(module))

    def use_session_dep(self, sym: Sym) -> tuple[bool, bool, bool, bool]:
        """The `AsyncSession` dependency of a route: its shape fixes the session options and whether
        the request commits after the endpoint. The first one seen is the application's default; a route whose
        dependency differs (a sync `Session` next to an `AsyncSession`) makes its session with its own."""
        cfg = self.session_config(sym)
        self.__dict__.setdefault("session_cfgs", {})[sym] = cfg
        if not hasattr(self, "session_cfg"):
            self.session_cfg = cfg
        return cfg

    SESSION_SHAPE = ("only `async with maker() as s: yield s`, optionally inside `try:` with `await s.commit()` "
                     "after the yield, `except ...: await s.rollback(); raise` and `finally: await s.close()`; "
                     "synchronous: the same with `with`, or `s = maker()` then `try: yield s` ... `finally: s.close()`")

    def session_config(self, sym: Sym) -> tuple[bool, bool, bool, bool]:
        """(commit after the endpoint, expire_on_commit, autoflush, sync) of a session dependency."""
        fn = self.ix.definition(sym)
        for _ in range(5):  # `get_async_session = get_db`
            if not (isinstance(fn, ast.Assign) and isinstance(fn.value, (ast.Name, ast.Attribute))):
                break
            t = self.resolve(sym.module, fn.value)
            if not isinstance(t, Sym):
                break
            sym, fn = t, self.ix.definition(t)
        module = sym.module
        bad = lambda node: self.err(f"session dependency {sym.name}: {self.SESSION_SHAPE}", node, module)  # noqa: E731
        if not isinstance(fn, (ast.AsyncFunctionDef, ast.FunctionDef)):
            raise bad(fn)
        sync = isinstance(fn, ast.FunctionDef)
        with_t = ast.With if sync else ast.AsyncWith
        body = [b for b in fn.body if not (isinstance(b, ast.Expr) and isinstance(b.value, ast.Constant))]
        outer_finally = []
        outer_handlers = []
        if (len(body) == 1 and isinstance(body[0], ast.Try) and not body[0].orelse
                and len(body[0].body) == 1 and isinstance(body[0].body[0], with_t)):
            # try: async with maker() as s: yield s / [except: await s.rollback(); raise] / finally: await s.aclose()
            # The handlers and the finally run once the `with` has closed the session: a rollback or a close of a
            # closed session does nothing, so they are checked (rollback then re-raise, cleanups only) and dropped.
            outer_finally = body[0].finalbody
            outer_handlers = body[0].handlers
            body = body[0].body

        def assigned(st):
            """`name = value` / `name: T = value`: (name, value), else None."""
            if isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name):
                return st.targets[0].id, st.value
            if isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name) and st.value is not None:
                return st.target.id, st.value
            return None
        if (len(body) == 1 and isinstance(body[0], ast.Try) and len(body[0].body) >= 2 and assigned(body[0].body[0])
                and isinstance(body[0].body[0].value, ast.Call) and not body[0].body[0].value.args):
            # try: s = maker() / yield s / finally: s.close() -- the maker called inside the try (it does not fail)
            t = body[0]
            body = [t.body[0], ast.copy_location(ast.Try(body=t.body[1:], handlers=t.handlers, orelse=t.orelse,
                                                         finalbody=t.finalbody), t)]
        factory = None
        if (len(body) == 2 and assigned(body[0]) and isinstance(body[1], with_t) and len(body[1].items) == 1
                and isinstance(body[1].items[0].context_expr, ast.Call)
                and isinstance(body[1].items[0].context_expr.func, ast.Name)
                and body[1].items[0].context_expr.func.id == assigned(body[0])[0]):
            # factory = get_session_factory() / async with factory() as s: ... (the maker read once, then called)
            factory, body = assigned(body[0])[1], body[1:]
        must_close = False
        if len(body) == 2 and assigned(body[0]) and isinstance(body[1], ast.Try):
            # s = maker() / try: yield s / finally: [await] s.close()
            sname, maker_call = assigned(body[0])
            inner = [body[1]]
            if not body[1].finalbody:
                raise bad(body[1])
            must_close = True  # no `with` closes it: the finally must
        elif len(body) == 1 and isinstance(body[0], with_t) and len(body[0].items) == 1:
            item = body[0].items[0]
            if not isinstance(item.optional_vars, ast.Name):
                raise bad(item.context_expr)
            maker_call, sname, inner = item.context_expr, item.optional_vars.id, body[0].body
        else:
            raise bad(fn)
        if factory is not None:
            if maker_call.args or maker_call.keywords:
                raise bad(maker_call)
            maker_call = ast.copy_location(ast.Call(func=factory, args=[], keywords=[]), maker_call)
        direct = None
        if isinstance(maker_call, ast.Call) and len(maker_call.args) == 1:
            # `with Session(engine) as session:` (SQLModel's documentation): a session of the engine, default options
            st = self.resolve(module, maker_call.func)
            if is_ext(st, "sqlalchemy.orm.Session" if sync else "sqlalchemy.ext.asyncio.AsyncSession"):
                direct = maker_call
        if direct is None and not (isinstance(maker_call, ast.Call) and not maker_call.args and not maker_call.keywords):
            raise bad(maker_call)

        def is_call(stmt, meth):
            meths = {"close", "aclose"} if meth == "close" else {meth}
            if not isinstance(stmt, ast.Expr):
                return False
            call = stmt.value
            if not sync:
                if not isinstance(call, ast.Await):
                    return False
                call = call.value
            return (isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute) and call.func.attr in meths
                    and isinstance(call.func.value, ast.Name) and call.func.value.id == sname and not call.args)

        def is_cleanup(stmt):
            """A statement of a `finally:` that leaves the request's result unchanged: `[await] s.close()` /
            `s.aclose()`, or `s.expunge_all()` (synchronous on both sessions: it only empties the identity map)."""
            if is_call(stmt, "close"):
                return True
            return (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)
                    and isinstance(stmt.value.func, ast.Attribute) and stmt.value.func.attr == "expunge_all"
                    and isinstance(stmt.value.func.value, ast.Name) and stmt.value.func.value.id == sname
                    and not stmt.value.args and not stmt.value.keywords)

        def is_yield(stmt):
            return (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Yield)
                    and isinstance(stmt.value.value, ast.Name) and stmt.value.value.id == sname)

        def is_reraise(stmt, caught):
            """`raise`, or `raise exc` of the caught exception itself (the traceback FastAPI logs is the endpoint's)."""
            return isinstance(stmt, ast.Raise) and stmt.cause is None and (
                stmt.exc is None or (caught is not None and isinstance(stmt.exc, ast.Name) and stmt.exc.id == caught))

        def rollback_reraise(h):
            return len(h.body) == 2 and is_call(h.body[0], "rollback") and is_reraise(h.body[1], h.name)

        commit = False
        if len(inner) == 1 and is_yield(inner[0]):
            pass
        elif len(inner) == 2 and is_yield(inner[0]) and is_call(inner[1], "commit"):
            # yield s / await s.commit(): an error thrown at the yield skips the commit, the `with` rolls back
            commit = True
        elif len(inner) == 1 and isinstance(inner[0], ast.Try):
            t = inner[0]
            if not (t.body and is_yield(t.body[0]) and all(is_call(x, "commit") for x in t.body[1:]) and len(t.body) <= 2):
                raise bad(t)
            # `else: await s.commit()` commits when the endpoint succeeded, as a commit after the yield does (an
            # error of the commit itself then skips the handler: the `with` rolls back on close, the same outcome)
            if t.orelse and not (len(t.body) == 1 and len(t.orelse) == 1 and is_call(t.orelse[0], "commit")):
                raise bad(t.orelse[0])
            commit = len(t.body) == 2 or bool(t.orelse)
            for h in t.handlers:
                if not rollback_reraise(h):
                    raise bad(h)
            if not all(is_cleanup(x) for x in t.finalbody) or (must_close and not any(is_call(x, "close") for x in t.finalbody)):
                raise bad(t)
        else:
            raise bad(inner[0] if inner else fn)
        for h in outer_handlers:
            if not rollback_reraise(h):
                raise bad(h)
        if not all(is_cleanup(x) for x in outer_finally):
            raise bad(fn)
        if direct is not None:
            expire, autoflush = True, True
            for kw in direct.keywords:
                v = literal(kw.value, self.src(module)) if kw.arg in {"expire_on_commit", "autoflush"} else None
                if kw.arg == "expire_on_commit":
                    expire = bool(v)
                elif kw.arg == "autoflush":
                    autoflush = bool(v)
                else:
                    raise self.err(f"Session({kw.arg}=) is not supported", kw, module)
            return commit, expire, autoflush, sync
        # the sessionmaker: a module constant, or a factory function creating it once (`get_factory()()`)
        makers = {"async_sessionmaker", "sessionmaker"}
        f = maker_call.func
        maker = None
        if isinstance(f, ast.Call) and (dotted(f.func) or "").split(".")[-1] in makers:
            # `maker = sessionmaker(engine, ...)` inside the dependency (a new maker per request, same sessions)
            maker, mk = f, sym
        elif isinstance(f, ast.Call) and not f.args and not f.keywords:
            fs = self.resolve(module, f.func)
            fdef = self.ix.definition(fs) if isinstance(fs, Sym) else None
            if isinstance(fdef, ast.FunctionDef):
                calls = [n for n in ast.walk(fdef) if isinstance(n, ast.Call) and (dotted(n.func) or "").split(".")[-1] in makers]
                if len(calls) == 1:
                    maker, mk = calls[0], fs
        elif isinstance(f, ast.Attribute) and isinstance(f.value, ast.Name) and self.instance_maker(module, f) is not None:
            # `db_manager.session_maker()`: the maker an instance of a project class keeps (instance_maker)
            maker, mk = self.instance_maker(module, f)
        else:
            mk = self.resolve(module, f)
            mdef = self.ix.definition(mk) if isinstance(mk, Sym) else None
            if isinstance(mdef, (ast.Assign, ast.AnnAssign)) and isinstance(mdef.value, ast.Call) \
                    and (dotted(mdef.value.func) or "").split(".")[-1] in makers:
                maker = mdef.value
            elif isinstance(mdef, (ast.Assign, ast.AnnAssign)) and isinstance(mdef.value, ast.Call):
                cls = self.resolve(mk.module, mdef.value.func)
                fdef = self.ix.definition(cls) if isinstance(cls, Sym) else None
                if isinstance(fdef, ast.FunctionDef):
                    # `SessionLocal = make_sessionmaker(engine)`: a project function whose body returns the one
                    # (async_)sessionmaker(...) it calls; its options must be literals (not its parameters)
                    calls = [n for n in ast.walk(fdef) if isinstance(n, ast.Call) and (dotted(n.func) or "").split(".")[-1] in makers]
                    rets = [n for n in ast.walk(fdef) if isinstance(n, ast.Return)]
                    if len(calls) == 1 and len(rets) == 1 and rets[0].value is calls[0]:
                        maker, mk = calls[0], cls
                if isinstance(cls, Sym) and isinstance(self.ix.definition(cls), ast.ClassDef):
                    if sync:
                        raise self.err("session maker class: only async_sessionmaker wrappers are supported", mdef.value, mk.module)
                    if any(k.arg == "connect_args" for k in mdef.value.keywords):
                        # the request's session runs on the process pool as is
                        raise self.err(f"session dependency {sym.name}: connect_args= of the session maker it calls is not "
                                       "supported (the request's session takes no engine options)", mdef.value, mk.module)
                    return (commit, *self.maker_class_options(cls, mdef.value, mk.module), False)
        if maker is None:
            raise self.err(f"session dependency {sym.name}: `{ast.unparse(f)}` must be a module-level "
                           "(async_)sessionmaker(...) or a function creating one", maker_call, module)
        expire, autoflush = True, True
        for kw in maker.keywords:
            v = literal(kw.value, self.src(mk.module)) if kw.arg in {"expire_on_commit", "autoflush"} else None
            if kw.arg == "expire_on_commit":
                expire = bool(v)
            elif kw.arg == "autoflush":
                autoflush = bool(v)
            elif kw.arg == "class_" and (dotted(kw.value) or "").split(".")[-1] == ("Session" if sync else "AsyncSession"):
                pass
            elif kw.arg == "autocommit" and literal(kw.value, self.src(mk.module)) is False:
                pass  # SQLAlchemy 2.x: the only accepted value
            elif kw.arg != "bind":
                raise self.err(f"{(dotted(maker.func) or '').split('.')[-1]}({kw.arg}=) is not supported", kw, mk.module)
        if sync != self._sync_maker(maker):
            raise self.err(f"session dependency {sym.name}: a {'synchronous' if sync else 'coroutine'} dependency "
                           f"needs {'sessionmaker' if sync else 'async_sessionmaker'}(...)", maker, mk.module)
        return commit, expire, autoflush, sync

    def instance_maker(self, module: str, f: ast.Attribute):
        """`inst.attr` where `inst = Cls(...)` is a module global of a project class whose methods set `self.attr`
        once to an (async_)sessionmaker(...) (else only to None): (that call, the class's Sym) when the maker is
        set by a statement of `__init__`'s body, or of a method the application's lifespan calls before its
        `yield` (`await db_manager.initialize()`); None when `inst.attr` is no such thing. Refused (fichier:ligne)
        when it is one but set otherwise: conditionally, twice, or from outside the class."""
        inst = self.resolve(module, f.value)
        idef = self.ix.definition(inst) if isinstance(inst, Sym) else None
        if not (isinstance(idef, (ast.Assign, ast.AnnAssign)) and isinstance(idef.value, ast.Call)):
            return None
        cls = self.resolve(inst.module, idef.value.func)
        cdef = self.ix.definition(cls) if isinstance(cls, Sym) else None
        if not isinstance(cdef, ast.ClassDef):
            return None
        makers = {"async_sessionmaker", "sessionmaker"}
        sets = []  # (method, statement, value) of each `self.<attr> = value`
        for meth in cdef.body:
            if not isinstance(meth, (ast.FunctionDef, ast.AsyncFunctionDef)) or not meth.args.args:
                continue
            me = meth.args.args[0].arg
            for n in ast.walk(meth):
                targets = n.targets if isinstance(n, ast.Assign) else [n.target] if isinstance(n, ast.AnnAssign) and n.value else []
                for tg in targets:
                    if (isinstance(tg, ast.Attribute) and tg.attr == f.attr and isinstance(tg.value, ast.Name)
                            and tg.value.id == me):
                        sets.append((meth, n, n.value))
        made = [(m, st, v) for m, st, v in sets if isinstance(v, ast.Call) and (dotted(v.func) or "").split(".")[-1] in makers]
        if not made:
            return None
        meth, st, maker = made[0]
        bad = lambda why, node, mod: self.err(f"session maker `{ast.unparse(f)}`: {why}", node, mod)  # noqa: E731
        if len(made) > 1 or any(not (isinstance(v, ast.Constant) and v.value is None) for _, _, v in sets if v is not maker):
            raise bad("set more than once in its class (only one (async_)sessionmaker(...), else None)", st, cls.module)
        if st not in meth.body:
            raise bad(f"set conditionally in {cdef.name}.{meth.name} (a statement of the method's body is supported)", st, cls.module)
        for m in list(self.ix.modules.values()):
            for n in ast.walk(m.tree):
                if (isinstance(n, (ast.Assign, ast.AugAssign, ast.AnnAssign))
                        and any(isinstance(tg, ast.Attribute) and tg.attr == f.attr and not (isinstance(tg.value, ast.Name) and tg.value.id == "self")
                                for tg in (n.targets if isinstance(n, ast.Assign) else [n.target]))):
                    raise bad("assigned outside its class", n, m.name)
        if meth.name != "__init__" and not self.lifespan_calls(inst, meth.name):
            raise bad(f"set by {cdef.name}.{meth.name}(), which the application's lifespan does not call before its yield "
                      "(only a maker set in __init__ or there is supported)", st, cls.module)
        return maker, cls

    def lifespan_calls(self, inst: Sym, meth: str) -> bool:
        """The application's lifespan (FastAPI(lifespan=)) calls `inst.meth()` (awaited or not) as a statement of its
        body before its `yield`."""
        # the native lifespan, or the one left to Python (--python-side lifespan): it runs at startup all the same
        spec = self.fe.__dict__.get("lifespan") or next(
            ((m.name, kw.value) for m in self.fe.index.package_modules() for n in ast.walk(m.tree)
             if isinstance(n, ast.Call) and dotted(n.func) in {"FastAPI", "fastapi.FastAPI"}
             for kw in n.keywords if kw.arg == "lifespan"), None)
        if spec is None:
            return False
        lmod, value = spec
        t = self.resolve(lmod, value) if isinstance(value, (ast.Name, ast.Attribute)) else None
        d = self.ix.definition(t) if isinstance(t, Sym) else None
        if not isinstance(d, ast.AsyncFunctionDef):
            return False
        for st in d.body:
            if isinstance(st, ast.Expr) and isinstance(st.value, ast.Yield):
                return False
            call = st.value.value if isinstance(st, ast.Expr) and isinstance(st.value, ast.Await) else \
                st.value if isinstance(st, ast.Expr) else None
            if (isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute) and call.func.attr == meth
                    and isinstance(call.func.value, (ast.Name, ast.Attribute)) and self.resolve(t.module, call.func.value) == inst):
                return True
        return False

    def maker_wrapper(self, sym: Sym, node) -> bool:
        """`X = Wrapper(url=..., connect_args=...)`: a module instance of a project class wrapping
        `async_sessionmaker` (maker_class_options), whatever reads it (a dependency, `get_worker_session()`)."""
        if not (isinstance(node, (ast.Assign, ast.AnnAssign)) and isinstance(node.value, ast.Call)):
            return False
        cls = self.resolve(sym.module, node.value.func)
        if not (isinstance(cls, Sym) and isinstance(self.ix.definition(cls), ast.ClassDef)
                and any(isinstance(n, ast.Name) and n.id == "async_sessionmaker" for n in ast.walk(self.ix.definition(cls)))):
            return False
        cdef = self.ix.definition(cls)
        if not cdef.bases and not any(isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)) and st.name == "__call__"
                                      for st in cdef.body):
            return False  # not callable: a manager keeping a maker in an attribute (instance_maker), not a wrapper
        self.maker_class_options(cls, node.value, sym.module)  # refuses what it cannot read
        return True

    def maker_wrapper_value(self, fc: "FnCompiler", sym: Sym, node) -> str:
        """The wrapper instance as the binary's own sessionmaker (`X()` is a new AsyncSession on the process pool):
        its engine's `connect_args` session parameters are set in each transaction of its sessions."""
        cls = self.resolve(sym.module, node.value.func)
        expire, autoflush = self.maker_class_options(cls, node.value, sym.module)
        ca = next((k.value for k in node.value.keywords if k.arg == "connect_args"), None)
        maker = (f"{RT}::orm::sessionmaker(&[], &[(\"expire_on_commit\".to_string(), V::Bool({str(expire).lower()})), "
                 f"(\"autoflush\".to_string(), V::Bool({str(autoflush).lower()}))], false)?")
        if ca is None:
            return maker
        if "timezone" in ast.unparse(ca).lower():
            # psycopg decodes timestamptz in the connection's zone: per engine, not per transaction
            raise self.err(f"{sym.name}: a TimeZone in the connect_args of a session maker wrapper is not supported "
                           "(set it on the application's engine)", ca, sym.module)
        return fc.q(f"{RT}::orm::maker_params({maker}, &{fc.expr(ca)})")

    def maker_class_options(self, cls: Sym, call: ast.Call, call_module: str) -> tuple[bool, bool]:
        """A project class wrapping `async_sessionmaker` (an instance per module, called for a session):
        (expire_on_commit, autoflush) from the `"opt": kwargs.pop("opt", default)` entries of an `__init__`
        in its class chain, applied to the constructor call."""
        chain, c = [], cls
        while isinstance(c, Sym):
            d = self.ix.definition(c)
            if not isinstance(d, ast.ClassDef):
                break
            chain.append((c, d))
            c = self.resolve(c.module, d.bases[0]) if len(d.bases) == 1 else None
        names = {n for _, d in chain for n in ast.walk(d) if isinstance(n, ast.Name)} | \
                {n.attr for _, d in chain for n in ast.walk(d) if isinstance(n, ast.Attribute)}
        has_maker = any(isinstance(n, ast.Name) and n.id == "async_sessionmaker" for _, d in chain for n in ast.walk(d))
        has_call = any(isinstance(st, ast.FunctionDef) and st.name == "__call__" for _, d in chain for st in d.body)
        opts = {}
        for _, d in chain:
            for n in ast.walk(d):
                if isinstance(n, ast.Dict):
                    for k, v in zip(n.keys, n.values):
                        if (isinstance(k, ast.Constant) and isinstance(v, ast.Call) and isinstance(v.func, ast.Attribute)
                                and v.func.attr == "pop" and len(v.args) == 2 and isinstance(v.args[0], ast.Constant)
                                and v.args[0].value == k.value and isinstance(v.args[1], ast.Constant)):
                            opts.setdefault(k.value, v.args[1].value)
        if not (has_maker and has_call and {"expire_on_commit", "autoflush"} <= opts.keys()) or not names:
            raise self.err(f"session maker class {cls.name}: only a wrapper of async_sessionmaker whose __init__ reads "
                           "expire_on_commit and autoflush with kwargs.pop(name, default) is supported", call, call_module)
        for kw in call.keywords:
            if kw.arg in opts:
                opts[kw.arg] = literal(kw.value, self.src(call_module))
            elif kw.arg is None:
                raise self.err("**kwargs in a session maker call is not supported", kw, call_module)
        if opts.get("autocommit", False) is not False:
            raise self.err("autocommit=True is not supported", call, call_module)
        return bool(opts["expire_on_commit"]), bool(opts["autoflush"])

    @staticmethod
    def _sync_maker(maker: ast.Call) -> bool:
        """`sessionmaker(...)` makes synchronous sessions unless `class_=AsyncSession`."""
        if (dotted(maker.func) or "").split(".")[-1] != "sessionmaker":
            return False
        return not any(kw.arg == "class_" and (dotted(kw.value) or "").split(".")[-1] == "AsyncSession" for kw in maker.keywords)

    def _expire_on_commit(self) -> bool:
        for m in list(self.ix.modules.values()):
            for n in ast.walk(m.tree):
                if isinstance(n, ast.Call) and (dotted(n.func) or "").split(".")[-1] in {"async_sessionmaker", "sessionmaker"}:
                    for kw in n.keywords:
                        if kw.arg == "expire_on_commit" and isinstance(kw.value, ast.Constant):
                            return bool(kw.value.value)
        return True

    def factory_member(self, module: str, scope, name: str):
        """`name` read by a function defined in the application factory (`create_app`), when the factory binds it
        once at its top level, by a `def` or a single assignment, and nowhere else: (that statement, the factory),
        else None. The factory runs once, at startup: what it binds there is what every request sees."""
        if not isinstance(scope, (ast.FunctionDef, ast.AsyncFunctionDef)):
            return None
        m = self.ix.module(module)
        factory = next((d for d in m.tree.body if isinstance(d, (ast.FunctionDef, ast.AsyncFunctionDef))
                        and any(st is scope for st in d.body)), None)
        if factory is None:
            return None
        binds = [st for st in factory.body if (isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)) and st.name == name)
                 or (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name)
                     and st.targets[0].id == name)]
        stores = [n for n in ast.walk(factory) if (isinstance(n, ast.Name) and n.id == name and isinstance(n.ctx, ast.Store))
                  or (isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)) and n.name == name)
                  or (isinstance(n, ast.arg) and n.arg == name)]
        if len(binds) != 1 or len(stores) != 1:
            return None
        if factory.body.index(binds[0]) > next(i for i, st in enumerate(factory.body) if st is scope):
            return None  # bound after the `def` reading it: Python's NameError when the factory runs
        return binds[0], factory

    def factory_function(self, module: str, scope, name: str) -> Sym | None:
        """A function of the application factory read by name (`Depends(claims_from_request)`): compiled with the
        factory's locals, like the endpoints defined there (`nested`)."""
        hit = self.factory_member(module, scope, name)
        if hit is None or not isinstance(hit[0], (ast.FunctionDef, ast.AsyncFunctionDef)):
            return None
        fn, factory = hit
        nsym = Sym(module, f"{factory.name}.<locals>.{fn.name}")
        self.__dict__.setdefault("nested", {})[nsym] = (fn, factory)
        return nsym

    def expand_alias(self, ann: ast.AST, module: str, scope=None) -> tuple[ast.AST, str]:
        """`DbDep = Annotated[AsyncSession, Depends(get_db)]` used as `db: DbDep` -> the aliased type (also bound in
        the application factory, `Auth = Annotated[Claims, Depends(local)]`)."""
        for _ in range(10):
            if not isinstance(ann, (ast.Name, ast.Attribute)):
                return ann, module
            t = self.ix.resolve_expr(module, ann, scope)
            if t is None and isinstance(ann, ast.Name):
                hit = self.factory_member(module, scope, ann.id)
                if hit is not None and isinstance(hit[0], ast.Assign) and isinstance(hit[0].value, ast.Subscript):
                    ann = hit[0].value  # read in the endpoint's scope: its names are the factory's
                    continue
            if not isinstance(t, Sym):
                return ann, module
            d = self.ix.definition(t)
            if isinstance(d, ast.Assign) and isinstance(d.value, (ast.Subscript, ast.BinOp)):
                ann, module, scope = d.value, t.module, None
            elif isinstance(d, ast.AnnAssign) and d.value is not None and (dotted(d.annotation) or "").endswith("TypeAlias"):
                ann, module, scope = d.value, t.module, None
            elif isinstance(d, ast.TypeAlias):
                ann, module, scope = d.value, t.module, None
            elif (isinstance(d, ast.Assign) and len(d.targets) == 1 and isinstance(d.value, ast.Name)
                  and d.value.id in BUILTIN_TYPES and self.ix.resolve_expr(t.module, d.value) is None):
                # `MyInt = int` (perhaps after the class that uses it, under `from __future__ import annotations`)
                ann, module, scope = d.value, t.module, None
            else:
                return ann, module
        return ann, module

    def resolve(self, module: str, node: ast.AST, scope=None):
        t = self.ix.resolve_expr(module, node, scope)
        if isinstance(t, Ext):
            return Ext(libmap.canonical(t.dotted))
        return t

    # ---------------------------------------------------------------- constant evaluation

    def const(self, node: ast.AST, module: str, scope=None):
        """Transpile-time value of a literal, possibly through module constants."""
        if isinstance(node, (ast.Name, ast.Attribute)):
            t = self.resolve(module, node, scope)
            if isinstance(t, Sym):
                d = self.ix.definition(t)
                if isinstance(d, (ast.Assign, ast.AnnAssign)) and d.value is not None:
                    return self.const(d.value, t.module)
            if isinstance(t, Ext):
                code = libmap.status_constant(t.dotted)
                if code is not None:
                    return code
            raise self.err(f"`{ast.unparse(node)}` is not a constant", node, module)
        if isinstance(node, ast.BinOp):
            import operator
            ops = {ast.Add: operator.add, ast.Sub: operator.sub, ast.Mult: operator.mul, ast.FloorDiv: operator.floordiv,
                   ast.Div: operator.truediv, ast.Mod: operator.mod, ast.Pow: operator.pow, ast.BitOr: operator.or_}
            f = ops.get(type(node.op))
            if f is not None:
                return f(self.const(node.left, module, scope), self.const(node.right, module, scope))
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.USub):
            return -self.const(node.operand, module, scope)
        if isinstance(node, ast.JoinedStr):
            raise self.err("f-strings are not constants", node, module)
        if isinstance(node, (ast.Tuple, ast.List, ast.Set)):
            vals = [self.const(e, module, scope) for e in node.elts]
            return {ast.Tuple: tuple, ast.List: list, ast.Set: set}[type(node)](vals)
        if isinstance(node, ast.Call) and not node.keywords:
            # pure methods/builtins over constants: "|".join(TYPES), sorted(...), len(...)
            f = node.func
            if isinstance(f, ast.Attribute) and f.attr in {"join", "upper", "lower", "strip", "replace", "format"}:
                recv = self.const(f.value, module, scope)
                if isinstance(recv, str):
                    return getattr(recv, f.attr)(*(self.const(a, module, scope) for a in node.args))
            if isinstance(f, ast.Name) and f.id in {"len", "sorted", "tuple", "list", "frozenset", "set", "str", "int"} \
                    and self.resolve(module, f, scope) is None:
                return {"len": len, "sorted": sorted, "tuple": tuple, "list": list, "frozenset": frozenset, "set": set,
                        "str": str, "int": int}[f.id](*(self.const(a, module, scope) for a in node.args))
        try:
            return ast.literal_eval(node)
        except Exception:
            raise self.err(f"`{ast.unparse(node)}` is not a constant", node, module) from None

    DT_CTORS = {"datetime.datetime", "datetime.date", "datetime.timezone", "datetime.timedelta"}

    def const_bound(self, node: ast.AST, module: str, scope=None):
        """A `gt=`/`ge=`/`lt=`/`le=` bound: a constant, or a `datetime(...)` / `date(...)` built from constants
        (`tzinfo=timezone.utc` or `timezone(timedelta(...))`), evaluated here as Python would."""
        import datetime as pydt
        if isinstance(node, ast.Attribute) and isinstance(self.resolve(module, node, scope), Ext) \
                and self.resolve(module, node, scope).dotted == "datetime.timezone.utc":
            return pydt.timezone.utc
        if isinstance(node, (ast.Name, ast.Attribute)):
            t = self.resolve(module, node, scope)
            if isinstance(t, Sym):
                d = self.ix.definition(t)
                if isinstance(d, (ast.Assign, ast.AnnAssign)) and d.value is not None:
                    return self.const_bound(d.value, t.module)
        if isinstance(node, ast.Call):
            t = self.resolve(module, node.func, scope)
            if isinstance(t, Ext) and t.dotted in self.DT_CTORS:
                args = [self.const_bound(a, module, scope) for a in node.args]
                kws = {k.arg: self.const_bound(k.value, module, scope) for k in node.keywords}
                if any(k is None for k in kws):
                    raise self.err(f"`{ast.unparse(node)}`: ** arguments are not supported", node, module)
                try:
                    return getattr(pydt, t.dotted.split(".")[1])(*args, **kws)
                except (TypeError, ValueError, OverflowError) as exc:
                    raise self.err(f"`{ast.unparse(node)}` raises {type(exc).__name__}: {exc}", node, module) from None
        return self.const(node, module, scope)

    def _dt_lit(self, v) -> str:
        import datetime as pydt
        if type(v) is pydt.date:
            return (f"{RT}::pyd::DtLit {{ ymd: ({v.year}, {v.month}, {v.day}), hms: (0, 0, 0, 0), offset: None, "
                    "date: true }")
        off = v.utcoffset()
        o = "None" if off is None else f"Some({int(off.total_seconds())})"
        return (f"{RT}::pyd::DtLit {{ ymd: ({v.year}, {v.month}, {v.day}), hms: ({v.hour}, {v.minute}, {v.second}, "
                f"{v.microsecond}), offset: {o}, date: false }}")

    def _dt_cons(self, base: str, kind: str, cons: dict, tz, ann, module: str) -> str:
        """datetime/date bounds (and AwareDatetime/NaiveDatetime) around the type; any other constraint is refused
        (pydantic raises when it builds the schema)."""
        import datetime as pydt
        bounds = {k: cons[k] for k in ("le", "lt", "ge", "gt") if k in cons and cons[k] is not None}
        if bad := [k for k in cons if not k.startswith("_") and k not in bounds and cons[k] is not None]:
            raise self.err(f"{bad[0]}= does not apply to `{kind}`", ann, module)
        for k, v in bounds.items():
            # pydantic-core compares like values only: a date bound on a datetime (or the reverse) fails to build
            if not (type(v) is pydt.datetime if kind == "datetime" else type(v) is pydt.date):
                raise self.err(f"{k}={v!r}: a `{kind}` bound must be a {kind}(...) value", ann, module)
            if kind == "datetime" and v.tzinfo is not None and not isinstance(v.tzinfo, pydt.timezone):
                raise self.err(f"{k}=: only fixed offsets (timezone.utc, timezone(timedelta(...))) are supported", ann, module)
        if not bounds and tz is None:
            return base
        lits = {k: f"Some({self._dt_lit(bounds[k])})" if k in bounds else "None" for k in ("le", "lt", "ge", "gt")}
        tzs = "None" if tz is None else f"Some({str(tz).lower()})"
        dtc = (f"{RT}::pyd::DtC {{ le: {lits['le']}, lt: {lits['lt']}, ge: {lits['ge']}, gt: {lits['gt']}, "
               f"tz: {tzs} }}")
        return f"{RT}::pyd::TD::DtC(&{self._static_td(base)}, {dtc})"

    # ---------------------------------------------------------------- types -> TD

    def td(self, ann: ast.AST, module: str, cons: dict | None = None, scope=None) -> str:
        """Pydantic type annotation -> name of a `static TD`. `cons["_val_bytes"]`: the model's `val_json_bytes`,
        for every `bytes` of the field (inside containers too, not inside another model)."""
        prev, self._val_bytes = self.__dict__.get("_val_bytes"), (cons or {}).get("_val_bytes")
        try:
            out = self._static_td(self._td_init(ann, module, cons or {}, scope))
        finally:
            self._val_bytes = prev
        late = [k for k, v in (cons or {}).items() if isinstance(v, LateCons)]
        if late and not cons.get("_late_used"):
            raise self.err(f"{late[0]}= read from the settings does not apply to `{ast.unparse(ann)}` (int, float and "
                           "str only)", ann, module)
        return out

    def _static_td(self, init: str) -> str:
        if init not in self.tds:
            self.tds[init] = f"TD_{len(self.tds) + 1}"
        return self.tds[init]

    def pat(self, src: str) -> str:
        if src not in self.pats:
            self.pats[src] = f"PAT_{len(self.pats) + 1}"
        return self.pats[src]

    def settings_value(self, node: ast.AST, module: str, scope=None) -> bool:
        """`settings.MAX_PAGE` (an attribute of a module global that is not a constant, like a pydantic-settings
        instance): a value the application reads when it starts."""
        if not isinstance(node, ast.Attribute):
            return False
        root = node
        while isinstance(root, ast.Attribute):
            root = root.value
        if not isinstance(root, ast.Name):
            return False
        t = self.resolve(module, root, scope)
        if not isinstance(t, Sym):
            return False
        d = self.ix.definition(t)
        if not (isinstance(d, (ast.Assign, ast.AnnAssign)) and isinstance(d.value, ast.Call)):
            return False
        try:
            self.const(node, module, scope)
            return False  # a constant after all
        except TranspileError:
            return True

    def late_bounds(self, late: dict) -> str:
        """`LB_n`, the bounds read from the settings at startup, and `lb_n` (in the startup list) that sets them."""
        items = self.__dict__.setdefault("late_items", [])
        key = tuple((k, lc.module, lc.node.lineno, lc.node.col_offset) for k, lc in late.items())
        done = self.__dict__.setdefault("late_done", {})
        if key in done:
            return done[key]
        n = len(items) + 1
        name, fn = f"LB_{n}", f"lb_{n}"
        first = next(iter(late.values()))
        fc = FnCompiler(self, first.module, None, fn)
        prev, self.cur = self.cur, ("global", Sym(first.module, f"<{fn}>"))
        try:
            fields = []
            for k, lc in late.items():
                fc.module = lc.module
                v = fc.expr(lc.node)
                conv = "late_len" if k in ("min_length", "max_length") else "late_num"
                field = {"min_length": "min", "max_length": "max"}.get(k, k)
                fields.append(f"{field}: {fc.q(f'{RT}::pyd::{conv}(&{v})')}")
        finally:
            self.cur = prev
        body = "\n".join("    " + line for line in fc.lines)
        self.items.append(
            f"pub static {name}: {RT}::pyd::LateBounds = {RT}::pyd::LateBounds::new();\n"
            f"/// constraints read from the settings ({self.rel_file(first.module)}:{first.node.lineno})\n"
            f"pub async fn {fn}(cx: &Cx) -> R {{\n{body}\n    {name}.set({RT}::pyd::LB {{ {', '.join(fields)}, ..Default::default() }});\n"
            f"    Ok(V::None)\n}}")
        items.append(name)
        done[key] = name
        self.__dict__.setdefault("eager", []).append((first.module, first.node.lineno, fn))
        return name

    def _late(self, cons: dict, keys: tuple) -> str:
        late = {k: cons[k] for k in keys if isinstance(cons.get(k), LateCons)}
        if not late:
            return "None"
        cons["_late_used"] = True
        return f"Some(&{self.late_bounds(late)})"

    def _num(self, cons: dict) -> str:
        def o(k):
            return f"Some({float(cons[k])!r}f64)" if k in cons and not isinstance(cons[k], LateCons) else "None"
        return (f"{RT}::pyd::NumC {{ ge: {o('ge')}, gt: {o('gt')}, le: {o('le')}, lt: {o('lt')}, mul: {o('multiple_of')}, "
                f"finite: {str(bool(cons.get('_finite'))).lower()}, late: {self._late(cons, ('ge', 'gt', 'le', 'lt'))} }}")

    def _dec(self, cons: dict, node, module: str) -> str:
        def bound(k):
            if k not in cons:
                return "None"
            v = cons[k]
            if isinstance(v, bool) or not isinstance(v, (int, float)):
                raise self.err(f"Decimal bound {k}={v!r}: only int and float literals are supported", node, module)
            return f"Some({rs(repr(v))})"  # Decimal(str(v)), as pydantic converts it

        def count(k):
            return f"Some({int(cons[k])})" if k in cons else "None"
        return (f"{RT}::pyd::DecC {{ ge: {bound('ge')}, gt: {bound('gt')}, le: {bound('le')}, lt: {bound('lt')}, "
                f"max_digits: {count('max_digits')}, decimal_places: {count('decimal_places')} }}")

    def _str(self, cons: dict) -> str:
        # a field's own length constraints, else model_config's str_min_length / str_max_length
        def length(k, cfg):
            if isinstance(cons.get(k), LateCons):
                return f"Some({cons[cfg]})" if cfg in cons else "None"
            return f"Some({cons[k]})" if k in cons else f"Some({cons[cfg]})" if cfg in cons else "None"
        mn, mx = length("min_length", "_smin"), length("max_length", "_smax")
        late = self._late(cons, ("min_length", "max_length"))
        p = f"Some(&{self.pat(cons['pattern'])})" if "pattern" in cons else "None"
        return (f"{RT}::pyd::StrC {{ late: {late}, min: {mn}, max: {mx}, pattern: {p}, strip: {str(bool(cons.get('_strip'))).lower()}, "
                f"lower: {str(bool(cons.get('_lower'))).lower()}, upper: {str(bool(cons.get('_upper'))).lower()}, "
                f"ascii: {str(bool(cons.get('_ascii'))).lower()} }}")

    CONFIG_CONS = {"_strip", "_lower", "_upper", "_smin", "_smax", "_enum_values", "_val_bytes", "_dc_strict", "_dc_extra",
                   "_finite"}

    @classmethod
    def _cfg(cls, cons: dict) -> dict:
        """The model_config part of a field's constraints: it also applies to the items of its containers (the
        core config of pydantic-core covers every str and enum the model validates)."""
        return {k: v for k, v in cons.items() if k in cls.CONFIG_CONS}

    def _sized(self, td: str, cons: dict, node: ast.AST, module: str) -> str:
        """A container's `Field(min_length=, max_length=)` (pydantic's other constraints do not apply to it)."""
        bad = [k for k in cons if not k.startswith("_") and k not in ("min_length", "max_length")]
        if bad:
            raise self.err(f"Field({bad[0]}=...) on `{ast.unparse(node)}` is not supported (min_length/max_length only)",
                           node, module)
        if "min_length" not in cons and "max_length" not in cons:
            return td
        opt = lambda k: f"Some({int(cons[k])})" if k in cons else "None"  # noqa: E731
        return f"{RT}::pyd::TD::Len(&{self._static_td(td)}, {opt('min_length')}, {opt('max_length')})"

    def _td_init(self, ann: ast.AST, module: str, cons: dict, scope=None) -> str:
        expanded, emod = self.expand_alias(ann, module, scope)
        if expanded is not ann:
            return self._td_init(expanded, emod, cons, None)
        if isinstance(ann, ast.Constant) and ann.value is None:
            return f"{RT}::pyd::TD::NoneT"
        if isinstance(ann, ast.Constant) and isinstance(ann.value, str):
            try:
                ann = ast.parse(ann.value, mode="eval").body
            except SyntaxError:
                raise self.err(f"bad string annotation {ann.value!r}", ann, module) from None
        if isinstance(ann, ast.BinOp) and isinstance(ann.op, ast.BitOr):
            parts = self._union_parts(ann)
            return self._union(parts, module, cons, scope, ann)
        if isinstance(ann, ast.Call):
            return self._constrained_call(ann, module, cons, scope)
        if isinstance(ann, ast.Subscript):
            outer = self.resolve(module, ann.value, scope)
            od = outer.dotted if isinstance(outer, Ext) else (dotted(ann.value) or "")
            last = od.split(".")[-1]
            args = ann.slice.elts if isinstance(ann.slice, ast.Tuple) else [ann.slice]
            if last == "Optional":
                return f"{RT}::pyd::TD::Optional(&{self._static_td(self._td_init(args[0], module, cons, scope))})"
            if last == "Union":
                return self._union(args, module, cons, scope, ann)
            if last == "Annotated":
                cons = dict(cons)
                for meta in args[1:]:
                    cons.update(self.annotated_meta(meta, module, scope))
                return self._td_init(args[0], module, cons, scope)
            if od in ("pydantic.Json", "pydantic.types.Json"):
                # Json[T]: parsed by jiter, then validated as T (errors at the field's loc)
                if len(args) != 1:
                    raise self.err(f"unsupported type annotation `{ast.unparse(ann)}`: Json[T] takes one type", ann, module)
                bad = [k for k in cons if not k.startswith("_") and k not in self.CONFIG_CONS]
                if bad:
                    raise self.err(f"`{ast.unparse(ann)}` with Field({bad[0]}=) is not supported", ann, module)
                return f"{RT}::pyd::TD::Json(Some(&{self._static_td(self._td_init(args[0], module, self._cfg(cons), scope))}))"
            if last == "Iterable":
                # pydantic's ValidatorIterator: the input must be iterable, its items are validated when reached
                bad = [k for k in cons if not k.startswith("_") and k not in self.CONFIG_CONS]
                if bad:
                    raise self.err(f"`{ast.unparse(ann)}` with Field({bad[0]}=) is not supported", ann, module)
                return f"{RT}::pyd::TD::Iter(&{self._static_td(self._td_init(args[0], module, self._cfg(cons), scope))})"
            if last in {"MutableSequence", "Collection"}:
                # pydantic: a Sequence rejects a str (`sequence_str`) and keeps the input's type, an Iterable is a
                # lazy validator: neither is a list
                raise self.err(f"`{ast.unparse(ann)}` is not supported (pydantic validates it differently from a list): "
                               "use list[...]", ann, module)
            if last in {"list", "List"}:
                return self._sized(f"{RT}::pyd::TD::List(Some(&{self._static_td(self._td_init(args[0], module, self._cfg(cons), scope))}))", cons, ann, module)
            if last in {"set", "Set"}:
                return self._sized(f"{RT}::pyd::TD::Set(Some(&{self._static_td(self._td_init(args[0], module, self._cfg(cons), scope))}))", cons, ann, module)
            if last in {"frozenset", "FrozenSet", "deque", "Deque", "Sequence"}:
                inner = self._static_td(self._td_init(args[0], module, self._cfg(cons), scope))
                base = "Set" if last in {"frozenset", "FrozenSet"} else "List"
                kind = {"frozenset": "Frozen", "FrozenSet": "Frozen", "deque": "Deque", "Deque": "Deque", "Sequence": "Seq"}[last]
                coll = f"{RT}::pyd::TD::Coll(&{self._static_td(f'{RT}::pyd::TD::{base}(Some(&{inner}))')}, {RT}::pyd::Coll::{kind})"
                return self._sized(coll, cons, ann, module)
            if last in {"Counter", "OrderedDict"} and len(args) in (1, 2):
                k = self._static_td(self._td_init(args[0], module, self._cfg(cons), scope))
                v = (self._static_td(f"{RT}::pyd::TD::Int({self._num({})})") if last == "Counter"
                     else self._static_td(self._td_init(args[1], module, self._cfg(cons), scope)))
                if (last == "Counter") != (len(args) == 1):
                    raise self.err(f"unsupported type annotation `{ast.unparse(ann)}`", ann, module)
                d = self._static_td(f"{RT}::pyd::TD::Dict(Some((&{k}, &{v})))")
                return self._sized(f"{RT}::pyd::TD::Coll(&{d}, {RT}::pyd::Coll::{'Counter' if last == 'Counter' else 'Ordered'})", cons, ann, module)
            if last in {"tuple", "Tuple"}:
                if not (len(args) == 2 and isinstance(args[1], ast.Constant) and args[1].value is Ellipsis):
                    if any(isinstance(a, ast.Constant) and a.value is Ellipsis for a in args):
                        raise self.err(f"`{ast.unparse(ann)}`: a variable tuple has one type", ann, module)
                    if [k for k in cons if not k.startswith("_")]:
                        raise self.err(f"constraints on `{ast.unparse(ann)}` are not supported", ann, module)
                    # `tuple[()]`: an empty tuple
                    elts = [] if len(args) == 1 and isinstance(args[0], ast.Tuple) and not args[0].elts else args
                    tds = ", ".join(f"&{self._static_td(self._td_init(a, module, self._cfg(cons), scope))}" for a in elts)
                    return f"{RT}::pyd::TD::TupleFix(&[{tds}])"
                return self._sized(f"{RT}::pyd::TD::Tuple(Some(&{self._static_td(self._td_init(args[0], module, self._cfg(cons), scope))}))", cons, ann, module)
            if last in {"dict", "Dict", "Mapping"}:
                k = self._static_td(self._td_init(args[0], module, self._cfg(cons), scope))
                v = self._static_td(self._td_init(args[1], module, self._cfg(cons), scope))
                return self._sized(f"{RT}::pyd::TD::Dict(Some((&{k}, &{v})))", cons, ann, module)
            if isinstance(outer, Sym) and outer in self.fe.schema_syms:
                return f"{RT}::pyd::TD::Schema(&{self.schema(self.generic_args(outer, ann, module, scope)).rust})"
            if isinstance(outer, Ext) and outer.dotted in {"typing.Callable", "collections.abc.Callable"}:
                return f"{RT}::pyd::TD::Callable"
            if isinstance(outer, Ext) and outer.dotted in {"pydantic.SkipValidation", "pydantic.functional_validators.SkipValidation"}:
                # no validation: the value as given
                return f"{RT}::pyd::TD::Any"
            if last in {"type", "Type"} and len(args) == 1:
                t = self.resolve(module, args[0], scope)
                if isinstance(t, Ext) and t.dotted in {"pydantic.BaseModel", "pydantic.main.BaseModel"}:
                    return f"{RT}::pyd::TD::Subclass(\"BaseModel\", None)"
                if isinstance(t, Sym) and t in self.fe.schema_syms:
                    return f"{RT}::pyd::TD::Subclass({rs(t.name)}, Some(&{self.schema(t).cls}))"
                if isinstance(t, Sym) and isinstance(self.ix.definition(t), ast.ClassDef):
                    # a plain class: a value can only be one of the project's model classes, never this one
                    node = self.ix.definition(t)
                    if any(not (isinstance(b, ast.Name) and b.id == "object") for b in node.bases) or node.keywords:
                        raise self.err(f"`{ast.unparse(ann)}` of a class with bases is not supported", ann, module)
                    return f"{RT}::pyd::TD::Subclass({rs(t.name)}, None)"
                raise self.err(f"unsupported type annotation `{ast.unparse(ann)}`", ann, module)
            if last == "Literal":
                lits = []
                for a in args:
                    v = self.const(a, module, scope)
                    if isinstance(v, bool):
                        lits.append(f"{RT}::pyd::Lit::Bool({str(v).lower()})")
                    elif isinstance(v, int):
                        lits.append(f"{RT}::pyd::Lit::Int({v})")
                    elif isinstance(v, str):
                        lits.append(f"{RT}::pyd::Lit::Str({rs(v)})")
                    elif v is None:
                        lits.append(f"{RT}::pyd::Lit::None")
                    else:
                        raise self.err(f"unsupported Literal value {v!r}", a, module)
                return f"{RT}::pyd::TD::Literal(&[{', '.join(lits)}])"
            raise self.err(f"unsupported type annotation `{ast.unparse(ann)}`", ann, module)
        name = dotted(ann)
        if name == "frozenset" and not isinstance(self.resolve(module, ann, scope), Sym):
            return self._sized(f"{RT}::pyd::TD::Coll(&{self._static_td(f'{RT}::pyd::TD::Set(None)')}, {RT}::pyd::Coll::Frozen)", cons, ann, module)
        if name == "complex" and not isinstance(self.resolve(module, ann, scope), Sym):
            if [k for k in cons if not k.startswith("_")]:
                raise self.err("constraints on `complex` are not supported", ann, module)
            return f"{RT}::pyd::TD::Complex"
        if name in {"int", "float", "str", "bool", "dict", "list", "set", "tuple", "bytes", "Any", "object"} and not isinstance(self.resolve(module, ann, scope), Sym):
            if bad := [k for k in ("max_digits", "decimal_places") if k in cons]:
                # pydantic raises TypeError at validation time ("Unable to apply constraint")
                raise self.err(f"{bad[0]}= applies to Decimal only, not to `{name}`", ann, module)
            return self._scalar(name, cons, ann, module)
        t = self.resolve(module, ann, scope)
        if isinstance(t, Ext):
            last = t.dotted.split(".")[-1]
            mapping = {
                "datetime.datetime": f"{RT}::pyd::TD::DateTime",
                "datetime.date": f"{RT}::pyd::TD::Date",
                "datetime.time": f"{RT}::pyd::TD::Time",
                "datetime.timedelta": f"{RT}::pyd::TD::Delta",
                "uuid.UUID": f"{RT}::pyd::TD::Uuid",
                "pathlib.Path": f"{RT}::pyd::TD::Path",
                **{f"pydantic{m}.UUID{n}": f"{RT}::pyd::TD::UuidV({n})" for m in ("", ".types") for n in range(1, 9)},
                **dict.fromkeys(("pydantic.SecretStr", "pydantic.types.SecretStr"), f"{RT}::pyd::TD::Secret"),
                # Json: the str/bytes input parsed by jiter (pydantic-core's parser), the value as it is
                **dict.fromkeys(("pydantic.Json", "pydantic.types.Json"), f"{RT}::pyd::TD::Json(None)"),
                **dict.fromkeys(("typing.Callable", "collections.abc.Callable"), f"{RT}::pyd::TD::Callable"),
                "types.EllipsisType": f"{RT}::pyd::TD::Ellipsis",
                "fractions.Fraction": f"{RT}::pyd::TD::Fraction",
                **dict.fromkeys(("pydantic.MISSING", "pydantic.experimental.missing_sentinel.MISSING"), f"{RT}::pyd::TD::MissingSentinel"),
                "typing.Any": f"{RT}::pyd::TD::Any",
                # the str schema under EmailStr's validator gets the str_* settings of model_config
                **dict.fromkeys(("pydantic.EmailStr", "pydantic.networks.EmailStr"), f"{RT}::pyd::TD::Email("
                                + self._str({k: v for k, v in cons.items() if k in ("_strip", "_lower", "_upper")}) + ")"),
                **{f"pydantic{m}.{n}": f"{RT}::pyd::TD::Url(&{RT}::pyd::{c})" for m in ("", ".networks")
                   for n, c in (("AnyUrl", "ANY_URL"), ("AnyHttpUrl", "ANY_HTTP_URL"), ("HttpUrl", "HTTP_URL"),
                                ("RedisDsn", "REDIS_DSN"), ("PostgresDsn", "POSTGRES_DSN"), ("CockroachDsn", "COCKROACH_DSN"),
                                ("MongoDsn", "MONGO_DSN"), ("AmqpDsn", "AMQP_DSN"), ("KafkaDsn", "KAFKA_DSN"), ("MySQLDsn", "MYSQL_DSN"),
                                ("MariaDBDsn", "MARIADB_DSN"))},
            }
            alias = self.CONSTRAINED_ALIASES.get(last) if t.dotted in (f"pydantic.{last}", f"pydantic.types.{last}") else None
            if alias is not None:
                base, extra = alias
                inner = self._td_init(ast.Name(id=base, ctx=ast.Load()), module, {**({} if extra == "strict" else extra), **cons}, scope)
                if extra == "strict":
                    return f"{RT}::pyd::TD::StrictTop(&{self._static_td(inner)})"
                return inner
            aware = {f"pydantic{m}.{n}": n == "AwareDatetime" for m in ("", ".types") for n in ("AwareDatetime", "NaiveDatetime")}
            if t.dotted in aware:
                return self._dt_cons(f"{RT}::pyd::TD::DateTime", "datetime", cons, aware[t.dotted], ann, module)
            if t.dotted in ("datetime.datetime", "datetime.date"):
                return self._dt_cons(mapping[t.dotted], t.dotted.split(".")[1], cons, None, ann, module)
            if t.dotted in mapping:
                if any(isinstance(v, __import__("datetime").date) for v in cons.values()):
                    raise self.err(f"a datetime bound does not apply to `{ast.unparse(ann)}`", ann, module)
                return mapping[t.dotted]
            if t.dotted == "decimal.Decimal":
                if "multiple_of" in cons:
                    raise self.err("Field(multiple_of=) on a Decimal is not supported", ann, module)
                return f"{RT}::pyd::TD::Decimal({self._dec(cons, ann, module)})"
            if t.package == "typing" and last in {"Dict", "List", "Set", "Tuple"}:
                return self._scalar(last.lower(), cons, ann, module)
            if libmap.canonical(t.dotted) in LIB_INSTANCE_TYPES:
                # an arbitrary type of a library the binary only stores (libs::lib_instance): an isinstance check
                return f"{RT}::pyd::TD::Instance({rs(last)})"
            raise self.err(f"unsupported type annotation `{ast.unparse(ann)}` ({t.dotted})", ann, module)
        if isinstance(t, Sym):
            if t in self.fe.schema_syms:
                return f"{RT}::pyd::TD::Schema(&{self.schema(t).rust})"
            if self.enum_kind(t):
                return f"{RT}::pyd::TD::Enum(&{self.enum(t)[0]}, {str(bool(cons.get('_enum_values'))).lower()})"
            if t in self.fe.namedtuple_syms:
                # pydantic-core validates a NamedTuple as call arguments (tuple, list or dict, loc by position or
                # name), with other errors and inputs from one minor version to the next (2.14: named_tuple_type)
                raise self.err(f"type `{t.name}` is not supported as a Pydantic type: a NamedTuple field or parameter "
                               "is validated as call arguments (use a BaseModel or a tuple[...])", ann, module)
            if t in self.fe.dataclass_syms:
                # a dataclass as a Pydantic type (FastAPI body, response_model, a model's field): validated like a
                # pydantic dataclass, into an instance of the class
                return f"{RT}::pyd::TD::Schema(&{self.dataclass_view_cfg(t, self._cfg(cons), ann, module).rust})"
            node = self.ix.definition(t)
            if (isinstance(node, ast.ClassDef) and t not in self.fe.dataclass_syms and t not in self.fe.model_syms
                    and not node.decorator_list and not node.keywords
                    and all(isinstance(b, ast.Name) and b.id == "object" for b in node.bases)
                    and not any(isinstance(x, (ast.FunctionDef, ast.AsyncFunctionDef))
                                and x.name.startswith(("__get_pydantic", "__get_validators__", "__modify_schema__"))
                                for x in node.body)):
                # an arbitrary type (`arbitrary_types_allowed`, else pydantic refuses the model): an isinstance check
                return f"{RT}::pyd::TD::Instance({rs(t.name)})"
            if isinstance(node, ast.ClassDef) and any(isinstance(x, (ast.FunctionDef, ast.AsyncFunctionDef))
                                                      and x.name.startswith("__get_pydantic") for x in node.body):
                raise self.err(f"type `{t.qual}` customizes its Pydantic schema (__get_pydantic_core_schema__): not supported",
                               ann, module)
            raise self.err(f"type `{t.qual}` is not a Pydantic model or an enum (other classes are not supported)", ann, module)
        raise self.err(f"unsupported type annotation `{ast.unparse(ann)}`", ann, module)

    # pydantic's constrained and strict aliases: the builtin under them, and its constraints ("strict": Strict())
    CONSTRAINED_ALIASES = {
        "PositiveInt": ("int", {"gt": 0}), "NegativeInt": ("int", {"lt": 0}),
        "NonNegativeInt": ("int", {"ge": 0}), "NonPositiveInt": ("int", {"le": 0}),
        "PositiveFloat": ("float", {"gt": 0}), "NegativeFloat": ("float", {"lt": 0}),
        "NonNegativeFloat": ("float", {"ge": 0}), "NonPositiveFloat": ("float", {"le": 0}),
        "StrictStr": ("str", "strict"), "StrictInt": ("int", "strict"), "StrictFloat": ("float", "strict"),
        "StrictBool": ("bool", "strict"), "StrictBytes": ("bytes", "strict"),
    }
    CONSTR_KW = {"strip_whitespace": "_strip", "to_upper": "_upper", "to_lower": "_lower", "min_length": "min_length",
                 "max_length": "max_length", "pattern": "pattern", "strict": "strict"}
    CONNUM_KW = {"strict", "gt", "ge", "lt", "le", "multiple_of"}

    def _constrained_call(self, ann: ast.Call, module: str, cons: dict, scope) -> str:
        """`constr(...)`, `conint(...)`, `confloat(...)`, `condecimal(...)`, `conlist(T, ...)`, `conset(T, ...)`: what
        pydantic builds is `Annotated[<type>, <constraints>]`, so they translate as the annotation they stand for."""
        f = self.resolve(module, ann.func, scope)
        last = f.dotted.rsplit(".", 1)[-1] if isinstance(f, Ext) and f.dotted.startswith("pydantic") else ""
        if last not in {"constr", "conint", "confloat", "condecimal", "conlist", "conset"}:
            raise self.err(f"unsupported type annotation `{ast.unparse(ann)}`", ann, module)
        item = None
        if last in {"conlist", "conset"}:
            if len(ann.args) != 1:
                raise self.err(f"`{ast.unparse(ann)}`: the item type is the one positional argument", ann, module)
            item = ann.args[0]
        elif ann.args:
            raise self.err(f"`{ast.unparse(ann)}`: keyword arguments only", ann, module)
        allowed = {"constr": set(self.CONSTR_KW), "conint": self.CONNUM_KW, "confloat": self.CONNUM_KW,
                   "condecimal": {"gt", "ge", "lt", "le", "multiple_of", "max_digits", "decimal_places"},
                   "conlist": {"min_length", "max_length"}, "conset": {"min_length", "max_length"}}[last]
        own: dict = {}
        for k in ann.keywords:
            if k.arg not in allowed:
                raise self.err(f"`{ast.unparse(ann)}`: {k.arg or '**'}= is not supported", ann, module)
            v = self.const_bound(k.value, module, scope) if k.arg in {"gt", "ge", "lt", "le"} else self.const(k.value, module, scope)
            if v is None and k.arg != "strict":
                continue
            own[self.CONSTR_KW.get(k.arg, k.arg) if last == "constr" else k.arg] = v
        strict = own.pop("strict", False)
        merged = {**own, **cons}  # the field's own constraints apply after the type's
        base = {"constr": "str", "conint": "int", "confloat": "float"}.get(last)
        if base is not None:
            inner = self._td_init(ast.Name(id=base, ctx=ast.Load()), module, merged, scope)
        elif last == "condecimal":
            if "multiple_of" in merged:
                raise self.err("Field(multiple_of=) on a Decimal is not supported", ann, module)
            inner = f"{RT}::pyd::TD::Decimal({self._dec(merged, ann, module)})"
        else:
            sub = self._static_td(self._td_init(item, module, self._cfg(cons), scope))
            kind = "List" if last == "conlist" else "Set"
            inner = self._sized(f"{RT}::pyd::TD::{kind}(Some(&{sub}))", merged, ann, module)
        return f"{RT}::pyd::TD::StrictTop(&{self._static_td(inner)})" if strict else inner

    def _flatten_union(self, parts, module, scope) -> list[ast.AST]:
        """typing flattens nested unions: `int | Optional[str]` is `int | str | None` (labels, nullability)."""
        out = []
        for p in parts:
            if isinstance(p, ast.BinOp) and isinstance(p.op, ast.BitOr):
                out += self._flatten_union(self._union_parts(p), module, scope)
                continue
            if isinstance(p, ast.Subscript):
                outer = self.resolve(module, p.value, scope)
                od = outer.dotted if isinstance(outer, Ext) else (dotted(p.value) or "")
                last = od.split(".")[-1]
                args = p.slice.elts if isinstance(p.slice, ast.Tuple) else [p.slice]
                if last == "Optional":
                    out += self._flatten_union(args, module, scope) + [ast.Constant(value=None)]
                    continue
                if last == "Union":
                    out += self._flatten_union(args, module, scope)
                    continue
            out.append(p)
        return out

    def _union_parts(self, node: ast.AST) -> list[ast.AST]:
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr):
            return self._union_parts(node.left) + self._union_parts(node.right)
        return [node]

    def _union(self, parts, module, cons, scope, node) -> str:
        parts = self._flatten_union(parts, module, scope)
        none = [p for p in parts if isinstance(p, ast.Constant) and p.value is None]
        rest = [p for p in parts if p not in none]
        if len(rest) == 1:
            inner = self._static_td(self._td_init(rest[0], module, cons, scope))
            return f"{RT}::pyd::TD::Optional(&{inner})" if none else self._td_init(rest[0], module, cons, scope)
        ltr, disc, cons = cons.get("_ltr"), cons.get("_disc"), {k: v for k, v in cons.items() if k not in ("_ltr", "_disc")}
        if disc is not None:
            t = self._tagged(rest, disc, module, cons, scope, node)
            return f"{RT}::pyd::TD::Optional(&{self._static_td(t)})" if none else t
        tds = ", ".join(f"&{self._static_td(self._td_init(p, module, cons, scope))}" for p in rest)
        u = f"{RT}::pyd::TD::{'UnionLtr' if ltr else 'Union'}(&[{tds}])"
        self.__dict__.setdefault("union_src", {}).setdefault(u, (node, module))  # where a refusal points
        return f"{RT}::pyd::TD::Optional(&{self._static_td(u)})" if none else u

    def _tagged(self, parts, disc, module, cons, scope, node) -> str:
        """A discriminated union: each member's tags are the `Literal[...]` values of its discriminator field
        (a member that is itself a union contributes all of its members' values)."""
        dnode, dmodule = disc
        custom, func = None, None
        if isinstance(dnode, ast.Call):
            t = self.resolve(dmodule, dnode.func)
            if not (isinstance(t, Ext) and t.dotted.split(".")[-1] == "Discriminator" and len(dnode.args) == 1):
                raise self.err(f"discriminator `{ast.unparse(dnode)}` is not supported", dnode, dmodule)
            kw = {k.arg: k.value for k in dnode.keywords}
            if set(kw) - {"custom_error_type", "custom_error_message", "custom_error_context"}:
                raise self.err(f"Discriminator({', '.join(sorted(kw))}=) options are not supported", dnode, dmodule)
            if kw:
                ctx = self.const(kw["custom_error_context"], dmodule) if "custom_error_context" in kw else {}
                ce_type = self.const(kw["custom_error_type"], dmodule) if "custom_error_type" in kw else None
                ce_msg = self.const(kw["custom_error_message"], dmodule) if "custom_error_message" in kw else None
                if not (isinstance(ce_type, str) and isinstance(ce_msg, str) and isinstance(ctx, dict)
                        and all(isinstance(k, str) and isinstance(v, str) for k, v in ctx.items())):
                    raise self.err("Discriminator custom errors: a type, a message and a dict of strings are supported",
                                   dnode, dmodule)
                custom = (ce_type, ce_msg, ctx)
            dnode = dnode.args[0]
        field = self.const(dnode, dmodule) if isinstance(dnode, ast.Constant) else None
        if field is None:
            # a function of the input returning the tag; the members are tagged `Annotated[T, Tag('x')]`
            known = self.__dict__.setdefault("disc_syms", {})
            if id(dnode) not in known:  # one function per discriminator (the fixpoint passes come back here)
                known[id(dnode)] = Sym(dmodule, f"<disc_{len(known)}>")
                self.__dict__.setdefault("lambda_validators", {})[known[id(dnode)]] = dnode
            func = known[id(dnode)]
            shown = "<lambda>()" if isinstance(dnode, ast.Lambda) else f"{ast.unparse(dnode).split('.')[-1]}()"
            tags = []
            for p in parts:
                expanded, emod = self.expand_alias(p, module, scope)
                tag = None
                if isinstance(expanded, ast.Subscript) and (dotted(expanded.value) or "").split(".")[-1] == "Annotated":
                    for m in expanded.slice.elts[1:]:
                        mt = self.resolve(emod, m.func) if isinstance(m, ast.Call) else None
                        if isinstance(mt, Ext) and mt.dotted.split(".")[-1] == "Tag" and len(m.args) == 1:
                            tag = self.const(m.args[0], emod)
                if not isinstance(tag, str):
                    raise self.err(f"discriminated union member `{ast.unparse(p)}` needs a Tag('...')", p, module)
                if any(tag == k for k, _ in tags):
                    raise self.err(f"discriminated union: tag {tag!r} is in two members", node, module)
                tags.append((tag, self._static_td(self._td_init(p, module, cons, scope))))
            entries = ", ".join(f"({rs(v)}, &{td})" for v, td in tags)
            return (f"{RT}::pyd::TD::Tagged(&{RT}::pyd::Tagged {{ key: {rs(shown)}, attr: \"\", tags: &[{entries}], "
                    f"func: Some({method_wrapper(self, func)} as {RT}::pyd::MethodFn), custom: {self._custom(custom)} }})")
        if not isinstance(field, str):
            raise self.err(f"discriminator `{ast.unparse(dnode)}` is not supported (a field name only)", dnode, dmodule)
        keys, tags = set(), []

        def values(p, pmod, pscope):
            expanded, emod = self.expand_alias(p, pmod, pscope)
            if expanded is not p:
                return values(expanded, emod, None)
            if isinstance(p, ast.Constant) and isinstance(p.value, str):
                return values(ast.parse(p.value, mode="eval").body, pmod, pscope)
            if isinstance(p, ast.Subscript):
                outer = self.resolve(pmod, p.value, pscope)
                last = (outer.dotted if isinstance(outer, Ext) else (dotted(p.value) or "")).split(".")[-1]
                args = p.slice.elts if isinstance(p.slice, ast.Tuple) else [p.slice]
                if last == "Annotated":
                    return values(args[0], pmod, pscope)
                if last in {"Union", "Optional"}:
                    return [v for a in self._flatten_union(args, pmod, pscope) for v in values(a, pmod, pscope)]
            if isinstance(p, ast.BinOp) and isinstance(p.op, ast.BitOr):
                return [v for a in self._flatten_union(self._union_parts(p), pmod, pscope) for v in values(a, pmod, pscope)]
            t = self.resolve(pmod, p, pscope)
            if not (isinstance(t, Sym) and t in self.fe.schema_syms):
                raise self.err(f"discriminated union member `{ast.unparse(p)}` is not a Pydantic model", p, pmod)
            info = self.schema(t)
            f = next((f for f in info.fields if f["name"] == field), None)
            ann, amod = f["ann"] if f else (None, None)
            lit = self.expand_alias(ann, amod, None)[0] if f else None
            if isinstance(lit, ast.Subscript) and (dotted(lit.value) or "").split(".")[-1] == "Annotated":
                lit = lit.slice.elts[0]
            if not (isinstance(lit, ast.Subscript) and (dotted(lit.value) or "").split(".")[-1] == "Literal"):
                raise self.err(f"discriminated union: {t.name}.{field} must be a Literal[...] of strings", p, pmod)
            vals = [self.const(a, amod) for a in (lit.slice.elts if isinstance(lit.slice, ast.Tuple) else [lit.slice])]
            if not all(isinstance(v, str) for v in vals):
                raise self.err(f"discriminated union: {t.name}.{field} tags other than strings are not supported", p, pmod)
            if f.get("paths"):
                raise self.err(f"discriminated union: {t.name}.{field} has an AliasPath/AliasChoices", p, pmod)
            keys.add(f["alias"] or field)
            if info.__dict__.get("before") or info.__dict__.get("model_before"):
                raise self.err(f"discriminated union: {t.name} has mode='before' validators (not supported)", p, pmod)
            return vals

        for p in parts:
            td = self._static_td(self._td_init(p, module, cons, scope))
            for v in dict.fromkeys(values(p, module, scope)):  # a nested union: its members may share a tag
                if any(v == k for k, _ in tags):
                    raise self.err(f"discriminated union: tag {v!r} is in two members", node, module)
                tags.append((v, td))
        if len(keys) != 1:
            raise self.err(f"discriminated union: the members' {field!r} fields have different aliases", node, module)
        entries = ", ".join(f"({rs(v)}, &{td})" for v, td in tags)
        return (f"{RT}::pyd::TD::Tagged(&{RT}::pyd::Tagged {{ key: {rs(keys.pop())}, attr: {rs(field)}, "
                f"tags: &[{entries}], func: None, custom: {self._custom(custom)} }})")

    @staticmethod
    def _custom(custom) -> str:
        if custom is None:
            return "None"
        t, m, ctx = custom
        return f"Some(({rs(t)}, {rs(m)}, &[{', '.join(f'({rs(k)}, {rs(v)})' for k, v in ctx.items())}]))"

    def _scalar(self, name: str, cons: dict, ann: ast.AST, module: str) -> str:
        if any(isinstance(v, __import__("datetime").date) for v in cons.values()):
            raise self.err(f"a datetime bound does not apply to `{name}`", ann, module)
        if name == "int":
            return f"{RT}::pyd::TD::Int({self._num(cons)})"
        if name == "float":
            return f"{RT}::pyd::TD::Float({self._num(cons)})"
        if name == "str":
            if bad := [k for k in ("multiple_of", "ge", "gt", "le", "lt") if k in cons]:
                # pydantic raises TypeError when it builds the schema ("Unable to apply constraint")
                raise self.err(f"{bad[0]}= does not apply to `str`", ann, module)
            return f"{RT}::pyd::TD::Str({self._str(cons)})"
        if name == "bool":
            return f"{RT}::pyd::TD::Bool"
        if name in {"dict", "Dict"}:
            return self._sized(f"{RT}::pyd::TD::Dict(None)", cons, ann, module)
        if name in {"list", "List"}:
            return self._sized(f"{RT}::pyd::TD::List(None)", cons, ann, module)
        if name in {"set", "Set"}:
            return self._sized(f"{RT}::pyd::TD::Set(None)", cons, ann, module)
        if name in {"tuple", "Tuple"}:
            return self._sized(f"{RT}::pyd::TD::Tuple(None)", cons, ann, module)
        if name in {"Any", "object"}:
            return f"{RT}::pyd::TD::Any"
        if name == "bytes":
            bad = [k for k in cons if not k.startswith("_") and k not in ("min_length", "max_length")]
            if bad:
                raise self.err(f"Field({bad[0]}=...) on `bytes` is not supported (min_length/max_length only)", ann, module)
            mn = f"Some({cons['min_length']})" if "min_length" in cons else "None"
            mx = f"Some({cons['max_length']})" if "max_length" in cons else "None"
            mode = {"base64": "Base64", "hex": "Hex"}.get(self.__dict__.get("_val_bytes") or "", "Utf8")
            return f"{RT}::pyd::TD::Bytes({mn}, {mx}, {RT}::pyd::BytesMode::{mode})"
        raise TranspileError(f"unsupported type {name}")

    FIELD_KW = {"ge", "gt", "le", "lt", "min_length", "max_length", "pattern", "regex", "max_digits", "decimal_places",
                "multiple_of"}
    IGNORED_KW = {"description", "title", "examples", "example", "json_schema_extra", "deprecated", "include_in_schema",
                  "openapi_examples"}
    # pydantic 2's `Field()` parameters: any other keyword is an extra, put in `json_schema_extra` (deprecated)
    PYD_FIELD_PARAMS = {"default", "default_factory", "alias", "alias_priority", "validation_alias", "serialization_alias",
                        "title", "field_title_generator", "description", "examples", "exclude", "exclude_if",
                        "discriminator", "deprecated", "json_schema_extra", "frozen", "validate_default", "repr", "init",
                        "init_var", "kw_only", "pattern", "strict", "coerce_numbers_to_str", "gt", "ge", "lt", "le",
                        "multiple_of", "allow_inf_nan", "max_digits", "decimal_places", "min_length", "max_length",
                        "union_mode", "fail_fast", "regex", "min_items", "max_items", "const", "unique_items"}

    def pyd_field_call(self, call: ast.Call, module: str) -> bool:
        """`call` is pydantic's `Field()` (not FastAPI's Query/Body...)"""
        t = self.resolve(module, call.func)
        return isinstance(t, Ext) and t.dotted in {"pydantic.Field", "pydantic.fields.Field", "sqlmodel.Field"}

    def field_cons(self, call: ast.Call, module: str, scope=None) -> tuple[dict, dict]:
        """Field()/Query()/Path()/Body()/Header() -> (constraints, options)."""
        cons, opts = {}, {}
        if call.args:
            a0 = call.args[0]
            if not (isinstance(a0, ast.Constant) and a0.value is Ellipsis):
                opts["default"] = a0
        is_field = (dotted(call.func) or "").split(".")[-1] == "Field"
        for kw in call.keywords:
            if kw.arg in {"gt", "ge", "lt", "le", "min_length", "max_length"} and self.settings_value(kw.value, module, scope):
                # `le=settings.MAX_PAGE`: read when the application starts (late_bounds)
                cons[kw.arg] = LateCons(kw.value, module)
            elif kw.arg in {"gt", "ge", "lt", "le"}:
                cons[kw.arg] = self.const_bound(kw.value, module, scope)
            elif kw.arg in self.FIELD_KW:
                cons["pattern" if kw.arg == "regex" else kw.arg] = self.const(kw.value, module, scope)
            elif kw.arg in {"min_items", "max_items"} and is_field:
                # pydantic 2 `Field()`: deprecated aliases, used only when min_length/max_length is not given
                v = self.const(kw.value, module, scope)
                if v is not None:
                    cons.setdefault("_" + kw.arg, v)
            elif kw.arg == "default":
                if not (isinstance(kw.value, ast.Constant) and kw.value.value is Ellipsis):
                    opts["default"] = kw.value
            elif kw.arg in {"default_factory", "alias", "embed", "convert_underscores", "validation_alias", "serialization_alias",
                            "validate_default"}:
                opts[kw.arg] = kw.value
            elif kw.arg in {"repr", "exclude", "frozen", "union_mode", "strict", "exclude_if"} and is_field:
                opts[kw.arg] = kw.value
            elif kw.arg in self.IGNORED_KW:
                pass
            elif kw.arg == "discriminator" and is_field:
                cons["_disc"] = (kw.value, module)  # the union below it is discriminated (`_union`)
            elif is_field and kw.arg is not None and kw.arg not in self.PYD_FIELD_PARAMS and self.pyd_field_call(call, module):
                pass  # `Field(True, nullable=True)`: pydantic's json_schema_extra (the schema only, as json_schema_extra)
            else:
                raise self.err(f"unsupported option {kw.arg}= in {dotted(call.func)}()", kw, module)
        for k in ("min", "max"):
            if f"_{k}_items" in cons:
                cons.setdefault(f"{k}_length", cons.pop(f"_{k}_items"))
        return cons, opts

    # what Annotated[T, ...] metadata may be: FastAPI/pydantic parameter and field markers (their constraints
    # apply), documentation only (no effect on validation), or constraints the runtime implements
    ANNOTATED_FIELDS = {"Field", "Query", "Path", "Body", "Header", "Cookie", "Form", "File"}
    ANNOTATED_DOC = {"typing_extensions.Doc", "typing.Doc", "annotated_doc.Doc", "fastapi.Doc",
                     "pydantic.WithJsonSchema", "pydantic.json_schema.WithJsonSchema",
                     "pydantic.json_schema.Examples", "pydantic.Examples"}
    ANNOTATED_TYPES = {"Gt": "gt", "Ge": "ge", "Lt": "lt", "Le": "le", "MinLen": "min_length", "MaxLen": "max_length"}
    STRING_CONSTRAINTS = {"strip_whitespace": "_strip", "to_upper": "_upper", "to_lower": "_lower",
                          "min_length": "min_length", "max_length": "max_length", "pattern": "pattern", "ascii_only": "_ascii"}

    def annotated_meta(self, meta: ast.AST, module: str, scope=None) -> dict:
        """The constraints one `Annotated[T, meta]` metadata adds ({} when it changes nothing); anything else
        (AfterValidator, a class with `__get_pydantic_core_schema__`, Discriminator, Strict...) is refused rather
        than ignored: pydantic would validate or serialise differently."""
        if isinstance(meta, ast.Constant) and isinstance(meta.value, str):
            return {}
        func = meta.func if isinstance(meta, ast.Call) else meta
        t = self.resolve(module, func, scope)
        name = t.dotted if isinstance(t, Ext) else (dotted(func) or "")
        last = name.split(".")[-1]
        if isinstance(meta, ast.Call) and last in self.ANNOTATED_FIELDS and not isinstance(t, Sym):
            return self.field_cons(meta, module, scope)[0]
        if last in {"Depends", "Security"} and not isinstance(t, Sym):
            return {}
        if isinstance(t, Ext) and t.dotted.startswith("pydantic") and last == "Tag":
            return {}  # a member's tag: read by the discriminated union around it
        if isinstance(t, Ext) and t.dotted.startswith("pydantic") and last == "Discriminator" and isinstance(meta, ast.Call):
            return {"_disc": (meta, module)}
        if isinstance(t, Ext) and t.dotted in self.ANNOTATED_DOC:
            return {}
        if isinstance(t, Ext) and t.dotted.startswith("annotated_types.") and isinstance(meta, ast.Call):
            if last in self.ANNOTATED_TYPES and len(meta.args) == 1 and not meta.keywords:
                key = self.ANNOTATED_TYPES[last]
                if key in {"gt", "ge", "lt", "le"}:
                    return {key: self.const_bound(meta.args[0], module, scope)}
                return {key: self.const(meta.args[0], module, scope)}
            if last in {"Len", "Interval"} and not meta.args:
                keys = {"Len": {"min_length", "max_length"}, "Interval": {"gt", "ge", "lt", "le"}}[last]
                if all(k.arg in keys for k in meta.keywords):
                    return {k.arg: self.const(k.value, module, scope) for k in meta.keywords
                            if self.const(k.value, module, scope) is not None}
        if (isinstance(t, Ext) and last == "StringConstraints" and isinstance(meta, ast.Call) and not meta.args
                and all(k.arg in self.STRING_CONSTRAINTS for k in meta.keywords)):
            out = {}
            for k in meta.keywords:
                v = self.const(k.value, module, scope)
                if v is not None and v is not False:
                    out[self.STRING_CONSTRAINTS[k.arg]] = v
            return out
        raise self.err(f"`{ast.unparse(meta)}` in Annotated[...] is not supported (pydantic would validate or "
                       "serialise the field differently)", meta, module)

    def annotated_opts(self, ann, module: str) -> dict:
        """Field options (default, alias, validate_default...) of a top-level `Annotated[T, Field(...)]`: they
        apply to the field, like the ones of an assigned `Field(...)` (which win)."""
        if not (isinstance(ann, ast.Subscript) and (dotted(ann.value) or "").split(".")[-1] == "Annotated"):
            return {}
        args = ann.slice.elts if isinstance(ann.slice, ast.Tuple) else [ann.slice]
        opts = {}
        for meta in args[1:]:
            if isinstance(meta, ast.Call) and (dotted(meta.func) or "").split(".")[-1] == "Field":
                opts.update(self.field_cons(meta, module)[1])
        return opts

    def dflt(self, node: ast.AST, module: str, factory: bool = False) -> str:
        """A `fn() -> V` producing a default value (a literal, or a factory such as `list`)."""
        if factory:
            name = dotted(node)
            body = {"list": "V::list(vec![])", "dict": "V::empty_dict()", "set": f"{RT}::methods::b_set(&[]).unwrap()"}.get(name or "")
            if body is None:
                raise self.err(f"unsupported default_factory `{ast.unparse(node)}`", node, module)
        else:
            body = self.enum_member(node, module) or self.lit_rust(node, module)
        if body not in self.dflts:
            self.dflts[body] = f"dflt_{len(self.dflts) + 1}"
        return self.dflts[body]

    def dyn_default(self, node: ast.AST, module: str, mode: str) -> str:
        """A `MethodFn` computing a default: mode "call" (node is a callable, called each time),
        "once" (an expression evaluated once, like at import) or "expr" (evaluated each time)."""
        n = self.uid()
        name = f"dyn_dflt_{n}"
        expr_node = ast.Call(func=node, args=[], keywords=[]) if mode == "call" else node
        ast.copy_location(expr_node, node)
        fc = FnCompiler(self, module, None, name)
        if mode == "data":
            f = fc.expr(node)
            self.items.append(
                f"fn {name}<'a>(cx: &'a Cx, _s: V, a: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{ "
                f"{' '.join(fc.lines)} let __f = {f}; {RT}::methods::call_value(cx, &__f, a, vec![]).await }}) }}"
            )
            return name
        code = fc.expr(expr_node)
        if mode == "once":
            g = f"G_{name}"
            self.items.append(
                f"static {g}: {RT}::Global = {RT}::Global::new();\n"
                f"fn {name}<'a>(cx: &'a Cx, _s: V, _a: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{ "
                f"{RT}::methods::default_copy(&{g}.get(cx, |cx| Box::pin(async move {{ Ok({code}) }})).await?) }}) }}"
            )
        else:
            self.items.append(
                f"fn {name}<'a>(cx: &'a Cx, _s: V, _a: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{ Ok({code}) }}) }}"
            )
        return name

    def default_spec(self, node: ast.AST, module: str, factory: bool) -> str:
        """`Dflt::...` variant text for a Pydantic field/param default."""
        if factory and self.takes_data(node, module):
            # pydantic passes the validated data to a factory with one positional parameter (and, since 2.14,
            # reports `default_factory_not_called` after an error of another field)
            return f"Data({self.dyn_default(node, module, 'data')})"
        try:
            return f"{'Factory' if factory else 'Value'}({self.dflt(node, module, factory=factory)})"
        except TranspileError:
            return f"Dyn({self.dyn_default(node, module, 'call' if factory else 'once')})"

    def takes_data(self, node: ast.AST, module: str) -> bool:
        """A default factory with a required parameter: pydantic calls it with the data validated so far."""
        fn = node
        if not isinstance(node, ast.Lambda):
            t = self.resolve(module, node)
            fn = self.ix.definition(t) if isinstance(t, Sym) else None
        if not isinstance(fn, (ast.Lambda, ast.FunctionDef, ast.AsyncFunctionDef)):
            return False
        a = fn.args
        return len(a.posonlyargs + a.args) - len(a.defaults) > 0

    def lit_rust(self, node: ast.AST, module: str) -> str:
        v = self.const(node, module)
        return self.py_to_rust(v, node, module)

    def py_to_rust(self, v, node, module) -> str:
        if v is None:
            return "V::None"
        if isinstance(v, bool):
            return f"V::Bool({str(v).lower()})"
        if isinstance(v, int):
            return f"V::Int({v})"
        if isinstance(v, float):
            return f"V::Float({v!r}f64)"
        if isinstance(v, str):
            return f"V::str({rs(v)})"
        if isinstance(v, (list, tuple)):
            items = ", ".join(self.py_to_rust(x, node, module) for x in v)
            return f"V::{'list' if isinstance(v, list) else 'tuple'}(vec![{items}])"
        if isinstance(v, dict):
            items = ", ".join(f"({self.py_to_rust(k, node, module)}, {self.py_to_rust(x, node, module)})" for k, x in v.items())
            return f"V::dict_from(vec![{items}]).unwrap()"
        raise self.err(f"unsupported default value {v!r}", node, module)

    # ---------------------------------------------------------------- classes

    def class_static(self, sym: Sym) -> str:
        """The `static CLS_x: Class` of a project class (model, schema, enum, exception)."""
        if sym in self.fe.model_syms:
            return self.model(sym).cls
        if sym in self.fe.schema_syms:
            return self.schema(sym).cls
        if sym in self.fe.dataclass_syms:
            return self.dataclass(sym).cls
        if self.is_plain_class(sym):
            return self.plain_class(sym).cls
        if self.enum_kind(sym):
            return self.enum(sym)[1]
        return self.exception(sym)

    ENUM_BASES = {"enum.Enum": "Plain", "enum.StrEnum": "StrEnum", "enum.IntEnum": "IntEnum",
                  "enum.Flag": None, "enum.IntFlag": None}

    def enum_kind(self, sym: Sym) -> str | None:
        node = self.ix.definition(sym)
        if not isinstance(node, ast.ClassDef):
            return None
        base_kind, mixin = None, None
        for b in node.bases:
            t = self.resolve(sym.module, b)
            if isinstance(t, Ext) and t.dotted in self.ENUM_BASES:
                base_kind = self.ENUM_BASES[t.dotted] or "unsupported"
            elif isinstance(t, Sym) and self.enum_kind(t):
                if not self.memberless_enum(t):
                    raise self.err(f"enum {sym.name}: subclassing an enum is not supported (only an enum without "
                                   "members or methods, as a common base)", b, sym.module)
                # `class AppEnum(StrEnum): pass` then `class Status(AppEnum)`: Python allows deriving from an enum
                # without members; the subclass is an enum of the base's kind
                base_kind = self.enum_kind(t)
                continue
            elif dotted(b) in {"str", "int"}:
                mixin = dotted(b)
        if base_kind is None:
            return None
        if base_kind == "unsupported":
            raise self.err(f"enum {sym.name}: Flag/IntFlag enums are not supported", node, sym.module)
        if base_kind == "Plain" and mixin:
            return "Str" if mixin == "str" else "Int"
        return base_kind

    def memberless_enum(self, sym: Sym) -> bool:
        """a project enum class whose body is only a docstring and `pass`/`...` (a base for other enums)"""
        node = self.ix.definition(sym)
        return isinstance(node, ast.ClassDef) and not node.keywords and not node.decorator_list and all(
            isinstance(st, ast.Pass) or (isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant)) for st in node.body)

    def enum(self, sym: Sym) -> tuple[str, str, list[str]]:
        if sym in self.enums:
            return self.enums[sym]
        node = self.ix.definition(sym)
        kind = self.enum_kind(sym)
        base = f"{mod_ident(sym.module)}__{ident(sym.name)}"
        names, values = [], []
        methods = []
        auto_n = 0
        for stmt in node.body:
            if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
                n = stmt.targets[0].id
                if n.startswith("_"):
                    continue
                v = stmt.value
                if isinstance(v, ast.Call) and (dotted(v.func) or "").split(".")[-1] == "auto":
                    auto_n += 1
                    val = n.lower() if kind == "StrEnum" else auto_n
                else:
                    val = self.const(v, sym.module)
                names.append(n)
                values.append(val)
            elif isinstance(stmt, (ast.Pass, ast.Expr)):
                continue
            elif isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                decos = [((dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "").split(".")[-1] for d in stmt.decorator_list]
                bad = [d for d, n in zip(stmt.decorator_list, decos) if n not in {"staticmethod", "classmethod", "property"}]
                if bad:
                    raise self.err(f"enum {sym.name}.{stmt.name}: decorator @{ast.unparse(bad[0])} is not supported", bad[0], sym.module)
                if stmt.name == "_missing_" and "classmethod" not in decos:
                    raise self.err(f"enum {sym.name}: _missing_ must be a classmethod", stmt, sym.module)
                if stmt.name.startswith("__") and stmt.name.endswith("__"):
                    raise self.err(f"enum {sym.name}: method {stmt.name} is not supported", stmt, sym.module)
                methods.append((stmt.name, "property" in decos))
            else:
                raise self.err(f"enum {sym.name}: unsupported statement", stmt, sym.module)
        info = (f"ENUM_{base}", f"CLS_{base}", names)
        self.enums[sym] = info
        wrappers = [(n, prop, method_wrapper(self, Sym(sym.module, f"{sym.name}.{n}"))) for n, prop in methods]
        missing = next((w for n, _, w in wrappers if n == "_missing_"), None)
        self.__dict__.setdefault("enum_missing", {})[sym] = missing is not None
        meths = ", ".join(f"({rs(n)}, {str(prop).lower()}, {w})" for n, prop, w in wrappers if n != "_missing_")
        def ev(v, top: bool) -> str:
            if isinstance(v, tuple) and top and kind in {"Str", "StrEnum", "Int", "IntEnum"}:
                # a str/int mixin calls the type with the tuple as arguments: `("A",)` is "A"
                want = str if kind in {"Str", "StrEnum"} else int
                if len(v) != 1 or type(v[0]) is not want:
                    raise self.err(f"enum {sym.name}: member value {v!r} is not supported ({want.__name__}(*value) "
                                   "with one argument only)", node, sym.module)
                v = v[0]
            if isinstance(v, bool):
                return f"{RT}::v::EV::Bool({str(v).lower()})"
            if isinstance(v, int):
                return f"{RT}::v::EV::Int({v})"
            if isinstance(v, float):
                return f"{RT}::v::EV::Float({v!r}f64)"
            if isinstance(v, str):
                return f"{RT}::v::EV::Str({rs(v)})"
            if v is None:
                return f"{RT}::v::EV::None"
            if isinstance(v, (tuple, list)) and kind == "Plain":
                return f"{RT}::v::EV::{type(v).__name__.title()}(&[{', '.join(ev(x, False) for x in v)}])"
            raise self.err(f"enum {sym.name}: unsupported member value {v!r}", node, sym.module)
        evs = [ev(v, True) for v in values]
        members = ", ".join(f"({rs(n)}, {e})" for n, e in zip(names, evs))
        self.items.append(
            f"pub static {info[1]}: {RT}::v::Class = {RT}::v::Class {{ name: {rs(sym.name)}, qualname: {rs(sym.qual)}, "
            f"bases: &[], kind: {RT}::v::ClassKind::Enum(&{info[0]}) }};\n"
            f"pub static {info[0]}: {RT}::v::EnumDesc = {RT}::v::EnumDesc {{ name: {rs(sym.name)}, class: &{info[1]}, "
            f"kind: {RT}::v::EnumKind::{kind}, members: &[{members}], methods: &[{meths}], "
            f"missing: {f'Some({missing})' if missing else 'None'} }};"
        )
        return info

    def enum_member(self, node: ast.AST, module: str, scope=None) -> str | None:
        """`Status.PENDING` -> `V::Enum(&ENUM_x, i)` when it names an enum member."""
        if isinstance(node, ast.Attribute):
            t = self.resolve(module, node.value, scope)
            if isinstance(t, Sym) and self.enum_kind(t):
                rust, _, names = self.enum(t)
                if node.attr in names:
                    return f"V::Enum(&{rust}, {names.index(node.attr)})"
        return None

    def enum_col(self, sym: Sym, by_value: bool, pg_type: str | None) -> str:
        rust = self.enum(sym)[0]
        key = f"{rust}|{by_value}|{pg_type}"
        if key not in self.enum_cols:
            name = f"ENUMCOL_{len(self.enum_cols) + 1}"
            self.enum_cols[key] = name
            pt = f"Some({rs(pg_type)})" if pg_type else "None"
            self.items.append(
                f"static {name}: {RT}::orm::EnumCol = {RT}::orm::EnumCol {{ desc: &{rust}, by_value: {str(by_value).lower()}, pg_type: {pt} }};"
            )
        return self.enum_cols[key]

    def exception(self, sym: Sym) -> str:
        if sym in self.exc_classes:
            return self.exc_classes[sym]
        node = self.ix.definition(sym)
        if not isinstance(node, ast.ClassDef):
            raise TranspileError(f"{sym.qual} is not a class")
        for k in node.keywords:
            # `metaclass=ABCMeta` is read on plain classes only (is_plain_class); any other keyword changes how the
            # class is built (a metaclass's __new__/__call__, __init_subclass__ arguments)
            raise self.err(f"class {sym.name}: the class keyword `{ast.unparse(k)}` is not supported (only "
                           "metaclass=ABCMeta, on a plain class)", k.value, sym.module)
        for d in node.decorator_list:
            raise self.err(f"class {sym.name}: decorator @{ast.unparse(d)} is not supported", d, sym.module)
        name = f"CLS_{mod_ident(sym.module)}__{ident(sym.name)}"
        self.exc_classes[sym] = name
        bases = []
        for b in node.bases:
            t = self.resolve(sym.module, b)
            if isinstance(t, Sym) and self.is_plain_class(t):
                raise self.err(f"class {sym.name}: only exception classes are supported (base `{ast.unparse(b)}`: "
                               "a plain class; multiple inheritance of plain classes is not supported)", b, sym.module)
            if isinstance(t, Sym):
                bases.append(f"&{self.exception(t)}")
            elif isinstance(t, Ext) and ("builtins." + t.dotted.split(".")[-1]) in libmap.EXCEPTIONS and t.dotted.count(".") == 0 or (
                isinstance(t, Ext) and t.dotted in libmap.EXCEPTIONS
            ):
                key = t.dotted if t.dotted in libmap.EXCEPTIONS else "builtins." + t.dotted
                bases.append(f"&{RT}::v::{libmap.EXCEPTIONS[key]}")
            elif isinstance(b, ast.Name) and b.id in libmap.BUILTIN_EXC_NAMES:
                bases.append(f"&{RT}::v::{libmap.BUILTIN_EXC_NAMES[b.id]}")
            else:
                raise self.err(f"class {sym.name}: only exception classes are supported (base `{ast.unparse(b)}`)", b, sym.module)
        attrs, methods = [], []
        for stmt in node.body:
            if isinstance(stmt, ast.FunctionDef) and stmt.name == "__init__" and not stmt.decorator_list:
                self.__dict__.setdefault("exc_inits", {})[sym] = Sym(sym.module, f"{sym.name}.__init__")
            elif isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                if stmt.name in COPY_DUNDERS and not self.copies_objects():
                    self.__dict__.setdefault("copy_ignored", []).append((sym, stmt, sym.module))
                    continue
                if is_dunder(stmt.name) and (stmt.name not in EXC_DUNDERS or stmt.decorator_list):
                    raise self.err(f"exception class {sym.name}: method {stmt.name} is not supported", stmt, sym.module)
                last = [((dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "").split(".")[-1] for d in stmt.decorator_list]
                if any(x != "property" for x in last):
                    raise self.err(f"exception class {sym.name}.{stmt.name}: only @property is supported", stmt, sym.module)
                methods.append((stmt.name, bool(last), Sym(sym.module, f"{sym.name}.{stmt.name}")))
            elif isinstance(stmt, (ast.Assign, ast.AnnAssign)):
                targets = stmt.targets if isinstance(stmt, ast.Assign) else [stmt.target]
                if len(targets) != 1 or not isinstance(targets[0], ast.Name):
                    raise self.err(f"exception class {sym.name}: unsupported class attribute", stmt, sym.module)
                if stmt.value is None:
                    continue  # annotation only
                attrs.append((targets[0].id, self.exc_attr(sym, targets[0].id, stmt)))
            elif not isinstance(stmt, (ast.Pass, ast.Expr)):
                raise self.err(f"exception class {sym.name}: unsupported statement in its body", stmt, sym.module)
        meths = ", ".join(f"({rs(n)}, {str(prop).lower()}, {method_wrapper(self, s)} as {RT}::pyd::MethodFn)" for n, prop, s in methods)
        cattrs = ", ".join(f"({rs(n)}, {g} as {RT}::v::ClsAttr)" for n, g in attrs)
        self.items.append(
            f"static EXC_{name}: {RT}::v::ExcDesc = {RT}::v::ExcDesc {{ attrs: &[{cattrs}], methods: &[{meths}], "
            f"async_methods: {async_names(self, methods)}, module: {rs(sym.module)} }};\n"
            f"pub static {name}: {RT}::v::Class = {RT}::v::Class {{ name: {rs(sym.name)}, qualname: {rs(sym.qual)}, "
            f"bases: &[{', '.join(bases)}], kind: {RT}::v::ClassKind::UserException(&EXC_{name}) }};"
        )
        return name

    HTTP_EXCS = {"fastapi.HTTPException", "fastapi.exceptions.HTTPException", "starlette.exceptions.HTTPException"}

    def http_exc_base(self, sym: Sym) -> bool:
        """Is the nearest library base of this project class (first bases) HTTPException?"""
        seen = set()
        while isinstance(sym, Sym) and sym not in seen:
            seen.add(sym)
            d = self.ix.definition(sym)
            if not isinstance(d, ast.ClassDef) or not d.bases:
                return False
            t = self.resolve(sym.module, d.bases[0])
            if isinstance(t, Ext):
                return t.dotted in self.HTTP_EXCS
            if isinstance(t, Sym) and self.exc_init(t) is not None:
                return False  # its own __init__ runs instead
            sym = t
        return False

    def exc_attr(self, sym: Sym, attr: str, stmt) -> str:
        """A class attribute of an exception class: evaluated once, at startup (the class body runs at import)."""
        name = f"ca_{mod_ident(sym.module)}__{ident(sym.name)}__{ident(attr)}"
        fc = FnCompiler(self, sym.module, None, name)
        code = fc.expr(stmt.value)
        self.__dict__.setdefault("eager", []).append((sym.module, stmt.lineno, name))
        body = "\n".join("        " + l for l in fc.lines)
        self.items.append(
            f"static G_{name}: {RT}::Global = {RT}::Global::new();\n"
            f"pub fn {name}(cx: &Cx) -> {RT}::BoxFut<'_> {{ Box::pin(async move {{\n"
            f"    G_{name}.get(cx, |cx| Box::pin(async move {{\n{body}\n        Ok({code})\n    }})).await\n}}) }}"
        )
        return name

    def model(self, sym: Sym) -> ModelInfo:
        # the function being compiled uses this mapped class: an error of its relationships (resolved after the
        # functions, Project.drain) blocks it, as it blocks the generation
        self.class_edge(sym)
        if sym in self.class_errors:
            raise self.class_errors[sym]
        if sym in self.models:
            return self.models[sym]
        try:
            info = self._model(sym)
        except TranspileError as e:
            self.models.pop(sym, None)
            self.class_errors[sym] = e
            raise
        if info.poly_on is not None:
            # a row of the hierarchy can be of any mapped subclass: all of them are translated with it
            for s2, n2 in self.fe.model_syms.items():
                if s2 not in self.models and any(self.resolve(s2.module, b) == sym for b in n2.bases):
                    self.model(s2)
        return info

    def _model(self, sym: Sym) -> ModelInfo:
        node = self.fe.model_syms[sym]
        module = sym.module
        base = f"{mod_ident(module)}__{ident(sym.name)}"
        info = ModelInfo(sym, f"MODEL_{base}", f"CLS_{base}", "", [], -1, [])
        self.models[sym] = info
        cols: list[dict] = []
        table = None
        hybrids: dict[str, dict] = {}
        info.__dict__["hybrids"] = hybrids
        comps: list[tuple] = []  # composite() attributes, resolved once every column is known
        info.__dict__["composites"] = []
        classes = [(sym, node)]
        parent = None
        joined = None  # the subclass's own __tablename__ (joined table inheritance)
        for b in node.bases:  # mixins: plain classes holding columns
            t = self.resolve(module, b)
            if isinstance(t, Sym) and t not in self.fe._declarative and t not in self.fe.model_syms:
                bn = self.ix.definition(t)
                if isinstance(bn, ast.ClassDef):
                    classes.insert(0, (t, bn))
            elif isinstance(t, Sym) and t in self.fe.model_syms:
                if parent is not None:
                    raise self.err(f"model {sym.name}: more than one mapped base class is not supported", b, module)
                parent = self.model(t)
        if parent is not None:
            # inheritance: the subclass starts with everything its mapped base has (columns in the same positions)
            if parent.poly_on is None:
                raise self.err(f"model {sym.name}: inheritance needs polymorphic_on in the __mapper_args__ of "
                               f"{parent.sym.name}", node, module)
            info.parent, info.poly_on = parent.sym, parent.poly_on
            info.joins = list(parent.joins)
            cols = [dict(c) for c in parent.cols]
            for c in cols:
                c.setdefault("table", parent.table)  # each inherited column keeps the table it lives in
            table = parent.table
            hybrids.update(parent.__dict__.get("hybrids", {}))
            info.methods = list(parent.methods)
            info.rels_raw = list(parent.rels_decl)
            info.rels_decl = list(parent.rels_decl)
            info.__dict__["inherited_rels"] = {r[0] for r in parent.rels_decl}
            info.eager_defaults = parent.eager_defaults
        for csym, cnode in classes:
            for stmt in cnode.body:
                if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
                    n = stmt.targets[0].id
                    if n == "__tablename__":
                        table = literal(stmt.value, self.src(csym.module))
                        if parent is not None and csym == sym:
                            joined = stmt
                    elif n == "__abstract__":
                        raise self.err(f"model {sym.name}: abstract models are not supported", stmt, csym.module)
                    elif n == "__mapper_args__":
                        d = stmt.value
                        keys = [literal(k, self.src(csym.module)) if isinstance(k, ast.Constant) else None
                                for k in getattr(d, "keys", [])]
                        allowed = {"eager_defaults", "polymorphic_identity", "polymorphic_on", "polymorphic_load"}
                        if not isinstance(d, ast.Dict) or any(k not in allowed for k in keys):
                            bad = next((k for k in keys if k not in allowed), None)
                            raise self.err(f"model {sym.name}: __mapper_args__ key {bad!r} is not supported (eager_defaults, "
                                           "polymorphic_on, polymorphic_identity and polymorphic_load only)", stmt, csym.module)
                        for k, v in zip(keys, d.values):
                            if k == "eager_defaults":
                                ev = literal(v, self.src(csym.module))
                                if ev not in (True, False, "auto"):
                                    raise self.err(f"model {sym.name}: eager_defaults={ev!r} is not supported", v, csym.module)
                                info.eager_defaults = ev is True
                            elif k == "polymorphic_load":
                                ev = literal(v, self.src(csym.module))
                                if ev not in ("selectin", "inline") or parent is None:
                                    raise self.err(f"model {sym.name}: polymorphic_load={ev!r} is not supported (\"selectin\" "
                                                   "or \"inline\" on a subclass)", v, csym.module)
                                # both load the subclass's columns with a query on the base (one SELECT or one join)
                                info.poly_load = True
                            elif k == "polymorphic_identity":
                                ev = literal(v, self.src(csym.module))
                                if not isinstance(ev, str):
                                    raise self.err(f"model {sym.name}: polymorphic_identity={ev!r} is not supported (a string)",
                                                   v, csym.module)
                                info.identity = ev
                            else:
                                if parent is not None:
                                    raise self.err(f"model {sym.name}: polymorphic_on on a subclass is not supported", v,
                                                   csym.module)
                                if isinstance(v, ast.Constant) and isinstance(v.value, str):
                                    info.poly_on = v.value
                                elif isinstance(v, ast.Name):
                                    info.poly_on = v.id
                                else:
                                    raise self.err(f"model {sym.name}: polymorphic_on must name a column of the class "
                                                   "(its attribute name, or the column itself)", v, csym.module)
                    elif n == "__table_args__":
                        # constraints and indexes only matter to the DDL (Alembic/create_all); a schema does not
                        for x in ast.walk(stmt.value):
                            if isinstance(x, ast.Dict) and any(isinstance(k, ast.Constant) and k.value == "schema" for k in x.keys):
                                raise self.err(f"model {sym.name}: __table_args__ schema is not supported", stmt, csym.module)
                    elif isinstance(stmt.value, ast.Call) and (dotted(stmt.value.func) or "").split(".")[-1] == "composite":
                        comps.append((n, None, stmt.value, csym.module, stmt))
                        cols.extend(self.composite_columns(sym, n, None, stmt.value, csym.module, stmt))
                    elif isinstance(stmt.value, ast.Call) and (dotted(stmt.value.func) or "").split(".")[-1] in {"Column", "mapped_column", "deferred"}:
                        cols.append(self.column(n, None, stmt.value, csym.module, stmt, self.tam_of(sym)))
                    elif isinstance(stmt.value, ast.Call) and (dotted(stmt.value.func) or "").split(".")[-1] == "relationship":
                        info.rels_raw.append((n, None, stmt.value, csym.module))
                        info.rels_decl.append(info.rels_raw[-1])
                    elif not n.startswith("__"):
                        raise self.err(f"model {sym.name}.{n}: only columns and relationships are supported in a "
                                       f"mapped class (`{ast.unparse(stmt.value)[:40]}`)", stmt, csym.module)
                elif isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
                    n = stmt.target.id
                    if isinstance(stmt.value, ast.Call) and (dotted(stmt.value.func) or "").split(".")[-1] == "relationship":
                        info.rels_raw.append((n, stmt.annotation, stmt.value, csym.module))
                        info.rels_decl.append(info.rels_raw[-1])
                        continue
                    if n.startswith("__"):
                        continue
                    if isinstance(stmt.value, ast.Call) and (dotted(stmt.value.func) or "").split(".")[-1] == "composite":
                        comps.append((n, stmt.annotation, stmt.value, csym.module, stmt))
                        cols.extend(self.composite_columns(sym, n, stmt.annotation, stmt.value, csym.module, stmt))
                        continue
                    if (dotted(stmt.annotation) or "").split(".")[-1] == "ClassVar" or (
                            isinstance(stmt.annotation, ast.Subscript) and (dotted(stmt.annotation.value) or "").split(".")[-1] == "ClassVar"):
                        raise self.err(f"model {sym.name}.{n}: class variables in a mapped class are not supported", stmt, csym.module)
                    cols.append(self.column(n, stmt.annotation, stmt.value, csym.module, stmt, self.tam_of(sym)))
                elif isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)) and self.hybrid_part(stmt, csym.module, sym.name, hybrids):
                    kind, target = self.hybrid_part(stmt, csym.module, sym.name, hybrids)
                    # each function of a hybrid compiled on its own (the classic style repeats the name)
                    nsym = Sym(csym.module, f"{cnode.name}.{stmt.name}~h{len(self.__dict__.setdefault('nested', {}))}")
                    self.nested[nsym] = (stmt, None)
                    self.function(nsym)  # compiled with the others (the model table is written last)
                    if kind in ("property", "method"):
                        hybrids[stmt.name] = {"name": stmt.name, "method": kind == "method", "get": nsym, "expr": None,
                                              "set": None}
                        info.methods = [m for m in info.methods if m[0] != stmt.name]
                        info.methods.append((stmt.name, kind == "property", nsym))
                    elif kind in ("expression", "comparator"):
                        # `.comparator`: its classmethod's Comparator object is what the class gives (SQLAlchemy
                        # sends the operators to it); one or the other, as SQLAlchemy says
                        if hybrids[target]["expr"] is not None:
                            raise self.err(f"model {sym.name}.{stmt.name}: a hybrid takes one @{target}.expression or "
                                           f"@{target}.comparator", stmt, csym.module)
                        hybrids[target]["expr"] = nsym
                    else:
                        hybrids[target]["set"] = nsym
                elif isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    if is_dunder(stmt.name) and stmt.name not in {"__str__", "__repr__", "__init__"}:
                        raise self.err(f"model {sym.name}: method {stmt.name} is not supported", stmt, csym.module)
                    if stmt.name == "__init__" and (stmt.decorator_list or isinstance(stmt, ast.AsyncFunctionDef)):
                        raise self.err(f"model {sym.name}: a decorated or async __init__ is not supported", stmt, csym.module)
                    for d in stmt.decorator_list:
                        if dotted(d) not in {"property", "staticmethod", "classmethod"}:
                            raise self.err(f"model {sym.name}.{stmt.name}: decorator @{ast.unparse(d)} is not supported "
                                           "(only @property, @staticmethod and @classmethod)", d, csym.module)
                    prop = any(dotted(d) == "property" for d in stmt.decorator_list)
                    info.methods.append((stmt.name, prop, Sym(csym.module, f"{cnode.name}.{stmt.name}")))
        if not table:
            raise self.err(f"model {sym.name} has no __tablename__", node, module)
        if info.poly_on is not None and not any(c["name"] == info.poly_on for c in cols):
            raise self.err(f"model {sym.name}: polymorphic_on={info.poly_on!r} is not a column of the class", node, module)
        if parent is not None and info.identity is None:
            raise self.err(f"model {sym.name}: a mapped subclass needs a polymorphic_identity", node, module)
        if joined is not None:
            # joined table inheritance: the subclass's key column (a foreign key to the parent's key) replaces the
            # inherited one in its position (`Manager.id` is manager.id in SQL); its other columns live in its table
            n_inh = len(parent.cols)
            own = cols[n_inh:]
            for c in own:
                c["table"] = table
            keep, linked = [], False
            for c in own:
                k = next((i for i, x in enumerate(cols[:n_inh]) if x["name"] == c["name"]), None)
                if k is None:
                    keep.append(c)
                    continue
                inh = cols[k]
                if not (c["pk"] and inh["pk"] and c.get("fk") == inh["table"]
                        and (c.get("fk_col") or inh.get("sql") or inh["name"]) == (inh.get("sql") or inh["name"])):
                    raise self.err(f"model {sym.name}: column {c['name']} redefined in a subclass is not supported (only "
                                   f"the key, as a foreign key to {inh['table']}.{inh.get('sql') or inh['name']})", joined,
                                   module)
                info.joins.append((table, c.get("sql") or c["name"], inh["table"], inh.get("sql") or inh["name"]))
                cols[k] = c
                linked = True
            if not linked:
                raise self.err(f"model {sym.name}: a joined subclass needs its primary key, a foreign key to the parent's "
                               f"key, under the same attribute name", joined, module)
            if len(parent.pks) != 1:
                raise self.err(f"model {sym.name}: joined inheritance with a composite key is not supported", joined, module)
            cols = cols[:n_inh] + keep
        if parent is not None and len({c["name"] for c in cols}) != len(cols):
            raise self.err(f"model {sym.name}: a column redefined in a subclass is not supported", node, module)
        pks = [i for i, c in enumerate(cols) if c["pk"]]
        if not pks:
            raise self.err(f"model {sym.name} has no primary key column", node, module)
        if len(pks) > 1:
            # SQLAlchemy's autoincrement="auto" only applies to a single-column integer key
            for i in pks:
                if not cols[i].get("autoincrement_set"):
                    cols[i]["autoincrement"] = False
        info.table, info.cols, info.pk, info.pks = table, cols, pks[0], pks
        info.fk_tables = sorted({c["fk"] for c in cols if c.get("fk")})
        for n, ann, call, cmod, stmt in comps:
            info.__dict__["composites"].append(self.composite(sym, n, ann, call, cmod, stmt, cols))
        return info

    COMPOSITE_KW = {"column_template"}

    def composite_parts(self, sym: Sym, name: str, ann, call: ast.Call, module: str):
        """`composite(...)`: (the class or callable building the value, None on all-NULL, its dataclass fields
        [(name, annotation)] or None, the column arguments)."""
        for k in call.keywords:
            if k.arg not in self.COMPOSITE_KW:
                raise self.err(f"composite {sym.name}.{name}: option {k.arg}= is not supported", k, module)
        args = list(call.args)
        colish = lambda a: (isinstance(a, ast.Call) and (dotted(a.func) or "").split(".")[-1] in {"Column", "mapped_column"}) \
            or (isinstance(a, ast.Constant) and isinstance(a.value, str))
        ctor, none = None, False
        if args and not colish(args[0]) and not (isinstance(args[0], ast.Name) and self._class_column_name(sym, args[0].id)):
            ctor = args.pop(0)
        inner = ann.slice if isinstance(ann, ast.Subscript) and (dotted(ann.value) or "").split(".")[-1] == "Mapped" else None
        if isinstance(inner, ast.Constant) and isinstance(inner.value, str):
            inner = ast.parse(inner.value, mode="eval").body
        if isinstance(inner, ast.BinOp) and isinstance(inner.op, ast.BitOr) and isinstance(inner.right, ast.Constant) and inner.right.value is None:
            inner, none = inner.left, True
        elif isinstance(inner, ast.Subscript) and (dotted(inner.value) or "").split(".")[-1] == "Optional":
            inner, none = inner.slice, True
        if ctor is None:
            ctor = inner
        if ctor is None:
            raise self.err(f"composite {sym.name}.{name}: the class is needed (Mapped[Cls] or composite(Cls, ...))", call, module)
        fields = None
        t = self.resolve(module, ctor) if isinstance(ctor, (ast.Name, ast.Attribute)) else None
        d = self.ix.definition(t) if isinstance(t, Sym) else None
        if isinstance(d, ast.ClassDef) and any((dotted(x.func if isinstance(x, ast.Call) else x) or "").split(".")[-1] == "dataclass"
                                               for x in d.decorator_list):
            fields = [(st.target.id, st.annotation) for st in d.body if isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name)
                      and "ClassVar" not in ast.unparse(st.annotation)]
        return ctor, none, fields, args

    def _class_column_name(self, sym: Sym, n: str) -> bool:
        node = self.fe.model_syms.get(sym)
        return node is not None and any(
            (isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name) and st.target.id == n) or
            (isinstance(st, ast.Assign) and any(isinstance(t, ast.Name) and t.id == n for t in st.targets)) for st in node.body)

    def composite_columns(self, sym: Sym, name: str, ann, call: ast.Call, module: str, stmt) -> list[dict]:
        """The columns a composite declares itself: `mapped_column("x1")` (typed by the dataclass field at its
        position when it names no type) and those of `column_template="home_%s"`."""
        ctor, _, fields, args = self.composite_parts(sym, name, ann, call, module)
        tpl = next((k.value for k in call.keywords if k.arg == "column_template"), None)
        out = []
        for k, a in enumerate(args):
            if isinstance(a, ast.Call) and (dotted(a.func) or "").split(".")[-1] in {"Column", "mapped_column"}:
                if not (a.args and isinstance(a.args[0], ast.Constant) and isinstance(a.args[0].value, str)):
                    raise self.err(f"composite {sym.name}.{name}: a column needs its name (mapped_column(\"x\"))", a, module)
                fann = None
                if len(a.args) == 1:
                    if fields is None or k >= len(fields):
                        raise self.err(f"composite {sym.name}.{name}: the column {a.args[0].value} needs a type (the class "
                                       "is not a dataclass with a field at its position)", a, module)
                    fann = ast.Subscript(value=ast.Name(id="Mapped", ctx=ast.Load()), slice=fields[k][1], ctx=ast.Load())
                out.append(self.column(a.args[0].value, fann, a, module, stmt, self.tam_of(sym)))
        if tpl is not None:
            if not (isinstance(tpl, ast.Constant) and isinstance(tpl.value, str) and tpl.value.count("%s") == 1) or fields is None:
                raise self.err(f"composite {sym.name}.{name}: column_template= needs a literal with one %s and a dataclass", call, module)
            for fname, fann in fields[len(args):]:
                col = ast.Call(func=ast.Name(id="mapped_column", ctx=ast.Load()), args=[ast.Constant(tpl.value % fname)], keywords=[])
                out.append(self.column(tpl.value % fname, ast.Subscript(value=ast.Name(id="Mapped", ctx=ast.Load()), slice=fann,
                                                                        ctx=ast.Load()), col, module, stmt, self.tam_of(sym)))
        return out

    def composite(self, sym: Sym, name: str, ann, call: ast.Call, module: str, stmt, cols: list[dict]) -> dict:
        ctor, none, fields, args = self.composite_parts(sym, name, ann, call, module)
        tpl = next((k.value.value for k in call.keywords if k.arg == "column_template"), None)
        names = []
        for a in args:
            if isinstance(a, ast.Call) and (dotted(a.func) or "").split(".")[-1] in {"Column", "mapped_column"}:
                names.append(a.args[0].value)
            elif isinstance(a, ast.Constant):
                names.append(a.value)
            elif isinstance(a, ast.Name):
                names.append(a.id)
            else:
                raise self.err(f"composite {sym.name}.{name}: `{ast.unparse(a)[:40]}` is not supported (columns, their "
                               "names or attribute names)", a, module)
        if tpl is not None:
            names += [tpl % f for f, _ in fields[len(args):]]
        idx = []
        for n in names:
            i = next((i for i, c in enumerate(cols) if c["name"] == n), None)
            if i is None:
                raise self.err(f"composite {sym.name}.{name}: no column {n}", call, module)
            idx.append(i)
        if isinstance(ctor, ast.Attribute):
            # `Cls.factory` (a classmethod or staticmethod): called directly, through a lambda of as many parameters
            ps = [f"_c{i}" for i in range(len(idx))]
            ctor = ast.parse(f"lambda {', '.join(ps)}: {ast.unparse(ctor)}({', '.join(ps)})", mode="eval").body
            ast.fix_missing_locations(ast.copy_location(ctor, stmt))
            for n in ast.walk(ctor):
                n.lineno, n.col_offset = stmt.lineno, getattr(n, "col_offset", 0)
        return {"name": name, "cols": idx, "ctor": self.exc_attr(Sym(sym.module, f"{sym.name}~comp"), name, ast.Expr(value=ctor, lineno=stmt.lineno)),
                "none": none}

    HYBRID_PARTS = {"expression", "setter", "comparator"}

    def hybrid_part(self, fn, module: str, model: str, hybrids: dict) -> tuple[str, str] | None:
        """What a method of a mapped class is to `sqlalchemy.ext.hybrid`: ("property"|"method", name) for
        `@hybrid_property`/`@hybrid_method`, ("expression"|"setter", hybrid name) for `@x.expression`,
        `@x.inplace.expression`, `@x.setter`, `@x.inplace.setter`; None for another method."""
        found = None
        for d in fn.decorator_list:
            ref = self.resolve(module, d) if isinstance(d, (ast.Name, ast.Attribute)) else None
            if isinstance(ref, Ext) and ref.dotted in {"sqlalchemy.ext.hybrid.hybrid_property", "sqlalchemy.ext.hybrid.hybrid_method"}:
                found = ("property" if ref.dotted.endswith("property") else "method", fn.name)
                continue
            chain = []
            node = d
            while isinstance(node, ast.Attribute):
                chain.insert(0, node.attr)
                node = node.value
            if isinstance(node, ast.Name) and node.id in hybrids and chain:
                parts = [c for c in chain if c != "inplace"]
                if len(parts) != 1 or parts[0] not in self.HYBRID_PARTS:
                    raise self.err(f"model {model}.{fn.name}: @{ast.unparse(d)} is not supported (a hybrid's expression, "
                                   "comparator and setter only)", d, module)
                found = (parts[0], node.id)
                continue
            if found is not None and (dotted(d) or "") == "classmethod" :
                continue  # `@x.expression` over `@classmethod` (both forms are SQLAlchemy's)
            if found is not None:
                raise self.err(f"model {model}.{fn.name}: decorator @{ast.unparse(d)} on a hybrid is not supported", d, module)
        if found is not None and isinstance(fn, ast.AsyncFunctionDef):
            raise self.err(f"model {model}.{fn.name}: an async hybrid is not supported", fn, module)
        if found is not None and found[0] in ("expression", "setter", "comparator") and found[1] != fn.name and \
                not any(isinstance(d, ast.Attribute) and isinstance(d.value, ast.Attribute) and d.value.attr == "inplace"
                        for d in fn.decorator_list):
            # `@x.expression def other(cls)`: the class gets `other` as a copy of the hybrid (not modelled)
            raise self.err(f"model {model}.{fn.name}: @{found[1]}.{found[0]} on a function of another name needs "
                           f"`.inplace` ({found[1]}.inplace.{found[0]})", fn, module)
        return found

    LAZY = {"select": "Select", "True": "Select", "selectin": "Selectin", "joined": "Selectin", "subquery": "Selectin",
            "immediate": "Selectin", "noload": "NoLoad", "None": "NoLoad", "raise": "Raise", "raise_on_sql": "Raise",
            "False": "Selectin"}
    REL_OPTIONS = {"back_populates", "backref", "foreign_keys", "lazy", "cascade", "uselist", "passive_deletes",
                   "overlaps", "argument", "doc", "info", "order_by", "remote_side", "secondary", "primaryjoin",
                   "post_update"}

    def primaryjoin_fk(self, node, info, tinfo, where: str, module: str) -> tuple[str, int, int]:
        """`relationship(primaryjoin=a == b)`: one column of each class, one of them the foreign key referencing the
        other. (name of the foreign key column, own column, target column); any other condition is refused."""
        def col_of(e):
            if isinstance(e, ast.Name):
                own = [i for i, c in enumerate(info.cols) if c["name"] == e.id]
                return (info, own[0]) if own else None
            if isinstance(e, ast.Attribute) and isinstance(e.value, ast.Name):
                t = self.resolve(module, e.value)
                for m in (info, tinfo):
                    if t == m.sym:
                        hit = [i for i, c in enumerate(m.cols) if c["name"] == e.attr]
                        return (m, hit[0]) if hit else None
            return None
        if not (isinstance(node, ast.Compare) and len(node.ops) == 1 and isinstance(node.ops[0], ast.Eq)):
            raise self.err(f"relationship {where}: primaryjoin= must be `column == column` (a foreign key and the column "
                           "it references; strings, lambdas, and_() and other conditions are not supported)", node, module)
        sides = [col_of(node.left), col_of(node.comparators[0])]
        if None in sides or {id(sides[0][0]), id(sides[1][0])} != {id(info), id(tinfo)}:
            raise self.err(f"relationship {where}: primaryjoin= must compare a column of {info.sym.name} with a column of "
                           f"{tinfo.sym.name}", node, module)
        own = next(i for m, i in sides if m is info)
        other = next(i for m, i in sides if m is tinfo)
        oc, tc = info.cols[own], tinfo.cols[other]
        if oc.get("fk") == tinfo.table and (oc.get("fk_col") or "id") == (tc.get("sql") or tc["name"]):
            return oc["name"], own, other
        if tc.get("fk") == info.table and (tc.get("fk_col") or "id") == (oc.get("sql") or oc["name"]):
            return tc["name"], own, other
        raise self.err(f"relationship {where}: primaryjoin= must equate a foreign key with the column it references",
                       node, module)

    def secondary_table(self, node: ast.AST, module: str, where: str) -> ModelInfo:
        """`relationship(secondary=...)`: the association table, a module-level `Table(name, metadata, Column(...),
        ...)` named by its variable or its table name, described like a mapped table (no class)."""
        sym, stmt = None, None
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            for m in self.ix.package_modules():
                for st in m.tree.body:
                    if (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name)
                            and isinstance(st.value, ast.Call) and (dotted(st.value.func) or "").split(".")[-1] == "Table"
                            and st.value.args and isinstance(st.value.args[0], ast.Constant) and st.value.args[0].value == node.value):
                        sym, stmt = Sym(m.name, st.targets[0].id), st
        elif isinstance(node, (ast.Name, ast.Attribute)):
            t = self.resolve(module, node)
            d = self.ix.definition(t) if isinstance(t, Sym) else None
            if isinstance(d, ast.Assign) and isinstance(d.value, ast.Call):
                sym, stmt = t, d
        if sym is None and isinstance(node, ast.Constant) and isinstance(node.value, str):
            # the table of a mapped class (`class RequestResponse(Base): __tablename__ = "request_response"`): SQLAlchemy
            # finds it by name in the metadata, as a Table
            mapped = [ms for ms in self.fe.model_syms if self.model(ms).table == node.value]
            if len(mapped) == 1:
                return self.mapped_core_table(mapped[0], node, module)
        if sym is None or not (isinstance(stmt.value, ast.Call) and isinstance(self.resolve(sym.module, stmt.value.func), Ext)
                               and self.resolve(sym.module, stmt.value.func).dotted in {"sqlalchemy.Table", "sqlalchemy.schema.Table"}):
            raise self.err(f"relationship {where}: secondary= must name a module-level Table(...) (its variable or its "
                           "table name)", node, module)
        return self.core_table(sym, stmt)

    def core_table(self, sym: Sym, stmt: ast.Assign) -> ModelInfo:
        """A module-level `Table(name, metadata, Column(...), ...)`: an association table (`secondary=`) or a Core
        table used as a value (`insert(t)`, `select(t)`, `t.c.col`), described like a mapped table without class."""
        secs = self.__dict__.setdefault("secondaries", {})
        if sym in secs:
            return secs[sym]
        call = stmt.value
        if len(call.args) < 2 or not isinstance(call.args[0], ast.Constant):
            raise self.err(f"Table {sym.name}: Table(name, metadata, *columns) expected", call, sym.module)
        if call.keywords:
            raise self.err(f"Table {sym.name}: option {call.keywords[0].arg}= of Table() is not supported",
                           call.keywords[0], sym.module)
        cols = []
        for a in call.args[2:]:
            if isinstance(a, ast.Call) and (dotted(a.func) or "").split(".")[-1] == "Column":
                if not (a.args and isinstance(a.args[0], ast.Constant) and isinstance(a.args[0].value, str)):
                    raise self.err(f"Table {sym.name}: a Column needs its name first", a, sym.module)
                cols.append(self.column(a.args[0].value, None, a, sym.module, stmt))
            elif isinstance(a, ast.Call) and (dotted(a.func) or "").split(".")[-1] in {"PrimaryKeyConstraint", "UniqueConstraint", "Index"}:
                continue  # the DDL's business
            else:
                raise self.err(f"Table {sym.name}: `{ast.unparse(a)[:40]}` is not supported in a Table() (Column(name, ...) "
                               "and constraints only)", a, sym.module)
        pks = [i for i, c in enumerate(cols) if c["pk"]] or [0]
        base = f"{mod_ident(sym.module)}__{ident(sym.name)}"
        info = ModelInfo(sym, f"MODEL_{base}", f"CLS_{base}", call.args[0].value, cols, pks[0],
                         sorted({c["fk"] for c in cols if c.get("fk")}), pks, rels=[])
        secs[sym] = info
        return info

    def mapped_core_table(self, sym: Sym, node, module: str) -> ModelInfo:
        """`Model.__table__` used as a Core table (`t.join(...)`, `t.c.col`, `select(t)`): the mapped table described
        like a module-level `Table()` (columns by their key, the column name; rows, not objects)."""
        secs = self.__dict__.setdefault("secondaries", {})
        key = ("__table__", sym)
        if key in secs:
            return secs[key]
        info = self.model(sym)
        if info.parent is not None or info.joins:
            raise self.err(f"{sym.name}.__table__: the Core use of the table of an inheriting class is not supported",
                           node, module)
        cols = []
        for c in info.cols:
            c = dict(c, name=c.get("sql") or c["name"], deferred=False, group=None, raiseload=False)
            c.pop("table", None)
            cols.append(c)
        base = f"{mod_ident(sym.module)}__{ident(sym.name)}__table"
        core = ModelInfo(sym, f"MODEL_{base}", f"CLS_{base}", info.table, cols, info.pk, list(info.fk_tables),
                         list(info.pks), rels=[])
        secs[key] = core
        return core

    def rel_order(self, v: ast.AST, tsym: Sym, tinfo, where: str, module: str) -> list[tuple[int, bool]]:
        """`relationship(order_by=...)`: target columns (Target.col, `.desc()`/`.asc()`, a list), or a string
        SQLAlchemy evaluates against the class registry ("Target.col", "[Target.a, Target.b.desc()]")."""
        from_str = isinstance(v, ast.Constant) and isinstance(v.value, str)
        node = ast.parse(v.value, mode="eval").body if from_str else v
        out = []
        for expr in node.elts if isinstance(node, (ast.List, ast.Tuple)) else [node]:
            desc = False
            if (isinstance(expr, ast.Call) and isinstance(expr.func, ast.Attribute) and expr.func.attr in {"desc", "asc"}
                    and not expr.args and not expr.keywords):
                desc, expr = expr.func.attr == "desc", expr.func.value
            target_ok = isinstance(expr, ast.Attribute) and (
                (isinstance(expr.value, ast.Name) and expr.value.id == tsym.name) if from_str
                else self.resolve(module, expr.value) == tsym)
            idx = next((i for i, c in enumerate(tinfo.cols) if target_ok and c["name"] == expr.attr), None)
            if idx is None:
                raise self.err(f"relationship {where}: order_by= must name columns of {tsym.name}", v, module)
            out.append((idx, desc))
        return out

    def resolve_rels(self, info: ModelInfo) -> None:
        """Resolve `relationship()` declarations: target model, direction from the foreign keys, options.
        Only plain foreign-key relationships between two different models (no secondary, primaryjoin,
        remote_side, order_by, dynamic/write_only loading)."""
        rels = info.rels if info.rels is not None else []
        info.rels = rels
        raw, info.rels_raw = info.rels_raw, []
        inherited = info.__dict__.get("inherited_rels", set())
        if info.parent is not None and inherited:
            # an inherited relationship is the parent's, resolved against the class that declares it (its
            # columns keep their positions in the subclass)
            parent = self.models[info.parent]
            if parent.rels is None or parent.rels_raw or parent.__dict__.get("_pending_rels"):
                self.resolve_rels(parent)
            have = {r["name"] for r in rels}
            rels.extend(dict(r) for r in parent.rels if r["name"] not in have)
            raw = [x for x in raw if x[0] not in inherited]
        for name, ann, call, module in raw:
            kind = (dotted(ann.value) or "").split(".")[-1] if isinstance(ann, ast.Subscript) else ""
            if kind == "DynamicMapped":
                raise self.err(f"relationship {info.sym.name}.{name}: DynamicMapped[...] is not supported "
                               "(use WriteOnlyMapped[...])", ann, module)
            for kw in call.keywords:
                if kw.arg not in self.REL_OPTIONS:
                    raise self.err(f"relationship {info.sym.name}.{name}: option {kw.arg}= is not supported", kw, module)
            opts = {kw.arg: kw.value for kw in call.keywords}
            target_node = call.args[0] if call.args else opts.get("argument")
            if target_node is None:
                if ann is None:
                    raise self.err(f"relationship {info.sym.name}.{name}: no target class", call, module)
                target_node = self._rel_target_from_ann(ann)
            tsym = self._rel_target(target_node, module, call)
            # a route reaching this class can load the other one (lazy loads, back_populates, cascades): what does
            # not translate there blocks it too (closure_errors), as the generation would
            self.edges.setdefault(("class", info.sym), set()).add(("class", tsym))
            self.edges.setdefault(("class", tsym), set()).add(("class", info.sym))
            tinfo = self.model(tsym)
            lazy_kw = opts.get("lazy")
            write_only = kind == "WriteOnlyMapped" or (isinstance(lazy_kw, ast.Constant) and lazy_kw.value == "write_only")
            if write_only:
                opts.pop("lazy", None)
                for k in ("back_populates", "backref", "uselist"):
                    if k in opts:
                        raise self.err(f"relationship {info.sym.name}.{name}: {k}= on a write-only collection is not supported",
                                       opts[k], module)
            if "secondary" in opts:
                self._secondary_rel(info, name, call, module, opts, tsym, tinfo, rels, write_only)
                continue
            pj = None
            if "primaryjoin" in opts:
                if tsym == info.sym or "secondary" in opts or "remote_side" in opts:
                    raise self.err(f"relationship {info.sym.name}.{name}: primaryjoin= is only supported between two "
                                   "classes, without secondary= or remote_side=", opts["primaryjoin"], module)
                pj = self.primaryjoin_fk(opts["primaryjoin"], info, tinfo, f"{info.sym.name}.{name}", module)
            if "remote_side" in opts and tsym != info.sym:
                raise self.err(f"relationship {info.sym.name}.{name}: remote_side= is only supported on a "
                               "self-referential relationship", opts["remote_side"], module)
            fks = None
            if "foreign_keys" in opts:
                v = opts["foreign_keys"]
                items = v.elts if isinstance(v, (ast.List, ast.Tuple)) else [v]
                fks = set()
                for it in items:
                    if isinstance(it, ast.Constant) and isinstance(it.value, str):
                        fks.add(it.value.split(".")[-1].strip("[] "))
                    elif isinstance(it, ast.Name):
                        fks.add(it.id)
                    elif isinstance(it, ast.Attribute):
                        fks.add(it.attr)
                    else:
                        raise self.err(f"relationship {info.sym.name}.{name}: unsupported foreign_keys=", it, module)
            if pj is not None:
                if fks is not None and fks != {pj[0]}:
                    raise self.err(f"relationship {info.sym.name}.{name}: foreign_keys= names another column than "
                                   "primaryjoin=", opts["foreign_keys"], module)
                fks = {pj[0]}
            local = [i for i, c in enumerate(info.cols) if c.get("fk") == tinfo.table and (fks is None or c["name"] in fks)]
            remote = [j for j, c in enumerate(tinfo.cols) if c.get("fk") == info.table and (fks is None or c["name"] in fks)]
            if tsym == info.sym:
                # adjacency list: one-to-many by default, many-to-one when remote_side= names the referenced
                # column(s) (SQLAlchemy: the side that is not remote holds the foreign key)
                if len(local) != 1:
                    raise self.err(f"relationship {info.sym.name}.{name}: a self-referential relationship needs exactly "
                                   f"one foreign key column of {info.table} (pass foreign_keys=)", call, module)
                fk_i = local[0]
                ref_i = self._fk_target_col(info.cols[fk_i], info, info, name, call, module)
                rs = self._remote_side(opts.get("remote_side"), info, name, module)
                if rs is None or rs == {fk_i}:
                    local, remote = [], [fk_i]
                elif rs == {ref_i}:
                    remote = []
                else:
                    raise self.err(f"relationship {info.sym.name}.{name}: remote_side= must name {info.cols[ref_i]['name']} "
                                   f"or {info.cols[fk_i]['name']}", opts["remote_side"], module)
            if len(local) == 1 and not remote:
                m2o, li = True, local[0]
                ri = self._fk_target_col(info.cols[li], tinfo, info, name, call, module)
            elif len(remote) == 1 and not local:
                m2o, ri = False, remote[0]
                li = self._fk_target_col(tinfo.cols[ri], info, info, name, call, module)
            else:
                raise self.err(f"relationship {info.sym.name}.{name}: cannot pick the foreign key between "
                               f"{info.table} and {tinfo.table} (pass foreign_keys=)", call, module)
            if pj is not None and (li, ri) != (pj[1], pj[2]):
                raise self.err(f"relationship {info.sym.name}.{name}: primaryjoin= must equate a foreign key with the "
                               "column it references", opts["primaryjoin"], module)
            lit = lambda k, d=None: literal(opts[k], self.src(module)) if k in opts else d  # noqa: E731
            post_update = bool(lit("post_update", False))
            if post_update and not m2o:
                # SQLAlchemy's advice (the many-to-one side); the one-to-many form updates the children instead
                raise self.err(f"relationship {info.sym.name}.{name}: post_update=True is only supported on a "
                               "many-to-one relationship", opts["post_update"], module)
            lazy = str(lit("lazy", "select"))
            if lazy not in self.LAZY:
                raise self.err(f"relationship {info.sym.name}.{name}: lazy={lazy!r} is not supported", opts["lazy"], module)
            cascade = {c.strip() for c in str(lit("cascade", "save-update, merge")).split(",")}
            if "all" in cascade:
                cascade |= {"save-update", "merge", "refresh-expire", "expunge", "delete"}
            uselist = lit("uselist", not m2o)
            if write_only and m2o:
                raise self.err(f"relationship {info.sym.name}.{name}: a write-only many-to-one relationship is not supported",
                               call, module)
            if write_only and not lit("passive_deletes", False):
                # without it SQLAlchemy loads the whole collection when the parent is deleted
                raise self.err(f"relationship {info.sym.name}.{name}: a write-only collection needs passive_deletes=True",
                               call, module)
            order = self.rel_order(opts["order_by"], tsym, tinfo, f"{info.sym.name}.{name}", module) if "order_by" in opts else []
            rels.append({"name": name, "target": tsym, "m2o": m2o, "local": li, "remote": ri, "uselist": bool(uselist), "order": order,
                         "lazy": self.LAZY[lazy], "back": lit("back_populates"), "delete": "delete" in cascade,
                         "orphan": "delete-orphan" in cascade, "passive_deletes": bool(lit("passive_deletes", False)),
                         "write_only": write_only, "refresh_expire": "refresh-expire" in cascade,
                         "post_update": post_update})
            if "backref" in opts and name not in info.__dict__.get("inherited_rels", ()):
                b = opts["backref"]
                bname, bkw = (b.value, {}) if isinstance(b, ast.Constant) else (None, None)
                if isinstance(b, ast.Call) and (dotted(b.func) or "").split(".")[-1] == "backref" and b.args:
                    bname = literal(b.args[0], self.src(module))
                    bkw = {k.arg: literal(k.value, self.src(module)) for k in b.keywords}
                if not isinstance(bname, str) or set(bkw) - {"passive_deletes", "lazy", "cascade", "uselist"}:
                    raise self.err(f"relationship {info.sym.name}.{name}: unsupported backref=", b, module)
                bcascade = {c.strip() for c in str(bkw.get("cascade", "save-update, merge")).split(",")}
                if "all" in bcascade:
                    bcascade |= {"delete", "refresh-expire"}
                rels[-1]["back"] = bname
                trels = tinfo.rels if tinfo.rels is not None else tinfo.__dict__.setdefault("_pending_rels", [])
                trels.append({"name": bname, "target": info.sym, "m2o": not m2o, "local": ri, "remote": li,
                              "uselist": bool(bkw.get("uselist", m2o)), "lazy": self.LAZY[str(bkw.get("lazy", "select"))],
                              "back": name, "delete": "delete" in bcascade, "orphan": "delete-orphan" in bcascade,
                              "passive_deletes": bool(bkw.get("passive_deletes", False)),
                              "refresh_expire": "refresh-expire" in bcascade})
        rels.extend(info.__dict__.pop("_pending_rels", []))

    def _secondary_rel(self, info, name, call, module, opts, tsym, tinfo, rels, write_only=False) -> None:
        """A many-to-many relationship through an association table (`secondary=`)."""
        where = f"{info.sym.name}.{name}"
        for k in ("foreign_keys", "remote_side", "order_by"):
            if k in opts:
                raise self.err(f"relationship {where}: secondary= with {k}= is not supported", opts[k], module)
        if tsym == info.sym:
            raise self.err(f"relationship {where}: a self-referential many-to-many relationship is not supported", call, module)
        sec = self.secondary_table(opts["secondary"], module, where)
        left = [i for i, c in enumerate(sec.cols) if c.get("fk") == info.table]
        right = [i for i, c in enumerate(sec.cols) if c.get("fk") == tinfo.table]
        if len(left) != 1 or len(right) != 1:
            raise self.err(f"relationship {where}: the secondary table {sec.table} needs one foreign key to {info.table} "
                           f"and one to {tinfo.table}", opts["secondary"], module)
        li = self._fk_target_col(sec.cols[left[0]], info, info, name, call, module)
        ri = self._fk_target_col(sec.cols[right[0]], tinfo, info, name, call, module)
        lit = lambda k, d=None: literal(opts[k], self.src(module)) if k in opts else d  # noqa: E731
        lazy = str(lit("lazy", "select"))
        if lazy not in self.LAZY:
            raise self.err(f"relationship {where}: lazy={lazy!r} is not supported", opts["lazy"], module)
        cascade = {c.strip() for c in str(lit("cascade", "save-update, merge")).split(",")}
        if "all" in cascade:
            cascade |= {"save-update", "merge", "refresh-expire", "expunge", "delete"}
        # passive_deletes=True on a write-only collection: the association rows are left to ON DELETE
        if "delete-orphan" in cascade or "delete" in cascade or (lit("passive_deletes", False) and not write_only):
            raise self.err(f"relationship {where}: cascade delete / delete-orphan / passive_deletes on a many-to-many "
                           "relationship is not supported", call, module)
        if not lit("uselist", True):
            raise self.err(f"relationship {where}: uselist=False on a many-to-many relationship is not supported", call, module)
        if "backref" in opts:
            raise self.err(f"relationship {where}: backref= on a many-to-many relationship is not supported", opts["backref"], module)
        if "back_populates" in opts:
            # SQLAlchemy keeps both collections in step when one changes in memory (instrumented lists): not modelled.
            # It changes nothing else (SQL, loading): accepted when no code of the project mutates either collection
            back = lit("back_populates")
            # SQLAlchemy configures the pair: the other class must declare `back` as a relationship to this one, with
            # back_populates naming this one (else InvalidRequestError / a warning at configuration)
            other = next((st for st in self.fe.model_syms[tsym].body
                          if isinstance(st, (ast.Assign, ast.AnnAssign)) and isinstance(st.value, ast.Call)
                          and (dotted(st.value.func) or "").split(".")[-1] == "relationship"
                          and any(isinstance(t, ast.Name) and t.id == back
                                  for t in (st.targets if isinstance(st, ast.Assign) else [st.target]))), None)
            pair = next((k.value.value for k in other.value.keywords if k.arg == "back_populates"
                         and isinstance(k.value, ast.Constant)), None) if other is not None else None
            if not isinstance(back, str) or pair != name or "secondary" not in {k.arg for k in other.value.keywords}:
                raise self.err(f"relationship {where}: back_populates={back!r} must name a many-to-many relationship of "
                               f"{tsym.name} whose back_populates is {name!r}", opts["back_populates"], module)
            mutated = self._collection_mutation({name, back} if isinstance(back, str) else {name}, {info.sym, tsym})
            if mutated is not None:
                node_, mmod = mutated
                raise self.err(f"relationship {where}: back_populates= on a many-to-many relationship whose collection is "
                               f"changed in memory (`{ast.unparse(node_)[:50]}`, {self.src(mmod)}:{node_.lineno}) is not "
                               "supported (both sides are not kept in step)", opts["back_populates"], module)
        rels.append({"name": name, "target": tsym, "m2o": False, "local": li, "remote": ri, "uselist": True, "order": [],
                     "lazy": self.LAZY[lazy], "back": None, "delete": False, "orphan": False,
                     "passive_deletes": bool(lit("passive_deletes", False)), "secondary": (sec, left[0], right[0]),
                     "write_only": write_only, "refresh_expire": "refresh-expire" in cascade})

    def _collection_mutation(self, names: set[str], classes: set[Sym]):
        """The first statement of the project that may change a collection attribute named in `names` in memory:
        `x.<name>.append/extend/insert/remove/pop/clear/add/discard/update(...)`, `x.<name> = ...`, `x.<name> += ...`,
        `del x.<name>[...]`, `x.<name>[i] = ...`, or a constructor of one of `classes` given `<name>=`. Matched by
        attribute name whatever the receiver (an over-approximation). (node, module) or None."""
        mut = {"append", "extend", "insert", "remove", "pop", "clear", "add", "discard", "update", "__setitem__",
               "__delitem__", "__iadd__"}
        from .frontend import _enclosing_fn

        def not_mapped(cls_node, module):
            t = self.ix.resolve_expr(module, cls_node) if isinstance(cls_node, (ast.Name, ast.Attribute)) else None
            return isinstance(t, Sym) and t not in self.fe.model_syms and isinstance(self.ix.definition(t), ast.ClassDef)

        def other_instance(recv, module, fn) -> bool:
            """`recv` is certainly an instance of a project class that is not mapped: a local bound only to
            `Cls(...)` (or `{k: Cls(...) ...}` / `[Cls(...) ...]`, read by subscript), or annotated `Cls`."""
            sub = isinstance(recv, ast.Subscript)
            base = recv.value if sub else recv
            if not (isinstance(base, ast.Name) and fn is not None):
                return False
            values = []
            for a in ast.walk(fn):
                if isinstance(a, ast.Assign) and any(isinstance(t, ast.Name) and t.id == base.id for t in a.targets):
                    values.append(a.value)
                elif isinstance(a, ast.AnnAssign) and isinstance(a.target, ast.Name) and a.target.id == base.id:
                    ann = a.annotation
                    if sub and isinstance(ann, ast.Subscript):
                        ann = ann.slice.elts[-1] if isinstance(ann.slice, ast.Tuple) else ann.slice
                    if not not_mapped(ann, module):
                        return False
                    if a.value is not None:
                        values.append(a.value)
                elif isinstance(a, (ast.For, ast.comprehension)) and isinstance(a.target, ast.Name) and a.target.id == base.id:
                    return False
            if not values:
                return False
            for v in values:
                if sub and isinstance(v, (ast.DictComp, ast.ListComp)):
                    v = v.value if isinstance(v, ast.DictComp) else v.elt
                if not (isinstance(v, ast.Call) and not_mapped(v.func, module)):
                    return False
            return True
        for m in self.ix.package_modules():
            parents = self.fe._parents(m.name)
            for n in ast.walk(m.tree):
                if isinstance(n, ast.Attribute) and n.attr in names:
                    if other_instance(n.value, m.name, _enclosing_fn(n, parents)):
                        continue
                    up = parents.get(n)
                    if isinstance(n.ctx, (ast.Store, ast.Del)):
                        return n, m.name
                    if isinstance(up, ast.Attribute) and up.attr in mut and isinstance(parents.get(up), ast.Call):
                        return up, m.name
                    if isinstance(up, ast.AugAssign) and up.target is n:
                        return up, m.name
                    if isinstance(up, ast.Subscript) and isinstance(up.ctx, (ast.Store, ast.Del)):
                        return up, m.name
                if isinstance(n, ast.Call) and any(k.arg in names for k in n.keywords) and isinstance(n.func, (ast.Name, ast.Attribute)):
                    if self.ix.resolve_expr(m.name, n.func) in classes:
                        return n, m.name
        return None

    def _remote_side(self, v, info, name, module) -> set[int] | None:
        """`remote_side=`: column indexes of the model (id, [id], "id", "[Model.id]", a set or tuple of them)."""
        if v is None:
            return None
        node = ast.parse(v.value, mode="eval").body if isinstance(v, ast.Constant) and isinstance(v.value, str) else v
        out = set()
        for it in node.elts if isinstance(node, (ast.List, ast.Tuple, ast.Set)) else [node]:
            cname = it.id if isinstance(it, ast.Name) else it.attr if isinstance(it, ast.Attribute) else None
            idx = next((i for i, c in enumerate(info.cols) if c["name"] == cname), None)
            if idx is None:
                raise self.err(f"relationship {info.sym.name}.{name}: remote_side= must name columns of {info.sym.name}",
                               v, module)
            out.add(idx)
        return out

    def _rel_target_from_ann(self, ann):
        inner = ann.slice if isinstance(ann, ast.Subscript) else ann
        if isinstance(inner, ast.Constant) and isinstance(inner.value, str):
            inner = ast.parse(inner.value, mode="eval").body
        if isinstance(inner, ast.Subscript) and (dotted(inner.value) or "") in {"list", "List", "typing.List"}:
            inner = inner.slice
        parts = [x for x in self._union_parts(inner) if not (isinstance(x, ast.Constant) and x.value is None)]
        return parts[0]

    def _rel_target(self, node, module, call) -> Sym:
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            name = node.value.split(".")[-1]
            hits = [s for s in self.fe.model_syms if s.name == name]
            if len(hits) != 1:
                raise self.err(f"relationship target {node.value!r}: {'unknown' if not hits else 'ambiguous'} mapped class",
                               call, module)
            return hits[0]
        if isinstance(node, ast.Lambda):
            return self._rel_target(node.body, module, call)
        t = self.resolve(module, node)
        if isinstance(t, Sym) and t in self.fe.model_syms:
            return t
        raise self.err(f"relationship target `{ast.unparse(node)}` is not a mapped class", call, module)

    def fk_type(self, table: str, cname: str, seen: set) -> str | None:
        """The column type of `table.cname` (a mapped class's column), None when not found."""
        if (table, cname) in seen:
            return None
        seen.add((table, cname))
        for sym, node in self.fe.model_syms.items():
            tn = next((st.value.value for st in node.body if isinstance(st, ast.Assign) and len(st.targets) == 1
                       and isinstance(st.targets[0], ast.Name) and st.targets[0].id == "__tablename__"
                       and isinstance(st.value, ast.Constant)), None)
            if tn != table:
                continue
            for st in node.body:
                tgt = st.target if isinstance(st, ast.AnnAssign) else (st.targets[0] if isinstance(st, ast.Assign) and len(st.targets) == 1 else None)
                if isinstance(tgt, ast.Name) and tgt.id == cname and isinstance(st.value, ast.Call):
                    try:
                        return self.column(cname, getattr(st, "annotation", None), st.value, sym.module, st,
                                           self.tam_of(sym))["ty"]
                    except TranspileError:
                        return None
        for m in self.ix.package_modules():  # a Core Table(name, metadata, Column("cname", Type), ...)
            for st in m.tree.body:
                if (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.value, ast.Call)
                        and (dotted(st.value.func) or "").split(".")[-1] == "Table" and st.value.args
                        and isinstance(st.value.args[0], ast.Constant) and st.value.args[0].value == table):
                    for a in st.value.args[2:]:
                        if (isinstance(a, ast.Call) and (dotted(a.func) or "").split(".")[-1] == "Column" and a.args
                                and isinstance(a.args[0], ast.Constant) and a.args[0].value == cname):
                            try:
                                return self.column(cname, None, a, m.name, st)["ty"]
                            except TranspileError:
                                return None
        return None

    def _fk_target_col(self, col, tinfo, info, name, call, module) -> int:
        """Index in `tinfo` of the column a foreign key column references."""
        if not col.get("fk_col") and len(tinfo.pks) > 1:
            raise self.err(f"relationship {info.sym.name}.{name}: foreign key to the composite key of {tinfo.table} "
                           "is not supported", call, module)
        cname = col.get("fk_col") or tinfo.cols[tinfo.pk].get("sql") or tinfo.cols[tinfo.pk]["name"]
        for j, c in enumerate(tinfo.cols):
            if (c.get("sql") or c["name"]) == cname:
                return j
        raise self.err(f"relationship {info.sym.name}.{name}: {tinfo.table}.{cname} is not a mapped column", call, module)

    TYPE_DECORATOR = ("sqlalchemy.types.TypeDecorator", "sqlalchemy.TypeDecorator", "sqlalchemy.sql.type_api.TypeDecorator")

    def is_type_decorator(self, sym: Sym) -> bool:
        d = self.ix.definition(sym)
        return isinstance(d, ast.ClassDef) and any(
            is_ext(self.resolve(sym.module, b.value if isinstance(b, ast.Subscript) else b), *self.TYPE_DECORATOR) for b in d.bases)

    def type_dec(self, sym: Sym) -> str:
        """A project `TypeDecorator` subclass: `impl` (a string type) and process_bind_param /
        process_result_value(self, value, dialect) compiled as functions that use neither self nor dialect."""
        tds = self.__dict__.setdefault("_type_decs", {})
        if sym in tds:
            return tds[sym]
        node = self.ix.definition(sym)
        module = sym.module
        if len(node.bases) != 1:
            raise self.err(f"TypeDecorator {sym.name}: a single base class is supported", node, module)
        impl, hooks = None, {}
        for st in node.body:
            if isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name):
                n = st.targets[0].id
                if n == "impl":
                    v = st.value.func if isinstance(st.value, ast.Call) else st.value
                    t = self.resolve(module, v)
                    last = t.dotted.split(".")[-1] if isinstance(t, Ext) and t.package == "sqlalchemy" else None
                    if self.SA_TYPES.get(last) != "Str":
                        raise self.err(f"TypeDecorator {sym.name}: impl `{ast.unparse(st.value)}` is not supported "
                                       "(string types only)", st.value, module)
                    impl = "Str"
                elif n != "cache_ok":
                    raise self.err(f"TypeDecorator {sym.name}: class attribute `{n}` is not supported", st, module)
            elif isinstance(st, ast.FunctionDef) and st.name in {"process_bind_param", "process_result_value"}:
                args = [a.arg for a in st.args.args]
                if len(args) != 3 or st.decorator_list or st.args.vararg or st.args.kwarg or st.args.kwonlyargs:
                    raise self.err(f"{sym.name}.{st.name} must be (self, value, dialect)", st, module)
                used = {n.id for n in ast.walk(st) if isinstance(n, ast.Name)}
                for unused in (args[0], args[2]):
                    if unused in used:
                        raise self.err(f"{sym.name}.{st.name}: using `{unused}` is not supported", st, module)
                hooks[st.name] = f"Some({method_wrapper(self, Sym(module, f'{node.name}.{st.name}'))} as {RT}::pyd::MethodFn)"
            elif not isinstance(st, (ast.Pass, ast.Expr)):
                raise self.err(f"TypeDecorator {sym.name}: `{getattr(st, 'name', type(st).__name__)}` is not supported "
                               "(only impl, cache_ok, process_bind_param, process_result_value)", st, module)
        if impl is None:
            raise self.err(f"TypeDecorator {sym.name}: `impl = ...` is required", node, module)
        name = f"TYPEDEC_{mod_ident(module)}__{ident(sym.name)}"
        tds[sym] = name
        self.items.append(
            f"static {name}: {RT}::orm::TypeDec = {RT}::orm::TypeDec {{ name: {rs(sym.name)}, impl_ty: {RT}::orm::ColTy::{impl}, "
            f"bind: {hooks.get('process_bind_param', 'None')}, result: {hooks.get('process_result_value', 'None')} }};"
        )
        return name

    def is_schema_item(self, a: ast.AST, module: str) -> bool:
        """A positional argument of mapped_column()/Column() that is not its type: ForeignKey(...), Identity()..."""
        f = a.func if isinstance(a, ast.Call) else a
        t = self.resolve(module, f) if isinstance(f, (ast.Name, ast.Attribute)) else None
        return isinstance(t, Ext) and t.dotted.split(".")[-1] in {"ForeignKey", "Identity", "Sequence", "Computed",
                                                                  "CheckConstraint", "FetchedValue"}

    SA_TYPES = {
        "Integer": "Int", "INTEGER": "Int", "BigInteger": "BigInt", "BIGINT": "BigInt", "SmallInteger": "SmallInt",
        "String": "Str", "VARCHAR": "Str", "Text": "Str", "TEXT": "Str", "Unicode": "Str", "UnicodeText": "Str",
        "Boolean": "Bool", "BOOLEAN": "Bool", "Float": "Float", "Double": "Float", "REAL": "Float",
        "Date": "Date", "DATE": "Date", "Time": "Time", "JSON": "Json", "JSONB": "Json", "Uuid": "Uuid", "UUID": "Uuid",
        "LargeBinary": "Bytes", "BYTEA": "Bytes",
    }
    PY_COL_TYPES = {"int": "Int", "str": "Str", "bool": "Bool", "float": "Float", "bytes": "Bytes"}

    def inline_type_factory(self, fn: Sym, call: ast.Call, module: str, col: str):
        """`_enum(Status, "status")` where `def _enum(c, n): return Enum(c, name=n, ...)`: the returned expression,
        parameters replaced by the arguments (evaluated once at class creation, like SQLAlchemy sees it)."""
        fd = self.ix.definition(fn)
        body = [s for s in fd.body if not (isinstance(s, ast.Expr) and isinstance(s.value, ast.Constant))]
        if (isinstance(fd, ast.AsyncFunctionDef) or fd.decorator_list or len(body) != 1
                or not isinstance(body[0], ast.Return) or body[0].value is None):
            raise self.err(f"column {col}: type factory {fn.name}() must be a plain function whose body is "
                           f"`return <column type>`", call, module)
        fa = fd.args
        if fa.vararg or fa.kwarg or fa.posonlyargs or fa.kwonlyargs:
            raise self.err(f"column {col}: type factory {fn.name}(): only plain parameters are supported", fd, fn.module)
        names = [p.arg for p in fa.args]
        bound: dict[str, ast.AST] = dict(zip(names[len(names) - len(fa.defaults):], fa.defaults))
        given = list(call.args) + [k.value for k in call.keywords]
        if len(call.args) > len(names):
            raise self.err(f"column {col}: too many arguments to {fn.name}()", call, module)
        bound.update(zip(names, call.args))
        for k in call.keywords:
            if k.arg not in names:
                raise self.err(f"column {col}: {fn.name}() has no parameter `{k.arg}`", k, module)
            bound[k.arg] = k.value
        if missing := [n for n in names if n not in bound]:
            raise self.err(f"column {col}: {fn.name}() missing argument `{missing[0]}`", call, module)
        for g in given:
            if not isinstance(g, (ast.Constant, ast.Name, ast.Attribute)):
                raise self.err(f"column {col}: argument `{ast.unparse(g)}` of {fn.name}() must be a literal or a name",
                               g, module)
        ret = body[0].value
        local = {n.arg for n in ast.walk(ret) if isinstance(n, ast.arg)} | {
            n.id for n in ast.walk(ret) if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Store)}
        for n in ast.walk(ret):
            # the expression is read where the column is declared: its other names must mean the same there
            if (isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load) and n.id not in bound and n.id not in local
                    and self.resolve(module, n) != self.resolve(fn.module, n)):
                raise self.err(f"column {col}: `{n.id}` in {fn.name}() does not mean the same in {module}", n, fn.module)
        if any(isinstance(n, ast.arg) and n.arg in bound for n in ast.walk(ret)) or any(
                isinstance(n, ast.Name) and isinstance(n.ctx, ast.Store) and n.id in bound for n in ast.walk(ret)):
            raise self.err(f"column {col}: type factory {fn.name}() rebinds one of its parameters", ret, fn.module)

        class Subst(ast.NodeTransformer):
            def visit_Name(self, n):
                return copy.deepcopy(bound[n.id]) if n.id in bound and isinstance(n.ctx, ast.Load) else n

        return ast.fix_missing_locations(Subst().visit(copy.deepcopy(ret))), module

    def tam_of(self, sym: Sym, depth: int = 0) -> dict:
        """The `type_annotation_map` of a mapped class's declarative base (its bases, first found): {annotation
        key ("str", "datetime.datetime"...): (type expression, its module)}."""
        cache = self.__dict__.setdefault("_tams", {})
        if sym in cache:
            return cache[sym]
        out: dict = {}
        node = self.ix.definition(sym)
        for b in getattr(node, "bases", []) if depth < 10 else []:
            t = self.resolve(sym.module, b)
            bn = self.ix.definition(t) if isinstance(t, Sym) else None
            if not isinstance(bn, ast.ClassDef):
                continue
            for st in bn.body:
                tgt = st.target if isinstance(st, ast.AnnAssign) else st.targets[0] if isinstance(st, ast.Assign) and len(st.targets) == 1 else None
                if isinstance(tgt, ast.Name) and tgt.id == "type_annotation_map":
                    if not isinstance(st.value, ast.Dict) or None in st.value.keys:
                        raise self.err(f"{t.name}.type_annotation_map: only a dict literal is supported", st, t.module)
                    for k, v in zip(st.value.keys, st.value.values):
                        key = self.annotation_key(k, t.module)
                        if key is None:
                            ks = self.resolve(t.module, k) if isinstance(k, (ast.Name, ast.Attribute)) else None
                            if isinstance(ks, Sym):
                                # a class of the project (an Enum): refused for the columns annotated with it only
                                out[("unsupported", ks)] = (k, t)
                                continue
                            raise self.err(f"{t.name}.type_annotation_map: key `{ast.unparse(k)}` is not supported "
                                           "(types written by name, or generics of them)", k, t.module)
                        out[key] = (v, t.module)
                    break
            else:
                out = self.tam_of(t, depth + 1)
            if out:
                break
        cache[sym] = out
        return out

    BUILTIN_KEYS = {"int", "str", "bool", "float", "bytes", "dict", "list"}

    def annotation_key(self, node, module: str) -> str | None:
        """A Python type as `type_annotation_map` keys it: a builtin's name, else the dotted name of a library type."""
        if isinstance(node, ast.Name) and node.id in self.BUILTIN_KEYS and self.resolve(module, node) is None:
            return node.id
        if isinstance(node, ast.Subscript):
            # a generic (`dict[str, Any]`): matched as written, the exact annotation (as SQLAlchemy looks it up first)
            origin = self.annotation_key(node.value, module)
            return f"{origin}[{ast.unparse(node.slice)}]" if origin else None
        t = self.resolve(module, node) if isinstance(node, (ast.Name, ast.Attribute)) else None
        return t.dotted if isinstance(t, Ext) else None

    def column(self, name: str, ann, call, module: str, stmt, tam: dict | None = None) -> dict:
        col = {"name": name, "ty": None, "nullable": None, "pk": False, "default": None, "server_default": False,
               "onupdate": None, "fk": None, "autoincrement": True, "deferred": False, "jsonb": False,
               "group": None, "raiseload": False}
        if isinstance(call, ast.Call) and isinstance(self.resolve(module, call.func), Ext) and \
                self.resolve(module, call.func).dotted in {"sqlalchemy.orm.deferred", "sqlalchemy.orm.strategy_options.deferred"}:
            # `deferred(Column(...), group=, raiseload=)`: left out of the entity's loaded state (read, then unloaded)
            if len(call.args) != 1 or not isinstance(call.args[0], ast.Call) or \
                    any(k.arg not in {"group", "raiseload"} for k in call.keywords):
                raise self.err(f"column {name}: only deferred(Column(...), group=, raiseload=) is supported (no "
                               "extra columns or other options)", call, module)
            col["deferred"] = True
            for k in call.keywords:
                v = literal(k.value, self.src(module))
                if k.arg == "group":
                    col["group"] = str(v)
                else:
                    col["raiseload"] = bool(v)
            call = call.args[0]
        optional = False
        if ann is not None:
            if not (isinstance(ann, ast.Subscript) and (dotted(ann.value) or "").split(".")[-1] == "Mapped"):
                raise self.err(f"column {name}: annotate with Mapped[...]", stmt, module)
            inner = ann.slice
            if isinstance(inner, ast.Constant) and isinstance(inner.value, str):
                inner = ast.parse(inner.value, mode="eval").body
            parts = self._union_parts(inner)
            optional = any(isinstance(p, ast.Constant) and p.value is None for p in parts)
            parts = [p for p in parts if not (isinstance(p, ast.Constant) and p.value is None)]
            if isinstance(inner, ast.Subscript) and dotted(inner.value) in {"Optional", "typing.Optional"}:
                optional, parts = True, [inner.slice]
            if len(parts) == 1:
                p = parts[0]
                pn = dotted(p) or (dotted(p.value) if isinstance(p, ast.Subscript) else None)
                t = self.resolve(module, p.value if isinstance(p, ast.Subscript) else p)
                if pn in self.PY_COL_TYPES:
                    col["ty"] = self.PY_COL_TYPES[pn]
                elif isinstance(t, Ext) and t.dotted == "datetime.datetime":
                    col["ty"] = "DateTime"
                elif isinstance(t, Ext) and t.dotted == "datetime.date":
                    col["ty"] = "Date"
                elif isinstance(t, Ext) and t.dotted == "uuid.UUID":
                    col["ty"] = "Uuid"
                elif isinstance(t, Ext) and t.dotted == "decimal.Decimal":
                    col["ty"] = "Numeric"
                elif isinstance(t, Ext) and t.dotted == "datetime.time":
                    col["ty"] = "Time"
                elif pn in {"dict", "list"}:
                    col["ty"] = "Json"
                elif isinstance(t, Sym) and self.enum_kind(t):
                    col["ty"] = f"Enum(&{self.enum_col(t, False, t.name.lower())})"
                key = self.annotation_key(p, module) if tam else None
                if tam and isinstance(t, Sym) and ("unsupported", t) in tam:
                    k, base = tam[("unsupported", t)]
                    raise self.err(f"column {name}: the type_annotation_map of {base.name} maps `{ast.unparse(k)}` "
                                   "(a project class as a key is not supported)", stmt, module)
                if tam and key not in tam and isinstance(p, ast.Subscript) and self.annotation_key(p.value, module) in tam:
                    # `list[int]` with only `list` in the map: SQLAlchemy's lookup by the generic's origin
                    raise self.err(f"column {name}: `{ast.unparse(p)}` would take the type_annotation_map entry of "
                                   f"`{ast.unparse(p.value)}`: not supported (map the generic as written)", stmt, module)
                if tam and key in tam:
                    # the base's type_annotation_map: its type for this annotation (an explicit type still wins)
                    v, vmod = tam[key]
                    inner_col = self.column(name, None, ast.copy_location(ast.Call(
                        func=ast.Name("mapped_column", ast.Load()), args=[v], keywords=[]), v), vmod, stmt)
                    col["ty"], col["jsonb"] = inner_col["ty"], inner_col["jsonb"]
        if call is not None:
            if not isinstance(call, ast.Call):
                raise self.err(f"column {name}: expected mapped_column(...)", stmt, module)
            type_kw = next((k for k in call.keywords if k.arg == "type_"), None)
            if type_kw is not None:
                # `mapped_column(ForeignKey(...), type_=BigInteger)`: the type given by keyword (with a positional one
                # too, SQLAlchemy raises)
                if any(not (isinstance(a, ast.Constant) and isinstance(a.value, str)) and not self.is_schema_item(a, module)
                       for a in call.args):
                    raise self.err(f"column {name}: type_= with a positional type is not supported", type_kw, module)
                call = ast.copy_location(ast.Call(func=call.func, args=[*call.args, type_kw.value],
                                                  keywords=[k for k in call.keywords if k is not type_kw]), call)
            for a in call.args:
                amod = module
                t0 = self.resolve(module, a) if isinstance(a, (ast.Name, ast.Attribute)) else None
                if isinstance(t0, Sym):
                    # a type defined once as a module constant (`Money = Numeric(10, 2, asdecimal=False)`)
                    dn = self.ix.definition(t0)
                    if isinstance(dn, (ast.Assign, ast.AnnAssign)) and isinstance(dn.value, ast.Call):
                        a, amod = dn.value, t0.module
                # `T.with_variant(V, "postgresql")` is V on PostgreSQL; a variant for another dialect is T
                while (isinstance(a, ast.Call) and isinstance(a.func, ast.Attribute) and a.func.attr == "with_variant"
                       and len(a.args) == 2 and not a.keywords):
                    dialects = literal(a.args[1], self.src(amod))
                    dialects = (dialects,) if isinstance(dialects, str) else tuple(dialects)
                    a = a.args[0] if "postgresql" in dialects else a.func.value
                if isinstance(a, ast.Call):
                    # a project function that only returns a type (`def _enum(cls, name): return Enum(cls, name=name)`)
                    f0 = self.resolve(amod, a.func)
                    if isinstance(f0, Sym) and isinstance(self.ix.definition(f0), (ast.FunctionDef, ast.AsyncFunctionDef)):
                        a, amod = self.inline_type_factory(f0, a, amod, name)
                d = dotted(a.func) if isinstance(a, ast.Call) else dotted(a)
                last = (d or "").split(".")[-1]
                fn_t = self.resolve(amod, a.func if isinstance(a, ast.Call) else a)
                if isinstance(fn_t, Ext) and fn_t.dotted in {"sqlalchemy.Enum", "sqlalchemy.types.Enum", "sqlalchemy.dialects.postgresql.ENUM"}:
                    if not (isinstance(a, ast.Call) and a.args):
                        raise self.err(f"column {name}: Enum() needs the Python enum class", a, module)
                    et = self.resolve(amod, a.args[0])
                    if not (isinstance(et, Sym) and self.enum_kind(et)):
                        raise self.err(f"column {name}: Enum() of string values is not supported (pass the enum class)", a, module)
                    by_value = any(k.arg == "values_callable" for k in a.keywords)
                    native = True
                    pg_name = et.name.lower()
                    for k in a.keywords:
                        if k.arg == "native_enum":
                            native = bool(self.const(k.value, amod))
                        elif k.arg == "name":
                            pg_name = self.const(k.value, amod)
                        elif k.arg not in {"values_callable", "create_type", "create_constraint", "validate_strings", "length", "inherit_schema", "schema"}:
                            raise self.err(f"column {name}: Enum({k.arg}=) is not supported", k, module)
                    col["ty"] = f"Enum(&{self.enum_col(et, by_value, pg_name if native else None)})"
                    continue
                if isinstance(fn_t, Ext) and fn_t.dotted in {"sqlmodel.sql.sqltypes.UTCDateTime", "sqlmodel.UTCDateTime"}:
                    col["ty"] = "UtcDateTime"
                    continue
                if isinstance(fn_t, Sym) and self.is_type_decorator(fn_t):
                    if isinstance(a, ast.Call):
                        for x in [*a.args, *(k.value for k in a.keywords)]:
                            if not isinstance(x, ast.Constant):
                                raise self.err(f"column {name}: {fn_t.name}(...) arguments must be literals", x, amod)
                    col["ty"] = f"Decorated(&{self.type_dec(fn_t)})"
                    continue
                if isinstance(fn_t, Ext) and fn_t.dotted in {"sqlalchemy.ARRAY", "sqlalchemy.types.ARRAY", "sqlalchemy.dialects.postgresql.ARRAY"}:
                    item = a.args[0] if isinstance(a, ast.Call) and len(a.args) == 1 else None
                    item_t = (dotted(item.func) if isinstance(item, ast.Call) else dotted(item)) if item is not None else None
                    if (item_t or "").split(".")[-1] not in {"String", "VARCHAR", "Text", "TEXT", "Unicode", "UnicodeText"}:
                        raise self.err(f"column {name}: only ARRAY(String) is supported", a, amod)
                    for k in a.keywords:
                        if k.arg != "dimensions" or literal(k.value, self.src(amod)) != 1:
                            raise self.err(f"column {name}: ARRAY({k.arg}=) is not supported", k, amod)
                    col["ty"] = "StrArray"
                    continue
                if last in {"Numeric", "NUMERIC", "DECIMAL"}:
                    asdecimal = True
                    if isinstance(a, ast.Call):
                        for k in a.keywords:
                            if k.arg == "asdecimal":
                                asdecimal = bool(literal(k.value, self.src(amod)))
                            elif k.arg not in {"precision", "scale", "decimal_return_scale"}:
                                raise self.err(f"column {name}: Numeric({k.arg}=) is not supported", k, amod)
                    col["ty"] = "Numeric" if asdecimal else "NumFloat"
                    continue
                if last in self.SA_TYPES:
                    col["ty"] = self.SA_TYPES[last]
                    col["jsonb"] = last == "JSONB"  # its comparator: @>, <@, ?, ?|, ?&
                    if isinstance(a, ast.Call):
                        # String(50), Text(), JSON(): length and no-op options only
                        ok = {"length", "timezone", "as_uuid", "astext_type"}
                        for k in a.keywords:
                            if k.arg == "none_as_null" and last in {"JSON", "JSONB"}:
                                if literal(k.value, self.src(amod)):
                                    col["ty"] = "JsonNull"  # Python None stored as SQL NULL, not JSON null
                                continue
                            if k.arg not in ok:
                                raise self.err(f"column {name}: {last}({k.arg}=) is not supported", k, amod)
                            if k.arg == "as_uuid" and not literal(k.value, self.src(amod)):
                                raise self.err(f"column {name}: {last}(as_uuid=False) is not supported (values are uuid.UUID)", k, amod)
                        if len(a.args) > 1 or (a.args and last not in {"String", "VARCHAR", "Unicode", "CHAR", "LargeBinary"}):
                            raise self.err(f"column {name}: {ast.unparse(a)} is not supported", a, amod)
                    if last in {"DateTime", "TIMESTAMP"}:
                        pass
                elif last in {"DateTime", "TIMESTAMP"}:
                    tz = False
                    if isinstance(a, ast.Call):
                        for kw in a.keywords:
                            if kw.arg == "timezone":
                                tz = bool(literal(kw.value, self.src(amod)))
                        if a.args:
                            tz = bool(literal(a.args[0], self.src(amod)))
                    col["ty"] = "DateTimeTz" if tz else "DateTime"
                elif last == "Identity":
                    # GENERATED ... AS IDENTITY: generated by the database like a server default
                    for k in getattr(a, "keywords", []):
                        if k.arg not in {"always", "start", "increment", "minvalue", "maxvalue", "cycle", "cache"}:
                            raise self.err(f"column {name}: Identity({k.arg}=) is not supported", k, amod)
                    col["server_default"] = True
                elif last == "ForeignKey":
                    target = literal(a.args[0], self.src(amod)) if isinstance(a, ast.Call) and a.args else None
                    col["fk"] = str(target).split(".")[0] if target else None
                    col["fk_col"] = str(target).split(".")[1] if target and "." in str(target) else None
                elif isinstance(a, ast.Constant) and isinstance(a.value, str):
                    # `extra_data = Column("metadata", JSON)`: the SQL name, the attribute stays the key
                    col["sql"] = a.value
                else:
                    raise self.err(f"column {name}: unsupported column type `{ast.unparse(a)}`", a, module)
            for kw in call.keywords:
                if kw.arg == "primary_key":
                    col["pk"] = bool(literal(kw.value, self.src(module)))
                elif kw.arg == "nullable":
                    col["nullable"] = bool(literal(kw.value, self.src(module)))
                elif kw.arg in ("default", "insert_default"):
                    # `default=None`: no default (the column stays out of the INSERT and of the instance dict)
                    if not (isinstance(kw.value, ast.Constant) and kw.value.value is None):
                        col["default"] = self.col_default(kw.value, module, name)
                elif kw.arg == "server_default":
                    col["server_default"] = True
                elif kw.arg == "onupdate":
                    col["onupdate"] = self.col_default(kw.value, module, name)
                elif kw.arg == "deferred":
                    col["deferred"] = col["deferred"] or bool(literal(kw.value, self.src(module)))
                elif kw.arg == "deferred_group":
                    # a group or a raiseload implies deferred=True (SQLAlchemy's mapped_column)
                    col["group"] = str(literal(kw.value, self.src(module)))
                    col["deferred"] = True
                elif kw.arg == "deferred_raiseload":
                    col["raiseload"] = bool(literal(kw.value, self.src(module)))
                    col["deferred"] = col["deferred"] or col["raiseload"]
                elif kw.arg == "autoincrement":
                    col["autoincrement"] = bool(literal(kw.value, self.src(module)))
                    col["autoincrement_set"] = True
                elif kw.arg == "name" and literal(kw.value, self.src(module)) != name:
                    raise self.err(f"column {name}: name= different from the attribute is not supported", kw, module)
                elif kw.arg in {"index", "unique", "comment", "doc", "info", "server_onupdate", "name"}:
                    pass
                else:
                    raise self.err(f"column {name}: unsupported option {kw.arg}=", kw, module)
        if col["ty"] is None and col.get("fk") and col.get("fk_col"):
            # SQLAlchemy types a foreign key column without a type like the column it references
            col["ty"] = self.fk_type(col["fk"], col["fk_col"], set())
        if col["ty"] is None:
            raise self.err(f"column {name}: cannot infer its SQL type", stmt, module)
        if col["nullable"] is None:
            col["nullable"] = optional and not col["pk"]
        return col

    def col_default(self, node, module, name) -> str:
        """`ColDefault::...` text for a SQLAlchemy column `default=`."""
        d = dotted(node)
        if d in {"list", "dict"}:
            return f"Value({self.dflt(node, module, factory=True)})"
        if isinstance(node, ast.Call):
            t = self.resolve(module, node.func)
            sql = isinstance(t, Ext) and (t.dotted.startswith("sqlalchemy.func.") or t.dotted in {"sqlalchemy.text", "sqlalchemy.sql.text"})
            return f"Dyn({self.dyn_default(node, module, 'expr' if sql else 'once')})"
        if isinstance(node, (ast.Name, ast.Attribute, ast.Lambda)):
            try:
                return f"Value({self.dflt(node, module)})"
            except TranspileError:
                return f"Dyn({self.dyn_default(node, module, 'call')})"
        return f"Value({self.dflt(node, module)})"

    def sa_type_class(self, dotted: str, node, module: str) -> str | None:
        """`module.qualname` of a SQLAlchemy column type class (`sqlalchemy.JSON`), None for anything else."""
        if not dotted.startswith("sqlalchemy."):
            return None
        import importlib
        parts = dotted.split(".")
        obj = None
        for i in range(len(parts) - 1, 0, -1):
            try:
                obj = importlib.import_module(".".join(parts[:i]))
            except ImportError:
                continue
            for a in parts[i:]:
                obj = getattr(obj, a, None)
            break
        try:
            from sqlalchemy.types import TypeEngine
        except ImportError:
            raise self.err("isinstance() with a SQLAlchemy type is resolved by SQLAlchemy: install it next to py2axum "
                           "(pip install sqlalchemy)", node, module) from None
        if isinstance(obj, type) and issubclass(obj, TypeEngine):
            return f"{obj.__module__}.{obj.__qualname__}"
        return None

    def sa_table(self, sym: Sym, node, module: str) -> str:
        """`TABLE_x`: `Model.__table__` as SQLAlchemy builds it (the mapped classes rebuilt by py2axum/ddl.py):
        column keys, names, nullability, primary key, and each type's class (MRO), `str()` and `repr()`."""
        done = self.__dict__.setdefault("sa_tables", {})
        if sym in done:
            return done[sym]
        from . import ddl
        try:
            import sqlalchemy  # noqa: F401
        except ImportError:
            raise self.err("Model.__table__ is read from SQLAlchemy: install it next to py2axum (pip install sqlalchemy)",
                           node, module) from None
        b = ddl.DdlBuilder(self, sym, module, None)
        base = b.root_base(sym)
        if base is None:
            raise self.err(f"{sym.name}.__table__: {sym.name} is not a mapped class of a declarative base", node, module)
        b.base = base
        b.build()
        table = getattr(b.value(sym, self.ix.definition(sym), sym.module), "__table__", None)
        if table is None or type(table).__name__ != "Table":
            raise self.err(f"{sym.name}.__table__ is not a Table", node, module)
        cols = []
        for c in table.columns:
            try:
                text, rep = str(c.type), repr(c.type)
            except Exception as e:
                raise self.err(f"{sym.name}.__table__: column {c.name}: SQLAlchemy cannot render its type "
                               f"({type(e).__name__}: {e})", node, module) from None
            mro = [f"{k.__module__}.{k.__qualname__}" for k in type(c.type).__mro__ if k is not object]
            cols.append(f"{RT}::satable::ColInfo {{ table: {rs(table.name)}, key: {rs(c.key)}, name: {rs(c.name)}, "
                        f"nullable: {str(bool(c.nullable)).lower()}, primary_key: {str(bool(c.primary_key)).lower()}, "
                        f"type_str: {rs(text)}, type_repr: {rs(rep)}, mro: &[{', '.join(rs(m) for m in mro)}] }}")
        name = f"TABLE_{mod_ident(sym.module)}__{ident(sym.name)}"
        done[sym] = name
        self.items.append(f"static {name}: {RT}::satable::TableInfo = {RT}::satable::TableInfo {{ name: {rs(table.name)}, "
                          f"cols: &[{', '.join(cols)}] }};")
        return name

    def copies_objects(self) -> bool:
        """Some translated code calls `copy`, `pickle` or `copyreg` (`pickle.dumps(obj)`, `copy.copy` translated
        from source) or a copy method itself (`obj.__getstate__()`, `super().__reduce__()`): these call a class's
        `__getstate__`, `__reduce__`, `__deepcopy__`... Then such methods are refused, never ignored. A library that
        defines them for pickling (loguru's enqueue, apscheduler's job stores) without the application reaching
        that code keeps its classes: nothing would call them. Checked again once everything is compiled."""
        return "copy_use" in self.__dict__

    def note_copy(self, dotted: str | None = None, attr: str | None = None) -> None:
        """Translated code reaches copy/pickle (`dotted`, a library name) or reads a copy method (`attr`)."""
        if (dotted is not None and dotted.split(".")[0] in COPY_ROOTS) or (attr is not None and attr in COPY_DUNDERS):
            self.__dict__.setdefault("copy_use", dotted or attr)

    def is_typeddict(self, sym: Sym) -> bool:
        """A `TypedDict` class of the project (bases: TypedDict or such classes; annotations only): calling it
        builds a plain dict, nothing is validated."""
        d = self.ix.definition(sym)
        if not isinstance(d, ast.ClassDef) or not d.bases or d.decorator_list:
            return False
        for b in d.bases:
            t = self.resolve(sym.module, b)
            if not ((isinstance(t, Ext) and t.dotted in {"typing.TypedDict", "typing_extensions.TypedDict"})
                    or (isinstance(t, Sym) and t != sym and self.is_typeddict(t))):
                return False
        return all(isinstance(s, (ast.AnnAssign, ast.Pass)) and getattr(s, "value", None) is None
                   or isinstance(s, ast.Expr) and isinstance(s.value, ast.Constant) for s in d.body)

    def is_plain_class(self, sym: Sym) -> bool:
        """A project class that is nothing else (no decorator; bases: `ABC` and/or one such class): instances
        with free attributes."""
        d = self.ix.definition(sym)
        return (isinstance(d, ast.ClassDef) and (self.plain_bases(sym) is not None or self.is_http_middleware(sym))
                and all(self.abcmeta_kw(sym, k) for k in d.keywords) and not d.decorator_list and sym not in self.fe.model_syms
                and sym not in self.fe.schema_syms and sym not in self.fe.dataclass_syms)

    def abcmeta_kw(self, sym: Sym, kw: ast.keyword) -> bool:
        """`metaclass=ABCMeta`: the metaclass `ABC` gives its subclasses (abstract methods block instantiation)."""
        t = self.resolve(sym.module, kw.value) if kw.arg == "metaclass" else None
        return isinstance(t, Ext) and t.dotted == "abc.ABCMeta"

    def abc_class(self, sym: Sym) -> bool:
        """Is ABCMeta the metaclass of this plain class (base `ABC`, `metaclass=ABCMeta`, or a base that has it)?
        Only then does `@abstractmethod` block `C()` (CPython: object.__new__ reads `__abstractmethods__`, which
        ABCMeta alone computes)."""
        d = self.ix.definition(sym)
        if any(self.abcmeta_kw(sym, k) for k in d.keywords):
            return True
        for b in d.bases:
            t = self.resolve(sym.module, b.value if isinstance(b, ast.Subscript) else b)
            if (isinstance(t, Ext) and t.dotted == "abc.ABC") or (isinstance(t, Sym) and t != sym and self.abc_class(t)):
                return True
        return False

    def plain_bases(self, sym: Sym) -> list[Sym] | None:
        """The project base of a plain class (`[]`: none, `ABC` or `object` only); None if a base is anything else."""
        out = []
        for b in self.ix.definition(sym).bases:
            if isinstance(b, ast.Subscript):
                # `Comparator[str]`, `Serializer[str]`, `Base["T"]`: a generic class's parameters only type it (CPython
                # makes the class's `__orig_bases__` of them, nothing more)
                b = b.value
            t = self.resolve(sym.module, b)
            if isinstance(t, Ext) and t.dotted in GENERIC_BASES:
                continue  # `Generic[T]`, `Protocol[T]`: typing only
            if isinstance(t, Ext) and t.dotted in ROUTE_BASES | REQUEST_BASES:
                # a custom route class: FastAPI calls its `get_route_handler()` (RouteBuilder.custom_route); a Request
                # subclass wraps the request (`GzipRequest(request.scope, request.receive)`, web::request_subclass)
                continue
            if (isinstance(t, Ext) and t.dotted in {"abc.ABC", "logging.Filter"}) or dotted(b) == "object":
                # logging.Filter: the logging runtime calls the instance's own `filter(record)`
                continue
            if isinstance(t, Sym) and t != sym and isinstance(self.ix.definition(t), ast.ClassDef) and self.is_plain_class(t):
                out.append(t)
                continue
            return None
        return out if len(out) <= 1 else None

    def request_subclass(self, sym: Sym) -> bool:
        """`class GzipRequest(Request)`: a plain class wrapping the request (its attributes fall back to it)"""
        d = self.ix.definition(sym)
        return isinstance(d, ast.ClassDef) and any(
            isinstance(t := self.resolve(sym.module, b), Ext) and t.dotted in REQUEST_BASES for b in d.bases)

    def is_http_middleware(self, sym: Sym) -> bool:
        """`class X(BaseHTTPMiddleware)`: a plain class whose `dispatch` the middleware stack calls."""
        d = self.ix.definition(sym)
        if not (isinstance(d, ast.ClassDef) and len(d.bases) == 1):
            return False
        t = self.ix.resolve_expr(sym.module, d.bases[0])
        return isinstance(t, Ext) and libmap.canonical(t.dotted) in MW_BASE

    def plain_class(self, sym: Sym) -> SchemaInfo:
        self.class_edge(sym)
        if sym in self.class_errors:
            raise self.class_errors[sym]
        if sym in self.schemas:
            return self.schemas[sym]
        node = self.ix.definition(sym)
        module = sym.module
        base = f"{mod_ident(module)}__{ident(sym.name)}"
        info = SchemaInfo(sym, f"SCHEMA_{base}", f"CLS_{base}", [], False, "Ignore")
        info.__dict__["open"] = True
        attrs = {}
        parent = None
        own: list = []
        ro_props: set[str] = set()  # the `@property`s (a cached_property is assignable)
        abstract: set[str] = set()
        try:
            for b in self.plain_bases(sym) or []:
                # single inheritance: the base's methods, __init__, class attributes and abstract methods
                parent = self.plain_class(b)
                abstract = set(parent.__dict__.get("abstract", ()))
                if parent.__dict__.get("init") is not None:
                    info.__dict__["init"] = parent.__dict__["init"]
                if parent.__dict__.get("new") is not None:
                    info.__dict__["new"] = parent.__dict__["new"]
            for st in node.body:
                if isinstance(st, ast.FunctionDef) and st.name == "__new__":
                    # `__new__(cls, ...)`: called with the class; `__init__` follows only on an instance of it
                    if st.decorator_list:
                        raise self.err(f"class {sym.name}: a decorated __new__ is not supported", st.decorator_list[0], module)
                    info.__dict__["new"] = Sym(module, f"{node.name}.__new__")
                    self.function(info.__dict__["new"])  # compiled even when the class is only called as a value
                    continue
                if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    if st.name in SCHEMA_HOOKS:
                        continue  # read statically where the class types a field (core_schema_rewrite)
                    if st.name in COPY_DUNDERS and not self.copies_objects():
                        self.__dict__.setdefault("copy_ignored", []).append((sym, st, module))
                        continue  # only pickle and copy call them, and the application uses neither
                    if is_dunder(st.name) and st.name != "__init__" and st.name not in DISPATCHED_DUNDERS:
                        raise self.err(f"class {sym.name}: method {st.name} is not supported", st, module)
                    last = [((dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "").split(".")[-1] for d in st.decorator_list]
                    acc = property_accessor(st)
                    if acc is not None:
                        # `@name.setter` after `@property def name`: the property's setter (`obj.name = v` calls it);
                        # the getter stays the first def of the name
                        if acc == "deleter":
                            raise self.err(f"{sym.name}.{st.name}: decorator @{st.name}.deleter is not supported "
                                           "(`del obj.name` is not translated): remove the deleter or keep the route in Python", st, module)
                        if not any(n == st.name and prop for n, prop, _ in own):
                            raise self.err(f"{sym.name}.{st.name}: decorator @{st.name}.setter is not supported without a `@property def {st.name}` "
                                           "earlier in the same class body (an inherited or assigned property)", st, module)
                        if len(st.decorator_list) != 1 or any(n == SETTER_PREFIX + st.name for n, _, _ in own):
                            raise self.err(f"{sym.name}.{st.name}: decorator @{st.name}.setter is not supported beside another decorator, "
                                           "or as a second setter", st, module)
                        own.append((SETTER_PREFIX + st.name, False, Sym(module, f"{node.name}.{st.name}{SETTER_SUFFIX}")))
                        continue
                    msym = Sym(module, f"{node.name}.{st.name}")
                    if any(ln == "cachedmethod" and cachedmethod_deco(self, msym, st) is not None for ln in last):
                        pass  # applied by its method wrapper (method_wrapper)
                    else:
                        vdecs, cached = self.method_value_decos(sym, st, module)
                        if cached:
                            self.__dict__.setdefault("cached_prop_syms", set()).add(msym)
                        elif vdecs:
                            self.__dict__.setdefault("vdeco_methods", set()).add(msym)
                            self.decorated_value(msym)  # the decorators compile (or refuse) with the class, not at the end
                    if "abstractmethod" in last:
                        abstract.add(st.name)
                    else:
                        abstract.discard(st.name)
                    if st.name == "__init__":
                        info.__dict__["init"] = Sym(module, f"{node.name}.__init__")
                    else:
                        # a later def of the same name rebinds it (the setter of a property aside)
                        own[:] = [m for m in own if m[0] not in (st.name, SETTER_PREFIX + st.name)]
                        ro_props.discard(st.name)
                        own.append((st.name, "property" in last or msym in self.__dict__.get("cached_prop_syms", ()), msym))
                        if "property" in last:
                            ro_props.add(st.name)
                elif (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name)
                      and st.targets[0].id == "__hash__" and isinstance(st.value, ast.Constant) and st.value.value is None):
                    info.__dict__["unhashable"] = True
                elif (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name)
                      and st.targets[0].id == "__hash__" and dotted(st.value) == "object.__hash__"):
                    info.__dict__["hash_id"] = True  # hashed by identity although it defines __eq__
                elif isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name):
                    attrs[st.targets[0].id] = st.value
                    if st.targets[0].id == "__slots__":
                        v = st.value
                        if isinstance(v, ast.Constant) and isinstance(v.value, str):
                            v = ast.Tuple([v], ast.Load())  # `__slots__ = "tokens"`: one slot
                        if not (isinstance(v, (ast.Tuple, ast.List)) and all(isinstance(e, ast.Constant) and isinstance(e.value, str) for e in v.elts)):
                            raise self.err(f"class {sym.name}: __slots__ must be a literal tuple of names", st, module)
                        info.__dict__["slots"] = [e.value for e in v.elts]
                elif isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name):
                    if st.value is not None:
                        attrs[st.target.id] = st.value
                elif startup_skipped(st):
                    pass  # `if TYPE_CHECKING:` in the class body: annotations only
                elif not isinstance(st, (ast.Pass, ast.Expr)):
                    raise self.err(f"unsupported statement in class {sym.name}", st, module)
        except TranspileError as e:
            self.class_errors[sym] = e
            raise
        # a name redefined here hides the inherited setter along with the getter; a property without setter refuses assignment
        # (`ro:<name>` marks it: a class attribute reads as a property too)
        own += [(RO_PREFIX + n, False, sy) for n, prop, sy in own if prop and n in ro_props and SETTER_PREFIX + n not in {m[0] for m in own}]
        names = {m[0] for m in own} | set(attrs)
        names |= {pre + n for n in names for pre in (SETTER_PREFIX, RO_PREFIX)}
        info.methods = own + [m for m in (parent.methods if parent else []) if m[0] not in names]
        # CPython: `__eq__` in the class body without `__hash__` sets `__hash__ = None`; otherwise inherited
        info.__dict__["hash_kind"] = ("Unhashable" if info.__dict__.get("unhashable") else "Id" if info.__dict__.get("hash_id")
                                      else "Unhashable" if any(m[0] == "__eq__" for m in own)
                                      else parent.__dict__.get("hash_kind", "Id") if parent else "Id")
        info.__dict__["abstract"] = sorted(abstract) if self.abc_class(sym) else []
        if parent is not None:
            info.__dict__["base_cls"] = parent.cls
        info.__dict__["class_attrs"] = {n: g for n, g in (parent.__dict__.get("class_attrs", {}) if parent else {}).items()
                                        if n not in names}
        for n, value in attrs.items():
            value = self.class_attr_value(sym, n, value, module)
            g = f"ca_{base}__{ident(n)}"
            info.__dict__["class_attrs"][n] = g
            fc = FnCompiler(self, module, None, g)
            code = fc.expr(value)
            body = "\n".join("        " + l for l in fc.lines)
            # one shared value (a class attribute), read through instances as a property
            self.items.append(
                f"static G_{g}: {RT}::Global = {RT}::Global::new();\n"
                f"pub async fn {g}(cx: &Cx) -> R {{\n"
                f"    G_{g}.get(cx, |cx| Box::pin(async move {{\n{body}\n        Ok({code})\n    }})).await\n}}\n"
                f"fn {g}_prop<'a>(cx: &'a Cx, _slf: V, _args: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin({g}(cx)) }}\n"
                # `cls.X = v` on the class held as a value (its own attribute only)
                f"fn {g}_setp<'a>(cx: &'a Cx, _slf: V, args: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{ "
                f"{g}(cx).await?; G_{g}.set(args.into_iter().next().unwrap_or(V::None)); Ok(V::None) }}) }}"
            )
        info.__dict__["own_class_attrs"] = list(attrs)
        self.schemas[sym] = info
        return info

    def method_value_decos(self, csym: Sym, st, module: str) -> tuple[list, bool]:
        """The decorators of a plain class's method applied as values (`@wraps` wrappers, `@contextmanager`, a project
        decorator...: `d(f)` built once at class creation, a function bound to the instance), and whether it is a
        `functools.cached_property`. property, classmethod, staticmethod and abstractmethod are handled apart; a decorator
        class (descriptor protocol), or one beside them, is refused."""
        fc = FnCompiler(self, module, None, "deco")
        shim = types.SimpleNamespace(decorator_list=[
            d for d in st.decorator_list
            if ((dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "").split(".")[-1] != "abstractmethod"])
        try:
            other = value_decorators(shim, fc.static_ref)
        except TranspileError as e:
            raise self.err(e.msg, e.node or st, module) from None
        cached = False
        vdecs = []
        for d in other:
            head = d.func if isinstance(d, ast.Call) else d
            ref = fc.static_ref(head)
            if ref == Ext("functools.cached_property") and not isinstance(d, ast.Call):
                cached = True
                continue
            if isinstance(ref, Sym) and isinstance(self.ix.definition(ref), ast.ClassDef):
                raise self.err(f"{csym.name}.{st.name}: decorator @{ast.unparse(d)} is not supported (it is a class: a "
                               "descriptor, bound by its `__get__`); use a function decorator, `@property` or "
                               "`functools.cached_property`", d, module)
            vdecs.append(d)
        if cached and (vdecs or len(shim.decorator_list) != 1):
            raise self.err(f"{csym.name}.{st.name}: decorator @cached_property is not supported beside another decorator", st, module)
        if vdecs and len(shim.decorator_list) != len(st.decorator_list):
            raise self.err(f"{csym.name}.{st.name}: decorator @abstractmethod is not supported beside the decorator @{ast.unparse(vdecs[0])}", st, module)
        return vdecs, cached

    def class_attr_value(self, sym: Sym, name: str, value: ast.expr, module: str) -> ast.expr:
        """The expression of a plain class's attribute, read as is through the class and its instances:
        `staticmethod(f)` is `f` (never bound); a function of the project or a lambda would be bound to the instance
        read through it (a method), and `classmethod(f)` to the class: refused."""
        if isinstance(value, ast.Call) and isinstance(value.func, ast.Name) and value.func.id in {"staticmethod", "classmethod"} \
                and self.ix.resolve(module, value.func.id) is None:
            if value.func.id == "classmethod" or len(value.args) != 1 or value.keywords:
                raise self.err(f"class {sym.name}: `{name} = {ast.unparse(value)}` is not supported (only staticmethod(f))",
                               value, module)
            return value.args[0]
        t = self.resolve(module, value) if isinstance(value, (ast.Name, ast.Attribute)) else None
        if isinstance(value, ast.Lambda) or (isinstance(t, Sym) and isinstance(self.ix.definition(t), (ast.FunctionDef, ast.AsyncFunctionDef))):
            raise self.err(f"class {sym.name}: the function `{name} = {ast.unparse(value)}` as a class attribute is not "
                           "supported (Python binds it to the instance it is read through): define a method, or wrap it in "
                           "staticmethod(...)", value, module)
        return value

    def exc_init(self, sym: Sym) -> Sym | None:
        """The `__init__` a project exception runs: its own or its nearest project base's."""
        self.exception(sym)
        init = self.__dict__.get("exc_inits", {}).get(sym)
        if init is not None:
            return init
        for b in self.ix.definition(sym).bases:
            t = self.resolve(sym.module, b)
            if isinstance(t, Sym):
                found = self.exc_init(t)
                if found is not None:
                    return found
        return None

    def dataclass(self, sym: Sym) -> SchemaInfo:
        """A plain `@dataclass`: an instance with ordered fields, no validation (TD Any), its methods."""
        self.class_edge(sym)
        if sym in self.class_errors:
            raise self.class_errors[sym]
        if sym in self.schemas:
            return self.schemas[sym]
        node = self.fe.dataclass_syms[sym]
        module = sym.module
        base = f"{mod_ident(module)}__{ident(sym.name)}"
        info = SchemaInfo(sym, f"SCHEMA_{base}", f"CLS_{base}", [], False, "Ignore")
        ntuple = sym in self.fe.namedtuple_syms
        info.__dict__["dataclass"] = not ntuple
        info.__dict__["ntuple"] = ntuple
        try:
            if ntuple:
                # `class P(NamedTuple)`: a tuple of named fields, immutable, hashed and compared as a tuple
                if (node.decorator_list or node.keywords or len(node.bases) != 1
                        or getattr(node, "type_params", None)):
                    raise self.err(f"NamedTuple {sym.name}: decorators, other bases, keywords and type parameters are "
                                   "not supported", node, module)
                info.__dict__["frozen"] = True
            for d in node.decorator_list:
                wc = self.resolve(module, d.func) if isinstance(d, ast.Call) else None
                if isinstance(wc, Ext) and wc.dotted in {"pydantic.with_config", "pydantic.config.with_config"}:
                    self.with_config(info, sym, d, module)
                    continue
                if isinstance(d, ast.Call) and d.args:
                    raise self.err(f"@dataclass options are not supported ({sym.name})", d, module)
                for k in getattr(d, "keywords", []):
                    if k.arg == "frozen":
                        info.__dict__["frozen"] = bool(self.const(k.value, module))
                    elif k.arg == "eq" and self.const(k.value, module) is False:
                        # no generated __eq__: identity (object's) or the base's, and the base's __hash__
                        info.__dict__["eq_id"] = True
                    elif k.arg not in {"eq", "order", "repr", "init"} or self.const(k.value, module) is not (k.arg != "order"):
                        raise self.err(f"@dataclass({k.arg}=) is not supported ({sym.name})", d, module)
                dt = self.resolve(module, d.func if isinstance(d, ast.Call) else d)
                if isinstance(dt, Ext) and dt.dotted == "pydantic.dataclasses.dataclass":
                    if isinstance(d, ast.Call):
                        raise self.err(f"@pydantic.dataclasses.dataclass(...) options are not supported ({sym.name})", d, module)
                    info.__dict__["pydantic_dc"] = True
                    continue
                if not is_ext(self.resolve(module, d.func if isinstance(d, ast.Call) else d), "dataclasses.dataclass"):
                    raise self.err(f"dataclass {sym.name}: decorator @{ast.unparse(d)} is not supported", d, module)
            parent = None
            if (node.bases or node.keywords) and not ntuple:
                # one plain class as base (SQLAlchemy's hybrid Comparator for a hybrid value object): its methods
                # (but the __eq__ and __repr__ the dataclass generates), not its __init__; its fields are none
                pb = self.plain_bases(sym)
                if node.keywords or not pb:
                    raise self.err(f"dataclass {sym.name}: base classes are not supported (a plain class only)", node, module)
                parent = self.plain_class(pb[0])
                if parent.__dict__.get("class_attrs") or parent.__dict__.get("abstract") or parent.__dict__.get("new"):
                    raise self.err(f"dataclass {sym.name}: a base class with class attributes, abstract methods or "
                                   "__new__ is not supported", node, module)
                info.__dict__["base_cls"] = parent.cls
            if info.__dict__.get("eq_id"):
                info.__dict__["hash_kind"] = parent.__dict__.get("hash_kind", "Id") if parent else "Id"
            for stmt in node.body:
                if isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
                    ann = stmt.annotation
                    if isinstance(ann, ast.Subscript) and (dotted(ann.value) or "").split(".")[-1] == "ClassVar":
                        if ntuple:
                            raise self.err(f"NamedTuple {sym.name}: ClassVar is not supported", stmt, module)
                        raise self.err(f"dataclass {sym.name}: ClassVar is not supported", stmt, module)
                    f = {"name": stmt.target.id, "alias": None, "default": "Required", "env": None, "any": True, "ann": (ann, module)}
                    v = stmt.value
                    if ntuple and stmt.target.id.startswith("_"):
                        raise self.err(f"NamedTuple {sym.name}: field names cannot start with an underscore: "
                                       f"{stmt.target.id!r}", stmt, module)
                    if ntuple and v is not None and not is_immutable_literal(v):
                        # the default object is shared by every instance: only immutable literals here
                        raise self.err(f"NamedTuple {sym.name}.{stmt.target.id}: only a literal immutable default "
                                       "(number, string, bytes, bool, None, tuple of those) is supported", v, module)
                    if v is not None:
                        if not ntuple and isinstance(v, ast.Call) and is_ext(self.resolve(module, v.func), "dataclasses.field"):
                            if v.args or any(k.arg not in {"default", "default_factory"} for k in v.keywords):
                                raise self.err(f"dataclass {sym.name}: field() supports default= and default_factory= only", v, module)
                            for k in v.keywords:
                                f["default"] = self.default_spec(k.value, module, k.arg == "default_factory")
                        else:
                            f["default"] = self.default_spec(v, module, False)
                    elif any(x["default"] != "Required" for x in info.fields):
                        raise self.err(f"non-default argument '{stmt.target.id}' follows default argument", stmt, module)
                    info.fields.append(f)
                elif isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    if stmt.name == "__post_init__" and not ntuple:
                        info.__dict__["post_init"] = Sym(module, f"{node.name}.__post_init__")
                        continue
                    if ntuple and (is_dunder(stmt.name) or stmt.name.startswith("_")
                                   or any(f["name"] == stmt.name for f in info.fields)):
                        # CPython refuses __new__/__init__ overrides; the tuple's own dunders and _methods stay
                        raise self.err(f"NamedTuple {sym.name}: method {stmt.name} is not supported", stmt, module)
                    if stmt.name in SCHEMA_HOOKS:
                        continue  # read statically where the class types a field (core_schema_rewrite)
                    if stmt.name in COPY_DUNDERS and not self.copies_objects():
                        self.__dict__.setdefault("copy_ignored", []).append((sym, stmt, module))
                        continue  # only pickle and copy call them, and the application uses neither
                    if is_dunder(stmt.name) and stmt.name not in DISPATCHED_DUNDERS:
                        raise self.err(f"dataclass {sym.name}: method {stmt.name} is not supported", stmt, module)
                    last = [((dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "").split(".")[-1] for d in stmt.decorator_list]
                    for d, ln in zip(stmt.decorator_list, last):
                        if ln not in {"property", "classmethod", "staticmethod"}:
                            raise self.err(f"dataclass {sym.name}.{stmt.name}: decorator @{ast.unparse(d)} is not supported", d, module)
                    info.methods.append((stmt.name, "property" in last, Sym(module, f"{node.name}.{stmt.name}")))
                elif isinstance(stmt, (ast.Pass, ast.Expr)):
                    continue
                elif ntuple:
                    raise self.err(f"unsupported statement in NamedTuple {sym.name}", stmt, module)
                else:
                    raise self.err(f"unsupported statement in dataclass {sym.name}", stmt, module)
        except TranspileError as e:
            self.class_errors[sym] = e
            raise
        if parent is not None:
            names = {m[0] for m in info.methods} | {"__init__", "__repr__"} | (set() if info.__dict__.get("eq_id") else {"__eq__"})
            info.methods += [m for m in parent.methods if m[0] not in names]
        self.schemas[sym] = info
        return info

    # model_config keys a stdlib dataclass takes from the model that holds it (or from its own @with_config)
    DC_CONFIG = {"str_strip_whitespace", "str_to_lower", "str_to_upper", "str_min_length", "str_max_length",
                 "use_enum_values"}

    def with_config(self, info: SchemaInfo, sym: Sym, d: ast.Call, module: str) -> None:
        """`@with_config(str_to_lower=True)` / `@with_config(ConfigDict(...))` / `@with_config({...})` on a stdlib
        dataclass: its own config, used instead of the one of the model holding it (the str and enum keys only)"""
        if len(d.args) == 1 and not d.keywords:
            a = d.args[0]
            if isinstance(a, ast.Call) and (dotted(a.func) or "").split(".")[-1] == "ConfigDict" and not a.args:
                items = a.keywords
            elif isinstance(a, ast.Dict) and None not in a.keys:
                items = [ast.keyword(arg=self.const(k, module), value=v) for k, v in zip(a.keys, a.values)]
            else:
                raise self.err(f"dataclass {sym.name}: @with_config(...) takes keywords, ConfigDict(...) or a dict literal", d, module)
        elif not d.args:
            items = d.keywords
        else:
            raise self.err(f"dataclass {sym.name}: @with_config(...) takes keywords, ConfigDict(...) or a dict literal", d, module)
        if bad := [k.arg for k in items if k.arg not in self.DC_CONFIG]:
            raise self.err(f"dataclass {sym.name}: @with_config({bad[0]}=) is not supported (only "
                           f"{', '.join(sorted(self.DC_CONFIG))})", d, module)
        self.config_items(info, items, module)
        info.__dict__["own_config"] = True

    def dataclass_view_cfg(self, sym: Sym, cfg: dict, ann: ast.AST, module: str) -> SchemaInfo:
        """A stdlib dataclass inside a model: validated with that model's config (pydantic pushes no config of its
        own), unless the dataclass has `@with_config`. The str and enum keys are kept (a variant of the view per
        config); strict= and extra= of the model change how the dataclass itself validates: refused."""
        view = self.dataclass_view(sym)
        if view.__dict__.get("pydantic_dc") or view.__dict__.get("own_config") or view.__dict__.get("ntuple"):
            return view
        if cfg.get("_dc_strict") or cfg.get("_dc_extra"):
            raise self.err(f"dataclass {sym.name} in a model with strict= or extra= in its model_config is not supported "
                           "(pydantic validates it with that config)", ann, module)
        keys = {"_strip": "strip", "_lower": "lower", "_upper": "upper", "_smin": "smin", "_smax": "smax",
                "_enum_values": "enum_values"}
        own = {keys[k]: v for k, v in cfg.items() if k in keys and v not in (None, False)}
        if not own:
            return view
        tag = "_".join(f"{k}{v if not isinstance(v, bool) else ''}" for k, v in sorted(own.items()))
        key = Sym(sym.module, f"{sym.name}.<pydantic:{tag}>")
        if key in self.schemas:
            return self.schemas[key]
        v2 = copy.copy(view)
        v2.__dict__ = dict(view.__dict__)
        v2.__dict__.update(own)
        v2.rust = f"{view.rust}_{tag.upper()}"
        self.schemas[key] = v2
        return v2

    def dataclass_view(self, sym: Sym) -> SchemaInfo:
        """A dataclass validated by pydantic (TypeAdapter of a stdlib or pydantic dataclass): its fields typed, extra
        inputs ignored, `dataclass_type` for anything but a dict or an instance; its instances are the class's own
        (same Class, methods and repr). Emitted next to the plain descriptor (`shared_cls`)."""
        key = Sym(sym.module, f"{sym.name}.<pydantic>")
        if key in self.schemas:
            return self.schemas[key]
        base = self.dataclass(sym)
        view = copy.copy(base)
        view.__dict__ = dict(base.__dict__)
        view.fields = [{k: v for k, v in f.items() if k != "any"} for f in base.fields]
        view.rust = base.rust + "_PYD"
        view.from_attributes = True
        view.__dict__["shared_cls"] = True
        self.schemas[key] = view
        return view

    def class_edge(self, sym: Sym) -> None:
        """The function being compiled uses class `sym` (validates, builds or serializes instances): the
        class's validators, model validators and computed fields are reachable from it (`closure_errors`)."""
        if self.cur is not None and self.cur != ("class", sym):
            self.edges.setdefault(self.cur, set()).add(("class", sym))

    def schema(self, sym: Sym) -> SchemaInfo:
        self.class_edge(sym)
        if sym in self.class_errors:
            raise self.class_errors[sym]
        if sym in self.schemas:
            return self.schemas[sym]
        cur, self.cur = self.cur, ("class", sym)
        try:
            info = self._schema(sym)
            for f in info.fields:
                ann, module = f["ann"]
                self.td(ann, module, self.model_field_cons(info, f))
            return info
        except TranspileError as e:
            self.schemas.pop(sym, None)
            self.class_errors[sym] = e
            raise
        finally:
            self.cur = cur

    @staticmethod
    def model_field_cons(info: SchemaInfo, f: dict) -> dict:
        """A field's constraints with the model_config keys that reach its type (str_*, enum values, bytes mode,
        strict= and extra= for the stdlib dataclasses it holds)"""
        cons = dict(f.get("cons", {}))
        if info.__dict__.get("strip"):
            cons["_strip"] = True
        if info.__dict__.get("lower"):
            cons["_lower"] = True
        if info.__dict__.get("upper"):
            cons["_upper"] = True
        for k in ("smin", "smax"):
            if info.__dict__.get(k) is not None:
                cons["_" + k] = info.__dict__[k]
        if f.get("ltr"):
            cons["_ltr"] = True
        if info.__dict__.get("enum_values"):
            cons["_enum_values"] = True
        if info.__dict__.get("val_json_bytes"):
            cons["_val_bytes"] = info.__dict__["val_json_bytes"]
        if info.__dict__.get("strict"):
            cons["_dc_strict"] = True
        if info.__dict__.get("finite"):
            cons["_finite"] = True
        if info.extra != "Ignore":
            cons["_dc_extra"] = info.extra
        return cons

    def flat_class_body(self, body: list) -> list:
        """A class body's statements, those under class-level `if`s included (in order)."""
        out = []
        for st in body:
            if isinstance(st, ast.If):
                out += self.flat_class_body(st.body) + self.flat_class_body(st.orelse)
            else:
                out.append(st)
        return out

    def class_namespace(self, sym: Sym, node: ast.ClassDef, module: str) -> str | None:
        """A class body that is a script (class-level `if`, defaults reading earlier class attributes):
        compiled as a function returning its namespace, evaluated once like at class creation.
        None for an ordinary body."""
        names = set()
        for st in self.flat_class_body(node.body):
            if isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name):
                names.add(st.target.id)
            elif isinstance(st, ast.Assign):
                for t in st.targets:
                    target_names(t, names)
        names.discard("model_config")
        script = any(isinstance(st, ast.If) for st in node.body)
        if not script:
            for st in node.body:
                v = st.value if isinstance(st, (ast.AnnAssign, ast.Assign)) else None
                if v is not None and any(isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load) and n.id in names
                                         for n in ast.walk(v)):
                    script = True
                    break
        if not script:
            return None

        def keep(body):
            out = []
            for st in body:
                if isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name):
                    v = st.value
                    if v is None:
                        continue
                    if isinstance(v, ast.Call) and (dotted(v.func) or "").split(".")[-1] == "Field":
                        d = next((k.value for k in v.keywords if k.arg == "default"), v.args[0] if v.args else None)
                        if d is None or (isinstance(d, ast.Constant) and d.value is Ellipsis):
                            continue
                        v = d
                    out.append(ast.copy_location(ast.Assign(targets=[ast.Name(st.target.id, ast.Store())], value=v), st))
                elif isinstance(st, ast.Assign):
                    if any(isinstance(t, ast.Name) and t.id == "model_config" for t in st.targets):
                        continue
                    out.append(st)
                elif isinstance(st, ast.If):
                    out.append(ast.copy_location(ast.If(test=st.test, body=keep(st.body) or [ast.Pass()],
                                                         orelse=keep(st.orelse)), st))
                elif isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Pass)) or (
                        isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant)):
                    continue
                else:
                    out.append(st)
            return out

        stmts = keep(node.body)
        ast.fix_missing_locations(ast.Module(body=stmts, type_ignores=[]))
        name = f"ns_{mod_ident(module)}__{ident(sym.name)}"
        fc = FnCompiler(self, module, None, name)
        fc.locals = set(names)
        for n in sorted(names):
            fc.emit(f"let mut v_{ident(n)}: V = V::Unbound;")
        fc.block(stmts)
        items = ", ".join(f"(V::str({rs(n)}), v_{ident(n)}.clone())" for n in sorted(names))
        body = "\n".join("        " + l for l in fc.lines)
        self.items.append(
            f"static G_{name}: {RT}::Global = {RT}::Global::new();\n"
            f"/// the class body of {sym.name}, run once like at class creation\n"
            f"pub async fn {name}(cx: &Cx) -> R {{\n"
            f"    G_{name}.get(cx, |cx| Box::pin(async move {{\n{body}\n"
            f"        V::dict_from(vec![{items}].into_iter().filter(|(_, v): &(V, V)| !matches!(v, V::Unbound)).collect())\n"
            f"    }})).await\n}}"
        )
        self.__dict__.setdefault("eager", []).append((module, node.lineno, name))
        self.globals[Sym(module, f"<ns {sym.name}>")] = name
        return name

    def ns_default(self, ns: str, field: str) -> str:
        """A field default read from a class namespace (a field never given a value is required)."""
        name = f"ns_dflt_{self.uid()}"
        self.items.append(
            f"fn {name}<'a>(cx: &'a Cx, _s: V, _a: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{ "
            f"{RT}::ops::getitem(&{ns}(cx).await?, &V::str({rs(field)})) }}) }}"
        )
        return name

    # ---------------------------------------------------------------- generic models

    def typevars(self, node: ast.ClassDef, module: str) -> dict:
        """The type parameters of a generic model, in order: name -> (bound, default) (AST or None).
        `Generic[T, ...]` among the bases, or the class's own `[T]` (PEP 695)."""
        out = {}
        for tp in getattr(node, "type_params", []) or []:
            if not isinstance(tp, ast.TypeVar):
                raise self.err("only TypeVar type parameters are supported", tp, module)
            out[tp.name] = (tp.bound, getattr(tp, "default_value", None))
        for b in node.bases:
            if not isinstance(b, ast.Subscript):
                continue
            t = self.resolve(module, b.value)
            if not (isinstance(t, Ext) and t.dotted in {"typing.Generic", "typing_extensions.Generic"}):
                continue
            for a in (b.slice.elts if isinstance(b.slice, ast.Tuple) else [b.slice]):
                if not isinstance(a, ast.Name):
                    raise self.err(f"`{ast.unparse(b)}`: TypeVar names only", b, module)
                d = self.ix.definition(Sym(module, a.id))
                call = d.value if isinstance(d, (ast.Assign, ast.AnnAssign)) else None
                ft = self.resolve(module, call.func) if isinstance(call, ast.Call) else None
                if not (isinstance(ft, Ext) and ft.dotted.split(".")[-1] == "TypeVar"):
                    raise self.err(f"`{a.id}` is not a TypeVar of this module", a, module)
                kw = {k.arg: k.value for k in call.keywords}
                if len(call.args) > 1 or set(kw) - {"bound", "default", "covariant", "contravariant"}:
                    raise self.err(f"TypeVar `{a.id}`: constraints and options other than bound=/default= are not supported", call, module)
                out[a.id] = (kw.get("bound"), kw.get("default"))
        return out

    def specialize(self, sym: Sym, node: ast.ClassDef, name: str, subst: dict, where, wmodule: str) -> Sym:
        """A generic model with its type parameters replaced (`Response[int]`, named like pydantic does): a copy
        of the class, compiled as a model of its own."""
        import copy
        s = Sym(sym.module, name)
        if s in self.fe.schema_syms:
            return s

        class Sub(ast.NodeTransformer):
            def visit_Name(self, n):
                return copy.deepcopy(subst[n.id]) if n.id in subst else n

        new = copy.deepcopy(node)
        new.name = name
        new.type_params = []
        new.bases = [Sub().visit(b) for b in new.bases]
        for st in new.body:
            if isinstance(st, ast.AnnAssign):
                st.annotation = Sub().visit(st.annotation)
        new._py2axum_specialized = True
        self.fe.schema_syms[s] = new
        m = self.ix.module(sym.module)
        if m is not None:
            m.defs[name] = new
        return s

    def generic_args(self, sym: Sym, sub: ast.Subscript, module: str, scope=None) -> Sym:
        """`Model[int]`: the specialized model. Its arguments are written in the model's own module (the same
        names resolve to the same things there)."""
        node = self.fe.schema_syms[sym]
        tvars = self.typevars(node, sym.module)
        if not tvars or getattr(node, "_py2axum_specialized", False):
            raise self.err(f"`{ast.unparse(sub)}`: {sym.name} is not a generic model", sub, module)
        args = sub.slice.elts if isinstance(sub.slice, ast.Tuple) else [sub.slice]
        if len(args) != len(tvars):
            raise self.err(f"`{ast.unparse(sub)}`: {len(tvars)} type parameter(s) expected", sub, module)
        shown = ", ".join(ast.unparse(a).split(".")[-1] if isinstance(a, ast.Attribute) else ast.unparse(a) for a in args)
        args, key = self.rebind_args(args, module, scope, sym.module, sub)
        name = f"{sym.name}[{shown}]"
        done = self.fe.schema_syms.get(Sym(sym.module, name))
        if done is not None and getattr(done, "_py2axum_key", key) != key:
            raise self.err(f"`{ast.unparse(sub)}`: two different `{shown}` for the same generic model", sub, module)
        out = self.specialize(sym, node, name, dict(zip(tvars, args)), sub, module)
        self.fe.schema_syms[out]._py2axum_key = key
        return out

    def rebind_args(self, args: list, module: str, scope, gmodule: str, sub: ast.AST) -> tuple[list, tuple]:
        """The arguments of `Model[X]` written in `module`, as the model's own module (`gmodule`) must read them:
        a name that means something else there (or nothing) is bound to what it means here, under a name of its
        own (`_p2ag_N`) in the model's module."""
        import copy
        m = self.ix.module(gmodule)
        key: list = []

        class Rebind(ast.NodeTransformer):
            def visit_Name(_, n):
                here, there = self.resolve(module, n, scope), self.resolve(gmodule, n)
                key.append((n.id, here))
                if here == there:
                    return n
                if here is None or m is None:
                    raise self.err(f"`{ast.unparse(sub)}`: `{n.id}` must mean the same in {gmodule}", n, module)
                spec = (("module", here.dotted) if isinstance(here, Ext) and "." not in here.dotted
                        else ("from", here.module, here.name) if isinstance(here, Sym)
                        else ("module", here.module) if isinstance(here, ModRef)
                        else ("from", *here.dotted.rsplit(".", 1)))
                alias = f"_p2ag_{abs(hash(spec)) % 10**8}"
                m.imports[alias] = spec
                return ast.copy_location(ast.Name(id=alias, ctx=ast.Load()), n)

        return [Rebind().visit(copy.deepcopy(a)) for a in args], tuple(key)

    def _schema(self, sym: Sym) -> SchemaInfo:
        node = self.fe.schema_syms[sym]
        module = sym.module
        tvars = self.typevars(node, module) if not getattr(node, "_py2axum_specialized", False) else {}
        if tvars:
            # the generic model used as such: a parameter is its default, else its bound, else Any
            fallback = {n: (d if d is not None else b if b is not None else ast.Name(id="object", ctx=ast.Load()))
                        for n, (b, d) in tvars.items()}
            self.specialize(sym, node, f"<generic {sym.name}>", fallback, node, module)
            node = self.fe.schema_syms.pop(Sym(module, f"<generic {sym.name}>"))
        base = f"{mod_ident(module)}__{ident(sym.name)}"
        info = SchemaInfo(sym, f"SCHEMA_{base}", f"CLS_{base}", [], False, "Ignore")
        self.schemas[sym] = info
        fields: list[dict] = []
        for b in node.bases:
            if isinstance(b, ast.Subscript):
                bt = self.resolve(module, b.value)
                if isinstance(bt, Ext) and bt.dotted in {"typing.Generic", "typing_extensions.Generic"}:
                    continue
                t = self.generic_args(bt, b, module) if isinstance(bt, Sym) and bt in self.fe.schema_syms else bt
            else:
                t = self.resolve(module, b)
            if isinstance(t, Sym) and t in self.fe.schema_syms:
                parent = self.schema(t)
                info.__dict__["base_cls"] = parent.cls  # isinstance(child, Parent), issubclass
                fields = [dict(f) for f in parent.fields]
                info.from_attributes = parent.from_attributes
                info.extra = parent.extra
                info.settings = parent.settings
                info.env_prefix = parent.env_prefix
                info.case_sensitive = parent.case_sensitive
                info.env_none = parent.env_none
                info.validators = list(parent.validators)
                info.methods = list(parent.methods)
                for k in ("model_after", "model_before", "before", "private", "computed"):
                    info.__dict__[k] = list(parent.__dict__.get(k, []))
                info.__dict__["vinfo"] = dict(parent.__dict__.get("vinfo", {}))
                if parent.__dict__.get("init") is not None:
                    info.__dict__["init"] = parent.__dict__["init"]
                for k in ("strip", "lower", "upper", "smin", "smax", "strict", "field_sers", "model_ser", "root", "enum_values", "validate_assignment", "populate_by_name", "post_init",
                          "alias_gen", "validate_by_alias", "ser_by_alias", "hide_input", "env_ignore_empty", "finite", "validate_default", "json_enc"):
                    if k in parent.__dict__:
                        info.__dict__[k] = parent.__dict__[k]
                # the merged `model_config` (pydantic updates it from each base in order): `**Base.model_config`
                info.__dict__.setdefault("config_kw", {}).update(parent.__dict__.get("config_kw", {}))
            elif isinstance(t, Ext) and t.dotted.split(".")[-1] == "BaseSettings":
                info.settings = True
            elif isinstance(t, Ext) and t.dotted in {"pydantic.RootModel", "pydantic.root_model.RootModel"}:
                info.__dict__["root"] = True
                if isinstance(b, ast.Subscript):
                    fields = [{"name": "root", "alias": None, "default": "Required", "env": None, "cons": {},
                               "ann": (b.slice, module)}]
            elif not (isinstance(t, Ext) and t.dotted.split(".")[-1] == "BaseModel"):
                raise self.err(f"schema {sym.name}: base class `{ast.unparse(b)}` is not supported", b, module)
        ns = self.class_namespace(sym, node, module)
        for stmt in self.flat_class_body(node.body) if ns else node.body:
            if isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
                n = stmt.target.id
                ann = stmt.annotation
                if isinstance(ann, ast.Subscript) and (dotted(ann.value) or "").split(".")[-1] == "ClassVar":
                    continue
                if ns and any(x["name"] == n and x.get("_ns") for x in fields):
                    continue  # annotated again in another branch: one field
                if n.startswith("_"):
                    # a private attribute: per instance, never validated nor dumped
                    self.private_attr(info, sym, n, stmt.value, module)
                    continue
                f = {"name": n, "alias": None, "default": "Required", "env": None}
                ann = self.annotated_validators(info, sym, f, ann, module)
                if ns:
                    # its default is the class body's final value (class-level `if`s, earlier attributes);
                    # never given a value in the body: required
                    f["_ns"] = True
                    valued = any((isinstance(x, ast.AnnAssign) and isinstance(x.target, ast.Name) and x.target.id == n
                                  and x.value is not None and not (isinstance(x.value, ast.Call)
                                  and (dotted(x.value.func) or "").split(".")[-1] == "Field"
                                  and not any(k.arg == "default" for k in x.value.keywords)
                                  and (not x.value.args or (isinstance(x.value.args[0], ast.Constant) and x.value.args[0].value is Ellipsis))))
                                 or (isinstance(x, ast.Assign) and any(isinstance(t, ast.Name) and t.id == n for t in x.targets))
                                 for x in self.flat_class_body(node.body))
                    if valued:
                        f["default"] = f"Dyn({self.ns_default(ns, n)})"
                    cons = {}
                    if isinstance(stmt.value, ast.Call) and (dotted(stmt.value.func) or "").split(".")[-1] == "Field":
                        cons, opts = self.field_cons(stmt.value, module)
                        self.field_aliases(f, opts, module)
                    f["cons"] = cons
                    f.setdefault("ann", (ann, module))
                    fields = self.put_field(fields, f)
                    continue
                cons, opts = {}, self.annotated_opts(ann, module)
                if stmt.value is not None:
                    v = stmt.value
                    if isinstance(v, ast.Call) and (dotted(v.func) or "").split(".")[-1] == "Field":
                        cons, vopts = self.field_cons(v, module)
                        opts.update(vopts)
                    else:
                        opts["default"] = v
                if "default" in opts:
                    f["default"] = self.default_spec(opts["default"], module, False)
                if "default_factory" in opts:
                    f["default"] = self.default_spec(opts["default_factory"], module, True)
                self.field_aliases(f, opts, module)
                if "validate_default" in opts:
                    # explicit: wins over model_config's validate_default
                    f["vd_explicit"] = bool(self.const(opts["validate_default"], module))
                    f.pop("vd_config", None)
                if f.get("vd_explicit"):
                    if f["default"].startswith(("Dyn(", "Data(")):
                        raise self.err("Field(validate_default=True) with a computed default is not supported", opts["validate_default"], module)
                    f["validate_default"] = True
                elif "vd_explicit" in f:
                    f.pop("validate_default", None)
                f["cons"] = cons
                f.setdefault("ann", (ann, module))
                fields = self.put_field(fields, f)
            elif isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
                n = stmt.targets[0].id
                v = stmt.value
                if n == "model_config":
                    self.model_config(info, stmt.value, module)
                elif (isinstance(v, ast.Call) and isinstance(v.func, ast.Call) and len(v.args) == 1 and not v.keywords
                      and (dotted(v.func.func) or "").split(".")[-1] == "field_validator" and isinstance(v.args[0], ast.Lambda)):
                    # `_x = field_validator("f", mode="before")(lambda v: ...)`
                    dec = v.func
                    mode = next((self.const(k.value, module) for k in dec.keywords if k.arg == "mode"), "after")
                    if mode not in {"after", "before"} or any(k.arg != "mode" for k in dec.keywords):
                        raise self.err(f"schema {sym.name}: field_validator(...) options not supported here", dec, module)
                    lam_args = v.args[0].args
                    if len(lam_args.posonlyargs + lam_args.args) - len(lam_args.defaults) != 1:
                        raise self.err(f"schema {sym.name}: a lambda validator taking `info` is not supported", v.args[0], module)
                    lam = Sym(module, f"{node.name}.<{n}>")
                    self.__dict__.setdefault("lambda_validators", {})[lam] = v.args[0]
                    entry = ([self.const(a, module) for a in dec.args], lam)
                    if mode == "before":
                        info.__dict__.setdefault("before", []).append(entry)
                    else:
                        info.validators.append(entry)
                elif ns and any(x["name"] == n for x in fields):
                    pass  # a field's value set by the class body (namespace)
                elif n == "__hash__" and isinstance(v, ast.Constant) and v.value is None:
                    info.__dict__["unhashable"] = True  # BaseModel's own __hash__ is None unless frozen
                else:
                    raise self.err(f"schema {sym.name}: class attribute `{n}` is not supported", stmt, module)
            elif isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                decos = [(dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "" for d in stmt.decorator_list]
                last = [d.split(".")[-1] for d in decos]
                msym = Sym(module, f"{node.name}.{stmt.name}")
                if "field_validator" in last or "validator" in last:
                    dec = stmt.decorator_list[last.index("field_validator" if "field_validator" in last else "validator")]
                    names = [self.const(a, module) for a in dec.args]
                    before = False
                    plain = wrap = False
                    for kw in dec.keywords:
                        if kw.arg == "mode":
                            mode = self.const(kw.value, module)
                            if mode not in {"after", "before", "plain", "wrap"}:
                                raise self.err(f"field validators mode={mode!r} are not supported", kw, module)
                            before = mode == "before"
                            plain = mode == "plain"
                            wrap = mode == "wrap"
                        elif kw.arg == "pre":
                            before = bool(self.const(kw.value, module))
                        elif kw.arg in {"each_item", "always"} and self.const(kw.value, module):
                            raise self.err(f"@validator({kw.arg}=True) is not supported", kw, module)
                        elif kw.arg not in {"mode", "pre", "each_item", "always", "allow_reuse", "check_fields"}:
                            raise self.err(f"@{last[0]}({kw.arg}=) is not supported", kw, module)
                    kind = self.validator_info(stmt, "field_validator" not in last, before, sym, module, wrap)
                    if wrap:
                        for n in names:
                            if n == "*" or any(n in v[0] for v in info.validators + info.__dict__.get("before", [])):
                                raise self.err(f"schema {sym.name}: a mode='wrap' validator of {n!r} after other validators "
                                               "of the field is not supported", stmt, module)
                            info.__dict__.setdefault("wrap_fields", {})[n] = msym
                    if kind:
                        info.__dict__.setdefault("vinfo", {})[msym] = kind
                    if plain:
                        # it replaces the field's validation: the type and the validators defined before it
                        self.plain_validation(info, sym, names, stmt, module)
                    if before:
                        # pydantic wraps the field's schema in definition order: an `after` validator defined
                        # earlier sees the raw input (its errors report it); a `before` one defined later
                        # would run first and change what it sees, an order the runtime does not keep
                        if any(set(n) & set(names) or "*" in n or "*" in names for n, _ in info.validators):
                            raise self.err(f"schema {sym.name}.{stmt.name}: a mode=\"before\" validator defined after a "
                                           "mode=\"after\" validator of the same field is not supported", stmt, module)
                        info.__dict__.setdefault("before", []).append((names, msym))
                    else:
                        info.validators.append((names, msym))
                elif "model_validator" in last:
                    dec = stmt.decorator_list[last.index("model_validator")]
                    mode = next((self.const(k.value, module) for k in getattr(dec, "keywords", []) if k.arg == "mode"), None)
                    if mode not in {"after", "before"}:
                        raise self.err(f"schema {sym.name}: @model_validator(mode={mode!r}) is not supported (after/before)", dec, module)
                    info.__dict__.setdefault("model_after" if mode == "after" else "model_before", []).append(msym)
                elif "computed_field" in last:
                    # `@computed_field` over `@property` (or alone): a property also serialized after the fields
                    dec = stmt.decorator_list[last.index("computed_field")]
                    if isinstance(dec, ast.Call) and (dec.args or dec.keywords):
                        raise self.err(f"schema {sym.name}.{stmt.name}: @computed_field(...) options are not supported", dec, module)
                    for d, ln in zip(stmt.decorator_list, last):
                        if ln not in {"property", "computed_field"}:
                            raise self.err(f"schema {sym.name}.{stmt.name}: decorator @{ast.unparse(d)} is not supported "
                                           "with @computed_field", d, module)
                    if isinstance(stmt, ast.AsyncFunctionDef):
                        raise self.err(f"schema {sym.name}.{stmt.name}: an async @computed_field is not supported", stmt, module)
                    info.methods.append((stmt.name, True, msym))
                    comp = info.__dict__.setdefault("computed", [])
                    if stmt.name not in comp:
                        comp.append(stmt.name)
                elif stmt.name == "model_post_init":
                    # pydantic-core calls it once the fields are valid, before the after validators, with context None
                    a = stmt.args
                    if (isinstance(stmt, ast.AsyncFunctionDef) or stmt.decorator_list or len(a.posonlyargs + a.args) != 2
                            or a.vararg or a.kwonlyargs or a.kwarg or a.defaults):
                        raise self.err(f"schema {sym.name}: only `def model_post_init(self, context)` is supported", stmt, module)
                    info.__dict__["post_init"] = msym
                elif stmt.name == "__init__":
                    # `def __init__(self, **data)`: run by `Model(...)` only (model_validate and FastAPI skip it)
                    a = stmt.args
                    if (isinstance(stmt, ast.AsyncFunctionDef) or stmt.decorator_list or len(a.args) != 1 or a.vararg
                            or a.kwonlyargs or a.posonlyargs or a.kwarg is None):
                        raise self.err(f"schema {sym.name}: only `def __init__(self, **data)` is supported", stmt, module)
                    info.__dict__["init"] = msym
                elif "field_serializer" in last or "model_serializer" in last:
                    self.serializer(info, sym, stmt, last, msym, module)
                elif is_dunder(stmt.name) and stmt.name not in DISPATCHED_DUNDERS:
                    raise self.err(f"schema {sym.name}: method {stmt.name} is not supported", stmt, module)
                else:
                    for d, ln in zip(stmt.decorator_list, last):
                        if ln not in {"property", "classmethod", "staticmethod"}:
                            raise self.err(f"schema {sym.name}.{stmt.name}: decorator @{ast.unparse(d)} is not supported", d, module)
                    info.methods.append((stmt.name, "property" in last, msym))
            elif isinstance(stmt, (ast.Pass, ast.Expr)):
                continue
            elif isinstance(stmt, ast.ClassDef) and stmt.name == "Config":
                self.config_class(info, stmt, module)
            else:
                raise self.err(f"unsupported statement in schema {sym.name}", stmt, module)
        # class keywords (`class M(BaseModel, extra="allow", frozen=True)`): configuration, applied after the
        # body's model_config as pydantic merges them (ConfigWrapper.for_model)
        cls_kw = [k for k in node.keywords if k.arg != "metaclass"]
        if any(k.arg is None for k in cls_kw):
            raise self.err(f"schema {sym.name}: `**` in the class keywords is not supported", node, module)
        self.config_items(info, cls_kw, module)
        self.json_encoded(sym, info, fields, node, module)
        # model_config validate_default (inherited, or the class's own): every field that does not set its own
        for f in fields:
            if "vd_explicit" in f:
                continue
            if info.__dict__.get("validate_default"):
                if f.get("default", "Required").startswith(("Dyn(", "Data(")):
                    raise self.err(f"schema {sym.name}.{f['name']}: model_config validate_default=True with a computed "
                                   "default is not supported", node, module)
                f["validate_default"] = f["vd_config"] = True
            elif f.pop("vd_config", None):
                f.pop("validate_default", None)
        for f in fields:
            if f.get("validate_default") and any(f["name"] in names for names, _ in info.__dict__.get("before", [])):
                raise self.err(f"schema {sym.name}.{f['name']}: validate_default=True with a mode='before' validator "
                               "is not supported", node, module)
        if info.__dict__.get("root"):
            if [f["name"] for f in fields] != ["root"]:
                raise self.err(f"schema {sym.name}: a RootModel has one field, `root`", node, module)
            if (info.validators or info.__dict__.get("before") or info.__dict__.get("model_after")
                    or info.__dict__.get("model_before") or info.__dict__.get("model_ser") or info.__dict__.get("field_sers")
                    or info.extra != "Ignore" or fields[0].get("aliases")):
                raise self.err(f"schema {sym.name}: validators, serializers, extra= or aliases on a RootModel are not "
                               "supported", node, module)
        self.apply_aliases(info, fields, node, module)
        for f in fields:
            if f["name"] in info.__dict__.get("plain_fields", ()):
                f["any"] = True
            if f["name"] in info.__dict__.get("wrap_fields", {}):
                if f.get("any") or f.get("wrap"):
                    raise self.err(f"schema {sym.name}.{f['name']}: a mode='wrap' validator with a plain or wrap "
                                   "validator in its annotation is not supported", node, module)
                f["wrap"] = info.__dict__["wrap_fields"][f["name"]]
        if info.__dict__.get("model_before") and info.__dict__.get("validate_assignment"):
            raise self.err(f"schema {sym.name}: validate_assignment=True with @model_validator(mode=\"before\") is not "
                           "supported (pydantic calls it on every assignment with the whole data)", node, module)
        info.fields = fields
        return info

    def alias_value(self, node: ast.AST, module: str):
        """A `validation_alias=`: a key (str), or the paths of an `AliasPath`/`AliasChoices` (list of lists)."""
        if isinstance(node, ast.Call):
            t = self.resolve(module, node.func)
            last = t.dotted.split(".")[-1] if isinstance(t, Ext) and t.dotted.startswith("pydantic") else None
            if last == "AliasPath" and node.args and not node.keywords:
                path = [self.const(a, module) for a in node.args]
                if not isinstance(path[0], str) or not all(isinstance(x, (str, int)) and not isinstance(x, bool) for x in path):
                    raise self.err("AliasPath(...): a key first, then keys or list indexes", node, module)
                return [path]
            if last == "AliasChoices" and node.args and not node.keywords:
                out = []
                for a in node.args:
                    v = self.alias_value(a, module)
                    out += [[v]] if isinstance(v, str) else v
                return out
        v = self.const(node, module)
        if not isinstance(v, str):
            raise self.err("an alias must be a string, an AliasPath or an AliasChoices", node, module)
        return v

    def field_aliases(self, f: dict, opts: dict, module: str) -> None:
        """The aliases a field declares (`alias=`, `validation_alias=`, `serialization_alias=`): kept apart, the
        model's alias generator and config settle them (`apply_aliases`)."""
        given = {}
        if "alias" in opts:
            v = self.const(opts["alias"], module)
            if v is not None and not isinstance(v, str):
                raise self.err("Field(alias=) must be a string", opts["alias"], module)
            given["alias"] = v
        if "validation_alias" in opts:
            given["validation"] = self.alias_value(opts["validation_alias"], module)
        if "serialization_alias" in opts:
            v = self.const(opts["serialization_alias"], module)
            if v is not None and not isinstance(v, str):
                raise self.err("Field(serialization_alias=) must be a string", opts["serialization_alias"], module)
            given["serialization"] = v
        if "union_mode" in opts:
            v = self.const(opts["union_mode"], module)
            if v not in ("smart", "left_to_right"):
                raise self.err("Field(union_mode=) must be 'smart' or 'left_to_right'", opts["union_mode"], module)
            f["ltr"] = v == "left_to_right"
        if "exclude_if" in opts:
            s = Sym(module, f"<ex_{len(self.__dict__.setdefault('lambda_validators', {}))}_{f['name']}>")
            self.lambda_validators[s] = opts["exclude_if"]
            f["exclude_if"] = s
        for k in ("repr", "exclude", "frozen", "strict"):
            # strict=: the field's own mode (`apply_strict`); repr=False: left out of repr(); exclude=True: of every dump; frozen=True: assignment refused
            if k in opts:
                v = self.const(opts[k], module)
                if not isinstance(v, bool):
                    raise self.err(f"Field({k}=) must be True or False", opts[k], module)
                f[k] = v
        given = {k: v for k, v in given.items() if v is not None}
        if "alias" in given:
            # FieldInfo: `alias=` is also the validation and serialization alias the others leave unset
            given.setdefault("validation", given["alias"])
            given.setdefault("serialization", given["alias"])
        if given:
            f["aliases"] = given
        f["alias"] = given.get("alias")

    def alias_function(self, node: ast.AST, module: str):
        """An alias generator as a Python function str -> str (evaluated now, on the field names)."""
        from . import pydstatic
        t = self.resolve(module, node)
        if isinstance(t, Ext) and t.dotted in pydstatic.LIBRARY:
            return pydstatic.LIBRARY[t.dotted]
        fn = node if isinstance(node, ast.Lambda) else None
        if fn is None and isinstance(t, Sym):
            d = self.ix.definition(t)
            fn, module = (d, t.module) if isinstance(d, ast.FunctionDef) else (None, module)
        if fn is None:
            raise self.err(f"alias generator `{ast.unparse(node)}` is not supported (a pydantic.alias_generators "
                           "function, a lambda or a one-return function of the name)", node, module)
        try:
            return pydstatic.string_function(fn)
        except pydstatic.NotStatic as x:
            raise self.err(f"alias generator: `{ast.unparse(x.node)}` cannot be evaluated at translation time "
                           "(string operations on the field name only)", x.node, module) from None

    def apply_aliases(self, info: SchemaInfo, fields: list[dict], node, module: str) -> None:
        """Each field's validation key or paths and serialization key, by pydantic's rules
        (`_apply_alias_generator_to_field_info`: a field that declares an alias keeps it, the generator only
        fills the others; `validate_by_alias=False` validates by name only)."""
        gen = info.__dict__.get("alias_gen")
        fns = {}
        if gen is not None:
            gnode, gmodule = gen
            t = self.resolve(gmodule, gnode.func) if isinstance(gnode, ast.Call) else None
            if isinstance(t, Ext) and t.dotted.split(".")[-1] == "AliasGenerator" and not gnode.args:
                for k in gnode.keywords:
                    if k.arg not in {"alias", "validation_alias", "serialization_alias"}:
                        raise self.err(f"AliasGenerator({k.arg}=) is not supported", k, gmodule)
                    if not (isinstance(k.value, ast.Constant) and k.value.value is None):
                        fns[{"validation_alias": "validation", "serialization_alias": "serialization"}.get(k.arg, k.arg)] = \
                            self.alias_function(k.value, gmodule)
            else:
                fns["alias"] = self.alias_function(gnode, gmodule)
        by_alias = info.__dict__.get("validate_by_alias", True)
        if not by_alias and not info.__dict__.get("populate_by_name"):
            raise self.err(f"schema {info.sym.name}: validate_by_alias=False requires validate_by_name=True", node, module)
        for f in fields:
            given = dict(f.get("aliases", {}))
            if fns:
                from . import pydstatic
                try:
                    made = {k: fn(f["name"]) for k, fn in fns.items()}
                except pydstatic.NotStatic as x:
                    raise self.err(f"alias generator: `{ast.unparse(x.node)}` cannot be evaluated at translation time "
                                   "(string operations on the field name only)", node, module) from None
                except Exception as x:  # the generator itself fails on this name: pydantic would raise too
                    raise self.err(f"alias generator failed on {f['name']!r}: {x!r}", node, module) from None
                if "alias" not in given:
                    given["alias"] = made.get("alias")
                for k in ("validation", "serialization"):
                    if k not in given:
                        given[k] = made.get(k) or made.get("alias")
            if given.get("alias") is not None and not isinstance(given["alias"], str):
                raise self.err(f"schema {info.sym.name}.{f['name']}: the alias generator must return strings", node, module)
            val = given.get("validation") or given.get("alias")
            if not by_alias:
                val = None
            f["paths"] = []
            if isinstance(val, list):
                if len(val) == 1 and len(val[0]) == 1:
                    val = val[0][0]
                else:
                    f["paths"], val = val, None
            f["alias"] = val
            f["ser"] = given.get("serialization") or given.get("alias")

    @staticmethod
    def put_field(fields: list[dict], f: dict) -> list[dict]:
        """A field (re)declared: a redefinition (in a subclass, or again in the class) keeps the inherited
        position, like Pydantic's field order (the annotations' dict order)."""
        out = [f if x["name"] == f["name"] else x for x in fields]
        return out if any(x["name"] == f["name"] for x in fields) else out + [f]

    ANNOTATED_VALIDATORS = {"AfterValidator": "after", "BeforeValidator": "before", "PlainValidator": "plain",
                            "WrapValidator": "wrap", "PlainSerializer": "ser", "WrapSerializer": "serwrap"}

    def is_function_ref(self, node: ast.AST, module: str) -> bool:
        """`f`, `mod.f`, `str.lower`: a project function or a method of a builtin type, named without a call"""
        if isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name) and node.value.id in BUILTIN_TYPES \
                and self.resolve(module, node.value) is None:
            return method_descriptor(node.value.id, node.attr)
        t = self.resolve(module, node)
        return isinstance(t, Sym) and isinstance(self.ix.definition(t), (ast.FunctionDef, ast.AsyncFunctionDef))

    def core_schema_hook(self, cls_sym: Sym):
        """A class's `__get_pydantic_core_schema__` (its FunctionDef, and whether it is a classmethod), or None"""
        d = self.ix.definition(cls_sym)
        if not isinstance(d, ast.ClassDef):
            return None
        for x in d.body:
            if isinstance(x, ast.FunctionDef) and x.name == "__get_pydantic_core_schema__":
                deco = {dotted(dd) for dd in x.decorator_list}
                if deco - {"classmethod"} or len(x.args.args) != 3:
                    raise self.err("__get_pydantic_core_schema__: (self or cls, source, handler) is expected", x, cls_sym.module)
                return x, "classmethod" in deco
        return None

    def run_hook(self, hook: ast.FunctionDef, owner: coreschema.Ref, source: coreschema.Schema, module: str,
                 hook_module: str, self_class: bool) -> coreschema.Schema:
        r = coreschema.Reader(self, module, hook_module, self.src(hook_module), source, self_class)
        a = hook.args.args
        env = {a[0].arg: owner, a[1].arg: coreschema.Source(), a[2].arg: coreschema.Handler()}
        out = r.run(hook.body, env)
        if not isinstance(out, coreschema.Schema):
            raise TranspileError("__get_pydantic_core_schema__: a core schema must be returned", hook, self.src(hook_module))
        return out

    def owner_ref(self, sym: Sym, expr: ast.AST, call: ast.Call | None, module: str) -> coreschema.Ref:
        """`cls` / `self` of a hook: the class expression, or the metadata instance with its dataclass fields"""
        d = self.ix.definition(sym)
        methods = {x.name for x in d.body if isinstance(x, (ast.FunctionDef, ast.AsyncFunctionDef))}
        fields = {}
        if call is not None:
            names = [x.target.id for x in d.body if isinstance(x, ast.AnnAssign) and isinstance(x.target, ast.Name)]
            if call.args or call.keywords:
                if sym not in self.fe.dataclass_syms or len(call.args) > len(names):
                    raise self.err(f"__get_pydantic_core_schema__ of `{ast.unparse(call)}`: a metadata instance with arguments must be a dataclass", call, module)
                for n, v in [*zip(names, call.args), *((k.arg, k.value) for k in call.keywords)]:
                    if isinstance(v, ast.Lambda) or (isinstance(v, (ast.Name, ast.Attribute)) and self.is_function_ref(v, module)):
                        # a function given to the metadata (`MyAfterValidator(str.lower)`): used as a validator
                        fields[n] = ("fn", v)
                    else:
                        fields[n] = self.const(v, module)
        return coreschema.Ref(expr, fields, methods, sym)

    def core_schema_rewrite(self, ann: ast.AST, module: str) -> ast.AST:
        """A field annotation whose type or Annotated metadata customize the core schema
        (`__get_pydantic_core_schema__`, `GetPydanticSchema`): the equivalent plain annotation (py2axum/coreschema.py)."""
        if isinstance(ann, (ast.Name, ast.Attribute)):
            t = self.resolve(module, ann)
            hook = self.core_schema_hook(t) if isinstance(t, Sym) else None
            if hook is None:
                return ann
            if not hook[1]:
                raise self.err(f"`{t.name}.__get_pydantic_core_schema__` must be a classmethod on a field type", hook[0], t.module)
            out = self.run_hook(hook[0], self.owner_ref(t, ann, None, module), coreschema.Schema(ann), module, t.module, True)
            return coreschema.annotation(out, [], ann)
        if not (isinstance(ann, ast.Subscript) and (dotted(ann.value) or "").split(".")[-1] == "Annotated"
                and isinstance(ann.slice, ast.Tuple)):
            return ann
        base, *metas = ann.slice.elts
        cur, keep, changed = None, [], False
        for meta in metas:
            func = meta.func if isinstance(meta, ast.Call) else meta
            t = self.resolve(module, func) if isinstance(func, (ast.Name, ast.Attribute)) else None
            hook = self.core_schema_hook(t) if isinstance(t, Sym) else None
            gps = isinstance(t, Ext) and t.dotted in {"pydantic.GetPydanticSchema", "pydantic.types.GetPydanticSchema"}
            if hook is None and not gps:
                if changed:
                    raise self.err(f"`{ast.unparse(meta)}` after a schema hook in Annotated[...] is not supported", meta, module)
                keep.append(meta)
                continue
            if cur is None:
                name = base.id if isinstance(base, ast.Name) and base.id in coreschema.SCALAR_KEYS else None
                cur = coreschema.Schema(base, name)
            if gps:
                if not (isinstance(meta, ast.Call) and len(meta.args) == 1 and not meta.keywords
                        and isinstance(meta.args[0], ast.Lambda) and len(meta.args[0].args.args) == 2):
                    raise self.err("GetPydanticSchema(lambda tp, handler: ...) is the supported form", meta, module)
                lam = meta.args[0]
                r = coreschema.Reader(self, module, module, self.src(module), cur)
                out = r.expr(lam.body, {lam.args.args[0].arg: coreschema.Source(), lam.args.args[1].arg: coreschema.Handler()})
                if not isinstance(out, coreschema.Schema):
                    raise self.err("GetPydanticSchema: a core schema must be returned", meta, module)
                cur = out
            else:
                call = meta if isinstance(meta, ast.Call) else None
                if hook[1] == (call is not None):
                    raise self.err(f"__get_pydantic_core_schema__ of `{ast.unparse(meta)}`: a classmethod hook on the class, "
                                   "an instance method on an instance", meta, module)
                cur = self.run_hook(hook[0], self.owner_ref(t, meta, call, module), cur, module, t.module, False)
            changed = True
        return coreschema.annotation(cur, keep, ann) if changed else ann

    def annotated_validators(self, info: SchemaInfo, sym: Sym, f: dict, ann, module: str):
        """`Annotated[T, AfterValidator(fn), ...]` at the top of a field's annotation (directly or through a type
        alias): the validators become the field's own, run like `@field_validator`s defined before the class's
        (pydantic applies the decorators around the annotation's); the annotation without them is returned.
        `PlainValidator` replaces the type and the validators before it."""
        expanded, emod = self.expand_alias(ann, module)
        rewritten = self.core_schema_rewrite(expanded, emod)
        if rewritten is not expanded:
            expanded = ann = rewritten
            f["ann"] = (ann, emod)
        if not (isinstance(expanded, ast.Subscript) and (dotted(expanded.value) or "").split(".")[-1] == "Annotated"
                and isinstance(expanded.slice, ast.Tuple)):
            return ann
        args = expanded.slice.elts
        keep, found = [], []
        for meta in args[1:]:
            t = self.resolve(emod, meta.func) if isinstance(meta, ast.Call) else None
            mode = self.ANNOTATED_VALIDATORS.get(t.dotted.split(".")[-1]) if isinstance(t, Ext) and t.dotted.startswith("pydantic") else None
            if mode is None:
                if any(m == "wrap" for m, _ in found):
                    raise self.err(f"`{ast.unparse(meta)}` after a WrapValidator in Annotated[...] is not supported", meta, emod)
                keep.append(meta)
                continue
            if mode in ("ser", "serwrap"):
                # a serializer: the field's dump, not its validation
                if len(meta.args) != 1 or any(k.arg != "when_used" for k in meta.keywords):
                    raise self.err(f"{ast.unparse(meta.func)}(...): a function and when_used= only are supported", meta, emod)
                when = next((self.const(k.value, emod) for k in meta.keywords), "always")
                vsym = Sym(emod, f"{sym.name}.<as_{f['name']}>")
                self.__dict__.setdefault("lambda_validators", {})[vsym] = meta.args[0]
                f["serializer"] = (vsym, mode == "serwrap", self.function_validator_info(meta.args[0], emod, 2 if mode == "serwrap" else 1),
                                   self.WHEN_USED[when] if when in self.WHEN_USED else self.err_when(when, meta, emod))
                continue
            if len(meta.args) != 1 or meta.keywords:
                raise self.err(f"{ast.unparse(meta.func)}(...): one function, no options, is supported", meta, emod)
            if mode == "plain":
                found, keep = [], []  # what it wraps is never run
                f["any"] = True
            if mode == "wrap" and (found or f.get("any") or f.get("wrap")):
                raise self.err("a WrapValidator after other validators in Annotated[...] is not supported", meta, emod)
            if found and found[0][0] == "wrap" and mode != "after":
                raise self.err("only AfterValidator(...) may follow a WrapValidator in Annotated[...]", meta, emod)
            found.append((mode, meta.args[0]))
        if not found and not f.get("any"):
            if len(keep) == len(args) - 1:
                return ann
            # serializers only: the annotation without them
            new = (args[0] if not keep else
                   ast.copy_location(ast.Subscript(value=expanded.value, slice=ast.Tuple(elts=[args[0], *keep], ctx=ast.Load()),
                                                   ctx=ast.Load()), expanded))
            f["ann"] = (new, emod)
            return new
        n = f["name"]
        annot = self.__dict__.setdefault("annotated_syms", set())
        # a field declared again in a subclass: the parent's annotation validators no longer apply
        info.validators = [v for v in info.validators if not (v[1] in annot and v[0] == [n])]
        info.__dict__["before"] = [v for v in info.__dict__.get("before", []) if not (v[1] in annot and v[0] == [n])]
        afters, befores = [], []
        for i, (mode, fn) in enumerate(found):
            vsym = Sym(emod, f"{sym.name}.<av_{n}_{i}>")
            annot.add(vsym)
            self.__dict__.setdefault("lambda_validators", {})[vsym] = fn
            kind = self.function_validator_info(fn, emod, 2 if mode == "wrap" else 1)
            if mode == "wrap":
                f["wrap"] = vsym
            if mode == "before":
                if kind:
                    raise self.err("a BeforeValidator taking `info` is not supported", fn, emod)
                befores.append(([n], vsym))
            else:
                if kind:
                    info.__dict__.setdefault("vinfo", {})[vsym] = kind
                afters.append(([n], vsym))
        info.validators[0:0] = afters
        info.__dict__["before"] = befores + info.__dict__.get("before", [])
        if befores and afters:
            raise self.err(f"schema {sym.name}.{n}: BeforeValidator and AfterValidator on one field are not supported", ann, module)
        if f.get("any"):
            f["ann"] = (ast.Name(id="object", ctx=ast.Load()), module)
            return ann
        if not keep:
            f["ann"] = (args[0], emod)
            return args[0]
        new = ast.Subscript(value=expanded.value, slice=ast.Tuple(elts=[args[0], *keep], ctx=ast.Load()), ctx=ast.Load())
        ast.copy_location(new, expanded)
        f["ann"] = (new, emod)
        return new

    WHEN_USED = {"always": 0, "unless-none": 1, "json": 2, "json-unless-none": 3}

    def err_when(self, when, node, module):
        raise self.err(f"when_used={when!r} is not supported", node, module)

    def serializer(self, info: SchemaInfo, sym: Sym, stmt, last: list, msym: Sym, module: str) -> None:
        """`@field_serializer(*fields, mode=, when_used=)` / `@model_serializer(mode=, when_used=)`: called while
        dumping (synchronously: the method must not await), with the value (a field's), a `handler` (wrap), an
        `info` when it takes one."""
        model = "model_serializer" in last
        dec = stmt.decorator_list[last.index("model_serializer" if model else "field_serializer")]
        args = dec.args if isinstance(dec, ast.Call) else []
        kws = dec.keywords if isinstance(dec, ast.Call) else []
        mode, when = "plain", "always"
        for k in kws:
            if k.arg == "mode":
                mode = self.const(k.value, module)
            elif k.arg == "when_used":
                when = self.const(k.value, module)
            elif k.arg == "return_type" or (k.arg == "check_fields" and not model):
                pass
            else:
                raise self.err(f"@{last[0]}({k.arg}=) is not supported", k, module)
        if mode not in ("plain", "wrap") or when not in self.WHEN_USED:
            raise self.err(f"@{'model' if model else 'field'}_serializer(mode={mode!r}, when_used={when!r}) is not supported", dec, module)
        if isinstance(stmt, ast.AsyncFunctionDef):
            raise self.err(f"schema {sym.name}.{stmt.name}: an async serializer is not supported", stmt, module)
        for d, ln in zip(stmt.decorator_list, last):
            if ln not in {"field_serializer", "model_serializer", "classmethod", "staticmethod"}:
                raise self.err(f"schema {sym.name}.{stmt.name}: decorator @{ast.unparse(d)} is not supported with a serializer", d, module)
        a = stmt.args
        pos = a.posonlyargs + a.args
        lead = 0 if "staticmethod" in last else 1
        required = len(pos) - len(a.defaults) - lead
        base = (0 if model else 1) + (1 if mode == "wrap" else 0)
        if required not in (base, base + 1) or a.vararg or a.kwonlyargs:
            raise self.err(f"schema {sym.name}.{stmt.name}: unrecognized serializer signature", stmt, module)
        entry = (msym, mode == "wrap", required == base + 1, self.WHEN_USED[when])
        if model:
            if args:
                raise self.err("@model_serializer(...) takes no field names", dec, module)
            info.__dict__["model_ser"] = entry
            return
        names = [self.const(x, module) for x in args]
        if not names or "*" in names:
            raise self.err("@field_serializer('*') or without field names is not supported", dec, module)
        sers = {n: e for n, e in info.__dict__.get("field_sers", {}).items()}
        for n in names:
            sers[n] = entry
        info.__dict__["field_sers"] = sers

    def json_encoded(self, sym: Sym, info, fields: list, node, module: str) -> None:
        """`model_config json_encoders={datetime: fn, ...}`: each field whose annotation is one of those types gets
        `fn` as its serializer in JSON mode (a field's own serializer wins). The type inside `Optional[...]` or a
        container (pydantic applies the encoder there too) is refused."""
        enc = info.__dict__.get("json_enc")
        if enc is None:
            return
        d, emod = enc
        keys = [(self.resolve(emod, k), v) for k, v in zip(d.keys, d.values)]
        for k, (rk, _) in zip(d.keys, keys):
            if not isinstance(rk, (Ext, Sym)):
                raise self.err(f"model_config json_encoders: key `{ast.unparse(k)}` is not a type", k, emod)
        for f in fields:
            ann = f.get("ann")
            if ann is None or f.get("serializer") or f["name"] in info.__dict__.get("field_sers", {}):
                continue
            a, amod = ann
            t = self.resolve(amod, a) if isinstance(a, (ast.Name, ast.Attribute)) else None
            hit = next((v for rk, v in keys if t is not None and rk == t), None)
            if hit is not None:
                vsym = Sym(emod, f"{sym.name}.<json_enc_{f['name']}>")
                self.__dict__.setdefault("lambda_validators", {})[vsym] = hit
                f["serializer"] = (vsym, False, self.function_validator_info(hit, emod, 1), self.WHEN_USED["json"])
                continue
            inner = [n for n in ast.walk(a) if isinstance(n, (ast.Name, ast.Attribute)) and n is not a]
            for n in inner:
                rt = self.resolve(amod, n)
                if any(rt is not None and rt == rk for rk, _ in keys):
                    raise self.err(f"schema {sym.name}.{f['name']}: model_config json_encoders for `{ast.unparse(n)}` inside "
                                   f"`{ast.unparse(a)}` is not supported (only a field of that type itself)", node, module)

    def function_validator_info(self, fn, module: str, lead: int = 1) -> int:
        """0 or 1: whether a validator function (a lambda, a project function) takes `info` after the value (and
        the handler of a wrap validator: `lead` 2)."""
        node = fn if isinstance(fn, ast.Lambda) else None
        if node is None:
            t = self.resolve(module, fn)
            node = self.ix.definition(t) if isinstance(t, Sym) else None
            if not isinstance(node, (ast.FunctionDef, ast.Lambda)):
                return 0
            module = t.module
        a = node.args
        pos = a.posonlyargs + a.args
        required = len(pos) - len(a.defaults) - (lead - 1)
        if required not in (1, 2) or a.vararg or a.kwonlyargs:
            raise self.err("unrecognized validator signature (the value, then optionally `info`)", node, module)
        if required == 2 and not isinstance(node, ast.Lambda):
            name = pos[lead].arg
            for x in ast.walk(node):
                if (isinstance(x, ast.Attribute) and isinstance(x.value, ast.Name) and x.value.id == name
                        and x.attr not in self.VALIDATION_INFO_ATTRS):
                    raise self.err(f"ValidationInfo.{x.attr} is not supported (only "
                                   f"{', '.join(sorted(self.VALIDATION_INFO_ATTRS))})", x, module)
        return required - 1

    def plain_validation(self, info: SchemaInfo, sym: Sym, names: list, stmt, module: str) -> None:
        """`@field_validator(..., mode="plain")`: the field's type and the validators defined before it are
        not run."""
        for n in names:
            if n == "*":
                raise self.err("a mode='plain' validator on '*' is not supported", stmt, module)
            if any(n in v[0] and v[0] != [n] for v in info.validators + info.__dict__.get("before", [])):
                raise self.err(f"schema {sym.name}: field {n!r} has a validator shared with other fields before a "
                               "mode='plain' one (not supported)", stmt, module)
            info.validators = [v for v in info.validators if v[0] != [n]]
            info.__dict__["before"] = [v for v in info.__dict__.get("before", []) if v[0] != [n]]
            info.__dict__.setdefault("plain_fields", set()).add(n)

    VALIDATION_INFO_ATTRS = {"data", "field_name", "context"}

    def validator_info(self, fn, v1: bool, before: bool, sym, module, wrap: bool = False) -> int:
        """What a field validator receives besides the value, by pydantic's own rules (`inspect_validator`,
        `make_generic_v1_field_validator`): 0 nothing, 1 `info` (positional), 2 `values=info.data`."""
        a = fn.args
        pos = (a.posonlyargs + a.args)[1:]  # after cls
        if wrap:
            pos = pos[:1] + pos[2:]  # the handler, after the value
        required = [x.arg for x in pos[:len(pos) - len(a.defaults)]] if len(a.defaults) <= len(pos) else []
        if v1:
            names = {x.arg for x in pos + a.kwonlyargs}
            if names & {"field", "config"}:
                raise self.err(f"schema {sym.name}.{fn.name}: the `field` and `config` validator parameters do not "
                               "exist in Pydantic v2 (use `info`)", fn, module)
            if [x for x in required[1:] if x != "values"]:
                raise self.err(f"schema {sym.name}.{fn.name}: unsupported signature for a v1 @validator", fn, module)
            kind = 2 if "values" in names or a.kwarg is not None else 0
        elif len(required) in (1, 2):
            kind = len(required) - 1
        else:
            raise self.err(f"schema {sym.name}.{fn.name}: unrecognized field validator signature", fn, module)
        if kind and before:
            raise self.err(f"schema {sym.name}.{fn.name}: `info`/`values` in a mode='before' validator is not supported",
                           fn, module)
        if kind == 1:
            name = required[1]
            for n in ast.walk(fn):
                if (isinstance(n, ast.Attribute) and isinstance(n.value, ast.Name) and n.value.id == name
                        and n.attr not in self.VALIDATION_INFO_ATTRS):
                    raise self.err(f"ValidationInfo.{n.attr} is not supported (only "
                                   f"{', '.join(sorted(self.VALIDATION_INFO_ATTRS))})", n, module)
        return kind

    def private_attr(self, info: SchemaInfo, sym: Sym, name: str, value, module: str) -> None:
        """`_name: T = value` / `PrivateAttr(default=, default_factory=)`: a literal default or a factory."""
        spec = None
        if isinstance(value, ast.Call) and (dotted(value.func) or "").split(".")[-1] == "PrivateAttr":
            if len(value.args) > 1 or any(k.arg not in {"default", "default_factory"} for k in value.keywords):
                raise self.err(f"schema {sym.name}.{name}: PrivateAttr() supports default= and default_factory= only", value, module)
            for k in value.keywords:
                spec = self.default_spec(k.value, module, k.arg == "default_factory")
            if value.args:
                spec = self.default_spec(value.args[0], module, False)
        elif value is not None:
            spec = self.default_spec(value, module, False)
        if spec is not None and spec.startswith("Data("):
            raise self.err(f"schema {sym.name}.{name}: a private attribute's default_factory cannot take the data", value, module)
        private = [p for p in info.__dict__.get("private", []) if p[0] != name]
        info.__dict__["private"] = private + ([(name, spec)] if spec is not None else [])

    def model_config(self, info: SchemaInfo, value: ast.AST, module: str) -> None:
        groups: list[tuple[list, str]] = []  # (keywords, their module), in order: a later key wins
        if isinstance(value, ast.Dict):
            for k, v in zip(value.keys, value.values):
                if k is None:
                    groups += self.config_of(v, module)
                else:
                    groups.append(([ast.keyword(arg=self.const(k, module), value=v)], module))
        elif isinstance(value, ast.Call) and (dotted(value.func) or "").split(".")[-1] in {"ConfigDict", "SettingsConfigDict"} \
                and not value.args:
            seen: set[str] = set()
            for kw in value.keywords:
                got = self.config_of(kw.value, module) if kw.arg is None else [([kw], module)]
                for items, _ in got:
                    for k in items:
                        if k.arg in seen:  # a TypeError when the class is defined
                            raise self.err(f"model_config: ConfigDict() got multiple values for keyword argument "
                                           f"'{k.arg}'", kw, module)
                        seen.add(k.arg)
                groups += got
        else:
            raise self.err("model_config must be ConfigDict(...) or a dict literal", value, module)
        for items, m in groups:
            self.config_items(info, items, m)

    def config_source(self, node: ast.AST, module: str):
        """`X.model_config` (or `X.model_config or {}`) for a project model `X`: its SchemaInfo; None for pydantic's
        own BaseModel (an empty config); refused otherwise."""
        if isinstance(node, ast.BoolOp) and isinstance(node.op, ast.Or) and len(node.values) == 2 \
                and isinstance(node.values[1], ast.Dict) and not node.values[1].keys:
            node = node.values[0]  # a model's config is a dict, `or {}` only matters when it is empty: same keys
        if isinstance(node, ast.Attribute) and node.attr == "model_config":
            t = self.resolve(module, node.value)
            if isinstance(t, Sym) and t in self.fe.schema_syms:
                return self.schema(t)
            if isinstance(t, Ext) and t.dotted in {"pydantic.BaseModel", "pydantic.main.BaseModel"}:
                return None
        raise self.err(f"model_config: `{ast.unparse(node)}` is not supported (only `Model.model_config` of a "
                       "project model)", node, module)

    def config_of(self, node: ast.AST, module: str) -> list[tuple[list, str]]:
        """`**X.model_config`: X's merged configuration, as keyword groups with the module each was written in."""
        src = self.config_source(node, module)
        if src is None:
            return []
        return [([kw], m) for kw, m in src.__dict__.get("config_kw", {}).values()]

    # Pydantic v2 only warns about renamed/removed v1 keys and IGNORES them (checked: `orm_mode` and
    # `anystr_strip_whitespace` have no effect in a v2 `class Config`). Same here: accepted, no effect.
    V1_IGNORED = {"allow_population_by_field_name", "anystr_lower", "anystr_strip_whitespace", "anystr_upper",
                  "keep_untouched", "max_anystr_length", "min_anystr_length", "orm_mode", "schema_extra",
                  "validate_all", "allow_mutation", "copy_on_model_validation", "error_msg_templates", "fields",
                  "getter_dict", "json_dumps", "json_loads", "post_init_call", "smart_union",
                  "underscore_attrs_are_private"}

    def config_class(self, info: SchemaInfo, node: ast.ClassDef, module: str) -> None:
        """Pydantic v1 `class Config:` (accepted by v2; v2 key names apply, v1 ones are ignored)."""
        items = []
        for stmt in node.body:
            if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
                name = stmt.targets[0].id
                if name in self.V1_IGNORED:
                    continue
                items.append(ast.keyword(arg=name, value=stmt.value))
            elif isinstance(stmt, (ast.Pass, ast.Expr)):
                continue
            else:
                raise self.err("class Config: only simple assignments are supported", stmt, module)
        self.config_items(info, items, module)

    def config_items(self, info: SchemaInfo, keywords, module: str) -> None:
        for kw in keywords:
            if kw.arg is not None:
                info.__dict__.setdefault("config_kw", {})[kw.arg] = (kw, module)
            v = self.const(kw.value, module) if kw.arg not in {"env_file", "json_encoders", "alias_generator"} else None
            if kw.arg == "alias_generator":
                gen = (kw.value, module)
                if isinstance(kw.value, ast.Subscript) and isinstance(kw.value.slice, ast.Constant) \
                        and kw.value.slice.value == "alias_generator":
                    # `Other.model_config["alias_generator"]`: the generator another model was configured with
                    src = self.config_source(kw.value.value, module)
                    got = src.__dict__.get("config_kw", {}).get("alias_generator") if src is not None else None
                    if got is None:  # a KeyError when the class is defined
                        raise self.err(f"`{ast.unparse(kw.value)}`: that model has no alias generator (a KeyError)", kw.value, module)
                    gen = (got[0].value, got[1])
                    info.__dict__.setdefault("config_kw", {})["alias_generator"] = got
                info.__dict__["alias_gen"] = None if isinstance(gen[0], ast.Constant) and gen[0].value is None else gen
                continue
            if kw.arg in {"validate_by_alias", "serialize_by_alias"}:
                info.__dict__["validate_by_alias" if kw.arg == "validate_by_alias" else "ser_by_alias"] = bool(v)
                continue
            if kw.arg == "from_attributes":
                info.from_attributes = bool(v)
            elif kw.arg == "extra":
                info.extra = {"ignore": "Ignore", "forbid": "Forbid", "allow": "Allow"}[v]
            elif kw.arg == "env_prefix":
                info.env_prefix = v
            elif kw.arg == "use_enum_values":
                info.__dict__["enum_values"] = bool(v)
            elif kw.arg in {"populate_by_name", "validate_by_name"}:
                info.__dict__["populate_by_name"] = bool(v)
            elif kw.arg == "case_sensitive":
                info.case_sensitive = bool(v)
            elif kw.arg == "env_ignore_empty":
                info.__dict__["env_ignore_empty"] = bool(v)  # an empty variable is as if unset
            elif kw.arg == "env_parse_none_str":
                if v is not None and not isinstance(v, str):
                    raise self.err("model_config env_parse_none_str= must be a string or None", kw, module)
                info.env_none = v
            elif kw.arg in {"env_file", "env_file_encoding",
                            "arbitrary_types_allowed", "protected_namespaces", "json_schema_extra", "title"}:
                pass
            elif kw.arg == "json_encoders":
                # pydantic 2 (deprecated, still applied): a JSON-mode serializer of the fields of that type (json_encoded)
                if not isinstance(kw.value, ast.Dict) or any(k is None for k in kw.value.keys):
                    raise self.err("model_config json_encoders= must be a dict literal {type: function}", kw, module)
                info.__dict__["json_enc"] = (kw.value, module)
            elif kw.arg in {"defer_build", "cache_strings", "validate_return", "use_attribute_docstrings"}:
                # when the validator is built, pydantic-core's string cache, `@validate_call` only, the JSON schema's
                # descriptions (model_json_schema() refuses a model_config beyond from_attributes/extra): no effect here
                pass
            elif kw.arg == "str_strip_whitespace":
                info.__dict__["strip"] = bool(v)  # the subclass's own config wins over the inherited one
            elif kw.arg == "str_to_lower":
                info.__dict__["lower"] = bool(v)  # the subclass's own config wins over the inherited one
            elif kw.arg == "str_to_upper":
                info.__dict__["upper"] = bool(v)  # the subclass's own config wins over the inherited one
            elif kw.arg == "strict":
                info.__dict__["strict"] = bool(v)
            elif kw.arg in {"str_min_length", "str_max_length"}:
                if v is not None and (not isinstance(v, int) or isinstance(v, bool) or v < 0):
                    raise self.err(f"model_config {kw.arg}= must be a non-negative int", kw, module)
                info.__dict__["smin" if kw.arg == "str_min_length" else "smax"] = v
            elif kw.arg == "validate_assignment":
                info.__dict__["validate_assignment"] = bool(v)
            elif kw.arg == "frozen":
                info.__dict__["frozen"] = bool(v)
            elif kw.arg == "allow_inf_nan":
                if v is not False:  # True is a float's default, but would let a Decimal take inf/nan
                    raise self.err("model_config allow_inf_nan= other than False is not supported", kw, module)
                info.__dict__["finite"] = True  # every float the model validates rejects inf/nan (finite_number)
            elif kw.arg == "validate_default":
                info.__dict__["validate_default"] = bool(v)  # applied to the fields without their own
            elif kw.arg == "hide_input_in_errors":
                info.__dict__["hide_input"] = bool(v)  # str(ValidationError) only: errors() keep their input
            elif kw.arg in {"val_json_bytes", "ser_json_bytes"}:
                if v not in ("utf8", "base64", "hex"):
                    raise self.err(f'model_config {kw.arg}= must be "utf8", "base64" or "hex"', kw, module)
                info.__dict__[kw.arg] = v
            else:
                raise self.err(f"model_config {kw.arg}= is not supported", kw, module)

    # ---------------------------------------------------------------- functions

    def function(self, sym: Sym, variant: str = "plain") -> str:
        """Rust name of a compiled project function (compiled lazily)."""
        key = (sym, variant)
        if self.cur is not None:
            self.edges.setdefault(self.cur, set()).add(key)
        if key not in self.fns:
            name = f"f_{mod_ident(sym.module)}__{ident(sym.name)}" + ("" if variant == "plain" else f"__{variant}")
            self.fns[key] = name
            self.fn_queue.append(key)
        return self.fns[key]

    def fn_node(self, sym: Sym):
        if sym in self.__dict__.get("nested", {}):
            return self.nested[sym][0]
        if "." in sym.name:  # Class.method
            cname, mname = sym.name.split(".", 1)
            cls = self.ix.definition(Sym(sym.module, cname))
            setter = mname.endswith(SETTER_SUFFIX)
            if setter:
                mname = mname[:-len(SETTER_SUFFIX)]
            found = [stmt for stmt in cls.body if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)) and stmt.name == mname
                     and (property_accessor(stmt) == "setter") == setter]
            if setter or not found:
                if not found:
                    raise TranspileError(f"no method {sym.name}")
                return found[0]
            return found[-1]  # the last def of a name is the one the class keeps
        return self.ix.definition(sym)

    SPAN_STMTS = (ast.Assign, ast.AnnAssign, ast.If, ast.Try, ast.For, ast.While, ast.With, ast.Pass)

    def module_spans(self, module: str) -> list[tuple[int, int]]:
        """Runs of module-level statements (indexes in the body, first and last included) from the first to the
        last binding of a name bound by several statements (`X = os.getenv(...)` then `if not X: X = ...`), merged
        when they overlap: run as one block at startup, in order, as CPython runs the module. Only plain assignments
        to names and compound statements (no definitions, imports, calls as statements) may be in one; anything
        else keeps the name refused."""
        spans = self.__dict__.setdefault("_module_spans", {})
        if module not in spans:
            m = self.ix.module(module)
            body = m.tree.body
            found = []
            for bs in m.binders.values():
                if len(bs) < 2 or not all(any(b is st for st in body) for b in bs):
                    continue
                idx = [next(i for i, st in enumerate(body) if st is b) for b in bs]
                found.append((min(idx), max(idx)))
            found.sort()
            merged: list[list[int]] = []
            for a, b in found:
                if merged and a <= merged[-1][1]:
                    merged[-1][1] = max(merged[-1][1], b)
                else:
                    merged.append([a, b])
            ok = []
            for a, b in merged:
                stmts = body[a:b + 1]
                if all(isinstance(st, self.SPAN_STMTS) and not import_guard(st) and not startup_skipped(st)
                       and (not isinstance(st, ast.Assign) or all(isinstance(t, ast.Name) for t in st.targets))
                       and not any(isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Import,
                                                  ast.ImportFrom, ast.Global, ast.Nonlocal)) for n in ast.walk(st))
                       for st in stmts):
                    ok.append((a, b))
            spans[module] = ok
        return spans[module]

    def span_of(self, module: str, st: ast.AST) -> tuple[int, int] | None:
        body = self.ix.module(module).tree.body
        for a, b in self.module_spans(module):
            if any(x is st for x in body[a:b + 1]):
                return a, b
        return None

    def stmt_runner(self, module: str, st: ast.stmt) -> str:
        """A module-level compound statement (`try`, `if`, `for`, `while`, `with`), run once at startup in its
        place: a global holding the values of the names it binds (its locals), in `stmt_bindings` order. A statement
        of a span (module_spans) runs with the whole span."""
        runners = self.__dict__.setdefault("stmt_runners", {})
        span = self.span_of(module, st)
        stmts = self.ix.module(module).tree.body[span[0]:span[1] + 1] if span else [st]
        st = stmts[0]
        key = (module, st.lineno)
        if key not in runners:
            name = f"ms_{mod_ident(module)}__{st.lineno}"
            runners[key] = name
            fc = FnCompiler(self, module, None, name)
            prev, self.cur = self.cur, ("modst", name)
            try:
                bound = list(dict.fromkeys(n for x in stmts for n in stmt_bindings(x)))
                m = self.ix.module(module)
                for n in bound:
                    if len(m.binders.get(n, [])) > 1 and span is None:
                        raise fc.err(f"the module variable `{n}` is bound by several module-level statements "
                                     f"(lines {', '.join(str(b.lineno) for b in m.binders[n])}), with a definition, an "
                                     "import or a call statement among them: not supported", st)
                fc.locals = set(n for x in stmts for n in stmt_bindings(x, handlers=True))
                for n in sorted(fc.locals):
                    fc.emit(f"let mut v_{ident(n)}: V = V::Unbound;")
                fc.block(stmts)
            except TranspileError:
                del runners[key]
                raise
            finally:
                self.cur = prev
            body = "\n".join("    " + l for l in fc.lines)
            out = ", ".join(f"v_{ident(n)}" for n in bound)
            self.items.append(
                f"static G_{name}: {RT}::Global = {RT}::Global::new();\n"
                f"/// {module}:{st.lineno} (module level, run at startup)\n"
                f"pub async fn {name}(cx: &Cx) -> R {{\n"
                f"    G_{name}.get(cx, |cx| Box::pin(async move {{\n{body}\n        #[allow(unreachable_code)]\n"
                f"        Ok(V::tuple(vec![{out}]))\n    }})).await\n}}"
            )
            self.__dict__.setdefault("eager", []).append((module, st.lineno, name))
            self.__dict__.setdefault("modst", set()).add(name)
        return runners[key]

    def global_value(self, sym: Sym) -> str:
        # call-graph node of the global: every reader reaches the functions its value queues (a lambda
        # `lambda: f()` stored by a module-level loop), even when an earlier reader compiled it
        if self.cur is not None:
            self.edges.setdefault(self.cur, set()).add(("global", sym))
        if sym not in self.globals:
            prev, self.cur = self.cur, ("global", sym)
            try:
                return self._global_value(sym)
            finally:
                self.cur = prev
        return self.globals[sym]

    def _global_value(self, sym: Sym) -> str:
        if sym not in self.globals:
            name = f"g_{mod_ident(sym.module)}__{ident(sym.name)}"
            node = self.ix.definition(sym)
            span = self.span_of(sym.module, node) if node is not None else None
            if span is not None or isinstance(node, (ast.Try, ast.If, ast.For, ast.While, ast.With)) and not import_guard(node):
                runner = self.stmt_runner(sym.module, node)
                self.edges.setdefault(("global", sym), set()).add(("modst", runner))
                self.globals[sym] = name
                stmts = self.ix.module(sym.module).tree.body[span[0]:span[1] + 1] if span else [node]
                i = list(dict.fromkeys(n for x in stmts for n in stmt_bindings(x))).index(sym.name)
                self.items.append(
                    f"static G_{name}: {RT}::Global = {RT}::Global::new();\n"
                    f"pub async fn {name}(cx: &Cx) -> R {{\n"
                    f"    G_{name}.get(cx, |cx| Box::pin(async move {{\n"
                    f"        match {runner}(cx).await? {{\n"
                    f"            V::Tuple(t) if !matches!(t[{i}], V::Unbound) => Ok(t[{i}].clone()),\n"
                    f"            _ => Err(Exc::msg(&{RT}::v::NAME_ERROR, {rs(f'name {sym.name!r} is not defined')})),\n"
                    f"        }}\n    }})).await\n}}"
                )
                self.__dict__.setdefault("eager", []).append((sym.module, node.lineno, name))
                return name
            if isinstance(node, ast.AnnAssign) and node.value is None:
                # `X: int` at module level then `global X; X = ...` in a function: no value to start from
                raise self.err(f"`{ast.unparse(node)}` declares the module global `{sym.name}` without a value: give it "
                               f"one (`{ast.unparse(node)} = ...`), py2axum starts every global from its value", node,
                               sym.module)
            self.globals[sym] = name
            self.__dict__.setdefault("eager", []).append((sym.module, getattr(node, "lineno", 0), name))
            fc = FnCompiler(self, sym.module, None, name)
            try:
                value = node.value
                t = self.resolve(sym.module, value.func) if isinstance(value, ast.Call) else None
                if isinstance(node, ast.Assign) and isinstance(t, Ext) and t.dotted in {"sqlalchemy.Table", "sqlalchemy.schema.Table"}:
                    # a Core table: its descriptor, as an association table's
                    code = f"V::Class(&{self.core_table(sym, node).cls})"
                elif self.maker_wrapper(sym, node):
                    code = self.maker_wrapper_value(fc, sym, node)
                else:
                    code = fc.expr(value)
                body = "\n".join("        " + l for l in fc.lines)
                self.items.append(
                    f"static G_{name}: {RT}::Global = {RT}::Global::new();\n"
                    f"pub async fn {name}(cx: &Cx) -> R {{\n"
                    f"    G_{name}.get(cx, |cx| Box::pin(async move {{\n{body}\n        Ok({code})\n    }})).await\n}}"
                )
            except TranspileError:
                del self.globals[sym]
                raise
        return self.globals[sym]

    def module_desc(self, module: str, node, fc: "FnCompiler") -> str:
        """A project module as a value: a static descriptor whose `attr` reads its top-level names (each
        compiled like a `from module import name`; a name that does not translate raises when read)."""
        mods = self.__dict__.setdefault("module_descs", {})
        if module not in mods:
            m = self.ix.module(module)
            if m.stars:
                raise fc.err(f"module `{module}` used as a value has `import *`: not supported", node)
            name = f"MOD_{mod_ident(module).upper()}"
            mods[module] = name
            arms = []
            for attr in sorted({*m.defs, *m.imports}):
                afc = FnCompiler(self, module, None, f"mattr_{mod_ident(module)}")
                try:
                    code = afc.ref_value(self.ix.resolve(module, attr), ast.Name(id=attr, lineno=1, col_offset=0))
                    body = "".join(f"{l} " for l in afc.lines)
                    arms.append(f"            {rs(attr)} => {{ {body}Ok({code}) }}")
                except TranspileError as e:
                    arms.append(f"            {rs(attr)} => Err(Exc::runtime({rs('py2axum: ' + e.render())})),")
            arms.append(f"            \"__name__\" => Ok(V::str({rs(module)})),")
            arms.append(f"            _ => Err(Exc::attr_error(format!(\"module '{module}' has no attribute '{{name}}'\"))),")
            fn = f"mattr_{mod_ident(module)}"
            self.items.append(
                f"fn {fn}<'a>(cx: &'a Cx, name: &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = R> + Send + 'a>> {{\n"
                f"    Box::pin(async move {{\n        match name {{\n" + "\n".join(arms) + "\n        }\n    })\n}\n"
                f"pub static {name}: {RT}::v::ModDesc = {RT}::v::ModDesc {{ name: {rs(module)}, attr: {fn} }};"
            )
        return mods[module]

    def decorated_value(self, sym: Sym) -> str:
        """A module-level `def` with project decorators: `d1(d2(f))` built once at startup (decorator
        expressions evaluated top to bottom, then applied bottom-up, like CPython at import)."""
        dec = self.__dict__.setdefault("decorated", {})
        if self.cur is not None:
            self.edges.setdefault(self.cur, set()).add(("decorated", sym))
        if sym not in dec:
            name = f"gd_{mod_ident(sym.module)}__{ident(sym.name)}"
            dec[sym] = name
            node = self.fn_node(sym)
            fc = FnCompiler(self, sym.module, None, name)
            prev, self.cur = self.cur, ("decorated", sym)
            try:
                for d in node.decorator_list:
                    ref = fc.static_ref(d.func if isinstance(d, ast.Call) else d)
                    if isinstance(ref, Ext) and ref.dotted.startswith("cachetools.") and isinstance(node, ast.AsyncFunctionDef):
                        raise fc.err(f"@{ref.dotted.removeprefix('cachetools.')} on an async function is not supported (cachetools "
                                     "caches the coroutine object, which a second await refuses)", d)
                decos = [mcp_tool_decorator(self, fc, sym, node, d) or fc.expr(d) for d in node.decorator_list]
                val = raw_fn_value(self, sym)
                # decorators introspect the function (typing.get_type_hints): its annotations as type values
                anns = [(a.arg, a.annotation) for a in [*node.args.posonlyargs, *node.args.args, *([node.args.vararg] if node.args.vararg else []),
                                                         *node.args.kwonlyargs, *([node.args.kwarg] if node.args.kwarg else [])]
                        if a.annotation is not None]
                if node.returns is not None:
                    anns.append(("return", node.returns))
                items = ", ".join(f"(V::str({rs(n)}), {fc.annot_value(a)})" for n, a in anns)
                val = fc.q(f"{RT}::with_annotations({val}, vec![{items}])")
                for d in reversed(decos):
                    val = fc.q(f"{RT}::methods::call_value(cx, &{d}, vec![{val}], vec![]).await")
                body = "\n".join("        " + l for l in fc.lines)
                self.items.append(
                    f"static G_{name}: {RT}::Global = {RT}::Global::new();\n"
                    f"pub async fn {name}(cx: &Cx) -> R {{\n"
                    f"    G_{name}.get(cx, |cx| Box::pin(async move {{\n{body}\n        Ok({val})\n    }})).await\n}}"
                )
                self.__dict__.setdefault("eager", []).append((sym.module, node.lineno, name))
            except TranspileError:
                del dec[sym]
                raise
            finally:
                self.cur = prev
        return dec[sym]

    def drain(self) -> None:
        while self.fn_queue:
            key = self.fn_queue.pop()
            sym, variant = key
            if variant == "cached":
                inner = self.function(sym)
                name = self.fns[key]
                self.items.append(
                    f"static G_{name}: {RT}::Global = {RT}::Global::new();\n"
                    f"pub async fn {name}(cx: &Cx) -> R {{ G_{name}.get(cx, |cx| Box::pin({inner}(cx))).await }}"
                )
                continue
            prev, self.cur = self.cur, key
            try:
                node = self.fn_node(sym)
                fc = FnCompiler(self, sym.module, node, self.fns[key], variant=variant)
                if sym in self.__dict__.get("nested", {}):
                    fc.factory = self.nested[sym][1]
                self.items.append(fc.compile_function())
            except TranspileError as e:
                if not self.collect and not self.__dict__.get("defer_fn_errors"):
                    raise
                self.errors.append(e)
                self.fn_errors[key] = e
                try:
                    stub_node = self.fn_node(sym)
                    nparams = len(fn_params(stub_node))
                except Exception:
                    stub_node, nparams = None, 0
                # the stub keeps the signature FnCompiler would have given it: a generator also takes its
                # yielder (its callers pass one)
                gen = "" if stub_node is None or isinstance(stub_node, ast.Lambda) or not has_yield(stub_node) \
                    else f", _: &{RT}::web::DepYield" if variant == "depgen" else f", _: &{RT}::web::Yielder"
                self.items.append(
                    f"pub async fn {self.fns[key]}(cx: &Cx{', _: V' * nparams}{gen}) -> R {{\n"
                    f"    Err(Exc::runtime({rs('py2axum: ' + e.render())}))\n}}"
                )
            finally:
                self.cur = prev


# ====================================================================== function compiler


# sentry_sdk.init options the binary reproduces (dynrt/sentry.rs); the platform ones (stack frames, local
# variables, source context, in-app rules) are accepted: there is no Python stack to describe
SENTRY_INIT_OPTIONS = {
    "dsn", "environment", "release", "server_name", "dist", "sample_rate", "traces_sample_rate", "traces_sampler",
    "enable_tracing", "send_default_pii", "attach_stacktrace", "max_value_length", "max_breadcrumbs", "before_send",
    "before_send_transaction", "integrations", "default_integrations", "auto_enabling_integrations", "debug",
    "shutdown_timeout", "max_request_body_size", "include_local_variables", "include_source_context",
    "in_app_include", "in_app_exclude", "project_root", "send_client_reports",
}
SENTRY_INIT_REFUSED = {
    "transport": "the binary sends envelopes through the Rust SDK's HTTP transport and cannot run a Python "
                 "Transport class (any Sentry server since 20.6 accepts envelopes); remove the option or declare "
                 "the code --python-side",
    "before_breadcrumb": "breadcrumbs are recorded by the runtime without a Python hook",
    "event_scrubber": "the default EventScrubber is applied, a custom one is not",
    "error_sampler": "use sample_rate=",
    "ignore_errors": "filter in before_send instead",
    "profiles_sample_rate": "profiling has no equivalent in the binary",
    "profiles_sampler": "profiling has no equivalent in the binary",
    "profile_session_sample_rate": "profiling has no equivalent in the binary",
    "enable_logs": "Sentry Logs are not reproduced",
    "before_send_log": "Sentry Logs are not reproduced",
    "trace_propagation_targets": "outgoing requests carry no trace headers in the binary",
    "propagate_traces": "outgoing requests carry no trace headers in the binary",
    "functions_to_trace": "no child spans in the binary",
    "http_proxy": "the Rust transport reads the proxy from the environment (HTTP_PROXY)",
    "https_proxy": "the Rust transport reads the proxy from the environment (HTTPS_PROXY)",
    "ca_certs": "the Rust transport uses the system roots",
    "_experiments": "experimental options are not reproduced",
}
_STARLETTE_OPTS = ({"transaction_style", "middleware_spans"},
                   {"failed_request_status_codes": "5xx only (the default)",
                    "http_methods_to_capture": "the default methods only (HEAD and OPTIONS are not traced)"})
SENTRY_INTEGRATIONS = {
    "sentry_sdk.integrations.fastapi.FastApiIntegration": ("fastapi", *_STARLETTE_OPTS),
    "sentry_sdk.integrations.starlette.StarletteIntegration": ("starlette", *_STARLETTE_OPTS),
    "sentry_sdk.integrations.logging.LoggingIntegration": ("logging", {"level", "event_level", "sentry_logs_level"}, {}),
    "sentry_sdk.integrations.sqlalchemy.SqlalchemyIntegration": ("sqlalchemy", set(), {}),
    "sentry_sdk.integrations.asyncio.AsyncioIntegration": ("asyncio", set(), {}),
}


class FnCompiler:
    def __init__(self, proj: Project, module: str, node, rust_name: str, *, variant: str = "plain",
                 captures: dict[str, str] | None = None, parent: "FnCompiler | None" = None):
        self.p = proj
        self.module = module
        self.node = node
        self.name = rust_name
        self.variant = variant
        self.lines: list[str] = []
        self.ind = 1
        self.locals: set[str] = set()
        self.params: set[str] = set()
        self.captures = captures or {}
        # `nonlocal`: own locals held in a cell (a nested function rebinds them), captured names that are
        # cells of an enclosing function, and the names this function rebinds there
        self.cells: set[str] = set()
        self.param_cells: set[str] = set()
        self.comp_cells: set[str] = set()  # Rust names of comprehension variables held in a cell
        self.cell_caps: set[str] = {v for v in self.captures if parent is not None and parent.is_cell(v)}
        self.nonlocals: set[str] = set()
        self.parent = parent
        self.comp_scopes: list[dict[str, str]] = []
        self.sinks: list[tuple[str, str]] = []  # (label, slot) of enclosing try bodies
        self.ret_capture: list[tuple[str, str]] = []  # (label, slot) of enclosing try/finally
        # per ret_capture entry: the try's jump slot and the break/continue it carries out of its finally,
        # [(kind, loop label, loop barrier)]: replayed after the finally (in CPython, the finally runs first)
        self.jumps: list[tuple[str, list]] = []
        self.loops: list[str] = []
        self.loop_barrier: list[int] = []
        self.cur_exc: list[str] = []
        self.gen = node is not None and not isinstance(node, ast.Lambda) and has_yield(node)
        self.extra_fns: list[str] = []
        # tracebacks: the nodes being compiled (the innermost is where an exception passes), and the Rust
        # name of the frame's id (a sub-compiler of the same Python frame shares its parent's)
        self.pos_nodes: list = []
        self.fid_name: str | None = parent.fid_name if (parent is not None and node is None) else None

    # ---------------------------------------------------------------- output

    def emit(self, line: str) -> None:
        self.lines.append("    " * self.ind + line)

    def err(self, msg: str, node) -> TranspileError:
        return self.p.err(msg, node, self.module)

    def lib_err(self, msg: str, dotted: str, node) -> TranspileError:
        """A library name outside the library map: why its installed distribution was not translated from source
        (--translate-libs), when it is installed."""
        why = self.p.ix.libs.why_not(dotted) if self.p.ix.libs is not None else None
        hint = libmap.stdlib_hint(dotted)
        if why is None:
            return self.err(f"{msg}: {hint}" if hint else msg, node)
        text, code = why
        if not code:
            return self.err(f"{msg}; {text}", node)
        return TranspileError(f"{msg}: {text}", node, self.p.src(self.module), code=code)

    def note_native(self, dotted: str, node) -> None:
        """A name the library map translates, of a library it covers in part (`jinja2.Environment`): kept to refuse
        the routes that would mix it with the library's source in one binary (link_partial_mix). Its exceptions are
        the map's in the source too (ModuleIndex.resolve): they mix."""
        libs = self.p.ix.libs
        top = dotted.split(".")[0]
        if libs is None or dotted in libmap.EXCEPTIONS or libs.partial(top) is None or not libs.mapped(dotted):
            return
        self.p.__dict__.setdefault("partial_native", {}).setdefault(top, {}).setdefault(
            self.p.cur, (dotted, self.p.src(self.module), getattr(node, "lineno", 0)))

    def tmp(self, prefix: str = "t") -> str:
        return f"__{prefix}{self.p.uid()}"

    def cur_pos_node(self):
        fc = self
        while fc is not None:
            if fc.pos_nodes:
                return fc.pos_nodes[-1]
            fc = fc.parent
        return None

    def tb_at(self, r_expr: str) -> str:
        """`r_expr` recording, when it fails, the traceback entry of the innermost node being compiled."""
        node = self.cur_pos_node()
        root = self
        while root.parent is not None:
            root = root.parent
        if getattr(root.node, "py2axum_noframe", False):
            # a function the translator writes in place of FastAPI's own call (`Depends(Cls)` calls the class):
            # no frame of its own in tracebacks
            return r_expr
        if node is None or self.p.ix.is_shim_module(self.module):
            # a library class translated from py2axum's source: its frames are a library's (not in tracebacks)
            return r_expr
        i = self.p.tb_index(self.module, self.py_func_name(), node)
        return f"{RT}::trace::at(cx, {r_expr}, &self::TB[{i}], {self.fid_name or '0'})"

    def q_method(self, r_expr: str, node: ast.Call) -> str:
        """`q` for `obj.name(...)`: a missing attribute is located at `obj.name` (LOAD_ATTR), anything the call
        raises at the whole call, as in CPython's tracebacks."""
        if (getattr(node.func, "end_col_offset", None) is None or getattr(node, "end_col_offset", None) is None
                or self.p.ix.is_shim_module(self.module)):
            return self.q(r_expr)
        f = self.py_func_name()
        # the call's position is the innermost node being compiled (`await obj.m()`: the await)
        cur = self.cur_pos_node() or node
        call, attr = self.p.tb_index(self.module, f, cur), self.p.tb_index(self.module, f, node.func)
        return self.q(f"{RT}::trace::at_attr(cx, {r_expr}, &self::TB[{call}], &self::TB[{attr}], "
                      f"{rs(node.func.attr)}, {self.fid_name or '0'})", located=True)

    def q(self, r_expr: str, located: bool = False) -> str:
        """Unwrap a `R` expression, routing the error to the enclosing try or out of the function."""
        if not located:
            r_expr = self.tb_at(r_expr)
        if self.sinks:
            label, slot = self.sinks[-1]
            return f"tri!({r_expr}, {label}, {slot})"
        return f"({r_expr})?"

    def raise_code(self, exc: str) -> str:
        if self.sinks:
            label, slot = self.sinks[-1]
            return f"{{ {slot} = Some({exc}); break {label}; }}"
        return f"return Err({exc});"

    # ---------------------------------------------------------------- function

    def param_list(self) -> list[ParamSpec]:
        if self.node is None:
            return []
        params = fn_params(self.node)
        if self.variant in {"method", "classmethod"} and params:
            return params  # self/cls is the first parameter, passed explicitly
        return params

    def compile_function(self) -> str:
        node = self.node
        params = self.param_list()
        self.params = {p.name for p in params}
        self.globals_decl: dict[str, Sym] = {}
        for n in walk_scope(node):
            if isinstance(n, ast.Nonlocal):
                for name in n.names:
                    if name not in self.cell_caps:
                        raise self.err(f"`nonlocal {name}`: only a local variable of the enclosing function is supported", n)
                    self.nonlocals.add(name)
            if isinstance(n, ast.Global):
                for name in n.names:
                    sym = Sym(self.module, name)
                    if not isinstance(self.p.ix.definition(sym), (ast.Assign, ast.AnnAssign, ast.Try, ast.If, ast.For, ast.While, ast.With)):
                        raise self.err(f"`global {name}`: only a variable assigned at module level is supported", n)
                    self.globals_decl[name] = sym
        self.locals = assigned_names(node.body) - self.params - set(self.globals_decl) - self.nonlocals
        rebound = {name for n in ast.walk(node) if isinstance(n, ast.Nonlocal) and n is not node for name in n.names}
        for n in ast.walk(node):
            if isinstance(n, ast.Nonlocal) and n is not node:
                for name in set(n.names) & self.params - self.nonlocals:
                    raise self.err(f"`nonlocal {name}` naming a parameter of the enclosing function is not supported", n)
        # cell variables, as CPython: a local a nested function or lambda reads is shared with it (read at call
        # time, not when the function is defined), and so is a parameter it reads that the body rebinds
        captured = set()
        for n in walk_scope(node):
            if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda)):
                captured |= set(self.free_vars(n))
        self.cells = ((rebound - self.nonlocals) | captured) & self.locals
        self.param_cells = captured & self.params & assigned_names(node.body)
        self.cells |= self.param_cells
        # function-level imports that resolve to nothing: raised at run time, names are plain locals
        self.failed_imports = set()
        for n in walk_scope(node):
            if isinstance(n, (ast.Import, ast.ImportFrom)):
                for a in n.names:
                    local = (a.asname or a.name).split(".")[0]
                    if self.p.resolve(self.module, ast.Name(local, ast.Load()), node) is None:
                        self.failed_imports.add(local)
        self.locals |= self.failed_imports
        self.fid_name = "__fid"
        sig = ", ".join(f"mut v_{ident(p.name)}: V" for p in params)
        gen_param = (f", y: &{RT}::web::DepYield" if self.variant == "depgen" else f", y: &{RT}::web::Yielder") if self.gen else ""
        caps = "".join(f", {c}: V" for c in self.captures.values())
        traced = self.p.traced()
        self.__dict__["traced_fn"] = traced
        for name in sorted(self.param_cells):
            self.emit(f"let v_{ident(name)}: V = {RT}::ops::cell_of(v_{ident(name)});")
        for name in sorted(self.locals):
            if name in self.cells:
                self.emit(f"let v_{ident(name)}: V = {RT}::ops::cell_new();")
            else:
                self.emit(f"let mut v_{ident(name)}: V = V::Unbound;")
        self.block(node.body)
        self.emit("#[allow(unreachable_code)]")
        self.emit("Ok(V::None)")
        head = f"pub async fn {self.name}(cx: &Cx{', ' + sig if sig else ''}{caps}{gen_param}) -> R {{"
        # RecursionError rather than a stack overflow (the process would abort): dynrt::stack_guard; the
        # frame's id (an address): a traceback records each frame once per invocation (dynrt/trace.rs)
        guard = f"    {RT}::stack_guard()?;\n    let __fidv = 0u8;\n    let __fid = std::ptr::addr_of!(__fidv) as usize;"
        if traced:
            # sys.settrace: `call` on entry, `return` / `exception` on exit (dynrt/trace.rs)
            ln, qual = node.lineno, self.qualname() or node.name
            pre = [f"    let __tf = ({RT}::trace::enter(cx, {rs(self.module)}, {rs(qual)}, {rs(self.p.rel_file(self.module))}, {ln}).await)?;",
                   f"    let __ln = std::sync::atomic::AtomicU32::new({ln});",
                   "    let (__lnr, __tfr) = (&__ln, &__tf);",
                   "    let _ = (__lnr, __tfr);",
                   "    let __r: R = async move {"]
            post = ["    }.await;",
                    f"    {RT}::trace::leave(cx, __tf, __r, __ln.load(std::sync::atomic::Ordering::Relaxed)).await"]
            return "\n".join([*self.extra_fns, head, guard, *pre, *("    " + l for l in self.lines), *post, "}"])
        return "\n".join([*self.extra_fns, head, guard, *self.lines, "}"])

    # ---------------------------------------------------------------- statements

    def block(self, stmts: list[ast.stmt]) -> None:
        i = 0
        while i < len(stmts):
            if n := self.streamed_list_return(stmts, i):
                i += n
                continue
            self.stmt(stmts[i])
            if isinstance(stmts[i], (ast.Return, ast.Raise, ast.Continue, ast.Break)):
                break  # the rest of the block never runs (CPython compiles it, names unresolved included)
            i += 1

    def streamed_list_return(self, stmts: list[ast.stmt], i: int) -> int:
        """A list returned straight from the session (`list_return`): when this function is the endpoint of a
        route with a list response_model, `orm::defer_list` may hand the statement to the response, which
        streams it; otherwise the statements run as written. Not under try/with (an exception of the query
        would escape them), in a generator, nor traced. Returns the statements consumed (0: not this shape)."""
        if (self.variant != "plain" or self.parent is not None or self.gen or self.sinks or self.ret_capture
                or self.__dict__.get("traced_fn")):
            return 0
        found = list_return(stmts, i)
        if found is None:
            return 0
        call, n = found
        ts, tq = f"_p2a_ls{self.p.uid()}", f"_p2a_lq{self.p.uid()}"
        self.emit(f"let mut v_{ident(ts)}: V = {self.expr(call.func.value)};")
        self.emit(f"let mut v_{ident(tq)}: V = {self.expr(call.args[0])};")
        self.locals |= {ts, tq}
        d = self.q(f"{RT}::orm::defer_list(cx, {rs(self.name)}, &v_{ident(ts)}, &v_{ident(tq)}).await")
        self.emit(f"if let Some(__l) = {d} {{ return Ok(__l); }}")
        # the session and the statement already evaluated: the same calls, on their values
        copied = copy.deepcopy(stmts[i:i + n])
        c2, _ = list_return(copied, 0)
        c2.func.value = ast.copy_location(ast.Name(ts, ast.Load()), c2.func.value)
        c2.args[0] = ast.copy_location(ast.Name(tq, ast.Load()), c2.args[0])
        for node in copied:
            self.stmt(node)
        return n

    def stmt(self, node: ast.stmt) -> None:
        done = self.p.fe.__dict__.get("configure_includes", ())
        if done and ((isinstance(node, ast.Expr) and id(node.value) in done) or (
                isinstance(node, ast.For) and all(isinstance(b, ast.Expr) and id(b.value) in done for b in node.body)) or (
                # `if lib_router is not None: app.include_router(lib_router)`: the include is relayed to Python, the
                # test has no side effect (no call)
                isinstance(node, ast.If) and not node.orelse and not any(isinstance(n, (ast.Call, ast.Await, ast.NamedExpr))
                                                                         for n in ast.walk(node.test))
                and all(isinstance(b, ast.Expr) and id(b.value) in done for b in node.body))):
            return  # `app.include_router(...)` (or a loop of them) registered statically by the frontend
        if getattr(node, "end_col_offset", None) is None:
            return self.stmt_(node)
        self.pos_nodes.append(node)
        try:
            return self.stmt_(node)
        finally:
            self.pos_nodes.pop()

    YIELD_FROM_CALLS = {"range", "sorted", "reversed", "enumerate", "zip", "list", "tuple"}

    def yield_from_loop(self, node: ast.Expr) -> ast.For:
        """`yield from it` as a statement, `it` not a generator (an open file, a literal, a builtin's result): the
        same items as `for x in it: yield x`. A generator would also receive send()/throw()/close() through it,
        and give its return value: refused."""
        it = node.value.value
        ok = isinstance(it, (ast.List, ast.Tuple, ast.Set)) or (isinstance(it, ast.Constant) and isinstance(it.value, (str, bytes)))
        if (isinstance(it, ast.Call) and isinstance(it.func, ast.Name) and it.func.id in self.YIELD_FROM_CALLS
                and self.p.resolve(self.module, it.func) is None):
            ok = True
        if isinstance(it, ast.Name) and self.node is not None:
            # bound by `with open(...) as name` in this function (and nowhere else)
            withs = [i for n in walk_scope(self.node) if isinstance(n, (ast.With, ast.AsyncWith)) for i in n.items
                     if isinstance(i.optional_vars, ast.Name) and i.optional_vars.id == it.id]
            others = [n for n in walk_scope(self.node) if isinstance(n, ast.Name) and n.id == it.id and isinstance(n.ctx, ast.Store)]
            ok = len(withs) == 1 and len(others) == 1 and self.file_like_call(withs[0].context_expr, self.module)
        if not ok:
            raise self.err("`yield from` is supported as a statement over a file opened by `with open(...) as f` (or "
                           "io.BytesIO/StringIO), a "
                           "literal or a builtin's result (a generator would also take send()/throw()/close() and "
                           "give its return value)", node)
        n = self.__dict__["yf_count"] = self.__dict__.get("yf_count", 0) + 1
        var = f"__p2a_yf{n}"
        self.locals.add(var)
        self.emit(f"let mut v_{ident(var)}: V = V::Unbound;")
        loop = ast.For(target=ast.Name(id=var, ctx=ast.Store()), iter=it,
                       body=[ast.Expr(ast.Yield(ast.Name(id=var, ctx=ast.Load())))], orelse=[])
        ast.copy_location(loop, node)
        ast.fix_missing_locations(loop)
        for x in ast.walk(loop):
            ast.copy_location(x, node) if getattr(x, "lineno", None) is None else None
        return loop

    def file_like_call(self, call: ast.AST, module: str, depth: int = 0) -> bool:
        """`open(...)`, `io.BytesIO(...)`, `io.StringIO(...)`, or a call of a project function that returns one of
        those: a file whose `__enter__` gives itself (not a generator)"""
        if not isinstance(call, ast.Call):
            return False
        if isinstance(call.func, ast.Name) and call.func.id == "open" and self.p.resolve(module, call.func) is None:
            return True
        t = self.p.resolve(module, call.func)
        if is_ext(t, "io.BytesIO", "io.StringIO"):
            return True
        d = self.p.ix.definition(t) if isinstance(t, Sym) else None
        if depth < 3 and isinstance(d, ast.FunctionDef) and not d.decorator_list and not has_yield(d):
            rets = [n for n in walk_scope(d) if isinstance(n, ast.Return)]
            return bool(rets) and all(self.file_like_call(r.value, t.module, depth + 1) for r in rets)
        return False

    def stmt_(self, node: ast.stmt) -> None:
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.YieldFrom) and self.gen:
            return self.stmt(self.yield_from_loop(node))
        if self.__dict__.get("traced_fn") and getattr(node, "lineno", None):
            self.emit(f"__lnr.store({node.lineno}, std::sync::atomic::Ordering::Relaxed);")
        if (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call) and isinstance(node.value.func, ast.Name)
                and node.value.func.id in {"\0ctx_exit", "\0ctx_exc"}):
            # `with`: __exit__(None, None, None) on a normal exit, __exit__(type, exc, tb) when the body raises
            m, flag = (a.value for a in node.value.args)
            if node.value.func.id == "\0ctx_exit":
                self.emit(f"if !{flag} {{")
                self.emit(f"    {self.q(f'{RT}::thread::exit(cx, &{m}, None).await')};")
                self.emit("}")
            else:
                e = self.cur_exc[-1]
                self.emit(f"{flag} = true;")
                sup = self.q(f"{RT}::thread::exit(cx, &{m}, Some({e}.clone())).await")
                self.emit(f"if !{RT}::ops::truthy(&{sup})? {{ {self.raise_code(f'{e}.clone()')} }}")
            return
        if (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call) and isinstance(node.value.func, ast.Name)
                and node.value.func.id == "\0actx_exit"):
            # normal exit (finally): __aexit__(None, None, None) unless the exception path already ran it
            m, flag = (a.value for a in node.value.args)
            self.emit(f"if !{flag} {{")
            self.emit(f"    {self.q(f'{RT}::aio::aexit(cx, &{m}, None).await')};")
            self.emit("}")
            return
        if (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call) and isinstance(node.value.func, ast.Name)
                and node.value.func.id == "\0actx_exc"):
            # exception path: __aexit__(type, exc, tb); a true result suppresses the exception
            m, flag = (a.value for a in node.value.args)
            e = self.cur_exc[-1]
            self.emit(f"{flag} = true;")
            sup = self.q(f"{RT}::aio::aexit(cx, &{m}, Some({e}.clone())).await")
            self.emit(f"if !{RT}::ops::truthy(&{sup})? {{ {self.raise_code(f'{e}.clone()')} }}")
            return
        if isinstance(node, ast.With):
            self.with_stmt(node)
            return
        if isinstance(node, ast.AsyncWith):
            self.async_with_stmt(node)
            return
        if isinstance(node, ast.Match):
            self.match_stmt(node)
            return
        if isinstance(node, ast.Expr):
            if isinstance(node.value, ast.Constant):
                return
            self.emit(f"let _ = {self.expr(node.value)};")
        elif isinstance(node, ast.Assign):
            val = self.expr(node.value)
            if len(node.targets) == 1:
                self.assign(node.targets[0], val)
            else:
                t = self.tmp()
                self.emit(f"let {t} = {val};")
                for tg in node.targets:
                    self.assign(tg, f"{t}.clone()")
        elif isinstance(node, ast.AnnAssign):
            if node.value is not None:
                self.assign(node.target, self.expr(node.value))
        elif isinstance(node, ast.AugAssign):
            self.aug_assign(node)
        elif isinstance(node, ast.If):
            self.emit(f"if {self.truthy(node.test)} {{")
            self.ind += 1
            self.block(node.body)
            self.ind -= 1
            if node.orelse:
                self.emit("} else {")
                self.ind += 1
                self.block(node.orelse)
                self.ind -= 1
            self.emit("}")
        elif isinstance(node, ast.Return):
            v = self.expr(node.value) if node.value is not None else "V::None"
            if self.ret_capture:
                label, slot = self.ret_capture[-1]
                self.emit(f"{{ {slot} = Some({v}); break {label}; }}")
            else:
                self.emit(f"return Ok({v});")
        elif isinstance(node, ast.Raise):
            self.raise_stmt(node)
        elif isinstance(node, ast.For):
            self.for_stmt(node)
        elif isinstance(node, ast.AsyncFor):
            self.async_for_stmt(node)
        elif isinstance(node, ast.While):
            lbl = f"'l{self.p.uid()}"
            flag = self.loop_else_start(node, lbl)
            self.emit(f"{lbl}: loop {{")
            self.ind += 1
            self.emit(f"if !{self.truthy(node.test)} {{ break; }}")
            self.loops.append(lbl)
            self.loop_barrier.append(len(self.ret_capture))
            self.block(node.body)
            self.loops.pop()
            self.loop_barrier.pop()
            self.ind -= 1
            self.emit("}")
            self.loop_else_end(node, flag)
        elif isinstance(node, (ast.Break, ast.Continue)):
            if not self.loops:
                raise self.err("break/continue outside a loop", node)
            flag = self.__dict__.get("loop_flags", {}).get(self.loops[-1])
            if isinstance(node, ast.Break) and flag is not None:
                self.emit(f"{flag} = false;")
            kind = "break" if isinstance(node, ast.Break) else "continue"
            if self.loop_barrier[-1] != len(self.ret_capture):
                # leaves a try/finally (or a with): through the innermost finally, then on outwards
                self.emit(self.jump_code(kind, self.loops[-1], self.loop_barrier[-1]))
            else:
                self.emit(f"{kind} {self.loops[-1]};")
        elif isinstance(node, ast.Try):
            self.try_stmt(node)
        elif isinstance(node, ast.Pass):
            pass
        elif isinstance(node, (ast.Import, ast.ImportFrom)):
            # resolved statically (function-level imports); a name that resolves nowhere is an
            # ImportError at run time, as in Python (`try: from x import y` / `except ImportError`)
            missing = [a for a in node.names if (a.asname or a.name).split(".")[0] in self.__dict__.get("failed_imports", set())]
            if missing:
                mod = getattr(node, "module", None) or missing[0].name
                msg = rs(f"cannot import name '{missing[0].name}' from '{mod}'" if isinstance(node, ast.ImportFrom)
                         else f"No module named '{missing[0].name}'")
                cls = "IMPORT_ERROR" if isinstance(node, ast.ImportFrom) else "MODULE_NOT_FOUND_ERROR"
                self.emit(self.raise_code(f"Exc::new(&{RT}::v::{cls}, vec![V::str({msg})])"))
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            self.nested_def(node)
        elif isinstance(node, ast.Assert):
            msg = self.expr(node.msg) if node.msg else "V::None"
            args = f"vec![{msg}]" if node.msg else "vec![]"
            self.emit(f"if !{self.truthy(node.test)} {{ {self.raise_code(f'Exc::new(&{RT}::v::ASSERTION_ERROR, {args})')} }}")
        elif isinstance(node, (ast.Global, ast.Nonlocal)):
            pass  # declared names were collected by compile_function
        elif isinstance(node, ast.Delete):
            for t in node.targets:
                if isinstance(t, ast.Subscript):
                    self.check_writable(t.value)
                    self.emit(f"{self.q(f'{RT}::ops::delitem(&{self.expr(t.value)}, &{self.expr(t.slice)})')};")
                elif isinstance(t, ast.Name) and (t.id in self.locals or t.id in self.nonlocals):
                    self.emit(f"{self.store_name(t.id, t)} = V::Unbound;")
                elif isinstance(t, ast.Name) and self.node is None and t.id in self.p.ix.module(self.module).imports \
                        and not any(isinstance(n, ast.Name) and n.id == t.id and isinstance(n.ctx, ast.Load)
                                    and n.lineno > t.lineno for n in ast.walk(self.p.ix.module(self.module).tree)):
                    pass  # `del redis` after a module-level import guard: imports are resolved statically, never read again
                else:
                    raise self.err("unsupported del target", t)
        else:
            raise self.err(f"unsupported statement `{type(node).__name__}`", node)

    def assign(self, target: ast.AST, val: str) -> None:
        # a failing store (STORE_ATTR, STORE_SUBSCR) is located at its target in a traceback, as in CPython
        if isinstance(target, (ast.Attribute, ast.Subscript)) and getattr(target, "end_col_offset", None) is not None:
            self.pos_nodes.append(target)
            try:
                return self.assign_(target, val)
            finally:
                self.pos_nodes.pop()
        return self.assign_(target, val)

    def assign_(self, target: ast.AST, val: str) -> None:
        if isinstance(target, ast.Name):
            if self.set_global(target.id, val):
                return
            self.emit(f"{self.store_name(target.id, target)} = {val};")
        elif isinstance(target, (ast.Tuple, ast.List)):
            stars = [i for i, e in enumerate(target.elts) if isinstance(e, ast.Starred)]
            if len(stars) > 1:
                raise self.err("multiple starred expressions in assignment", target)
            if stars:
                i = stars[0]
                after = len(target.elts) - i - 1
                t = self.tmp()
                self.emit(f"let {t} = {self.q(f'{RT}::ops::unpack_star(&{val}, {i}, {after})')};")
                for j, e in enumerate(target.elts):
                    self.assign(e.value if isinstance(e, ast.Starred) else e, f"{t}[{j}].clone()")
                return
            t = self.tmp()
            self.emit(f"let {t} = {self.q(f'{RT}::ops::unpack(&{val}, {len(target.elts)})')};")
            for i, e in enumerate(target.elts):
                self.assign(e, f"{t}[{i}].clone()")
        elif isinstance(target, ast.Attribute):
            if isinstance(target.value, ast.Name) and target.value.id in self.request_names() and target.attr not in {"_body", "_receive"}:
                # `request._body = data` (with `request._receive`): what a BaseHTTPMiddleware hands the inner app
                raise self.err(f"attribute .{target.attr} of a Request cannot be assigned here: a Request keeps no attribute of its "
                               "own (use request.state)", target)
            ref = self.static_ref(target)
            if isinstance(ref, Ext) and ref.dotted in libmap.HOOKS:
                # `aiohttp.ClientSession._request = wrapper(...)`: the runtime's client calls the wrapper
                self.emit(f"{self.q(libmap.HOOKS[ref.dotted].format(val=val))};")
                return
            if isinstance(self.static_ref(target.value), Ext):
                raise self.err(f"assigning `{ast.unparse(target)}` (a library attribute) is not supported", target)
            if isinstance(base := self.static_ref(target.value), ModRef):
                sym = Sym(base.module, target.attr)
                if sym in self.p.rebound_globals():
                    # `conf.api_key = v` (`stripe.api_key = ...`): rebinds that module's variable, as `global` would
                    g = self.p.global_value(sym)
                    t = self.tmp()
                    self.emit(f"let {t} = {val};")
                    self.emit(f"{self.q(f'{g}(cx).await')};")
                    self.emit(f"G_{g}.set({t});")
                    return
                raise self.err(f"assigning `{ast.unparse(target)}` (an attribute of module {base.module}) is not supported: "
                               "assign it in that module (`global`) or keep it in a mutable container", target)
            obj = self.expr(target.value)
            self.emit(f"{self.q(f'{RT}::methods::setattr_cx(cx, &{obj}, {rs(target.attr)}, {val}).await')};")
        elif isinstance(target, ast.Subscript) and isinstance(target.slice, ast.Slice):
            self.check_writable(target.value)
            obj = self.expr(target.value)
            sl = target.slice
            parts = [self.expr(x) if x is not None else "V::None" for x in (sl.lower, sl.upper, sl.step)]
            self.emit(f"{self.q(f'{RT}::ops::setslice(&{obj}, &{parts[0]}, &{parts[1]}, &{parts[2]}, {val})')};")
        elif isinstance(target, ast.Subscript):
            self.check_writable(target.value)
            obj = self.expr(target.value)
            key = self.expr(target.slice)
            self.emit(f"{self.q(f'{RT}::ops::setitem(&{obj}, &{key}, {val})')};")
        else:
            raise self.err(f"unsupported assignment target `{ast.unparse(target)}`", target)

    def check_writable(self, node: ast.AST) -> None:
        ref = self.static_ref(node)
        if isinstance(ref, Ext) and ref.dotted in libmap.READ_ONLY_VALUES:
            raise self.err(f"writing into {ref.dotted} is not supported (the binary reads a snapshot)", node)

    def set_global(self, name: str, val: str) -> bool:
        """`x = val` where `x` is declared `global` here: read it once (initialisation), then rebind."""
        sym = self.__dict__.get("globals_decl", {}).get(name)
        if sym is None:
            return False
        g = self.p.global_value(sym)
        t = self.tmp()
        self.emit(f"let {t} = {val};")
        self.emit(f"{self.q(f'{g}(cx).await')};")
        self.emit(f"G_{g}.set({t});")
        return True

    def store_name(self, name: str, node) -> str:
        for scope in reversed(self.comp_scopes):
            if name in scope:
                return f"*{RT}::ops::cell_mut(&{scope[name]})" if scope[name] in self.comp_cells else scope[name]
        if name in self.cells:
            return f"*{RT}::ops::cell_mut(&v_{ident(name)})"
        if name in self.nonlocals:
            return f"*{RT}::ops::cell_mut(&{self.captures[name]})"
        if name in self.locals or name in self.params:
            return f"v_{ident(name)}"
        raise self.err(f"cannot assign to `{name}` here", node)

    BINOPS = {ast.Add: "add", ast.Sub: "sub", ast.Mult: "mul", ast.Div: "truediv", ast.FloorDiv: "floordiv",
              ast.Mod: "modulo", ast.BitOr: "bitor", ast.BitAnd: "bitand", ast.Pow: "pow", ast.BitXor: "bitxor",
              ast.LShift: "lshift", ast.RShift: "rshift"}

    def aug_assign(self, node: ast.AugAssign) -> None:
        op = self.BINOPS.get(type(node.op))
        if op is None:
            raise self.err("unsupported augmented assignment", node)
        rhs = self.expr(node.value)
        if isinstance(node.target, ast.Name):
            cur = self.expr(node.target)
            if node.target.id in self.__dict__.get("globals_decl", {}):
                self.set_global(node.target.id, self.q(self.aug_call(op, cur, rhs)))
                return
            self.emit(f"{self.store_name(node.target.id, node)} = {self.q(self.aug_call(op, cur, rhs))};")
        elif isinstance(node.target, (ast.Attribute, ast.Subscript)):
            base = self.static_ref(node.target.value) if isinstance(node.target, ast.Attribute) else None
            if isinstance(base, ModRef) and Sym(base.module, node.target.attr) in self.p.rebound_globals():
                # `conf.calls += 1`: rebinds that module's variable (as `global calls; calls += 1` would)
                cur = self.q(f"{self.p.global_value(Sym(base.module, node.target.attr))}(cx).await")
                self.assign(node.target, self.q(self.aug_call(op, cur, rhs, inplace_list=False)))
                return
            if isinstance(node.target, ast.Subscript):
                self.check_writable(node.target.value)
            t = self.tmp()
            self.emit(f"let {t} = {self.expr(node.target.value)};")
            if isinstance(node.target, ast.Attribute):
                cur = self.q(f"{RT}::methods::getattr(cx, &{t}, {rs(node.target.attr)}).await")
                new = self.q(self.aug_call(op, cur, rhs, inplace_list=False))
                self.emit(f"{self.q(f'{RT}::methods::setattr_cx(cx, &{t}, {rs(node.target.attr)}, {new}).await')};")
            else:
                k = self.tmp()
                self.emit(f"let {k} = {self.expr(node.target.slice)};")
                cur = self.q(f"{RT}::ops::getitem(&{t}, &{k})")
                new = self.q(self.aug_call(op, cur, rhs, inplace_list=False))
                self.emit(f"{self.q(f'{RT}::ops::setitem(&{t}, &{k}, {new})')};")
        else:
            raise self.err("unsupported augmented assignment target", node)

    AUG_SYMBOLS = {"add": "+", "sub": "-", "mul": "*", "truediv": "/", "floordiv": "//", "modulo": "%", "bitor": "|",
                   "bitand": "&", "pow": "**", "bitxor": "^", "lshift": "<<", "rshift": ">>"}

    def aug_call(self, op: str, cur: str, rhs: str, inplace_list: bool = True) -> str:
        """`a OP= b`: an instance's in-place method first (`__iadd__`), else `a OP b` (`+=` extends a list)."""
        if op == "add" and inplace_list:
            return f"{RT}::iadd(&{cur}, &{rhs})"
        return f"{RT}::proto::aug(&{cur}, {rs(self.AUG_SYMBOLS[op])}, &{rhs}, {RT}::ops::{op})"

    def raise_stmt(self, node: ast.Raise) -> None:
        if node.exc is None:
            if not self.cur_exc:
                raise self.err("bare `raise` outside an except block", node)
            self.emit(self.raise_code(f"{self.cur_exc[-1]}.clone()"))
            return
        v = self.expr(node.exc)
        # `raise X from Y`: Y the cause (None: no context shown)
        cause = f"Some(&{self.expr(node.cause)})" if node.cause is not None else "None"
        if node.cause is None and self.p.ix.is_shim_module(self.module):
            self.emit(self.raise_code(f"{RT}::raise_v(&{v})"))  # a library's frame: not in the traceback
            return
        i = self.p.tb_index(self.module, self.py_func_name(), node)
        self.emit(self.raise_code(f"{RT}::trace::raised(cx, {RT}::raise_v(&{v}), &self::TB[{i}], {cause})"))

    def for_stmt(self, node: ast.For) -> None:
        if isinstance(node.iter, ast.Name) and node.iter.id in self.request_names():
            raise self.err("attribute .__iter__ of a Request (a Mapping over its scope) is not supported", node.iter)
        it = self.expr(node.iter)
        lbl = f"'l{self.p.uid()}"
        var = self.tmp("it")
        flag = self.loop_else_start(node, lbl)
        li = self.tmp("li")
        # an instance's `__next__` runs between the bodies, as in CPython (dynrt/proto.rs); builtins: their elements
        self.emit(f"let mut {li} = {self.q(f'{RT}::proto::LoopIter::start(&{it}).await')};")
        self.emit(f"{lbl}: while let Some({var}) = {self.q(f'{li}.next()')} {{")
        self.ind += 1
        self.assign(node.target, var)
        self.loops.append(lbl)
        self.loop_barrier.append(len(self.ret_capture))
        self.block(node.body)
        self.loops.pop()
        self.loop_barrier.pop()
        self.ind -= 1
        self.emit("}")
        self.loop_else_end(node, flag)

    def async_for_stmt(self, node: ast.AsyncFor) -> None:
        """`async for x in it`: `__aiter__` once, then `__anext__` until StopAsyncIteration."""
        it = self.expr(node.iter)
        lbl = f"'l{self.p.uid()}"
        ai = self.tmp("ai")
        var = self.tmp("it")
        self.emit(f"let {ai} = {self.q(f'{RT}::agen::aiter(cx, &{it}).await')};")
        flag = self.loop_else_start(node, lbl)
        self.emit(f"{lbl}: loop {{")
        self.ind += 1
        self.emit(f"let {var} = match {self.q(f'{RT}::agen::anext_opt(cx, &{ai}).await')} {{ Some(v) => v, None => break {lbl}, }};")
        self.assign(node.target, var)
        self.loops.append(lbl)
        self.loop_barrier.append(len(self.ret_capture))
        self.block(node.body)
        self.loops.pop()
        self.loop_barrier.pop()
        self.ind -= 1
        self.emit("}")
        self.loop_else_end(node, flag)

    def loop_else_start(self, node, lbl: str) -> str | None:
        """for/while ... else: the else block runs when the loop ends without `break`."""
        self.__dict__.setdefault("loop_flags", {})
        if not node.orelse:
            self.loop_flags[lbl] = None
            return None
        flag = self.tmp("noBreak")
        self.emit(f"let mut {flag} = true;")
        self.loop_flags[lbl] = flag
        return flag

    def loop_else_end(self, node, flag: str | None) -> None:
        if flag is not None:
            self.emit(f"if {flag} {{")
            self.ind += 1
            self.block(node.orelse)
            self.ind -= 1
            self.emit("}")

    def exc_match(self, t: ast.AST | None, e: str) -> str:
        if t is None:
            return "true"
        if isinstance(t, (ast.Name, ast.Attribute)) and not (isinstance(t, ast.Name) and self.is_local(t.id)):
            # `except NETWORK_ERRORS:` with `NETWORK_ERRORS = (TimeoutError, aiohttp.ClientError, ...)` a module constant
            # bound once (a tuple: immutable): its classes, each resolved in the constant's module
            ref = self.p.resolve(self.module, t, self.scope_root())
            m = self.p.ix.module(ref.module) if isinstance(ref, Sym) else None
            d = m.defs.get(ref.name) if m is not None else None
            if (isinstance(d, (ast.Assign, ast.AnnAssign)) and isinstance(d.value, ast.Tuple)
                    and len(m.binders.get(ref.name, [d])) <= 1
                    and not any(isinstance(x, ast.Starred) for x in d.value.elts)):
                saved = self.module
                self.module = ref.module
                try:
                    return "(" + (" || ".join(self.exc_match(x, e) for x in d.value.elts) or "false") + ")"
                finally:
                    self.module = saved
        if isinstance(t, ast.Tuple):
            # the whole tuple is evaluated before any test: an element that raises does so first
            for x in t.elts:
                if self.stripe_error_missing(x):
                    return self.exc_match(x, e)
            return " || ".join(self.exc_match(x, e) for x in t.elts)
        if self.stripe_error_missing(t):
            return self.no_stripe_error(t, f"Some(&{e})")
        return f"{e}.isinstance(&{self.exc_class(t)})"

    def stripe_error_missing(self, t: ast.AST) -> bool:
        ref = self.p.resolve(self.module, t, self.scope_root()) if isinstance(t, ast.Attribute) else None
        return isinstance(ref, Ext) and libmap.stripe_error_missing(ref.dotted)

    def no_stripe_error(self, t: ast.Attribute, tested: str) -> str:
        """`stripe.error.X` under stripe 14.0.0: the AttributeError raised at `stripe.error`, evaluated only when
        reached (an except clause: when an exception is tested against it)."""
        self.pos_nodes.append(t.value)
        try:
            return self.q(f"{RT}::stripe::no_error_module(cx, {tested})")
        finally:
            self.pos_nodes.pop()

    def exc_class(self, t: ast.AST) -> str:
        if isinstance(t, ast.Name) and t.id in libmap.BUILTIN_EXC_NAMES and not self.is_local(t.id):
            ref = self.p.resolve(self.module, t, self.scope_root())
            if ref is None or isinstance(ref, Ext):
                return f"{RT}::v::{libmap.BUILTIN_EXC_NAMES[t.id]}"
        ref = self.p.resolve(self.module, t, self.scope_root())
        if isinstance(ref, Sym):
            return self.p.class_static(ref)
        if isinstance(ref, Ext) and libmap.too_new(ref.dotted):
            raise self.err(libmap.too_new(ref.dotted), t)
        if isinstance(ref, Ext) and ref.dotted in libmap.EXCEPTIONS:
            return f"{RT}::v::{libmap.EXCEPTIONS[ref.dotted]}"
        raise self.err(f"unsupported exception class `{ast.unparse(t)}`", t)

    # methods of SQLAlchemy sessions, connections and results whose value is never a context manager (a coroutine,
    # a result, None...): `async with session.execute(...)` is a TypeError in Python, refused at generation. Names
    # other libraries the runtime maps use for context managers stay out (`http.get(...)` of aiohttp/httpx, `close`)
    NON_CM_METHODS = {"execute", "scalars", "scalar", "get_one", "merge", "flush", "commit", "rollback", "refresh",
                      "add", "add_all", "expunge", "expunge_all", "expire", "expire_all", "run_sync", "invalidate",
                      "scalar_one", "scalar_one_or_none", "one_or_none", "fetchall", "fetchone", "fetchmany",
                      "mappings", "partitions"}

    def check_cm(self, cm: ast.AST, asynchronous: bool) -> None:
        """Refuse at generation, with its file:line, a `with`/`async with` whose manager can only be a value
        without the protocol (CPython's TypeError, a 500 at run time otherwise)."""
        if not (isinstance(cm, ast.Call) and isinstance(cm.func, ast.Attribute) and cm.func.attr in self.NON_CM_METHODS):
            return
        if isinstance(self.static_ref(cm.func.value), (Sym, ModRef, Ext)) or self.project_defines_method(cm.func.attr):
            return  # a module's function, a library call or a project class's method: not a session's
        kind = "async with" if asynchronous else "with"
        raise self.err(f"`{kind} {ast.unparse(cm)}`: the value of .{cm.func.attr}() of a session, connection or result "
                       f"is not {'an asynchronous' if asynchronous else 'a'} context manager (a TypeError in Python); "
                       "`async with session.begin()` / `session.begin_nested()` open a transaction block", cm)

    def project_defines_method(self, name: str) -> bool:
        return any(isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)) and st.name == name
                   for m in list(self.p.ix.modules.values()) for c in m.tree.body if isinstance(c, ast.ClassDef)
                   for st in c.body)

    def with_stmt(self, node: ast.With) -> None:
        """`with cm as x: body` -> x = enter(cm); try: body finally: exit(cm) (runtime values that are their
        own context managers: open() files, BytesIO)."""
        item = node.items[0]
        self.check_cm(item.context_expr, False)
        body = node.body if len(node.items) == 1 else [ast.copy_location(ast.With(items=node.items[1:], body=node.body), node)]
        m, flag = self.tmp("w"), self.tmp("wx")
        self.emit(f"let {m} = {self.expr(item.context_expr)};")
        entered = self.q(f"{RT}::thread::enter(cx, &{m}).await")
        if item.optional_vars is not None:
            self.assign(item.optional_vars, entered)
        else:
            self.emit(f"let _ = {entered};")
        self.emit(f"let mut {flag} = false;")
        mk = lambda name: ast.copy_location(ast.Expr(ast.Call(ast.Name(name, ast.Load()), [ast.Constant(m), ast.Constant(flag)], [])), node)  # noqa: E731
        handler = ast.ExceptHandler(type=ast.Name("BaseException", ast.Load()), name=None, body=[mk("\0ctx_exc")])
        ast.copy_location(handler, node)
        self.try_stmt(ast.copy_location(ast.Try(body=body, handlers=[handler], orelse=[], finalbody=[mk("\0ctx_exit")]), node))

    def is_engine(self, node: ast.AST) -> bool:
        """A module global created by `create_async_engine(...)`."""
        ref = self.static_ref(node)
        d = self.p.ix.definition(ref) if isinstance(ref, Sym) else None
        if isinstance(d, (ast.Assign, ast.AnnAssign)) and isinstance(d.value, ast.Call):
            t = self.p.resolve(ref.module, d.value.func)
            return isinstance(t, Ext) and t.dotted == "sqlalchemy.ext.asyncio.create_async_engine"
        return False

    def match_stmt(self, node: ast.Match) -> None:
        """`match subject:` -> the cases tried in order in a labeled block (value, singleton, wildcard,
        capture, `|` and `as` patterns, guards)."""
        subj = self.tmp("m")
        self.emit(f"let {subj} = {self.expr(node.subject)};")
        lbl = f"'match{self.p.uid()}"
        self.emit(f"{lbl}: {{")
        self.ind += 1
        for case in node.cases:
            test = self.pattern_test(case.pattern, subj)
            self.emit(f"if {test} {{")
            self.ind += 1
            self.pattern_bind(case.pattern, subj)
            if case.guard is not None:
                self.emit(f"if {self.truthy(case.guard)} {{")
                self.ind += 1
            self.block(case.body)
            self.emit(f"break {lbl};")
            if case.guard is not None:
                self.ind -= 1
                self.emit("}")
            self.ind -= 1
            self.emit("}")
        self.ind -= 1
        self.emit("}")

    def pattern_test(self, pat: ast.pattern, subj: str) -> str:
        if isinstance(pat, ast.MatchValue):
            return self.q(f"{RT}::ops::eq(&{subj}, &{self.expr(pat.value)}).and_then(|v| {RT}::ops::truthy(&v))")
        if isinstance(pat, ast.MatchSingleton):
            v = pat.value
            return f"matches!(&{subj}, V::None)" if v is None else f"matches!(&{subj}, V::Bool({str(v).lower()}))"
        if isinstance(pat, ast.MatchAs):
            return "true" if pat.pattern is None else self.pattern_test(pat.pattern, subj)
        if isinstance(pat, ast.MatchOr):
            if any(isinstance(n, ast.MatchAs) and n.name for p in pat.patterns for n in ast.walk(p)):
                raise self.err("capture names inside `|` patterns are not supported", pat)
            parts = []
            for p in pat.patterns:
                parts.append(f"({self.pattern_test(p, subj)})")
            return " || ".join(parts)
        if isinstance(pat, ast.MatchClass) and not pat.patterns and not pat.kwd_patterns:
            return self.isinstance_test(pat.cls, subj)
        raise self.err(f"`case {ast.unparse(pat)}`: this pattern kind is not supported (values, None/True/False, "
                       "`_`, names, `Class()`, `|` and `as` only)", pat)

    def pattern_bind(self, pat: ast.pattern, subj: str) -> None:
        if isinstance(pat, ast.MatchAs):
            if pat.pattern is not None:
                self.pattern_bind(pat.pattern, subj)
            if pat.name:
                self.assign(ast.copy_location(ast.Name(pat.name, ast.Store()), pat), f"{subj}.clone()")

    ASYNC_CMS = {"httpx.AsyncClient", "aiohttp.ClientSession"}
    REQUEST_METHODS = {"get", "post", "put", "patch", "delete", "head", "options", "request"}

    def async_with_stmt(self, node: ast.AsyncWith) -> None:
        """`async with cm as x`, CPython's protocol: `__aenter__`, then `__aexit__(type, exc, tb)` when the body
        raises (a true result suppresses the exception), `__aexit__(None, None, None)` otherwise. The runtime
        knows its own managers (HTTP clients and responses, sessions, connections, Semaphore/Lock) and calls
        a project class's `__aenter__`/`__aexit__`; anything else raises CPython's TypeError."""
        item = node.items[0]
        cm = item.context_expr
        self.check_cm(cm, True)
        body = node.body if len(node.items) == 1 else [ast.copy_location(ast.AsyncWith(items=node.items[1:], body=node.body), node)]
        m, flag = self.tmp("w"), self.tmp("wx")
        self.emit(f"let {m} = {self.expr(cm)};")
        entered = self.q(f"{RT}::aio::aenter(cx, &{m}).await")
        if item.optional_vars is not None:
            self.assign(item.optional_vars, entered)
        else:
            self.emit(f"let _ = {entered};")
        self.emit(f"let mut {flag} = false;")
        mk = lambda name: ast.copy_location(ast.Expr(ast.Call(ast.Name(name, ast.Load()), [ast.Constant(m), ast.Constant(flag)], [])), node)  # noqa: E731
        handler = ast.ExceptHandler(type=ast.Name("BaseException", ast.Load()), name=None, body=[mk("\0actx_exc")])
        ast.copy_location(handler, node)
        self.try_stmt(ast.copy_location(ast.Try(body=body, handlers=[handler], orelse=[], finalbody=[mk("\0actx_exit")]), node))

    def guard_never_passes(self, node: ast.Try) -> bool:
        """A module-level `try` whose body imports a module that is not installed (`try: import aiomcache`): the
        import raises, so its `else` branch never runs (and its names, imported from there, resolve nowhere)."""
        if self.node is not None or not node.orelse:
            return False
        from .modules import _import_specs
        imps = [st for st in node.body if isinstance(st, (ast.Import, ast.ImportFrom))]
        specs, _ = _import_specs(imps, self.p.ix.module(self.module).package)
        return any(not self.p.ix.importable(spec[1]) for spec in specs.values())

    def try_stmt(self, node: ast.Try) -> None:
        n = self.p.uid()
        slot, lbl = f"__exc{n}", f"'try{n}"
        fin = bool(node.finalbody)
        self.emit(f"let mut {slot}: Option<Exc> = None;")
        ret = f"__ret{n}"
        jmp, jumps = f"__jmp{n}", []
        if fin:
            self.emit(f"let mut {ret}: Option<V> = None;")
            jmp_at = len(self.lines)
            self.ret_capture.append((lbl, ret))
            self.jumps.append((jmp, jumps))
        self.emit(f"{lbl}: {{")
        self.ind += 1
        self.sinks.append((lbl, slot))
        self.block(node.body)
        self.sinks.pop()
        self.ind -= 1
        self.emit("}")
        if fin:
            self.ret_capture.pop()
            self.jumps.pop()
        hl = f"'hnd{n}"
        if fin:
            # the handlers and the else run unless the body left by return/break/continue
            body_jumps = bool(jumps)
            self.emit(f"if {ret}.is_none(){f' && {jmp}.is_none()' if body_jumps else ''} {{")
            self.ind += 1
            self.emit(f"{hl}: {{")
            self.ind += 1
            self.ret_capture.append((hl, ret))
            self.jumps.append((jmp, jumps))
            self.sinks.append((hl, slot))
        e = f"__e{n}"
        self.emit(f"match {slot}.take() {{")
        self.ind += 1
        self.emit("None => {")
        self.ind += 1
        if not self.guard_never_passes(node):
            self.block(node.orelse)
        self.ind -= 1
        self.emit("}")
        self.emit(f"Some({e}) => {{")
        self.ind += 1
        if self.__dict__.get("traced_fn"):
            # the exception entered this frame: its `exception` event, before the handlers
            ev = self.q(f"{RT}::trace::caught(cx, __tfr, &{e}, __lnr.load(std::sync::atomic::Ordering::Relaxed)).await.map(|_| V::None)")
            self.emit(f"let _ = {ev};")
        if fin:
            self.sinks.pop()  # handlers: their own raises go to 'hnd
            self.sinks.append((hl, slot))
        first = True
        for h in node.handlers:
            cond = self.exc_match(h.type, e)
            self.emit(f"{'if' if first else '} else if'} {cond} {{")
            first = False
            self.ind += 1
            if h.name:
                self.emit(f"{self.store_name(h.name, h)} = V::Exc({e}.clone());")
            if self.p.tracks_handling():
                self.emit(f"let __hg{n} = {RT}::Handling::new(cx, &{e});")
            self.cur_exc.append(e)
            self.block(h.body)
            self.cur_exc.pop()
            self.ind -= 1
        if node.handlers:
            self.emit("} else {")
            self.ind += 1
            self.emit(self.raise_code(f"{e}.clone()"))
            self.ind -= 1
            self.emit("}")
        else:
            self.emit(self.raise_code(f"{e}.clone()"))
        self.ind -= 1
        self.emit("}")
        self.ind -= 1
        self.emit("}")
        if fin:
            self.sinks.pop()
            self.ret_capture.pop()
            self.jumps.pop()
            self.ind -= 1
            self.emit("}")
            self.ind -= 1
            self.emit("}")
            self.block(node.finalbody)
            self.emit(f"if let Some(__e) = {slot}.take() {{ {self.raise_code('__e')} }}")
            if self.ret_capture:
                label, rslot = self.ret_capture[-1]
                self.emit(f"if let Some(__r) = {ret}.take() {{ {rslot} = Some(__r); break {label}; }}")
            else:
                self.emit(f"if let Some(__r) = {ret}.take() {{ return Ok(__r); }}")
            for code, (kind, loop, barrier) in enumerate(jumps):
                go = f"{kind} {loop};" if len(self.ret_capture) == barrier else self.jump_code(kind, loop, barrier)
                self.emit(f"if {jmp} == Some({code}) {{ {go} }}")
            if jumps:
                self.lines.insert(jmp_at, self.lines[jmp_at - 1].replace(f"let mut {ret}: Option<V> = None;", f"let mut {jmp}: Option<u32> = None;"))

    def jump_code(self, kind: str, loop: str, barrier: int) -> str:
        """`break`/`continue` of `loop` from inside a try/finally: recorded in the innermost try's jump slot,
        then out of its body (or handlers) so that its finally runs first."""
        label, _ = self.ret_capture[-1]
        jmp, jumps = self.jumps[-1]
        if (kind, loop, barrier) not in jumps:
            jumps.append((kind, loop, barrier))
        return f"{{ {jmp} = Some({jumps.index((kind, loop, barrier))}); break {label}; }}"

    def nested_def(self, node) -> None:
        """A nested `def`: a function object closing over the enclosing locals (by value), bound like
        CPython at each call; decorators evaluated, then defaults, then applied bottom-up."""
        free = self.free_vars(node)
        if isinstance(node, ast.FunctionDef) and has_yield(node):
            self.__dict__.setdefault("sync_gens", {})[node.name] = node
        name = f"{self.name}__{ident(node.name)}_{self.p.uid()}"
        caps = {v: f"c_{ident(v)}" for v in free}
        sub = FnCompiler(self.p, self.module, node, name, captures=caps, parent=self)
        code = sub.compile_function()  # carries its own nested functions
        if self.node is None:
            # a block compiled outside a function (a module-level `if`/`try` run at startup, a global's value): no
            # compile_function will emit it
            self.p.items.append(code)
        else:
            self.extra_fns.append(code)
        decos = [self.expr(d) for d in node.decorator_list]
        params = fn_params(node)
        dflts: dict[int, str] = {}
        for i, q in enumerate(params):
            if q.default is not None:
                t = self.tmp("d")
                self.emit(f"let {t} = {self.expr(q.default)};")
                dflts[i] = t
        kinds = {"positional": f"{RT}::P_POS", "vararg": f"{RT}::P_VARARG", "kwonly": f"{RT}::P_KWONLY", "kwarg": f"{RT}::P_KWARG"}
        spec = ", ".join(f"({rs(q.name)}, {kinds[q.kind]}, {str(q.default is not None).lower()})" for q in params)
        vals = [f"__s[{i}].take().unwrap_or_else(|| {dflts[i]}.clone())" if i in dflts else f"__s[{i}].take().unwrap_or(V::None)"
                for i in range(len(params))]
        outer = self.qualname()
        qual = f"{outer}.<locals>.{node.name}" if outer else node.name
        # a function naming itself (`return handler`, recursion): CPython reads the enclosing cell at call
        # time; here a cell filled once the name is bound
        # (a cell variable needs none: the closure holds the cell the binding below fills)
        selfref = node.name in caps and node.name not in self.params and not self.is_cell(node.name)
        # bound once, by this undecorated `def`: the name always holds this function, read through a weak
        # reference (`SelfRef`: a strong one is a cycle `Arc` never frees); a cell variable gets a cell of it
        weak = (node.name in caps and node.name not in self.params and not node.decorator_list
                and self.bound_once(node))
        selfref = selfref or weak
        cell = self.tmp("cell") if selfref else None
        if weak:
            self.emit(f"let {cell} = std::sync::Arc::new({RT}::SelfRef::default());")
        elif selfref:
            self.emit(f"let {cell} = std::sync::Arc::new(std::sync::OnceLock::<V>::new());")
        held = [c for v, c in caps.items() if not (selfref and v == node.name)] + list(dflts.values())
        hold = "".join(f"let {c} = {c}.clone(); " for c in held)
        if weak and self.is_cell(node.name):
            hold += f"let {caps[node.name]} = {RT}::ops::cell_of({cell}_c.get()); "
        elif weak:
            hold += f"let {caps[node.name]} = {cell}_c.get(); "
        elif selfref:
            hold += f"let {caps[node.name]} = {cell}_c.get().cloned().unwrap_or(V::Unbound); "
        bind = (f"let mut __s = {RT}::bind_params({rs(qual + '()')}, 0, args, kwargs, &[{spec}])?; let _ = &mut __s; "
                + "".join(f"let __a{i} = {v}; " for i, v in enumerate(vals)))
        argl = "".join(f", __a{i}" for i in range(len(vals)))
        capl = "".join(f", {c}" for c in caps.values())
        if has_yield(node):
            body = (f"{hold}let cx2 = cx.clone(); Box::pin(async move {{ {bind}"
                    f"Ok({RT}::web::spawn_gen(move |y| Box::pin(async move {{ {name}(&cx2{argl}{capl}, &y).await }}))) }})")
        else:
            body = f"{hold}Box::pin(async move {{ {bind}{name}(cx{argl}{capl}).await }})"
        cap_clone = " ".join(f"let {cell}_c = {cell}.clone();" if selfref and v == node.name else f"let {caps[v]} = {self.capture(v)};"
                             for v in free)
        doc = fn_doc(self.p, node)
        is_async = isinstance(node, ast.AsyncFunctionDef) and not has_yield(node)
        fv = (f"{RT}::pyfn({rs(self.module)}, {rs(qual)}, {'Some(' + rs(doc) + ')' if doc is not None else 'None'}, "
              f"{str(is_async).lower()}, std::sync::Arc::new("
              + (f"{{ {cap_clone} " if cap_clone else "")
              + f"move |cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>| -> {RT}::BoxFut<'_> {{ {body} }}"
              + (" }" if cap_clone else "") + "))")
        for d in reversed(decos):
            fv = self.q(f"{RT}::methods::call_value(cx, &{d}, vec![{fv}], vec![]).await")
        if weak:
            t = self.tmp("f")
            self.emit(f"let {t} = {fv};")
            self.emit(f"{cell}.set(&{t});")
            fv = t
        self.emit(f"{self.store_name(node.name, node)} = {fv};")
        if selfref and not weak:
            self.emit(f"let _ = {cell}.set({self.load_local(node.name)});")

    def bound_once(self, d) -> bool:
        """Is the nested `def` `d` the only binding of its name in this function, run at most once per call
        (not in a loop), never rebound by a nested function (`nonlocal`) nor deleted?"""
        if self.node is None or self.comp_scopes:
            return False
        name, count = d.name, 0
        for n in walk_scope(self.node):
            found: set[str] = set()
            if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                found.add(n.name)
            elif isinstance(n, ast.Assign):
                for t in n.targets:
                    target_names(t, found)
            elif isinstance(n, (ast.AugAssign, ast.AnnAssign, ast.NamedExpr, ast.For, ast.AsyncFor)):
                target_names(n.target, found)
            elif isinstance(n, ast.withitem) and n.optional_vars is not None:
                target_names(n.optional_vars, found)
            elif isinstance(n, ast.Delete):
                for t in n.targets:
                    target_names(t, found)
            elif isinstance(n, (ast.ExceptHandler, ast.MatchAs, ast.MatchStar)) and n.name:
                found.add(n.name)
            elif isinstance(n, (ast.Import, ast.ImportFrom)):
                found |= {(a.asname or a.name).split(".")[0] for a in n.names}
            elif isinstance(n, (ast.Global, ast.Nonlocal)) and name in n.names:
                return False
            count += name in found
        if count != 1 or any(isinstance(n, ast.Nonlocal) and name in n.names for n in ast.walk(self.node)):
            return False

        def in_loop(body, loop: bool) -> bool | None:
            for st in body:
                if st is d:
                    return loop
                if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                    continue
                inner = isinstance(st, (ast.For, ast.AsyncFor, ast.While))
                for attr in ("body", "orelse", "finalbody", "handlers", "cases"):
                    sub = getattr(st, attr, None)
                    if isinstance(sub, list) and sub:
                        if attr in ("handlers", "cases"):
                            for h in sub:
                                r = in_loop(h.body, loop or inner)
                                if r is not None:
                                    return r
                        else:
                            r = in_loop(sub, loop or (inner and attr == "body"))
                            if r is not None:
                                return r
            return None

        return in_loop(self.node.body, False) is False

    def scope_root(self):
        """The outermost function enclosing this code: its local imports are visible to nested functions
        and lambdas (closures)."""
        c, root = self, self.node
        while c.parent is not None:
            c = c.parent
            if c.node is not None:
                root = c.node
        return root

    def py_func_name(self) -> str:
        """`co_name` of the code being compiled: the function's name, `<lambda>`, `<module>` at module level."""
        fc = self
        while fc.node is None and fc.parent is not None:
            fc = fc.parent
        if fc.node is None:
            return "<module>"
        return "<lambda>" if isinstance(fc.node, ast.Lambda) else getattr(fc.node, "py2axum_name", fc.node.name)

    def log_loc(self, node) -> str:
        """A logging call site (`dynrt::logging::Loc`): file relative to --root, line, function name."""
        for k in node.keywords:
            if k.arg == "stacklevel" and not (isinstance(k.value, ast.Constant) and k.value.value == 1):
                raise self.err(f"logging `stacklevel={ast.unparse(k.value)}` is not supported: the binary records "
                               "the call site itself, not its callers", k.value)
        from . import tbfmt
        text = tbfmt.frame_text(self.p.src(self.module), node.lineno, node.end_lineno, node.col_offset, node.end_col_offset)
        return (f"&{RT}::logging::Loc {{ file: {rs(self.p.rel_file(self.module))}, line: {node.lineno}, "
                f"func: {rs(self.py_func_name())}, text: {rs(text)} }}")

    def qualname(self) -> str | None:
        """`__qualname__` of the function being compiled (prefix of its nested functions'), None at
        module level."""
        if self.node is None:
            return None
        own = "<lambda>" if isinstance(self.node, ast.Lambda) else getattr(self.node, "py2axum_name", self.node.name)
        if getattr(self.node, "py2axum_qualname", None):
            return self.node.py2axum_qualname
        if self.parent is not None:
            outer = self.parent.qualname()
            return f"{outer}.<locals>.{own}" if outer else own
        if self.p.cur is not None:
            return self.p.cur[0].name
        return own

    def free_vars(self, node) -> list[str]:
        own = {p.name for p in fn_params(node)}
        if not isinstance(node, ast.Lambda):
            own |= assigned_names(node.body) - {name for n in walk_scope(node) if isinstance(n, ast.Nonlocal) for name in n.names}
        out = []
        for n in ast.walk(node):
            names = n.names if isinstance(n, ast.Nonlocal) else [n.id] if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load) else []
            for name in names:
                if name not in own and name not in out and self.is_local(name):
                    out.append(name)
        return out

    def is_cell(self, name: str) -> bool:
        """Is `name` held in a cell here (captured as the cell itself, not its value)?"""
        for scope in reversed(self.comp_scopes):
            if name in scope:
                return scope[name] in self.comp_cells
        return name in self.cells or (name in self.cell_caps and name not in self.locals)

    def capture(self, name: str) -> str:
        """The value a nested function or lambda closes over: the cell itself for a cell variable."""
        if self.is_cell(name):
            for scope in reversed(self.comp_scopes):
                if name in scope:
                    return f"{scope[name]}.clone()"
            return f"v_{ident(name)}.clone()" if name in self.cells else f"{self.captures[name]}.clone()"
        return self.load_local(name)

    def is_local(self, name: str) -> bool:
        return (any(name in s for s in self.comp_scopes) or name in self.locals or name in self.params
                or name in self.captures)

    def load_local(self, name: str) -> str:
        for scope in reversed(self.comp_scopes):
            if name in scope:
                if scope[name] in self.comp_cells:
                    return self.q(f"{RT}::ops::cell_get(&{scope[name]}, {rs(name)}, false)")
                return f"{scope[name]}.clone()"
        if name in self.cells:
            return self.q(f"{RT}::ops::cell_get(&v_{ident(name)}, {rs(name)}, false)")
        if name in self.params:
            return f"v_{ident(name)}.clone()"
        if name in self.cells:
            return self.q(f"{RT}::ops::cell_get(&v_{ident(name)}, {rs(name)}, false)")
        if name in self.locals:
            return self.q(f"{RT}::ops::bound(&v_{ident(name)}, {rs(name)})")
        if name in self.cell_caps:
            return self.q(f"{RT}::ops::cell_get(&{self.captures[name]}, {rs(name)}, true)")
        if name in self.captures:
            return f"{self.captures[name]}.clone()"
        raise KeyError(name)

    # ---------------------------------------------------------------- expressions

    def truthy(self, node: ast.AST) -> str:
        return self.q(f"{RT}::ops::truthy(&{self.expr(node)})")

    def expr(self, node: ast.AST) -> str:
        if isinstance(node, ast.Attribute) or (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)):
            self.p.note_copy(attr=(node if isinstance(node, ast.Attribute) else node.func).attr)
        m = getattr(self, "e_" + type(node).__name__, None)
        if m is None:
            raise self.err(f"unsupported expression `{ast.unparse(node)}`", node)
        if getattr(node, "end_col_offset", None) is None:
            return m(node)
        self.pos_nodes.append(node)
        try:
            return m(node)
        except LoneSurrogate as e:
            if e.node is None:
                raise self.err(e.msg, node) from None
            raise
        finally:
            self.pos_nodes.pop()

    def e_Constant(self, node: ast.Constant) -> str:
        v = node.value
        if v is None:
            return "V::None"
        if v is True or v is False:
            return f"V::Bool({str(v).lower()})"
        if isinstance(v, int):
            if not -(2**63) <= v < 2**63:
                raise self.err("integer literal out of 64-bit range", node)
            return f"V::Int({v}i64)"
        if isinstance(v, float):
            return f"V::Float({v!r}f64)" if v == v and v not in (float("inf"), float("-inf")) else f"V::Float(f64::{'NAN' if v != v else ('INFINITY' if v > 0 else 'NEG_INFINITY')})"
        if isinstance(v, str):
            return f"V::str({rs(v)})"
        if isinstance(v, bytes):
            return f"V::Bytes(std::sync::Arc::from(&[{', '.join(f'{b}u8' for b in v)}][..]))"
        if v is Ellipsis:
            # a value (a "not given" sentinel default: `x: str | None = ...`, then `x is not ...`)
            return f"{RT}::ops::ellipsis()"
        raise self.err(f"unsupported constant {v!r}", node)

    def e_Name(self, node: ast.Name) -> str:
        if self.is_local(node.id):
            return self.load_local(node.id)
        if node.id == "__name__":
            return f"V::str({rs(self.module)})"
        ref = self.static_ref(node)
        return self.ref_value(ref, node)

    def static_ref(self, node: ast.AST):
        """Resolve a Name/Attribute chain without evaluating it: Sym, ModRef, Ext, ('builtin', n),
        ('classattr', Sym, attr) — or None when it starts from a runtime value."""
        if (isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "__import__"
                and len(node.args) == 1 and isinstance(node.args[0], ast.Constant) and not node.keywords):
            # __import__("datetime"): the module itself
            name = node.args[0].value
            return ModRef(name) if self.p.ix.module(name) else Ext(libmap.canonical(name))
        if (isinstance(node, ast.Call) and len(node.args) == 1 and isinstance(node.args[0], ast.Constant)
                and isinstance(node.args[0].value, str) and not node.keywords
                and self.static_ref(node.func) == Ext("importlib.import_module")):
            # importlib.import_module("pkg.mod"): the module itself (a literal project module only)
            name = node.args[0].value
            if not self.p.ix.module(name):
                if self.p.ix.importable(name):
                    # an installed library (or the standard library) the library map covers: the module, as `import` gives it
                    return Ext(libmap.canonical(name))
                raise self.err(f"importlib.import_module({name!r}): the module is neither a module of the project nor "
                               "installed in the project's environment (--venv)", node)
            return ModRef(name)
        if isinstance(node, ast.Name):
            if self.is_local(node.id):
                return None
            t = self.p.resolve(self.module, node, self.scope_root())
            if t is None:
                if node.id in BUILTIN_FUNCS or node.id in BUILTIN_TYPES or node.id in TYPE_ONLY or node.id in libmap.BUILTIN_EXC_NAMES:
                    return ("builtin", node.id)
                return None
            return t
        if isinstance(node, ast.Attribute):
            base = self.static_ref(node.value)
            if base is None:
                return None
            if isinstance(base, ModRef):
                t = self.p.ix.resolve(base.module, node.attr)
                if isinstance(t, Ext):
                    return Ext(libmap.canonical(t.dotted))
                return t if t is not None else ("missing", base.module, node.attr)
            if isinstance(base, Ext):
                name = libmap.canonical(f"{base.dotted}.{node.attr}")
                # `jinja2.Template`: read from the source of a library the map covers in part (--translate-libs)
                return self.p.ix._partial_source(name) or Ext(name)
            if isinstance(base, Sym):
                d = self.p.ix.definition(base)
                if isinstance(d, ast.ClassDef):
                    return ("classattr", base, node.attr)
                return None
            return None
        return None

    def factory_local(self, name: str):
        """A local of the application factory read by an endpoint defined in it: its single top-level
        assignment, evaluated once (the factory runs once, at startup)."""
        if name in self.__dict__.get("app_alias", ()):
            return f"{RT}::routing::app()"
        if name in self.__dict__.get("raw_names", {}):
            return self.__dict__["raw_names"][name]
        factory = self.__dict__.get("factory")
        if factory is None:
            return None
        assigns = [st for st in factory.body if isinstance(st, ast.Assign) and len(st.targets) == 1
                   and isinstance(st.targets[0], ast.Name) and st.targets[0].id == name]
        others = [n for n in ast.walk(factory) if isinstance(n, ast.Name) and n.id == name and isinstance(n.ctx, ast.Store)]
        a = factory.args
        positional = [*a.posonlyargs, *a.args]
        defaults = {p.arg: d for p, d in zip(positional[len(positional) - len(a.defaults):], a.defaults)}
        defaults |= {p.arg: d for p, d in zip(a.kwonlyargs, a.kw_defaults) if d is not None}
        if name in defaults and not others:
            value, line, scope = defaults[name], factory.lineno, None  # the server calls the factory without arguments
        elif len(assigns) == 1 and len(others) == 1:
            value, line, scope = assigns[0].value, assigns[0].lineno, factory
        else:
            return None
        g = f"fl_{mod_ident(self.module)}__{ident(factory.name)}__{ident(name)}"
        done = self.p.__dict__.setdefault("_factory_locals", set())
        if g not in done:
            done.add(g)
            self.p.__dict__.setdefault("eager", []).append((self.module, line, g))
            fc = FnCompiler(self.p, self.module, None, g)
            fc.factory = scope
            code = fc.expr(value)
            body = "\n".join("        " + l for l in fc.lines)
            self.p.items.append(
                f"static G_{g}: {RT}::Global = {RT}::Global::new();\n"
                f"pub async fn {g}(cx: &Cx) -> R {{\n"
                f"    G_{g}.get(cx, |cx| Box::pin(async move {{\n{body}\n        Ok({code})\n    }})).await\n}}"
            )
        return self.q(f"{g}(cx).await")

    def ref_value(self, ref, node) -> str:
        if ref is None and isinstance(node, ast.Name) and node.id == "__file__":
            # the module's path relative to --root: the binary runs from the project root, like the app
            path = Path(self.p.src(self.module)).resolve()
            try:
                rel = path.relative_to(self.p.ix.root)
            except ValueError:
                rel = path
            return f"V::str({rs(str(rel))})"
        if ref is None and isinstance(node, ast.Name) and node.id == "NotImplemented":
            return f"{RT}::proto::not_implemented()"
        if ref is None and isinstance(node, ast.Name) and node.id == "Ellipsis":
            return f"{RT}::ops::ellipsis()"
        if ref is None and isinstance(node, ast.Name):
            fl = self.factory_local(node.id)
            if fl is not None:
                return fl
        if ref is None:
            if isinstance(node, ast.Name) and node.id in self.REFUSED_BUILTINS:
                raise self.err(f"unknown name `{node.id}`: the builtin {node.id}() is not supported "
                               f"({self.REFUSED_BUILTINS[node.id]})", node)
            raise self.err(f"unknown name `{ast.unparse(node)}`", node)
        if isinstance(ref, tuple):
            kind = ref[0]
            if kind == "builtin":
                n = ref[1]
                if n in libmap.BUILTIN_EXC_NAMES:
                    return f"V::Class(&{RT}::v::{libmap.BUILTIN_EXC_NAMES[n]})"
                return f"V::native({RT}::Native::Type({rs(n)}))"
            if kind == "classattr":
                _, sym, attr = ref
                info = None
                if sym in self.p.fe.schema_syms:
                    info = self.p.schema(sym)
                elif sym not in self.p.fe.model_syms and sym not in self.p.fe.dataclass_syms and self.p.is_plain_class(sym):
                    info = self.p.plain_class(sym)
                if info is not None:
                    # a class attribute of a plain or Pydantic class read on the class: its shared value
                    cattrs = info.__dict__.get("class_attrs", {})
                    if attr in cattrs:
                        return self.q(f"{cattrs[attr]}(cx).await")
                self.check_async_class_method(sym, attr, node)
                return self.q(f"{RT}::methods::getattr(cx, &{self.class_value(sym, node)}, {rs(attr)}).await")
            if kind == "missing":
                raise self.err(f"module {ref[1]} has no attribute `{ref[2]}`", node)
        if isinstance(ref, Sym):
            if ref.module == self.module and ref.name in self.__dict__.get("app_alias", ()):
                return f"{RT}::routing::app()"  # the application handed to a configuration function
            d = self.p.ix.definition(ref)
            if isinstance(d, ast.ClassDef):
                return self.class_value(ref, node)
            if isinstance(d, (ast.FunctionDef, ast.AsyncFunctionDef)):
                return self.fn_value(ref, d, node)
            if isinstance(d, (ast.Assign, ast.AnnAssign, ast.Try, ast.If, ast.For, ast.While, ast.With)):
                if isinstance(node, ast.Name) and ref.module != self.module and ref in self.p.rebound_globals():
                    raise self.err(f"`from {ref.module} import {ref.name}` copies a variable that a function rebinds "
                                   "with `global`: not supported (read it as module attribute or via a function)", node)
                return self.q(f"{self.p.global_value(ref)}(cx).await")
            raise self.err(f"`{ref.qual}` cannot be used as a value", node)
        if isinstance(ref, ModRef):
            return f"V::native({RT}::Native::Module(&{self.p.module_desc(ref.module, node, self)}))"
        if isinstance(ref, Ext):
            self.p.note_copy(ref.dotted)
            self.note_native(ref.dotted, node)
            if ref.dotted == "sys.modules":
                self.check_sys_modules(node)
            if ref.dotted in STARLETTE_CONTEXT or ref.dotted.removesuffix(".data") in STARLETTE_CONTEXT:
                return self.starlette_context(ref.dotted, node)
            if ref.dotted in libmap.VALUES:
                return libmap.VALUES[ref.dotted]
            if ref.dotted == "datetime.datetime":
                # the class itself (callable like datetime(...)): `ret_type=datetime`, `type(x) is datetime`
                return f'V::native({RT}::Native::Type("datetime"))'
            code = libmap.status_constant(ref.dotted)
            if code is not None:
                return f"V::Int({code})"
            if ref.dotted in libmap.EXCEPTIONS:
                if libmap.stripe_error_missing(ref.dotted) and isinstance(node, ast.Attribute):
                    return self.no_stripe_error(node, "None")
                if libmap.too_new(ref.dotted):
                    raise self.err(libmap.too_new(ref.dotted), node)
                return f"V::Class(&{RT}::v::{libmap.EXCEPTIONS[ref.dotted]})"
            if ref.dotted.startswith("builtins.") and ref.dotted[9:] in BUILTIN_TYPES:
                return f"V::native({RT}::Native::Type({rs(ref.dotted[9:])}))"
            if ref.dotted in libmap.CALLS and ref.dotted not in self.p.LIB_FN_NOT_VALUES:
                return self.lib_fn_value(ref.dotted, node)
            obj = self.object_value(ref.dotted)
            if obj is not None:
                return obj
            raise self.lib_err(f"library value `{ref.dotted}` is not supported (not in the py2axum library map)", ref.dotted, node)
        raise self.err(f"unsupported name `{ast.unparse(node)}`", node)

    def class_value(self, sym: Sym, node) -> str:
        try:
            return f"V::Class(&{self.p.class_static(sym)})"
        except TranspileError as e:
            if e.file:
                raise  # keep the location of the offending declaration (model/schema file)
            raise self.err(e.msg, node) from None

    def deco_resolver(self, sym: Sym):
        """static_ref in the module defining `sym` (its decorators' names)."""
        fc = FnCompiler(self.p, sym.module, None, "deco")
        return fc.static_ref

    def fn_value(self, sym: Sym, d, node) -> str:
        try:
            vdecos = value_decorators(d, self.deco_resolver(sym))
        except TranspileError as e:
            raise self.err(e.msg, e.node or d) from None
        if "." not in sym.name and vdecos:
            return self.q(f"{self.p.decorated_value(sym)}(cx).await")
        params = fn_params(d)
        if not has_yield(d) or "." not in sym.name:
            # a function object bound at each call like CPython (keywords, defaults, *args/**kwargs, TypeErrors);
            # calling an async generator function gives the generator
            if "." in sym.name:
                return f"V::native({RT}::Native::Bound({function_wrapper(self.p, sym)}, V::None))"
            return raw_fn_value(self.p, sym)
        if any(p.kind in {"vararg", "kwarg", "kwonly"} for p in params):
            raise self.err(f"`{sym.name}` used as a value: only plain positional parameters are supported", node)
        rust = self.p.function(sym)
        args = ", ".join(f"args.get({i}).cloned().unwrap_or(V::None)" for i in range(len(params)))
        return f"{RT}::func(std::sync::Arc::new(|cx: &Cx, args: Vec<V>| -> {RT}::BoxFut<'_> {{ Box::pin(async move {{ {rust}(cx{', ' + args if args else ''}).await }}) }}))"

    def model_table(self, node: ast.AST, depth: int = 0) -> bool:
        """`Model.__table__`, or a local variable bound only to one."""
        if isinstance(node, ast.Attribute) and node.attr == "__table__":
            ref = self.static_ref(node.value)
            return isinstance(ref, Sym) and ref in self.p.fe.model_syms
        if isinstance(node, ast.Name) and depth < 3 and self.is_local(node.id) and self.node is not None:
            values = [a.value for a in ast.walk(self.scope_root())
                      if isinstance(a, ast.Assign) and any(isinstance(t, ast.Name) and t.id == node.id for t in a.targets)]
            return bool(values) and all(self.model_table(v, depth + 1) for v in values)
        return False

    TABLE_ATTRS = {"name", "columns", "c"}
    COLUMNS_ATTRS = {"keys", "values", "get"}
    # methods whose arguments are SQL constructs: a table given to them is used as a Core table
    SQL_ARG_METHODS = {"select_from", "join", "outerjoin", "join_from", "where", "filter", "with_only_columns"}

    # the standard-library modules PEP 594 removed in Python 3.13 (`sys.stdlib_module_names` of 3.12 minus 3.13's)
    REMOVED_IN_313 = {"aifc", "audioop", "cgi", "cgitb", "chunk", "crypt", "imghdr", "mailcap", "msilib", "nis",
                      "nntplib", "ossaudiodev", "pipes", "sndhdr", "spwd", "sunau", "telnetlib", "uu", "xdrlib"}

    def starlette_context(self, dotted: str, node: ast.AST) -> str:
        """`starlette_context.context`, a UserDict over the request's context dict (dynrt/ctxmw.rs): `context.data`
        is the dict itself; `context` is read as that dict where the UserDict behaves the same (`context[k]`, `k in
        context`, `len()`, iteration, a test, `dict()`/`list()`/`str()`, `**context`). Anywhere else (a value handed
        on, `repr()`, `isinstance`, JSON) the UserDict differs from a dict: refused."""
        data = self.q(f"{RT}::ctxmw::data(cx)")
        if dotted.endswith(".data"):
            return data
        par = self.p.fe._parents(self.module)
        p = par.get(node)
        if isinstance(p, ast.Subscript) and p.value is node:
            return data
        if isinstance(p, ast.Compare) and node in p.comparators \
                and isinstance(p.ops[p.comparators.index(node)], (ast.In, ast.NotIn)):
            return data
        if isinstance(p, (ast.For, ast.AsyncFor, ast.comprehension)) and p.iter is node:
            return data
        if isinstance(p, (ast.If, ast.While, ast.IfExp)) and p.test is node \
                or isinstance(p, ast.comprehension) and node in p.ifs or isinstance(p, ast.UnaryOp) and isinstance(p.op, ast.Not):
            return data
        if isinstance(p, ast.Dict) and any(k is None and v is node for k, v in zip(p.keys, p.values)):
            return data
        if isinstance(p, ast.Call) and p.args == [node] and not p.keywords \
                and self.static_ref(p.func) in {("builtin", n) for n in ("len", "dict", "list", "str", "bool", "sorted", "set", "tuple")}:
            return data
        raise self.err("library value `starlette_context.context` is not supported here: it is supported as a mapping "
                       "(`context[key]`, `key in context`, `context.get(...)`, `len()`, iteration, `context.data`), not "
                       "handed on as the UserDict object itself; use `context.data`", node)

    def starlette_context_call(self, meth: str, node: ast.Call) -> str:
        """`context.get(...)`, `context.exists()`...: UserDict's methods, those of the request's context dict (the same
        behaviour) except `popitem` (MutableMapping's pops the first key, dict's the last) and `exists`."""
        if meth == "exists":
            if node.args or node.keywords:
                raise self.err("context.exists() takes no arguments", node)
            return self.q(f"{RT}::ctxmw::exists(cx)")
        if meth not in self.CONTEXT_METHODS:
            raise self.err(f"library call `starlette_context.context.{meth}()` is not supported (of the context's "
                           f"methods: {', '.join(sorted(self.CONTEXT_METHODS | {'exists'}))})", node)
        args, kwargs = self.dyn_args(node)
        return self.q(f"{RT}::methods::call_method(cx, &{self.q(f'{RT}::ctxmw::data(cx)')}, {rs(meth)}, {args}, {kwargs}).await")

    CONTEXT_METHODS = {"get", "setdefault", "pop", "update", "keys", "values", "items", "clear", "copy"}

    def check_sys_modules(self, node: ast.AST) -> None:
        """`sys.modules` with a literal key the project's Python cannot import (a module PEP 594 removed, on 3.13+,
        with no backport installed): absent at startup, as in CPython; `"k" in`/`not in`, `sys.modules["k"]` (read,
        store), `sys.modules.get("k")`. The other keys name modules CPython may have loaded: refused."""
        par = self.p.fe._parents(self.module)
        p = par.get(node)
        key = None
        if isinstance(p, ast.Compare) and len(p.ops) == 1 and isinstance(p.ops[0], (ast.In, ast.NotIn)) \
                and p.comparators[0] is node and isinstance(p.left, ast.Constant):
            key = p.left.value
        elif isinstance(p, ast.Subscript) and p.value is node and isinstance(p.slice, ast.Constant):
            key = p.slice.value
        elif isinstance(p, ast.Attribute) and p.attr == "get" and isinstance(par.get(p), ast.Call) \
                and par.get(p).func is p and par.get(p).args and isinstance(par.get(p).args[0], ast.Constant):
            key = par.get(p).args[0].value
        if not isinstance(key, str):
            raise self.err("sys.modules is only supported with a literal key (`'name' in sys.modules`, "
                           "`sys.modules['name']`, `.get('name')`): the binary keeps no table of loaded modules", node)
        if not (libmap.PYTHON >= (3, 13) and key in self.REMOVED_IN_313 and self.p.ix.module(key) is None
                and not locked_version(self.p.ix.root, f"standard-{key}")):
            raise self.err(f"sys.modules[{key!r}] is not supported: only the key of a module the project's Python "
                           "cannot import (removed from the standard library in 3.13, no backport installed) is "
                           "known to be absent; CPython may have loaded any other", node)

    def table_uses(self, node: ast.AST, depth: int = 0) -> set[str]:
        """How the value of `node` (`Model.__table__`, or a name bound to it) is used: "core" (a Core construct
        needs the table: `t.join(...)`, `t.c.col`, `select(t)`), "both" (`t.name`, served either way), "intro"
        (introspection: `t.columns` iterated, `t.c.keys()`), "other" (anything else, given to project code)."""
        par = self.p.fe._parents(self.module)
        p = par.get(node)
        if isinstance(p, ast.Attribute) and p.value is node:
            if p.attr in {"name", "fullname"}:
                return {"both"}
            if p.attr in {"c", "columns"}:
                pp = par.get(p)
                if isinstance(pp, ast.Attribute) and pp.value is p:
                    return {"intro" if pp.attr in self.COLUMNS_ATTRS else "core"}
                return {"intro"}
            return {"core"}
        call = par.get(p) if isinstance(p, ast.keyword) else p
        if isinstance(call, ast.Call) and (node in call.args or p in call.keywords):
            f = call.func
            if isinstance(f, ast.Attribute) and f.attr in self.SQL_ARG_METHODS:
                return {"core"}
            ref = self.static_ref(f)
            if isinstance(ref, Ext) and ref.dotted.startswith("sqlalchemy."):
                return {"core"}
            return {"other"}
        if (isinstance(p, ast.Assign) and p.value is node and len(p.targets) == 1 and isinstance(p.targets[0], ast.Name)
                and depth < 3):
            name = p.targets[0].id
            root = self.scope_root() if self.is_local(name) and self.node is not None else self.p.ix.module(self.module).tree
            out: set[str] = set()
            for n in ast.walk(root):
                if isinstance(n, ast.Name) and n.id == name and isinstance(n.ctx, ast.Load):
                    out |= self.table_uses(n, depth + 1)
            return out
        return {"other"}

    def table_core(self, node: ast.AST) -> bool:
        """`Model.__table__` (or a local bound to it) used as a Core table; mixed with an introspection use of the
        same value, refused (the runtime's Core table does not iterate its columns as SQLAlchemy objects)."""
        defs = [node] if isinstance(node, ast.Attribute) else [
            a.value for a in ast.walk(self.scope_root() if self.node is not None else self.p.ix.module(self.module).tree)
            if isinstance(a, ast.Assign) and any(isinstance(t, ast.Name) and t.id == getattr(node, "id", None) for t in a.targets)]
        uses = set()
        for d in defs:
            uses |= self.table_uses(d)
        if "core" not in uses:
            return False
        if uses & {"intro", "other"}:
            raise self.err("Model.__table__ used both as a Core table and for its introspection (or given to other "
                           "code): not supported, use two separate reads of __table__", node)
        return True

    def check_model_table(self, node: ast.Attribute) -> None:
        """What the runtime gives of `Model.__table__` is its introspection (name, columns and their name, key,
        nullable, primary_key, type), or a Core table when the code uses it as one (`table_core`): a Core use mixed
        with introspection is refused here rather than raising at run time."""
        if self.model_table(node.value) and self.table_core(node.value):
            return
        tab = node.value.value if isinstance(node.value, ast.Attribute) and node.value.attr in {"c", "columns"} else None
        if tab is not None and self.model_table(tab) and self.table_core(tab):
            return
        if self.model_table(node.value) and node.attr not in self.TABLE_ATTRS:
            raise self.err(f"Model.__table__.{node.attr}: the Core use of a mapped class's table is not supported "
                           "(introspection only: name, columns)", node)
        if (isinstance(node.value, ast.Attribute) and node.value.attr in {"c", "columns"} and self.model_table(node.value.value)
                and node.attr not in self.COLUMNS_ATTRS):
            raise self.err(f"Model.__table__.{node.value.attr}.{node.attr}: a column of a mapped class's table is not "
                           "supported (iterate the columns, or use Model.attribute)", node)

    def check_async_class_method(self, sym, attr: str, node: ast.AST) -> None:
        """`Cls.method` where `method` is an `async def` of the plain or Pydantic class `Cls`: the runtime reads a
        method on its class as a function it runs at once (no coroutine), so it is refused here (it also is at run
        time, through a class held as a value)."""
        if sym in self.p.fe.schema_syms:
            info = self.p.schema(sym)
        elif sym not in self.p.fe.model_syms and sym not in self.p.fe.dataclass_syms and self.p.is_plain_class(sym):
            info = self.p.plain_class(sym)
        else:
            return
        if attr in async_method_names(self.p, info.methods):
            raise self.err(f"the async method {sym.name}.{attr} read on its class is not supported (call it on an "
                           "instance)", node)

    # what the runtime offers of Starlette's Request (dynrt/methods.rs, `Native::Request`): the rest of the class
    # (session, user, auth, close, stream, url_for, send_push_promise, the Mapping of the scope, free attributes)
    # is refused where the variable is a parameter annotated `Request`
    REQUEST_ATTRS = frozenset({"headers", "state", "url", "base_url", "method", "query_params", "cookies", "client",
                               "path_params", "app", "scope", "receive", "body", "json", "is_disconnected",
                               # Starlette's cache of body(): per Request object (the route's, a middleware's...)
                               "_body"})

    ITERATING = frozenset({"sorted", "list", "dict", "len", "tuple", "set", "frozenset", "iter", "enumerate", "sum", "min", "max", "any", "all"})

    def request_names(self) -> frozenset:
        """Parameters annotated `Request` (never rebound in the function), of this function and the ones around it."""
        got = self.__dict__.get("_request_names")
        if got is not None:
            return got
        names: set[str] = set()
        fn = self.node
        if isinstance(fn, (ast.FunctionDef, ast.AsyncFunctionDef)):
            a = fn.args
            rebound = {n.id for n in ast.walk(fn) if isinstance(n, ast.Name) and isinstance(n.ctx, (ast.Store, ast.Del))}
            for arg in (*a.posonlyargs, *a.args, *a.kwonlyargs):
                if arg.annotation is not None and arg.arg not in rebound and is_ext(
                        self.p.resolve(self.module, arg.annotation), "fastapi.Request", "starlette.requests.Request",
                        "fastapi.requests.Request"):
                    names.add(arg.arg)
        if self.parent is not None:
            names |= {n for n in self.parent.request_names() if n not in self.locals and n not in self.params}
        self.__dict__["_request_names"] = frozenset(names)
        return self._request_names

    # the attributes of the objects a Request hands out (`request.url`...) that the runtime reproduces
    REQUEST_SUB = {"url": {"path", "query", "hostname", "scheme", "netloc", "fragment", "port", "username", "password"},
                   "base_url": {"path", "scheme"},
                   "query_params": {"get", "keys", "values", "items"},
                   "headers": {"get", "getlist", "keys", "values", "items"},
                   "client": {"host", "port"},
                   "app": {"state", "router", "routes", "dependency_overrides"}}
    # the keys of `request.scope` the runtime has (the real one holds more: asgi, server, extensions, fastapi_*...)
    REQUEST_SCOPE_KEYS = {"type", "http_version", "scheme", "method", "root_path", "path", "raw_path", "query_string",
                          "headers", "client", "app", "endpoint", "path_params", "route"}
    # keys only a middleware sets (Starlette's SessionMiddleware, AuthenticationMiddleware: refused by the runtime,
    # or a raw ASGI middleware of the project, whose scope is the request's): absent otherwise, on both sides
    REQUEST_SCOPE_MW_KEYS = {"session", "user", "auth"}

    def request_sub(self, node: ast.AST):
        """`request.<sub>` when `node` is that read on a Request parameter, else None."""
        if (isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name) and node.value.id in self.request_names()):
            return node.attr
        return None

    # the scope keys only a middleware of Starlette sets: without that middleware in the project, `scope.get(key)` is None
    SCOPE_BY_MIDDLEWARE = {"session": "SessionMiddleware", "user": "AuthenticationMiddleware",
                           "auth": "AuthenticationMiddleware"}

    def scope_key_unset(self, key) -> bool:
        mw = self.SCOPE_BY_MIDDLEWARE.get(key)
        if mw is None:
            return False
        used = self.p.__dict__.setdefault("_scope_middleware", {})
        if mw not in used:
            used[mw] = any(isinstance(n, (ast.Name, ast.Attribute, ast.alias)) and mw in (
                getattr(n, "id", None), getattr(n, "attr", None), getattr(n, "name", None))
                for m in self.p.ix.package_modules() for n in ast.walk(m.tree))
        return not used[mw]

    def check_request_use(self, node: ast.AST) -> None:
        """An attribute, a subscript or a membership test on a `Request` parameter that the runtime does not provide."""
        if isinstance(node, ast.Attribute):
            sub = self.request_sub(node.value)
            if sub in self.REQUEST_SUB and node.attr not in self.REQUEST_SUB[sub]:
                raise self.err(f"attribute .{node.attr} of a Request is not supported on request.{sub} (the runtime provides "
                               f"{', '.join(sorted(self.REQUEST_SUB[sub]))})", node)
            if sub == "scope" and node.attr in {"keys", "values", "items", "copy", "update", "pop", "setdefault"}:
                raise self.err(f"attribute .{node.attr} of a Request's scope is not supported (the runtime's scope holds only "
                               f"{', '.join(sorted(self.REQUEST_SCOPE_KEYS))})", node)
        if isinstance(node, ast.Subscript) and self.request_sub(node.value) == "scope" and isinstance(node.slice, ast.Constant):
            if node.slice.value not in self.REQUEST_SCOPE_KEYS | self.REQUEST_SCOPE_MW_KEYS:
                raise self.err(f"attribute .{node.slice.value} of a Request's scope is not supported (the runtime's scope holds "
                               f"only {', '.join(sorted(self.REQUEST_SCOPE_KEYS))})", node)
        if (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == "get"
                and self.request_sub(node.func.value) == "scope" and node.args and isinstance(node.args[0], ast.Constant)
                and node.args[0].value not in self.REQUEST_SCOPE_KEYS
                and not self.scope_key_unset(node.args[0].value)):
            raise self.err(f"attribute .{node.args[0].value} of a Request's scope is not supported (the runtime's scope holds "
                           f"only {', '.join(sorted(self.REQUEST_SCOPE_KEYS))})", node)
        if (isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id in {"str", "repr", "type", "id", "hash", "format"}
                and any(self.request_sub(a) == "query_params" for a in node.args)):
            raise self.err("attribute .__str__ of a Request's query_params is not supported (the runtime holds a dict, "
                           "not Starlette's QueryParams)", node)
        if isinstance(node, ast.FormattedValue) and self.request_sub(node.value) == "query_params":
            raise self.err("attribute .__str__ of a Request's query_params is not supported (the runtime holds a dict, "
                           "not Starlette's QueryParams)", node)
        if isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name) and node.value.id in self.request_names():
            # form() and stream() have their own refusal, in check_method
            if node.attr not in self.REQUEST_ATTRS and node.attr not in {"form", "stream"}:
                raise self.err(f"attribute .{node.attr} of a Request is not supported (the runtime provides "
                               f"{', '.join(sorted(self.REQUEST_ATTRS))}): it would fail at run time, or read another value", node)
        elif isinstance(node, ast.Subscript) and isinstance(node.value, ast.Name) and node.value.id in self.request_names():
            raise self.err(f"attribute .__getitem__ of a Request (a Mapping over its scope) is not supported: read `{node.value.id}.scope[...]`", node)
        elif (isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id in self.ITERATING
              and any(isinstance(a, ast.Name) and a.id in self.request_names() for a in node.args)):
            raise self.err(f"attribute .__iter__ of a Request (a Mapping over its scope) is not supported: {node.func.id}(request) "
                           "iterates the scope's keys", node)
        elif isinstance(node, ast.Compare) and any(isinstance(op, (ast.In, ast.NotIn)) and isinstance(c, ast.Name)
                                                    and c.id in self.request_names() for op, c in zip(node.ops, node.comparators)):
            raise self.err("attribute .__contains__ of a Request (a Mapping over its scope) is not supported: test `in request.scope`", node)

    def e_Attribute(self, node: ast.Attribute) -> str:
        if node.attr == "__class__" and isinstance(node.ctx, ast.Load) and self.static_ref(node) is None:
            # `x.__class__`: `type(x)` (`c.__class__ is int`, olefile's i8)
            return self.expr(ast.copy_location(ast.Call(ast.Name("type", ast.Load()), [node.value], []), node))
        self.check_request_use(node)
        self.check_passlib(node)
        self.check_httpx_request(node)
        self.check_model_table(node)
        ref = self.static_ref(node)
        if ref is not None:
            if isinstance(ref, tuple) and ref[0] == "classattr" and self.p.enum_kind(ref[1]):
                _, _, names = self.p.enum(ref[1])
                if ref[2] in names:
                    return f"V::Enum(&{self.p.enum(ref[1])[0]}, {names.index(ref[2])})"
            if isinstance(ref, tuple) and ref[0] == "classattr":
                sym, attr = ref[1], ref[2]
                if sym in self.p.fe.model_syms and attr == "__mapper__":
                    raise self.err(f"{sym.name}.__mapper__: the mapper of a mapped class is not supported", node)
                if sym in self.p.fe.model_syms:
                    if attr == "__table__" and self.table_core(node):
                        return f"V::Class(&{self.p.mapped_core_table(sym, node, self.module).cls})"
                    if attr == "__table__":
                        return f"V::native({RT}::v::Native::SaTable(&{self.p.sa_table(sym, node, self.module)}))"
                    info = self.p.model(sym)
                    for i, c in enumerate(info.cols):
                        if c["name"] == attr:
                            return f"V::Col(&{info.rust}, {i})"
            return self.ref_value(ref, node)
        if (isinstance(node.value, ast.Name) and node.value.id in BUILTIN_TYPES - {"object"} and node.value.id not in self.locals
                and self.p.resolve(self.module, node.value) is None and method_descriptor(node.value.id, node.attr)):
            return self.method_descriptor_value(node.value.id, node.attr)
        if (node.attr in CT_WRAPPER_ATTRS and isinstance(node.value, ast.Attribute)
                and node.value.attr in self.p.cachedmethod_names()):
            raise self.err(f"`.{node.value.attr}.{node.attr}` of a @cachedmethod is not supported (only its calls)", node)
        obj = self.expr(node.value)
        self.check_attr(node)
        return self.q(f"{RT}::methods::getattr(cx, &{obj}, {rs(node.attr)}).await")

    def method_descriptor_value(self, ty: str, attr: str) -> str:
        """`str.lower`, `dict.get`... as a value (`sorted(xs, key=str.lower)`, a validator): a function calling the
        method on its first argument, which must be an instance of the type (CPython's descriptor TypeErrors)"""
        tname = {"frozenset": "frozenset"}.get(ty, ty)
        body = (f"let mut __it = args.into_iter(); "
                f"let Some(__recv) = __it.next() else {{ return Err(Exc::type_error({rs(f'unbound method {ty}.{attr}() needs an argument')})) }}; "
                f"if !{RT}::methods::isinstance_builtin(&__recv, {rs(tname)}) {{ return Err(Exc::type_error(format!("
                f"\"descriptor '{attr}' for '{ty}' objects doesn't apply to a '{{}}' object\", __recv.type_name()))) }} "
                f"let (__a, __k) = {RT}::unpack(__it.collect()); "
                f"{RT}::methods::call_method(cx, &__recv, {rs(attr)}, __a, __k).await")
        return (f"{RT}::func(std::sync::Arc::new(|cx: &Cx, args: Vec<V>| -> {RT}::BoxFut<'_> "
                f"{{ Box::pin(async move {{ {body} }}) }}))")

    def check_attr(self, node: ast.Attribute) -> None:
        """An attribute read at run time that no runtime type provides (and the project never defines nor
        names) is a certain AttributeError on that path: refused here, so that --report counts it."""
        name = node.attr
        self.check_passlib(node)
        if name in self.p.project_attrs() or name in runtime_attr_names() or self.p.open_attrs() or name in self.p.stripe_fields():
            return
        raise self.err(f"attribute .{name} is not provided by the runtime for any type "
                       "(it would raise AttributeError at run time)", node)

    GENERIC_BUILTINS = {"list", "dict", "set", "tuple", "frozenset", "type"}
    TYPING_GENERICS = {"List", "Dict", "Set", "Tuple", "FrozenSet", "Optional", "Union", "Literal", "Annotated",
                       "Sequence", "Iterable", "Mapping"}

    def annot_value(self, ann) -> str:
        """An annotation evaluated as a value (`typing.get_type_hints`); opaque when not expressible."""
        if isinstance(ann, ast.Constant) and isinstance(ann.value, str):
            try:
                ann = ast.parse(ann.value, mode="eval").body
            except SyntaxError:
                return f"V::native({RT}::Native::ExtType({rs(ann.value)}))"
        if isinstance(ann, ast.Constant) and ann.value is None:
            return f"V::native({RT}::Native::Type(\"NoneType\"))"
        te = self.type_expr(ann) if isinstance(ann, (ast.Subscript, ast.BinOp)) else None
        if te is not None:
            return te
        if self.is_type_ref(ann) or isinstance(self.static_ref(ann), Ext):
            saved = list(self.lines)
            try:
                return self.expr(ann)
            except TranspileError:
                self.lines = saved
        return f"V::native({RT}::Native::ExtType({rs(ast.unparse(ann))}))"

    def is_type_ref(self, node) -> bool:
        """Does this expression denote a type statically (class, builtin type, None, generic alias)?"""
        if isinstance(node, ast.Constant) and node.value is None:
            return True
        if isinstance(node, (ast.Subscript, ast.BinOp)):
            return self.type_expr(node) is not None
        ref = self.static_ref(node)
        if isinstance(ref, tuple) and ref[0] == "builtin":
            return ref[1] in BUILTIN_TYPES
        if isinstance(ref, Sym):
            return isinstance(self.p.ix.definition(ref), ast.ClassDef)
        return isinstance(ref, Ext) and ref.dotted in {"datetime.datetime", "datetime.date", "uuid.UUID", "decimal.Decimal"}

    def type_expr(self, node) -> str | None:
        """`list[Schema]`, `X | None`, `Optional[X]` used as a value: a type value backed by its validator."""
        if isinstance(node, ast.Subscript):
            ref = self.static_ref(node.value)
            ok = (isinstance(ref, tuple) and ref[0] == "builtin" and ref[1] in self.GENERIC_BUILTINS) or (
                isinstance(ref, Ext) and ref.dotted.startswith("typing.") and ref.dotted.split(".")[-1] in self.TYPING_GENERICS)
            if not ok:
                return None
        elif isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr):
            if not (self.is_type_ref(node.left) and self.is_type_ref(node.right)):
                return None
        else:
            return None
        try:
            td = self.p.td(node, self.module, None, self.scope_root())
        except TranspileError:
            return None
        return f"V::native({RT}::Native::TypeExpr(&{td}, {rs(ast.unparse(node))}))"

    def e_Subscript(self, node: ast.Subscript) -> str:
        self.check_request_use(node)
        te = self.type_expr(node)
        if te is not None:
            return te
        if isinstance(node.slice, ast.Tuple) and isinstance(node.value, ast.Attribute):
            # `Model.json_col[("a", "b")]`: SQLAlchemy's JSON path (#>, #>>), not in the runtime
            owner = self.p.resolve(self.module, node.value.value, self.scope_root())
            if isinstance(owner, Sym) and owner in self.p.models:
                raise self.err("a JSON path index (`col[(\"a\", \"b\")]`, PostgreSQL #> / #>>) is not supported: "
                               "index one key with col[\"k\"] and its accessors (as_string(), ...), or keep the route "
                               "on the Python side", node)
        obj = self.expr(node.value)
        if isinstance(node.slice, ast.Slice):
            s = node.slice
            parts = [self.expr(x) if x is not None else "V::None" for x in (s.lower, s.upper, s.step)]
            return self.q(f"{RT}::ops::getslice(&{obj}, &{parts[0]}, &{parts[1]}, &{parts[2]})")
        return self.q(f"{RT}::ops::getitem(&{obj}, &{self.expr(node.slice)})")

    def e_BinOp(self, node: ast.BinOp) -> str:
        te = self.type_expr(node)
        if te is not None:
            return te
        op = self.BINOPS.get(type(node.op))
        if op is None:
            raise self.err("unsupported binary operator", node)
        self.check_int_range(node)
        return self.q(f"{RT}::ops::{op}(&{self.expr(node.left)}, &{self.expr(node.right)})")

    def check_int_range(self, node: ast.BinOp) -> None:
        """An integer expression of literals (`10**100`, `1 << 70`), or of comprehension variables over a literal
        tuple of integers (`[10**x for x in (3, 100)]`), beyond 64 bits: CPython makes a big integer, the runtime's
        int raises OverflowError at run time (at startup for a module constant). Refused here instead."""
        envs = [{}]
        for scope in getattr(self, "comp_ints", []):
            for name, vals in scope.items():
                if vals is None:
                    envs = [{k: v for k, v in e.items() if k != name} for e in envs]
                else:
                    envs = [{**e, name: v} for e in envs for v in vals]
                if len(envs) > 1000:
                    return
        for env in envs:
            v = const_int(node, env)
            if v is not None and not -(2**63) <= v < 2**63:
                where = "" if not env else " (with " + ", ".join(f"{k} = {x}" for k, x in env.items()) + ")"
                raise self.err(f"integer expression out of 64-bit range{where}: CPython makes a big integer, the "
                               "runtime's int is 64-bit (OverflowError)", node)

    def e_UnaryOp(self, node: ast.UnaryOp) -> str:
        v = self.expr(node.operand)
        if isinstance(node.op, ast.Not):
            return f"V::Bool(!{self.q(f'{RT}::ops::truthy(&{v})')})"
        f = {ast.USub: "neg", ast.UAdd: "pos", ast.Invert: "invert"}[type(node.op)]
        return self.q(f"{RT}::ops::{f}(&{v})")

    def e_BoolOp(self, node: ast.BoolOp) -> str:
        vals = node.values
        code = self.expr(vals[-1])
        for v in reversed(vals[:-1]):
            t = self.tmp()
            test = self.q(f"{RT}::ops::truthy(&{t})")
            if isinstance(node.op, ast.And):
                code = f"{{ let {t} = {self.expr(v)}; if !{test} {{ {t} }} else {{ {code} }} }}"
            else:
                code = f"{{ let {t} = {self.expr(v)}; if {test} {{ {t} }} else {{ {code} }} }}"
        return code

    CMP = {ast.Eq: "eq", ast.NotEq: "ne", ast.Lt: "lt", ast.LtE: "le", ast.Gt: "gt", ast.GtE: "ge"}

    def cmp1(self, op, a: str, b: str) -> str:
        if type(op) in self.CMP:
            return self.q(f"{RT}::ops::{self.CMP[type(op)]}(&{a}, &{b})")
        if isinstance(op, ast.In):
            return f"V::Bool({self.q(f'{RT}::ops::contains(&{b}, &{a})')})"
        if isinstance(op, ast.NotIn):
            return f"V::Bool(!{self.q(f'{RT}::ops::contains(&{b}, &{a})')})"
        if isinstance(op, ast.Is):
            return f"V::Bool({RT}::ops::is(&{a}, &{b}))"
        if isinstance(op, ast.IsNot):
            return f"V::Bool(!{RT}::ops::is(&{a}, &{b}))"
        raise TranspileError("unsupported comparison")

    def e_Compare(self, node: ast.Compare) -> str:
        self.check_request_use(node)
        if (len(node.ops) == 1 and isinstance(node.ops[0], (ast.Is, ast.IsNot)) and isinstance(node.left, ast.Name)
                and isinstance(node.comparators[0], ast.Constant) and node.comparators[0].value is None
                and node.left.id not in self.locals and self.p.ix.module(self.module) is not None
                and (self.p.ix.module(self.module).imports.get(node.left.id) or ("",))[0] == "module"
                and node.left.id not in self.p.ix.module(self.module).defs):
            # `import orjson` ... `orjson is not None` (an import guard): a module is never None
            return f"V::Bool({str(isinstance(node.ops[0], ast.IsNot)).lower()})"
        if len(node.ops) == 1:
            return self.cmp1(node.ops[0], self.expr(node.left), self.expr(node.comparators[0]))
        parts = []
        left = self.tmp()
        code_lines = [f"let {left} = {self.expr(node.left)};"]
        prev = left
        result = None
        for op, c in zip(node.ops, node.comparators):
            cur = self.tmp()
            code_lines.append(f"let {cur} = {self.expr(c)};")
            parts.append(self.cmp1(op, prev, cur))
            prev = cur
        code = parts[-1]
        for p in reversed(parts[:-1]):
            t = self.tmp()
            code = f"{{ let {t} = {p}; if !{self.q(f'{RT}::ops::truthy(&{t})')} {{ {t} }} else {{ {code} }} }}"
        result = "{ " + " ".join(code_lines) + f" {code} }}"
        return result

    def e_IfExp(self, node: ast.IfExp) -> str:
        return f"(if {self.truthy(node.test)} {{ {self.expr(node.body)} }} else {{ {self.expr(node.orelse)} }})"

    def e_TemplateStr(self, node) -> str:
        raise self.err("t-strings (template strings, PEP 750, Python 3.14) are not supported: the runtime has no "
                       "string.templatelib.Template. Build the text with an f-string, or leave the route to Python", node)

    def e_JoinedStr(self, node: ast.JoinedStr) -> str:
        for part in node.values:
            if isinstance(part, ast.FormattedValue):
                self.check_request_use(part)
        s = self.tmp("s")
        parts = [f"let mut {s} = String::new();"]
        for v in node.values:
            if isinstance(v, ast.Constant):
                parts.append(f"{s}.push_str({rs(v.value)});")
            else:
                val = self.expr(v.value)
                if v.conversion == ord("r"):
                    val = f"V::str({self.q(f'{RT}::ops::repr(&{val})')})"
                elif v.conversion == ord("a"):
                    raise self.err("!a conversion is not supported", v)
                if v.format_spec is not None:
                    if all(isinstance(x, ast.Constant) for x in v.format_spec.values):
                        self.check_strftime("".join(x.value for x in v.format_spec.values), v.format_spec, "an f-string format spec")
                    spec = self.expr(v.format_spec)
                    parts.append(f"{s}.push_str(&{self.q(f'{RT}::ops::format_spec(&{val}, &{RT}::ops::str_(&{spec})?)')});")
                elif v.conversion == ord("s") or v.conversion == ord("r"):
                    parts.append(f"{s}.push_str(&{self.q(f'{RT}::ops::str_(&{val})')});")
                else:
                    # `format(x, '')`: str(x), unless the type has its own __format__
                    parts.append(f"{s}.push_str(&{self.q(f'{RT}::ops::format_empty(&{val})')});")
        return "{ " + " ".join(parts) + f" V::str({s}) }}"

    def e_FormattedValue(self, node) -> str:
        return self.e_JoinedStr(ast.JoinedStr(values=[node]))

    def seq_items(self, elts) -> str:
        if any(isinstance(e, ast.Starred) for e in elts):
            parts = ", ".join(f"({'true' if isinstance(e, ast.Starred) else 'false'}, {self.expr(e.value if isinstance(e, ast.Starred) else e)})" for e in elts)
            return self.q(f"{RT}::args(vec![{parts}])")
        return "vec![" + ", ".join(self.expr(e) for e in elts) + "]"

    def e_List(self, node: ast.List) -> str:
        return f"V::list({self.seq_items(node.elts)})"

    def e_Tuple(self, node: ast.Tuple) -> str:
        return f"V::tuple({self.seq_items(node.elts)})"

    def e_Set(self, node: ast.Set) -> str:
        return self.q(f"{RT}::methods::b_set(&[V::list({self.seq_items(node.elts)})])")

    def e_Dict(self, node: ast.Dict) -> str:
        d = self.tmp("d")
        parts = [f"let mut {d}: Vec<(V, V)> = Vec::new();"]
        for k, v in zip(node.keys, node.values):
            if k is None:
                src = self.expr(v)
                it = self.tmp()
                parts.append(f"for {it} in {self.q(f'{RT}::ops::iter(&{RT}::methods::call_method(cx, &{src}, \"items\", vec![], vec![]).await?)')} {{ let __kv = {RT}::ops::unpack(&{it}, 2)?; {d}.push((__kv[0].clone(), __kv[1].clone())); }}")
            else:
                parts.append(f"{d}.push(({self.expr(k)}, {self.expr(v)}));")
        return "{ " + " ".join(parts) + f" {self.q(f'V::dict_from({d})')} }}"

    # ---- comprehensions

    def comprehension(self, generators, emit_inner, parts=()) -> str:
        out_lines = []
        depth = 0
        pushed = 0
        # a comprehension variable a lambda (or generator expression's function) inside reads: one cell for the
        # whole comprehension, rebound at each iteration (CPython's late binding: `[lambda: i for i in r]`)
        inner_fns = [n for x in [*parts, *(c for g in generators for c in [*g.ifs, g.iter])] for n in ast.walk(x)
                     if isinstance(n, (ast.Lambda, ast.FunctionDef, ast.AsyncFunctionDef))]
        read = {n.id for f in inner_fns for n in ast.walk(f) if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load)
                and n.id not in {p.name for p in fn_params(f)}}
        for g in generators:
            it = self.expr(g.iter)
            if g.is_async:
                # `async for` in a comprehension consumes the whole async iterator: collected first
                it = self.q(f"{RT}::aio::collect(cx, {it}).await")
            scope: dict[str, str] = {}
            names: set[str] = set()
            target_names(g.target, names)
            for n in sorted(names):  # sorted: set order follows PYTHONHASHSEED
                scope[n] = f"cv{self.p.uid()}_{ident(n)}"
                if n in read:
                    self.comp_cells.add(scope[n])
                    out_lines.insert(0, f"let {scope[n]}: V = {RT}::ops::cell_new();")
            self.comp_scopes.append(scope)
            # the integers a variable takes when it runs over a literal tuple (`check_int_range`), else unknown
            ints = None
            if isinstance(g.target, ast.Name) and isinstance(g.iter, (ast.Tuple, ast.List, ast.Set)):
                vals = [const_int(e, {}) for e in g.iter.elts]
                ints = vals if vals and all(v is not None for v in vals) else None
            self.__dict__.setdefault("comp_ints", []).append({n: (ints if isinstance(g.target, ast.Name) else None)
                                                             for n in names})
            pushed += 1
            var = self.tmp("c")
            li = self.tmp("li")
            out_lines.append(f"let mut {li} = {self.q(f'{RT}::proto::LoopIter::start(&{it}).await')};")
            out_lines.append(f"while let Some({var}) = {self.q(f'{li}.next()')} {{")
            depth += 1
            out_lines.append(self.bind_comp_target(g.target, var))
            for cond in g.ifs:
                out_lines.append(f"if !{self.truthy(cond)} {{ continue; }}")
        out_lines.append(emit_inner())
        out_lines.append("}" * depth)
        for _ in range(pushed):
            self.comp_scopes.pop()
            self.comp_ints.pop()
        return " ".join(out_lines)

    def bind_comp_target(self, target, val: str) -> str:
        if isinstance(target, ast.Name):
            var = self.comp_scopes[-1][target.id]
            if var in self.comp_cells:
                return f"*{RT}::ops::cell_mut(&{var}) = {val};"
            return f"let {var} = {val};"
        if isinstance(target, (ast.Tuple, ast.List)):
            t = self.tmp()
            parts = [f"let {t} = {self.q(f'{RT}::ops::unpack(&{val}, {len(target.elts)})')};"]
            for i, e in enumerate(target.elts):
                parts.append(self.bind_comp_target(e, f"{t}[{i}].clone()"))
            return " ".join(parts)
        raise self.err("unsupported comprehension target", target)

    def e_ListComp(self, node, star_msg: bool = True) -> str:
        out = self.tmp("l")
        if isinstance(node.elt, ast.Starred):
            # Python 3.15 (PEP 798): `[*it for x in xs]` extends with each iterable; a list display words the
            # error like `[*1]`, a set or a generator like iter()
            elt = node.elt.value
            body = self.comprehension(
                node.generators,
                lambda: f"{out}.extend({self.q(f'{RT}::ops::iter_star(&{self.expr(elt)}, {str(star_msg).lower()})')});", [elt])
        else:
            body = self.comprehension(node.generators, lambda: f"{out}.push({self.expr(node.elt)});", [node.elt])
        return f"{{ let mut {out}: Vec<V> = Vec::new(); {body} V::list({out}) }}"

    def e_GeneratorExp(self, node) -> str:
        return self.e_ListComp(node, star_msg=False)

    def e_SetComp(self, node) -> str:
        return self.q(f"{RT}::methods::b_set(&[{self.e_ListComp(node, star_msg=False)}])")

    def e_DictComp(self, node) -> str:
        out = self.tmp("d")
        if node.value is None:
            # Python 3.15 (PEP 798): `{**m for x in xs}` merges each mapping, later keys win
            body = self.comprehension(node.generators,
                                      lambda: f"{out}.extend({self.q(f'{RT}::ops::mapping_pairs(&{self.expr(node.key)})')});",
                                      [node.key])
        else:
            body = self.comprehension(node.generators, lambda: f"{out}.push(({self.expr(node.key)}, {self.expr(node.value)}));",
                                      [node.key, node.value])
        return f"{{ let mut {out}: Vec<(V, V)> = Vec::new(); {body} {self.q(f'V::dict_from({out})')} }}"

    def e_NamedExpr(self, node: ast.NamedExpr) -> str:
        t = self.tmp()
        name = self.store_name(node.target.id, node)
        return f"{{ let {t} = {self.expr(node.value)}; {name} = {t}.clone(); {t} }}"

    def e_Lambda(self, node: ast.Lambda) -> str:
        """A function object named `<lambda>`, bound like CPython (defaults evaluated here)."""
        free = self.free_vars(node)
        caps = {v: f"c_{ident(v)}" for v in free}
        sub = FnCompiler(self.p, self.module, node, "lambda", captures=caps, parent=self)
        params = fn_params(node)
        sub.params = {q.name for q in params}
        lfid = self.tmp("fid")
        sub.fid_name = lfid
        body = sub.expr(node.body)
        pre = " ".join(sub.lines)
        dflts: dict[int, str] = {}
        for i, q in enumerate(params):
            if q.default is not None:
                t = self.tmp("d")
                self.emit(f"let {t} = {self.expr(q.default)};")
                dflts[i] = t
        outer = self.qualname()
        qual = f"{outer}.<locals>.<lambda>" if outer else "<lambda>"
        kinds = {"positional": f"{RT}::P_POS", "vararg": f"{RT}::P_VARARG", "kwonly": f"{RT}::P_KWONLY", "kwarg": f"{RT}::P_KWARG"}
        spec = ", ".join(f"({rs(q.name)}, {kinds[q.kind]}, {str(q.default is not None).lower()})" for q in params)
        binds = (f"let mut __s = {RT}::bind_params({rs(qual + '()')}, 0, args, kwargs, &[{spec}])?; let _ = &mut __s; "
                 + "".join(f"let mut v_{ident(q.name)} = "
                           + (f"__s[{i}].take().unwrap_or_else(|| {dflts[i]}.clone()); " if i in dflts else f"__s[{i}].take().unwrap_or(V::None); ")
                           for i, q in enumerate(params)))
        cap_clone = " ".join(f"let {caps[v]} = {self.capture(v)};" for v in free)
        inner = "".join(f"let {c} = {c}.clone(); " for c in [*caps.values(), *dflts.values()])
        run = f"let {lfid}v = 0u8; let {lfid} = std::ptr::addr_of!({lfid}v) as usize; {pre} Ok({body})"
        if self.p.traced():
            run = (f"let __tf = ({RT}::trace::enter(cx, {rs(self.module)}, {rs(qual)}, {rs(self.p.rel_file(self.module))}, {node.lineno}).await)?; "
                   f"let __r: R = async {{ {run} }}.await; {RT}::trace::leave(cx, __tf, __r, {node.lineno}).await")
        clo = (f"move |cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>| -> {RT}::BoxFut<'_> {{ "
               f"{inner}Box::pin(async move {{ {binds}{run} }}) }}")
        clo = f"{{ {cap_clone} {clo} }}" if cap_clone else clo
        return f"{RT}::pyfn({rs(self.module)}, {rs(qual)}, None, false, std::sync::Arc::new({clo}))"

    def e_Await(self, node: ast.Await) -> str:
        v = node.value
        if isinstance(v, ast.Call):
            ref = self.static_ref(v.func)
            if isinstance(ref, Ext) and ref.dotted == "asyncio.wait_for":
                inner = v.args[0]
                timeout = self.expr(v.args[1]) if len(v.args) > 1 else next(
                    (self.expr(k.value) for k in v.keywords if k.arg == "timeout"), "V::None")
                sub = FnCompiler(self.p, self.module, None, "wait_for", parent=self)
                sub.locals, sub.params, sub.captures, sub.comp_scopes = self.locals, self.params, self.captures, list(self.comp_scopes)
                # the same scope: the same cells (a cell read as a value would be its one-slot list)
                sub.cells, sub.param_cells, sub.cell_caps = self.cells, self.param_cells, self.cell_caps
                sub.comp_cells, sub.nonlocals = self.comp_cells, self.nonlocals
                sub.node = self.node  # name resolution in the function's scope (its local imports)
                sub.factory = self.__dict__.get("factory")
                if isinstance(inner, ast.Call):
                    code = sub.call(inner, awaited=True)
                else:
                    # a future, a task or a coroutine held in a variable (`await asyncio.wait_for(fut, 5)`)
                    code = f"{RT}::aio::await_value({sub.expr(inner)}.clone()).await?"
                pre = " ".join(sub.lines)
                return self.q(f"{RT}::web::wait_for(&{timeout}, async {{ {pre} Ok({code}) }}).await")
            if isinstance(ref, Ext) and ref.dotted == "asyncio.to_thread":
                # the synchronous call itself (on the runtime's worker: no GIL to release)
                if not v.args or isinstance(v.args[0], ast.Starred):
                    raise self.err("asyncio.to_thread(func, ...) needs the function first", node)
                inner = ast.Call(func=v.args[0], args=v.args[1:], keywords=v.keywords)
                ast.copy_location(inner, v)
                return self.call(inner, awaited=False)
            if isinstance(v.func, ast.Attribute) and v.func.attr == "run_sync":
                return self.run_sync(v)
            return self.q(f"{RT}::aio::await_value({self.call(v, awaited=True)}).await")
        # a coroutine or task held in a variable, an attribute...
        return self.q(f"{RT}::aio::await_value({self.expr(v)}).await")

    def e_Yield(self, node: ast.Yield) -> str:
        if not self.gen:
            raise self.err("`yield` outside a generator", node)
        v = self.expr(node.value) if node.value is not None else "V::None"
        if self.variant == "depgen":
            return self.q(f"y.yield_({v}).await")
        return self.q(f"y.send({v}).await")

    def e_Call(self, node: ast.Call) -> str:
        self.check_request_use(node)
        if (len(node.args) == 1 and not node.keywords and isinstance(node.func, (ast.Name, ast.Attribute))
                and self.static_ref(node.func) == Ext("importlib.import_module")):
            if not (isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str)):
                raise self.err("importlib.import_module() of a computed name is not supported (only a literal module "
                               "of the project)", node)
            return self.ref_value(self.static_ref(node), node)  # a literal project module, or refused
        if (isinstance(node.func, ast.Attribute) and node.func.attr == "__getattribute__"
                and isinstance(node.func.value, ast.Name) and node.func.value.id == "object"
                and self.static_ref(node.func.value) in (None, ("builtin", "object"))):
            # `object.__getattribute__(obj, name)`: the normal lookup, without the class's `__getattr__` fallback
            if len(node.args) != 2 or node.keywords or any(isinstance(a, ast.Starred) for a in node.args):
                raise self.err("object.__getattribute__(obj, name) takes two positional arguments", node)
            obj, attr = self.expr(node.args[0]), self.expr(node.args[1])
            return self.q(f"{RT}::methods::getattribute(cx, &{obj}, &{RT}::ops::str_(&{attr})?).await")
        return self.call(node, awaited=False)

    # ---------------------------------------------------------------- calls

    def args_kwargs(self, node: ast.Call) -> tuple[list[str], dict[str, str], bool]:
        """(positional, keywords, has_spreads)."""
        spreads = any(isinstance(a, ast.Starred) for a in node.args) or any(k.arg is None for k in node.keywords)
        pos = [self.expr(a) for a in node.args if not isinstance(a, ast.Starred)]
        kw = {k.arg: self.expr(k.value) for k in node.keywords if k.arg is not None}
        return pos, kw, spreads

    def dyn_args(self, node: ast.Call) -> tuple[str, str]:
        """Runtime (args, kwargs) vectors, `*`/`**` expansions included."""
        if any(isinstance(a, ast.Starred) for a in node.args):
            parts = ", ".join(f"({'true' if isinstance(a, ast.Starred) else 'false'}, {self.expr(a.value if isinstance(a, ast.Starred) else a)})" for a in node.args)
            args = self.q(f"{RT}::args(vec![{parts}])")
        else:
            args = "vec![" + ", ".join(self.expr(a) for a in node.args) + "]"
        fixed = ", ".join(f"({rs(k.arg)}.to_string(), {self.expr(k.value)})" for k in node.keywords if k.arg is not None)
        spreads = [self.expr(k.value) for k in node.keywords if k.arg is None]
        if spreads:
            kwargs = self.q(f"{RT}::kwargs(vec![{fixed}], vec![{', '.join(spreads)}])")
        else:
            kwargs = f"vec![{fixed}]"
        return args, kwargs

    # BaseModel's class methods the runtime implements on a schema class
    BASEMODEL_CLASS_METHODS = {"model_validate", "model_validate_json", "model_json_schema"}

    def super_classmethod(self, node: ast.Call) -> str | None:
        """`super().m(...)` / `super(Cls, cls).m(...)` in a @classmethod of a Pydantic model: the parent's class
        method, `cls` still bound to the class called (a project parent's override, else BaseModel's)."""
        f = node.func
        sup = f.value
        if self.node is None or self.p.cur is None or "." not in self.p.cur[0].name or f.attr == "__init__":
            return None
        if "classmethod" not in {dotted(d) for d in self.node.decorator_list}:
            return None
        csym = Sym(self.p.cur[0].module, self.p.cur[0].name.split(".")[0])
        if csym not in self.p.fe.schema_syms:
            return None
        first = self.node.args.args[0].arg if self.node.args.args else None
        if sup.args and not (len(sup.args) == 2 and isinstance(sup.args[0], ast.Name) and sup.args[0].id in {csym.name, first}
                             and isinstance(sup.args[1], ast.Name) and sup.args[1].id == first):
            raise self.err(f"only `super()` or `super({csym.name}, {first})` is supported in a class method", sup)
        if sup.args and sup.args[0].id == first:
            # `super(cls, cls)`: the parent of the class called, the defining class's only if nothing subclasses it
            subs = [s.name for s, d in self.p.fe.schema_syms.items()
                    if any(self.p.resolve(s.module, b) == csym for b in d.bases)]
            if subs:
                raise self.err(f"`super({first}, {first})` in {csym.name}, which {', '.join(sorted(subs))} subclass: "
                               "the parent depends on the class called (write `super()`)", sup)
        me = f"v_{ident(first)}"
        t = self.p.resolve(csym.module, self.p.ix.definition(csym).bases[0])
        if isinstance(t, Sym) and t in self.p.fe.schema_syms:
            pdef = self.p.ix.definition(t)
            for stmt in pdef.body:
                if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)) and stmt.name == f.attr:
                    if "classmethod" not in {dotted(d) for d in stmt.decorator_list}:
                        raise self.err(f"super().{f.attr}(): {t.name}.{f.attr} is not a class method", node)
                    return self.project_call(Sym(t.module, f"{t.name}.{f.attr}"), stmt, node, False, prefix=[f"{me}.clone()"])
        elif not is_ext(t, "pydantic.BaseModel"):
            raise self.err(f"super().{f.attr}(): the parent of {csym.name} is not supported", node)
        if f.attr not in self.BASEMODEL_CLASS_METHODS:
            raise self.err(f"super().{f.attr}() in a class method: only {', '.join(sorted(self.BASEMODEL_CLASS_METHODS))} "
                           "of BaseModel are supported", node)
        args, kwargs = self.dyn_args(node)
        return self.q(f"{RT}::methods::call_method(cx, &{me}, {rs(f.attr)}, {args}, {kwargs}).await")

    def object_new(self, node: ast.Call) -> str:
        """`object.__new__(cls)` / `super().__new__(cls)` in a `__new__`: a bare instance (no `__init__`)."""
        if len(node.args) != 1 or node.keywords:
            raise self.err("only `object.__new__(cls)` / `super().__new__(cls)` (the class alone) is supported", node)
        return self.q(f"{RT}::pyd::object_new_of(&{self.expr(node.args[0])})")

    def super_init(self, node: ast.Call) -> str | None:
        """`super().__init__(...)` in the `__init__` of a project class."""
        f = node.func
        if not (isinstance(f, ast.Attribute) and isinstance(f.value, ast.Call) and isinstance(f.value.func, ast.Name)
                and f.value.func.id == "super"):
            return None
        if not f.value.args and f.attr == "_missing_" and self.p.cur is not None and "." in self.p.cur[0].name:
            # Enum._missing_: no member
            for a in node.args:
                self.expr(a)
            return "V::None"
        if f.attr == "__new__" and not f.value.args:
            return self.object_new(node)
        cm = self.super_classmethod(node)
        if cm is not None:
            return cm
        if (not f.value.args and f.attr != "__init__" and self.p.cur is not None and "." in self.p.cur[0].name
                and self.node is not None and self.node.args.args
                and self.p.request_subclass(Sym(self.p.cur[0].module, self.p.cur[0].name.split(".")[0]))):
            # Request's own method (`await super().body()`): the wrapped request's
            me = f"v_{ident(self.node.args.args[0].arg)}"
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::web::request_super(cx, &{me}, {rs(f.attr)}, {args}, {kwargs}).await")
        if (f.attr == "get_route_handler" and not f.value.args and not node.args and not node.keywords
                and self.p.cur is not None and "." in self.p.cur[0].name):
            csym = Sym(self.p.cur[0].module, self.p.cur[0].name.split(".")[0])
            bases = [self.p.resolve(csym.module, b) for b in self.p.ix.definition(csym).bases]
            if any(isinstance(b, Ext) and b.dotted in ROUTE_BASES for b in bases):
                # APIRoute.get_route_handler(): the route's own handler (request -> Response)
                return self.q(f"{RT}::web::original_handler(cx)")
        if f.value.args or f.attr != "__init__" or self.node is None or self.p.cur is None or "." not in self.p.cur[0].name:
            raise self.err("only `super().__init__(...)` inside a method is supported", node)
        csym = Sym(self.p.cur[0].module, self.p.cur[0].name.split(".")[0])
        me = f"v_{ident(self.node.args.args[0].arg)}"
        args, kwargs = self.dyn_args(node)
        cls = self.p.ix.definition(csym)
        if not cls.bases:
            if all(isinstance(a, ast.Starred) for a in node.args) and all(k.arg is None for k in node.keywords):
                # `super().__init__(*args, **kwargs)` (a cooperative mixin): a TypeError when anything is left over
                return self.q(f"{RT}::methods::object_init_args(&{args}, &{kwargs})")
            if node.args or node.keywords:
                raise self.err("object.__init__() takes no arguments", node)
            return "V::None"
        t = self.p.resolve(csym.module, cls.bases[0])
        if isinstance(t, Ext) and libmap.canonical(t.dotted) in MW_BASE:
            # BaseHTTPMiddleware.__init__(app): the stack calls `dispatch` itself
            if len(node.args) != 1 or node.keywords:
                raise self.err("only `super().__init__(app)` is supported in a BaseHTTPMiddleware", node)
            self.expr(node.args[0])
            return "V::None"
        if self.p.is_plain_class(csym) and not self.p.is_http_middleware(csym):
            # a plain class: its project base's __init__ (inherited ones included), else object.__init__
            pb = self.p.plain_bases(csym)
            pinit = self.p.plain_class(pb[0]).__dict__.get("init") if pb else None
            if pinit is not None:
                return self.q(f"{method_wrapper(self.p, pinit)}(cx, {me}.clone(), {RT}::pack({args}, {kwargs})).await")
            if node.args or node.keywords:
                raise self.err("object.__init__() takes exactly one argument (the instance to initialize)", node)
            return "V::None"
        if csym in self.p.fe.schema_syms:
            # a model's own __init__: the parent's __init__, or BaseModel's validation into `self`
            pinit = self.p.schema(t).__dict__.get("init") if isinstance(t, Sym) and t in self.p.fe.schema_syms else None
            if pinit is not None:
                return self.q(f"{method_wrapper(self.p, pinit)}(cx, {me}.clone(), {RT}::pack({args}, {kwargs})).await")
            if node.args:
                raise self.err("BaseModel.__init__() takes keyword arguments only", node)
            return self.q(f"{RT}::pyd::init_in_place(cx, &{me}, {RT}::kwargs_dict({kwargs})?).await")
        if isinstance(t, Sym):
            init = self.p.exc_init(t)
            if init is not None:
                return self.q(f"{method_wrapper(self.p, init)}(cx, {me}.clone(), {RT}::pack({args}, {kwargs})).await")
        if self.p.http_exc_base(csym):
            return self.q(f"{RT}::exc_http_init(&{me}, {args}, {kwargs})")
        if node.keywords:
            raise self.err("BaseException.__init__() takes no keyword arguments", node)
        return self.q(f"{RT}::exc_set_args(&{me}, {args})")

    DEFAULT_FACTORIES = {"int", "float", "str", "list", "dict", "set", "tuple", "bool"}

    def defaultdict_call(self, node: ast.Call) -> str:
        """`defaultdict(int)`, `defaultdict(list, mapping, **kw)`: a dict whose missing keys are filled by a
        builtin type (other factories call project code from a subscript: refused)."""
        if not node.args or any(isinstance(a, ast.Starred) for a in node.args) or len(node.args) > 2:
            raise self.err("defaultdict(factory[, mapping]) with a builtin type as factory is the only supported form", node)
        fac = node.args[0]
        if isinstance(fac, (ast.Name, ast.Attribute)) and self.p.resolve(self.module, fac, self.scope_root()) == Ext("collections.deque"):
            init = [self.expr(a) for a in node.args[1:]]
            _, kwargs = self.dyn_args(node)
            if any(k.arg is None for k in node.keywords):
                raise self.err("defaultdict(..., **mapping) is not supported", node)
            return self.q(f"{RT}::ops::defaultdict(\"deque\", {RT}::methods::b_dict(&vec![{', '.join(init)}], &{kwargs})?)")
        if isinstance(fac, ast.Lambda) and not fac.args.args and not fac.args.vararg and not fac.args.kwonlyargs \
                and not fac.args.kwarg and not fac.args.posonlyargs and self.literal_factory(fac.body):
            # `defaultdict(lambda: {"n": 0})`, `defaultdict(lambda: defaultdict(int))`: a literal, made fresh at each
            # missing key (a deep copy of it: what calling the lambda builds)
            init = [self.expr(a) for a in node.args[1:]]
            if any(k.arg is None for k in node.keywords):
                raise self.err("defaultdict(..., **mapping) is not supported", node)
            kwargs = "vec![" + ", ".join(f"({rs(k.arg)}.to_string(), {self.expr(k.value)})" for k in node.keywords) + "]"
            tmpl = self.expr(fac.body)
            return self.q(f"{RT}::ops::defaultdict_template({tmpl}, {RT}::methods::b_dict(&vec![{', '.join(init)}], &{kwargs})?)")
        if not (isinstance(fac, ast.Name) and self.static_ref(fac) == ("builtin", fac.id) and fac.id in self.DEFAULT_FACTORIES):
            raise self.err(f"defaultdict({ast.unparse(fac)}): only a builtin type factory or a lambda returning a literal "
                           f"is supported ({', '.join(sorted(self.DEFAULT_FACTORIES))})", node)
        init = [self.expr(a) for a in node.args[1:]]
        kwargs = "vec![" + ", ".join(f"({rs(k.arg)}.to_string(), {self.expr(k.value)})" for k in node.keywords if k.arg) + "]"
        if any(k.arg is None for k in node.keywords):
            raise self.err("defaultdict(..., **mapping) is not supported", node)
        return self.q(f"{RT}::ops::defaultdict({rs(fac.id)}, {RT}::methods::b_dict(&vec![{', '.join(init)}], &{kwargs})?)")

    def literal_factory(self, e: ast.AST) -> bool:
        """A lambda body that builds the same value at each call without reading anything: constants, displays of
        such, builtin types called without arguments, `defaultdict(<builtin>)` / `defaultdict(lambda: <literal>)`."""
        if isinstance(e, ast.Constant):
            return True
        if isinstance(e, (ast.List, ast.Tuple, ast.Set)):
            return all(self.literal_factory(x) for x in e.elts)
        if isinstance(e, ast.Dict):
            return all(k is not None and self.literal_factory(k) for k in e.keys) and all(self.literal_factory(v) for v in e.values)
        if isinstance(e, ast.UnaryOp) and isinstance(e.op, (ast.USub, ast.UAdd)) and isinstance(e.operand, ast.Constant):
            return True
        if isinstance(e, ast.Call) and not e.keywords and isinstance(e.func, ast.Name):
            ref = self.static_ref(e.func)
            if ref == ("builtin", e.func.id) and e.func.id in self.DEFAULT_FACTORIES | {"dict"} and not e.args:
                return True
            if ref == Ext("collections.defaultdict") and len(e.args) == 1:
                f = e.args[0]
                return (isinstance(f, ast.Name) and self.static_ref(f) == ("builtin", f.id) and f.id in self.DEFAULT_FACTORIES) \
                    or (isinstance(f, ast.Lambda) and not f.args.args and self.literal_factory(f.body))
        return False

    def formatter_call(self, node: ast.Call) -> str | None:
        """`string.Formatter().vformat(fmt, args, mapping)` / `.format(fmt, *args, **kw)` (the base class:
        same rules as `str.format`, named fields looked up with `mapping[name]`)."""
        f = node.func
        if not (isinstance(f, ast.Attribute) and isinstance(f.value, ast.Call) and not f.value.args
                and not f.value.keywords and self.static_ref(f.value.func) == Ext("string.Formatter")):
            return None
        if f.attr == "vformat":
            if len(node.args) != 3 or node.keywords:
                raise self.err("Formatter().vformat(format_string, args, kwargs) takes three positional arguments", node)
            fmt, args, mapping = (self.expr(a) for a in node.args)
            return self.q(f"{RT}::methods::vformat(&{fmt}, &{args}, &{mapping})")
        if f.attr == "format":
            if not node.args or isinstance(node.args[0], ast.Starred):
                raise self.err("Formatter().format(format_string, ...) needs the format string first", node)
            fmt = self.expr(node.args[0])
            rest = ast.Call(func=node.func, args=node.args[1:], keywords=node.keywords)
            args, kwargs = self.dyn_args(rest)
            return self.q(f"{RT}::methods::call_method(cx, &{fmt}, \"format\", {args}, {kwargs}).await")
        if f.attr == "parse" and node in getattr(self, "_eager_calls", ()):
            # consumed whole by list()/tuple(): the items, or the error, at once (CPython's iterator raises on
            # the item that fails: elsewhere, items before it would be seen first)
            if len(node.args) != 1 or node.keywords or isinstance(node.args[0], ast.Starred):
                raise self.err("Formatter().parse(format_string) takes one positional argument", node)
            return self.q(f"{RT}::methods::formatter_parse(&{self.expr(node.args[0])})")
        raise self.err(f"string.Formatter().{f.attr}() is not supported (vformat and format are; parse() inside "
                       "list() or tuple())", node)

    def task_call(self, node: ast.Call) -> str | None:
        """`asyncio.create_task(f(...))`, `asyncio.get_running_loop().create_task(f(...))`: `f` and its
        arguments are evaluated now (creating the coroutine), the call runs on its own.
        `asyncio.run(f(...))`: always inside the running loop here, so CPython's RuntimeError."""
        f = node.func
        ref = self.static_ref(f)
        kind = None
        if isinstance(ref, Ext) and ref.dotted in {"asyncio.create_task", "asyncio.ensure_future"}:
            kind = "task"
        elif isinstance(ref, Ext) and ref.dotted == "asyncio.run":
            kind = "run"
        elif (isinstance(f, ast.Attribute) and f.attr == "create_task" and isinstance(f.value, ast.Call)
              and not f.value.args and isinstance(self.static_ref(f.value.func), Ext)
              and self.static_ref(f.value.func).dotted in {"asyncio.get_running_loop", "asyncio.get_event_loop"}):
            kind = "task"
        if kind is None:
            return None
        if len(node.args) != 1 or any(k.arg != "name" for k in node.keywords) or not isinstance(node.args[0], ast.Call):
            raise self.err(f"{ast.unparse(f)}(coroutine_function(...)) is the only supported form", node)
        coro = node.args[0]
        fv = self.expr(coro.func)
        args, kwargs = self.dyn_args(coro)
        if kind == "run":
            return self.q(f"{{ let _: (V, Vec<V>, Vec<(String, V)>) = ({fv}, {args}, {kwargs}); "
                          f"Err::<V, Exc>(Exc::runtime(\"asyncio.run() cannot be called from a running event loop\")) }}")
        # asyncio's repr of the task in « Task exception was never retrieved »: a module-level coroutine function
        # of the project, its `name=`
        ref = self.static_ref(coro.func)
        d = self.p.ix.definition(ref) if isinstance(ref, Sym) else None
        info = (f"Some(&{RT}::web::TaskCoro {{ qualname: {rs(d.name)}, file: {rs(self.p.rel_file(ref.module))}, "
                f"line: {d.decorator_list[0].lineno if d.decorator_list else d.lineno} }})"
                if isinstance(d, ast.AsyncFunctionDef) else "None")
        name = next((k.value for k in node.keywords if k.arg == "name"), None)
        name = f"Some({self.expr(name)})" if name is not None else "None"
        return self.q(f"{RT}::web::spawn_task_at(cx, {fv}, {args}, {kwargs}, {info}, {name})")

    def run_sync(self, node: ast.Call) -> str:
        """`await conn.run_sync(Base.metadata.create_all)`: the DDL compiled at translation time (py2axum/ddl.py),
        replayed on the connection with SQLAlchemy's checkfirst."""
        a = node.args[0] if len(node.args) == 1 and not node.keywords else None
        if not (isinstance(a, ast.Attribute) and a.attr == "create_all" and isinstance(a.value, ast.Attribute)
                and a.value.attr == "metadata"):
            raise self.err("conn.run_sync(fn): only `run_sync(Base.metadata.create_all)` is supported (its DDL is "
                           "compiled at translation time)", node)
        holder = self.static_ref(a.value.value)
        ddl = self.p.create_all_ddl(holder, a, self.module, self.scope_root())
        recv = self.expr(node.func.value)
        return self.q(f"{RT}::orm::run_create_all(&{recv}, &{ddl}).await")

    SQL_KINDS = {"sqlalchemy.insert": "insert", "sqlalchemy.dialects.postgresql.insert": "insert",
                 "sqlalchemy.update": "update", "sqlalchemy.delete": "delete", "sqlalchemy.select": "select",
                 "sqlalchemy.text": "text", "sqlalchemy.sql.text": "text"}

    def sql_chain(self, node: ast.AST, depth: int = 0) -> set[str]:
        """The methods called on a SQL construct expression (`insert(M).values(...).returning(...)`), through the
        local variables bound to one."""
        out = set()
        while isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            out.add(node.func.attr)
            node = node.func.value
        if isinstance(node, ast.Name) and depth < 3 and self.is_local(node.id) and self.node is not None:
            for a in ast.walk(self.scope_root()):
                if isinstance(a, ast.Assign) and any(isinstance(t, ast.Name) and t.id == node.id for t in a.targets):
                    out |= self.sql_chain(a.value, depth + 1)
        return out

    JOINED_REFUSED = {"sqlalchemy.dialects.postgresql.insert": "insert", "sqlalchemy.orm.aliased": "aliased"}

    def check_joined_construct(self, node: ast.Call) -> None:
        """postgresql `insert(Sub)` (ON CONFLICT) / `aliased(Sub)` of a joined table inheritance subclass: refused."""
        ref = self.static_ref(node.func) if isinstance(node.func, (ast.Name, ast.Attribute)) else None
        what = self.JOINED_REFUSED.get(ref.dotted) if isinstance(ref, Ext) else None
        if what is None or not node.args:
            return
        t = self.p.resolve(self.module, node.args[0]) if isinstance(node.args[0], (ast.Name, ast.Attribute)) else None
        info = self.p.models.get(t) if isinstance(t, Sym) and t in self.p.fe.model_syms else None
        if info is None and isinstance(t, Sym) and t in self.p.fe.model_syms:
            info = self.p.model(t)
        if info is not None and info.joins:
            raise self.err(f"{what}({info.sym.name}) of a joined table inheritance subclass is not supported "
                           "(several tables)", node)

    def sql_kind(self, node: ast.AST, depth: int = 0) -> str | None:
        """The statement a SQL construct expression builds (insert, update, delete, select, text), through its
        method chain (`.where(...).returning(...)`) and the local variables bound to one; None when unknown."""
        while isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            if node.func.attr in ("insert", "update", "delete", "select") and not isinstance(node.func.value, ast.Call):
                # `obj.collection.insert()` (WriteOnlyCollection), `table.insert()`
                if not isinstance(self.static_ref(node.func), Ext):
                    return node.func.attr
            node = node.func.value
        if isinstance(node, ast.Call):
            ref = self.static_ref(node.func)
            return self.SQL_KINDS.get(ref.dotted) if isinstance(ref, Ext) else None
        if isinstance(node, ast.Name) and depth < 3 and self.is_local(node.id) and self.node is not None:
            kinds = {self.sql_kind(a.value, depth + 1) for a in ast.walk(self.scope_root())
                     if isinstance(a, ast.Assign) and any(isinstance(t, ast.Name) and t.id == node.id for t in a.targets)}
            return kinds.pop() if len(kinds) == 1 else None
        return None

    STREAM_RESPONSES = {"StreamingResponse", "EventSourceResponse"}

    def check_sync_gen_call(self, node: ast.Call, fn: ast.AST) -> None:
        """A synchronous generator function called: the runtime runs it as an async generator, which only a
        streamed response consumes like Starlette (`StreamingResponse(gen())`, iterated in a threadpool); a
        synchronous consumer (`list()`, `for`, `next()`, a comprehension...) cannot drive it lazily."""
        if isinstance(fn, ast.FunctionDef) and has_yield(fn) and id(node) not in self.p.__dict__.get("stream_args", set()):
            raise self.err(f"`{ast.unparse(node.func)}()` calls a synchronous generator function: only "
                           "`StreamingResponse(gen())` is supported; build a list (`return [..]`), or make it an "
                           "`async def` generator iterated with `async for`", node)

    def call(self, node: ast.Call, awaited: bool) -> str:
        f = node.func
        if (isinstance(f, ast.Attribute) and f.attr == "create_all" and isinstance(f.value, ast.Attribute)
                and f.value.attr == "metadata" and len(node.args) == 1 and not node.keywords
                and isinstance(self.static_ref(f.value.value), Sym)):
            # `Base.metadata.create_all(engine)` (a synchronous engine): the DDL compiled at translation time
            ddl = self.p.create_all_ddl(self.static_ref(f.value.value), f, self.module, self.scope_root())
            return self.q(f"{RT}::orm::create_all_engine(cx, &{self.expr(node.args[0])}, &{ddl}).await")
        if (isinstance(f, ast.Attribute) and f.attr == "join" and len(node.args) == 1 and not node.keywords
                and isinstance(node.args[0], ast.Call)):
            # str.join(chain(...)): CPython makes a list of the iterable first, so it is consumed whole at once
            self.__dict__.setdefault("_eager_calls", set()).add(node.args[0])
        if (isinstance(f, ast.Attribute) and f.attr == "fromkeys" and isinstance(f.value, ast.Name) and f.value.id == "dict"
                and node.args and isinstance(node.args[0], ast.Call)):
            # dict.fromkeys(chain(...)): the whole iterable is read before the dict is returned
            self.__dict__.setdefault("_eager_calls", set()).add(node.args[0])
        ref = self.static_ref(node.func)
        if isinstance(ref, Ext) and ref.dotted.split(".")[-1] in self.STREAM_RESPONSES:
            content = node.args[0] if node.args else next((k.value for k in node.keywords if k.arg == "content"), None)
            if isinstance(content, ast.Call):
                self.p.__dict__.setdefault("stream_args", set()).add(id(content))
        if isinstance(node.func, ast.Name) and node.func.id in self.__dict__.get("sync_gens", ()):
            self.check_sync_gen_call(node, self.sync_gens[node.func.id])
        if (isinstance(node.func, ast.Attribute) and node.func.attr == "__new__" and isinstance(node.func.value, ast.Name)
                and node.func.value.id == "object" and "object" not in self.locals):
            return self.object_new(node)
        if isinstance(node.func, ast.Attribute):
            self.check_model_table(node.func)
        self.check_joined_construct(node)
        if isinstance(node.func, ast.Attribute) and node.func.attr == "run_sync":
            raise self.err("conn.run_sync(...) without `await` does nothing in Python: not supported", node)
        if isinstance(node.func, ast.Attribute) and node.func.attr in ("execute", "scalars", "scalar") and node.args and (
                len(node.args) >= 2 or any(k.arg == "params" for k in node.keywords)):
            # parameters with an ORM / Core construct: executemany, ORM bulk INSERT / UPDATE by primary key
            params = node.args[1] if len(node.args) >= 2 else next(k.value for k in node.keywords if k.arg == "params")
            kind = self.sql_kind(node.args[0])
            given = not (isinstance(params, ast.Constant) and params.value is None)
            chain = self.sql_chain(node.args[0])
            if given and kind == "insert" and node.func.attr in ("execute", "scalars") and not chain & {
                    "on_conflict_do_update", "on_conflict_do_nothing"}:
                # insert().values(...) with a parameter list: one INSERT per dict, the values and the dict's columns
                chain = set()
            if given and kind == "update" and node.func.attr == "execute" and not chain & {"returning", "ordered_values"}:
                # executemany with criteria / bindparam(): Core, one execution per dict (the runtime raises
                # SQLAlchemy's InvalidRequestError for a list on an ORM entity through the Session)
                chain = set()
            if given and kind in ("insert", "update") and chain & {"where", "filter", "filter_by", "values", "ordered_values",
                                                                    "on_conflict_do_update", "on_conflict_do_nothing"}:
                # executemany of a statement with its own criteria/values (bindparam()): not modelled
                raise self.err(f"{node.func.attr}({kind}(...).{sorted(chain & {'where', 'filter', 'filter_by', 'values', 'ordered_values', 'on_conflict_do_update', 'on_conflict_do_nothing'})[0]}(...), "
                               "parameters) is not supported (ORM bulk INSERT / UPDATE by primary key take the bare "
                               "statement)", node)
            if given and kind == "update" and "returning" in chain:
                raise self.err(f"{node.func.attr}(update(...).returning(...), parameters) is not supported", node)
            if given and node.func.attr != "execute" and kind not in ("insert", "update"):
                raise self.err(f".{node.func.attr}(statement, parameters) is not supported (the statement's own bound "
                               "values only; execute(text(...), parameters) takes parameters)", node)
            if given and kind not in (None, "text", "insert", "update", "delete", "select"):
                raise self.err(f"{node.func.attr}({kind}(...), parameters) is not supported (executemany with a "
                               "parameter list): only text(), insert() and update() statements take parameters", node)
        if isinstance(node.func, ast.Attribute) and node.func.attr == "model_json_schema":
            if node.args or node.keywords:
                raise self.err(".model_json_schema() with arguments is not supported (pydantic's defaults only)", node)
            e = self.p.json_schemas()[1].get((self.module, node.lineno, node.col_offset))
            if e is not None:
                raise e
        if (isinstance(node.func, ast.Name) and node.func.id == "__import__" and not self.is_local("__import__")
                and self.p.resolve(self.module, node.func, self.scope_root()) is None and self.static_ref(node) is None):
            # `__import__(name, fromlist=[...])` of a name computed at run time: modules are resolved at translation,
            # there is no table of them to import from by name; raised when reached (a library's configuration path)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{{ let _ = ({args}, {kwargs}); Err::<V, Exc>(Exc::msg(&{RT}::v::IMPORT_ERROR, "
                          f"\"py2axum: __import__() of a module name computed at run time is not supported\")) }}")
        sup = self.super_init(node)
        if sup is not None:
            return sup
        task = self.task_call(node)
        if task is not None:
            return task
        f = node.func
        if (isinstance(f, ast.Attribute) and f.attr == "__setattr__" and isinstance(f.value, ast.Name) and f.value.id == "object"
                and not self.is_local("object") and self.p.resolve(self.module, f.value, self.scope_root()) is None):
            # object.__setattr__(obj, name, value): the instance's own slot, past a __setattr__ override
            if len(node.args) != 3 or node.keywords:
                raise self.err("object.__setattr__() takes 3 positional arguments", node)
            obj, attr, val = (self.expr(a) for a in node.args)
            return f"{{ {self.q(f'{RT}::methods::setattr_raw(&{obj}, &{RT}::ops::str_(&{attr})?, {val})')}; V::None }}"
        ref = self.static_ref(node.func)
        if ref == Ext("collections.defaultdict"):
            return self.defaultdict_call(node)
        if ref is not None:
            return self.static_call(ref, node, awaited)
        fmt = self.formatter_call(node)
        if fmt is not None:
            return fmt
        if isinstance(node.func, ast.Attribute):
            if instr_call(self.p.fe, self.module, node, ("instrument", "expose")):
                # prometheus_fastapi_instrumentator: `.instrument(app, ...)` / `.expose(app, ...)` (dynrt/instr.rs)
                if len(node.args) != 1 or any(k.arg is None for k in node.keywords):
                    raise self.err(f"Instrumentator.{node.func.attr}(app, ...) takes the application then keyword options", node)
                recv = self.expr(node.func.value)
                kw = ", ".join(f"({rs(k.arg)}.to_string(), {self.expr(k.value)})" for k in node.keywords)
                return self.q(f"{RT}::instr::{node.func.attr}(&{recv}, vec![{kw}])")
            if node.func.attr == "add_middleware" and node.args:
                # `app.add_middleware(Cls, ...)` in a function given the app: the instance is built now (the
                # stack is being built), its `dispatch` registered
                kind = mw_kind(self.p.fe, self.module, node)
                if kind not in {"user", "asgi"} or any(isinstance(a, ast.Starred) for a in node.args) \
                        or any(k.arg is None for k in node.keywords):
                    raise self.err(f"add_middleware({ast.unparse(node.args[0])}) outside the app factory is not supported "
                                   "(only a project BaseHTTPMiddleware subclass or raw ASGI class, arguments written out)", node)
                recv = self.expr(node.func.value)
                if kind == "asgi":
                    self.p.__dict__.setdefault("stack_classes", []).append(self.p.resolve(self.module, node.args[0]))
                    fe_ = self.p.fe
                    fe_.raw_mws = [*fe_.__dict__.get("raw_mws", []), (self.p.resolve(self.module, node.args[0]), node, self.p.src(self.module))]
                    slot = self.tmp("slot")
                    self.emit(f"let {slot} = {RT}::asgi::Slot::new();")
                    obj = asgi_instance(self, node, slot)
                    return self.q(f"{RT}::routing::add_asgi(&{recv}, {obj}, {slot})")
                inst = ast.Call(func=node.args[0], args=[ast.Constant(None)] + node.args[1:], keywords=node.keywords)
                ast.copy_location(inst, node)
                ast.fix_missing_locations(inst)
                obj = self.expr(inst)
                d = self.q(f"{RT}::methods::getattr(cx, &{obj}, \"dispatch\").await")
                return self.q(f"{RT}::routing::add_dispatch(&{recv}, {d})")
            if node.func.attr == "__getattribute__" and len(node.args) == 1 and not node.keywords:
                # `obj.__getattribute__(name)`: the lookup without the class's `__getattr__` fallback
                recv = self.expr(node.func.value)
                name = self.expr(node.args[0])
                return self.q(f"{RT}::methods::getattribute(cx, &{recv}, &{self.q(f'{RT}::ops::str_(&{name})')}).await")
            recv = self.expr(node.func.value)
            self.check_method(node)
            self.p.method_edges(node.func.attr)
            args, kwargs = self.dyn_args(node)
            if not awaited and node.func.attr in self.p.async_method_names():
                return self.q_method(f"{RT}::aio::call_method_lazy(cx, &{recv}, {rs(node.func.attr)}, {args}, {kwargs}).await", node)
            if awaited:
                # a synchronous Session's plain methods awaited by mistake: TypeError, like CPython
                return self.q_method(f"{RT}::aio::call_method_awaited(cx, &{recv}, {rs(node.func.attr)}, {args}, {kwargs}).await", node)
            if node.func.attr in LOG_METHODS:
                # `logger.info(...)`: a logger (or adapter) records its call site; any other object's method runs as usual
                return self.q_method(f"{RT}::logging::call_at(cx, &{recv}, {rs(node.func.attr)}, {args}, {kwargs}, {self.log_loc(node)}).await", node)
            return self.q_method(f"{RT}::methods::call_method(cx, &{recv}, {rs(node.func.attr)}, {args}, {kwargs}).await", node)
        f = self.expr(node.func)
        args, kwargs = self.dyn_args(node)
        if not awaited:
            return self.q(f"{RT}::aio::call_value_lazy(cx, &{f}, {args}, {kwargs}).await")
        return self.q(f"{RT}::methods::call_value(cx, &{f}, {args}, {kwargs}).await")

    # passlib's CryptContext as dynrt/auth.rs translates it: method -> (positional parameters, keyword-only ones)
    PASSLIB_METHODS = {"hash": (("secret",), ()), "verify": (("secret", "hash"), ()),
                       "verify_and_update": (("secret", "hash"), ()), "needs_update": (("hash",), ("secret",)),
                       "identify": (("hash",), ("required",)), "dummy_verify": ((), ())}

    def check_passlib(self, attr: ast.Attribute, call: ast.Call | None = None) -> None:
        """A CryptContext's attribute or method call outside the translated surface: refused here (at run time a
        py2axum error could be caught by the application's `except Exception` and answered differently)."""
        recv = attr.value
        globs, attrs = self.p.crypt_context_names()
        if not globs and not attrs:
            return
        if isinstance(recv, ast.Name) and self.is_local(recv.id):
            return
        if not isinstance(recv, (ast.Name, ast.Attribute)):
            return
        try:
            t = self.p.resolve(self.module, recv)
        except TranspileError:
            t = None
        if not (t in globs or isinstance(recv, ast.Attribute) and recv.attr in attrs):
            return
        spec = self.PASSLIB_METHODS.get(attr.attr)
        if spec is None:
            raise self.err(f"passlib CryptContext.{attr.attr} is not supported (of passlib, hash, verify, verify_and_update, "
                           "needs_update, identify and dummy_verify are translated)", attr)
        if call is None:
            return
        pos, kwonly = spec
        if any(isinstance(a, ast.Starred) for a in call.args) or any(k.arg is None for k in call.keywords):
            raise self.err(f"passlib CryptContext.{attr.attr}(): `*`/`**` arguments are not supported", call)
        if len(call.args) > len(pos):
            raise self.err(f"passlib CryptContext.{attr.attr}() with {len(call.args)} positional arguments is not supported "
                           f"(scheme, category and handler options are not translated)", call)
        for k in call.keywords:
            if k.arg not in pos and k.arg not in kwonly:
                raise self.err(f"passlib CryptContext.{attr.attr}({k.arg}=) is not supported", call)

    def check_httpx_request(self, node: ast.Attribute) -> None:
        """`exc.request.<attr>` of an httpx exception: method and url only (the runtime's Request)"""
        v = node.value
        if (isinstance(v, ast.Attribute) and v.attr == "request" and isinstance(v.value, ast.Name)
                and v.value.id in self.p.httpx_exc_names() and node.attr not in ("method", "url")):
            raise self.err(f"httpx.Request.{node.attr} is not supported (of an exception's request, method and url "
                           "are translated)", node)

    def check_method(self, node: ast.Call) -> None:
        """A method resolved at run time that no runtime type implements (and no project class defines)
        is a certain 500 on that path: refused here, so that --report counts it."""
        name = node.func.attr
        self.check_passlib(node.func, node)
        self.check_httpx_request(node.func)
        self.check_request_use(node.func)
        if (name in CT_WRAPPER_ATTRS and isinstance(node.func.value, ast.Attribute)
                and node.func.value.attr in self.p.cachedmethod_names()):
            raise self.err(f"`.{node.func.value.attr}.{name}` of a @cachedmethod is not supported (only its calls)", node)
        if name in self.p.project_attrs() or name == "__wrapped__":
            return
        recv = node.func.value
        if isinstance(recv, ast.Name) and recv.id in self.__dict__.get("failed_imports", set()):
            return  # unreachable: the import raised
        if name == "columns":
            # `text(...).columns(...)` (a TextualSelect): no runtime value has a columns() method
            raise self.err("method .columns() (text().columns(), a TextualSelect) is not supported", node)
        if name == "form":
            # Starlette's `await request.form()`: no runtime value has it (the name is in the runtime's catalog for
            # other reasons), it would raise AttributeError at run time
            raise self.err(f"`request.{name}()` is not supported: declare Form()/File() parameters, or read "
                           "`await request.body()`", node)
        if name not in runtime_names():
            raise self.err(f"method .{name}() is not implemented by the runtime for any type "
                           "(it would raise AttributeError at run time)", node)
        if name == "strftime" and node.args and isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str):
            self.check_strftime(node.args[0].value, node.args[0], "strftime()")
        allowed = METHOD_KWARGS.get(name)
        if allowed is not None:
            for k in node.keywords:
                if k.arg is not None and k.arg not in allowed:
                    raise self.err(f".{name}({k.arg}=) is not supported (only {', '.join(sorted(allowed))})", k.value)
        most = METHOD_MAX_ARGS.get(name)
        if most is not None and (len(node.args) > most or any(isinstance(a, ast.Starred) for a in node.args)
                                 or any(k.arg is None for k in node.keywords)):
            raise self.err(f".{name}() takes at most {most} positional argument{'s' if most != 1 else ''} here "
                           "(no * or ** unpacking)", node)
        if name in self.CODEC_ERRORS:
            self.check_codec_errors(node, name)

    CODEC_ERRORS = {"decode": {"strict", "ignore", "replace", "backslashreplace"},
                    "read_text": {"strict", "ignore", "replace", "backslashreplace"},
                    "encode": {"strict", "ignore", "replace", "backslashreplace", "xmlcharrefreplace",
                               "surrogateescape", "surrogatepass"}}

    def check_codec_errors(self, node: ast.Call, name: str) -> None:
        """`errors=` of decode / encode / read_text written as a literal: only the handlers the runtime renders as
        CPython does (decode: surrogateescape needs lone surrogates, a Rust string has none; xmlcharrefreplace and
        namereplace raise TypeError for a decode error; a custom handler is registered at run time)."""
        lit = next((k.value for k in node.keywords if k.arg == "errors"), None)
        if lit is None and len(node.args) > 1:
            lit = node.args[1]
        if isinstance(lit, ast.Constant) and isinstance(lit.value, str) and lit.value not in self.CODEC_ERRORS[name]:
            raise self.err(f".{name}(errors=) is not supported (only {', '.join(sorted(self.CODEC_ERRORS[name]))}; "
                           f"got {lit.value!r})", lit)

    def check_strftime(self, fmt: str, node: ast.AST, where: str) -> None:
        """A literal strftime format (or a date's f-string spec) may only use the directives `dynrt::libs::strftime`
        renders as CPython does on both glibc and macOS (`STRFTIME_DIRECTIVES`): flags, widths, E/O modifiers, %s
        (the local time zone) and letters the C library does not know render differently on each platform."""
        i = 0
        while i < len(fmt):
            if fmt[i] != "%" or i + 1 == len(fmt):
                i += 1
                continue
            d = fmt[i + 1]
            if d == ":" and fmt[i + 2:i + 3] == "z":
                i += 3
                continue
            if d not in STRFTIME_DIRECTIVES:
                raise self.err(f"strftime directive %{fmt[i + 1:i + 3] if d in STRFTIME_PREFIXES else d} in {where} is not "
                               f"supported (glibc and macOS render it differently{', it reads the local time zone' if d == 's' else ''}); "
                               f"supported: %{' %'.join(STRFTIME_DIRECTIVES)} and %:z", node)
            i += 2

    def check_model_construct(self, sym: Sym, node: ast.Call) -> None:
        """`Model.model_construct(...)` (dynrt `pyd::model_construct`): what pydantic does differently from the
        runtime is refused here; a required field left out is refused when the keywords are all literal."""
        info = self.p.schema(sym)
        if info.__dict__.get("root"):
            raise self.err(f"{sym.name}.model_construct(): a RootModel is not supported", node)
        if info.__dict__.get("post_init") or any(spec.startswith("Dyn(") for _, spec in info.__dict__.get("private", [])):
            raise self.err(f"{sym.name}.model_construct(): model_post_init is not supported, nor a private "
                           "attribute with a computed default", node)
        for f in info.fields:
            al = f.get("aliases") or {}
            if "alias" in al and al.get("validation") != al["alias"]:
                raise self.err(f"{sym.name}.model_construct(): field `{f['name']}` has both alias= and validation_alias= "
                               "(model_construct takes either: not supported)", node)
        if len(node.args) > 1 or any(isinstance(a, ast.Starred) for a in node.args):
            raise self.err("model_construct() takes `_fields_set` as its only positional argument here", node)
        if any(k.arg is None for k in node.keywords):
            return
        given = {k.arg for k in node.keywords}
        for f in info.fields:
            al = f.get("aliases") or {}
            v = al.get("validation")
            keys = {f["name"]} | ({v} if isinstance(v, str) else set()) | ({p[0] for p in v if len(p) == 1 and isinstance(p[0], str)}
                                                                             if isinstance(v, list) else set())
            if f["default"] == "Required" and not (keys & given) and not (isinstance(v, list) and any(len(p) > 1 for p in v)):
                raise self.err(f"{sym.name}.model_construct() without the required field `{f['name']}`: pydantic leaves "
                               "it unset (reading it raises AttributeError), the runtime does not model that", node)

    def static_call(self, ref, node: ast.Call, awaited: bool) -> str:
        if isinstance(ref, tuple) and ref[0] == "builtin":
            return self.builtin_call(ref[1], node)
        if isinstance(ref, tuple) and ref[0] == "classattr":
            _, sym, attr = ref
            owner, seen = sym, set()
            while owner is not None and owner not in seen:
                # along the bases of a plain class (one project base each): `Sub.make()` of a classmethod of
                # `Base` gets `Sub` as `cls`
                seen.add(owner)
                d = self.p.ix.definition(owner)
                stmt = next((s for s in d.body if isinstance(s, (ast.FunctionDef, ast.AsyncFunctionDef)) and s.name == attr), None)
                if stmt is not None:
                    decos = {dotted(x) for x in stmt.decorator_list}
                    msym = Sym(owner.module, f"{owner.name}.{attr}")
                    if "staticmethod" in decos:
                        return self.project_call(msym, stmt, node, awaited, prefix=[])
                    if "classmethod" in decos:
                        return self.project_call(msym, stmt, node, awaited, prefix=[self.class_value(sym, node)])
                    break
                if any(isinstance(s, (ast.Assign, ast.AnnAssign)) and attr in {getattr(t, "id", None) for t in (
                        s.targets if isinstance(s, ast.Assign) else [s.target])} for s in d.body):
                    break
                plain = (owner not in self.p.fe.schema_syms and owner not in self.p.fe.model_syms
                         and owner not in self.p.fe.dataclass_syms and self.p.is_plain_class(owner))
                bases = self.p.plain_bases(owner) if plain else None
                owner = bases[0] if bases else None
            if sym not in self.p.fe.schema_syms and sym not in self.p.fe.model_syms and sym not in self.p.fe.dataclass_syms \
                    and self.p.is_plain_class(sym):
                cattrs = self.p.plain_class(sym).__dict__.get("class_attrs", {})
                if attr in cattrs:
                    # `Cls.attr(...)`: the value a class attribute holds (`staticmethod(f)`: f), called
                    args, kwargs = self.dyn_args(node)
                    return self.q(f"{RT}::methods::call_value(cx, &{self.q(f'{cattrs[attr]}(cx).await')}, {args}, {kwargs}).await")
            self.check_async_class_method(sym, attr, node)
            recv = self.class_value(sym, node)
            self.check_method(node)
            if attr == "model_construct" and sym in self.p.fe.schema_syms:
                self.check_model_construct(sym, node)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::methods::call_method(cx, &{recv}, {rs(attr)}, {args}, {kwargs}).await")
        if isinstance(ref, Sym):
            d = self.p.ix.definition(ref)
            if isinstance(d, (ast.FunctionDef, ast.AsyncFunctionDef)):
                return self.project_call(ref, d, node, awaited, prefix=[])
            if isinstance(d, ast.ClassDef):
                return self.construct(ref, d, node)
            if isinstance(d, (ast.Assign, ast.AnnAssign, ast.If, ast.Try, ast.For, ast.While, ast.With)):
                # a module variable, or a name a module-level block binds (`if PY2: def f(...)` / `else: def f(...)`):
                # whatever value it holds at run time, called
                f = self.ref_value(ref, node)
                args, kwargs = self.dyn_args(node)
                return self.q(f"{RT}::methods::call_value(cx, &{f}, {args}, {kwargs}).await")
            raise self.err(f"`{ref.qual}` is not callable", node)
        if isinstance(ref, Ext):
            return self.ext_call(ref, node)
        if isinstance(ref, tuple) and ref[0] == "missing":
            raise self.err(f"module {ref[1]} has no attribute `{ref[2]}`", node)
        raise self.err(f"unsupported call `{ast.unparse(node.func)}`", node)

    @staticmethod
    def binds_statically(params, n_prefix: int, node: ast.Call) -> bool:
        """Would this call bind (names only, nothing evaluated)? Otherwise CPython raises a TypeError."""
        npos = n_prefix + len(node.args)
        kw = {k.arg for k in node.keywords}
        slots = [p for p in params if p.kind == "positional"]
        if npos > len(slots) and not any(p.kind == "vararg" for p in params):
            return False
        for i, p in enumerate(slots):
            if i < npos and p.name in kw:
                return False
            if i >= npos and p.name not in kw and p.default is None:
                return False
        for p in params:
            if p.kind == "kwonly" and p.name not in kw and p.default is None:
                return False
        names = {p.name for p in params if p.kind in {"positional", "kwonly"}}
        return all(k in names for k in kw) or any(p.kind == "kwarg" for p in params)

    def project_call(self, sym: Sym, d, node: ast.Call, awaited: bool, prefix: list[str]) -> str:
        # not awaited: a coroutine object (arguments evaluated now, the body run when awaited or as a task)
        lazy = isinstance(d, ast.AsyncFunctionDef) and not awaited and not has_yield(d)
        try:
            vdecos = value_decorators(d, self.deco_resolver(sym))
        except TranspileError as e:
            raise self.err(e.msg, e.node or d) from None
        cm = cachedmethod_deco(self.p, sym, d)
        if cm is not None:
            if not prefix:
                raise self.err(f"the @cachedmethod {sym.name} called through its class is not supported", node)
            if not awaited and isinstance(d, ast.AsyncFunctionDef):
                raise self.err(f"the @cachedmethod {sym.name} not awaited is not supported", node)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{method_wrapper(self.p, sym)}(cx, {prefix[0]}.clone(), {RT}::pack({args}, {kwargs})).await")
        if sym in self.p.__dict__.get("vdeco_methods", ()):
            if not prefix:
                raise self.err(f"decorated {sym.name} called with a receiver is not supported: a decorated method called through its class", node)
            args, kwargs = self.dyn_args(node)
            call = self.q(f"{method_wrapper(self.p, sym)}(cx, {prefix[0]}.clone(), {RT}::pack({args}, {kwargs})).await")
            return call
        if vdecos:
            if "." in sym.name:
                raise self.err(f"decorator @{ast.unparse(vdecos[0])} on method {sym.name} is not supported", vdecos[0])
            g = self.q(f"{self.p.decorated_value(sym)}(cx).await")
            if prefix:
                raise self.err(f"decorated {sym.name} called with a receiver is not supported", node)
            args, kwargs = self.dyn_args(node)
            if not awaited:
                return self.q(f"{RT}::aio::call_value_lazy(cx, &{g}, {args}, {kwargs}).await")
            return self.q(f"{RT}::methods::call_value(cx, &{g}, {args}, {kwargs}).await")
        decos = [(dotted(x.func) if isinstance(x, ast.Call) else dotted(x)) or "" for x in d.decorator_list]
        params = fn_params(d)
        spread = any(isinstance(a, ast.Starred) for a in node.args) or any(k.arg is None for k in node.keywords)
        cached = "lru_cache" in {x.split(".")[-1] for x in decos} or "cache" in {x.split(".")[-1] for x in decos}
        if (spread or not self.binds_statically(params, len(prefix), node)) and not has_yield(d) and not cached:
            # bound at run time like CPython: *args/**kwargs, or the TypeError a bad call raises
            fw = function_wrapper(self.p, sym)
            if any(isinstance(a, ast.Starred) for a in node.args):
                parts = [f"(false, {x})" for x in prefix] + [
                    f"({'true' if isinstance(a, ast.Starred) else 'false'}, {self.expr(a.value if isinstance(a, ast.Starred) else a)})"
                    for a in node.args]
                args = self.q(f"{RT}::args(vec![{', '.join(parts)}])")
            else:
                args = "vec![" + ", ".join(prefix + [self.expr(a) for a in node.args]) + "]"
            fixed = ", ".join(f"({rs(k.arg)}.to_string(), {self.expr(k.value)})" for k in node.keywords if k.arg is not None)
            spreads = [self.expr(k.value) for k in node.keywords if k.arg is None]
            kwargs = self.q(f"{RT}::kwargs(vec![{fixed}], vec![{', '.join(spreads)}])") if spreads else f"vec![{fixed}]"
            if lazy:
                binds, (a, k) = self.lazy_bind([args, kwargs])
                return self.coro(binds, f"{fw}(cx, V::None, {RT}::pack({a}, {k}))")
            return self.q(f"{fw}(cx, V::None, {RT}::pack({args}, {kwargs})).await")
        if spread:
            raise self.err(f"`*`/`**` expansion in a call to {sym.name}() is not supported", node)
        pos = prefix + [self.expr(a) for a in node.args]
        kw = {k.arg: self.expr(k.value) for k in node.keywords}
        args: list[str] = []
        slots = [p for p in params if p.kind == "positional"]
        extra_pos = pos[len(slots):]
        for i, p in enumerate(params):
            if p.kind == "positional":
                if i < len(pos):
                    if p.name in kw:
                        raise self.err(f"{sym.name}() got multiple values for argument '{p.name}'", node)
                    args.append(pos[i])
                elif p.name in kw:
                    args.append(kw.pop(p.name))
                elif p.default is not None:
                    args.append(self.default_arg(sym, p, d))
                else:
                    raise self.err(f"{sym.name}() missing required argument '{p.name}'", node)
            elif p.kind == "vararg":
                args.append(f"V::tuple(vec![{', '.join(extra_pos)}])")
                extra_pos = []
            elif p.kind == "kwonly":
                if p.name in kw:
                    args.append(kw.pop(p.name))
                elif p.default is not None:
                    args.append(self.default_arg(sym, p, d))
                else:
                    raise self.err(f"{sym.name}() missing required keyword argument '{p.name}'", node)
            elif p.kind == "kwarg":
                items = ", ".join(f"(V::str({rs(k)}), {v})" for k, v in kw.items())
                args.append(self.q(f"V::dict_from(vec![{items}])"))
                kw = {}
        if extra_pos:
            raise self.err(f"{sym.name}() takes {len(slots)} positional arguments", node)
        if kw:
            raise self.err(f"{sym.name}() got an unexpected keyword argument '{next(iter(kw))}'", node)
        if "lru_cache" in {x.split(".")[-1] for x in decos} or "cache" in {x.split(".")[-1] for x in decos}:
            if params:
                raise self.err("@lru_cache is only supported on functions without parameters", d)
            if lazy:
                raise self.err("@lru_cache on an async function called without await is not supported", node)
            rust = self.p.function(sym, "cached")
            return self.q(f"{rust}(cx).await")
        rust = self.p.function(sym)
        if has_yield(d):
            self.check_sync_gen_call(node, d)
            caps = [self.tmp("g") for _ in args]
            binds = " ".join(f"let {c} = {a};" for c, a in zip(caps, args))
            return (f"{{ {binds} let cx2 = cx.clone(); {RT}::web::spawn_gen(move |y| Box::pin(async move {{ "
                    f"{rust}(&cx2{''.join(', ' + c for c in caps)}, &y).await }})) }}")
        if lazy:
            binds, bound = self.lazy_bind(args)
            return self.coro(binds, f"{rust}(cx{''.join(', ' + a for a in bound)})")
        call = f"{rust}(cx{''.join(', ' + a for a in args)})"
        if self.p.cur is not None and self.p.cur[0] == sym:
            call = f"Box::pin({call})"  # a recursive async fn needs its future boxed
        return self.q(f"{call}.await")

    def lazy_bind(self, exprs: list[str]) -> tuple[str, list[str]]:
        """The arguments evaluated now (into temporaries) for a call deferred into a coroutine."""
        names = [self.tmp("ca") for _ in exprs]
        return "".join(f"let {n} = {e}; " for n, e in zip(names, exprs)), names

    def coro(self, binds: str, call: str) -> str:
        """A coroutine object running `call` (a future using `cx` and the owned temporaries) when awaited."""
        return (f"{{ {binds}let cx2 = cx.clone(); {RT}::aio::coro(Box::pin(async move {{ let cx = &cx2; {call}.await }})) }}")

    def default_arg(self, sym: Sym, p: ParamSpec, d) -> str:
        """Default value of a parameter, evaluated in the callee's module (module constants...)."""
        sub = FnCompiler(self.p, sym.module, None, "default")
        sub.sinks = list(self.sinks)
        return sub.expr(p.default)

    def construct(self, sym: Sym, d: ast.ClassDef, node: ast.Call) -> str:
        args, kwargs = self.dyn_args(node)
        if sym in self.p.fe.model_syms:
            info = self.p.model(sym)
            if any(n == "__init__" for n, _, _ in info.methods):
                # the class's own constructor, on a new (transient) object
                return self.q(f"{RT}::orm::construct_init(cx, &{info.rust}, {args}, {kwargs}).await")
            if node.args:
                raise self.err(f"{sym.name}(): mapped classes take keyword arguments only", node)
            return self.q(f"{RT}::orm::construct(&{info.rust}, {kwargs})")
        if sym in self.p.fe.schema_syms:
            info = self.p.schema(sym)
            if node.args:
                raise self.err(f"{sym.name}(): Pydantic models take keyword arguments only", node)
            if info.settings:
                for kw in node.keywords:
                    if kw.arg and kw.arg.startswith("_"):
                        raise self.err(f"{sym.name}({kw.arg}=): pydantic-settings init options are not supported "
                                       "(set them in model_config)", kw)
                none = f"Some({rs(info.env_none)})" if info.env_none is not None else "None"
                return self.q(f"{RT}::settings(cx, &{info.rust}, {rs(info.env_prefix)}, "
                              f"{'true' if info.case_sensitive else 'false'}, {none}, "
                              f"{'true' if info.__dict__.get('env_ignore_empty') else 'false'}, {kwargs}).await")
            if info.__dict__.get("init") is not None:
                mw = method_wrapper(self.p, info.__dict__["init"])
                return self.q(f"{{ let __o = {RT}::pyd::object_new_blank(&{info.rust}); "
                              f"{mw}(cx, __o.clone(), {RT}::pack(vec![], {kwargs})).await.map(|_| __o) }}")
            return self.q(f"{RT}::pyd::construct(cx, &{info.rust}, {RT}::kwargs_dict({kwargs})?).await")
        if self.p.is_typeddict(sym):
            # CPython: `TD(...)` is `dict(...)` (keys neither checked nor completed)
            return self.expr(ast.copy_location(ast.Call(ast.Name("dict", ast.Load()), node.args, node.keywords), node))
        if sym in self.p.fe.dataclass_syms:
            info = self.p.dataclass(sym)
            if info.__dict__.get("pydantic_dc"):
                raise self.err(f"calling the pydantic dataclass {sym.name}(...) is not supported (it validates its arguments); "
                               "build it from the request or a response model", node)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::pyd::dataclass_new(cx, &{info.rust}, {args}, {kwargs}).await")
        if self.p.enum_kind(sym):
            if len(node.args) != 1 or node.keywords:
                raise self.err(f"{sym.name}(value) takes exactly one argument", node)
            return self.q(f"{RT}::methods::enum_call(cx, &{self.p.enum(sym)[0]}, &{self.expr(node.args[0])}).await")
        if self.p.is_plain_class(sym):
            info = self.p.plain_class(sym)
            if info.__dict__.get("abstract"):
                ab = info.__dict__["abstract"]
                msg = (f"Can't instantiate abstract class {sym.name} without an implementation for abstract "
                       f"method{'s' if len(ab) > 1 else ''} {', '.join(repr(a) for a in ab)}")
                for a in [*node.args, *(k.value for k in node.keywords)]:
                    self.expr(a)
                return self.q(f"Err::<V, Exc>(Exc::type_error({rs(msg)}))")
            init = info.__dict__.get("init")
            if init is None and self.p.request_subclass(sym):
                args, kwargs = self.dyn_args(node)
                return self.q(f"{RT}::web::request_subclass(cx, &{info.rust}, {args}, {kwargs}).await")
            if info.__dict__.get("new") is not None:
                args, kwargs = self.dyn_args(node)
                return self.q(f"{RT}::methods::plain_new(cx, &{info.rust}, {args}, {kwargs}).await")
            if init is None and self.p.is_http_middleware(sym):
                # BaseHTTPMiddleware.__init__(app, dispatch=None) inherited: the stack calls `dispatch` itself
                if len(node.args) + len(node.keywords) != 1 or any(k.arg != "app" for k in node.keywords):
                    raise self.err(f"{sym.name}(app): only the application argument is supported "
                                   "(BaseHTTPMiddleware's dispatch= is not)", node)
                self.expr(node.args[0] if node.args else node.keywords[0].value)
                return f"{RT}::pyd::object_new(&{info.rust})"
            if init is None:
                if node.args or node.keywords:
                    raise self.err(f"{sym.name}() takes no arguments", node)
                return f"{RT}::pyd::object_new(&{info.rust})"
            mw = method_wrapper(self.p, init)
            return self.q(f"{{ let __o = {RT}::pyd::object_new(&{info.rust}); "
                          f"{mw}(cx, __o.clone(), {RT}::pack({args}, {kwargs})).await.map(|_| __o) }}")
        cls = self.p.exception(sym)
        init = self.p.exc_init(sym)
        if init is not None:
            mw = method_wrapper(self.p, init)
            return self.q(f"{{ let __a = {args}; let __e = V::Exc(Exc::new(&{cls}, __a.clone())); "
                          f"{mw}(cx, __e.clone(), {RT}::pack(__a, {kwargs})).await.map(|_| __e) }}")
        return f"V::Exc(Exc::new(&{cls}, {args}))"

    def dist_version(self, node: ast.Call) -> str:
        """`importlib.metadata.version("name")`: what `uv sync --frozen` installs, read at compile time: the
        project itself (its pyproject.toml) or a package of its uv.lock; any other name is not installed."""
        if len(node.args) != 1 or node.keywords or not (isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str)):
            raise self.err("importlib.metadata.version() needs one literal distribution name", node)
        name = node.args[0].value
        found = project_distribution(self.p.fe.index.root)
        libs = self.p.ix.libs
        if found is None and libs is not None:
            # --translate-libs: the distributions installed in the project's environment (what the binary carries)
            dist = libs.dists.get(re.sub(r"[-_.]+", "-", name).lower())
            if dist is not None:
                return f"V::str({rs(dist.version)})"
            return self.q(f"Err(Exc::new(&{RT}::v::PACKAGE_NOT_FOUND, vec![V::str({rs(name)})]))")
        if found is None:
            raise self.err("importlib.metadata.version(): no pyproject.toml with a uv.lock next to it under --root "
                           "(the installed distributions cannot be known)", node)
        norm = lambda n: re.sub(r"[-_.]+", "-", n).lower()  # noqa: E731
        project, lock = found
        if norm(name) == norm(project[0]):
            return f"V::str({rs(project[1])})"
        for pkg, ver in lock:
            if norm(pkg) == norm(name):
                return f"V::str({rs(ver)})"
        return self.q(f"Err(Exc::new(&{RT}::v::PACKAGE_NOT_FOUND, vec![V::str({rs(name)})]))")

    def ct_markers(self, name: str, node: ast.Call) -> ast.Call:
        """cachetools' timer=, getsizeof= and key= arguments: the library functions the runtime reproduces passed as
        markers (time.monotonic, time.time, len, keys.hashkey); anything else refused here, condition= too."""
        node = ast.Call(func=node.func, args=list(node.args), keywords=[ast.keyword(arg=k.arg, value=k.value) for k in node.keywords])
        ast.copy_location(node, node.func)
        for pos, kwname, kind in libmap.CT_MARKED[name]:
            if pos < len(node.args):
                expr, put = node.args[pos], (lambda v, pos=pos: node.args.__setitem__(pos, v))
            else:
                k = next((k for k in node.keywords if k.arg == kwname), None)
                if k is None:
                    continue
                expr, put = k.value, (lambda v, k=k: setattr(k, "value", v))
            if isinstance(expr, ast.Constant) and expr.value is None:
                continue
            if kind == "info":
                if isinstance(expr, ast.Constant) and expr.value is False:
                    continue
                raise self.err(f"{name}(info=...) is not supported (the per-instance hit counters are not reproduced)", expr)
            if kind == "typed":
                if isinstance(expr, ast.Constant) and expr.value is False:
                    continue
                raise self.err(f"{name}(typed=...) is not supported (keys.typedkey is not reproduced)", expr)
            ref = self.static_ref(expr)
            marker = None
            if kind == "timer" and ref in (Ext("time.monotonic"), Ext("time.time")):
                marker = "\x00timer:" + ref.dotted.split(".")[-1]
            elif kind == "getsizeof" and ref == ("builtin", "len"):
                marker = "\x00getsizeof:len"
            elif kind == "key" and ref == Ext("cachetools.keys.hashkey"):
                marker = "\x00key:hashkey"
            elif kind == "key" and ref == Ext("cachetools.keys.methodkey") and name == "cachetools.cachedmethod":
                marker = "\x00key:methodkey"
            elif kind == "key":
                if isinstance(ref, Ext) and ref.dotted != "cachetools.keys.methodkey":
                    raise self.err(f"{name}(key=...) is not supported with {ref.dotted} (keys.hashkey, keys.methodkey or a "
                                   "function of the project)", expr)
                continue
            if marker is None:
                what = {"timer": "time.monotonic or time.time", "getsizeof": "len", "condition": "no condition"}[kind]
                raise self.err(f"{name}({kwname}=...) is not supported ({what} only)", expr)
            put(ast.copy_location(ast.Constant(value=marker), expr))
        return node

    def lib_fn_value(self, name: str, node) -> str:
        """A library function used as a value (`run_in_executor(None, threading.get_ident)`): a function object
        calling its translation with 0 to 3 positional arguments."""
        tmpl = libmap.CALLS[name]
        if name in libmap.DYN_ARGS and not name.startswith("operator."):
            # the template takes the run-time vectors: a function object taking keyword arguments too
            code = libmap.DYN_ARGS[name]("args", "kwargs")
            mod, _, qual = name.rpartition(".")
            return (f"{RT}::pyfn({rs(mod)}, {rs(qual)}, None, false, std::sync::Arc::new(|cx: &Cx, args: Vec<V>, "
                    f"kwargs: Vec<(String, V)>| -> {RT}::BoxFut<'_> {{ Box::pin(async move {{ let _ = (cx, &args, &kwargs); {code} }}) }}))")
        arms = []
        for n in range(4):
            if name in libmap.DYN_ARGS:
                code = libmap.DYN_ARGS[name]("args.clone()", "Vec::<(String, V)>::new()")
            else:
                tpos, tkw = _Used([f"args[{i}].clone()" for i in range(n)]), _UsedKw({})
                try:
                    code = tmpl(tpos, tkw)
                except (ValueError, KeyError, IndexError, TypeError):
                    continue
                if not tpos.all_read and n > len(tpos.read):
                    continue
            arms.append(f"{n} => {{ {code} }}")
        if not arms:
            raise self.err(f"library function `{name}` cannot be used as a value", node)
        body = (f"match args.len() {{ {', '.join(arms)}, n => Err(Exc::type_error(format!("
                f"\"py2axum: {name}() with {{n}} arguments as a function value\"))) }}")
        closure = (f"std::sync::Arc::new(|cx: &Cx, args: Vec<V>| -> {RT}::BoxFut<'_> "
                   f"{{ Box::pin(async move {{ let _ = cx; {body} }}) }})")
        if name.startswith("operator."):
            # a builtin of CPython's C module: `str(operator.lt)` is `<built-in function lt>`
            return f"V::native({RT}::Native::Builtin({rs(name.rpartition('.')[2])}, {closure}))"
        return f"{RT}::func({closure})"

    def type_adapter(self, node: ast.Call) -> str:
        """`TypeAdapter(T)` with `T` a type written in the source (the validator built at compile time)."""
        if len(node.args) != 1 or node.keywords:
            raise self.err("only TypeAdapter(type) is supported", node)
        t = node.args[0]
        if not (isinstance(t, ast.Name) and self.is_local(t.id)):
            try:
                td = self.p.td(t, self.module, None, self.scope_root())
                return f"V::native({RT}::Native::Adapter(&{td}, {rs(ast.unparse(t))}))"
            except TranspileError as e:
                if isinstance(t, ast.Subscript):  # a type written in the source: its own refusal is the actionable one
                    raise e
        # a type known at run time (a parameter, `hints["return"]`...)
        tv = self.expr(t)
        return f"V::native({RT}::Native::Adapter({self.q(f'{RT}::types::td_of(&{tv})')}, \"\"))"

    def object_value(self, dotted: str, values: bool = True) -> str | None:
        """`REGISTRY._names_to_collectors`, `Match.FULL.value`: attributes of a library object
        (`libmap.OBJECT_VALUES`) or of a library enum member, read at run time."""
        base, attrs = dotted, []
        while base and base not in libmap.OBJECT_VALUES:
            base, _, a = base.rpartition(".")
            attrs.insert(0, a)
            if values and libmap.VALUES.get(base, "").startswith("V::Enum("):  # `Match.FULL.value`
                break
        if not base:
            return None
        code = libmap.VALUES[base]
        for a in attrs:
            code = self.q(f"{RT}::methods::getattr(cx, &{code}, {rs(a)}).await")
        return code

    def ext_call(self, ref: Ext, node: ast.Call) -> str:
        name = ref.dotted
        self.p.note_copy(name)
        self.note_native(name, node)
        # generic namespaces: func.count(...)
        for ns, tmpl in libmap.NAMESPACE_CALLS.items():
            if name.startswith(ns + "."):
                pos, kw, spreads = self.args_kwargs(node)
                if spreads or kw:
                    raise self.err(f"unsupported arguments to {name}()", node)
                return self.q(tmpl(name[len(ns) + 1:], pos, kw))
        if name in libmap.EXCEPTIONS and name not in libmap.CALLS:
            args, _ = self.dyn_args(node)
            return f"V::Exc(Exc::new(&{RT}::v::{libmap.EXCEPTIONS[name]}, {args}))"
        if name == "pydantic.TypeAdapter":
            return self.type_adapter(node)
        if name == "importlib.metadata.version":
            return self.dist_version(node)
        if name.startswith("sentry_sdk."):
            return self.sentry_call(name, node)
        if name == "loguru.logger" or name.startswith("loguru."):
            return self.loguru_call(name, node)
        if name in ("warnings.warn", "warnings.simplefilter", "warnings.filterwarnings", "warnings.catch_warnings"):
            return self.warnings_call(name, node)
        if name.startswith("logging.") and name[8:] in LOG_METHODS:
            # logging.info(...): the root logger, with the call site (the record's pathname, lineno, funcName)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::logging::module_log(cx, {rs(name[8:])}, {args}, {kwargs}, {self.log_loc(node)}).await")
        if name in ("sqlalchemy.bindparam", "sqlalchemy.sql.expression.bindparam") and any(k.arg == "type_" for k in node.keywords):
            # bindparam(key, type_=ARRAY(String())): a parameter bound as VARCHAR[] (no other type)
            tk = next(k for k in node.keywords if k.arg == "type_")
            t = tk.value
            ok = isinstance(t, ast.Call) and len(t.args) == 1 and not t.keywords
            if ok:
                arr = self.static_ref(t.func)
                item = t.args[0]
                if isinstance(item, ast.Call) and not item.args and not item.keywords:
                    item = item.func
                it = self.static_ref(item) if isinstance(item, (ast.Name, ast.Attribute)) else None
                ok = (isinstance(arr, Ext) and arr.dotted in {"sqlalchemy.ARRAY", "sqlalchemy.types.ARRAY", "sqlalchemy.dialects.postgresql.ARRAY"}
                      and isinstance(it, Ext) and it.dotted.split(".")[-1] in {"String", "Text", "Unicode", "VARCHAR", "TEXT"})
            if not ok:
                raise self.err("bindparam(type_=) is supported with ARRAY(String()) only", tk.value)
            rest = ast.copy_location(ast.Call(func=node.func, args=node.args, keywords=[k for k in node.keywords if k is not tk]), node)
            return self.q(f"{RT}::orm::str_array_param(&{self.ext_call(ref, rest)})")
        if name in ("itertools.chain", "itertools.chain.from_iterable"):
            # the items of each iterable, in order: lazily in CPython, so only where they are consumed whole
            if node not in getattr(self, "_eager_calls", ()):
                raise self.err(f"{name}() is supported consumed whole only (list(), tuple(), set(), sorted(), str.join(), dict.fromkeys())", node)
            if node.keywords:
                raise self.err(f"{name}() takes no keyword arguments", node)
            for a in node.args:  # chain(map(...), chain.from_iterable(...)): consumed whole too
                if isinstance(a, ast.Call):
                    self.__dict__.setdefault("_eager_calls", set()).add(a)
            if name.endswith("from_iterable"):
                if len(node.args) != 1 or isinstance(node.args[0], ast.Starred):
                    raise self.err("chain.from_iterable() takes exactly one argument", node)
                return self.q(f"{RT}::methods::chain(&{self.expr(node.args[0])})")
            args, _ = self.dyn_args(node)
            return self.q(f"{RT}::methods::chain(&V::list({args}))")
        if name.startswith("builtins.") and name[9:] in BUILTIN_FUNCS and name not in libmap.CALLS:
            # `from builtins import list` (or `builtins.len(x)`): the builtin itself
            return self.builtin_call(name[9:], node)
        if name in ("secrets.choice", "random.choice") and node.args and isinstance(node.args[0], ast.GeneratorExp):
            # Random.choice takes len(seq): a TypeError for a generator, which the runtime evaluates as a list
            raise self.err(f"library call `{name}()` is not supported on a generator expression (CPython raises TypeError: "
                           "object of type 'generator' has no len()); pass a list", node.args[0])
        tmpl = libmap.CALLS.get(name)
        recv, _, meth = name.rpartition(".")
        if recv in STARLETTE_CONTEXT:
            return self.starlette_context_call(meth, node)
        if tmpl is None and self.object_value(recv, values=False) is not None:
            # a method of a library object (`REGISTRY.register(c)`): dispatched at run time like any value's
            obj = self.object_value(recv, values=False)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::methods::call_method(cx, &{obj}, {rs(meth)}, {args}, {kwargs}).await")
        if tmpl is None and name.split(".")[0] == "pgpy":
            raise self.lib_err(f"library call `{name}()` is not supported: of pgpy, only PGPKey.from_blob() is "
                               "reproduced (dynrt/pgpy.rs)", name, node)
        if recv == "sys.modules" and isinstance(node.func, ast.Attribute):
            self.check_sys_modules(node.func.value)
        if tmpl is None and recv in libmap.VALUES and not libmap.VALUES[recv].startswith("V::Enum("):
            # a method of a library constant (`datetime.min.time()`)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::methods::call_method(cx, &{libmap.VALUES[recv]}, {rs(meth)}, {args}, {kwargs}).await")
        if tmpl is None:
            raise self.lib_err(f"library call `{name}()` is not supported (not in the py2axum library map)", name, node)
        if name.startswith("jwt."):
            try:
                libmap.pyjwt_static(name, node)
            except ValueError as e:
                raise self.err(f"{name}(): {e}", node) from None
        if name.startswith("qrcode.") and qrcode_without_pillow(self.p.ix.root):
            raise self.lib_err(f"library call `{name}()` is not supported: the project's uv.lock has qrcode without "
                               "Pillow, so qrcode draws with PyPNGImage, whose PNG the runtime does not reproduce "
                               "(dynrt/qr.rs writes Pillow's)", name, node)
        if name in libmap.REDIS_CLIENTS or name in libmap.REDIS_FROM_URL or name in libmap.REDIS_POOL_FROM_URL:
            try:
                libmap.redis_static(name, node)
            except ValueError as e:
                raise self.err(f"{name}(): {e}", node) from None
        if name.startswith("croniter."):
            try:
                libmap.croniter_static(name, node)
            except ValueError as e:
                raise self.err(f"{name}(): {e}", node) from None
        if name.startswith("passlib."):
            try:
                libmap.passlib_static(name, node)
            except ValueError as e:
                raise self.err(f"{name}(): {e}", node) from None
        if name.startswith("dateutil.parser"):
            try:
                libmap.du_parse_static(name, node)
            except ValueError as e:
                raise self.err(f"{name}(): {e}", node) from None
        if name in libmap.CT_MARKED:
            node = self.ct_markers(name, node)
        for k in node.keywords:
            if k.arg in libmap.REFUSED_KWARGS.get(name, ()):
                raise self.err(f"{name}({k.arg}=) is not supported", node)
        if name in {"sqlalchemy.and_", "sqlalchemy.or_"} and any(isinstance(a, ast.Starred) for a in node.args) and not node.keywords:
            # and_(*conditions): the list is expanded at run time
            args, _ = self.dyn_args(node)
            op = "AND" if name.endswith("and_") else "OR"
            return self.q(f"{RT}::orm::and_or(\"{op}\", {args})")
        if name == "sqlalchemy.select" and any(isinstance(a, ast.Starred) for a in node.args) and not node.keywords:
            args, _ = self.dyn_args(node)
            return self.q(f"{RT}::orm::select({args})")
        if name.startswith("builtins.") and name[9:] in BUILTIN_FUNCS:
            return self.builtin_call(name[9:], node)
        if name in libmap.DYN_ARGS:
            # the template takes the run-time (args, kwargs) vectors: `*`/`**` expansions allowed
            args, kwargs = self.dyn_args(node)
            return self.q(libmap.DYN_ARGS[name](args, kwargs))
        pos, kw, spreads = self.args_kwargs(node)
        if spreads:
            raise self.err(f"`*`/`**` expansion in {name}() is not supported", node)
        tpos, tkw = _Used(pos), _UsedKw(kw)
        try:
            code = tmpl(tpos, tkw)
        except (ValueError, KeyError, IndexError) as e:
            raise self.err(f"{name}(): {e}", node) from None
        # an argument the template did not read would be silently dropped: refuse it
        unread = [k for k in kw if k not in tkw.read]
        if unread:
            raise self.err(f"{name}({unread[0]}=) is not supported", node)
        if not tpos.all_read and len(pos) > len(tpos.read):
            raise self.err(f"{name}(): {len(pos)} positional arguments, only {len(tpos.read)} supported", node)
        return self.q(code)

    def loguru_call(self, name: str, node: ast.Call) -> str:
        """loguru 0.7's `logger` (dynrt/loguru.rs): the level methods and `log()` at their call site, `add()` on
        sys.stderr / sys.stdout with a literal format, `remove()`. What the binary does not reproduce is refused
        here, with the reason, never at run time."""
        short = name[len("loguru.logger."):] if name.startswith("loguru.logger.") else None
        supported = "the level methods (trace, debug, info, success, warning, error, critical), log(), add(sys.stderr | sys.stdout, ...) and remove()"
        if short in LOGURU_LEVEL_METHODS or short == "log":
            self.p.__dict__["loguru"] = True
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::loguru::log(cx, {rs(short)}, {args}, {kwargs}, {self.log_loc(node)}, {rs(self.module)}).await")
        if short == "exception":
            raise self.err("loguru `logger.exception()` is not supported: loguru formats the exception itself (its "
                           "`backtrace`/`diagnose` traceback, with the values of the variables by default), which the "
                           "binary does not reproduce", node)
        if short == "remove":
            if len(node.args) + len(node.keywords) > 1 or any(isinstance(a, ast.Starred) for a in node.args) \
                    or any(k.arg != "handler_id" for k in node.keywords):
                raise self.err("loguru `logger.remove()` takes one argument, `handler_id`", node)
            self.p.__dict__["loguru"] = True
            arg = node.args[0] if node.args else (node.keywords[0].value if node.keywords else None)
            return self.q(f"{RT}::loguru::remove({'None' if arg is None else f'Some({self.expr(arg)})'})")
        if short == "add":
            return self.loguru_add(node)
        what = f"`logger.{short}()`" if short else f"`{name}`"
        raise self.err(f"loguru {what} is not supported: py2axum reproduces {supported}", node)

    def loguru_add(self, node: ast.Call) -> str:
        from . import logurufmt
        if any(isinstance(a, ast.Starred) for a in node.args) or any(k.arg is None for k in node.keywords):
            raise self.err("loguru `logger.add()`: `*`/`**` expansion is not supported", node)
        kws = {k.arg: k.value for k in node.keywords}
        if len(node.args) > 1:
            raise self.err("loguru `logger.add()` takes the sink only as positional argument", node)
        sink = node.args[0] if node.args else kws.pop("sink", None)
        if node.args and "sink" in kws:
            raise self.err("loguru `logger.add()`: the sink is given twice", node)
        if sink is None:
            raise self.err("loguru `logger.add()` needs a sink", node)
        ref = self.static_ref(sink) if isinstance(sink, (ast.Name, ast.Attribute)) else None
        fd = {"sys.stdout": 1, "sys.stderr": 2}.get(ref.dotted) if isinstance(ref, Ext) else None
        if fd is None:
            raise self.err(f"loguru `logger.add({ast.unparse(sink)})` is not supported: the binary writes to sys.stderr "
                           "or sys.stdout only (no file sink, rotation or retention, no callable or logging handler)", sink)

        def falsy(k):
            v = kws.get(k)
            return v is None or isinstance(v, ast.Constant) and not v.value
        refused = {"serialize": "the JSON record (serialize=True) is not reproduced",
                   "enqueue": "the binary writes each message at once (enqueue=True)",
                   "filter": "a filter is not reproduced", "context": "a multiprocessing context is not reproduced"}
        for k in kws:
            if k in refused and not falsy(k):
                raise self.err(f"loguru `logger.add({k}=...)` is not supported: {refused[k]}", kws[k])
            if k not in {"level", "format", "colorize", "backtrace", "diagnose", "catch", *refused}:
                raise self.err(f"loguru `logger.add({k}=...)` is not supported (supported: level, format, colorize, "
                               "backtrace, diagnose, catch)", kws[k])
        fmt = kws.get("format")
        if fmt is None:
            text = logurufmt.DEFAULT_FORMAT
        elif isinstance(fmt, ast.Constant) and isinstance(fmt.value, str):
            text = fmt.value
        else:
            raise self.err("loguru `logger.add(format=...)` is supported with a literal text only (compiled at "
                           "translation)", fmt)
        try:
            compiled = logurufmt.compile_format(text)
        except ValueError as e:
            raise self.err(f"loguru `logger.add(format=...)`: {e}", fmt or node) from None
        self.p.__dict__["loguru"] = True
        # the arguments in the order of the call, each evaluated once (the sink is sys.stderr / sys.stdout)
        vals = {}
        for k in node.keywords:
            if k.arg not in ("sink", "format"):
                vals[k.arg] = t = self.tmp("lg")
                self.emit(f"let {t} = {self.expr(k.value)};")
        level = f"Some({vals['level']})" if "level" in vals else "None"
        colorize = f"Some({vals['colorize']})" if "colorize" in vals else "None"
        return self.q(f"{RT}::loguru::add({fd}, {level}, {colorize}, {compiled})")

    def sentry_call(self, name: str, node: ast.Call) -> str:
        """sentry_sdk 2.x on the Rust SDK (dynrt/sentry.rs). An option the binary does not reproduce is
        refused here, with the reason."""
        short = name[len("sentry_sdk."):]
        if any(isinstance(a, ast.Starred) for a in node.args) or any(k.arg is None for k in node.keywords):
            raise self.err(f"`*`/`**` expansion in {name}() is not supported", node)
        kws = [k.arg for k in node.keywords]
        src = f"Some({rs(f'{self.p.rel_file(self.module)}:{node.lineno}')})"

        def check(allowed: set[str], refused: dict[str, str] | None = None, max_pos: int = 0) -> None:
            for k in kws:
                if refused and k in refused:
                    raise self.err(f"{name}({k}=...) is not supported: {refused[k]}", node)
                if k not in allowed:
                    raise self.err(f"{name}({k}=...) is not supported (supported: {', '.join(sorted(allowed)) or 'none'})", node)
            if len(node.args) > max_pos:
                raise self.err(f"{name}(): {len(node.args)} positional arguments, at most {max_pos} supported", node)

        integ = SENTRY_INTEGRATIONS.get(name)
        if integ is not None:
            kind, allowed, refused = integ
            check(allowed, refused)
            _, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::sentry::integration({rs(kind)}, {kwargs})")
        if short == "init":
            check(SENTRY_INIT_OPTIONS, SENTRY_INIT_REFUSED, max_pos=1)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::sentry::init(cx, {args}, {kwargs}).await")
        binds = {"set_tag": ["key", "value"], "set_context": ["key", "value"], "set_extra": ["key", "value"],
                 "set_tags": ["tags"], "set_user": ["value"], "set_level": ["value"]}
        if short in binds:
            params = binds[short]
            check(set(params), max_pos=len(params))
            given = dict(zip(params, node.args)) | {k.arg: k.value for k in node.keywords}
            missing = [x for x in params if x not in given]
            if missing:
                raise self.err(f"{name}() missing required argument: '{missing[0]}'", node)
            vals = ", ".join(f"&{self.expr(given[x])}" for x in params)
            return self.q(f"{RT}::sentry::{short}(cx, {vals})")
        if short in ("capture_message", "capture_exception"):
            first = "message" if short == "capture_message" else "error"
            check({first, "level", "tags", "extras", "contexts", "user", "fingerprint"} - ({"level"} if first == "error" else set()),
                  {"scope": "pass the scope keywords (tags=, extras=, contexts=, user=, fingerprint=) or use "
                             "`with sentry_sdk.new_scope() as scope: scope.capture_...`"}, max_pos=1)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::sentry::{short}(cx, {src}, {args}, {kwargs}).await")
        if short == "add_breadcrumb":
            if len(node.args) > 2:
                raise self.err(f"{name}(): at most 2 positional arguments (crumb, hint)", node)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::sentry::add_breadcrumb(cx, {args}, {kwargs})")
        if short in ("new_scope", "push_scope", "get_isolation_scope", "get_current_scope", "last_event_id", "is_initialized"):
            check(set(), {"callback": "use `with sentry_sdk.push_scope() as scope:`"})
            if short in ("new_scope", "push_scope"):
                return self.q(f"{RT}::sentry::new_scope(cx)")
            if short == "is_initialized":
                return self.q(f"{RT}::sentry::is_initialized()")
            return self.q(f"{RT}::sentry::{short}(cx)")
        if short == "flush":
            check({"timeout"}, {"callback": "flush() waits for the queue in the binary; no callback"}, max_pos=1)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::sentry::flush({args}, {kwargs}).await")
        raise self.err(f"`{name}()` is not supported: py2axum maps sentry_sdk's init, capture_message, capture_exception, "
                       "set_tag(s), set_user, set_context, set_extra, set_level, add_breadcrumb, new_scope, push_scope, "
                       "get_isolation_scope, get_current_scope, flush, last_event_id, is_initialized and the FastAPI, "
                       "Starlette, logging, SQLAlchemy and asyncio integrations", node)

    REFUSED_BUILTINS = {
        "dir": "it lists the attributes an object has at run time, which the binary does not keep as names; read the "
               "attribute you need with getattr()/hasattr(), or vars() for an instance's own fields",
        "globals": "the binary has no module namespace to hand out; name the module-level variable you need",
        "locals": "the binary has no frame to hand out; use the local variables by name",
        "eval": "the binary has no Python interpreter to run source text with",
        "exec": "the binary has no Python interpreter to run source text with",
        "compile": "the binary has no Python interpreter to compile source text with",
    }

    # the categories CPython's default filters ignore (`ignore::DeprecationWarning`...); the others are printed on stderr
    IGNORED_WARNINGS = {"DeprecationWarning", "PendingDeprecationWarning", "ImportWarning", "ResourceWarning"}
    # builtin categories CPython's default filters print (action "default": once per message, category and line)
    PRINTED_WARNINGS = {"UserWarning", "RuntimeWarning", "FutureWarning", "SyntaxWarning", "UnicodeWarning",
                        "BytesWarning", "EncodingWarning", "Warning"}

    def printed_warning(self, node: ast.AST | None, msg: ast.AST | None) -> str | None:
        """the name of a builtin category the default filters print, written at `node` (None: UserWarning, unless the
        message is a warning instance)"""
        if node is None:
            if isinstance(msg, ast.Call):
                ref = self.static_ref(msg.func)
                if isinstance(ref, tuple) and ref[:1] == ("builtin",) and ref[1] in self.PRINTED_WARNINGS | self.IGNORED_WARNINGS:
                    return None  # an instance written in place: refused below (its text is its args)
            return "UserWarning"
        ref = self.static_ref(node)
        if isinstance(ref, tuple) and ref[:1] == ("builtin",) and ref[1] in self.PRINTED_WARNINGS:
            return ref[1]
        if isinstance(ref, Ext) and ref.dotted.startswith("builtins.") and ref.dotted[9:] in self.PRINTED_WARNINGS:
            return ref.dotted[9:]
        return None

    def warning_ignored(self, node: ast.AST, depth: int = 0) -> bool:
        """The category written at `node` is one the default filters ignore: a builtin of IGNORED_WARNINGS, or a project
        class deriving only from such (False when it cannot be told)."""
        ref = self.static_ref(node)
        if isinstance(ref, tuple) and ref[:1] == ("builtin",):
            return ref[1] in self.IGNORED_WARNINGS
        if isinstance(ref, Ext):
            return ref.dotted.startswith("builtins.") and ref.dotted[9:] in self.IGNORED_WARNINGS
        if isinstance(ref, Sym) and depth < 8:
            m = self.p.ix.module(ref.module)
            d = m.defs.get(ref.name) if m is not None else None
            if isinstance(d, ast.ClassDef) and d.bases and not d.keywords:
                saved = self.module
                self.module = ref.module
                try:
                    return all(self.warning_ignored(b, depth + 1) for b in d.bases)
                finally:
                    self.module = saved
        return False

    def warnings_call(self, name: str, node: ast.Call) -> str:
        """`warnings.warn` of a category the default filters ignore (evaluated for its effects, then dropped);
        `simplefilter`/`filterwarnings("ignore")` and `catch_warnings()` (no recording), which then change nothing.
        What would print to stderr (a module path and line only CPython knows, once per location) or raise
        (`error` filters) is refused."""
        if any(isinstance(a, ast.Starred) for a in node.args) or any(k.arg is None for k in node.keywords):
            raise self.err(f"warnings.{name[9:]}() with * or ** arguments is not supported", node)
        kws = {k.arg: k.value for k in node.keywords}
        if name == "warnings.catch_warnings":
            if node.args or any(k != "record" for k in kws) or (
                    "record" in kws and not (isinstance(kws["record"], ast.Constant) and kws["record"].value is False)):
                raise self.err("warnings.catch_warnings(record=True, module=, action=) is not supported: the warnings "
                               "py2axum drops are never recorded", node)
            return self.q(f"Ok::<V, Exc>(V::native({RT}::Native::Suppress(Vec::<V>::new())))")
        if name in ("warnings.simplefilter", "warnings.filterwarnings"):
            act = node.args[0] if node.args else kws.get("action")
            if not (isinstance(act, ast.Constant) and act.value == "ignore"):
                raise self.err(f"warnings.{name[9:]}() is supported with the action \"ignore\" only: another action prints the warnings "
                               "py2axum drops, or turns them into exceptions", node)
        else:
            order = ["message", "category", "stacklevel", "source"]
            if len(node.args) > 4 or any(k not in order for k in kws) or len(node.args) + len(kws) == 0:
                raise self.err("warnings.warn(message, category=None, stacklevel=1, source=None) only", node)
            cat = node.args[1] if len(node.args) > 1 else kws.get("category")
            msg = node.args[0] if node.args else kws.get("message")
            if cat is not None:
                ok = self.warning_ignored(cat)
            else:  # the category is the message's own: a warning instance written in place
                ok = isinstance(msg, ast.Call) and self.warning_ignored(msg.func)
            printed = None if ok else self.printed_warning(cat, msg)
            level = node.args[2] if len(node.args) > 2 else kws.get("stacklevel")
            if printed is not None and msg is not None and (level is None or (isinstance(level, ast.Constant) and level.value == 1)) \
                    and "source" not in kws and len(node.args) < 4:
                # printed on stderr as CPython's default filters do: `file:line: Category: message` and the line
                lines = Path(self.p.src(self.module)).read_text(encoding="utf-8", errors="replace").splitlines()
                line = lines[node.lineno - 1].strip() if 0 < node.lineno <= len(lines) else ""
                text = self.expr(msg)
                return self.q(f"{RT}::logging::warn_print({rs(printed)}, &{text}, {rs(self.p.rel_file(self.module))}, "
                              f"{node.lineno}, {rs(line)}, {rs(self.module)})")
            if not ok:
                raise self.err("warnings.warn(): only categories CPython's default filters ignore (DeprecationWarning, "
                               "PendingDeprecationWarning, ImportWarning, ResourceWarning and their subclasses), or a "
                               "builtin category they print (UserWarning, RuntimeWarning, FutureWarning...) with "
                               "stacklevel=1, are supported: another warning is printed with the location of a caller "
                               "py2axum does not know; use logging.warning() for it", node)
        args, kwargs = self.dyn_args(node)
        return self.q(f"{{ let _a: Vec<V> = {args}; let _k: Vec<(String, V)> = {kwargs}; Ok::<V, Exc>(V::None) }}")

    # builtins that consume a whole iterator at once, before returning
    EAGER_CONSUMERS = {"list", "tuple", "set", "frozenset", "sorted"}

    def builtin_call(self, name: str, node: ast.Call) -> str:
        if name in self.EAGER_CONSUMERS and len(node.args) == 1 and isinstance(node.args[0], ast.Call):
            # list(string.Formatter().parse(s)), list(itertools.chain(...)): an iterator consumed whole at once
            self.__dict__.setdefault("_eager_calls", set()).add(node.args[0])
        # any/all/next over a generator expression: lazy and short-circuiting, as in Python
        if name in {"any", "all", "next"} and node.args and isinstance(node.args[0], ast.GeneratorExp) and not node.keywords:
            g = node.args[0]
            lbl, res = f"'g{self.p.uid()}", self.tmp("r")
            if name == "next":
                if len(node.args) > 2:
                    raise self.err("next() takes at most 2 arguments", node)
                miss = (f"{res} = {self.expr(node.args[1])};" if len(node.args) == 2
                        else f"{self.raise_code(f'Exc::new(&{RT}::v::STOP_ITERATION, vec![])')}")
                body = self.comprehension(g.generators, lambda: f"{res} = {self.expr(g.elt)}; break {lbl};", [g.elt])
                return f"{{ let mut {res} = V::None; {lbl}: {{ {body} {miss} }} {res} }}"
            if len(node.args) != 1:
                raise self.err(f"{name}() takes exactly one argument", node)
            hit = "true" if name == "any" else "false"
            neg = "" if name == "any" else "!"
            body = self.comprehension(g.generators, lambda: f"if {neg}{self.truthy(g.elt)} {{ {res} = {hit}; break {lbl}; }}", [g.elt])
            return f"{{ let mut {res} = !{hit}; {lbl}: {{ {body} }} V::Bool({res}) }}"
        # iter()/next() keep no state here: only the one-shot forms are translated
        if name == "iter" and not getattr(self, "_iter_ok", False) and self.stored_iter(node):
            # `it = iter(x)`: an iterator keeping its position (next(it), then `for y in it` / list(it) take the rest)
            if len(node.args) != 1 or node.keywords:
                raise self.err("builtin `iter()` is not supported with a callable and a sentinel (iter(callable, sentinel))", node)
            return self.q(f"{RT}::methods::b_iter_stored(&{self.expr(node.args[0])})")
        if name == "next" and node.args:
            a0 = node.args[0]
            if isinstance(a0, ast.Call) and isinstance(a0.func, ast.Name) and a0.func.id == "iter":
                self._iter_ok = True
                try:
                    return self._builtin_call(name, node)
                finally:
                    self._iter_ok = False
            if not isinstance(a0, (ast.List, ast.Tuple, ast.ListComp)):
                # a stored iterator (csv.reader...): the runtime keeps its position; a list is a TypeError
                args, _ = self.dyn_args(node)
                return self.q(f"{RT}::methods::b_next_iter(&{args})")
        return self._builtin_call(name, node)

    def _builtin_call(self, name: str, node: ast.Call) -> str:
        if name in libmap.BUILTIN_EXC_NAMES:
            args, _ = self.dyn_args(node)
            return f"V::Exc(Exc::new(&{RT}::v::{libmap.BUILTIN_EXC_NAMES[name]}, {args}))"
        if name == "isinstance":
            v = self.expr(node.args[0])
            t = self.tmp()
            return f"{{ let {t} = {v}; V::Bool({self.isinstance_test(node.args[1], t)}) }}"
        if name in {"getattr", "hasattr"}:
            obj = self.expr(node.args[0])
            attr = self.expr(node.args[1])
            if name == "hasattr":
                return self.q(f"{RT}::methods::hasattr(cx, &{obj}, &{RT}::ops::str_(&{attr})?).await")
            if len(node.args) > 2:
                dflt = self.expr(node.args[2])
                return (f"match {RT}::methods::getattr(cx, &{obj}, &{self.q(f'{RT}::ops::str_(&{attr})')}).await {{ Ok(v) => v, "
                        f"Err(e) if e.isinstance(&{RT}::v::ATTRIBUTE_ERROR) => {dflt}, Err(e) => {self.q_err('e')} }}")
            return self.q(f"{RT}::methods::getattr(cx, &{obj}, &{RT}::ops::str_(&{attr})?).await")
        if name == "setattr":
            obj, attr, val = (self.expr(a) for a in node.args)
            return f"{{ {self.q(f'{RT}::methods::setattr_cx(cx, &{obj}, &{RT}::ops::str_(&{attr})?, {val}).await')}; V::None }}"
        if node.keywords and name not in KWARG_BUILTINS:
            # a keyword argument the template would drop: positional when CPython allows it, else refused
            names = POSITIONAL_KWARGS.get(name, ())
            given = [k.arg for k in node.keywords]
            if (not names or None in given or len(node.args) != 1 or any(a not in names for a in given)
                    or given != list(names[:len(given)])):
                raise self.err(f"{name}({', '.join(f'{k}=' for k in given if k)}) is not supported"
                               + (f" (only {', '.join(f'{n}=' for n in names)}, in that order)" if names else ""), node)
            node = ast.Call(func=node.func, args=[*node.args, *(k.value for k in node.keywords)], keywords=[])
        args, kwargs = self.dyn_args(node)
        simple = {
            "len": f"{RT}::methods::b_len(&{args}[0])", "repr": f"{RT}::methods::b_repr(&{args}[0])",
            "str": f"{RT}::methods::b_str(&{args})", "int": f"{RT}::methods::b_int(&{args})",
            "float": f"{RT}::methods::b_float(&{args})", "bool": f"{RT}::methods::b_bool(&{args})",
            "list": f"{RT}::methods::b_list(&{args})", "tuple": f"{RT}::methods::b_tuple(&{args})",
            "set": f"{RT}::methods::b_set(&{args})", "frozenset": f"{RT}::methods::b_frozenset(&{args})",
            "dict": f"{RT}::methods::b_dict(&{args}, &{kwargs})", "reversed": f"{RT}::methods::b_reversed(&{args}[0])",
            "sum": f"{RT}::methods::b_sum(&{args})", "any": f"{RT}::methods::b_any(&{args}[0])",
            "all": f"{RT}::methods::b_all(&{args}[0])", "next": f"{RT}::methods::b_next(&{args})",
            "round": f"{RT}::methods::b_round(&{args})", "abs": f"{RT}::methods::b_abs(&{args}[0])",
            "enumerate": f"{RT}::methods::b_enumerate(&{args})", "zip": f"{RT}::methods::b_zip(&{args}, &{kwargs})",
            "range": f"{RT}::methods::b_range(&{args})", "print": f"{RT}::methods::b_print(&{args})",
            "iter": f"{RT}::methods::b_list(&{args})", "hash": f"{RT}::methods::b_hash(&{args})",
            "type": f"{RT}::methods::b_type(&{args})",
            "chr": f"{RT}::methods::b_chr(&{args})", "ord": f"{RT}::methods::b_ord(&{args})",
            "divmod": f"{RT}::methods::b_divmod(&{args})", "format": f"{RT}::methods::b_format(&{args})",
            "bytes": f"{RT}::methods::b_bytes(&{args}, &{kwargs})",
            "object": f"{RT}::proto::object_new(&{args}, &{kwargs})",
            "open": f"{RT}::pathio::open(&{args}, &{kwargs})",
            "sorted": f"{RT}::methods::b_sorted(cx, &{args}[0], &{kwargs}).await",
            "id": f"{RT}::methods::b_id(&{args})", "callable": f"{RT}::methods::b_callable(&{args})",
            "vars": f"{RT}::methods::getattr(cx, &{args}[0], \"__dict__\").await", "issubclass": f"{RT}::types::issubclass(&{args})",
            "filter": f"{RT}::methods::b_filter(cx, &{args}).await", "map": f"{RT}::methods::b_map(cx, &{args}).await",
            "min": f"{RT}::methods::b_minmax(cx, false, &{args}, &{kwargs}).await",
            "max": f"{RT}::methods::b_minmax(cx, true, &{args}, &{kwargs}).await",
            # an awaitable, like CPython: the generator advances when it is awaited
            "anext": f"{{ let __it = {args}[0].clone(); Ok::<V, Exc>({RT}::aio::coro(Box::pin(async move {{ {RT}::agen::anext(&__it).await }}))) }}",
        }
        if name in simple:
            return self.q(simple[name])
        raise self.err(f"builtin `{name}()` is not supported", node)

    def stored_iter(self, call: ast.Call) -> bool:
        """`it = iter(x)`: a stateful iterator (next(it) twice...), not translated; `f(iter(x))` is. So is
        `iter(x) is y`, an identity test."""
        scope = self.node if self.node is not None else None
        if scope is None:
            return False
        for n in walk_scope(scope):
            if isinstance(n, (ast.Assign, ast.AnnAssign, ast.NamedExpr)) and n.value is call:
                return True
            # `iter(it) is it`: an iterator's iter() is itself
            if (isinstance(n, ast.Compare) and all(isinstance(o, (ast.Is, ast.IsNot)) for o in n.ops)
                    and any(x is call for x in [n.left, *n.comparators])):
                return True
        return False

    def q_err(self, e: str) -> str:
        if self.sinks:
            label, slot = self.sinks[-1]
            return f"{{ {slot} = Some({e}); break {label}; }}"
        return f"return Err({e})"

    def isinstance_test(self, t: ast.AST, v: str) -> str:
        if isinstance(t, ast.Tuple):
            return "(" + " || ".join(self.isinstance_test(x, v) for x in t.elts) + ")"
        # a union `int | float`, `X | None`, `Optional[X]`, `Union[X, Y]` (Python >= 3.10): each member, like a tuple;
        # a parameterized member (`list[int]`) keeps the run-time path, which raises as CPython does
        members = self.union_members(t)
        if members is not None and not any(isinstance(m, ast.Subscript) for m in members):
            return "(" + " || ".join("matches!(" + v + ", V::None)" if isinstance(m, ast.Constant) and m.value is None
                                     else self.isinstance_test(m, v) for m in members) + ")"
        ref = self.static_ref(t)
        if isinstance(ref, tuple) and ref[0] == "builtin":
            if ref[1] in libmap.BUILTIN_EXC_NAMES:
                return f"{RT}::methods::isinstance_class(&{v}, &{RT}::v::{libmap.BUILTIN_EXC_NAMES[ref[1]]})"
            return f"{RT}::methods::isinstance_builtin(&{v}, {rs(ref[1])})"
        if isinstance(ref, Sym) and isinstance(self.p.ix.definition(ref), ast.ClassDef):
            return f"{RT}::methods::isinstance_class(&{v}, &{self.p.class_static(ref)})"
        if isinstance(ref, Sym):
            # a module variable holding types (`string_types = str,`, `unicode = str`): read at run time below
            pass
        elif isinstance(ref, Ext):
            short = {"datetime.datetime": "datetime", "datetime.date": "date", "datetime.timedelta": "timedelta",
                     "datetime.time": "time", "uuid.UUID": "UUID", "enum.Enum": "Enum", "pathlib.Path": "Path",
                     "decimal.Decimal": "Decimal", "datetime.tzinfo": "tzinfo", "zoneinfo.ZoneInfo": "ZoneInfo",
                     "datetime.timezone": "timezone",
                     **TZ_CLASSES}.get(ref.dotted)
            if short:
                return f"{RT}::methods::isinstance_builtin(&{v}, {rs(short)})"
            if ref.dotted in libmap.EXCEPTIONS:
                return f"{RT}::methods::isinstance_class(&{v}, &{RT}::v::{libmap.EXCEPTIONS[ref.dotted]})"
            sa = self.p.sa_type_class(ref.dotted, t, self.module)
            if sa is not None:
                # a SQLAlchemy column type: only the types of `Model.__table__` columns are its instances here
                return f"{RT}::satable::isinstance_type(&{v}, {rs(sa)})"
        # a type known at run time (a variable, a library class value, a tuple built by the code)
        tv = self.expr(t)
        return self.q(f"{RT}::types::isinstance(&{v}, &{tv})")


    def union_members(self, t: ast.AST) -> list[ast.AST] | None:
        """The members of a union written in the source (`A | B | None`, `typing.Optional[A]`, `typing.Union[A, B]`),
        flattened; None when `t` is not one."""
        if isinstance(t, ast.BinOp) and isinstance(t.op, ast.BitOr):
            out = []
            for side in (t.left, t.right):
                inner = self.union_members(side)
                out.extend(inner if inner is not None else [side])
            return out
        if isinstance(t, ast.Subscript) and self.static_ref(t.value) in (Ext("typing.Optional"), Ext("typing.Union")):
            elts = t.slice.elts if isinstance(t.slice, ast.Tuple) else [t.slice]
            out = []
            for e in elts:
                inner = self.union_members(e)
                out.extend(inner if inner is not None else [e])
            if self.static_ref(t.value) == Ext("typing.Optional"):
                out.append(ast.Constant(value=None))
            return out
        return None


# ====================================================================== routes and dependencies


# library classes whose instances the binary only stores (libmap: libs::lib_instance), usable as an arbitrary type
LIB_INSTANCE_TYPES = {"fastapi.security.OAuth2PasswordBearer", "fastapi.security.HTTPBearer", "fastapi.security.HTTPBasic",
                      "fastapi.security.oauth2.OAuth2PasswordBearer", "fastapi.security.http.HTTPBearer",
                      "fastapi.security.http.HTTPBasic"}
# fastapi/starlette Response classes (a return annotation of one of them is no response model)
RESPONSE_CLASSES = {"Response", "StreamingResponse", "FileResponse", "RedirectResponse", "JSONResponse", "HTMLResponse",
                    "PlainTextResponse", "ORJSONResponse", "UJSONResponse", "EventSourceResponse"}


class LateCons:
    """A constraint value that is an expression of the settings (`le=settings.MAX_PAGE`), read at startup."""

    def __init__(self, node: ast.AST, module: str):
        self.node, self.module = node, module

    def __repr__(self) -> str:
        return f"<settings {ast.unparse(self.node)}>"


@dataclass
class RParam:
    name: str
    kind: str  # session | request | response | dep | path | query | header | body
    td: str | None = None
    required: bool = True
    default: str | None = None  # dflt fn name
    alias: str | None = None
    dep: Sym | None = None
    dyn_default: str | None = None
    security: tuple[str, bool] | None = None  # (runtime Security variant, auto_error)
    list: bool = False  # list[UploadFile]
    source: str | None = None  # pmodel: query | header | cookie | form
    convert: bool = True  # pmodel of headers: Header(convert_underscores=)
    scope: str = "request"  # dep: Depends(scope=), when its exit code runs
    afters: tuple = ()  # path/query/header/cookie: Annotated AfterValidator functions
    oauth: tuple = ()  # dep: `Security(f, scopes=[...])`, its own scopes


class RouteBuilder:
    def __init__(self, proj: Project):
        self.p = proj
        self.fe = proj.fe

    def is_ext(self, module, node, scope, *names) -> bool:
        if node is None:
            return False
        t = self.p.resolve(module, node, scope)
        return is_ext(t, *names)

    def dependency_elements(self, value: ast.AST, module: str, file: str, depth: int = 0) -> list[tuple[ast.AST, str, str]]:
        """The elements of a `dependencies=` value, each with the module and file it is resolved in: a list or tuple
        literal, a module constant bound once to one and never changed (`SUPER_ADMIN_ONLY = [Depends(auth)]`), a
        concatenation of those, or None. Anything else is refused: dropping a dependency would serve the route
        without its authentication."""
        if isinstance(value, ast.Constant) and value.value is None:
            return []
        if isinstance(value, (ast.List, ast.Tuple)):
            if any(isinstance(e, ast.Starred) for e in value.elts):
                raise TranspileError("dependencies=[*...]: unpacking is not supported", value, file)
            return [(e, module, file) for e in value.elts]
        if isinstance(value, ast.BinOp) and isinstance(value.op, ast.Add) and depth < 8:
            return (self.dependency_elements(value.left, module, file, depth + 1)
                    + self.dependency_elements(value.right, module, file, depth + 1))
        if isinstance(value, (ast.Name, ast.Attribute)) and depth < 8:
            t = self.p.resolve(module, value)
            m = self.p.ix.module(t.module) if isinstance(t, Sym) else None
            d = m.defs.get(t.name) if m is not None else None
            if (isinstance(d, (ast.Assign, ast.AnnAssign)) and d.value is not None
                    and len(m.binders.get(t.name, [d])) <= 1 and not self._mutated_global(t)):
                return self.dependency_elements(d.value, t.module, str(m.path), depth + 1)
        raise TranspileError(f"dependencies={ast.unparse(value)[:60]}: must be a list of Depends(...) read statically "
                             "(a literal, or a module constant bound once to one): the route would otherwise be served "
                             "without these dependencies", value, file)

    def _mutated_global(self, t: Sym) -> bool:
        """`NAME.append(...)`, `NAME += ...`, `NAME[i] = ...`, `mod.NAME.extend(...)` anywhere in the package."""
        for m in self.p.ix.package_modules():
            parents = self.fe._parents(m.name)
            for n in ast.walk(m.tree):
                ref = ((isinstance(n, ast.Name) and n.id == t.name and m.name == t.module)
                       or (isinstance(n, ast.Attribute) and n.attr == t.name and m.name != t.module))
                if not ref:
                    continue
                up = parents.get(n)
                if (isinstance(up, ast.Attribute) and up.attr in {"append", "extend", "insert", "remove", "pop", "clear",
                                                                   "__setitem__", "__iadd__"}) \
                        or (isinstance(up, ast.AugAssign) and up.target is n) \
                        or (isinstance(up, ast.Subscript) and isinstance(up.ctx, (ast.Store, ast.Del))) \
                        or (isinstance(n, ast.Attribute) and isinstance(n.ctx, ast.Store)):
                    return True
        return False

    SECURITY = {"OAuth2PasswordBearer": ("OAuth2Bearer", {"tokenUrl", "scheme_name", "scopes", "description", "refreshUrl"}),
                "HTTPBearer": ("HttpBearer", {"bearerFormat", "scheme_name", "description"}),
                "HTTPBasic": ("HttpBasic", {"scheme_name", "description"})}

    def security_scheme(self, t: Sym, src) -> tuple[str, bool, dict] | None:
        """`scheme = OAuth2PasswordBearer(...)` / `HTTPBearer(...)` / `HTTPBasic(...)` at module level, used as `Depends(scheme)`.
        Other fastapi.security classes are refused (their errors and challenges differ)."""
        d = self.p.ix.definition(t)
        if not (isinstance(d, ast.Assign) and isinstance(d.value, ast.Call)):
            return None
        return self.security_call(d.value, t.module)

    SECURITY_HOOKS = ("make_not_authenticated_error", "make_authenticate_headers")

    def security_subclass(self, sym: Sym) -> tuple[Ext, dict] | None:
        """`class HTTPBearer403(HTTPBearer)` overriding `make_not_authenticated_error` and/or
        `make_authenticate_headers` (the exception and the challenge headers of a missing or bad credential):
        (the fastapi class, {hook: its method}). The overrides must not read `self` (the runtime has no
        instance of the scheme); any other member is refused."""
        node = self.p.ix.definition(sym)
        if not (isinstance(node, ast.ClassDef) and len(node.bases) == 1):
            return None
        base = self.p.resolve(sym.module, node.bases[0], None)
        if not (isinstance(base, Ext) and base.package == "fastapi" and base.dotted.split(".")[-1] in self.SECURITY):
            return None
        file = str(self.p.ix.module(sym.module).path)
        if node.keywords or node.decorator_list:
            raise TranspileError(f"security scheme {sym.name}: class keywords and decorators are not supported", node, file)
        hooks = {}
        for st in node.body:
            if isinstance(st, ast.Pass) or (isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant)):
                continue
            a = getattr(st, "args", None)
            if not (isinstance(st, ast.FunctionDef) and st.name in self.SECURITY_HOOKS and not st.decorator_list
                    and len(a.posonlyargs + a.args) == 1 and not (a.vararg or a.kwonlyargs or a.kwarg)):
                raise TranspileError(f"security scheme {sym.name}: only `def make_not_authenticated_error(self)` and "
                                     "`def make_authenticate_headers(self)` overrides are supported", st, file)
            me = a.args[0].arg if a.args else a.posonlyargs[0].arg
            if any(isinstance(n, ast.Name) and n.id == me for n in ast.walk(st)):
                raise TranspileError(f"security scheme {sym.name}.{st.name}: reading `{me}` is not supported (the "
                                     "scheme's attributes are not kept)", st, file)
            if st.name == "make_authenticate_headers" and base.dotted.split(".")[-1] == "OAuth2PasswordBearer":
                continue  # OAuth2's error does not call it: no effect
            hooks[st.name] = Sym(sym.module, f"{node.name}.{st.name}")
        return base, hooks

    def security_call(self, call: ast.Call, module: str) -> tuple[str, bool, dict] | None:
        """`OAuth2PasswordBearer(...)` / `HTTPBearer(...)` / `HTTPBasic(...)`, or a project subclass of one of them:
        (runtime variant, auto_error, overridden hooks)."""
        target = self.p.resolve(module, call.func, None)
        hooks = {}
        if isinstance(target, Sym):
            sub = self.security_subclass(target)
            if sub is None:
                return None
            target, hooks = sub
        if not (isinstance(target, Ext) and target.package == "fastapi"):
            return None
        cls = target.dotted.split(".")[-1]
        file = str(self.p.ix.module(module).path)
        if cls not in self.SECURITY:
            raise TranspileError(f"security scheme {cls} is not supported (only OAuth2PasswordBearer, HTTPBearer, HTTPBasic)", call, file)
        variant, doc_only = self.SECURITY[cls]
        if len(call.args) > (1 if cls == "OAuth2PasswordBearer" else 0):
            raise TranspileError(f"{cls}(...): positional arguments are not supported", call, file)
        auto_error = True
        for kw in call.keywords:
            if kw.arg == "auto_error":
                if not (isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, bool)):
                    raise TranspileError(f"{cls}(auto_error=) must be a literal True/False", kw.value, file)
                auto_error = kw.value.value
            elif kw.arg == "realm" and cls == "HTTPBasic":
                if not (isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, (str, type(None)))):
                    raise TranspileError("HTTPBasic(realm=) must be a literal string", kw.value, file)
                if kw.value.value:
                    variant = f"HttpBasic(Some({rs(kw.value.value)}))"
            elif kw.arg not in doc_only:
                raise TranspileError(f"{cls}({kw.arg}=) is not supported", kw.value, file)
        if variant == "HttpBasic":
            variant = "HttpBasic(None)"
        return variant, auto_error, hooks

    # Field() options that constrain the value (FastAPI reads them from the model's signature, as `Annotated`
    # metadata); the others only document it
    CLASS_DEP_CONS = {"ge", "gt", "le", "lt", "min_length", "max_length", "pattern", "multiple_of", "strict",
                      "allow_inf_nan", "max_digits", "decimal_places"}
    CLASS_DEP_DOC = {"description", "title", "examples", "deprecated", "json_schema_extra"}

    def class_dependency(self, sym: Sym, src: str) -> Sym:
        """`p: Model = Depends()`: FastAPI calls the class, its parameters read from the model's signature: one
        query parameter per field (type, default, Field constraints), then `Model(**fields)`. Compiled as that
        function, written next to the class. Refused: a base other than BaseModel, validators (a ValueError
        would surface as a pydantic ValidationError, not a 422), aliases, non-scalar fields (body in FastAPI)."""
        name = f"__py2axum_depends_{sym.name}"
        m = self.p.ix.module(sym.module)
        if name in m.defs:
            return Sym(sym.module, name)
        cls = self.p.fe.schema_syms[sym]
        msrc = self.p.src(sym.module)
        if len(cls.bases) != 1 or not is_ext(self.p.resolve(sym.module, cls.bases[0]), "pydantic.BaseModel") or cls.keywords:
            raise TranspileError(f"{sym.name}: a class dependency must subclass BaseModel directly", cls, msrc)
        params, names = [], []
        for st in cls.body:
            if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)) and st.decorator_list:
                raise TranspileError(f"{sym.name}: a class dependency with validators or decorated methods is not supported", st, msrc)
            if isinstance(st, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "model_config" for t in st.targets):
                raise TranspileError(f"{sym.name}: model_config on a class dependency is not supported", st, msrc)
            if not (isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name)):
                continue
            ann_t = (dotted(st.annotation) or "").split(".")[-1]
            if ann_t == "ClassVar" or (isinstance(st.annotation, ast.Subscript) and (dotted(st.annotation.value) or "").split(".")[-1] in {"ClassVar", "Annotated"}):
                raise TranspileError(f"{sym.name}.{st.target.id}: only plain annotations are supported in a class dependency", st, msrc)
            for n in ast.walk(st.annotation):
                if isinstance(n, ast.Name) and n.id in {"list", "List", "dict", "Dict", "set", "Set", "tuple", "Tuple"}:
                    raise TranspileError(f"{sym.name}.{st.target.id}: a container field of a class dependency is a body "
                                         "parameter in FastAPI: not supported", st, msrc)
            default, cons = None, []
            v = st.value
            if isinstance(v, ast.Call) and (dotted(v.func) or "").split(".")[-1] == "Field":
                if v.args:
                    default = v.args[0]
                for kw in v.keywords:
                    if kw.arg == "default":
                        default = kw.value
                    elif kw.arg in self.CLASS_DEP_CONS:
                        cons.append(f"{kw.arg}={ast.unparse(kw.value)}")
                    elif kw.arg not in self.CLASS_DEP_DOC:
                        raise TranspileError(f"{sym.name}.{st.target.id}: Field({kw.arg}=) is not supported in a class dependency", kw, msrc)
                if isinstance(default, ast.Constant) and default.value is Ellipsis:
                    default = None
            elif v is not None:
                default = v
            ann = ast.unparse(st.annotation)
            p = f"{st.target.id}: Annotated[{ann}, Query({', '.join(cons)})]" if cons else f"{st.target.id}: {ann}"
            params.append(p + (f" = {ast.unparse(default)}" if default is not None else ""))
            names.append(st.target.id)
        code = (f"def {name}(*, {', '.join(params)}):\n" if params else f"def {name}():\n") + \
            f"    return {sym.name}({', '.join(f'{n}={n}' for n in names)})\n"
        fn = ast.parse(code).body[0]
        for n in ast.walk(fn):
            if "lineno" in n._attributes:
                n.lineno, n.end_lineno, n.col_offset, n.end_col_offset = cls.lineno, cls.lineno, 0, 0
        m.defs[name] = fn
        return Sym(sym.module, name)

    def plain_class_dependency(self, sym: Sym, src: str) -> Sym:
        """`Depends(CommonQueryParams)` on a plain project class: FastAPI reads the parameters from its `__init__`
        signature (without `self`) and calls the class. Compiled as that function (its parameters the
        `__init__`'s own nodes, so errors point there), returning `CommonQueryParams(**params)`. The `__init__`
        may be inherited (`inspect.signature` reads the first one of the MRO) from a plain base: the function is
        then written in the base's module, where its annotations and defaults are read, and calls the class through
        an import of it there (`class_alias`). Refused: other bases, a decorated `__init__`, `*args`/`**kwargs`."""
        cls = self.p.ix.definition(sym)
        msrc = self.p.src(sym.module)
        owner, init = self.dep_class_member(sym, "__init__", msrc)
        home = owner.module if owner is not None else sym.module
        name = f"__py2axum_depends_{sym.name}" if home == sym.module else f"__py2axum_depends_{mod_ident(sym.module)}__{sym.name}"
        m = self.p.ix.module(home)
        if name in m.defs:
            return Sym(home, name)
        if init is None:
            args = ast.arguments(posonlyargs=[], args=[], vararg=None, kwonlyargs=[], kw_defaults=[], kwarg=None, defaults=[])
        else:
            a = init.args
            if init.decorator_list or a.vararg or a.kwarg or a.posonlyargs:
                raise TranspileError(f"{sym.name}.__init__: decorators, *args, **kwargs or positional-only parameters "
                                     "are not supported in a class dependency", init, msrc)
            args = ast.arguments(posonlyargs=[], args=a.args[1:], vararg=None, kwonlyargs=a.kwonlyargs,
                                 kw_defaults=a.kw_defaults, kwarg=None, defaults=a.defaults)
        names = [x.arg for x in args.args + args.kwonlyargs]
        call = ast.Call(func=ast.Name(self.class_alias(home, sym), ast.Load()), args=[],
                        keywords=[ast.keyword(n, ast.Name(n, ast.Load())) for n in names])
        fn = ast.FunctionDef(name=name, args=args, body=[ast.Return(call)], decorator_list=[], returns=None,
                             type_params=[])
        anchor = init or cls
        for n in ast.walk(fn.body[0]):
            if "lineno" in n._attributes:
                n.lineno, n.end_lineno, n.col_offset, n.end_col_offset = anchor.lineno, anchor.lineno, 0, 0
        fn.lineno, fn.end_lineno, fn.col_offset, fn.end_col_offset = anchor.lineno, anchor.end_lineno, 0, 0
        fn.py2axum_noframe = True
        m.defs[name] = fn
        return Sym(home, name)

    def class_alias(self, home: str, sym: Sym) -> str:
        """The name `sym` (a class) has in `home`: its own there, else a static import of it written into `home`'s
        index only (`__py2axum_cls_...`: nothing runs, no import cycle), for a function compiled in a base's module
        that calls the subclass."""
        if home == sym.module:
            return sym.name
        alias = f"__py2axum_cls_{mod_ident(sym.module)}__{sym.name}"
        self.p.ix.module(home).imports.setdefault(alias, ("from", sym.module, sym.name))
        return alias

    def dep_class_member(self, sym: Sym, attr: str, msrc: str):
        """(defining class, function) of `attr` along the MRO of a plain class dependency (None, None: not
        defined), as `inspect.signature` finds it. Each class: no decorator, keywords `metaclass=ABCMeta` only,
        bases `ABC`/`object` and at most one plain project class; no `__new__` (the signature would be its)."""
        cur, seen = sym, set()
        while cur is not None and cur not in seen:
            seen.add(cur)
            d = self.p.ix.definition(cur)
            if (d.decorator_list or not all(self.p.abcmeta_kw(cur, k) for k in d.keywords)
                    or cur in self.p.fe.model_syms or cur in self.p.fe.schema_syms or cur in self.p.fe.dataclass_syms):
                raise TranspileError(f"{sym.name}: a class dependency must be a plain class (no decorator, no class "
                                     f"keyword but metaclass=ABCMeta) or a Pydantic model ({cur.name})", d, msrc)
            for st in d.body:
                tgt = st.name if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)) else None
                if tgt is None and isinstance(st, (ast.Assign, ast.AnnAssign)):
                    ts = st.targets if isinstance(st, ast.Assign) else [st.target]
                    if any(isinstance(t, ast.Name) and t.id in {attr, "__new__", "__signature__"} for t in ts):
                        raise TranspileError(f"{cur.name}: `{ast.unparse(ts[0])}` assigned in the class body of a "
                                             "class dependency is not supported (FastAPI reads its signature)", st,
                                             self.p.src(cur.module))
                if tgt == "__new__":
                    raise TranspileError(f"{cur.name}.__new__: a class dependency defining __new__ is not supported",
                                         st, self.p.src(cur.module))
                if tgt == attr:
                    return cur, st
            nxt = None
            for b in d.bases:
                t = self.p.resolve(cur.module, b)
                if (isinstance(t, Ext) and t.dotted == "abc.ABC") or dotted(b) == "object":
                    continue
                if (nxt is None and isinstance(t, Sym) and t != cur and isinstance(self.p.ix.definition(t), ast.ClassDef)
                        and self.p.is_plain_class(t)):
                    nxt = t
                    continue
                raise TranspileError(f"{sym.name}: a class dependency's bases may be `ABC`, `object` and one plain class "
                                     f"of the project (`{ast.unparse(b)}` of {cur.name})", b, self.p.src(cur.module))
            cur = nxt
        return None, None

    def method_dependency(self, node: ast.AST, module: str, scope, src: str) -> Sym | None:
        """`Depends(Service.async_init)`: a classmethod (FastAPI reads its signature without `cls`) or a staticmethod
        of a plain project class, found along its MRO. Compiled as a function of the class's module with that
        signature, returning `Service.async_init(**params)` (awaited if async). None when `node` is not
        `<project class>.<name>`."""
        if not isinstance(node, ast.Attribute):
            return None
        csym = self.p.resolve(module, node.value, scope)
        if not (isinstance(csym, Sym) and isinstance(self.p.ix.definition(csym), ast.ClassDef)):
            return None
        msrc = self.p.src(csym.module)
        if csym in self.p.fe.schema_syms or csym in self.p.fe.model_syms or csym in self.p.fe.dataclass_syms:
            raise TranspileError(f"Depends({ast.unparse(node)}): a method of a model or dataclass as a dependency is not "
                                 "supported", node, src)
        owner, meth = self.dep_class_member(csym, node.attr, msrc)
        if meth is None:
            raise TranspileError(f"Depends({ast.unparse(node)}): {csym.name} has no method `{node.attr}`", node, src)
        # written in the module defining the method (its signature's names are read there)
        home = owner.module
        name = (f"__py2axum_depmeth_{csym.name}_{node.attr}" if home == csym.module
                else f"__py2axum_depmeth_{mod_ident(csym.module)}__{csym.name}_{node.attr}")
        m = self.p.ix.module(home)
        if name in m.defs:
            return Sym(home, name)
        osrc = self.p.src(owner.module)
        decos = [dotted(d) for d in meth.decorator_list]
        if decos not in (["classmethod"], ["staticmethod"]) or any(
                self.p.resolve(owner.module, d) is not None for d in meth.decorator_list):
            raise TranspileError(f"Depends({ast.unparse(node)}): a method dependency must be a @classmethod or a "
                                 "@staticmethod (an unbound instance method needs `self`)", meth, osrc)
        a = meth.args
        cm = decos == ["classmethod"]
        if a.vararg or a.kwarg or a.posonlyargs or (cm and not a.args) or has_yield(meth):
            raise TranspileError(f"{owner.name}.{meth.name}: *args, **kwargs, positional-only parameters or `yield` are "
                                 "not supported in a method dependency", meth, osrc)
        pargs = a.args[1:] if cm else a.args
        ndef = len(a.defaults)
        defaults = a.defaults[max(0, ndef - len(pargs)):] if cm else a.defaults
        args = ast.arguments(posonlyargs=[], args=pargs, vararg=None, kwonlyargs=a.kwonlyargs,
                             kw_defaults=a.kw_defaults, kwarg=None, defaults=defaults)
        names = [x.arg for x in args.args + args.kwonlyargs]
        ret = ast.Call(func=ast.Attribute(ast.Name(self.class_alias(home, csym), ast.Load()), node.attr, ast.Load()), args=[],
                       keywords=[ast.keyword(n, ast.Name(n, ast.Load())) for n in names])
        if isinstance(meth, ast.AsyncFunctionDef):
            ret = ast.Await(ret)
        kind = ast.AsyncFunctionDef if isinstance(meth, ast.AsyncFunctionDef) else ast.FunctionDef
        fn = kind(name=name, args=args, body=[ast.Return(ret)], decorator_list=[], returns=None, type_params=[])
        for n in ast.walk(fn.body[0]):
            if "lineno" in n._attributes:
                n.lineno, n.end_lineno, n.col_offset, n.end_col_offset = meth.lineno, meth.lineno, 0, 0
        fn.lineno, fn.end_lineno, fn.col_offset, fn.end_col_offset = meth.lineno, meth.end_lineno, 0, 0
        fn.py2axum_noframe = True
        m.defs[name] = fn
        return Sym(home, name)

    def library_dependency(self, node, module: str, scope, depth: int = 0) -> str:
        """Why a dependency that is a library's object is refused (appended to the message): its function, a global
        the project builds by calling a library (`get_current_user = fastapi_users.current_user()`), an instance of
        a project subclass of a library class. FastAPI reads the signature of what the library makes, often built at
        run time (fastapi-users: makefun)."""
        lib = "; leave the route to Python: --python-side auto)"
        while isinstance(node, (ast.Call, ast.Subscript)):
            node = node.func if isinstance(node, ast.Call) else node.value
        t = self.p.resolve(module, node, scope) if isinstance(node, (ast.Name, ast.Attribute)) else None
        if isinstance(t, Ext):
            return (f" (`{t.dotted}`, of the library {t.package}: FastAPI reads the signature of what the library "
                    f"makes, which the translator does not have{lib}")
        d = self.p.ix.definition(t) if isinstance(t, Sym) else None
        if depth < 4 and isinstance(d, (ast.Assign, ast.AnnAssign)) and d.value is not None:
            inner = self.library_dependency(d.value, t.module, None, depth + 1)
            if inner:
                return inner
        if depth < 4 and isinstance(d, ast.ClassDef):
            for b in d.bases:
                bt = self.p.resolve(t.module, b.value if isinstance(b, ast.Subscript) else b)
                if isinstance(bt, Ext):
                    return (f" (an instance of {t.name}, a subclass of `{bt.dotted}` of the library {bt.package}: "
                            f"FastAPI calls the library's `__call__`{lib}")
                if isinstance(bt, Sym) and bt != t:
                    inner = self.library_dependency(b, t.module, None, depth + 1)
                    if inner:
                        return inner
        if t is None and isinstance(node, ast.Attribute):
            inner = self.library_dependency(node.value, module, scope, depth + 1)
            if inner:
                return inner
            if depth == 0:
                return (" (a method bound to an instance is not supported: make it a module function, a classmethod "
                        "or a staticmethod)")
        return ""

    def lambda_dependency(self, lam: ast.Lambda, module: str, scope, src: str) -> Sym:
        """`Depends(lambda: manager)`: FastAPI reads the lambda's signature and calls it (one object per lambda: its
        own cache key). Compiled as a module function of that signature returning the lambda's body; the lambda must
        be written at module level or in the signature of a module-level function (its names are then globals)."""
        if scope is not None and self.p.ix.definition(Sym(module, getattr(scope, "name", ""))) is not scope:
            raise TranspileError("Depends(lambda ...) inside a nested function is not supported (it reads that "
                                 "function's locals)", lam, src)
        a = lam.args
        if a.vararg or a.kwarg or a.posonlyargs:
            raise TranspileError("Depends(lambda ...): *args, **kwargs or positional-only parameters are not "
                                 "supported", lam, src)
        name = f"__py2axum_deplambda_{lam.lineno}_{lam.col_offset}"
        m = self.p.ix.module(module)
        if name not in m.defs:
            fn = ast.FunctionDef(name=name, args=copy.deepcopy(a), body=[ast.Return(copy.deepcopy(lam.body))],
                                 decorator_list=[], returns=None, type_params=[])
            ast.copy_location(fn, lam)
            fn.end_lineno = lam.end_lineno
            ast.fix_missing_locations(fn)
            fn.py2axum_name = fn.py2axum_qualname = "<lambda>"
            m.defs[name] = fn
        return Sym(module, name)

    def instance_dependency(self, sym: Sym, src: str) -> Sym | None:
        """`checker = Checker("bar")` then `Depends(checker)`: FastAPI reads the parameters of `Checker.__call__`
        (without `self`) and calls the instance. Compiled as that function, returning `checker(**params)`.
        None when the global is not an instance of a plain class of the same module with a `__call__`."""
        name = f"__py2axum_depcall_{sym.name}"
        m = self.p.ix.module(sym.module)
        if name in m.defs:
            return Sym(sym.module, name)
        st = self.p.ix.definition(sym)
        value = st.value
        if not (isinstance(value, ast.Call) and isinstance(value.func, ast.Name)):
            return None
        cls = self.p.ix.definition(Sym(sym.module, value.func.id))
        if not isinstance(cls, ast.ClassDef) or Sym(sym.module, cls.name) in self.p.fe.schema_syms:
            return None
        msrc = self.p.src(sym.module)
        call = next((x for x in cls.body if isinstance(x, (ast.FunctionDef, ast.AsyncFunctionDef)) and x.name == "__call__"), None)
        if call is None:
            return None
        a = call.args
        if cls.bases or cls.keywords or cls.decorator_list:
            raise TranspileError(f"{sym.name}: an instance dependency must be of a plain class (no base, no decorator)",
                                 cls, msrc)
        if call.decorator_list or a.vararg or a.kwarg or a.posonlyargs or not a.args or has_yield(call):
            raise TranspileError(f"{cls.name}.__call__: decorators, *args, **kwargs, positional-only parameters or "
                                 "`yield` are not supported in an instance dependency", call, msrc)
        args = ast.arguments(posonlyargs=[], args=a.args[1:], vararg=None, kwonlyargs=a.kwonlyargs,
                             kw_defaults=a.kw_defaults, kwarg=None, defaults=a.defaults)
        names = [x.arg for x in args.args + args.kwonlyargs]
        ret = ast.Call(func=ast.Name(sym.name, ast.Load()), args=[],
                       keywords=[ast.keyword(n, ast.Name(n, ast.Load())) for n in names])
        if isinstance(call, ast.AsyncFunctionDef):
            ret = ast.Await(ret)
        kind = ast.AsyncFunctionDef if isinstance(call, ast.AsyncFunctionDef) else ast.FunctionDef
        fn = kind(name=name, args=args, body=[ast.Return(ret)], decorator_list=[], returns=None, type_params=[])
        for n in ast.walk(fn.body[0]):
            if "lineno" in n._attributes:
                n.lineno, n.end_lineno, n.col_offset, n.end_col_offset = call.lineno, call.lineno, 0, 0
        fn.lineno, fn.end_lineno, fn.col_offset, fn.end_col_offset = call.lineno, call.end_lineno, 0, 0
        fn.py2axum_noframe = True
        m.defs[name] = fn
        return Sym(sym.module, name)

    def factory_dependency(self, call: ast.Call, module: str, scope, src: str, site: str = "") -> Sym | None:
        """`Depends(make_dep(Query([], alias="ids"), int))`: FastAPI reads the signature of the function the factory
        returns, its defaults and annotations holding the factory's arguments. Compiled as a copy of that inner
        function (in the factory's module) where each factory parameter is replaced by its argument, `<marker>.default`
        by the marker's default. The factory may only define functions and return one of them; its arguments are
        constants, builtin types and FastAPI markers of constants, named alike in both modules; a `*roles` parameter
        holds the tuple of the extra positional ones. A factory whose body only returns another factory's call
        (`def require_admin(): return require_role("admin")`) is that call, its arguments substituted. None when
        `call` is not a call of a project function."""
        f_sym = self.p.resolve(module, call.func, scope)
        fdef = self.p.ix.definition(f_sym) if isinstance(f_sym, Sym) else None
        if not isinstance(fdef, ast.FunctionDef):
            return None
        fsrc = self.p.src(f_sym.module)
        # one function per call site (FastAPI: one object per call, its own cache key), the outer one first
        site = site or f"{call.lineno}_{call.col_offset}"
        name = f"__py2axum_depfactory_{f_sym.name}_{site}"
        m = self.p.ix.module(f_sym.module)
        if name in m.defs:
            return Sym(f_sym.module, name)

        def refuse(msg, node=call, where=src):
            raise TranspileError(f"Depends({ast.unparse(call)}): {msg}", node, where)

        body = list(fdef.body)
        if body and isinstance(body[0], ast.Expr) and isinstance(body[0].value, ast.Constant) and isinstance(body[0].value.value, str):
            body = body[1:]
        chained = (len(body) == 1 and isinstance(body[0], ast.Return) and isinstance(body[0].value, ast.Call)
                   and isinstance(self.p.ix.definition(self.p.resolve(f_sym.module, body[0].value.func)
                                                       if isinstance(body[0].value.func, (ast.Name, ast.Attribute)) else None),
                                  ast.FunctionDef))
        if (fdef.decorator_list or fdef.args.kwarg or fdef.args.posonlyargs or not body
                or (not chained and (not isinstance(body[-1], ast.Return) or not isinstance(body[-1].value, ast.Name)))
                or not all(isinstance(x, (ast.FunctionDef, ast.AsyncFunctionDef)) for x in body[:-1])):
            refuse("a dependency factory must only define functions and return one of them (no decorator, "
                   "*args or **kwargs)", fdef, fsrc)
        nested = {x.name: x for x in body[:-1]}
        inner = None if chained else nested.get(body[-1].value.id)
        if not chained and (inner is None or inner.decorator_list):
            refuse("the factory must return one of the functions it defines, undecorated", fdef, fsrc)
        # the factory's parameters bound to the call's arguments (its defaults otherwise)
        a = fdef.args
        params = [x.arg for x in a.args] + [x.arg for x in a.kwonlyargs]
        bound: dict[str, ast.expr] = {}
        if any(isinstance(x, ast.Starred) for x in call.args) or any(k.arg is None for k in call.keywords):
            refuse("`*`/`**` expansion in a dependency factory call is not supported")
        if len(call.args) > len(a.args) and a.vararg is None:
            refuse(f"{fdef.name}() takes {len(a.args)} positional arguments")
        for p_, v in zip(a.args, call.args):
            bound[p_.arg] = v
        if a.vararg is not None:
            bound[a.vararg.arg] = ast.copy_location(ast.Tuple(list(call.args[len(a.args):]), ast.Load()), call)
        for k in call.keywords:
            if k.arg not in params or k.arg in bound:
                refuse(f"{fdef.name}() got an unexpected or repeated keyword argument '{k.arg}'")
            bound[k.arg] = k.value
        defaults = dict(zip([x.arg for x in a.args][len(a.args) - len(a.defaults):], a.defaults))
        defaults.update({x.arg: d for x, d in zip(a.kwonlyargs, a.kw_defaults) if d is not None})
        for p_ in params:
            if p_ not in bound:
                if p_ not in defaults:
                    refuse(f"{fdef.name}() missing required argument '{p_}'")
                bound[p_] = defaults[p_]
        # the arguments: constants, builtin types, FastAPI markers of those, the same objects in both modules
        for v in bound.values():
            for n in ast.walk(v):
                if isinstance(n, (ast.Constant, ast.List, ast.Tuple, ast.Dict, ast.Set, ast.keyword, ast.Load,
                                  ast.UnaryOp, ast.USub, ast.Call)):
                    if isinstance(n, ast.Call) and (dotted(n.func) or "").split(".")[-1] not in self.PARAM_MARKERS - {"Depends", "Security"}:
                        refuse("a dependency factory's arguments may only be constants, builtin types and FastAPI "
                               "parameter markers (Query, Header...)", n)
                    continue
                if isinstance(n, (ast.Name, ast.Attribute)) and isinstance(n.ctx, ast.Load):
                    here, there = self.p.resolve(module, n, scope), self.p.resolve(f_sym.module, n)
                    if isinstance(n, ast.Name) and here is None and there is None and n.id in BUILTIN_TYPES:
                        continue
                    if here is not None and here == there:
                        continue
                    refuse(f"`{ast.unparse(n)}` does not name the same object in {f_sym.module} (where the factory is "
                           "defined)", n)
                    continue
                if isinstance(n, ast.Attribute):
                    continue
                refuse(f"unsupported argument `{ast.unparse(n)}` of a dependency factory", n)
        # no factory parameter rebound in the functions
        for fn in nested.values():
            for n in ast.walk(fn):
                if isinstance(n, ast.Name) and n.id in bound and not isinstance(n.ctx, ast.Load):
                    refuse(f"the factory parameter `{n.id}` is reassigned in {fn.name}()", n, fsrc)
                if isinstance(n, ast.arg) and n.arg in bound:
                    refuse(f"the factory parameter `{n.arg}` is shadowed in {fn.name}()", n, fsrc)
                if isinstance(n, (ast.Nonlocal, ast.Global)):
                    refuse(f"global/nonlocal in {fn.name}() is not supported in a dependency factory", n, fsrc)

        def marker_default(c: ast.Call):
            d = c.args[0] if c.args else next((k.value for k in c.keywords if k.arg == "default"), None)
            return d

        class Subst(ast.NodeTransformer):
            def visit_Attribute(self, node):
                if (isinstance(node.value, ast.Name) and node.value.id in bound and isinstance(bound[node.value.id], ast.Call)
                        and node.attr == "default" and isinstance(node.ctx, ast.Load)):
                    d = marker_default(bound[node.value.id])
                    if d is None:
                        refuse(f"`{node.value.id}.default`: the marker has no default", node, fsrc)
                    return ast.copy_location(copy.deepcopy(d), node)
                return self.generic_visit(node)

            def visit_Name(self, node):
                if node.id in bound and isinstance(node.ctx, ast.Load):
                    return ast.copy_location(copy.deepcopy(bound[node.id]), node)
                return node

        if chained:
            # `return require_role("admin", *roles)`: the inner factory's call, the outer's arguments in it
            inner_call = Subst().visit(copy.deepcopy(body[0].value))
            flat = []
            for x in inner_call.args:
                if isinstance(x, ast.Starred) and isinstance(x.value, (ast.Tuple, ast.List)):
                    flat += x.value.elts
                else:
                    flat.append(x)
            inner_call.args = flat
            return self.factory_dependency(inner_call, f_sym.module, None, fsrc, site)
        new = Subst().visit(copy.deepcopy(inner))
        new.name = name
        # its frames in tracebacks and logs: the returned function's (`make.<locals>.inner`)
        new.py2axum_name, new.py2axum_qualname = inner.name, f"{fdef.name}.<locals>.{inner.name}"
        # a marker left in the body (other than its default) is an object the runtime does not have
        for stmt in new.body:
            for n in ast.walk(stmt):
                if isinstance(n, ast.Call) and (dotted(n.func) or "").split(".")[-1] in self.PARAM_MARKERS:
                    refuse("a FastAPI marker argument is only supported as a parameter default or through its "
                           "`.default`", n, fsrc)
        # the factory's other functions, read by the returned one, nested in the copy
        siblings = [Subst().visit(copy.deepcopy(fn)) for fn in nested.values() if fn is not inner]
        new.body = siblings + new.body
        ast.fix_missing_locations(new)
        m.defs[name] = new
        return Sym(f_sym.module, name)

    PARAM_MARKERS = {"Query", "Path", "Body", "Header", "Cookie", "Form", "File", "Depends", "Security"}
    param_name: str | None = None

    def param_marker(self, node, module: str, scope):
        """A parameter's default or `Annotated` metadata naming a module variable that holds a marker
        (`RadiusQuery = Query(50.0, ge=5)` at module level, then `radius: float = RadiusQuery`): FastAPI reads the
        object, so the call it was built by stands for it (resolved in its own module). The variable must be bound
        once, by that statement, and never rebound with `global`."""
        if not isinstance(node, (ast.Name, ast.Attribute)):
            return node, module, scope
        t = self.p.resolve(module, node, scope)
        if not isinstance(t, Sym):
            return node, module, scope
        d = self.p.ix.definition(t)
        if not (isinstance(d, (ast.Assign, ast.AnnAssign)) and isinstance(d.value, ast.Call)
                and (len(d.targets) == 1 if isinstance(d, ast.Assign) else True)):
            return node, module, scope
        f = self.p.resolve(t.module, d.value.func)
        if not (isinstance(f, Ext) and f.package == "fastapi" and f.dotted.split(".")[-1] in self.PARAM_MARKERS):
            return node, module, scope
        if t in self.p.rebound_globals():
            raise TranspileError(f"`{t.name}` holds a FastAPI parameter marker that a function rebinds with `global`: "
                                 "not supported", node, self.p.src(module))
        if (f.dotted.split(".")[-1] not in {"Depends", "Security"} and self.param_name is not None
                and not any(k.arg in {"alias", "validation_alias"} for k in d.value.keywords)):
            # FastAPI sets the marker's alias to the name of the first parameter it builds (`field_info.alias = ...`):
            # shared by parameters of other names, the later ones read the first one's name
            seen = self.p.__dict__.setdefault("marker_names", {})
            first = seen.setdefault(t, (self.param_name, node, module))
            if first[0] != self.param_name:
                raise TranspileError(f"`{t.name}` (a FastAPI parameter marker without alias=) is the default of parameters "
                                     f"`{first[0]}` and `{self.param_name}`: FastAPI gives it the alias of the first one it "
                                     "builds, which depends on the registration order (give it an alias=, or one marker "
                                     "per name)", node, self.p.src(module))
        return d.value, t.module, None

    def params(self, fn, module: str, path_names: set[str], for_dep: bool = False) -> list[RParam]:
        out: list[RParam] = []
        src = self.p.src(module)
        for spec in fn_params(fn):
            if spec.kind in {"vararg", "kwarg"}:
                raise TranspileError(f"{fn.name}: *args/**kwargs are not supported in endpoints", fn, src)
        a = fn.args
        args = a.posonlyargs + a.args + a.kwonlyargs
        defaults = [None] * (len(a.posonlyargs + a.args) - len(a.defaults)) + list(a.defaults) + list(a.kw_defaults)
        for arg, default in zip(args, defaults):
            ann = arg.annotation
            amod, ascope = module, fn
            if ann is not None:
                ann, amod = self.p.expand_alias(ann, module, fn)
                if amod != module:
                    ascope = None
            metas: list[tuple[ast.AST, str, object]] = []  # (node, module, scope)
            if isinstance(ann, ast.Subscript) and (dotted(ann.value) or "").split(".")[-1] == "Annotated":
                elts = ann.slice.elts if isinstance(ann.slice, ast.Tuple) else [ann.slice]
                ann = elts[0]
                metas = [(m, amod, ascope) for m in elts[1:]]
            if default is not None:
                metas.append((default, module, fn))
            self.param_name = arg.arg
            metas = [self.param_marker(m, mm, ms) for m, mm, ms in metas]
            self.param_name = None

            def find(kinds):
                return next(((m, mm, ms) for m, mm, ms in metas if isinstance(m, ast.Call) and (dotted(m.func) or "").split(".")[-1] in kinds), (None, None, None))

            dep_call, dmod, dscope = find({"Depends", "Security"})
            pcall, pmod, pscope = find({"Query", "Path", "Body", "Header", "Cookie", "Form", "File"})
            name = arg.arg
            sync_sess = self.is_ext(amod, ann, ascope, "sqlalchemy.orm.Session", "sqlalchemy.orm.session.Session")
            if sync_sess and not (dep_call is not None and dep_call.args):
                raise TranspileError(f"{name}: a synchronous Session parameter needs Depends(<session dependency>)", arg, src)
            if sync_sess or self.is_ext(amod, ann, ascope, "sqlalchemy.ext.asyncio.AsyncSession", "sqlalchemy.ext.asyncio.session.AsyncSession"):
                cfg = None
                if dep_call is not None and dep_call.args:
                    t = self.p.resolve(dmod, dep_call.args[0], dscope)
                    if isinstance(t, Sym):
                        cfg = self.p.use_session_dep(t)
                rp = RParam(name, "session")
                rp.__dict__["session_cfg"] = cfg
                out.append(rp)
                continue
            if dep_call is not None and not dep_call.args and self.is_ext(
                    amod, ann, ascope, "fastapi.security.OAuth2PasswordRequestForm", "fastapi.security.oauth2.OAuth2PasswordRequestForm"):
                if for_dep:
                    raise TranspileError(f"{name}: OAuth2PasswordRequestForm in a dependency is not supported", arg, src)
                p = RParam(name, "oauth2form")
                ostr = self.p.td(ast.parse("str | None", mode="eval").body, module, {})
                p.security = (self.p.td(ast.parse("str", mode="eval").body, module, {}),
                              self.p.td(ast.parse("str | None", mode="eval").body, module, {"pattern": "^password$"}), ostr)
                out.append(p)
                continue
            if dep_call is not None:
                if not dep_call.args:
                    cls_t = self.p.resolve(amod, ann, ascope) if ann is not None else None
                    if (isinstance(cls_t, Sym) and cls_t not in self.p.fe.schema_syms
                            and isinstance(self.p.ix.definition(cls_t), ast.ClassDef)):
                        if dep_call.keywords:
                            raise TranspileError("Depends() options are not supported on a class dependency", dep_call, src)
                        out.append(RParam(name, "dep", dep=self.plain_class_dependency(cls_t, src)))
                        continue
                    if not (isinstance(cls_t, Sym) and cls_t in self.p.fe.schema_syms):
                        raise TranspileError(f"{fn.name}: Depends() without a callable is only supported for AsyncSession "
                                             "and Pydantic models", arg, src)
                    if dep_call.keywords:
                        raise TranspileError("Depends() options are not supported on a class dependency", dep_call, src)
                    out.append(RParam(name, "dep", dep=self.class_dependency(cls_t, src)))
                    continue
                t = self.p.resolve(dmod, dep_call.args[0], dscope)
                if isinstance(dep_call.args[0], ast.Call):
                    t = self.factory_dependency(dep_call.args[0], dmod, dscope, src) or t
                elif isinstance(dep_call.args[0], ast.Lambda):
                    t = self.lambda_dependency(dep_call.args[0], dmod, dscope, src)
                elif t is None and isinstance(dep_call.args[0], ast.Name):
                    t = self.p.factory_function(dmod, dscope, dep_call.args[0].id)
                elif t is None:
                    t = self.method_dependency(dep_call.args[0], dmod, dscope, src)
                sec = self.security_scheme(t, src) if isinstance(t, Sym) else None
                if sec is None and isinstance(dep_call.args[0], ast.Call):
                    sec = self.security_call(dep_call.args[0], dmod)
                if sec is not None:
                    if [k for k in dep_call.keywords if not (k.arg == "scopes" and self.depends_scopes(dep_call, src) is not None)]:
                        raise TranspileError("Depends(security_scheme, ...) options are not supported", dep_call, src)
                    out.append(RParam(name, "security", security=sec))
                    continue
                if (isinstance(t, Sym) and t not in self.p.fe.schema_syms
                        and isinstance(self.p.ix.definition(t), ast.ClassDef)):
                    t = self.plain_class_dependency(t, src)
                elif isinstance(t, Sym) and isinstance(self.p.ix.definition(t), (ast.Assign, ast.AnnAssign)):
                    t = self.instance_dependency(t, src) or t
                if not isinstance(t, Sym) or not isinstance(self.p.fn_node(t) if t in self.p.__dict__.get("nested", {}) else self.p.ix.definition(t),
                                                            (ast.FunctionDef, ast.AsyncFunctionDef)):
                    raise TranspileError(f"{fn.name}: dependency `{ast.unparse(dep_call.args[0])}` must be a project function "
                                         f"or class{self.library_dependency(dep_call.args[0], dmod, dscope)}", arg, src)
                out.append(RParam(name, "dep", dep=t, scope=self.depends_scope(dep_call, src),
                                  oauth=self.depends_scopes(dep_call, src)))
                continue
            if self.is_ext(amod, ann, ascope, "fastapi.WebSocket", "starlette.websockets.WebSocket", "fastapi.websockets.WebSocket"):
                out.append(RParam(name, "websocket"))
                continue
            if self.is_ext(amod, ann, ascope, "fastapi.security.SecurityScopes", "fastapi.security.oauth2.SecurityScopes"):
                out.append(RParam(name, "security_scopes"))
                continue
            if self.is_ext(amod, ann, ascope, "starlette.requests.HTTPConnection", "fastapi.requests.HTTPConnection"):
                out.append(RParam(name, "connection"))
                continue
            if self.is_ext(amod, ann, ascope, "fastapi.Request", "starlette.requests.Request", "fastapi.requests.Request"):
                out.append(RParam(name, "request"))
                continue
            if self.is_ext(amod, ann, ascope, "fastapi.Response", "starlette.responses.Response", "fastapi.responses.Response"):
                out.append(RParam(name, "response"))
                continue
            if self.is_ext(amod, ann, ascope, "fastapi.BackgroundTasks", "starlette.background.BackgroundTasks",
                           "fastapi.background.BackgroundTasks"):
                out.append(RParam(name, "background"))
                continue
            cons, opts = ({}, {})
            source = None
            if pcall is not None:
                cons, opts = self.p.field_cons(pcall, pmod, pscope)
                if default is not None and default is not pcall and not {"default", "default_factory"} & set(opts):
                    opts["default"] = default  # `x: Annotated[T, Query()] = default`
                if "validate_default" in opts:
                    raise TranspileError(f"{dotted(pcall.func)}(validate_default=) is not supported", opts["validate_default"], src)
                source = (dotted(pcall.func) or "").split(".")[-1].lower()
                if default is not None and default is not pcall and "default" not in opts and "default_factory" not in opts:
                    # `x: Annotated[T, Header(...)] = v`: the default is the parameter's own
                    opts["default"] = default
            elif default is not None:
                opts["default"] = default
            afters = []
            for meta, mmod, mscope in metas:
                # the other metadata of `Annotated[T, ...]`: constraints, documentation, AfterValidator, or refused
                if meta is not default and meta is not pcall and meta is not dep_call:
                    mt = self.p.resolve(mmod, meta.func, mscope) if isinstance(meta, ast.Call) else None
                    if is_ext(mt, "pydantic.AfterValidator", "pydantic.functional_validators.AfterValidator"):
                        if len(meta.args) != 1 or meta.keywords:
                            raise TranspileError("AfterValidator(...): one function, no options, is supported", meta, src)
                        fa = meta.args[0]
                        ft = self.p.resolve(mmod, fa, mscope) if isinstance(fa, (ast.Name, ast.Attribute)) else None
                        if not (isinstance(fa, ast.Lambda) or (isinstance(ft, Sym) and isinstance(
                                self.p.ix.definition(ft), (ast.FunctionDef, ast.AsyncFunctionDef)))):
                            raise TranspileError(f"AfterValidator({ast.unparse(fa)}) on a parameter: a lambda or a function of "
                                                 "the project is supported", meta, src)
                        if self.p.function_validator_info(meta.args[0], mmod, 1):
                            raise TranspileError("an AfterValidator taking `info` on a parameter is not supported", meta, src)
                        vsym = Sym(mmod, f"{fn.name}.<param_{name}_{len(afters)}>")
                        self.p.__dict__.setdefault("lambda_validators", {})[vsym] = meta.args[0]
                        afters.append(vsym)
                        continue
                    cons = {**cons, **self.p.annotated_meta(meta, mmod, mscope)}
            upload = self.upload_kind(amod, ann, ascope)
            if source in {"form", "file"} or upload:
                if for_dep:
                    raise TranspileError(f"{name}: Form()/File() parameters of a dependency are not supported", arg, src)
            file_bytes = source == "file" and upload is None and self.bytes_kind(amod, ann, ascope)
            if (source == "file" or upload) and not file_bytes:
                if upload is None:
                    raise TranspileError(f"{name}: File() parameters must be UploadFile, list[UploadFile], bytes or "
                                         "list[bytes]", arg, src)
                p = RParam(name, "file")
                p.list = upload == "list"
                if "default" in opts or "default_factory" in opts:
                    dv = opts.get("default")
                    if not (isinstance(dv, ast.Constant) and dv.value is None):
                        # a literal (`File(default=[])`): FastAPI hands a copy of it when no file is sent
                        try:
                            p.default = self.p.dflt(dv, module) if dv is not None else None
                        except TranspileError:
                            p.default = None
                        if p.default is None:
                            raise TranspileError(f"{name}: a File() default other than None or a literal is not supported",
                                                 arg, src)
                    p.required = False
                if "alias" in opts:
                    p.alias = self.p.const(opts["alias"], module, fn)
                out.append(p)
                continue
            if ann is None:
                # FastAPI: an unannotated parameter is `Any` (its raw str from the path, query...)
                if cons:
                    raise TranspileError(f"parameter `{name}`: constraints without a type annotation are not supported",
                                         arg, src)
                td = self.p._static_td(f"{RT}::pyd::TD::Any")
            else:
                td = self.p.td(ann, amod, cons, ascope)
            p = RParam(name, "query", td)
            if "default" in opts or "default_factory" in opts:
                p.required = False
                factory = "default_factory" in opts
                spec = self.p.default_spec(opts["default_factory" if factory else "default"], module, factory)
                if spec.startswith("Data("):
                    raise TranspileError(f"{name}: a default_factory taking data is not supported on a parameter", arg, src)
                if spec.startswith("Dyn("):
                    p.dyn_default = spec[4:-1]
                else:
                    p.default = spec[spec.index("(") + 1:-1]
            if "alias" in opts:
                p.alias = self.p.const(opts["alias"], module, fn)
            init = next(k for k, v in self.p.tds.items() if v == td)
            by_name = {v: k for k, v in self.p.tds.items()}

            def complex_td(i: str, depth=0) -> bool:
                # FastAPI's field_annotation_is_complex: model, mapping, sequence, or a union with one
                if any(i.startswith(f"{RT}::pyd::TD::{k}") for k in ("Schema", "Dict", "List", "Set", "Tuple", "Coll", "Tagged")):
                    return True
                if depth < 5 and (i.startswith(f"{RT}::pyd::TD::Optional") or i.startswith(f"{RT}::pyd::TD::Union")
                                  or i.startswith((f"{RT}::pyd::TD::Len(", f"{RT}::pyd::TD::Strict(",
                                                   f"{RT}::pyd::TD::StrictTop("))):
                    return any(complex_td(by_name[r], depth + 1) for r in re.findall(r"&(\w+)", i) if r in by_name)
                return False

            is_model = complex_td(init)
            is_dictlike = False
            if source in {"query", "header", "cookie", "form"} and init.startswith(f"{RT}::pyd::TD::Schema"):
                # a Pydantic model of parameters (`Annotated[Model, Query()]`): its fields read from the source
                if for_dep:
                    raise TranspileError(f"{name}: a model of {source} parameters in a dependency is not supported",
                                         arg, src)
                if p.required is False:
                    raise TranspileError(f"{name}: a default for a model of {source} parameters is not supported", arg, src)
                if afters:
                    raise TranspileError(f"{name}: AfterValidator on a model of {source} parameters is not supported", arg, src)
                p.kind, p.source = "pmodel", source
                for kw in pcall.keywords:
                    if kw.arg == "convert_underscores":
                        p.convert = bool(self.p.const(kw.value, module))
                out.append(p)
                continue
            p.afters = tuple(afters)
            if file_bytes:
                p.kind = "file_bytes"
            elif source == "form":
                p.kind = "form"
            elif source == "header":
                p.kind = "header"
                conv = True
                for kw in pcall.keywords:
                    if kw.arg == "convert_underscores":
                        conv = bool(self.p.const(kw.value, module))
                p.alias = p.alias or (name.replace("_", "-") if conv else name)
            elif source == "cookie":
                if is_model:
                    raise TranspileError(f"{name}: an optional or union model of cookies is not supported", pcall, src)
                p.kind = "cookie"
            elif source == "body" or (source is None and (is_model or is_dictlike) and name not in path_names):
                p.kind = "body"
                if opts.get("embed") is not None and self.p.const(opts["embed"], module):
                    p.kind = "body_embed"
            elif name in path_names or source == "path":
                p.kind = "path"
            if afters and p.kind not in {"query", "path", "header", "cookie"}:
                raise TranspileError(f"{name}: AfterValidator on a {p.kind.replace('_', ' ')} parameter is not supported "
                                     "(path, query, header and cookie parameters only)", arg, src)
            out.append(p)
        return out

    def check_param_models(self, fn, params: list[RParam], router_deps: bool, src: str) -> None:
        """FastAPI reads a model of parameters field by field only when it is the single parameter of its source
        among the route's and all its dependencies' (`request_params_to_args`, `request_body_to_args`); the
        flattened dependencies are not seen here, so a route with dependencies is refused."""
        for p in params:
            if p.kind != "pmodel":
                continue
            same = {"form": {"form", "file", "file_bytes", "oauth2form", "body", "body_embed"}}.get(p.source, {p.source})
            others = [q for q in params if q is not p and (q.kind in same or (q.kind == "pmodel" and q.source in same))]
            if others or router_deps or any(q.kind in {"dep", "security"} for q in params):
                raise TranspileError(f"{fn.name}: a model of {p.source} parameters is supported as the route's only "
                                     f"{p.source} parameter, without dependencies", fn, src)

    def bytes_kind(self, module, ann, scope) -> bool:
        """`bytes`, `bytes | None`, `list[bytes]`: what FastAPI reads a File() into (`is_bytes_or_nonable_bytes_annotation`,
        `is_bytes_sequence_annotation`)."""
        if ann is None:
            return False
        if isinstance(ann, ast.Name) and ann.id == "bytes" and not isinstance(self.p.resolve(module, ann, scope), Sym):
            return True
        if isinstance(ann, ast.BinOp) and isinstance(ann.op, ast.BitOr):
            parts = [x for x in (ann.left, ann.right) if not (isinstance(x, ast.Constant) and x.value is None)]
            return len(parts) == 1 and isinstance(parts[0], ast.Name) and parts[0].id == "bytes"
        if isinstance(ann, ast.Subscript) and (dotted(ann.value) or "").split(".")[-1] in {"list", "List"}:
            return isinstance(ann.slice, ast.Name) and ann.slice.id == "bytes"
        return False

    def upload_kind(self, module, ann, scope) -> str | None:
        """`UploadFile` -> "one", `UploadFile | None` -> "opt", `list[UploadFile]` -> "list"."""
        names = ("fastapi.UploadFile", "starlette.datastructures.UploadFile", "fastapi.datastructures.UploadFile")
        if ann is None:
            return None
        if self.is_ext(module, ann, scope, *names):
            return "one"
        if isinstance(ann, ast.BinOp) and isinstance(ann.op, ast.BitOr):
            parts = [x for x in (ann.left, ann.right) if not (isinstance(x, ast.Constant) and x.value is None)]
            if len(parts) == 1 and self.is_ext(module, parts[0], scope, *names):
                return "opt"
        if isinstance(ann, ast.Subscript) and (dotted(ann.value) or "").split(".")[-1] in {"list", "List"}:
            if self.is_ext(module, ann.slice, scope, *names):
                return "list"
        return None

    @staticmethod
    def depends_scope(call: ast.Call, src) -> str:
        """The options of `Depends(f, ...)`: `use_cache=True` and `scope=` ("request" by default for a
        generator: its exit code runs once the response is sent; "function": when the endpoint returns)."""
        scope = "request"
        for kw in call.keywords:
            if kw.arg == "use_cache":
                if not (isinstance(kw.value, ast.Constant) and kw.value.value is True):
                    raise TranspileError("Depends(use_cache=False) is not supported (the result is cached per request)", kw, src)
            elif kw.arg == "scope":
                if not (isinstance(kw.value, ast.Constant) and kw.value.value in ("function", "request")):
                    raise TranspileError('Depends(scope=) must be the literal "function" or "request"', kw, src)
                scope = kw.value.value
            elif kw.arg == "scopes" and (dotted(call.func) or "").split(".")[-1] == "Security":
                pass  # read by depends_scopes
            else:
                raise TranspileError(f"Depends({kw.arg}=) is not supported", kw, src)
        return scope

    @staticmethod
    def depends_scopes(call: ast.Call, src) -> tuple:
        """`Security(f, scopes=["a", "b"])`: its own OAuth2 scopes (a literal list of strings)"""
        for kw in call.keywords:
            if kw.arg == "scopes" and (dotted(call.func) or "").split(".")[-1] == "Security":
                v = kw.value
                if not (isinstance(v, (ast.List, ast.Tuple)) and all(isinstance(e, ast.Constant) and isinstance(e.value, str) for e in v.elts)):
                    raise TranspileError("Security(scopes=) must be a literal list of strings", kw, src)
                return tuple(e.value for e in v.elts)
        return ()

    def uses_scopes(self, sym: Sym) -> bool:
        """FastAPI's `_uses_scopes`: a dependency with a SecurityScopes parameter, a security scheme, or one with
        own scopes, here or below (its cache key then holds the scopes it is solved with)"""
        memo = self.p.__dict__.setdefault("uses_scopes", {})
        if sym in memo:
            return memo[sym]
        memo[sym] = False  # a cycle is FastAPI's own error
        fn = self.p.fn_node(sym)
        params = self.params(fn, sym.module, set(), for_dep=True)
        memo[sym] = any(p.kind in {"security_scopes", "security"} or (p.kind == "dep" and (p.oauth or self.uses_scopes(p.dep)))
                        for p in params)
        return memo[sym]

    def needs_body(self, params) -> bool:
        """FastAPI reads (and parses) the request body only when the route or one of its dependencies
        has a body parameter."""
        dep_body = self.p.__dict__.setdefault("dep_body", {})
        return any(p.kind in {"body", "body_embed"} or (p.kind == "dep" and dep_body.get(p.dep, False)) for p in params)

    def dep_overrides(self) -> dict[Sym, Sym]:
        """`app.dependency_overrides[dep] = other` written at module level: FastAPI then solves and calls `other`
        wherever `dep` is depended on (its own parameters, its own cache key)."""
        if "dep_overrides_map" in self.p.__dict__:
            return self.p.dep_overrides_map
        out: dict[Sym, Sym] = {}
        for m in self.fe.index.package_modules():
            for st in m.tree.body:
                if not dep_override_target(self.fe, m.name, st):
                    continue
                src = str(m.path)
                key = self.p.resolve(m.name, st.targets[0].slice)
                val = self.p.resolve(m.name, st.value)
                ok = lambda t: isinstance(t, Sym) and isinstance(self.p.ix.definition(t), (ast.FunctionDef, ast.AsyncFunctionDef))  # noqa: E731
                if not (ok(key) and ok(val)):
                    raise TranspileError("app.dependency_overrides[...] = ...: only a project function overridden by a "
                                         "project function is supported", st, src)
                out[key] = val
        self.p.dep_overrides_map = out
        return out

    def dep_solver(self, sym: Sym, scope: str = "request", parent: tuple = (), own: tuple = ()) -> str:
        """`dep_x(cx, body, errs) -> R<Option<V>>`: solve and call a dependency (cached per request, per scope:
        FastAPI's cache key holds the scope, and the OAuth2 scopes (`parent` + its `own`, without repeats) when it
        uses them)."""
        orig, sym = sym, self.dep_overrides().get(sym, sym)
        if orig != sym:
            name = self.dep_solver(sym, scope, parent, own)
            # what the routes read under the dependency they declare
            for table in ("dep_body", "dep_kinds"):
                t = self.p.__dict__.setdefault(table, {})
                if sym in t:
                    t[orig] = t[sym]
            return name
        # the caller depends on it on every call, cached or not (errors are attributed through the edges)
        if self.p.cur is not None:
            self.p.edges.setdefault(self.p.cur, set()).add(("dep", sym))
        errors = self.p.__dict__.setdefault("dep_errors", {})
        if sym in errors:
            raise errors[sym]  # every route using a refused dependency is blocked, not only the first
        current = tuple(dict.fromkeys(parent + own))
        oauth = None
        if own or self.uses_scopes(sym):
            oauth = tuple(sorted(set(current)))
            orders = self.p.__dict__.setdefault("dep_scope_orders", {})
            if orders.setdefault((sym, scope, oauth), current) != current:
                raise TranspileError(f"dependency {sym.name}: solved with the scopes {list(current)} and "
                                     f"{list(orders[(sym, scope, oauth)])} (same set, another order) in one app is not "
                                     "supported (FastAPI caches the first)", self.p.fn_node(sym), self.p.src(sym.module))
        else:
            current = ()
        dkey = (sym, scope) if oauth is None else (sym, scope, oauth)
        if dkey in self.p.deps:
            return self.p.deps[dkey]
        name = f"dep_{mod_ident(sym.module)}__{ident(sym.name)}" + ("__fnscope" if scope == "function" else "")
        if oauth is not None:
            name += "__sc" + ("_" + "_".join(ident(x) for x in oauth) if oauth else "")
            n = self.p.__dict__.setdefault("dep_scope_names", {})
            name = n.setdefault(dkey, name if name not in n.values() else f"{name}_{len(n)}")
        self.p.deps[dkey] = name
        prev, self.p.cur = self.p.cur, ("dep", sym)
        try:
            return self._dep_solver(sym, name, scope, current if oauth is not None else None)
        except TranspileError as e:
            del self.p.deps[dkey]
            errors[sym] = e
            raise
        finally:
            self.p.cur = prev

    def check_dep_yield(self, fn, sym: Sym, src) -> None:
        """A generator dependency: its `yield`s are statements, under `if`, `try` and `with`/`async with` blocks
        (FastAPI throws the request's exception in at the `yield`: the enclosing `except` clauses and context
        managers see it, as in the binary; a path without one is contextlib's "generator didn't yield"). Several
        only in exclusive branches of an `if`; none in a loop: a second `yield` reached is contextlib's
        "generator didn't stop", raised outside the generator."""
        yields = sorted((n for n in walk_scope(fn) if isinstance(n, (ast.Yield, ast.YieldFrom))),
                        key=lambda n: (n.lineno, n.col_offset))
        for y in yields:
            if isinstance(y, ast.YieldFrom):
                raise TranspileError(f"dependency {sym.name}: `yield from` is not supported", y, src)
        paths: dict[int, tuple] = {}  # id(yield) -> ((id(if), branch), ...) of the `if`s it is under

        def find(stmts, path) -> None:
            for st in stmts:
                if isinstance(st, ast.Expr) and isinstance(st.value, ast.Yield):
                    paths[id(st.value)] = path
                    continue
                if isinstance(st, (ast.For, ast.AsyncFor, ast.While)) and any(n in yields for n in walk_scope(st)):
                    y = next(n for n in walk_scope(st) if n in yields)
                    raise TranspileError(f"dependency {sym.name}: a `yield` in a loop is not supported (a second "
                                         "`yield` is contextlib's \"generator didn't stop\")", y, src)
                if isinstance(st, ast.If):
                    find(st.body, path + ((id(st), 0),))
                    find(st.orelse, path + ((id(st), 1),))
                elif isinstance(st, (ast.With, ast.AsyncWith)):
                    find(st.body, path)
                elif isinstance(st, ast.Try):
                    for b in (st.body, st.orelse, st.finalbody, *(h.body for h in st.handlers)):
                        find(b, path)

        find(fn.body, ())
        for y in yields:
            if id(y) not in paths:
                raise TranspileError(f"dependency {sym.name}: the `yield` must be a statement, under `if`, `try` or "
                                     "`with` blocks only", y, src)
        for k, a in enumerate(yields):
            for b in yields[k + 1:]:
                pa, pb = dict(paths[id(a)]), dict(paths[id(b)])
                if not any(i in pb and pb[i] != br for i, br in pa.items()):
                    raise TranspileError(f"dependency {sym.name}: two `yield`s on one path are not supported (only "
                                         "in exclusive branches of an `if`)", b, src)

    def _dep_solver(self, sym: Sym, name: str, scope: str, oauth: tuple | None = None) -> str:
        fn = self.p.fn_node(sym)
        src = self.p.src(sym.module)
        gen = has_yield(fn)
        if gen:
            self.check_dep_yield(fn, sym, src)
        params = self.params(fn, sym.module, set(), for_dep=True)
        if gen and scope == "request":
            for p in params:
                if p.kind == "dep" and p.scope == "function":
                    # FastAPI's DependencyScopeError when the app is built
                    raise TranspileError(f'dependency {sym.name} has a scope of "request", it cannot depend on '
                                         f'dependencies with scope "function" ({p.name})', fn, src)
        self.p.__dict__.setdefault("dep_body", {})[sym] = self.needs_body(params)
        body, args = self.solve(params, "__body", oauth or ())
        kinds = self.p.__dict__.setdefault("dep_kinds", {})
        kinds[sym] = {(p.kind, sym.name, src, fn.lineno) for p in params if p.kind != "dep"}
        for p in params:
            if p.kind == "dep":
                kinds[sym] |= kinds.get(p.dep, set())
        rust = self.p.function(sym, "depgen" if gen else "plain")
        key = sym.qual + ("#function" if scope == "function" else "") + (f"#scopes={' '.join(sorted(set(oauth)))}" if oauth is not None else "")
        if gen:
            caps = [f"c{i}" for i in range(len(args))]
            call = (f"{{ {' '.join(f'let {c} = {a}.clone();' for c, a in zip(caps, args))} "
                    f"{RT}::web::dep_gen(cx, {str(scope == 'function').lower()}, move |cx2, y| Box::pin(async move {{ {rust}(&cx2{''.join(', ' + c for c in caps)}, &y).await }})).await? }}")
        else:
            call = f"{rust}(cx{''.join(', ' + a for a in args)}).await?"
        self.p.items.append(
            f"async fn {name}(cx: &Cx, __body: &Option<V>, __errs: &mut Vec<{RT}::pyd::ErrDetail>) -> R<Option<V>> {{\n"
            f"    if let Some(v) = {RT}::web::dep_cache_get(cx, {rs(key)}) {{ return Ok(Some(v)); }}\n"
            f"    let __start = __errs.len();\n"
            + "".join(f"    {line}\n" for line in body)
            + f"    if __errs.len() > __start {{ return Ok(None); }}\n"
            f"    let __v = {call};\n"
            f"    {RT}::web::dep_cache_put(cx, {rs(key)}, __v.clone());\n"
            f"    Ok(Some(__v))\n}}"
        )
        return name

    def solve(self, params: list[RParam], body_var: str, oauth: tuple = ()) -> tuple[list[str], list[str]]:
        lines, args = [], []
        n_body = sum(p.kind in {"body", "body_embed"} for p in params)
        # FastAPI: sub-dependencies first (in declaration order), then path/query/header, then body
        for p in params:
            if p.kind == "dep":
                solver = self.dep_solver(p.dep, p.scope, oauth, p.oauth)
                lines.append(f"let p_{ident(p.name)} = {solver}(cx, {body_var}, __errs).await?.unwrap_or(V::None);")
            elif p.kind == "security":
                variant, auto_error, hooks = p.security
                if hooks:
                    fns = [f"Some({method_wrapper(self.p, hooks[h])} as {RT}::pyd::MethodFn)" if h in hooks else "None"
                           for h in self.SECURITY_HOOKS]
                    lines.append(f"let p_{ident(p.name)} = {RT}::web::security_with(cx, {RT}::web::Security::{variant}, "
                                 f"{str(auto_error).lower()}, {fns[0]}, {fns[1]}).await?;")
                else:
                    lines.append(f"let p_{ident(p.name)} = {RT}::web::security(cx, {RT}::web::Security::{variant}, {str(auto_error).lower()})?;")
            elif p.kind == "security_scopes":
                lines.append(f"let p_{ident(p.name)} = {RT}::web::security_scopes(&[{', '.join(rs(x) for x in oauth)}]);")
            elif p.kind == "session":
                cfg = p.__dict__.get("session_cfg")
                if cfg is None or len(set(self.p.__dict__.get("session_cfgs", {}).values())) < 2:
                    lines.append(f"let p_{ident(p.name)} = {RT}::session(cx).await?;")
                else:
                    lines.append(f"let p_{ident(p.name)} = {RT}::session_with(cx, {', '.join(str(x).lower() for x in cfg)}).await?;")
            elif p.kind == "request":
                lines.append(f"let p_{ident(p.name)} = {RT}::request(cx);")
            elif p.kind == "websocket":
                lines.append(f"let p_{ident(p.name)} = {RT}::ws::current(cx)?;")
            elif p.kind == "connection":
                lines.append(f"let p_{ident(p.name)} = {RT}::ws::connection(cx);")
            elif p.kind == "response":
                lines.append(f"let p_{ident(p.name)} = {RT}::response(cx);")
            elif p.kind == "background":
                lines.append(f"let p_{ident(p.name)} = {RT}::resp::background(cx);")
        for kind in ("path", "query", "header", "cookie"):
            for p in params:
                if p.kind == "pmodel" and p.source == kind:
                    lines.append(f"let p_{ident(p.name)} = {RT}::web::param_model(cx, {rs(kind)}, &{p.td}, "
                                 f"{str(p.convert).lower()}, None, __errs).await?;")
                if p.kind == kind and p.afters:
                    d = p.default or ("dflt_unbound" if p.dyn_default else "dflt_none")
                    fns = ", ".join(f"{method_wrapper(self.p, v)} as {RT}::pyd::MethodFn" for v in p.afters)
                    lines.append(
                        f"let p_{ident(p.name)} = {RT}::web::param_after(cx, {rs(kind)}, {rs(p.name)}, {rs(p.alias or p.name)}, "
                        f"&{p.td}, {str(p.required).lower()}, {d}, &[{fns}], __errs).await?;"
                    )
                elif p.kind == kind:
                    d = p.default or ("dflt_unbound" if p.dyn_default else "dflt_none")
                    lines.append(
                        f"let p_{ident(p.name)} = {RT}::web::param(cx, {rs(kind)}, {rs(p.name)}, {rs(p.alias or p.name)}, "
                        f"&{p.td}, {str(p.required).lower()}, {d}, __errs).await?;"
                    )
        for p in params:
            d = p.default or ("dflt_unbound" if p.dyn_default else "dflt_none")
            if p.kind == "body" and n_body == 1:
                lines.append(f"let p_{ident(p.name)} = {RT}::web::body_param(cx, {body_var}, &{p.td}, {str(p.required).lower()}, {d}, __errs).await?;")
            elif p.kind == "pmodel" and p.source == "form":
                lines.append(f"let p_{ident(p.name)} = {RT}::web::param_model(cx, \"form\", &{p.td}, true, Some(&__form), "
                             "__errs).await?;")
            elif p.kind == "form":
                lines.append(f"let p_{ident(p.name)} = {RT}::web::form_field(cx, &__form, {rs(p.alias or p.name)}, &{p.td}, "
                             f"{str(p.required).lower()}, {d}, __errs).await?;")
            elif p.kind == "file_bytes":
                lines.append(f"let p_{ident(p.name)} = {RT}::web::form_file_bytes(cx, &__form, {rs(p.alias or p.name)}, "
                             f"&{p.td}, {str(p.required).lower()}, {d}, __errs).await?;")
            elif p.kind == "file":
                lines.append(f"let p_{ident(p.name)} = {RT}::web::form_file(&__form, {rs(p.alias or p.name)}, "
                             f"{str(p.list).lower()}, {str(p.required).lower()}, {d}, __errs);")
            elif p.kind == "oauth2form":
                # fastapi.security.OAuth2PasswordRequestForm: its six Form() fields, then the object
                tstr, tgrant, topt = p.security
                fields = [("grant_type", tgrant, False, "dflt_none"), ("username", tstr, True, "dflt_none"),
                          ("password", tstr, True, "dflt_none"), ("scope", tstr, False, "dflt_empty_str"),
                          ("client_id", topt, False, "dflt_none"), ("client_secret", topt, False, "dflt_none")]
                names = []
                for fname, td, req, dflt in fields:
                    v = f"__of_{ident(p.name)}_{fname}"
                    names.append((fname, v))
                    lines.append(f"let {v} = {RT}::web::form_field(cx, &__form, {rs(fname)}, &{td}, {str(req).lower()}, {dflt}, __errs).await?;")
                pairs = ", ".join(f"({rs(f)}.to_string(), {v}.clone())" for f, v in names)
                lines.append(f"let p_{ident(p.name)} = {RT}::libs::oauth2_form(vec![{pairs}])?;")
            elif p.kind in {"body", "body_embed"}:
                lines.append(f"let p_{ident(p.name)} = {RT}::web::body_field(cx, {body_var}, {rs(p.alias or p.name)}, &{p.td}, {str(p.required).lower()}, {d}, __errs).await?;")
        for p in params:
            if p.dyn_default:
                v = f"p_{ident(p.name)}"
                lines.append(f"let {v} = if matches!({v}, V::Unbound) {{ {p.dyn_default}(cx, V::None, vec![]).await? }} else {{ {v} }};")
        args = [f"p_{ident(p.name)}" for p in params]
        return lines, args

    def route(self, fn, module: str, path: str, method: str, deco: ast.Call, router, mount: Mount, idx: int) -> tuple[str, str]:
        """Compile one route: returns (handler fn name, axum path)."""
        src = self.p.src(module)
        path_names = set(re.findall(r"{(\w+)(?::\w+)?}", path))
        if isinstance(fn, ast.FunctionDef):
            pass  # sync endpoints run in a threadpool in FastAPI; same observable result here
        response_model = None
        status = None
        resp_cls = None
        custom_resp = (None, None)  # a project response class: (media_type, render method)
        has_rm = False
        dump_opts: dict[str, ast.keyword] = {}
        for kw in deco.keywords:
            if kw.arg == "response_model":
                has_rm = True
                if not (isinstance(kw.value, ast.Constant) and kw.value.value is None):
                    response_model = self.p.td(kw.value, module, None, fn)
            elif kw.arg == "status_code":
                status = int(self.p.const(kw.value, module, fn))
            elif kw.arg in {"tags", "summary", "description", "responses", "deprecated", "operation_id",
                            "include_in_schema", "name", "response_description", "openapi_extra", "callbacks"}:
                pass  # OpenAPI metadata (openapi_extra, callbacks: the schema only, /docs is not served)
            elif kw.arg in {"response_model_include", "response_model_exclude", "response_model_exclude_unset",
                            "response_model_exclude_none"}:
                dump_opts[kw.arg] = kw
            elif kw.arg in ("dependencies", "path"):
                pass  # path=: the route's path (route_path)
            elif kw.arg == "response_class":
                # a returned Response is sent as is; any other value is encoded, then wrapped in this class
                t = self.p.resolve(module, kw.value, fn)
                if isinstance(t, Sym):
                    t, custom_resp = self.response_subclass(t)
                last = t.dotted.split(".")[-1] if isinstance(t, Ext) and t.dotted.split(".")[0] in {"fastapi", "starlette"} else None
                if last not in {"StreamingResponse", "FileResponse", "RedirectResponse", "JSONResponse", "Response",
                                "HTMLResponse", "PlainTextResponse", "EventSourceResponse", "ORJSONResponse"}:
                    raise TranspileError(f"response_class={ast.unparse(kw.value)} is not supported "
                                         "(only the fastapi/starlette response classes)", kw, src)
                resp_cls = last
            else:
                raise TranspileError(f"unsupported route option {kw.arg}=", kw, src)
        if resp_cls is None and not any(kw.arg == "response_class" for kw in deco.keywords):
            # APIRouter(default_response_class=): the route's own router, else the nearest router including it
            # (FastAPI fixes it on the route as it is added or included), else FastAPI(default_response_class=)
            own = self.fe.routers[router].resp_cls if router is not None else None
            node, where = (own, router.module) if own is not None else (mount.resp_cls or (None, None))
            if node is None and self.fe.__dict__.get("default_resp_custom") is not None:
                node, where = self.fe.default_resp_custom  # FastAPI(default_response_class=<project class>)
            if node is not None:
                t = self.p.resolve(where, node)
                if isinstance(t, Sym):
                    # a project subclass (`class ORJSONResponse(Response)` with media_type and render()): as on a route
                    t, custom_resp = self.response_subclass(t)
                last = t.dotted.split(".")[-1] if isinstance(t, Ext) and t.dotted.split(".")[0] in {"fastapi", "starlette"} else None
                if custom_resp != (None, None) and last in {"Response", "JSONResponse"}:
                    last = "JSONResponse" if last == "JSONResponse" else "Response"
                if last not in {"JSONResponse", "HTMLResponse", "PlainTextResponse", "ORJSONResponse"} and not (
                        custom_resp != (None, None) and last == "Response"):
                    raise TranspileError(f"APIRouter(default_response_class={ast.unparse(node)}) is not supported "
                                         "(JSONResponse, ORJSONResponse, HTMLResponse or PlainTextResponse)", node,
                                         self.p.src(where))
                resp_cls = last
            else:
                resp_cls = self.fe.__dict__.get("default_resp_cls")  # FastAPI(default_response_class=)
        gen = self.gen_endpoint(fn, module, resp_cls, has_rm, dump_opts, deco, src)
        if gen is not None and custom_resp != (None, None):
            if custom_resp[1] is not None or gen[0] != "Raw":
                raise TranspileError("a generator endpoint with a response class overriding render() (or not a "
                                     "StreamingResponse) is not supported", fn, src)
            if custom_resp[0] is not None:
                gen = (f"RawAs({rs(custom_resp[0])})", gen[1])
        if gen is not None and gen[0] != "Raw" and not fastapi_at_least(self.fe.index.root, (0, 141)):
            # FastAPI < 0.141: JSON Lines and SSE responses ignore status_code=, and include_router re-creates a
            # route with response_model=None, which drops its stream item type (items neither validated nor filtered)
            status = None
            if router is not None:
                gen = (gen[0], None)
        if gen is None and resp_cls == "EventSourceResponse":
            raise TranspileError("response_class=EventSourceResponse is only supported on a generator endpoint "
                                 "(one that yields its events)", deco, src)
        if not has_rm and fn.returns is not None and gen is None:
            r = fn.returns
            rt = self.p.resolve(module, r, fn)
            # FastAPI: a Response subclass as the return annotation is no response model
            if not (isinstance(rt, Ext) and rt.dotted.split(".")[0] in {"fastapi", "starlette"}
                    and rt.dotted.split(".")[-1] in RESPONSE_CLASSES):
                # FastAPI validates and filters the response with the return annotation
                response_model = self.p.td(r, module, None, fn)
        dump = self.resp_dump(dump_opts, response_model, module, fn, src) if dump_opts else None
        params = self.params(fn, module, path_names)
        lines: list[str] = []
        pre_deps: list[ast.AST] = []
        r = self.fe.routers.get(router) if router else None
        for node, file in mount.deps:
            pre_deps.append((node, file))
        self.check_param_models(fn, params, bool(mount.deps) or (r is not None and r.deps is not None), src)
        if r is not None and r.deps is not None:
            pre_deps.append((r.deps, r.file))
        for kw in deco.keywords:
            if kw.arg == "dependencies":
                pre_deps.append((kw, src))
        pre_syms: list[Sym] = []
        for node, nfile in pre_deps:
            for e, mod, file in self.dependency_elements(node.value, self.p.ix.module_of(nfile).name, nfile):
                if not (isinstance(e, ast.Call) and (dotted(e.func) or "").split(".")[-1] in {"Depends", "Security"} and e.args):
                    raise TranspileError("dependencies=[...] must contain Depends(function)", e, file)
                t = self.p.resolve(mod, e.args[0])
                if isinstance(e.args[0], ast.Call):
                    t = self.factory_dependency(e.args[0], mod, None, file) or t
                elif isinstance(e.args[0], ast.Lambda):
                    t = self.lambda_dependency(e.args[0], mod, None, file)
                elif t is None and isinstance(e.args[0], ast.Name):
                    # the decorator of an endpoint defined in the application factory reads the factory's names
                    t = self.p.factory_function(mod, fn, e.args[0].id) if file == src else None
                if t is None and not isinstance(e.args[0], (ast.Call, ast.Lambda)):
                    t = self.method_dependency(e.args[0], mod, None, file)
                if isinstance(t, Sym) and isinstance(self.p.ix.definition(t), (ast.Assign, ast.AnnAssign)):
                    t = self.instance_dependency(t, file) or t
                elif (isinstance(t, Sym) and t not in self.p.fe.schema_syms
                      and isinstance(self.p.ix.definition(t), ast.ClassDef)):
                    t = self.plain_class_dependency(t, file)
                if not (isinstance(t, Sym) and isinstance(self.p.fn_node(t) if t in self.p.__dict__.get("nested", {})
                                                          else self.p.ix.definition(t), (ast.FunctionDef, ast.AsyncFunctionDef))):
                    raise TranspileError(f"dependency `{ast.unparse(e.args[0])}` must be a project function"
                                         f"{self.library_dependency(e.args[0], mod, None)}", e, file)
                lines.append(f"let _ = {self.dep_solver(t, self.depends_scope(e, file), (), self.depends_scopes(e, file))}(cx, &__body, __errs).await?;")
                pre_syms.append(t)
        body_lines, args = self.solve(params, "&__body")
        lines += body_lines
        endpoint = self.endpoint_fn(fn, module, src)
        name = f"route_{idx}_{ident(fn.name)}"
        has_body = self.needs_body(params) or any(self.p.__dict__.get("dep_body", {}).get(t) for t in pre_syms)
        rm = f"Some(&{response_model})" if response_model else "None"
        form_route = any(p.kind in {"form", "file", "file_bytes", "oauth2form"} or (p.kind == "pmodel" and p.source == "form")
                         for p in params)
        self.p.items.append(
            f"/// {method.upper()} {self.p.fe.shown_path(path)}  (from {Path(src).name}:{fn.lineno} `{fn.name}`)\n"
            f"async fn {name}(cx: &Cx) -> R<axum::response::Response> {{\n"
            + (f"    let __body = {RT}::web::read_body(cx)?;\n" if has_body else "    let __body: Option<V> = None;\n")
            + (f"    let __form = {RT}::web::read_form(cx).await?;\n" if form_route else "")
            + f"    let mut __errv: Vec<{RT}::pyd::ErrDetail> = Vec::new();\n"
            f"    let __errs = &mut __errv;\n"
            + "".join(f"    {line}\n" for line in lines)
            # a form route's `exc.body` (a FormData) is not set: reading it is an AttributeError, never a wrong value
            + (f"    {RT}::web::check(cx, __errv)?;\n" if form_route else f"    {RT}::web::check_body(cx, __errv, &__body)?;\n")
            + (f"    {RT}::web::stream_list_ok(cx, {rs(endpoint)}, &{response_model});\n"
               if response_model and resp_cls in {None, "JSONResponse"} and self.p.__dict__.get("stream_lists", True)
               and has_list_return(fn) and dump is None else "")
            + (f"    let __ret = {endpoint}(cx{''.join(', ' + a for a in args)}).await?;\n" if gen is None else
               f"    let __ret = {{ let cx2 = cx.clone(); {' '.join(f'let __a{i} = {a}.clone();' for i, a in enumerate(args))} "
               f"{RT}::web::spawn_gen(move |y| Box::pin(async move {{ {endpoint}(&cx2{''.join(f', __a{i}' for i in range(len(args)))}, &y).await }})) }};\n")
            + (f"    {RT}::web::gen_response(cx, __ret, {RT}::web::GenMode::{gen[0]}, "
               f"{f'Some(&{gen[1]})' if gen[1] else 'None'}, {status or 200}).await\n}}" if gen is not None else
               f"    {RT}::web::respond(cx, __ret, {rm}, {status or 200}).await\n}}" if resp_cls in {None, "JSONResponse"} and dump is None and custom_resp == (None, None) else
               f"    {RT}::web::respond_custom(cx, __ret, {rm}, {f'Some({status})' if status else 'None'}, "
               f"{f'Some({rs(resp_cls)})' if resp_cls else 'None'}, "
               f"{f'Some({rs(custom_resp[0])})' if custom_resp[0] is not None else 'None'}, "
               f"{f'Some({method_wrapper(self.p, custom_resp[1])} as {RT}::pyd::MethodFn)' if custom_resp[1] else 'None'}, "
               f"&{dump or f'{RT}::web::RESP_DUMP'}).await\n}}")
        )
        handler = f"run_{name}"
        self.p.items.append(
            f"fn {handler}<'a>(cx: &'a Cx) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<axum::response::Response>> + Send + 'a>> "
            f"{{ Box::pin({name}(cx)) }}"
        )
        self.p.__dict__.setdefault("route_body", {})[handler] = bool(has_body or form_route)
        rc = self.route_class(fn, module, router)
        if rc is not None:
            handler = self.custom_route(name, handler, *rc)
        return handler, starlette_pattern(path, fn, src)

    def route_class(self, fn, module: str, router) -> tuple[ast.AST, str] | None:
        """The custom `route_class` of a route (its APIRouter's, or the app's `app.router.route_class = R` set
        before the route's decorator in the same module), as (the class expression, its module)."""
        fe = self.p.fe
        if router is not None:
            r = fe.routers.get(router)
            return (r.route_class, router.module) if r is not None and r.route_class is not None else None
        found = app_route_class(fe)
        if found is None:
            return None
        node, amod, line = found
        if module != amod:
            raise TranspileError("a route of the app outside the module that sets `app.router.route_class` is not "
                                 "supported (declare it on an APIRouter(route_class=...))", fn, self.p.src(module))
        first = fn.decorator_list[0].lineno if fn.decorator_list else fn.lineno
        return (node, amod) if first > line else None

    def custom_route(self, name: str, raw: str, cls: ast.AST, module: str) -> str:
        """A route of a custom APIRoute class: `R().get_route_handler()` (once per route, at its first request; FastAPI
        calls it when the route is created) makes the handler the request goes through; `super().get_route_handler()`
        in it is this route's own handler `raw`."""
        t = self.p.resolve(module, cls)
        d = self.p.ix.definition(t) if isinstance(t, Sym) else None
        if not (isinstance(d, ast.ClassDef) and self.p.is_plain_class(t)
                and any(isinstance(b, Ext) and b.dotted in ROUTE_BASES for b in (self.p.resolve(t.module, x) for x in d.bases))):
            raise TranspileError(f"route_class `{ast.unparse(cls)}`: a project subclass of APIRoute is expected", cls, self.p.src(module))
        if any(isinstance(x, (ast.FunctionDef, ast.AsyncFunctionDef)) and x.name not in {"get_route_handler"} for x in d.body):
            bad = next(x for x in d.body if isinstance(x, (ast.FunctionDef, ast.AsyncFunctionDef)) and x.name != "get_route_handler")
            raise TranspileError(f"{t.name}.{bad.name}(): a custom route class may only override get_route_handler()",
                                 bad, self.p.src(t.module))
        grh = next((x for x in d.body if isinstance(x, (ast.FunctionDef, ast.AsyncFunctionDef)) and x.name == "get_route_handler"), None)
        if grh is not None and grh.args.args:
            me = grh.args.args[0].arg
            used = next((n for n in ast.walk(grh) if isinstance(n, ast.Name) and n.id == me), None)
            if used is not None:
                raise TranspileError(f"{t.name}.get_route_handler(): `{me}` (the APIRoute: path, endpoint, dependant...) is not "
                                     "supported, only `super().get_route_handler()`", used, self.p.src(t.module))
        call = ast.Call(func=ast.Attribute(value=ast.Call(func=cls, args=[], keywords=[]), attr="get_route_handler", ctx=ast.Load()),
                        args=[], keywords=[])
        ast.copy_location(call, cls)
        ast.fix_missing_locations(call)
        mk = f"mk_handler_{name}"
        fc = FnCompiler(self.p, module, None, mk)
        code = fc.expr(call)
        self.p.items.append(
            f"static HANDLER_{name}: std::sync::OnceLock<V> = std::sync::OnceLock::new();\n"
            f"fn {mk}<'a>(cx: &'a Cx) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<V>> + Send + 'a>> "
            f"{{ Box::pin(async move {{ {' '.join(fc.lines)} Ok({code}) }}) }}\n"
            f"fn run_custom_{name}<'a>(cx: &'a Cx) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<axum::response::Response>> + Send + 'a>> "
            f"{{ Box::pin({RT}::web::custom_route(cx, &HANDLER_{name}, {mk})) }}"
        )
        self.p.__dict__.setdefault("route_raw", {})[f"run_custom_{name}"] = f"run_{name}"
        return f"run_custom_{name}"

    STREAM_ORIGINS = {"AsyncIterable", "AsyncIterator", "AsyncGenerator", "Iterable", "Iterator", "Generator"}

    RESPONSE_BASES = {"Response", "JSONResponse", "HTMLResponse", "PlainTextResponse", "StreamingResponse", "ORJSONResponse"}

    def response_subclass(self, sym: Sym) -> tuple:
        """`class CustomORJSONResponse(Response)` with `media_type = "..."` and/or `def render(self, content)` (not
        reading `self`): (the base class, (media_type, render method))."""
        node = self.p.ix.definition(sym)
        file = self.p.src(sym.module)
        base = self.p.resolve(sym.module, node.bases[0], None) if isinstance(node, ast.ClassDef) and len(node.bases) == 1 else None
        if not (isinstance(base, Ext) and base.dotted.split(".")[0] in {"fastapi", "starlette"}
                and base.dotted.split(".")[-1] in self.RESPONSE_BASES) or node.keywords or node.decorator_list:
            raise TranspileError(f"response_class={sym.name}: a subclass of one fastapi/starlette response class "
                                 f"({', '.join(sorted(self.RESPONSE_BASES))}) is expected", node, file)
        media, render = None, None
        for st in node.body:
            if isinstance(st, ast.Pass) or (isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant)):
                continue
            if (isinstance(st, (ast.Assign, ast.AnnAssign)) and len(getattr(st, "targets", [None])) == 1
                    and isinstance(st.targets[0] if isinstance(st, ast.Assign) else st.target, ast.Name)
                    and (st.targets[0] if isinstance(st, ast.Assign) else st.target).id == "media_type"
                    and isinstance(st.value, ast.Constant) and isinstance(st.value.value, str)):
                media = st.value.value
                continue
            a = getattr(st, "args", None)
            if (isinstance(st, ast.FunctionDef) and st.name == "render" and not st.decorator_list
                    and len(a.args) == 2 and not (a.posonlyargs or a.vararg or a.kwonlyargs or a.kwarg or a.defaults)):
                me = a.args[0].arg
                if any(isinstance(n, ast.Name) and n.id == me for n in ast.walk(st)):
                    raise TranspileError(f"response class {sym.name}.render: reading `{me}` is not supported", st, file)
                render = Sym(sym.module, f"{node.name}.render")
                continue
            raise TranspileError(f"response class {sym.name}: only `media_type = \"...\"` and `def render(self, content)` "
                                 "are supported in its body", st, file)
        return base, (media, render)

    def gen_endpoint(self, fn, module: str, resp_cls: str | None, has_rm: bool, dump_opts: dict, deco: ast.Call,
                     src: str) -> tuple[str, str | None] | None:
        """A generator endpoint (FastAPI >= 0.134): (mode, stream item TD or None), None for any other endpoint.
        The default response class streams JSON Lines, EventSourceResponse Server-Sent Events, StreamingResponse
        the items as they are; the item type is that of an `AsyncIterable[T]`-like return annotation (not for
        StreamingResponse, not ServerSentEvent)."""
        if not has_yield(fn):
            return None
        explicit = any(kw.arg == "response_class" for kw in deco.keywords)
        mode = ("Jsonl" if not explicit else "Sse" if resp_cls == "EventSourceResponse"
                else "Raw" if resp_cls == "StreamingResponse" else None)
        if mode is None:
            raise TranspileError(f"a generator endpoint with response_class={resp_cls} is not supported (the default "
                                 "class, EventSourceResponse or StreamingResponse)", fn, src)
        if dump_opts:
            raise TranspileError(f"{next(iter(dump_opts))}= on a generator endpoint is not supported", fn, src)
        item = None
        r = fn.returns
        if r is not None and not has_rm and mode != "Raw":
            base = r.value if isinstance(r, ast.Subscript) else r
            rt = self.p.resolve(module, base, fn)
            if isinstance(rt, Ext) and rt.dotted.split(".")[-1] in self.STREAM_ORIGINS and rt.dotted.split(".")[0] in {
                    "typing", "collections"}:
                arg = (r.slice.elts[0] if isinstance(r.slice, ast.Tuple) else r.slice) if isinstance(r, ast.Subscript) else None
                at = self.p.resolve(module, arg, fn) if arg is not None else None
                if arg is not None and not is_ext(at, "fastapi.sse.ServerSentEvent"):
                    item = self.p.td(arg, module, None, fn)
                elif arg is None:
                    item = self.p._static_td(f"{RT}::pyd::TD::Any")
        return mode, item

    def resp_dump(self, opts: dict[str, ast.keyword], response_model: str | None, module: str, fn, src) -> str:
        """`response_model_include=` / `_exclude=` (literal field names of a model response_model),
        `_exclude_unset=`, `_exclude_none=` -> a `static RespDump`."""
        if response_model is None:
            # FastAPI hands them to the response field only: without one they have no effect
            return f"{RT}::web::RESP_DUMP"
        init = next(k for k, v in self.p.tds.items() if v == response_model)
        m = re.fullmatch(rf"{re.escape(RT)}::pyd::TD::Schema\(&(\w+)\)", init)
        info = next((i for i in self.p.schemas.values() if m and i.rust == m[1]), None)
        sets = {}
        for key in ("response_model_include", "response_model_exclude"):
            if key not in opts:
                continue
            v = opts[key].value
            if isinstance(v, ast.Constant) and v.value is None:
                continue
            if not (isinstance(v, (ast.Set, ast.List, ast.Tuple))
                    and all(isinstance(e, ast.Constant) and isinstance(e.value, str) for e in v.elts)):
                raise TranspileError(f"{key}= must be a literal set or list of field names (nested selections are "
                                     "not supported)", opts[key], src)
            if info is None:
                raise TranspileError(f"{key}= is only supported with a Pydantic model as the response model", opts[key], src)
            names = [e.value for e in v.elts]
            fields = {f["name"] for f in info.fields}
            if unknown := [n for n in names if n not in fields]:
                raise TranspileError(f"{key}=: `{unknown[0]}` is not a field of {info.sym.name} (computed fields and "
                                     "unknown names are not supported)", opts[key], src)
            sets[key] = "Some(&[" + ", ".join(rs(n) for n in sorted(set(names))) + "])"
        flags = {}
        for key in ("response_model_exclude_unset", "response_model_exclude_none"):
            flags[key] = "false"
            if key in opts:
                v = self.p.const(opts[key].value, module, fn)
                if not isinstance(v, bool):
                    raise TranspileError(f"{key}= must be True or False", opts[key], src)
                flags[key] = str(v).lower()
        name = f"RESP_DUMP_{len(self.p.__dict__.setdefault('resp_dumps', [])) + 1}"
        self.p.resp_dumps.append(name)
        self.p.items.append(
            f"static {name}: {RT}::web::RespDump = {RT}::web::RespDump {{ include: {sets.get('response_model_include', 'None')}, "
            f"exclude: {sets.get('response_model_exclude', 'None')}, exclude_unset: {flags['response_model_exclude_unset']}, "
            f"exclude_none: {flags['response_model_exclude_none']} }};")
        return name

    def endpoint_fn(self, fn, module: str, src: str) -> str:
        """The compiled endpoint function: module level, or defined in the application factory."""
        endpoint = self.p.function(Sym(module, fn.name)) if self._toplevel(fn, module) else None
        if endpoint is None and fn in self.p.ix.module(module).tree.body:
            # a module-level function shadowed by a later definition of the same name: the decorator
            # registered this one before the name was rebound
            nsym = Sym(module, f"{fn.name}.<line{fn.lineno}>")
            self.p.__dict__.setdefault("nested", {})[nsym] = (fn, None)
            endpoint = self.p.function(nsym)
        if endpoint is None:
            # defined inside the application factory (create_app): compiled with the factory's locals
            factory = next((d for d in self.p.ix.module(module).tree.body
                            if isinstance(d, (ast.FunctionDef, ast.AsyncFunctionDef)) and any(n is fn for n in ast.walk(d))), None)
            if factory is None or fn not in factory.body:
                raise TranspileError(f"endpoint {fn.name} must be a module-level function or defined in the app factory", fn, src)
            nsym = Sym(module, f"{factory.name}.<locals>.{fn.name}")
            self.p.__dict__.setdefault("nested", {})[nsym] = (fn, factory)
            endpoint = self.p.function(nsym)
        return endpoint

    # what FastAPI does not give a WebSocket endpoint or its dependencies (a TypeError or a 403 there)
    WS_REFUSED = {"request": "a Request", "response": "a Response", "background": "BackgroundTasks",
                  "body": "a body", "body_embed": "a body", "form": "a Form()", "file": "a File()", "file_bytes": "a File()",
                  "oauth2form": "OAuth2PasswordRequestForm", "security": "a security scheme"}

    def ws_route(self, fn, module: str, path: str, deco: ast.Call, router, mount: Mount, idx: int) -> tuple[str, str]:
        """Compile one `@app.websocket` / `@router.websocket` route: returns (handler fn name, path regex)."""
        src = self.p.src(module)
        if not isinstance(fn, ast.AsyncFunctionDef):
            raise TranspileError(f"WebSocket endpoint {fn.name} must be an `async def`", fn, src)
        path_names = set(re.findall(r"{(\w+)(?::\w+)?}", path))
        pre_deps: list = list(mount.deps)
        r = self.fe.routers.get(router) if router else None
        if r is not None and r.deps is not None:
            pre_deps.append((r.deps, r.file))
        for kw in deco.keywords:
            if kw.arg == "dependencies":
                pre_deps.append((kw, src))
            elif kw.arg != "name":
                raise TranspileError(f"unsupported WebSocket route option {kw.arg}=", kw, src)
        params = self.params(fn, module, path_names)
        for p in params:
            if p.kind in self.WS_REFUSED:
                raise TranspileError(f"WebSocket endpoint {fn.name}: parameter `{p.name}` ({self.WS_REFUSED[p.kind]}) is not "
                                     "provided by FastAPI on a WebSocket route", fn, src)
        lines: list[str] = []
        deps: list[Sym] = [p.dep for p in params if p.kind == "dep"]
        for node, nfile in pre_deps:
            for e, mod, file in self.dependency_elements(node.value, self.p.ix.module_of(nfile).name, nfile):
                if not (isinstance(e, ast.Call) and (dotted(e.func) or "").split(".")[-1] in {"Depends", "Security"} and e.args):
                    raise TranspileError("dependencies=[...] must contain Depends(function)", e, file)
                t = self.p.resolve(mod, e.args[0])
                if isinstance(e.args[0], ast.Call):
                    t = self.factory_dependency(e.args[0], mod, None, file) or t
                elif isinstance(e.args[0], ast.Lambda):
                    t = self.lambda_dependency(e.args[0], mod, None, file)
                elif t is None and isinstance(e.args[0], ast.Name):
                    # the decorator of an endpoint defined in the application factory reads the factory's names
                    t = self.p.factory_function(mod, fn, e.args[0].id) if file == src else None
                if t is None and not isinstance(e.args[0], (ast.Call, ast.Lambda)):
                    t = self.method_dependency(e.args[0], mod, None, file)
                if isinstance(t, Sym) and isinstance(self.p.ix.definition(t), (ast.Assign, ast.AnnAssign)):
                    t = self.instance_dependency(t, file) or t
                elif (isinstance(t, Sym) and t not in self.p.fe.schema_syms
                      and isinstance(self.p.ix.definition(t), ast.ClassDef)):
                    t = self.plain_class_dependency(t, file)
                if not (isinstance(t, Sym) and isinstance(self.p.fn_node(t) if t in self.p.__dict__.get("nested", {})
                                                          else self.p.ix.definition(t), (ast.FunctionDef, ast.AsyncFunctionDef))):
                    raise TranspileError(f"dependency `{ast.unparse(e.args[0])}` must be a project function"
                                         f"{self.library_dependency(e.args[0], mod, None)}", e, file)
                lines.append(f"let _ = {self.dep_solver(t, self.depends_scope(e, file), (), self.depends_scopes(e, file))}(cx, &__body, __errs).await?;")
                deps.append(t)
        body_lines, args = self.solve(params, "&__body")
        lines += body_lines
        kinds = self.p.__dict__.get("dep_kinds", {})
        for d in deps:
            for kind, dname, dsrc, line in sorted(kinds.get(d, set())):
                if kind in self.WS_REFUSED:
                    raise TranspileError(f"WebSocket endpoint {fn.name}: dependency {dname} ({Path(dsrc).name}:{line}) takes "
                                         f"{self.WS_REFUSED[kind]}, which FastAPI does not provide on a WebSocket route", fn, src)
        endpoint = self.endpoint_fn(fn, module, src)
        name = f"ws_route_{idx}_{ident(fn.name)}"
        self.p.items.append(
            f"/// WEBSOCKET {self.p.fe.shown_path(path)}  (from {Path(src).name}:{fn.lineno} `{fn.name}`)\n"
            f"async fn {name}(cx: &Cx) -> R<()> {{\n"
            "    let __body: Option<V> = None;\n"
            f"    let mut __errv: Vec<{RT}::pyd::ErrDetail> = Vec::new();\n"
            f"    let __errs = &mut __errv;\n"
            + "".join(f"    {line}\n" for line in lines)
            + f"    {RT}::ws::check(__errv)?;\n"
            + f"    let _ = {endpoint}(cx{''.join(', ' + a for a in args)}).await?;\n"
            "    Ok(())\n}"
        )
        handler = f"run_{name}"
        self.p.items.append(
            f"fn {handler}<'a>(cx: &'a Cx) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<()>> + Send + 'a>> "
            f"{{ Box::pin({name}(cx)) }}"
        )
        return handler, starlette_pattern(path, fn, src)

    def _toplevel(self, fn, module) -> bool:
        return self.p.ix.module(module).defs.get(fn.name) is fn


ROUTE_DECORATOR_ATTRS = HTTP_METHODS | {"websocket", "api_route"}
# route decorators of FastAPI/Starlette outside HTTP_METHODS: refused rather than left unregistered
UNTRANSLATED_ROUTE_ATTRS = {"api_route", "trace", "route"}


def endpoint_decorators(fe: Frontend, fn, m, deco: ast.Call) -> None:
    """The endpoint's decorators besides its route decorator `deco` are refused (fichier:ligne): FastAPI registers
    the decorated function (a decorator below the route decorator wraps every call: auth, cache, rate limit,
    transaction...) and runs every decorator at import, while the binary compiles the bare function. Another route
    decorator of the app or a router (`@app.get` over `@app.post`, `@app.get` with `@app.websocket`) registers the
    function once more: only one HTTP route decorator per function is translated."""
    src = str(m.path)
    parents = fe._parents(m.name)
    http = [d for d in fn.decorator_list if d is not deco and isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute)
            and d.func.attr in ROUTE_DECORATOR_ATTRS
            and (fe._is_app(m.name, d.func.value)
                 or fe.index.resolve_expr(m.name, d.func.value, frontend_enclosing_fn(fn, parents)) in fe.routers)]
    for d in fn.decorator_list:
        if d is getattr(deco, "p2a_orig", None):
            continue  # the `api_route(methods=[...])` this per-method decorator stands for
        if d is deco or (d in http and d.func.attr == "websocket" and deco.func.attr != "websocket") \
                or (d in http and d.func.attr != "websocket" and deco.func.attr == "websocket"):
            continue
        shown = ast.unparse(d)
        shown = shown if len(shown) <= 60 else shown[:57] + "..."
        if d in http:
            continue  # another registration of the function (build_routes translates each)
        below = fn.decorator_list.index(d) > fn.decorator_list.index(deco)
        raise TranspileError(f"endpoint {fn.name}: decorator @{shown} on an endpoint is not supported: "
                             + ("FastAPI registers the decorated function, the binary would run the endpoint without it"
                                if below else "it runs at import on the registered function, the binary would never run it")
                             + "; move its logic into a dependency, or leave the route to Python with "
                             "--python-side '<path>'", d, src)


def ws_route(proj: Project, rb: RouteBuilder, fe: Frontend, fn, m, found, idx: int, out: list, collect: bool) -> int:
    """`@app.websocket` / `@router.websocket`: one compiled route per mount, appended to `out` as
    (pattern, handler, fn, module); returns the last route index used."""
    deco, router = found
    src = str(m.path)
    for mt in (fe._mounts_cache.get(router, []) if router else [Mount("", deps=list(fe.__dict__.get("app_deps", [])))]):
        idx += 1
        r = fe.routers.get(router) if router else None
        rid = ("route", idx)
        proj.cur = rid
        info = {"method": "websocket", "path": "?", "func": fn.name, "file": src, "line": fn.lineno,
                "conditional": mt.conditional, "node": fn, "module": m.name, "router": router, "id": rid}
        try:
            path = mt.prefix + (r.prefix if r else "") + route_path(deco, src)
            info["path"] = fe.shown_path(path)
            for k in re.findall("\x01(\\d+)\x01", path):
                proj.edges.setdefault(rid, set()).add(("prefix", int(k)))
            if fe.shown_path(path) in fe.__dict__.get("python_side", ()):
                # relayed to the Python process by the binary (ws::relay), by its runtime path like an HTTP route
                fe.__dict__.setdefault("python_side_raw", set()).add(path)
                fe.notes.append(f"{src}:{fn.lineno}: WEBSOCKET {fe.shown_path(path)} declared Python-side")
                proj.cur = None
                continue
            if r is not None and r.unsupported:
                raise TranspileError(f"unsupported APIRouter option {r.unsupported[0].arg}=", r.unsupported[0], r.file)
            if mt.unsupported:
                node, f = mt.unsupported[0]
                raise TranspileError(f"unsupported include_router option {getattr(node, 'arg', '?')}=", node, f)
            if mt.conditional:
                raise TranspileError("include_router(...) under an `if` is not supported: the binary would "
                                     "serve the route unconditionally", fn, src)
            endpoint_decorators(fe, fn, m, deco)
            handler, pattern = rb.ws_route(fn, m.name, path, deco, router, mt, idx)
            proj.cur = None
            proj.drain()
            out.append((pattern, handler, fn, m))
            proj.__dict__.setdefault("route_infos", []).append((info, None))
        except TranspileError as e:
            proj.cur = None
            if not collect:
                raise
            proj.errors.append(e)
            proj.__dict__.setdefault("route_infos", []).append((info, e))
    return idx


def route_key(fn, deco, n: int, k: int) -> tuple:
    """Where a route decorator registers among the module's routes: its line; the decorators of one function in
    registration order (the lowest first)."""
    return (deco.lineno,) if n <= 1 else (fn.decorator_list[0].lineno, k)


def route_node_item(items, nodes, members, order, m, fn, deco, router, path, pattern, found, n: int, k: int) -> None:
    """The route tree's node of one route decorator (`APIRoute` or WebSocket route)."""
    name = next((kw.value.value for kw in deco.keywords if kw.arg == "name" and isinstance(kw.value, ast.Constant)), fn.name)
    ident_ = f"RN_{len(nodes) + 1}"
    nodes[(m.name, deco.lineno)] = ident_
    if found is None:
        items.append(f"static {ident_}: {RT}::routing::Node = {RT}::routing::Node::Ws {{ path: {rs(path)}, "
                     f"pattern: {rs(pattern)}, name: {rs(name)} }};")
    else:
        items.append(f"static {ident_}: {RT}::routing::Node = {RT}::routing::Node::Api {{ path: {rs(path)}, pattern: {rs(pattern)}, "
                     f"methods: &[{rs(deco.func.attr.upper())}], name: {rs(name)} }};")
    members.setdefault(router or "app", []).append(((order.get(m.name, 0), *route_key(fn, deco, n, k)), ident_))


def emit_descriptors(p: Project) -> list[str]:
    out = []
    for src, name in p.pats.items():
        out.append(f"static {name}: {RT}::pyd::Pat = {RT}::pyd::Pat::new({rs(src)});")
    for body, name in p.dflts.items():
        out.append(f"fn {name}() -> V {{ {body} }}")
    out.append("fn dflt_none() -> V { V::None }")
    out.append("fn dflt_unbound() -> V { V::Unbound }")
    out.append("fn dflt_empty_str() -> V { V::str(\"\") }")
    # schemas (their TDs reference each other through statics)
    errs, has_before = union_errors(p)
    if errs:
        raise errs[0][0]
    for info in list(p.schemas.values()):
        fields = []
        wrap_tds = {}
        for f in info.fields:
            ann, module = f["ann"]
            cons = p.model_field_cons(info, f)
            td = p._static_td(f"{RT}::pyd::TD::Any") if f.get("any") else p.td(ann, module, cons)
            if "TD::MissingSentinel" in next((i for i, n in p.tds.items() if n == td), "") and f["default"] != "Required":
                # a field left out of dumps while it holds the sentinel: not reproduced
                raise p.err(f"schema {info.sym.name}.{f['name']}: a MISSING field with a default is not supported", ann, module)
            if f["default"] not in ("Required", f"Value({p.dflts.get('V::None')})") and is_iterable_td(p, td) and not f.get("validate_default"):
                # pydantic keeps such a default unvalidated (a list) and serializes it with a warning
                # (PydanticSerializationUnexpectedValue: Expected `generator`): not reproduced
                raise p.err(f"schema {info.sym.name}.{f['name']}: an Iterable field with a default other than None is "
                            "not supported (use list[...])", ann, module)
            if f.get("wrap"):
                # a wrap validator decides: the field's type is what its handler validates
                wrap_tds[f["wrap"]], td = td, p._static_td(f"{RT}::pyd::TD::Any")
            td = apply_strict(p, info, f, td, ann, module)
            alias = f"Some({rs(f['alias'])})" if f["alias"] else "None"
            ser = f"Some({rs(f['ser'])})" if f.get("ser") else "None"
            paths = ", ".join("&[" + ", ".join(f"{RT}::pyd::PK::S({rs(x)})" if isinstance(x, str) else f"{RT}::pyd::PK::I({x})"
                                              for x in p_) + "]" for p_ in f.get("paths", []))
            fields.append(
                f"{RT}::pyd::FieldDesc {{ name: {rs(f['name'])}, alias: {alias}, ser: {ser}, paths: &[{paths}], td: &{td}, "
                f"default: {RT}::pyd::Dflt::{f['default']}, env: None, validate_default: {str(bool(f.get('validate_default'))).lower()} }}"
            )
        validators = ", ".join(
            f"{RT}::pyd::ValidatorDesc {{ fields: &[{', '.join(rs(n) for n in names)}], f: {method_wrapper(p, s)}, "
            f"info: {info.__dict__.get('vinfo', {}).get(s, 0)}, "
            f"wrap: {('Some(&' + wrap_tds[s] + ')') if s in wrap_tds else 'None'} }}"
            for names, s in info.validators
        )
        methods = ", ".join(f"({rs(n)}, {str(prop).lower()}, {method_wrapper(p, s)} as {RT}::pyd::MethodFn)" for n, prop, s in info.methods)
        cattrs = info.__dict__.get("class_attrs", {})
        if cattrs:
            methods = ", ".join(x for x in [methods, *(f"({rs(n)}, true, {g}_prop as {RT}::pyd::MethodFn)" for n, g in cattrs.items())] if x)
            own = info.__dict__.get("own_class_attrs", [])
            methods = ", ".join(x for x in [methods, *(f"({rs('=' + n)}, false, {cattrs[n]}_setp as {RT}::pyd::MethodFn)"
                                                       for n in own if n in cattrs)] if x)
        out.append(
            ("" if info.__dict__.get("shared_cls") else
             f"pub static {info.cls}: {RT}::v::Class = {RT}::v::Class {{ name: {rs(shown_name(info.sym.name))}, qualname: {rs(shown_name(info.sym.qual))}, "
             f"bases: &[{('&' + info.__dict__['base_cls']) if info.__dict__.get('base_cls') else ''}], kind: {RT}::v::ClassKind::Schema(&{info.rust}) }};\n")
            + f"pub static {info.rust}: {RT}::pyd::SchemaDesc = {RT}::pyd::SchemaDesc {{ name: {rs(shown_name(info.sym.name))}, class: &{info.cls}, "
            f"fields: &[{', '.join(fields)}], from_attributes: {str(info.from_attributes).lower()}, "
            f"extra: {RT}::pyd::Extra::{info.extra}, validators: &[{validators}], "
            f"validate_assignment: {str(bool(info.__dict__.get('validate_assignment'))).lower()}, "
            f"populate_by_name: {str(bool(info.__dict__.get('populate_by_name'))).lower()}, methods: &[{methods}], "
            f"open: {str(bool(info.__dict__.get('open'))).lower()}, "
            f"model_after: &[{', '.join(f'{method_wrapper(p, m)} as {RT}::pyd::MethodFn' for m in info.__dict__.get('model_after', []))}], "
            f"before: &[{', '.join(f'{RT}::pyd::ValidatorDesc {{ fields: &[{chr(44).join(rs(n) for n in names)}], f: {method_wrapper(p, s_)}, info: 0, wrap: None }}' for names, s_ in info.__dict__.get('before', []))}], "
            f"model_before: &[{', '.join(f'{method_wrapper(p, m)} as {RT}::pyd::MethodFn' for m in info.__dict__.get('model_before', []))}], "
            f"has_before: {str(info.rust in has_before).lower()}, "
            f"frozen: {str(bool(info.__dict__.get('frozen'))).lower()}, "
            f"post_init: {('Some(' + method_wrapper(p, info.__dict__['post_init']) + ' as ' + RT + '::pyd::MethodFn)') if info.__dict__.get('post_init') else 'None'}, "
            f"hash: {RT}::pyd::HashKind::{hash_kind(info)}, dataclass: {str(bool(info.__dict__.get('dataclass'))).lower()}, "
            f"async_methods: {async_names(p, info.methods)}, "
            f"slots: &[{', '.join(rs(x) for x in info.__dict__.get('slots', []))}], "
            f"settings: {settings_desc(info)}, "
            f"ser_bytes: {RT}::pyd::BytesMode::{ {'base64': 'Base64', 'hex': 'Hex'}.get(info.__dict__.get('ser_json_bytes') or '', 'Utf8') }, "
            f"computed: &[{', '.join(computed_entry(p, info, n) for n in info.__dict__.get('computed', []))}], "
            f"json_schema: {('Some(' + rs(p.json_schemas()[0][info.sym]) + ')') if info.sym in p.json_schemas()[0] else 'None'}, "
            f"init: {('Some(' + method_wrapper(p, info.__dict__['init']) + ' as ' + RT + '::pyd::MethodFn)') if info.__dict__.get('init') else 'None'}, "
            f"private: &[{', '.join(f'({rs(n)}, {RT}::pyd::Dflt::{d})' for n, d in info.__dict__.get('private', []))}], "
            f"x: {schema_x(p, info)} }};"
        )
    # INSERT order: rank = 1 + the highest rank of the tables a model references (cycles cut)
    secondaries = list(p.__dict__.get("secondaries", {}).values())
    by_table = {m.table: m for m in [*p.models.values(), *secondaries]}
    ranks: dict[str, int] = {}

    def post_only(m, t: str) -> bool:
        """every foreign key of `m` to table `t` is a post_update relationship's (set by an UPDATE after the INSERTs)"""
        fk = [i for i, c in enumerate(m.cols) if c.get("fk") == t]
        post = {r["local"] for r in (getattr(m, "rels", None) or []) if r.get("post_update")}
        return bool(fk) and set(fk) <= post

    def rank(table: str, seen: frozenset = frozenset()) -> int:
        if table not in ranks:
            refs = [t for t in by_table[table].fk_tables if t != table and t in by_table and t not in seen
                    and not post_only(by_table[table], t)]
            ranks[table] = 1 + max((rank(t, seen | {table}) for t in refs), default=-1)
        return ranks[table]

    for t in by_table:
        rank(t)
    for info in [*p.models.values(), *secondaries]:
        cols = []
        for c in info.cols:
            d = f"{RT}::orm::ColDefault::{c['default']}" if c["default"] else f"{RT}::orm::ColDefault::None"
            u = f"{RT}::orm::ColDefault::{c['onupdate']}" if c["onupdate"] else f"{RT}::orm::ColDefault::None"
            cols.append(
                f"{RT}::orm::ColDesc {{ name: {rs(c['name'])}, sql: {rs(c.get('sql') or c['name'])}, ty: {RT}::orm::ColTy::{c['ty']}, nullable: {str(c['nullable']).lower()}, "
                f"pk: {str(c['pk']).lower()}, autoincrement: {str(c['autoincrement']).lower()}, default: {d}, "
                f"server_default: {str(c['server_default']).lower()}, onupdate: {u}, deferred: {str(c['deferred']).lower()}, "
                f"group: {'Some(' + rs(c['group']) + ')' if c.get('group') else 'None'}, raiseload: {str(c.get('raiseload', False)).lower()}, "
                f"jsonb: {str(c['jsonb']).lower()}, "
                f"table: {('Some(' + rs(c['table']) + ')') if c.get('table') and c['table'] != info.table else 'None'}, "
                f"qsql: {rs(quote_ident(c.get('sql') or c['name']))}, qref: {rs(col_ref(info, c))} }}"
            )
        methods = ", ".join(f"({rs(n)}, {str(prop).lower()}, {method_wrapper(p, s)} as {RT}::pyd::MethodFn)" for n, prop, s in info.methods)
        b = lambda x: str(bool(x)).lower()  # noqa: E731

        def sec_of(r):
            sec = r.get("secondary")
            return f"Some((&{sec[0].rust}, {sec[1]}, {sec[2]}))" if sec else "None"

        rels = ", ".join(
            f"{RT}::orm::RelDesc {{ name: {rs(r['name'])}, target: &{p.models[r['target']].rust}, m2o: {b(r['m2o'])}, "
            f"local: {r['local']}, remote: {r['remote']}, uselist: {b(r['uselist'])}, lazy: {RT}::orm::Lazy::{r['lazy']}, "
            f"back: {'Some(' + rs(r['back']) + ')' if r['back'] else 'None'}, delete: {b(r['delete'])}, "
            f"orphan: {b(r['orphan'])}, passive_deletes: {b(r['passive_deletes'])}, "
            f"order: &[{', '.join(f'({i}, {b(d)})' for i, d in r.get('order', []))}], "
            f"secondary: {sec_of(r)}, write_only: {b(r.get('write_only', False))}, "
            f"refresh_expire: {b(r.get('refresh_expire', False))}, post_update: {b(r.get('post_update', False))} }}"
            for r in (info.rels or [])
        )
        fks = ", ".join(f"({i}, {rs(c['fk'])}, {rs(c.get('fk_col') or 'id')})" for i, c in enumerate(info.cols) if c.get("fk"))
        out.append(
            f"pub static {info.cls}: {RT}::v::Class = {RT}::v::Class {{ name: {rs(info.sym.name)}, qualname: {rs(info.sym.qual)}, "
            f"bases: &[{('&' + p.models[info.parent].cls) if getattr(info, 'parent', None) else ''}], "
            f"kind: {RT}::v::ClassKind::Model(&{info.rust}) }};\n"
            f"pub static {info.rust}: {RT}::orm::ModelDesc = {RT}::orm::ModelDesc {{ name: {rs(info.sym.name)}, "
            f"class_qualname: {rs(info.sym.qual)}, table: {rs(info.table)}, qtable: {rs(quote_ident(info.table))}, "
            f"sel: {rs(', '.join(col_ref(info, c) for c in info.cols))}, class: &{info.cls}, cols: &[{', '.join(cols)}], "
            f"pk: {info.pk}, pks: &[{', '.join(str(i) for i in info.pks)}], fk_tables: &[{', '.join(rs(t) for t in info.fk_tables)}], "
            f"fks: &[{fks}], "
            f"rank: {ranks[info.table]}, "
            f"methods: &[{methods}], class_methods: &[{', '.join(rs(n) for n in class_level_methods(p, info.methods))}], "
            f"rels: &[{rels}], async_methods: {async_names(p, info.methods)}, hybrids: &[{hybrids_of(p, info)}], "
            f"eager_defaults: {str(info.eager_defaults).lower()}, {poly_fields(p, info)}, "
            f"core: {str(any(info is x for x in secondaries)).lower()}, "
            f"composites: &[{', '.join(composite_desc(c) for c in info.__dict__.get('composites', []))}] }};"
        )
    # TDs last: emitting schemas may have created new ones
    for init, name in p.tds.items():
        out.append(f"static {name}: {RT}::pyd::TD = {init};")
    return out


# PostgreSQL's reserved words, as SQLAlchemy's IdentifierPreparer quotes them: the same list as
# `orm::quote_ident` in the runtime (tests/test_quote_ident.py compares them)
RESERVED_SQL = frozenset("""
    all analyse analyze and any array as asc asymmetric authorization binary both case cast check collate collation
    column concurrently constraint create cross current_catalog current_date current_role current_schema current_time
    current_timestamp current_user default deferrable desc distinct do else end except false fetch for foreign freeze
    from full grant group having ilike in initially inner intersect into is isnull join lateral leading left like limit
    localtime localtimestamp natural not notnull null offset on only or order outer overlaps placing primary references
    returning right select session_user similar some symmetric system_user table tablesample then to trailing true
    union unique user using variadic verbose when where window with""".split())


def quote_ident(name: str) -> str:
    """A table or column name in SQL, quoted like `orm::quote_ident` does at run time (`"end"`, `"Mixed"`)."""
    plain = bool(name) and (name[0].islower() and name[0].isascii() or name[0] == "_") and all(
        ch.isascii() and (ch.islower() or ch.isdigit() or ch in "_$") for ch in name)
    return name if plain and name not in RESERVED_SQL else '"' + name.replace('"', '""') + '"'


def col_ref(info, c: dict) -> str:
    """`table.column` of a mapped column (an inherited column of joined table inheritance names its own table)."""
    return f"{quote_ident(c.get('table') or info.table)}.{quote_ident(c.get('sql') or c['name'])}"


def composite_desc(c: dict) -> str:
    return (f"{RT}::orm::CompositeDesc {{ name: {rs(c['name'])}, cols: &[{', '.join(map(str, c['cols']))}], "
            f"ctor: {c['ctor']} as {RT}::v::ClsAttr, none_on_null: {str(c['none']).lower()} }}")


PLAIN_DECORATORS = {"lru_cache", "cache", "staticmethod", "classmethod", "property", "cachedmethod"}

# the attributes cachetools sets on a cached function (a @cachedmethod's are refused)
CT_WRAPPER_ATTRS = {"cache_clear", "cache_info", "cache_parameters", "cache", "cache_key", "cache_lock", "cache_condition"}

# the tzinfo classes of pytz and dateutil.tz an isinstance() test names (methods::isinstance_builtin)
TZ_CLASSES = {
    **{f"pytz{m}.{c}": f"pytz.{c}" for m in ("", ".tzinfo") for c in ("BaseTzInfo", "DstTzInfo", "StaticTzInfo")},
    **{f"dateutil.tz{m}.{c}": c for m in ("", ".tz") for c in ("tzutc", "tzoffset", "tzlocal", "tzfile", "tzstr", "tzrange")},
    "dateutil.tz._common.tzrangebase": "tzrange",
    **{f"cachetools.{c}": f"cachetools.{c}" for c in ("Cache", "FIFOCache", "LRUCache", "TTLCache")},
}


def async_method_names(p: "Project", methods) -> list[str]:
    """The methods of a class table that are `async def`s (not generators)."""
    out = []
    for n, prop, s in methods:
        try:
            node = p.fn_node(s)
        except TranspileError:
            continue
        if isinstance(node, ast.AsyncFunctionDef) and not has_yield(node) and not prop and s not in p.__dict__.get("vdeco_methods", ()):
            out.append(n)  # (a decorated one returns what its decorators made: awaited by the caller)
    return out


def async_names(p: "Project", methods) -> str:
    """`&["m", ...]`: the methods of a class table that are `async def`s (not generators)."""
    return "&[" + ", ".join(rs(n) for n in async_method_names(p, methods)) + "]"

# dunder methods the runtime dispatches to (str(), repr(), ==/!= and the containers' comparisons, `async for`)
# the methods only pickle and copy call: ignored in a class when nothing imports them (Project.copies_objects)
# the protocol methods of a project exception class the runtime calls (ops::dunder on V::Exc): str() (and every
# text of the exception: tracebacks, logs, pydantic's "Value error, ..."), repr(), truth
EXC_DUNDERS = {"__str__", "__repr__", "__bool__"}
COPY_ROOTS = {"copy", "pickle", "copyreg", "_pickle"}
COPY_DUNDERS = {"__getstate__", "__setstate__", "__reduce__", "__reduce_ex__", "__copy__", "__deepcopy__",
                "__getnewargs__", "__getnewargs_ex__"}

# bases that only type a class (subscripted or not)
GENERIC_BASES = {"typing.Generic", "typing.Protocol", "typing_extensions.Protocol", "typing_extensions.Generic"}

# the protocol methods the operators and builtins call (dynrt/proto.rs): iteration, containers, truth, arithmetic
PROTOCOL_DUNDERS = {"__iter__", "__next__", "__len__", "__bool__", "__contains__", "__getitem__", "__setitem__",
                    "__delitem__", "__neg__", "__pos__", "__invert__", "__abs__",
                    *(f"__{p}{n}__" for n in ("add", "sub", "mul", "truediv", "floordiv", "mod", "pow", "and", "or",
                                              "xor", "lshift", "rshift", "matmul") for p in ("", "r", "i"))}

DISPATCHED_DUNDERS = {"__str__", "__repr__", "__eq__", "__aenter__", "__aexit__", "__enter__", "__exit__", "__call__",
                      "__aiter__", "__anext__", "__getattr__", "__setattr__", "__composite_values__",
                      # comparisons (`ops::richcmp`) and SQL coercion (`orm::to_sql`): SQLAlchemy's Comparator and
                      # hybrid value objects
                      "__ne__", "__lt__", "__le__", "__gt__", "__ge__", "__clause_element__", *PROTOCOL_DUNDERS}


def is_immutable_literal(v: ast.AST) -> bool:
    """A number, string, bytes, bool or None literal (a negative number included), or a tuple of those."""
    if isinstance(v, ast.UnaryOp) and isinstance(v.op, (ast.USub, ast.UAdd)):
        v = v.operand
    if isinstance(v, ast.Constant):
        return v.value is not Ellipsis
    return isinstance(v, ast.Tuple) and all(is_immutable_literal(x) for x in v.elts)


def is_dunder(name: str) -> bool:
    return name.startswith("__") and name.endswith("__")


SCALAR_TDS = ("Int(", "Float(", "Str(", "Bool", "Date", "DateTime", "Time", "Delta", "Decimal(", "Uuid", "Enum(", "Bytes(", "Url(",
              "Email(", "Path")


def is_app_route_class(fe, module: str, st: ast.AST) -> bool:
    """`app.router.route_class = R` (module level)"""
    return (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(t := st.targets[0], ast.Attribute)
            and t.attr == "route_class" and isinstance(t.value, ast.Attribute) and t.value.attr == "router"
            and fe._is_app(module, t.value.value))


def app_route_class(fe) -> tuple[ast.AST, str, int] | None:
    """The app's `app.router.route_class = R`: (R, module, line); refused when set twice"""
    if "app_route_class" not in fe.__dict__:
        found = [(st.value, m.name, st.lineno) for m in fe.index.package_modules() for st in m.tree.body
                 if is_app_route_class(fe, m.name, st)]
        if len(found) > 1:
            raise TranspileError("`app.router.route_class` set more than once is not supported", found[1][0],
                                 str(next(m.path for m in fe.index.package_modules() if m.name == found[1][1])))
        fe.__dict__["app_route_class"] = found[0] if found else None
    return fe.__dict__["app_route_class"]


def is_iterable_td(p, td: str) -> bool:
    """The field's TD is `Iterable[...]`, or Optional of it"""
    text = next((i for i, n in p.tds.items() if n == td), "")
    m = re.match(rf"{re.escape(RT)}::pyd::TD::Optional\(&(\w+)\)$", text)
    if m:
        text = next((i for i, n in p.tds.items() if n == m.group(1)), "")
    return text.startswith(f"{RT}::pyd::TD::Iter(")


def apply_strict(p, info, f, td: str, ann, module) -> str:
    """Strict mode of a field: `model_config strict=True` covers its whole type (items included, nested models
    excepted), `Field(strict=True)` the type itself (a container's items keep the model's mode), `Field(strict=
    False)` in a strict model a scalar (an optional one included)."""
    own, model = f.get("strict"), bool(info.__dict__.get("strict"))
    if own is None:
        return p._static_td(f"{RT}::pyd::TD::Strict(&{td}, true)") if model else td
    init = next(i for i, n in p.tds.items() if n == td)
    base = init
    if base.startswith(f"{RT}::pyd::TD::Optional(&"):
        base = next(i for i, n in p.tds.items() if n == base[len(f"{RT}::pyd::TD::Optional(&"):-1])
    shape = base[len(f"{RT}::pyd::TD::"):]
    if own:
        if not (shape.startswith(SCALAR_TDS) or shape.startswith(("List(", "Tuple(", "TupleFix(", "Set(", "Dict(", "Len("))):
            raise p.err(f"Field(strict=True) on `{ast.unparse(ann)}` is not supported (a scalar, a container, or "
                        "an optional one)", ann, module)
        inner = p._static_td(f"{RT}::pyd::TD::StrictTop(&{td})")
        return p._static_td(f"{RT}::pyd::TD::Strict(&{inner}, true)") if model else inner
    if model and not shape.startswith(SCALAR_TDS):
        raise p.err(f"Field(strict=False) on `{ast.unparse(ann)}` in a strict model is not supported (scalars "
                    "only)", ann, module)
    return p._static_td(f"{RT}::pyd::TD::Strict(&{td}, false)")


def union_errors(p) -> tuple[list, set]:
    """The unions the runtime cannot validate as Pydantic does: (error, the union's TD), checked on every TD.
    A member with `mode='before'` validators (directly or in a model it holds: the before pass does not enter a
    union), or, for a smart union, with validators (a raising one would make Pydantic try the next member)."""
    # transitive `has_before`: a model with mode="before" validators, or containing one
    by_init = {v: k for k, v in p.tds.items()}

    def refs(td: str, seen: frozenset = frozenset()) -> set:
        init = by_init.get(td, "")
        out = set(re.findall(r"SCHEMA_\w+|ENUM_\w+", init))
        for t in re.findall(r"\bTD_\d+\b", init):
            if t not in seen:
                out |= refs(t, seen | {td})
        return out

    field_tds = {}
    for info in p.schemas.values():
        field_tds[info.rust] = set()
        for f in info.fields:
            if not f.get("any"):
                ann, module = f["ann"]
                field_tds[info.rust] |= refs(p.td(ann, module, dict(f.get("cons", {}))))
    def closure(base: set) -> set:
        changed = True
        while changed:
            changed = False
            for r, deps in field_tds.items():
                if r not in base and deps & base:
                    base.add(r)
                    changed = True
        return base

    has_before = {i.rust for i in p.schemas.values() if i.__dict__.get("before") or i.__dict__.get("model_before")}
    has_before |= {rust for sym, (rust, _, _) in p.enums.items() if p.__dict__.get("enum_missing", {}).get(sym)}
    has_before = closure(has_before)
    # after validators run once the member is chosen: a raising one would make Pydantic try the next member
    has_after = closure({i.rust for i in p.schemas.values() if i.validators or i.__dict__.get("model_after")})
    names = {i.rust: i.sym.name for i in p.schemas.values()}
    out = []
    for init, td in list(p.tds.items()):
        if "TD::Union(" not in init and "TD::UnionLtr(" not in init and "TD::Tagged(" not in init:
            continue
        node, module = p.__dict__.get("union_src", {}).get(init, (None, None))
        # a discriminated union never tries another member: only the before pass (not run inside it) matters
        tagged = "TD::Tagged(" in init
        for bad, what in ((refs(td) & has_before, "mode='before' validators"),
                          (set() if tagged else refs(td) & has_after,
                           "validators, which would make Pydantic try the next member when they raise")):
            if bad:
                msg = f"a Union containing {names.get(sorted(bad)[0], sorted(bad)[0])} ({what}) is not supported"
                out.append((p.err(msg, node, module) if node is not None else TranspileError(msg), td))
                break
    return out, has_before


def schema_x(p, info) -> str:
    """The `SchemaX` of a model (its rarer options)."""
    names = lambda flag, value: ", ".join(rs(f["name"]) for f in info.fields if f.get(flag, not value) == value)  # noqa: E731
    excl = [f"({rs(f['name'])}, {method_wrapper(p, f['exclude_if'])} as {RT}::pyd::MethodFn)"
            for f in info.fields if f.get("exclude_if")]

    def ser(field: str, e, deco: bool) -> str:
        s, wrap, info_arg, when = e
        return (f"{RT}::pyd::Ser {{ field: {rs(field)}, f: {method_wrapper(p, s)}, wrap: {str(wrap).lower()}, "
                f"info: {str(bool(info_arg)).lower()}, when: {when}, deco: {str(deco).lower()} }}")
    sers = {f["name"]: (f["serializer"], False) for f in info.fields if f.get("serializer")}
    # the decorator wins over the annotation's
    sers.update({n: (e, True) for n, e in info.__dict__.get("field_sers", {}).items()})
    for n in sers:
        if not any(f["name"] == n for f in info.fields):
            raise TranspileError(f"{info.sym.module}: schema {info.sym.name}: @field_serializer of an unknown field {n!r}")
    ms = info.__dict__.get("model_ser")
    return (f"{RT}::pyd::SchemaX {{ ser_by_alias: {str(bool(info.__dict__.get('ser_by_alias'))).lower()}, "
            f"exclude: &[{names('exclude', True)}], norepr: &[{names('repr', False)}], frozen: &[{names('frozen', True)}], "
            f"exclude_if: &[{', '.join(excl)}], field_ser: &[{', '.join(ser(n, e, d) for n, (e, d) in sers.items())}], "
            f"model_ser: {('Some(' + ser('', ms, True) + ')') if ms else 'None'}, "
            f"root: {str(bool(info.__dict__.get('root'))).lower()}, eq_id: {str(bool(info.__dict__.get('eq_id'))).lower()}, "
            f"ntuple: {str(bool(info.__dict__.get('ntuple'))).lower()}, "
            f"hide_input: {str(bool(info.__dict__.get('hide_input'))).lower()}, "
            f"classmethods: &[{', '.join(rs(n) for n in class_level_methods(p, info.methods, {'classmethod'}))}], "
            f"statics: &[{', '.join(rs(n) for n in class_level_methods(p, info.methods, {'staticmethod'}))}], "
            f"new_: {('Some(' + method_wrapper(p, info.__dict__['new']) + ' as ' + RT + '::pyd::MethodFn)') if info.__dict__.get('new') else 'None'} }}")


def settings_desc(info) -> str:
    """A BaseSettings class's environment options, for calling the class held as a value (`type(s)()`)."""
    if not info.settings:
        return "None"
    none = f"Some({rs(info.env_none)})" if info.env_none is not None else "None"
    return (f"Some({RT}::pyd::SettingsDesc {{ prefix: {rs(info.env_prefix)}, "
            f"case_sensitive: {'true' if info.case_sensitive else 'false'}, none_str: {none}, "
            f"ignore_empty: {'true' if info.__dict__.get('env_ignore_empty') else 'false'} }})")


def hash_kind(info) -> str:
    """`hash()` of an instance, CPython's rules: `__hash__ = None` or `__eq__` without `__hash__` →
    unhashable; Pydantic models and dataclasses: by value when frozen, else unhashable (they define
    `__eq__`); plain classes: identity."""
    if info.__dict__.get("unhashable"):
        return "Unhashable"
    if info.__dict__.get("hash_kind"):
        return info.__dict__["hash_kind"]
    if info.__dict__.get("open"):
        return "Unhashable" if any(n == "__eq__" for n, _, _ in info.methods) else "Id"
    return "Value" if info.__dict__.get("frozen") else "Unhashable"


def value_decorators(d, resolve=None) -> list:
    """Decorators of a project `def` applied as values (anything but lru_cache & co); mixing both kinds
    is refused. `resolve` (expression -> static reference, in the def's module) tells functools' lru_cache/cache from a
    library's own (cachetools.func.lru_cache is a value decorator)."""
    decos = [d for d in d.decorator_list]
    last = [((dotted(x.func) if isinstance(x, ast.Call) else dotted(x)) or "").split(".")[-1] for x in decos]
    if resolve is not None:
        for i, x in enumerate(decos):
            ref = resolve(x.func if isinstance(x, ast.Call) else x)
            if last[i] in ("lru_cache", "cache") and isinstance(ref, Ext) and not ref.dotted.startswith("functools."):
                last[i] = ref.dotted
            if ref == Ext("cachetools.cachedmethod"):
                last[i] = "cachedmethod"
    plain = [x for x, n in zip(decos, last) if n in PLAIN_DECORATORS]
    other = [x for x, n in zip(decos, last) if n not in PLAIN_DECORATORS]
    if plain and other:
        raise TranspileError(f"@{ast.unparse(plain[0])} combined with @{ast.unparse(other[0])} is not supported", plain[0])
    return other


def clean_doc_313(doc: str) -> str:
    """CPython 3.13's compile-time `_PyCompile_CleanDoc`: tabs expanded, the first line's leading spaces
    removed, then the smallest indentation of the following non-blank lines; trailing lines kept."""
    doc = doc.expandtabs()
    lines = doc.split("\n")
    margins = [len(l) - len(l.lstrip(" ")) for l in lines[1:] if l.strip(" ")]
    margin = min(margins) if margins else 0
    out = [lines[0].lstrip(" ")]
    for l in lines[1:]:
        k = 0
        while k < margin and k < len(l) and l[k] == " ":
            k += 1
        out.append(l[k:])
    return "\n".join(out)


def fn_doc(p: "Project", node) -> str | None:
    """`__doc__` as the project's CPython stores it (3.13+ cleans the indentation at compile time)."""
    doc = ast.get_docstring(node, clean=False)
    if doc is not None and target_python(p.ix.root) >= (3, 13):
        doc = clean_doc_313(doc)
    return doc


def raw_fn_value(p: Project, sym: Sym) -> str:
    """A project `def` as a function object (`__name__`, `__wrapped__`...), CPython's binding at call."""
    node = p.fn_node(sym)
    fw = function_wrapper(p, sym)
    doc = fn_doc(p, node)
    is_async = isinstance(node, ast.AsyncFunctionDef) and not has_yield(node)
    # one object per function, like CPython (`is`, attributes set on it)
    cell = f"FNV_{fw}"
    if cell not in p.__dict__.setdefault("_fn_cells", set()):
        p._fn_cells.add(cell)
        p.items.append(f"static {cell}: std::sync::OnceLock<V> = std::sync::OnceLock::new();")
    return (f"{cell}.get_or_init(|| "
            f"{RT}::pyfn({rs(sym.module)}, {rs(sym.name)}, {'Some(' + rs(doc) + ')' if doc is not None else 'None'}, "
            f"{str(is_async).lower()}, std::sync::Arc::new(|cx: &Cx, args: Vec<V>, kwargs: Vec<(String, V)>| -> {RT}::BoxFut<'_> "
            f"{{ {fw}(cx, V::None, {RT}::pack(args, kwargs)) }}))).clone()")


def function_wrapper(p: Project, sym: Sym) -> str:
    """A `MethodFn` calling a project function with run-time argument binding (all its parameters,
    `self` included for a method called through the class): CPython's TypeErrors on a bad call."""
    rust = p.function(sym)
    name = f"fw_{rust}"
    if name not in p.__dict__.setdefault("_wrappers", set()):
        p._wrappers.add(name)
        node = p.fn_node(sym)
        params = fn_params(node)
        kinds = {"positional": f"{RT}::P_POS", "vararg": f"{RT}::P_VARARG", "kwonly": f"{RT}::P_KWONLY", "kwarg": f"{RT}::P_KWARG"}
        spec = ", ".join(f"({rs(q.name)}, {kinds[q.kind]}, {str(q.default is not None).lower()})" for q in params)
        vals = []
        for i, q in enumerate(params):
            if q.default is not None:
                sub = FnCompiler(p, sym.module, None, "default")
                e = sub.expr(q.default)
                vals.append(f"match __s[{i}].take() {{ Some(v) => v, None => {{ {' '.join(sub.lines)} {e} }} }}")
            else:
                vals.append(f"__s[{i}].take().unwrap_or(V::None)")
        qual = sym.name.split(".")[-1] + "()" if "." not in sym.name else sym.name + "()"
        if has_yield(node):
            # an async generator: the call binds its arguments, the body waits for the first __anext__
            binds = "".join(f"    let __a{i} = {v};\n" for i, v in enumerate(vals))
            call = (f"{binds}    let cx2 = cx.clone();\n    Ok({RT}::web::spawn_gen(move |y| Box::pin(async move {{ "
                    f"{rust}(&cx2{''.join(f', __a{i}' for i in range(len(vals)))}, &y).await }})))")
        else:
            call = f"    {rust}(cx, {', '.join(vals)}).await"
        p.items.append(
            f"fn {name}<'a>(cx: &'a Cx, _slf: V, args: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{\n"
            f"    let (args, kwargs) = {RT}::unpack(args);\n"
            f"    let mut __s = {RT}::bind_params({rs(qual)}, 0, args, kwargs, &[{spec}])?;\n"
            f"    let _ = &mut __s;\n"
            f"{call}\n}}) }}"
        )
    return name


def computed_entry(p, info, name: str) -> str:
    """A `@computed_field` of a schema: its most derived property."""
    sym = [s for n, _, s in info.methods if n == name][-1]
    return f"({rs(name)}, {method_wrapper(p, sym)} as {RT}::pyd::MethodFn)"


def method_wrapper(p: Project, sym: Sym) -> str:
    """A `MethodFn` wrapper: binds (self, *args, **kwargs) like CPython (positional, keyword, defaults,
    *args/**kwargs, TypeErrors), keyword arguments arriving packed at the end of `args`."""
    lam = p.__dict__.get("lambda_validators", {}).get(sym)
    if lam is not None:
        name = f"lv_{mod_ident(sym.module)}__{ident(sym.name.replace('<', '').replace('>', ''))}"
        if name not in p.__dict__.setdefault("_wrappers", set()):
            p._wrappers.add(name)
            fc = FnCompiler(p, sym.module, None, name)
            code = fc.expr(lam)
            p.items.append(
                f"fn {name}<'a>(cx: &'a Cx, _cls: V, args: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{\n"
                f"    {' '.join(fc.lines)}\n    let __f = {code};\n    let (args, kwargs) = {RT}::unpack(args);\n"
                f"    {RT}::methods::call_value(cx, &__f, args, kwargs).await\n}}) }}"
            )
        return name
    if sym in p.__dict__.get("vdeco_methods", ()):
        # decorated by functions (`@wraps` wrappers, `@contextmanager`...): `d(f)` built at startup, called with the instance
        name = f"mwd_{mod_ident(sym.module)}__{ident(sym.name)}"
        if name not in p.__dict__.setdefault("_wrappers", set()):
            p._wrappers.add(name)
            g = p.decorated_value(sym)
            p.items.append(
                f"fn {name}<'a>(cx: &'a Cx, slf: V, args: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{\n"
                f"    let __d = {g}(cx).await?;\n    let (args, kwargs) = {RT}::unpack(args);\n"
                f"    {RT}::methods::call_decorated_method(cx, &__d, slf, args, kwargs).await\n}}) }}")
        return name
    if sym in p.__dict__.get("cached_prop_syms", ()):
        # functools.cached_property: computed on the first read, then kept in the instance (which then shadows the property)
        raw = raw_method_wrapper(p, sym, f"mwr_{p.function(sym)}")
        name = f"mwc_{mod_ident(sym.module)}__{ident(sym.name)}"
        if name not in p.__dict__.setdefault("_wrappers", set()):
            p._wrappers.add(name)
            attr = sym.name.split(".")[-1]
            p.items.append(
                f"fn {name}<'a>(cx: &'a Cx, slf: V, args: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{\n"
                f"    let __v = {raw}(cx, slf.clone(), args).await?;\n"
                f"    {RT}::methods::setattr_raw(&slf, {rs(attr)}, __v.clone())?;\n    Ok(__v)\n}}) }}")
        return name
    rust = p.function(sym)
    name = f"mw_{rust}"
    cm = cachedmethod_deco(p, sym, p.fn_node(sym))
    if cm is not None and name not in p.__dict__.setdefault("_wrappers", set()):
        # @cachedmethod: the decorator's configuration built once (class creation), then the cache around the method
        p._wrappers.add(name)
        raw = raw_method_wrapper(p, sym, f"mwr_{rust}")
        g = f"gcm_{rust}"
        fc = FnCompiler(p, sym.module, None, g)
        code = fc.expr(cm)
        body = "\n".join("        " + l for l in fc.lines)
        p.items.append(
            f"static G_{g}: {RT}::Global = {RT}::Global::new();\n"
            f"async fn {g}(cx: &Cx) -> R {{\n    G_{g}.get(cx, |cx| Box::pin(async move {{\n{body}\n        Ok({code})\n    }})).await\n}}\n"
            f"fn {name}<'a>(cx: &'a Cx, slf: V, args: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{\n"
            f"    let __d = {g}(cx).await?;\n    {RT}::ctools::method_call(cx, &__d, slf, args, {raw}).await\n}}) }}")
        return name
    if cm is not None:
        return name
    return raw_method_wrapper(p, sym, name)


def cachedmethod_deco(p: Project, sym: Sym, node) -> ast.AST | None:
    """The `@cachedmethod(...)` decorator expression of a method (None: it has none); refused on a module function,
    an async method, beside another decorator, with info= or a lock taking no instance."""
    found = None
    fc = None
    for d in node.decorator_list:
        target = d.func if isinstance(d, ast.Call) else d
        if (dotted(target) or "").split(".")[-1] != "cachedmethod":
            continue
        fc = fc or FnCompiler(p, sym.module, None, "deco")
        if fc.static_ref(target) == Ext("cachetools.cachedmethod"):
            found = d
    if found is None:
        return None
    if "." not in sym.name:
        raise fc.err(f"@cachedmethod on the function {sym.name} (not a method) is not supported", found)
    if isinstance(node, ast.AsyncFunctionDef):
        raise fc.err("@cachedmethod on an async method is not supported (cachetools caches the coroutine object, "
                             "which a second await refuses)", found)
    if len(node.decorator_list) > 1:
        raise fc.err("@cachedmethod combined with another decorator is not supported", found)
    if not isinstance(found, ast.Call):
        raise fc.err("@cachedmethod without arguments is not supported (cachedmethod(cache) takes the cache)", found)
    return found


def raw_method_wrapper(p: Project, sym: Sym, name: str) -> str:
    """method_wrapper's code for the method itself (under `name`)."""
    rust = p.function(sym)
    if name not in p.__dict__.setdefault("_wrappers", set()):
        p._wrappers.add(name)
        node = p.fn_node(sym)
        decos = {((dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "").split(".")[-1] for d in node.decorator_list}
        params = fn_params(node)
        lead = "static" if "staticmethod" in decos else ("cls" if "classmethod" in decos else "self")
        if lead != "static":
            params = params[1:]
        kinds = {"positional": f"{RT}::P_POS", "vararg": f"{RT}::P_VARARG", "kwonly": f"{RT}::P_KWONLY", "kwarg": f"{RT}::P_KWARG"}
        spec = ", ".join(f"({rs(q.name)}, {kinds[q.kind]}, {str(q.default is not None).lower()})" for q in params)
        vals = []
        for i, q in enumerate(params):
            if q.default is not None:
                sub = FnCompiler(p, sym.module, None, "default")
                e = sub.expr(q.default)
                vals.append(f"match __s[{i}].take() {{ Some(v) => v, None => {{ {' '.join(sub.lines)} {e} }} }}")
            else:
                vals.append(f"__s[{i}].take().unwrap_or(V::None)")
        first = {"self": "slf, ", "cls": f"{RT}::class_of(&slf), ", "static": ""}[lead]
        if has_yield(node):
            # a generator method: the call binds its arguments, the body waits for the first __anext__
            # (as function_wrapper does for a generator function)
            binds = "".join(f"    let __a{i} = {v};\n" for i, v in enumerate(([first[:-2]] if first else []) + vals))
            n = len(vals) + (1 if first else 0)
            call = (f"{binds}    let cx2 = cx.clone();\n    Ok({RT}::web::spawn_gen(move |y| Box::pin(async move {{ "
                    f"{rust}(&cx2{''.join(f', __a{i}' for i in range(n))}, &y).await }})))")
        else:
            call = f"    {rust}(cx, {first}{', '.join(vals)}).await"
        p.items.append(
            f"fn {name}<'a>(cx: &'a Cx, slf: V, args: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{\n"
            f"    let (args, kwargs) = {RT}::unpack(args);\n"
            f"    let mut __s = {RT}::bind_params({rs(sym.name + '()')}, {0 if lead == 'static' else 1}, args, kwargs, &[{spec}])?;\n"
            f"    let _ = &mut __s;\n"
            f"{call}\n}}) }}"
        )
    return name


CARGO = """\
[workspace]

[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

# Generated by py2axum (dyn backend) from {source}. Do not edit by hand: change the Python and regenerate.

[dependencies]
# py2axum's runtime, a library crate of its own (dynrt/): compiled once, apart from the application's code
dynrt = {{ path = "dynrt" }}
# the runtime's dependencies (dynrt/Cargo.toml), which the generated code names too
{deps}
mimalloc = {{ version = "0.1", default-features = false, optional = true }}
{extra}
[features]
# the global allocator (src/main.rs): mimalloc (C, compiled like zlib), faster than the C library's malloc and
# without its contention between threads; `cargo build --release --no-default-features` keeps the system's
default = ["mimalloc"]

[profile.release]
# thin LTO across the runtime and the application: as fast as fat LTO on the measured routes, with a compilation
# peak a third lower on a large project. CARGO_PROFILE_RELEASE_LTO=fat restores the whole-program optimisation.
lto = "thin"
codegen-units = 1
"""


def runtime_dependencies() -> str:
    """The [dependencies] lines of the runtime's Cargo.toml (the generated crate declares the same)."""
    text = (RUNTIME_DIR / "Cargo.toml").read_text()
    section = text.split("\n[dependencies]\n", 1)[1].split("\n[", 1)[0]
    return "\n".join(line for line in section.strip().splitlines() if line and not line.startswith("#"))


MAIN = """\
// Generated by py2axum (dyn backend). Do not edit.
// the runtime's macros, which a small app may not use (`-D warnings` would refuse the unused import)
#[allow(unused_imports)]
#[macro_use]
extern crate dynrt;
mod gen;

use std::sync::Arc;

// the `mimalloc` feature (default): see Cargo.toml
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn env_or(key: &str, default: &str) -> String {{
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}}

fn main() {{
    // the environment CPython starts with (its child processes and os.environ see it), before any thread
    dynrt::coerce_c_locale();
    // Runtime threads get a large stack (reserved address space, touched only as deep as a request goes):
    // CPython accepts JSON nested ~10 000 levels deep (more on 3.14) and the runtime walks such values
    // recursively (validation, serialisation, repr); tokio's 2 MiB default would let one small request
    // overflow it and abort the whole process. PY2AXUM_STACK_SIZE (bytes) overrides it.
    let stack = std::env::var("PY2AXUM_STACK_SIZE").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(dynrt::STACK_SIZE);
    dynrt::set_stack_size(stack);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack)
        .on_thread_start(dynrt::stack_thread_start)
        .build()
        .expect("tokio runtime");
    // `<binary> migrate ...`: the project's alembic migrations (dynrt/migrate.rs), not the server
    if std::env::args().nth(1).as_deref() == Some("migrate") {{
        dynrt::set_python({pymajor}, {pyminor});
        dynrt::logging::init_clock();
        // the runtime's state (the logging module reads it); the pool is never used: `migrate` has its own connection
        let _guard = rt.enter();
        let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1)
            .connect_lazy_with(sqlx::postgres::PgConnectOptions::new());
        dynrt::set_root(Arc::new(dynrt::AppState {{ pool, expire_on_commit: {expire}, autoflush: {autoflush}, commit_after: {commit}, sync_session: {sync} }}));
        let args: Vec<String> = std::env::args().skip(2).collect();
        if gen::MIGRATIONS.is_none() {{
            eprintln!("py2axum migrate: {{}}", gen::MIGRATIONS_WHY);
            std::process::exit(1);
        }}
        std::process::exit(rt.block_on(dynrt::migrate::run(gen::MIGRATIONS, args)));
    }}
    rt.block_on(serve());
}}

async fn serve() {{
    let database_url = env_or("DATABASE_URL", "postgresql://postgres@127.0.0.1/postgres");
    dynrt::orm::set_driver(&database_url);
    let database_url = database_url
        .replace("postgresql+psycopg://", "postgresql://")
        .replace("postgresql+asyncpg://", "postgresql://");
    let pool_size: u32 = env_or("DB_POOL_SIZE", "32").parse().expect("DB_POOL_SIZE");
    let opts: sqlx::postgres::PgConnectOptions = database_url.parse().expect("DATABASE_URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(pool_size)
        .after_connect(|conn, _meta| Box::pin(async move {{
            if dynrt::orm::db_tz_name().is_none() {{
                let name = dynrt::orm::discover_db_tz_conn(conn).await;
                dynrt::orm::set_db_tz(&name);
            }}
            let tz = dynrt::orm::db_tz_name().unwrap_or_else(|| "UTC".into());
            // bound, never spliced: the name comes from the environment or the server's catalog
            sqlx::query("SELECT set_config('TimeZone', $1, false)").bind(&tz).execute(&mut *conn).await?;
            dynrt::orm::session_params(conn).await?;
            Ok(())
        }}))
        .connect_lazy_with(opts);
    let app = Arc::new(dynrt::AppState {{ pool, expire_on_commit: {expire}, autoflush: {autoflush}, commit_after: {commit}, sync_session: {sync} }});
    dynrt::set_root(app.clone());
    dynrt::set_python({pymajor}, {pyminor});
    dynrt::set_python_patch({pypatch});
    dynrt::set_python_features(&[{pyfeatures}]);
    dynrt::orm::set_sqlalchemy({sa_major}, {sa_minor});
    dynrt::set_fastapi({fa_ver});
    dynrt::logging::init_clock();
    dynrt::logging::set_source_root({source_root});
    // uvicorn's Config.configure_logging(): its LOGGING_CONFIG and options, before the import
    dynrt::uvicorn::configure_logging();
    // `import loguru` (its default handler on sys.stderr), when translated code logs through it
    {loguru_init}
    dynrt::web::set_strict_content_type({strict_ct});
    dynrt::web::set_root_path({root_path});
    dynrt::web::set_starlette({starlette_major}, {starlette_minor});
    dynrt::web::set_response_dump_json({dump_json});
    dynrt::methods::set_str_classes(&gen::STR_CLASSES);
    dynrt::set_pydantic("{pydantic}");
    dynrt::sentry::set_sdk("{sentry_sdk}", {multipart});
    dynrt::stripe::set_version("{stripe}");
    gen::register_classes();
    dynrt::asgi::set_python_side(&[{python_side}], &[{mount_fallback}]);
    dynrt::routing::set_app(&gen::APP_ROUTES);
    dynrt::sentry::set_importing(true);
    gen::init_globals(&dynrt::root_cx()).await;
    dynrt::sentry::set_importing(false);
    if let Err(e) = gen::init_prefixes(&dynrt::root_cx()).await {{
        eprintln!("ERROR:py2axum:include_router prefix: {{:?}}", e);
        std::process::exit(1);
    }}
    let router = gen::router(app.clone()){layers};
    dynrt::uvicorn::started();
    // FastAPI(lifespan=...): its code before `yield` runs before the server listens, the rest after it stops
    let lcx = dynrt::root_cx();
    dynrt::uvicorn::lifespan_info("Waiting for application startup.");
    let lifespan = match gen::lifespan(&lcx).await {{
        Ok(None) => None,
        Ok(Some(f)) => match dynrt::web::lifespan_start(&lcx, f).await {{
            Ok(cm) => Some(cm),
            Err(e) => {{
                dynrt::uvicorn::lifespan_failed(&e, "startup");
                std::process::exit(3);
            }}
        }},
        Err(e) => {{
            dynrt::uvicorn::lifespan_failed(&e, "startup");
            std::process::exit(3);
        }}
    }};
    dynrt::uvicorn::lifespan_info("Application startup complete.");
    let listener = dynrt::uvicorn::bind().await;
    let pool = app.pool.clone();
    let serve = axum::serve(listener, router.with_state(app).into_make_service_with_connect_info::<std::net::SocketAddr>());
    // SIGTERM/SIGINT, as uvicorn: no new connection, idle keep-alive connections closed, in-flight requests
    // finished (bounded: an endless stream would hold the shutdown forever), WebSocket sessions closed (1012)
    tokio::select! {{
        r = async {{
            serve.with_graceful_shutdown(dynrt::web::shutdown_signal()).await?;
            dynrt::web::drain_tasks().await;
            Ok::<(), std::io::Error>(())
        }} => r.expect("server"),
        _ = dynrt::web::shutdown_deadline() => {{}}
    }}
    // FastAPI(lifespan=...): the code after `yield`
    dynrt::uvicorn::lifespan_info("Waiting for application shutdown.");
    let ended = match lifespan {{
        Some(cm) => dynrt::web::lifespan_end(&lcx, cm).await,
        None => Ok(()),
    }};
    match ended {{
        Ok(()) => dynrt::uvicorn::lifespan_info("Application shutdown complete."),
        Err(e) => dynrt::uvicorn::lifespan_failed(&e, "shutdown"),
    }}
    dynrt::uvicorn::finished();
    // sentry_sdk initialised: the queued events are sent (the atexit integration of the Python SDK)
    dynrt::sentry::shutdown();
    // like the interpreter: the process ends here (threads still using the runtime included)
    dynrt::web::exit_after_shutdown(&pool).await
}}
"""


def finalize(p: Project) -> None:
    """Register everything the descriptors need (field TDs, method wrappers) until a fixpoint."""
    while True:
        before = (len(p.fns), len(p.tds), len(p.schemas), len(p.models), len(p.dflts), len(p.pats), len(p.items))
        p.drain()
        for info in list(p.schemas.values()):
            for f in info.fields:
                if f.get("any"):
                    p._static_td(f"{RT}::pyd::TD::Any")
                    continue
                ann, module = f["ann"]
                try:
                    p.td(ann, module, p.model_field_cons(info, f))
                except TranspileError as e:
                    if not p.collect:
                        raise
                    p.class_errors[info.sym] = e
            # what validating or serializing an instance runs: reached by every route that uses the class
            cur, p.cur = p.cur, ("class", info.sym)
            try:
                for _, s in info.validators:
                    method_wrapper(p, s)
                if info.__dict__.get("init") is not None:
                    method_wrapper(p, info.__dict__["init"])
                for m in info.__dict__.get("model_after", []) + info.__dict__.get("model_before", []):
                    method_wrapper(p, m)
                if info.__dict__.get("post_init"):
                    method_wrapper(p, info.__dict__["post_init"])
                for _, m in info.__dict__.get("before", []):
                    method_wrapper(p, m)
                # serializers and exclude_if= run while dumping
                for e in [*info.__dict__.get("field_sers", {}).values(), info.__dict__.get("model_ser"),
                          *(f.get("serializer") for f in info.fields)]:
                    if e:
                        method_wrapper(p, e[0])
                for f in info.fields:
                    if f.get("exclude_if"):
                        method_wrapper(p, f["exclude_if"])
                computed = set(info.__dict__.get("computed", []))
                for n, _, s in info.methods:
                    if n in computed:
                        method_wrapper(p, s)
            finally:
                p.cur = cur
            for _, _, s in info.methods:
                method_wrapper(p, s)
        for info in list(p.models.values()):
            for _, _, s in info.methods:
                method_wrapper(p, s)
            if info.rels is None or info.rels_raw or info.__dict__.get("_pending_rels"):
                try:
                    p.resolve_rels(info)
                except TranspileError as e:
                    if not p.collect:
                        raise
                    p.class_errors[info.sym] = e
        after = (len(p.fns), len(p.tds), len(p.schemas), len(p.models), len(p.dflts), len(p.pats), len(p.items))
        if after == before and not p.fn_queue:
            ignored = p.__dict__.get("copy_ignored", [])
            if ignored and p.copies_objects():
                # a module importing copy/pickle was loaded after a class ignored its pickle/copy methods
                sym, st, module = ignored[0]
                e = TranspileError(f"class {sym.name}: method {st.name} is not supported (the application uses copy or "
                                   "pickle, which call it)", st, p.src(module))
                if not p.collect:
                    raise e
                p.fe.global_errors.append(e)
            return


def prepare(fe: Frontend, python_side: set[str]):
    """Run the discovery passes of the frontend that the dyn backend shares (no IR building)."""
    fe._load_imports()
    for mod, node, names in fe.index.lazy:
        project = sorted((n for n in names if fe.index.module(n) is not None), key=len)
        if project:
            fe._global(TranspileError(
                f"`lazy` import of the project module `{project[-1]}` is not supported: the binary runs every project "
                "module's top-level code at startup, Python 3.15 at its first use. Import it without `lazy` "
                "(lazy imports of libraries are translated like plain imports)",
                node, str(fe.index.modules[mod].path)))
    fe._classify()
    fe._discover_apps()
    collect = fe.collect
    fe.collect = True
    gzip = fe._middlewares()
    fe.collect = collect
    gzip = list_gzip(fe, gzip)
    kept = []
    on_events: list[TranspileError] = []
    for e in fe.global_errors:
        txt = e.render()
        if (e.node is not None and e.file and stack_node(fe, fe.index.module_of(e.file).name, e.node)
                and not ("add_route" in txt and (_route_path(fe, e) in python_side or fe.__dict__.get("auto_side") is not None))):
            continue  # compiled into the middleware stack (generate_project)
        raw = isinstance(e.node, ast.Call) and isinstance(e.node.func, ast.Attribute) and e.node.func.attr in {
            "add_route", "add_api_route", "add_websocket_route", "add_api_websocket_route"}
        if raw and e.node.args:
            # `app.add_api_route("/readiness", readiness, methods=["GET"])`: a route registered by a call, its literal
            # path relayed before routing (the Python app answers it, in its own registration order)
            mod = fe.index.module_of(e.file).name
            try:
                path = Project(fe).const(e.node.args[0], mod)
            except TranspileError:
                path = None
            if path in python_side:
                fe.notes.append(f"{txt} — {path} declared Python-side")
                continue
            if isinstance(path, str) and fe.__dict__.get("auto_side") is not None:
                fe.auto_side[path] = e  # --python-side auto: a raw route with a literal path moves too
                continue
        if isinstance(e.node, ast.Call) and isinstance(e.node.func, ast.Attribute) and e.node.func.attr == "websocket":
            continue  # @app.websocket: compiled (ws_route)
        if isinstance(e.node, ast.Call) and isinstance(e.node.func, ast.Attribute) and e.node.func.attr == "on_event":
            on_events.append(e)  # compiled into a lifespan (events_lifespan), or refused there
            continue
        if _is_mount(e):
            why = mount_not_last(fe, e)
            if why:
                kept.append(TranspileError(
                    "app.mount(...) can stay on the Python side only among the app's last registrations "
                    f"(nothing but other mounts after it, in the same module and function): {why}", e.node, e.file))
            elif "mount" in python_side or fe.__dict__.get("auto_side") is not None or collect:
                fe.mount_fallback = [*fe.__dict__.get("mount_fallback", []), mount_prefix(fe, e)]
                fe.notes.append(f"{e.file}:{e.node.lineno}: app.mount(...) stays on the Python side: a request that no "
                                "translated route fully matches (path and method) is relayed to PY2AXUM_PYTHON_URL")
            else:
                kept.append(TranspileError(
                    "app.mount(...) is not translated: as the app's last registration it can stay on the Python side "
                    "(--python-side mount, or --python-side auto)", e.node, e.file))
            continue
        if "middleware" in txt and e.node is not None and _conditional(fe, e):
            fe.notes.append(f"{txt} — under an `if`: ignored (documented difference)")
            continue
        if "(only GZipMiddleware)" in e.msg and isinstance(e.node, ast.Call):
            # the dyn backend compiles more middlewares than the typed one
            e = TranspileError(e.msg.replace("(only GZipMiddleware)", "(CORSMiddleware, GZipMiddleware, BaseHTTPMiddleware, a "
                                             "project BaseHTTPMiddleware subclass or raw ASGI class with `async def "
                                             "__call__(self, scope, receive, send)`, starlette_context's RawContextMiddleware)"),
                               e.node, e.file)
        kept.append(e)
    fe.global_errors = kept
    fe.python_side = set(python_side)
    for m in fe.index.package_modules():
        for node in ast.walk(m.tree):
            if isinstance(node, ast.Call) and dotted(node.func) in {"FastAPI", "fastapi.FastAPI"}:
                for kw in node.keywords:
                    if kw.arg == "lifespan" and "lifespan" not in python_side:
                        if not (isinstance(kw.value, ast.Constant) and kw.value.value is None):
                            fe.lifespan = (m.name, kw.value)  # compiled natively (generate_project)
                    elif kw.arg in FASTAPI_DOC_OPTIONS or kw.arg == "lifespan":
                        pass  # OpenAPI/docs metadata: no effect on the translated routes (/docs is not served)
                    elif kw.arg == "debug" and isinstance(kw.value, ast.Constant) and kw.value.value is False:
                        pass
                    elif kw.arg == "middleware":
                        pass  # compiled into the middleware stack (build_stack)
                    elif kw.arg == "dependencies":
                        # the app's own dependencies: first for every route (FastAPI's add_api_route prepends them)
                        fe.app_deps = [(kw, str(m.path))]
                    elif kw.arg == "root_path":
                        if isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                            fe.root_path = (kw.value.value, kw, str(m.path))
                        else:
                            kept.append(TranspileError("FastAPI(root_path=...) must be a literal string", kw, str(m.path)))
                    elif kw.arg == "root_path_in_servers":
                        pass  # the OpenAPI `servers` only (/openapi.json is not served)
                    elif kw.arg == "default_response_class":
                        t = fe.index.resolve_expr(m.name, kw.value)
                        last = t.dotted.split(".")[-1] if isinstance(t, Ext) and t.dotted.split(".")[0] in {"fastapi", "starlette"} else None
                        if isinstance(t, Sym) and isinstance(fe.index.definition(t), ast.ClassDef):
                            # a project response class: checked and applied per route (RouteBuilder.response_subclass)
                            fe.default_resp_custom = (kw.value, m.name)
                        elif last not in {"JSONResponse", "HTMLResponse", "PlainTextResponse", "ORJSONResponse"}:
                            kept.append(TranspileError(f"FastAPI(default_response_class={ast.unparse(kw.value)}) is not "
                                                       "supported (JSONResponse, ORJSONResponse, HTMLResponse or "
                                                       "PlainTextResponse)", kw, str(m.path)))
                        elif last != "JSONResponse":
                            fe.default_resp_cls = last
                    elif kw.arg == "telemetry":
                        # FastAPI 0.142's OpenTelemetry: the binary emits no telemetry (docs/supported.md); options that
                        # only switch parts of it on or off are accepted, a provider or `exclude=` refused
                        v = kw.value
                        fv = tuple(int(x) for x in re.findall(r"\d+", locked_version(fe.index.root, "fastapi") or "0.142")[:2])
                        if fv < (0, 142):
                            kept.append(TranspileError("FastAPI(telemetry=...) appeared in FastAPI 0.142 (the locked version "
                                                       "raises TypeError)", kw, str(m.path)))
                        elif not (isinstance(v, ast.Dict) and None not in v.keys and all(
                                isinstance(k, ast.Constant) and k.value in TELEMETRY_SWITCHES for k in v.keys)
                                and all(isinstance(x, ast.Constant) and isinstance(x.value, bool) for x in v.values)):
                            kept.append(TranspileError("FastAPI(telemetry=...) is supported as a dict literal of "
                                                       f"{', '.join(sorted(TELEMETRY_SWITCHES))} switched True or False only "
                                                       "(the binary emits no OpenTelemetry data)", kw, str(m.path)))
                    elif kw.arg == "strict_content_type":
                        if isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, bool):
                            fe.strict_ct = kw.value.value
                        else:
                            kept.append(TranspileError("FastAPI(strict_content_type=...) must be a literal True or False",
                                                       kw, str(m.path)))
                    else:
                        kept.append(TranspileError(f"FastAPI({kw.arg}=...) is not supported", kw, str(m.path)))
    if on_events:
        kept += [scoped(e, "lifespan") for e in events_lifespan(fe, on_events, python_side)]
    fe._discover_routers()
    n_before = len(fe.global_errors)
    fe.collect = True
    fe._mounts_cache = fe._mounts()
    fe.collect = collect
    new = fe.global_errors[n_before:]
    del fe.global_errors[n_before:]
    for e in new:
        if _is_lib_router(e):
            e = lib_router_side(fe, e, python_side, collect)
            if e is None:
                continue
        elif (isinstance(e.node, ast.Call) and isinstance(e.node.func, ast.Attribute) and e.node.func.attr == "include_router"
              and "cannot resolve" in e.msg and (prefix := factory_router_prefix(fe, e)) is not None):
            # a router a project function builds from its arguments: every route of it under `prefix`, relayed
            fe.python_side = {*fe.__dict__.get("python_side", ()), prefix}
            fe.__dict__.setdefault("configure_includes", set()).add(id(e.node))
            fe.__dict__.setdefault("lib_prefix_includes", set()).add(id(e.node))
            fe.notes.append(f"{e.file}:{e.node.lineno}: include_router({ast.unparse(e.node.args[0])[:60]}) stays on the "
                            f"Python side: every request under {prefix} is relayed to PY2AXUM_PYTHON_URL (the router its "
                            "function builds has all its routes under that prefix)")
            continue
        fe.global_errors.append(e)
    if fe.global_errors and not fe.collect:
        raise fe.global_errors[0]
    return gzip


def events_lifespan(fe: Frontend, decos: list[TranspileError], python_side: set[str]) -> list[TranspileError]:
    """`@app.on_event("startup"/"shutdown")`: FastAPI's default lifespan (`_DefaultLifespan`) runs the startup
    handlers in registration order before serving, the shutdown ones after. Compiled as that lifespan, a
    generated async generator (`await h()` or `h()` for each handler, `yield` between both lists) in the app's
    module. Refused: next to `FastAPI(lifespan=)` (FastAPI ignores the handlers then), a non-literal event
    name, a handler with parameters, handlers in several modules. Errors are returned (refusals)."""
    out = []
    if "lifespan" in python_side:
        fe.notes.append("@app.on_event(...) handlers run in the Python process (--python-side lifespan)")
        return out
    mods = {fe.index.module_of(e.file).name for e in decos}
    if len(mods) != 1:
        return [TranspileError("@app.on_event(...) handlers in several modules are not supported", e.node, e.file)
                for e in decos[1:2]]
    module = mods.pop()
    m = fe.index.module(module)
    if fe.__dict__.get("lifespan") is not None:
        return [TranspileError("@app.on_event(...) next to FastAPI(lifespan=...) is not supported (FastAPI then "
                               "ignores the handlers)", decos[0].node, decos[0].file)]
    handlers = {"startup": [], "shutdown": []}
    deco_nodes = {id(e.node): e for e in decos}
    for node in m.tree.body:
        if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            continue
        for d in node.decorator_list:
            e = deco_nodes.get(id(d))
            if e is None:
                continue
            ev = d.args[0] if len(d.args) == 1 and not d.keywords else None
            if not (isinstance(ev, ast.Constant) and ev.value in handlers):
                out.append(TranspileError('@app.on_event(...) needs a literal "startup" or "shutdown"', d, e.file))
                continue
            if fn_params(node):
                out.append(TranspileError(f"@app.on_event(...) handler {node.name} must take no parameters", node, e.file))
                continue
            if len(node.decorator_list) != 1:
                out.append(TranspileError(f"@app.on_event(...) handler {node.name}: other decorators are not supported",
                                          node, e.file))
                continue
            handlers[ev.value].append(node)
    if len([x for v in handlers.values() for x in v]) + len(out) != len(decos):
        out.append(TranspileError("@app.on_event(...) is supported on a module-level function only", decos[0].node,
                                  decos[0].file))
    if out:
        return out
    name = "__py2axum_on_event_lifespan"

    def call(h):
        # the handler without its decorator (`app.on_event(...)` registers it, nothing else): a copy under its own name
        plain = f"__py2axum_on_event_{h.name}"
        if plain not in m.defs:
            m.defs[plain] = type(h)(name=plain, args=h.args, body=h.body, decorator_list=[], returns=h.returns,
                                    type_params=[], lineno=h.lineno, end_lineno=h.end_lineno, col_offset=h.col_offset,
                                    end_col_offset=h.end_col_offset)
        c = ast.Call(func=ast.Name(plain, ast.Load()), args=[], keywords=[])
        return ast.Expr(ast.Await(c) if isinstance(h, ast.AsyncFunctionDef) else c)

    body = [call(h) for h in handlers["startup"]] + [ast.Expr(ast.Yield(None))] + [call(h) for h in handlers["shutdown"]]
    args = ast.arguments(posonlyargs=[], args=[ast.arg("_app")], vararg=None, kwonlyargs=[], kw_defaults=[], kwarg=None,
                         defaults=[])
    fn = ast.AsyncFunctionDef(name=name, args=args, body=body, decorator_list=[], returns=None, type_params=[])
    line = decos[0].node.lineno
    for n in ast.walk(fn):
        if "lineno" in n._attributes:
            n.lineno, n.end_lineno, n.col_offset, n.end_col_offset = line, line, 0, 0
    m.defs[name] = fn
    ref = ast.Name(name, ast.Load())
    ref.lineno, ref.end_lineno, ref.col_offset, ref.end_col_offset = line, line, 0, 0
    fe.lifespan = (module, ref)
    return out


MW_CORS = {"starlette.middleware.cors.CORSMiddleware", "fastapi.middleware.cors.CORSMiddleware"}
MW_HTTPS = {"starlette.middleware.httpsredirect.HTTPSRedirectMiddleware",
            "fastapi.middleware.httpsredirect.HTTPSRedirectMiddleware"}
MW_TRUSTED = {"starlette.middleware.trustedhost.TrustedHostMiddleware",
              "fastapi.middleware.trustedhost.TrustedHostMiddleware"}
MW_BASE = {"starlette.middleware.base.BaseHTTPMiddleware", "fastapi.middleware.base.BaseHTTPMiddleware"}
# pydantic's schema hooks: read statically (py2axum/coreschema.py), never compiled; the JSON schema one is refused
# where a JSON schema is asked for
SCHEMA_HOOKS = {"__get_pydantic_core_schema__", "__get_pydantic_json_schema__"}
ROUTE_BASES = {"fastapi.routing.APIRoute", "fastapi.APIRoute"}
REQUEST_BASES = {"fastapi.Request", "fastapi.requests.Request", "starlette.requests.Request"}
MW_GZIP = {"starlette.middleware.gzip.GZipMiddleware", "fastapi.middleware.gzip.GZipMiddleware"}
# Starlette >= 1.0's SessionMiddleware changes a response only when the session was read or written
# (`Session.accessed`/`.modified`): with every read of `request.session` / `scope["session"]` refused in the
# translated code (FnCompiler.SCOPE_BY_MIDDLEWARE), it leaves the binary's responses as they are
MW_SESSION = {"starlette.middleware.sessions.SessionMiddleware", "fastapi.middleware.sessions.SessionMiddleware"}
# sentry-sdk 2's SentryAsgiMiddleware added explicitly: sentry-sdk's FastAPI/Starlette integration already wraps the
# app (the middleware is applied once), and the binary reports as that integration does (dynrt/sentry.rs)
MW_SENTRY = {"sentry_sdk.integrations.asgi.SentryAsgiMiddleware"}


# what registers a route on the app (call or decorator), in Starlette's routing order
APP_ROUTE_CALLS = {"include_router", "mount", "host", "add_api_route", "add_route", "add_websocket_route",
                   "add_api_websocket_route"}
APP_ROUTE_DECOS = {"get", "post", "put", "patch", "delete", "head", "options", "trace", "api_route", "route",
                   "websocket", "websocket_route"}


def _is_mount(e: TranspileError) -> bool:
    return (isinstance(e.node, ast.Call) and isinstance(e.node.func, ast.Attribute) and e.node.func.attr == "mount"
            and "is not supported" in e.msg)


def _is_lib_router(e: TranspileError) -> bool:
    return (isinstance(e.node, ast.Call) and isinstance(e.node.func, ast.Attribute) and e.node.func.attr == "include_router"
            and "is a router of the library" in e.msg)


def lib_router_side(fe: Frontend, e: TranspileError, python_side: set[str], collect: bool) -> TranspileError | None:
    """`app.include_router(<library router>)`: left to Python (None) like a final mount, else the refusal."""
    prefix = lib_router_static_prefix(fe, e)
    if prefix:
        # every route of the library is under `prefix`: a request there is relayed to the Python app, which routes it
        # as Starlette does (its own routes included, whatever the registration order); no other request can reach
        # the library's routes
        fe.python_side = {*fe.__dict__.get("python_side", ()), prefix}
        fe.__dict__.setdefault("configure_includes", set()).add(id(e.node))
        fe.__dict__.setdefault("lib_prefix_includes", set()).add(id(e.node))
        fe.notes.append(f"{e.file}:{e.node.lineno}: include_router({ast.unparse(e.node.args[0])}) stays on the Python side: "
                        f"every request under {prefix} is relayed to PY2AXUM_PYTHON_URL (the library's routes are all "
                        "under that prefix, read from its installed source)")
        return None
    why = lib_router_refusal(fe, e)
    if why:
        return TranspileError(f"{e.msg}; it can stay on the Python side only as one of the app's last registrations: {why}",
                              e.node, e.file)
    if "mount" in python_side or fe.__dict__.get("auto_side") is not None or collect:
        fe.mount_fallback = [*fe.__dict__.get("mount_fallback", []), lib_router_prefix(fe, e)]
        fe.notes.append(f"{e.file}:{e.node.lineno}: include_router({ast.unparse(e.node.args[0])}) stays on the Python side: "
                        "a request that no translated route fully matches (path and method) is relayed to PY2AXUM_PYTHON_URL")
        return None
    return TranspileError(f"{e.msg}; as the app's last registration it can stay on the Python side (--python-side mount, "
                          "or --python-side auto)", e.node, e.file)


def lib_router_prefix(fe: Frontend, e: TranspileError) -> str:
    """The literal `prefix=` of `app.include_router(<library router>, prefix=...)` ("" without one: every path)."""
    kw = next((k for k in e.node.keywords if k.arg == "prefix"), None)
    return "" if kw is None else Project(fe).const(kw.value, fe.index.module_of(e.file).name).rstrip("/")


def lib_router_static_prefix(fe: Frontend, e: TranspileError) -> str | None:
    """The full prefix every route of `app.include_router(<library router>[, prefix="/p"])` is under, when it can be
    read statically: the router is a module global `r = APIRouter(prefix="/x", ...)` of the library's installed
    source whose routes are only added with `@r.<method>(...)` decorators (no mount, no included router, no
    add_route), and the include's own prefix is a literal. None otherwise (an empty prefix included: every path)."""
    from . import libsrc
    from .frontend import _enclosing_fn

    mod = fe.index.module_of(e.file).name
    call = e.node
    if not (call.args and fe._is_app(mod, call.func.value)):
        return None
    outer = ""
    kw = next((k for k in call.keywords if k.arg == "prefix"), None)
    if kw is not None:
        if not (isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str)):
            return None
        outer = kw.value.value
    if any(k.arg not in {"prefix", "tags", "dependencies", "responses", "include_in_schema", "deprecated"}
           for k in call.keywords):
        return None
    scope = _enclosing_fn(call, fe._parents(mod))
    t = fe.index.resolve_expr(mod, call.args[0], scope)
    if not isinstance(t, Ext) or "." not in t.dotted:
        return None
    lib_mod, _, var = t.dotted.rpartition(".")
    rel = Path(*lib_mod.split("."))
    try:
        sites = libsrc.site_packages(Path(fe.index.root))
    except ValueError:
        return None
    src = next((p for s in sites for p in (s / rel.with_suffix(".py"), s / rel / "__init__.py") if p.is_file()), None)
    if src is None:
        return None
    try:
        tree = ast.parse(src.read_text(encoding="utf-8"))
    except (OSError, SyntaxError, UnicodeDecodeError):
        return None
    inner = None
    for st in tree.body:
        if (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name)
                and st.targets[0].id == var):
            if inner is not None or not (isinstance(st.value, ast.Call) and (dotted(st.value.func) or "").split(".")[-1] == "APIRouter"):
                return None
            pk = next((k for k in st.value.keywords if k.arg == "prefix"), None)
            if pk is None or not (isinstance(pk.value, ast.Constant) and isinstance(pk.value.value, str)):
                return None
            inner = pk.value.value
    if inner is None:
        return None
    for n in ast.walk(tree):
        # anything else than `@router.<method>(...)` that adds a route to the router (or rebinds it)
        if (isinstance(n, ast.Attribute) and isinstance(n.value, ast.Name) and n.value.id == var
                and n.attr in {"mount", "include_router", "add_route", "add_api_route", "add_websocket_route",
                               "host", "routes", "prefix"}):
            return None
        if isinstance(n, (ast.Global, ast.Nonlocal)) and var in n.names:
            return None
    prefix = (outer.rstrip("/") + inner).rstrip("/")
    if not prefix.startswith("/") or "{" in prefix:
        return None
    return prefix


def factory_router_prefix(fe: Frontend, e: TranspileError) -> str | None:
    """`app.include_router(build_router(..., prefix="/x"))`, `build_router` a project function that makes one local
    `r = APIRouter(prefix=<literal, or one of its parameters>)`, adds routes to it only with `@r.<method>(...)`
    decorators and returns it: the full prefix every route is under (with the include's literal prefix), when it
    can be read statically (a literal argument, else the parameter's default, a literal or a module constant)."""
    mod = fe.index.module_of(e.file).name
    call = e.node
    if not (call.args and isinstance(call.args[0], ast.Call) and fe._is_app(mod, call.func.value)):
        return None
    outer = ""
    for k in call.keywords:
        if k.arg == "prefix":
            if not (isinstance(k.value, ast.Constant) and isinstance(k.value.value, str)):
                return None
            outer = k.value.value
        elif k.arg not in {"tags", "dependencies", "responses", "include_in_schema", "deprecated"}:
            return None
    made = call.args[0]
    scope = frontend_enclosing_fn(call, fe._parents(mod))
    t = fe.index.resolve_expr(mod, made.func, scope) if isinstance(made.func, (ast.Name, ast.Attribute)) else None
    fn = fe.index.definition(t) if isinstance(t, Sym) else None
    if not isinstance(fn, ast.FunctionDef) or fn.decorator_list:
        return None
    routers = [st for st in ast.walk(fn) if isinstance(st, ast.Assign) and len(st.targets) == 1
               and isinstance(st.targets[0], ast.Name) and isinstance(st.value, ast.Call)
               and is_ext(fe.index.resolve_expr(t.module, st.value.func), "fastapi.APIRouter")]
    if len(routers) != 1:
        return None
    local = routers[0].targets[0].id
    rets = [n for n in walk_scope(fn) if isinstance(n, ast.Return)]  # the endpoints' own returns excluded
    if not rets or not all(isinstance(r.value, ast.Name) and r.value.id == local for r in rets):
        return None
    for n in ast.walk(fn):
        if (isinstance(n, ast.Attribute) and isinstance(n.value, ast.Name) and n.value.id == local
                and n.attr not in HTTP_METHODS | {"api_route"}):
            return None  # include_router, mount, add_api_route, routes...: a route that may be elsewhere
        if isinstance(n, ast.Name) and n.id == local and isinstance(n.ctx, ast.Store) and n is not routers[0].targets[0]:
            return None
    pk = next((k for k in routers[0].value.keywords if k.arg == "prefix"), None)
    if pk is None:
        return None
    proj = Project(fe)

    def const(node, module):
        try:
            v = proj.const(node, module)
        except TranspileError:
            return None
        return v if isinstance(v, str) else None
    if isinstance(pk.value, ast.Constant):
        inner = pk.value.value if isinstance(pk.value.value, str) else None
    elif isinstance(pk.value, ast.Name):
        a = fn.args
        params = [*a.posonlyargs, *a.args]
        names = [x.arg for x in params]
        if pk.value.id in names:
            i = names.index(pk.value.id)
            given = made.args[i] if i < len(made.args) else next((k.value for k in made.keywords if k.arg == pk.value.id), None)
            if given is not None:
                inner = const(given, mod)
            else:
                d = dict(zip(names[len(names) - len(a.defaults):], a.defaults)).get(pk.value.id)
                inner = const(d, t.module) if d is not None else None
        elif pk.value.id in [x.arg for x in a.kwonlyargs]:
            given = next((k.value for k in made.keywords if k.arg == pk.value.id), None)
            d = dict(zip([x.arg for x in a.kwonlyargs], a.kw_defaults)).get(pk.value.id)
            inner = const(given, mod) if given is not None else const(d, t.module) if d is not None else None
        else:
            inner = None
        if any(isinstance(n, ast.Name) and n.id == pk.value.id and isinstance(n.ctx, ast.Store) for n in ast.walk(fn)):
            return None
    else:
        inner = None
    if inner is None or any(isinstance(x, ast.Starred) for x in made.args) or any(k.arg is None for k in made.keywords):
        return None
    prefix = (outer.rstrip("/") + inner).rstrip("/")
    if not prefix.startswith("/") or "{" in prefix:
        return None
    return prefix


def lib_router_refusal(fe: Frontend, e: TranspileError) -> str | None:
    """`app.include_router(<library router>)`: why its routes cannot be left to Python, else None. Starlette tries
    the routes in registration order and the library's are unknown here: whatever the app registers after them (a
    route, or a function given the app that registers some) would be tried before them by the binary."""
    from .frontend import _enclosing_fn

    mod = fe.index.module_of(e.file).name
    call = e.node
    if not fe._is_app(mod, call.func.value):
        return "only `app.include_router(...)` on the application itself is supported (not on a router)"
    kw = next((k for k in call.keywords if k.arg == "prefix"), None)
    if kw is not None:
        try:
            if not isinstance(Project(fe).const(kw.value, mod), str):
                raise TranspileError("", kw.value, e.file)
        except TranspileError:
            return f"its prefix `{ast.unparse(kw.value)}` must be a literal string"
    parents = fe._parents(mod)
    scope = _enclosing_fn(call, parents)
    if any(isinstance(p, (ast.If, ast.For, ast.While, ast.Try, ast.With)) for p in _ancestors(call, parents, scope)):
        return "it is registered conditionally (under an `if`, a loop, a `try` or a `with`)"
    # a route a function given the app registers after it (`configure(app)` -> `app.add_route`) is added at startup:
    # the binary relays it to Python, which tries the library's routes first (routing::lib_barrier)
    return mount_not_last(fe, e)


def _ancestors(node: ast.AST, parents: dict, stop) -> list:
    out = []
    p = parents.get(node)
    while p is not None and p is not stop:
        out.append(p)
        p = parents.get(p)
    return out


def mount_not_last(fe: Frontend, e: TranspileError) -> str | None:
    """`app.mount(...)`: why it is not among the app's last registrations (only mounts follow it, in the same
    module and scope), else None. Starlette's router tries routes in order and a mount matches every path under
    its prefix: a route registered after it would be shadowed in Python but served by the binary."""
    from .frontend import _enclosing_fn

    mod = fe.index.module_of(e.file).name
    parents = fe._parents(mod)
    scope = _enclosing_fn(e.node, parents)
    prefix = mount_prefix(fe, e)

    def is_app(module: str, node: ast.AST) -> bool:  # the app, also imported from its module
        if fe._is_app(module, node):
            return True
        t = fe.index.resolve_expr(module, node) if isinstance(node, (ast.Name, ast.Attribute)) else None
        return isinstance(t, Sym) and t.name in fe.app_vars.get(t.module, ())
    for m in fe.index.package_modules():
        ps = fe._parents(m.name)
        for node in ast.walk(m.tree):
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr in APP_ROUTE_CALLS:
                owner = node.func.value
                if not (is_app(m.name, owner) or (isinstance(owner, ast.Attribute) and owner.attr == "router"
                                                  and is_app(m.name, owner.value))):
                    continue
                if node.func.attr == "mount" or node is e.node or (node.func.attr == "include_router" and node.args
                                                                     and fe._library_router(m.name, node.args[0], _enclosing_fn(node, ps))):
                    continue  # left to Python too (mounts, routers of libraries)
                at, where = node, node
            elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                d = next((d for d in node.decorator_list
                          if isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute)
                          and d.func.attr in APP_ROUTE_DECOS and is_app(m.name, d.func.value)), None)
                if d is None:
                    continue
                at, where = node, d
            else:
                continue
            what = f"`{ast.unparse(where.func)}(...)` at {m.path}:{where.lineno}"
            if prefix and not _may_reach_mount(fe, m.name, where, prefix):
                continue  # its paths are outside the mount's prefix: the order does not matter
            if m.name != mod:
                return f"{what} is in another module (the registration order depends on the imports)"
            if _enclosing_fn(at, ps) is not scope:
                return f"{what} is in another function (the registration order depends on the calls)"
            if where.lineno > e.node.lineno:
                return f"{what} comes after it"
    return None


def _may_reach_mount(fe: Frontend, mod: str, call: ast.Call, prefix: str) -> bool:
    """Whether a registration (`@app.get(path)`, `app.add_api_route(path, ...)`, `app.include_router(r,
    prefix=...)`) may match a path under a mount's prefix (the prefix itself or below it): its literal part before
    the first parameter is compared; not a constant, or an included router without a prefix, may."""
    if call.func.attr == "include_router":
        arg = next((k.value for k in call.keywords if k.arg == "prefix"), None)
        wild = True  # the router's own paths follow the prefix
    else:
        arg = call.args[0] if call.args else next((k.value for k in call.keywords if k.arg == "path"), None)
        wild = False
    try:
        path = Project(fe).const(arg, mod) if arg is not None else None
    except TranspileError:
        return True
    if not isinstance(path, str) or (wild and not path):
        return True
    lit, brace, _ = path.partition("{")
    if wild or brace:
        # the rest of the path is free: it reaches the mount when it extends the prefix, or the prefix extends it
        return lit == prefix or lit.startswith(prefix + "/") or prefix.startswith(lit)
    return path == prefix or path.startswith(prefix + "/")


def mount_prefix(fe: Frontend, e: TranspileError) -> str:
    """The path of `app.mount(path, ...)` as Starlette keeps it (trailing `/` removed); "" (every path) when it
    is not a constant."""
    call = e.node
    arg = call.args[0] if call.args else next((k.value for k in call.keywords if k.arg == "path"), None)
    try:
        path = Project(fe).const(arg, fe.index.module_of(e.file).name)
    except TranspileError:
        return ""
    return path.rstrip("/") if isinstance(path, str) else ""


def _route_path(fe: Frontend, e: TranspileError):
    """The literal path of an `add_route(...)` global error, else None."""
    if not (isinstance(e.node, ast.Call) and e.node.args):
        return None
    try:
        return Project(fe).const(e.node.args[0], fe.index.module_of(e.file).name)
    except TranspileError:
        return None


# starlette_context's `context` (a UserDict over the request's context dict, dynrt/ctxmw.rs)
STARLETTE_CONTEXT = {"starlette_context.context", "starlette_context.ctx.context"}
MW_CONTEXT = {"starlette_context.middleware.RawContextMiddleware",
              "starlette_context.middleware.raw_middleware.RawContextMiddleware"}
# starlette_context plugins the runtime ports (dynrt/ctxmw.rs): class -> header key
CONTEXT_PLUGINS = {**{f"starlette_context.plugins.{n}": k for n, k in (("RequestIdPlugin", "X-Request-ID"),
                                                                      ("CorrelationIdPlugin", "X-Correlation-ID"))},
                   "starlette_context.plugins.request_id.RequestIdPlugin": "X-Request-ID",
                   "starlette_context.plugins.correlation_id.CorrelationIdPlugin": "X-Correlation-ID"}
MW_ENTRY = {"starlette.middleware.Middleware", "fastapi.middleware.Middleware"}


def asgi_call_method(fe: Frontend, sym: Sym):
    """`async def __call__(self, scope, receive, send)` of a project class (or of its project bases), else None."""
    seen = set()
    while isinstance(sym, Sym) and sym not in seen:
        seen.add(sym)
        d = fe.index.definition(sym)
        if not isinstance(d, ast.ClassDef):
            return None
        for f in d.body:
            if isinstance(f, (ast.FunctionDef, ast.AsyncFunctionDef)) and f.name == "__call__":
                a = f.args
                ok = (isinstance(f, ast.AsyncFunctionDef) and len(a.posonlyargs) + len(a.args) == 4
                      and not a.vararg and not a.kwarg and not a.kwonlyargs)
                return f if ok else None
        bases = [fe.index.resolve_expr(sym.module, b) for b in d.bases]
        bases = [b for b in bases if isinstance(b, Sym)]
        sym = bases[0] if len(bases) == 1 and len(d.bases) == 1 else None
    return None


def mw_class_kind(fe: Frontend, module: str, cls: ast.AST) -> str | None:
    """The kind of a middleware class: cors, base, user (a project subclass of BaseHTTPMiddleware), asgi (a
    project class with `async __call__(self, scope, receive, send)`), context (starlette_context's
    RawContextMiddleware), gzip (a tower layer) or None (unsupported)."""
    t = fe.index.resolve_expr(module, cls)
    if isinstance(t, Ext):
        c = libmap.canonical(t.dotted)
        return ("cors" if c in MW_CORS else "base" if c in MW_BASE else "gzip" if c in MW_GZIP
                else "context" if c in MW_CONTEXT else "https" if c in MW_HTTPS else "trusted" if c in MW_TRUSTED
                else "session" if c in MW_SESSION else "sentry" if c in MW_SENTRY else None)
    if isinstance(t, Sym):
        d = fe.index.definition(t)
        if isinstance(d, ast.ClassDef) and len(d.bases) == 1:
            b = fe.index.resolve_expr(t.module, d.bases[0])
            if isinstance(b, Ext) and libmap.canonical(b.dotted) in MW_BASE:
                return "user"
        if isinstance(d, ast.ClassDef) and asgi_call_method(fe, t) is not None:
            return "asgi"
    return None


def mw_kind(fe: Frontend, module: str, call: ast.Call) -> str | None:
    """The middleware class of `app.add_middleware(X, ...)` (see mw_class_kind)."""
    if not call.args:
        return None
    return mw_class_kind(fe, module, call.args[0])


def mw_entries(fe: Frontend, module: str, call: ast.Call) -> list | None:
    """`FastAPI(..., middleware=[Middleware(X, ...), ...])`: the list's elements, else None."""
    kw = next((k for k in call.keywords if k.arg == "middleware"), None)
    if kw is None or not (isinstance(call.func, (ast.Name, ast.Attribute)) and dotted(call.func) in {"FastAPI", "fastapi.FastAPI"}):
        return None
    if isinstance(kw.value, ast.Name):
        lit = _single_list_binding(fe, module, call, kw.value)
        if lit is not None:
            return list(lit.elts)
    if isinstance(kw.value, ast.Call) and not kw.value.args and not kw.value.keywords:
        lit = _returned_list(fe, module, kw.value.func)
        if lit is not None:
            return list(lit.elts)
    if not isinstance(kw.value, (ast.List, ast.Tuple)):
        raise TranspileError("FastAPI(middleware=...) must be a literal list of Middleware(...)", kw.value, str(fe.index.module(module).path))
    return list(kw.value.elts)


def _returned_list(fe: Frontend, module: str, func: ast.AST) -> ast.List | None:
    """`make_middleware()`: a function of the same module without parameters whose body only returns a list literal
    (`return [...]`, or `x = [...]` then `return x`)"""
    t = fe.index.resolve_expr(module, func)
    d = fe.index.definition(t) if isinstance(t, Sym) and t.module == module else None
    if not isinstance(d, ast.FunctionDef) or d.decorator_list or d.args.args or d.args.vararg or d.args.kwonlyargs or d.args.kwarg:
        return None
    body = [st for st in d.body if not (isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant))]
    if len(body) == 1 and isinstance(body[0], ast.Return) and isinstance(body[0].value, ast.List):
        return body[0].value
    if (len(body) == 2 and isinstance(body[0], ast.Assign) and len(body[0].targets) == 1
            and isinstance(body[0].targets[0], ast.Name) and isinstance(body[0].value, ast.List)
            and isinstance(body[1], ast.Return) and isinstance(body[1].value, ast.Name)
            and body[1].value.id == body[0].targets[0].id):
        return body[0].value
    return None


def session_mw_options(p: "Project", module: str, call: ast.Call) -> None:
    """`SessionMiddleware(app, secret_key, ...)`: its options, checked as Starlette's signature checks them (the
    layer itself is transparent, MW_SESSION)."""
    names = ["secret_key", "session_cookie", "max_age", "path", "same_site", "https_only", "domain", "partitioned"]
    given = len(call.args) - 1 + len(call.keywords)
    if len(call.args) - 1 > len(names) or any(k.arg not in names for k in call.keywords) or not given:
        raise TranspileError("SessionMiddleware(secret_key, session_cookie=, max_age=, path=, same_site=, https_only=, "
                             "domain=, partitioned=): unknown or missing option", call, p.src(module))


def list_gzip(fe: Frontend, gzip):
    """`FastAPI(middleware=[..., Middleware(GZipMiddleware, minimum_size=..., compresslevel=...)])`: its options, as
    `app.add_middleware(GZipMiddleware, ...)` gives them (the binary's GZip is the outermost layer: a documented
    difference, docs/tutorial/middleware.md). Refused next to another GZipMiddleware."""
    for m in fe.index.package_modules():
        if m.name not in fe.app_vars:
            continue  # another application's module (--app)
        for n in ast.walk(m.tree):
            if not isinstance(n, ast.Call):
                continue
            try:
                entries = mw_entries(fe, m.name, n)
            except TranspileError:
                continue
            for e in entries or ():
                if not (isinstance(e, ast.Call) and e.args and mw_class_kind(fe, m.name, e.args[0]) == "gzip"):
                    continue
                as_add = ast.copy_location(ast.Call(func=ast.Name("add_middleware", ast.Load()),
                                                    args=[ast.Name("GZipMiddleware", ast.Load())], keywords=e.keywords), e)
                try:
                    if gzip is not None:
                        raise TranspileError("GZipMiddleware registered twice is not supported", e, str(m.path))
                    gzip = fe._middleware(as_add, str(m.path))
                except TranspileError as err:
                    fe.global_errors.append(err)
                    if not fe.collect:
                        raise
    return gzip


def _single_list_binding(fe: Frontend, module: str, call: ast.Call, name: ast.Name) -> ast.List | ast.Tuple | None:
    """`middleware = [Middleware(...), ...]` then `FastAPI(middleware=middleware)` in the same scope: the list literal,
    when it is the name's only binding, made before the call, and the name is read nowhere else (no append, no
    reassignment, no other use that could change the list)."""
    parents = fe._parents(module)
    scope = frontend_enclosing_fn(call, parents)
    tree = scope if scope is not None else fe.index.module(module).tree
    stores, loads = [], []
    for n in ast.walk(tree):
        if isinstance(n, ast.Name) and n.id == name.id:
            (stores if isinstance(n.ctx, ast.Store) else loads).append(n)
        elif isinstance(n, (ast.Global, ast.Nonlocal)) and name.id in n.names:
            return None
    if len(stores) != 1 or loads != [name]:
        return None
    st = parents.get(stores[0])
    if not (isinstance(st, ast.Assign) and len(st.targets) == 1 and st.targets[0] is stores[0]
            and isinstance(st.value, (ast.List, ast.Tuple)) and st.lineno < call.lineno
            and frontend_enclosing_fn(st, parents) is scope):
        return None
    return st.value


def stack_deco(fe: Frontend, module: str, d: ast.AST) -> str | None:
    """`@app.exception_handler(...)` / `@app.middleware("http")` on a function."""
    if (isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and fe._is_app(module, d.func.value)
            and d.func.attr in {"exception_handler", "middleware"}):
        return d.func.attr
    return None


def is_add_route(fe: Frontend, module: str, node: ast.AST) -> bool:
    """`app.add_route(...)` / `app.router.add_route(...)`."""
    if not (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == "add_route"):
        return False
    owner = node.func.value
    return fe._is_app(module, owner) or (isinstance(owner, ast.Attribute) and owner.attr == "router"
                                         and fe._is_app(module, owner.value))


def stack_node(fe: Frontend, module: str, node: ast.AST) -> bool:
    """A registration the middleware stack compiles (so not a global error)."""
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
        if node.func.attr == "add_middleware" and fe._is_app(module, node.func.value):
            return mw_kind(fe, module, node) in {"cors", "base", "user", "asgi", "context", "https", "trusted", "session",
                                                  "sentry"}
        if node.func.attr == "add_exception_handler" and fe._is_app(module, node.func.value):
            return True
        if is_add_route(fe, module, node):
            return True  # run while the stack is built, like Starlette registering the route
        return stack_deco(fe, module, node) is not None
    return False


def configure_call(fe: Frontend, module: str, call: ast.Call) -> set[str]:
    """`configure(app)`: a call of a project function given the application (as argument or keyword): the
    names of the application in it, else an empty set."""
    names = {a.id for a in [*call.args, *(k.value for k in call.keywords)] if isinstance(a, ast.Name) and fe._is_app(module, a)}
    if not names:
        return set()
    t = fe.index.resolve_expr(module, call.func)
    if isinstance(t, Sym) and isinstance(fe.index.definition(t), (ast.FunctionDef, ast.AsyncFunctionDef)):
        return names
    return set()


# registrations a function given the app cannot make in the binary (routes and handlers are compiled)
CONFIGURE_REFUSED = {"include_router", "mount", "add_api_route", "add_websocket_route",
                     "add_event_handler", "on_event", "websocket", "api_route",
                     "get", "post", "put", "patch", "delete", "head", "options"}


def check_configure(p: "Project", fe: Frontend, module: str, call: ast.Call) -> None:
    """`configure(app)`: refuse the registrations its body makes on the app parameter."""
    t = fe.index.resolve_expr(module, call.func)
    fn = fe.index.definition(t)
    params = [a.arg for a in [*fn.args.posonlyargs, *fn.args.args]]
    names = configure_call(fe, module, call)
    bound = {params[i] for i, a in enumerate(call.args) if i < len(params) and isinstance(a, ast.Name) and a.id in names}
    bound |= {k.arg for k in call.keywords if isinstance(k.value, ast.Name) and k.value.id in names}
    for n in ast.walk(fn):
        if id(n) in fe.__dict__.get("configure_includes", ()):
            continue  # an include_router the frontend registered where the function is called (Frontend._configure_site)
        if (isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute) and n.func.attr in CONFIGURE_REFUSED
                and isinstance(n.func.value, ast.Name) and n.func.value.id in bound):
            raise TranspileError(f"{fn.name}(): `{n.func.value.id}.{n.func.attr}(...)` on the application passed to a "
                                 "function is not supported (register routes and handlers in the factory or at module level)",
                                 n, p.src(t.module))


def lib_router_include(fe: Frontend, module: str, node: ast.AST) -> bool:
    """`app.include_router(<library router>)` (left to Python, see lib_router_side)."""
    return (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == "include_router"
            and bool(node.args) and fe._is_app(module, node.func.value)
            and fe._library_router(module, node.args[0], None) is not None)


def _stack_call(fe: Frontend, module: str, n: ast.AST) -> bool:
    """A call the middleware stack runs: a registration, `configure(app)`, a library router's inclusion,
    `FastAPI(middleware=[...])`."""
    return isinstance(n, ast.Call) and bool(
        stack_node(fe, module, n) or configure_call(fe, module, n) or lib_router_include(fe, module, n)
        or instr_call(fe, module, n, ("instrument",))
        or (isinstance(n.func, (ast.Name, ast.Attribute)) and dotted(n.func) in {"FastAPI", "fastapi.FastAPI"}
            and any(k.arg == "middleware" for k in n.keywords)))


def instr_call(fe: Frontend, module: str, n: ast.AST, names: tuple) -> bool:
    """`<instrumentator>.instrument(app, ...)` / `.expose(app, ...)` of prometheus_fastapi_instrumentator: the
    receiver an `Instrumentator(...)` call (or such a chain), or a name bound to one."""
    if not (isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute) and n.func.attr in names and n.args
            and fe._is_app(module, n.args[0])):
        return False
    recv = n.func.value
    for _ in range(4):
        if isinstance(recv, ast.Call) and isinstance(recv.func, ast.Attribute) and recv.func.attr in ("instrument", "expose"):
            recv = recv.func.value
            continue
        if isinstance(recv, ast.Call):
            t = fe.index.resolve_expr(module, recv.func)
            return isinstance(t, Ext) and t.dotted.startswith("prometheus_fastapi_instrumentator.")
        if isinstance(recv, (ast.Name, ast.Attribute)):
            t = fe.index.resolve_expr(module, recv)
            d = fe.index.definition(t) if isinstance(t, Sym) else None
            if isinstance(d, (ast.Assign, ast.AnnAssign)) and d.value is not None:
                module, recv = t.module, d.value
                continue
        return False
    return False


def _stack_relevant(fe: Frontend, module: str, node: ast.AST) -> bool:
    return any(_stack_call(fe, module, n) for n in ast.walk(node))


def target_python(root) -> tuple[int, int]:
    """The Python the project runs on (versions.target_python)."""
    import os
    return _target_python(str(Path(root).resolve()), os.environ.get("PY2AXUM_TARGET_PYTHON"))


def python_features(root) -> dict[str, bool]:
    """Behaviours of the project's Python probed on the transpiler's (versions.probe_features)."""
    from .versions import probe_features
    return probe_features(Path(root))


def target_python_patch(root) -> str:
    """The patch release of the project's Python, as a Rust u32 literal (`u32::MAX`: unknown)."""
    from .versions import target_python_patch as _tpp
    p = _tpp(Path(root))
    return "u32::MAX" if p is None else str(p)


@functools.lru_cache(maxsize=None)
def _target_python(root: str, _forced) -> tuple[int, int]:
    from .versions import target_python as _tp
    return _tp(Path(root))


def fastapi_at_least(root, version: tuple[int, int]) -> bool:
    v = locked_version(root, "fastapi") or "0.142"
    return tuple(int(x) for x in re.findall(r"\d+", v)[:2]) >= version


def fastapi_strict_content_type(root) -> bool:
    """FastAPI 0.132+ does not decode a body without Content-Type as JSON (`strict_content_type`, True by default)."""
    return fastapi_at_least(root, (0, 132))


def _installed_stripe() -> str | None:
    """the stripe-python installed next to py2axum, when the supported range has it"""
    from . import versions
    v = _installed("stripe")
    return v if v and versions.in_range("stripe", v) else None


def _installed(name: str) -> str | None:
    """the version of a distribution installed next to py2axum (the reference's, when the project locks none)"""
    from importlib import metadata
    try:
        return metadata.version(name)
    except metadata.PackageNotFoundError:
        return None


def qrcode_without_pillow(root) -> bool:
    """The project's uv.lock locks qrcode but not Pillow: qrcode then falls back to PyPNGImage (pure Python)."""
    d = Path(root).resolve()
    for cand in (d, *d.parents):
        lock = cand / "uv.lock"
        if lock.is_file():
            text = lock.read_text()
            return '\nname = "qrcode"\n' in text and '\nname = "pillow"\n' not in text
        if (cand / ".git").exists():
            break
    return False


def locked_version(root, name: str) -> str | None:
    """The version of a library the project locks: the `uv.lock` of --root or of a parent directory (up to
    the repository's root); for a supported library (versions.py), else its `==` pin in requirements*.txt /
    pyproject.toml; else the one installed next to the transpiler (or, when the project's specifier excludes
    it, the highest version of the tested ones the specifier allows); else None."""
    from . import versions
    d = Path(root).resolve()
    for cand in (d, *d.parents):
        lock = cand / "uv.lock"
        if lock.is_file():
            m = re.search(rf'name = "{re.escape(name)}"\nversion = "([^"]+)"', lock.read_text())
            if m:
                return m.group(1)
            break
        if (cand / ".git").exists():
            break  # a lock further up belongs to another project
    found = [f for f in versions.project_constraints(root)[0] if f.name == name] if name in versions.SUPPORTED else []
    if exact := next((f.constraint for f in found if f.exact), None):
        return exact
    from importlib.metadata import PackageNotFoundError, version
    try:
        installed = version(name)
    except PackageNotFoundError:
        installed = None
    ivs = [iv for f in found if (iv := versions.interval(f.constraint)) is not None]
    if ivs and (installed is None or not all(lo <= versions.parse(installed) < hi for lo, hi in ivs)):
        lo_t, hi_t, _ = versions.SUPPORTED[name]
        tested = [lo_t, hi_t, *(m[name] for m in versions.MIDDLES.values() if name in m)]
        allowed = [v for v in tested if all(lo <= versions.parse(v) < hi for lo, hi in ivs)]
        if allowed:
            return max(allowed, key=versions.parse)
    return installed


def project_distribution(root) -> tuple[tuple[str, str], list[tuple[str, str]]] | None:
    """(the project's (name, version), the uv.lock packages) of the pyproject.toml found at --root or above
    (up to the repository's root), when a uv.lock sits next to it."""
    import tomllib
    d = Path(root).resolve()
    for cand in (d, *d.parents):
        pp, lock = cand / "pyproject.toml", cand / "uv.lock"
        if pp.is_file() and lock.is_file():
            proj = tomllib.loads(pp.read_text()).get("project", {})
            if "name" not in proj or "version" not in proj:
                return None
            pkgs = [(p["name"], p["version"]) for p in tomllib.loads(lock.read_text()).get("package", [])
                    if "version" in p and p["name"] != proj["name"]]
            return (proj["name"], proj["version"]), pkgs
        if (cand / ".git").exists():
            return None
    return None


def app_imports(fe: Frontend) -> set[str]:
    """The modules importing the application runs (its module, their imports, transitively)."""
    seen: set[str] = set()

    def visit(name: str) -> None:
        m = fe.index.module(name)
        if m is None or name in seen:
            return
        seen.add(name)
        parts = name.split(".")
        for i in range(1, len(parts)):
            visit(".".join(parts[:i]))
        for spec in m.imports.values():
            for t in ([spec[1]] if spec[0] == "module" else [spec[1], f"{spec[1]}.{spec[2]}"]):
                visit(t)

    for m in fe.index.package_modules():
        if m.name in fe.app_vars:
            visit(m.name)
    return seen


def import_order(fe: Frontend) -> list[str]:
    """Modules in the order importing the application runs them: a module's imports (and parent packages)
    before it, depth first from the application's module."""
    order, seen = [], set()

    def visit(name: str) -> None:
        m = fe.index.module(name)
        if m is None or name in seen:
            return
        seen.add(name)
        parts = name.split(".")
        for i in range(1, len(parts)):
            visit(".".join(parts[:i]))
        for spec in m.imports.values():
            targets = [spec[1]] if spec[0] == "module" else [spec[1], f"{spec[1]}.{spec[2]}"]
            for t in targets:
                visit(t)
        order.append(name)

    for m in fe.index.package_modules():
        if m.name in fe.app_vars:
            visit(m.name)
    for m in fe.index.package_modules():
        visit(m.name)
    return order


DDL_ONLY_CALLS = {f"sqlalchemy.{n}" for n in ("Index", "UniqueConstraint", "CheckConstraint", "ForeignKeyConstraint",
                                                "PrimaryKeyConstraint")}
FRAMEWORK_CALLS = {"include_router", "add_middleware", "mount", "add_api_route", "add_route", "add_exception_handler",
                   "add_websocket_route", "add_event_handler"}


MCP_SERVER = "mcp.server.mcpserver.MCPServer"
MCP_TOOL_KW = {"name", "title", "description"}
# BaseModel attributes FastMCP renames a parameter after (alias + `field_` prefix): refused
BASEMODEL_CALLABLES = {"copy", "dict", "json", "schema", "schema_json", "validate", "construct", "parse_obj", "parse_raw",
                       "parse_file", "from_orm", "update_forward_refs"}


def mcp_servers(p: "Project") -> dict:
    """Module globals `X = MCPServer(...)`: {Sym: call}."""
    found = p.__dict__.get("_mcp_servers")
    if found is None:
        found = {}
        for m in p.fe.index.package_modules():
            for st in m.tree.body:
                target = st.targets[0] if isinstance(st, ast.Assign) and len(st.targets) == 1 else (
                    st.target if isinstance(st, ast.AnnAssign) else None)
                value = getattr(st, "value", None)
                if isinstance(target, ast.Name) and isinstance(value, ast.Call):
                    t = p.resolve(m.name, value.func)
                    if isinstance(t, Ext) and t.dotted == MCP_SERVER:
                        found[Sym(m.name, target.id)] = value
        p.__dict__["_mcp_servers"] = found
    return found


def mcp_deco_server(p: "Project", module: str, d: ast.AST):
    """The server of a `@server.tool(...)` decorator, else None."""
    f = d.func if isinstance(d, ast.Call) else d
    if isinstance(f, ast.Attribute) and f.attr == "tool" and isinstance(f.value, (ast.Name, ast.Attribute)):
        t = p.resolve(module, f.value)
        if isinstance(t, Sym) and t in mcp_servers(p):
            return t
    return None


def mcp_tool_decorator(p: "Project", fc: "FnCompiler", sym: Sym, node, d) -> str | None:
    """`@server.tool(name=, title=, description=)` on a project function: the runtime decorator with the
    tool's spec computed here (FastMCP's argument model, its JSON schema), else None."""
    if mcp_deco_server(p, sym.module, d) is None:
        return None
    spec = mcp_tool_spec(p, sym, node, d)
    return fc.q(f"{RT}::mcp::tool_decorator(&{fc.expr(d.func.value)}, &{spec})")


def mcp_tool_spec(p: "Project", sym: Sym, node, d) -> str:
    from .jsonschema import SchemaGen

    module = sym.module
    err = lambda msg, n: TranspileError(msg, n, p.src(module))  # noqa: E731
    if not isinstance(d, ast.Call):
        raise err("@server.tool without parentheses is not supported (FastMCP requires @server.tool())", d)
    if not isinstance(node, ast.AsyncFunctionDef) or has_yield(node):
        raise err(f"MCP tool {node.name}: only `async def` functions are supported (a sync tool runs in a thread)", node)
    opts = {}
    if d.args:
        if len(d.args) > 1:
            raise err("@server.tool(): only the name may be positional", d)
        opts["name"] = d.args[0]
    for k in d.keywords:
        if k.arg not in MCP_TOOL_KW:
            raise err(f"@server.tool({k.arg}=...) is not supported (name, title, description)", k)
        opts[k.arg] = k.value
    vals = {}
    for k, v in opts.items():
        if not (isinstance(v, ast.Constant) and isinstance(v.value, str)):
            try:
                vals[k] = p.const(v, module)
            except TranspileError:
                raise err(f"@server.tool({k}=...) must be a constant string", v) from None
            if not isinstance(vals[k], str):
                raise err(f"@server.tool({k}=...) must be a constant string", v)
        else:
            vals[k] = v.value
    ret = node.returns
    ok_ret = (isinstance(ret, ast.Subscript) and p.resolve(module, ret.value) in (None,) and isinstance(ret.value, ast.Name)
              and ret.value.id == "dict" and isinstance(ret.slice, ast.Tuple) and len(ret.slice.elts) == 2
              and isinstance(ret.slice.elts[0], ast.Name) and ret.slice.elts[0].id == "str"
              and p.resolve(module, ret.slice.elts[1]) == Ext("typing.Any"))
    if not ok_ret:
        raise err(f"MCP tool {node.name}: only the return annotation dict[str, Any] is supported (structured output)",
                  ret or node)
    a = node.args
    for q in [*a.posonlyargs, *a.args, *a.kwonlyargs]:
        if q.arg.startswith("_"):
            raise err(f"MCP tool {node.name}: parameter {q.arg} cannot start with '_' (FastMCP refuses it)", q)
        if q.arg in BASEMODEL_CALLABLES or q.arg.startswith("model_"):
            raise err(f"MCP tool {node.name}: parameter {q.arg} shadows a BaseModel attribute (FastMCP aliases it)", q)
    schema = SchemaGen(p).arguments(node, module)
    # FastMCP's `<function>Arguments` model, as a project schema compiled like any other
    asym = Sym(module, f"{node.name}Arguments")
    if asym not in p.fe.schema_syms:
        body = []
        defaults = [None] * (len(a.args) - len(a.defaults)) + list(a.defaults) + list(a.kw_defaults)
        for q, dflt in zip([*a.args, *a.kwonlyargs], defaults):
            st = ast.AnnAssign(target=ast.Name(q.arg, ast.Store()), annotation=q.annotation, value=dflt, simple=1)
            body.append(ast.copy_location(st, q))
        cls = ast.copy_location(ast.ClassDef(name=asym.name, bases=[], keywords=[], body=body, decorator_list=[],
                                             type_params=[]), node)
        p.fe.schema_syms[asym] = cls
    info = p.schema(asym)
    params = []
    for q in [*a.args, *a.kwonlyargs]:
        inner = q.annotation
        if isinstance(inner, ast.Subscript) and (dotted(inner.value) or "").split(".")[-1] == "Annotated":
            inner = inner.slice.elts[0] if isinstance(inner.slice, ast.Tuple) else inner.slice
        params.append((q.arg, isinstance(inner, ast.Name) and inner.id == "str"))
    name = vals.get("name", node.name)
    desc = vals.get("description", fn_doc(p, node) or "")
    out = json.dumps({"type": "object", "additionalProperties": True, "title": f"{node.name}DictOutput"},
                     separators=(",", ":"))
    static = f"MCP_TOOL_{mod_ident(module)}__{ident(node.name)}"
    if static not in p.__dict__.setdefault("_mcp_specs", set()):
        p._mcp_specs.add(static)
        title = f"Some({rs(vals['title'])})" if "title" in vals else "None"
        plist = ", ".join(f"({rs(n)}, {str(b).lower()})" for n, b in params)
        p.items.append(
            f"/// @tool {name} ({p.rel_file(module)}:{node.lineno})\n"
            f"pub static {static}: {RT}::mcp::ToolSpec = {RT}::mcp::ToolSpec {{ name: {rs(name)}, title: {title}, "
            f"description: {rs(desc)}, input_schema: {rs(json.dumps(schema, separators=(',', ':'), ensure_ascii=False))}, "
            f"output_schema: {rs(out)}, args: &{info.rust}, params: &[{plist}] }};")
    return static


MCP_SERVER_ATTRS = {"tool", "streamable_http_app", "session_manager"}


def mcp_check(p: "Project", fe: Frontend) -> None:
    """The MCP servers' use in the project stays within what dynrt/mcp.rs implements (refused otherwise)."""
    servers = mcp_servers(p)
    if not servers:
        return
    for m in fe.index.package_modules():
        file = str(m.path)
        parents = {}
        for n in ast.walk(m.tree):
            for c in ast.iter_child_nodes(n):
                parents[c] = n
        for n in ast.walk(m.tree):
            if not (isinstance(n, ast.Attribute) and isinstance(n.value, (ast.Name, ast.Attribute))):
                continue
            t = p.resolve(m.name, n.value)
            if not (isinstance(t, Sym) and t in servers):
                continue
            if n.attr not in MCP_SERVER_ATTRS:
                raise TranspileError(f"MCPServer.{n.attr} is not supported (tools only: @server.tool, "
                                     "streamable_http_app, session_manager)", n, file)
            up = parents.get(n)
            if n.attr == "tool":
                deco = up if isinstance(up, ast.Call) and up.func is n else n
                owner = parents.get(deco)
                if not (isinstance(owner, (ast.FunctionDef, ast.AsyncFunctionDef)) and deco in owner.decorator_list):
                    raise TranspileError("server.tool(...) is only supported as a decorator", n, file)
            elif n.attr == "session_manager":
                if not (isinstance(up, ast.Attribute) and up.attr in {"run", "handle_request"}):
                    raise TranspileError("server.session_manager: only .run() and .handle_request(scope, receive, send) "
                                         "are supported", n, file)
            elif n.attr == "streamable_http_app":
                call = up if isinstance(up, ast.Call) and up.func is n else None
                kw = {k.arg: k.value for k in call.keywords} if call else None
                if call is None or call.args or kw is None or set(kw) - {"stateless_http", "json_response", "transport_security"}:
                    raise TranspileError("streamable_http_app(): only stateless_http=True, json_response=True, "
                                         "transport_security=TransportSecuritySettings(enable_dns_rebinding_protection=False)",
                                         n, file)
                for k in ("stateless_http", "json_response"):
                    if not (isinstance(kw.get(k), ast.Constant) and kw[k].value is True):
                        raise TranspileError(f"streamable_http_app({k}=True) is required: sessions and SSE responses "
                                             "are not supported", call, file)
                ts = kw.get("transport_security")
                if not (isinstance(ts, ast.Call)
                        and p.resolve(m.name, ts.func) == Ext("mcp.server.transport_security.TransportSecuritySettings")):
                    raise TranspileError("streamable_http_app(): transport_security=TransportSecuritySettings("
                                         "enable_dns_rebinding_protection=False) is required (Host/Origin checks are "
                                         "not supported)", call, file)


def mcp_register(p: "Project", fe: Frontend) -> None:
    """The `@server.tool(...)` functions are registered at import in Python: built at startup here too."""
    servers = mcp_servers(p)
    if not servers:
        return
    p.cur = ("mcp", 0)
    try:
        mcp_check(p, fe)
        for m in fe.index.package_modules():
            for st in m.tree.body:
                if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)) and any(
                        mcp_deco_server(p, m.name, d) for d in st.decorator_list):
                    p.decorated_value(Sym(m.name, st.name))
        p.drain()
    except TranspileError as e:
        if not p.collect:
            raise
        fe.global_errors.append(e)
    finally:
        p.cur = None


def factory_lifespan(p: "Project", module: str, value: ast.Name):
    """`FastAPI(lifespan=lifespan)` in the app factory, `lifespan` a function the factory defines (under an `if`
    even): (its definition, the factory, the `if`s around the call as (test, branch taken)), else None."""
    from .fallback import _local_def

    tree = p.ix.module(module).tree
    call = next((n for n in ast.walk(tree) if isinstance(n, ast.Call)
                 and any(k.arg == "lifespan" and k.value is value for k in n.keywords)), None)
    factory = next((f for f in tree.body if isinstance(f, (ast.FunctionDef, ast.AsyncFunctionDef))
                    and call is not None and any(n is call for n in ast.walk(f))), None)
    if factory is None:
        return None
    d = _local_def(tree, call, value.id)
    if d is None:
        return None
    guards = []

    def path(stmts) -> bool:
        for st in stmts:
            if any(n is call for n in ast.walk(st)):
                if isinstance(st, ast.If):
                    inside = any(n is call for b in st.body for n in ast.walk(b))
                    guards.append((st.test, inside))
                    return path(st.body if inside else st.orelse)
                if isinstance(st, (ast.Assign, ast.AnnAssign, ast.Expr, ast.Return)):
                    return True
                return False  # a loop, a `try`, a `with` around the app's creation
        return False

    return (d, factory, guards) if path(factory.body) else None


def lifespan_fn(p: "Project", fe: Frontend) -> str:
    """`pub async fn lifespan`: the value of `FastAPI(lifespan=...)` (an `@asynccontextmanager` function or an
    async generator function, called with the app before the server listens), None without one."""
    spec = fe.__dict__.get("lifespan")
    if spec is None:
        return "pub async fn lifespan(_cx: &Cx) -> R<Option<V>> {\n    Ok(None)\n}"
    module, value = spec
    t = p.resolve(module, value) if isinstance(value, (ast.Name, ast.Attribute)) else None
    d = p.ix.definition(t) if isinstance(t, Sym) else None
    local = None
    if d is None and isinstance(value, ast.Name):
        local = factory_lifespan(p, module, value)
        if local is not None:
            d, factory, guards = local
            t = Sym(module, f"{factory.name}.<locals>.{d.name}")
    if not isinstance(d, ast.AsyncFunctionDef) or not has_yield(d):
        e = TranspileError("FastAPI(lifespan=...): only an `async def` generator of the project (decorated with "
                           "@asynccontextmanager or not) is supported", value, p.src(module))
        if not p.collect:
            raise e
        fe.global_errors.append(scoped(e, "lifespan"))
        return "pub async fn lifespan(_cx: &Cx) -> R<Option<V>> {\n    Ok(None)\n}"
    for n in walk_scope(d):
        if isinstance(n, ast.Yield) and n.value is not None and not (isinstance(n.value, ast.Constant) and n.value.value is None):
            e = TranspileError("lifespan state (`yield <value>` in the lifespan) is not supported: use module globals",
                               n, p.src(t.module))
            if not p.collect:
                raise e
            fe.global_errors.append(scoped(e, "lifespan"))
            return "pub async fn lifespan(_cx: &Cx) -> R<Option<V>> {\n    Ok(None)\n}"
    p.cur = ("lifespan", 0)
    fc = FnCompiler(p, module, None, "lifespan")
    try:
        if local is None:
            code = fc.expr(value)
        else:
            # defined in the app factory: compiled with the factory's locals (read once, as the factory runs once),
            # used only when the `if`s around `FastAPI(lifespan=...)` hold at startup
            fc.factory = factory
            fc.__dict__["app_alias"] = set(fe.app_vars.get(module, ()))
            p.__dict__.setdefault("nested", {})[t] = (d, factory)
            for test, taken in guards:
                cond = fc.truthy(test)
                fc.emit(f"if {'!' if taken else ''}({cond}) {{ return Ok(None); }}")
            code = f"V::native({RT}::Native::Bound({function_wrapper(p, t)}, V::None))"
        p.drain()
    except TranspileError as e:
        if not p.collect:
            raise
        fe.global_errors.append(scoped(e, "lifespan"))
        return "pub async fn lifespan(_cx: &Cx) -> R<Option<V>> {\n    Ok(None)\n}"
    finally:
        p.cur = None
    body = "\n".join("    " + l for l in fc.lines)
    return (f"/// FastAPI(lifespan={ast.unparse(value)}) ({p.rel_file(module)}:{value.lineno})\n"
            f"pub async fn lifespan(cx: &Cx) -> R<Option<V>> {{\n{body}\n    Ok(Some({code}))\n}}")


def _configures_output(t) -> bool:
    """`sentry_sdk.init`, or the `logging` module and the libraries configuring it (python-json-logger,
    uvicorn's formatters), or loguru's logger: what an import-time or factory statement must not skip, since it
    decides what the application prints and reports."""
    return is_ext(t, "sentry_sdk.init") or (isinstance(t, Ext) and t.dotted.startswith(("logging.", "pythonjsonlogger.", "uvicorn.logging.",
                                                                                          "loguru.logger.")))


def _reaches_sentry_init(p: "Project", module: str, call: ast.Call, depth: int = 4, seen=None) -> bool:
    """Does this call run `sentry_sdk.init(...)` or configure logging, directly or through project functions?"""
    t = p.ix.resolve_expr(module, call.func)
    if _configures_output(t):
        return True
    if depth == 0 or not isinstance(t, Sym):
        return False
    fn = p.ix.definition(t)
    seen = seen if seen is not None else set()
    if not isinstance(fn, (ast.FunctionDef, ast.AsyncFunctionDef)) or t in seen:
        return False
    seen.add(t)
    return any(isinstance(n, ast.Call) and _reaches_sentry_init(p, t.module, n, depth - 1, seen) for n in walk_scope(fn))


def factory_sentry_init(p: "Project", fe: Frontend) -> None:
    """The statements of an app factory (`def create_app(): ... app = FastAPI()`) that initialise Sentry
    (`init_sentry()`) or configure logging (`setup_logging()`): run at startup with the module globals, as the
    factory runs them at import. The factory's other plain statements are not translated (its middlewares and
    handlers are, by the stack)."""
    for m in fe.index.package_modules():
        for fn in m.tree.body:
            if not isinstance(fn, ast.FunctionDef) or not any(
                    isinstance(n, (ast.Assign, ast.AnnAssign)) and isinstance(n.value, ast.Call)
                    and is_ext(p.ix.resolve_expr(m.name, n.value.func), "fastapi.FastAPI") for n in walk_scope(fn)):
                continue
            for st in fn.body:
                if not (isinstance(st, ast.Expr) and isinstance(st.value, ast.Call) and _reaches_sentry_init(p, m.name, st.value)):
                    continue
                name = f"factst_{mod_ident(m.name)}__{st.lineno}"
                fc = FnCompiler(p, m.name, None, name)
                fc.factory = fn
                p.drain()  # functions queued earlier (module globals, routes): their errors are not this statement's
                p.cur = ("modst", name)
                before = len(p.errors)
                try:
                    fc.block([st])
                    p.drain()
                    # a function it calls that does not translate would only fail at startup: refused here
                    if len(p.errors) > before:
                        raise p.errors[before]
                except TranspileError as e:
                    if not p.collect:
                        raise
                    fe.global_errors.append(TranspileError(f"app factory statement not translated: {e.msg}", e.node, e.file))
                    continue
                finally:
                    p.cur = None
                body = "\n".join("    " + l for l in fc.lines)
                p.items.append(f"/// {m.name}:{st.lineno} (in the app factory {fn.name}(), run at startup)\n"
                               f"pub async fn {name}(cx: &Cx) -> R {{\n{body}\n    Ok(V::None)\n}}")
                p.__dict__.setdefault("eager", []).append((m.name, st.lineno, name))
                p.__dict__.setdefault("modst", set()).add(name)


@functools.lru_cache(maxsize=None)
def _mapped_roots() -> frozenset[str]:
    from . import libmap
    tables = (libmap.VALUES, libmap.EXCEPTIONS, libmap.CALLS, libmap.NAMESPACE_CALLS)
    return frozenset({k.split(".")[0] for t in tables for k in t} | {"sentry_sdk", "loguru"})


def unmapped_library(t) -> bool:
    """A third-party library the runtime maps nothing of (not the standard library)."""
    import sys
    if not isinstance(t, Ext):
        return False
    root = t.dotted.split(".")[0]
    return root not in _mapped_roots() and root not in sys.stdlib_module_names


def _app_state_target(fe: Frontend, module: str, t: ast.expr) -> bool:
    """`app.state.x` / `app.state.x[k]` / `app.state` itself: the application's state object."""
    chain = []
    while isinstance(t, (ast.Subscript, ast.Attribute)):
        chain.append(t)
        t = t.value
    return bool(chain) and fe._is_app(module, t) and isinstance(chain[-1], ast.Attribute) and chain[-1].attr == "state"


def _store_bases(st: ast.stmt) -> list[ast.expr]:
    """The objects an assignment stores into (`a.b["k"] = v` -> `a`)."""
    out = []
    for t in (st.targets if isinstance(st, ast.Assign) else [st.target]):
        while isinstance(t, (ast.Subscript, ast.Attribute)):
            t = t.value
        out.append(t)
    return out


def link_mutated_globals(p: "Project", module: str, st: ast.stmt, node) -> None:
    """A module-level statement that fills a global in place (`REG[k] = lambda: f()`, `REG.update(...)`):
    the routes reading that global reach what the statement queued (a function that does not translate
    then blocks them, like one the global's own value calls)."""
    names = set()
    for n in walk_scope(st):
        base = None
        if isinstance(n, (ast.Subscript, ast.Attribute)) and isinstance(n.ctx, (ast.Store, ast.Del)):
            base = n.value
        elif isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute):
            base = n.func.value  # a method call may mutate its receiver
        while isinstance(base, (ast.Subscript, ast.Attribute)):
            base = base.value
        if isinstance(base, ast.Name):
            names.add(base.id)
    for name in sorted(names - set(stmt_bindings(st, handlers=True))):
        sym = p.ix.resolve(module, name)
        if isinstance(sym, Sym):
            p.edges.setdefault(("global", sym), set()).add(node)


ENGINE_CALLS = ("sqlalchemy.create_engine", "sqlalchemy.ext.asyncio.create_async_engine")


def engine_globals(p: "Project", fe: Frontend) -> None:
    """`engine = create_[async_]engine(..., connect_args=...)` in a module the application imports: evaluated
    at startup even when nothing reads it (the session dependency is native), before the pool's first
    connection, so its session parameters (`orm::engine_connect_args`) apply to every connection."""
    for name in import_order(fe):
        m = fe.index.module(name)
        if m is None:
            continue
        for st in m.tree.body:
            if not (isinstance(st, (ast.Assign, ast.AnnAssign)) and isinstance(st.value, ast.Call)
                    and any(k.arg == "connect_args" for k in st.value.keywords)):
                continue
            targets = st.targets if isinstance(st, ast.Assign) else [st.target]
            t = p.resolve(m.name, st.value.func)
            if not (isinstance(t, Ext) and t.dotted in ENGINE_CALLS) or len(targets) != 1 or not isinstance(targets[0], ast.Name):
                continue
            try:
                check_libpq_options(p, m.name, st.value)
                p.global_value(Sym(m.name, targets[0].id))
            except TranspileError as e:
                if not p.collect:
                    raise
                fe.global_errors.append(e)


def libpq_switches(opts: str) -> list[str]:
    """libpq `options` split as the server does (`pg_split_opts`); the switches other than `-c name=value`
    and `--name=value` (what `orm::engine_connect_args` applies)."""
    words, cur, started, i = [], "", False, 0
    while i < len(opts):
        c = opts[i]
        if c in " \t\n\r\f\v":
            if started:
                words.append(cur)
                cur, started = "", False
        elif c == "\\":
            started = True
            i += 1
            cur += opts[i] if i < len(opts) else ""
        else:
            started = True
            cur += c
        i += 1
    if started:
        words.append(cur)
    bad, it = [], iter(words)
    for w in it:
        opt = next(it, None) if w == "-c" else w[2:] if w.startswith(("-c", "--")) else None
        if opt is None or "=" not in opt or opt.startswith("="):
            bad.append(w if opt is None or w != "-c" else f"-c {opt}")
    return bad


def check_libpq_options(p: "Project", module: str, call: ast.Call) -> None:
    """A literal `connect_args={"options": "..."}`: a switch the runtime would not apply is refused here."""
    ca = next(k.value for k in call.keywords if k.arg == "connect_args")
    if not isinstance(ca, ast.Dict):
        return
    for k, v in zip(ca.keys, ca.values):
        if isinstance(k, ast.Constant) and k.value == "options" and isinstance(v, ast.Constant) and isinstance(v.value, str):
            bad = libpq_switches(v.value)
            if bad:
                raise TranspileError(f"connect_args options: only `-c name=value` and `--name=value` switches are supported "
                                     f"(found {bad[0]!r})", v, p.src(module))


def passes_websockets(cls: ast.AST | None) -> bool:
    """A raw ASGI middleware whose `__call__` starts by handing every non-HTTP scope to the app unchanged:
    `if scope["type"] != "http" [or ...]: return await self.app(scope, receive, send)` (or the call, then
    `return`). Skipping it for a WebSocket connection is then what Starlette does."""
    if not isinstance(cls, ast.ClassDef):
        return False
    call = next((f for f in cls.body if isinstance(f, ast.AsyncFunctionDef) and f.name == "__call__"), None)
    if call is None or len(call.args.args) != 4 or call.args.vararg or call.args.kwarg:
        return False
    sc, rcv, snd = (a.arg for a in call.args.args[1:])
    body = [st for st in call.body if not (isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant))]
    if not body or not isinstance(body[0], ast.If) or body[0].orelse:
        return False
    test = body[0].test
    first = test.values[0] if isinstance(test, ast.BoolOp) and isinstance(test.op, ast.Or) else test
    if not (isinstance(first, ast.Compare) and len(first.ops) == 1 and isinstance(first.ops[0], ast.NotEq)
            and ast.dump(first.left) == ast.dump(ast.Subscript(value=ast.Name(id=sc, ctx=ast.Load()), slice=ast.Constant(value="type"), ctx=ast.Load()))
            and isinstance(first.comparators[0], ast.Constant) and first.comparators[0].value == "http"):
        return False

    def forwards(e):
        return (isinstance(e, ast.Await) and isinstance(e.value, ast.Call) and not e.value.keywords
                and ast.unparse(e.value.func) == f"{call.args.args[0].arg}.app"
                and [ast.unparse(a) for a in e.value.args] == [sc, rcv, snd])
    then = body[0].body
    return ((len(then) == 1 and isinstance(then[0], ast.Return) and forwards(then[0].value))
            or (len(then) == 2 and isinstance(then[0], ast.Expr) and forwards(then[0].value)
                and isinstance(then[1], ast.Return) and then[1].value is None))


def root_path_checked(fe: Frontend, ws_routes: list) -> str:
    """`FastAPI(root_path="/api/v1")`: the scope's root_path, stripped from the path before routing (Starlette's
    get_route_path), part of `base_url`. Refused with mounts, Python-side routes and WebSocket routes (their
    paths would need the same treatment, not done)."""
    if ws_routes and fe.__dict__.get("host_mws"):
        call, src = fe.host_mws[0]
        raise TranspileError("HTTPSRedirectMiddleware/TrustedHostMiddleware with WebSocket routes is not supported (the "
                             "binary checks HTTP requests only)", call, src)
    # a WebSocket connection goes to its route without crossing raw ASGI middlewares: allowed only for those
    # that hand non-HTTP scopes straight on (an auth, IP or rate-limit check on websockets would be skipped)
    for sym, call, src in fe.__dict__.get("raw_mws", []) if ws_routes else ():
        if not passes_websockets(fe.index.definition(sym) if isinstance(sym, Sym) else None):
            raise TranspileError(f"raw ASGI middleware {ast.unparse(call.args[0])} with WebSocket routes: its __call__ must "
                                 "start with `if scope[\"type\"] != \"http\": return await self.app(scope, receive, send)` (the "
                                 "binary does not run raw middlewares for WebSocket connections)", call, src)
    rp = fe.__dict__.get("root_path")
    if rp is None:
        return ""
    value, kw, src = rp
    if ws_routes or fe.__dict__.get("mount_fallback") or {x for x in fe.__dict__.get("python_side", ()) if x.startswith("/")} \
            or fe.__dict__.get("python_side_raw"):
        raise TranspileError("FastAPI(root_path=...) together with WebSocket routes, app.mount() or Python-side routes "
                             "is not supported", kw, src)
    return value


def dep_override_target(fe: Frontend, module: str, st: ast.stmt) -> bool:
    """`app.dependency_overrides[dep] = other` at module level."""
    return (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Subscript)
            and isinstance(st.targets[0].value, ast.Attribute) and st.targets[0].value.attr == "dependency_overrides"
            and fe._is_app(module, st.targets[0].value.value))


def module_statements(p: "Project", fe: Frontend) -> None:
    """Module-level call statements (`INI.set_main_option(...)`, `logger.setLevel(...)`) of the modules the
    translation uses, compiled to run at startup in their place among the module's globals."""
    used = {k[0].module for k in p.fns if isinstance(k[0], Sym)} | {s.module for s in p.globals if isinstance(s, Sym)}
    imported = app_imports(fe)
    # the modules of the libraries translated from source (--translate-libs) the translation uses: their
    # statements run at import, as CPython does (`for k, v in a.items(): b[v] = k` filling a table)
    libs = [x for x in list(fe.index.modules.values()) if x.lib is not None and x.name in used]

    def refused(m, e: TranspileError) -> None:
        """A statement that does not translate. In a library translated from source it blocks only what uses the
        library: an error node (`("libmod", module, line)`) the functions and globals of the library (and of its
        submodules, whose import runs it) reach, so the routes using the library go --python-side and the
        others never run its module code (link_lib_statements). In the project, it blocks the application."""
        if m.lib is not None:
            node = ("libmod", m.name, e.node.lineno if e.node is not None and hasattr(e.node, "lineno") else 0)
            p.fn_errors.setdefault(node, e)
            p.__dict__.setdefault("libmod_nodes", {}).setdefault(m.name, []).append(node)
            return
        if not p.collect:
            raise e
        fe.global_errors.append(e)

    for m in [*fe.index.package_modules(), *libs]:
        # a module the application imports but the translation does not use: only what configures its
        # output runs (`setup_logging()` at import), as importing it does in Python
        output_only = m.name not in used
        if output_only and m.name not in imported:
            continue
        for st in m.tree.body:
            if output_only and not (isinstance(st, ast.Expr) and isinstance(st.value, ast.Call)
                                    and _reaches_sentry_init(p, m.name, st.value)) and not _sets_library_conf(p, m.name, st):
                continue
            if startup_skipped(st):
                continue  # `sys.path += [...]` (imports are resolved statically), `if __name__ == "__main__":`
            if isinstance(st, ast.Expr) and isinstance(st.value, ast.Call):
                f = st.value.func
                if isinstance(f, ast.Attribute) and (f.attr in FRAMEWORK_CALLS or fe._is_app(m.name, f.value)):
                    continue
                if any(instr_call(fe, m.name, n, ("instrument",)) for n in ast.walk(st)):
                    continue  # run where the middleware stack is built
                if configure_call(fe, m.name, st.value):
                    continue  # run by the middleware stack
                t = p.resolve(m.name, f)
                if isinstance(t, Ext) and t.dotted in DDL_ONLY_CALLS:
                    continue  # `Index(Model.col, ...)`: attaches to the table's DDL, compiled with create_all
            elif isinstance(st, (ast.Assign, ast.AugAssign)) and any(
                    isinstance(t, (ast.Subscript, ast.Attribute)) for t in (st.targets if isinstance(st, ast.Assign) else [st.target])):
                # `REG["k"] = ...`, `obj.attr = ...`: fills a global in place (the plain names it binds are globals)
                if dep_override_target(fe, m.name, st):
                    continue  # applied when the dependencies are solved (RouteBuilder.dep_overrides)
                if is_app_route_class(fe, m.name, st):
                    app_route_class(fe)  # set once only
                    continue  # applied to the app's routes declared after it (RouteBuilder.route_class)
                if any(fe._is_app(m.name, b) for b in _store_bases(st)):
                    # `app.state.x = ...`: the frontend's; any other attribute of the application changes how it
                    # serves (`app.router.route_class = GzipRoute`, `app.openapi = ...`): never ignored in silence
                    other = [t for t in (st.targets if isinstance(st, ast.Assign) else [st.target])
                             if not _app_state_target(fe, m.name, t)]
                    if other:
                        e = TranspileError(f"assigning `{ast.unparse(other[0])}` at module level is not supported "
                                           "(only `app.state` attributes)", st, p.src(m.name))
                        if not p.collect:
                            raise e
                        fe.global_errors.append(e)
                    continue
                if all(unmapped_library(p.resolve(m.name, b)) for b in _store_bases(st)):
                    # `stripe.api_key = ...`: configures a library the runtime does not know, so no native code can
                    # read it (any use of the library is refused where it occurs); the routes using it run
                    # --python-side, where the module runs as written
                    continue
                cls = next((t for t in (st.targets if isinstance(st, ast.Assign) else [st.target])
                            if isinstance(t, ast.Attribute) and isinstance(p.resolve(m.name, t.value), Sym)
                            and isinstance(p.ix.definition(p.resolve(m.name, t.value)), ast.ClassDef)), None)
                if cls is not None:
                    refused(m, TranspileError(f"assigning the class attribute `{ast.unparse(cls)}` at module level is not "
                                              "supported (class attributes are read as declared): set it in the class body",
                                              st, p.src(m.name)))
                    continue
            elif not isinstance(st, (ast.For, ast.While, ast.If, ast.With, ast.Try)) or import_guard(st) or startup_skipped(st):
                continue
            elif any(isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute)
                     and (n.func.attr in FRAMEWORK_CALLS or fe._is_app(m.name, n.func.value)) for n in walk_scope(st)):
                continue  # registrations: the frontend's
            else:
                try:
                    link_mutated_globals(p, m.name, st, ("modst", p.stmt_runner(m.name, st)))
                except TranspileError as e:
                    refused(m, TranspileError(f"module-level statement not translated: {e.msg}", e.node, e.file))
                continue
            name = f"modst_{mod_ident(m.name)}__{st.lineno}"
            fc = FnCompiler(p, m.name, None, name)
            p.cur = ("modst", name)
            try:
                fc.block([st])
                link_mutated_globals(p, m.name, st, ("modst", name))
            except TranspileError as e:
                p.cur = None
                refused(m, TranspileError(f"module-level statement not translated: {e.msg}", e.node, e.file))
                continue
            finally:
                p.cur = None
            body = "\n".join("    " + l for l in fc.lines)
            p.items.append(f"/// {m.name}:{st.lineno} (module level, run at startup)\npub async fn {name}(cx: &Cx) -> R {{\n{body}\n    Ok(V::None)\n}}")
            p.__dict__.setdefault("eager", []).append((m.name, st.lineno, name))
            p.__dict__.setdefault("modst", set()).add(name)


def _sets_library_conf(p: "Project", module: str, st: ast.stmt) -> bool:
    """`stripe.api_key = ...` at module level: configures a library for the whole process, so it runs at import
    even in a module the translation does not otherwise use (as importing it does in Python)."""
    from . import libmap
    if not isinstance(st, ast.Assign):
        return False
    return all(isinstance(t, ast.Attribute) and isinstance(r := p.resolve(module, t), Ext) and r.dotted in libmap.HOOKS
               and r.dotted.startswith("stripe.") for t in st.targets)


def node_sym(n) -> Sym | None:
    """The definition a node of the call graph stands for (`(sym, kind)` functions, `("global", sym)`...)."""
    if isinstance(n, tuple) and n:
        if isinstance(n[0], Sym):
            return n[0]
        if len(n) > 1 and isinstance(n[1], Sym):
            return n[1]
    return None


def link_partial_mix(p: "Project") -> None:
    """A library the library map covers in part (`jinja2`: Environment...) whose other names were read from its
    source (`jinja2.Template`): one binary cannot hold both, the map's objects are not the source's (a map's
    Environment is no instance of the source's class, and the source's methods do not take it). When a route that
    translates uses the map's names, the source's nodes get an error: the routes reaching the source are refused
    (--python-side auto leaves them to Python), those using the map only stay native as before."""
    native = p.__dict__.get("partial_native", {})
    if not native or p.ix.libs is None:
        return
    nodes = set(p.fns) | set(p.fn_errors) | set(p.edges) | {m for v in list(p.edges.values()) for m in v}
    sources: dict[str, list] = {}
    for n in nodes:
        sym = node_sym(n)
        top = p.ix.partial_lib(sym.module) if sym is not None else None
        if top in native:
            sources.setdefault(top, []).append(n)
    roots = [info["id"] for info, err in p.__dict__.get("route_infos", []) if err is None]
    roots += [("stack", 0), ("lifespan", 0), ("mcp", 0)]
    for top, srcs in sorted(sources.items()):
        uses = native[top]
        live = uses.get(None)
        for rid in roots:
            if live is not None:
                break
            if closure_errors(p, rid):
                continue
            seen, todo = {rid}, [rid]
            while todo and live is None:
                n = todo.pop()
                live = uses.get(n)
                for m in p.edges.get(n, ()):
                    if m not in seen:
                        seen.add(m)
                        todo.append(m)
        if live is None:
            continue
        dotted, file, line = live
        dist = p.ix.libs.partial(top)
        first = min((node_sym(n) for n in srcs), key=lambda s: (s.module, s.name))
        d = p.ix.definition(first) if p.ix.modules.get(first.module) else None
        at = d if d is not None else p.ix.modules[first.module].tree
        names = ", ".join(x for x in p.ix.libs.mapped_of(top) if x not in libmap.EXCEPTIONS)
        e = TranspileError(
            f"`{first.module}.{first.name}` is translated from the source of {dist.name} {dist.version} (the library "
            f"map does not cover it), but the map's `{dotted}` is used too ({file}:{line}): the map's objects and the "
            f"source's do not mix in one binary (an object of one is no instance of the other's classes); use only "
            f"the map's names of {top} ({names}), or leave the routes reaching its source to Python (--python-side)",
            at, str(p.ix.modules[first.module].path))
        p.fn_errors[("libmix", top)] = e
        for n in srcs:
            p.edges.setdefault(n, set()).add(("libmix", top))


def link_lib_statements(p: "Project") -> None:
    """The module statements of libraries that did not translate (module_statements): every function, class
    and global of the library, or of one of its submodules (importing `lib.sub` runs `lib/__init__.py`),
    reaches their error nodes, so whatever uses the library is refused or moved --python-side with it."""
    failed = p.__dict__.get("libmod_nodes", {})
    if not failed:
        return
    nodes = set(p.fns) | set(p.fn_errors) | set(p.edges) | {m for v in list(p.edges.values()) for m in v}
    for n in nodes:
        sym = node_sym(n)
        mod = p.ix.modules.get(sym.module) if sym is not None else None
        if mod is None or mod.lib is None:
            continue  # the project's own code reaches a library through the library's definitions
        parts = sym.module.split(".")
        for i in range(1, len(parts) + 1):
            for err in failed.get(".".join(parts[:i]), ()):
                if err != n:
                    p.edges.setdefault(n, set()).add(err)
    # a project statement run at startup (`REG.update(lib.table())`) runs whatever it reaches, used or not
    lib_errs = {id(v) for k, v in p.fn_errors.items() if isinstance(k, tuple) and k and k[0] == "libmod"}
    of = {name: module for module, _, name in p.__dict__.get("eager", [])}
    for name in sorted(p.__dict__.get("modst", set())):
        if name not in of or p.ix.modules[of[name]].lib is not None:
            continue  # a library's own statements run only when it is used: their errors are its nodes
        for e in closure_errors(p, ("modst", name)):
            if id(e) in lib_errs:
                if not p.collect:
                    raise e
                if e not in p.fe.global_errors:
                    p.fe.global_errors.append(e)


def starlette_version(p: "Project") -> tuple[int, int]:
    """The project's Starlette (its CORS behaviour changes between versions)."""
    m = re.match(r"(\d+)\.(\d+)", locked_version(p.ix.root, "starlette") or "1.7")
    return int(m.group(1)), int(m.group(2))


def creates_app(fn) -> bool:
    """an application factory: its body builds the FastAPI application"""
    return any(isinstance(n, ast.Call) and isinstance(n.func, (ast.Name, ast.Attribute))
               and dotted(n.func) in {"FastAPI", "fastapi.FastAPI"} for n in ast.walk(fn))


def asgi_instance(fc: "FnCompiler", call: ast.Call, slot: str) -> str:
    """`Cls(app, *args, **kwargs)` of a raw ASGI middleware, `app` being the stack after it (`slot`)."""
    if any(isinstance(a, ast.Starred) for a in call.args) or any(k.arg is None for k in call.keywords):
        raise fc.err("add_middleware(*args/**kwargs) is not supported", call)
    name = "__py2axum_next_app"
    inst = ast.Call(func=call.args[0], args=[ast.Name(id=name, ctx=ast.Load())] + call.args[1:], keywords=call.keywords)
    ast.copy_location(inst, call)
    ast.fix_missing_locations(inst)
    fc.__dict__.setdefault("raw_names", {})[name] = f"V::native({RT}::Native::AsgiApp({slot}.clone()))"
    try:
        return fc.expr(inst)
    finally:
        fc.__dict__["raw_names"].pop(name)


def context_mw(fe: Frontend, module: str, call: ast.Call, src: str) -> str:
    """`RawContextMiddleware(plugins=(RequestIdPlugin(...), ...))`: the plugins, read statically."""
    def err(msg, node):
        return TranspileError(f"RawContextMiddleware: {msg}", node, src)
    plugins = []
    for k in call.keywords:
        if k.arg != "plugins":
            raise err(f"option {k.arg}= is not supported (only plugins=)", k)
        if not isinstance(k.value, (ast.Tuple, ast.List)):
            raise err("plugins= must be a literal tuple or list", k.value)
        for pc in k.value.elts:
            t = fe.index.resolve_expr(module, pc.func) if isinstance(pc, ast.Call) else None
            key = CONTEXT_PLUGINS.get(libmap.canonical(t.dotted)) if isinstance(t, Ext) else None
            if key is None or pc.args:
                raise err(f"plugin {ast.unparse(pc)} is not supported (RequestIdPlugin, CorrelationIdPlugin, keyword options)", pc)
            opts = {"force_new_uuid": False, "validate": True}
            for o in pc.keywords:
                if o.arg == "version" and isinstance(o.value, ast.Constant) and o.value.value == 4:
                    continue
                if o.arg not in opts or not (isinstance(o.value, ast.Constant) and isinstance(o.value.value, bool)):
                    raise err(f"plugin option {o.arg}= is not supported (force_new_uuid, validate as literals, version=4)", o)
                opts[o.arg] = o.value.value
            plugins.append(f"{RT}::ctxmw::Plugin {{ key: {rs(key)}, force_new_uuid: {str(opts['force_new_uuid']).lower()}, "
                           f"validate: {str(opts['validate']).lower()} }}")
    if len(call.args) > 1:
        raise err("pass plugins= by keyword", call)
    return f"{RT}::ctxmw::RawContext {{ plugins: vec![{', '.join(plugins)}] }}"


def build_stack(p: "Project", fe: Frontend) -> str:
    """`pub fn stack`: the app's middlewares and exception handlers, registered in source order, the
    `if`s around them evaluated at startup (in the factory's scope for a factory)."""
    out = []
    p.__dict__["stack_classes"] = []

    def stmts(fc: "FnCompiler", module: str, body: list, factory) -> None:
        relayed = fe.__dict__.get("lib_prefix_includes", set())
        for st in body:
            if not _stack_relevant(fe, module, st):
                continue
            if (isinstance(st, ast.Expr) and id(st.value) in relayed) or (
                    isinstance(st, ast.If) and not st.orelse and all(isinstance(b, ast.Expr) and id(b.value) in relayed for b in st.body)
                    and not any(isinstance(n, (ast.Call, ast.Await, ast.NamedExpr)) for n in ast.walk(st.test))):
                # a library router relayed by its prefix (lib_router_static_prefix): no barrier, its `if` not read
                continue
            if isinstance(st, ast.Expr) and isinstance(st.value, ast.Call) and configure_call(fe, module, st.value):
                # `configure(app)`: a project function given the application, run while the stack is built
                check_configure(p, fe, module, st.value)
                saved = fc.__dict__.get("app_alias")
                fc.__dict__["app_alias"] = configure_call(fe, module, st.value) | (saved or set())
                try:
                    fc.emit(f"let _ = {fc.expr(st.value)};")
                finally:
                    fc.__dict__.pop("app_alias")
                    if saved is not None:
                        fc.__dict__["app_alias"] = saved
                fc.emit(f"for __mw in {RT}::routing::take_built() {{ __st.add_middleware(__mw); }}")
                fc.emit(f"for (__k, __h) in {RT}::routing::take_handlers() {{ __st.exception_handler(&__k, __h)?; }}")
            elif isinstance(st, (ast.Expr, ast.Assign, ast.AnnAssign)) and any(
                    instr_call(fe, module, n, ("instrument",)) for n in ast.walk(st)):
                # `Instrumentator(...).instrument(app)...` (or `x = ...` of it): its middleware goes here in the stack
                if isinstance(st, ast.Expr):
                    fc.emit(f"let _ = {fc.expr(st.value)};")
                else:
                    target = st.targets[0] if isinstance(st, ast.Assign) else st.target
                    if not (isinstance(target, ast.Name) and (not isinstance(st, ast.Assign) or len(st.targets) == 1)):
                        raise TranspileError("Instrumentator().instrument(app) assigned to anything else than a name is "
                                             "not supported", st, p.src(module))
                    fc.emit(f"let _ = {fc.expr(ast.copy_location(ast.Name(target.id, ast.Load()), target))};")
                fc.emit(f"for __mw in {RT}::instr::take_pending()? {{ __st.add_middleware(__mw); }}")
                fc.emit(f"for __mw in {RT}::routing::take_built() {{ __st.add_middleware(__mw); }}")
            elif isinstance(st, ast.Expr) and lib_router_include(fe, module, st.value):
                # the library's routes come here in Starlette's order: a route `configure(app)` adds after this
                # point is tried after them, so only Python can answer it (asgi::under_mount)
                fc.emit(f"{RT}::routing::lib_barrier();")
            elif isinstance(st, ast.Expr) and isinstance(st.value, ast.Call) and stack_node(fe, module, st.value):
                middleware(fc, module, st.value)
            elif (isinstance(st, (ast.Assign, ast.AnnAssign)) and isinstance(st.value, ast.Call)
                  and mw_entries(fe, module, st.value) is not None):
                # `app = FastAPI(middleware=[...])`: Starlette's user_middleware starts as that list
                for e in mw_entries(fe, module, st.value):
                    entry(fc, module, e)
            elif isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)) and any(stack_deco(fe, module, d) for d in st.decorator_list):
                handler(fc, module, st, factory)
            elif isinstance(st, ast.If):
                cond = fc.truthy(st.test)
                fc.emit(f"if {cond} {{")
                stmts(fc, module, st.body, factory)
                fc.emit("} else {")
                stmts(fc, module, st.orelse, factory)
                fc.emit("}")
            elif isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)) and factory is None and not creates_app(st):
                continue  # a function given the application (`configure(app)`): run where the factory calls it
            elif isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)) and factory is None:
                sub = FnCompiler(p, module, None, "stack")
                sub.factory = st
                sub.__dict__["app_alias"] = set(fe.app_vars.get(module, ()))
                stmts(sub, module, st.body, st)
                fc.emit("{")
                fc.lines.extend(sub.lines)
                fc.emit("}")
            else:
                # the first call that made the statement relevant (_stack_relevant), whatever its kind
                n = next(n for n in ast.walk(st) if _stack_call(fe, module, n))
                raise TranspileError(f"{ast.unparse(n)[:60]} inside `{type(st).__name__}` is not supported "
                                     "(only at the top level of the module or of the app factory, or under an `if`)",
                                     n, p.src(module))

    def entry(fc: "FnCompiler", module: str, e: ast.AST) -> None:
        """`Middleware(cls, *args, **kwargs)` of `FastAPI(middleware=[...])`"""
        f = fe.index.resolve_expr(module, e.func) if isinstance(e, ast.Call) else None
        if not (isinstance(f, Ext) and libmap.canonical(f.dotted) in MW_ENTRY and e.args):
            raise TranspileError("FastAPI(middleware=[...]) entries must be Middleware(cls, ...)", e, p.src(module))
        if mw_class_kind(fe, module, e.args[0]) == "gzip":
            return  # the outermost tower layer (prepare: list_gzip), as for app.add_middleware(GZipMiddleware)
        middleware(fc, module, e, push=True)

    def middleware(fc: "FnCompiler", module: str, call: ast.Call, push: bool = False) -> None:
        add = "push_middleware" if push else "add_middleware"
        if (isinstance(call.func, ast.Attribute) and call.func.attr == "add_exception_handler"
                and not push and fe._is_app(module, call.func.value)):
            # `app.add_exception_handler(key, handler)`
            args = list(call.args) + [k.value for k in call.keywords]
            if len(args) != 2 or any(isinstance(a, ast.Starred) for a in call.args) or any(k.arg is None for k in call.keywords):
                raise TranspileError("app.add_exception_handler(key, handler) takes two arguments", call, p.src(module))
            fc.emit(f"__st.exception_handler(&{fc.expr(args[0])}, {fc.expr(args[1])})?;")
            return
        if is_add_route(fe, module, call):
            path = _route_path(fe, TranspileError("", call, p.src(module)))
            if path in fe.python_side:
                return  # proxied to the Python side
            ep = call.args[1] if len(call.args) > 1 else next((k.value for k in call.keywords if k.arg == "route"), None)
            t = p.resolve(module, ep) if isinstance(ep, (ast.Name, ast.Attribute)) else None
            if isinstance(t, Sym) and isinstance(p.ix.definition(t), ast.ClassDef):
                raise TranspileError("add_route() with a class endpoint (HTTPEndpoint, raw ASGI class) is not supported: "
                                     "pass an instance or a function", ep, p.src(module))
            names = {n.id for n in ast.walk(call.func) if isinstance(n, ast.Name)}
            saved = fc.__dict__.get("app_alias")
            fc.__dict__["app_alias"] = names | (saved or set())
            try:
                fc.emit(f"let _ = {fc.expr(call)};")
            finally:
                fc.__dict__.pop("app_alias")
                if saved is not None:
                    fc.__dict__["app_alias"] = saved
            return
        kind = mw_kind(fe, module, call)
        if any(k.arg is None for k in call.keywords) or any(isinstance(a, ast.Starred) for a in call.args):
            raise TranspileError("add_middleware(*args/**kwargs) is not supported", call, p.src(module))
        if kind == "cors":
            if len(call.args) > 1:
                raise TranspileError("CORSMiddleware options must be passed by keyword", call, p.src(module))
            kw = ", ".join(f"({rs(k.arg)}.to_string(), {fc.expr(k.value)})" for k in call.keywords)
            major, minor = starlette_version(p)
            fc.emit(f"__st.{add}({RT}::asgi::Mw::Cors({RT}::asgi::Cors::new(vec![{kw}], ({major}, {minor}))?));")
        elif kind == "base":
            disp = [k for k in call.keywords if k.arg == "dispatch"]
            if len(call.args) != 1 or len(disp) != 1 or len(call.keywords) != 1:
                raise TranspileError("add_middleware(BaseHTTPMiddleware, dispatch=f) is the only supported form", call, p.src(module))
            fc.emit(f"__st.{add}({RT}::asgi::Mw::Dispatch({fc.expr(disp[0].value)}));")
        elif kind == "context":
            fc.emit(f"__st.{add}({RT}::asgi::Mw::Context({context_mw(fe, module, call, p.src(module))}));")
        elif kind == "session":
            session_mw_options(p, module, call)  # no layer: transparent while nothing reads the session (MW_SESSION)
        elif kind == "sentry":
            if len(call.args) != 1 or call.keywords:
                raise TranspileError("add_middleware(SentryAsgiMiddleware) is supported without options", call, p.src(module))
        elif kind in {"https", "trusted"}:
            # HTTP only here: a WebSocket handshake does not cross the stack (refused with WebSocket routes)
            fe.host_mws = [*fe.__dict__.get("host_mws", []), (call, p.src(module))]
            if kind == "https":
                if len(call.args) > 1 or call.keywords:
                    raise TranspileError("HTTPSRedirectMiddleware takes no options", call, p.src(module))
                fc.emit(f"__st.{add}({RT}::asgi::Mw::HttpsRedirect);")
            else:
                if len(call.args) > 1:
                    raise TranspileError("TrustedHostMiddleware options must be passed by keyword", call, p.src(module))
                hosts, www = ["*"], True
                for kw in call.keywords:
                    v = p.const(kw.value, module)
                    if kw.arg == "allowed_hosts" and (v is None or (isinstance(v, (list, tuple)) and all(isinstance(h, str) for h in v))):
                        hosts = ["*"] if v is None else list(v)
                    elif kw.arg == "www_redirect" and isinstance(v, bool):
                        www = v
                    else:
                        raise TranspileError(f"TrustedHostMiddleware({kw.arg}=): a literal list of host patterns "
                                             "(allowed_hosts) or a bool (www_redirect) only", kw, p.src(module))
                for h in hosts:
                    # Starlette's own assertions, raised when the stack is built
                    if "*" in h[1:] or (h.startswith("*") and h != "*" and not h.startswith("*.")):
                        raise TranspileError(f"TrustedHostMiddleware: {h!r}: domain wildcard patterns must be like "
                                             "'*.example.com'", call, p.src(module))
                vec = ", ".join(f"{rs(h)}.to_string()" for h in hosts)
                fc.emit(f"__st.{add}({RT}::asgi::Mw::TrustedHost({RT}::asgi::TrustedHost {{ hosts: vec![{vec}], "
                        f"www_redirect: {str(www).lower()} }}));")
        elif kind == "asgi":
            # Starlette: cls(app, *args, **kwargs) once the stack is built; `app` = the rest of the stack
            t = fe.index.resolve_expr(module, call.args[0])
            p.stack_classes.append(t)
            fe.raw_mws = [*fe.__dict__.get("raw_mws", []), (t, call, p.src(module))]
            slot = fc.tmp("slot")
            fc.emit(f"let {slot} = {RT}::asgi::Slot::new();")
            obj = asgi_instance(fc, call, slot)
            fc.emit(f"__st.{add}({RT}::asgi::Mw::Asgi({obj}, {slot}));")
        elif kind is None:
            raise TranspileError(f"middleware {ast.unparse(call.args[0]) if call.args else '?'} is not supported "
                                 "(CORSMiddleware, GZipMiddleware, BaseHTTPMiddleware, a project BaseHTTPMiddleware "
                                 "subclass or raw ASGI class, starlette_context's RawContextMiddleware)", call, p.src(module))
        else:
            # Starlette: cls(app, *args, **kwargs), then its `dispatch` for every request
            t = fe.index.resolve_expr(module, call.args[0])
            p.stack_classes.append(t)
            inst = ast.Call(func=call.args[0], args=[ast.Constant(None)] + call.args[1:], keywords=call.keywords)
            ast.copy_location(inst, call)
            ast.fix_missing_locations(inst)
            obj = fc.expr(inst)
            d = fc.q(f"{RT}::methods::getattr(cx, &{obj}, \"dispatch\").await")
            fc.emit(f"__st.{add}({RT}::asgi::Mw::Dispatch({d}));")

    def handler(fc: "FnCompiler", module: str, fn, factory) -> None:
        for d in fn.decorator_list:
            if stack_deco(fe, module, d) is None:
                raise TranspileError(f"decorator @{ast.unparse(d)} on a handler is not supported", d, p.src(module))
        if factory is None:
            sym = Sym(module, fn.name)
        else:
            if fn not in factory.body:
                raise TranspileError(f"handler {fn.name} must be at the top level of the app factory", fn, p.src(module))
            sym = Sym(module, f"{factory.name}.<locals>.{fn.name}")
            p.__dict__.setdefault("nested", {})[sym] = (fn, factory)
        value = f"V::native({RT}::Native::Bound({function_wrapper(p, sym)}, V::None))"
        for d in fn.decorator_list:
            if d.func.attr == "exception_handler":
                if len(d.args) != 1 or d.keywords:
                    raise TranspileError("@app.exception_handler(key) takes one argument", d, p.src(module))
                key = fc.expr(d.args[0])
                fc.emit(f"__st.exception_handler(&{key}, {value})?;")
            else:
                if not (len(d.args) == 1 and isinstance(d.args[0], ast.Constant) and d.args[0].value == "http"):
                    raise TranspileError('only @app.middleware("http") is supported', d, p.src(module))
                fc.emit(f"__st.add_middleware({RT}::asgi::Mw::Dispatch({value}));")

    for m in fe.index.package_modules():
        if m.name not in fe.app_vars or not _stack_relevant(fe, m.name, m.tree):
            continue
        fc = FnCompiler(p, m.name, None, "stack")
        fc.__dict__["app_alias"] = set(fe.app_vars[m.name])  # `state=app.state`: the application object
        stmts(fc, m.name, m.tree.body, None)
        out.extend(fc.lines)
    body = "\n".join("        " + l for l in out)
    return (f"pub fn stack(cx: &Cx) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<{RT}::asgi::Stack>> + Send + '_>> {{\n"
            f"    Box::pin(async move {{\n        let mut __st = {RT}::asgi::Stack::default();\n        {RT}::routing::begin_build();\n"
            f"        let __r = async {{\n{body}\n        Ok::<(), Exc>(())\n        }}.await;\n"
            f"        {RT}::routing::end_build();\n        __r?;\n        Ok(__st)\n    }})\n}}")


def route_tree(p: "Project", fe: Frontend) -> tuple[str, dict]:
    """`APP_ROUTES`: the application's routes as `app.router.routes` lists them (FastAPI's docs routes, then
    each `include_router` as an `_IncludedRouter` and each app route, in registration order), for code
    inspecting them at run time. Returns the Rust items and {(module, def line): APIRoute node}."""
    order = {name: i for i, name in enumerate(import_order(fe))}
    items: list[str] = []
    nodes: dict[tuple[str, int], str] = {}
    members: dict[object, list[tuple[tuple, str]]] = {}  # owner ("app" or router sym) -> [(key, node)]
    for m in fe.index.package_modules():
        parents = fe._parents(m.name)
        for fn in ast.walk(m.tree):
            if not isinstance(fn, (ast.AsyncFunctionDef, ast.FunctionDef)):
                continue
            founds = fe._route_decorators(fn, m.name, parents)
            ws = fe._ws_decorator(fn, m.name, parents)
            if not founds and ws is None:
                continue
            for k_, (deco, router) in enumerate(founds or [ws]):
                found = founds[k_] if founds else None
                r = fe.routers.get(router) if router else None
                try:
                    path = (r.prefix if r else "") + route_path(deco, str(m.path))
                    pattern = starlette_pattern(path, fn, str(m.path))
                except (TranspileError, IndexError):
                    continue  # the route itself is refused
                route_node_item(items, nodes, members, order, m, fn, deco, router, path, pattern, found, len(founds), k_)
    routers: dict[object, str] = {}


    def router_def(sym) -> str:
        if sym not in routers:
            routers[sym] = f"RT_{mod_ident(sym.module)}__{ident(sym.name)}"
            items.append(f"static {routers[sym]}: {RT}::routing::RouterDef = {RT}::routing::RouterDef {{ "
                         f"prefix: {rs(fe.routers[sym].prefix)}, routes: &[{', '.join('&' + n for n in owned(sym))}], error: \"\" }};")
        return routers[sym]

    def owned(owner) -> list[str]:
        out = list(members.get(owner, []))
        for i, (own, child, call, *_) in enumerate(fe.__dict__.get("include_edges", [])):
            if own != owner:
                continue
            prefix = next((k.value.value for k in call.keywords if k.arg == "prefix" and isinstance(k.value, ast.Constant)),
                          fe.__dict__.get("include_prefix", {}).get(id(call), ""))
            # a runtime prefix (settings): its marker, resolved at run time
            prefix = next((f"\x01{j}\x01" for k in call.keywords if k.arg == "prefix"
                           for j, (v, _, _) in enumerate(fe.dyn_prefixes) if v is k.value), prefix)
            node = f"RI_{len(items) + 1}_{i}"
            items.append(f"static {node}: {RT}::routing::Node = {RT}::routing::Node::Included {{ prefix: {rs_marked(prefix)}, "
                         f"router: &{router_def(child)} }};")
            kmod, kline = fe.include_edges[i][7]
            out.append(((order.get(kmod, 0), *kline), node))
        return [n for _, n in sorted(out, key=lambda x: x[0])]

    docs = []
    docs_routes, error = fastapi_docs_routes(fe)
    for i, (path, name) in enumerate(docs_routes or []):
        node = f"RD_{i}"
        items.append(f"static {node}: {RT}::routing::Node = {RT}::routing::Node::Starlette {{ path: {rs(path)}, "
                     f"pattern: {rs(starlette_pattern(path, None, ''))}, name: {rs(name)} }};")
        docs.append(node)
    app_routes = docs + owned("app")
    items.append(f"pub static APP_ROUTES: {RT}::routing::RouterDef = {RT}::routing::RouterDef {{ prefix: \"\", "
                 f"routes: &[{', '.join('&' + n for n in app_routes)}], error: {rs(error)} }};")
    return "\n".join(items), nodes


DOCS_URL_DEFAULTS = {"openapi_url": "/openapi.json", "docs_url": "/docs", "swagger_ui_oauth2_redirect_url": "/docs/oauth2-redirect",
                     "redoc_url": "/redoc"}


def fastapi_docs_options(fe: Frontend) -> tuple[str, dict] | None:
    """`FastAPI(docs_url=None if SETTINGS.is_production() else "/", ...)`: (module, {option: expression}) of the
    application's `*_url` options when one is not a literal (read at startup, routing::set_docs), else None."""
    for m in fe.index.package_modules():
        if fe.app_vars and m.name not in fe.app_vars:
            continue
        for node in ast.walk(m.tree):
            if (isinstance(node, (ast.Assign, ast.AnnAssign)) and isinstance(node.value, ast.Call)
                    and is_ext(fe.index.resolve_expr(m.name, node.value.func), "fastapi.FastAPI")):
                kws = {k.arg: k.value for k in node.value.keywords if k.arg in DOCS_URL_DEFAULTS}
                if any(not isinstance(v, ast.Constant) for v in kws.values()):
                    return m.name, kws
    return None


def fastapi_docs_routes(fe: Frontend) -> tuple[list[tuple[str, str]] | None, str]:
    """The routes `FastAPI(...)` adds for its documentation, with its literal `*_url` options (else none here:
    read at startup, fastapi_docs_options)."""
    opts = dict(DOCS_URL_DEFAULTS)
    for m in fe.index.package_modules():
        if fe.app_vars and m.name not in fe.app_vars:
            continue
        for node in ast.walk(m.tree):
            if (isinstance(node, (ast.Assign, ast.AnnAssign)) and isinstance(node.value, ast.Call)
                    and is_ext(fe.index.resolve_expr(m.name, node.value.func), "fastapi.FastAPI")):
                for k in node.value.keywords:
                    if k.arg in opts:
                        if not isinstance(k.value, ast.Constant):
                            return [], ""
                        opts[k.arg] = k.value.value
    out = []
    if opts["openapi_url"]:
        out.append((opts["openapi_url"], "openapi"))
        if opts["docs_url"]:
            out.append((opts["docs_url"], "swagger_ui_html"))
            if opts["swagger_ui_oauth2_redirect_url"]:
                out.append((opts["swagger_ui_oauth2_redirect_url"], "swagger_ui_redirect"))
        if opts["redoc_url"]:
            out.append((opts["redoc_url"], "redoc_html"))
    return out, ""


FASTAPI_DOC_OPTIONS = {"title", "description", "version", "summary", "openapi_tags", "contact", "license_info",
                       "terms_of_service", "servers", "swagger_ui_parameters", "docs_url", "redoc_url", "openapi_url",
                       "swagger_ui_oauth2_redirect_url", "separate_input_output_schemas", "generate_unique_id_function",
                       "openapi_prefix", "swagger_ui_init_oauth", "webhooks"}

STARLETTE_CONVERTORS = {"str": "[^/]+", "path": ".*", "int": "[0-9]+", "float": r"[0-9]+(?:\.[0-9]+)?",
                        "uuid": "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"}


def starlette_pattern(path: str, node, src: str) -> str:
    """Starlette's `compile_path` regex for a route path (`/tasks/{task_id}` -> `^/tasks/(?P<task_id>[^/]+)$`)."""
    out, pos = "^", 0
    for m in re.finditer(r"{([a-zA-Z_][a-zA-Z0-9_]*)(?::([a-zA-Z_][a-zA-Z0-9_]*))?}", path):
        conv = m.group(2) or "str"
        if conv not in ("str", "path"):
            raise TranspileError(f"path convertor `:{conv}` is not supported", node, src)
        out += "".join("\\" + c if c in ".^$*+?()[]{}|\\" else c for c in path[pos:m.start()])
        out += f"(?P<{m.group(1)}>{STARLETTE_CONVERTORS[conv]})"
        pos = m.end()
    out += "".join("\\" + c if c in ".^$*+?()[]{}|\\" else c for c in path[pos:])
    return out + "$"


def _conditional(fe: Frontend, e: TranspileError) -> bool:
    from .frontend import _under_if
    for m in fe.index.package_modules():
        if str(m.path) == e.file:
            return _under_if(e.node, fe._parents(m.name))
    return False


RELAY_STACK = ("pub fn stack(cx: &Cx) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<dynrt::asgi::Stack>> + Send + '_>> {\n"
               "    Box::pin(async move {\n        let mut __st = dynrt::asgi::Stack::default();\n"
               "        dynrt::routing::begin_build();\n        dynrt::routing::end_build();\n        Ok(__st)\n    })\n}")


def prepare_relay(fe: Frontend) -> None:
    """--python-side auto when nothing but the whole application can stay in Python (fallback.py): the binary
    serves no route and relays every request to PY2AXUM_PYTHON_URL (an empty `mount` prefix: everything is
    under it), so the Python application's own middlewares, exception handlers and lifespan answer for it. The
    discovery passes only find the routes' paths (`APP_ROUTES`); their errors are the reason for the relay."""
    collect = fe.collect
    fe.collect = True
    for step in (fe._load_imports, fe._classify, fe._discover_apps, fe._discover_routers):
        try:
            step()
        except TranspileError:
            pass
    fe.collect = collect
    fe._mounts_cache = {}
    fe.global_errors = []
    fe.python_side = set()
    fe.lifespan = None
    fe.__dict__.pop("root_path", None)
    fe.relay = True
    fe.mount_fallback = [""]
    fe.notes.append("the whole application stays in Python (--python-side auto): every request is relayed to "
                    "PY2AXUM_PYTHON_URL")


def generate_project(fe: Frontend, out_dir: Path, source: str, crate_name: str, gzip=None, collect: bool = False,
                     stream: bool = True) -> Project:
    """Compile every route of the project with the dyn backend and write the Rust crate (`stream=False`:
    list responses are always buffered, `--no-stream`)."""
    proj = Project(fe, collect=collect)
    proj.__dict__["stream_lists"] = stream
    # functions that fail to translate become stubs that raise; the build fails only if a translated
    # route can reach one (below)
    proj.__dict__["defer_fn_errors"] = True
    for lib in ("httpx", "aiohttp", "uvloop", "stripe", "bcrypt", "python-dotenv", "pyjwt", "prometheus-fastapi-instrumentator"):
        # stripe: the installed release when the project locks none (as the conformance fixtures run)
        v = locked_version(fe.index.root, lib) or (_installed_stripe() if lib == "stripe" else None)
        if v:
            libmap.LIB_VERSIONS[lib] = v
        else:
            libmap.LIB_VERSIONS.pop(lib, None)  # a previous project's in the same process
    from .libsrc import locked as _locked
    # openpyxl writes through lxml when the project installs it: only its ElementTree writer is translated
    if lxml := _locked(fe.index.root, "lxml"):
        libmap.LIB_VERSIONS["lxml"] = lxml
    else:
        libmap.LIB_VERSIONS.pop("lxml", None)

    # reportlab's C accelerators and bitmap renderer: their output is not the pure-Python one reproduced
    for dist in libmap.RL_REFUSED_DISTS:
        if v := _locked(fe.index.root, dist):
            libmap.LIB_VERSIONS[dist] = v
        else:
            libmap.LIB_VERSIONS.pop(dist, None)
    libmap.set_python(target_python(fe.index.root))
    rb = RouteBuilder(proj)
    routes = []
    idx = 0
    prefix_fns, empty_under = [], {}
    # --python-side auto, whole application in Python (fallback.py): no route, stack, lifespan nor module code
    relay = bool(fe.__dict__.get("relay"))
    for i, (value, file, module) in enumerate([] if relay else fe.dyn_prefixes):
        name = f"prefix_{i}"
        prefix_fns.append(name)
        proj.cur = ("prefix", i)
        fc = FnCompiler(proj, module, None, name)
        try:
            code = fc.expr(value)
            body = "\n".join("    " + l for l in fc.lines)
            proj.items.append(f"/// include_router(prefix={ast.unparse(value)}) ({Path(file).name}:{value.lineno})\n"
                              f"async fn {name}(cx: &Cx) -> R {{\n{body}\n    Ok({code})\n}}")
            proj.drain()
        except TranspileError as e:
            if not collect:
                raise
            proj.fn_errors[("prefix", i)] = e
            proj.items.append(f"async fn {name}(cx: &Cx) -> R {{ unreachable!() }}")
        proj.cur = None
    cond_fns = []
    for i, (test, file, module) in enumerate([] if relay else fe.dyn_conds):
        name = f"route_cond_{i}"
        cond_fns.append(name)
        proj.cur = ("cond", i)
        fc = FnCompiler(proj, module, None, name)
        try:
            code = fc.truthy(test)
            body = "\n".join("    " + l for l in fc.lines)
            proj.items.append(f"/// `if {ast.unparse(test)}:` around include_router ({Path(file).name}:{test.lineno}), read at startup\n"
                              f"async fn {name}(cx: &Cx) -> R<bool> {{\n{body}\n    Ok({code})\n}}")
            proj.drain()
        except TranspileError as e:
            if not collect:
                raise
            proj.fn_errors[("cond", i)] = e
            proj.items.append(f"async fn {name}(cx: &Cx) -> R<bool> {{ unreachable!() }}")
        proj.cur = None
    ws_routes = []
    route_keys: dict[int, tuple] = {}
    route_decos: dict[int, int] = {}  # the line of the route decorator that registered it (its node in the tree)
    route_conds: dict[int, tuple] = {}
    for m in [] if relay else fe.index.package_modules():
        parents = fe._parents(m.name)
        for fn in ast.walk(m.tree):
            if not isinstance(fn, (ast.AsyncFunctionDef, ast.FunctionDef)):
                continue
            for d in fn.decorator_list:
                if (isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and d.func.attr == "websocket_route"
                        and not fe._is_app(m.name, d.func.value)):  # the app's is a global error
                    e = TranspileError("@router.websocket_route(...) is not supported: use @router.websocket(...)", d, str(m.path))
                    if not collect:
                        raise e
                    fe.global_errors.append(e)
                if (isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and d.func.attr in UNTRANSLATED_ROUTE_ATTRS
                        and not (d.func.attr == "api_route" and fe._expand(d, m.name))
                        and (fe._is_app(m.name, d.func.value)
                             or fe.index.resolve_expr(m.name, d.func.value, frontend_enclosing_fn(fn, parents)) in fe.routers)):
                    # not a route of the translation: the binary would not serve it at all (404/405 where FastAPI answers)
                    e = TranspileError(f"route decorator @{ast.unparse(d.func)}(...) is not supported: the binary would not "
                                       "serve this route; use @app.get/post/put/patch/delete (one per method)", d, str(m.path))
                    if not collect:
                        raise e
                    fe.global_errors.append(e)
            wsf = fe._ws_decorator(fn, m.name, parents)
            if wsf is not None:
                idx = ws_route(proj, rb, fe, fn, m, wsf, idx, ws_routes, collect)
            founds = fe._route_decorators(fn, m.name, parents)
            for k_, (deco, router) in enumerate(founds):
              mounts = fe._mounts_cache.get(router, []) if router else [Mount("", deps=list(fe.__dict__.get("app_deps", [])))]
              for mt in mounts:
                  idx += 1
                  r = fe.routers.get(router) if router else None
                  path = mt.prefix + (r.prefix if r else "") + route_path(deco, str(m.path))
                  if fe.shown_path(path) in fe.__dict__.get("python_side", ()):
                      # proxied by its runtime path: a prefix read from the settings stays a marker until startup
                      fe.__dict__.setdefault("python_side_raw", set()).add(path)
                      fe.notes.append(f"{m.path}:{fn.lineno}: {deco.func.attr.upper()} {fe.shown_path(path)} declared Python-side")
                      continue
                  rid = ("route", idx)
                  for k in re.findall("\x01(\\d+)\x01", path):
                      proj.edges.setdefault(rid, set()).add(("prefix", int(k)))
                  for k, _ in mt.conds:
                      proj.edges.setdefault(rid, set()).add(("cond", k))
                  tail = path.rsplit("\x01", 1)[-1] if "\x01" in path else None
                  if tail == "":
                      empty_under.setdefault(int(path.split("\x01")[-2]), fn.name)
                  proj.cur = rid
                  info = {"method": deco.func.attr, "path": fe.shown_path(path), "func": fn.name, "file": str(m.path), "line": fn.lineno,
                          "conditional": mt.conditional, "node": fn, "module": m.name, "router": router, "id": rid}
                  try:
                      if r is not None and r.unsupported:
                          raise TranspileError(f"unsupported APIRouter option {r.unsupported[0].arg}=", r.unsupported[0], r.file)
                      if mt.unsupported:
                          node, f = mt.unsupported[0]
                          if getattr(node, "arg", None) == "prefix":
                              raise TranspileError("include_router(prefix=<non-literal>) inside a function (app factory) "
                                                   "is not supported: call include_router at module level, or use a "
                                                   "literal prefix", node, f)
                          raise TranspileError(f"unsupported include_router option {getattr(node, 'arg', '?')}=", node, f)
                      if mt.conditional:
                          raise TranspileError("include_router(...) under an `if` is not supported: the binary would "
                                               "serve the route unconditionally", fn, str(m.path))
                      endpoint_decorators(fe, fn, m, deco)
                      handler, axum_path = rb.route(fn, m.name, path, deco.func.attr, deco, router, mt, idx)
                      proj.cur = None
                      proj.drain()
                      routes.append((deco.func.attr, axum_path, handler, path, fn, m, router))
                      route_keys[id(routes[-1])] = (*mt.chain, (m.name, route_key(fn, deco, len(founds), k_)))
                      route_decos[id(routes[-1])] = deco.lineno
                      if mt.conds:
                          route_conds[id(routes[-1])] = mt.conds
                      proj.__dict__.setdefault("route_infos", []).append((info, None))
                  except TranspileError as e:
                      proj.cur = None
                      if not collect:
                          raise
                      proj.errors.append(e)
                      proj.__dict__.setdefault("route_infos", []).append((info, e))
    if not relay:
        engine_globals(proj, fe)
        module_statements(proj, fe)
        link_lib_statements(proj)
        factory_sentry_init(proj, fe)
    proj.cur = ("stack", 0)
    try:
        stack = RELAY_STACK if relay else build_stack(proj, fe)
    except TranspileError as e:
        if not collect:
            raise
        fe.global_errors.append(e)
        stack = ""
    proj.cur = None
    if not relay:
        mcp_register(proj, fe)
    lifespan = lifespan_fn(proj, fe)
    finalize(proj)
    link_partial_mix(proj)
    if collect:
        # a model holding a union the runtime refuses: its routes are refused (generation would fail)
        bad = union_errors(proj)[0]
        if bad:
            by_init = {v: k for k, v in proj.tds.items()}

            def tds_of(td, seen):
                if td in seen:
                    return seen
                seen.add(td)
                for t in re.findall(r"\bTD_\d+\b", by_init.get(td, "")):
                    tds_of(t, seen)
                return seen
            for info in list(proj.schemas.values()):
                reach = set()
                for f in info.fields:
                    if not f.get("any"):
                        ann, module = f["ann"]
                        try:
                            tds_of(proj.td(ann, module, dict(f.get("cons", {}))), reach)
                        except TranspileError:
                            pass
                for e, td in bad:
                    if td in reach and info.sym not in proj.class_errors:
                        proj.class_errors[info.sym] = e
        # a middleware that cannot be compiled blocks every route; the lifespan's errors are marked as such
        # (--python-side auto can leave the lifespan alone to Python: fallback.py)
        keys = {("stack", 0), ("mcp", 0)} | {k for k in proj.fn_errors
                                 if isinstance(k, tuple) and k and isinstance(k[0], Sym)
                                 and any(isinstance(c, Sym) and k[0].module == c.module and k[0].name.startswith(c.name + ".")
                                        for c in proj.__dict__.get("stack_classes", []))}
        app_wide = closure_errors_of(proj, keys)
        fe.global_errors += app_wide
        fe.global_errors += [scoped(e, "lifespan") for e in closure_errors_of(proj, {("lifespan", 0)})
                             if all(e is not x for x in app_wide)]
        fe.__dict__["collected"] = proj
        return proj
    for info, err in proj.__dict__.get("route_infos", []):
        for e in ([err] if err else []) + closure_errors(proj, info["id"]):
            raise e
    stack_keys = {("stack", 0), ("lifespan", 0), ("mcp", 0)} | {k for k in proj.fn_errors
                                   if isinstance(k, tuple) and k and isinstance(k[0], Sym)
                                 and any(isinstance(c, Sym) and k[0].module == c.module and k[0].name.startswith(c.name + ".")
                                          for c in proj.__dict__.get("stack_classes", []))}
    for e in closure_errors_of(proj, stack_keys):
        raise e
    proj.items.append(stack)
    proj.items.append(lifespan)
    # module globals (and the app factory's locals) evaluated at startup, as importing the app does
    order = {name: i for i, name in enumerate(import_order(fe))}
    keep = set(proj.globals.values()) | proj.__dict__.get("modst", set())
    eager = sorted({e for e in proj.__dict__.get("eager", []) if e[2] in keep or e[2].startswith(("fl_", "gd_", "ca_", "lb_"))},
                   key=lambda e: (order.get(e[0], 0), e[1], e[2]))
    proj.items.append(
        "/// Module globals evaluated at startup like Python's import (a failure is logged; the routes reading\n"
        "/// the value then raise it).\n"
        "pub async fn init_globals(cx: &Cx) {\n"
        + "".join(f"    if let Err(e) = {g}(cx).await {{ eprintln!(\"ERROR:py2axum:module global {g}: {{:?}}\", e); }}\n"
                  for _, _, g in eager)
        + "}")

    # the routes under an `if` read at startup: served only when every test is on their branch
    off = []
    if route_conds:
        rank = {name: i for i, name in enumerate(import_order(fe))}
        routes.sort(key=lambda r: tuple((rank.get(mod, 0), ln) for mod, ln in route_keys.get(id(r), ())))
        for r in routes:
            c = route_conds.get(id(r), ())
            off.append("false" if not c else "!(" + " && ".join(f"{'' if b else '!'}__c[{k}]" for k, b in c) + ")")
    docs_init = ""
    if (dopts := fastapi_docs_options(fe)) is not None and not relay:
        dmod, kws = dopts
        proj.cur = ("docs", 0)
        fc = FnCompiler(proj, dmod, None, "docs_urls")
        try:
            vals = [fc.expr(kws[k]) if k in kws else ("V::None" if v is None else f"V::str({rs(v)})")
                    for k, v in DOCS_URL_DEFAULTS.items()]
            body = "\n".join("    " + l for l in fc.lines)
            proj.items.append(f"/// FastAPI's docs routes, from its `*_url` options read at startup\n"
                              f"async fn docs_urls(cx: &Cx) -> R<()> {{\n{body}\n    {RT}::routing::set_docs("
                              + ", ".join(f"&{v}" for v in vals) + ")\n}")
            proj.drain()
            docs_init = "    docs_urls(cx).await?;\n"
        except TranspileError as e:
            if not collect:
                raise
            fe.global_errors.append(e)
        finally:
            proj.cur = None
    proj.items.append(
        "/// The runtime `include_router(prefix=...)` values, read once the module globals are set.\n"
        "pub async fn init_prefixes(cx: &Cx) -> R<()> {\n" + docs_init +
        f"    {RT}::web::set_prefixes(vec![" + "".join(
            f"({f}(cx).await?, {'Some(' + rs(empty_under[i]) + ')' if i in empty_under else 'None'}), "
            for i, f in enumerate(prefix_fns)) + "])?;\n"
        + (f"    let __c: Vec<bool> = vec![{''.join(f'{f}(cx).await?, ' for f in cond_fns)}];\n"
           f"    {RT}::web::set_disabled(vec![{', '.join(off)}]);\n" if route_conds else "")
        + "    Ok(())\n}")
    body = emit_descriptors(proj)
    # the project's schema / plain / enum classes, found by name when unpickling
    picklable = sorted({m.group(1) for it in [*proj.items, *body] for m in re.finditer(
        r"pub static (CLS_\w+): [\w:]*Class = [^;]*?ClassKind::(?:Schema|Enum)\(", it)})
    body.append("pub fn register_classes() {\n    " + f"{RT}::pickle::register(&[{', '.join('&' + c for c in picklable)}]);\n}}")
    # registration order, as Starlette tries them: each include_router at its place among its owner's routes
    # (not the order the modules are walked in: `/items/nested/ping` included before `/items/{id}` wins)
    if route_keys:
        rank = {name: i for i, name in enumerate(import_order(fe))}
        routes.sort(key=lambda r: tuple((rank.get(mod, 0), ln) for mod, ln in route_keys.get(id(r), ())))
    tree, nodes = route_tree(proj, fe)
    proj.items.append(tree)
    def node_of(r, m) -> str:
        key = (m.name, route_decos.get(id(r), 0))
        return ('Some(&' + nodes[key] + ')') if key in nodes else 'None'

    route_defs = [f"    {RT}::web::RouteDef {{ method: {rs(method.upper())}, pattern: {rs_marked(pattern)}, run: {handler}, "
                  f"node: {node_of(r, m)}, "
                  f"path: {rs_marked(path)}, src: ({rs(proj.rel_file(m.name) + ':' + str(fn.lineno))}, {rs(fn.name)}, {rs(m.name)}), "
                  f"sync_endpoint: {str(not isinstance(fn, ast.AsyncFunctionDef)).lower()}, "
                  f"first_line: {fn.decorator_list[0].lineno if fn.decorator_list else fn.lineno}, "
                  f"raw: {('Some(' + proj.__dict__['route_raw'][handler] + ')') if handler in proj.__dict__.get('route_raw', {}) else 'None'}, "
                  f"body: {str(proj.__dict__.get('route_body', {}).get(proj.__dict__.get('route_raw', {}).get(handler, handler), False)).lower()} }},"
                  for r in routes for method, pattern, handler, path, fn, m, _ in [r]]

    def ws_node(fn, m) -> str:
        wsd = fe._ws_decorator(fn, m.name, fe._parents(m.name))
        key = (m.name, wsd[0].lineno if wsd else 0)
        return ('Some(&' + nodes[key] + ')') if key in nodes else 'None'

    ws_defs = [f"    {RT}::ws::WsRouteDef {{ pattern: {rs_marked(pattern)}, run: {handler}, node: {ws_node(fn, m)} }},"
               for pattern, handler, fn, m in ws_routes]
    if any(isinstance(n, (ast.Import, ast.ImportFrom)) and "unicodedata" in ast.unparse(n)
           for m in list(proj.ix.modules.values()) for n in ast.walk(m.tree)):
        proj.items.append(ucd_unassigned(libmap.PYTHON))
        tables = ucd_tables(proj.items)
        if tables:
            proj.items.append(tables)
    proj.items.append(str_classes(libmap.PYTHON))
    proj.items.append(migrations_item(source, fe.index.root))
    gen = [
        "// Generated by py2axum (dyn backend) from the FastAPI project. Do not edit.",
        "#![allow(unused_mut, unused_variables, unused_imports, unreachable_code, dead_code, non_snake_case, unused_parens, unused_braces, unused_labels, unused_assignments, non_upper_case_globals, clippy::all)]",
        f"use {RT}::{{self, Cx, Exc, R, V}};",
        "",
        *body,
        *proj.items,
        "",
        f"pub static ROUTES: &[{RT}::web::RouteDef] = &[",
        *route_defs,
        "];",
        "",
        "/// the WebSocket routes, in declaration order",
        f"pub static WS_ROUTES: &[{RT}::ws::WsRouteDef] = &[",
        *ws_defs,
        "];",
        "",
        "/// traceback entries: where an exception passes in the project's code (dynrt/trace.rs)",
        f"pub static TB: &[{RT}::trace::Pos] = &[",
        *proj.__dict__.get("tb_rows", []),
        "];",
        "",
        f"pub fn router(_app: std::sync::Arc<{RT}::AppState>) -> axum::Router<std::sync::Arc<{RT}::AppState>> {{",
        "    axum::Router::new().fallback(|axum::extract::State(app): axum::extract::State<std::sync::Arc<"
        f"{RT}::AppState>>, req: axum::extract::Request| {RT}::asgi::app(app, req, ROUTES, WS_ROUTES, stack))",
        "}",
    ]
    (out_dir / "src").mkdir(parents=True, exist_ok=True)
    # the runtime, a library crate next to the application's (before 0.9: the module src/dynrt/)
    for rt_dir in (out_dir / "dynrt", out_dir / "src" / "dynrt"):
        if rt_dir.exists():
            shutil.rmtree(rt_dir)
    # copy2 keeps the files' times: cargo then finds an already compiled runtime fresh (a shared target directory,
    # the Docker image's precompiled one)
    shutil.copytree(RUNTIME_DIR, out_dir / "dynrt", ignore=shutil.ignore_patterns("target", "*.orig", "*.rej"))
    # always declared (already in the dependency graph): the shipped Cargo.lock lists the root crate's
    # dependencies, so a crate with GZip would otherwise fail `cargo build --locked`
    extra = 'tower-http = { version = "0.6", features = ["compression-gzip"] }\n'
    layers = ""
    if gzip:
        layers = (f"\n        .layer(tower_http::compression::CompressionLayer::new()"
                  f".quality(tower_http::compression::CompressionLevel::Precise({gzip.compresslevel}))"
                  f".compress_when(tower_http::compression::predicate::Predicate::and("
                  f"tower_http::compression::predicate::SizeAbove::new({gzip.minimum_size}), "
                  f"tower_http::compression::predicate::NotForContentType::GRPC)))"
                  f"\n        .layer(axum::middleware::map_response(dynrt::web::starlette_vary))")
    (out_dir / "Cargo.toml").write_text(CARGO.format(name=crate_name, source=source, extra=extra, deps=runtime_dependencies()))
    # the dependency versions the runtime is tested with (an existing lock file is kept)
    lock = Path(__file__).parent / "runtime" / "Cargo.lock"
    out_lock = out_dir / "Cargo.lock"
    # a lock file written before 0.9 (runtime copied as a module) lacks the runtime's crate: `--locked` would refuse it
    if lock.exists() and (not out_lock.exists() or '\nname = "dynrt"\n' not in out_lock.read_text()):
        out_lock.write_text(lock.read_text().replace('name = "dynapp_axum"', f'name = "{crate_name}"'))
    commit, expire, autoflush, sync = getattr(proj, "session_cfg", (False, proj.expire_on_commit, True, False))
    b = lambda x: str(x).lower()  # noqa: E731
    pyver = target_python(fe.index.root)
    sa_ver = tuple(int(x) for x in re.findall(r"\d+", locked_version(fe.index.root, "sqlalchemy") or _installed("sqlalchemy") or "2.1")[:2])
    fa_ver = (tuple(int(x) for x in re.findall(r"\d+", locked_version(fe.index.root, "fastapi") or "0.142.3")[:3]) + (0, 0, 0))[:3]
    (out_dir / "src" / "main.rs").write_text(MAIN.format(expire=b(expire), autoflush=b(autoflush), commit=b(commit), sync=b(sync), layers=layers,
                                                         pymajor=pyver[0], pyminor=pyver[1], pypatch=target_python_patch(fe.index.root),
                                                         pyfeatures=", ".join(f"({rs(k)}, {str(v).lower()})" for k, v in python_features(fe.index.root).items()),
                                                         strict_ct=b(fe.__dict__.get("strict_ct", fastapi_strict_content_type(fe.index.root))),
                                                         root_path=rs(root_path_checked(fe, ws_routes)),
                                                         starlette_major=starlette_version(proj)[0], starlette_minor=starlette_version(proj)[1],
                                                         dump_json=b(fastapi_at_least(fe.index.root, (0, 130))),
                                                         pydantic=".".join((locked_version(fe.index.root, "pydantic") or "2.13").split(".")[:2]),
                                                          sentry_sdk=locked_version(fe.index.root, "sentry-sdk") or "2.71.0",
                                                          stripe=libmap.LIB_VERSIONS.get("stripe", "14.4.1"),
                                                          multipart=b(locked_version(fe.index.root, "python-multipart") is not None),
                                                          python_side=", ".join(rs_marked(x) for x in sorted({x for x in fe.__dict__.get("python_side", ()) if x.startswith("/")}
                                                                                                        | fe.__dict__.get("python_side_raw", set()))),
                                                          mount_fallback=", ".join(rs(x) for x in sorted(set(fe.__dict__.get("mount_fallback", [])))),
                                                          source_root=rs(str(Path(fe.index.root).resolve())),
                                                          loguru_init=loguru_init(proj),
                                                          sa_major=sa_ver[0], sa_minor=sa_ver[1], fa_ver=", ".join(map(str, fa_ver))))
    (out_dir / "src" / "gen.rs").write_text("\n".join(gen) + "\n")
    if shutil.which("rustfmt"):
        subprocess.run(["rustfmt", "--edition", "2021", str(out_dir / "src" / "gen.rs"), str(out_dir / "src" / "main.rs")], check=False,
                       capture_output=True)
    return proj


def loguru_init(proj) -> str:
    """`import loguru` in the binary: its default handler (the default format, compiled here), when translated code
    calls the logger"""
    if not proj.__dict__.get("loguru"):
        return ""
    from . import logurufmt
    return f"dynrt::loguru::init({logurufmt.compile_format(logurufmt.DEFAULT_FORMAT)});"


# keyword arguments the runtime implements for these methods (others raise TypeError at run time)
# the strftime directives dynrt::libs::strftime renders (%:z aside); keep in step with it
STRFTIME_DIRECTIVES = "aAbBcCdDeFfgGhHIjklmMnpPrRStTuUVwWxXyYzZ%"
STRFTIME_PREFIXES = "-_0^#EO:123456789"

METHOD_KWARGS = {
    "model_dump": {"mode", "exclude_none", "exclude_unset", "exclude_defaults", "by_alias", "exclude", "include", "context"},
    "model_dump_json": {"exclude_none", "exclude_unset", "exclude_defaults", "by_alias", "exclude", "include", "context"},
    "model_copy": {"update", "deep"},
    # itsdangerous.URLSafeTimedSerializer (dynrt/itsd.rs): the only runtime type with these methods
    "loads": {"max_age", "return_timestamp", "salt"},
    "dumps": {"salt"},
    # each name below: the union over every runtime type implementing it (audit of 09/10/2026)
    "encode": {"encoding", "errors"},
    "decode": {"encoding", "errors"},
    "read_text": {"encoding", "errors"},
    "write_text": {"data", "encoding", "errors", "newline"},
    "write_bytes": set(),
    "read_bytes": set(),
    "set_cookie": {"key", "value", "max_age", "expires", "path", "domain", "secure", "httponly", "samesite"},
    "delete_cookie": {"key", "path", "domain", "secure", "httponly", "samesite"},
    "encrypt": set(),
    "decrypt": set(),
    "scan_iter": {"match", "count", "_type"},
    "model_validate": {"from_attributes"},
    "model_validate_json": set(),
    "find": set(), "rfind": set(), "rindex": set(),
    # email.message.EmailMessage (dynrt/emailmsg.rs); MIMEText.as_string() takes none (refused at run time)
    "set_content": {"subtype", "charset", "cte"},
    "as_bytes": {"unixfrom", "policy"},
    "as_string": {"unixfrom", "maxheaderlen", "policy"},
    # dns.resolver.Resolver.resolve (dynrt/dnsres.rs) and Path.resolve(strict=)
    "resolve": {"rdtype", "rdclass", "tcp", "raise_on_no_answer", "lifetime", "search", "strict"},
    "use_edns": {"edns", "ednsflags", "payload", "options"},
    "to_text": {"omit_final_dot"},
    "groups": {"default"},
    "groupdict": {"default"},
    "search": {"string", "pos", "endpos"}, "fullmatch": {"string"}, "findall": {"string"}, "finditer": {"string"},
    # window functions (dynrt/orm.rs): frames (rows=, range_=, groups=) not rendered
    "over": {"partition_by", "order_by"},
    # recursive= and nesting= not rendered
    "cte": {"name"}, "lateral": {"name"}, "correlate": set(), "correlate_except": set(),
}
# positional arguments the runtime implements for these methods (same union)
METHOD_MAX_ARGS = {"loads": 4, "dumps": 2, "encode": 2, "decode": 2, "read_text": 2, "write_text": 4, "write_bytes": 1,
                   "read_bytes": 0, "set_cookie": 9, "delete_cookie": 6, "encrypt": 1, "decrypt": 1, "scan_iter": 3,
                   "model_validate": 1, "model_validate_json": 1, "find": 3, "rfind": 3, "index": 3, "rindex": 3,
                   "count": 3, "startswith": 3, "endswith": 3, "groups": 1, "groupdict": 1, "search": 3,
                   "fullmatch": 1, "findall": 1, "finditer": 1, "over": 0, "cte": 1, "lateral": 1}

# calls whose string arguments or keywords name attributes of the objects they make or change
ATTR_NAMING_CALLS = {"label", "setattr", "getattr", "hasattr", "namedtuple", "NamedTuple", "make_dataclass",
                     "SimpleNamespace", "column", "literal_column", "alias", "type", "Column", "mapped_column"}

_RUNTIME_NAMES: set[str] | None = None
_RUNTIME_ATTRS: set[str] | None = None


def runtime_attr_names() -> set[str]:
    """The names the runtime dispatches on (`runtime_names`), compares to (`name == "x"`), keeps in a table
    (`("x", value)`, `&["x", "y"]`), declares as a field (`name: "x"`) or sets on an instance
    (`attrs.insert("x".into(), ..)`: an exception's `colno`, `index`...; `attrs.push(("x".into(), ..))`, `setattr(o,
    "x", ..)`): over-approximates the attributes some runtime value provides, so a name outside it is certainly
    missing."""
    global _RUNTIME_ATTRS
    if _RUNTIME_ATTRS is None:
        names, ident = set(runtime_names()), r'"([A-Za-z_][A-Za-z0-9_]*)"'
        # OAuth2PasswordRequestForm's attributes: its form fields, listed by the generator (`libs::oauth2_form`)
        names |= {"grant_type", "username", "password", "scopes", "client_id", "client_secret"}
        for f in (Path(__file__).parent / "runtime" / "dynrt").glob("*.rs"):
            src = f.read_text()
            for pat in (rf"==\s*{ident}", rf"{ident}\s*==", rf"\(\s*{ident}\s*,", rf"\bname:\s*{ident}",
                        rf"\binsert\(\s*{ident}\s*\.(?:into|to_string|to_owned)\(\)",
                        # `attrs.push(("__wrapped__".into(), f))`, `kw.push(("scopes".to_string(), v))`
                        rf"\(\s*{ident}\s*\.(?:into|to_string|to_owned)\(\)\s*,",
                        rf"\bsetattr\([^,()]*,\s*{ident}"):
                names |= set(re.findall(pat, src))
            for table in re.findall(r'&\[((?:\s*"[A-Za-z_][A-Za-z0-9_]*"\s*,?)+)\s*\]', src):
                names |= set(re.findall(ident, table))
        _RUNTIME_ATTRS = names
    return _RUNTIME_ATTRS


@functools.lru_cache(maxsize=1)
def migrations_item(source: str, root) -> str:
    """`gen::MIGRATIONS`: the project's alembic revisions rendered for `<binary> migrate` (py2axum/migrations.py);
    without one, why (a migration that cannot be translated never blocks the routes)."""
    from . import migrations
    try:
        plan = migrations.collect(Path(source), Path(root))
        why = "this binary has no migrations (no alembic.ini at the package or above when it was generated)"
    except TranspileError as e:
        plan, why = None, f"the migrations were not translated: {e}"
    if plan is None:
        return (f"pub static MIGRATIONS: Option<&'static {RT}::migrate::Plan> = None;\n"
                f"pub static MIGRATIONS_WHY: &str = {rs(why)};")
    steps = ", ".join(
        f"{RT}::migrate::Step {{ revision: {rs(st.revision)}, log: {rs(st.log)}, "
        f"heads_after: &[{', '.join(rs(h) for h in st.heads_after)}], stmts: &[{', '.join(rs(x) for x in st.stmts)}], "
        f"refused: {('Some(' + rs(st.refused) + ')') if st.refused else 'None'} }}" for st in plan.steps)
    log = ("Some(" + rs(json.dumps(plan.logging)) + ")") if plan.logging else "None"
    return (f"/// the alembic revisions of {Path(plan.ini).name}, rendered by alembic's offline mode\n"
            f"pub static MIGRATIONS: Option<&'static {RT}::migrate::Plan> = Some(&{RT}::migrate::Plan {{ "
            f"ini: {rs(plan.ini)}, version_table: {rs(plan.version_table)}, "
            f"version_schema: {('Some(' + rs(plan.version_schema) + ')') if plan.version_schema else 'None'}, "
            f"per_migration: {str(plan.per_migration).lower()}, create_version: {rs(plan.create_version)}, "
            f"steps: &[{steps}], sql: {rs(plan.sql)}, logging: {log} }});\n"
            f"pub static MIGRATIONS_WHY: &str = \"\";")


@functools.lru_cache(maxsize=4)
def unassigned_later(python: tuple[int, int]) -> frozenset[int]:
    """The code points the translating Python's Unicode assigned after the project's Python's (`ucdages.py`):
    unassigned (Cn) for the project, whatever the translating Python's `unicodedata` says"""
    import sys

    from . import ucdages

    mine = sys.version_info[:2]
    return frozenset(cp for v, rs in ucdages.ADDED.items() if python < v <= mine for lo, hi in rs
                     for cp in range(lo, hi + 1))


@functools.lru_cache(maxsize=4)
def str_classes(python: tuple[int, int]) -> str:
    """`str.isdigit/isdecimal/isnumeric/isalpha/isspace/islower/isupper/isprintable` (and the titlecase letters) of the
    project's Python (the translating Python's tables, less the characters assigned after the project's
    Unicode), as code point ranges: Rust's
    char predicates follow other Unicode properties ('¾' is numeric there, not a digit in Python; U+001C is
    a Python space)."""
    import unicodedata

    later = unassigned_later(python)

    def ranges(pred) -> str:
        out, start = [], None
        for cp in range(0x110001):
            ok = cp <= 0x10FFFF and not 0xD800 <= cp <= 0xDFFF and cp not in later and pred(chr(cp))
            if ok and start is None:
                start = cp
            elif not ok and start is not None:
                out.append(f"({start:#x}, {cp - 1:#x})")
                start = None
        return "&[" + ", ".join(out) + "]"
    fields = ", ".join(f"{k}: {ranges(getattr(str, 'is' + k))}" for k in ("digit", "decimal", "numeric", "alpha", "space", "lower", "upper",
                                                                                     "printable"))
    fields += f", title: {ranges(lambda c: unicodedata.category(c) == 'Lt')}"
    return (f"/// Unicode {unicode_of(python)} (the project's Python): the str.is*() classes\n"
            f"pub static STR_CLASSES: {RT}::methods::StrClasses = {RT}::methods::StrClasses {{ {fields} }};")


def unicode_of(python: tuple[int, int]) -> str:
    import unicodedata

    from . import ucdages

    return ucdages.UNICODE.get(python, unicodedata.unidata_version)


def ucd_tables(items: list[str]) -> str:
    """The unicodedata tables the generated code reads (category, bidirectional, name), from the translating
    Python (libmap._ucd_same checked its Unicode version is the project's): only those referenced."""
    import re
    import unicodedata

    used = {n for n in ("UCD_CATEGORY", "UCD_BIDI", "UCD_NAMES", "UCD_DECOMP") if any(f"self::{n}" in it for it in items)}
    out = []

    def runs(f, skip):
        r, start, cur = [], None, None
        for cp in range(0x110001):
            v = f(chr(cp)) if cp <= 0x10FFFF else None
            if v != cur:
                if cur is not None and cur != skip:
                    r.append(f"({start:#x}, {cp - 1:#x}, {rs(cur)})")
                start, cur = cp, v
        return ", ".join(r)

    if "UCD_CATEGORY" in used:
        out.append(f"pub static UCD_CATEGORY: &[(u32, u32, &str)] = &[{runs(unicodedata.category, 'Cn')}];")
    if "UCD_BIDI" in used:
        out.append(f"pub static UCD_BIDI: &[(u32, u32, &str)] = &[{runs(unicodedata.bidirectional, '')}];")
    if "UCD_DECOMP" in used:
        pairs = [f"({cp:#x}, {rs(d)})" for cp in range(0x110000) if (d := unicodedata.decomposition(chr(cp)))]
        out.append(f"pub static UCD_DECOMP: &[(u32, &str)] = &[{', '.join(pairs)}];")
    if "UCD_NAMES" in used:
        cps, offs, text, hexes, start, pre = [], [0], [], [], None, None
        for cp in range(0x110001):
            nm = unicodedata.name(chr(cp), None) if cp <= 0x10FFFF else None
            m = re.fullmatch(r"(.*-)([0-9A-F]{4,6})", nm) if nm else None
            p = m.group(1) if m and int(m.group(2), 16) == cp and len(m.group(2)) == max(4, len(f"{cp:X}")) else None
            if p != pre:
                if pre is not None:
                    hexes.append(f"({start:#x}, {cp - 1:#x}, {rs(pre)})")
                start, pre = cp, p
            if nm and p is None:
                cps.append(str(cp))
                text.append(nm)
                offs.append(str(int(offs[-1]) + len(nm)))
        out.append(f"pub static UCD_NAMES: {RT}::stdlib::UcdNames = {RT}::stdlib::UcdNames {{ cps: &[{', '.join(cps)}], "
                   f"offs: &[{', '.join(map(str, offs))}], text: {rs(''.join(text))}, hex: &[{', '.join(hexes)}] }};")
    return "\n".join(out)


def ucd_unassigned(python: tuple[int, int]) -> str:
    """The code points unassigned in the project's Python's `unicodedata` (category Cn: the translating
    Python's, plus what a later Unicode assigned, `unassigned_later`): it leaves them alone, while the
    runtime's tables (a newer Unicode) may decompose or class them. Assigned characters never change their
    decomposition or combining class (Unicode stability policy), so these ranges are the whole difference."""
    import unicodedata

    later = unassigned_later(python)
    ranges, start = [], None
    for cp in range(0x110001):
        cn = cp <= 0x10FFFF and (unicodedata.category(chr(cp)) == "Cn" or cp in later)
        if cn and start is None:
            start = cp
        elif not cn and start is not None:
            ranges.append(f"({start:#x}, {cp - 1:#x})")
            start = None
    return (f"/// Unicode {unicode_of(python)} (the project's Python): unassigned ranges\n"
            f"pub static UCD_UNASSIGNED: &[(u32, u32)] = &[{', '.join(ranges)}];")


# builtins whose runtime function reads the keyword arguments itself (the others get positional ones only);
# print's (sep=, end=, file=, flush=) only shape the server's stdout, never a response
KWARG_BUILTINS = {"dict", "bytes", "open", "sorted", "min", "max", "print", "zip"}
# keyword arguments CPython also takes positionally, after the single first argument
POSITIONAL_KWARGS = {"enumerate": ("start",), "sum": ("start",), "round": ("ndigits",), "int": ("base",)}


def runtime_names() -> set[str]:
    """Every name the runtime dispatches on (match arms of dynrt/*.rs, names in libmap): over-approximates
    the methods some runtime type implements, so a name outside it is certainly missing."""
    global _RUNTIME_NAMES
    if _RUNTIME_NAMES is None:
        names = set()
        for f in (Path(__file__).parent / "runtime" / "dynrt").glob("*.rs"):
            # `"x" =>`, `"x" | "y"`, `"x" if ..`, and inside a tuple pattern: `(Kind::A, "x") =>`, `(Kind::A, "x") | (..)`, `(.., "x") if ..`
            names |= set(re.findall(r'"([A-Za-z_][A-Za-z0-9_]*)"(?=\s*(?:\||=>|if\b|\)\s*(?:=>|\||if\b)))', f.read_text()))
        # libmap's quoted names (keywords, attributes...), less the bare names of library functions and values
        # (`_bind('compile', ...)` of `re.compile`): a library function is no evidence that a runtime value has
        # a method of that name (`stmt.compile()` failed only at run time)
        lib = set(re.findall(r"""["']([A-Za-z_][A-Za-z0-9_]*)["']""", Path(libmap.__file__).read_text()))
        lib -= {k.rsplit(".", 1)[-1] for k in (*libmap.CALLS, *libmap.VALUES)} - names
        names |= lib
        _RUNTIME_NAMES = names
    return _RUNTIME_NAMES


def poly_fields(p: Project, info) -> str:
    """Single table inheritance fields of a ModelDesc: parent mapper, discriminator column, identity, descendants."""
    if not isinstance(info, ModelInfo) or info.poly_on is None:
        return "parent: None, poly_on: None, poly_load: false, identity: None, subs: &[], joins: &[]"

    def ancestors(i):
        while i.parent is not None:
            i = p.models[i.parent]
            yield i.sym

    subs = [m.rust for m in p.models.values() if isinstance(m, ModelInfo) and info.sym in set(ancestors(m))]
    pi = next(k for k, c in enumerate(info.cols) if c["name"] == info.poly_on)
    parent = f"Some(&{p.models[info.parent].rust})" if info.parent else "None"
    ident_ = f"Some({rs(info.identity)})" if info.identity is not None else "None"
    joins = ", ".join(f"({rs(t)}, {rs(c)}, {rs(pt)}, {rs(pc)})" for t, c, pt, pc in info.joins)
    return (f"parent: {parent}, poly_on: Some({pi}), poly_load: {str(info.poly_load).lower()}, identity: {ident_}, "
            f"subs: &[{', '.join('&' + r for r in subs)}], "
            f"joins: &[{joins}]")


def hybrids_of(p: Project, info) -> str:
    """`HybridDesc`s of a mapped class (`sqlalchemy.ext.hybrid`): instance function, class-level expression, setter."""
    out = []
    for h in info.__dict__.get("hybrids", {}).values():
        opt = lambda s: f"Some({method_wrapper(p, s)} as {RT}::pyd::MethodFn)" if s else "None"  # noqa: E731
        out.append(f"{RT}::orm::HybridDesc {{ name: {rs(h['name'])}, method: {str(h['method']).lower()}, "
                   f"get: {method_wrapper(p, h['get'])} as {RT}::pyd::MethodFn, expr: {opt(h['expr'])}, set: {opt(h['set'])} }}")
    return ", ".join(out)


def const_int(node: ast.AST, env: dict[str, int]) -> int | None:
    """The value of an integer expression of literals and of the names in `env`, else None. A power or shift
    whose result would be huge stops at a bound past 64 bits (never computed in full)."""
    if isinstance(node, ast.Constant):
        return node.value if type(node.value) is int else None
    if isinstance(node, ast.Name):
        return env.get(node.id)
    if isinstance(node, ast.UnaryOp) and isinstance(node.op, (ast.USub, ast.UAdd, ast.Invert)):
        v = const_int(node.operand, env)
        return None if v is None else {ast.USub: -v, ast.UAdd: v, ast.Invert: ~v}[type(node.op)]
    if not isinstance(node, ast.BinOp):
        return None
    a, b = const_int(node.left, env), const_int(node.right, env)
    if a is None or b is None:
        return None
    big = 2**64
    match node.op:
        case ast.Add():
            return a + b
        case ast.Sub():
            return a - b
        case ast.Mult():
            return a * b
        case ast.Pow():
            if b < 0:
                return None  # a float
            if abs(a) > 1 and b > 64:
                return big if a > 0 or b % 2 == 0 else -big
            return a ** b
        case ast.LShift():
            if b < 0:
                return None  # ValueError
            return (big if a > 0 else -big) if a and b > 64 else a << b
        case ast.FloorDiv() | ast.Mod():
            if b == 0:
                return None
            return a // b if isinstance(node.op, ast.FloorDiv) else a % b
        case ast.BitOr():
            return a | b
        case ast.BitAnd():
            return a & b
        case ast.BitXor():
            return a ^ b
    return None


def class_level_methods(p: Project, methods, kinds=frozenset({"staticmethod", "classmethod"})) -> list[str]:
    """The `@staticmethod`/`@classmethod`s of a class (or only those of `kinds`): the methods read on the class
    itself."""
    out = []
    for n, _prop, sym in methods:
        decos = {((dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "").split(".")[-1]
                 for d in p.fn_node(sym).decorator_list}
        if decos & kinds:
            out.append(n)
    return out


def _source_order(e: TranspileError):
    return (e.file or "", getattr(e.node, "lineno", 0) or 0, getattr(e.node, "col_offset", 0) or 0, e.msg)


def scoped(e: TranspileError, scope: str) -> TranspileError:
    """Mark a global error with the part of the application it blocks (`"lifespan"`): an unmarked one blocks the
    whole application (its middleware stack, exception handlers, module code)."""
    e.__dict__.setdefault("scope", scope)
    return e


def closure_errors_of(p: Project, starts) -> list[TranspileError]:
    """The errors reachable from a set of nodes (the middleware stack, the lifespan...), once each, in source
    order: the set is iterated in hash order."""
    seen, out = set(), []
    for k in starts:
        for e in closure_errors(p, k):
            if id(e) not in seen:
                seen.add(id(e))
                out.append(e)
    return sorted(out, key=_source_order)


def closure_errors(p: Project, start) -> list[TranspileError]:
    """Errors of every function reachable from a route (endpoint, dependencies, callees), in a fixed order: the
    closest to the route first (breadth first), then source order (file, line, message) at the same distance.
    The edges are sets, iterated in hash order: walking them as they come changed the first reason of `check`
    from one run to the next (PYTHONHASHSEED)."""
    seen, level, out = {start}, [start], []
    while level:
        errs = []
        for n in level:
            if n in p.fn_errors:
                errs.append(p.fn_errors[n])
            if isinstance(n, tuple) and n[0] == "class" and n[1] in p.class_errors:
                errs.append(p.class_errors[n[1]])
        out += sorted(errs, key=_source_order)
        nxt = []
        for n in level:
            for m in p.edges.get(n, ()):
                if m not in seen:
                    seen.add(m)
                    nxt.append(m)
        level = nxt
    return out

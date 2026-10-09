"""The compiler: a FastAPI project to Rust code operating on dynamic Python values.

Real projects (three layers, services, dynamic attribute access...) are compiled as written:
every project function reachable from the routes becomes an `async fn(cx, V...) -> R<V>`, Python
semantics come from `runtime/dynrt` (values, Pydantic, SQLAlchemy session, FastAPI plumbing), and
library calls go through the closed list of `libmap.py`. No Python interpreter is involved.

Unsupported constructs raise `TranspileError` with file:line, as everywhere else.
"""
from __future__ import annotations

import ast
import copy
import functools
import json
import re
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

from . import libmap
from .frontend import Frontend, Mount, dotted, literal
from .ir import TranspileError
from .modules import Ext, ModRef, Sym, import_guard, is_ext, startup_skipped, stmt_bindings

RT = "crate::dynrt"
RUNTIME_DIR = Path(__file__).parent / "runtime" / "dynrt"

BUILTIN_FUNCS = {
    "len", "str", "repr", "int", "float", "bool", "list", "tuple", "set", "frozenset", "dict", "sorted",
    "reversed", "min", "max", "sum", "any", "all", "next", "round", "abs", "enumerate", "zip", "range",
    "print", "isinstance", "getattr", "setattr", "hasattr", "iter", "open", "type", "chr", "ord", "divmod", "callable",
    "hash", "id", "filter", "map", "issubclass", "vars", "anext",
}
BUILTIN_TYPES = {"str", "int", "float", "bool", "dict", "list", "tuple", "set", "frozenset", "bytes", "object"}


_RS_SPECIAL = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\ud800-\udfff]")


def rs(s: str) -> str:
    """A Rust string literal (JSON's escapes are Rust's, but for control characters: `\\u{1c}`, not `\\u001c`)."""
    if not _RS_SPECIAL.search(s):
        return json.dumps(s, ensure_ascii=False)
    out = []
    for ch in s:
        o = ord(ch)
        if 0xD800 <= o <= 0xDFFF:
            raise ValueError(f"a lone surrogate (\\u{o:04x}) cannot be held in a Rust string: {s[:40]!r}")
        out.append(json.dumps(ch, ensure_ascii=False)[1:-1] if o >= 0x20 or ch in "\n\r\t" else f"\\u{{{o:x}}}")
    return '"' + "".join(out) + '"'


def rs_marked(s: str) -> str:
    """A Rust string literal that may hold runtime prefix markers (`\\x01<index>\\x01`)."""
    return rs(s).replace("\\u0001", "\\u{1}")


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
        nodes = [stmt, *walk_scope(stmt)]
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
        self.deps: dict[Sym, str] = {}
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

    def project_attrs(self) -> set[str]:
        """Names a project object may carry: methods and class attributes of every class, attributes
        assigned anywhere (`obj.cb = f`), columns and fields included."""
        if self.__dict__.get("_attrs") is None:
            names = set()
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
                     if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute) and n.func.attr == "model_json_schema"]
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

    def use_session_dep(self, sym: Sym) -> None:
        """The `AsyncSession` dependency of a route: its shape fixes the session options and whether
        the request commits after the endpoint. One configuration per application."""
        cfg = self.session_config(sym)
        cfgs = self.__dict__.setdefault("session_cfgs", {})
        cfgs[sym] = cfg
        if len(set(cfgs.values())) > 1:
            fn = self.ix.definition(sym)
            raise self.err(f"session dependency {sym.name}: the routes use session dependencies with different "
                           f"behaviour ({', '.join(s.name for s in cfgs)})", fn, sym.module)
        self.session_cfg = cfg

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
        if (len(body) == 1 and isinstance(body[0], ast.Try) and not body[0].handlers and not body[0].orelse
                and len(body[0].body) == 1 and isinstance(body[0].body[0], with_t)):
            # try: async with maker() as s: yield s / finally: await s.aclose()
            outer_finally = body[0].finalbody
            body = body[0].body
        if (sync and len(body) == 2 and isinstance(body[0], ast.Assign) and len(body[0].targets) == 1
                and isinstance(body[0].targets[0], ast.Name) and isinstance(body[1], ast.Try)):
            # s = maker() / try: yield s / finally: s.close()
            maker_call, sname, inner = body[0].value, body[0].targets[0].id, [body[1]]
            if not body[1].finalbody:
                raise bad(body[1])
        elif len(body) == 1 and isinstance(body[0], with_t) and len(body[0].items) == 1:
            item = body[0].items[0]
            if not isinstance(item.optional_vars, ast.Name):
                raise bad(item.context_expr)
            maker_call, sname, inner = item.context_expr, item.optional_vars.id, body[0].body
        else:
            raise bad(fn)
        if not (isinstance(maker_call, ast.Call) and not maker_call.args and not maker_call.keywords):
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

        def is_yield(stmt):
            return (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Yield)
                    and isinstance(stmt.value.value, ast.Name) and stmt.value.value.id == sname)

        commit = False
        if len(inner) == 1 and is_yield(inner[0]):
            pass
        elif len(inner) == 1 and isinstance(inner[0], ast.Try) and not inner[0].orelse:
            t = inner[0]
            if not (t.body and is_yield(t.body[0]) and all(is_call(x, "commit") for x in t.body[1:]) and len(t.body) <= 2):
                raise bad(t)
            commit = len(t.body) == 2
            for h in t.handlers:
                if not (len(h.body) == 2 and is_call(h.body[0], "rollback") and isinstance(h.body[1], ast.Raise) and h.body[1].exc is None):
                    raise bad(h)
            if not all(is_call(x, "close") for x in t.finalbody):
                raise bad(t)
        else:
            raise bad(inner[0] if inner else fn)
        if not all(is_call(x, "close") for x in outer_finally):
            raise bad(fn)
        # the sessionmaker
        # the sessionmaker: a module constant, or a factory function creating it once (`get_factory()()`)
        makers = {"async_sessionmaker", "sessionmaker"}
        f = maker_call.func
        maker = None
        if isinstance(f, ast.Call) and not f.args and not f.keywords:
            fs = self.resolve(module, f.func)
            fdef = self.ix.definition(fs) if isinstance(fs, Sym) else None
            if isinstance(fdef, ast.FunctionDef):
                calls = [n for n in ast.walk(fdef) if isinstance(n, ast.Call) and (dotted(n.func) or "").split(".")[-1] in makers]
                if len(calls) == 1:
                    maker, mk = calls[0], fs
        else:
            mk = self.resolve(module, f)
            mdef = self.ix.definition(mk) if isinstance(mk, Sym) else None
            if isinstance(mdef, (ast.Assign, ast.AnnAssign)) and isinstance(mdef.value, ast.Call) \
                    and (dotted(mdef.value.func) or "").split(".")[-1] in makers:
                maker = mdef.value
            elif isinstance(mdef, (ast.Assign, ast.AnnAssign)) and isinstance(mdef.value, ast.Call):
                cls = self.resolve(mk.module, mdef.value.func)
                if isinstance(cls, Sym) and isinstance(self.ix.definition(cls), ast.ClassDef):
                    if sync:
                        raise self.err("session maker class: only async_sessionmaker wrappers are supported", mdef.value, mk.module)
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

    def expand_alias(self, ann: ast.AST, module: str, scope=None) -> tuple[ast.AST, str]:
        """`DbDep = Annotated[AsyncSession, Depends(get_db)]` used as `db: DbDep` -> the aliased type."""
        for _ in range(10):
            if not isinstance(ann, (ast.Name, ast.Attribute)):
                return ann, module
            t = self.ix.resolve_expr(module, ann, scope)
            if not isinstance(t, Sym):
                return ann, module
            d = self.ix.definition(t)
            if isinstance(d, ast.Assign) and isinstance(d.value, (ast.Subscript, ast.BinOp)):
                ann, module, scope = d.value, t.module, None
            elif isinstance(d, ast.AnnAssign) and d.value is not None and (dotted(d.annotation) or "").endswith("TypeAlias"):
                ann, module, scope = d.value, t.module, None
            elif isinstance(d, ast.TypeAlias):
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

    # ---------------------------------------------------------------- types -> TD

    def td(self, ann: ast.AST, module: str, cons: dict | None = None, scope=None) -> str:
        """Pydantic type annotation -> name of a `static TD`."""
        return self._static_td(self._td_init(ann, module, cons or {}, scope))

    def _static_td(self, init: str) -> str:
        if init not in self.tds:
            self.tds[init] = f"TD_{len(self.tds) + 1}"
        return self.tds[init]

    def pat(self, src: str) -> str:
        if src not in self.pats:
            self.pats[src] = f"PAT_{len(self.pats) + 1}"
        return self.pats[src]

    def _num(self, cons: dict) -> str:
        def o(k):
            return f"Some({float(cons[k])!r}f64)" if k in cons else "None"
        return f"{RT}::pyd::NumC {{ ge: {o('ge')}, gt: {o('gt')}, le: {o('le')}, lt: {o('lt')} }}"

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
        mn = f"Some({cons['min_length']})" if "min_length" in cons else "None"
        mx = f"Some({cons['max_length']})" if "max_length" in cons else "None"
        p = f"Some(&{self.pat(cons['pattern'])})" if "pattern" in cons else "None"
        return (f"{RT}::pyd::StrC {{ min: {mn}, max: {mx}, pattern: {p}, strip: {str(bool(cons.get('_strip'))).lower()}, "
                f"lower: {str(bool(cons.get('_lower'))).lower()}, upper: {str(bool(cons.get('_upper'))).lower()} }}")

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
                    if isinstance(meta, ast.Call) and (dotted(meta.func) or "").split(".")[-1] in {"Field", "Query", "Path", "Body", "Header"}:
                        cons.update(self.field_cons(meta, module, scope)[0])
                return self._td_init(args[0], module, cons, scope)
            if last == "Iterable" and {"min_length", "max_length"} & set(cons):
                # pydantic validates an Iterable lazily (a generator): its length errors are raised on iteration
                # ("Generator should have..."), and without an item validator only since pydantic 2.14
                raise self.err(f"length constraints on `{ast.unparse(ann)}` are not supported", ann, module)
            if last in {"list", "List", "Sequence", "Iterable"}:
                return self._sized(f"{RT}::pyd::TD::List(Some(&{self._static_td(self._td_init(args[0], module, {}, scope))}))", cons, ann, module)
            if last in {"set", "Set", "frozenset", "FrozenSet"}:
                return self._sized(f"{RT}::pyd::TD::Set(Some(&{self._static_td(self._td_init(args[0], module, {}, scope))}))", cons, ann, module)
            if last in {"tuple", "Tuple"}:
                if not (len(args) == 2 and isinstance(args[1], ast.Constant) and args[1].value is Ellipsis):
                    raise self.err(f"`{ast.unparse(ann)}`: only variable-length tuple[X, ...] is supported", ann, module)
                return self._sized(f"{RT}::pyd::TD::Tuple(Some(&{self._static_td(self._td_init(args[0], module, {}, scope))}))", cons, ann, module)
            if last in {"dict", "Dict", "Mapping"}:
                k = self._static_td(self._td_init(args[0], module, {}, scope))
                v = self._static_td(self._td_init(args[1], module, {}, scope))
                return self._sized(f"{RT}::pyd::TD::Dict(Some((&{k}, &{v})))", cons, ann, module)
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
                "typing.Any": f"{RT}::pyd::TD::Any",
                # the str schema under EmailStr's validator gets the str_* settings of model_config
                **dict.fromkeys(("pydantic.EmailStr", "pydantic.networks.EmailStr"), f"{RT}::pyd::TD::Email("
                                + self._str({k: v for k, v in cons.items() if k in ("_strip", "_lower", "_upper")}) + ")"),
                **{f"pydantic{m}.{n}": f"{RT}::pyd::TD::Url(&{RT}::pyd::{c})" for m in ("", ".networks")
                   for n, c in (("AnyUrl", "ANY_URL"), ("AnyHttpUrl", "ANY_HTTP_URL"), ("HttpUrl", "HTTP_URL"),
                                ("RedisDsn", "REDIS_DSN"))},
            }
            if t.dotted in mapping:
                return mapping[t.dotted]
            if t.dotted == "decimal.Decimal":
                return f"{RT}::pyd::TD::Decimal({self._dec(cons, ann, module)})"
            if t.package == "typing" and last in {"Dict", "List", "Set", "Tuple"}:
                return self._scalar(last.lower(), cons, ann, module)
            raise self.err(f"unsupported type annotation `{ast.unparse(ann)}` ({t.dotted})", ann, module)
        if isinstance(t, Sym):
            if t in self.fe.schema_syms:
                return f"{RT}::pyd::TD::Schema(&{self.schema(t).rust})"
            if self.enum_kind(t):
                return f"{RT}::pyd::TD::Enum(&{self.enum(t)[0]}, {str(bool(cons.get('_enum_values'))).lower()})"
            raise self.err(f"type `{t.qual}` is not a Pydantic model or an enum (other classes are not supported)", ann, module)
        raise self.err(f"unsupported type annotation `{ast.unparse(ann)}`", ann, module)

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
        tds = ", ".join(f"&{self._static_td(self._td_init(p, module, cons, scope))}" for p in rest)
        u = f"{RT}::pyd::TD::Union(&[{tds}])"
        self.__dict__.setdefault("union_src", {}).setdefault(u, (node, module))  # where a refusal points
        return f"{RT}::pyd::TD::Optional(&{self._static_td(u)})" if none else u

    def _scalar(self, name: str, cons: dict, ann: ast.AST, module: str) -> str:
        if name == "int":
            return f"{RT}::pyd::TD::Int({self._num(cons)})"
        if name == "float":
            return f"{RT}::pyd::TD::Float({self._num(cons)})"
        if name == "str":
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
        raise TranspileError(f"unsupported type {name}")

    FIELD_KW = {"ge", "gt", "le", "lt", "min_length", "max_length", "pattern", "regex", "max_digits", "decimal_places"}
    IGNORED_KW = {"description", "title", "examples", "example", "json_schema_extra", "deprecated", "include_in_schema"}

    def field_cons(self, call: ast.Call, module: str, scope=None) -> tuple[dict, dict]:
        """Field()/Query()/Path()/Body()/Header() -> (constraints, options)."""
        cons, opts = {}, {}
        if call.args:
            a0 = call.args[0]
            if not (isinstance(a0, ast.Constant) and a0.value is Ellipsis):
                opts["default"] = a0
        is_field = (dotted(call.func) or "").split(".")[-1] == "Field"
        for kw in call.keywords:
            if kw.arg in self.FIELD_KW:
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
            elif kw.arg in self.IGNORED_KW:
                pass
            else:
                raise self.err(f"unsupported option {kw.arg}= in {dotted(call.func)}()", kw, module)
        for k in ("min", "max"):
            if f"_{k}_items" in cons:
                cons.setdefault(f"{k}_length", cons.pop(f"_{k}_items"))
        return cons, opts

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
        code = fc.expr(expr_node)
        if mode == "once":
            g = f"G_{name}"
            self.items.append(
                f"static {g}: {RT}::Global = {RT}::Global::new();\n"
                f"fn {name}<'a>(cx: &'a Cx, _s: V, _a: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{ "
                f"{g}.get(cx, |cx| Box::pin(async move {{ Ok({code}) }})).await }}) }}"
            )
        else:
            self.items.append(
                f"fn {name}<'a>(cx: &'a Cx, _s: V, _a: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{ Ok({code}) }}) }}"
            )
        return name

    def default_spec(self, node: ast.AST, module: str, factory: bool) -> str:
        """`Dflt::...` variant text for a Pydantic field/param default."""
        if factory and isinstance(node, ast.Lambda) and (node.args.posonlyargs or node.args.args):
            # pydantic passes the validated data to a factory with one positional parameter (and, since 2.14,
            # reports `default_factory_not_called` after an error of another field)
            raise self.err("a default_factory taking the validated data (`lambda data: ...`) is not supported", node, module)
        try:
            return f"{'Factory' if factory else 'Value'}({self.dflt(node, module, factory=factory)})"
        except TranspileError:
            return f"Dyn({self.dyn_default(node, module, 'call' if factory else 'once')})"

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
                raise self.err(f"enum {sym.name}: subclassing an enum is not supported", b, sym.module)
            elif dotted(b) in {"str", "int"}:
                mixin = dotted(b)
        if base_kind is None:
            return None
        if base_kind == "unsupported":
            raise self.err(f"enum {sym.name}: Flag/IntFlag enums are not supported", node, sym.module)
        if base_kind == "Plain" and mixin:
            return "Str" if mixin == "str" else "Int"
        return base_kind

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
                if is_dunder(stmt.name):
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
        if sym in self.class_errors:
            raise self.class_errors[sym]
        if sym in self.models:
            return self.models[sym]
        try:
            return self._model(sym)
        except TranspileError as e:
            self.models.pop(sym, None)
            self.class_errors[sym] = e
            raise

    def _model(self, sym: Sym) -> ModelInfo:
        node = self.fe.model_syms[sym]
        module = sym.module
        base = f"{mod_ident(module)}__{ident(sym.name)}"
        info = ModelInfo(sym, f"MODEL_{base}", f"CLS_{base}", "", [], -1, [])
        self.models[sym] = info
        cols: list[dict] = []
        table = None
        classes = [(sym, node)]
        for b in node.bases:  # mixins: plain classes holding columns
            t = self.resolve(module, b)
            if isinstance(t, Sym) and t not in self.fe._declarative and t not in self.fe.model_syms:
                bn = self.ix.definition(t)
                if isinstance(bn, ast.ClassDef):
                    classes.insert(0, (t, bn))
            elif isinstance(t, Sym) and t in self.fe.model_syms:
                raise self.err(f"model {sym.name}: inheritance between mapped classes is not supported", b, module)
        for csym, cnode in classes:
            for stmt in cnode.body:
                if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
                    n = stmt.targets[0].id
                    if n == "__tablename__":
                        table = literal(stmt.value, self.src(csym.module))
                    elif n == "__abstract__":
                        raise self.err(f"model {sym.name}: abstract models are not supported", stmt, csym.module)
                    elif n == "__mapper_args__":
                        if not (isinstance(stmt.value, ast.Dict) and not stmt.value.keys):
                            raise self.err(f"model {sym.name}: __mapper_args__ is not supported (versioning, "
                                           "polymorphism and eager defaults change the SQL)", stmt, csym.module)
                    elif n == "__table_args__":
                        # constraints and indexes only matter to the DDL (Alembic/create_all); a schema does not
                        for x in ast.walk(stmt.value):
                            if isinstance(x, ast.Dict) and any(isinstance(k, ast.Constant) and k.value == "schema" for k in x.keys):
                                raise self.err(f"model {sym.name}: __table_args__ schema is not supported", stmt, csym.module)
                    elif isinstance(stmt.value, ast.Call) and (dotted(stmt.value.func) or "").split(".")[-1] in {"Column", "mapped_column", "deferred"}:
                        cols.append(self.column(n, None, stmt.value, csym.module, stmt))
                    elif isinstance(stmt.value, ast.Call) and (dotted(stmt.value.func) or "").split(".")[-1] == "relationship":
                        info.rels_raw.append((n, None, stmt.value, csym.module))
                    elif not n.startswith("__"):
                        raise self.err(f"model {sym.name}.{n}: only columns and relationships are supported in a "
                                       f"mapped class (`{ast.unparse(stmt.value)[:40]}`)", stmt, csym.module)
                elif isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
                    n = stmt.target.id
                    if isinstance(stmt.value, ast.Call) and (dotted(stmt.value.func) or "").split(".")[-1] == "relationship":
                        info.rels_raw.append((n, stmt.annotation, stmt.value, csym.module))
                        continue
                    if n.startswith("__"):
                        continue
                    if (dotted(stmt.annotation) or "").split(".")[-1] == "ClassVar" or (
                            isinstance(stmt.annotation, ast.Subscript) and (dotted(stmt.annotation.value) or "").split(".")[-1] == "ClassVar"):
                        raise self.err(f"model {sym.name}.{n}: class variables in a mapped class are not supported", stmt, csym.module)
                    cols.append(self.column(n, stmt.annotation, stmt.value, csym.module, stmt))
                elif isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    if is_dunder(stmt.name) and stmt.name not in {"__str__", "__repr__"}:
                        raise self.err(f"model {sym.name}: method {stmt.name} is not supported", stmt, csym.module)
                    for d in stmt.decorator_list:
                        if dotted(d) not in {"property", "staticmethod", "classmethod"}:
                            raise self.err(f"model {sym.name}.{stmt.name}: decorator @{ast.unparse(d)} is not supported "
                                           "(only @property, @staticmethod and @classmethod)", d, csym.module)
                    prop = any(dotted(d) == "property" for d in stmt.decorator_list)
                    info.methods.append((stmt.name, prop, Sym(csym.module, f"{cnode.name}.{stmt.name}")))
        if not table:
            raise self.err(f"model {sym.name} has no __tablename__", node, module)
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
        return info

    LAZY = {"select": "Select", "True": "Select", "selectin": "Selectin", "joined": "Selectin", "subquery": "Selectin",
            "immediate": "Selectin", "noload": "NoLoad", "None": "NoLoad", "raise": "Raise", "raise_on_sql": "Raise",
            "False": "Selectin"}
    REL_OPTIONS = {"back_populates", "backref", "foreign_keys", "lazy", "cascade", "uselist", "passive_deletes",
                   "overlaps", "argument", "doc", "info", "order_by", "remote_side"}

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
        for name, ann, call, module in raw:
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
            tinfo = self.model(tsym)
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
            lit = lambda k, d=None: literal(opts[k], self.src(module)) if k in opts else d  # noqa: E731
            lazy = str(lit("lazy", "select"))
            if lazy not in self.LAZY:
                raise self.err(f"relationship {info.sym.name}.{name}: lazy={lazy!r} is not supported", opts["lazy"], module)
            cascade = {c.strip() for c in str(lit("cascade", "save-update, merge")).split(",")}
            if "all" in cascade:
                cascade |= {"save-update", "merge", "refresh-expire", "expunge", "delete"}
            uselist = lit("uselist", not m2o)
            order = self.rel_order(opts["order_by"], tsym, tinfo, f"{info.sym.name}.{name}", module) if "order_by" in opts else []
            rels.append({"name": name, "target": tsym, "m2o": m2o, "local": li, "remote": ri, "uselist": bool(uselist), "order": order,
                         "lazy": self.LAZY[lazy], "back": lit("back_populates"), "delete": "delete" in cascade,
                         "orphan": "delete-orphan" in cascade, "passive_deletes": bool(lit("passive_deletes", False))})
            if "backref" in opts:
                b = opts["backref"]
                bname, bkw = (b.value, {}) if isinstance(b, ast.Constant) else (None, None)
                if isinstance(b, ast.Call) and (dotted(b.func) or "").split(".")[-1] == "backref" and b.args:
                    bname = literal(b.args[0], self.src(module))
                    bkw = {k.arg: literal(k.value, self.src(module)) for k in b.keywords}
                if not isinstance(bname, str) or set(bkw) - {"passive_deletes", "lazy", "cascade", "uselist"}:
                    raise self.err(f"relationship {info.sym.name}.{name}: unsupported backref=", b, module)
                bcascade = {c.strip() for c in str(bkw.get("cascade", "save-update, merge")).split(",")}
                if "all" in bcascade:
                    bcascade.add("delete")
                rels[-1]["back"] = bname
                trels = tinfo.rels if tinfo.rels is not None else tinfo.__dict__.setdefault("_pending_rels", [])
                trels.append({"name": bname, "target": info.sym, "m2o": not m2o, "local": ri, "remote": li,
                              "uselist": bool(bkw.get("uselist", m2o)), "lazy": self.LAZY[str(bkw.get("lazy", "select"))],
                              "back": name, "delete": "delete" in bcascade, "orphan": "delete-orphan" in bcascade,
                              "passive_deletes": bool(bkw.get("passive_deletes", False))})
        rels.extend(info.__dict__.pop("_pending_rels", []))

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
                        return self.column(cname, getattr(st, "annotation", None), st.value, sym.module, st)["ty"]
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

    def column(self, name: str, ann, call, module: str, stmt) -> dict:
        col = {"name": name, "ty": None, "nullable": None, "pk": False, "default": None, "server_default": False,
               "onupdate": None, "fk": None, "autoincrement": True, "deferred": False, "jsonb": False}
        if isinstance(call, ast.Call) and isinstance(self.resolve(module, call.func), Ext) and \
                self.resolve(module, call.func).dotted in {"sqlalchemy.orm.deferred", "sqlalchemy.orm.strategy_options.deferred"}:
            # `deferred(Column(...))`: left out of the entity's loaded state (read, then unloaded)
            if len(call.args) != 1 or call.keywords or not isinstance(call.args[0], ast.Call):
                raise self.err(f"column {name}: only deferred(Column(...)) is supported (no group, raiseload or "
                               "extra columns)", call, module)
            col["deferred"] = True
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
                elif pn in {"dict", "list"}:
                    col["ty"] = "Json"
                elif isinstance(t, Sym) and self.enum_kind(t):
                    col["ty"] = f"Enum(&{self.enum_col(t, False, t.name.lower())})"
        if call is not None:
            if not isinstance(call, ast.Call):
                raise self.err(f"column {name}: expected mapped_column(...)", stmt, module)
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
                elif kw.arg == "default":
                    col["default"] = self.col_default(kw.value, module, name)
                elif kw.arg == "server_default":
                    col["server_default"] = True
                elif kw.arg == "onupdate":
                    col["onupdate"] = self.col_default(kw.value, module, name)
                elif kw.arg == "deferred":
                    col["deferred"] = bool(literal(kw.value, self.src(module)))
                elif kw.arg == "autoincrement":
                    col["autoincrement"] = bool(literal(kw.value, self.src(module)))
                    col["autoincrement_set"] = True
                elif kw.arg == "name" and literal(kw.value, self.src(module)) != name:
                    raise self.err(f"column {name}: name= different from the attribute is not supported", kw, module)
                elif kw.arg == "type_":
                    raise self.err(f"column {name}: type_= is not supported (pass the type positionally)", kw, module)
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

    def is_plain_class(self, sym: Sym) -> bool:
        """A project class that is nothing else (no decorator; bases: `ABC` and/or one such class): instances
        with free attributes."""
        d = self.ix.definition(sym)
        return (isinstance(d, ast.ClassDef) and (self.plain_bases(sym) is not None or self.is_http_middleware(sym))
                and not d.keywords and not d.decorator_list and sym not in self.fe.model_syms
                and sym not in self.fe.schema_syms and sym not in self.fe.dataclass_syms)

    def plain_bases(self, sym: Sym) -> list[Sym] | None:
        """The project base of a plain class (`[]`: none, `ABC` or `object` only); None if a base is anything else."""
        out = []
        for b in self.ix.definition(sym).bases:
            t = self.resolve(sym.module, b)
            if (isinstance(t, Ext) and t.dotted == "abc.ABC") or dotted(b) == "object":
                continue
            if isinstance(t, Sym) and t != sym and isinstance(self.ix.definition(t), ast.ClassDef) and self.is_plain_class(t):
                out.append(t)
                continue
            return None
        return out if len(out) <= 1 else None

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
        abstract: set[str] = set()
        try:
            for b in self.plain_bases(sym) or []:
                # single inheritance: the base's methods, __init__, class attributes and abstract methods
                parent = self.plain_class(b)
                abstract = set(parent.__dict__.get("abstract", ()))
                if parent.__dict__.get("init") is not None:
                    info.__dict__["init"] = parent.__dict__["init"]
            for st in node.body:
                if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    if is_dunder(st.name) and st.name != "__init__" and st.name not in DISPATCHED_DUNDERS:
                        raise self.err(f"class {sym.name}: method {st.name} is not supported", st, module)
                    last = [((dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "").split(".")[-1] for d in st.decorator_list]
                    for d, ln in zip(st.decorator_list, last):
                        if ln not in {"property", "classmethod", "staticmethod", "abstractmethod"}:
                            raise self.err(f"{sym.name}.{st.name}: decorator @{ast.unparse(d)} is not supported", d, module)
                    if "abstractmethod" in last:
                        abstract.add(st.name)
                    else:
                        abstract.discard(st.name)
                    if st.name == "__init__":
                        info.__dict__["init"] = Sym(module, f"{node.name}.__init__")
                    else:
                        own.append((st.name, "property" in last, Sym(module, f"{node.name}.{st.name}")))
                elif (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name)
                      and st.targets[0].id == "__hash__" and isinstance(st.value, ast.Constant) and st.value.value is None):
                    info.__dict__["unhashable"] = True
                elif isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name):
                    attrs[st.targets[0].id] = st.value
                    if st.targets[0].id == "__slots__":
                        v = st.value
                        if not (isinstance(v, (ast.Tuple, ast.List)) and all(isinstance(e, ast.Constant) and isinstance(e.value, str) for e in v.elts)):
                            raise self.err(f"class {sym.name}: __slots__ must be a literal tuple of names", st, module)
                        info.__dict__["slots"] = [e.value for e in v.elts]
                elif isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name):
                    if st.value is not None:
                        attrs[st.target.id] = st.value
                elif not isinstance(st, (ast.Pass, ast.Expr)):
                    raise self.err(f"unsupported statement in class {sym.name}", st, module)
        except TranspileError as e:
            self.class_errors[sym] = e
            raise
        names = {m[0] for m in own} | set(attrs)
        info.methods = own + [m for m in (parent.methods if parent else []) if m[0] not in names]
        info.__dict__["abstract"] = sorted(abstract)
        if parent is not None:
            info.__dict__["base_cls"] = parent.cls
        info.__dict__["class_attrs"] = {n: g for n, g in (parent.__dict__.get("class_attrs", {}) if parent else {}).items()
                                        if n not in names}
        for n, value in attrs.items():
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
                f"fn {g}_prop<'a>(cx: &'a Cx, _slf: V, _args: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin({g}(cx)) }}"
            )
        self.schemas[sym] = info
        return info

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
        info.__dict__["dataclass"] = True
        try:
            for d in node.decorator_list:
                if isinstance(d, ast.Call) and d.args:
                    raise self.err(f"@dataclass options are not supported ({sym.name})", d, module)
                for k in getattr(d, "keywords", []):
                    if k.arg == "frozen":
                        info.__dict__["frozen"] = bool(self.const(k.value, module))
                    elif k.arg not in {"eq", "order", "repr", "init"} or self.const(k.value, module) is not (k.arg != "order"):
                        raise self.err(f"@dataclass({k.arg}=) is not supported ({sym.name})", d, module)
                if not is_ext(self.resolve(module, d.func if isinstance(d, ast.Call) else d), "dataclasses.dataclass"):
                    raise self.err(f"dataclass {sym.name}: decorator @{ast.unparse(d)} is not supported", d, module)
            if node.bases or node.keywords:
                raise self.err(f"dataclass {sym.name}: base classes are not supported", node, module)
            for stmt in node.body:
                if isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
                    ann = stmt.annotation
                    if isinstance(ann, ast.Subscript) and (dotted(ann.value) or "").split(".")[-1] == "ClassVar":
                        raise self.err(f"dataclass {sym.name}: ClassVar is not supported", stmt, module)
                    f = {"name": stmt.target.id, "alias": None, "default": "Required", "env": None, "any": True, "ann": (ann, module)}
                    v = stmt.value
                    if v is not None:
                        if isinstance(v, ast.Call) and is_ext(self.resolve(module, v.func), "dataclasses.field"):
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
                    if stmt.name == "__post_init__":
                        info.__dict__["post_init"] = Sym(module, f"{node.name}.__post_init__")
                        continue
                    if is_dunder(stmt.name) and stmt.name not in DISPATCHED_DUNDERS:
                        raise self.err(f"dataclass {sym.name}: method {stmt.name} is not supported", stmt, module)
                    last = [((dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "").split(".")[-1] for d in stmt.decorator_list]
                    for d, ln in zip(stmt.decorator_list, last):
                        if ln not in {"property", "classmethod", "staticmethod"}:
                            raise self.err(f"dataclass {sym.name}.{stmt.name}: decorator @{ast.unparse(d)} is not supported", d, module)
                    info.methods.append((stmt.name, "property" in last, Sym(module, f"{node.name}.{stmt.name}")))
                elif isinstance(stmt, (ast.Pass, ast.Expr)):
                    continue
                else:
                    raise self.err(f"unsupported statement in dataclass {sym.name}", stmt, module)
        except TranspileError as e:
            self.class_errors[sym] = e
            raise
        self.schemas[sym] = info
        return info

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
                cons = dict(f.get("cons", {}))
                if info.__dict__.get("enum_values"):
                    cons["_enum_values"] = True
                self.td(ann, module, cons)
            return info
        except TranspileError as e:
            self.schemas.pop(sym, None)
            self.class_errors[sym] = e
            raise
        finally:
            self.cur = cur

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

    def _schema(self, sym: Sym) -> SchemaInfo:
        node = self.fe.schema_syms[sym]
        module = sym.module
        base = f"{mod_ident(module)}__{ident(sym.name)}"
        info = SchemaInfo(sym, f"SCHEMA_{base}", f"CLS_{base}", [], False, "Ignore")
        self.schemas[sym] = info
        fields: list[dict] = []
        for b in node.bases:
            t = self.resolve(module, b)
            if isinstance(t, Sym) and t in self.fe.schema_syms:
                parent = self.schema(t)
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
                for k in ("strip", "lower", "upper", "enum_values", "validate_assignment", "populate_by_name"):
                    if k in parent.__dict__:
                        info.__dict__[k] = parent.__dict__[k]
            elif isinstance(t, Ext) and t.dotted.split(".")[-1] == "BaseSettings":
                info.settings = True
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
                        if "alias" in opts:
                            f["alias"] = self.const(opts["alias"], module)
                    f["cons"] = cons
                    f["ann"] = (ann, module)
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
                if "alias" in opts:
                    f["alias"] = self.const(opts["alias"], module)
                for k in ("validation_alias", "serialization_alias"):
                    # one alias for input and output only: a separate one changes one side
                    if k in opts and self.const(opts[k], module) != f.get("alias"):
                        raise self.err(f"Field({k}=) different from alias= is not supported", opts[k], module)
                if "validate_default" in opts and self.const(opts["validate_default"], module):
                    if f["default"].startswith("Dyn("):
                        raise self.err("Field(validate_default=True) with a computed default is not supported", opts["validate_default"], module)
                    f["validate_default"] = True
                f["cons"] = cons
                f["ann"] = (ann, module)
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
                    for kw in dec.keywords:
                        if kw.arg == "mode":
                            mode = self.const(kw.value, module)
                            if mode not in {"after", "before"}:
                                raise self.err(f"field validators mode={mode!r} are not supported (after/before)", kw, module)
                            before = mode == "before"
                        elif kw.arg == "pre":
                            before = bool(self.const(kw.value, module))
                        elif kw.arg in {"each_item", "always"} and self.const(kw.value, module):
                            raise self.err(f"@validator({kw.arg}=True) is not supported", kw, module)
                        elif kw.arg not in {"mode", "pre", "each_item", "always", "allow_reuse", "check_fields"}:
                            raise self.err(f"@{last[0]}({kw.arg}=) is not supported", kw, module)
                    kind = self.validator_info(stmt, "field_validator" not in last, before, sym, module)
                    if kind:
                        info.__dict__.setdefault("vinfo", {})[msym] = kind
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
                elif stmt.name == "__init__":
                    # `def __init__(self, **data)`: run by `Model(...)` only (model_validate and FastAPI skip it)
                    a = stmt.args
                    if (isinstance(stmt, ast.AsyncFunctionDef) or stmt.decorator_list or len(a.args) != 1 or a.vararg
                            or a.kwonlyargs or a.posonlyargs or a.kwarg is None):
                        raise self.err(f"schema {sym.name}: only `def __init__(self, **data)` is supported", stmt, module)
                    info.__dict__["init"] = msym
                elif "field_serializer" in last or "model_serializer" in last:
                    raise self.err(f"schema {sym.name}: @{last[0]} is not supported", stmt, module)
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
        for f in fields:
            if f.get("validate_default") and any(f["name"] in names for names, _ in info.__dict__.get("before", [])):
                raise self.err(f"schema {sym.name}.{f['name']}: validate_default=True with a mode='before' validator "
                               "is not supported", node, module)
        if info.__dict__.get("model_before") and info.__dict__.get("validate_assignment"):
            raise self.err(f"schema {sym.name}: validate_assignment=True with @model_validator(mode=\"before\") is not "
                           "supported (pydantic calls it on every assignment with the whole data)", node, module)
        info.fields = fields
        return info

    @staticmethod
    def put_field(fields: list[dict], f: dict) -> list[dict]:
        """A field (re)declared: a redefinition (in a subclass, or again in the class) keeps the inherited
        position, like Pydantic's field order (the annotations' dict order)."""
        out = [f if x["name"] == f["name"] else x for x in fields]
        return out if any(x["name"] == f["name"] for x in fields) else out + [f]

    VALIDATION_INFO_ATTRS = {"data", "field_name"}

    def validator_info(self, fn, v1: bool, before: bool, sym, module) -> int:
        """What a field validator receives besides the value, by pydantic's own rules (`inspect_validator`,
        `make_generic_v1_field_validator`): 0 nothing, 1 `info` (positional), 2 `values=info.data`."""
        a = fn.args
        pos = (a.posonlyargs + a.args)[1:]  # after cls
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
        if spec is not None and spec.startswith("Dyn("):
            raise self.err(f"schema {sym.name}.{name}: a private attribute's default must be a literal or a factory", value, module)
        private = [p for p in info.__dict__.get("private", []) if p[0] != name]
        info.__dict__["private"] = private + ([(name, spec)] if spec is not None else [])

    def model_config(self, info: SchemaInfo, value: ast.AST, module: str) -> None:
        if isinstance(value, ast.Dict):
            items = []
            for k, v in zip(value.keys, value.values):
                if k is None:
                    raise self.err("model_config: ** expansion is not supported", value, module)
                items.append(ast.keyword(arg=self.const(k, module), value=v))
        elif isinstance(value, ast.Call) and (dotted(value.func) or "").split(".")[-1] in {"ConfigDict", "SettingsConfigDict"}:
            items = value.keywords
        else:
            raise self.err("model_config must be ConfigDict(...) or a dict literal", value, module)
        self.config_items(info, items, module)

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
            v = self.const(kw.value, module) if kw.arg not in {"env_file", "json_encoders", "alias_generator"} else None
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
            elif kw.arg == "env_parse_none_str":
                if v is not None and not isinstance(v, str):
                    raise self.err("model_config env_parse_none_str= must be a string or None", kw, module)
                info.env_none = v
            elif kw.arg in {"env_file", "env_file_encoding",
                            "arbitrary_types_allowed", "protected_namespaces", "json_schema_extra", "title"}:
                pass
            elif kw.arg == "str_strip_whitespace":
                info.__dict__.setdefault("strip", bool(v))
            elif kw.arg == "str_to_lower":
                info.__dict__.setdefault("lower", bool(v))
            elif kw.arg == "str_to_upper":
                info.__dict__.setdefault("upper", bool(v))
            elif kw.arg == "validate_assignment":
                info.__dict__["validate_assignment"] = bool(v)
            elif kw.arg == "frozen":
                info.__dict__["frozen"] = bool(v)
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
            for stmt in cls.body:
                if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)) and stmt.name == mname:
                    return stmt
            raise TranspileError(f"no method {sym.name}")
        return self.ix.definition(sym)

    def stmt_runner(self, module: str, st: ast.stmt) -> str:
        """A module-level compound statement (`try`, `if`, `for`, `while`, `with`), run once at startup in its
        place: a global holding the values of the names it binds (its locals), in `stmt_bindings` order."""
        runners = self.__dict__.setdefault("stmt_runners", {})
        key = (module, st.lineno)
        if key not in runners:
            name = f"ms_{mod_ident(module)}__{st.lineno}"
            runners[key] = name
            fc = FnCompiler(self, module, None, name)
            prev, self.cur = self.cur, ("modst", name)
            try:
                bound = stmt_bindings(st)
                m = self.ix.module(module)
                for n in bound:
                    if len(m.binders.get(n, [])) > 1:
                        raise fc.err(f"the module variable `{n}` is bound by several module-level statements "
                                     f"(lines {', '.join(str(b.lineno) for b in m.binders[n])}): not supported", st)
                fc.locals = set(stmt_bindings(st, handlers=True))
                for n in sorted(fc.locals):
                    fc.emit(f"let mut v_{ident(n)}: V = V::Unbound;")
                fc.block([st])
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
            if isinstance(node, (ast.Try, ast.If, ast.For, ast.While, ast.With)) and not import_guard(node):
                runner = self.stmt_runner(sym.module, node)
                self.edges.setdefault(("global", sym), set()).add(("modst", runner))
                self.globals[sym] = name
                i = stmt_bindings(node).index(sym.name)
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
            self.globals[sym] = name
            self.__dict__.setdefault("eager", []).append((sym.module, getattr(node, "lineno", 0), name))
            fc = FnCompiler(self, sym.module, None, name)
            try:
                value = node.value
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
                    nparams = len(fn_params(self.fn_node(sym)))
                except Exception:
                    nparams = 0
                self.items.append(
                    f"pub async fn {self.fns[key]}(cx: &Cx{', _: V' * nparams}) -> R {{\n"
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

    # ---------------------------------------------------------------- output

    def emit(self, line: str) -> None:
        self.lines.append("    " * self.ind + line)

    def err(self, msg: str, node) -> TranspileError:
        return self.p.err(msg, node, self.module)

    def tmp(self, prefix: str = "t") -> str:
        return f"__{prefix}{self.p.uid()}"

    def q(self, r_expr: str) -> str:
        """Unwrap a `R` expression, routing the error to the enclosing try or out of the function."""
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
                raise self.err("`nonlocal` is not supported", n)
            if isinstance(n, ast.Global):
                for name in n.names:
                    sym = Sym(self.module, name)
                    if not isinstance(self.p.ix.definition(sym), (ast.Assign, ast.AnnAssign, ast.Try, ast.If, ast.For, ast.While, ast.With)):
                        raise self.err(f"`global {name}`: only a variable assigned at module level is supported", n)
                    self.globals_decl[name] = sym
        self.locals = assigned_names(node.body) - self.params - set(self.globals_decl)
        # function-level imports that resolve to nothing: raised at run time, names are plain locals
        self.failed_imports = set()
        for n in walk_scope(node):
            if isinstance(n, (ast.Import, ast.ImportFrom)):
                for a in n.names:
                    local = (a.asname or a.name).split(".")[0]
                    if self.p.resolve(self.module, ast.Name(local, ast.Load()), node) is None:
                        self.failed_imports.add(local)
        self.locals |= self.failed_imports
        sig = ", ".join(f"mut v_{ident(p.name)}: V" for p in params)
        gen_param = (f", y: &{RT}::web::DepYield" if self.variant == "depgen" else f", y: &{RT}::web::Yielder") if self.gen else ""
        caps = "".join(f", {c}: V" for c in self.captures.values())
        traced = self.p.traced()
        self.__dict__["traced_fn"] = traced
        for name in sorted(self.locals):
            self.emit(f"let mut v_{ident(name)}: V = V::Unbound;")
        self.block(node.body)
        self.emit("#[allow(unreachable_code)]")
        self.emit("Ok(V::None)")
        head = f"pub async fn {self.name}(cx: &Cx{', ' + sig if sig else ''}{caps}{gen_param}) -> R {{"
        # RecursionError rather than a stack overflow (the process would abort): dynrt::stack_guard
        guard = f"    {RT}::stack_guard()?;"
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
        elif isinstance(node, ast.Global):
            pass  # declared names were collected by compile_function
        elif isinstance(node, ast.Delete):
            for t in node.targets:
                if isinstance(t, ast.Subscript):
                    self.check_writable(t.value)
                    self.emit(f"{self.q(f'{RT}::ops::delitem(&{self.expr(t.value)}, &{self.expr(t.slice)})')};")
                elif isinstance(t, ast.Name) and t.id in self.locals:
                    self.emit(f"v_{ident(t.id)} = V::Unbound;")
                else:
                    raise self.err("unsupported del target", t)
        else:
            raise self.err(f"unsupported statement `{type(node).__name__}`", node)

    def assign(self, target: ast.AST, val: str) -> None:
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
            ref = self.static_ref(target)
            if isinstance(ref, Ext) and ref.dotted in libmap.HOOKS:
                # `aiohttp.ClientSession._request = wrapper(...)`: the runtime's client calls the wrapper
                self.emit(f"{self.q(libmap.HOOKS[ref.dotted].format(val=val))};")
                return
            if isinstance(self.static_ref(target.value), Ext):
                raise self.err(f"assigning `{ast.unparse(target)}` (a library attribute) is not supported", target)
            if isinstance(base := self.static_ref(target.value), ModRef):
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
                return scope[name]
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
                fn = f"{RT}::iadd" if op == "add" else f"{RT}::ops::{op}"
                self.set_global(node.target.id, self.q(f"{fn}(&{cur}, &{rhs})"))
                return
            if op == "add":
                self.emit(f"{self.store_name(node.target.id, node)} = {self.q(f'{RT}::iadd(&{cur}, &{rhs})')};")
            else:
                self.emit(f"{self.store_name(node.target.id, node)} = {self.q(f'{RT}::ops::{op}(&{cur}, &{rhs})')};")
        elif isinstance(node.target, (ast.Attribute, ast.Subscript)):
            if isinstance(node.target, ast.Subscript):
                self.check_writable(node.target.value)
            t = self.tmp()
            self.emit(f"let {t} = {self.expr(node.target.value)};")
            if isinstance(node.target, ast.Attribute):
                cur = self.q(f"{RT}::methods::getattr(cx, &{t}, {rs(node.target.attr)}).await")
                new = self.q(f"{RT}::ops::{op}(&{cur}, &{rhs})")
                self.emit(f"{self.q(f'{RT}::methods::setattr_cx(cx, &{t}, {rs(node.target.attr)}, {new}).await')};")
            else:
                k = self.tmp()
                self.emit(f"let {k} = {self.expr(node.target.slice)};")
                cur = self.q(f"{RT}::ops::getitem(&{t}, &{k})")
                new = self.q(f"{RT}::ops::{op}(&{cur}, &{rhs})")
                self.emit(f"{self.q(f'{RT}::ops::setitem(&{t}, &{k}, {new})')};")
        else:
            raise self.err("unsupported augmented assignment target", node)

    def raise_stmt(self, node: ast.Raise) -> None:
        if node.exc is None:
            if not self.cur_exc:
                raise self.err("bare `raise` outside an except block", node)
            self.emit(self.raise_code(f"{self.cur_exc[-1]}.clone()"))
            return
        exc = node.exc
        if isinstance(exc, (ast.Name, ast.Attribute)) and self.static_ref(exc) is not None and not isinstance(exc, ast.Call):
            v = self.expr(exc)
        else:
            v = self.expr(exc)
        self.emit(self.raise_code(f"{RT}::raise_v(&{v})"))

    def for_stmt(self, node: ast.For) -> None:
        it = self.expr(node.iter)
        lbl = f"'l{self.p.uid()}"
        var = self.tmp("it")
        flag = self.loop_else_start(node, lbl)
        self.emit(f"{lbl}: for {var} in {self.q(f'{RT}::orm::iter(&{it}).await')} {{")
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
        if isinstance(t, ast.Tuple):
            return " || ".join(self.exc_match(x, e) for x in t.elts)
        return f"{e}.isinstance(&{self.exc_class(t)})"

    def exc_class(self, t: ast.AST) -> str:
        if isinstance(t, ast.Name) and t.id in libmap.BUILTIN_EXC_NAMES and not self.is_local(t.id):
            ref = self.p.resolve(self.module, t, self.scope_root())
            if ref is None or isinstance(ref, Ext):
                return f"{RT}::v::{libmap.BUILTIN_EXC_NAMES[t.id]}"
        ref = self.p.resolve(self.module, t, self.scope_root())
        if isinstance(ref, Sym):
            return self.p.class_static(ref)
        if isinstance(ref, Ext) and ref.dotted in libmap.EXCEPTIONS:
            return f"{RT}::v::{libmap.EXCEPTIONS[ref.dotted]}"
        raise self.err(f"unsupported exception class `{ast.unparse(t)}`", t)

    def with_stmt(self, node: ast.With) -> None:
        """`with cm as x: body` -> x = enter(cm); try: body finally: exit(cm) (runtime values that are their
        own context managers: open() files, BytesIO)."""
        item = node.items[0]
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
            if self.p.uses_sentry():
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
        name = f"{self.name}__{ident(node.name)}_{self.p.uid()}"
        caps = {v: f"c_{ident(v)}" for v in free}
        sub = FnCompiler(self.p, self.module, node, name, captures=caps, parent=self)
        self.extra_fns.append(sub.compile_function())  # carries its own nested functions
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
        selfref = node.name in caps and node.name not in self.params
        cell = self.tmp("cell") if selfref else None
        if selfref:
            self.emit(f"let {cell} = std::sync::Arc::new(std::sync::OnceLock::<V>::new());")
        held = [c for v, c in caps.items() if not (selfref and v == node.name)] + list(dflts.values())
        hold = "".join(f"let {c} = {c}.clone(); " for c in held)
        if selfref:
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
        cap_clone = " ".join(f"let {cell}_c = {cell}.clone();" if selfref and v == node.name else f"let {caps[v]} = {self.load_local(v)};"
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
        self.emit(f"{self.store_name(node.name, node)} = {fv};")
        if selfref:
            self.emit(f"let _ = {cell}.set({self.load_local(node.name)});")

    def scope_root(self):
        """The outermost function enclosing this code: its local imports are visible to nested functions
        and lambdas (closures)."""
        c, root = self, self.node
        while c.parent is not None:
            c = c.parent
            if c.node is not None:
                root = c.node
        return root

    def qualname(self) -> str | None:
        """`__qualname__` of the function being compiled (prefix of its nested functions'), None at
        module level."""
        if self.node is None:
            return None
        own = "<lambda>" if isinstance(self.node, ast.Lambda) else self.node.name
        if self.parent is not None:
            outer = self.parent.qualname()
            return f"{outer}.<locals>.{own}" if outer else own
        if self.p.cur is not None:
            return self.p.cur[0].name
        return own

    def free_vars(self, node) -> list[str]:
        own = {p.name for p in fn_params(node)}
        if not isinstance(node, ast.Lambda):
            own |= assigned_names(node.body)
        out = []
        for n in ast.walk(node):
            if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load) and n.id not in own and n.id not in out:
                if self.is_local(n.id):
                    out.append(n.id)
        return out

    def is_local(self, name: str) -> bool:
        return (any(name in s for s in self.comp_scopes) or name in self.locals or name in self.params
                or name in self.captures)

    def load_local(self, name: str) -> str:
        for scope in reversed(self.comp_scopes):
            if name in scope:
                return f"{scope[name]}.clone()"
        if name in self.params:
            return f"v_{ident(name)}.clone()"
        if name in self.locals:
            return self.q(f"{RT}::ops::bound(&v_{ident(name)}, {rs(name)})")
        if name in self.captures:
            return f"{self.captures[name]}.clone()"
        raise KeyError(name)

    # ---------------------------------------------------------------- expressions

    def truthy(self, node: ast.AST) -> str:
        return self.q(f"{RT}::ops::truthy(&{self.expr(node)})")

    def expr(self, node: ast.AST) -> str:
        m = getattr(self, "e_" + type(node).__name__, None)
        if m is None:
            raise self.err(f"unsupported expression `{ast.unparse(node)}`", node)
        return m(node)

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
                raise self.err(f"importlib.import_module({name!r}): only modules of the project are supported", node)
            return ModRef(name)
        if isinstance(node, ast.Name):
            if self.is_local(node.id):
                return None
            t = self.p.resolve(self.module, node, self.scope_root())
            if t is None:
                if node.id in BUILTIN_FUNCS or node.id in BUILTIN_TYPES or node.id in libmap.BUILTIN_EXC_NAMES:
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
                return Ext(libmap.canonical(f"{base.dotted}.{node.attr}"))
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
        if ref is None and isinstance(node, ast.Name):
            fl = self.factory_local(node.id)
            if fl is not None:
                return fl
        if ref is None:
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
            if ref.dotted in libmap.VALUES:
                return libmap.VALUES[ref.dotted]
            code = libmap.status_constant(ref.dotted)
            if code is not None:
                return f"V::Int({code})"
            if ref.dotted in libmap.EXCEPTIONS:
                return f"V::Class(&{RT}::v::{libmap.EXCEPTIONS[ref.dotted]})"
            if ref.dotted.startswith("builtins.") and ref.dotted[9:] in BUILTIN_TYPES:
                return f"V::native({RT}::Native::Type({rs(ref.dotted[9:])}))"
            if ref.dotted in libmap.CALLS and ref.dotted not in self.p.LIB_FN_NOT_VALUES:
                return self.lib_fn_value(ref.dotted, node)
            obj = self.object_value(ref.dotted)
            if obj is not None:
                return obj
            raise self.err(f"library value `{ref.dotted}` is not supported (not in the py2axum library map)", node)
        raise self.err(f"unsupported name `{ast.unparse(node)}`", node)

    def class_value(self, sym: Sym, node) -> str:
        try:
            return f"V::Class(&{self.p.class_static(sym)})"
        except TranspileError as e:
            if e.file:
                raise  # keep the location of the offending declaration (model/schema file)
            raise self.err(e.msg, node) from None

    def fn_value(self, sym: Sym, d, node) -> str:
        try:
            vdecos = value_decorators(d)
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

    def e_Attribute(self, node: ast.Attribute) -> str:
        ref = self.static_ref(node)
        if ref is not None:
            if isinstance(ref, tuple) and ref[0] == "classattr" and self.p.enum_kind(ref[1]):
                _, _, names = self.p.enum(ref[1])
                if ref[2] in names:
                    return f"V::Enum(&{self.p.enum(ref[1])[0]}, {names.index(ref[2])})"
            if isinstance(ref, tuple) and ref[0] == "classattr":
                sym, attr = ref[1], ref[2]
                if sym in self.p.fe.model_syms:
                    info = self.p.model(sym)
                    for i, c in enumerate(info.cols):
                        if c["name"] == attr:
                            return f"V::Col(&{info.rust}, {i})"
            return self.ref_value(ref, node)
        obj = self.expr(node.value)
        self.check_attr(node)
        return self.q(f"{RT}::methods::getattr(cx, &{obj}, {rs(node.attr)}).await")

    def check_attr(self, node: ast.Attribute) -> None:
        """An attribute read at run time that no runtime type provides (and the project never defines nor
        names) is a certain AttributeError on that path: refused here, so that --report counts it."""
        name = node.attr
        if name in self.p.project_attrs() or name in runtime_attr_names() or self.p.open_attrs():
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
        te = self.type_expr(node)
        if te is not None:
            return te
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
        return self.q(f"{RT}::ops::{op}(&{self.expr(node.left)}, &{self.expr(node.right)})")

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

    def e_JoinedStr(self, node: ast.JoinedStr) -> str:
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
                    spec = self.expr(v.format_spec)
                    parts.append(f"{s}.push_str(&{self.q(f'{RT}::ops::format_spec(&{val}, &{RT}::ops::str_(&{spec})?)')});")
                else:
                    parts.append(f"{s}.push_str(&{self.q(f'{RT}::ops::str_(&{val})')});")
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

    def comprehension(self, generators, emit_inner) -> str:
        out_lines = []
        depth = 0
        pushed = 0
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
            self.comp_scopes.append(scope)
            pushed += 1
            var = self.tmp("c")
            out_lines.append(f"for {var} in {self.q(f'{RT}::orm::iter(&{it}).await')} {{")
            depth += 1
            out_lines.append(self.bind_comp_target(g.target, var))
            for cond in g.ifs:
                out_lines.append(f"if !{self.truthy(cond)} {{ continue; }}")
        out_lines.append(emit_inner())
        out_lines.append("}" * depth)
        for _ in range(pushed):
            self.comp_scopes.pop()
        return " ".join(out_lines)

    def bind_comp_target(self, target, val: str) -> str:
        if isinstance(target, ast.Name):
            return f"let {self.comp_scopes[-1][target.id]} = {val};"
        if isinstance(target, (ast.Tuple, ast.List)):
            t = self.tmp()
            parts = [f"let {t} = {self.q(f'{RT}::ops::unpack(&{val}, {len(target.elts)})')};"]
            for i, e in enumerate(target.elts):
                parts.append(self.bind_comp_target(e, f"{t}[{i}].clone()"))
            return " ".join(parts)
        raise self.err("unsupported comprehension target", target)

    def e_ListComp(self, node) -> str:
        out = self.tmp("l")
        body = self.comprehension(node.generators, lambda: f"{out}.push({self.expr(node.elt)});")
        return f"{{ let mut {out}: Vec<V> = Vec::new(); {body} V::list({out}) }}"

    e_GeneratorExp = e_ListComp

    def e_SetComp(self, node) -> str:
        return self.q(f"{RT}::methods::b_set(&[{self.e_ListComp(node)}])")

    def e_DictComp(self, node) -> str:
        out = self.tmp("d")
        body = self.comprehension(node.generators, lambda: f"{out}.push(({self.expr(node.key)}, {self.expr(node.value)}));")
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
        cap_clone = " ".join(f"let {caps[v]} = {self.load_local(v)};" for v in free)
        inner = "".join(f"let {c} = {c}.clone(); " for c in [*caps.values(), *dflts.values()])
        run = f"{pre} Ok({body})"
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
                sub.node = self.node  # name resolution in the function's scope (its local imports)
                sub.factory = self.__dict__.get("factory")
                code = sub.call(inner, awaited=True)
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
        if (len(node.args) == 1 and not node.keywords and isinstance(node.func, (ast.Name, ast.Attribute))
                and self.static_ref(node.func) == Ext("importlib.import_module")):
            if not (isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str)):
                raise self.err("importlib.import_module() of a computed name is not supported (only a literal module "
                               "of the project)", node)
            return self.ref_value(self.static_ref(node), node)  # a literal project module, or refused
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
        cm = self.super_classmethod(node)
        if cm is not None:
            return cm
        if f.value.args or f.attr != "__init__" or self.node is None or self.p.cur is None or "." not in self.p.cur[0].name:
            raise self.err("only `super().__init__(...)` inside a method is supported", node)
        csym = Sym(self.p.cur[0].module, self.p.cur[0].name.split(".")[0])
        me = f"v_{ident(self.node.args.args[0].arg)}"
        args, kwargs = self.dyn_args(node)
        cls = self.p.ix.definition(csym)
        if not cls.bases:
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
        if not (isinstance(fac, ast.Name) and self.static_ref(fac) == ("builtin", fac.id) and fac.id in self.DEFAULT_FACTORIES):
            raise self.err(f"defaultdict({ast.unparse(fac)}): only a builtin type factory is supported "
                           f"({', '.join(sorted(self.DEFAULT_FACTORIES))})", node)
        init = [self.expr(a) for a in node.args[1:]]
        kwargs = "vec![" + ", ".join(f"({rs(k.arg)}.to_string(), {self.expr(k.value)})" for k in node.keywords if k.arg) + "]"
        if any(k.arg is None for k in node.keywords):
            raise self.err("defaultdict(..., **mapping) is not supported", node)
        return self.q(f"{RT}::ops::defaultdict({rs(fac.id)}, {RT}::methods::b_dict(&vec![{', '.join(init)}], &{kwargs})?)")

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
        raise self.err(f"string.Formatter().{f.attr}() is not supported (vformat and format are)", node)

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
        return self.q(f"{RT}::web::spawn_task(cx, {fv}, {args}, {kwargs})")

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

    def call(self, node: ast.Call, awaited: bool) -> str:
        if isinstance(node.func, ast.Attribute) and node.func.attr == "run_sync":
            raise self.err("conn.run_sync(...) without `await` does nothing in Python: not supported", node)
        if isinstance(node.func, ast.Attribute) and node.func.attr == "model_json_schema":
            if node.args or node.keywords:
                raise self.err(".model_json_schema() with arguments is not supported (pydantic's defaults only)", node)
            e = self.p.json_schemas()[1].get((self.module, node.lineno, node.col_offset))
            if e is not None:
                raise e
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
            recv = self.expr(node.func.value)
            self.check_method(node)
            self.p.method_edges(node.func.attr)
            args, kwargs = self.dyn_args(node)
            if not awaited and node.func.attr in self.p.async_method_names():
                return self.q(f"{RT}::aio::call_method_lazy(cx, &{recv}, {rs(node.func.attr)}, {args}, {kwargs}).await")
            if awaited:
                # a synchronous Session's plain methods awaited by mistake: TypeError, like CPython
                return self.q(f"{RT}::aio::call_method_awaited(cx, &{recv}, {rs(node.func.attr)}, {args}, {kwargs}).await")
            return self.q(f"{RT}::methods::call_method(cx, &{recv}, {rs(node.func.attr)}, {args}, {kwargs}).await")
        f = self.expr(node.func)
        args, kwargs = self.dyn_args(node)
        if not awaited:
            return self.q(f"{RT}::aio::call_value_lazy(cx, &{f}, {args}, {kwargs}).await")
        return self.q(f"{RT}::methods::call_value(cx, &{f}, {args}, {kwargs}).await")

    def check_method(self, node: ast.Call) -> None:
        """A method resolved at run time that no runtime type implements (and no project class defines)
        is a certain 500 on that path: refused here, so that --report counts it."""
        name = node.func.attr
        if name in self.p.project_attrs() or name == "__wrapped__":
            return
        recv = node.func.value
        if isinstance(recv, ast.Name) and recv.id in self.__dict__.get("failed_imports", set()):
            return  # unreachable: the import raised
        if name not in runtime_names():
            raise self.err(f"method .{name}() is not implemented by the runtime for any type "
                           "(it would raise AttributeError at run time)", node)
        allowed = METHOD_KWARGS.get(name)
        if allowed is not None:
            for k in node.keywords:
                if k.arg is not None and k.arg not in allowed:
                    raise self.err(f".{name}({k.arg}=) is not supported (only {', '.join(sorted(allowed))})", k.value)

    def static_call(self, ref, node: ast.Call, awaited: bool) -> str:
        if isinstance(ref, tuple) and ref[0] == "builtin":
            return self.builtin_call(ref[1], node)
        if isinstance(ref, tuple) and ref[0] == "classattr":
            _, sym, attr = ref
            d = self.p.ix.definition(sym)
            for stmt in d.body:
                if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)) and stmt.name == attr:
                    decos = {dotted(x) for x in stmt.decorator_list}
                    msym = Sym(sym.module, f"{sym.name}.{attr}")
                    if "staticmethod" in decos:
                        return self.project_call(msym, stmt, node, awaited, prefix=[])
                    if "classmethod" in decos:
                        return self.project_call(msym, stmt, node, awaited, prefix=[self.class_value(sym, node)])
            recv = self.class_value(sym, node)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::methods::call_method(cx, &{recv}, {rs(attr)}, {args}, {kwargs}).await")
        if isinstance(ref, Sym):
            d = self.p.ix.definition(ref)
            if isinstance(d, (ast.FunctionDef, ast.AsyncFunctionDef)):
                return self.project_call(ref, d, node, awaited, prefix=[])
            if isinstance(d, ast.ClassDef):
                return self.construct(ref, d, node)
            if isinstance(d, (ast.Assign, ast.AnnAssign)):
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
            vdecos = value_decorators(d)
        except TranspileError as e:
            raise self.err(e.msg, e.node or d) from None
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
                              f"{'true' if info.case_sensitive else 'false'}, {none}, {kwargs}).await")
            if info.__dict__.get("init") is not None:
                mw = method_wrapper(self.p, info.__dict__["init"])
                return self.q(f"{{ let __o = {RT}::pyd::object_new_blank(&{info.rust}); "
                              f"{mw}(cx, __o.clone(), {RT}::pack(vec![], {kwargs})).await.map(|_| __o) }}")
            return self.q(f"{RT}::pyd::construct(cx, &{info.rust}, {RT}::kwargs_dict({kwargs})?).await")
        if sym in self.p.fe.dataclass_syms:
            info = self.p.dataclass(sym)
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

    def lib_fn_value(self, name: str, node) -> str:
        """A library function used as a value (`run_in_executor(None, threading.get_ident)`): a function object
        calling its translation with 0 to 3 positional arguments."""
        tmpl = libmap.CALLS[name]
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
        return (f"{RT}::func(std::sync::Arc::new(|cx: &Cx, args: Vec<V>| -> {RT}::BoxFut<'_> "
                f"{{ Box::pin(async move {{ let _ = cx; {body} }}) }}))")

    def type_adapter(self, node: ast.Call) -> str:
        """`TypeAdapter(T)` with `T` a type written in the source (the validator built at compile time)."""
        if len(node.args) != 1 or node.keywords:
            raise self.err("only TypeAdapter(type) is supported", node)
        t = node.args[0]
        if not (isinstance(t, ast.Name) and self.is_local(t.id)):
            try:
                td = self.p.td(t, self.module, None, self.scope_root())
                return f"V::native({RT}::Native::Adapter(&{td}, {rs(ast.unparse(t))}))"
            except TranspileError:
                pass
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
        tmpl = libmap.CALLS.get(name)
        recv, _, meth = name.rpartition(".")
        if tmpl is None and self.object_value(recv, values=False) is not None:
            # a method of a library object (`REGISTRY.register(c)`): dispatched at run time like any value's
            obj = self.object_value(recv, values=False)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::methods::call_method(cx, &{obj}, {rs(meth)}, {args}, {kwargs}).await")
        if tmpl is None and recv in libmap.VALUES and not libmap.VALUES[recv].startswith("V::Enum("):
            # a method of a library constant (`datetime.min.time()`)
            args, kwargs = self.dyn_args(node)
            return self.q(f"{RT}::methods::call_method(cx, &{libmap.VALUES[recv]}, {rs(meth)}, {args}, {kwargs}).await")
        if tmpl is None:
            raise self.err(f"library call `{name}()` is not supported (not in the py2axum library map)", node)
        if name.startswith("jwt."):
            try:
                libmap.pyjwt_static(name, node)
            except ValueError as e:
                raise self.err(f"{name}(): {e}", node) from None
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

    def builtin_call(self, name: str, node: ast.Call) -> str:
        # any/all/next over a generator expression: lazy and short-circuiting, as in Python
        if name in {"any", "all", "next"} and node.args and isinstance(node.args[0], ast.GeneratorExp) and not node.keywords:
            g = node.args[0]
            lbl, res = f"'g{self.p.uid()}", self.tmp("r")
            if name == "next":
                if len(node.args) > 2:
                    raise self.err("next() takes at most 2 arguments", node)
                miss = (f"{res} = {self.expr(node.args[1])};" if len(node.args) == 2
                        else f"{self.raise_code(f'Exc::new(&{RT}::v::STOP_ITERATION, vec![])')}")
                body = self.comprehension(g.generators, lambda: f"{res} = {self.expr(g.elt)}; break {lbl};")
                return f"{{ let mut {res} = V::None; {lbl}: {{ {body} {miss} }} {res} }}"
            if len(node.args) != 1:
                raise self.err(f"{name}() takes exactly one argument", node)
            hit = "true" if name == "any" else "false"
            neg = "" if name == "any" else "!"
            body = self.comprehension(g.generators, lambda: f"if {neg}{self.truthy(g.elt)} {{ {res} = {hit}; break {lbl}; }}")
            return f"{{ let mut {res} = !{hit}; {lbl}: {{ {body} }} V::Bool({res}) }}"
        # iter()/next() keep no state here: only the one-shot forms are translated
        if name == "iter" and not getattr(self, "_iter_ok", False) and self.stored_iter(node):
            raise self.err("iter() stored in a variable is not supported (its consumption state is not kept)", node)
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
            "set": f"{RT}::methods::b_set(&{args})", "frozenset": f"{RT}::methods::b_set(&{args})",
            "dict": f"{RT}::methods::b_dict(&{args}, &{kwargs})", "reversed": f"{RT}::methods::b_reversed(&{args}[0])",
            "sum": f"{RT}::methods::b_sum(&{args})", "any": f"{RT}::methods::b_any(&{args}[0])",
            "all": f"{RT}::methods::b_all(&{args}[0])", "next": f"{RT}::methods::b_next(&{args})",
            "round": f"{RT}::methods::b_round(&{args})", "abs": f"{RT}::methods::b_abs(&{args}[0])",
            "enumerate": f"{RT}::methods::b_enumerate(&{args})", "zip": f"{RT}::methods::b_zip(&{args}, &{kwargs})",
            "range": f"{RT}::methods::b_range(&{args})", "print": f"{RT}::methods::b_print(&{args})",
            "iter": f"{RT}::methods::b_list(&{args})", "hash": f"{RT}::methods::b_hash(&{args})",
            "type": f"{RT}::methods::b_type(&{args})",
            "chr": f"{RT}::methods::b_chr(&{args})", "ord": f"{RT}::methods::b_ord(&{args})",
            "divmod": f"{RT}::methods::b_divmod(&{args})",
            "bytes": f"{RT}::methods::b_bytes(&{args}, &{kwargs})",
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
        """`it = iter(x)`: a stateful iterator (next(it) twice...), not translated; `f(iter(x))` is."""
        scope = self.node if self.node is not None else None
        if scope is None:
            return False
        for n in walk_scope(scope):
            if isinstance(n, (ast.Assign, ast.AnnAssign, ast.NamedExpr)) and n.value is call:
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
        ref = self.static_ref(t)
        if isinstance(ref, tuple) and ref[0] == "builtin":
            if ref[1] in libmap.BUILTIN_EXC_NAMES:
                return f"{RT}::methods::isinstance_class(&{v}, &{RT}::v::{libmap.BUILTIN_EXC_NAMES[ref[1]]})"
            return f"{RT}::methods::isinstance_builtin(&{v}, {rs(ref[1])})"
        if isinstance(ref, Sym):
            return f"{RT}::methods::isinstance_class(&{v}, &{self.p.class_static(ref)})"
        if isinstance(ref, Ext):
            short = {"datetime.datetime": "datetime", "datetime.date": "date", "datetime.timedelta": "timedelta",
                     "datetime.time": "time", "uuid.UUID": "UUID", "enum.Enum": "Enum", "pathlib.Path": "Path",
                     "decimal.Decimal": "Decimal"}.get(ref.dotted)
            if short:
                return f"{RT}::methods::isinstance_builtin(&{v}, {rs(short)})"
            if ref.dotted in libmap.EXCEPTIONS:
                return f"{RT}::methods::isinstance_class(&{v}, &{RT}::v::{libmap.EXCEPTIONS[ref.dotted]})"
        # a type known at run time (a variable, a library class value, a tuple built by the code)
        tv = self.expr(t)
        return self.q(f"{RT}::types::isinstance(&{v}, &{tv})")


# ====================================================================== routes and dependencies


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


class RouteBuilder:
    def __init__(self, proj: Project):
        self.p = proj
        self.fe = proj.fe

    def is_ext(self, module, node, scope, *names) -> bool:
        if node is None:
            return False
        t = self.p.resolve(module, node, scope)
        return is_ext(t, *names)

    SECURITY = {"OAuth2PasswordBearer": ("OAuth2Bearer", {"tokenUrl", "scheme_name", "scopes", "description", "refreshUrl"}),
                "HTTPBearer": ("HttpBearer", {"bearerFormat", "scheme_name", "description"}),
                "HTTPBasic": ("HttpBasic", {"scheme_name", "description"})}

    def security_scheme(self, t: Sym, src) -> tuple[str, bool] | None:
        """`scheme = OAuth2PasswordBearer(...)` / `HTTPBearer(...)` / `HTTPBasic(...)` at module level, used as `Depends(scheme)`.
        Other fastapi.security classes are refused (their errors and challenges differ)."""
        d = self.p.ix.definition(t)
        if not (isinstance(d, ast.Assign) and isinstance(d.value, ast.Call)):
            return None
        call = d.value
        target = self.p.resolve(t.module, call.func, None)
        if not (isinstance(target, Ext) and target.package == "fastapi"):
            return None
        cls = target.dotted.split(".")[-1]
        file = str(self.p.ix.module(t.module).path)
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
        return variant, auto_error

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

            def find(kinds):
                return next(((m, mm, ms) for m, mm, ms in metas if isinstance(m, ast.Call) and (dotted(m.func) or "").split(".")[-1] in kinds), (None, None, None))

            dep_call, dmod, dscope = find({"Depends", "Security"})
            pcall, pmod, pscope = find({"Query", "Path", "Body", "Header", "Cookie", "Form", "File"})
            name = arg.arg
            sync_sess = self.is_ext(amod, ann, ascope, "sqlalchemy.orm.Session", "sqlalchemy.orm.session.Session")
            if sync_sess and not (dep_call is not None and dep_call.args):
                raise TranspileError(f"{name}: a synchronous Session parameter needs Depends(<session dependency>)", arg, src)
            if sync_sess or self.is_ext(amod, ann, ascope, "sqlalchemy.ext.asyncio.AsyncSession", "sqlalchemy.ext.asyncio.session.AsyncSession"):
                if dep_call is not None and dep_call.args:
                    t = self.p.resolve(dmod, dep_call.args[0], dscope)
                    if isinstance(t, Sym):
                        self.p.use_session_dep(t)
                out.append(RParam(name, "session"))
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
                    if not (isinstance(cls_t, Sym) and cls_t in self.p.fe.schema_syms):
                        raise TranspileError(f"{fn.name}: Depends() without a callable is only supported for AsyncSession "
                                             "and Pydantic models", arg, src)
                    if dep_call.keywords:
                        raise TranspileError("Depends() options are not supported on a class dependency", dep_call, src)
                    out.append(RParam(name, "dep", dep=self.class_dependency(cls_t, src)))
                    continue
                t = self.p.resolve(dmod, dep_call.args[0], dscope)
                sec = self.security_scheme(t, src) if isinstance(t, Sym) else None
                if sec is not None:
                    if dep_call.keywords:
                        raise TranspileError("Depends(security_scheme, ...) options are not supported", dep_call, src)
                    out.append(RParam(name, "security", security=sec))
                    continue
                if not isinstance(t, Sym) or not isinstance(self.p.ix.definition(t), (ast.FunctionDef, ast.AsyncFunctionDef)):
                    raise TranspileError(f"{fn.name}: dependency `{ast.unparse(dep_call.args[0])}` must be a project function", arg, src)
                for kw in dep_call.keywords:
                    if kw.arg != "use_cache":
                        raise TranspileError(f"Depends({kw.arg}=) is not supported", kw, src)
                    if not (isinstance(kw.value, ast.Constant) and kw.value.value is True):
                        raise TranspileError("Depends(use_cache=False) is not supported (the result is cached per request)", kw, src)
                out.append(RParam(name, "dep", dep=t))
                continue
            if self.is_ext(amod, ann, ascope, "fastapi.WebSocket", "starlette.websockets.WebSocket", "fastapi.websockets.WebSocket"):
                out.append(RParam(name, "websocket"))
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
            if ann is None:
                raise TranspileError(f"parameter `{name}` needs a type annotation", arg, src)
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
                if source == "cookie":
                    raise TranspileError("Cookie() parameters are not supported yet", pcall, src)
            elif default is not None:
                opts["default"] = default
            upload = self.upload_kind(amod, ann, ascope)
            if source in {"form", "file"} or upload:
                if for_dep:
                    raise TranspileError(f"{name}: Form()/File() parameters of a dependency are not supported", arg, src)
            if source == "file" or upload:
                if upload is None:
                    raise TranspileError(f"{name}: File() parameters must be UploadFile or list[UploadFile]", arg, src)
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
            td = self.p.td(ann, amod, cons, ascope)
            p = RParam(name, "query", td)
            if "default" in opts or "default_factory" in opts:
                p.required = False
                factory = "default_factory" in opts
                spec = self.p.default_spec(opts["default_factory" if factory else "default"], module, factory)
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
                if any(i.startswith(f"{RT}::pyd::TD::{k}") for k in ("Schema", "Dict", "List", "Set", "Tuple")):
                    return True
                if depth < 5 and (i.startswith(f"{RT}::pyd::TD::Optional") or i.startswith(f"{RT}::pyd::TD::Union")):
                    return any(complex_td(by_name[r], depth + 1) for r in re.findall(r"&(\w+)", i) if r in by_name)
                return False

            is_model = complex_td(init)
            is_dictlike = False
            if source == "form":
                p.kind = "form"
            elif source == "header":
                p.kind = "header"
                conv = True
                for kw in pcall.keywords:
                    if kw.arg == "convert_underscores":
                        conv = bool(self.p.const(kw.value, module))
                p.alias = p.alias or (name.replace("_", "-") if conv else name)
            elif source == "body" or (source is None and (is_model or is_dictlike) and name not in path_names):
                p.kind = "body"
                if opts.get("embed") is not None and self.p.const(opts["embed"], module):
                    p.kind = "body_embed"
            elif name in path_names or source == "path":
                p.kind = "path"
            out.append(p)
        return out

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

    def needs_body(self, params) -> bool:
        """FastAPI reads (and parses) the request body only when the route or one of its dependencies
        has a body parameter."""
        dep_body = self.p.__dict__.setdefault("dep_body", {})
        return any(p.kind in {"body", "body_embed"} or (p.kind == "dep" and dep_body.get(p.dep, False)) for p in params)

    def dep_solver(self, sym: Sym) -> str:
        """`dep_x(cx, body, errs) -> R<Option<V>>`: solve and call a dependency (cached per request)."""
        # the caller depends on it on every call, cached or not (errors are attributed through the edges)
        if self.p.cur is not None:
            self.p.edges.setdefault(self.p.cur, set()).add(("dep", sym))
        errors = self.p.__dict__.setdefault("dep_errors", {})
        if sym in errors:
            raise errors[sym]  # every route using a refused dependency is blocked, not only the first
        if sym in self.p.deps:
            return self.p.deps[sym]
        name = f"dep_{mod_ident(sym.module)}__{ident(sym.name)}"
        self.p.deps[sym] = name
        prev, self.p.cur = self.p.cur, ("dep", sym)
        try:
            return self._dep_solver(sym, name)
        except TranspileError as e:
            del self.p.deps[sym]
            errors[sym] = e
            raise
        finally:
            self.p.cur = prev

    def check_dep_yield(self, fn, sym: Sym, src) -> None:
        """A generator dependency: one `yield`, as a statement of the body or alone in a `try:` that has
        only a `finally:` (FastAPI raises the endpoint's exception at the `yield`: an `except` around it
        would see it, the binary cannot reproduce that)."""
        yields = [n for n in walk_scope(fn) if isinstance(n, (ast.Yield, ast.YieldFrom))]
        if len(yields) != 1 or isinstance(yields[0], ast.YieldFrom):
            raise TranspileError(f"dependency {sym.name}: exactly one `yield` is supported", yields[-1], src)
        y = yields[0]
        is_stmt = lambda st: isinstance(st, ast.Expr) and st.value is y  # noqa: E731
        for st in fn.body:
            if is_stmt(st):
                return
            if isinstance(st, ast.Try) and any(is_stmt(b) for b in st.body):
                if st.handlers or st.orelse:
                    raise TranspileError(f"dependency {sym.name}: `yield` inside try/except is not supported "
                                         "(only try/finally)", st, src)
                return
        raise TranspileError(f"dependency {sym.name}: the `yield` must be a statement of the function body "
                             "or of a top-level try/finally", y, src)

    def _dep_solver(self, sym: Sym, name: str) -> str:
        fn = self.p.ix.definition(sym)
        src = self.p.src(sym.module)
        gen = has_yield(fn)
        if gen:
            self.check_dep_yield(fn, sym, src)
        params = self.params(fn, sym.module, set(), for_dep=True)
        self.p.__dict__.setdefault("dep_body", {})[sym] = self.needs_body(params)
        body, args = self.solve(params, "__body")
        kinds = self.p.__dict__.setdefault("dep_kinds", {})
        kinds[sym] = {(p.kind, sym.name, src, fn.lineno) for p in params if p.kind != "dep"}
        for p in params:
            if p.kind == "dep":
                kinds[sym] |= kinds.get(p.dep, set())
        rust = self.p.function(sym, "depgen" if gen else "plain")
        key = sym.qual
        if gen:
            caps = [f"c{i}" for i in range(len(args))]
            call = (f"{{ {' '.join(f'let {c} = {a}.clone();' for c, a in zip(caps, args))} "
                    f"{RT}::web::dep_gen(cx, move |cx2, y| Box::pin(async move {{ {rust}(&cx2{''.join(', ' + c for c in caps)}, &y).await }})).await? }}")
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

    def solve(self, params: list[RParam], body_var: str) -> tuple[list[str], list[str]]:
        lines, args = [], []
        n_body = sum(p.kind in {"body", "body_embed"} for p in params)
        # FastAPI: sub-dependencies first (in declaration order), then path/query/header, then body
        for p in params:
            if p.kind == "dep":
                solver = self.dep_solver(p.dep)
                lines.append(f"let p_{ident(p.name)} = {solver}(cx, {body_var}, __errs).await?.unwrap_or(V::None);")
            elif p.kind == "security":
                variant, auto_error = p.security
                lines.append(f"let p_{ident(p.name)} = {RT}::web::security(cx, {RT}::web::Security::{variant}, {str(auto_error).lower()})?;")
            elif p.kind == "session":
                lines.append(f"let p_{ident(p.name)} = {RT}::session(cx).await?;")
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
        for kind in ("path", "query", "header"):
            for p in params:
                if p.kind == kind:
                    d = p.default or ("dflt_unbound" if p.dyn_default else "dflt_none")
                    lines.append(
                        f"let p_{ident(p.name)} = {RT}::web::param(cx, {rs(kind)}, {rs(p.name)}, {rs(p.alias or p.name)}, "
                        f"&{p.td}, {str(p.required).lower()}, {d}, __errs).await?;"
                    )
        for p in params:
            d = p.default or ("dflt_unbound" if p.dyn_default else "dflt_none")
            if p.kind == "body" and n_body == 1:
                lines.append(f"let p_{ident(p.name)} = {RT}::web::body_param(cx, {body_var}, &{p.td}, {str(p.required).lower()}, {d}, __errs).await?;")
            elif p.kind == "form":
                lines.append(f"let p_{ident(p.name)} = {RT}::web::form_field(cx, &__form, {rs(p.alias or p.name)}, &{p.td}, "
                             f"{str(p.required).lower()}, {d}, __errs).await?;")
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
        has_rm = False
        for kw in deco.keywords:
            if kw.arg == "response_model":
                has_rm = True
                if not (isinstance(kw.value, ast.Constant) and kw.value.value is None):
                    response_model = self.p.td(kw.value, module, None, fn)
            elif kw.arg == "status_code":
                status = int(self.p.const(kw.value, module, fn))
            elif kw.arg in {"tags", "summary", "description", "responses", "deprecated", "operation_id",
                            "include_in_schema", "name", "response_description", "response_model_exclude_none"}:
                if kw.arg == "response_model_exclude_none":
                    raise TranspileError("response_model_exclude_none= is not supported yet", kw, src)
            elif kw.arg == "dependencies":
                pass
            elif kw.arg == "response_class":
                # a returned Response is sent as is; any other value is encoded, then wrapped in this class
                t = self.p.resolve(module, kw.value, fn)
                last = t.dotted.split(".")[-1] if isinstance(t, Ext) and t.dotted.split(".")[0] in {"fastapi", "starlette"} else None
                if last not in {"StreamingResponse", "FileResponse", "RedirectResponse", "JSONResponse", "Response",
                                "HTMLResponse", "PlainTextResponse"}:
                    raise TranspileError(f"response_class={ast.unparse(kw.value)} is not supported "
                                         "(only the fastapi/starlette response classes)", kw, src)
                resp_cls = last
            else:
                raise TranspileError(f"unsupported route option {kw.arg}=", kw, src)
        if not has_rm and fn.returns is not None:
            r = fn.returns
            if not self.is_ext(module, r, fn, "fastapi.Response", "starlette.responses.Response",
                               "fastapi.responses.StreamingResponse", "starlette.responses.StreamingResponse"):
                # FastAPI validates and filters the response with the return annotation
                response_model = self.p.td(r, module, None, fn)
        params = self.params(fn, module, path_names)
        lines: list[str] = []
        pre_deps: list[ast.AST] = []
        r = self.fe.routers.get(router) if router else None
        for node, file in mount.deps:
            pre_deps.append((node, file))
        if r is not None and r.deps is not None:
            pre_deps.append((r.deps, r.file))
        for kw in deco.keywords:
            if kw.arg == "dependencies":
                pre_deps.append((kw, src))
        pre_syms: list[Sym] = []
        for node, file in pre_deps:
            value = node.value
            mod = self.p.ix.module_of(file).name
            elts = value.elts if isinstance(value, (ast.List, ast.Tuple)) else []
            for e in elts:
                if not (isinstance(e, ast.Call) and (dotted(e.func) or "").split(".")[-1] in {"Depends", "Security"} and e.args):
                    raise TranspileError("dependencies=[...] must contain Depends(function)", e, file)
                t = self.p.resolve(mod, e.args[0])
                if not isinstance(t, Sym):
                    raise TranspileError(f"dependency `{ast.unparse(e.args[0])}` must be a project function", e, file)
                lines.append(f"let _ = {self.dep_solver(t)}(cx, &__body, __errs).await?;")
                pre_syms.append(t)
        body_lines, args = self.solve(params, "&__body")
        lines += body_lines
        endpoint = self.endpoint_fn(fn, module, src)
        name = f"route_{idx}_{ident(fn.name)}"
        has_body = self.needs_body(params) or any(self.p.__dict__.get("dep_body", {}).get(t) for t in pre_syms)
        rm = f"Some(&{response_model})" if response_model else "None"
        self.p.items.append(
            f"/// {method.upper()} {self.p.fe.shown_path(path)}  (from {Path(src).name}:{fn.lineno} `{fn.name}`)\n"
            f"async fn {name}(cx: &Cx) -> R<axum::response::Response> {{\n"
            + (f"    let __body = {RT}::web::read_body(cx)?;\n" if has_body else "    let __body: Option<V> = None;\n")
            + (f"    let __form = {RT}::web::read_form(cx).await?;\n" if any(p.kind in {"form", "file", "oauth2form"} for p in params) else "")
            + f"    let mut __errv: Vec<{RT}::pyd::ErrDetail> = Vec::new();\n"
            f"    let __errs = &mut __errv;\n"
            + "".join(f"    {line}\n" for line in lines)
            + f"    {RT}::web::check(__errv)?;\n"
            + (f"    {RT}::web::stream_list_ok(cx, {rs(endpoint)}, &{response_model});\n"
               if response_model and resp_cls in {None, "JSONResponse"} and self.p.__dict__.get("stream_lists", True)
               and has_list_return(fn) else "")
            + f"    let __ret = {endpoint}(cx{''.join(', ' + a for a in args)}).await?;\n"
            + (f"    {RT}::web::respond(cx, __ret, {rm}, {status or 200}).await\n}}" if resp_cls in {None, "JSONResponse"} else
               f"    {RT}::web::respond_as(cx, __ret, {rm}, {f'Some({status})' if status else 'None'}, Some({rs(resp_cls)})).await\n}}")
        )
        handler = f"run_{name}"
        self.p.items.append(
            f"fn {handler}<'a>(cx: &'a Cx) -> std::pin::Pin<Box<dyn std::future::Future<Output = R<axum::response::Response>> + Send + 'a>> "
            f"{{ Box::pin({name}(cx)) }}"
        )
        return handler, starlette_pattern(path, fn, src)

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
                  "body": "a body", "body_embed": "a body", "form": "a Form()", "file": "a File()",
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
        for node, file in pre_deps:
            mod = self.p.ix.module_of(file).name
            elts = node.value.elts if isinstance(node.value, (ast.List, ast.Tuple)) else []
            for e in elts:
                if not (isinstance(e, ast.Call) and (dotted(e.func) or "").split(".")[-1] in {"Depends", "Security"} and e.args):
                    raise TranspileError("dependencies=[...] must contain Depends(function)", e, file)
                t = self.p.resolve(mod, e.args[0])
                if not isinstance(t, Sym):
                    raise TranspileError(f"dependency `{ast.unparse(e.args[0])}` must be a project function", e, file)
                lines.append(f"let _ = {self.dep_solver(t)}(cx, &__body, __errs).await?;")
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


def ws_route(proj: Project, rb: RouteBuilder, fe: Frontend, fn, m, found, idx: int, out: list, collect: bool) -> int:
    """`@app.websocket` / `@router.websocket`: one compiled route per mount, appended to `out` as
    (pattern, handler, fn, module); returns the last route index used."""
    deco, router = found
    src = str(m.path)
    for mt in (fe._mounts_cache.get(router, []) if router else [Mount("")]):
        idx += 1
        r = fe.routers.get(router) if router else None
        rid = ("route", idx)
        proj.cur = rid
        info = {"method": "websocket", "path": "?", "func": fn.name, "file": src, "line": fn.lineno,
                "conditional": mt.conditional, "node": fn, "module": m.name, "router": router, "id": rid}
        try:
            if not deco.args:
                raise TranspileError("@websocket(...) needs a path", deco, src)
            path = mt.prefix + (r.prefix if r else "") + literal(deco.args[0], src)
            info["path"] = fe.shown_path(path)
            for k in re.findall("\x01(\\d+)\x01", path):
                proj.edges.setdefault(rid, set()).add(("prefix", int(k)))
            if fe.shown_path(path) in fe.__dict__.get("python_side", ()):
                raise TranspileError(f"WebSocket route {fe.shown_path(path)} cannot be declared --python-side: the binary "
                                     "does not relay WebSocket connections", deco, src)
            if r is not None and r.unsupported:
                raise TranspileError(f"unsupported APIRouter option {r.unsupported[0].arg}=", r.unsupported[0], r.file)
            if mt.unsupported:
                node, f = mt.unsupported[0]
                raise TranspileError(f"unsupported include_router option {getattr(node, 'arg', '?')}=", node, f)
            if mt.conditional:
                raise TranspileError("include_router(...) under an `if` is not supported: the binary would "
                                     "serve the route unconditionally", fn, src)
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
    for init, td in p.tds.items():
        if "TD::Union(" not in init:
            continue
        node, module = p.__dict__.get("union_src", {}).get(init, (None, None))
        for bad, what in ((refs(td) & has_before, "mode='before' validators"),
                          (refs(td) & has_after, "validators, which would make Pydantic try the next member when they raise")):
            if bad:
                msg = f"a Union containing {names.get(sorted(bad)[0], sorted(bad)[0])} ({what}) is not supported"
                raise p.err(msg, node, module) if node is not None else TranspileError(msg)
    for info in list(p.schemas.values()):
        fields = []
        for f in info.fields:
            ann, module = f["ann"]
            cons = dict(f.get("cons", {}))
            if info.__dict__.get("strip"):
                cons["_strip"] = True
            if info.__dict__.get("lower"):
                cons["_lower"] = True
            if info.__dict__.get("upper"):
                cons["_upper"] = True
            if info.__dict__.get("enum_values"):
                cons["_enum_values"] = True
            td = p._static_td(f"{RT}::pyd::TD::Any") if f.get("any") else p.td(ann, module, cons)
            alias = f"Some({rs(f['alias'])})" if f["alias"] else "None"
            fields.append(
                f"{RT}::pyd::FieldDesc {{ name: {rs(f['name'])}, alias: {alias}, td: &{td}, "
                f"default: {RT}::pyd::Dflt::{f['default']}, env: None, validate_default: {str(bool(f.get('validate_default'))).lower()} }}"
            )
        validators = ", ".join(
            f"{RT}::pyd::ValidatorDesc {{ fields: &[{', '.join(rs(n) for n in names)}], f: {method_wrapper(p, s)}, "
            f"info: {info.__dict__.get('vinfo', {}).get(s, 0)} }}"
            for names, s in info.validators
        )
        methods = ", ".join(f"({rs(n)}, {str(prop).lower()}, {method_wrapper(p, s)} as {RT}::pyd::MethodFn)" for n, prop, s in info.methods)
        cattrs = info.__dict__.get("class_attrs", {})
        if cattrs:
            methods = ", ".join(x for x in [methods, *(f"({rs(n)}, true, {g}_prop as {RT}::pyd::MethodFn)" for n, g in cattrs.items())] if x)
        out.append(
            f"pub static {info.cls}: {RT}::v::Class = {RT}::v::Class {{ name: {rs(info.sym.name)}, qualname: {rs(info.sym.qual)}, "
            f"bases: &[{('&' + info.__dict__['base_cls']) if info.__dict__.get('base_cls') else ''}], kind: {RT}::v::ClassKind::Schema(&{info.rust}) }};\n"
            f"pub static {info.rust}: {RT}::pyd::SchemaDesc = {RT}::pyd::SchemaDesc {{ name: {rs(info.sym.name)}, class: &{info.cls}, "
            f"fields: &[{', '.join(fields)}], from_attributes: {str(info.from_attributes).lower()}, "
            f"extra: {RT}::pyd::Extra::{info.extra}, validators: &[{validators}], "
            f"validate_assignment: {str(bool(info.__dict__.get('validate_assignment'))).lower()}, "
            f"populate_by_name: {str(bool(info.__dict__.get('populate_by_name'))).lower()}, methods: &[{methods}], "
            f"open: {str(bool(info.__dict__.get('open'))).lower()}, "
            f"model_after: &[{', '.join(f'{method_wrapper(p, m)} as {RT}::pyd::MethodFn' for m in info.__dict__.get('model_after', []))}], "
            f"before: &[{', '.join(f'{RT}::pyd::ValidatorDesc {{ fields: &[{chr(44).join(rs(n) for n in names)}], f: {method_wrapper(p, s_)}, info: 0 }}' for names, s_ in info.__dict__.get('before', []))}], "
            f"model_before: &[{', '.join(f'{method_wrapper(p, m)} as {RT}::pyd::MethodFn' for m in info.__dict__.get('model_before', []))}], "
            f"has_before: {str(info.rust in has_before).lower()}, "
            f"frozen: {str(bool(info.__dict__.get('frozen'))).lower()}, "
            f"post_init: {('Some(' + method_wrapper(p, info.__dict__['post_init']) + ' as ' + RT + '::pyd::MethodFn)') if info.__dict__.get('post_init') else 'None'}, "
            f"hash: {RT}::pyd::HashKind::{hash_kind(info)}, dataclass: {str(bool(info.__dict__.get('dataclass'))).lower()}, "
            f"async_methods: {async_names(p, info.methods)}, "
            f"slots: &[{', '.join(rs(x) for x in info.__dict__.get('slots', []))}], "
            f"settings: {settings_desc(info)}, "
            f"computed: &[{', '.join(computed_entry(p, info, n) for n in info.__dict__.get('computed', []))}], "
            f"json_schema: {('Some(' + rs(p.json_schemas()[0][info.sym]) + ')') if info.sym in p.json_schemas()[0] else 'None'}, "
            f"init: {('Some(' + method_wrapper(p, info.__dict__['init']) + ' as ' + RT + '::pyd::MethodFn)') if info.__dict__.get('init') else 'None'}, "
            f"private: &[{', '.join(f'({rs(n)}, {RT}::pyd::Dflt::{d})' for n, d in info.__dict__.get('private', []))}] }};"
        )
    # INSERT order: rank = 1 + the highest rank of the tables a model references (cycles cut)
    by_table = {m.table: m for m in p.models.values()}
    ranks: dict[str, int] = {}

    def rank(table: str, seen: frozenset = frozenset()) -> int:
        if table not in ranks:
            refs = [t for t in by_table[table].fk_tables if t != table and t in by_table and t not in seen]
            ranks[table] = 1 + max((rank(t, seen | {table}) for t in refs), default=-1)
        return ranks[table]

    for t in by_table:
        rank(t)
    for info in p.models.values():
        cols = []
        for c in info.cols:
            d = f"{RT}::orm::ColDefault::{c['default']}" if c["default"] else f"{RT}::orm::ColDefault::None"
            u = f"{RT}::orm::ColDefault::{c['onupdate']}" if c["onupdate"] else f"{RT}::orm::ColDefault::None"
            cols.append(
                f"{RT}::orm::ColDesc {{ name: {rs(c['name'])}, sql: {rs(c.get('sql') or c['name'])}, ty: {RT}::orm::ColTy::{c['ty']}, nullable: {str(c['nullable']).lower()}, "
                f"pk: {str(c['pk']).lower()}, autoincrement: {str(c['autoincrement']).lower()}, default: {d}, "
                f"server_default: {str(c['server_default']).lower()}, onupdate: {u}, deferred: {str(c['deferred']).lower()}, "
                f"jsonb: {str(c['jsonb']).lower()} }}"
            )
        methods = ", ".join(f"({rs(n)}, {str(prop).lower()}, {method_wrapper(p, s)} as {RT}::pyd::MethodFn)" for n, prop, s in info.methods)
        b = lambda x: str(bool(x)).lower()  # noqa: E731
        rels = ", ".join(
            f"{RT}::orm::RelDesc {{ name: {rs(r['name'])}, target: &{p.models[r['target']].rust}, m2o: {b(r['m2o'])}, "
            f"local: {r['local']}, remote: {r['remote']}, uselist: {b(r['uselist'])}, lazy: {RT}::orm::Lazy::{r['lazy']}, "
            f"back: {'Some(' + rs(r['back']) + ')' if r['back'] else 'None'}, delete: {b(r['delete'])}, "
            f"orphan: {b(r['orphan'])}, passive_deletes: {b(r['passive_deletes'])}, "
            f"order: &[{', '.join(f'({i}, {b(d)})' for i, d in r.get('order', []))}] }}"
            for r in (info.rels or [])
        )
        fks = ", ".join(f"({i}, {rs(c['fk'])}, {rs(c.get('fk_col') or 'id')})" for i, c in enumerate(info.cols) if c.get("fk"))
        out.append(
            f"pub static {info.cls}: {RT}::v::Class = {RT}::v::Class {{ name: {rs(info.sym.name)}, qualname: {rs(info.sym.qual)}, "
            f"bases: &[], kind: {RT}::v::ClassKind::Model(&{info.rust}) }};\n"
            f"pub static {info.rust}: {RT}::orm::ModelDesc = {RT}::orm::ModelDesc {{ name: {rs(info.sym.name)}, "
            f"class_qualname: {rs(info.sym.qual)}, table: {rs(info.table)}, class: &{info.cls}, cols: &[{', '.join(cols)}], "
            f"pk: {info.pk}, pks: &[{', '.join(str(i) for i in info.pks)}], fk_tables: &[{', '.join(rs(t) for t in info.fk_tables)}], "
            f"fks: &[{fks}], "
            f"rank: {ranks[info.table]}, "
            f"methods: &[{methods}], class_methods: &[{', '.join(rs(n) for n in class_level_methods(p, info.methods))}], "
            f"rels: &[{rels}], async_methods: {async_names(p, info.methods)} }};"
        )
    # TDs last: emitting schemas may have created new ones
    for init, name in p.tds.items():
        out.append(f"static {name}: {RT}::pyd::TD = {init};")
    return out


PLAIN_DECORATORS = {"lru_cache", "cache", "staticmethod", "classmethod", "property"}


def async_names(p: "Project", methods) -> str:
    """`&["m", ...]`: the methods of a class table that are `async def`s (not generators)."""
    out = []
    for n, prop, s in methods:
        try:
            node = p.fn_node(s)
        except TranspileError:
            continue
        if isinstance(node, ast.AsyncFunctionDef) and not has_yield(node) and not prop:
            out.append(rs(n))
    return "&[" + ", ".join(out) + "]"

# dunder methods the runtime dispatches to (str(), repr(), ==/!= and the containers' comparisons, `async for`)
DISPATCHED_DUNDERS = {"__str__", "__repr__", "__eq__", "__aenter__", "__aexit__", "__enter__", "__exit__", "__call__",
                      "__aiter__", "__anext__", "__getattr__", "__setattr__"}


def is_dunder(name: str) -> bool:
    return name.startswith("__") and name.endswith("__")


def settings_desc(info) -> str:
    """A BaseSettings class's environment options, for calling the class held as a value (`type(s)()`)."""
    if not info.settings:
        return "None"
    none = f"Some({rs(info.env_none)})" if info.env_none is not None else "None"
    return (f"Some({RT}::pyd::SettingsDesc {{ prefix: {rs(info.env_prefix)}, "
            f"case_sensitive: {'true' if info.case_sensitive else 'false'}, none_str: {none} }})")


def hash_kind(info) -> str:
    """`hash()` of an instance, CPython's rules: `__hash__ = None` or `__eq__` without `__hash__` →
    unhashable; Pydantic models and dataclasses: by value when frozen, else unhashable (they define
    `__eq__`); plain classes: identity."""
    if info.__dict__.get("unhashable"):
        return "Unhashable"
    if info.__dict__.get("open"):
        return "Unhashable" if any(n == "__eq__" for n, _, _ in info.methods) else "Id"
    return "Value" if info.__dict__.get("frozen") else "Unhashable"


def value_decorators(d) -> list:
    """Decorators of a project `def` applied as values (anything but lru_cache & co); mixing both kinds
    is refused."""
    decos = [d for d in d.decorator_list]
    last = [((dotted(x.func) if isinstance(x, ast.Call) else dotted(x)) or "").split(".")[-1] for x in decos]
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
    rust = p.function(sym)
    name = f"mw_{rust}"
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
        p.items.append(
            f"fn {name}<'a>(cx: &'a Cx, slf: V, args: Vec<V>) -> {RT}::BoxFut<'a> {{ Box::pin(async move {{\n"
            f"    let (args, kwargs) = {RT}::unpack(args);\n"
            f"    let mut __s = {RT}::bind_params({rs(sym.name + '()')}, {0 if lead == 'static' else 1}, args, kwargs, &[{spec}])?;\n"
            f"    let _ = &mut __s;\n"
            f"    {rust}(cx, {first}{', '.join(vals)}).await\n}}) }}"
        )
    return name


CARGO = """\
[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

# Generated by py2axum (dyn backend) from {source}. Do not edit by hand: change the Python and regenerate.

[dependencies]
axum = {{ version = "0.8", features = ["ws"] }}
tokio = {{ version = "1", features = ["rt-multi-thread", "macros", "net", "sync", "time", "signal", "io-util"] }}
futures-util = "0.3"
sqlx = {{ version = "0.8", default-features = false, features = ["runtime-tokio", "postgres", "macros", "chrono", "json", "uuid"] }}
serde = {{ version = "1", features = ["derive"] }}
serde_json = {{ version = "1", features = ["preserve_order", "float_roundtrip"] }}
form_urlencoded = "1"
chrono = {{ version = "0.4", default-features = false, features = ["std", "clock"] }}
chrono-tz = "0.10"
indexmap = "2"
parking_lot = "0.12"
regex = "1"
sha2 = "0.10"
sha1 = "0.10"
md-5 = "0.10"
hex = "0.4"
rand = "0.8"
base64 = "0.22"
subtle = "2"
hmac = "0.12"
flate2 = {{ version = "1", default-features = false, features = ["zlib"] }}
aes = "0.8"
multer = "3"
fancy-regex = "0.14"
minijinja = {{ version = "2", features = ["loader"] }}
lettre = {{ version = "0.11", default-features = false, features = ["smtp-transport", "tokio1", "tokio1-rustls", "rustls-native-certs", "ring"] }}
cbc = {{ version = "0.1", features = ["alloc"] }}
bcrypt = "0.17"
num-bigint = "0.4"
num-integer = "0.1"
num-traits = "0.2"
url = "2"
sysinfo = {{ version = "0.33", default-features = false, features = ["system"] }}
libc = "0.2"
p256 = {{ version = "0.13", features = ["ecdh", "ecdsa", "pkcs8"] }}
aes-gcm = "0.10"
hkdf = "0.12"
rsa = {{ version = "0.9", features = ["sha2"] }}
x509-cert = {{ version = "0.2", features = ["pem"] }}
reqwest = {{ version = "0.12", default-features = false, features = ["rustls-tls", "gzip", "deflate", "stream"] }}
sentry = {{ version = "0.46", default-features = false, features = ["reqwest", "rustls"] }}
unicode-normalization = "0.1"
redis = {{ version = "0.27", default-features = false, features = ["tokio-comp", "connection-manager", "aio"] }}
lapin = "4"
ammonia = "4.1"

{extra}
[profile.release]
lto = "fat"
codegen-units = 1
"""


MAIN = """\
// Generated by py2axum (dyn backend). Do not edit.
#[macro_use]
mod dynrt;
mod gen;

use std::sync::Arc;

fn env_or(key: &str, default: &str) -> String {{
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}}

fn main() {{
    // Runtime threads get a large stack (reserved address space, touched only as deep as a request goes):
    // CPython accepts JSON nested ~10 000 levels deep (more on 3.14) and the runtime walks such values
    // recursively (validation, serialisation, repr); tokio's 2 MiB default would let one small request
    // overflow it and abort the whole process. PY2AXUM_STACK_SIZE (bytes) overrides it.
    let stack = std::env::var("PY2AXUM_STACK_SIZE").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(dynrt::STACK_SIZE);
    dynrt::set_stack_size(stack);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack)
        .on_thread_start(dynrt::stack_thread_start)
        .build()
        .expect("tokio runtime")
        .block_on(serve());
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
    dynrt::web::set_strict_content_type({strict_ct});
    dynrt::web::set_starlette({starlette_major}, {starlette_minor});
    dynrt::web::set_response_dump_json({dump_json});
    dynrt::methods::set_str_classes(&gen::STR_CLASSES);
    dynrt::set_pydantic("{pydantic}");
    dynrt::sentry::set_sdk("{sentry_sdk}", {multipart});
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
    // FastAPI(lifespan=...): its code before `yield` runs before the server listens, the rest after it stops
    let lcx = dynrt::root_cx();
    let lifespan = match gen::lifespan(&lcx).await {{
        Ok(None) => None,
        Ok(Some(f)) => match dynrt::web::lifespan_start(&lcx, f).await {{
            Ok(cm) => Some(cm),
            Err(e) => {{
                eprintln!("ERROR:    {{:?}}\nERROR:    Application startup failed. Exiting.", e);
                std::process::exit(3);
            }}
        }},
        Err(e) => {{
            eprintln!("ERROR:    {{:?}}\nERROR:    Application startup failed. Exiting.", e);
            std::process::exit(3);
        }}
    }};
    let addr = format!("{{}}:{{}}", env_or("HOST", "0.0.0.0"), env_or("PORT", "8080"));
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    eprintln!("listening on http://{{addr}}");
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
    if let Some(cm) = lifespan {{
        if let Err(e) = dynrt::web::lifespan_end(&lcx, cm).await {{
            eprintln!("ERROR:    {{:?}}\nERROR:    Application shutdown failed. Exiting.", e);
        }}
    }}
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
                cons = dict(f.get("cons", {}))
                if info.__dict__.get("strip"):
                    cons["_strip"] = True
                if info.__dict__.get("lower"):
                    cons["_lower"] = True
                if info.__dict__.get("upper"):
                    cons["_upper"] = True
                if info.__dict__.get("enum_values"):
                    cons["_enum_values"] = True
                try:
                    p.td(ann, module, cons)
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
            return


def prepare(fe: Frontend, python_side: set[str]):
    """Run the discovery passes of the frontend that the dyn backend shares (no IR building)."""
    fe._load_imports()
    fe._classify()
    fe._discover_apps()
    collect = fe.collect
    fe.collect = True
    gzip = fe._middlewares()
    fe.collect = collect
    kept = []
    for e in fe.global_errors:
        txt = e.render()
        if (e.node is not None and e.file and stack_node(fe, fe.index.module_of(e.file).name, e.node)
                and not ("add_route" in txt and (_route_path(fe, e) in python_side or fe.__dict__.get("auto_side") is not None))):
            continue  # compiled into the middleware stack (generate_project)
        if "add_route" in txt and isinstance(e.node, ast.Call) and e.node.args:
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
                    elif kw.arg == "strict_content_type":
                        if isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, bool):
                            fe.strict_ct = kw.value.value
                        else:
                            kept.append(TranspileError("FastAPI(strict_content_type=...) must be a literal True or False",
                                                       kw, str(m.path)))
                    else:
                        kept.append(TranspileError(f"FastAPI({kw.arg}=...) is not supported", kw, str(m.path)))
    fe._discover_routers()
    fe._mounts_cache = fe._mounts()
    if kept and not fe.collect:
        raise kept[0]
    return gzip


MW_CORS = {"starlette.middleware.cors.CORSMiddleware", "fastapi.middleware.cors.CORSMiddleware"}
MW_BASE = {"starlette.middleware.base.BaseHTTPMiddleware", "fastapi.middleware.base.BaseHTTPMiddleware"}
MW_GZIP = {"starlette.middleware.gzip.GZipMiddleware", "fastapi.middleware.gzip.GZipMiddleware"}


# what registers a route on the app (call or decorator), in Starlette's routing order
APP_ROUTE_CALLS = {"include_router", "mount", "host", "add_api_route", "add_route", "add_websocket_route",
                   "add_api_websocket_route"}
APP_ROUTE_DECOS = {"get", "post", "put", "patch", "delete", "head", "options", "trace", "api_route", "route",
                   "websocket", "websocket_route"}


def _is_mount(e: TranspileError) -> bool:
    return (isinstance(e.node, ast.Call) and isinstance(e.node.func, ast.Attribute) and e.node.func.attr == "mount"
            and "is not supported" in e.msg)


def mount_not_last(fe: Frontend, e: TranspileError) -> str | None:
    """`app.mount(...)`: why it is not among the app's last registrations (only mounts follow it, in the same
    module and scope), else None. Starlette's router tries routes in order and a mount matches every path under
    its prefix: a route registered after it would be shadowed in Python but served by the binary."""
    from .frontend import _enclosing_fn

    mod = fe.index.module_of(e.file).name
    parents = fe._parents(mod)
    scope = _enclosing_fn(e.node, parents)

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
                if node.func.attr == "mount":
                    continue
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
            if m.name != mod:
                return f"{what} is in another module (the registration order depends on the imports)"
            if _enclosing_fn(at, ps) is not scope:
                return f"{what} is in another function (the registration order depends on the calls)"
            if where.lineno > e.node.lineno:
                return f"{what} comes after it"
    return None


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
                else "context" if c in MW_CONTEXT else None)
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
    if not isinstance(kw.value, (ast.List, ast.Tuple)):
        raise TranspileError("FastAPI(middleware=...) must be a literal list of Middleware(...)", kw.value, str(fe.index.module(module).path))
    return list(kw.value.elts)


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
            return mw_kind(fe, module, node) in {"cors", "base", "user", "asgi", "context"}
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
        if (isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute) and n.func.attr in CONFIGURE_REFUSED
                and isinstance(n.func.value, ast.Name) and n.func.value.id in bound):
            raise TranspileError(f"{fn.name}(): `{n.func.value.id}.{n.func.attr}(...)` on the application passed to a "
                                 "function is not supported (register routes and handlers in the factory or at module level)",
                                 n, p.src(t.module))


def _stack_relevant(fe: Frontend, module: str, node: ast.AST) -> bool:
    for n in ast.walk(node):
        if isinstance(n, ast.Call) and (stack_node(fe, module, n) or configure_call(fe, module, n)
                                       or (isinstance(n.func, (ast.Name, ast.Attribute))
                                           and dotted(n.func) in {"FastAPI", "fastapi.FastAPI"}
                                           and any(k.arg == "middleware" for k in n.keywords))):
            return True
    return False


def target_python(root) -> tuple[int, int]:
    """The Python the project runs on: the lower bound of `requires-python` in its uv.lock, else the
    transpiler's own."""
    import sys
    d = Path(root).resolve()
    for cand in (d, *d.parents):
        lock = cand / "uv.lock"
        if lock.is_file():
            m = re.search(r'requires-python = ">=\s*(\d+)\.(\d+)', lock.read_text())
            if m:
                return int(m.group(1)), int(m.group(2))
            break
        if (cand / ".git").exists():
            break
    return sys.version_info[:2]


def fastapi_at_least(root, version: tuple[int, int]) -> bool:
    v = locked_version(root, "fastapi") or "0.142"
    return tuple(int(x) for x in re.findall(r"\d+", v)[:2]) >= version


def fastapi_strict_content_type(root) -> bool:
    """FastAPI 0.132+ does not decode a body without Content-Type as JSON (`strict_content_type`, True by default)."""
    return fastapi_at_least(root, (0, 132))


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


def lifespan_fn(p: "Project", fe: Frontend) -> str:
    """`pub async fn lifespan`: the value of `FastAPI(lifespan=...)` (an `@asynccontextmanager` function or an
    async generator function, called with the app before the server listens), None without one."""
    spec = fe.__dict__.get("lifespan")
    if spec is None:
        return "pub async fn lifespan(_cx: &Cx) -> R<Option<V>> {\n    Ok(None)\n}"
    module, value = spec
    t = p.resolve(module, value) if isinstance(value, (ast.Name, ast.Attribute)) else None
    d = p.ix.definition(t) if isinstance(t, Sym) else None
    if not isinstance(d, ast.AsyncFunctionDef) or not has_yield(d):
        e = TranspileError("FastAPI(lifespan=...): only an `async def` generator of the project (decorated with "
                           "@asynccontextmanager or not) is supported", value, p.src(module))
        if not p.collect:
            raise e
        fe.global_errors.append(e)
        return "pub async fn lifespan(_cx: &Cx) -> R<Option<V>> {\n    Ok(None)\n}"
    for n in walk_scope(d):
        if isinstance(n, ast.Yield) and n.value is not None and not (isinstance(n.value, ast.Constant) and n.value.value is None):
            e = TranspileError("lifespan state (`yield <value>` in the lifespan) is not supported: use module globals",
                               n, p.src(t.module))
            if not p.collect:
                raise e
            fe.global_errors.append(e)
            return "pub async fn lifespan(_cx: &Cx) -> R<Option<V>> {\n    Ok(None)\n}"
    p.cur = ("lifespan", 0)
    fc = FnCompiler(p, module, None, "lifespan")
    try:
        code = fc.expr(value)
        p.drain()
    except TranspileError as e:
        if not p.collect:
            raise
        fe.global_errors.append(e)
        return "pub async fn lifespan(_cx: &Cx) -> R<Option<V>> {\n    Ok(None)\n}"
    finally:
        p.cur = None
    body = "\n".join("    " + l for l in fc.lines)
    return (f"/// FastAPI(lifespan={ast.unparse(value)}) ({p.rel_file(module)}:{value.lineno})\n"
            f"pub async fn lifespan(cx: &Cx) -> R<Option<V>> {{\n{body}\n    Ok(Some({code}))\n}}")


def _reaches_sentry_init(p: "Project", module: str, call: ast.Call, depth: int = 4, seen=None) -> bool:
    """Does this call run `sentry_sdk.init(...)`, directly or through project functions?"""
    t = p.ix.resolve_expr(module, call.func)
    if is_ext(t, "sentry_sdk.init"):
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
    (`init_sentry()`): run at startup with the module globals, as the factory runs them at import. The
    factory's other plain statements are not translated (its middlewares and handlers are, by the stack)."""
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
    return frozenset({k.split(".")[0] for t in tables for k in t} | {"sentry_sdk"})


def unmapped_library(t) -> bool:
    """A third-party library the runtime maps nothing of (not the standard library)."""
    import sys
    if not isinstance(t, Ext):
        return False
    root = t.dotted.split(".")[0]
    return root not in _mapped_roots() and root not in sys.stdlib_module_names


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


def module_statements(p: "Project", fe: Frontend) -> None:
    """Module-level call statements (`INI.set_main_option(...)`, `logger.setLevel(...)`) of the modules the
    translation uses, compiled to run at startup in their place among the module's globals."""
    used = {k[0].module for k in p.fns if isinstance(k[0], Sym)} | {s.module for s in p.globals if isinstance(s, Sym)}
    for m in fe.index.package_modules():
        if m.name not in used:
            continue
        for st in m.tree.body:
            if isinstance(st, ast.Expr) and isinstance(st.value, ast.Call):
                f = st.value.func
                if isinstance(f, ast.Attribute) and (f.attr in FRAMEWORK_CALLS or fe._is_app(m.name, f.value)):
                    continue
                if configure_call(fe, m.name, st.value):
                    continue  # run by the middleware stack
                t = p.resolve(m.name, f)
                if isinstance(t, Ext) and t.dotted in DDL_ONLY_CALLS:
                    continue  # `Index(Model.col, ...)`: attaches to the table's DDL, compiled with create_all
            elif isinstance(st, (ast.Assign, ast.AugAssign)) and any(
                    isinstance(t, (ast.Subscript, ast.Attribute)) for t in (st.targets if isinstance(st, ast.Assign) else [st.target])):
                # `REG["k"] = ...`, `obj.attr = ...`: fills a global in place (the plain names it binds are globals)
                if any(fe._is_app(m.name, b) for b in _store_bases(st)):
                    continue  # `app.state.x = ...`: the frontend's
                if all(unmapped_library(p.resolve(m.name, b)) for b in _store_bases(st)):
                    # `stripe.api_key = ...`: configures a library the runtime does not know, so no native code can
                    # read it (any use of the library is refused where it occurs); the routes using it run
                    # --python-side, where the module runs as written
                    continue
                cls = next((t for t in (st.targets if isinstance(st, ast.Assign) else [st.target])
                            if isinstance(t, ast.Attribute) and isinstance(p.resolve(m.name, t.value), Sym)
                            and isinstance(p.ix.definition(p.resolve(m.name, t.value)), ast.ClassDef)), None)
                if cls is not None:
                    e = TranspileError(f"assigning the class attribute `{ast.unparse(cls)}` at module level is not supported "
                                       "(class attributes are read as declared): set it in the class body", st, p.src(m.name))
                    if not p.collect:
                        raise e
                    fe.global_errors.append(e)
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
                    if not p.collect:
                        raise
                    fe.global_errors.append(TranspileError(f"module-level statement not translated: {e.msg}", e.node, e.file))
                continue
            name = f"modst_{mod_ident(m.name)}__{st.lineno}"
            fc = FnCompiler(p, m.name, None, name)
            p.cur = ("modst", name)
            try:
                fc.block([st])
                link_mutated_globals(p, m.name, st, ("modst", name))
            except TranspileError as e:
                if not p.collect:
                    raise
                fe.global_errors.append(TranspileError(f"module-level statement not translated: {e.msg}", e.node, e.file))
                continue
            finally:
                p.cur = None
            body = "\n".join("    " + l for l in fc.lines)
            p.items.append(f"/// {m.name}:{st.lineno} (module level, run at startup)\npub async fn {name}(cx: &Cx) -> R {{\n{body}\n    Ok(V::None)\n}}")
            p.__dict__.setdefault("eager", []).append((m.name, st.lineno, name))
            p.__dict__.setdefault("modst", set()).add(name)


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
        for st in body:
            if not _stack_relevant(fe, module, st):
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
                n = next(n for n in ast.walk(st) if isinstance(n, ast.Call) and stack_node(fe, module, n))
                raise TranspileError(f"{ast.unparse(n)[:60]} inside `{type(st).__name__}` is not supported "
                                     "(only at the top level of the module or of the app factory, or under an `if`)",
                                     n, p.src(module))

    def entry(fc: "FnCompiler", module: str, e: ast.AST) -> None:
        """`Middleware(cls, *args, **kwargs)` of `FastAPI(middleware=[...])`"""
        f = fe.index.resolve_expr(module, e.func) if isinstance(e, ast.Call) else None
        if not (isinstance(f, Ext) and libmap.canonical(f.dotted) in MW_ENTRY and e.args):
            raise TranspileError("FastAPI(middleware=[...]) entries must be Middleware(cls, ...)", e, p.src(module))
        if mw_class_kind(fe, module, e.args[0]) == "gzip":
            raise TranspileError("Middleware(GZipMiddleware) in FastAPI(middleware=[...]) is not supported "
                                 "(use app.add_middleware(GZipMiddleware, ...))", e, p.src(module))
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
        elif kind == "asgi":
            # Starlette: cls(app, *args, **kwargs) once the stack is built; `app` = the rest of the stack
            t = fe.index.resolve_expr(module, call.args[0])
            p.stack_classes.append(t)
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
            found = fe._route_decorator(fn, m.name, parents)
            ws = fe._ws_decorator(fn, m.name, parents)
            if found is None and ws is None:
                continue
            deco, router = found or ws
            r = fe.routers.get(router) if router else None
            try:
                path = (r.prefix if r else "") + literal(deco.args[0], str(m.path))
                pattern = starlette_pattern(path, fn, str(m.path))
            except (TranspileError, IndexError):
                continue  # the route itself is refused
            name = next((k.value.value for k in deco.keywords if k.arg == "name" and isinstance(k.value, ast.Constant)), fn.name)
            ident_ = f"RN_{len(nodes) + 1}"
            nodes[(m.name, fn.lineno)] = ident_
            if found is None:
                items.append(f"static {ident_}: {RT}::routing::Node = {RT}::routing::Node::Ws {{ path: {rs(path)}, "
                             f"pattern: {rs(pattern)}, name: {rs(name)} }};")
            else:
                items.append(f"static {ident_}: {RT}::routing::Node = {RT}::routing::Node::Api {{ path: {rs(path)}, pattern: {rs(pattern)}, "
                             f"methods: &[{rs(deco.func.attr.upper())}], name: {rs(name)} }};")
            members.setdefault(router or "app", []).append(((order.get(m.name, 0), fn.lineno), ident_))
    routers: dict[object, str] = {}

    def router_def(sym) -> str:
        if sym not in routers:
            routers[sym] = f"RT_{mod_ident(sym.module)}__{ident(sym.name)}"
            items.append(f"static {routers[sym]}: {RT}::routing::RouterDef = {RT}::routing::RouterDef {{ "
                         f"prefix: {rs(fe.routers[sym].prefix)}, routes: &[{', '.join('&' + n for n in owned(sym))}], error: \"\" }};")
        return routers[sym]

    def owned(owner) -> list[str]:
        out = list(members.get(owner, []))
        for i, (own, child, call, file, *_) in enumerate(fe.__dict__.get("include_edges", [])):
            if own != owner:
                continue
            prefix = next((k.value.value for k in call.keywords if k.arg == "prefix" and isinstance(k.value, ast.Constant)), "")
            # a runtime prefix (settings): its marker, resolved at run time
            prefix = next((f"\x01{j}\x01" for k in call.keywords if k.arg == "prefix"
                           for j, (v, _, _) in enumerate(fe.dyn_prefixes) if v is k.value), prefix)
            node = f"RI_{len(items) + 1}_{i}"
            items.append(f"static {node}: {RT}::routing::Node = {RT}::routing::Node::Included {{ prefix: {rs_marked(prefix)}, "
                         f"router: &{router_def(child)} }};")
            mod = next((mm.name for mm in fe.index.package_modules() if str(mm.path) == file), "")
            out.append(((order.get(mod, 0), call.lineno), node))
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


def fastapi_docs_routes(fe: Frontend) -> tuple[list[tuple[str, str]] | None, str]:
    """The routes `FastAPI(...)` adds for its documentation, with its literal `*_url` options (else None and
    the error raised when the routes are inspected)."""
    opts = {"openapi_url": "/openapi.json", "docs_url": "/docs", "redoc_url": "/redoc",
            "swagger_ui_oauth2_redirect_url": "/docs/oauth2-redirect"}
    for m in fe.index.package_modules():
        for node in ast.walk(m.tree):
            if (isinstance(node, (ast.Assign, ast.AnnAssign)) and isinstance(node.value, ast.Call)
                    and is_ext(fe.index.resolve_expr(m.name, node.value.func), "fastapi.FastAPI")):
                for k in node.value.keywords:
                    if k.arg in opts:
                        if not isinstance(k.value, ast.Constant):
                            return None, f"py2axum: app.routes needs a literal FastAPI({k.arg}=) ({m.path}:{k.value.lineno})"
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


def generate_project(fe: Frontend, out_dir: Path, source: str, crate_name: str, gzip=None, collect: bool = False,
                     stream: bool = True) -> Project:
    """Compile every route of the project with the dyn backend and write the Rust crate (`stream=False`:
    list responses are always buffered, `--no-stream`)."""
    proj = Project(fe, collect=collect)
    proj.__dict__["stream_lists"] = stream
    # functions that fail to translate become stubs that raise; the build fails only if a translated
    # route can reach one (below)
    proj.__dict__["defer_fn_errors"] = True
    for lib in ("httpx", "aiohttp", "uvloop"):
        v = locked_version(fe.index.root, lib)
        if v:
            libmap.LIB_VERSIONS[lib] = v
    rb = RouteBuilder(proj)
    routes = []
    idx = 0
    prefix_fns, empty_under = [], {}
    for i, (value, file, module) in enumerate(fe.dyn_prefixes):
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
    ws_routes = []
    for m in fe.index.package_modules():
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
            wsf = fe._ws_decorator(fn, m.name, parents)
            if wsf is not None:
                idx = ws_route(proj, rb, fe, fn, m, wsf, idx, ws_routes, collect)
            found = fe._route_decorator(fn, m.name, parents)
            if found is None:
                continue
            deco, router = found
            mounts = fe._mounts_cache.get(router, []) if router else [Mount("")]
            for mt in mounts:
                idx += 1
                r = fe.routers.get(router) if router else None
                path = mt.prefix + (r.prefix if r else "") + literal(deco.args[0], str(m.path))
                if fe.shown_path(path) in fe.__dict__.get("python_side", ()):
                    # proxied by its runtime path: a prefix read from the settings stays a marker until startup
                    fe.__dict__.setdefault("python_side_raw", set()).add(path)
                    fe.notes.append(f"{m.path}:{fn.lineno}: {deco.func.attr.upper()} {fe.shown_path(path)} declared Python-side")
                    continue
                rid = ("route", idx)
                for k in re.findall("\x01(\\d+)\x01", path):
                    proj.edges.setdefault(rid, set()).add(("prefix", int(k)))
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
                    handler, axum_path = rb.route(fn, m.name, path, deco.func.attr, deco, router, mt, idx)
                    proj.cur = None
                    proj.drain()
                    routes.append((deco.func.attr, axum_path, handler, path, fn, m, router))
                    proj.__dict__.setdefault("route_infos", []).append((info, None))
                except TranspileError as e:
                    proj.cur = None
                    if not collect:
                        raise
                    proj.errors.append(e)
                    proj.__dict__.setdefault("route_infos", []).append((info, e))
    engine_globals(proj, fe)
    module_statements(proj, fe)
    factory_sentry_init(proj, fe)
    proj.cur = ("stack", 0)
    try:
        stack = build_stack(proj, fe)
    except TranspileError as e:
        if not collect:
            raise
        fe.global_errors.append(e)
        stack = ""
    proj.cur = None
    mcp_register(proj, fe)
    lifespan = lifespan_fn(proj, fe)
    finalize(proj)
    if collect:
        # a middleware that cannot be compiled blocks every route
        keys = {("stack", 0), ("lifespan", 0), ("mcp", 0)} | {k for k in proj.fn_errors
                                 if any(isinstance(c, Sym) and k[0].module == c.module and k[0].name.startswith(c.name + ".")
                                        for c in proj.__dict__.get("stack_classes", []))}
        seen = set()
        for k in keys:
            for e in closure_errors(proj, k):
                if id(e) not in seen:
                    seen.add(id(e))
                    fe.global_errors.append(e)
        return proj
    for info, err in proj.__dict__.get("route_infos", []):
        for e in ([err] if err else []) + closure_errors(proj, info["id"]):
            raise e
    stack_keys = {("stack", 0), ("lifespan", 0), ("mcp", 0)} | {k for k in proj.fn_errors
                                   if any(isinstance(c, Sym) and k[0].module == c.module and k[0].name.startswith(c.name + ".")
                                          for c in proj.__dict__.get("stack_classes", []))}
    for k in stack_keys:
        for e in closure_errors(proj, k):
            raise e
    proj.items.append(stack)
    proj.items.append(lifespan)
    # module globals (and the app factory's locals) evaluated at startup, as importing the app does
    order = {name: i for i, name in enumerate(import_order(fe))}
    keep = set(proj.globals.values()) | proj.__dict__.get("modst", set())
    eager = sorted({e for e in proj.__dict__.get("eager", []) if e[2] in keep or e[2].startswith(("fl_", "gd_", "ca_"))},
                   key=lambda e: (order.get(e[0], 0), e[1], e[2]))
    proj.items.append(
        "/// Module globals evaluated at startup like Python's import (a failure is logged; the routes reading\n"
        "/// the value then raise it).\n"
        "pub async fn init_globals(cx: &Cx) {\n"
        + "".join(f"    if let Err(e) = {g}(cx).await {{ eprintln!(\"ERROR:py2axum:module global {g}: {{:?}}\", e); }}\n"
                  for _, _, g in eager)
        + "}")

    proj.items.append(
        "/// The runtime `include_router(prefix=...)` values, read once the module globals are set.\n"
        "pub async fn init_prefixes(cx: &Cx) -> R<()> {\n"
        f"    {RT}::web::set_prefixes(vec![" + "".join(
            f"({f}(cx).await?, {'Some(' + rs(empty_under[i]) + ')' if i in empty_under else 'None'}), "
            for i, f in enumerate(prefix_fns)) + "])\n}")
    body = emit_descriptors(proj)
    # the project's schema / plain / enum classes, found by name when unpickling
    picklable = sorted({m.group(1) for it in [*proj.items, *body] for m in re.finditer(
        r"pub static (CLS_\w+): [\w:]*Class = [^;]*?ClassKind::(?:Schema|Enum)\(", it)})
    body.append("pub fn register_classes() {\n    " + f"{RT}::pickle::register(&[{', '.join('&' + c for c in picklable)}]);\n}}")
    # declaration order, as Starlette tries them
    tree, nodes = route_tree(proj, fe)
    proj.items.append(tree)
    route_defs = [f"    {RT}::web::RouteDef {{ method: {rs(method.upper())}, pattern: {rs_marked(pattern)}, run: {handler}, "
                  f"node: {('Some(&' + nodes[(m.name, fn.lineno)] + ')') if (m.name, fn.lineno) in nodes else 'None'}, "
                  f"path: {rs_marked(path)}, src: ({rs(proj.rel_file(m.name) + ':' + str(fn.lineno))}, {rs(fn.name)}, {rs(m.name)}), "
                  f"sync_endpoint: {str(not isinstance(fn, ast.AsyncFunctionDef)).lower()} }},"
                  for method, pattern, handler, path, fn, m, _ in routes]
    ws_defs = [f"    {RT}::ws::WsRouteDef {{ pattern: {rs_marked(pattern)}, run: {handler}, "
               f"node: {('Some(&' + nodes[(m.name, fn.lineno)] + ')') if (m.name, fn.lineno) in nodes else 'None'} }},"
               for pattern, handler, fn, m in ws_routes]
    if any(isinstance(n, (ast.Import, ast.ImportFrom)) and "unicodedata" in ast.unparse(n)
           for m in list(proj.ix.modules.values()) for n in ast.walk(m.tree)):
        proj.items.append(ucd_unassigned())
    proj.items.append(str_classes())
    gen = [
        "// Generated by py2axum (dyn backend) from the FastAPI project. Do not edit.",
        "#![allow(unused_mut, unused_variables, unused_imports, unreachable_code, dead_code, non_snake_case, unused_parens, unused_labels, unused_assignments, non_upper_case_globals, clippy::all)]",
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
        f"pub fn router(_app: std::sync::Arc<{RT}::AppState>) -> axum::Router<std::sync::Arc<{RT}::AppState>> {{",
        "    axum::Router::new().fallback(|axum::extract::State(app): axum::extract::State<std::sync::Arc<"
        f"{RT}::AppState>>, req: axum::extract::Request| {RT}::asgi::app(app, req, ROUTES, WS_ROUTES, stack))",
        "}",
    ]
    (out_dir / "src").mkdir(parents=True, exist_ok=True)
    rt_dir = out_dir / "src" / "dynrt"
    if rt_dir.exists():
        shutil.rmtree(rt_dir)
    shutil.copytree(RUNTIME_DIR, rt_dir)
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
    (out_dir / "Cargo.toml").write_text(CARGO.format(name=crate_name, source=source, extra=extra))
    # the dependency versions the runtime is tested with (an existing lock file is kept)
    lock = Path(__file__).parent / "runtime" / "Cargo.lock"
    if lock.exists() and not (out_dir / "Cargo.lock").exists():
        (out_dir / "Cargo.lock").write_text(lock.read_text().replace('name = "dynapp_axum"', f'name = "{crate_name}"'))
    commit, expire, autoflush, sync = getattr(proj, "session_cfg", (False, proj.expire_on_commit, True, False))
    b = lambda x: str(x).lower()  # noqa: E731
    pyver = target_python(fe.index.root)
    (out_dir / "src" / "main.rs").write_text(MAIN.format(expire=b(expire), autoflush=b(autoflush), commit=b(commit), sync=b(sync), layers=layers,
                                                         pymajor=pyver[0], pyminor=pyver[1],
                                                         strict_ct=b(fe.__dict__.get("strict_ct", fastapi_strict_content_type(fe.index.root))),
                                                         starlette_major=starlette_version(proj)[0], starlette_minor=starlette_version(proj)[1],
                                                         dump_json=b(fastapi_at_least(fe.index.root, (0, 130))),
                                                         pydantic=".".join((locked_version(fe.index.root, "pydantic") or "2.13").split(".")[:2]),
                                                          sentry_sdk=locked_version(fe.index.root, "sentry-sdk") or "2.71.0",
                                                          multipart=b(locked_version(fe.index.root, "python-multipart") is not None),
                                                          python_side=", ".join(rs_marked(x) for x in sorted({x for x in fe.__dict__.get("python_side", ()) if x.startswith("/")}
                                                                                                        | fe.__dict__.get("python_side_raw", set()))),
                                                          mount_fallback=", ".join(rs(x) for x in sorted(set(fe.__dict__.get("mount_fallback", []))))))
    (out_dir / "src" / "gen.rs").write_text("\n".join(gen) + "\n")
    if shutil.which("rustfmt"):
        subprocess.run(["rustfmt", "--edition", "2021", str(out_dir / "src" / "gen.rs"), str(out_dir / "src" / "main.rs")], check=False,
                       capture_output=True)
    return proj


# keyword arguments the runtime implements for these methods (others raise TypeError at run time)
METHOD_KWARGS = {
    "model_dump": {"mode", "exclude_none", "exclude_unset", "by_alias", "exclude", "include"},
    "model_dump_json": {"exclude_none", "exclude_unset", "by_alias", "exclude", "include"},
    "model_copy": {"update", "deep"},
}

# calls whose string arguments or keywords name attributes of the objects they make or change
ATTR_NAMING_CALLS = {"label", "setattr", "getattr", "hasattr", "namedtuple", "NamedTuple", "make_dataclass",
                     "SimpleNamespace", "column", "literal_column", "alias", "type"}

_RUNTIME_NAMES: set[str] | None = None
_RUNTIME_ATTRS: set[str] | None = None


def runtime_attr_names() -> set[str]:
    """The names the runtime dispatches on (`runtime_names`), compares to (`name == "x"`), keeps in a table
    (`("x", value)`, `&["x", "y"]`) or declares as a field (`name: "x"`): over-approximates the attributes
    some runtime value provides, so a name outside it is certainly missing."""
    global _RUNTIME_ATTRS
    if _RUNTIME_ATTRS is None:
        names, ident = set(runtime_names()), r'"([A-Za-z_][A-Za-z0-9_]*)"'
        for f in (Path(__file__).parent / "runtime" / "dynrt").glob("*.rs"):
            src = f.read_text()
            for pat in (rf"==\s*{ident}", rf"{ident}\s*==", rf"\(\s*{ident}\s*,", rf"\bname:\s*{ident}"):
                names |= set(re.findall(pat, src))
            for table in re.findall(r'&\[((?:\s*"[A-Za-z_][A-Za-z0-9_]*"\s*,?)+)\s*\]', src):
                names |= set(re.findall(ident, table))
        _RUNTIME_ATTRS = names
    return _RUNTIME_ATTRS


@functools.lru_cache(maxsize=1)
def str_classes() -> str:
    """`str.isdigit/isdecimal/isnumeric/isalpha/isspace/islower/isupper` (and the titlecase letters) of the
    translating Python, as code point ranges: Rust's
    char predicates follow other Unicode properties ('¾' is numeric there, not a digit in Python; U+001C is
    a Python space)."""
    import unicodedata

    def ranges(pred) -> str:
        out, start = [], None
        for cp in range(0x110001):
            ok = cp <= 0x10FFFF and not 0xD800 <= cp <= 0xDFFF and pred(chr(cp))
            if ok and start is None:
                start = cp
            elif not ok and start is not None:
                out.append(f"({start:#x}, {cp - 1:#x})")
                start = None
        return "&[" + ", ".join(out) + "]"
    fields = ", ".join(f"{k}: {ranges(getattr(str, 'is' + k))}" for k in ("digit", "decimal", "numeric", "alpha", "space", "lower", "upper"))
    fields += f", title: {ranges(lambda c: unicodedata.category(c) == 'Lt')}"
    return (f"/// Unicode {unicodedata.unidata_version} (the translating Python): the str.is*() classes\n"
            f"pub static STR_CLASSES: {RT}::methods::StrClasses = {RT}::methods::StrClasses {{ {fields} }};")


def ucd_unassigned() -> str:
    """The code points unassigned in this Python's `unicodedata` (category Cn): it leaves them alone, while the
    runtime's tables (a newer Unicode) may decompose or class them. Assigned characters never change their
    decomposition or combining class (Unicode stability policy), so these ranges are the whole difference."""
    import unicodedata

    ranges, start = [], None
    for cp in range(0x110001):
        cn = cp <= 0x10FFFF and unicodedata.category(chr(cp)) == "Cn"
        if cn and start is None:
            start = cp
        elif not cn and start is not None:
            ranges.append(f"({start:#x}, {cp - 1:#x})")
            start = None
    return (f"/// Unicode {unicodedata.unidata_version} (the translating Python): unassigned ranges\n"
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
            names |= set(re.findall(r'"([A-Za-z_][A-Za-z0-9_]*)"(?=\s*(?:\||=>|if\b|\)\s*=>))', f.read_text()))
        names |= set(re.findall(r"""["']([A-Za-z_][A-Za-z0-9_]*)["']""", Path(libmap.__file__).read_text()))
        _RUNTIME_NAMES = names
    return _RUNTIME_NAMES


def class_level_methods(p: Project, methods) -> list[str]:
    """The `@staticmethod`/`@classmethod`s of a class: the methods read on the class itself."""
    out = []
    for n, _prop, sym in methods:
        decos = {((dotted(d.func) if isinstance(d, ast.Call) else dotted(d)) or "").split(".")[-1]
                 for d in p.fn_node(sym).decorator_list}
        if decos & {"staticmethod", "classmethod"}:
            out.append(n)
    return out


def closure_errors(p: Project, start) -> list[TranspileError]:
    """Errors of every function reachable from a route (endpoint, dependencies, callees)."""
    seen, todo, out = set(), [start], []
    while todo:
        n = todo.pop()
        if n in seen:
            continue
        seen.add(n)
        if n in p.fn_errors:
            out.append(p.fn_errors[n])
        if isinstance(n, tuple) and n[0] == "class" and n[1] in p.class_errors:
            out.append(p.class_errors[n[1]])
        todo.extend(p.edges.get(n, ()))
    return out

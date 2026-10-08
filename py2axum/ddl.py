"""`await conn.run_sync(Base.metadata.create_all)`: the DDL SQLAlchemy would emit, computed at translation time.

The mapped classes of the declarative base are rebuilt as real SQLAlchemy classes from the source, without running
the project: each expression that matters to the DDL (column types and options, `__table_args__`, `Table(...)`
objects, the base's `metadata`/`type_annotation_map`, enums, TypeDecorator `impl`) is evaluated by a small static
evaluator that only calls SQLAlchemy itself. Python-side values (`default=`, `onupdate=`) never reach the database
and are replaced by a placeholder (their presence still matters: a primary key with a default is not SERIAL).
`metadata.create_all()` then runs against a mock PostgreSQL engine, which yields the statements SQLAlchemy emits on
an empty database, in order: `CREATE TYPE` for every named enum, then per table `CREATE TABLE`, its indexes and
comments, then the foreign keys of cycles (`ALTER TABLE ... ADD CONSTRAINT`). The runtime replays SQLAlchemy's checkfirst (`dynrt::orm::create_all`): every enum type absent from
`pg_type` is created (even when its table exists, as `MetaData.before_create` does), then each table absent from
`pg_class` with its own statements, then the ALTERs of the tables it created. Anything not rebuilt faithfully is refused at its source line.
"""
from __future__ import annotations

import ast
import enum
import importlib
import re
import types
import warnings

from .ir import TranspileError
from .modules import Ext, ModRef, Sym, function_imports

# where values may come from: what these packages define is safe to build at translation time
REAL_ROOTS = {"sqlalchemy", "datetime", "decimal", "uuid", "typing", "typing_extensions", "enum", "ipaddress"}
BUILTINS = {"int": int, "str": str, "float": float, "bool": bool, "bytes": bytes, "dict": dict, "list": list,
            "set": set, "tuple": tuple, "frozenset": frozenset, "object": object, "type": type}
# mapped attributes without DDL (their columns, if any, are declared elsewhere)
NO_DDL = {"relationship", "relation", "backref", "dynamic_loader", "association_proxy", "synonym", "composite",
          "query_expression", "column_property", "hybrid_property", "WriteOnlyMapped"}
# column options that never reach the database; `default`-like ones keep their presence
CLIENT_ONLY = {"onupdate", "doc", "info", "repr", "init", "kw_only", "compare", "hash", "dataclass_metadata",
               "active_history", "deferred", "deferred_group", "deferred_raiseload", "use_existing_column"}
PRESENCE_ONLY = {"default", "insert_default", "default_factory"}
DDL_EVENTS = {"before_create", "after_create", "before_drop", "after_drop", "before_parent_attach",
              "after_parent_attach", "column_reflect"}


def _placeholder() -> None:
    """Stands for a Python-side default: same DDL as any default, nothing of the project runs."""


class DdlBuilder:
    """One declarative base's MetaData, rebuilt from the source of the modules the application imports."""

    def __init__(self, p, base: Sym, call_module: str, call_fn: ast.AST | None):
        self.p, self.ix, self.base = p, p.ix, base
        self.call_module, self.call_fn = call_module, call_fn
        self.memo: dict = {}  # Sym -> real object
        self.busy: set = set()

    def err(self, msg: str, node, module: str) -> TranspileError:
        return self.p.err(f"create_all: {msg}", node, module)

    # ---------------------------------------------------------------- which classes are in the metadata

    def imported_modules(self, roots: list[str]) -> list[str]:
        """Modules importing `roots` runs (module-level imports, parent packages first), in execution order."""
        order, seen = [], set()

        def visit(name: str) -> None:
            m = self.ix.module(name)
            if m is None or name in seen:
                return
            seen.add(name)
            parts = name.split(".")
            for i in range(1, len(parts)):
                visit(".".join(parts[:i]))
            for target in self.module_imports(m.tree.body, m.package):
                visit(target)
            order.append(name)

        for r in roots:
            visit(r)
        return order

    @staticmethod
    def module_imports(stmts, package: str) -> list[str]:
        """Modules imported by these statements (`import a.b` imports a and a.b; `from m import x` imports m and
        m.x when it is a module); `if`/`try` blocks at that level included."""
        out = []
        for node in stmts:
            if isinstance(node, ast.Import):
                for a in node.names:
                    parts = a.name.split(".")
                    out += [".".join(parts[:i]) for i in range(1, len(parts) + 1)]
            elif isinstance(node, ast.ImportFrom):
                base = node.module or ""
                if node.level:
                    parts = package.split(".") if package else []
                    parts = parts[: len(parts) - (node.level - 1)]
                    base = ".".join(x for x in [*parts, node.module or ""] if x)
                out.append(base)
                out += [f"{base}.{a.name}" for a in node.names if a.name != "*"]
            elif isinstance(node, (ast.If, ast.Try)):
                inner = list(node.body) + list(node.orelse)
                if isinstance(node, ast.Try):
                    for h in node.handlers:
                        inner += h.body
                    inner += node.finalbody
                out += DdlBuilder.module_imports(inner, package)
        return out

    def root_base(self, sym: Sym, seen=None) -> Sym | None:
        """The declarative base a mapped class descends from."""
        if sym in self.p.fe._declarative:
            return sym
        node = self.ix.definition(sym)
        seen = seen or set()
        if sym in seen or not isinstance(node, ast.ClassDef):
            return None
        seen.add(sym)
        for b in node.bases:
            t = self.ix.resolve_expr(sym.module, b)
            if isinstance(t, Sym):
                r = self.root_base(t, seen)
                if r is not None:
                    return r
        return None

    def build(self):
        """The real MetaData of the base, with every table of the modules imported when create_all runs."""
        fe = self.p.fe
        roots = [m.name for m in fe.index.package_modules() if m.name in fe.app_vars]
        mods = self.imported_modules(roots)
        if self.call_fn is not None:
            m = self.ix.module(self.call_module)
            lazy = [n for n in ast.walk(self.call_fn) if isinstance(n, (ast.Import, ast.ImportFrom))]
            mods += [x for x in self.imported_modules(self.module_imports(lazy, m.package)) if x not in mods]
        reachable = set(mods)
        # a module imported lazily by another function: whether it is imported before create_all runs is not static
        late = set()
        for m in list(self.ix.modules.values()):
            for fn in ast.walk(m.tree):
                if isinstance(fn, (ast.FunctionDef, ast.AsyncFunctionDef)) and fn is not self.call_fn:
                    for spec in function_imports(fn, m.package).values():
                        late.update([spec[1]] if spec[0] == "module" else [spec[1], f"{spec[1]}.{spec[2]}"])
        for sym, node in fe.model_syms.items():
            if sym.module not in reachable and sym.module in late and self.root_base(sym) == self.base:
                raise self.err(f"model {sym.name} is in a module only imported inside a function: whether its table "
                               "exists when create_all runs is not known statically (import it at module level)",
                               node, sym.module)
        base_obj = self.value(self.base, None, self.base.module)
        md = getattr(base_obj, "metadata", None)
        for mod in mods:
            m = self.ix.module(mod)
            for st in m.tree.body:
                if isinstance(st, ast.ClassDef):
                    sym = Sym(mod, st.name)
                    if sym in fe.model_syms and self.root_base(sym) == self.base:
                        self.value(sym, st, mod)
                else:
                    self.module_statement(st, mod, md)
        return md

    def module_statement(self, st: ast.stmt, module: str, md) -> None:
        """A module-level statement adding to the metadata (`Table(...)`, `Index(...)` on mapped columns) is
        evaluated in order; DDL event listeners and changes to an existing table are refused."""
        for n in ast.walk(st):
            if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda, ast.ClassDef)) and n is not st:
                continue
            fn = n.func if isinstance(n, ast.Call) else None
            t = self.ix.resolve_expr(module, fn) if fn is not None else None
            dotted = t.dotted if isinstance(t, Ext) else ""
            if dotted in {"sqlalchemy.event.listen", "sqlalchemy.event.listens_for"}:
                name = n.args[1] if len(n.args) > 1 else None
                if not (isinstance(name, ast.Constant) and isinstance(name.value, str)) or name.value in DDL_EVENTS:
                    raise self.err(f"`{ast.unparse(n)[:60]}`: a DDL event listener is not reproduced", n, module)
            if isinstance(n, ast.Attribute) and n.attr in {"append_constraint", "append_column", "_set_parent"}:
                raise self.err(f"`{ast.unparse(n)}`: changing a table after its definition is not reproduced", n, module)
        if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
            for d in st.decorator_list:
                self.module_statement(ast.Expr(d, lineno=d.lineno, col_offset=d.col_offset), module, md)
            return
        if isinstance(st, (ast.Import, ast.ImportFrom, ast.ClassDef)):
            return
        value = st.value if isinstance(st, (ast.Assign, ast.AnnAssign, ast.Expr)) else None
        if isinstance(value, ast.Call):
            t = self.ix.resolve_expr(module, value.func)
            last = t.dotted.rsplit(".", 1)[-1] if isinstance(t, Ext) and t.package == "sqlalchemy" else ""
            if last == "Table":
                meta = value.args[1] if len(value.args) > 1 else next((k.value for k in value.keywords if k.arg == "metadata"), None)
                targets = [tg for tg in (st.targets if isinstance(st, ast.Assign) else [getattr(st, "target", None)])
                           if isinstance(tg, ast.Name)]
                if any(Sym(module, tg.id) in self.memo for tg in targets):
                    return  # already built (referenced earlier)
                if meta is not None and self.ev(meta, module) is md:
                    obj = self.ev(value, module)
                    if isinstance(st, (ast.Assign, ast.AnnAssign)):
                        for tg in (st.targets if isinstance(st, ast.Assign) else [st.target]):
                            if isinstance(tg, ast.Name):
                                self.memo[Sym(module, tg.id)] = obj
            elif last in {"Index", "UniqueConstraint", "CheckConstraint", "ForeignKeyConstraint", "PrimaryKeyConstraint"}:
                cols = [a for a in value.args if not isinstance(a, ast.Constant)]
                if cols:  # attached to the table of the columns it names
                    self.ev(value, module)

    # ---------------------------------------------------------------- evaluation

    def value(self, sym: Sym, node, module: str):
        """The real object a project name stands for."""
        if sym in self.memo:
            return self.memo[sym]
        if sym in self.busy:
            raise self.err(f"`{sym.name}` refers to itself", node, module)
        d = self.ix.definition(sym)
        self.busy.add(sym)
        try:
            if isinstance(d, ast.ClassDef):
                obj = self.klass(sym, d)
            elif isinstance(d, (ast.Assign, ast.AnnAssign)) and d.value is not None:
                obj = self.ev(d.value, sym.module)
            elif isinstance(d, ast.FunctionDef):
                obj = ProjectFn(self, sym, d)
            else:
                raise self.err(f"`{sym.name}` cannot be evaluated at translation time", node or d, module)
        finally:
            self.busy.discard(sym)
        self.memo[sym] = obj
        return obj

    def real(self, dotted: str, node, module: str):
        parts = dotted.split(".")
        if parts[0] not in REAL_ROOTS:
            raise self.err(f"`{dotted}` is not evaluated at translation time (only SQLAlchemy and type constructs)",
                           node, module)
        for i in range(len(parts), 0, -1):
            try:
                obj = importlib.import_module(".".join(parts[:i]))
            except ImportError:
                continue
            try:
                for a in parts[i:]:
                    obj = getattr(obj, a)
            except AttributeError:
                raise self.err(f"`{dotted}` does not exist in the installed SQLAlchemy", node, module) from None
            return obj
        raise self.err(f"`{dotted}` cannot be imported", node, module)

    def target(self, t, node, module: str, env: dict | None):
        if isinstance(t, Sym):
            return self.value(t, node, module)
        if isinstance(t, Ext):
            return self.real(t.dotted, node, module)
        if isinstance(t, ModRef):
            return t
        raise self.err(f"`{ast.unparse(node)}` is not defined", node, module)

    def ev(self, node: ast.AST, module: str, env: dict | None = None, ann: bool = False):
        env = env or {}
        if isinstance(node, ast.Constant):
            if ann and isinstance(node.value, str):
                return self.ev(ast.parse(node.value, mode="eval").body, module, env, ann)
            return node.value
        if isinstance(node, ast.Name):
            if node.id in env:
                return env[node.id]
            t = self.ix.resolve(module, node.id)
            if t is None:
                if node.id in BUILTINS:
                    return BUILTINS[node.id]
                raise self.err(f"`{node.id}` is not defined", node, module)
            return self.target(t, node, module, env)
        if isinstance(node, ast.Attribute):
            root = node.value
            while isinstance(root, ast.Attribute):
                root = root.value
            if not (isinstance(root, ast.Name) and root.id in env):
                t = self.ix.resolve_expr(module, node)
                if t is not None:
                    return self.target(t, node, module, env)
            recv = self.ev(node.value, module, env, ann)
            if isinstance(recv, ModRef):
                return self.target(self.ix.resolve(recv.module, node.attr), node, module, env)
            return self.getattr(recv, node.attr, node, module)
        if isinstance(node, (ast.List, ast.Tuple, ast.Set)):
            vals = []
            for e in node.elts:
                if isinstance(e, ast.Starred):
                    vals += list(self.ev(e.value, module, env, ann))
                else:
                    vals.append(self.ev(e, module, env, ann))
            return {ast.List: list, ast.Tuple: tuple, ast.Set: set}[type(node)](vals)
        if isinstance(node, ast.Dict):
            out = {}
            for k, v in zip(node.keys, node.values):
                if k is None:
                    out.update(self.ev(v, module, env, ann))
                else:
                    out[self.ev(k, module, env, ann)] = self.ev(v, module, env, ann)
            return out
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.USub):
            return -self.ev(node.operand, module, env)
        if isinstance(node, ast.BinOp):
            left, right = self.ev(node.left, module, env, ann), self.ev(node.right, module, env, ann)
            if isinstance(node.op, ast.BitOr) and ann:
                return left | right
            if isinstance(node.op, ast.Add) and type(left) is type(right) and isinstance(left, (str, int, float, tuple, list)):
                return left + right
            raise self.err(f"`{ast.unparse(node)}` is not evaluated at translation time", node, module)
        if isinstance(node, ast.Subscript):
            recv = self.ev(node.value, module, env, ann)
            # Literal["a"] holds values, not forward references
            key = self.ev(node.slice, module, env, getattr(recv, "_name", None) != "Literal")
            if not self.typelike(recv):
                raise self.err(f"`{ast.unparse(node)}`: only type subscripts are evaluated", node, module)
            return recv[key]
        if isinstance(node, ast.Call):
            return self.call(node, module, env)
        if isinstance(node, ast.Lambda):
            return self.values_lambda(node, module)
        raise self.err(f"`{ast.unparse(node)[:60]}` is not evaluated at translation time", node, module)

    @staticmethod
    def origin(obj) -> str:
        mod = getattr(obj, "__module__", None)
        if not isinstance(mod, str):
            mod = type(obj).__module__
        return mod or ""

    def typelike(self, obj) -> bool:
        return isinstance(obj, (type, types.GenericAlias)) or self.origin(obj).split(".")[0] in {"typing", "typing_extensions", "sqlalchemy"}

    def getattr(self, recv, attr: str, node, module: str):
        if attr.startswith("__") and attr not in {"__table__", "__members__"}:
            raise self.err(f"`.{attr}` is not evaluated at translation time", node, module)
        if not (isinstance(recv, type) and (getattr(recv, "_py2axum_ddl", False) or issubclass(recv, enum.Enum))
                or self.origin(recv).split(".")[0] == "sqlalchemy"):
            raise self.err(f"`{ast.unparse(node)}`: attribute of a value not evaluated at translation time", node, module)
        try:
            return getattr(recv, attr)
        except AttributeError as e:
            raise self.err(f"`{ast.unparse(node)}`: {e}", node, module) from None

    def call(self, node: ast.Call, module: str, env: dict):
        f = self.ev(node.func, module, env)
        if isinstance(f, ProjectFn):
            return f(node, module, env)
        from sqlalchemy.types import TypeEngine
        project_type = isinstance(f, type) and getattr(f, "_py2axum_ddl", False) and issubclass(f, TypeEngine)
        if not (callable(f) and self.origin(f).split(".")[0] == "sqlalchemy" or project_type):
            raise self.err(f"`{ast.unparse(node.func)}(...)` is not evaluated at translation time (only SQLAlchemy "
                           "constructors and project functions returning one)", node, module)
        args = []
        for a in node.args:
            if isinstance(a, ast.Starred):
                args += list(self.ev(a.value, module, env))
            else:
                args.append(self.ev(a, module, env))
        kwargs = {}
        for k in node.keywords:
            if k.arg is None:
                kwargs.update(self.ev(k.value, module, env))
            elif k.arg in CLIENT_ONLY:
                continue
            elif k.arg in PRESENCE_ONLY:
                kwargs[k.arg] = None if isinstance(k.value, ast.Constant) and k.value.value is None else _placeholder
            else:
                kwargs[k.arg] = self.ev(k.value, module, env)
        for x in [*args, *kwargs.values()]:
            if self.origin(x) == "sqlalchemy.sql.schema" and type(x).__name__ == "Sequence":
                raise self.err("a Sequence (CREATE SEQUENCE) is not supported", node, module)
        try:
            return f(*args, **kwargs)
        except Exception as e:
            raise self.err(f"`{ast.unparse(node)[:80]}`: SQLAlchemy raised {type(e).__name__}: {e}", node, module) from None

    def values_lambda(self, node: ast.Lambda, module: str):
        """`values_callable=lambda x: [e.value for e in x]`: the one shape recognized (no project code runs)."""
        b = node.body
        a = node.args
        if (len(a.args) == 1 and not a.vararg and not a.kwarg and isinstance(b, ast.ListComp) and len(b.generators) == 1
                and isinstance(b.elt, ast.Attribute) and b.elt.attr in {"value", "name"}
                and isinstance(b.elt.value, ast.Name) and isinstance(b.generators[0].target, ast.Name)
                and b.elt.value.id == b.generators[0].target.id and not b.generators[0].ifs
                and isinstance(b.generators[0].iter, ast.Name) and b.generators[0].iter.id == a.args[0].arg):
            attr = b.elt.attr
            return lambda cls: [getattr(e, attr) for e in cls]
        raise self.err(f"`{ast.unparse(node)}`: only `lambda x: [e.value for e in x]` is evaluated", node, module)

    # ---------------------------------------------------------------- classes

    def klass(self, sym: Sym, node: ast.ClassDef):
        module = sym.module
        if node.decorator_list:
            raise self.err(f"class {sym.name}: decorators are not reproduced", node.decorator_list[0], module)
        kind = self.p.enum_kind(sym)
        if kind:
            return self.enum(sym, node, kind)
        bases = tuple(self.ev(b, module) for b in node.bases) or (object,)
        for b, bn in zip(bases, node.bases):
            b = getattr(b, "__origin__", b)  # TypeDecorator[str]
            if not isinstance(b, type) or not (getattr(b, "_py2axum_ddl", False) or self.origin(b).split(".")[0] == "sqlalchemy"
                                               or b is object):
                raise self.err(f"class {sym.name}: base `{ast.unparse(bn)}` is not reproduced", bn, module)
        kwds = {k.arg: self.ev(k.value, module) for k in node.keywords}
        env: dict = {}
        ns: dict = {"__module__": f"py2axum_ddl.{module}", "__qualname__": node.name, "__annotations__": {},
                    "_py2axum_ddl": True}
        for st in node.body:
            self.class_statement(sym, st, module, env, ns)
        try:
            cls = types.new_class(node.name, bases, kwds, lambda d: d.update(ns))
        except Exception as e:
            raise self.err(f"class {sym.name}: SQLAlchemy raised {type(e).__name__}: {e}", node, module) from None
        return cls

    def class_statement(self, sym: Sym, st: ast.stmt, module: str, env: dict, ns: dict) -> None:
        if isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant) or isinstance(st, ast.Pass):
            return
        if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
            for d in st.decorator_list:
                t = self.ix.resolve_expr(module, d.func if isinstance(d, ast.Call) else d)
                if isinstance(d, ast.Attribute):
                    t = t or self.ix.resolve_expr(module, d.value)
                if isinstance(t, Ext) and "declared_attr" in t.dotted:
                    raise self.err(f"{sym.name}.{st.name}: @declared_attr is computed at class creation and not "
                                   "reproduced", d, module)
            if st.name in {"load_dialect_impl", "get_col_spec", "__init__"} and self.is_type(sym):
                raise self.err(f"{sym.name}.{st.name}() changes the column type: not reproduced", st, module)
            return
        if isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name):
            name, ann, value = st.targets[0].id, None, st.value
        elif isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name):
            name, ann, value = st.target.id, st.annotation, st.value
        else:
            raise self.err(f"class {sym.name}: `{ast.unparse(st)[:50]}` is not reproduced", st, module)
        if ann is not None:
            head = ann.value if isinstance(ann, ast.Subscript) else ann
            t = self.ix.resolve_expr(module, head)
            if isinstance(t, Ext) and t.dotted.rsplit(".", 1)[-1] in {"ClassVar", "WriteOnlyMapped", "DynamicMapped"}:
                return
            if isinstance(ann, ast.Constant) and isinstance(ann.value, str) and ann.value.startswith("ClassVar"):
                return
        if isinstance(value, ast.Call):
            t = self.ix.resolve_expr(module, value.func)
            last = t.dotted.rsplit(".", 1)[-1] if isinstance(t, Ext) and t.package == "sqlalchemy" else ""
            if last in NO_DDL:
                if any(isinstance(x, ast.Call) and (ast.unparse(x.func).rsplit(".", 1)[-1] in {"Column", "mapped_column"})
                       for x in ast.walk(value) if x is not value):
                    raise self.err(f"{sym.name}.{name}: a column inside {last}() is not reproduced", value, module)
                return
        if value is not None:
            try:
                v = self.ev(value, module, env)
            except TranspileError:
                if ann is not None or name.startswith("__") or any(
                        isinstance(t := self.ix.resolve_expr(module, x), Ext) and t.package == "sqlalchemy"
                        for x in ast.walk(value) if isinstance(x, (ast.Name, ast.Attribute))):
                    raise
                return  # a plain class attribute (a constant of the project): no DDL
            ns[name] = env[name] = v
        if ann is not None:
            ns["__annotations__"][name] = self.ev(ann, module, env, ann=True)

    def is_type(self, sym: Sym) -> bool:
        return self.p.is_type_decorator(sym)

    def enum(self, sym: Sym, node: ast.ClassDef, kind: str):
        members, n_auto = [], 0
        for st in node.body:
            if isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name):
                n = st.targets[0].id
                if n.startswith("_"):
                    continue
                v = st.value
                if isinstance(v, ast.Call) and (ast.unparse(v.func)).rsplit(".", 1)[-1] == "auto":
                    n_auto += 1
                    val = n.lower() if kind == "StrEnum" else n_auto
                else:
                    val = self.p.const(v, sym.module)
                members.append((n, val))
        base = {"Plain": enum.Enum, "Str": enum.Enum, "Int": enum.Enum, "StrEnum": enum.StrEnum,
                "IntEnum": enum.IntEnum}[kind]
        mixin = {"Str": str, "Int": int}.get(kind)
        cls = base(node.name, members, module=f"py2axum_ddl.{sym.module}", qualname=node.name, type=mixin)
        return cls


class ProjectFn:
    """A project function whose body is `return <expression>` (a type factory): evaluated with its arguments."""

    def __init__(self, b: DdlBuilder, sym: Sym, fd: ast.FunctionDef):
        self.b, self.sym, self.fd = b, sym, fd

    def __call__(self, node: ast.Call, module: str, env: dict):
        fd, b = self.fd, self.b
        body = [s for s in fd.body if not (isinstance(s, ast.Expr) and isinstance(s.value, ast.Constant))]
        fa = fd.args
        if (fd.decorator_list or len(body) != 1 or not isinstance(body[0], ast.Return) or body[0].value is None
                or fa.vararg or fa.kwarg or fa.posonlyargs or fa.kwonlyargs):
            raise b.err(f"{self.sym.name}(): only a plain function whose body is `return <expression>` is evaluated",
                        node, module)
        names = [a.arg for a in fa.args]
        local = {}
        for n, d in zip(names[len(names) - len(fa.defaults):], fa.defaults):
            local[n] = b.ev(d, self.sym.module)
        if len(node.args) > len(names):
            raise b.err(f"too many arguments to {self.sym.name}()", node, module)
        for n, a in zip(names, node.args):
            local[n] = b.ev(a, module, env)
        for k in node.keywords:
            if k.arg not in names:
                raise b.err(f"{self.sym.name}() has no parameter `{k.arg}`", k, module)
            local[k.arg] = b.ev(k.value, module, env)
        if missing := [n for n in names if n not in local]:
            raise b.err(f"{self.sym.name}() missing argument `{missing[0]}`", node, module)
        return b.ev(body[0].value, self.sym.module, local)


def compile_ddl(p, base: Sym, node: ast.AST, module: str, fn: ast.AST | None) -> dict:
    """{types: [(name, sql)], tables: [(name, [sql, ...])]} for `base.metadata.create_all(checkfirst=True)`."""
    try:
        import sqlalchemy
        from sqlalchemy import create_mock_engine
        from sqlalchemy.dialects.postgresql.named_types import CreateEnumType
        from sqlalchemy.schema import AddConstraint, CreateIndex, CreateTable, SetColumnComment, SetTableComment
    except ImportError:
        raise p.err("create_all is compiled by SQLAlchemy: install it next to py2axum (pip install sqlalchemy)",
                    node, module) from None
    if not sqlalchemy.__version__.startswith("2."):
        raise p.err(f"create_all needs SQLAlchemy 2.x at translation time (found {sqlalchemy.__version__})", node, module)
    b = DdlBuilder(p, base, module, fn)
    md = b.build()
    if md is None or type(md).__name__ != "MetaData":
        raise p.err(f"create_all: {base.name}.metadata is not a MetaData", node, module)
    for t in md.tables.values():
        if t.schema is not None:
            raise p.err(f"create_all: table {t.name} has schema={t.schema!r} (only the default schema is supported)",
                        node, module)
        if len(t.name) > 63:
            raise p.err(f"create_all: table name {t.name!r} is longer than PostgreSQL's 63 characters "
                        "(SQLAlchemy raises IdentifierError)", node, module)
    emitted = []
    engine = create_mock_engine("postgresql+psycopg://", lambda sql, *a, **k: emitted.append(sql))
    try:
        md.create_all(engine, checkfirst=False)
    except Exception as e:
        raise p.err(f"create_all: SQLAlchemy raised {type(e).__name__}: {e}", node, module) from None
    # a computed column without `persisted=`: SQLAlchemy 2.1 renders it bare (VIRTUAL) on PostgreSQL 18+ and STORED
    # before (the dialect's flag, set from the server's version at connection); 2.0 always renders STORED
    from .dyn import locked_version
    virtual = hasattr(engine.dialect, "supports_virtual_generated_columns")
    proj = re.match(r"(\d+)\.(\d+)", locked_version(p.ix.root, "sqlalchemy") or sqlalchemy.__version__)
    if proj and (int(proj.group(1)), int(proj.group(2))) >= (2, 1) and not virtual:
        raise p.err(f"create_all: the project uses SQLAlchemy {proj.group(0)}, translate with SQLAlchemy >= 2.1 "
                    f"(found {sqlalchemy.__version__}): its DDL depends on the PostgreSQL version", node, module)
    project_virtual = virtual and (not proj or (int(proj.group(1)), int(proj.group(2))) >= (2, 1))

    def render(el, virt: bool) -> str:
        if not virtual:
            return str(el.compile(dialect=engine.dialect)).strip()
        engine.dialect.supports_virtual_generated_columns = virt
        try:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")  # SQLAlchemy's "created as STORED" warning
                return str(el.compile(dialect=engine.dialect)).strip()
        finally:
            engine.dialect.supports_virtual_generated_columns = True

    out_types, out_tables, alters = [], [], {}
    for el in emitted:
        sql = render(el, project_virtual)
        pre18 = render(el, False)
        if isinstance(el, CreateEnumType):
            if out_tables or el.element.schema is not None:
                raise p.err(f"create_all: enum type {el.element.name} created outside the metadata step "
                            "(Enum(metadata=...) or a schema) is not supported", node, module)
            out_types.append((el.element.name, sql))
        elif isinstance(el, CreateTable):
            out_tables.append((el.element.name, [(-1, 0, sql)], pre18 if pre18 != sql else None))
        elif isinstance(el, CreateIndex) and out_tables and not alters:
            # `table.indexes` is a set: SQLAlchemy's order changes with the hash seed (and is not observable)
            out_tables[-1][1].append((0, el.element.name or "", sql))
        elif isinstance(el, (SetTableComment, SetColumnComment)) and out_tables and not alters:
            out_tables[-1][1].append((1, len(out_tables[-1][1]), sql))
        elif isinstance(el, AddConstraint) and type(el.element).__name__ == "ForeignKeyConstraint":
            # a foreign key of a cycle (or use_alter=True), added once every table exists; SQLAlchemy only
            # sorts the tables it creates, so it belongs to its table and runs when that table is created
            alters.setdefault(el.element.table.name, []).append(sql)
        else:
            raise p.err(f"create_all: SQLAlchemy emits `{sql.splitlines()[0][:60]}`, which is not reproduced "
                        "(sequences, domains...)", node, module)
    return {"types": out_types,
            "tables": [(n, [x[2] for x in sorted(stmts)], sorted(alters.get(n, [])), pre)
                       for n, stmts, pre in out_tables]}

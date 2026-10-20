"""`--python-side auto`: what goes to the Python process when something does not translate.

A refusal blocks the smallest part of the application that can be served by Python without any observable
difference, in this order:

1. a route (its endpoint, dependencies, models, the functions it reaches, the module code of a library it
   uses): the route's path is relayed to Python;
2. the lifespan (`FastAPI(lifespan=...)`, `@app.on_event(...)`): it runs in the Python process only, when no
   translated code reads what it sets in the process (an `app.state` attribute, a module global, a class
   attribute, the logging or Sentry configuration, the environment). A translated route that reads such a
   value goes to Python too; a middleware that does moves the whole application;
3. the whole application (a middleware, an exception handler, module code of the project, an app option): the
   binary keeps no route and relays every request, so the Python application's own stack answers each one
   (WebSocket connections included: ws::relay).

Without `auto`, every refusal keeps its `file:line` error (nothing moves silently).
"""
from __future__ import annotations

import ast
from dataclasses import dataclass, field

from .ir import TranspileError
from .modules import Ext, ModRef, Sym

# library calls that change process-wide state the binary's runtime reads (its own equivalent of the library)
STATE_CALLS = {
    "os.putenv", "os.unsetenv", "os.chdir", "os.umask", "random.seed", "locale.setlocale", "mimetypes.add_type",
    "mimetypes.init", "socket.setdefaulttimeout", "sys.setrecursionlimit", "sys.set_int_max_str_digits",
    "warnings.filterwarnings", "warnings.simplefilter", "warnings.resetwarnings", "signal.signal", "time.tzset",
    "decimal.setcontext", "decimal.getcontext", "asyncio.set_event_loop_policy", "threading.stack_size",
    "sys.setswitchinterval", "codecs.register", "atexit.register",
}
LOG_METHODS = {"debug", "info", "warning", "warn", "error", "exception", "critical", "log", "trace", "success"}  # + loguru's
# a global holding one of these: its methods change nothing the binary's code can read (its own pool)
INERT_FACTORIES = {"sqlalchemy.create_engine", "sqlalchemy.ext.asyncio.create_async_engine",
                   "sqlalchemy.orm.sessionmaker", "sqlalchemy.ext.asyncio.async_sessionmaker",
                   "logging.getLogger"}


@dataclass
class Plan:
    """What --python-side auto leaves to Python."""

    routes: dict = field(default_factory=dict)  # path -> (what, TranspileError | None)
    lifespan: TranspileError | None = None  # why the lifespan runs in Python
    whole: TranspileError | None = None  # why the whole application does
    refused: list = field(default_factory=list)  # what cannot be left to Python (the generation fails)


def plan(fe, per_route) -> Plan:
    """The fallback for the collected errors of `report.collect_dyn(..., auto=True)`."""
    out = Plan()
    for p, e in fe.auto_side.items():
        out.routes[p] = ("raw route", e)
    if fe.__dict__.get("mount_fallback"):
        out.routes["mount"] = ("app.mount() or a library's router", None)
    for info, errs in per_route:
        if errs and info["path"] not in out.routes:
            out.routes[info["path"]] = (f"{info['method'].upper()} {info['func']}", errs[0])
    errs = list(fe.global_errors)
    if not errs:
        return out
    if any(e.msg.startswith("syntax error") for e in errs):
        out.refused = errs  # the Python application does not import either
        return out
    life = [e for e in errs if e.__dict__.get("scope") == "lifespan"]
    app_wide = [e for e in errs if e.__dict__.get("scope") != "lifespan"]
    proj = fe.__dict__.get("collected")
    if not app_wide and proj is not None:
        why, readers = lifespan_conflicts(fe, proj, per_route, out.routes)
        if why is None:
            out.lifespan = life[0]
            for info, reason in readers:
                out.routes.setdefault(info["path"], (f"{info['method'].upper()} {info['func']}", reason))
            return out
        app_wide = [why]
    out.whole = app_wide[0]
    return out


def lifespan_roots(fe) -> list | None:
    """(module, function) of the lifespan and the `@app.on_event` handlers; None when the lifespan is not a
    project function (what it changes cannot be read)."""
    from .frontend import dotted

    roots = []
    for m in fe.index.package_modules():
        for node in ast.walk(m.tree):
            if isinstance(node, ast.Call) and dotted(node.func) in {"FastAPI", "fastapi.FastAPI"}:
                for kw in node.keywords:
                    if kw.arg != "lifespan" or (isinstance(kw.value, ast.Constant) and kw.value.value is None):
                        continue
                    t = fe.index.resolve_expr(m.name, kw.value)
                    d = fe.index.definition(t) if isinstance(t, Sym) else None
                    if d is None and isinstance(kw.value, ast.Name):
                        # a lifespan defined in the app factory (under `if not settings.TESTING:` even): the
                        # function of that name the factory defines
                        d = _local_def(m.tree, node, kw.value.id)
                    if not isinstance(d, (ast.FunctionDef, ast.AsyncFunctionDef)):
                        return None
                    roots.append((m.name, d))
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                for d in node.decorator_list:
                    if (isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and d.func.attr == "on_event"
                            and fe._is_app(m.name, d.func.value)):
                        roots.append((m.name, node))
    return roots


def _local_def(tree: ast.Module, call: ast.Call, name: str):
    """The function `name` defined in the function enclosing `call` (at any depth of its blocks, not in a nested
    function), when it is the only definition of that name there; else None."""
    for fn in ast.walk(tree):
        if not isinstance(fn, (ast.FunctionDef, ast.AsyncFunctionDef)):
            continue
        if not any(n is call for n in ast.walk(fn)):
            continue
        found, todo = [], list(fn.body)
        while todo:
            st = todo.pop()
            if isinstance(st, (ast.FunctionDef, ast.AsyncFunctionDef)):
                if st.name == name:
                    found.append(st)
                continue  # its own locals are another scope
            if isinstance(st, ast.ClassDef):
                continue
            for f in ("body", "orelse", "finalbody", "handlers"):
                todo.extend(getattr(st, f, []) or [])
        inner = [f for f in found if any(n is call for n in ast.walk(f))]
        if not inner:
            return found[0] if len(found) == 1 else None
    return None


class Writes:
    """What a set of functions may change in the process."""

    def __init__(self):
        self.globals: set[Sym] = set()  # module variables and classes rebound or mutated
        self.state: set[str] = set()  # `app.state.<x>` / `request.state.<x>` attributes set
        self.unknown: tuple | None = None  # (message, node, file): a change that cannot be bounded

    def opaque(self, msg: str, node, file: str) -> None:
        if self.unknown is None:
            self.unknown = (msg, node, file)


def reach(fe, roots: list) -> list:
    """The project's (and translated libraries') functions the roots may run, read syntactically: every name
    that resolves to a function, or to a class (its methods), is followed."""
    seen, out, todo = set(), [], list(roots)
    while todo:
        module, fn = todo.pop()
        if id(fn) in seen:
            continue
        seen.add(id(fn))
        out.append((module, fn))
        for n in ast.walk(fn):
            if not isinstance(n, (ast.Name, ast.Attribute)) or not isinstance(n.ctx, ast.Load):
                continue
            try:
                t = fe.index.resolve_expr(module, n)
            except Exception:
                continue
            if not isinstance(t, Sym):
                continue
            d = fe.index.definition(t)
            if isinstance(d, (ast.FunctionDef, ast.AsyncFunctionDef)):
                todo.append((t.module, d))
            elif isinstance(d, ast.ClassDef):
                todo += [(t.module, b) for b in d.body if isinstance(b, (ast.FunctionDef, ast.AsyncFunctionDef))]
    return out


def _root(e):
    """The name an attribute/subscript chain starts from, and the chain's attribute names in order."""
    names = []
    while isinstance(e, (ast.Attribute, ast.Subscript, ast.Call)):
        if isinstance(e, ast.Attribute):
            names.append(e.attr)
        e = e.func if isinstance(e, ast.Call) else e.value
    return (e.id if isinstance(e, ast.Name) else None), names[::-1]


def bound(t) -> set[str]:
    """The names an assignment target binds (`a`, `a, *b`), not those it stores into (`a[k]`, `a.x`)."""
    if isinstance(t, ast.Name):
        return {t.id}
    if isinstance(t, (ast.Tuple, ast.List)):
        return {n for e in t.elts for n in bound(e)}
    if isinstance(t, ast.Starred):
        return bound(t.value)
    return set()


def writes_of(fe, fns: list, entry_ids: set) -> Writes:
    """What the functions may change: rebinding a module global (`global x`), storing into or calling a method
    of a global object, `<param>.state.<x> = ...`, a library's process-wide setting. A store into, or a method
    call on, a parameter or a local aliasing another object (`c = CACHE`) cannot be bounded."""
    from .dyn import _configures_output, unmapped_library

    w = Writes()
    for module, fn in fns:
        file = str(fe.index.module(module).path) if fe.index.module(module) else module
        params = {a.arg for a in [*fn.args.posonlyargs, *fn.args.args, *fn.args.kwonlyargs,
                                  *([fn.args.vararg] if fn.args.vararg else []), *([fn.args.kwarg] if fn.args.kwarg else [])]}
        declared = {n for s in ast.walk(fn) if isinstance(s, (ast.Global, ast.Nonlocal)) for n in s.names}
        fresh, alias = set(), set()
        for s in ast.walk(fn):
            if isinstance(s, (ast.Assign, ast.AnnAssign, ast.NamedExpr)):
                targets = s.targets if isinstance(s, ast.Assign) else [s.target]
                value_alias = isinstance(s.value, (ast.Name, ast.Attribute, ast.Subscript)) or s.value is None
                for t in targets:
                    (alias if value_alias else fresh).update(bound(t))
            elif isinstance(s, (ast.For, ast.AsyncFor, ast.comprehension)):
                alias |= bound(s.target)
            elif isinstance(s, ast.withitem) and s.optional_vars is not None:
                fresh |= bound(s.optional_vars)
            elif isinstance(s, ast.ExceptHandler) and s.name:
                fresh.add(s.name)
        local = (fresh | alias | params) - declared
        for g in declared:
            t = fe.index.resolve(module, g)
            w.globals.add(t if isinstance(t, Sym) else Sym(module, g))

        def target(base_expr, node, what):
            """A store into `base_expr` (or a method call on it): what it changes."""
            name, chain = _root(base_expr)
            meth = node.func.attr if what == "call" else None
            if name is None:
                return
            if name in local and name not in alias and name not in params:
                return  # an object the function made itself
            if name in params or name in alias:
                if len(chain) >= 2 and chain[0] == "state":
                    w.state.add(chain[1])
                elif chain[:1] == ["state"] and what == "store" and isinstance(node, ast.Attribute):
                    w.state.add(node.attr)
                elif meth in LOG_METHODS:
                    pass
                else:
                    w.opaque(f"the lifespan changes `{ast.unparse(base_expr)}`, an object it was given", node, file)
                return
            t = fe.index.resolve(module, name)
            if isinstance(t, Ext):
                if unmapped_library(t) or meth in LOG_METHODS:
                    return
                w.opaque(f"the lifespan changes `{ast.unparse(base_expr)}`, a value of a library the binary maps",
                         node, file)
            elif isinstance(t, Sym):
                d = fe.index.definition(t)
                if what == "call" and (meth in LOG_METHODS or inert_global(fe, t, d)):
                    return
                if isinstance(d, (ast.FunctionDef, ast.AsyncFunctionDef)) and what == "call":
                    return
                w.globals.add(t)
            elif isinstance(t, ModRef):
                w.opaque(f"the lifespan changes the module `{t.module}`", node, file)

        for n in ast.walk(fn):
            if isinstance(n, (ast.Attribute, ast.Subscript)) and isinstance(n.ctx, (ast.Store, ast.Del)):
                if isinstance(n, ast.Attribute) and isinstance(n.value, ast.Attribute) and n.value.attr == "state" \
                        and _root(n.value)[0] in (params | alias):
                    w.state.add(n.attr)
                    continue
                target(n.value, n, "store")
            elif isinstance(n, ast.Call):
                t = fe.index.resolve_expr(module, n.func) if isinstance(n.func, (ast.Name, ast.Attribute)) else None
                if isinstance(t, Ext):
                    if t.dotted in STATE_CALLS or t.dotted.startswith("os.environ.") or _configures_output(t) \
                            and t.dotted.split(".")[-1] not in LOG_METHODS:
                        w.opaque(f"the lifespan calls `{t.dotted}`, which changes the process (the binary's "
                                 "equivalent would not run)", n, file)
                    for a in n.args:
                        if isinstance(a, ast.Name) and a.id not in local:
                            g = fe.index.resolve(module, a.id)
                            if isinstance(g, Sym) and not isinstance(fe.index.definition(g),
                                                                     (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                                w.globals.add(g)  # handed to a library, which may change it
                    continue
                if isinstance(n.func, ast.Attribute) and not isinstance(t, Sym):
                    target(n.func.value, n, "call")
            elif isinstance(n, ast.Yield) and id(fn) in entry_ids and n.value is not None \
                    and not (isinstance(n.value, ast.Constant) and n.value.value is None):
                if isinstance(n.value, ast.Dict) and all(isinstance(k, ast.Constant) and isinstance(k.value, str)
                                                         for k in n.value.keys):
                    w.state |= {k.value for k in n.value.keys}  # lifespan state: `request.state.<key>`
                else:
                    w.opaque("the lifespan yields a state whose keys are not literal", n, file)
    return w


def inert_global(fe, t: Sym, d) -> bool:
    """A global built by an engine, session or logger factory: its methods change nothing translated code reads."""
    m = fe.index.module(t.module)
    for st in m.tree.body if m else ():
        if isinstance(st, (ast.Assign, ast.AnnAssign)) and isinstance(st.value, ast.Call):
            names = [n.id for tg in (st.targets if isinstance(st, ast.Assign) else [st.target]) for n in ast.walk(tg)
                     if isinstance(n, ast.Name)]
            if t.name in names:
                f = fe.index.resolve_expr(t.module, st.value.func)
                return isinstance(f, Ext) and any(f.dotted == x or f.dotted.split(".")[-1] == x.split(".")[-1]
                                                  and f.package == x.split(".")[0] for x in INERT_FACTORIES)
    return False


def reads_of(proj, start, info=None) -> tuple[set, set]:
    """The globals/classes and the attribute names (and string constants: `getattr(state, "x")`) the translated
    code reached from a node reads."""
    syms, names = set(), set()
    seen, todo = {start}, [start]
    asts = [info["node"]] if info else []
    from .dyn import node_sym

    while todo:
        n = todo.pop()
        sym = node_sym(n)
        if sym is not None:
            syms.add(sym)
            d = proj.ix.definition(sym)
            if d is not None:
                asts.append(d)
        for m in proj.edges.get(n, ()):
            if m not in seen:
                seen.add(m)
                todo.append(m)
    for a in asts:
        for x in ast.walk(a):
            if isinstance(x, ast.Attribute):
                names.add(x.attr)
            elif isinstance(x, ast.Constant) and isinstance(x.value, str):
                names.add(x.value)
    return syms, names, asts


_NONE = object()


def init_value(roots: list, fns: list, name: str):
    """The constant `c` when the lifespan's only stores into a `.state.<name>` attribute are `....<name> = c`
    statements of the lifespan itself before its `yield` (an initialisation), else _NONE."""
    values, inits = set(), set()
    for _, fn in roots:
        for st in fn.body:
            if any(isinstance(n, (ast.Yield, ast.YieldFrom)) for n in ast.walk(st)):
                break
            if (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Attribute)
                    and st.targets[0].attr == name and isinstance(st.value, ast.Constant)):
                inits.add(id(st.targets[0]))
                values.add((type(st.value.value), st.value.value))
    for _, fn in fns:
        for n in ast.walk(fn):
            stored = (isinstance(n, ast.Attribute) and n.attr == name and isinstance(n.ctx, ast.Store)
                      or isinstance(n, ast.Call) and isinstance(n.func, ast.Name) and n.func.id in {"setattr", "delattr"}
                      and len(n.args) > 1 and isinstance(n.args[1], ast.Constant) and n.args[1].value == name)
            if stored and id(n) not in inits:
                return _NONE
    return next(iter(values))[1] if len(values) == 1 else _NONE


def reads_default(asts: list, name: str, c) -> bool:
    """Every read of `<x>.<name>` in the translated code is `getattr(<x>, "name", c)`, or comes after a store of
    it in the same function (`x.n = getattr(x, "n", c) + 1` ... `x.n -= 1`): what the lifespan's initialisation
    to `c` would have given it."""
    for a in asts:
        fns = [f for f in ast.walk(a) if isinstance(f, (ast.FunctionDef, ast.AsyncFunctionDef))] or [a]
        for fn in fns:
            stores = sorted(n.lineno for n in ast.walk(fn) if isinstance(n, ast.Assign) and any(
                isinstance(t, ast.Attribute) and t.attr == name for t in n.targets))
            getattrs = {id(n.args[1]) for n in ast.walk(fn) if isinstance(n, ast.Call) and isinstance(n.func, ast.Name)
                        and n.func.id == "getattr" and len(n.args) == 3 and isinstance(n.args[1], ast.Constant)
                        and n.args[1].value == name and isinstance(n.args[2], ast.Constant)
                        and type(n.args[2].value) is type(c) and n.args[2].value == c}
            for n in ast.walk(fn):
                if isinstance(n, ast.Constant) and n.value == name and id(n) not in getattrs:
                    return False
                if isinstance(n, ast.AugAssign) and isinstance(n.target, ast.Attribute) and n.target.attr == name:
                    if not stores or stores[0] >= n.lineno:
                        return False
                elif isinstance(n, ast.Attribute) and n.attr == name and isinstance(n.ctx, ast.Load):
                    return False
    return True


def lifespan_conflicts(fe, proj, per_route, moved: dict):
    """(None, [(route info, reason)...]) when the lifespan can run in Python alone, listing the translated routes
    that read what it sets (they go to Python too); (error, []) when the whole application must."""
    roots = lifespan_roots(fe)
    if roots is None:
        return TranspileError("--python-side auto: the lifespan is not a function of the project, what it changes in "
                              "the process cannot be read, so the whole application stays in Python"), []
    w = writes_of(fe, reach(fe, roots), {id(fn) for _, fn in roots})
    if w.unknown is not None:
        msg, node, file = w.unknown
        return TranspileError(f"{msg}: the lifespan cannot run in Python alone (translated code could read the "
                              "change), so the whole application stays in Python", node, file), []

    fns = reach(fe, roots)
    inits = {x: init_value(roots, fns, x) for x in w.state}

    def conflict(syms, names, asts):
        # a state attribute the lifespan only initialises, read with that value as getattr's default: the
        # binary's process, where the lifespan does not run, reads the same
        state = {x for x in names & w.state if inits[x] is _NONE or not reads_default(asts, x, inits[x])}
        hit = sorted(f"{s.module}.{s.name}" for s in syms & w.globals) + sorted(f"state.{x}" for x in state)
        return hit[0] if hit else None

    for key in (("stack", 0), ("mcp", 0)):
        hit = conflict(*reads_of(proj, key))
        if hit:
            return TranspileError(f"a middleware reads `{hit}`, which the lifespan (left to Python) sets: the whole "
                                  "application stays in Python"), []
    readers = []
    for info, errs in per_route:
        if errs or info["path"] in moved:
            continue
        hit = conflict(*reads_of(proj, info["id"], info))
        if hit:
            readers.append((info, TranspileError(f"reads `{hit}`, which the lifespan (left to Python) sets",
                                                 info["node"], info["file"])))
    return None, readers

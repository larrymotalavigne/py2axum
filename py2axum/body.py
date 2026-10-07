"""Handler body translation: Python AST statements -> Rust statements.

The translator is type-directed. Every Python local gets a static type (`T`) the first
time it is bound, and each supported idiom maps to a fixed Rust shape:

  await session.get(M, pk)                       -> SELECT ... WHERE pk = $1 (static SQL)
  select(M).where(...).order_by(...).limit(...)  -> rt::Select builder (composable at runtime)
  await session.execute(q) / session.scalar(q)   -> fetch_all / fetch_optional
  M(**schema.model_dump()) + add + commit        -> INSERT ... RETURNING (static SQL)
  setattr loop / obj.attr = v + commit           -> UPDATE ... SET (only changed columns)
  await session.delete(obj) + commit             -> DELETE (static SQL)
  async with http.get(url) as resp               -> reqwest (shared client)
  if x is None: raise HTTPException(...)         -> match narrowing Option<M> -> M

Anything else raises TranspileError with the source line.
"""
from __future__ import annotations

import ast
import json
from dataclasses import dataclass, field

from .frontend import dotted
from .ir import MISSING, App, OrmModel, Route, TranspileError, TypeRef

RUST_KEYWORDS = {
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "st", "errs",
}

SQL_OPS = {ast.Eq: "=", ast.NotEq: "<>", ast.Lt: "<", ast.LtE: "<=", ast.Gt: ">", ast.GtE: ">="}
RUST_CMP = {ast.Eq: "==", ast.NotEq: "!=", ast.Lt: "<", ast.LtE: "<=", ast.Gt: ">", ast.GtE: ">="}


@dataclass(frozen=True)
class T:
    """Static type of a translated Python expression."""

    kind: str  # int i32 str strref bool float json none model schema option list select rows new resp http session dict
    name: str | None = None  # model / schema name
    inner: "T | None" = None

    def __str__(self):
        if self.inner:
            return f"{self.kind}[{self.inner}]"
        return f"{self.kind}:{self.name}" if self.name else self.kind


INT, I32, STR, STRREF, BOOL, FLOAT, JSON, NONE = (T(k) for k in ("int", "i32", "str", "strref", "bool", "float", "json", "none"))
COPY_KINDS = {"int", "i32", "bool", "float"}


def rust_ident(name: str) -> str:
    return f"{name}_" if name in RUST_KEYWORDS else name


def rust_str(s: str) -> str:
    return json.dumps(s)  # a JSON string literal is a valid Rust string literal for our purposes


def typeref_to_t(t: TypeRef) -> T:
    if t.kind == "optional":
        return T("option", inner=typeref_to_t(t.inner))
    if t.kind == "list":
        return T("list", inner=typeref_to_t(t.inner))
    if t.kind == "schema":
        return T("schema", t.name)
    return {"int": INT, "str": STR, "bool": BOOL, "float": FLOAT, "json": JSON}[t.kind]


def column_t(col) -> T:
    base = {"int": INT if col.big else I32, "str": STR, "bool": BOOL, "float": FLOAT}[col.py_type]
    return T("option", inner=base) if col.nullable else base


@dataclass
class Var:
    rust: str
    t: T


@dataclass
class PendingOp:
    kind: str  # insert | update | delete
    var: str


@dataclass
class Ctx:
    """Translation state for one handler."""

    app: App
    route: Route
    vars: dict[str, Var] = field(default_factory=dict)
    lines: list[str] = field(default_factory=list)
    indent: int = 1
    pending: list[PendingOp] = field(default_factory=list)
    fresh: set[str] = field(default_factory=set)  # model vars reloaded by the last commit
    mutated: set[str] = field(default_factory=set)  # model vars with an UPDATE change list
    tmp_counter: int = 0
    uses: set[str] = field(default_factory=set)  # schema<-model conversions needed: "Schema:Model"
    dirty: set[str] = field(default_factory=set)  # model vars with un-flushed attribute changes
    new_cols: dict[str, list[str]] = field(default_factory=dict)  # insert column list per new object
    stream: bool = True  # stream large list responses straight from Postgres

    def emit(self, line: str) -> None:
        self.lines.append("    " * self.indent + line)

    def err(self, msg: str, node: ast.AST | None) -> TranspileError:
        return TranspileError(msg, node, self.route.file)

    def tmp(self, prefix: str) -> str:
        self.tmp_counter += 1
        return f"__{prefix}{self.tmp_counter}"

    def bind(self, name: str, t: T) -> Var:
        prev = self.vars.get(name)
        if prev is not None and prev.t == t:
            return prev
        v = Var(rust_ident(name), t)
        self.vars[name] = v
        return v

    def model(self, name: str, node) -> OrmModel:
        m = self.app.models.get(name)
        if m is None:
            raise self.err(f"`{name}` is not a known SQLAlchemy model", node)
        return m

    def entity(self, ident: str) -> str | None:
        """Key of the model/schema/constant a source identifier refers to (resolved via imports)."""
        return self.route.names.get(ident)

    def model_ref(self, ident: str, node) -> OrmModel:
        """A model named in the source (`select(User)`, `User(...)`)."""
        key = self.entity(ident)
        if key is None or key not in self.app.models:
            raise self.err(f"`{ident}` is not a known SQLAlchemy model", node)
        return self.app.models[key]

    def session_var(self) -> str | None:
        for p in self.route.params:
            if p.dep == "session":
                return p.name
        return None

    def http_var(self) -> str | None:
        for p in self.route.params:
            if p.dep == "http":
                return p.name
        return None


# ====================================================================== expressions


def owned(code: str, t: T) -> str:
    """Produce an owned value from a place expression."""
    if t.kind in COPY_KINDS:
        return code
    if t.kind == "strref":
        return f"{code}.to_string()"
    if t.kind == "option" and t.inner.kind in COPY_KINDS:
        return code
    return f"{code}.clone()"


def to_val(code: str, t: T) -> str:
    """Wrap a Rust expression into an `rt::Val` bind value."""
    if t.kind == "none":
        raise TranspileError("cannot bind a bare None here (use .is_(None) or == None in SQL filters)")
    base = t.inner if t.kind == "option" else t
    if base.kind not in {"int", "i32", "str", "strref", "bool", "float"}:
        raise TranspileError(f"cannot use a value of type {t} as a SQL parameter")
    return f"rt::Val::from({owned(code, t)})"


def coerce(code: str, src: T, dst: T, node, ctx: Ctx) -> str:
    """Convert an owned Rust value of type `src` into type `dst`."""
    if src == dst:
        return code
    if src.kind == "strref" and dst.kind == "str":
        return f"{code}.to_string()"
    if src.kind == "i32" and dst.kind == "int":
        return f"i64::from({code})"
    if dst.kind == "option":
        if src.kind == "none":
            return "None"
        if src.kind == "option":
            if src.inner.kind == "i32" and dst.inner.kind == "int":
                return f"{code}.map(i64::from)"
            if src.inner == dst.inner:
                return code
        else:
            return f"Some({coerce(code, src, dst.inner, node, ctx)})"
    if dst.kind == "json":
        return f"serde_json::json!({code})"
    raise ctx.err(f"type mismatch: expected {dst}, got {src}", node)


def expr(node: ast.AST, ctx: Ctx) -> tuple[str, T]:
    """Translate a value expression. Returns (rust place/value expression, type)."""
    if isinstance(node, ast.Constant):
        v = node.value
        if v is None:
            return "None", NONE
        if isinstance(v, bool):
            return ("true" if v else "false"), BOOL
        if isinstance(v, int):
            return f"{v}i64", INT
        if isinstance(v, float):
            return f"{v!r}f64", FLOAT
        if isinstance(v, str):
            return rust_str(v), STRREF
        raise ctx.err(f"unsupported constant {v!r}", node)

    if isinstance(node, ast.Name):
        if node.id in ctx.vars:
            v = ctx.vars[node.id]
            return v.rust, v.t
        if ctx.entity(node.id) in ctx.app.consts:
            return f"{ctx.entity(node.id)}.as_str()", STRREF
        raise ctx.err(f"unknown name `{node.id}`", node)

    if isinstance(node, ast.Attribute):
        return attribute(node, ctx)

    if isinstance(node, ast.JoinedStr):
        fmt, args = [], []
        for part in node.values:
            if isinstance(part, ast.Constant):
                fmt.append(part.value.replace("{", "{{").replace("}", "}}"))
            elif isinstance(part, ast.FormattedValue):
                if part.format_spec is not None or part.conversion != -1:
                    raise ctx.err("format specs/conversions in f-strings are not supported", part)
                code, t = expr(part.value, ctx)
                if t.kind not in {"int", "i32", "str", "strref", "bool", "float"}:
                    raise ctx.err(f"cannot interpolate a value of type {t}", part)
                fmt.append("{}")
                args.append(code)
        return f"format!({rust_str(''.join(fmt))}{''.join(', ' + a for a in args)})", STR

    if isinstance(node, ast.Compare):
        return compare(node, ctx)

    if isinstance(node, ast.BoolOp):
        op = " && " if isinstance(node.op, ast.And) else " || "
        parts = [truthy(v, ctx) for v in node.values]
        return "(" + op.join(parts) + ")", BOOL

    if isinstance(node, ast.UnaryOp):
        if isinstance(node.op, ast.Not):
            return f"!({truthy(node.operand, ctx)})", BOOL
        if isinstance(node.op, ast.USub):
            code, t = expr(node.operand, ctx)
            if t.kind in {"int", "i32", "float"}:
                return f"(-{code})", t
        raise ctx.err("unsupported unary operator", node)

    if isinstance(node, ast.BinOp):
        lc, lt = expr(node.left, ctx)
        rc, rt_ = expr(node.right, ctx)
        if isinstance(node.op, ast.Add) and lt.kind in {"str", "strref"} and rt_.kind in {"str", "strref"}:
            return f"format!(\"{{}}{{}}\", {lc}, {rc})", STR
        nums = {"int", "i32", "float"}
        if lt.kind in nums and rt_.kind in nums:
            if lt.kind == "i32":
                lc, lt = f"i64::from({lc})", INT
            if rt_.kind == "i32":
                rc, rt_ = f"i64::from({rc})", INT
            if lt != rt_:
                raise ctx.err("mixing int and float arithmetic is not supported", node)
            ops = {ast.Add: "+", ast.Sub: "-", ast.Mult: "*", ast.Mod: "%"}
            op = ops.get(type(node.op))
            if op:
                return f"({lc} {op} {rc})", lt
            if isinstance(node.op, ast.FloorDiv) and lt == INT:
                return f"({lc}).div_euclid({rc})", INT
        raise ctx.err("unsupported binary operation", node)

    if isinstance(node, ast.Subscript):
        code, t = expr(node.value, ctx)
        if t.kind == "json":
            key = node.slice
            if isinstance(key, ast.Constant) and isinstance(key.value, (str, int)):
                k = rust_str(key.value) if isinstance(key.value, str) else str(key.value)
                return f"{code}[{k}]", JSON
        raise ctx.err("subscripts are only supported on JSON values with a literal key", node)

    if isinstance(node, ast.Dict):
        items = []
        for k, v in zip(node.keys, node.values):
            if not (isinstance(k, ast.Constant) and isinstance(k.value, str)):
                raise ctx.err("dict literals need string keys", node)
            items.append(f"{rust_str(k.value)}: {json_value(v, ctx)}")
        return "serde_json::json!({" + ", ".join(items) + "})", T("dict")

    if isinstance(node, ast.Call):
        return call_expr(node, ctx)

    if isinstance(node, ast.Await):
        raise ctx.err("this `await` must be the whole right-hand side of an assignment", node)

    raise ctx.err(f"unsupported expression `{ast.unparse(node)}`", node)


def json_value(node: ast.AST, ctx: Ctx) -> str:
    code, t = expr(node, ctx)
    if t.kind in {"model", "rows", "select", "new", "resp", "http", "session"}:
        raise ctx.err(f"cannot put a value of type {t} in a JSON dict", node)
    if t.kind == "list" and t.inner.kind == "model":
        raise ctx.err("cannot put ORM objects in a dict; use a response_model", node)
    return owned(code, t) if t.kind not in {"dict"} else code


def truthy(node: ast.AST, ctx: Ctx) -> str:
    code, t = expr(node, ctx)
    if t.kind == "bool":
        return code
    if t.kind == "option":
        return f"{code}.is_some()"
    if t.kind in {"str", "strref", "list", "rows"}:
        return f"!{code}.is_empty()"
    if t.kind in {"int", "i32"}:
        return f"({code} != 0)"
    if t.kind in {"model", "schema"}:
        return "true"
    raise ctx.err(f"cannot use a value of type {t} as a condition", node)


def compare(node: ast.Compare, ctx: Ctx) -> tuple[str, T]:
    if len(node.ops) != 1:
        raise ctx.err("chained comparisons are not supported", node)
    op, right = node.ops[0], node.comparators[0]
    if isinstance(op, (ast.Is, ast.IsNot)) and isinstance(right, ast.Constant) and right.value is None:
        code, t = expr(node.left, ctx)
        if t.kind != "option":
            return ("false" if isinstance(op, ast.Is) else "true"), BOOL
        return f"{code}.{'is_none' if isinstance(op, ast.Is) else 'is_some'}()", BOOL
    if type(op) in RUST_CMP:
        lc, lt = expr(node.left, ctx)
        rc, rt_ = expr(right, ctx)
        if lt.kind == "i32" and rt_.kind == "int":
            lc, lt = f"i64::from({lc})", INT
        if rt_.kind == "i32" and lt.kind == "int":
            rc, rt_ = f"i64::from({rc})", INT
        strs = {"str", "strref"}
        if lt.kind in strs and rt_.kind in strs:
            return f"({lc}.as_str() {RUST_CMP[type(op)]} {rc})" if lt.kind == "str" and rt_.kind == "strref" else (
                f"({lc} {RUST_CMP[type(op)]} {rc}.as_str())" if lt.kind == "strref" and rt_.kind == "str" else
                f"({lc} {RUST_CMP[type(op)]} {rc})"), BOOL
        if lt != rt_:
            raise ctx.err(f"cannot compare {lt} with {rt_}", node)
        return f"({lc} {RUST_CMP[type(op)]} {rc})", BOOL
    raise ctx.err("unsupported comparison", node)


def attribute(node: ast.Attribute, ctx: Ctx) -> tuple[str, T]:
    base_code, bt = expr(node.value, ctx)
    if bt.kind == "model":
        col = ctx.model(bt.name, node).col(node.attr)
        if col is None:
            raise ctx.err(f"model {bt.name} has no column `{node.attr}`", node)
        if isinstance(node.value, ast.Name) and node.value.id in ctx.dirty:
            raise ctx.err(
                f"reading `{ast.unparse(node)}` after modifying it in this handler is not supported "
                "(commit and refresh first)", node,
            )
        return f"{base_code}.{rust_ident(node.attr)}", column_t(col)
    if bt.kind == "schema":
        f = ctx.app.schemas[bt.name].field(node.attr)
        if f is None:
            raise ctx.err(f"schema {bt.name} has no field `{node.attr}`", node)
        return f"{base_code}.{rust_ident(node.attr)}", typeref_to_t(f.typ)
    if bt.kind == "resp" and node.attr == "status":
        return f"{base_code}__status", INT
    raise ctx.err(f"unsupported attribute access `{ast.unparse(node)}` on {bt}", node)


def call_expr(node: ast.Call, ctx: Ctx) -> tuple[str, T]:
    f = node.func
    # result.scalars().all() / .first() / result.scalar_one_or_none()
    if isinstance(f, ast.Attribute):
        if f.attr in {"all", "first", "one_or_none"} and isinstance(f.value, ast.Call) and isinstance(
            f.value.func, ast.Attribute
        ) and f.value.func.attr == "scalars":
            code, t = expr(f.value.func.value, ctx)
            if t.kind == "rows":
                m = T("model", t.name)
                if f.attr == "all":
                    return code, T("list", inner=m)
                return f"{code}.into_iter().next()", T("option", inner=m)
        if f.attr in {"scalar_one_or_none", "scalar"}:
            code, t = expr(f.value, ctx)
            if t.kind == "rows":
                return f"{code}.into_iter().next()", T("option", inner=T("model", t.name))
        if f.attr == "model_dump" and not node.args:
            code, t = expr(f.value, ctx)
            if t.kind == "schema" and not node.keywords:
                return f"serde_json::to_value(&{code}).unwrap()", JSON
    name = dotted(f)
    if name in {"len"} and len(node.args) == 1:
        code, t = expr(node.args[0], ctx)
        if t.kind in {"list", "rows", "str", "strref"}:
            return f"({code}.len() as i64)", INT
    if name == "str" and len(node.args) == 1:
        code, t = expr(node.args[0], ctx)
        return f"{code}.to_string()", STR
    raise ctx.err(f"unsupported call `{ast.unparse(node)}`", node)


# ====================================================================== SQL expressions


def sql_column(node: ast.AST, ctx: Ctx, model: OrmModel) -> str | None:
    """`User.email` -> "email" when it refers to a column of `model`."""
    if isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name) and node.value.id == model.name:
        if model.col(node.attr) is None:
            raise ctx.err(f"model {model.name} has no column `{node.attr}`", node)
        return node.attr
    return None


def sql_cond(node: ast.AST, ctx: Ctx, model: OrmModel) -> str:
    """Translate a SQLAlchemy filter expression into an `rt::Cond` constructor."""
    if isinstance(node, ast.Call) and dotted(node.func) in {"and_", "or_", "sqlalchemy.and_", "sqlalchemy.or_"}:
        ctor = "And" if dotted(node.func).endswith("and_") else "Or"
        parts = ", ".join(sql_cond(a, ctx, model) for a in node.args)
        return f"rt::Cond::{ctor}(vec![{parts}])"
    if isinstance(node, ast.BoolOp):
        raise ctx.err("use and_()/or_() instead of `and`/`or` inside SQL filters", node)
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
        col = sql_column(node.func.value, ctx, model)
        meth = node.func.attr
        if col and meth in {"is_", "is_not", "isnot"} and len(node.args) == 1 and (
            isinstance(node.args[0], ast.Constant) and node.args[0].value is None
        ):
            return f'rt::Cond::{"IsNull" if meth == "is_" else "NotNull"}("{col}")'
        if col and meth in {"like", "ilike"} and len(node.args) == 1:
            code, t = expr(node.args[0], ctx)
            return f'rt::Cond::Cmp("{col}", "{meth.upper()}", {to_val(code, t)})'
    if isinstance(node, ast.Compare) and len(node.ops) == 1:
        left, op, right = node.left, node.ops[0], node.comparators[0]
        col = sql_column(left, ctx, model)
        flipped = False
        if col is None:
            col = sql_column(right, ctx, model)
            left, right, flipped = right, left, True
        if col is None:
            raise ctx.err("a SQL filter must compare a model column", node)
        if isinstance(right, ast.Constant) and right.value is None:
            if isinstance(op, (ast.Eq, ast.Is)):
                return f'rt::Cond::IsNull("{col}")'
            if isinstance(op, (ast.NotEq, ast.IsNot)):
                return f'rt::Cond::NotNull("{col}")'
        sql_op = SQL_OPS.get(type(op))
        if sql_op is None:
            raise ctx.err("unsupported SQL comparison operator", node)
        if flipped:
            sql_op = {"<": ">", ">": "<", "<=": ">=", ">=": "<="}.get(sql_op, sql_op)
        code, t = expr(right, ctx)
        try:
            val = to_val(code, t)
        except TranspileError as e:
            raise ctx.err(e.msg, node) from None
        return f'rt::Cond::Cmp("{col}", "{sql_op}", {val})'
    raise ctx.err(f"unsupported SQL filter `{ast.unparse(node)}`", node)


def select_chain(node: ast.AST, ctx: Ctx) -> tuple[str | None, OrmModel, list[str]] | None:
    """Flatten `select(M).where(..).order_by(..)` / `q.where(..)`.

    Returns (base query variable or None for a fresh select, model, builder calls).
    """
    calls: list[ast.Call] = []
    cur = node
    while isinstance(cur, ast.Call) and isinstance(cur.func, ast.Attribute):
        calls.append(cur)
        cur = cur.func.value
    base_var = None
    if isinstance(cur, ast.Call) and dotted(cur.func) in {"select", "sqlalchemy.select"}:
        if len(cur.args) != 1 or not isinstance(cur.args[0], ast.Name):
            raise ctx.err("only select(Model) with a single ORM model is supported", cur)
        model = ctx.model_ref(cur.args[0].id, cur)
    elif isinstance(cur, ast.Name) and cur.id in ctx.vars and ctx.vars[cur.id].t.kind == "select":
        base_var = cur.id
        model = ctx.model(ctx.vars[cur.id].t.name, cur)
    else:
        return None
    ops = []
    for c in reversed(calls):
        meth = c.func.attr
        if meth in {"where", "filter"}:
            for a in c.args:
                ops.append(f"and_where({sql_cond(a, ctx, model)})")
        elif meth == "order_by":
            for a in c.args:
                direction = "ASC"
                if isinstance(a, ast.Call) and isinstance(a.func, ast.Attribute) and a.func.attr in {"desc", "asc"}:
                    direction = a.func.attr.upper()
                    a = a.func.value
                col = sql_column(a, ctx, model)
                if col is None:
                    raise ctx.err("order_by needs a model column", a)
                ops.append(f'order_by("{col} {direction}")')
        elif meth in {"limit", "offset"}:
            if len(c.args) != 1:
                raise ctx.err(f".{meth}() takes one argument", c)
            code, t = expr(c.args[0], ctx)
            ops.append(f"{meth}({to_val(code, t)})")
        else:
            raise ctx.err(f"unsupported query method .{meth}()", c)
    return base_var, model, ops


# ====================================================================== statements


def translate_route(ctx: Ctx) -> list[str]:
    session = ctx.session_var()
    if session:
        ctx.vars[session] = Var("st.pool", T("session"))
    http = ctx.http_var()
    if http:
        ctx.vars[http] = Var("st.http", T("http"))
    for name in mutated_names(ctx.route.body):
        ctx.mutated.add(name)
        ctx.emit(f"let mut {rust_ident(name)}__set: Vec<(&'static str, rt::Val)> = Vec::new();")
    block(ctx.route.body, ctx)
    if not ends_with_exit(ctx.route.body):
        finish_return(None, NONE, ctx.route.node, ctx)
    return ctx.lines


def mutated_names(body: list[ast.stmt]) -> set[str]:
    names = set()
    for node in ast.walk(ast.Module(body=body, type_ignores=[])):
        if isinstance(node, ast.Assign):
            for t in node.targets:
                if isinstance(t, ast.Attribute) and isinstance(t.value, ast.Name):
                    names.add(t.value.id)
        if isinstance(node, ast.Call) and dotted(node.func) == "setattr" and node.args:
            if isinstance(node.args[0], ast.Name):
                names.add(node.args[0].id)
    return names


def ends_with_exit(body: list[ast.stmt]) -> bool:
    return bool(body) and isinstance(body[-1], (ast.Return, ast.Raise))


def block(stmts: list[ast.stmt], ctx: Ctx) -> None:
    i = 0
    while i < len(stmts):
        if i + 1 < len(stmts) and try_stream_return(stmts[i], stmts[i + 1], ctx):
            i += 2
            continue
        stmt(stmts[i], ctx)
        i += 1


def stmt(node: ast.stmt, ctx: Ctx) -> None:
    if isinstance(node, (ast.Pass,)) or (isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant)):
        return  # pass / docstring
    if isinstance(node, (ast.Assign, ast.AnnAssign)):
        return assign(node, ctx)
    if isinstance(node, ast.Expr):
        return expr_stmt(node.value, ctx)
    if isinstance(node, ast.If):
        return if_stmt(node, ctx)
    if isinstance(node, ast.Raise):
        return ctx.emit(f"return Err({http_exception(node, ctx)});")
    if isinstance(node, ast.Return):
        if node.value is None:
            return finish_return(None, NONE, node, ctx)
        code, t = expr(node.value, ctx)
        return finish_return(code, t, node, ctx)
    if isinstance(node, ast.For):
        return for_stmt(node, ctx)
    if isinstance(node, ast.AsyncWith):
        return async_with(node, ctx)
    raise ctx.err(f"unsupported statement `{type(node).__name__}`", node)


def http_exception(node: ast.Raise, ctx: Ctx) -> str:
    exc = node.exc
    if not (isinstance(exc, ast.Call) and dotted(exc.func) in {"HTTPException", "fastapi.HTTPException"}):
        raise ctx.err("only `raise HTTPException(status_code=..., detail=...)` is supported", node)
    args = {"status_code": None, "detail": None}
    for name, a in zip(["status_code", "detail"], exc.args):
        args[name] = a
    for kw in exc.keywords:
        if kw.arg not in args:
            raise ctx.err(f"HTTPException({kw.arg}=) is not supported", kw)
        args[kw.arg] = kw.value
    if args["status_code"] is None:
        raise ctx.err("HTTPException needs a status_code", node)
    code, t = expr(args["status_code"], ctx)
    if t.kind != "int":
        raise ctx.err("status_code must be an int", node)
    status = code.removesuffix("i64")
    if args["detail"] is None:
        from http import HTTPStatus

        try:
            detail = rust_str(HTTPStatus(int(status)).phrase)
        except ValueError:
            detail = '""'
        return f"AppError::Http({status} as u16, {detail}.to_string())"
    dcode, dt = expr(args["detail"], ctx)
    if dt.kind not in {"str", "strref"}:
        raise ctx.err("HTTPException detail must be a string", node)
    return f"AppError::Http({status} as u16, {owned(dcode, dt)})"


# ---------------------------------------------------------------- assignment


def assign(node: ast.Assign | ast.AnnAssign, ctx: Ctx) -> None:
    targets = node.targets if isinstance(node, ast.Assign) else [node.target]
    if len(targets) != 1:
        raise ctx.err("multiple assignment targets are not supported", node)
    target, value = targets[0], node.value
    if value is None:
        raise ctx.err("bare annotations are not supported", node)
    if isinstance(target, ast.Attribute):
        return attr_assign(target, value, ctx)
    if not isinstance(target, ast.Name):
        raise ctx.err("only simple names can be assigned", node)
    name = target.id

    # query composition: q = select(...)... / q = q.where(...)
    chain = select_chain(value, ctx)
    if chain is not None:
        base_var, model, ops = chain
        if base_var == name:
            rv = ctx.vars[name].rust
        else:
            src = f"{ctx.vars[base_var].rust}.clone()" if base_var else (
                f'rt::Select::new("{model.table}", "{model.select_cols}")'
            )
            rv = ctx.bind(name, T("select", model.name)).rust
            ctx.emit(f"let mut {rv} = {src};")
        for op in ops:
            ctx.emit(f"{rv}.{op};")
        return

    if isinstance(value, ast.Await):
        return await_assign(name, value.value, ctx)

    # ORM object construction
    if isinstance(value, ast.Call) and isinstance(value.func, ast.Name) and ctx.entity(value.func.id) in ctx.app.models:
        return new_object(name, value, ctx)

    code, t = expr(value, ctx)
    if t.kind in {"strref"}:
        code, t = f"{code}.to_string()", STR
    else:
        code = owned(code, t) if t.kind not in {"dict"} else code
    declare(name, code, t, node, ctx)


def declare(name: str, code: str, t: T, node, ctx: Ctx) -> Var:
    prev = ctx.vars.get(name)
    if prev is not None and prev.t == t and prev.t.kind not in {"session", "http"}:
        ctx.emit(f"{prev.rust} = {code};")
        return prev
    v = ctx.bind(name, t)
    ctx.emit(f"let mut {v.rust} = {code};")
    return v


def await_assign(name: str, call: ast.AST, ctx: Ctx) -> None:
    if not (isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute)):
        raise ctx.err(f"unsupported awaited expression `{ast.unparse(call)}`", call)
    recv_code, recv_t = expr(call.func.value, ctx)
    meth = call.func.attr
    if recv_t.kind == "session":
        if meth == "get":
            if len(call.args) != 2 or not isinstance(call.args[0], ast.Name):
                raise ctx.err("session.get(Model, pk) expected", call)
            model = ctx.model_ref(call.args[0].id, call)
            code, t = expr(call.args[1], ctx)
            sql = f"SELECT {model.select_cols} FROM {model.table} WHERE {model.pk.name} = $1"
            v = ctx.bind(name, T("option", inner=T("model", model.name)))
            ctx.emit(
                f"let mut {v.rust} = sqlx::query_as::<_, models::{model.name}>({rust_str(sql)})"
                f".bind({owned(code, t)}).fetch_optional(&st.pool).await?;"
            )
            return
        if meth in {"execute", "scalar", "scalars"}:
            if len(call.args) != 1:
                raise ctx.err(f"session.{meth}(query) expected", call)
            model, q_expr = query_expr(call.args[0], ctx, call, meth)
            if meth == "scalar":
                v = ctx.bind(name, T("option", inner=T("model", model.name)))
                ctx.emit(f"let mut {v.rust} = {q_expr}.fetch_optional::<models::{model.name}, _>(&st.pool).await?;")
            else:
                kind = "rows" if meth == "execute" else "list"
                t = T("rows", model.name) if kind == "rows" else T("list", inner=T("model", model.name))
                v = ctx.bind(name, t)
                ctx.emit(f"let mut {v.rust} = {q_expr}.fetch_all::<models::{model.name}, _>(&st.pool).await?;")
            return
        raise ctx.err(f"unsupported session method .{meth}()", call)
    if recv_t.kind == "resp":
        if meth == "json":
            v = ctx.bind(name, JSON)
            ctx.emit(f"let mut {v.rust}: serde_json::Value = {recv_code}.json().await?;")
            return
        if meth == "text":
            v = ctx.bind(name, STR)
            ctx.emit(f"let mut {v.rust} = {recv_code}.text().await?;")
            return
    raise ctx.err(f"unsupported awaited call `{ast.unparse(call)}`", call)


def query_expr(arg: ast.AST, ctx: Ctx, call: ast.AST, meth: str) -> tuple[OrmModel, str]:
    """Rust expression of an owned `rt::Select` for a query argument (emits builder lines)."""
    chain = select_chain(arg, ctx)
    if chain is None:
        q_code, q_t = expr(arg, ctx)
        if q_t.kind != "select":
            raise ctx.err(f"session.{meth}() needs a select(...) query", call)
        return ctx.model(q_t.name, call), f"{q_code}.clone()"
    base_var, model, ops = chain
    if base_var and not ops:
        return model, f"{ctx.vars[base_var].rust}.clone()"
    q = ctx.tmp("q")
    src = f"{ctx.vars[base_var].rust}.clone()" if base_var else (
        f'rt::Select::new("{model.table}", "{model.select_cols}")'
    )
    ctx.emit(f"let mut {q} = {src};")
    for op in ops:
        ctx.emit(f"{q}.{op};")
    return model, q


def try_stream_return(first: ast.stmt, second: ast.stmt, ctx: Ctx) -> bool:
    """`rows = await session.execute(q)` immediately followed by `return rows.scalars().all()`
    on a `response_model=list[Schema]` route: stream the rows instead of buffering them."""
    rm = ctx.route.response_model
    if not ctx.stream or rm is None or rm.kind != "list" or rm.inner.kind != "schema":
        return False
    if not isinstance(first, (ast.Assign, ast.AnnAssign)):
        return False
    targets = first.targets if isinstance(first, ast.Assign) else [first.target]
    if len(targets) != 1 or not isinstance(targets[0], ast.Name):
        return False
    name = targets[0].id
    v = first.value
    if not (
        isinstance(v, ast.Await) and isinstance(v.value, ast.Call)
        and isinstance(v.value.func, ast.Attribute) and v.value.func.attr == "execute"
        and len(v.value.args) == 1
    ):
        return False
    recv = v.value.func.value
    if not (isinstance(recv, ast.Name) and recv.id in ctx.vars and ctx.vars[recv.id].t.kind == "session"):
        return False
    r = second.value if isinstance(second, ast.Return) else None
    if not (
        isinstance(r, ast.Call) and isinstance(r.func, ast.Attribute) and r.func.attr == "all" and not r.args
        and isinstance(r.func.value, ast.Call) and isinstance(r.func.value.func, ast.Attribute)
        and r.func.value.func.attr == "scalars" and isinstance(r.func.value.func.value, ast.Name)
        and r.func.value.func.value.id == name
    ):
        return False
    model, q = query_expr(v.value.args[0], ctx, v.value, "execute")
    q = q.removesuffix(".clone()")  # last use: the query can be moved into the stream task
    schema = rm.inner.name
    ctx.uses.add(f"{schema}:{model.name}")
    ctx.pending.clear()  # un-committed changes are rolled back, as in finish_return
    ctx.emit(
        f"return rt::stream_json_list::<models::{model.name}, schemas::{schema}>"
        f"(st.pool.clone(), {q}, {ctx.route.status_code}).await;"
    )
    return True


def new_object(name: str, call: ast.Call, ctx: Ctx) -> None:
    """`User(**payload.model_dump())` or `User(email=..., name=...)`."""
    model = ctx.model_ref(call.func.id, call)
    given: dict[str, tuple[str, T]] = {}
    for kw in call.keywords:
        if kw.arg is None:
            v = kw.value
            if not (
                isinstance(v, ast.Call)
                and isinstance(v.func, ast.Attribute)
                and v.func.attr == "model_dump"
                and not v.args
            ):
                raise ctx.err("only **schema.model_dump() can be unpacked into a model", kw)
            if v.keywords:
                raise ctx.err("model_dump() options are not supported when constructing a model", kw)
            scode, st = expr(v.func.value, ctx)
            if st.kind != "schema":
                raise ctx.err("model_dump() must be called on a Pydantic object", kw)
            schema = ctx.app.schemas[st.name]
            for f in schema.fields:
                given[f.name] = (f"{scode}.{rust_ident(f.name)}", typeref_to_t(f.typ))
        else:
            given[kw.arg] = expr(kw.value, ctx)
    cols, vals = [], []
    for col in model.columns:
        if col.name in given:
            code, t = given.pop(col.name)
            dst = column_t(col)
            # Bind schema ints (i64) straight into INTEGER columns: Postgres applies the assignment cast.
            if dst.kind == "i32":
                dst = INT
            if dst.kind == "option" and dst.inner.kind == "i32":
                dst = T("option", inner=INT)
            if t.kind == "strref":
                code, t = f"{code}.to_string()", STR
            else:
                code = owned(code, t)
            vals.append(coerce(code, t, dst, call, ctx))
            cols.append(col.name)
        elif col.default is not MISSING:
            cols.append(col.name)
            vals.append(rust_literal(col.default, column_t(col), call, ctx))
        elif col.primary_key and col.py_type == "int":
            continue  # SERIAL
        elif col.nullable:
            continue
        else:
            raise ctx.err(f"no value for non-nullable column {model.name}.{col.name}", call)
    if given:
        raise ctx.err(f"{model.name} has no column(s) {sorted(given)}", call)
    rust = rust_ident(name) + "__new"
    ctx.emit(f"let {rust} = ({', '.join(vals)},);")
    ctx.vars[name] = Var(rust, T("new", model.name))
    ctx.new_cols[name] = cols


def rust_literal(value, t: T, node, ctx: Ctx) -> str:
    base = t.inner if t.kind == "option" else t
    if isinstance(value, bool):
        lit = "true" if value else "false"
    elif isinstance(value, int):
        lit = f"{value}i64" if base.kind == "int" else f"{value}i32"
    elif isinstance(value, float):
        lit = f"{value!r}f64"
    elif isinstance(value, str):
        lit = f"{rust_str(value)}.to_string()"
    elif value is None:
        return "None"
    else:
        raise ctx.err(f"unsupported default {value!r}", node)
    return f"Some({lit})" if t.kind == "option" else lit


def attr_assign(target: ast.Attribute, value: ast.AST, ctx: Ctx) -> None:
    if not isinstance(target.value, ast.Name):
        raise ctx.err("only obj.attr = value is supported", target)
    obj = target.value.id
    v = ctx.vars.get(obj)
    if v is None or v.t.kind != "model":
        raise ctx.err(f"`{obj}` is not a loaded ORM object", target)
    model = ctx.model(v.t.name, target)
    col = model.col(target.attr)
    if col is None:
        raise ctx.err(f"model {model.name} has no column `{target.attr}`", target)
    code, t = expr(value, ctx)
    push_set(obj, col.name, to_val(code, t), ctx)


def push_set(obj: str, col: str, val: str, ctx: Ctx) -> None:
    ctx.emit(f'{rust_ident(obj)}__set.push(("{col}", {val}));')
    ctx.dirty.add(obj)
    if not any(p.kind == "update" and p.var == obj for p in ctx.pending):
        ctx.pending.append(PendingOp("update", obj))
    ctx.fresh.discard(obj)


# ---------------------------------------------------------------- expression statements (session ops)


def expr_stmt(node: ast.AST, ctx: Ctx) -> None:
    awaited = isinstance(node, ast.Await)
    call = node.value if awaited else node
    if isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute):
        recv_code, recv_t = expr(call.func.value, ctx)
        meth = call.func.attr
        if recv_t.kind == "session":
            if meth == "add" and not awaited:
                obj = single_name(call, ctx)
                if ctx.vars[obj].t.kind != "new":
                    raise ctx.err("session.add() expects a freshly constructed model object", call)
                ctx.pending.append(PendingOp("insert", obj))
                return
            if meth == "delete" and awaited:
                obj = single_name(call, ctx)
                if ctx.vars[obj].t.kind != "model":
                    raise ctx.err("session.delete() expects a loaded model object", call)
                ctx.pending.append(PendingOp("delete", obj))
                return
            if meth == "commit" and awaited:
                return commit(ctx, call)
            if meth == "refresh" and awaited:
                obj = single_name(call, ctx)
                return refresh(obj, call, ctx)
            if meth in {"rollback"} and awaited:
                ctx.pending.clear()
                return
        raise ctx.err(f"unsupported call `{ast.unparse(node)}`", node)
    raise ctx.err(f"unsupported expression statement `{ast.unparse(node)}`", node)


def single_name(call: ast.Call, ctx: Ctx) -> str:
    if len(call.args) != 1 or not isinstance(call.args[0], ast.Name) or call.args[0].id not in ctx.vars:
        raise ctx.err("expected a single local variable argument", call)
    return call.args[0].id


def commit(ctx: Ctx, node) -> None:
    ops = sorted(ctx.pending, key=lambda p: {"insert": 0, "update": 1, "delete": 2}[p.kind])
    ctx.pending = []
    if not ops:
        return
    tx = len(ops) > 1
    ex = "&mut *__tx" if tx else "&st.pool"
    if tx:
        ctx.emit("let mut __tx = st.pool.begin().await?;")
    for op in ops:
        v = ctx.vars[op.var]
        model = ctx.model(v.t.name, node)
        if op.kind == "insert":
            cols = ctx.new_cols[op.var]
            placeholders = ", ".join(f"${i + 1}" for i in range(len(cols)))
            sql = (
                f"INSERT INTO {model.table} ({', '.join(cols)}) VALUES ({placeholders}) "
                f"RETURNING {model.select_cols}"
            )
            binds = "".join(f".bind({v.rust}.{i})" for i in range(len(cols)))
            nv = ctx.bind(op.var, T("model", model.name))
            ctx.emit(f"let mut {nv.rust} = sqlx::query_as::<_, models::{model.name}>({rust_str(sql)}){binds}.fetch_one({ex}).await?;")
            ctx.fresh.add(op.var)
        elif op.kind == "update":
            pk = model.pk.name
            ctx.emit(f"if !{v.rust}__set.is_empty() {{")
            ctx.emit(
                f'    {v.rust} = rt::update_returning::<models::{model.name}, _>("{model.table}", "{pk}", '
                f'rt::Val::from({v.rust}.{rust_ident(pk)}), std::mem::take(&mut {v.rust}__set), '
                f'"{model.select_cols}", {ex}).await?;'
            )
            ctx.emit("}")
            ctx.fresh.add(op.var)
        elif op.kind == "delete":
            pk = model.pk.name
            sql = f"DELETE FROM {model.table} WHERE {pk} = $1"
            ctx.emit(f"sqlx::query({rust_str(sql)}).bind({v.rust}.{rust_ident(pk)}).execute({ex}).await?;")
    if tx:
        ctx.emit("__tx.commit().await?;")
    for op in ops:
        ctx.dirty.discard(op.var)


def refresh(obj: str, node, ctx: Ctx) -> None:
    v = ctx.vars[obj]
    if v.t.kind != "model":
        raise ctx.err("session.refresh() expects a committed model object", node)
    if obj in ctx.fresh:
        return  # already reloaded by INSERT/UPDATE ... RETURNING
    model = ctx.model(v.t.name, node)
    pk = model.pk.name
    sql = f"SELECT {model.select_cols} FROM {model.table} WHERE {pk} = $1"
    ctx.emit(
        f"{v.rust} = sqlx::query_as::<_, models::{model.name}>({rust_str(sql)})"
        f".bind({v.rust}.{rust_ident(pk)}).fetch_one(&st.pool).await?;"
    )
    ctx.fresh.add(obj)


# ---------------------------------------------------------------- control flow


def if_stmt(node: ast.If, ctx: Ctx) -> None:
    # Narrowing: `if x is None: <raise|return>` turns Option<T> into T for the rest of the handler.
    t = node.test
    narrowed = None
    if (
        isinstance(t, ast.Compare) and len(t.ops) == 1 and isinstance(t.ops[0], ast.Is)
        and isinstance(t.left, ast.Name) and isinstance(t.comparators[0], ast.Constant)
        and t.comparators[0].value is None
    ):
        narrowed = t.left.id
    elif isinstance(t, ast.UnaryOp) and isinstance(t.op, ast.Not) and isinstance(t.operand, ast.Name):
        narrowed = t.operand.id
    if narrowed and narrowed in ctx.vars and ctx.vars[narrowed].t.kind == "option" and ends_with_exit(node.body) and not node.orelse:
        v = ctx.vars[narrowed]
        ctx.emit(f"let mut {v.rust} = match {v.rust} {{")
        ctx.emit("    Some(v) => v,")
        ctx.emit("    None => {")
        ctx.indent += 2
        block(node.body, ctx)
        ctx.indent -= 2
        ctx.emit("    }")
        ctx.emit("};")
        ctx.vars[narrowed] = Var(v.rust, v.t.inner)
        return
    ctx.emit(f"if {truthy(node.test, ctx)} {{")
    snapshot = dict(ctx.vars)
    ctx.indent += 1
    block(node.body, ctx)
    ctx.indent -= 1
    check_scope(snapshot, ctx, node)
    if node.orelse:
        ctx.emit("} else {")
        ctx.indent += 1
        block(node.orelse, ctx)
        ctx.indent -= 1
        check_scope(snapshot, ctx, node)
    ctx.emit("}")


def check_scope(snapshot: dict[str, Var], ctx: Ctx, node) -> None:
    """Names first bound inside a branch do not escape it (Rust block scoping)."""
    for name, var in list(ctx.vars.items()):
        if name not in snapshot:
            del ctx.vars[name]
        elif snapshot[name].t != var.t:
            raise ctx.err(f"`{name}` changes type inside a branch; declare it before the `if`", node)


def for_stmt(node: ast.For, ctx: Ctx) -> None:
    # for field, value in payload.model_dump(exclude_unset=True).items(): setattr(obj, field, value)
    it = node.iter
    ok = (
        isinstance(node.target, ast.Tuple) and len(node.target.elts) == 2
        and all(isinstance(e, ast.Name) for e in node.target.elts)
        and isinstance(it, ast.Call) and isinstance(it.func, ast.Attribute) and it.func.attr == "items"
        and isinstance(it.func.value, ast.Call) and isinstance(it.func.value.func, ast.Attribute)
        and it.func.value.func.attr == "model_dump"
        and len(node.body) == 1 and isinstance(node.body[0], ast.Expr)
        and isinstance(node.body[0].value, ast.Call) and dotted(node.body[0].value.func) == "setattr"
        and not node.orelse
    )
    if not ok:
        raise ctx.err(
            "only the partial-update idiom `for k, v in schema.model_dump(...).items(): setattr(obj, k, v)` "
            "is supported as a for loop", node,
        )
    k, val = node.target.elts[0].id, node.target.elts[1].id
    sa = node.body[0].value
    if not (len(sa.args) == 3 and isinstance(sa.args[0], ast.Name)
            and isinstance(sa.args[1], ast.Name) and sa.args[1].id == k
            and isinstance(sa.args[2], ast.Name) and sa.args[2].id == val):
        raise ctx.err("expected setattr(obj, key, value) with the loop variables", sa)
    obj = sa.args[0].id
    ov = ctx.vars.get(obj)
    if ov is None or ov.t.kind != "model":
        raise ctx.err(f"`{obj}` must be a loaded ORM object", sa)
    model = ctx.model(ov.t.name, sa)
    dump = it.func.value
    scode, st = expr(dump.func.value, ctx)
    if st.kind != "schema":
        raise ctx.err("model_dump() must be called on a Pydantic object", dump)
    exclude_unset = exclude_none = False
    for kw in dump.keywords:
        if kw.arg == "exclude_unset":
            exclude_unset = bool(ast.literal_eval(kw.value))
        elif kw.arg == "exclude_none":
            exclude_none = bool(ast.literal_eval(kw.value))
        else:
            raise ctx.err(f"model_dump({kw.arg}=) is not supported here", kw)
    schema = ctx.app.schemas[st.name]
    for i, f in enumerate(schema.fields):
        col = model.col(f.name)
        if col is None:
            raise ctx.err(f"{schema.name}.{f.name} has no matching column on {model.name}", node)
        ft = typeref_to_t(f.typ)
        place = f"{scode}.{rust_ident(f.name)}"
        conds = []
        if exclude_unset:
            conds.append(f"{scode}.is_set({i})")
        if exclude_none and ft.kind == "option":
            conds.append(f"{place}.is_some()")
        line = f'{rust_ident(obj)}__set.push(("{col.name}", {to_val(place, ft)}));'
        if conds:
            ctx.emit(f"if {' && '.join(conds)} {{ {line} }}")
        else:
            ctx.emit(line)
    ctx.dirty.add(obj)
    if not any(p.kind == "update" and p.var == obj for p in ctx.pending):
        ctx.pending.append(PendingOp("update", obj))
    ctx.fresh.discard(obj)


def async_with(node: ast.AsyncWith, ctx: Ctx) -> None:
    # async with http.get(url, json=..., params=...) as resp:
    if len(node.items) != 1:
        raise ctx.err("one context manager per `async with` is supported", node)
    item = node.items[0]
    call = item.context_expr
    if not (isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute)):
        raise ctx.err("expected `async with http.<method>(url) as resp`", node)
    rcode, rt_ = expr(call.func.value, ctx)
    meth = call.func.attr
    if rt_.kind != "http" or meth not in {"get", "post", "put", "patch", "delete"}:
        raise ctx.err("expected `async with http.<method>(url) as resp` on an aiohttp.ClientSession", node)
    if not isinstance(item.optional_vars, ast.Name):
        raise ctx.err("bind the response with `as resp`", node)
    if len(call.args) != 1:
        raise ctx.err("the request URL must be the single positional argument", call)
    ucode, ut = expr(call.args[0], ctx)
    if ut.kind not in {"str", "strref"}:
        raise ctx.err("the request URL must be a string", call)
    req = f"{rcode}.{meth}({ucode})"
    for kw in call.keywords:
        if kw.arg == "json":
            jcode, jt = expr(kw.value, ctx)
            if jt.kind not in {"schema", "dict", "json"}:
                raise ctx.err("json= must be a Pydantic object, a dict literal or a JSON value", kw)
            req += f".json(&{jcode})"
        elif kw.arg == "timeout":
            tcode, tt = expr(kw.value, ctx)
            req += f".timeout(std::time::Duration::from_secs_f64({tcode} as f64))"
        else:
            raise ctx.err(f"aiohttp option {kw.arg}= is not supported", kw)
    name = item.optional_vars.id
    v = ctx.bind(name, T("resp"))
    ctx.emit(f"let {v.rust} = {req}.send().await?;")
    ctx.emit(f"let {v.rust}__status: i64 = {v.rust}.status().as_u16() as i64;")
    block(node.body, ctx)


# ---------------------------------------------------------------- responses


def finish_return(code: str | None, t: T, node, ctx: Ctx) -> None:
    if ctx.pending:
        # Matches SQLAlchemy: un-committed changes are rolled back when the session closes.
        ctx.pending.clear()
    route = ctx.route
    status = route.status_code
    rm = route.response_model
    if code is None or t.kind == "none":
        if status == 204:
            ctx.emit(f"return Ok(rt::empty_response({status}));")
        else:
            ctx.emit(f"return Ok(rt::json_response({status}, &serde_json::Value::Null));")
        return
    if status == 204:
        raise ctx.err("a 204 route must not return a body", node)
    if rm is None:
        if t.kind in {"dict", "json", "schema", "str", "strref", "int", "bool", "float"}:
            ctx.emit(f"return Ok(rt::json_response({status}, &{code}));")
            return
        if t.kind == "list" and t.inner.kind == "schema":
            ctx.emit(f"return Ok(rt::json_response({status}, &{code}));")
            return
        raise ctx.err(f"returning {t} needs a response_model on the route", node)
    target = typeref_to_t(rm)
    out = convert_out(code, t, target, node, ctx)
    ctx.emit(f"return Ok(rt::json_response({status}, &{out}));")


def convert_out(code: str, src: T, dst: T, node, ctx: Ctx) -> str:
    if src == dst:
        return code
    if dst.kind == "schema" and src.kind == "model":
        ctx.uses.add(f"{dst.name}:{src.name}")
        return f"schemas::{dst.name}::from({code})"
    if dst.kind == "schema" and src.kind == "schema":
        raise ctx.err("returning a different Pydantic type than response_model is not supported", node)
    if dst.kind == "list" and src.kind == "list":
        if dst.inner.kind == "schema" and src.inner.kind == "model":
            ctx.uses.add(f"{dst.inner.name}:{src.inner.name}")
            return f"{code}.into_iter().map(schemas::{dst.inner.name}::from).collect::<Vec<_>>()"
    if dst.kind == "option":
        if src.kind == "option":
            inner = convert_out("v", src.inner, dst.inner, node, ctx)
            return f"{code}.map(|v| {inner})"
        return convert_out(code, src, dst.inner, node, ctx)
    raise ctx.err(f"cannot return {src} for response_model {dst}", node)

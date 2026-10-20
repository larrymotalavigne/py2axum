"""Pydantic options evaluated at translation time: alias generators.

An alias generator only ever sees the field names, all known statically, so the aliases are computed here and
the runtime gets plain keys. Supported: `pydantic.alias_generators` (ported below, pydantic 2.14), and a lambda
or a one-`return` function of the name built from string operations (`str` methods, `+`, `*`, indexing,
slicing, comprehensions, `''.join(...)`); anything else is refused by the caller.
"""
from __future__ import annotations

import ast
import re


# ---------------------------------------------------------------- pydantic.alias_generators (2.14)

def to_pascal(snake: str) -> str:
    camel = snake.title()
    return re.sub('([0-9A-Za-z])_(?=[0-9A-Z])', lambda m: m.group(1), camel)


def to_camel(snake: str) -> str:
    if re.match('^[a-z]+[A-Za-z0-9]*$', snake) and not re.search(r'\d[a-z]', snake):
        return snake
    camel = to_pascal(snake)
    return re.sub('(^_*[A-Z])', lambda m: m.group(1).lower(), camel)


def to_snake(camel: str) -> str:
    snake = re.sub(r'([A-Z]+)([A-Z][a-z])', lambda m: f'{m.group(1)}_{m.group(2)}', camel)
    snake = re.sub(r'([a-z])([A-Z])', lambda m: f'{m.group(1)}_{m.group(2)}', snake)
    snake = re.sub(r'([0-9])([A-Z])', lambda m: f'{m.group(1)}_{m.group(2)}', snake)
    snake = re.sub(r'([a-z])([0-9])', lambda m: f'{m.group(1)}_{m.group(2)}', snake)
    snake = snake.replace('-', '_')
    return snake.lower()


LIBRARY = {"pydantic.alias_generators.to_pascal": to_pascal, "pydantic.alias_generators.to_camel": to_camel,
           "pydantic.alias_generators.to_snake": to_snake}

# ---------------------------------------------------------------- string expressions

STR_METHODS = {"upper", "lower", "title", "capitalize", "swapcase", "casefold", "strip", "lstrip", "rstrip", "split",
               "rsplit", "join", "replace", "removeprefix", "removesuffix", "startswith", "endswith", "zfill",
               "isupper", "islower", "isdigit", "isalpha", "isalnum", "find", "rfind", "count", "partition",
               "rpartition", "center", "ljust", "rjust"}
BUILTINS = {"str": str, "len": len, "list": list, "reversed": reversed, "enumerate": enumerate, "range": range,
            "tuple": tuple, "sorted": sorted, "min": min, "max": max, "bool": bool, "int": int}
COMPARE = {ast.Eq: lambda a, b: a == b, ast.NotEq: lambda a, b: a != b, ast.Lt: lambda a, b: a < b,
           ast.LtE: lambda a, b: a <= b, ast.Gt: lambda a, b: a > b, ast.GtE: lambda a, b: a >= b,
           ast.In: lambda a, b: a in b, ast.NotIn: lambda a, b: a not in b}
# a value the evaluator may hold: what string operations on a name produce
VALUES = (str, int, bool, type(None), list, tuple)
MAX_STEPS = 100_000


class NotStatic(Exception):
    """The expression uses something this evaluator does not know: the caller refuses it."""

    def __init__(self, node: ast.AST):
        super().__init__(ast.unparse(node))
        self.node = node


class Scope(dict):
    """The bound names of a function being evaluated; `local` = every name the function binds (a builtin of
    the same name is shadowed, even before it is bound, as in Python)."""

    def __init__(self, values: dict, local: frozenset = frozenset()):
        super().__init__(values)
        self.local = local

    def child(self, binds: dict) -> "Scope":
        return Scope({**self, **binds}, self.local | frozenset(binds))


def _value(v, node):
    if isinstance(v, (enumerate, reversed, range, type(iter("")), type(iter([])))):
        return v  # a real iterator: consumed and measured exactly as in Python
    if not isinstance(v, VALUES) or isinstance(v, (list, tuple)) and not all(isinstance(x, VALUES) for x in v):
        raise NotStatic(node)
    return v


def unpack(target: ast.AST, value, env: dict, node: ast.AST) -> None:
    """Bind `target` (a name, or a tuple/list of names with at most one `*name`) to `value`, like Python."""
    if isinstance(target, ast.Name):
        env[target.id] = value
        return
    if not isinstance(target, (ast.Tuple, ast.List)):
        raise NotStatic(target)
    vals = list(value)
    stars = [i for i, t in enumerate(target.elts) if isinstance(t, ast.Starred)]
    if len(stars) > 1:
        raise NotStatic(target)
    if not stars:
        if len(vals) != len(target.elts):
            raise ValueError(f"too many values to unpack (expected {len(target.elts)})" if len(vals) > len(target.elts)
                             else f"not enough values to unpack (expected {len(target.elts)}, got {len(vals)})")
        for t, v in zip(target.elts, vals):
            unpack(t, v, env, node)
        return
    k = stars[0]
    after = len(target.elts) - k - 1
    if len(vals) < len(target.elts) - 1:
        raise ValueError(f"not enough values to unpack (expected at least {len(target.elts) - 1}, got {len(vals)})")
    for t, v in zip(target.elts[:k], vals[:k]):
        unpack(t, v, env, node)
    star = target.elts[k].value
    if not isinstance(star, ast.Name):
        raise NotStatic(star)
    env[star.id] = vals[k:len(vals) - after]
    for t, v in zip(target.elts[k + 1:], vals[len(vals) - after:]):
        unpack(t, v, env, node)


def target_names(target: ast.AST) -> set[str]:
    return {n.id for n in ast.walk(target) if isinstance(n, ast.Name)}


def evaluate(node: ast.AST, env: dict):
    """Evaluate a pure string expression over `env` (names -> values)."""
    if not isinstance(env, Scope):
        env = Scope(env)
    if isinstance(node, ast.Constant) and isinstance(node.value, (str, int, bool, type(None))):
        return node.value
    if isinstance(node, ast.Name):
        if node.id in env:
            return env[node.id]
        raise NotStatic(node)
    if isinstance(node, ast.BinOp) and isinstance(node.op, (ast.Add, ast.Mult, ast.Sub)):
        a, b = evaluate(node.left, env), evaluate(node.right, env)
        if isinstance(node.op, ast.Sub) and not (type(a) is int and type(b) is int):
            raise NotStatic(node)
        return _value(a + b if isinstance(node.op, ast.Add) else a * b if isinstance(node.op, ast.Mult) else a - b, node)
    if isinstance(node, ast.UnaryOp) and isinstance(node.op, (ast.Not, ast.USub)):
        v = evaluate(node.operand, env)
        if isinstance(node.op, ast.Not):
            return not v
        if type(v) is not int:
            raise NotStatic(node)
        return -v
    if isinstance(node, ast.BoolOp):
        v = None
        for x in node.values:  # `a or b` / `a and b` give an operand, short-circuited
            v = evaluate(x, env)
            if bool(v) == isinstance(node.op, ast.Or):
                return v
        return v
    if isinstance(node, (ast.Tuple, ast.List)) and not any(isinstance(e, ast.Starred) for e in node.elts):
        vals = [evaluate(e, env) for e in node.elts]
        return tuple(vals) if isinstance(node, ast.Tuple) else vals
    if isinstance(node, ast.Subscript):
        v = evaluate(node.value, env)
        if isinstance(node.slice, ast.Slice):
            parts = [evaluate(x, env) if x is not None else None for x in (node.slice.lower, node.slice.upper, node.slice.step)]
            return v[slice(*parts)]
        return v[evaluate(node.slice, env)]
    if isinstance(node, ast.JoinedStr):
        out = []
        for part in node.values:
            if isinstance(part, ast.Constant):
                out.append(part.value)
            elif isinstance(part, ast.FormattedValue) and part.conversion == -1 and part.format_spec is None:
                out.append(str(evaluate(part.value, env)))
            else:
                raise NotStatic(part)
        return "".join(out)
    if isinstance(node, ast.IfExp):
        return evaluate(node.body, env) if evaluate(node.test, env) else evaluate(node.orelse, env)
    if isinstance(node, ast.Compare) and all(type(o) in COMPARE for o in node.ops):
        a = evaluate(node.left, env)
        for op, c in zip(node.ops, node.comparators):  # chained: `a < b < c`, each operand evaluated once
            b = evaluate(c, env)
            if not COMPARE[type(op)](a, b):
                return False
            a = b
        return True
    if isinstance(node, (ast.GeneratorExp, ast.ListComp)) and len(node.generators) == 1:
        g = node.generators[0]
        if g.is_async:
            raise NotStatic(node)
        out = []
        for item in evaluate(g.iter, env):
            inner = env.child({})
            unpack(g.target, item, inner, node)
            inner.local = inner.local | target_names(g.target)
            if all(evaluate(c, inner) for c in g.ifs):
                out.append(evaluate(node.elt, inner))
        return out
    if isinstance(node, ast.Call) and not any(isinstance(a, ast.Starred) for a in node.args):
        args = [evaluate(a, env) for a in node.args]
        kwargs = {k.arg: evaluate(k.value, env) for k in node.keywords if k.arg is not None}
        if len(kwargs) != len(node.keywords):
            raise NotStatic(node)
        if isinstance(node.func, ast.Attribute) and node.func.attr in STR_METHODS:
            recv = evaluate(node.func.value, env)
            if isinstance(recv, str):
                return _value(getattr(recv, node.func.attr)(*args, **kwargs), node)
        if isinstance(node.func, ast.Name) and node.func.id in BUILTINS and node.func.id not in env.local:
            return _value(BUILTINS[node.func.id](*args, **kwargs), node)
    raise NotStatic(node)


class _Return(Exception):
    def __init__(self, value):
        self.value = value


class _Break(Exception):
    pass


class _Continue(Exception):
    pass


def run_body(body: list[ast.stmt], env: Scope, steps: list[int]) -> None:
    """Run a function body of simple statements (`if`, `for`, assignments, `return`) over `env`; raises
    `_Return` with the result, NotStatic on anything else."""
    for st in body:
        steps[0] += 1
        if steps[0] > MAX_STEPS:
            raise NotStatic(st)
        if isinstance(st, ast.Return):
            raise _Return(evaluate(st.value, env) if st.value is not None else None)
        if isinstance(st, ast.Pass):
            continue
        if isinstance(st, ast.Assign):
            v = evaluate(st.value, env)
            for t in st.targets:
                unpack(t, v, env, st)
        elif isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name) and st.value is not None:
            env[st.target.id] = evaluate(st.value, env)
        elif isinstance(st, ast.AugAssign) and isinstance(st.target, ast.Name) and isinstance(st.op, (ast.Add, ast.Mult)):
            if st.target.id not in env:
                raise NotStatic(st)  # UnboundLocalError in Python
            a, b = env[st.target.id], evaluate(st.value, env)
            if isinstance(a, list) and isinstance(st.op, ast.Add):
                a.extend(b)  # `+=` on a list extends it in place (any alias sees it)
            else:
                env[st.target.id] = _value(a + b if isinstance(st.op, ast.Add) else a * b, st)
        elif isinstance(st, ast.If):
            run_body(st.body if evaluate(st.test, env) else st.orelse, env, steps)
        elif isinstance(st, ast.For) and not st.orelse:
            for item in list(evaluate(st.iter, env)):
                unpack(st.target, item, env, st)
                try:
                    run_body(st.body, env, steps)
                except _Break:
                    break
                except _Continue:
                    continue
        elif isinstance(st, ast.Break):
            raise _Break()
        elif isinstance(st, ast.Continue):
            raise _Continue()
        elif isinstance(st, ast.Expr) and isinstance(st.value, ast.Call) and isinstance(st.value.func, ast.Attribute) \
                and st.value.func.attr == "append":
            recv = evaluate(st.value.func.value, env)
            if not isinstance(recv, list) or len(st.value.args) != 1 or st.value.keywords:
                raise NotStatic(st)
            recv.append(_value(evaluate(st.value.args[0], env), st))
        else:
            raise NotStatic(st)


def string_function(fn: ast.Lambda | ast.FunctionDef):
    """A Python function str -> str from a lambda or a function of simple statements over its one parameter
    (`if`, `for`, assignments, `return`; a docstring allowed): raises NotStatic when it is not."""
    a = fn.args
    params = a.posonlyargs + a.args
    if len(params) != 1 or a.vararg or a.kwarg or a.kwonlyargs or a.defaults:
        raise NotStatic(fn)
    name = params[0].arg
    if isinstance(fn, ast.Lambda):
        body = [ast.Return(value=fn.body)]
    else:
        if fn.decorator_list or any(isinstance(n, (ast.Global, ast.Nonlocal, ast.FunctionDef, ast.AsyncFunctionDef,
                                                   ast.ClassDef, ast.Lambda, ast.NamedExpr)) for n in ast.walk(fn)
                                    if n is not fn):
            raise NotStatic(fn)
        body = list(fn.body)
        if body and isinstance(body[0], ast.Expr) and isinstance(body[0].value, ast.Constant) \
                and isinstance(body[0].value.value, str):
            body = body[1:]
    local = frozenset({name} | {t for st in ast.walk(fn) if isinstance(st, (ast.Assign, ast.AugAssign, ast.AnnAssign, ast.For))
                                for tg in (st.targets if isinstance(st, ast.Assign) else [st.target])
                                for t in target_names(tg)})

    def run(s: str) -> str:
        try:
            run_body(body, Scope({name: s}, local), [0])
            out = None  # falls off the end
        except _Return as r:
            out = r.value
        if not isinstance(out, str):
            raise NotStatic(fn)
        return out
    return run

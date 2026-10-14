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
               "rsplit", "join", "replace", "removeprefix", "removesuffix", "startswith", "endswith", "zfill"}
BUILTINS = {"str": str, "len": len, "list": list, "reversed": reversed, "enumerate": enumerate}


class NotStatic(Exception):
    """The expression uses something this evaluator does not know: the caller refuses it."""

    def __init__(self, node: ast.AST):
        super().__init__(ast.unparse(node))
        self.node = node


def evaluate(node: ast.AST, env: dict):
    """Evaluate a pure string expression over `env` (names -> values)."""
    if isinstance(node, ast.Constant) and isinstance(node.value, (str, int, type(None))):
        return node.value
    if isinstance(node, ast.Name):
        if node.id in env:
            return env[node.id]
        raise NotStatic(node)
    if isinstance(node, ast.BinOp) and isinstance(node.op, (ast.Add, ast.Mult)):
        a, b = evaluate(node.left, env), evaluate(node.right, env)
        return a + b if isinstance(node.op, ast.Add) else a * b
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
    if isinstance(node, ast.Compare) and len(node.ops) == 1 and isinstance(node.ops[0], (ast.Eq, ast.NotEq)):
        a, b = evaluate(node.left, env), evaluate(node.comparators[0], env)
        return (a == b) if isinstance(node.ops[0], ast.Eq) else (a != b)
    if isinstance(node, (ast.GeneratorExp, ast.ListComp)) and len(node.generators) == 1:
        g = node.generators[0]
        if not isinstance(g.target, ast.Name) or g.is_async:
            raise NotStatic(node)
        out = []
        for item in evaluate(g.iter, env):
            inner = {**env, g.target.id: item}
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
                return getattr(recv, node.func.attr)(*args, **kwargs)
        if isinstance(node.func, ast.Name) and node.func.id in BUILTINS and node.func.id not in env:
            return BUILTINS[node.func.id](*args, **kwargs)
    raise NotStatic(node)


def string_function(fn: ast.Lambda | ast.FunctionDef):
    """A Python function str -> str from a lambda or a function whose body is one `return` (a docstring
    allowed): raises NotStatic when it is not."""
    a = fn.args
    params = a.posonlyargs + a.args
    if len(params) != 1 or a.vararg or a.kwarg or a.kwonlyargs or a.defaults:
        raise NotStatic(fn)
    if isinstance(fn, ast.Lambda):
        body = fn.body
    else:
        stmts = [s for s in fn.body if not (isinstance(s, ast.Expr) and isinstance(s.value, ast.Constant))]
        if fn.decorator_list or len(stmts) != 1 or not isinstance(stmts[0], ast.Return) or stmts[0].value is None:
            raise NotStatic(fn)
        body = stmts[0].value
    name = params[0].arg

    def run(s: str) -> str:
        out = evaluate(body, {name: s})
        if not isinstance(out, str):
            raise NotStatic(body)
        return out
    return run

"""`Model = pydantic.create_model('Model', ...)` at module level, rewritten at load into the class it creates.

Supported: the name (a literal, the same as the variable), fields as `name=type` or `name=(type, default)` /
`name=(type, Field(...))`, `__base__=` one class, `__doc__=` a literal, and in `__namespace__=` / `__validators__=` a
dict literal (`__namespace__` exists since pydantic 2.14: before, it is ignored, as pydantic does) whose values are `field_validator(...)(f)` / `model_validator(mode=...)(f)` (f a module-level function
taking `cls` first) or `computed_field(property(f))` (f taking `self`). The class written in its place has the
fields in that order and one method per entry calling the module function, as pydantic binds it. Anything else is
left as written: the translator then refuses the call."""
from __future__ import annotations

import ast
import copy


def _is(node: ast.AST, names: set[str]) -> bool:
    return isinstance(node, ast.Name) and node.id in names


def _locals(tree: ast.Module, wanted: str) -> set[str]:
    """the local names bound to `pydantic.<wanted>` by `from pydantic import ...`"""
    out = set()
    for n in tree.body:
        if isinstance(n, ast.ImportFrom) and n.module == "pydantic" and n.level == 0:
            out |= {a.asname or a.name for a in n.names if a.name == wanted}
    return out


def _functions(tree: ast.Module) -> dict[str, ast.FunctionDef]:
    return {n.name: n for n in tree.body if isinstance(n, ast.FunctionDef)}


class _Skip(Exception):
    pass


def _method(entry: ast.AST, key: str, funcs: dict, names: dict[str, set[str]]) -> ast.FunctionDef:
    """one `__namespace__` value as a method of the class"""
    # field_validator('a', 'b')(f) / model_validator(mode='after')(f)
    if (isinstance(entry, ast.Call) and len(entry.args) == 1 and not entry.keywords and isinstance(entry.func, ast.Call)
            and (_is(entry.func.func, names["field_validator"]) or _is(entry.func.func, names["model_validator"]))
            and isinstance(entry.args[0], ast.Name) and entry.args[0].id in funcs):
        f = funcs[entry.args[0].id]
        params = [a.arg for a in f.args.posonlyargs + f.args.args]
        if f.args.vararg or f.args.kwarg or f.args.kwonlyargs or f.args.defaults or len(params) not in (1, 2, 3):
            raise _Skip
        model_after = _is(entry.func.func, names["model_validator"])
        lead = "self" if model_after and any(k.arg == "mode" and isinstance(k.value, ast.Constant) and k.value.value == "after"
                                             for k in entry.func.keywords) else "cls"
        args = ", ".join(params)
        deco = ast.unparse(entry.func)
        src = (f"@{deco}\n" + ("" if lead == "self" else "@classmethod\n")
               + f"def {key}({args}):\n    return {f.name}({args})\n")
        return ast.parse(src).body[0]
    # computed_field(property(f))
    if (isinstance(entry, ast.Call) and _is(entry.func, names["computed_field"]) and len(entry.args) == 1 and not entry.keywords
            and isinstance(entry.args[0], ast.Call) and _is(entry.args[0].func, {"property"}) and len(entry.args[0].args) == 1
            and isinstance(entry.args[0].args[0], ast.Name) and entry.args[0].args[0].id in funcs):
        f = funcs[entry.args[0].args[0].id]
        if len(f.args.posonlyargs + f.args.args) != 1 or f.args.vararg or f.args.kwarg or f.args.kwonlyargs:
            raise _Skip
        ret = f" -> {ast.unparse(f.returns)}" if f.returns is not None else ""
        return ast.parse(f"@{ast.unparse(entry.func)}\n@property\ndef {key}(self){ret}:\n    return {f.name}(self)\n").body[0]
    raise _Skip


def _class(assign: ast.Assign, names: dict[str, set[str]], funcs: dict, namespace: bool = True) -> ast.ClassDef:
    call = assign.value
    target = assign.targets[0].id
    if not (call.args and isinstance(call.args[0], ast.Constant) and call.args[0].value == target and len(call.args) == 1):
        raise _Skip
    bases = [ast.Name(id=next(iter(names["BaseModel"]), "BaseModel"), ctx=ast.Load())]
    body: list[ast.stmt] = []
    methods: list[ast.stmt] = []
    for kw in call.keywords:
        if kw.arg is None:
            raise _Skip
        if kw.arg == "__base__":
            if not isinstance(kw.value, (ast.Name, ast.Attribute)):
                raise _Skip
            bases = [kw.value]
        elif kw.arg == "__doc__":
            if not (isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str)):
                raise _Skip
            body.insert(0, ast.Expr(kw.value))
        elif kw.arg == "__namespace__" and not namespace:
            continue  # before pydantic 2.14 create_model has no __namespace__: the keyword is ignored
        elif kw.arg in ("__namespace__", "__validators__"):
            if not isinstance(kw.value, ast.Dict) or None in kw.value.keys:
                raise _Skip
            for k, v in zip(kw.value.keys, kw.value.values):
                if not (isinstance(k, ast.Constant) and isinstance(k.value, str) and k.value.isidentifier()):
                    raise _Skip
                methods.append(_method(v, k.value, funcs, names))
        elif kw.arg.startswith("__"):
            raise _Skip  # __config__, __module__, __cls_kwargs__...
        elif isinstance(kw.value, ast.Tuple):
            if len(kw.value.elts) != 2:
                raise _Skip
            body.append(ast.AnnAssign(target=ast.Name(id=kw.arg, ctx=ast.Store()), annotation=kw.value.elts[0],
                                      value=kw.value.elts[1], simple=1))
        else:
            body.append(ast.AnnAssign(target=ast.Name(id=kw.arg, ctx=ast.Store()), annotation=kw.value, value=None, simple=1))
    cls = ast.ClassDef(name=target, bases=bases, keywords=[], body=(body + methods) or [ast.Pass()],
                       decorator_list=[], type_params=[])
    for n in ast.walk(cls):
        ast.copy_location(n, assign)
    return cls


def rewrite(tree: ast.Module, namespace: bool = True) -> ast.Module:
    """`namespace`: the locked pydantic has create_model(__namespace__=) (2.14+)"""
    names = {w: _locals(tree, w) for w in ("create_model", "field_validator", "model_validator", "computed_field", "BaseModel")}
    if not names["create_model"]:
        return tree
    funcs = _functions(tree)
    out = []
    for node in tree.body:
        if (isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name)
                and isinstance(node.value, ast.Call) and _is(node.value.func, names["create_model"])):
            try:
                if not names["BaseModel"]:
                    imp = ast.ImportFrom(module="pydantic", names=[ast.alias("BaseModel", "BaseModel")], level=0)
                    out.append(ast.copy_location(imp, node))
                    names["BaseModel"] = {"BaseModel"}
                out.append(_class(copy.deepcopy(node), names, funcs, namespace))
                continue
            except _Skip:
                pass
        out.append(node)
    tree.body = out
    ast.fix_missing_locations(tree)
    return tree

"""`__get_pydantic_core_schema__` and `GetPydanticSchema`, read statically: the schema they return is rebuilt as the
equivalent annotation (`Annotated[str, Field(max_length=10), AfterValidator(f), PlainSerializer(g)]`), which the
translator already compiles. Nothing of the project runs: the method's statements are interpreted over a closed set
of `pydantic_core.core_schema` constructors; anything else is refused with file:line.

Supported: the scalar schemas (`int_schema()`, `str_schema(max_length=...)`...), `handler(source)` / `handler(T)`,
the `no_info_*` / `with_info_*` after, before, wrap and plain validator functions (with `serialization=`),
`chain_schema([schema, plain validator...])`, `plain_serializer_function_ser_schema(f)`, assignments,
`schema['max_length'] = 10`, and `if`/`assert` whose condition is known at translation (`schema['type'] != 'str'`,
a field of the metadata instance). The validator and serializer functions may be lambdas, functions, and methods
of the class (`cls._validate`, `self.validate`)."""
from __future__ import annotations

import ast
import copy
from dataclasses import dataclass, field

from .ir import TranspileError

SCALARS = {"int_schema": "int", "str_schema": "str", "float_schema": "float", "bool_schema": "bool",
           "bytes_schema": "bytes"}
# constraint keys of the scalar schemas, as Field() options
SCALAR_KEYS = {"int": {"gt", "ge", "lt", "le", "multiple_of", "strict"},
               "float": {"gt", "ge", "lt", "le", "multiple_of", "strict", "allow_inf_nan"},
               "str": {"min_length", "max_length", "pattern", "strict"},
               "bytes": {"min_length", "max_length", "strict"},
               "bool": {"strict"}}
VALIDATORS = {"no_info_after_validator_function": "after", "with_info_after_validator_function": "after",
              "no_info_before_validator_function": "before", "with_info_before_validator_function": "before",
              "no_info_wrap_validator_function": "wrap", "with_info_wrap_validator_function": "wrap",
              "no_info_plain_validator_function": "plain", "with_info_plain_validator_function": "plain"}
WRAPPERS = {"after": "AfterValidator", "before": "BeforeValidator", "wrap": "WrapValidator", "plain": "PlainValidator"}


@dataclass
class Schema:
    """A core schema: the annotation it comes from, its constraints, the validators around it (innermost first),
    its serializer."""
    base: ast.AST | None
    type: str | None = None
    cons: dict = field(default_factory=dict)
    vals: list = field(default_factory=list)  # (mode, function expression)
    ser: tuple | None = None  # (function expression, when_used)


class Handler:
    """GetCoreSchemaHandler"""


class Source:
    """the annotation the hook receives (`source` / `source_type`)"""


@dataclass
class Ref:
    """`cls` (a class expression) or `self` (the metadata instance: its call, its dataclass fields)"""
    expr: ast.AST
    fields: dict = field(default_factory=dict)
    methods: set = field(default_factory=set)
    sym: object = None  # the class it names (`source is CompressedString`)


class Reader:
    def __init__(self, proj, module: str, hook_module: str, file: str, source: Schema, self_class: bool = False):
        self.p = proj
        self.module = module  # where the expressions built here are compiled (the field's module)
        self.hook_module = hook_module  # where the hook is written (its names, its errors)
        self.file = file
        self.source = source
        self.self_class = self_class  # the hook of the field's own class: handler(source) would recurse

    def err(self, msg: str, node: ast.AST) -> TranspileError:
        return TranspileError(msg, node, self.file)

    # ------------------------------------------------------------------ statements

    def run(self, body: list, env: dict):
        for st in body:
            if isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant):
                continue  # docstring
            if isinstance(st, ast.Return):
                if st.value is None:
                    raise self.err("__get_pydantic_core_schema__: a schema must be returned", st)
                return self.expr(st.value, env)
            if isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Name):
                env[st.targets[0].id] = self.expr(st.value, env)
                continue
            if (isinstance(st, ast.Assign) and len(st.targets) == 1 and isinstance(st.targets[0], ast.Subscript)
                    and isinstance(st.targets[0].value, ast.Name) and isinstance(env.get(st.targets[0].value.id), Schema)):
                s = env[st.targets[0].value.id]
                key = self.expr(st.targets[0].slice, env)
                if s.vals or s.type not in SCALAR_KEYS or key not in SCALAR_KEYS[s.type]:
                    raise self.err(f"__get_pydantic_core_schema__: setting schema[{key!r}] is not supported", st)
                v = self.expr(st.value, env)
                if not isinstance(v, (int, float, str, bool)) or v is None:
                    raise self.err(f"__get_pydantic_core_schema__: schema[{key!r}] must be set to a constant", st)
                s.cons[key] = v
                continue
            if isinstance(st, ast.If):
                r = self.run(st.body if self.cond(st.test, env) else st.orelse, env)
                if r is not None:
                    return r
                continue
            if isinstance(st, ast.Assert):
                if not self.cond(st.test, env):
                    raise self.err("__get_pydantic_core_schema__: this assertion fails (pydantic raises when the model is built)", st)
                continue
            if isinstance(st, ast.Raise):
                raise self.err("__get_pydantic_core_schema__: this hook raises (pydantic refuses the model)", st)
            raise self.err(f"__get_pydantic_core_schema__: `{ast.unparse(st)}` is not supported", st)
        return None

    def cond(self, node: ast.AST, env: dict) -> bool:
        v = self.expr(node, env)
        if isinstance(v, (Schema, Ref, Handler, Source)) or (isinstance(v, tuple) and v and v[0] == "fn"):
            return True
        return bool(v)

    # ------------------------------------------------------------------ expressions

    def expr(self, node: ast.AST, env: dict):
        if isinstance(node, ast.Constant):
            return node.value
        if isinstance(node, ast.Name):
            if node.id in env:
                return env[node.id]
            t = self.p.resolve(self.hook_module, node)
            if t is not None and type(t).__name__ == "Sym" and isinstance(self.p.ix.definition(t), ast.ClassDef):
                return Ref(node, sym=t)
            v = self.p.const(node, self.hook_module)
            if v is None and node.id != "None":
                raise self.err(f"__get_pydantic_core_schema__: `{node.id}` is not supported here (a constant, a hook parameter or a schema)", node)
            return v
        if isinstance(node, (ast.List, ast.Tuple)):
            return [self.expr(e, env) for e in node.elts]
        if isinstance(node, ast.Lambda):
            free = {n.id for n in ast.walk(node.body) if isinstance(n, ast.Name)} - {a.arg for a in node.args.args}
            if free & set(env):
                raise self.err("__get_pydantic_core_schema__: a lambda using the hook's names is not supported", node)
            self.same_module(node)
            return ("fn", node)
        if isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name) and isinstance(env.get(node.value.id), Ref):
            ref = env[node.value.id]
            if node.attr in ref.fields:
                return ref.fields[node.attr]
            if node.attr not in ref.methods:
                raise self.err(f"__get_pydantic_core_schema__: `{ast.unparse(node)}` is not supported (a dataclass field given as a constant, or a "
                               "method of the class)", node)
            return ("fn", self.method_lambda(ref, node.attr, node))
        if isinstance(node, ast.Subscript) and isinstance(env.get(getattr(node.value, "id", None)), Schema):
            s = env[node.value.id]
            key = self.expr(node.slice, env)
            if key == "type" and not s.vals:
                return s.type
            if key in s.cons:
                return s.cons[key]
            raise self.err(f"__get_pydantic_core_schema__: reading schema[{key!r}] is not supported", node)
        if isinstance(node, ast.Compare) and len(node.ops) == 1:
            a, b = self.expr(node.left, env), self.expr(node.comparators[0], env)
            op = type(node.ops[0])
            if op in (ast.Is, ast.IsNot) and (isinstance(a, Source) or isinstance(b, Source)):
                other = b if isinstance(a, Source) else a
                src = self.p.resolve(self.module, self.source.base) if isinstance(self.source.base, (ast.Name, ast.Attribute)) else None
                same = isinstance(other, Ref) and other.sym is not None and other.sym == src
                return same if op is ast.Is else not same
            ops = {ast.Eq: lambda x, y: x == y, ast.NotEq: lambda x, y: x != y, ast.Is: lambda x, y: x is y,
                   ast.IsNot: lambda x, y: x is not y, ast.In: lambda x, y: x in y, ast.NotIn: lambda x, y: x not in y}
            if op in ops:
                return ops[op](a, b)
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.Not):
            return not self.cond(node.operand, env)
        if isinstance(node, ast.BoolOp):
            vals = [self.cond(v, env) for v in node.values]
            return all(vals) if isinstance(node.op, ast.And) else any(vals)
        if isinstance(node, ast.Call):
            return self.call(node, env)
        raise self.err(f"__get_pydantic_core_schema__: `{ast.unparse(node)}` is not supported", node)

    def call(self, node: ast.Call, env: dict):
        f = node.func
        target = self.expr(f.value, env) if isinstance(f, ast.Attribute) and isinstance(f.value, ast.Name) and f.value.id in env else None
        if isinstance(f, ast.Name) and isinstance(env.get(f.id), Handler) or (
                isinstance(target, Handler) and f.attr == "generate_schema"):
            if len(node.args) != 1 or node.keywords:
                raise self.err("__get_pydantic_core_schema__: handler(type) takes one argument", node)
            return self.schema_of(node.args[0], env)
        name = self.p.resolve(self.module_of(f, env), f)
        dotted = getattr(name, "dotted", "")
        if not dotted.startswith("pydantic_core.core_schema."):
            raise self.err(f"__get_pydantic_core_schema__: `{ast.unparse(f)}(...)` is not supported (pydantic_core.core_schema constructors only)", node)
        fn = dotted.rsplit(".", 1)[1]
        kw = {k.arg: k.value for k in node.keywords}
        if fn in SCALARS:
            if node.args:
                raise self.err(f"__get_pydantic_core_schema__: core_schema.{fn}() takes keyword arguments only", node)
            t = SCALARS[fn]
            bad = [k for k in kw if k not in SCALAR_KEYS[t]]
            if bad:
                raise self.err(f"__get_pydantic_core_schema__: core_schema.{fn}({bad[0]}=) is not supported", node)
            return Schema(ast.Name(id=t, ctx=ast.Load()), t, {k: self.expr(v, env) for k, v in kw.items()})
        if fn in VALIDATORS:
            mode = VALIDATORS[fn]
            params = ["function"] if mode == "plain" else ["function", "schema"]
            args = dict(zip(params, node.args))
            extra = [k for k in kw if k not in {*params, "serialization"}]
            if extra or len(node.args) > len(params):
                raise self.err(f"__get_pydantic_core_schema__: core_schema.{fn}({(extra or ['...'])[0]}=) is not supported", node)
            args.update({k: v for k, v in kw.items() if k in params})
            func = self.function(args.get("function"), env, node)
            if mode == "plain":
                s = Schema(None)
            else:
                inner = self.expr(args["schema"], env) if "schema" in args else None
                if not isinstance(inner, Schema):
                    raise self.err(f"__get_pydantic_core_schema__: core_schema.{fn}(): a schema is expected", node)
                s = copy.deepcopy(inner)
            s.vals.append((mode, func))
            if "serialization" in kw:
                s.ser = self.serializer(kw["serialization"], env)
            return s
        if fn == "plain_serializer_function_ser_schema":
            return ("ser", self.serializer(node, env))
        if fn == "chain_schema":
            steps = self.expr(node.args[0], env) if len(node.args) == 1 and not node.keywords else None
            if not (isinstance(steps, list) and steps and isinstance(steps[0], Schema)):
                raise self.err("__get_pydantic_core_schema__: core_schema.chain_schema([schema, ...]) is supported with a list of schemas", node)
            s = copy.deepcopy(steps[0])
            for st in steps[1:]:
                if not (isinstance(st, Schema) and st.base is None and len(st.vals) == 1 and st.vals[0][0] == "plain"):
                    raise self.err("__get_pydantic_core_schema__: core_schema.chain_schema(): only plain validator functions may follow the first schema",
                                   node)
                s.vals.append(("after", st.vals[0][1]))
            return s
        raise self.err(f"__get_pydantic_core_schema__: core_schema.{fn}() is not supported", node)

    def method_lambda(self, ref: Ref, name: str, at: ast.AST) -> ast.Lambda:
        """`cls.validate` / `self.validate` as a function: `lambda value, info: Cls.validate(value, info)` (a method
        read from a class is not a value the translator keeps; calling it is)"""
        cls = ref.sym if ref.sym is not None else None
        d = self.p.ix.definition(cls) if cls is not None else None
        fn = next((x for x in getattr(d, "body", []) if isinstance(x, ast.FunctionDef) and x.name == name), None)
        if fn is None:
            raise self.err(f"__get_pydantic_core_schema__: `{name}`: a method of the class is expected", at)
        deco = {getattr(x, "id", None) for x in fn.decorator_list}
        params = fn.args.posonlyargs + fn.args.args
        if "staticmethod" not in deco:
            params = params[1:]
        if fn.args.vararg or fn.args.kwonlyargs or fn.args.defaults:
            raise self.err(f"__get_pydantic_core_schema__: `{name}`: a method of positional parameters without defaults is expected", at)
        names = [f"p2a_{p.arg}" for p in params]
        call = ast.Call(func=ast.Attribute(value=copy.deepcopy(ref.expr), attr=name, ctx=ast.Load()),
                        args=[ast.Name(id=n, ctx=ast.Load()) for n in names], keywords=[])
        lam = ast.Lambda(args=ast.arguments(posonlyargs=[], args=[ast.arg(arg=n) for n in names], kwonlyargs=[],
                                            kw_defaults=[], defaults=[]), body=call)
        ast.copy_location(lam, at)
        ast.fix_missing_locations(lam)
        return lam

    def module_of(self, f: ast.AST, env: dict) -> str:
        return self.hook_module

    def same_module(self, node: ast.AST) -> None:
        if self.hook_module != self.module:
            raise self.err("__get_pydantic_core_schema__: a function written in the hook's module, used by a field of another module, is not "
                           "supported", node)

    def function(self, node: ast.AST | None, env: dict, call: ast.AST):
        if node is None:
            raise self.err("__get_pydantic_core_schema__: a validator function is expected", call)
        if isinstance(node, (ast.Name, ast.Attribute)) and not (
                isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name) and node.value.id in env):
            self.same_module(node)
            v = ("fn", node)
        else:
            v = self.expr(node, env)
        if not (isinstance(v, tuple) and v and v[0] == "fn"):
            raise self.err(f"__get_pydantic_core_schema__: `{ast.unparse(node)}`: a function is expected", node)
        return v[1]

    def serializer(self, node: ast.AST, env: dict) -> tuple:
        if not isinstance(node, ast.Call):
            raise self.err("__get_pydantic_core_schema__: serialization=core_schema.plain_serializer_function_ser_schema(f) is expected", node)
        t = self.p.resolve(self.hook_module, node.func)
        if getattr(t, "dotted", "") != "pydantic_core.core_schema.plain_serializer_function_ser_schema":
            raise self.err(f"__get_pydantic_core_schema__: `{ast.unparse(node.func)}` is not supported as serialization=", node)
        kw = {k.arg: k.value for k in node.keywords}
        bad = [k for k in kw if k not in {"function", "info_arg", "return_schema", "when_used"}]
        if bad or len(node.args) > 1:
            raise self.err(f"__get_pydantic_core_schema__: plain_serializer_function_ser_schema({(bad or ['...'])[0]}=) is not supported", node)
        info = self.expr(kw["info_arg"], env) if "info_arg" in kw else None
        if info:
            raise self.err("__get_pydantic_core_schema__: plain_serializer_function_ser_schema(info_arg=True) is not supported", node)
        func = self.function(node.args[0] if node.args else kw.get("function"), env, node)
        when = self.expr(kw["when_used"], env) if "when_used" in kw else "always"
        return func, when

    def schema_of(self, node: ast.AST, env: dict) -> Schema:
        v = env.get(node.id) if isinstance(node, ast.Name) else None
        if isinstance(v, Source):
            if self.self_class:
                raise self.err("__get_pydantic_core_schema__: handler(source) on the class that defines the hook is not supported", node)
            return copy.deepcopy(self.source)
        if isinstance(v, Schema):
            return copy.deepcopy(v)
        name = getattr(node, "id", None)
        if name in {"int", "str", "float", "bool", "bytes"}:
            return Schema(ast.Name(id=name, ctx=ast.Load()), name)
        raise self.err(f"__get_pydantic_core_schema__: handler({ast.unparse(node)}) is not supported (the source type or a builtin scalar)", node)


def _lib(root: str, name: str) -> ast.Attribute:
    return ast.Attribute(value=ast.Name(id=f"__p2a_{root}", ctx=ast.Load()), attr=name, ctx=ast.Load())


def annotation(schema: Schema, keep: list, at: ast.AST) -> ast.AST:
    """The annotation a schema stands for: `Annotated[base, keep..., Field(...), <validators>, PlainSerializer(...)]`
    (`keep`: the metadata before the hook, which constrain the base type)"""
    base = schema.base if schema.base is not None else ast.Name(id="object", ctx=ast.Load())
    metas = list(keep)
    if schema.cons:
        metas.append(ast.Call(func=_lib("pydantic", "Field"), args=[],
                              keywords=[ast.keyword(arg=k, value=ast.Constant(v)) for k, v in schema.cons.items()]))
    for mode, fn in schema.vals:
        metas.append(ast.Call(func=_lib("pydantic", WRAPPERS[mode]), args=[fn], keywords=[]))
    if schema.ser is not None:
        fn, when = schema.ser
        metas.append(ast.Call(func=_lib("pydantic", "PlainSerializer"), args=[fn],
                              keywords=[] if when == "always" else [ast.keyword(arg="when_used", value=ast.Constant(when))]))
    if not metas:
        return base
    out = ast.Subscript(value=_lib("typing", "Annotated"),
                        slice=ast.Tuple(elts=[base, *metas], ctx=ast.Load()), ctx=ast.Load())
    ast.copy_location(out, at)
    ast.fix_missing_locations(out)
    return out

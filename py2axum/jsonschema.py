"""Pydantic's JSON Schema (`model_json_schema()`, pydantic 2.13 `GenerateJsonSchema`), computed statically from
annotations: the `inputSchema` of an MCP tool is a constant of the binary. A closed subset; anything else is
refused with file:line (tests/test_jsonschema.py compares every case with the real pydantic)."""
from __future__ import annotations

import ast
import inspect

from .ir import TranspileError
from .modules import Ext, Sym

SCALARS = {
    "builtins.str": {"type": "string"},
    "builtins.int": {"type": "integer"},
    "builtins.float": {"type": "number"},
    "builtins.bool": {"type": "boolean"},
    "datetime.date": {"format": "date", "type": "string"},
    "datetime.datetime": {"format": "date-time", "type": "string"},
    "datetime.time": {"format": "time", "type": "string"},
    "datetime.timedelta": {"format": "duration", "type": "string"},
    "uuid.UUID": {"format": "uuid", "type": "string"},
}
CONSTRAINTS = {
    "string": {"min_length": "minLength", "max_length": "maxLength", "pattern": "pattern"},
    "integer": {"ge": "minimum", "gt": "exclusiveMinimum", "le": "maximum", "lt": "exclusiveMaximum",
                "multiple_of": "multipleOf"},
    "array": {"min_length": "minItems", "max_length": "maxItems"},
}
CONSTRAINTS["number"] = CONSTRAINTS["integer"]
FIELD_KW = {"default", "default_factory", "description", "title", "examples"} | {
    k for c in CONSTRAINTS.values() for k in c}
_MISSING = object()
# model_config extra= -> additionalProperties
EXTRA = {"ignore": None, "forbid": False, "allow": True}


def title_of(name: str) -> str:
    """pydantic's `GenerateJsonSchema.get_title_from_name`."""
    return name.title().replace("_", " ").strip()


def sort_schema(value, parent_key=None):
    """pydantic's `_sort_recursive`: keys sorted except under `properties` and `default`."""
    if isinstance(value, dict):
        keys = list(value) if parent_key in ("properties", "default") else sorted(value)
        return {k: sort_schema(value[k], k) for k in keys}
    if isinstance(value, list):
        return [sort_schema(v, parent_key) for v in value]
    return value


class SchemaGen:
    def __init__(self, proj):
        self.p = proj
        self.defs: dict[str, dict] = {}
        self._building: set = set()
        self._def_syms: dict[str, Sym] = {}
        self._extra: dict[Sym, str | None] = {}

    def err(self, msg: str, node: ast.AST, module: str) -> TranspileError:
        return TranspileError(f"JSON schema: {msg}", node, self.p.src(module))

    def ref(self, module: str, node: ast.AST):
        return self.p.resolve(module, node) if isinstance(node, (ast.Name, ast.Attribute)) else None

    def dotted(self, module: str, node: ast.AST) -> str | None:
        t = self.ref(module, node)
        if isinstance(t, Ext):
            return t.dotted
        if isinstance(node, ast.Name) and node.id in {"str", "int", "float", "bool", "dict", "list", "set"}:
            return f"builtins.{node.id}"
        return None

    # ---------------------------------------------------------------- Field(...)

    def field_info(self, call: ast.Call, module: str) -> dict:
        info = {}
        args = list(call.args)
        if args:
            if len(args) > 1:
                raise self.err("Field() with more than one positional argument", call, module)
            info["default"] = args[0]
        for k in call.keywords:
            if k.arg not in FIELD_KW:
                raise self.err(f"Field({k.arg}=...) is not supported in a schema", k, module)
            info[k.arg] = k.value
        return info

    def is_field_call(self, module: str, node: ast.AST) -> bool:
        return isinstance(node, ast.Call) and (self.dotted(module, node.func) or "").endswith(".Field")

    def literal(self, node: ast.AST, module: str):
        try:
            v = ast.literal_eval(node)
        except ValueError:
            raise self.err(f"`{ast.unparse(node)}` is not a constant", node, module) from None
        if not isinstance(v, (str, int, float, bool, type(None), list, dict)):
            raise self.err(f"`{ast.unparse(node)}` is not a JSON value", node, module)
        return v

    # ---------------------------------------------------------------- annotations

    def split_annotated(self, ann: ast.AST, module: str) -> tuple[ast.AST, dict]:
        """`Annotated[T, Field(...)]` -> (T, field info)."""
        if isinstance(ann, ast.Subscript) and (self.dotted(module, ann.value) or "").split(".")[-1] == "Annotated":
            elts = ann.slice.elts if isinstance(ann.slice, ast.Tuple) else [ann.slice]
            info: dict = {}
            for meta in elts[1:]:
                if not self.is_field_call(module, meta):
                    raise self.err(f"Annotated metadata `{ast.unparse(meta)}` is not supported", meta, module)
                info.update(self.field_info(meta, module))
            return elts[0], info
        return ann, {}

    def union_members(self, ann: ast.AST, module: str) -> list[ast.AST] | None:
        if isinstance(ann, ast.BinOp) and isinstance(ann.op, ast.BitOr):
            return (self.union_members(ann.left, module) or [ann.left]) + (self.union_members(ann.right, module) or [ann.right])
        if isinstance(ann, ast.Subscript):
            name = (self.dotted(module, ann.value) or "").split(".")[-1]
            if name == "Optional":
                return [ann.slice, ast.Constant(None)]
            if name == "Union":
                return list(ann.slice.elts) if isinstance(ann.slice, ast.Tuple) else [ann.slice]
        return None

    @staticmethod
    def is_none(node: ast.AST) -> bool:
        return isinstance(node, ast.Constant) and node.value is None

    def type_schema(self, ann: ast.AST, module: str, cons: dict) -> dict:
        """The schema of a type, `cons` (Field constraints) applied to it (to its non-None member)."""
        members = self.union_members(ann, module)
        if members is not None:
            non_none = [m for m in members if not self.is_none(m)]
            if cons and len(non_none) != 1:
                raise self.err("constraints on a union of several types", ann, module)
            out = [self.type_schema(m, module, cons) for m in non_none]
            if len(non_none) != len(members):
                out.append({"type": "null"})
            return {"anyOf": out} if len(out) > 1 else out[0]
        if self.is_none(ann):
            return {"type": "null"}
        if isinstance(ann, ast.Subscript):
            name = self.dotted(module, ann.value) or ""
            short = name.split(".")[-1]
            args = ann.slice.elts if isinstance(ann.slice, ast.Tuple) else [ann.slice]
            if short == "Literal":
                return self.with_cons(self.literal_schema(args, module), cons, ann, module)
            if short in {"list", "List", "Sequence", "set", "Set"}:
                s = {"items": self.type_schema(args[0], module, {}), "type": "array"}
                if short in {"set", "Set"}:
                    s["uniqueItems"] = True
                return self.with_cons(s, cons, ann, module)
            if short in {"dict", "Dict", "Mapping"}:
                if len(args) != 2 or self.dotted(module, args[0]) != "builtins.str":
                    raise self.err(f"`{ast.unparse(ann)}`: only dict[str, T] is supported", ann, module)
                v = self.type_schema(args[1], module, {})
                return self.with_cons({"additionalProperties": v if v else True, "type": "object"}, cons, ann, module)
            raise self.err(f"type `{ast.unparse(ann)}` is not supported", ann, module)
        name = self.dotted(module, ann)
        if name in SCALARS:
            return self.with_cons(dict(SCALARS[name]), cons, ann, module)
        if name in {"typing.Any", "typing_extensions.Any"}:
            return self.with_cons({}, cons, ann, module)
        if name in {"builtins.dict"}:
            return self.with_cons({"additionalProperties": True, "type": "object"}, cons, ann, module)
        if name in {"builtins.list"}:
            return self.with_cons({"items": {}, "type": "array"}, cons, ann, module)
        t = self.ref(module, ann)
        if isinstance(t, Sym) and t in self.p.fe.schema_syms:
            if cons:
                raise self.err(f"constraints on the model {t.name}", ann, module)
            self.model_def(t)
            return {"$ref": f"#/$defs/{t.name}"}
        raise self.err(f"type `{ast.unparse(ann)}` is not supported", ann, module)

    def is_model(self, ann: ast.AST, module: str) -> bool:
        members = self.union_members(ann, module)
        if members is not None:
            non_none = [m for m in members if not self.is_none(m)]
            return len(non_none) == 1 and len(members) == 2 and self.is_model(non_none[0], module)
        t = self.ref(module, ann)
        return isinstance(t, Sym) and t in self.p.fe.schema_syms

    def literal_schema(self, args: list, module: str) -> dict:
        values = [self.literal(a, module) for a in args]
        types = {type(v) for v in values}
        if len(values) == 1:
            s = {"const": values[0]}
        else:
            s = {"enum": values}
        kind = {str: "string", int: "integer", bool: "boolean", float: "number", type(None): "null"}
        if len(types) == 1 and next(iter(types)) in kind:
            s["type"] = kind[next(iter(types))]
        elif len(values) > 1:
            raise self.err("Literal of mixed types", args[0], module)
        return s

    def with_cons(self, s: dict, cons: dict, node: ast.AST, module: str) -> dict:
        for k, v in cons.items():
            key = CONSTRAINTS.get(s.get("type"), {}).get(k)
            if key is None:
                raise self.err(f"constraint {k}= on `{ast.unparse(node)}` is not supported", node, module)
            s[key] = self.literal(v, module)
        return s

    # ---------------------------------------------------------------- fields and models

    def field(self, name: str, ann: ast.AST, default, module: str) -> tuple[dict, bool]:
        """(property schema, required) of `name: ann = default` (default: an AST, or _MISSING)."""
        inner, info = self.split_annotated(ann, module)
        if default is not _MISSING and self.is_field_call(module, default):
            info = {**info, **self.field_info(default, module)}
            default = info.get("default", _MISSING)
        elif default is not _MISSING:
            info = {**info, "default": default}
        else:
            default = info.get("default", _MISSING)
        if isinstance(default, ast.Constant) and default.value is Ellipsis:
            info.pop("default", None)
            default = _MISSING
        cons = {k: v for k, v in info.items() if k not in {"default", "default_factory", "description", "title", "examples"}}
        s = self.type_schema(inner, module, cons)
        s = dict(s)
        if "$ref" in s and any(k in info for k in ("description", "title", "examples", "default")):
            s = {"$ref": s["$ref"]}
        if "title" in info:
            s["title"] = self.literal(info["title"], module)
        elif not self.is_model(inner, module):
            # pydantic's field_title_should_be_set: no title on a model (optional or not), its own title says it
            s["title"] = title_of(name)
        if "description" in info:
            s["description"] = self.literal(info["description"], module)
        if "examples" in info:
            s["examples"] = self.literal(info["examples"], module)
        required = "default" not in info and "default_factory" not in info
        if "default" in info:
            s["default"] = self.literal(info["default"], module)
        return s, required

    def class_fields(self, sym: Sym) -> list[tuple[str, ast.AST, object]]:
        node = self.p.fe.schema_syms[sym]
        out: list = []
        for b in node.bases:
            t = self.ref(sym.module, b)
            if isinstance(t, Sym) and t in self.p.fe.schema_syms:
                out = self.class_fields(t)
                self._extra[sym] = self._extra.get(t)
            elif not (isinstance(t, Ext) and t.dotted.split(".")[-1] == "BaseModel"):
                raise self.err(f"base class `{ast.unparse(b)}` of {sym.name}", b, sym.module)
        for st in node.body:
            if isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name):
                if isinstance(st.annotation, ast.Subscript) and (ast.unparse(st.annotation.value).split(".")[-1] == "ClassVar"):
                    continue
                out = [f for f in out if f[0] != st.target.id]
                out.append((st.target.id, st.annotation, st.value if st.value is not None else _MISSING))
            elif isinstance(st, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "model_config" for t in st.targets):
                ok = isinstance(st.value, ast.Call) and all(
                    k.arg in {"from_attributes", "str_strip_whitespace"}
                    or k.arg == "extra" and isinstance(k.value, ast.Constant) and k.value.value in EXTRA
                    for k in st.value.keywords)
                if not ok:
                    raise self.err(f"{sym.name}.model_config other than from_attributes, extra=", st, sym.module)
                for k in st.value.keywords:
                    if k.arg == "extra":
                        self._extra[sym] = k.value.value
            elif isinstance(st, ast.ClassDef) and st.name == "Config":
                raise self.err(f"{sym.name}: class Config", st, sym.module)
            elif isinstance(st, ast.FunctionDef) and st.decorator_list:
                raise self.err(f"{sym.name}.{st.name}: decorated methods (validators, computed fields)", st, sym.module)
        return out

    def object_schema(self, fields: list, module_of, title: str, doc: str | None) -> dict:
        props, required = {}, []
        for name, ann, default, module in fields:
            s, req = self.field(name, ann, default, module)
            props[name] = s
            if req:
                required.append(name)
        s = {"properties": props, "title": title, "type": "object"}
        if required:
            s["required"] = required
        if doc:
            s["description"] = inspect.cleandoc(doc)
        return s

    def model_def(self, sym: Sym) -> None:
        other = self._def_syms.setdefault(sym.name, sym)
        if other != sym:
            # pydantic then qualifies both names with their module: not reproduced
            raise self.err(f"two models named {sym.name} ({other.module} and {sym.module}) in one schema",
                           self.p.fe.schema_syms[sym], sym.module)
        if sym.name in self.defs or sym in self._building:
            return
        self._building.add(sym)
        node = self.p.fe.schema_syms[sym]
        fields = [(n, a, d, sym.module) for n, a, d in self.class_fields(sym)]
        self.defs[sym.name] = self.object_schema(fields, None, sym.name, ast.get_docstring(node, clean=False))
        if EXTRA.get(self._extra.get(sym)) is not None:
            self.defs[sym.name]["additionalProperties"] = EXTRA[self._extra[sym]]
        self._building.discard(sym)

    def model(self, sym: Sym) -> dict:
        """`Model.model_json_schema()`: the class's own schema at the top, the models it uses under `$defs`."""
        self.model_def(sym)
        if sym.name in self.defs and any(f'"#/$defs/{sym.name}"' in repr(v).replace("'", '"')
                                         for v in self.defs.values()):
            raise self.err(f"{sym.name} refers to itself (a recursive model's schema is a $ref)",
                           self.p.fe.schema_syms[sym], sym.module)
        s = dict(self.defs.pop(sym.name))
        if self.defs:
            s["$defs"] = dict(self.defs)
        return sort_schema(s)

    def arguments(self, fn: ast.AsyncFunctionDef | ast.FunctionDef, module: str) -> dict:
        """FastMCP's `<function>Arguments` model schema, top-level keys in the wire model's order (properties,
        required, type, then the others as pydantic sorted them)."""
        a = fn.args
        if a.vararg or a.kwarg or a.posonlyargs:
            raise self.err(f"{fn.name}(): *args, **kwargs and positional-only parameters", fn, module)
        params = [*a.args, *a.kwonlyargs]
        defaults = [_MISSING] * (len(a.args) - len(a.defaults)) + list(a.defaults) + [
            d if d is not None else _MISSING for d in a.kw_defaults]
        fields = []
        for p, d in zip(params, defaults):
            if p.annotation is None:
                raise self.err(f"{fn.name}(): parameter `{p.arg}` without annotation", p, module)
            fields.append((p.arg, p.annotation, d, module))
        s = self.object_schema(fields, None, f"{fn.name}Arguments", None)
        if self.defs:
            s["$defs"] = dict(self.defs)
        s = sort_schema(s)
        head = {k: s[k] for k in ("properties", "required", "type") if k in s}
        return head | {k: v for k, v in s.items() if k not in head}

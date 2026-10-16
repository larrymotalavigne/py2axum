"""SQLModel, rewritten at load into what the translator compiles: SQLAlchemy mapped classes and Pydantic models.

SQLModel (0.0.2x) is a Pydantic model and, with `table=True`, a SQLAlchemy mapped class at once. A module that
imports it is rewritten before anything reads it:

- `class X(SQLModel)` (no table): a Pydantic model (`BaseModel`, `from_attributes=True` as SQLModelConfig sets it),
  its `Field(...)` without the column options (`primary_key=`, `index=`...);
- `class T(Base, table=True)`: a mapped class of the shared declarative base (`_py2axum_shim_sqlmodel.SQLModel`,
  whose `metadata` is SQLModel's), one column per field, inherited ones first, as SQLModel's
  `get_column_from_field` builds it (type from the annotation, `nullable` from `Optional`, `index=`, `unique=`,
  `primary_key=`, the default); next to it `T_p2a_sm_view`, a Pydantic model of the same fields that validates and
  dumps it where Pydantic sees `T` (a route's parameter or return annotation, `response_model=`,
  `T.model_validate(x)`), and `T_p2a_sm_build(v)` that turns a validated view into an instance of `T`;
- `session.exec(select(A)...)` -> `session.scalars(...)` (one entity or column), `session.execute(...)` (several);
- `obj.sqlmodel_update(data)` -> a generated function with SQLModel's rules.

Anything else of SQLModel (relationships, `sa_column=`, `Relationship`, `col()`, a table class used as a Pydantic
type from another module...) is left as written: the translator then refuses it with its own message."""
from __future__ import annotations

import ast
import copy

SHIM_MODULE = "_py2axum_shim_sqlmodel"
SUPPORTED = {"SQLModel", "Field", "Session", "create_engine", "select"}
SA_FIELD_KW = {"primary_key", "index", "unique", "nullable", "foreign_key", "ondelete"}
SA_REFUSED_KW = {"sa_type", "sa_column", "sa_column_args", "sa_column_kwargs", "sa_relationship",
                 "sa_relationship_args", "sa_relationship_kwargs"}
# SQLModel's get_sqlalchemy_type for a plain annotation (str -> AutoString, a VARCHAR without length)
SA_TYPES = {"str": "String", "int": "Integer", "float": "Float", "bool": "Boolean", "bytes": "LargeBinary",
            "date": "Date", "time": "Time", "timedelta": "Interval", "UUID": "Uuid", "Decimal": "Numeric"}


def imports_sqlmodel(tree: ast.Module) -> bool:
    return any(isinstance(n, ast.ImportFrom) and n.module == "sqlmodel" and n.level == 0 for n in tree.body)


def _name(n: str) -> ast.Name:
    return ast.Name(id=n, ctx=ast.Load())


def _attr(mod: str, name: str) -> ast.Attribute:
    return ast.Attribute(value=_name(mod), attr=name, ctx=ast.Load())


def _base_annotation(ann: ast.AST) -> tuple[ast.AST, bool]:
    """(the annotation without None, whether it allowed None): `int | None`, `Optional[int]`, `Union[int, None]`"""
    if isinstance(ann, ast.BinOp) and isinstance(ann.op, ast.BitOr):
        parts = []

        def flat(x):
            if isinstance(x, ast.BinOp) and isinstance(x.op, ast.BitOr):
                flat(x.left)
                flat(x.right)
            else:
                parts.append(x)
        flat(ann)
        rest = [p for p in parts if not (isinstance(p, ast.Constant) and p.value is None)]
        if len(rest) == 1:
            return rest[0], len(rest) != len(parts)
        return ann, False
    if isinstance(ann, ast.Subscript):
        head = ann.value.attr if isinstance(ann.value, ast.Attribute) else getattr(ann.value, "id", "")
        if head == "Optional":
            return ann.slice, True
        if head == "Union" and isinstance(ann.slice, ast.Tuple):
            rest = [p for p in ann.slice.elts if not (isinstance(p, ast.Constant) and p.value is None)]
            if len(rest) == 1:
                return rest[0], len(rest) != len(ann.slice.elts)
    return ann, False


class Rewriter:
    def __init__(self, tree: ast.Module):
        self.tree = tree
        self.names: dict[str, str] = {}  # local name -> sqlmodel name
        self.nontable: dict[str, ast.ClassDef] = {}  # SQLModel classes without a table
        self.tables: dict[str, dict] = {}  # table class -> {"fields": [...], "node": ClassDef}

    # ------------------------------------------------------------------ module

    def run(self) -> ast.Module:
        body = []
        for node in self.tree.body:
            if isinstance(node, ast.ImportFrom) and node.module == "sqlmodel" and node.level == 0:
                body.extend(self.imports(node))
                continue
            body.append(node)
        if not self.names:
            return self.tree
        out = []
        for node in body:
            if isinstance(node, ast.ClassDef) and self.is_sqlmodel_class(node):
                out.extend(self.klass(node))
            else:
                out.append(node)
        self.tree.body = out
        if self.tables:
            for node in self.tree.body:
                if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    self.function(node)
                elif isinstance(node, ast.ClassDef) and not node.name.endswith("_p2a_sm_view"):
                    pass
        _Calls(self).visit(self.tree)
        if self.tables:
            self.tree.body.append(self.update_fn())
        ast.fix_missing_locations(self.tree)
        return self.tree

    def imports(self, node: ast.ImportFrom) -> list[ast.stmt]:
        """`from sqlmodel import ...`: the supported names rebound, the others left to the translator"""
        keep, out = [], []
        for a in node.names:
            local = a.asname or a.name
            if a.name not in SUPPORTED:
                keep.append(a)
                continue
            self.names[local] = a.name
            src = {"SQLModel": ("sqlmodel", "SQLModel"), "Session": ("sqlalchemy.orm", "Session"),
                   "create_engine": ("sqlalchemy", "create_engine"), "select": ("sqlalchemy", "select"),
                   "Field": ("pydantic", "Field")}[a.name]
            out.append(ast.copy_location(ast.ImportFrom(module=src[0], names=[ast.alias(src[1], local)], level=0), node))
        # names the translator also reads by their last attribute (`Mapped[...]`, `mapped_column(...)`)
        extra = [ast.copy_location(ast.ImportFrom(module="pydantic", names=[ast.alias("BaseModel", "_p2a_sm_BaseModel")], level=0), node),
                 ast.copy_location(ast.ImportFrom(module="typing", names=[ast.alias("Any", "_p2a_sm_Any")], level=0), node),
                 ast.copy_location(ast.Import(names=[ast.alias("sqlalchemy", "_p2a_sm_sa")]), node),
                 ast.copy_location(ast.Import(names=[ast.alias("sqlalchemy.orm", "_p2a_sm_orm")]), node)]
        if keep:
            out.append(ast.copy_location(ast.ImportFrom(module="sqlmodel", names=keep, level=0), node))
        return out + extra

    def local(self, name: str) -> str | None:
        return next((k for k, v in self.names.items() if v == name), None)

    def is_sqlmodel_class(self, node: ast.ClassDef) -> bool:
        sm = self.local("SQLModel")
        return any(isinstance(b, ast.Name) and (b.id == sm or b.id in self.nontable or b.id in self.tables) for b in node.bases)

    # ------------------------------------------------------------------ classes

    def fields_of(self, node: ast.ClassDef) -> list[tuple[str, ast.AnnAssign]]:
        out = []
        for b in node.bases:
            if isinstance(b, ast.Name) and b.id in self.nontable:
                for n, st in self.fields_of(self.nontable[b.id]):
                    out = [(x, s) for x, s in out if x != n] + [(n, st)]
        for st in node.body:
            if isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name):
                ann = st.annotation
                if isinstance(ann, ast.Subscript) and getattr(ann.value, "id", getattr(ann.value, "attr", "")) == "ClassVar":
                    continue
                if any(x == st.target.id for x, _ in out):
                    out = [(x, st if x == st.target.id else s) for x, s in out]
                else:
                    out.append((st.target.id, st))
        return out

    def field_call(self, v: ast.AST | None) -> ast.Call | None:
        f = self.local("Field")
        if isinstance(v, ast.Call) and isinstance(v.func, ast.Name) and v.func.id == f:
            return v
        return None

    def pyd_field(self, st: ast.AnnAssign) -> ast.AnnAssign:
        """the field for Pydantic: SQLModel's Field() without its column options"""
        st = copy.deepcopy(st)
        call = self.field_call(st.value)
        if call is not None:
            call.keywords = [k for k in call.keywords if k.arg not in SA_FIELD_KW]
        return st

    def klass(self, node: ast.ClassDef) -> list[ast.stmt]:
        table = any(k.arg == "table" and isinstance(k.value, ast.Constant) and k.value.value is True for k in node.keywords)
        if any(k.arg not in {"table"} for k in node.keywords) or any(
                isinstance(k.value, ast.Constant) and k.arg == "table" and k.value.value not in (True, False) for k in node.keywords):
            return [node]  # class keywords of SQLModel other than table=: left to the translator (refused)
        if any(isinstance(b, ast.Name) and b.id in self.tables for b in node.bases):
            return [node]  # inheriting from a table class: left as written (refused)
        for st in node.body:
            call = self.field_call(getattr(st, "value", None))
            if call is not None and any(k.arg in SA_REFUSED_KW for k in call.keywords):
                return [node]
        if not table:
            self.nontable[node.name] = node
            new = copy.copy(node)
            new.bases = [ast.copy_location(_name("_p2a_sm_BaseModel") if isinstance(b, ast.Name) and b.id == self.local("SQLModel") else b, b)
                         for b in node.bases]
            new.keywords = []
            body = []
            has_cfg = False
            for st in node.body:
                if isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name):
                    body.append(self.pyd_field(st))
                elif isinstance(st, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "model_config" for t in st.targets):
                    has_cfg = True
                    body.append(st)
                else:
                    body.append(st)
            if not has_cfg and any(isinstance(b, ast.Name) and b.id == self.local("SQLModel") for b in node.bases):
                cfg = ast.parse("model_config = {'from_attributes': True}").body[0]
                body.insert(0, ast.copy_location(cfg, node))
            new.body = body or [ast.Pass()]
            return [new]
        return self.table(node)

    def column(self, name: str, st: ast.AnnAssign) -> ast.AnnAssign:
        """SQLModel's get_column_from_field, statically"""
        call = self.field_call(st.value)
        kw = {k.arg: k.value for k in call.keywords} if call is not None else {}
        default = None
        if call is not None and call.args:
            default = call.args[0]
        elif "default" in kw:
            default = kw["default"]
        elif call is None and st.value is not None:
            default = st.value
        base, optional = _base_annotation(st.annotation)
        tname = base.attr if isinstance(base, ast.Attribute) else getattr(base, "id", None)
        sa = SA_TYPES.get(tname)
        if sa is None:
            raise _Unsupported(f"SQLModel field `{name}`: type `{ast.unparse(st.annotation)}` (a column of str, int, float, "
                               "bool, bytes, date, time, timedelta, UUID or Decimal is supported)", st)
        pk = isinstance(kw.get("primary_key"), ast.Constant) and kw["primary_key"].value is True
        if isinstance(default, ast.Constant) and default.value is Ellipsis:
            default = None  # Field(...): required
        # is_field_noneable: an Optional annotation (a default of None alone does not make it nullable)
        nullable = not pk and optional
        if "nullable" in kw:
            nullable = kw["nullable"]
        else:
            nullable = ast.Constant(nullable)
        args: list[ast.AST] = [_attr("_p2a_sm_sa", sa)]
        if sa == "String" and "max_length" in kw:
            # AutoString(length=max_length)
            args = [ast.Call(_attr("_p2a_sm_sa", "String"), [kw["max_length"]], [])]
        if sa == "Numeric":
            # Numeric(precision=max_digits, scale=decimal_places)
            args = [ast.Call(_attr("_p2a_sm_sa", "Numeric"), [], [ast.keyword("precision", kw.get("max_digits", ast.Constant(None))),
                                                                   ast.keyword("scale", kw.get("decimal_places", ast.Constant(None)))])]
        if "foreign_key" in kw:
            fk_kw = [ast.keyword("ondelete", kw["ondelete"])] if "ondelete" in kw else []
            args.append(ast.Call(_attr("_p2a_sm_sa", "ForeignKey"), [kw["foreign_key"]], fk_kw))
        kws = [ast.keyword("primary_key", ast.Constant(pk)), ast.keyword("nullable", nullable),
               ast.keyword("index", kw.get("index", ast.Constant(False))), ast.keyword("unique", kw.get("unique", ast.Constant(False)))]
        if "default_factory" in kw:
            kws.append(ast.keyword("default", kw["default_factory"]))
        elif default is not None and not (isinstance(default, ast.Constant) and default.value is None):
            # Column(default=None) is no default (SQLAlchemy): a primary key left None comes from its sequence
            kws.append(ast.keyword("default", default))
        mapped = ast.Subscript(value=_attr("_p2a_sm_orm", "Mapped"), slice=copy.deepcopy(st.annotation), ctx=ast.Load())
        out = ast.AnnAssign(target=ast.Name(id=name, ctx=ast.Store()), annotation=mapped,
                            value=ast.Call(_attr("_p2a_sm_orm", "mapped_column"), args, kws), simple=1)
        return ast.copy_location(out, st)

    def table(self, node: ast.ClassDef) -> list[ast.stmt]:
        fields = self.fields_of(node)
        try:
            cols = [self.column(n, st) for n, st in fields]
        except _Unsupported:
            return [node]
        others = [st for st in node.body if not (isinstance(st, ast.AnnAssign) or isinstance(st, ast.Pass)
                                                 or (isinstance(st, ast.Expr) and isinstance(st.value, ast.Constant)))]
        tablename = [st for st in others if isinstance(st, ast.Assign) and any(getattr(t, "id", "") == "__tablename__" for t in st.targets)]
        if [st for st in others if st not in tablename]:
            return [node]  # methods, config... on a table class: left to the translator
        sa = ast.ClassDef(name=node.name, bases=[_name(self.local("SQLModel"))], keywords=[],
                          body=(tablename or [ast.Assign(targets=[ast.Name("__tablename__", ast.Store())],
                                                         value=ast.Constant(node.name.lower()))]) + cols,
                          decorator_list=[], type_params=[])
        ast.copy_location(sa, node)
        ast.fix_missing_locations(sa)
        view_bases = [ast.copy_location(_name("_p2a_sm_BaseModel") if isinstance(b, ast.Name) and b.id == self.local("SQLModel") else b, b)
                      for b in node.bases]
        own = [self.pyd_field(st) for st in node.body if isinstance(st, ast.AnnAssign) and isinstance(st.target, ast.Name)]
        view_body = own
        if any(isinstance(b, ast.Name) and b.id == self.local("SQLModel") for b in node.bases):
            view_body = [ast.parse("model_config = {'from_attributes': True}").body[0]] + own
        view = ast.ClassDef(name=f"{node.name}_p2a_sm_view", bases=view_bases, keywords=[], body=view_body or [ast.Pass()],
                            decorator_list=[], type_params=[])
        ast.copy_location(view, node)
        ast.fix_missing_locations(view)
        build = ast.parse(f"def {node.name}_p2a_sm_build(v):\n    return {node.name}("
                          + ", ".join(f"{n}=v.{n}" for n, _ in fields) + ")").body[0]
        ast.copy_location(build, node)
        for x in ast.walk(build):
            ast.copy_location(x, node)
        # FastAPI sees a table model through a TypeAdapter: SQLModel's table models do not validate there (the
        # values as given, the fields left out without their default absent, extra keys dropped)
        raw_fields, set_lines = [], []
        for n, st in fields:
            call = self.field_call(st.value)
            kw = {k.arg: k.value for k in call.keywords} if call is not None else {}
            if call is not None and call.args:
                dflt = ast.unparse(call.args[0])
            elif "default" in kw:
                dflt = ast.unparse(kw["default"])
            elif "default_factory" in kw:
                dflt = f"{self.local('Field')}(default_factory={ast.unparse(kw['default_factory'])})"
            elif call is None and st.value is not None:
                dflt = ast.unparse(st.value)
            else:
                dflt = None
            raw_fields.append(f"    {n}: _p2a_sm_Any = {dflt if dflt is not None else 'None'}")
            set_lines.append(f"    kw[{n!r}] = v.{n}" if dflt is not None else f"    if {n!r} in v.model_fields_set:\n        kw[{n!r}] = v.{n}")
        raw = ast.parse(f"class {node.name}_p2a_sm_raw(_p2a_sm_BaseModel):\n    model_config = {{'from_attributes': True}}\n"
                        + "\n".join(raw_fields)).body[0]
        build_raw = ast.parse(f"def {node.name}_p2a_sm_build_raw(v):\n    kw = {{}}\n" + "\n".join(set_lines)
                              + f"\n    return {node.name}(**kw)").body[0]
        for t in (raw, build_raw):
            for x in ast.walk(t):
                ast.copy_location(x, node)
        self.tables[node.name] = {"fields": [n for n, _ in fields], "node": node}
        return [sa, view, build, raw, build_raw]

    # ------------------------------------------------------------------ functions

    def view_of(self, ann: ast.AST) -> tuple[ast.AST, bool]:
        """an annotation with each table class replaced by its unvalidating view (and whether one was)"""
        ann = copy.deepcopy(ann)
        hit = False
        for n in ast.walk(ann):
            if isinstance(n, ast.Name) and n.id in self.tables:
                n.id = f"{n.id}_p2a_sm_raw"
                hit = True
        return ann, hit

    def function(self, fn: ast.FunctionDef | ast.AsyncFunctionDef) -> None:
        """a route (a decorator call) or a dependency: a table class as a parameter's or the return annotation, or in
        response_model=, is its view; a parameter of that type is turned into the table's instance first"""
        routed = any(isinstance(d, ast.Call) for d in fn.decorator_list)
        for d in fn.decorator_list:
            if isinstance(d, ast.Call):
                for k in d.keywords:
                    if k.arg == "response_model":
                        k.value, _ = self.view_of(k.value)
        if fn.returns is not None and routed:
            fn.returns, _ = self.view_of(fn.returns)
        pre = []
        for a in fn.args.args + fn.args.kwonlyargs:
            if a.annotation is None:
                continue
            base = a.annotation
            if isinstance(base, ast.Subscript) and getattr(base.value, "id", "") == "Annotated" and isinstance(base.slice, ast.Tuple):
                base = base.slice.elts[0]
            if isinstance(base, ast.Name) and base.id in self.tables:
                a.annotation, _ = self.view_of(a.annotation)
                st = ast.parse(f"{a.arg} = {base.id}_p2a_sm_build_raw({a.arg})").body[0]
                pre.append(st)
        for st in pre:
            for x in ast.walk(st):
                ast.copy_location(x, fn.body[0])
        fn.body[0:0] = pre

    def update_fn(self) -> ast.FunctionDef:
        """SQLModel.sqlmodel_update(obj, update=None): a dict sets the keys that are fields of the instance, a model
        each of its fields (a key of `update` wins), then the rest of `update`"""
        branches = "\n".join(f"    {'if' if i == 0 else 'elif'} isinstance(self, {t}):\n        fields = {tuple(v['fields'])!r}"
                             for i, (t, v) in enumerate(self.tables.items()))
        src = f'''def _p2a_sm_sqlmodel_update(self, obj, update=None):
    fields = ()
{branches}
    use_update = dict(update or {{}})
    if isinstance(obj, dict):
        for key, value in {{**obj, **use_update}}.items():
            if key in fields:
                setattr(self, key, value)
    elif isinstance(obj, _p2a_sm_BaseModel):
        for key in type(obj).model_fields:
            if key in use_update:
                value = use_update.pop(key)
            else:
                value = getattr(obj, key)
            setattr(self, key, value)
        for key, value in use_update.items():
            if key in fields:
                setattr(self, key, value)
    else:
        raise ValueError("Can't use sqlmodel_update() with something that is not a dict, SQLModel, or Pydantic model: " + str(obj))
    return self
'''
        fn = ast.parse(src).body[0]
        last = self.tree.body[-1]
        for x in ast.walk(fn):
            ast.copy_location(x, last)
        return fn


class _Unsupported(Exception):
    pass


class _Calls(ast.NodeTransformer):
    """`session.exec(stmt)`, `obj.sqlmodel_update(...)`, `Table.model_validate(x)`"""

    def __init__(self, rw: Rewriter):
        self.rw = rw
        self.assigned: dict[str, ast.AST] = {}

    def visit_FunctionDef(self, node):
        prev, self.assigned = self.assigned, {}
        for n in ast.walk(node):
            if isinstance(n, ast.Assign) and len(n.targets) == 1 and isinstance(n.targets[0], ast.Name):
                self.assigned.setdefault(n.targets[0].id, []).append(n.value)
        self.generic_visit(node)
        self.assigned = prev
        return node

    visit_AsyncFunctionDef = visit_FunctionDef

    def select_width(self, e: ast.AST) -> int | None:
        """the number of entities of a `select(...)` chain (a name assigned once to one, too), else None"""
        if isinstance(e, ast.Name) and len(self.assigned.get(e.id, [])) == 1:
            e = self.assigned[e.id][0]
        while isinstance(e, ast.Call) and isinstance(e.func, ast.Attribute):
            e = e.func.value
        if (isinstance(e, ast.Call) and isinstance(e.func, ast.Name) and e.func.id == self.rw.local("select")
                and not e.keywords and not any(isinstance(a, ast.Starred) for a in e.args)):
            return len(e.args)
        return None

    def visit_Call(self, node: ast.Call):
        self.generic_visit(node)
        f = node.func
        if isinstance(f, ast.Attribute) and f.attr == "exec" and len(node.args) == 1 and not node.keywords:
            w = self.select_width(node.args[0])
            if w is not None:
                f.attr = "scalars" if w == 1 else "execute"
        elif isinstance(f, ast.Attribute) and f.attr == "sqlmodel_update" and self.rw.tables and node.args:
            node = ast.copy_location(ast.Call(func=_name("_p2a_sm_sqlmodel_update"), args=[f.value, *node.args],
                                              keywords=node.keywords), node)
        elif (isinstance(f, ast.Attribute) and f.attr == "model_validate" and isinstance(f.value, ast.Name)
              and f.value.id in self.rw.tables and len(node.args) == 1 and not node.keywords):
            t = f.value.id
            inner = ast.Call(func=ast.Attribute(value=_name(f"{t}_p2a_sm_view"), attr="model_validate", ctx=ast.Load()),
                             args=node.args, keywords=[])
            node = ast.copy_location(ast.Call(func=_name(f"{t}_p2a_sm_build"), args=[inner], keywords=[]), node)
        return node


def rewrite(tree: ast.Module) -> ast.Module:
    if not imports_sqlmodel(tree):
        return tree
    return Rewriter(tree).run()



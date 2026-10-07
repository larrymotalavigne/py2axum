"""Intermediate representation shared by the front-end and the Rust generator."""
from __future__ import annotations

import ast
from dataclasses import dataclass, field
from typing import Any


class TranspileError(Exception):
    """A construct outside the supported subset. Always points at a source line."""

    def __init__(self, msg: str, node: ast.AST | None = None, file: str | None = None):
        self.msg = msg
        self.node = node
        self.file = file
        super().__init__(self.render())

    def render(self) -> str:
        where = ""
        if self.file:
            where = self.file
            if self.node is not None and hasattr(self.node, "lineno"):
                where += f":{self.node.lineno}"
            where += ": "
        return f"{where}{self.msg}"


class BlockedBy(TranspileError):
    """A route that uses a model/schema/middleware which itself failed to translate."""

    def __init__(self, entity: str, cause: TranspileError, node: ast.AST | None = None, file: str | None = None):
        self.entity = entity
        self.cause = cause
        super().__init__(f"uses {entity}, which is not translatable: {cause.render()}", node, file)


MISSING = object()  # no default value


@dataclass(frozen=True)
class TypeRef:
    """A Python annotation, normalised.

    kind: int | str | bool | float | schema | list | optional | json
    """

    kind: str
    name: str | None = None  # schema name
    inner: "TypeRef | None" = None

    @property
    def is_optional(self) -> bool:
        return self.kind == "optional"

    def base(self) -> "TypeRef":
        return self.inner if self.kind == "optional" else self


@dataclass
class Column:
    name: str
    py_type: str  # int | str | bool | float
    nullable: bool
    primary_key: bool = False
    unique: bool = False
    length: int | None = None
    big: bool = False  # BigInteger
    default: Any = MISSING  # Python-side default literal

    @property
    def rust_type(self) -> str:
        t = {
            "int": "i64" if self.big else "i32",
            "str": "String",
            "bool": "bool",
            "float": "f64",
        }[self.py_type]
        return f"Option<{t}>" if self.nullable else t

    @property
    def sql_type(self) -> str:
        if self.py_type == "int":
            if self.primary_key:
                return "BIGSERIAL" if self.big else "SERIAL"
            return "BIGINT" if self.big else "INTEGER"
        if self.py_type == "str":
            return f"VARCHAR({self.length})" if self.length else "VARCHAR"
        return {"bool": "BOOLEAN", "float": "FLOAT"}[self.py_type]


@dataclass
class OrmModel:
    name: str
    table: str
    columns: list[Column]

    @property
    def pk(self) -> Column:
        pks = [c for c in self.columns if c.primary_key]
        if len(pks) != 1:
            raise TranspileError(f"model {self.name}: exactly one primary key column is supported")
        return pks[0]

    def col(self, name: str) -> Column | None:
        return next((c for c in self.columns if c.name == name), None)

    @property
    def select_cols(self) -> str:
        return ", ".join(c.name for c in self.columns)


@dataclass
class SchemaField:
    name: str
    typ: TypeRef
    default: Any = MISSING
    constraints: dict[str, Any] = field(default_factory=dict)


@dataclass
class Schema:
    name: str
    fields: list[SchemaField]

    def field(self, name: str) -> SchemaField | None:
        return next((f for f in self.fields if f.name == name), None)


@dataclass
class Const:
    name: str
    env: str | None  # environment variable read with os.environ.get
    default: str


@dataclass
class Param:
    name: str
    source: str  # path | query | body | dep
    typ: TypeRef | None = None
    default: Any = MISSING
    constraints: dict[str, Any] = field(default_factory=dict)
    dep: str | None = None  # session | http


@dataclass
class Route:
    method: str
    path: str
    func: str
    params: list[Param]
    response_model: TypeRef | None
    status_code: int
    body: list[ast.stmt]
    file: str
    node: ast.AST
    rust_name: str = ""  # handler fn name, unique across the project (defaults to func)
    module: str = ""  # qualified module of the handler
    names: dict[str, str] = field(default_factory=dict)  # identifier in the body -> model/schema/const key

    def __post_init__(self):
        if not self.rust_name:
            self.rust_name = self.func


@dataclass
class Gzip:
    """`app.add_middleware(GZipMiddleware, minimum_size=..., compresslevel=...)`"""

    minimum_size: int = 500  # Starlette defaults
    compresslevel: int = 9


@dataclass
class App:
    models: dict[str, OrmModel]
    schemas: dict[str, Schema]
    consts: dict[str, Const]
    routes: list[Route]
    gzip: Gzip | None = None

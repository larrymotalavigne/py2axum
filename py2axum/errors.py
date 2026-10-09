"""Error codes: one stable code per class of refusal, with why it is refused and what to do.

Every `TranspileError` gets a code (`P2A0501`...). A message is classified by the first pattern that matches it,
so the 800 or so refusals of the compiler need no code at their call site; a call site may still pass
`TranspileError(..., code="P2A0501")`. Codes are stable: a code is never reused for another class, and a class
that disappears keeps its number retired. The reference page of the site (docs/reference/errors.md) is
generated from this table (`python -m py2axum.errors --markdown`, checked by tests/test_errors.py).

Numbering: 01xx Python language, 02xx libraries, 03xx FastAPI/Starlette, 04xx Pydantic, 05xx SQLAlchemy,
06xx project and versions, 09xx the rest.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field

DOCS = "https://larrymotalavigne.github.io/py2axum"
PY_SIDE = ("If the construct is essential, leave the route to Python: `--python-side auto` (or "
           "`--python-side '<path>'`) keeps it in a Python process next to the binary.")


@dataclass(frozen=True)
class ErrorClass:
    code: str
    title: str
    patterns: tuple[str, ...]
    why: str
    fix: str
    see: str  # page of the site, relative to DOCS
    label_fr: str = ""  # construction label of the internal coverage report (may use the pattern's groups)
    label_en: str = ""  # construction label of `py2axum check` (idem)
    whole_app: bool = False  # concerns the whole application: --python-side auto cannot move it
    compiled: list = field(default_factory=list, compare=False, repr=False)

    def anchor(self) -> str:
        return f"{DOCS}/reference/errors/#{self.code.lower()}"

    def url(self) -> str:
        """The page of the site that describes what is supported here (MkDocs directory URLs)."""
        page, _, frag = self.see.partition("#")
        return f"{DOCS}/{page.removesuffix('.md')}/" + (f"#{frag}" if frag else "")


def E(code, title, patterns, why, fix, see, fr="", en="", whole_app=False) -> ErrorClass:
    if isinstance(patterns, str):
        patterns = (patterns,)
    return ErrorClass(code, title, tuple(patterns), why, fix, see, fr or title, en or title, whole_app)


# Order matters: the first class with a matching pattern wins (specific before general).
CLASSES: list[ErrorClass] = [
    # ------------------------------------------------------------------ 09xx first: never shadowed
    E("P2A0901", "Internal transpiler error", (r"erreur interne", r"internal (transpiler )?error"),
      "py2axum failed on a construct it should have either translated or refused.",
      "This is a py2axum bug: please report it with the file:line and a minimal reproduction. Meanwhile, "
      "`--python-side '<path>'` leaves the route to Python.",
      "getting-started/check.md#when-something-is-refused", "erreur interne du transpileur",
      "internal transpiler error"),
    E("P2A0115", "Syntax error", r"syntax error",
      "The file does not parse with the Python running py2axum.",
      "Fix the syntax, or run py2axum on a Python at least as recent as the project's (Python 3.14 syntax such "
      "as `except A, B:` needs py2axum on 3.14).",
      "reference/versions.md", "erreur de syntaxe", "syntax error", whole_app=True),

    # ------------------------------------------------------------------ 02xx libraries
    E("P2A0201", "Library call not supported",
      (r"library call `([a-zA-Z0-9_]+)\.?[^`]*\(\)` is not supported", r"library function `([\w.]+)` cannot be used",
       r"py2axum maps sentry_sdk", r"string\.Formatter\(\)\.\w+\(\) is not supported",
       r"^Formatter\(\)\."),
      "The call goes to a library the binary has no equivalent for: py2axum maps a closed list of library "
      "functions to its runtime and never calls Python libraries from the binary.",
      "Use a supported library or the standard library for the same job (see Libraries and Standard library). "
      + PY_SIDE,
      "reference/libraries.md", "lib \\1", "library \\1"),
    E("P2A0202", "Library value not supported", r"library value `([a-zA-Z0-9_]+)[^`]*` is not supported",
      "A value of a library outside py2axum's library map is read (a constant, a class used as a value...).",
      "Read it from a supported library or write the value itself. " + PY_SIDE,
      "reference/libraries.md", "lib \\1", "library \\1"),
    E("P2A0203", "Method not implemented by the runtime", r"method \.(\w+)\(\) is not implemented by the runtime",
      "No value type of the runtime has this method, and no project class defines it: the binary would fail "
      "with a 500 where Python runs it.",
      "Use a supported method of the value (see Python semantics), or check the receiver's type: it may be a "
      "value py2axum does not model. " + PY_SIDE,
      "reference/python.md", "méthode .\\1() absente du runtime (500 exécution)", "method .\\1() not in the runtime"),
    E("P2A0204", "Attribute not provided by the runtime", r"attribute \.(\w+) is not provided by the runtime",
      "No value type of the runtime has this attribute, and no project class defines it.",
      "Read a supported attribute, or keep the value in a project class or a Pydantic model. " + PY_SIDE,
      "reference/python.md", "attribut .\\1 absent du runtime (500 exécution)", "attribute .\\1 not in the runtime"),
    E("P2A0205", "Keyword option not supported", (r"\.(\w+)\((\w+)=\) is not supported \(only",),
      "The function is supported, but not with this keyword argument: its behaviour with it is not reproduced.",
      "Drop the option or use one of the supported options listed in the message. " + PY_SIDE,
      "reference/stdlib.md", ".\\1(\\2=) (500 exécution)", ".\\1(\\2=)"),
    E("P2A0206", "Library exception class", r"unsupported exception class",
      "`except` or `raise` names an exception class of a library the runtime does not model.",
      "Catch a supported exception (the library's documented base class, or a project exception). " + PY_SIDE,
      "reference/python.md", "classe d'exception externe", "library exception class"),
    E("P2A0207", "MCP server construct not supported",
      (r"^MCP tool", r"^MCPServer\.", r"@server\.tool", r"^server\.tool", r"^server\.session_manager",
       r"streamable_http_app"),
      "py2axum compiles the stateless, JSON-response subset of the MCP SDK's server (tools only).",
      "Write tools as `async def` with `@server.tool()` and serve `streamable_http_app(stateless_http=True, "
      "json_response=True)`. " + PY_SIDE,
      "advanced/mcp.md", "MCP", "MCP"),

    # ------------------------------------------------------------------ 06xx project, versions
    E("P2A0601", "Library version outside the tested ranges",
      (r"is outside the range .* is tested on", r"excludes every version .* is tested on", r"^requires-python",
       r": .* need \w+>="),
      "The project locks a version of FastAPI, Starlette, Pydantic, SQLAlchemy or Python that py2axum is not "
      "tested against: the binary must behave like the version you run, and behaviour changes between versions.",
      "Use a version in the tested ranges, or pass `--allow-untested-versions` after checking the behaviour with "
      "a conformance run.",
      "reference/versions.md", "version hors fourchette", "library version", whole_app=True),
    E("P2A0602", "Cannot be left to Python",
      (r"cannot be declared --python-side", r"root_path=\.\.\.\) together with"),
      "The binary is the only entry point of a hybrid deployment, and this construct cannot be relayed to the "
      "Python process.",
      "Make the construct translate, or route it to Python at your ingress instead of through the binary.",
      "getting-started/hybrid.md", "python-side impossible", "cannot be python-side"),

    # ------------------------------------------------------------------ 03xx FastAPI / Starlette
    E("P2A0303", "Lifespan or startup event",
      (r"FastAPI\(lifespan=", r"lifespan state", r"on_event"),
      "Startup and shutdown code is compiled when it uses supported constructs; this form is not.",
      "Use `FastAPI(lifespan=...)` with an `@asynccontextmanager` and module globals, or leave it to Python with "
      "`--python-side lifespan`.",
      "tutorial/lifespan.md", "lifespan (à déclarer --python-side)", "lifespan"),
    E("P2A0310", "app.mount()", r"app\.mount\(",
      "A mounted sub-application is not compiled; as the app's last registrations, mounts can stay in Python.",
      "Register mounts last and pass `--python-side mount` (implied by `--python-side auto`).",
      "tutorial/bigger-applications.md#mounted-applications", "app.mount", "app.mount"),
    E("P2A0313", "Exception handler", (r"exception_handler", r"add_exception_handler"),
      "This form of exception handler registration is not reproduced.",
      "Register handlers with `@app.exception_handler(Exc)` or `app.add_exception_handler(Exc, handler)` at "
      "module level or at the top level of the app factory.",
      "tutorial/errors.md", "gestionnaire d'exception", "exception handler", whole_app=True),
    E("P2A0301", "Application method or decorator",
      (r"@app\.(\w+)\(\.\.\.\) is not supported", r"app\.(\w+)\(\.\.\.\) is not supported",
       r"app\.dependency_overrides"),
      "This method of the FastAPI application has no equivalent in the binary.",
      "See the supported subset for the application methods the binary reproduces; register the same thing "
      "with a supported form, or leave the part that needs it to Python.",
      "supported.md", "@app.\\1", "@app.\\1", whole_app=True),
    E("P2A0302", "Middleware not supported",
      (r"middleware (\S+) is not supported", r"add_middleware", r"Middleware\(", r"Middleware\b.*options",
       r"^CORSMiddleware", r"TrustedHostMiddleware", r"HTTPSRedirectMiddleware", r"^GZipMiddleware",
       r"minimum_size must be", r"^RawContextMiddleware", r"^plugin", r"BaseHTTPMiddleware",
       r"wrapped in an ASGI class", r"^option \w+= is not supported \(only plugins=\)", r"pass plugins=",
       r"plugins= must be"),
      "Middlewares are compiled one by one: GZip, CORS, TrustedHost, HTTPSRedirect, project "
      "`BaseHTTPMiddleware` subclasses and raw ASGI middlewares of the project, with literal options.",
      "Use one of the supported middlewares with literal keyword options. A middleware of a library must stay "
      "in Python: the whole application then runs in Python (a middleware wraps every route).",
      "tutorial/middleware.md", "middleware \\1", "middleware \\1", whole_app=True),
    E("P2A0304", "Router registration",
      (r"include_router", r"APIRouter", r"add_route\(\)"),
      "Routes are discovered statically: py2axum must know at translation time which routes exist and under "
      "which prefix.",
      "Call `include_router` unconditionally, at module level or at the top level of the app factory, with a "
      "literal prefix or one read from settings.",
      "tutorial/bigger-applications.md", "include_router non résolu", "router registration", whole_app=True),
    E("P2A0312", "Registration outside the app factory",
      (r"must be at the top level of the app factory", r"on the application passed to a function",
       r"inside `[^`]+` is not supported \(only at the top level", r"outside the app factory"),
      "Routes, middlewares and handlers must be registered where py2axum can read them without running code.",
      "Move the registration to module level or to the top level of the app factory function.",
      "tutorial/bigger-applications.md", "enregistrement hors fabrique", "registration outside the factory",
      whole_app=True),
    E("P2A0311", "FastAPI() option", r"FastAPI\(",
      "This option of the FastAPI constructor is not reproduced, or not with a computed value.",
      "Pass the option as a literal, or drop it if it only affects OpenAPI documentation.",
      "supported.md", "option de FastAPI()", "FastAPI() option", whole_app=True),
    E("P2A0309", "WebSocket route", (r"WebSocket", r"websocket"),
      "This WebSocket construct is not reproduced (the binary implements Starlette's WebSocket protocol).",
      "Use `@app.websocket(path)` with an `async def` endpoint and the parameters FastAPI provides on a WebSocket "
      "route. WebSocket routes are never relayed to Python.",
      "tutorial/websockets.md", "WebSocket", "WebSocket"),
    E("P2A0308", "Security scheme", (r"security scheme", r"HTTPBasic\(", r"\(auto_error=\)", r"Depends\(security_scheme"),
      "Only OAuth2PasswordBearer, HTTPBearer and HTTPBasic are compiled, with literal options.",
      "Use one of these schemes (they cover Bearer tokens and Basic auth), with literal keyword options.",
      "tutorial/security.md", "schéma de sécurité", "security scheme"),
    E("P2A0305", "Route or response option",
      (r"unsupported route option (\w+)=", r"response_class=", r"EventSourceResponse", r"generator endpoint",
       r"is only supported with a Pydantic model as the response model", r"must be a literal set or list of field",
       r"default_response_class"),
      "This option of the route decorator changes the response in a way the binary does not reproduce.",
      "Use the supported response classes and options (see Responses), or return a response object.",
      "tutorial/responses.md", "option de route \\1=", "route option \\1="),
    E("P2A0306", "Endpoint parameter",
      (r"(Cookie|Form|File)\(\) parameters", r"needs a type annotation", r"without annotation",
       r"File\(\) default", r"OAuth2PasswordRequestForm", r"model of \w+ parameters", r"model of cookies",
       r"path convertor", r"constraints without a type annotation", r"are not supported in endpoints",
       r"only plain positional parameters"),
      "FastAPI decides from each parameter's annotation and default where its value comes from; this one "
      "cannot be decided statically or is not reproduced.",
      "Annotate the parameter with a supported type and `Query()`, `Path()`, `Header()`, `Cookie()`, `Body()`, "
      "`Form()` or `File()`.",
      "tutorial/parameters.md", "paramètre \\1()", "\\1() parameter"),
    E("P2A0314", "Nested endpoint", r"must be a module-level function",
      "Endpoints are compiled as functions of the generated crate: they must be defined at module level.",
      "Define the endpoint at module level (or at the top level of the app factory).",
      "tutorial/bigger-applications.md", "endpoint imbriqué", "nested endpoint"),
    E("P2A0307", "Dependency",
      (r"dependency `.*` must be a project function", r"generator dependencies", r"Depends\(", r"class dependency",
       r"instance dependency", r"^dependency ", r"dependencies=\[", r"\.__call__: "),
      "Dependencies are compiled when they are project functions (sync, async, generators with one `yield`), "
      "Pydantic models, plain classes or instances with `__call__`.",
      "Wrap the library object in a project function: `def get_x(): return lib.X()` and `Depends(get_x)`.",
      "tutorial/dependencies.md", "dépendance", "dependency"),

    # ------------------------------------------------------------------ 05xx SQLAlchemy
    E("P2A0508", "TypeDecorator", (r"TypeDecorator", r"must be \(self, value, dialect\)", r"^\w+\.process_\w+: using"),
      "A `TypeDecorator` is compiled when it wraps a string type with `process_bind_param`/`process_result_value`.",
      "Keep `impl` a string type and only these two methods (plus `cache_ok`).",
      "tutorial/sql.md#what-is-native", "TypeDecorator", "TypeDecorator"),
    E("P2A0507", "create_all and DDL",
      (r"^create_all", r"not reproduced", r"\bDDL\b", r"is in a module only imported inside a function",
       r"Sequence \(CREATE SEQUENCE\)", r"SQLAlchemy raised", r"does not exist in the installed SQLAlchemy",
       ),
      "`metadata.create_all` is compiled by running SQLAlchemy at translation time on the declared classes; what "
      "SQLAlchemy computes at class creation or in event listeners is not reproduced.",
      "Declare the tables and columns statically, or create the schema with your migrations (Alembic) and drop "
      "`create_all` from the startup code, or keep the lifespan in Python (`--python-side lifespan`).",
      "tutorial/sql.md", "create_all / DDL", "create_all / DDL"),
    E("P2A0509", "Table introspection",
      (r"__table__", r"isinstance\(\) with a SQLAlchemy type"),
      "`Model.__table__` and `isinstance(col.type, <SQLAlchemy type>)` are computed by SQLAlchemy at translation "
      "time, on the mapped classes rebuilt from the source.",
      "Install SQLAlchemy 2.x next to py2axum, and read `__table__` from a mapped class of a declarative base.",
      "tutorial/sql.md", "introspection de table", "table introspection"),
    E("P2A0504", "Relationship", r"^relationship",
      "This `relationship()` configuration changes which SQL is emitted in a way the runtime does not reproduce.",
      "Use the supported options (`back_populates`, `foreign_keys`, `lazy='select'|'selectin'|'joined'|'raise'`, "
      "`cascade`, `order_by`, self-referential `remote_side`).",
      "tutorial/sql.md#what-is-native", "relation", "relationship"),
    E("P2A0501", "Column type",
      (r"unsupported column type `(?:[\w.]*\.)?(\w+)", r"^column \w+: (ARRAY|Enum|Numeric|Identity)\(",
       r"as_uuid=False", r"type factory", r"type_= is not supported", r"changes the column type",
       r"only ARRAY\(String\)", r"Enum\(\) (needs|of string)"),
      "The column type decides how values are bound and decoded; this one has no mapping in the runtime.",
      "Use a supported column type (String, Integer, Numeric, Boolean, Date/DateTime, JSON/JSONB, Uuid, Enum of a "
      "Python enum class, LargeBinary, ARRAY(String)).",
      "tutorial/sql.md#what-is-native", "colonne \\1", "column type \\1"),
    E("P2A0502", "Column declaration",
      (r"^column \w+", r"column .*: (callable/SQL defaults|only onupdate)", r"column .*: cannot infer",
       r"a column inside"),
      "This column declaration or option is not reproduced.",
      "Declare the column as `name: Mapped[T] = mapped_column(...)` with literal options.",
      "tutorial/sql.md#what-is-native", "déclaration de colonne", "column declaration"),
    E("P2A0503", "Mapped class declaration",
      (r"inheritance between mapped classes", r"abstract models", r"exactly one primary key", r"^model \w+",
       r"__tablename__", r"__mapper_args__", r"__table_args__", r"mapped classes take keyword", r"declared_attr"),
      "This declaration of a mapped class changes the SQL SQLAlchemy emits in a way the runtime does not reproduce.",
      "Declare plain mapped classes: one table each, a single primary key, columns and relationships, "
      "`@property`/`@staticmethod`/`@classmethod` methods.",
      "tutorial/sql.md#what-is-native", "déclaration de modèle", "mapped class declaration"),
    E("P2A0505", "Session or engine",
      (r"session", r"sessionmaker", r"autocommit=True", r"connect_args", r"run_sync"),
      "The session dependency fixes when the binary commits and rolls back; this form cannot be read statically.",
      "Use a module-level `async_sessionmaker(engine, ...)` (or `sessionmaker`) and a dependency that yields a "
      "session from it.",
      "tutorial/sql.md", "session / moteur", "session or engine"),
    E("P2A0506", "SQL statement", (r"JSON path index", r"executemany", r"\(statement, parameters\)",
                                    r"bulk \w+ / executemany"),
      "This way of building or executing a statement emits SQL the runtime does not reproduce.",
      "Build the statement with the supported constructs (see SQL databases) and its own bound values.",
      "tutorial/sql.md#what-is-native", "requête SQL", "SQL statement"),

    # ------------------------------------------------------------------ 04xx Pydantic
    E("P2A0408", "JSON schema", (r"^JSON schema:", r"model_json_schema", r"refers to itself", r"two models named"),
      "The JSON schema of the model is computed statically, as Pydantic would; this model cannot be.",
      "Avoid self-referencing models and duplicate model names in one schema.",
      "tutorial/models.md", "schéma JSON", "JSON schema"),
    E("P2A0405", "Pydantic decorator", r"@(model_validator|computed_field|field_serializer|model_serializer)",
      "This Pydantic decorator is not reproduced in this form.",
      "Use the supported forms (see Models).",
      "tutorial/models.md#what-is-native", "Pydantic @\\1", "Pydantic @\\1"),
    E("P2A0403", "Pydantic validator",
      (r"only mode='after' field validators", r"validator", r"ValidationInfo"),
      "Validators run inside the runtime's port of pydantic-core; this signature or mode is not reproduced.",
      "Write `@field_validator('f')` (mode after or before) as a classmethod taking `(cls, v)` or "
      "`(cls, v, info)`.",
      "tutorial/models.md#what-is-native", "validateur Pydantic", "Pydantic validator"),
    E("P2A0404", "Model configuration",
      (r"class `Config`", r"class Config", r"model_config", r"pydantic-settings init options"),
      "This configuration option changes validation or serialisation in a way the runtime does not reproduce.",
      "Use `model_config = ConfigDict(...)` with supported literal options.",
      "tutorial/models.md#what-is-native", "model_config", "model_config"),
    E("P2A0407", "Annotated metadata", r"Annotated",
      "Pydantic validates or serialises the field differently with this metadata; py2axum never ignores "
      "metadata it does not know.",
      "Use `Field(...)` constraints or a supported validator instead.",
      "tutorial/models.md#what-is-native", "métadonnée Annotated", "Annotated metadata"),
    E("P2A0402", "Field option",
      (r"Field\(", r"constraint \w+= on", r"^constraints on", r"Decimal bound", r"applies to Decimal",
       r"PrivateAttr", r"private attribute", r"default_factory"),
      "This `Field()` option or constraint is not reproduced on this type.",
      "Use the supported options (see Models) or a validator.",
      "tutorial/models.md#what-is-native", "option de Field", "Field option"),
    E("P2A0410", "Enum or non-Pydantic type",
      (r"is not a Pydantic model \(enums", r"is not a Pydantic model or an enum"),
      "Models, parameters and bodies are validated like Pydantic; only Pydantic models, enums, dataclasses and "
      "the supported scalar types can be validated.",
      "Turn the class into a `BaseModel` (or a dataclass), or use a supported type.",
      "tutorial/models.md", "type Enum ou classe non Pydantic", "Enum or non-Pydantic type"),
    E("P2A0401", "Type annotation",
      (r"unsupported type annotation `(?:[\w.]*\.)?(UUID|Decimal|EmailStr|HttpUrl|SecretStr|AnyUrl)",
       r"unsupported type annotation", r"^type `[^`]+` is not supported", r"bad string annotation", r"Literal",
       r"pydantic validates it differently", r"only dict\[str", r"only variable-length tuple", r"TypeAdapter",
       r"constraints on a union", r"^unsupported type "),
      "The annotation decides validation and serialisation; this type has no equivalent in the runtime's "
      "port of pydantic-core.",
      "Use a supported type (see Models): builtins, `list`/`dict`/`tuple`, unions, `Literal`, enums, models, "
      "datetime types, UUID, Decimal, EmailStr...",
      "tutorial/models.md#what-is-native", "type \\1", "type \\1"),
    E("P2A0406", "Model class body",
      (r"class attribute `.*` is not supported", r"^schema \w+", r"BaseModel\.__init__", r"Pydantic models take keyword",
       r"of BaseModel are supported", r"unsupported statement in schema"),
      "Model classes are compiled from their fields, validators and simple methods; this member is not reproduced.",
      "Keep the model to fields, `model_config`, validators, `@property` and plain methods.",
      "tutorial/models.md#what-is-native", "attribut de classe de schéma", "model class body"),

    # ------------------------------------------------------------------ 01xx Python language
    E("P2A0102", "global/nonlocal",
      (r"`global`/`nonlocal`", r"`nonlocal`", r"^`nonlocal \w+`", r"^`global ", r"rebinds with `global`"),
      "Module state rebound from functions is reproduced for variables assigned at module level only; `nonlocal` "
      "rebinds a local variable of the enclosing function, not one of its parameters.",
      "Assign the variable at module level and rebind it with `global` in the module that defines it; for "
      "`nonlocal`, copy the parameter into a local variable first.",
      "reference/python.md", "global/nonlocal", "global/nonlocal"),
    E("P2A0103", "*/** expansion",
      (r"starred assignment|`\*`/`\*\*` expansion", r"\*args", r"\*\*kwargs", r"\*\* expansion", r"\*\*mapping",
       r"multiple starred"),
      "Calls and signatures are resolved statically; this expansion cannot be.",
      "Pass the arguments explicitly.",
      "reference/python.md", "expansion */** non supportée", "*/** expansion"),
    E("P2A0109", "async/await",
      (r"coroutine `.*` is not awaited", r"unsupported await", r"without `await`", r"called without await",
       r"to_thread", r"coroutine_function"),
      "A coroutine that is not awaited directly (stored, passed around) is not reproduced.",
      "`await` the call where it is made, or use `asyncio.create_task`/`gather` on direct calls.",
      "reference/python.md#asyncio-and-threading", "coroutine non attendue (create_task/gather)",
      "coroutine not awaited"),
    E("P2A0116", "Decorator",
      (r"decorator @(\S+) on", r"^@\w+ combined with", r"@lru_cache", r"^@\w+\(\w+=\) is not supported",
       r"decorators are not", r"decorated \w+ called with a receiver", r"decorator @(\S+) is not supported"),
      "Decorators are applied at translation time; only the decorators py2axum knows are reproduced.",
      "Remove the decorator or use a supported one (see Python semantics). " + PY_SIDE,
      "reference/python.md", "décorateur @\\1", "decorator @\\1"),
    E("P2A0112", "Dataclass", (r"^dataclass", r"@dataclass", r"unsupported statement in dataclass"),
      "Dataclasses are compiled with plain fields, `field(default=, default_factory=)` and methods.",
      "Keep the dataclass to fields and plain methods, without options or base classes.",
      "reference/python.md", "dataclass", "dataclass"),
    E("P2A0113", "Enum class", (r"^enum \w+", r"Flag/IntFlag"),
      "Enums are compiled with literal member values and plain methods.",
      "Use literal member values, no Flag/IntFlag, no subclassing.",
      "reference/python.md", "classe Enum", "enum class"),
    E("P2A0114", "match statement", (r"`case ", r"capture names inside `\|`"),
      "Only some pattern kinds of `match` are compiled.",
      "Use the supported patterns (values, None/True/False, `_`, names, `Class()`, `|`, `as`) or `if`/`elif`.",
      "reference/python.md", "match", "match statement"),
    E("P2A0111", "Project class",
      (r"only exception classes are supported", r"^exception class", r"super\(", r"__slots__", r"is not a class",
       r"^no method", r"object\.__(init|setattr)__", r"BaseException\.__init__", r"^base class `",
       r"class variables", r"method \w+ is not supported",
       r"unsupported statement in class"),
      "Project classes are compiled when they are models, schemas, exceptions, dataclasses, enums or plain "
      "classes with plain methods; this member or form is not reproduced.",
      "Keep the class to `__init__`, plain methods and properties, or turn it into a dataclass or a model.",
      "reference/python.md#values-functions-classes-modules", "classe projet", "project class"),
    E("P2A0110", "Module-level statement",
      (r"module-level statement not translated", r"app factory statement not translated",
       r"bound by several module-level", r"^assigning ", r"cannot assign to",
       r"^writing into"),
      "Module code runs once at startup in the binary; only statements py2axum can reproduce there are "
      "translated.",
      "Move the computation into a function, or assign the variable once at module level.",
      "reference/python.md#values-functions-classes-modules", "instruction de module", "module-level statement",
      whole_app=True),
    E("P2A0105", "Unknown name or import",
      (r"unknown name", r"is not defined", r"cannot be imported", r"module .* has no attribute", r"import \*",
       r"importlib"),
      "py2axum resolves every name statically, over the project and the libraries it knows; this one does not "
      "resolve.",
      "Import the name from a module of the project or of a supported library, with a literal import.",
      "reference/python.md#values-functions-classes-modules", "nom inconnu", "unknown name"),
    E("P2A0108", "Call signature",
      (r"takes (no|exactly|at most|\d+|one|two|three) .*argument", r"missing (required )?(keyword )?argument",
       r"got an unexpected keyword", r"got multiple values", r"has no parameter", r"non-default argument",
       r"positional arguments", r"keyword arguments only", r"takes no arguments", r"needs (the|a|one) ",
       r"^too many arguments", r"^unsupported arguments"),
      "The call does not match the function's signature, or uses a form py2axum cannot bind statically: Python "
      "would raise a TypeError or behave in a way that is not reproduced.",
      "Call the function with the arguments the message lists.",
      "reference/python.md", "signature d'appel", "call signature"),
    E("P2A0107", "Loop with else", r"with/else|for/else|while/else",
      "`else` clauses of loops are not compiled.",
      "Use a flag variable or a helper function that returns.",
      "reference/python.md", "boucle avec else", "loop with else"),
    E("P2A0104", "Builtin not supported",
      (r"builtin `(\w+)\(\)` is not supported", r"defaultdict", r"^iter\(\)", r"^next\(\)"),
      "This builtin, or this use of it, has no equivalent in the runtime.",
      "Use a supported builtin (see Python semantics).",
      "reference/python.md", "builtin \\1()", "builtin \\1()"),
    E("P2A0106", "Value not known at translation time",
      (r"is not a constant", r"expected a literal value", r"f-strings are not constants",
       r"evaluated at translation time", r"must be a literal", r"must be a constant", r"must be literals",
       r"only a plain function whose body is `return", r"only type subscripts are evaluated",
       r"is not a JSON value", r"must be True or False", r"cannot be used as a value", r"is not callable",
       r"must be a module-level", r"of a computed name", r"attribute of a value not evaluated", r"`[^`]+`: only "),
      "py2axum reads configuration (options, types, routes) without running the project: this value must be "
      "written literally or computed by something py2axum evaluates.",
      "Write the value itself (a literal, or a module constant assigned once).",
      "advanced/how-it-works.md", "valeur non constante", "non-constant value"),
    E("P2A0101", "Python construct not supported",
      (r"unsupported statement `(\w+)`", r"unsupported expression", r"^unsupported (augmented assignment|binary "
       r"operator|comparison|comprehension target|del target)", r"conversion is not supported",
       r"integer literal out of 64-bit", r"bare `raise`", r"break/continue outside", r"`yield` outside",
       r"stored in a variable",
       r"^unsupported (default value|constant|name|call|assignment target)"),
      "py2axum compiles a subset of Python; this construct has no equivalent in the runtime yet.",
      "Rewrite it with a supported construct (see Python semantics). " + PY_SIDE,
      "reference/python.md", "construction Python", "Python construct"),

    # ------------------------------------------------------------------ catch-alls by keyword (last)
    E("P2A0209", "Option not supported", (r"\w\((\w+)=(\.\.\.)?\) is not supported", r"option \w+= is not supported",
                                          r"unsupported option (\w+)= in", r"options? (are|is) not supported"),
      "The function or class is supported, but not with this option: its effect is not reproduced.",
      "Drop the option, or use one of the supported options listed in the message. " + PY_SIDE,
      "supported.md", "option \\1=", "option \\1="),
    E("P2A0999", "Not supported", r"",
      "The construct is outside the supported subset.",
      "Rewrite it with a supported construct (see the supported subset). " + PY_SIDE,
      "supported.md", "autre", "other"),
]

BY_CODE = {c.code: c for c in CLASSES}
for _c in CLASSES:
    _c.compiled.extend(re.compile(p) for p in _c.patterns)
OTHER = BY_CODE["P2A0999"]


def lookup(msg: str) -> tuple[ErrorClass, re.Match | None]:
    """The class of a message (the first matching pattern) and the match, for labels with groups."""
    for c in CLASSES:
        for p in c.compiled:
            m = p.search(msg)
            if m:
                return c, m
    return OTHER, None


def get(code: str) -> ErrorClass | None:
    return BY_CODE.get(code.strip().upper())


def label(msg: str, english: bool = True, code: str | None = None) -> str:
    """The construction label of a message for the coverage tables (groups expanded)."""
    c, m = lookup(msg)
    if code and code != c.code and code in BY_CODE:
        c, m = BY_CODE[code], None
    if c is OTHER:
        return ("other: " if english else "autre : ") + re.sub(r"`[^`]*`", "`…`", msg)[:80]
    tmpl = c.label_en if english else c.label_fr
    if m is None or "\\" not in tmpl:
        return re.sub(r"\\\d", "…", tmpl)
    try:
        return m.expand(tmpl)
    except (IndexError, re.error):
        return re.sub(r"\\\d", "…", tmpl)


def explain(code: str) -> str | None:
    c = get(code)
    if c is None:
        return None
    import textwrap

    wrap = lambda t: textwrap.fill(t, 100)  # noqa: E731
    o = [f"{c.code}: {c.title}", "", wrap(c.why), "", wrap(f"What to do: {c.fix}")]
    if c.whole_app:
        o += ["", wrap("This refusal concerns the whole application: --python-side auto cannot move it to Python.")]
    o += ["", f"Supported subset: {c.url()}", f"This code:        {c.anchor()}"]
    return "\n".join(o)


def markdown() -> str:
    """docs/reference/errors.md."""
    o = ["# Error codes", "",
         "<!-- generated by `python -m py2axum.errors --markdown`: edit py2axum/errors.py, not this page -->", "",
         "Every refusal of py2axum names what is not supported, at `file:line`, with a stable code. "
         "The code tells why and what to do; `py2axum check --explain CODE` prints the same text in a terminal:",
         "", "```text",
         "error[P2A0201]: app/routers/export.py:26: library call `zipfile.ZipFile()` is not supported "
         "(not in the py2axum library map)",
         "  = why: The call goes to a library the binary has no equivalent for: py2axum maps a closed list of ...",
         "  = help: Use a supported library or the standard library for the same job (see Libraries and ...",
         f"  = see: {DOCS}/reference/errors/#p2a0201",
         "```", "",
         "Codes are stable: a code always names the same class of refusal, and a retired code is never reused. "
         "Classes marked *whole application* refuse the generation whatever the routes: "
         "`--python-side auto` cannot move them to Python.", ""]
    groups = [("01", "Python language"), ("02", "Libraries"), ("03", "FastAPI and Starlette"),
              ("04", "Pydantic"), ("05", "SQLAlchemy"), ("06", "Project and versions"), ("09", "Other")]
    o += ["| Code | Class |", "|---|---|"]
    for prefix, _ in groups:
        for c in sorted((c for c in CLASSES if c.code[3:5] == prefix), key=lambda c: c.code):
            o.append(f"| [{c.code}](#{c.code.lower()}) | {c.title} |")
    for prefix, name in groups:
        o += ["", f"## {name}"]
        for c in sorted((c for c in CLASSES if c.code[3:5] == prefix), key=lambda c: c.code):
            o += ["", f"### {c.code}", "", f"**{c.title}**" + (" — *whole application*" if c.whole_app else ""), "",
                  c.why, "", f"**What to do:** {c.fix}", "", f"See [{_page_title(c.see)}](../{_rel(c.see)})."]
    return "\n".join(o) + "\n"


def _rel(see: str) -> str:
    return see


def _page_title(see: str) -> str:
    page = see.split("#")[0]
    titles = {"reference/libraries.md": "Libraries", "reference/python.md": "Python semantics",
              "reference/stdlib.md": "Standard library", "reference/versions.md": "Versions",
              "supported.md": "Supported subset", "getting-started/check.md": "py2axum check",
              "getting-started/hybrid.md": "Hybrid mode", "advanced/mcp.md": "MCP servers",
              "advanced/how-it-works.md": "How it works", "tutorial/lifespan.md": "Lifespan events",
              "tutorial/bigger-applications.md": "Bigger applications", "tutorial/errors.md": "Handling errors",
              "tutorial/middleware.md": "Middleware", "tutorial/websockets.md": "WebSockets",
              "tutorial/security.md": "Security", "tutorial/responses.md": "Responses",
              "tutorial/parameters.md": "Parameters", "tutorial/dependencies.md": "Dependencies",
              "tutorial/sql.md": "SQL databases", "tutorial/models.md": "Models (Pydantic)"}
    return titles.get(page, page)


if __name__ == "__main__":
    import sys

    if sys.argv[1:] == ["--markdown"]:
        sys.stdout.write(markdown())
    elif len(sys.argv) == 2:
        text = explain(sys.argv[1])
        print(text or f"unknown error code {sys.argv[1]}")
    else:
        print("usage: python -m py2axum.errors --markdown | CODE")

"""The closed list of Python library APIs the dyn backend translates .

Each entry maps a resolved external name (`modules.Ext.dotted`, e.g. `datetime.datetime.now`) to
Rust code over the `dynrt` runtime. Anything not listed is refused at transpile time with
`fichier:ligne`, so a library is never approximated silently.

Templates receive the compiled positional arguments (Rust expressions of type `V`) and keyword
arguments (name -> expression) and return a Rust expression of type `R` (a `Result<V, Exc>`).
"""
from __future__ import annotations

import re

RT = "dynrt"


def _kwvec(kw: dict[str, str]) -> str:
    # only the key is quoted here: the values are Rust code (lifetimes and labels contain ')
    return "vec![" + ", ".join(f'("{k}".to_string(), {v})' for k, v in kw.items()) + "]"


def _engine(a, kw, sync: bool):
    """`create_[async_]engine(url, ...)`: the binary's own pool; only `connect_args` (session
    parameters, `orm::engine_connect_args`) changes what it does. A sync engine's connections are sync
    (`with engine.connect()`, lazy loads)."""
    ev = "".join(f"let _ = {x}; " for k, x in [(None, x) for x in a] + list(kw.items()) if k != "connect_args")
    eng = f"V::native({RT}::Native::Engine({'true' if sync else 'false'}))"
    if "connect_args" in kw:
        return "{ " + ev + f"{RT}::orm::engine_connect_args(&{kw['connect_args']}).map(|_| {eng}) }}"
    return "{ " + ev + f"Ok::<V, Exc>({eng}) }}"


def _jsonable_encoder(a, kw):
    """`jsonable_encoder(obj)`: its options (include, exclude, by_alias=False, custom_encoder...) are refused."""
    if len(a) != 1 or kw:
        raise ValueError("jsonable_encoder() is supported with the object only (no include/exclude/by_alias/"
                         "custom_encoder options)")
    return f"{RT}::pyd::jsonable(&{a[0]})"


def _default_handler(which: str):
    """FastAPI's `(request, exc)` default handlers: the request is evaluated, unused (as FastAPI's own)."""
    def tmpl(a, kw):
        if len(a) + len(kw) != 2 or set(kw) - {"request", "exc"}:
            raise ValueError("expected two arguments (request, exc)")
        req = a[0] if a else kw["request"]
        exc = a[1] if len(a) == 2 else kw["exc"]
        return f"{{ let _ = {req}; {RT}::web::default_exc_handler(&{exc}, {which!r}) }}".replace("'", '"')
    return tmpl


def _session(a, kw, sync: bool):
    """`Session(engine)` / `AsyncSession(engine)`: a session of its own on the binary's pool."""
    bad = sorted(set(kw) - {"bind", "expire_on_commit", "autoflush"})
    if bad or len(a) > 1:
        raise ValueError(f"{'Session' if sync else 'AsyncSession'}() is supported with the engine, bind=, "
                         "expire_on_commit= and autoflush= only" + (f" (not {bad[0]}=)" if bad else ""))
    return f"{RT}::orm::new_session(cx, {_argv(a)}, {_kwvec(kw)}, {'true' if sync else 'false'})"


def _argv(args: list[str]) -> str:
    return "vec![" + ", ".join(args) + "]"


def _mcp_server(a, kw):
    bad = sorted(set(kw) - {"name", "title", "instructions", "version"})
    if bad or len(a) > 2:
        raise ValueError(f"MCPServer({bad[0] if bad else '*args'}=...) is not supported (name, title, instructions, version)")
    return f"{RT}::mcp::server(&{_argv(a)}, &{_kwvec(kw)})"


def _mcp_security(a, kw):
    if a or set(kw) != {"enable_dns_rebinding_protection"} or kw["enable_dns_rebinding_protection"] != "V::Bool(false)":
        raise ValueError("TransportSecuritySettings: only enable_dns_rebinding_protection=False is supported "
                         "(the binary does not check Host/Origin)")
    return "Ok::<V, Exc>(V::None)"


def _checked(name, a, kw, params):
    """The keyword arguments of a call whose parameters are `params` (anything else refused)."""
    _bind(name, a, kw, params, set(params))
    return kw


def _zipfile(a, kw):
    """`ZipFile(file, "w", ...)`: writing only, the mode a literal (reading an archive is not supported)."""
    _bind("zipfile.ZipFile", a, kw, ["file", "mode", "compression", "allowZip64", "compresslevel"],
          {"file", "mode", "compression", "allowZip64", "compresslevel"})
    mode = a[1] if len(a) > 1 else kw.get("mode")
    if mode != 'V::str("w")':
        raise ValueError('only zipfile.ZipFile(file, "w", ...) is supported (writing, the mode a literal)')
    return f"{RT}::zipw::open(&{_argv(a)}, &{_kwvec(kw)})"


def _noargs(name, code):
    def f(a, kw):
        if a or kw:
            raise ValueError(f"{name}() takes no arguments here")
        return code
    return f


def _bindparam(a, kw):
    """`bindparam(key, value=...)`: the value from the execution's parameters (or `value`); the type comes from
    the column it meets (type_=ARRAY(String()) is read by the translator; expanding=, required=, callable_=...
    are refused)"""
    if set(kw) - {"value"} or not 1 <= len(a) <= 2 or (len(a) == 2 and kw):
        raise ValueError("bindparam(key, value=None, type_=ARRAY(String())): other arguments are not supported")
    v = a[1] if len(a) == 2 else kw.get("value")
    return f"{RT}::orm::bindparam(&{a[0]}, {f'Some(&{v})' if v else 'None'})"


def _aliased(a, kw):
    """`aliased(Model)`, `aliased(Model, name=...)`, `aliased(Model, select(Model)...subquery())`; `flat=` changes only
    the aliases of a joined table inheritance's tables (refused there): accepted and ignored elsewhere"""
    kw = {k: v for k, v in kw.items() if k != "flat"}
    if len(a) == 1 and not kw:
        return f"{RT}::orm::aliased(&{a[0]})"
    if len(a) == 1 and set(kw) == {"name"}:
        return f"{RT}::orm::aliased_named(&{a[0]}, &{kw['name']})"
    if len(a) == 2 and not kw:
        return f"{RT}::orm::aliased_sub(&{a[0]}, &{a[1]})"
    raise ValueError("aliased(Model), aliased(Model, name=...), aliased(Model, subquery) and flat= only")


def _raiseload(name, kw):
    """the `raiseload=` keyword of a column loader option (the only one it takes here)"""
    if set(kw) - {"raiseload"}:
        raise ValueError(f"{name}(): only the raiseload= keyword is supported")
    return kw.get("raiseload", "V::Bool(false)")


def _op2(name, a, kw):
    """`operator.eq(a, b)` & co: exactly two positional arguments"""
    if len(a) != 2 or kw:
        raise ValueError(f"operator.{name}() takes exactly two positional arguments")
    return f"{RT}::ops::{name}(&{a[0]}, &{a[1]})"


def _one_pos(name, tmpl):
    """a function of exactly one positional-only argument"""
    def f(a, kw):
        if len(a) != 1 or kw:
            raise ValueError(f"{name}() takes exactly one positional argument")
        return tmpl(a[0])
    return f


def _orjson_dumps(a, kw):
    """`orjson.dumps(obj, default=None, option=None)`"""
    _bind("orjson.dumps", a, kw, ["obj", "default", "option"], {"obj", "default", "option"})
    return f"{RT}::orj::dumps(cx, {_argv(a)}, {_kwvec(kw)}).await"


_ECS_PARAMS = ["fmt", "datefmt", "style", "validate", "stack_trace_limit", "extra", "exclude_fields", "ensure_ascii"]


def _ecs_formatter(a, kw):
    _bind("ecs_logging.StdlibFormatter", a, kw, _ECS_PARAMS, set(_ECS_PARAMS))
    return f"{RT}::logging::ecs_formatter_new(&{_argv(a)}, &{_kwvec(kw)})"


def _unidecode(name, a, kw):
    _bind(f"unidecode.{name}", a, kw, ["string", "errors", "replace_str"], {"string", "errors", "replace_str"})
    return f"{RT}::unidec::unidecode(&{_argv(a)}, &{_kwvec(kw)}, {'false' if name.endswith('nonascii') else 'true'})"


def _one(name):
    def f(a, kw):
        if len(a) != 1 or kw:
            raise ValueError(f"{name}() takes exactly one positional argument here")
        return a[0]
    return f


# --- values (attribute or name used as a value) -------------------------------------------
VALUES: dict[str, str] = {
    "sys.maxsize": "V::Int(i64::MAX)",  # CPython's on 64-bit platforms
    "datetime.UTC": f"V::Tz({RT}::dt::Tz::Utc)",
    "datetime.timezone.utc": f"V::Tz({RT}::dt::Tz::Utc)",
    "sqlalchemy.null": "V::None",
    **{f"sqlalchemy.{t}": f'V::str("{t}")' for t in ("String", "Text", "Unicode", "Integer", "BigInteger", "Float", "Date", "Boolean", "Numeric")},
    "os.environ": f"{RT}::libs::environ()",
    **{f"orjson.{k}": f"V::Int({v})" for k, v in (
        ("OPT_APPEND_NEWLINE", 1024), ("OPT_INDENT_2", 1), ("OPT_NAIVE_UTC", 2), ("OPT_NON_STR_KEYS", 4),
        ("OPT_OMIT_MICROSECONDS", 8), ("OPT_PASSTHROUGH_DATACLASS", 2048), ("OPT_PASSTHROUGH_DATETIME", 512),
        ("OPT_PASSTHROUGH_SUBCLASS", 256), ("OPT_SERIALIZE_DATACLASS", 0), ("OPT_SERIALIZE_NUMPY", 16),
        ("OPT_SERIALIZE_UUID", 0), ("OPT_SORT_KEYS", 32), ("OPT_STRICT_INTEGER", 64), ("OPT_UTC_Z", 128))},
    "aio_pika.DeliveryMode.PERSISTENT": "V::Int(2)",
    # aio_pika.ExchangeType members are their values (like DeliveryMode's: documented)
    **{f"aio_pika.{p}ExchangeType.{m}": f'V::str("{m.lower().replace("_", "-")}")' for p in ("", "exchange.")
       for m in ("DIRECT", "FANOUT", "TOPIC", "HEADERS", "X_DELAYED_MESSAGE", "X_CONSISTENT_HASH", "X_MODULUS_HASH")},
    "zipfile.ZIP_STORED": "V::Int(0)",
    "zipfile.ZIP_DEFLATED": "V::Int(8)",
    # cryptography's serialization constants (dynrt/crypto.rs)
    **{f"cryptography.hazmat.primitives.serialization.{c}": f'V::str("{c}")' for c in (
        "Encoding.PEM", "Encoding.DER", "PrivateFormat.PKCS8", "PrivateFormat.TraditionalOpenSSL",
        "PublicFormat.SubjectPublicKeyInfo", "PublicFormat.PKCS1")},
    # the HTTP client modules as values (`if aiohttp is not None`) and their patchable request methods
    "aiohttp": f'V::native({RT}::Native::Namespace("aiohttp"))',
    "httpx": f'V::native({RT}::Native::Namespace("httpx"))',
    "aiohttp.ClientSession._request": f'{RT}::http::request_fn("aiohttp")',
    "httpx.AsyncClient.request": f'{RT}::http::request_fn("httpx")',
    # starlette.websockets.WebSocketState (dynrt/ws.rs)
    **{f"{m}.WebSocketState": f"V::Class(&{RT}::ws::CLS_WS_STATE)" for m in ("starlette.websockets", "fastapi.websockets")},
    **{f"{m}.WebSocketState.{n}": f"V::Enum(&{RT}::ws::ENUM_WS_STATE, {i})" for m in ("starlette.websockets", "fastapi.websockets")
       for i, n in enumerate(("CONNECTING", "CONNECTED", "DISCONNECTED", "RESPONSE"))},
    # starlette.routing.Match and the routing classes (dynrt/routing.rs)
    "starlette.routing.Match": f"V::Class(&{RT}::routing::CLS_MATCH)",
    **{f"starlette.routing.Match.{n}": f"V::Enum(&{RT}::routing::ENUM_MATCH, {i})" for i, n in enumerate(("NONE", "PARTIAL", "FULL"))},
    **{n: f'V::native({RT}::Native::ExtType("{c}"))' for n, c in (
        ("starlette.routing.Route", "starlette.routing.Route"), ("starlette.routing.BaseRoute", "starlette.routing.BaseRoute"),
        ("fastapi.routing.APIRoute", "fastapi.routing.APIRoute"), ("fastapi.APIRoute", "fastapi.routing.APIRoute"),
        ("starlette.routing.WebSocketRoute", "starlette.routing.WebSocketRoute"),
        ("fastapi.routing.APIWebSocketRoute", "fastapi.routing.APIWebSocketRoute"),
        ("fastapi.APIWebSocketRoute", "fastapi.routing.APIWebSocketRoute"))},
    # prometheus_client 0.26 (dynrt/prom.rs)
    **{f"prometheus_client.{n}": f'{RT}::prom::value("{n}")' for n in ("REGISTRY", "GC_COLLECTOR", "PLATFORM_COLLECTOR", "PROCESS_COLLECTOR")},
    "prometheus_client.CONTENT_TYPE_LATEST": 'V::str("text/plain; version=1.0.0; charset=utf-8")',
    "prometheus_client.CONTENT_TYPE_PLAIN_1_0_0": 'V::str("text/plain; version=1.0.0; charset=utf-8")',
    "prometheus_client.CONTENT_TYPE_PLAIN_0_0_4": 'V::str("text/plain; version=0.0.4; charset=utf-8")',
    "prometheus_client.openmetrics.exposition.CONTENT_TYPE_LATEST": 'V::str("application/openmetrics-text; version=1.0.0; charset=utf-8")',
    **{f"prometheus_client.{n}": f'V::native({RT}::Native::ExtType("prometheus_client.{n}"))'
       for n in ("Counter", "Gauge", "Summary", "Histogram", "Info", "Enum", "CollectorRegistry")},
    **{f"tenacity.{n}": f'{RT}::tenacity::value("{n}")' for n in ("stop_never", "retry_always", "retry_never")},
    "logging.root": f"{RT}::logging::logger_value({RT}::logging::root())",
    "logging.lastResort": f"{RT}::logging::last_resort()",
    "logging.raiseExceptions": "V::Bool(true)",
    **{f"sys.{n}": f"{RT}::logging::stream({1 if 'out' in n else 2})" for n in ("stdout", "stderr", "__stdout__", "__stderr__")},
    **{f"logging.{n}": f"V::Int({v})" for n, v in (("NOTSET", 0), ("DEBUG", 10), ("INFO", 20), ("WARNING", 30), ("WARN", 30),
                                                    ("ERROR", 40), ("CRITICAL", 50), ("FATAL", 50))},
    "aio_pika.DeliveryMode.NOT_PERSISTENT": "V::Int(1)",
    "datetime.time.min": "V::Time(chrono::NaiveTime::MIN)",
    "datetime.time.max": "V::Time(chrono::NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999).unwrap())",
    "datetime.datetime.min": f"V::DateTime({RT}::dt::DateTime::naive(chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()))",
    "datetime.datetime.max": f"V::DateTime({RT}::dt::DateTime::naive(chrono::NaiveDate::from_ymd_opt(9999, 12, 31).unwrap().and_hms_micro_opt(23, 59, 59, 999_999).unwrap()))",
    "datetime.date.min": "V::Date(chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap())",
    "datetime.date.max": "V::Date(chrono::NaiveDate::from_ymd_opt(9999, 12, 31).unwrap())",
    # library classes used as values (isinstance targets, issubclass, `X is not None`)
    **{n: f'V::native({RT}::Native::ExtType("{c}"))' for n, c in (
        ("pydantic.BaseModel", "pydantic.BaseModel"), ("pydantic.main.BaseModel", "pydantic.BaseModel"),
        ("pydantic.TypeAdapter", "pydantic.TypeAdapter"),
        ("types.GenericAlias", "types.GenericAlias"), ("typing.Any", "typing.Any"),
        ("sqlalchemy.ext.asyncio.AsyncSession", "sqlalchemy.ext.asyncio.AsyncSession"),
        ("sqlalchemy.ext.asyncio.AsyncEngine", "sqlalchemy.ext.asyncio.AsyncEngine"),
        ("sqlalchemy.ext.asyncio.AsyncConnection", "sqlalchemy.ext.asyncio.AsyncConnection"),
        ("sqlalchemy.orm.Session", "sqlalchemy.orm.Session"),
        # pool classes: create_engine(poolclass=...) options are ignored (the binary's pool)
        ("sqlalchemy.pool.StaticPool", "sqlalchemy.pool.StaticPool"), ("sqlalchemy.pool.NullPool", "sqlalchemy.pool.NullPool"),
        ("sqlalchemy.pool.QueuePool", "sqlalchemy.pool.QueuePool"))},
    **{f"re.{n}": f"V::Int({v})" for n, v in (("I", 2), ("IGNORECASE", 2), ("M", 8), ("MULTILINE", 8), ("S", 16),
                                             ("DOTALL", 16), ("X", 64), ("VERBOSE", 64), ("UNICODE", 32), ("U", 32))},
    **{f"csv.QUOTE_{n}": f"V::Int({v})" for n, v in (("MINIMAL", 0), ("ALL", 1), ("NONNUMERIC", 2), ("NONE", 3))},
    "csv.excel": 'V::str("excel")',
    # PyYAML: the loader/dumper classes a load()/dump() call may name (dynrt/pyyaml.rs, yamlemit.rs)
    **{f"yaml{m}.{n}": f'V::native({RT}::Native::ExtType("yaml.{n}"))'
       for m in ("", ".loader", ".dumper") for n in ("SafeLoader", "SafeDumper", "Dumper")},
    "math.pi": "V::Float(std::f64::consts::PI)",
    **{f"decimal.{r}": f'V::str("{r}")' for r in ("ROUND_HALF_EVEN", "ROUND_HALF_UP", "ROUND_HALF_DOWN", "ROUND_DOWN",
                                                   "ROUND_UP", "ROUND_FLOOR", "ROUND_CEILING", "ROUND_05UP")},
    "math.e": "V::Float(std::f64::consts::E)",
    "math.inf": "V::Float(f64::INFINITY)",
    **{f"string.{n}": f'V::str("{v}")' for n, v in (
        ("ascii_letters", "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"),
        ("ascii_lowercase", "abcdefghijklmnopqrstuvwxyz"), ("ascii_uppercase", "ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
        ("digits", "0123456789"), ("hexdigits", "0123456789abcdefABCDEF"), ("octdigits", "01234567"))},
    **{f"hashlib.{n}": f'V::native({RT}::Native::HashCtor("{n}"))' for n in ("sha256", "sha1", "md5", "sha512")},
    # dnspython 2.8 (dynrt/dnsres.rs)
    "dns.name.root": f"{RT}::dnsres::root_name()",
    "dns.name.empty": f"{RT}::dnsres::empty_name()",
    # the modern email policies EmailMessage takes (dynrt/emailmsg.rs)
    **{f"email.policy.{n}": f'V::native({RT}::Native::ExtType("email.policy.{n}"))' for n in ("default", "SMTP", "SMTPUTF8", "HTTP")},
}

# values above that are snapshots: writing into them would be lost, so it is refused
READ_ONLY_VALUES = {"os.environ"}
# library attributes a project may replace (monkeypatching the HTTP clients' request method to wrap it):
# the runtime's clients then call the replacement
HOOKS = {
    "aiohttp.ClientSession._request": f'{RT}::http::set_hook("aiohttp", {{val}})',
    "httpx.AsyncClient.request": f'{RT}::http::set_hook("httpx", {{val}})',
}
# library values that are run-time objects: their methods are called on the value
OBJECT_VALUES = {"prometheus_client.REGISTRY", "aiohttp.ClientSession._request", "httpx.AsyncClient.request"}

# --- exception classes usable in `except` / `isinstance` / `raise` -------------------------
EXCEPTIONS: dict[str, str] = {
    "builtins.BaseException": "BASE_EXCEPTION",
    "builtins.Exception": "EXCEPTION",
    "builtins.ValueError": "VALUE_ERROR",
    "builtins.TypeError": "TYPE_ERROR",
    "builtins.KeyError": "KEY_ERROR",
    "builtins.IndexError": "INDEX_ERROR",
    "builtins.LookupError": "LOOKUP_ERROR",
    "builtins.AttributeError": "ATTRIBUTE_ERROR",
    "builtins.RuntimeError": "RUNTIME_ERROR",
    "builtins.AssertionError": "ASSERTION_ERROR",
    "builtins.NotImplementedError": "NOT_IMPLEMENTED_ERROR",
    "builtins.RecursionError": "RECURSION_ERROR",
    "builtins.MemoryError": "MEMORY_ERROR",
    "builtins.ZeroDivisionError": "ZERO_DIVISION_ERROR",
    "builtins.ArithmeticError": "ARITHMETIC_ERROR",
    "builtins.OverflowError": "OVERFLOW_ERROR",
    "builtins.TimeoutError": "TIMEOUT_ERROR",
    "builtins.OSError": "OS_ERROR",
    "builtins.ConnectionError": "CONNECTION_ERROR",
    "builtins.ConnectionRefusedError": "CONNECTION_REFUSED_ERROR",
    "builtins.ConnectionResetError": "CONNECTION_RESET_ERROR",
    "builtins.ConnectionAbortedError": "CONNECTION_ABORTED_ERROR",
    "builtins.BrokenPipeError": "BROKEN_PIPE_ERROR",
    "builtins.StopIteration": "STOP_ITERATION",
    "builtins.StopAsyncIteration": "STOP_ASYNC_ITERATION",
    "builtins.NameError": "NAME_ERROR",
    "builtins.FileNotFoundError": "FILE_NOT_FOUND_ERROR",
    "builtins.UnicodeError": "UNICODE_ERROR",
    "builtins.UnicodeDecodeError": "UNICODE_DECODE_ERROR",
    "builtins.UnicodeEncodeError": "UNICODE_ENCODE_ERROR",
    "builtins.ImportError": "IMPORT_ERROR",
    "builtins.ModuleNotFoundError": "MODULE_NOT_FOUND_ERROR",
    "importlib.metadata.PackageNotFoundError": "PACKAGE_NOT_FOUND",
    "builtins.FileExistsError": "FILE_EXISTS_ERROR",
    "builtins.PermissionError": "PERMISSION_ERROR",
    "builtins.IsADirectoryError": "IS_A_DIRECTORY_ERROR",
    "builtins.GeneratorExit": "GENERATOR_EXIT",
    "binascii.Error": "BINASCII_ERROR",
    "json.JSONDecodeError": "JSON_DECODE_ERROR",
    "orjson.JSONDecodeError": "ORJSON_DECODE_ERROR",
    "orjson.JSONEncodeError": "TYPE_ERROR",
    "unidecode.UnidecodeError": "UNIDECODE_ERROR",
    "gzip.BadGzipFile": "BAD_GZIP_FILE",
    "zlib.error": "ZLIB_ERROR",
    "sqlalchemy.exc.CompileError": "COMPILE_ERROR",
    "decimal.DecimalException": "DECIMAL_EXCEPTION",
    "decimal.InvalidOperation": "DECIMAL_INVALID_OPERATION",
    "decimal.DivisionByZero": "DECIMAL_DIVISION_BY_ZERO",
    "pywebpush.WebPushException": "WEBPUSH_EXCEPTION",
    **{f"google.auth.exceptions.{n}": f"GOOGLE_{c}" for n, c in (
        ("GoogleAuthError", "AUTH_ERROR"), ("TransportError", "TRANSPORT_ERROR"),
        ("DefaultCredentialsError", "DEFAULT_CREDENTIALS_ERROR"), ("MalformedError", "MALFORMED_ERROR"),
        ("InvalidValue", "INVALID_VALUE"))},
    "py_vapid.VapidException": "VAPID_EXCEPTION",
    "requests.RequestException": "REQUESTS_EXCEPTION",
    "requests.exceptions.RequestException": "REQUESTS_EXCEPTION",
    "requests.ConnectionError": "REQUESTS_CONNECTION_ERROR",
    "requests.exceptions.ConnectionError": "REQUESTS_CONNECTION_ERROR",
    "json.decoder.JSONDecodeError": "JSON_DECODE_ERROR",
    "socket.gaierror": "SOCKET_GAIERROR",
    **{f"httpx.{n}": f"HTTPX_{c}" for n, c in (
        ("HTTPError", "HTTP_ERROR"), ("RequestError", "REQUEST_ERROR"), ("TransportError", "TRANSPORT_ERROR"),
        ("TimeoutException", "TIMEOUT_EXCEPTION"), ("ConnectTimeout", "CONNECT_TIMEOUT"), ("ReadTimeout", "READ_TIMEOUT"),
        ("NetworkError", "NETWORK_ERROR"), ("ConnectError", "CONNECT_ERROR"), ("UnsupportedProtocol", "UNSUPPORTED_PROTOCOL"),
        ("TooManyRedirects", "TOO_MANY_REDIRECTS"), ("DecodingError", "DECODING_ERROR"), ("HTTPStatusError", "STATUS_ERROR"), ("InvalidURL", "INVALID_URL"))},
    **{f"aiohttp.{n}": f"AIO_{c}" for n, c in (
        ("ClientError", "CLIENT_ERROR"), ("ClientResponseError", "RESPONSE_ERROR"), ("ContentTypeError", "CONTENT_TYPE_ERROR"),
        ("TooManyRedirects", "TOO_MANY_REDIRECTS"), ("ClientConnectionError", "CONNECTION_ERROR"), ("ClientOSError", "OS_ERROR"),
        ("ClientConnectorError", "CONNECTOR_ERROR"), ("ServerConnectionError", "SERVER_CONNECTION_ERROR"),
        ("ServerTimeoutError", "SERVER_TIMEOUT"), ("ConnectionTimeoutError", "CONNECTION_TIMEOUT"), ("InvalidURL", "INVALID_URL"))},
    "asyncio.TimeoutError": "TIMEOUT_ERROR",
    "asyncio.CancelledError": "CANCELLED_ERROR",
    "asyncio.InvalidStateError": "INVALID_STATE_ERROR",
    "mcp.server.mcpserver.exceptions.ToolError": "MCP_TOOL_ERROR",
    "asyncio.QueueFull": "QUEUE_FULL",
    "asyncio.QueueEmpty": "QUEUE_EMPTY",
    "fastapi.HTTPException": "HTTP_EXCEPTION",
    "fastapi.exceptions.HTTPException": "HTTP_EXCEPTION",
    # WebSockets (dynrt/ws.rs)
    **{n: "WS_DISCONNECT" for n in ("fastapi.WebSocketDisconnect", "fastapi.websockets.WebSocketDisconnect",
                                    "starlette.websockets.WebSocketDisconnect")},
    "starlette.websockets.WebSocketDisconnected": "WS_DISCONNECTED",
    **{n: "WS_EXCEPTION" for n in ("fastapi.WebSocketException", "fastapi.exceptions.WebSocketException",
                                   "starlette.exceptions.WebSocketException")},
    "fastapi.exceptions.WebSocketRequestValidationError": "WS_VALIDATION_ERROR",
    "fastapi.exceptions.RequestValidationError": "REQUEST_VALIDATION_ERROR",
    "fastapi.exceptions.ValidationException": "VALIDATION_EXCEPTION",
    "starlette.exceptions.HTTPException": "HTTP_EXCEPTION",
    "pydantic.ValidationError": "VALIDATION_ERROR",
    **{f"pydantic_core{m}.{n}": c for m in ("", "._pydantic_core") for n, c in (("PydanticCustomError", "PYDANTIC_CUSTOM_ERROR"),
                                                                               ("PydanticUseDefault", "PYDANTIC_USE_DEFAULT"))},
    "sqlalchemy.exc.SQLAlchemyError": "SQLALCHEMY_ERROR",
    "sqlalchemy.exc.DBAPIError": "DBAPI_ERROR",
    "sqlalchemy.exc.IntegrityError": "INTEGRITY_ERROR",
    "sqlalchemy.exc.OperationalError": "OPERATIONAL_ERROR",
    "sqlalchemy.exc.DataError": "DATA_ERROR",
    "sqlalchemy.exc.ProgrammingError": "PROGRAMMING_ERROR",
    "sqlalchemy.exc.InternalError": "INTERNAL_ERROR",
    "sqlalchemy.exc.NotSupportedError": "NOT_SUPPORTED_ERROR",
    "sqlalchemy.exc.NoResultFound": "NO_RESULT_FOUND",
    "sqlalchemy.exc.InvalidRequestError": "INVALID_REQUEST_ERROR",
    "sqlalchemy.orm.exc.UnmappedInstanceError": "UNMAPPED_INSTANCE_ERROR",
    "sqlalchemy.exc.StatementError": "STATEMENT_ERROR",
    "sqlalchemy.exc.MissingGreenlet": "MISSING_GREENLET",
    "sqlalchemy.exc.CircularDependencyError": "CIRCULAR_DEPENDENCY_ERROR",
    "sqlalchemy.exc.ResourceClosedError": "RESOURCE_CLOSED_ERROR",
    "xml.parsers.expat.ExpatError": "EXPAT_ERROR",
    "xml.parsers.expat.error": "EXPAT_ERROR",
    "pyexpat.ExpatError": "EXPAT_ERROR",
    "builtins.EOFError": "EOF_ERROR",
    "pickle.PickleError": "PICKLE_ERROR",
    "aio_pika.exceptions.AMQPError": "AMQP_ERROR",
    "tenacity.RetryError": "TENACITY_RETRY_ERROR",
    "prometheus_client.registry.DuplicateTimeseries": "DUPLICATE_TIMESERIES",
    "aio_pika.exceptions.AMQPConnectionError": "AMQP_CONNECTION_ERROR",
    "aio_pika.exceptions.QueueEmpty": "AMQP_QUEUE_EMPTY",
    "aio_pika.exceptions.AMQPChannelError": "AMQP_CHANNEL_ERROR",
    "aio_pika.exceptions.ChannelClosed": "AMQP_CHANNEL_CLOSED",
    "aio_pika.exceptions.ChannelAccessRefused": "AMQP_CHANNEL_ACCESS_REFUSED",
    "aio_pika.exceptions.ChannelNotFoundEntity": "AMQP_CHANNEL_NOT_FOUND",
    "aio_pika.exceptions.ChannelLockedResource": "AMQP_CHANNEL_LOCKED",
    "aio_pika.exceptions.ChannelPreconditionFailed": "AMQP_CHANNEL_PRECONDITION",
    "aio_pika.exceptions.ConnectionClosed": "AMQP_CONNECTION_CLOSED",
    "aiormq.exceptions.AMQPChannelError": "AMQP_CHANNEL_ERROR",
    "aiormq.exceptions.ChannelClosed": "AMQP_CHANNEL_CLOSED",
    "aiormq.exceptions.ChannelAccessRefused": "AMQP_CHANNEL_ACCESS_REFUSED",
    "aiormq.exceptions.ChannelNotFoundEntity": "AMQP_CHANNEL_NOT_FOUND",
    "aiormq.exceptions.ChannelLockedResource": "AMQP_CHANNEL_LOCKED",
    "aiormq.exceptions.ChannelPreconditionFailed": "AMQP_CHANNEL_PRECONDITION",
    "aiormq.exceptions.ConnectionClosed": "AMQP_CONNECTION_CLOSED",
    "aiormq.AMQPChannelError": "AMQP_CHANNEL_ERROR",
    "aiormq.ChannelClosed": "AMQP_CHANNEL_CLOSED",
    "aiormq.ChannelAccessRefused": "AMQP_CHANNEL_ACCESS_REFUSED",
    "aiormq.ChannelNotFoundEntity": "AMQP_CHANNEL_NOT_FOUND",
    "aiormq.ChannelLockedResource": "AMQP_CHANNEL_LOCKED",
    "aiormq.ChannelPreconditionFailed": "AMQP_CHANNEL_PRECONDITION",
    "aiormq.ConnectionClosed": "AMQP_CONNECTION_CLOSED",
    "aio_pika.exceptions.MessageProcessError": "AMQP_MESSAGE_PROCESS_ERROR",
    **{f"redis.exceptions.{n}": c for n, c in (("RedisError", "REDIS_ERROR"), ("ConnectionError", "REDIS_CONNECTION_ERROR"),
                                                ("TimeoutError", "REDIS_TIMEOUT_ERROR"), ("DataError", "REDIS_DATA_ERROR"),
                                                ("ResponseError", "REDIS_RESPONSE_ERROR"))},
    "pickle.PicklingError": "PICKLING_ERROR",
    "pickle.UnpicklingError": "UNPICKLING_ERROR",
    "sqlalchemy.exc.MultipleResultsFound": "MULTIPLE_RESULTS_FOUND",
    "sqlalchemy.orm.exc.NoResultFound": "NO_RESULT_FOUND",
    # sqlalchemy.orm.exc only (canonical() folds it into sqlalchemy.exc)
    "sqlalchemy.exc.StaleDataError": "STALE_DATA_ERROR",
    "sqlalchemy.exc.DetachedInstanceError": "DETACHED_INSTANCE_ERROR",  # sqlalchemy.orm.exc (canonical())
    # what python-jose re-exports from jose and jose.jwt
    **{f"jose.{n}": c for n, c in (("ExpiredSignatureError", "EXPIRED_SIGNATURE_ERROR"), ("JOSEError", "JOSE_ERROR"),
                                   ("JWSError", "JWS_ERROR"), ("JWTError", "JWT_ERROR"))},
    **{f"jose.jwt.{n}": c for n, c in (("ExpiredSignatureError", "EXPIRED_SIGNATURE_ERROR"), ("JWSError", "JWS_ERROR"),
                                       ("JWTClaimsError", "JWT_CLAIMS_ERROR"), ("JWTError", "JWT_ERROR"))},
    "jose.exceptions.JOSEError": "JOSE_ERROR",
    "jose.exceptions.JWSError": "JWS_ERROR",
    "jose.exceptions.JWTError": "JWT_ERROR",
    "jose.exceptions.JWTClaimsError": "JWT_CLAIMS_ERROR",
    "jose.exceptions.ExpiredSignatureError": "EXPIRED_SIGNATURE_ERROR",
    "jose.exceptions.JWKError": "JWK_ERROR",
    "jose.JWTError": "JWT_ERROR",
    "jose.JWSError": "JWS_ERROR",
    "jose.ExpiredSignatureError": "EXPIRED_SIGNATURE_ERROR",
    "jose.JWKError": "JWK_ERROR",
    # PyJWT: jwt.exceptions, and the names `jwt` re-exports
    **{f"jwt.exceptions.{n}": c for n, c in (("PyJWTError", "PYJWT_ERROR"), ("InvalidTokenError", "PYJWT_INVALID_TOKEN"), ("DecodeError", "PYJWT_DECODE_ERROR"), ("InvalidSignatureError", "PYJWT_INVALID_SIGNATURE"), ("ExpiredSignatureError", "PYJWT_EXPIRED_SIGNATURE"), ("InvalidAudienceError", "PYJWT_INVALID_AUDIENCE"), ("InvalidIssuerError", "PYJWT_INVALID_ISSUER"), ("InvalidIssuedAtError", "PYJWT_INVALID_ISSUED_AT"), ("ImmatureSignatureError", "PYJWT_IMMATURE_SIGNATURE"), ("InvalidKeyError", "PYJWT_INVALID_KEY"), ("InvalidAlgorithmError", "PYJWT_INVALID_ALGORITHM"), ("MissingRequiredClaimError", "PYJWT_MISSING_REQUIRED_CLAIM"), ("PyJWKError", "PYJWK_ERROR"), ("MissingCryptographyError", "PYJWT_MISSING_CRYPTOGRAPHY"), ("PyJWKSetError", "PYJWK_SET_ERROR"), ("PyJWKClientError", "PYJWK_CLIENT_ERROR"), ("PyJWKClientConnectionError", "PYJWK_CLIENT_CONNECTION_ERROR"), ("InvalidSubjectError", "PYJWT_INVALID_SUBJECT"), ("InvalidJTIError", "PYJWT_INVALID_JTI"))},
    **{f"jwt.{n}": c for n, c in (("PyJWTError", "PYJWT_ERROR"), ("InvalidTokenError", "PYJWT_INVALID_TOKEN"), ("DecodeError", "PYJWT_DECODE_ERROR"), ("InvalidSignatureError", "PYJWT_INVALID_SIGNATURE"), ("ExpiredSignatureError", "PYJWT_EXPIRED_SIGNATURE"), ("InvalidAudienceError", "PYJWT_INVALID_AUDIENCE"), ("InvalidIssuerError", "PYJWT_INVALID_ISSUER"), ("InvalidIssuedAtError", "PYJWT_INVALID_ISSUED_AT"), ("ImmatureSignatureError", "PYJWT_IMMATURE_SIGNATURE"), ("InvalidKeyError", "PYJWT_INVALID_KEY"), ("InvalidAlgorithmError", "PYJWT_INVALID_ALGORITHM"), ("MissingRequiredClaimError", "PYJWT_MISSING_REQUIRED_CLAIM"), ("PyJWKError", "PYJWK_ERROR"), ("PyJWKSetError", "PYJWK_SET_ERROR"), ("PyJWKClientError", "PYJWK_CLIENT_ERROR"), ("PyJWKClientConnectionError", "PYJWK_CLIENT_CONNECTION_ERROR"))},
    "cryptography.fernet.InvalidToken": "INVALID_TOKEN",
    # PyYAML: yaml re-exports YAMLError and MarkedYAMLError only, each module its own class
    **{f"yaml{m}.{n}": c for m, names in (
        ("", (("YAMLError", "YAML_ERROR"), ("MarkedYAMLError", "YAML_MARKED_ERROR"))),
        (".error", (("YAMLError", "YAML_ERROR"), ("MarkedYAMLError", "YAML_MARKED_ERROR"))),
        (".reader", (("ReaderError", "YAML_READER_ERROR"),)), (".scanner", (("ScannerError", "YAML_SCANNER_ERROR"),)),
        (".parser", (("ParserError", "YAML_PARSER_ERROR"),)), (".composer", (("ComposerError", "YAML_COMPOSER_ERROR"),)),
        (".constructor", (("ConstructorError", "YAML_CONSTRUCTOR_ERROR"),)),
        (".representer", (("RepresenterError", "YAML_REPRESENTER_ERROR"),)),
        (".emitter", (("EmitterError", "YAML_EMITTER_ERROR"),)), (".serializer", (("SerializerError", "YAML_SERIALIZER_ERROR"),)))
       for n, c in names},
    "jinja2.TemplateNotFound": "TEMPLATE_NOT_FOUND",
    "re.error": "RE_ERROR",
    "csv.Error": "CSV_ERROR",
    "jinja2.exceptions.TemplateNotFound": "TEMPLATE_NOT_FOUND",
    "aiosmtplib.SMTPException": "SMTP_EXCEPTION",
    "aiosmtplib.errors.SMTPException": "SMTP_EXCEPTION",
    # dnspython 2.8: each module's classes (dns.resolver.Timeout is LifetimeTimeout)
    **{f"dns.{m}.{n}": c for m, names in (
        ("exception", (("DNSException", "DNS_EXCEPTION"), ("FormError", "DNS_FORM_ERROR"), ("SyntaxError", "DNS_SYNTAX_ERROR"),
                       ("Timeout", "DNS_TIMEOUT"))),
        ("resolver", (("NXDOMAIN", "DNS_NXDOMAIN"), ("YXDOMAIN", "DNS_YXDOMAIN"), ("NoAnswer", "DNS_NO_ANSWER"),
                      ("NoNameservers", "DNS_NO_NAMESERVERS"), ("NotAbsolute", "DNS_NOT_ABSOLUTE"), ("NoRootSOA", "DNS_NO_ROOT_SOA"),
                      ("NoMetaqueries", "DNS_NO_METAQUERIES"), ("NoResolverConfiguration", "DNS_NO_RESOLVER_CONFIGURATION"),
                      ("LifetimeTimeout", "DNS_LIFETIME_TIMEOUT"), ("Timeout", "DNS_LIFETIME_TIMEOUT"))),
        ("name", (("EmptyLabel", "DNS_EMPTY_LABEL"), ("BadEscape", "DNS_BAD_ESCAPE"), ("LabelTooLong", "DNS_LABEL_TOO_LONG"),
                  ("NameTooLong", "DNS_NAME_TOO_LONG"), ("BadPointer", "DNS_BAD_POINTER"), ("BadLabelType", "DNS_BAD_LABEL_TYPE"))),
        ("rdatatype", (("UnknownRdatatype", "DNS_UNKNOWN_RDATATYPE"),)),
        ("message", (("ShortHeader", "DNS_SHORT_HEADER"), ("TrailingJunk", "DNS_TRAILING_JUNK"), ("BadEDNS", "DNS_BAD_EDNS"))))
       for n, c in names},
    **{f"email.errors.{n}": c for n, c in (("MessageError", "EMAIL_MESSAGE_ERROR"), ("HeaderParseError", "HEADER_PARSE_ERROR"),
                                            ("HeaderWriteError", "HEADER_WRITE_ERROR"))},
    "itsdangerous.BadData": "BAD_DATA",
    "itsdangerous.exc.BadData": "BAD_DATA",
    "itsdangerous.BadSignature": "BAD_SIGNATURE",
    "itsdangerous.exc.BadSignature": "BAD_SIGNATURE",
    "itsdangerous.BadTimeSignature": "BAD_TIME_SIGNATURE",
    "itsdangerous.exc.BadTimeSignature": "BAD_TIME_SIGNATURE",
    "itsdangerous.SignatureExpired": "SIGNATURE_EXPIRED",
    "itsdangerous.exc.SignatureExpired": "SIGNATURE_EXPIRED",
    "itsdangerous.BadHeader": "BAD_HEADER",
    "itsdangerous.exc.BadHeader": "BAD_HEADER",
    "itsdangerous.BadPayload": "BAD_PAYLOAD",
    "itsdangerous.exc.BadPayload": "BAD_PAYLOAD",
    **{f"{m}.{n}": c for m in ("croniter", "croniter.croniter") for n, c in (
        ("CroniterError", "CRONITER_ERROR"), ("CroniterBadCronError", "CRONITER_BAD_CRON_ERROR"),
        ("CroniterBadDateError", "CRONITER_BAD_DATE_ERROR"), ("CroniterBadTypeRangeError", "CRONITER_BAD_TYPE_RANGE_ERROR"),
        ("CroniterNotAlphaError", "CRONITER_NOT_ALPHA_ERROR"),
        ("CroniterUnsupportedSyntaxError", "CRONITER_UNSUPPORTED_SYNTAX_ERROR"))},
}

# keyword arguments refused at transpile time (the call is otherwise supported)
REFUSED_KWARGS = {
    "tenacity.retry": {"sleep", "retry_error_cls"},
    "tenacity.before_sleep_log": {"exc_info"},
    **{f"prometheus_client.{n}": {"_labelvalues"} for n in ("Counter", "Gauge", "Summary", "Histogram", "Info", "Enum")},
}

BUILTIN_EXC_NAMES = {k.split(".", 1)[1]: v for k, v in EXCEPTIONS.items() if k.startswith("builtins.")}


def _timedelta(a, kw):
    return f"{RT}::libs::timedelta(&{_argv(a)}, &{_kwvec(kw)})"


def _dt_now(a, kw):
    tz = a[0] if a else kw.get("tz", "V::None")
    return f"{RT}::methods::now(&{tz})"


def _stream(a, kw):
    content = a[0] if a else kw["content"]
    mt = kw.get("media_type", "V::None")
    status = kw.get("status_code", "V::Int(200)")
    headers = kw.get("headers", "V::None")
    return f"{RT}::streaming({content}, &{mt}, &{status}, &{headers})"


def _http_exc(a, kw):
    names = ["status_code", "detail", "headers"]
    vals = dict(zip(names, a))
    vals.update(kw)
    status = vals.get("status_code")
    if status is None:
        raise ValueError("HTTPException needs status_code")
    detail = vals.get("detail", "V::None")
    headers = vals.get("headers", "V::None")
    return f"{RT}::http_exc(&{status}, {detail}, &{headers})"


def _sa_inspect(a, kw):
    # inspect(x, raiseerr=False) only: with raiseerr=True a non-mapped value raises NoInspectionAvailable
    if len(a) != 1 or set(kw) != {"raiseerr"} or kw.get("raiseerr") != "V::Bool(false)":
        raise ValueError("only inspect(x, raiseerr=False) is supported (state.unloaded, state.mapper.column_attrs)")
    return f"{RT}::orm::inspect(&{a[0]})"


def _json_dumps(a, kw):
    for k in kw:
        if k not in ("default", "ensure_ascii", "separators", "sort_keys", "indent"):
            raise ValueError(f"json.dumps({k}=) is not supported (default=str, ensure_ascii, separators, sort_keys, indent)")
    return f"{RT}::libs::json_dumps(&{a[0]}, &{_kwvec(kw)})"


# versions of the project's libraries (its uv.lock), filled by the dyn backend before compiling
LIB_VERSIONS: dict[str, str] = {}
# the Python the project runs on (dyn.target_python), set by the dyn backend before compiling
PYTHON: tuple[int, int] = (3, 12)


# library names that exist from a Python on (refused for an older project: AttributeError there)
SINCE = {"re.PatternError": (3, 13)}


def too_new(dotted: str) -> str | None:
    """The refusal of a library name newer than the project's Python, None if it exists there."""
    v = SINCE.get(dotted)
    if v is None or PYTHON >= v:
        return None
    return (f"{dotted} needs Python {v[0]}.{v[1]} (the project runs on {PYTHON[0]}.{PYTHON[1]}, "
            "where it raises AttributeError)")


def set_python(version: tuple[int, int]) -> None:
    """The project's Python: names that exist from a version on (`re.PatternError`, 3.13, `re.error` its alias)"""
    global PYTHON
    PYTHON = version
    re_cls = "RE_PATTERN_ERROR" if version >= (3, 13) else "RE_ERROR"
    EXCEPTIONS["re.error"] = re_cls
    if version >= (3, 13):
        EXCEPTIONS["re.PatternError"] = re_cls
    else:
        EXCEPTIONS.pop("re.PatternError", None)


def _no_kw(name: str, kw, refused) -> str:
    """Refuses keyword arguments the runtime does not implement (Python 3.15's `missing_as_none=`...)."""
    for k in kw:
        if k in refused:
            raise ValueError(f"{name}({k}=) is not supported")
    return ""


def _since(version: tuple[int, int], what: str) -> str:
    """Refuses a construct newer than the project's Python (it would raise there: AttributeError, TypeError)."""
    if PYTHON < version:
        raise ValueError(f"{what} needs Python {version[0]}.{version[1]} (the project runs on "
                         f"{PYTHON[0]}.{PYTHON[1]}, where it raises)")
    return ""


def _need(vals, name, *required):
    """`vals` (from _bind) with its required parameters given (CPython's TypeError otherwise, at transpile time)"""
    for r in required:
        if r not in vals:
            raise ValueError(f"{name}() missing required argument '{r}'")
    return vals


def _bind(name, a, kw, params, allowed):
    """Positional + keyword arguments of a library call, by parameter name; anything else is refused."""
    if len(a) > len(params):
        raise ValueError(f"{name}() takes at most {len(params)} positional arguments here")
    vals = dict(zip(params, a))
    for k, v in kw.items():
        if k not in allowed:
            raise ValueError(f"{name}({k}=) is not supported")
        vals[k] = v
    return vals


_YAML_SAFE_LOADER = {f'V::native({RT}::Native::ExtType("yaml.SafeLoader"))'}
_YAML_DUMPERS = {f'V::native({RT}::Native::ExtType("yaml.{n}"))': n for n in ("SafeDumper", "Dumper")}
_YAML_DUMP_KW = ["default_style", "default_flow_style", "canonical", "indent", "width", "allow_unicode", "line_break",
                 "encoding", "explicit_start", "explicit_end", "version", "tags", "sort_keys"]


def _yaml_load(name, a, kw, loader):
    """`yaml.safe_load(stream)` / `yaml.load(stream, Loader=yaml.SafeLoader)`: PyYAML's pure-Python SafeLoader
    (dynrt/pyyaml.rs); the other loaders construct Python objects (and the C ones word their errors otherwise)."""
    params = ["stream", "Loader"] if loader else ["stream"]
    v = _bind(name, a, kw, params, set(params))
    if "stream" not in v:
        raise ValueError(f"{name}() needs the stream")
    if loader and v.get("Loader") not in _YAML_SAFE_LOADER:
        raise ValueError("yaml.load() is supported with Loader=yaml.SafeLoader only (Loader, FullLoader, UnsafeLoader "
                         "and the libyaml CLoader/CSafeLoader are not: use yaml.safe_load())")
    return f"{RT}::pyyaml::safe_load(&{v['stream']})"


def _yaml_dump(name, a, kw, full, all_):
    """`yaml.safe_dump(data, stream=None, **kw)`, `yaml.dump(..., Dumper=yaml.Dumper|yaml.SafeDumper)` and their
    `_all` forms (dynrt/yamlemit.rs); yaml.dump's Representer is ported for the builtin types only."""
    first = "documents" if all_ else "data"
    params = [first, "stream"] + (["Dumper"] if full else [])
    if all_ and full:
        params += _YAML_DUMP_KW
    v = _bind(name, a, kw, params, set(params) | set(_YAML_DUMP_KW))
    if first not in v:
        raise ValueError(f"{name}() needs the {first}")
    if full and "Dumper" in v:
        d = _YAML_DUMPERS.get(v["Dumper"])
        if d is None:
            raise ValueError(f"{name}() is supported with Dumper=yaml.Dumper or yaml.SafeDumper only")
        full = d == "Dumper"
    opts = {k: x for k, x in v.items() if k in _YAML_DUMP_KW}
    stream = v.get("stream", "V::None")
    return (f"{RT}::yamlemit::dump({'true' if full else 'false'}, {'true' if all_ else 'false'}, &{v[first]}, "
            f"&{stream}, &{_kwvec(opts)})")


# Starlette's signatures (1.x): arguments bound by name, so the runtime never reads a positional at a wrong index;
# those the runtime does not implement are refused here (`background=` was dropped when given positionally)
_RESP_PARAMS = {
    "RedirectResponse": ["url", "status_code", "headers", "background"],
    "FileResponse": ["path", "status_code", "headers", "media_type", "background", "filename", "stat_result", "method",
                     "content_disposition_type"],
}
_RESP_UNSUPPORTED = {"background", "stat_result", "method"}


def _resp(kind):
    params = _RESP_PARAMS.get(kind, ["content", "status_code", "headers", "media_type", "background"])

    def build(a, kw):
        v = _bind(kind, a, kw, params, set(params) - _RESP_UNSUPPORTED)
        # a literal None (`FileResponse(p, 200, None, None, None, "a.txt")`) is the default
        v = {k: x for k, x in v.items() if not (k in _RESP_UNSUPPORTED and x == "V::None")}
        bad = sorted(set(v) & _RESP_UNSUPPORTED)
        if bad:
            raise ValueError(f"{kind}({bad[0]}=) is not supported (a background task: return it from the endpoint "
                             "or use BackgroundTasks)" if bad[0] == "background" else f"{kind}({bad[0]}=) is not supported")
        return f"{RT}::resp::new(\"{kind}\", &[], &{_kwvec(v)})"
    return build


# email.mime constructors: (named parameters, those implemented, whether other keywords are `**_params` of the
# Content-Type); `policy=`, `boundary=`, `_subparts=`, `_encoder=` are refused, never ignored
_MIME = {
    "MIMEMultipart": (["_subtype", "boundary", "_subparts"], {"_subtype"}, True),
    "MIMEText": (["_text", "_subtype", "_charset"], {"_text", "_subtype", "_charset"}, False),
    "MIMEApplication": (["_data", "_subtype", "_encoder"], {"_data", "_subtype"}, True),
    "MIMEBase": (["_maintype", "_subtype"], {"_maintype", "_subtype"}, True),
}


def _mime(cls):
    params, allowed, extra = _MIME[cls]

    def build(a, kw):
        named = {k: v for k, v in kw.items() if k in params or not extra or k == "policy"}
        v = _bind(cls, a, named, params, allowed)
        bad = sorted(set(v) - allowed)
        if bad:
            raise ValueError(f"{cls}({bad[0]}=) is not supported")
        rest = {k: x for k, x in kw.items() if k not in named}
        return f"{RT}::mail::mime_new(\"{cls}\", &[], &{_kwvec({**v, **rest})})"
    return build


def _its_serializer(cls):
    def build(a, kw):
        v = _bind(cls, a, kw, ["secret_key", "salt"], {"secret_key", "salt"})
        salt = f"Some(&{v['salt']})" if "salt" in v else "None"
        return f"{RT}::itsd::new(&{v['secret_key']}, {salt}, {'true' if cls.endswith('TimedSerializer') else 'false'})"
    return build


# dns.resolver.resolve(): the keywords the runtime takes (source=, source_port= are refused)
DNS_RESOLVE_KW = {"rdtype", "rdclass", "tcp", "raise_on_no_answer", "lifetime", "search"}


def _dns_call(name, a, kw, most, allowed, code):
    """A constructor or function of dnspython/email: its keywords and positional arguments checked here"""
    for k in kw:
        if k not in allowed:
            raise ValueError(f"{name}({k}=) is not supported (only {', '.join(sorted(allowed))})")
    if len(a) > most:
        raise ValueError(f"{name}() takes at most {most} positional argument{'s' if most != 1 else ''} here")
    return code


def _mimetypes(name, params, kw_only=()):
    """`mimetypes.guess_type(url, strict=True)` & co, on the database `mimetypes.init()` builds (dynrt/resp.rs)"""
    def build(a, kw):
        v = _bind(f"mimetypes.{name}", a, {k: x for k, x in kw.items() if k not in kw_only}, params, set(params))
        if params[0] not in v:
            raise ValueError(f"mimetypes.{name}() needs its {params[0]} argument")
        v.update({k: x for k, x in kw.items() if k in kw_only})
        return f"{RT}::resp::mimetypes(\"{name}\", &vec![{v[params[0]]}], &{_kwvec({k: v[k] for k in ('strict',) if k in v})})"
    return build


def _getenv(name, a, kw):
    v = _bind(name, a, kw, ["key", "default"], {"key", "default"} if name == "os.getenv" else set())
    d = f"Some(&{v['default']})" if "default" in v else "None"
    return f"{RT}::libs::getenv(&{v['key']}, {d})"


def _jwt_encode(a, kw):
    v = _bind("jwt.encode", a, kw, ["claims", "key", "algorithm"], {"claims", "key", "algorithm"})
    alg = f"Some(&{v['algorithm']})" if "algorithm" in v else "None"
    return f"{RT}::jose::encode(&{v['claims']}, &{v['key']}, {alg})"


def _jwt_decode(a, kw):
    names = ["token", "key", "algorithms", "options", "audience", "issuer", "subject"]
    v = _bind("jwt.decode", a, kw, names, set(names))
    algs = f"Some(&{v['algorithms']})" if "algorithms" in v else "None"
    rest = ", ".join(f"&{v[n]}" if n in v else "&V::None" for n in ("options", "audience", "issuer", "subject"))
    return f"{RT}::jose::decode_with(&{v['token']}, &{v['key']}, {algs}, {rest})"


def _pyjwt_encode(a, kw):
    names = ["payload", "key", "algorithm", "headers", "json_encoder", "sort_headers"]
    v = _bind("jwt.encode", a, kw, names, set(names) - {"json_encoder"})
    if "json_encoder" in v:
        raise ValueError("json_encoder is not supported")
    alg = f"Some(&{v['algorithm']})" if "algorithm" in v else "None"
    return (f"{RT}::pyjwt::encode(&{v['payload']}, &{v['key']}, {alg}, &{v.get('headers', 'V::None')}, "
            f"&{v.get('sort_headers', 'V::Bool(true)')})")


def _pyjwt_decode(fn, order):
    def tmpl(a, kw):
        v = _bind(f"jwt.{fn}", a, kw, order, set(order) - {"verify", "detached_payload"})
        for k in ("verify", "detached_payload"):
            if k in v:
                raise ValueError(f"{k} is not supported")
        args = ", ".join(f"&{v.get(n, d)}" for n, d in (("jwt", None), ("key", 'V::str("")'), ("algorithms", "V::None"),
                                                         ("options", "V::None"), ("audience", "V::None"), ("issuer", "V::None"),
                                                         ("subject", "V::None"), ("leeway", "V::Int(0)")))
        return f"{RT}::pyjwt::{fn}({args})"
    return tmpl


# PyJWT algorithms that need `cryptography`'s RSA/EC/EdDSA keys (not reproduced; HS*, none and unknown names are)
PYJWT_ASYMMETRIC = {"RS256", "RS384", "RS512", "ES256", "ES256K", "ES384", "ES521", "ES512", "PS256", "PS384", "PS512", "EdDSA"}


def pyjwt_static(name, node):
    """Asymmetric algorithms written as literals in a PyJWT call: refused at transpile time."""
    import ast

    def lits(e):
        if isinstance(e, ast.Constant) and isinstance(e.value, str):
            return [e.value]
        if isinstance(e, (ast.List, ast.Tuple, ast.Set)):
            return [x.value for x in e.elts if isinstance(x, ast.Constant) and isinstance(x.value, str)]
        return []
    found = []
    for k in node.keywords:
        if k.arg in ("algorithm", "algorithms"):
            found += lits(k.value)
        if k.arg == "headers" and isinstance(k.value, ast.Dict):
            found += [x for kk, vv in zip(k.value.keys, k.value.values)
                      if isinstance(kk, ast.Constant) and kk.value == "alg" for x in lits(vv)]
    if len(node.args) >= 3:
        found += lits(node.args[2])
    bad = [x for x in found if x in PYJWT_ASYMMETRIC]
    if bad:
        raise ValueError(f"algorithm {bad[0]!r} is not supported (HMAC HS256/HS384/HS512 and 'none' only: "
                         "RSA/EC/EdDSA keys are not reproduced)")


def _relativedelta(a, kw):
    allowed = {"years", "months", "weeks", "days", "hours", "minutes", "seconds", "microseconds"}
    if a:
        raise ValueError("relativedelta(dt1, dt2) is not supported (relative keyword arguments only)")
    for k in kw:
        if k not in allowed:
            raise ValueError(f"relativedelta({k}=) is not supported (only {', '.join(sorted(allowed))})")
    return f"{RT}::reldelta::new(&{_argv(a)}, &{_kwvec(kw)})"


# croniter 6.2 (dynrt/croniter.rs): the class (`from croniter import croniter`, or its module path), its
# classmethods, croniter_range; parameters as in croniter.py (checked statically by croniter_static)
_CRON_CLASSES = ("croniter.croniter", "croniter.croniter.croniter")
_CRON_MODULES = ("croniter", "croniter.croniter")
_CRON_SIGS = {
    "": ("expr_format", "start_time", "ret_type", "day_or", "max_years_between_matches", "is_prev", "hash_id",
         "implement_cron_bug", "second_at_beginning", "expand_from_start_time"),
    "is_valid": ("expression", "hash_id", "encoding", "second_at_beginning", "strict", "strict_year"),
    "match": ("cron_expression", "testdate", "day_or", "second_at_beginning", "precision_in_seconds"),
    "match_range": ("cron_expression", "from_datetime", "to_datetime", "day_or", "second_at_beginning", "precision_in_seconds"),
    "expand": ("expr_format", "hash_id", "second_at_beginning", "from_timestamp", "strict", "strict_year"),
    "croniter_range": ("start", "stop", "expr_format", "ret_type", "day_or", "exclude_ends", "_croniter",
                       "second_at_beginning", "expand_from_start_time"),
    "datetime_to_timestamp": ("d",),
}
_CRON_FN = {"": "new", "is_valid": "is_valid", "match": "match_fn", "match_range": "match_range", "expand": "expand_fn",
            "croniter_range": "croniter_range", "datetime_to_timestamp": "datetime_to_timestamp"}
_CRON_RANDOM = re.compile(r"^r(\(\d+-\d+\))?(/\d+)?$")


def _cron_kind(name: str) -> str | None:
    for c in _CRON_CLASSES:
        if name == c:
            return ""
        if name.startswith(c + ".") and name[len(c) + 1:] in ("is_valid", "match", "match_range", "expand", "datetime_to_timestamp"):
            return name[len(c) + 1:]
    for m in _CRON_MODULES:
        if name in (f"{m}.croniter_range", f"{m}.datetime_to_timestamp"):
            return name[len(m) + 1:]
    return None


def croniter_static(name, node):
    """What croniter's runtime cannot reproduce, refused at transpile time: an `R` (random) field in a literal
    expression, `croniter_range(_croniter=)`, `expand(from_timestamp=)`, an `is_valid(encoding=)` other than
    UTF-8; arguments croniter does not take."""
    import ast

    kind = _cron_kind(name)
    if kind is None:
        return
    sig = _CRON_SIGS[kind]
    if len(node.args) > len(sig):
        raise ValueError(f"takes at most {len(sig)} positional arguments")
    given = {sig[i]: a for i, a in enumerate(node.args)}
    for k in node.keywords:
        if k.arg is None or k.arg not in sig:
            raise ValueError(f"unexpected keyword argument {k.arg!r}")
        given[k.arg] = k.value
    expr = given.get({"is_valid": "expression", "match": "cron_expression", "match_range": "cron_expression"}.get(kind, "expr_format"))
    if isinstance(expr, ast.Constant) and isinstance(expr.value, str):
        if any(_CRON_RANDOM.match(f) for f in expr.value.lower().split()):
            raise ValueError(f"R (random) fields in {expr.value!r} are not supported: Python draws a random "
                             "number, its output cannot be reproduced")
    for k, why in (("_croniter", "a croniter subclass"), ("from_timestamp", "from_timestamp")):
        v = given.get(k)
        if v is not None and not (isinstance(v, ast.Constant) and v.value is None):
            raise ValueError(f"{k}= is not supported ({why})")
    enc = given.get("encoding")
    if enc is not None and not (isinstance(enc, ast.Constant) and isinstance(enc.value, str)
                                and enc.value.lower().replace("-", "").replace("_", "") == "utf8"):
        raise ValueError("encoding= other than a literal UTF-8 is not supported")


CALLS = {
    **{c + ("." + k if k else ""): (lambda f: lambda a, kw: f"{RT}::croniter::{f}(&{_argv(a)}, &{_kwvec(kw)})")(_CRON_FN[k])
       for c in _CRON_CLASSES for k in ("", "is_valid", "match", "match_range", "expand", "datetime_to_timestamp")},
    **{f"{m}.{k}": (lambda f: lambda a, kw: f"{RT}::croniter::{f}(&{_argv(a)}, &{_kwvec(kw)})")(_CRON_FN[k])
       for m in _CRON_MODULES for k in ("croniter_range", "datetime_to_timestamp")},
    # python-dateutil 2.9 (dynrt/reldelta.rs)
    "dateutil.relativedelta.relativedelta": _relativedelta,
    # xmltodict 1.0 (dynrt/xmld.rs): default options, namespaces; dict_constructor= has no effect (dicts)
    "xmltodict.parse": lambda a, kw: (_bind("xmltodict.parse", a, kw, ["xml_input"], {"xml_input", "process_namespaces", "namespaces", "dict_constructor"})
                                       and f"{RT}::xmld::parse(&{_argv(a)}, &{_kwvec(kw)})"),
    # outgoing HTTP: httpx 0.28, aiohttp 3.14 (the version goes in the User-Agent)
    "httpx.AsyncClient": lambda a, kw: f"{RT}::http::client(\"httpx\", \"{LIB_VERSIONS.get('httpx', '0.28.1')}\", &{_argv(a)}, &{_kwvec(kw)})",
    "aiohttp.ClientSession": lambda a, kw: f"{RT}::http::client(\"aiohttp\", \"{LIB_VERSIONS.get('aiohttp', '3.14.3')}\", &{_argv(a)}, &{_kwvec(kw)})",
    "httpx.BasicAuth": lambda a, kw: f"{RT}::http::basic_auth(&{_argv(a)}, &{_kwvec(kw)})",
    "httpx.Timeout": lambda a, kw: f"{RT}::http::timeout(\"httpx\", &{_argv(a)}, &{_kwvec(kw)})",
    "aiohttp.ClientTimeout": lambda a, kw: f"{RT}::http::timeout(\"aiohttp\", &{_argv(a)}, &{_kwvec(kw)})",
    "httpx.URL": lambda a, kw: f"{RT}::http::url(&{_argv(a)})",
    "yarl.URL": lambda a, kw: f"{RT}::http::yarl_url(&{_argv(a)}, &{_kwvec(kw)})",
    # google-auth 2.49 (ID tokens)
    "google.oauth2.id_token.verify_oauth2_token": lambda a, kw: f"{RT}::google::verify(true, &{_argv(a)}, &{_kwvec(kw)}).await",
    "google.oauth2.id_token.verify_token": lambda a, kw: f"{RT}::google::verify(false, &{_argv(a)}, &{_kwvec(kw)}).await",
    "google.auth.transport.requests.Request": lambda a, kw: f"Ok::<V, Exc>(V::native({RT}::Native::Namespace(\"google.auth.transport.requests.Request\")))",
    # pywebpush 2.3 (aes128gcm, VAPID)
    "pywebpush.webpush": lambda a, kw: f"{RT}::webpush::webpush(&{_argv(a)}, &{_kwvec(kw)}).await",
    # psutil (live system values, psutil's formulas)
    **{f"psutil.{f}": (lambda f: lambda a, kw: f"{RT}::sysmon::call(\"{f}\", &{_argv(a)}, &{_kwvec(kw)}).await")(f)
       for f in ("cpu_percent", "virtual_memory", "disk_usage", "pids")},
    # alembic's Config (the ini file only)
    "alembic.config.Config": lambda a, kw: f"{RT}::ini::new(&{_argv(a)}, &{_kwvec(kw)})",
    # tenacity 9 (dynrt/tenacity.rs)
    "tenacity.retry": lambda a, kw: f"{RT}::tenacity::retry(&{_argv(a)}, &{_kwvec(kw)})",
    **{f"tenacity.{n}": (lambda n: lambda a, kw: f'{RT}::tenacity::make("{n}", &{_argv(a)}, &{_kwvec(kw)})')(n)
       for n in ("stop_after_attempt", "stop_after_delay", "stop_any", "stop_all", "wait_fixed", "wait_none", "wait_random",
                 "wait_exponential", "wait_exponential_jitter", "wait_incrementing", "wait_combine", "wait_chain",
                 "retry_if_exception_type", "retry_if_not_exception_type", "retry_if_exception", "retry_if_result",
                 "retry_any", "retry_all", "before_sleep_log")},
    # prometheus_client 0.26 (dynrt/prom.rs)
    **{f"prometheus_client.{n}": (lambda n: lambda a, kw: f'{RT}::prom::new_metric("{n}", {_argv(a)}, {_kwvec(kw)})')(n)
       for n in ("Counter", "Gauge", "Summary", "Histogram", "Info", "Enum")},
    "prometheus_client.CollectorRegistry": lambda a, kw: f"{RT}::prom::registry_new({_argv(a)}, {_kwvec(kw)})",
    "prometheus_client.generate_latest": lambda a, kw: f"{RT}::prom::generate_latest(cx, {_argv(a)}, {_kwvec(kw)}).await",
    **{f"prometheus_client.{n}": (lambda a, kw: f"{RT}::prom::start_http_server({_argv(a)}, {_kwvec(kw)})")
       for n in ("start_http_server", "start_wsgi_server")},
    "prometheus_client.openmetrics.exposition.generate_latest":
        lambda a, kw: f"{RT}::prom::generate_openmetrics(cx, {_argv(a)}, {_kwvec(kw)}).await",
    "prometheus_client.multiprocess.MultiProcessCollector": lambda a, kw: f"{RT}::prom::multiproc_new({_argv(a)}, {_kwvec(kw)})",
    "prometheus_client.multiprocess.mark_process_dead": lambda a, kw: f"{RT}::prom::mark_process_dead({_argv(a)}, {_kwvec(kw)})",
    "prometheus_client.disable_created_metrics": lambda a, kw: f"{RT}::prom::set_created(false)",
    "prometheus_client.enable_created_metrics": lambda a, kw: f"{RT}::prom::set_created(true)",
    # aio_pika 10 (dynrt/rmq.rs)
    **{n: (lambda robust: lambda a, kw: f"{RT}::rmq::connect(&{_argv(a)}, &{_kwvec(kw)}, {robust}).await")(
        "true" if n.endswith("robust") else "false") for n in ("aio_pika.connect_robust", "aio_pika.connect")},
    "aio_pika.Message": lambda a, kw: f"{RT}::rmq::message(&{_argv(a)}, &{_kwvec(kw)})",
    # redis.asyncio (dynrt/rds.rs)
    **{n: (lambda a, kw: f"{RT}::rds::from_url(&{_argv(a)}, &{_kwvec(kw)})") for n in ("redis.asyncio.from_url", "redis.asyncio.client.from_url")},
    **{n: (lambda a, kw: f"{RT}::rds::new(&{_argv(a)}, &{_kwvec(kw)})") for n in ("redis.asyncio.Redis", "redis.asyncio.client.Redis", "redis.asyncio.StrictRedis")},
    "redis.asyncio.retry.Retry": lambda a, kw: f"{RT}::rds::retry(&{_argv(a)}, &{_kwvec(kw)})",
    **{f"redis.backoff.{n}": (lambda a, kw: "{ " + "".join(f"let _ = {x}; " for x in list(a) + list(kw.values())) + f"Ok::<V, Exc>(V::native({RT}::Native::Namespace(\"backoff\"))) }}")
       for n in ("ExponentialBackoff", "ConstantBackoff", "NoBackoff", "EqualJitterBackoff", "FullJitterBackoff", "DecorrelatedJitterBackoff")},
    # pickle in CPython's format (dynrt/pickle.rs)
    "pickle.dumps": lambda a, kw: f"{RT}::pickle::dumps(&{_argv(a)}, &{_kwvec(kw)})",
    "pickle.loads": lambda a, kw: f"{RT}::pickle::loads(&{_argv(a)}, &{_kwvec(kw)})",
    # threading and asyncio event loops (dynrt/thread.rs)
    "threading.Lock": lambda a, kw: f"{RT}::thread::lock(false, &{_argv(a)})",
    "threading.RLock": lambda a, kw: f"{RT}::thread::lock(true, &{_argv(a)})",
    "threading.Event": lambda a, kw: f"{RT}::thread::event(&{_argv(a)})",
    "threading.Thread": lambda a, kw: f"{RT}::thread::thread_new(&{_argv(a)}, &{_kwvec(kw)})",
    "threading.get_ident": lambda a, kw: f"Ok::<V, Exc>(V::Int({RT}::thread::get_ident() as i64))",
    "asyncio.get_running_loop": lambda a, kw: f"{RT}::thread::running_loop()",
    "asyncio.get_event_loop": lambda a, kw: f"{RT}::thread::running_loop()",
    "asyncio.new_event_loop": lambda a, kw: f"{RT}::thread::new_loop()",
    "asyncio.run_coroutine_threadsafe": lambda a, kw: f"{RT}::thread::run_coroutine_threadsafe(cx, &{_argv(a)})",
    "asyncio.wrap_future": lambda a, kw: f"Ok::<V, Exc>({a[0]})",
    # typing / inspect over type values (dynrt/types.rs)
    "inspect.isclass": lambda a, kw: f"{RT}::types::isclass(&{_argv(a)})",
    "typing.get_args": lambda a, kw: f"{RT}::types::get_args(&{_argv(a)})",
    "typing.get_origin": lambda a, kw: f"{RT}::types::get_origin(&{_argv(a)})",
    "typing.get_type_hints": lambda a, kw: f"{RT}::types::get_type_hints(cx, &{_argv(a)}, &{_kwvec(kw)}).await",
    # functools / inspect over function objects
    "functools.wraps": lambda a, kw: f"{RT}::functools_wraps(&{_argv(a)}, &{_kwvec(kw)})",
    "inspect.iscoroutinefunction": lambda a, kw: f"{RT}::iscoroutinefunction(&{_argv(a)}, &{_kwvec(kw)})",
    # decimal
    "decimal.Decimal": lambda a, kw: f"{RT}::decimal::new(&{_argv(a)}, &{_kwvec(kw)})",
    # base64
    # the parameters the runtime reads, by function (3.15's wrapcol=, ignorechars=, canonical=... are refused;
    # padded= follows the project's Python at run time)
    **{f"base64.{f}": (lambda f, ps: lambda a, kw: (_bind(f, a, kw, ps, set(ps) | ({"padded"} if "decode" in f and "b16" not in f else set())),
                                                     f"{RT}::stdlib::base64(\"{f}\", &{_argv(a)}, &{_kwvec(kw)})")[1])(f, ps)
       for f, ps in (("b64encode", ["s", "altchars"]), ("b64decode", ["s", "altchars", "validate"]),
                     ("standard_b64encode", ["s"]), ("standard_b64decode", ["s"]), ("urlsafe_b64encode", ["s"]),
                     ("urlsafe_b64decode", ["s"]), ("b16encode", ["s"]), ("b16decode", ["s", "casefold"]),
                     ("b32encode", ["s"]), ("b32decode", ["s", "casefold", "map01"]))},
    # mimetypes (the module's functions; add_type, init, read and the tables are not: refused)
    "mimetypes.guess_type": _mimetypes("guess_type", ["url", "strict"]),
    "mimetypes.guess_file_type": lambda a, kw: (_since((3, 13), "mimetypes.guess_file_type()")
                                                or _mimetypes("guess_file_type", ["path"], ("strict",))(a, kw)),
    "mimetypes.guess_extension": _mimetypes("guess_extension", ["type", "strict"]),
    "mimetypes.guess_all_extensions": _mimetypes("guess_all_extensions", ["type", "strict"]),
    # urllib.parse
    **{f"urllib.parse.{f}": (lambda f: lambda a, kw: _no_kw(f, kw, ("missing_as_none",))
                             or f"{RT}::stdlib::urllib(\"{f}\", &{_argv(a)}, &{_kwvec(kw)})")(f)
       for f in ("quote", "quote_plus", "unquote", "unquote_plus", "urlencode", "urlparse", "urlsplit")},
    # bcrypt 5.0, pyotp 2.9
    **{f"bcrypt.{f}": (lambda f: lambda a, kw: f"{RT}::auth::call(\"{f}\", &{_argv(a)}, &{_kwvec(kw)})")(f)
       for f in ("gensalt", "hashpw", "checkpw")},
    # pwdlib 0.2+: the recommended Argon2 hasher (hash, verify, verify_and_update)
    "pwdlib.PasswordHash.recommended": lambda a, kw: f"{RT}::auth::pwd_recommended()" if not a and not kw else
        (_ for _ in ()).throw(ValueError("PasswordHash.recommended() takes no arguments")),
    "pyotp.random_base32": lambda a, kw: f"{RT}::auth::random_base32(&{_argv(a)}, &{_kwvec(kw)})",
    "pyotp.TOTP": lambda a, kw: f"{RT}::auth::totp_new(&{_argv(a)}, &{_kwvec(kw)})",
    "pyotp.totp.TOTP": lambda a, kw: f"{RT}::auth::totp_new(&{_argv(a)}, &{_kwvec(kw)})",
    # datetime
    "datetime.datetime.now": _dt_now,
    "datetime.datetime.utcnow": lambda a, kw: f"{RT}::methods::now(&V::None).map(|_| V::DateTime({RT}::dt::DateTime::naive(chrono::Utc::now().naive_utc())))",
    "datetime.datetime": lambda a, kw: f"{RT}::libs::datetime_new(&{_argv(a)}, &{_kwvec(kw)})",
    "datetime.datetime.fromisoformat": lambda a, kw: f"{RT}::libs::fromisoformat_dt(&{a[0]})",
    "datetime.datetime.strptime": lambda a, kw: f"{RT}::libs::strptime(&{a[0]}, &{a[1]})",
    "datetime.datetime.fromtimestamp": lambda a, kw: f"{RT}::libs::fromtimestamp(&{_bind('fromtimestamp', a, kw, ['timestamp', 'tz'], {'tz'})['timestamp']}, {('Some(&' + _bind('fromtimestamp', a, kw, ['timestamp', 'tz'], {'tz'})['tz'] + ')') if (len(a) > 1 or 'tz' in kw) else 'None'})",
    "secrets.choice": lambda a, kw: f"{RT}::libs::choice(&{a[0]})",
    **{f"random.{n}": (lambda n: lambda a, kw: f"{RT}::libs::random(\"{n}\", &{_argv(a)})")(n)
       for n in ("random", "uniform", "randint", "sample", "shuffle", "choice")},
    "datetime.date": lambda a, kw: f"{RT}::libs::date_new(&{_argv(a)}, &{_kwvec(kw)})",
    "datetime.time": lambda a, kw: f"{RT}::libs::time_new(&{_argv(a)}, &{_kwvec(kw)})",
    "datetime.datetime.combine": lambda a, kw: f"{RT}::libs::combine(&{_argv(a)}, &{_kwvec(kw)})",
    "datetime.date.today": lambda a, kw: f"{RT}::methods::today()",
    # datetime.today(): local naive time, like datetime.now()
    "datetime.datetime.today": lambda a, kw: (f"{RT}::methods::now(&V::None)" if not a and not kw else
                                              (_ for _ in ()).throw(ValueError("datetime.today() takes no arguments"))),
    "datetime.date.fromisoformat": lambda a, kw: f"{RT}::libs::fromisoformat_date(&{a[0]})",
    "datetime.timedelta": _timedelta,
    "datetime.timezone": lambda a, kw: f"{RT}::libs::timezone_new(&{a[0]})",
    "zoneinfo.ZoneInfo": lambda a, kw: f"{RT}::libs::zoneinfo(&{a[0]})",
    # hashing / secrets / statistics / json
    "hashlib.sha256": lambda a, kw: f"{RT}::libs::hash_new(\"sha256\", {('Some(&' + a[0] + ')') if a else 'None'})",
    "hashlib.sha1": lambda a, kw: f"{RT}::libs::hash_new(\"sha1\", {('Some(&' + a[0] + ')') if a else 'None'})",
    "hashlib.md5": lambda a, kw: f"{RT}::libs::hash_new(\"md5\", {('Some(&' + a[0] + ')') if a else 'None'})",
    "secrets.token_urlsafe": lambda a, kw: f"{RT}::libs::token_urlsafe(&{a[0] if a else kw.get('nbytes', 'V::None')})",
    "secrets.token_hex": lambda a, kw: f"{RT}::libs::token_hex(&{a[0] if a else kw.get('nbytes', 'V::None')})",
    "secrets.compare_digest": lambda a, kw: f"{RT}::libs::compare_digest(&{a[0]}, &{a[1]})",
    "statistics.median": lambda a, kw: f"{RT}::libs::median(&{a[0]})",
    "json.dumps": _json_dumps,
    # FastAPI's encoder: what a response without response_model goes through (`pyd::jsonable`)
    "fastapi.encoders.jsonable_encoder": _jsonable_encoder,
    "json.loads": lambda a, kw: f"{RT}::libs::json_loads(&{a[0]})",
    # orjson 3.11-3.13 (dynrt/orj.rs); orjson.Fragment is refused (not listed)
    "orjson.loads": _one_pos("orjson.loads", lambda x: f"{RT}::orj::loads(&{x})"),
    "orjson.dumps": lambda a, kw: _orjson_dumps(a, kw),
    # Unidecode 1.3-1.4 (dynrt/unidec.rs, tables of tools/gen_unidecode.py)
    **{f"unidecode.{f}": (lambda f: lambda a, kw: _unidecode(f, a, kw))(f)
       for f in ("unidecode", "unidecode_expect_ascii", "unidecode_expect_nonascii")},
    # python-jose (HMAC only; other options refused)
    "jose.jwt.encode": _jwt_encode,
    "jose.jwt.decode": _jwt_decode,
    # PyJWT (HMAC and none; asymmetric algorithms refused)
    "jwt.encode": _pyjwt_encode,
    "jwt.decode": _pyjwt_decode("decode", ["jwt", "key", "algorithms", "options", "verify", "detached_payload",
                                           "audience", "subject", "issuer", "leeway"]),
    "jwt.decode_complete": _pyjwt_decode("decode_complete", ["jwt", "key", "algorithms", "options", "verify",
                                                             "detached_payload", "audience", "issuer", "subject", "leeway"]),
    "jwt.get_unverified_header": lambda a, kw: f"{RT}::pyjwt::get_unverified_header(&{_bind('jwt.get_unverified_header', a, kw, ['jwt'], {'jwt'})['jwt']})",
    # itsdangerous (URLSafeTimedSerializer and URLSafeSerializer with their default signer and serializer only)
    "itsdangerous.URLSafeTimedSerializer": _its_serializer("URLSafeTimedSerializer"),
    "itsdangerous.url_safe.URLSafeTimedSerializer": _its_serializer("URLSafeTimedSerializer"),
    "itsdangerous.URLSafeSerializer": _its_serializer("URLSafeSerializer"),
    "itsdangerous.url_safe.URLSafeSerializer": _its_serializer("URLSafeSerializer"),
    # cryptography (Fernet only)
    "cryptography.hazmat.primitives.asymmetric.rsa.generate_private_key": lambda a, kw: (
        f"{RT}::crypto::generate_private_key(&{_argv(a)}, &{_kwvec({k: v for k, v in _bind('generate_private_key', a, kw, ['public_exponent', 'key_size', 'backend'], {'public_exponent', 'key_size', 'backend'}).items() if k != 'backend' and k in kw})}).await"),
    "cryptography.hazmat.primitives.serialization.NoEncryption": _noargs("NoEncryption", 'Ok::<V, Exc>(V::str("NoEncryption"))'),
    "cryptography.hazmat.backends.default_backend": _noargs("default_backend", "Ok::<V, Exc>(V::None)"),
    "cryptography.fernet.Fernet.generate_key": lambda a, kw: f"{RT}::fernet::generate_key()",
    "cryptography.fernet.Fernet": lambda a, kw: f"{RT}::fernet::new(&{_bind('Fernet', a, kw, ['key'], {'key'})['key']})",
    # e-mail: jinja2 templates, email.mime, aiosmtplib
    "jinja2.Environment": lambda a, kw: f"{RT}::mail::environment(&{_kwvec(kw)})" if not a else (_ for _ in ()).throw(ValueError("pass keyword arguments")),
    **{f"{m}.templating.Jinja2Templates": lambda a, kw: f"{RT}::mail::templates(&{_argv(a)}, &{_kwvec(kw)})" for m in ("fastapi", "starlette")},
    "jinja2.FileSystemLoader": lambda a, kw: f"{RT}::mail::fs_loader(&{_bind('FileSystemLoader', a, kw, ['searchpath'], {'searchpath'})['searchpath']})",
    "jinja2.select_autoescape": lambda a, kw: f"{RT}::mail::select_autoescape({('Some(&' + _bind('select_autoescape', a, kw, ['enabled_extensions'], {'enabled_extensions'})['enabled_extensions'] + ')') if (a or kw) else 'None'})",
    **{f"email.mime.{m}.{c}": _mime(c)
       for m, c in (("multipart", "MIMEMultipart"), ("text", "MIMEText"), ("application", "MIMEApplication"), ("base", "MIMEBase"))},
    "email.encoders.encode_base64": lambda a, kw: f"{RT}::mail::encode_base64(&{a[0]})",
    # dnspython 2.8's stub resolver (dynrt/dnsres.rs)
    "dns.resolver.Resolver": lambda a, kw: _dns_call("Resolver", a, kw, 2, {"filename", "configure"},
                                                     f"{RT}::dnsres::new(&{_argv(a)}, &{_kwvec(kw)})"),
    "dns.resolver.resolve": lambda a, kw: _dns_call("resolve", a, kw, 9, DNS_RESOLVE_KW | {"qname"},
                                                    f"{RT}::dnsres::resolve_default({_argv(a)}, {_kwvec(kw)}).await"),
    "dns.resolver.get_default_resolver": _noargs("get_default_resolver", f"{RT}::dnsres::get_default_resolver()"),
    "dns.resolver.reset_default_resolver": _noargs("reset_default_resolver", f"{RT}::dnsres::reset_default_resolver()"),
    "dns.name.from_text": lambda a, kw: f"{RT}::dnsres::name_from_text(&{_argv(a)}, &{_kwvec(kw)})",
    # email.message.EmailMessage under the modern policies (dynrt/emailmsg.rs, emailhdr.rs)
    "email.message.EmailMessage": lambda a, kw: _dns_call("EmailMessage", a, kw, 0, {"policy"},
                                                          f"{RT}::emailmsg::new(&{_argv(a)}, &{_kwvec(kw)})"),
    # positional-only in CPython: keywords are a TypeError there, refused here
    "unicodedata.normalize": lambda a, kw: f"{RT}::stdlib::unicodedata_normalize(self::UCD_UNASSIGNED, &{a[0]}, &{a[1]})" if len(a) == 2 and not kw else (_ for _ in ()).throw(ValueError("normalize(form, unistr): two positional arguments")),
    "unicodedata.combining": lambda a, kw: f"{RT}::stdlib::unicodedata_combining(self::UCD_UNASSIGNED, &{a[0]})" if len(a) == 1 and not kw else (_ for _ in ()).throw(ValueError("combining(chr): one positional argument")),
    "collections.deque": lambda a, kw: f"{RT}::deque::new(&{_argv(a)}, &{_kwvec(kw)})",
    "socket.getaddrinfo": lambda a, kw: f"{RT}::net::getaddrinfo(&{_argv(a)}, &{_kwvec(kw)})",
    "collections.Counter": lambda a, kw: f"{RT}::ops::counter(&{_argv(a)}, &{_kwvec(kw)})",
    "zipfile.ZipFile": lambda a, kw: _zipfile(a, kw),
    "socket.create_connection": lambda a, kw: (
        f"{RT}::net::create_connection(&{_argv(a)}, &{_kwvec(_checked('socket.create_connection', a, kw, ['address', 'timeout']))}).await"),
    # uvicorn runs on uvloop when the project installs it (`uvicorn[standard]`): its error messages differ
    "asyncio.open_connection": lambda a, kw: (
        f"{RT}::net::open_connection(&{_argv(a)}, &{_kwvec(_checked('asyncio.open_connection', a, kw, ['host', 'port']))}, "
        f"{str('uvloop' in LIB_VERSIONS).lower()}).await"),
    "ipaddress.ip_address": lambda a, kw: f"{RT}::net::ip_address(&{_argv(a)})",
    "ipaddress.ip_network": lambda a, kw: f"{RT}::net::ip_network(&{_argv(a)}, &{_kwvec(kw)})",
    "nh3.clean": lambda a, kw: f"{RT}::nh3::clean(&{_argv(a)}, &{_kwvec(kw)})",
    "nh3.clean_text": lambda a, kw: f"{RT}::nh3::clean_text(&{_argv(a)}, &{_kwvec(kw)})",
    "nh3.escape": lambda a, kw: f"{RT}::nh3::clean_text(&{_argv(a)}, &{_kwvec(kw)})",
    "nh3.is_html": lambda a, kw: f"{RT}::nh3::is_html(&{_argv(a)})",
    "string.Template": lambda a, kw: f"{RT}::stdlib::template_new(&{_argv(a)}, &{_kwvec(kw)})",
    "html.escape": lambda a, kw: f"{RT}::stdlib::html_escape(&{_argv(a)}, &{_kwvec(kw)})",
    "yaml.safe_load": lambda a, kw: _yaml_load("yaml.safe_load", a, kw, False),
    "yaml.load": lambda a, kw: _yaml_load("yaml.load", a, kw, True),
    "yaml.safe_dump": lambda a, kw: _yaml_dump("yaml.safe_dump", a, kw, False, False),
    "yaml.dump": lambda a, kw: _yaml_dump("yaml.dump", a, kw, True, False),
    "yaml.safe_dump_all": lambda a, kw: _yaml_dump("yaml.safe_dump_all", a, kw, False, True),
    "yaml.dump_all": lambda a, kw: _yaml_dump("yaml.dump_all", a, kw, True, True),
    "html.unescape": lambda a, kw: f"{RT}::stdlib::html_unescape(&{_argv(a)}, &{_kwvec(kw)})",
    # icalendar 7.0: components built empty, then add()/add_component()/to_ical() (dynrt/ical.rs)
    **{f"icalendar.{k}": (lambda k: lambda a, kw: f'{RT}::ical::new("{k}")'
                          if not a and not kw else (_ for _ in ()).throw(ValueError(f"icalendar.{k}() with arguments")))(k)
       for k in ("Calendar", "Event", "Alarm")},
    "email.utils.formataddr": lambda a, kw: f"{RT}::mail::formataddr(&{a[0]})",
    "email.utils.formatdate": lambda a, kw: f"{RT}::mail::formatdate(&{_argv(a)}, &{_kwvec(kw)})",
    "email.utils.make_msgid": lambda a, kw: f"{RT}::mail::make_msgid({('Some(&' + _bind('make_msgid', a, kw, ['idstring', 'domain'], {'domain'})['domain'] + ')') if kw.get('domain') else 'None'})",
    "aiosmtplib.send": lambda a, kw: f"{RT}::mail::smtp_send({_argv(a)}, {_kwvec(kw)}).await",
    # Starlette responses returned by an endpoint
    "fastapi.responses.Response": _resp("Response"),
    "starlette.responses.Response": _resp("Response"),
    "fastapi.responses.JSONResponse": _resp("JSONResponse"),
    "fastapi.responses.ORJSONResponse": _resp("ORJSONResponse"),
    "starlette.responses.JSONResponse": _resp("JSONResponse"),
    "fastapi.responses.PlainTextResponse": _resp("PlainTextResponse"),
    "starlette.responses.PlainTextResponse": _resp("PlainTextResponse"),
    "fastapi.responses.HTMLResponse": _resp("HTMLResponse"),
    "starlette.responses.HTMLResponse": _resp("HTMLResponse"),
    "fastapi.responses.RedirectResponse": _resp("RedirectResponse"),
    "starlette.responses.RedirectResponse": _resp("RedirectResponse"),
    "fastapi.responses.FileResponse": _resp("FileResponse"),
    "starlette.responses.FileResponse": _resp("FileResponse"),
    "fastapi.Response": _resp("Response"),
    # re
    "re.compile": lambda a, kw: f"{RT}::stdlib::compile(&{_bind('compile', a, kw, ['pattern', 'flags'], {'pattern', 'flags'})['pattern']}, "
                                f"{('Some(&' + _bind('compile', a, kw, ['pattern', 'flags'], {'pattern', 'flags'})['flags'] + ')') if (len(a) > 1 or 'flags' in kw) else 'None'})",
    **{f"re.{n}": (lambda n: lambda a, kw: f"{RT}::stdlib::re_call(cx, \"{n}\", {_argv(a)}, {_kwvec(kw)}).await")(n)
       for n in ("search", "match", "fullmatch", "findall", "finditer", "sub", "subn", "split")},
    "re.prefixmatch": lambda a, kw: _since((3, 15), "re.prefixmatch()") or f"{RT}::stdlib::re_call(cx, \"prefixmatch\", {_argv(a)}, {_kwvec(kw)}).await",
    "re.escape": lambda a, kw: f"{RT}::stdlib::escape(&{a[0]})",
    # io.StringIO, csv
    "io.StringIO": lambda a, kw: f"{RT}::stdlib::stringio_new({('Some(&' + _bind('StringIO', a, kw, ['initial_value'], {'initial_value'})['initial_value'] + ')') if (a or kw) else 'None'})",
    "csv.writer": lambda a, kw: f"{RT}::stdlib::writer_new(&{_argv(a)}, &{_kwvec(kw)}, false)",
    "csv.DictWriter": lambda a, kw: f"{RT}::stdlib::writer_new(&{_argv(a)}, &{_kwvec(kw)}, true)",
    "csv.reader": lambda a, kw: f"{RT}::stdlib::reader(&{_argv(a)}, &{_kwvec(kw)})",
    "csv.DictReader": lambda a, kw: f"{RT}::stdlib::dict_reader(&{_argv(a)}, &{_kwvec(kw)})",
    "csv.Sniffer": lambda a, kw: f"Ok::<V, Exc>(V::native({RT}::Native::SnifferObj))",
    # operator's comparisons (a SQLAlchemy Comparator's `operate(op, other)` gets them): `a OP b`, project methods
    # and SQL expressions included
    **{f"operator.{n}": (lambda n: lambda a, kw: _op2(n, a, kw))(n) for n in ("eq", "ne", "lt", "le", "gt", "ge")},
    # math, time, os.path, hmac
    **{f"math.{n}": (lambda n: lambda a, kw: f"{RT}::stdlib::math(\"{n}\", &{_argv(a)})")(n)
       for n in ("ceil", "floor", "trunc", "pow", "sqrt", "log", "log10", "exp", "fabs", "isclose", "isnan", "isinf",
                 "radians", "degrees", "sin", "cos", "tan", "asin", "acos", "atan", "atan2", "hypot", "log2", "isfinite",
                 "copysign", "fmod")},
    **{f"time.{n}": (lambda n: lambda a, kw: f"{RT}::stdlib::time_now(\"{n}\")")(n) for n in ("time", "monotonic", "perf_counter")},
    # sys.settrace and the threading variants (dynrt/trace.rs; the project's functions report events)
    **{n: (lambda a, kw: f"{RT}::trace::settrace(&{_argv(a)})") for n in ("sys.settrace", "threading.settrace", "threading.settrace_all_threads")},
    "sys.gettrace": lambda a, kw: f"{RT}::trace::gettrace()",
    "gzip.decompress": lambda a, kw: f"{RT}::stdlib::gzip_decompress(&{_argv(a)}, &{_kwvec(kw)})",
    "traceback.extract_tb": lambda a, kw: f"{RT}::trace::extract_tb(&{_argv(a)}, &{_kwvec(kw)})",
    "traceback.format_exception": lambda a, kw: f"{RT}::trace::format_exception(cx, &{_argv(a)}, &{_kwvec(kw)})",
    "traceback.format_exception_only": lambda a, kw: f"{RT}::trace::format_exception_only(&{_argv(a)}[0])",
    "timeit.default_timer": lambda a, kw: f"{RT}::stdlib::time_now(\"perf_counter\")",
    **{f"os.path.{n}": (lambda n: lambda a, kw: f"{RT}::stdlib::os_path(\"{n}\", &{_argv(a)})")(n)
       for n in ("exists", "isfile", "isdir", "join", "basename", "dirname", "splitext", "normpath", "abspath", "realpath")},
    **{f"os.{n}": (lambda n: lambda a, kw: f"{RT}::pathio::os_remove(\"{n}\", &{_argv(a)})"
                   if not kw else (_ for _ in ()).throw(ValueError(f"os.{n}(dir_fd=) is not supported")))(n)
       for n in ("remove", "unlink")},
    "hmac.new": lambda a, kw: f"{RT}::stdlib::hmac_new(&{_argv(a)}, &{_kwvec(kw)})",
    "hmac.compare_digest": lambda a, kw: f"{RT}::libs::compare_digest(&{a[0]}, &{a[1]})",
    # calendar, types
    "calendar.monthrange": lambda a, kw: f"{RT}::libs::monthrange(&{a[0]}, &{a[1]})",
    "types.SimpleNamespace": lambda a, kw: f"{RT}::libs::namespace(&{_argv(a)}, &{_kwvec(kw)})",
    "operator.index": lambda a, kw: _no_kw("operator.index", kw, kw) or f"{RT}::libs::operator_index(&{_argv(a)})",
    "threading.local": lambda a, kw: f"{RT}::libs::thread_local(&{_argv(a)}, &{_kwvec(kw)})",
    # FastAPI's SSE event (yielded by a generator endpoint with response_class=EventSourceResponse)
    "fastapi.sse.ServerSentEvent": lambda a, kw: f"{RT}::web::sse_event(&{_argv(a)}, &{_kwvec(kw)})",
    # pathlib, uuid
    **{f"pathlib.{n}": (lambda a, kw: f"{RT}::pathio::path_new(&{_argv(a)})") for n in ("Path", "PurePath", "PosixPath", "PurePosixPath")},
    "uuid.uuid4": lambda a, kw: f"Ok::<V, Exc>({RT}::pathio::uuid4())",
    "uuid.uuid7": lambda a, kw: (_since((3, 14), "uuid.uuid7()"), _bind("uuid7", a, kw, [], set()),
                                 f"Ok::<V, Exc>({RT}::pathio::uuid7())")[2],
    "uuid.UUID": lambda a, kw: f"{RT}::pathio::uuid_new(&{_argv(a)}, &{_kwvec(kw)})",
    "fractions.Fraction": lambda a, kw: f"{RT}::fraction::call(&{_argv(a)}, &{_kwvec(kw)})",
    **dict.fromkeys(("pydantic.SecretStr", "pydantic.types.SecretStr"),
                    lambda a, kw: f"{RT}::pyd::secret_new(&{_argv(a)}, &{_kwvec(kw)})"),
    # io
    "io.BytesIO": lambda a, kw: f"{RT}::files::bytesio_new({('Some(&' + _bind('BytesIO', a, kw, ['initial_bytes'], {'initial_bytes'})['initial_bytes'] + ')') if (a or kw) else 'None'})",
    # os (read-only environment)
    "os.getenv": lambda a, kw: _getenv("os.getenv", a, kw),
    "os.environ.get": lambda a, kw: _getenv("os.environ.get", a, kw),
    # logging / asyncio
    # logging (dynrt/logging.rs): the module-level logging functions are compiled with their call site (dyn.py)
    "logging.basicConfig": lambda a, kw: f"{RT}::logging::basic_config(&{_argv(a)}, &{_kwvec(kw)})",
    "logging.getLogger": lambda a, kw: f"{RT}::logging::get_logger_v(&{_argv(a)}, &{_kwvec(kw)})",
    **{f"logging.{n}": (lambda f: lambda a, kw: f"{RT}::logging::{f}(&{_argv(a)}, &{_kwvec(kw)})")(f) for n, f in (
        ("StreamHandler", "stream_handler_new"), ("FileHandler", "file_handler_new"), ("NullHandler", "null_handler_new"),
        ("Formatter", "formatter_new"), ("Filter", "filter_new"), ("LoggerAdapter", "adapter_new"), ("disable", "disable"))},
    "logging.config.dictConfig": lambda a, kw: f"{RT}::logging::dict_config(&{a[0] if a else kw['config']})",
    "logging.getLevelName": lambda a, kw: f"{RT}::logging::get_level_name(&{a[0] if a else kw['level']})",
    "logging.addLevelName": lambda a, kw: (f"{{ let (__l, __n) = ({a[0] if a else kw['level']}, {a[1] if len(a) > 1 else kw['levelName']}); "
                                           f"{RT}::logging::add_level_name(match __l {{ V::Int(i) => i, _ => 0 }}, &{RT}::ops::str_(&__n)?); Ok::<V, Exc>(V::None) }}"),
    "logging.getLevelNamesMapping": lambda a, kw: f"{RT}::logging::level_names_mapping()",
    "logging.getHandlerByName": lambda a, kw: f"{RT}::logging::handler_by_name(&{_argv(a)})",
    "logging.getHandlerNames": lambda a, kw: f"{RT}::logging::handler_names()",
    "logging.shutdown": lambda a, kw: f"{RT}::logging::shutdown()",
    **{f"pythonjsonlogger.{m}.JsonFormatter": lambda a, kw: f"{RT}::logging::json_formatter_new(&{_argv(a)}, &{_kwvec(kw)})"
       for m in ("json", "jsonlogger")},
    # ecs-logging 2.x (dynrt/logging.rs); its structlog processor is not translated
    "ecs_logging.StdlibFormatter": lambda a, kw: _ecs_formatter(a, kw),
    **{f"uvicorn.logging.{c}Formatter": (lambda acc: lambda a, kw: f"{RT}::logging::uv_formatter_new({acc}, &{_argv(a)}, &{_kwvec(kw)})")(c == "Access")
       for c in ("Default", "Access")},
    "asyncio.Event": lambda a, kw: f"{RT}::aio::event_new(&{_argv(a)}, &{_kwvec(kw)})",
    "asyncio.Future": _noargs("asyncio.Future", f"Ok::<V, Exc>({RT}::aio::future_new(cx))"),
    "asyncio.Queue": lambda a, kw: f"Ok::<V, Exc>({RT}::web::AQueue::new(match &{a[0] if a else kw.get('maxsize', 'V::Int(0)')} {{ V::Int(i) => *i as usize, _ => 0 }}))",
    "asyncio.sleep": lambda a, kw: (lambda b: f"{RT}::web::sleep(&{b['delay']}, {b.get('result', 'V::None')}, "
                                    f"{str('uvloop' in LIB_VERSIONS).lower()}).await")(
        _need(_bind("asyncio.sleep", a, kw, ["delay", "result"], {"delay", "result"}), "asyncio.sleep", "delay")),
    "anyio.sleep": lambda a, kw: (lambda b: f"{RT}::web::sleep(&{b['delay']}, V::None, "
                                  f"{str('uvloop' in LIB_VERSIONS).lower()}).await")(
        _need(_bind("anyio.sleep", a, kw, ["delay"], {"delay"}), "anyio.sleep", "delay")),
    # FastAPI's default exception handlers, awaited by an application's own handler
    "fastapi.exception_handlers.http_exception_handler": _default_handler("http"),
    "fastapi.exception_handlers.request_validation_exception_handler": _default_handler("validation"),
    "contextlib.asynccontextmanager": lambda a, kw: f"{RT}::agen::asynccontextmanager(&{a[0]})",
    # mcp 2.2 (dynrt/mcp.rs): the server object; its tools, transport options and attributes are checked
    # by the transpiler (dyn.mcp_tools)
    "mcp.server.mcpserver.MCPServer": lambda a, kw: _mcp_server(a, kw),
    # `Request(scope, receive)` inside a raw ASGI app (dynrt/rawasgi.rs): the request being served
    "starlette.requests.Request": lambda a, kw: f"{{ let _ = {a[1]}; {RT}::rawasgi::request_from_scope(cx, &{a[0]}) }}",
    "fastapi.Request": lambda a, kw: f"{{ let _ = {a[1]}; {RT}::rawasgi::request_from_scope(cx, &{a[0]}) }}",
    "mcp.server.transport_security.TransportSecuritySettings": lambda a, kw: _mcp_security(a, kw),
    "contextvars.ContextVar": lambda a, kw: f"{RT}::agen::context_var(&{_argv(a)}, &{_kwvec(kw)})",
    "contextlib.suppress": lambda a, kw: f"Ok::<V, Exc>(V::native({RT}::Native::Suppress({_argv(a)})))",
    # FastAPI / Starlette
    "fastapi.HTTPException": _http_exc,
    "fastapi.exceptions.HTTPException": _http_exc,
    "starlette.exceptions.HTTPException": _http_exc,
    "fastapi.responses.StreamingResponse": _stream,
    **{n: lambda a, kw: f"{RT}::ws::disconnect_exc({_argv(a)}, {_kwvec(kw)})"
       for n in ("fastapi.WebSocketDisconnect", "fastapi.websockets.WebSocketDisconnect", "starlette.websockets.WebSocketDisconnect")},
    **{n: lambda a, kw: f"{RT}::ws::ws_exception({_argv(a)}, {_kwvec(kw)})"
       for n in ("fastapi.WebSocketException", "fastapi.exceptions.WebSocketException", "starlette.exceptions.WebSocketException")},
    "starlette.responses.StreamingResponse": _stream,
    # SQLAlchemy core
    "sqlalchemy.select": lambda a, kw: f"{RT}::orm::select({_argv(a)})",
    "sqlalchemy.future.select": lambda a, kw: f"{RT}::orm::select({_argv(a)})",
    "sqlalchemy.ext.asyncio.async_sessionmaker": lambda a, kw: f"{RT}::orm::sessionmaker(&{_argv(a)}, &{_kwvec(kw)}, false)",
    "sqlalchemy.orm.Session": lambda a, kw: _session(a, kw, True),
    "sqlalchemy.ext.asyncio.AsyncSession": lambda a, kw: _session(a, kw, False),
    "sqlalchemy.orm.sessionmaker": lambda a, kw: f"{RT}::orm::sessionmaker(&{_argv(a)}, &{_kwvec(kw)}, true)",
    "sqlalchemy.create_engine": lambda a, kw: _engine(a, kw, True),
    "sqlalchemy.ext.asyncio.create_async_engine": lambda a, kw: _engine(a, kw, False),
    # inspect(x, raiseerr=False) only: with raiseerr=True a non-mapped value raises NoInspectionAvailable
    "sqlalchemy.inspect": lambda a, kw: _sa_inspect(a, kw),
    "sqlalchemy.text": lambda a, kw: f"{RT}::orm::text(&{a[0]})",
    "sqlalchemy.orm.aliased": lambda a, kw: _aliased(a, kw),
    "sqlalchemy.orm.with_parent": lambda a, kw: f"{RT}::orm::with_parent(&{a[0]}, &{a[1]})" if len(a) == 2 and not kw else (_ for _ in ()).throw(ValueError("with_parent(instance, Parent.relationship) only")),
    "sqlalchemy.tuple_": lambda a, kw: f"{RT}::orm::tuple_({_argv(a)})",
    "sqlalchemy.and_": lambda a, kw: f"{RT}::orm::and_or(\"AND\", {_argv(a)})",
    "sqlalchemy.or_": lambda a, kw: f"{RT}::orm::and_or(\"OR\", {_argv(a)})",
    "sqlalchemy.not_": lambda a, kw: f"{RT}::orm::not_(&{a[0]})",
    "sqlalchemy.update": lambda a, kw: f"{RT}::orm::update(&{a[0]})",
    "sqlalchemy.insert": lambda a, kw: f"{RT}::orm::insert(&{a[0]}, false)",
    "sqlalchemy.dialects.postgresql.insert": lambda a, kw: f"{RT}::orm::insert(&{a[0]}, true)",
    "sqlalchemy.literal": lambda a, kw: f"{RT}::orm::literal(&{a[0]})",
    **{f"sqlalchemy.{n}": (lambda op: lambda a, kw: (
        f"{RT}::orm::compound(\"{op}\", &{_argv(a)})" if not kw
        else (_ for _ in ()).throw(ValueError("keyword arguments are not supported"))))(op)
       for n, op in (("union_all", "UNION ALL"), ("union", "UNION"), ("intersect", "INTERSECT"),
                     ("intersect_all", "INTERSECT ALL"), ("except_", "EXCEPT"), ("except_all", "EXCEPT ALL"))},
    **{f"{m}.array": lambda a, kw: (f"{RT}::orm::pg_array(&{a[0]})" if len(a) == 1 and not kw
                                    else (_ for _ in ()).throw(ValueError("array(items) only: type_= is not supported")))
       for m in ("sqlalchemy.dialects.postgresql", "sqlalchemy.dialects.postgresql.array")},
    **{f"{m}.{n}": (lambda n: lambda a, kw: f"{RT}::orm::any_all(\"{n}\", &{_one(n)(a, kw)})")(n)
       for m in ("sqlalchemy", "sqlalchemy.sql.expression") for n in ("any_", "all_")},
    # a type made without arguments (String() and String are the same type here; String(50) is not)
    **{f"sqlalchemy.{t}": (lambda t: lambda a, kw: f'V::str("{t}")' if not a and not kw
                           else (_ for _ in ()).throw(ValueError(f"{t}() is supported without arguments only")))(t)
       for t in ("String", "Text", "Unicode", "Integer", "BigInteger", "Float", "Date", "Boolean", "Numeric")},
    # literal_column("TRUE"): its text as is (type_= not supported)
    "sqlalchemy.literal_column": lambda a, kw: (
        f"{RT}::orm::literal_column(&{a[0]})" if len(a) == 1 and not kw
        else (_ for _ in ()).throw(ValueError("literal_column(text) only: type_= is not supported"))),
    "sqlalchemy.sql.elements.literal_column": lambda a, kw: (
        f"{RT}::orm::literal_column(&{a[0]})" if len(a) == 1 and not kw
        else (_ for _ in ()).throw(ValueError("literal_column(text) only: type_= is not supported"))),
    "sqlalchemy.sql.expression.literal_column": lambda a, kw: (
        f"{RT}::orm::literal_column(&{a[0]})" if len(a) == 1 and not kw
        else (_ for _ in ()).throw(ValueError("literal_column(text) only: type_= is not supported"))),
    # true() / false(): the literals `true` / `false` (PostgreSQL has a native boolean)
    "sqlalchemy.true": lambda a, kw: f"{RT}::orm::text(&V::str(\"true\"))" if not a and not kw else (_ for _ in ()).throw(ValueError("true() takes no arguments")),
    "sqlalchemy.false": lambda a, kw: f"{RT}::orm::text(&V::str(\"false\"))" if not a and not kw else (_ for _ in ()).throw(ValueError("false() takes no arguments")),
    "sqlalchemy.delete": lambda a, kw: f"{RT}::orm::delete(&{a[0]})",
    "sqlalchemy.exists": lambda a, kw: f"{RT}::orm::exists(&{_argv(a)})",
    **{f"sqlalchemy.orm.{n}": (lambda n: lambda a, kw: f"{RT}::orm::loader(\"{n}\", &{_one(n)(a, kw)})")(n)
       for n in ("selectinload", "joinedload", "subqueryload", "immediateload", "noload", "lazyload", "defaultload")},
    "sqlalchemy.orm.selectin_polymorphic": lambda a, kw: (
        f"{RT}::orm::selectin_polymorphic(&{a[0]}, &{a[1]})" if len(a) == 2 and not kw
        else (_ for _ in ()).throw(ValueError("selectin_polymorphic(Base, [Sub, ...]) only"))),
    "sqlalchemy.orm.with_polymorphic": lambda a, kw: (
        f"{RT}::orm::with_polymorphic(&{a[0]}, &{a[1]})" if len(a) == 2 and not kw
        else f"{RT}::orm::with_polymorphic_opts(&{a[0]}, &{a[1]}, &{kw.get('aliased', 'V::Bool(false)')}, "
             f"&{kw.get('flat', 'V::Bool(false)')})" if len(a) == 2 and set(kw) <= {"aliased", "flat"}
        else (_ for _ in ()).throw(ValueError("with_polymorphic(Base, [Sub, ...] or \"*\", aliased=, flat=) only: "
                                              "selectable= and polymorphic_on= are not supported"))),
    "sqlalchemy.orm.undefer": lambda a, kw: f"{RT}::orm::undefer(&{_one('undefer')(a, kw)})",
    "sqlalchemy.orm.undefer_group": lambda a, kw: f"{RT}::orm::undefer_group(&{_one('undefer_group')(a, kw)})",
    "sqlalchemy.orm.defer": lambda a, kw: f"{RT}::orm::defer(&{_one('defer')(a, {})}, &{_raiseload('defer', kw)})",
    "sqlalchemy.orm.load_only": lambda a, kw: f"{RT}::orm::load_only({_argv(a)}, &{_raiseload('load_only', kw)})",
    "dataclasses.astuple": lambda a, kw: f"{RT}::pyd::dataclass_convert(&{_one('astuple')(a, kw)}, false)",
    "dataclasses.asdict": lambda a, kw: f"{RT}::pyd::dataclass_convert(&{_one('asdict')(a, kw)}, true)",
    "sqlalchemy.bindparam": lambda a, kw: _bindparam(a, kw),
    "sqlalchemy.sql.expression.bindparam": lambda a, kw: _bindparam(a, kw),
    "sqlalchemy.desc": lambda a, kw: f"{RT}::orm::order_fn(&{a[0]}, true)",
    "sqlalchemy.asc": lambda a, kw: f"{RT}::orm::order_fn(&{a[0]}, false)",
    "sqlalchemy.nullslast": lambda a, kw: f"{RT}::orm::sql_method(&{a[0]}, \"nulls_last\", vec![], vec![])",
    "sqlalchemy.nulls_last": lambda a, kw: f"{RT}::orm::sql_method(&{a[0]}, \"nulls_last\", vec![], vec![])",
    "sqlalchemy.extract": lambda a, kw: f"{RT}::orm::extract(&{a[0]}, &{a[1]})",
    "sqlalchemy.cast": lambda a, kw: f"{RT}::orm::cast(&{a[0]}, &{a[1]})",
    "sqlalchemy.orm.attributes.flag_modified": lambda a, kw: f"{RT}::orm::flag_modified(&{a[0]}, &{a[1]})",
    "sqlalchemy.orm.flag_modified": lambda a, kw: f"{RT}::orm::flag_modified(&{a[0]}, &{a[1]})",
    "sqlalchemy.distinct": lambda a, kw: f"{RT}::orm::sql_method(&{a[0]}, \"distinct\", vec![], vec![])",
    # typing no-ops
    # cast(type, value) is the value (the type only informs a type checker: read, never evaluated)
    "typing.cast": lambda a, kw: (a[0], f"Ok::<V, Exc>({a[1]})")[1],
}

# namespaces whose attribute calls are generic: `func.count(...)` -> orm::func("count", ...)
NAMESPACE_CALLS = {
    "sqlalchemy.func": lambda name, a, kw: f"{RT}::orm::func({name!r}, {_argv(a)})".replace("'", '"'),
    "sqlalchemy.sql.func": lambda name, a, kw: f"{RT}::orm::func({name!r}, {_argv(a)})".replace("'", '"'),
}

STATUS_MODULES = ("fastapi.status", "starlette.status")


def status_constant(dotted: str) -> int | None:
    """`fastapi.status.HTTP_404_NOT_FOUND` -> 404."""
    for mod in STATUS_MODULES:
        if dotted.startswith(mod + "."):
            m = re.match(r"(?:HTTP|WS)_(\d{3,4})_", dotted[len(mod) + 1:])
            if m:
                return int(m.group(1))
    return None


# --- calls whose template receives the run-time vectors (args: Vec<V>, kwargs: Vec<(String, V)>) ---
DYN_ARGS = {
    "asyncio.gather": lambda a, kw: f"{RT}::aio::gather({a}, &{kw}).await",
    "asyncio.Semaphore": lambda a, kw: f"{RT}::aio::semaphore(&{a}, &{kw})",
    "asyncio.BoundedSemaphore": lambda a, kw: f"{RT}::aio::semaphore(&{a}, &{kw})",
    "asyncio.Lock": lambda a, kw: f"{RT}::aio::semaphore(&{a}, &{kw})",
    # case(*whens, else_=...): the (condition, value) tuples of a list built at run time
    "sqlalchemy.case": lambda a, kw: f"{RT}::orm::case(&{a}, &{kw})",
}
for _n in DYN_ARGS:
    CALLS.setdefault(_n, DYN_ARGS[_n])


# prometheus_client's submodules re-exported by the package
PROM_SUBMODULES = {
    "metrics": ("Counter", "Gauge", "Summary", "Histogram", "Info", "Enum", "disable_created_metrics", "enable_created_metrics"),
    "registry": ("CollectorRegistry", "REGISTRY"),
    "exposition": ("generate_latest", "CONTENT_TYPE_LATEST", "CONTENT_TYPE_PLAIN_0_0_4", "CONTENT_TYPE_PLAIN_1_0_0",
                   "start_http_server", "start_wsgi_server"),
    "gc_collector": ("GC_COLLECTOR",),
    "platform_collector": ("PLATFORM_COLLECTOR",),
    "process_collector": ("PROCESS_COLLECTOR",),
}


def canonical(dotted: str) -> str:
    """Aliases of the same API (`sqlalchemy.sql.expression.select` -> `sqlalchemy.select`)."""
    for prefix, repl in (
        ("sqlalchemy.sql.expression.", "sqlalchemy."),
        ("sqlalchemy.sql.functions.func", "sqlalchemy.func"),
        ("sqlalchemy.orm.exc.", "sqlalchemy.exc."),
        ("sqlalchemy.ext.asyncio.session.", "sqlalchemy.ext.asyncio."),
        ("fastapi.routing.", "fastapi."),
        ("starlette.status", "fastapi.status"),
        ("sqlalchemy.dialects.postgresql.dml.insert", "sqlalchemy.dialects.postgresql.insert"),
        ("sqlalchemy.sql.dml.insert", "sqlalchemy.insert"),
        *((f"prometheus_client.{m}.{n}", f"prometheus_client.{n}") for m, ns in PROM_SUBMODULES.items() for n in ns),
    ):
        if dotted.startswith(prefix):
            return repl + dotted[len(prefix):]
    return dotted

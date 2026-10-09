"""The closed list of Python library APIs the dyn backend translates .

Each entry maps a resolved external name (`modules.Ext.dotted`, e.g. `datetime.datetime.now`) to
Rust code over the `dynrt` runtime. Anything not listed is refused at transpile time with
`fichier:ligne`, so a library is never approximated silently.

Templates receive the compiled positional arguments (Rust expressions of type `V`) and keyword
arguments (name -> expression) and return a Rust expression of type `R` (a `Result<V, Exc>`).
"""
from __future__ import annotations

import re

RT = "crate::dynrt"


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


def _one(name):
    def f(a, kw):
        if len(a) != 1 or kw:
            raise ValueError(f"{name}() takes exactly one positional argument here")
        return a[0]
    return f


# --- values (attribute or name used as a value) -------------------------------------------
VALUES: dict[str, str] = {
    "datetime.UTC": f"V::Tz({RT}::dt::Tz::Utc)",
    "datetime.timezone.utc": f"V::Tz({RT}::dt::Tz::Utc)",
    "sqlalchemy.null": "V::None",
    **{f"sqlalchemy.{t}": f'V::str("{t}")' for t in ("String", "Text", "Unicode", "Integer", "BigInteger", "Float", "Date", "Boolean")},
    "os.environ": f"{RT}::libs::environ()",
    "aio_pika.DeliveryMode.PERSISTENT": "V::Int(2)",
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
    **{f"redis.exceptions.{n}": c for n, c in (("RedisError", "REDIS_ERROR"), ("ConnectionError", "REDIS_CONNECTION_ERROR"),
                                                ("TimeoutError", "REDIS_TIMEOUT_ERROR"), ("DataError", "REDIS_DATA_ERROR"),
                                                ("ResponseError", "REDIS_RESPONSE_ERROR"))},
    "pickle.PicklingError": "PICKLING_ERROR",
    "pickle.UnpicklingError": "UNPICKLING_ERROR",
    "sqlalchemy.exc.MultipleResultsFound": "MULTIPLE_RESULTS_FOUND",
    "sqlalchemy.orm.exc.NoResultFound": "NO_RESULT_FOUND",
    # sqlalchemy.orm.exc only (canonical() folds it into sqlalchemy.exc)
    "sqlalchemy.exc.StaleDataError": "STALE_DATA_ERROR",
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
    "jinja2.TemplateNotFound": "TEMPLATE_NOT_FOUND",
    "re.error": "RE_ERROR",
    "csv.Error": "CSV_ERROR",
    "jinja2.exceptions.TemplateNotFound": "TEMPLATE_NOT_FOUND",
    "aiosmtplib.SMTPException": "SMTP_EXCEPTION",
    "aiosmtplib.errors.SMTPException": "SMTP_EXCEPTION",
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


def _its_serializer(a, kw):
    v = _bind("URLSafeTimedSerializer", a, kw, ["secret_key", "salt"], {"secret_key", "salt"})
    salt = f"Some(&{v['salt']})" if "salt" in v else "None"
    return f"{RT}::itsd::new(&{v['secret_key']}, {salt})"


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


CALLS = {
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
    **{n: (lambda a, kw: f"{RT}::rmq::connect(&{_argv(a)}, &{_kwvec(kw)}).await") for n in ("aio_pika.connect_robust", "aio_pika.connect")},
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
    **{f"base64.{f}": (lambda f: lambda a, kw: f"{RT}::stdlib::base64(\"{f}\", &{_argv(a)}, &{_kwvec(kw)})")(f)
       for f in ("b64encode", "b64decode", "standard_b64encode", "standard_b64decode", "urlsafe_b64encode",
                 "urlsafe_b64decode", "b16encode", "b16decode", "b32encode", "b32decode")},
    # urllib.parse
    **{f"urllib.parse.{f}": (lambda f: lambda a, kw: f"{RT}::stdlib::urllib(\"{f}\", &{_argv(a)}, &{_kwvec(kw)})")(f)
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
    # itsdangerous (URLSafeTimedSerializer with its default signer and serializer only)
    "itsdangerous.URLSafeTimedSerializer": _its_serializer,
    "itsdangerous.url_safe.URLSafeTimedSerializer": _its_serializer,
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
    # positional-only in CPython: keywords are a TypeError there, refused here
    "unicodedata.normalize": lambda a, kw: f"{RT}::stdlib::unicodedata_normalize(crate::gen::UCD_UNASSIGNED, &{a[0]}, &{a[1]})" if len(a) == 2 and not kw else (_ for _ in ()).throw(ValueError("normalize(form, unistr): two positional arguments")),
    "unicodedata.combining": lambda a, kw: f"{RT}::stdlib::unicodedata_combining(crate::gen::UCD_UNASSIGNED, &{a[0]})" if len(a) == 1 and not kw else (_ for _ in ()).throw(ValueError("combining(chr): one positional argument")),
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
    "re.escape": lambda a, kw: f"{RT}::stdlib::escape(&{a[0]})",
    # io.StringIO, csv
    "io.StringIO": lambda a, kw: f"{RT}::stdlib::stringio_new({('Some(&' + _bind('StringIO', a, kw, ['initial_value'], {'initial_value'})['initial_value'] + ')') if (a or kw) else 'None'})",
    "csv.writer": lambda a, kw: f"{RT}::stdlib::writer_new(&{_argv(a)}, &{_kwvec(kw)}, false)",
    "csv.DictWriter": lambda a, kw: f"{RT}::stdlib::writer_new(&{_argv(a)}, &{_kwvec(kw)}, true)",
    "csv.reader": lambda a, kw: f"{RT}::stdlib::reader(&{_argv(a)}, &{_kwvec(kw)})",
    "csv.DictReader": lambda a, kw: f"{RT}::stdlib::dict_reader(&{_argv(a)}, &{_kwvec(kw)})",
    "csv.Sniffer": lambda a, kw: f"Ok::<V, Exc>(V::native({RT}::Native::SnifferObj))",
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
    # FastAPI's SSE event (yielded by a generator endpoint with response_class=EventSourceResponse)
    "fastapi.sse.ServerSentEvent": lambda a, kw: f"{RT}::web::sse_event(&{_argv(a)}, &{_kwvec(kw)})",
    # pathlib, uuid
    **{f"pathlib.{n}": (lambda a, kw: f"{RT}::pathio::path_new(&{_argv(a)})") for n in ("Path", "PurePath", "PosixPath", "PurePosixPath")},
    "uuid.uuid4": lambda a, kw: f"Ok::<V, Exc>({RT}::pathio::uuid4())",
    "uuid.UUID": lambda a, kw: f"{RT}::pathio::uuid_new(&{_argv(a)}, &{_kwvec(kw)})",
    # io
    "io.BytesIO": lambda a, kw: f"{RT}::files::bytesio_new({('Some(&' + _bind('BytesIO', a, kw, ['initial_bytes'], {'initial_bytes'})['initial_bytes'] + ')') if (a or kw) else 'None'})",
    # os (read-only environment)
    "os.getenv": lambda a, kw: _getenv("os.getenv", a, kw),
    "os.environ.get": lambda a, kw: _getenv("os.environ.get", a, kw),
    # logging / asyncio
    "logging.basicConfig": lambda a, kw: "{ " + "".join(f"let _ = {x}; " for x in list(a) + list(kw.values())) + "Ok::<V, Exc>(V::None) }",
    # module-level functions: the root logger
    **{f"logging.{n}": (lambda n: lambda a, kw: f"{RT}::web::log(cx, \"root\", \"{n}\", &{_argv(a)}, &{_kwvec(kw)}).await")(n)
       for n in ("debug", "info", "warning", "error", "exception", "critical", "log")},
    "logging.getLogger": lambda a, kw: f"Ok::<V, Exc>(V::native({RT}::Native::Logger(std::sync::Arc::from({RT}::ops::str_(&{a[0] if a else 'V::str(\"root\")'})?.as_str()))))",
    "asyncio.Queue": lambda a, kw: f"Ok::<V, Exc>({RT}::web::AQueue::new(match &{a[0] if a else kw.get('maxsize', 'V::Int(0)')} {{ V::Int(i) => *i as usize, _ => 0 }}))",
    "asyncio.sleep": lambda a, kw: f"{RT}::web::sleep(&{a[0]}).await",
    "anyio.sleep": lambda a, kw: f"{RT}::web::sleep(&{a[0] if a else kw.get('delay', 'V::Int(0)')}).await",
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
    "sqlalchemy.orm.sessionmaker": lambda a, kw: f"{RT}::orm::sessionmaker(&{_argv(a)}, &{_kwvec(kw)}, true)",
    "sqlalchemy.create_engine": lambda a, kw: _engine(a, kw, True),
    "sqlalchemy.ext.asyncio.create_async_engine": lambda a, kw: _engine(a, kw, False),
    # inspect(x, raiseerr=False) only: with raiseerr=True a non-mapped value raises NoInspectionAvailable
    "sqlalchemy.inspect": lambda a, kw: _sa_inspect(a, kw),
    "sqlalchemy.text": lambda a, kw: f"{RT}::orm::text(&{a[0]})",
    "sqlalchemy.orm.aliased": lambda a, kw: f"{RT}::orm::aliased(&{a[0]})" if len(a) == 1 and not kw else (_ for _ in ()).throw(ValueError("aliased(Model) only")),
    "sqlalchemy.tuple_": lambda a, kw: f"{RT}::orm::tuple_({_argv(a)})",
    "sqlalchemy.and_": lambda a, kw: f"{RT}::orm::and_or(\"AND\", {_argv(a)})",
    "sqlalchemy.or_": lambda a, kw: f"{RT}::orm::and_or(\"OR\", {_argv(a)})",
    "sqlalchemy.not_": lambda a, kw: f"{RT}::orm::not_(&{a[0]})",
    "sqlalchemy.update": lambda a, kw: f"{RT}::orm::update(&{a[0]})",
    "sqlalchemy.insert": lambda a, kw: f"{RT}::orm::insert(&{a[0]}, false)",
    "sqlalchemy.dialects.postgresql.insert": lambda a, kw: f"{RT}::orm::insert(&{a[0]}, true)",
    "sqlalchemy.case": lambda a, kw: f"{RT}::orm::case(&{_argv(a)}, &{_kwvec(kw)})",
    "sqlalchemy.literal": lambda a, kw: f"{RT}::orm::literal(&{a[0]})",
    # true() / false(): the literals `true` / `false` (PostgreSQL has a native boolean)
    "sqlalchemy.true": lambda a, kw: f"{RT}::orm::text(&V::str(\"true\"))" if not a and not kw else (_ for _ in ()).throw(ValueError("true() takes no arguments")),
    "sqlalchemy.false": lambda a, kw: f"{RT}::orm::text(&V::str(\"false\"))" if not a and not kw else (_ for _ in ()).throw(ValueError("false() takes no arguments")),
    "sqlalchemy.delete": lambda a, kw: f"{RT}::orm::delete(&{a[0]})",
    "sqlalchemy.exists": lambda a, kw: f"{RT}::orm::exists(&{_argv(a)})",
    **{f"sqlalchemy.orm.{n}": (lambda n: lambda a, kw: f"{RT}::orm::loader(\"{n}\", &{a[0]})")(n)
       for n in ("selectinload", "joinedload", "subqueryload", "immediateload", "noload", "lazyload")},
    "sqlalchemy.orm.undefer": lambda a, kw: f"{RT}::orm::undefer(&{_one('undefer')(a, kw)})",
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
    "typing.cast": lambda a, kw: f"Ok::<V, Exc>({a[1]})",
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

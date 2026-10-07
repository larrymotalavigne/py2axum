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


def _argv(args: list[str]) -> str:
    return "vec![" + ", ".join(args) + "]"


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
    **{f"tenacity.{n}": f'{RT}::tenacity::value("{n}")' for n in ("stop_never", "retry_always", "retry_never")},
    **{f"logging.{n}": f"V::Int({v})" for n, v in (("NOTSET", 0), ("DEBUG", 10), ("INFO", 20), ("WARNING", 30), ("WARN", 30),
                                                    ("ERROR", 40), ("CRITICAL", 50), ("FATAL", 50))},
    "aio_pika.DeliveryMode.NOT_PERSISTENT": "V::Int(1)",
    "datetime.time.min": "V::Time(chrono::NaiveTime::MIN)",
    "datetime.time.max": "V::Time(chrono::NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999).unwrap())",
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
        ("sqlalchemy.orm.Session", "sqlalchemy.orm.Session"))},
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
    "builtins.NameError": "NAME_ERROR",
    "builtins.FileNotFoundError": "FILE_NOT_FOUND_ERROR",
    "builtins.UnicodeDecodeError": "UNICODE_DECODE_ERROR",
    "builtins.ImportError": "IMPORT_ERROR",
    "builtins.ModuleNotFoundError": "MODULE_NOT_FOUND_ERROR",
    "builtins.FileExistsError": "FILE_EXISTS_ERROR",
    "builtins.PermissionError": "PERMISSION_ERROR",
    "builtins.IsADirectoryError": "IS_A_DIRECTORY_ERROR",
    "builtins.GeneratorExit": "GENERATOR_EXIT",
    "binascii.Error": "BINASCII_ERROR",
    "json.JSONDecodeError": "JSON_DECODE_ERROR",
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
    **{f"httpx.{n}": f"HTTPX_{c}" for n, c in (
        ("HTTPError", "HTTP_ERROR"), ("RequestError", "REQUEST_ERROR"), ("TransportError", "TRANSPORT_ERROR"),
        ("TimeoutException", "TIMEOUT_EXCEPTION"), ("ConnectTimeout", "CONNECT_TIMEOUT"), ("ReadTimeout", "READ_TIMEOUT"),
        ("NetworkError", "NETWORK_ERROR"), ("ConnectError", "CONNECT_ERROR"), ("UnsupportedProtocol", "UNSUPPORTED_PROTOCOL"),
        ("TooManyRedirects", "TOO_MANY_REDIRECTS"), ("HTTPStatusError", "STATUS_ERROR"), ("InvalidURL", "INVALID_URL"))},
    **{f"aiohttp.{n}": f"AIO_{c}" for n, c in (
        ("ClientError", "CLIENT_ERROR"), ("ClientResponseError", "RESPONSE_ERROR"), ("ContentTypeError", "CONTENT_TYPE_ERROR"),
        ("TooManyRedirects", "TOO_MANY_REDIRECTS"), ("ClientConnectionError", "CONNECTION_ERROR"), ("ClientOSError", "OS_ERROR"),
        ("ClientConnectorError", "CONNECTOR_ERROR"), ("ServerConnectionError", "SERVER_CONNECTION_ERROR"),
        ("ServerTimeoutError", "SERVER_TIMEOUT"), ("ConnectionTimeoutError", "CONNECTION_TIMEOUT"), ("InvalidURL", "INVALID_URL"))},
    "asyncio.TimeoutError": "TIMEOUT_ERROR",
    "asyncio.CancelledError": "CANCELLED_ERROR",
    "asyncio.QueueFull": "QUEUE_FULL",
    "asyncio.QueueEmpty": "QUEUE_EMPTY",
    "fastapi.HTTPException": "HTTP_EXCEPTION",
    "fastapi.exceptions.HTTPException": "HTTP_EXCEPTION",
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
    "builtins.EOFError": "EOF_ERROR",
    "pickle.PickleError": "PICKLE_ERROR",
    "aio_pika.exceptions.AMQPError": "AMQP_ERROR",
    "tenacity.RetryError": "TENACITY_RETRY_ERROR",
    "aio_pika.exceptions.AMQPConnectionError": "AMQP_CONNECTION_ERROR",
    "aio_pika.exceptions.QueueEmpty": "AMQP_QUEUE_EMPTY",
    **{f"redis.exceptions.{n}": c for n, c in (("RedisError", "REDIS_ERROR"), ("ConnectionError", "REDIS_CONNECTION_ERROR"),
                                                ("TimeoutError", "REDIS_TIMEOUT_ERROR"), ("DataError", "REDIS_DATA_ERROR"),
                                                ("ResponseError", "REDIS_RESPONSE_ERROR"))},
    "pickle.PicklingError": "PICKLING_ERROR",
    "pickle.UnpicklingError": "UNPICKLING_ERROR",
    "sqlalchemy.exc.MultipleResultsFound": "MULTIPLE_RESULTS_FOUND",
    "sqlalchemy.orm.exc.NoResultFound": "NO_RESULT_FOUND",
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


def _json_dumps(a, kw):
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


def _its_serializer(a, kw):
    v = _bind("URLSafeTimedSerializer", a, kw, ["secret_key", "salt"], {"secret_key", "salt"})
    salt = f"Some(&{v['salt']})" if "salt" in v else "None"
    return f"{RT}::itsd::new(&{v['secret_key']}, {salt})"


def _getenv(name, a, kw):
    v = _bind(name, a, kw, ["key", "default"], {"key", "default"} if name == "os.getenv" else set())
    d = f"Some(&{v['default']})" if "default" in v else "None"
    return f"{RT}::libs::getenv(&{v['key']}, {d})"


def _sentry_noop(a, kw):
    vals = list(a) + [kw[k] for k in kw]
    return "{ " + "".join(f"let _ = {v}; " for v in vals) + "Ok::<V, Exc>(V::None) }"


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


CALLS = {
    # outgoing HTTP: httpx 0.28, aiohttp 3.14 (the version goes in the User-Agent)
    "httpx.AsyncClient": lambda a, kw: f"{RT}::http::client(\"httpx\", \"{LIB_VERSIONS.get('httpx', '0.28.1')}\", &{_argv(a)}, &{_kwvec(kw)})",
    "aiohttp.ClientSession": lambda a, kw: f"{RT}::http::client(\"aiohttp\", \"{LIB_VERSIONS.get('aiohttp', '3.14.3')}\", &{_argv(a)}, &{_kwvec(kw)})",
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
    "json.loads": lambda a, kw: f"{RT}::libs::json_loads(&{a[0]})",
    # python-jose (HMAC only; other options refused)
    "jose.jwt.encode": _jwt_encode,
    "jose.jwt.decode": _jwt_decode,
    # itsdangerous (URLSafeTimedSerializer with its default signer and serializer only)
    "itsdangerous.URLSafeTimedSerializer": _its_serializer,
    "itsdangerous.url_safe.URLSafeTimedSerializer": _its_serializer,
    # cryptography (Fernet only)
    "cryptography.fernet.Fernet": lambda a, kw: f"{RT}::fernet::new(&{_bind('Fernet', a, kw, ['key'], {'key'})['key']})",
    # e-mail: jinja2 templates, email.mime, aiosmtplib
    "jinja2.Environment": lambda a, kw: f"{RT}::mail::environment(&{_kwvec(kw)})" if not a else (_ for _ in ()).throw(ValueError("pass keyword arguments")),
    "jinja2.FileSystemLoader": lambda a, kw: f"{RT}::mail::fs_loader(&{_bind('FileSystemLoader', a, kw, ['searchpath'], {'searchpath'})['searchpath']})",
    "jinja2.select_autoescape": lambda a, kw: f"{RT}::mail::select_autoescape({('Some(&' + _bind('select_autoescape', a, kw, ['enabled_extensions'], {'enabled_extensions'})['enabled_extensions'] + ')') if (a or kw) else 'None'})",
    **{f"email.mime.{m}.{c}": (lambda c: lambda a, kw: f"{RT}::mail::mime_new(\"{c}\", &{_argv(a)}, &{_kwvec(kw)})")(c)
       for m, c in (("multipart", "MIMEMultipart"), ("text", "MIMEText"), ("application", "MIMEApplication"))},
    "email.utils.formataddr": lambda a, kw: f"{RT}::mail::formataddr(&{a[0]})",
    "email.utils.make_msgid": lambda a, kw: f"{RT}::mail::make_msgid({('Some(&' + _bind('make_msgid', a, kw, ['idstring', 'domain'], {'domain'})['domain'] + ')') if kw.get('domain') else 'None'})",
    "aiosmtplib.send": lambda a, kw: f"{RT}::mail::smtp_send({_argv(a)}, {_kwvec(kw)}).await",
    # Starlette responses returned by an endpoint
    "fastapi.responses.Response": (lambda a, kw: f"{RT}::resp::new(\"Response\", &{_argv(a)}, &{_kwvec(kw)})"),
    "starlette.responses.Response": (lambda a, kw: f"{RT}::resp::new(\"Response\", &{_argv(a)}, &{_kwvec(kw)})"),
    "fastapi.responses.JSONResponse": (lambda a, kw: f"{RT}::resp::new(\"JSONResponse\", &{_argv(a)}, &{_kwvec(kw)})"),
    "starlette.responses.JSONResponse": (lambda a, kw: f"{RT}::resp::new(\"JSONResponse\", &{_argv(a)}, &{_kwvec(kw)})"),
    "fastapi.responses.PlainTextResponse": (lambda a, kw: f"{RT}::resp::new(\"PlainTextResponse\", &{_argv(a)}, &{_kwvec(kw)})"),
    "starlette.responses.PlainTextResponse": (lambda a, kw: f"{RT}::resp::new(\"PlainTextResponse\", &{_argv(a)}, &{_kwvec(kw)})"),
    "fastapi.responses.HTMLResponse": (lambda a, kw: f"{RT}::resp::new(\"HTMLResponse\", &{_argv(a)}, &{_kwvec(kw)})"),
    "starlette.responses.HTMLResponse": (lambda a, kw: f"{RT}::resp::new(\"HTMLResponse\", &{_argv(a)}, &{_kwvec(kw)})"),
    "fastapi.responses.RedirectResponse": (lambda a, kw: f"{RT}::resp::new(\"RedirectResponse\", &{_argv(a)}, &{_kwvec(kw)})"),
    "starlette.responses.RedirectResponse": (lambda a, kw: f"{RT}::resp::new(\"RedirectResponse\", &{_argv(a)}, &{_kwvec(kw)})"),
    "fastapi.responses.FileResponse": (lambda a, kw: f"{RT}::resp::new(\"FileResponse\", &{_argv(a)}, &{_kwvec(kw)})"),
    "starlette.responses.FileResponse": (lambda a, kw: f"{RT}::resp::new(\"FileResponse\", &{_argv(a)}, &{_kwvec(kw)})"),
    "fastapi.Response": (lambda a, kw: f"{RT}::resp::new(\"Response\", &{_argv(a)}, &{_kwvec(kw)})"),
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
    **{f"os.path.{n}": (lambda n: lambda a, kw: f"{RT}::stdlib::os_path(\"{n}\", &{_argv(a)})")(n)
       for n in ("exists", "isfile", "isdir", "join", "basename", "dirname", "splitext", "normpath", "abspath", "realpath")},
    "hmac.new": lambda a, kw: f"{RT}::stdlib::hmac_new(&{_argv(a)}, &{_kwvec(kw)})",
    "hmac.compare_digest": lambda a, kw: f"{RT}::libs::compare_digest(&{a[0]}, &{a[1]})",
    # calendar, types
    "calendar.monthrange": lambda a, kw: f"{RT}::libs::monthrange(&{a[0]}, &{a[1]})",
    "types.SimpleNamespace": lambda a, kw: f"{RT}::libs::namespace(&{_argv(a)}, &{_kwvec(kw)})",
    # pathlib, uuid
    **{f"pathlib.{n}": (lambda a, kw: f"{RT}::pathio::path_new(&{_argv(a)})") for n in ("Path", "PurePath", "PosixPath", "PurePosixPath")},
    "uuid.uuid4": lambda a, kw: f"Ok::<V, Exc>({RT}::pathio::uuid4())",
    "uuid.UUID": lambda a, kw: f"{RT}::pathio::uuid_new(&{_argv(a)}, &{_kwvec(kw)})",
    # io
    "io.BytesIO": lambda a, kw: f"{RT}::files::bytesio_new({('Some(&' + _bind('BytesIO', a, kw, ['initial_bytes'], {'initial_bytes'})['initial_bytes'] + ')') if (a or kw) else 'None'})",
    # os (read-only environment)
    "os.getenv": lambda a, kw: _getenv("os.getenv", a, kw),
    "os.environ.get": lambda a, kw: _getenv("os.environ.get", a, kw),
    # sentry_sdk: the binary behaves as an SDK that was never initialised (no DSN): calls do nothing and
    # return None; their arguments are still evaluated (README)
    **{f"sentry_sdk.{n}": _sentry_noop for n in (
        "init", "set_tag", "set_tags", "set_user", "set_context", "set_extra", "capture_message",
        "capture_exception", "add_breadcrumb")},
    # logging / asyncio
    "logging.basicConfig": lambda a, kw: "{ " + "".join(f"let _ = {x}; " for x in list(a) + list(kw.values())) + "Ok::<V, Exc>(V::None) }",
    # module-level functions: the root logger
    **{f"logging.{n}": (lambda n: lambda a, kw: f"{RT}::web::log(\"root\", \"{n}\", &{_argv(a)})")(n)
       for n in ("debug", "info", "warning", "error", "exception", "critical", "log")},
    "logging.getLogger": lambda a, kw: f"Ok::<V, Exc>(V::native({RT}::Native::Logger(std::sync::Arc::from({RT}::ops::str_(&{a[0] if a else 'V::str(\"root\")'})?.as_str()))))",
    "asyncio.Queue": lambda a, kw: f"Ok::<V, Exc>({RT}::web::AQueue::new(match &{a[0] if a else kw.get('maxsize', 'V::Int(0)')} {{ V::Int(i) => *i as usize, _ => 0 }}))",
    "asyncio.sleep": lambda a, kw: f"{RT}::web::sleep(&{a[0]}).await",
    # FastAPI / Starlette
    "fastapi.HTTPException": _http_exc,
    "fastapi.exceptions.HTTPException": _http_exc,
    "starlette.exceptions.HTTPException": _http_exc,
    "fastapi.responses.StreamingResponse": _stream,
    "starlette.responses.StreamingResponse": _stream,
    # SQLAlchemy core
    "sqlalchemy.select": lambda a, kw: f"{RT}::orm::select({_argv(a)})",
    "sqlalchemy.future.select": lambda a, kw: f"{RT}::orm::select({_argv(a)})",
    "sqlalchemy.ext.asyncio.async_sessionmaker": lambda a, kw: f"{RT}::orm::sessionmaker(&{_argv(a)}, &{_kwvec(kw)})",
    "sqlalchemy.ext.asyncio.create_async_engine": lambda a, kw: "{ " + "".join(f"let _ = {x}; " for x in list(a) + list(kw.values())) + f"Ok::<V, Exc>(V::native({RT}::Native::Engine)) }}",
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
    "sqlalchemy.delete": lambda a, kw: f"{RT}::orm::delete(&{a[0]})",
    "sqlalchemy.exists": lambda a, kw: f"{RT}::orm::exists(&{a[0]})",
    **{f"sqlalchemy.orm.{n}": (lambda n: lambda a, kw: f"{RT}::orm::loader(\"{n}\", &{a[0]})")(n)
       for n in ("selectinload", "joinedload", "subqueryload", "immediateload", "noload", "lazyload")},
    "sqlalchemy.desc": lambda a, kw: f"{RT}::orm::sql_method(&{a[0]}, \"desc\", vec![], vec![])",
    "sqlalchemy.asc": lambda a, kw: f"{RT}::orm::sql_method(&{a[0]}, \"asc\", vec![], vec![])",
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
    ):
        if dotted.startswith(prefix):
            return repl + dotted[len(prefix):]
    return dotted

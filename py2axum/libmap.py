"""The closed list of Python library APIs the dyn backend translates .

Each entry maps a resolved external name (`modules.Ext.dotted`, e.g. `datetime.datetime.now`) to
Rust code over the `dynrt` runtime. Anything not listed is refused at transpile time with
`fichier:ligne`, so a library is never approximated silently.

Templates receive the compiled positional arguments (Rust expressions of type `V`) and keyword
arguments (name -> expression) and return a Rust expression of type `R` (a `Result<V, Exc>`).
"""
from __future__ import annotations

import json
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
        # the name quoted alone: the argument expressions may hold string literals with `'` (Rust code as is)
        return f"{{ let _ = {req}; {RT}::web::default_exc_handler(&{exc}, {json.dumps(which)}) }}"
    return tmpl


def _session(a, kw, sync: bool):
    """`Session(engine)` / `AsyncSession(engine)`: a session of its own on the binary's pool."""
    bad = sorted(set(kw) - {"bind", "expire_on_commit", "autoflush"})
    if bad or len(a) > 1:
        raise ValueError(f"{'Session' if sync else 'AsyncSession'}() is supported with the engine, bind=, "
                         "expire_on_commit= and autoflush= only" + (f" (not {bad[0]}=)" if bad else ""))
    return f"{RT}::orm::new_session(cx, {_argv(a)}, {_kwvec(kw)}, {'true' if sync else 'false'})"


def rs_(s: str) -> str:
    return '"' + s + '"'


def _pyjwt_215(what: str) -> str:
    """PyJWT's JWK classes are ported from 2.15 (2.13 and 2.14 differ: no refresh cooldown, no scheme check)."""
    v = LIB_VERSIONS.get("pyjwt")
    if v is not None and not v.startswith("2.15."):
        raise ValueError(f"jwt.{what} is ported from PyJWT 2.15 (the project locks {v})")
    return ""


def _pyjwk(a, kw) -> str:
    v = _bind("PyJWK", a, kw, ["jwk_data", "algorithm"], {"jwk_data", "algorithm"})
    alg = "None" if v.get("algorithm") is None else f"Some(&{v['algorithm']})"
    return f"{RT}::jwks::jwk_value({RT}::jwks::jwk_new(&{v['jwk_data']}, {alg}))"


def _dotenv_patch() -> int:
    """The patch release of the project's python-dotenv 1.2 (its parser changed in 1.2.3 and 1.2.4)."""
    v = LIB_VERSIONS.get("python-dotenv", "1.2.4").split(".")
    return int(re.match(r"\d+", v[2]).group()) if len(v) > 2 and re.match(r"\d+", v[2]) else 0


def _argv(args: list[str]) -> str:
    return "vec![" + ", ".join(args) + "]"


def _array_new(a, kw):
    # Python 3.15 adds the typecodes e, Zf and Zd: not reproduced, refused when the literal names one
    if PYTHON >= (3, 15) and a and re.fullmatch(r'V::str\("(e|Zf|Zd)"\)', a[0].strip()):
        raise ValueError("array.array typecodes e, Zf and Zd (Python 3.15) are not supported (b B h H i I l L q Q f d)")
    return f"{RT}::arr::new(&{_argv(a)}, &{_kwvec(kw)})"


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


def _qrcode(name, a, kw):
    """`qrcode.QRCode(...)` / `qrcode.make(data, **kw)` (dynrt/qr.rs): the default image factory only (Pillow's
    PilImage, the PNG Pillow writes); image_factory= is refused (the factory's own output is not reproduced)"""
    if name == "make":
        if len(a) > 1:
            raise ValueError("qrcode.make(data, **kwargs): the QRCode arguments by keyword")
        if kw.get("image_factory", "V::None") != "V::None":
            raise ValueError("qrcode.make(image_factory=) is not supported: only the default factory (Pillow's "
                             "PilImage, its PNG) is reproduced")
        _bind("qrcode.make", [], {k: v for k, v in kw.items() if k != "data"}, _QR_PARAMS, set(_QR_PARAMS))
        return f"{RT}::qr::make(&{_argv(a)}, &{_kwvec(kw)})"
    vals = _bind("qrcode.QRCode", a, kw, _QR_PARAMS, set(_QR_PARAMS))
    if vals.get("image_factory", "V::None") != "V::None":
        raise ValueError("qrcode.QRCode(image_factory=) is not supported: only the default factory (Pillow's PilImage, "
                         "its PNG) is reproduced")
    return f"{RT}::qr::new(&{_argv(a)}, &{_kwvec(kw)})"


_QR_PARAMS = ["version", "error_correction", "box_size", "border", "image_factory", "mask_pattern"]


def _tempfile(a, kw):
    """`NamedTemporaryFile(...)`: keywords only, without the buffering and text options."""
    allowed = ("mode", "suffix", "prefix", "dir", "delete")
    if a:
        raise ValueError("tempfile.NamedTemporaryFile() with positional arguments is not supported (pass them by keyword)")
    bad = [k for k in kw if k not in allowed]
    if bad:
        raise ValueError(f"tempfile.NamedTemporaryFile({bad[0]}=) is not supported (supported: {', '.join(allowed)})")
    return f"{RT}::pathio::named_temporary(&{_argv(a)}, &{_kwvec(kw)})"


def _tempdir(a, kw):
    """`TemporaryDirectory(suffix, prefix, dir, ignore_cleanup_errors, *, delete)`."""
    allowed = ("suffix", "prefix", "dir", "ignore_cleanup_errors", "delete")
    if len(a) > 4:
        raise ValueError("tempfile.TemporaryDirectory() takes at most 4 positional arguments")
    bad = [k for k in kw if k not in allowed]
    if bad:
        raise ValueError(f"tempfile.TemporaryDirectory({bad[0]}=) is not supported (supported: {', '.join(allowed)})")
    return f"{RT}::pathio::temporary_directory(&{_argv(a)}, &{_kwvec(kw)})"


def _noargs_call(name, a, kw, code):
    if a or kw:
        raise ValueError(f"{name}() takes no argument")
    return code


def _xl(kind, a, kw, allowed):
    """openpyxl's constructors: keyword arguments only, among those dynrt/xlsx.rs reproduces."""
    if a:
        raise ValueError(f"openpyxl {kind}() with positional arguments is not supported (pass them by keyword)")
    bad = [k for k in kw if k not in allowed]
    if bad:
        raise ValueError(f"openpyxl {kind}({bad[0]}=) is not supported (supported: {', '.join(allowed) or 'no argument'})")
    if kind == "Workbook" and "lxml" in LIB_VERSIONS:
        raise ValueError(f"openpyxl Workbook() with lxml {LIB_VERSIONS['lxml']} installed is not supported: openpyxl then "
                         "writes its XML through lxml, and only its ElementTree writer (et_xmlfile, without lxml) is "
                         "translated. Drop lxml from the lock, or leave the route to Python (--python-side)")
    fn = "workbook(" if kind == "Workbook" else f'style("{kind}", '
    return f"{RT}::xlsx::{fn}&{_argv(a)}, &{_kwvec(kw)})"


def _xl_letter(a, kw):
    if len(a) != 1 or kw:
        raise ValueError("get_column_letter() takes the column index, positionally")
    return f"{RT}::xlsx::get_column_letter(&{a[0]})"


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
def _pgpy_dir() -> str:
    """pgpy's site-packages directory as the transpiler finds it (nothing is imported): the path its warnings
    name on stderr, as a Rust string literal"""
    import importlib.util
    from pathlib import Path
    try:
        spec = importlib.util.find_spec("pgpy")
    except (ImportError, ValueError):
        spec = None
    locs = list(spec.submodule_search_locations or []) if spec is not None else []
    d = str(Path(locs[0]).parent) if locs else ""
    return '"' + d.replace("\\", "\\\\").replace('"', '\\"') + '"'


_PGPY_DIR = _pgpy_dir()

VALUES: dict[str, str] = {
    # subprocess.PIPE / STDOUT / DEVNULL, also asyncio.subprocess's (dynrt/subproc.rs)
    **{f"{m}.{n}": f"V::Int({v})" for m in ("subprocess", "asyncio.subprocess")
       for n, v in (("PIPE", -1), ("STDOUT", -2), ("DEVNULL", -3))},
    "sys.maxsize": "V::Int(i64::MAX)",  # CPython's on 64-bit platforms
    "datetime.UTC": f"V::Tz({RT}::dt::Tz::Utc)",
    # pytz 2026.5 (dynrt/pytzrt.rs)
    "pytz.utc": f"V::Tz({RT}::dt::Tz::PyUtc)",
    "pytz.UTC": f"V::Tz({RT}::dt::Tz::PyUtc)",
    **{f"pytz.{n}": f"{RT}::pytzrt::{n}()" for n in ("all_timezones", "all_timezones_set", "common_timezones", "common_timezones_set")},
    **{f"pytz.{n}": f'V::str("{v}")' for n, v in (("VERSION", "2026.5"), ("__version__", "2026.5"), ("OLSON_VERSION", "2026e"))},
    # python-dateutil 2.9 (dynrt/dutz.rs)
    **{f"{m}.UTC": f"V::Tz({RT}::dt::Tz::DuUtc)" for m in ("dateutil.tz", "dateutil.tz.tz")},
    "datetime.timezone.utc": f"V::Tz({RT}::dt::Tz::Utc)",
    "sqlalchemy.null": "V::None",
    **{f"sqlalchemy.{t}": f'V::str("{t}")' for t in ("String", "Text", "Unicode", "Integer", "BigInteger", "Float", "Date", "Boolean", "Numeric")},
    "os.environ": f"{RT}::libs::environ()",
    # redis connection classes (`connection_class=` of a pool)
    **{f"redis.{m}{c}": f'V::native({RT}::Native::ExtType("redis.{"asyncio." if m.startswith("asyncio") else ""}connection.{c}"))'
       for m in ("", "connection.", "asyncio.", "asyncio.connection.") for c in ("Connection", "SSLConnection")},
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
    # qrcode 8.2's error correction levels (dynrt/qr.rs)
    **{f"qrcode{m}.ERROR_CORRECT_{n}": f"V::Int({v})" for m in ("", ".constants")
       for n, v in (("L", 1), ("M", 0), ("Q", 3), ("H", 2))},
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
    # errno: the numbers glibc and macOS agree on (EAGAIN, ENOSYS... differ between the two: refused)
    **{f"errno.{n}": f"V::Int({v})" for n, v in (
        ("EPERM", 1), ("ENOENT", 2), ("ESRCH", 3), ("EINTR", 4), ("EIO", 5), ("ENXIO", 6), ("E2BIG", 7), ("ENOEXEC", 8),
        ("EBADF", 9), ("ECHILD", 10), ("ENOMEM", 12), ("EACCES", 13), ("EFAULT", 14), ("ENOTBLK", 15), ("EBUSY", 16),
        ("EEXIST", 17), ("EXDEV", 18), ("ENODEV", 19), ("ENOTDIR", 20), ("EISDIR", 21), ("EINVAL", 22), ("ENFILE", 23),
        ("EMFILE", 24), ("ENOTTY", 25), ("ETXTBSY", 26), ("EFBIG", 27), ("ENOSPC", 28), ("ESPIPE", 29), ("EROFS", 30),
        ("EMLINK", 31), ("EPIPE", 32), ("EDOM", 33), ("ERANGE", 34))},
    # the interpreter's constants that do not depend on the build (sys.warnoptions: no -W option, as under uvicorn)
    # fcntl.flock operations (<sys/file.h>: the same on Linux and macOS)
    **{f"fcntl.LOCK_{n}": f"V::Int({v})" for n, v in (("SH", 1), ("EX", 2), ("NB", 4), ("UN", 8))},
    "sys.modules": f"{RT}::libs::sys_modules()",
    # pgpy 0.6.0 (dynrt/pgpy.rs): the module and its PGPKey class as values (`return pgpy`)
    "pgpy": f"{RT}::pgpy::module({_PGPY_DIR})",
    "pgpy.PGPKey": f"{RT}::pgpy::key_class({_PGPY_DIR})",
    "sys.byteorder": 'V::str("little")',
    "sys.maxunicode": "V::Int(1114111)",
    "sys.warnoptions": "V::list(vec![])",
    # the abstract base classes of collections.abc as isinstance targets (types::abc_isinstance; the ABCs decided by a __subclasshook__ stay refused)
    **{f"collections.abc.{n}": f'V::native({RT}::Native::ExtType("collections.abc.{n}"))' for n in (
        "Mapping", "MutableMapping", "Sequence", "MutableSequence", "Set", "MutableSet")},
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
READ_ONLY_VALUES = {"os.environ", "sys.warnoptions"}
# library attributes a project may replace (monkeypatching the HTTP clients' request method to wrap it):
# the runtime's clients then call the replacement
HOOKS = {
    "aiohttp.ClientSession._request": f'{RT}::http::set_hook("aiohttp", {{val}})',
    "httpx.AsyncClient.request": f'{RT}::http::set_hook("httpx", {{val}})',
}
# library values that are run-time objects: their methods are called on the value
OBJECT_VALUES = {"prometheus_client.REGISTRY", "aiohttp.ClientSession._request", "httpx.AsyncClient.request",
                 # tzinfo singletons (`pytz.utc.zone`, `tz.UTC.utcoffset(None)`)
                 "pytz.utc", "pytz.UTC", "dateutil.tz.UTC", "dateutil.tz.tz.UTC"}

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
    "builtins.Warning": "WARNING",
    "builtins.UserWarning": "USER_WARNING",
    "builtins.DeprecationWarning": "DEPRECATION_WARNING",
    "builtins.PendingDeprecationWarning": "PENDING_DEPRECATION_WARNING",
    "builtins.SyntaxWarning": "SYNTAX_WARNING",
    "builtins.RuntimeWarning": "RUNTIME_WARNING",
    "builtins.FutureWarning": "FUTURE_WARNING",
    "builtins.ImportWarning": "IMPORT_WARNING",
    "builtins.UnicodeWarning": "UNICODE_WARNING",
    "builtins.BytesWarning": "BYTES_WARNING",
    "builtins.ResourceWarning": "RESOURCE_WARNING",
    "builtins.EncodingWarning": "ENCODING_WARNING",
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
    "builtins.BlockingIOError": "BLOCKING_IO_ERROR",
    "builtins.PermissionError": "PERMISSION_ERROR",
    "builtins.IsADirectoryError": "IS_A_DIRECTORY_ERROR",
    "builtins.ProcessLookupError": "PROCESS_LOOKUP_ERROR",
    "builtins.NotADirectoryError": "NOT_A_DIRECTORY_ERROR",
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
    **{f"starlette_context{m}.{n}": c for m in ("", ".errors") for n, c in (
        ("StarletteContextError", "STARLETTE_CONTEXT_ERROR"), ("ContextDoesNotExistError", "CONTEXT_DOES_NOT_EXIST_ERROR"))},
    **{f"google.auth.exceptions.{n}": f"GOOGLE_{c}" for n, c in (
        ("GoogleAuthError", "AUTH_ERROR"), ("TransportError", "TRANSPORT_ERROR"),
        ("DefaultCredentialsError", "DEFAULT_CREDENTIALS_ERROR"), ("MalformedError", "MALFORMED_ERROR"),
        ("InvalidValue", "INVALID_VALUE"))},
    "py_vapid.VapidException": "VAPID_EXCEPTION",
    **{f"passlib.exc.{n}": f"PASSLIB_{c}" for n, c in (
        ("UnknownHashError", "UNKNOWN_HASH_ERROR"), ("PasswordValueError", "PASSWORD_VALUE_ERROR"),
        ("PasswordSizeError", "PASSWORD_SIZE_ERROR"))},
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
        ("TooManyRedirects", "TOO_MANY_REDIRECTS"), ("DecodingError", "DECODING_ERROR"), ("HTTPStatusError", "STATUS_ERROR"), ("InvalidURL", "INVALID_URL"),
        ("WriteTimeout", "WRITE_TIMEOUT"), ("PoolTimeout", "POOL_TIMEOUT"), ("ReadError", "READ_ERROR"),
        ("WriteError", "WRITE_ERROR"), ("CloseError", "CLOSE_ERROR"), ("ProtocolError", "PROTOCOL_ERROR"),
        ("LocalProtocolError", "LOCAL_PROTOCOL_ERROR"), ("RemoteProtocolError", "REMOTE_PROTOCOL_ERROR"),
        ("ProxyError", "PROXY_ERROR"), ("CookieConflict", "COOKIE_CONFLICT"), ("StreamError", "STREAM_ERROR"),
        ("StreamConsumed", "STREAM_CONSUMED"), ("StreamClosed", "STREAM_CLOSED"), ("ResponseNotRead", "RESPONSE_NOT_READ"),
        ("RequestNotRead", "REQUEST_NOT_READ"))},
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
    "ipaddress.AddressValueError": "ADDRESS_VALUE_ERROR",
    "ipaddress.NetmaskValueError": "NETMASK_VALUE_ERROR",
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
    "starlette.exceptions.HTTPException": "STARLETTE_HTTP_EXCEPTION",
    "pydantic.ValidationError": "VALIDATION_ERROR",
    **{f"pydantic_core{m}.{n}": c for m in ("", "._pydantic_core") for n, c in (("PydanticCustomError", "PYDANTIC_CUSTOM_ERROR"),
                                                                               ("PydanticUseDefault", "PYDANTIC_USE_DEFAULT"),
                                                                               ("PydanticSerializationError", "PYDANTIC_SERIALIZATION_ERROR"))},
    "sqlalchemy.exc.SQLAlchemyError": "SQLALCHEMY_ERROR",
    "sqlalchemy.exc.DBAPIError": "DBAPI_ERROR",
    "sqlalchemy.exc.IntegrityError": "INTEGRITY_ERROR",
    "sqlalchemy.exc.OperationalError": "OPERATIONAL_ERROR",
    "sqlalchemy.exc.DataError": "DATA_ERROR",
    "sqlalchemy.exc.ProgrammingError": "PROGRAMMING_ERROR",
    "sqlalchemy.exc.InternalError": "INTERNAL_ERROR",
    "sqlalchemy.exc.NotSupportedError": "NOT_SUPPORTED_ERROR",
    "sqlalchemy.exc.InterfaceError": "INTERFACE_ERROR",
    "sqlalchemy.exc.NoResultFound": "NO_RESULT_FOUND",
    "sqlalchemy.exc.InvalidRequestError": "INVALID_REQUEST_ERROR",
    "sqlalchemy.exc.PendingRollbackError": "PENDING_ROLLBACK_ERROR",
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
    **{f"redis.{m}{n}": c for m in ("exceptions.", "") for n, c in (("RedisError", "REDIS_ERROR"), ("ConnectionError", "REDIS_CONNECTION_ERROR"),
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
    **{f"{m}.{n}": c for m in ("pytz", "pytz.exceptions") for n, c in (
        ("UnknownTimeZoneError", "PYTZ_UNKNOWN_TIME_ZONE_ERROR"), ("InvalidTimeError", "PYTZ_INVALID_TIME_ERROR"),
        ("AmbiguousTimeError", "PYTZ_AMBIGUOUS_TIME_ERROR"), ("NonExistentTimeError", "PYTZ_NON_EXISTENT_TIME_ERROR"))},
    "pytz.exceptions.Error": "PYTZ_ERROR",
    **{f"dateutil.parser{m}.ParserError": "DU_PARSER_ERROR" for m in ("", "._parser")},
    "qrcode.exceptions.DataOverflowError": "QRCODE_DATA_OVERFLOW_ERROR",
    **{f"{m}.{n}": c for m in ("croniter", "croniter.croniter") for n, c in (
        ("CroniterError", "CRONITER_ERROR"), ("CroniterBadCronError", "CRONITER_BAD_CRON_ERROR"),
        ("CroniterBadDateError", "CRONITER_BAD_DATE_ERROR"), ("CroniterBadTypeRangeError", "CRONITER_BAD_TYPE_RANGE_ERROR"),
        ("CroniterNotAlphaError", "CRONITER_NOT_ALPHA_ERROR"),
        ("CroniterUnsupportedSyntaxError", "CRONITER_UNSUPPORTED_SYNTAX_ERROR"))},
}

# keyword arguments refused at transpile time (the call is otherwise supported)
# redis clients (dynrt/rds.rs): the constructors, and the options they take (rds::OPTIONS, plus the address)
REDIS_CLIENTS = tuple(f"redis.{m}{c}" for m in ("", "client.", "asyncio.", "asyncio.client.") for c in ("Redis", "StrictRedis"))
REDIS_FROM_URL = ("redis.from_url", "redis.utils.from_url", "redis.asyncio.from_url", "redis.asyncio.utils.from_url",
                  "redis.asyncio.client.from_url", *(f"{c}.from_url" for c in REDIS_CLIENTS))
REDIS_POOL_FROM_URL = tuple(f"redis.{m}ConnectionPool.from_url" for m in ("", "connection.", "asyncio.", "asyncio.connection."))
REDIS_OPTIONS = {"decode_responses", "encoding", "retry", "retry_on_error", "retry_on_timeout", "health_check_interval",
                 "socket_keepalive", "socket_timeout", "socket_connect_timeout", "max_connections",
                 "single_connection_client", "client_name"}


def redis_static(name: str, node) -> None:
    """The options of a redis client or pool written in the call: the ones dynrt/rds.rs applies or that only tune
    the connection (ValueError otherwise); `encoding=` other than a literal UTF-8 is refused."""
    import ast
    allowed = REDIS_OPTIONS | ({"url"} if name.endswith("from_url") else
                               {"host", "port", "db", "password", "username", "connection_pool"})
    for k in node.keywords:
        if k.arg is None:
            raise ValueError("`**options` is not supported (write the options)")
        if k.arg not in allowed:
            raise ValueError(f"option {k.arg}= is not supported (only {', '.join(sorted(allowed))})")
        if k.arg == "encoding" and not (isinstance(k.value, ast.Constant) and isinstance(k.value.value, str)
                                        and k.value.value.lower().replace("-", "") == "utf8"):
            raise ValueError("encoding= other than a literal 'utf-8' is not supported")
    if node.args and not name.endswith("from_url"):
        raise ValueError("pass the address as keywords (host=, port=, db=...)")


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
    """The refusal of a library name newer than the project's Python (or than its locked library), None if it
    exists there."""
    v = SINCE.get(dotted)
    if v is None or PYTHON >= v:
        return None
    return (f"{dotted} needs Python {v[0]}.{v[1]} (the project runs on {PYTHON[0]}.{PYTHON[1]}, "
            "where it raises AttributeError)")


def stripe_error_missing(dotted: str) -> bool:
    """`stripe.error.X` under a locked stripe 14.0.0: the module's `__getattr__` has no `error` entry (added back in
    14.0.1), so evaluating `stripe.error` raises a bare AttributeError() (dynrt/stripe.rs `no_error_module`)."""
    if not dotted.startswith("stripe.error."):
        return False
    sv = LIB_VERSIONS.get("stripe", "14.4.1")
    return tuple(int(x) for x in re.findall(r"\d+", sv)[:3]) < (14, 0, 1)


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


def _instr_version() -> str:
    """prometheus_fastapi_instrumentator is ported from 8.x (7.x names partial and trailing-slash matches otherwise)."""
    v = LIB_VERSIONS.get("prometheus-fastapi-instrumentator")
    if v is not None and not v.startswith("8."):
        raise ValueError(f"prometheus_fastapi_instrumentator is ported from 8.x (the project locks {v})")
    return ""


def _starlette_request(a, kw) -> str:
    """`Request(scope, receive)` of the request being served; `Request(scope)` has no receive channel (its body
    raises when read). `send=` is unused by a Request."""
    v = _bind("Request", a, kw, ["scope", "receive", "send"], {"scope", "receive", "send"})
    if "scope" not in v:
        raise ValueError("Request() missing 1 required positional argument: 'scope'")
    ev = "".join(f"let _ = {v[k]}; " for k in ("receive", "send") if k in v)
    f = "request_from_scope" if "receive" in v else "request_from_scope_norecv"
    return f"{{ {ev}{RT}::rawasgi::{f}(cx, &{v['scope']}) }}"


def _arg1(name: str, a, kw, pname: str) -> str:
    """A function of one argument given positionally or by name (`copy.deepcopy(x)`; its memo= is refused)."""
    v = _bind(name.rpartition(".")[2], a, kw, [pname], {pname})
    if pname not in v:
        raise ValueError(f"{name}() missing 1 required positional argument: '{pname}'")
    return v[pname]


def _no_kw(name: str, kw, refused) -> str:
    """Refuses keyword arguments the runtime does not implement (Python 3.15's `missing_as_none=`...)."""
    for k in kw:
        if k in refused:
            raise ValueError(f"{name}({k}=) is not supported")
    return ""


def _ucd_same() -> str:
    """unicodedata's category/bidirectional/name tables come from the translating Python: refused when its Unicode
    version is not the project's (a property of an assigned character may change between versions)."""
    import unicodedata

    from . import ucdages

    want = ucdages.UNICODE.get(PYTHON)
    if want is not None and want != unicodedata.unidata_version:
        raise ValueError(f"unicodedata's tables are written from the translating Python (Unicode {unicodedata.unidata_version}), "
                         f"the project's Python {PYTHON[0]}.{PYTHON[1]} has Unicode {want}: translate with Python "
                         f"{PYTHON[0]}.{PYTHON[1]}")
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
    verify_only = {"RS256", "RS384", "RS512", "PS256", "PS384", "PS512"} if name.split(".")[-1] in {"decode", "decode_complete"} else set()
    bad = [x for x in found if x in PYJWT_ASYMMETRIC - verify_only]
    if bad:
        raise ValueError(f"algorithm {bad[0]!r} is not supported (HMAC HS256/HS384/HS512 and 'none', and RS*/PS* to "
                         "verify with a key from PyJWK / PyJWKClient: EC/EdDSA keys and signing with RSA are not reproduced)")


def _bcrypt5() -> bool:
    """the project locks (or the transpiler has) bcrypt 5: its hashpw refuses a password over 72 bytes"""
    v = LIB_VERSIONS.get("bcrypt")
    return v is None or int((v.split(".") + ["0"])[0] or 0) >= 5


def _passlib_bcrypt() -> str:
    """the bcrypt release passlib's backend loads (it decides what passlib logs, and whether it works at all)"""
    v = LIB_VERSIONS.get("bcrypt")
    if v is None:
        raise ValueError("passlib's bcrypt handler without the `bcrypt` package (none locked or installed) is not "
                         "supported: passlib then falls back to another backend")
    if tuple(int(x) for x in re.findall(r"\d+", v)[:2]) < (4, 0):
        raise ValueError(f"passlib's bcrypt handler over bcrypt {v} is not supported (bcrypt 4.0 or later)")
    return v


def passlib_static(name, node):
    """passlib.context.CryptContext: the literal configuration `schemes=["bcrypt"]` (positional or keyword),
    `deprecated="auto"` (or `["auto"]`, or absent), nothing else."""
    import ast
    if name != "passlib.context.CryptContext":
        return

    def lit(e):
        if isinstance(e, ast.Constant) and isinstance(e.value, str):
            return e.value
        if isinstance(e, (ast.List, ast.Tuple)) and all(isinstance(x, ast.Constant) and isinstance(x.value, str) for x in e.elts):
            return tuple(x.value for x in e.elts)
        return None
    if any(isinstance(a, ast.Starred) for a in node.args) or any(k.arg is None for k in node.keywords):
        raise ValueError("`*`/`**` arguments are not supported")
    if len(node.args) > 1:
        raise ValueError("only schemes= and deprecated= are supported")
    kws = {k.arg: k.value for k in node.keywords}
    for k in kws:
        if k not in ("schemes", "deprecated"):
            raise ValueError(f"{k}= is not supported (only schemes=[\"bcrypt\"] and deprecated=\"auto\": passlib's "
                             "bcrypt handler with its defaults is reproduced)")
    schemes = node.args[0] if node.args else kws.get("schemes")
    if schemes is None or lit(schemes) not in ("bcrypt", ("bcrypt",)):
        raise ValueError("schemes must be the literal [\"bcrypt\"] (of passlib, only the bcrypt handler is reproduced)")
    dep = kws.get("deprecated")
    if dep is not None and not (isinstance(dep, ast.Constant) and dep.value is None) and lit(dep) not in ("auto", ("auto",), ()):
        raise ValueError("deprecated must be \"auto\" (or [\"auto\"]) with the single bcrypt scheme")


def _relativedelta(a, kw):
    allowed = {"years", "months", "weeks", "days", "hours", "minutes", "seconds", "microseconds"}
    if a:
        raise ValueError("relativedelta(dt1, dt2) is not supported (relative keyword arguments only)")
    for k in kw:
        if k not in allowed:
            raise ValueError(f"relativedelta({k}=) is not supported (only {', '.join(sorted(allowed))})")
    return f"{RT}::reldelta::new(&{_argv(a)}, &{_kwvec(kw)})"


# cachetools: the arguments that must be one of the supported library functions (position, keyword, kind)
CT_MARKED = {
    **{f"cachetools.{n}": ((1, "getsizeof", "getsizeof"),) for n in ("Cache", "FIFOCache", "LRUCache")},
    "cachetools.TTLCache": ((2, "timer", "timer"), (3, "getsizeof", "getsizeof")),
    "cachetools.func.ttl_cache": ((2, "timer", "timer"), (3, "typed", "typed")),
    **{f"cachetools.func.{n}": ((1, "typed", "typed"),) for n in ("lru_cache", "fifo_cache")},
    "cachetools.cached": ((1, "key", "key"), (3, "condition", "condition")),
    "cachetools.cachedmethod": ((1, "key", "key"), (3, "condition", "condition"), (4, "info", "info")),
}


def du_parse_static(name, node):
    """dateutil.parser.parse(timestr, parserinfo=None, **kwargs): the default parserinfo only (a custom one is a
    class of the project the runtime's parser does not read)."""
    import ast
    if not name.endswith(".parse"):
        return
    pi = node.args[1] if len(node.args) > 1 else next((k.value for k in node.keywords if k.arg == "parserinfo"), None)
    if pi is not None and not (isinstance(pi, ast.Constant) and pi.value is None):
        raise ValueError("a parserinfo is not supported (the default parserinfo only)")


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
    # fastapi's security schemes as plain values (a settings default never used as a dependency): only stored.
    # Used as `Depends(scheme)` they are compiled by the route builder (dyn.RouteBuilder.security_call) instead
    **{f"fastapi.security{m}.{c}": (lambda mod, c, ps, npos, req: lambda a, kw: (
        f'{RT}::libs::lib_instance("{mod}", "{c}", &[{", ".join(chr(34) + p + chr(34) for p in ps)}], {npos}, '
        f'&[{", ".join(chr(34) + p + chr(34) for p in req)}], &{_argv(a)}, &{_kwvec(kw)})'))(mod, c, ps, npos, req)
       for c, mod, ps, npos, req in (
           ("OAuth2PasswordBearer", "fastapi.security.oauth2",
            ("tokenUrl", "scheme_name", "scopes", "description", "auto_error", "refreshUrl"), 6, ("tokenUrl",)),
           ("HTTPBearer", "fastapi.security.http", ("bearerFormat", "scheme_name", "description", "auto_error"), 0, ()),
           ("HTTPBasic", "fastapi.security.http", ("scheme_name", "realm", "description", "auto_error"), 0, ()))
       for m in ("", "." + mod.rsplit(".", 1)[1])},
    # PyJWT 2.15's JWKs (dynrt/jwks.rs): PyJWKClient, PyJWK, PyJWKSet, verification with RS*/PS* keys
    **{f"jwt{m}.PyJWKClient": (lambda a, kw: _pyjwt_215("PyJWKClient") or f"{RT}::jwks::client_new(&{_argv(a)}, &{_kwvec(kw)})")
       for m in ("", ".jwks_client")},
    **{f"jwt{m}.PyJWK{f}": (lambda a, kw: _pyjwt_215("PyJWK") or _pyjwk(a, kw)) for m in ("", ".api_jwk") for f in ("", ".from_dict")},
    **{f"jwt{m}.PyJWKSet.from_dict": (lambda a, kw: _pyjwt_215("PyJWKSet") or
                                      f"{RT}::jwks::set_value({RT}::jwks::set_from_dict(&{_bind('PyJWKSet.from_dict', a, kw, ['obj'], {'obj'})['obj']}))")
       for m in ("", ".api_jwk")},
    **{f"jwt{m}.PyJWKSet": (lambda a, kw: _pyjwt_215("PyJWKSet") or
                            f"{RT}::jwks::set_value({RT}::jwks::set_new(&{_bind('PyJWKSet', a, kw, ['keys'], {'keys'})['keys']}))")
       for m in ("", ".api_jwk")},
    # gc (dynrt/gcmod.rs): the binary has no cycle collector; thresholds and the enabled flag are kept
    **{f"gc.{n}": (lambda n: lambda a, kw: f'{RT}::gcmod::call("{n}", &{_argv(a)}, &{_kwvec(kw)})')(n)
       for n in ("collect", "enable", "disable", "isenabled", "freeze", "unfreeze", "get_freeze_count", "get_count",
                 "get_threshold", "set_threshold")},
    # python-dotenv 1.2 (dynrt/dotenv.rs)
    **{f"dotenv{m}.{f}": (lambda f: lambda a, kw: f"{RT}::dotenv::{f}({RT}::dotenv::Rev::of({_dotenv_patch()}), "
                                                  f"&{_argv(a)}, &{_kwvec(kw)})")(f)
       for m in ("", ".main") for f in ("load_dotenv", "dotenv_values")},
    **{c + ("." + k if k else ""): (lambda f: lambda a, kw: f"{RT}::croniter::{f}(&{_argv(a)}, &{_kwvec(kw)})")(_CRON_FN[k])
       for c in _CRON_CLASSES for k in ("", "is_valid", "match", "match_range", "expand", "datetime_to_timestamp")},
    **{f"{m}.{k}": (lambda f: lambda a, kw: f"{RT}::croniter::{f}(&{_argv(a)}, &{_kwvec(kw)})")(_CRON_FN[k])
       for m in _CRON_MODULES for k in ("croniter_range", "datetime_to_timestamp")},
    # python-dateutil 2.9 (dynrt/reldelta.rs)
    "dateutil.relativedelta.relativedelta": _relativedelta,
    # cachetools 7.2 (dynrt/ctools.rs); the timer, getsizeof and key arguments checked by dyn.FnCompiler.ct_markers
    **{f"cachetools.{n}": (lambda n: lambda a, kw: f'{RT}::ctools::cache_new("{n}", &{_argv(a)}, &{_kwvec(kw)})')(n)
       for n in ("Cache", "FIFOCache", "LRUCache")},
    "cachetools.TTLCache": lambda a, kw: f"{RT}::ctools::ttl_new(&{_argv(a)}, &{_kwvec(kw)})",
    "cachetools.cached": lambda a, kw: f"{RT}::ctools::cached(&{_argv(a)}, &{_kwvec(kw)})",
    "cachetools.cachedmethod": lambda a, kw: f"{RT}::ctools::cachedmethod(&{_argv(a)}, &{_kwvec(kw)})",
    "cachetools.keys.hashkey": lambda a, kw: f"{RT}::ctools::hashkey_fn(&{_argv(a)}, &{_kwvec(kw)})",
    "cachetools.keys.methodkey": lambda a, kw: f"{RT}::ctools::methodkey_fn(&{_argv(a)}, &{_kwvec(kw)})",
    **{f"cachetools.func.{n}": (lambda n: lambda a, kw: f'{RT}::ctools::func_cache("{n}", &{_argv(a)}, &{_kwvec(kw)})')(n)
       for n in ("lru_cache", "fifo_cache", "ttl_cache")},
    # dateutil.parser (dynrt/duparse.rs)
    **{f"dateutil.parser{m}.isoparse": (lambda a, kw: f"{RT}::duparse::isoparse(&{_argv(a)}, &{_kwvec(kw)})") for m in ("", ".isoparser")},
    **{f"dateutil.parser{m}.isoparser": (lambda a, kw: f"{RT}::duparse::isoparser_new(&{_argv(a)}, &{_kwvec(kw)})") for m in ("", ".isoparser")},
    # dateutil.tz (dynrt/dutz.rs)
    **{f"{m}.{n}": (lambda f: lambda a, kw: f"{RT}::dutz::{f}(&{_argv(a)}, &{_kwvec(kw)})")(f)
       for m in ("dateutil.tz", "dateutil.tz.tz") for n, f in (
           ("tzoffset", "tzoffset"), ("gettz", "gettz"), ("tzstr", "tzstr"), ("enfold", "enfold"),
           ("datetime_exists", "datetime_exists"), ("datetime_ambiguous", "datetime_ambiguous"),
           ("resolve_imaginary", "resolve_imaginary"))},
    **{f"{m}.tzutc": _noargs("tzutc", f"Ok::<V, Exc>(V::Tz({RT}::dt::Tz::DuUtc))") for m in ("dateutil.tz", "dateutil.tz.tz")},
    **{f"{m}.tzlocal": _noargs("tzlocal", f"Ok::<V, Exc>({RT}::dutz::tzlocal())") for m in ("dateutil.tz", "dateutil.tz.tz")},
    # pytz 2026.5 (dynrt/pytzrt.rs)
    "pytz.timezone": lambda a, kw: f"{RT}::pytzrt::timezone(&{_argv(a)}, &{_kwvec(kw)})",
    "pytz.FixedOffset": lambda a, kw: f"{RT}::pytzrt::fixed_offset(&{_argv(a)}, &{_kwvec(kw)})",
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
    # redis.asyncio and redis (dynrt/rds.rs): the same client, `sync` for redis's
    **{n: (lambda sync: lambda a, kw: f"{RT}::rds::from_url(&{_argv(a)}, &{_kwvec(kw)}, {sync})")(
        "false" if n.startswith("redis.asyncio") else "true") for n in REDIS_FROM_URL},
    **{n: (lambda sync: lambda a, kw: f"{RT}::rds::new(&{_argv(a)}, &{_kwvec(kw)}, {sync})")(
        "false" if n.startswith("redis.asyncio") else "true") for n in REDIS_CLIENTS},
    **{n: (lambda sync: lambda a, kw: f"{RT}::rds::pool_from_url(&{_argv(a)}, &{_kwvec(kw)}, {sync})")(
        "false" if n.startswith("redis.asyncio") else "true") for n in REDIS_POOL_FROM_URL},
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
    # prometheus_fastapi_instrumentator (dynrt/instr.rs): .instrument(app) / .expose(app) are compiled by the
    # translator (the app is the application itself)
    **{n: (lambda a, kw: _instr_version() or f"{RT}::instr::new(&{_argv(a)}, &{_kwvec(kw)})") for n in (
        "prometheus_fastapi_instrumentator.Instrumentator", "prometheus_fastapi_instrumentator.PrometheusFastApiInstrumentator",
        "prometheus_fastapi_instrumentator.instrumentation.PrometheusFastApiInstrumentator")},
    # copy (dynrt/copymod.rs): a class's own __copy__/__deepcopy__/__reduce__ is refused (Project.copies_objects)
    "copy.copy": lambda a, kw: f"{RT}::copymod::copy(&{_arg1('copy.copy', a, kw, 'x')})",
    "copy.deepcopy": lambda a, kw: f"{RT}::copymod::deepcopy(&{_arg1('copy.deepcopy', a, kw, 'x')})",
    # urllib.parse
    **{f"urllib.parse.{f}": (lambda f: lambda a, kw: _no_kw(f, kw, ("missing_as_none",))
                             or f"{RT}::stdlib::urllib(\"{f}\", &{_argv(a)}, &{_kwvec(kw)})")(f)
       for f in ("quote", "quote_plus", "unquote", "unquote_plus", "urlencode", "urlparse", "urlsplit", "parse_qs", "parse_qsl",
                 "urljoin")},
    # urlunsplit/urlunparse: Python 3.15's keep_empty= is refused
    **{f"urllib.parse.{f}": (lambda f: lambda a, kw: _no_kw(f, kw, ("keep_empty",))
                             or f"{RT}::stdlib::urllib(\"{f}\", &{_argv(a)}, &{_kwvec(kw)})")(f)
       for f in ("urlunsplit", "urlunparse")},
    # bcrypt 4.3 / 5.0 (5.0 refuses a password over 72 bytes), pyotp 2.9
    **{f"bcrypt.{f}": (lambda f: lambda a, kw: f"{RT}::auth::call(\"{f}\", {str(_bcrypt5()).lower()}, &{_argv(a)}, &{_kwvec(kw)})")(f)
       for f in ("gensalt", "hashpw", "checkpw")},
    # passlib 1.7.4: CryptContext(schemes=["bcrypt"]) over the bcrypt package (passlib_static checks the call)
    "passlib.context.CryptContext": lambda a, kw: (list(a), kw.items(), f"{RT}::auth::passlib_context({_passlib_bcrypt()!r})".replace("'", '"'))[2],
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
    "secrets.token_bytes": lambda a, kw: f"{RT}::libs::token_bytes(&{a[0] if a else kw.get('nbytes', 'V::None')})",
    "secrets.randbelow": lambda a, kw: f"{RT}::libs::randbelow(&{a[0] if a else kw['exclusive_upper_bound']})",
    "secrets.randbits": lambda a, kw: f"{RT}::libs::randbits(&{a[0] if a else kw['k']})",
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
    # category, bidirectional, name: tables of the project's Unicode version, written into the crate (dyn.ucd_tables)
    "unicodedata.category": lambda a, kw: _ucd_same() or (f"{RT}::stdlib::unicodedata_prop(\"category\", self::UCD_CATEGORY, \"Cn\", &{a[0]})" if len(a) == 1 and not kw else (_ for _ in ()).throw(ValueError("category(chr): one positional argument"))),
    "unicodedata.bidirectional": lambda a, kw: _ucd_same() or (f"{RT}::stdlib::unicodedata_prop(\"bidirectional\", self::UCD_BIDI, \"\", &{a[0]})" if len(a) == 1 and not kw else (_ for _ in ()).throw(ValueError("bidirectional(chr): one positional argument"))),
    "unicodedata.name": lambda a, kw: _ucd_same() or (f"{RT}::stdlib::unicodedata_name(&self::UCD_NAMES, &{_argv(a)})" if 1 <= len(a) <= 2 and not kw else (_ for _ in ()).throw(ValueError("name(chr[, default]): positional arguments"))),
    "unicodedata.decomposition": lambda a, kw: _ucd_same() or (f"{RT}::stdlib::unicodedata_decomposition(self::UCD_DECOMP, &{a[0]})" if len(a) == 1 and not kw else (_ for _ in ()).throw(ValueError("decomposition(chr): one positional argument"))),
    "unicodedata.combining": lambda a, kw: f"{RT}::stdlib::unicodedata_combining(self::UCD_UNASSIGNED, &{a[0]})" if len(a) == 1 and not kw else (_ for _ in ()).throw(ValueError("combining(chr): one positional argument")),
    **{f"bisect.{n}": (lambda n, left, f: lambda a, kw: f"{RT}::bisect::{f}(cx, {rs_(n)}, {left}, &{_argv(a)}, &{_kwvec(kw)}).await")(n, l, f)
       for n, l, f in (("bisect_left", "true", "bisect"), ("bisect_right", "false", "bisect"), ("bisect", "false", "bisect"),
                       ("insort_left", "true", "insort"), ("insort_right", "false", "insort"), ("insort", "false", "insort"))},
    "collections.deque": lambda a, kw: f"{RT}::deque::new(&{_argv(a)}, &{_kwvec(kw)})",
    "socket.getaddrinfo": lambda a, kw: f"{RT}::net::getaddrinfo(&{_argv(a)}, &{_kwvec(kw)})",
    "collections.Counter": lambda a, kw: f"{RT}::ops::counter(&{_argv(a)}, &{_kwvec(kw)})",
    "zipfile.ZipFile": lambda a, kw: _zipfile(a, kw),
    # qrcode 8.2 with Pillow's PilImage (dynrt/qr.rs): QRCode and make; their methods in the runtime
    **{f"qrcode{m}.{n}": (lambda n: lambda a, kw: _qrcode(n, a, kw))(n) for m in ("", ".main") for n in ("QRCode", "make")},
    # tempfile: a named file, mode w/w+ (dynrt/pathio.rs)
    "tempfile.NamedTemporaryFile": lambda a, kw: _tempfile(a, kw),
    "tempfile.TemporaryDirectory": lambda a, kw: _tempdir(a, kw),
    "tempfile.gettempdir": lambda a, kw: _noargs_call("tempfile.gettempdir", a, kw, f"{RT}::pathio::gettempdir().map(V::str)"),
    # openpyxl 3.1 (no lxml): Workbook(), styles, get_column_letter (dynrt/xlsx.rs)
    **{f"openpyxl{m}.Workbook": lambda a, kw: _xl("Workbook", a, kw, ()) for m in ("", ".workbook", ".workbook.workbook")},
    **{f"openpyxl.styles{m}.{k}": (lambda k, ok: lambda a, kw: _xl(k, a, kw, ok))(k, ok)
       for k, mods, ok in (("Font", (".fonts",), ("name", "size", "sz", "bold", "b", "italic", "i", "strike", "color")),
                           ("PatternFill", (".fills",), ("patternType", "fill_type", "fgColor", "start_color", "bgColor", "end_color")),
                           ("Side", (".borders",), ("style", "border_style", "color")),
                           ("Border", (".borders",), ("left", "right", "top", "bottom", "diagonal")),
                           ("Alignment", (".alignment",), ("horizontal", "vertical", "wrap_text", "wrapText", "shrink_to_fit", "shrinkToFit")))
       for m in ("",) + mods},
    **{f"openpyxl.utils{m}.get_column_letter": lambda a, kw: _xl_letter(a, kw) for m in ("", ".cell")},
    "socket.create_connection": lambda a, kw: (
        f"{RT}::net::create_connection(&{_argv(a)}, &{_kwvec(_checked('socket.create_connection', a, kw, ['address', 'timeout']))}).await"),
    # uvicorn runs on uvloop when the project installs it (`uvicorn[standard]`): its error messages differ
    "asyncio.open_connection": lambda a, kw: (
        f"{RT}::net::open_connection(&{_argv(a)}, &{_kwvec(_checked('asyncio.open_connection', a, kw, ['host', 'port']))}, "
        f"{str('uvloop' in LIB_VERSIONS).lower()}).await"),
    # array.array: typed arrays of numbers (dynrt/arr.rs)
    "array.array": lambda a, kw: _array_new(a, kw),
    "ipaddress.ip_address": lambda a, kw: f"{RT}::net::ip_address(&{_argv(a)})",
    "ipaddress.IPv4Address": lambda a, kw: f"{RT}::net::ip_typed(false, &{_argv(a)})" if not kw else (_ for _ in ()).throw(ValueError("IPv4Address(address): one positional argument")),
    "ipaddress.IPv6Address": lambda a, kw: f"{RT}::net::ip_typed(true, &{_argv(a)})" if not kw else (_ for _ in ()).throw(ValueError("IPv6Address(address): one positional argument")),
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
    "gzip.compress": lambda a, kw: f"{RT}::stdlib::gzip_compress(&{_argv(a)}, &{_kwvec(kw)})",
    "traceback.extract_tb": lambda a, kw: f"{RT}::trace::extract_tb(&{_argv(a)}, &{_kwvec(kw)})",
    "traceback.format_exception": lambda a, kw: f"{RT}::trace::format_exception(cx, &{_argv(a)}, &{_kwvec(kw)})",
    "traceback.format_exception_only": lambda a, kw: f"{RT}::trace::format_exception_only(&{_argv(a)}[0])",
    "traceback.format_exc": lambda a, kw: f"{RT}::trace::format_exc_now(cx, &{_argv(a)}, &{_kwvec(kw)})",
    "timeit.default_timer": lambda a, kw: f"{RT}::stdlib::time_now(\"perf_counter\")",
    **{f"os.path.{n}": (lambda n: lambda a, kw: f"{RT}::stdlib::os_path(\"{n}\", &{_argv(a)})")(n)
       for n in ("exists", "isfile", "isdir", "join", "basename", "dirname", "splitext", "normpath", "abspath", "realpath")},
    **{f"os.{n}": (lambda n: lambda a, kw: f"{RT}::pathio::os_remove(\"{n}\", &{_argv(a)})"
                   if not kw else (_ for _ in ()).throw(ValueError(f"os.{n}(dir_fd=) is not supported")))(n)
       for n in ("remove", "unlink")},
    "fcntl.flock": lambda a, kw: f"{RT}::pathio::flock(&{_argv(a)}, &{_kwvec(kw)})",
    "hmac.new": lambda a, kw: f"{RT}::stdlib::hmac_new(&{_argv(a)}, &{_kwvec(kw)})",
    "hmac.compare_digest": lambda a, kw: f"{RT}::libs::compare_digest(&{a[0]}, &{a[1]})",
    # calendar, types
    "calendar.monthrange": lambda a, kw: f"{RT}::libs::monthrange(&{a[0]}, &{a[1]})",
    "types.ModuleType": lambda a, kw: f"{RT}::libs::module_type(&{_argv(a)}, &{_kwvec(kw)})",
    # pgpy 0.6.0: PGPKey.from_blob(blob) only (dynrt/pgpy.rs, pgpycore.rs)
    "pgpy.PGPKey.from_blob": lambda a, kw: f"{{ let _ = {RT}::pgpy::key_class({_PGPY_DIR}); {RT}::pgpy::from_blob(&{_argv(a)}, &{_kwvec(kw)}) }}",
    "types.SimpleNamespace": lambda a, kw: f"{RT}::libs::namespace(&{_argv(a)}, &{_kwvec(kw)})",
    "operator.index": lambda a, kw: _no_kw("operator.index", kw, kw) or f"{RT}::libs::operator_index(&{_argv(a)})",
    "threading.local": lambda a, kw: f"{RT}::libs::thread_local(&{_argv(a)}, &{_kwvec(kw)})",
    # FastAPI's SSE event (yielded by a generator endpoint with response_class=EventSourceResponse)
    "fastapi.sse.ServerSentEvent": lambda a, kw: f"{RT}::web::sse_event(&{_argv(a)}, &{_kwvec(kw)})",
    # pathlib, uuid
    **{f"pathlib.{n}": (lambda a, kw: f"{RT}::pathio::path_new(&{_argv(a)})") for n in ("Path", "PurePath", "PosixPath", "PurePosixPath")},
    **{f"pathlib.{n}.cwd": _noargs(f"{n}.cwd", f"{RT}::pathio::cwd(true)") for n in ("Path", "PosixPath")},
    "os.getcwd": _noargs("os.getcwd", f"{RT}::pathio::cwd(false)"),
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
    "contextlib.contextmanager": lambda a, kw: f"{RT}::agen::contextmanager(&{a[0]})",
    # mcp 2.2 (dynrt/mcp.rs): the server object; its tools, transport options and attributes are checked
    # by the transpiler (dyn.mcp_tools)
    "mcp.server.mcpserver.MCPServer": lambda a, kw: _mcp_server(a, kw),
    # `Request(scope, receive)` inside a raw ASGI app (dynrt/rawasgi.rs): the request being served
    "starlette.requests.Request": lambda a, kw: _starlette_request(a, kw),
    "fastapi.Request": lambda a, kw: f"{{ let _ = {a[1]}; {RT}::rawasgi::request_from_scope(cx, &{a[0]}) }}",
    "mcp.server.transport_security.TransportSecuritySettings": lambda a, kw: _mcp_security(a, kw),
    "contextvars.ContextVar": lambda a, kw: f"{RT}::agen::context_var(&{_argv(a)}, &{_kwvec(kw)})",
    "contextlib.suppress": lambda a, kw: f"Ok::<V, Exc>(V::native({RT}::Native::Suppress({_argv(a)})))",
    # FastAPI / Starlette
    "fastapi.HTTPException": _http_exc,
    "fastapi.exceptions.HTTPException": _http_exc,
    "starlette.exceptions.HTTPException": lambda a, kw: _http_exc(a, kw).replace("::http_exc(", "::http_exc_starlette("),
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
    # markers on a def or class: the object itself (CPython also sets an introspection attribute on it)
    **{f"{m}.{n}": (lambda a, kw: f"Ok::<V, Exc>({a[0]})") for m in ("typing", "typing_extensions") for n in ("final", "override", "no_type_check")},
    "typing.cast": lambda a, kw: (a[0], f"Ok::<V, Exc>({a[1]})")[1],
}

# namespaces whose attribute calls are generic: `func.count(...)` -> orm::func("count", ...)
NAMESPACE_CALLS = {
    # the function name quoted alone (`func.date_trunc(text("'day'"), ...)`: an argument may hold a `'`)
    "sqlalchemy.func": lambda name, a, kw: f"{RT}::orm::func({json.dumps(name)}, {_argv(a)})",
    "sqlalchemy.sql.func": lambda name, a, kw: f"{RT}::orm::func({json.dumps(name)}, {_argv(a)})",
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
    # redis clients and pools, also as values called with keyword arguments (`pool_class(host=..., ...)`, aiocache)
    **{n: (lambda sync: lambda a, kw: f"{RT}::rds::new(&{a}, &{kw}, {sync})")(
        "false" if n.startswith("redis.asyncio") else "true") for n in REDIS_CLIENTS},
    **{f"redis.{m}ConnectionPool": (lambda sync: lambda a, kw: f"{RT}::rds::pool_new(&{a}, &{kw}, {sync})")(
        "false" if m.startswith("asyncio") else "true") for m in ("", "connection.", "asyncio.", "asyncio.connection.")},
    "asyncio.as_completed": lambda a, kw: f"{RT}::aio::as_completed(cx, {a}, &{kw})",
    "asyncio.Semaphore": lambda a, kw: f"{RT}::aio::semaphore(&{a}, &{kw})",
    "asyncio.BoundedSemaphore": lambda a, kw: f"{RT}::aio::semaphore(&{a}, &{kw})",
    "asyncio.Lock": lambda a, kw: f"{RT}::aio::semaphore(&{a}, &{kw})",
    # dateutil.parser.parse(s, **options) (dynrt/duparse.rs)
    **{f"dateutil.parser{m}.parse": (lambda a, kw: f"{RT}::duparse::parse(cx, {a}, {kw}).await") for m in ("", "._parser")},
    # case(*whens, else_=...): the (condition, value) tuples of a list built at run time
    "sqlalchemy.case": lambda a, kw: f"{RT}::orm::case(&{a}, &{kw})",
    # create_subprocess_exec(*cmd, stdout=PIPE): the OSError of a failed spawn names the file under asyncio, not
    # under uvloop (uvicorn runs on it when the project installs it)
    **{f"asyncio{m}.create_subprocess_exec": (lambda a, kw: f"{RT}::subproc::exec(&{a}, &{kw}, "
                                              f"{str('uvloop' in LIB_VERSIONS).lower()}).await")
       for m in ("", ".subprocess")},
}
# stripe-python 14 (dynrt/stripe.rs): every resource classmethod of the release (`stripe.Customer.create(...)`),
# by the number of positional ids it is given (py2axum/stripetab.py, tools/gen_stripe.py); `*_async`, StripeClient
# and the instance methods (`customer.save()`) are not mapped
from . import stripetab as _stripetab  # noqa: E402


def _stripe_method(path: str):
    arms = ", ".join(f"({n}, {i})" for n, i in sorted(_stripetab.METHODS[path].items()))
    return lambda a, kw: f"{RT}::stripe::call(cx, &[{arms}], {a}, {kw}).await"


DYN_ARGS.update({p: _stripe_method(p) for p in _stripetab.METHODS})
DYN_ARGS["stripe.Webhook.construct_event"] = lambda a, kw: f"{RT}::stripe::construct_event(cx, {a}, {kw}).await"
DYN_ARGS["stripe.WebhookSignature.verify_header"] = lambda a, kw: f"{RT}::stripe::verify_header_call(cx, {a}, {kw}).await"
# the module configuration the binary keeps (read and assigned, at module level or in a function)
STRIPE_CONF = ("api_key", "api_base", "api_version", "max_network_retries", "enable_telemetry", "log")
VALUES.update({f"stripe.{n}": f'{RT}::stripe::conf("{n}")' for n in STRIPE_CONF + ("VERSION",)})
VALUES["stripe.Webhook.DEFAULT_TOLERANCE"] = "V::Int(300)"
VALUES["secrets.DEFAULT_ENTROPY"] = "V::Int(32)"
HOOKS.update({f"stripe.{n}": f'{RT}::stripe::set_conf("{n}", {{val}})' for n in STRIPE_CONF})
_STRIPE_ERRORS = {"StripeError": "STRIPE_ERROR", "APIError": "STRIPE_API_ERROR", "APIConnectionError": "STRIPE_API_CONNECTION_ERROR",
                  "StripeErrorWithParamCode": "STRIPE_ERROR_WITH_PARAM_CODE", "CardError": "STRIPE_CARD_ERROR",
                  "IdempotencyError": "STRIPE_IDEMPOTENCY_ERROR", "InvalidRequestError": "STRIPE_INVALID_REQUEST_ERROR",
                  "AuthenticationError": "STRIPE_AUTHENTICATION_ERROR", "PermissionError": "STRIPE_PERMISSION_ERROR",
                  "RateLimitError": "STRIPE_RATE_LIMIT_ERROR", "SignatureVerificationError": "STRIPE_SIGNATURE_VERIFICATION_ERROR",
                  "TemporarySessionExpiredError": "STRIPE_TEMPORARY_SESSION_EXPIRED_ERROR"}
EXCEPTIONS.update({f"{m}.{n}": c for n, c in _STRIPE_ERRORS.items() for m in ("stripe", "stripe.error", "stripe._error")})
EXCEPTIONS.update({f"stripe.oauth_error.{n}": c for n, c in (
    ("OAuthError", "STRIPE_OAUTH_ERROR"), ("InvalidClientError", "STRIPE_OAUTH_INVALID_CLIENT"),
    ("InvalidGrantError", "STRIPE_OAUTH_INVALID_GRANT"), ("InvalidRequestError", "STRIPE_OAUTH_INVALID_REQUEST"),
    ("InvalidScopeError", "STRIPE_OAUTH_INVALID_SCOPE"), ("UnsupportedGrantTypeError", "STRIPE_OAUTH_UNSUPPORTED_GRANT_TYPE"),
    ("UnsupportedResponseTypeError", "STRIPE_OAUTH_UNSUPPORTED_RESPONSE_TYPE"))})

# reportlab 4.4.5, pure-Python path (dynrt/rl/, bound in dynrt/rlbind.rs): SimpleDocTemplate into an io.BytesIO,
# Paragraph and its styles, Spacer, PageBreak, HRFlowable, Table and TableStyle, colours, the graphics shapes; the
# parameters below are the reproduced ones, any other is refused here
_RL_STYLE_KW = ("fontName", "fontSize", "leading", "leftIndent", "rightIndent", "firstLineIndent", "alignment",
                "spaceBefore", "spaceAfter", "bulletFontName", "bulletFontSize", "bulletIndent", "textColor", "backColor",
                "wordWrap", "borderWidth", "borderPadding", "borderColor", "borderRadius", "allowWidows", "allowOrphans",
                "textTransform", "endDots", "splitLongWords", "bulletAnchor", "justifyLastLine", "justifyBreaks",
                "spaceShrinkage", "hyphenationLang", "embeddedHyphenation", "uriWasteReduce", "linkUnderline", "shaping",
                "autoLeading", "keepWithNext", "bulletText")
_RL_DOC_KW = ("pagesize", "leftMargin", "rightMargin", "topMargin", "bottomMargin", "title", "author", "subject",
              "creator", "producer", "keywords", "invariant", "pageCompression", "allowSplitting", "rotation",
              "initialFontName", "initialFontSize", "initialLeading")
_RL_LINE_KW = ("strokeColor", "strokeWidth", "strokeLineCap", "strokeLineJoin", "strokeMiterLimit", "strokeDashArray",
               "strokeOpacity")
_RL_SOLID_KW = ("fillColor", "fillOpacity", "fillMode") + _RL_LINE_KW
# class -> (modules, positional parameters, other keywords, *args)
_RL_CLASSES = {
    "SimpleDocTemplate": ((".platypus", ".platypus.doctemplate"), ("filename",), _RL_DOC_KW, False),
    "Paragraph": ((".platypus", ".platypus.paragraph"), ("text", "style"), (), False),
    "Spacer": ((".platypus", ".platypus.flowables"), ("width", "height"), (), False),
    "PageBreak": ((".platypus", ".platypus.flowables"), (), (), False),
    "HRFlowable": ((".platypus", ".platypus.flowables"),
                   ("width", "thickness", "lineCap", "color", "spaceBefore", "spaceAfter", "hAlign", "vAlign", "dash"), (), False),
    "Table": ((".platypus", ".platypus.tables"), ("data", "colWidths", "rowHeights", "style", "repeatRows"),
              ("hAlign", "vAlign", "spaceBefore", "spaceAfter"), False),
    "TableStyle": ((".platypus", ".platypus.tables"), ("cmds",), (), False),
    "ParagraphStyle": ((".lib.styles",), ("name", "parent"), _RL_STYLE_KW, False),
    "getSampleStyleSheet": ((".lib.styles",), (), (), False),
    "HexColor": ((".lib.colors",), ("val",), (), False),
    "Color": ((".lib.colors",), ("red", "green", "blue", "alpha"), (), False),
    "landscape": ((".lib.pagesizes",), ("pagesize",), (), False),
    "portrait": ((".lib.pagesizes",), ("pagesize",), (), False),
    "Drawing": ((".graphics.shapes",), ("width", "height"), ("hAlign", "vAlign", "renderScale"), True),
    "Group": ((".graphics.shapes",), (), (), True),
    "Rect": ((".graphics.shapes",), ("x", "y", "width", "height", "rx", "ry"), _RL_SOLID_KW, False),
    "Circle": ((".graphics.shapes",), ("cx", "cy", "r"), _RL_SOLID_KW, False),
    "Ellipse": ((".graphics.shapes",), ("cx", "cy", "rx", "ry"), _RL_SOLID_KW, False),
    "Polygon": ((".graphics.shapes",), ("points",), _RL_SOLID_KW, False),
    "Line": ((".graphics.shapes",), ("x1", "y1", "x2", "y2"), _RL_LINE_KW, False),
    "PolyLine": ((".graphics.shapes",), ("points",), _RL_LINE_KW, False),
}
# distributions that change what reportlab writes (C accelerators of fp_str, escapes, widths; the bitmap renderer)
RL_REFUSED_DISTS = ("rl-accel", "rl-renderpm")


def _rl(kind, a, kw):
    _mods, params, other, star = _RL_CLASSES[kind]
    if len(a) > len(params) and not star:
        raise ValueError(f"reportlab {kind}() takes at most {len(params)} positional arguments here")
    bad = [k for k in kw if k not in params and k not in other]
    if bad:
        allowed = ", ".join(params + other) or "no argument"
        raise ValueError(f"reportlab {kind}({bad[0]}=) is not supported (supported: {allowed})")
    if kind in ("SimpleDocTemplate", "Paragraph"):
        for d in RL_REFUSED_DISTS:
            if d in LIB_VERSIONS:
                raise ValueError(f"reportlab {kind}() with {d} {LIB_VERSIONS[d]} installed is not supported: its C code "
                                 "replaces the pure-Python functions the binary reproduces. Drop it from the lock, or "
                                 "leave the route to Python (--python-side)")
    if kind in ("HexColor", "Color"):
        return f'{RT}::rlbind::color("{kind}", &{_argv(a)}, &{_kwvec(kw)})'
    if kind in ("landscape", "portrait"):
        if len(a) != 1 or kw:
            raise ValueError(f"{kind}(pagesize): one positional argument")
        return f"{RT}::rlbind::orient({str(kind == 'landscape').lower()}, &{_argv(a)})"
    return f'{RT}::rlbind::new("{kind}", &{_argv(a)}, &{_kwvec(kw)})'


def _rl_colors() -> tuple[str, ...]:
    """The colour names of reportlab.lib.colors, as the engine's table has them (dynrt/rl/fontdata.rs)."""
    from pathlib import Path

    src = (Path(__file__).parent / "runtime" / "dynrt" / "rl" / "fontdata.rs").read_text()
    return tuple(re.findall(r'\("(\w+)", NamedColor::', src))


CALLS.update({f"reportlab{m}.{k}": (lambda k: lambda a, kw: _rl(k, a, kw))(k)
              for k, (mods, *_r) in _RL_CLASSES.items() for m in mods})
VALUES.update({
    **{f"reportlab.lib.colors.{n}": f'{RT}::rlbind::named_color("{n}")' for n in _rl_colors()},
    **{f"reportlab.lib.pagesizes.{n}": f'{RT}::rlbind::pagesize("{n}")' for n in ("A3", "A4", "A5", "letter", "LETTER", "legal", "LEGAL")},
    "reportlab.lib.units.inch": f"V::Float({RT}::rl::INCH)",
    "reportlab.lib.units.cm": f"V::Float({RT}::rl::cm())",
    "reportlab.lib.units.mm": f"V::Float({RT}::rl::mm())",
    **{f"reportlab.lib.enums.TA_{n}": f"V::Int({v})" for n, v in (("LEFT", 0), ("CENTER", 1), ("RIGHT", 2), ("JUSTIFY", 4))},
})
EXCEPTIONS.update({f"reportlab.platypus{m}.LayoutError": "RL_LAYOUT_ERROR" for m in ("", ".doctemplate")})
# the colours are objects (`colors.grey.red`)
OBJECT_VALUES.update(k for k in VALUES if k.startswith("reportlab.lib.colors."))

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


# standard-library names outside the map for a reason worth saying (what to do instead), by dotted prefix
STDLIB_HINTS = (
    ("inspect.", "the binary keeps no function objects to reflect on (signatures, source, frames); a library built on "
                 "them (decorator, makefun...) cannot be translated: call the function it wraps directly"),
    ("sys._getframe", "the binary has no Python frames; pass the information the caller needs as an argument"),
    ("sys.exc_info", "the binary has no sys.exc_info(); catch the exception with `except E as e`"),
    ("xml.", "no XML parser or tree is in the map (xml.etree, xml.dom, xml.sax); parse the document with a supported "
             "library (xmltodict) or leave the route to Python (--python-side)"),
    ("array.", "array.array is in the map (numbers, typecodes b B h H i I l L q Q f d), not the rest of the module"),
    ("ctypes.", "the binary calls no C library through ctypes"),
    ("multiprocessing.", "the binary starts no worker processes of its own; leave the route to Python (--python-side)"),
    ("importlib.", "only importlib.import_module('literal') of an installed module is supported"),
)


def stdlib_hint(dotted: str) -> str | None:
    return next((h for p, h in STDLIB_HINTS if dotted.startswith(p)), None)


# the SQLAlchemy objects `sqlmodel` re-exports as they are (`from sqlmodel import Column, DateTime, func`): the same
# objects (checked on sqlmodel 0.0.48); sqlmodel's own select/Session/Field are rewritten (sqlmodel_rewrite.py)
SQLMODEL_SQLALCHEMY = frozenset({
    "ARRAY", "BIGINT", "BINARY", "BLANK_SCHEMA", "BLOB", "BOOLEAN", "BigInteger", "Boolean", "CHAR", "CLOB",
    "CheckConstraint", "Column", "ColumnDefault", "Computed", "Constraint", "DATE", "DATETIME", "DDL", "DECIMAL",
    "DOUBLE", "DOUBLE_PRECISION", "Date", "DateTime", "DefaultClause", "Double", "Enum", "FLOAT", "FetchedValue",
    "Float", "ForeignKey", "ForeignKeyConstraint", "INT", "INTEGER", "Identity", "Index", "Integer", "Interval",
    "JSON", "LABEL_STYLE_DEFAULT", "LABEL_STYLE_DISAMBIGUATE_ONLY", "LABEL_STYLE_NONE",
    "LABEL_STYLE_TABLENAME_PLUS_COL", "LargeBinary", "MetaData", "NCHAR", "NUMERIC", "NVARCHAR", "Numeric",
    "PickleType", "PrimaryKeyConstraint", "QueuePool", "REAL", "SMALLINT", "Sequence", "SmallInteger",
    "StaticPool", "String", "TEXT", "TIME", "TIMESTAMP", "Table", "Text", "Time", "TupleType", "TypeDecorator",
    "UUID", "Unicode", "UnicodeText", "UniqueConstraint", "Uuid", "VARBINARY", "VARCHAR", "alias", "bindparam",
    "column", "create_engine", "create_mock_engine", "delete", "engine_from_config", "except_", "except_all",
    "exists", "false", "func", "insert", "inspect", "intersect", "intersect_all", "join", "lambda_stmt", "lateral",
    "literal", "literal_column", "modifier", "null", "nullsfirst", "nullslast", "outerjoin", "outparam", "table",
    "tablesample", "text", "true", "union", "union_all", "update", "values",
})


def canonical(dotted: str) -> str:
    """Aliases of the same API (`sqlalchemy.sql.expression.select` -> `sqlalchemy.select`)."""
    if dotted.startswith("sqlmodel.") and dotted[9:].split(".")[0] in SQLMODEL_SQLALCHEMY:
        return "sqlalchemy." + dotted[9:]
    for prefix, repl in (
        ("sqlalchemy.sql.expression.", "sqlalchemy."),
        ("sqlalchemy.sql.functions.func", "sqlalchemy.func"),
        ("sqlalchemy.orm.exc.", "sqlalchemy.exc."),
        ("sqlalchemy.ext.asyncio.session.", "sqlalchemy.ext.asyncio."),
        # SQLModel's sessions: SQLAlchemy's, plus `exec()` (rewritten by sqlmodel_rewrite where it is translated)
        ("sqlmodel.ext.asyncio.session.AsyncSession", "sqlalchemy.ext.asyncio.AsyncSession"),
        ("sqlmodel.orm.session.Session", "sqlalchemy.orm.Session"),
        ("fastapi.routing.", "fastapi."),
        ("starlette.status", "fastapi.status"),
        ("sqlalchemy.dialects.postgresql.dml.insert", "sqlalchemy.dialects.postgresql.insert"),
        ("sqlalchemy.sql.dml.insert", "sqlalchemy.insert"),
        *((f"prometheus_client.{m}.{n}", f"prometheus_client.{n}") for m, ns in PROM_SUBMODULES.items() for n in ns),
    ):
        if dotted.startswith(prefix):
            return repl + dotted[len(prefix):]
    return dotted


# loguru's `logger` (dyn.loguru_call, dynrt/loguru.rs): every attribute of loguru.Logger, so that --translate-libs
# never reads one from loguru's source; dyn.loguru_call translates the level methods, log(), add() and remove()
# and refuses the others with the reason
LOGURU = frozenset(f"loguru.logger.{m}" for m in (
    "trace", "debug", "info", "success", "warning", "error", "critical", "exception", "log", "add", "remove", "complete",
    "catch", "opt", "bind", "contextualize", "patch", "level", "disable", "enable", "configure", "parse", "start", "stop"))

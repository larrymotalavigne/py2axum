# Standard library

The modules of the standard library that the runtime implements, and their limits. Anything not listed here is
refused at compile time with `file:line`.

`datetime` (with time zones, `zoneinfo`, Pydantic and `isoformat` formats, `strptime` without `%f`/`%Z`,
`combine`, `fromtimestamp`), `decimal` (`_pydecimal` rules: 28 digits, ROUND_HALF_EVEN, exact
add/mul then rounding, CPython's division, `quantize`, `round`, formatting; mixing with float refused),
`uuid`, `json`, `re` (fancy-regex with Python syntax translated; consecutive empty matches follow Rust, not
Python 3.7+; `pos`/`endpos` of a compiled pattern's methods refused), `csv` (simplified `Sniffer`; `DictReader(restval=, restkey=)`; a positional dialect refused), `io.StringIO/BytesIO`, `pathlib`/`open()` (UTF-8, POSIX; `Path.read_text/write_text(encoding=, errors=)` with the codecs below),
`os.environ`/`os.getenv` (read-only), `os.path`, `math`, `random` (OS-seeded), `secrets`, `hashlib`,
`hmac`, `base64`, `urllib.parse`, `string` constants, `time.time/monotonic/perf_counter`,
`statistics.median`, `logging` (stderr, `LEVEL:logger:message`, level from `PY2AXUM_LOG_LEVEL`; level constants,
`Logger.log` with a standard level),
`email.mime` (incl. `MIMEBase` with `set_payload` and `email.encoders.encode_base64`)/`email.utils` (`formataddr`, `formatdate`, `make_msgid`), `html.escape`, `ipaddress.ip_address`/`ip_network` (prefix length, `strict=`; membership, str/repr; no netmask form, no IPv6 scope id), `socket.getaddrinfo` (the C library's answer; family and kind are plain ints, not `AddressFamily`/`SocketKind` members; `gaierror` carries only its message),
codecs of `str.encode`, `bytes.decode` and `str(b, encoding)`: utf-8, utf-8-sig, latin-1 and ascii with their CPython aliases (`errors=` strict, ignore or replace when encoding, strict when decoding; any other codec raises), `unicodedata.normalize/combining` (the Unicode version of the Python that ran the translation: code points it
leaves unassigned are left alone, as CPython does), `bytes()`, `pickle` (see below), `functools.wraps`, `inspect.iscoroutinefunction`,
`importlib.metadata.version("literal")` (resolved at compile time from what `uv sync --frozen` installs: the project of the `pyproject.toml` next to a `uv.lock` at `--root` or above, or a package of that lock; any other name raises `PackageNotFoundError`; refused without such a lock), `typing.get_args/get_origin/get_type_hints`, `collections.defaultdict` with a builtin type factory (`int`, `list`, `str`...) or `deque` (`type()` of it reports `dict`), `zipfile.ZipFile(io.BytesIO(), "w", ZIP_STORED | ZIP_DEFLATED, compresslevel=)` with `writestr(name, str | bytes,
compress_type=, compresslevel=)`, `namelist`, `close`/`with` (CPython's bytes: same headers, `0o600` permissions,
local time of the call, zlib raw deflate; a mode other than a literal `"w"` is refused; `write(path)`,
`ZipInfo`, ZIP64 and a file object other than `io.BytesIO` raise),
`socket.create_connection((host, port), timeout=)` (blocking connect, the last failure raised as CPython does
without `all_errors`; the socket supports `with`, `close`, `getpeername`, `getsockname`),
`cryptography`'s RSA keys (`rsa.generate_private_key(65537 or 3, key_size >= 1024)`, `private_bytes` in PEM or
DER, PKCS8 or TraditionalOpenSSL, `NoEncryption()` only; `public_key().public_bytes` as SubjectPublicKeyInfo or
PKCS1; `key_size`), `...` as a value (a "not given" sentinel default compared with `is`),
`collections.Counter` (from an iterable, a mapping or keywords; a missing key reads 0 and is not stored;
`most_common`, `update`/`subtract`, `elements`, `total`, `copy`, `+ - | &` between Counters, CPython's `repr`;
`type()` of it reports `dict`, and `==` compares like a dict, where CPython 3.10+ ignores zero counts),
`collections.deque` (`maxlen`, append/pop on both ends, `extend(left)`, `rotate`, `remove`, `index`, `count`, indexing; JSON-encoded as a list by FastAPI), `string.Formatter().vformat/format`, `string.Template` (`substitute`/`safe_substitute`, CPython's KeyError and invalid-placeholder ValueError). `str.format` and `Formatter` support `{}`/`{0}`/`{name}`, `!r`/`!s` and format specs; attribute/index fields (`{a.b}`, `{a[0]}`), nested specs and `!a` raise. Not yet: other `Formatter` methods.

## pickle

`pickle.dumps/loads` use CPython's format (protocol 5 when the project targets Python ≥ 3.14, else 4):
bytes written by the binary are read by CPython and the other way round (e.g. a Redis cache shared with
Python workers). Supported: scalars, str/bytes, containers (shared references kept), datetime types,
time zones, `Decimal`, `UUID`, enums, project class instances (`__dict__` or `__slots__`), dataclasses,
Pydantic models. Refused: mapped (ORM) objects, other globals, out-of-band buffers. Integers beyond 64 bits
read back as integral `Decimal`s.

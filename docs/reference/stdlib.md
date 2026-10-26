# Standard library

The modules of the standard library that the runtime implements, and their limits. Anything not listed here is
refused at compile time with `file:line`. With `--translate-libs`, a few pure-Python modules are also translated
from the source of the project's Python (see [Libraries translated from source](../advanced/libraries.md#the-standard-library)).

`datetime` (with time zones, `zoneinfo`, Pydantic and `isoformat` formats, `strptime` without `%f`/`%Z`,
`combine`, `fromtimestamp`; `strftime` and date f-string specs with the directives that render the same on glibc and macOS, `%a %A %b %B %c %C %d %D %e %f %F %g %G %h %H %I %j %k %l %m %M %n %p %P %r %R %S %t %T %u %U %V %w %W %x %X %y %Y %z %:z %Z %%`, in the C locale, with each platform's padding of a year below 1000 (glibc before 3.14: none, and never in `%c`); flags, widths, `%E`/`%O`, `%s` and unknown letters are refused, P2A0119, or raise ValueError for a computed format), `decimal` (`_pydecimal` rules: 28 digits, ROUND_HALF_EVEN, exact
add/mul then rounding, CPython's division, `quantize`, `round`, formatting; mixing with float refused),
`uuid`, `json`, `re` (fancy-regex with Python syntax translated; consecutive empty matches follow Rust, not
Python 3.7+; `pos`/`endpos` of a compiled pattern's methods refused), `csv` (simplified `Sniffer`; `DictReader(restval=, restkey=)`; a positional dialect refused), `io.StringIO/BytesIO`, `pathlib`/`open()` (UTF-8, POSIX; `for line in f` gives the lines, a text file with universal newlines; `Path.read_text/write_text(encoding=, errors=)` with the codecs below; `Path.cwd()` and `os.getcwd()`, the process's current directory),
`os.environ`/`os.getenv` (read-only), `os.path`, `os.remove`/`os.unlink` (str or Path; `OSError`s carry
`errno`, `strerror`, `filename`), `fcntl.flock` (an `open()` file or a descriptor number; `LOCK_SH`/`LOCK_EX`/`LOCK_NB`/`LOCK_UN`;
a lock held elsewhere with `LOCK_NB` raises `BlockingIOError`), `math`, `random` (OS-seeded), `secrets` (`choice`, `randbelow`, `randbits` up to 63 bits, `token_bytes`/`token_hex`/`token_urlsafe`, `compare_digest`, `DEFAULT_ENTROPY`; `SystemRandom` refused), `hashlib`,
`hmac`, `base64`, `urllib.parse` (`urlsplit`'s netloc checks and their `ValueError`s: brackets, IPvFuture, NFKC; `urlparse`/`urlsplit` with `scheme=` and `allow_fragments=`, `._replace()`, `.geturl()`; `urlunsplit`, `urlunparse` and `urljoin` with each Python's rules (3.12.4's `urlunsplit`, 3.14's `urljoin` keeping empty components); `parse_qs`/`parse_qsl` of a str with their options, utf-8 and errors='replace' only; str arguments only), `array.array` (typecodes b B h H i I l L q Q f d with CPython's OverflowError for each, `itemsize`, `tolist`, `tobytes`/`frombytes`, `append`/`extend`, indexing, `bisect` over it; 'u'/'w' and slicing refused), `mimetypes.guess_type`/`guess_file_type` (3.13+)/`guess_extension`/`guess_all_extensions` (`strict=`; CPython's built-in tables of the project's Python, then the host's `mime.types` files in `mimetypes.knownfiles` order, as `mimetypes.init()` reads them; `add_type`, `init`, `read` and the module's tables are refused), `string` constants, `time.time/monotonic/perf_counter`,
`statistics.median`, `operator.index`, `sys.maxsize`, `threading.local()` (one state for the whole process: uvicorn runs
every `async` endpoint on one thread, but FastAPI runs a `def` endpoint in a thread pool, where CPython gives each
thread its own state), `logging` (loggers, handlers, formatters, filters, `basicConfig`, `dictConfig`,
`LoggerAdapter`, tracebacks: see [Logging](../advanced/logging.md); `traceback.format_exc()` without arguments, the
handled exception's chain with the project's frames only, as in the logs), python-json-logger's `JsonFormatter`,
`email.mime` (incl. `MIMEBase` with `set_payload` and `email.encoders.encode_base64`)/`email.utils` (`formataddr`, `formatdate`, `make_msgid`), `email.message.EmailMessage` under `email.policy.default`, `SMTP`, `SMTPUTF8` or `HTTP` (headers set and read back with the header registry's unstructured and address parsing: decoded values, encoded words, folding at 78 columns; `get`, `get_all`, `keys`, `values`, `items`, `del`, `in`, `len`; `set_content(str, subtype=, charset='utf-8' or 'us-ascii', cte=)`; `as_bytes()`, `as_string()`, `str()`, `bytes()`; `email.errors.HeaderWriteError`; headers are read back as `str`, not header objects; Date, Message-ID, Content-*, MIME-Version, In-Reply-To and References set by the project, an encoded word in another charset than UTF-8, Latin-1 or ASCII or that does not decode, and the other methods are refused), `html.escape`, `html.unescape` (CPython's HTML5 entity table, longest-prefix rule and numeric references), `ipaddress.ip_address`/`ip_network`, `IPv4Address`/`IPv6Address` (int, packed bytes or str, CPython's `AddressValueError` messages; `version`, `max_prefixlen`, `packed`; an IPv6 scope id refused) (prefix length, `strict=`; membership, str/repr; no netmask form, no IPv6 scope id), `socket.getaddrinfo` (the C library's answer; family and kind are plain ints, not `AddressFamily`/`SocketKind` members; `gaierror` carries only its message),
codecs of `str.encode`, `bytes.decode` and `str(b, encoding)`: utf-8, utf-8-sig, latin-1 and ascii with their CPython aliases (`errors=` strict, ignore or replace when encoding, strict when decoding; any other codec raises), `unicodedata.normalize/combining`, `unicodedata.category/bidirectional/decomposition/name` (tables of the translating Python, which must have the project's Unicode version: translate with the project's Python) (the Unicode version of the Python that ran the translation: code points it
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

## tempfile and subprocesses

`tempfile.NamedTemporaryFile(mode, suffix, prefix, dir, delete)` (`w`/`w+`, binary or text) and `tempfile.gettempdir()`
are native: see [Documents § Temporary files](../advanced/documents.md#temporary-files-tempfile); the other `tempfile`
functions are refused. `asyncio.create_subprocess_exec` is native (pipes, `communicate()`, return codes, signals, the
`OSError` of a failed spawn), with the differences listed in [Python language § asyncio](python.md#asyncio-and-threading); `create_subprocess_shell`
and the synchronous `subprocess` module are refused at translation time.

## errno, sys constants, warnings, bisect, collections.abc

`errno.E*` for the numbers glibc and macOS agree on (EPERM to ERANGE; `EAGAIN`, `ENOSYS`... differ between the two and are refused), `sys.byteorder`, `sys.maxunicode`, `sys.warnoptions` (always `[]`, as under uvicorn without `-W`), `sys.modules` with a literal key (`"name" in sys.modules`, `sys.modules["name"]` read or assigned, `.get("name")`) for a module the project's Python cannot import (one PEP 594 removed in 3.13, such as `imghdr`, without a backport installed): absent at startup as in CPython, then holding what the project stores (any other key, or `sys.modules` used otherwise, is refused), `types.ModuleType(name, doc=None)` (an empty module object, attributes only), `bisect.bisect/bisect_left/bisect_right/insort/insort_left/insort_right` (`lo`, `hi`, `key`; CPython's messages, `lo cannot be negative` from 3.15). `isinstance(x, collections.abc.Mapping | MutableMapping | Sequence | MutableSequence | Set | MutableSet)` (a `frozenset` reads as a `set`, so `MutableSet` is true for it: documented difference; the ABCs decided by a `__subclasshook__`, `Iterable`, `Sized`... are refused).

`warnings.warn(msg, category, stacklevel)` with a category CPython's default filters ignore (`DeprecationWarning`, `PendingDeprecationWarning`, `ImportWarning`, `ResourceWarning` and subclasses): the arguments are evaluated and nothing is printed. Any other warning is printed on stderr with the module's file path, once per location, and is refused at generation (use `logging.warning`). `warnings.simplefilter/filterwarnings("ignore")` and `catch_warnings()` (no `record=True`) are accepted; other actions are refused. The `Warning` classes can be raised, caught and subclassed.

`sys.version_info` comparisons (single or chained: `(3, 0) <= sys.version_info < (3, 2)`) fold at translation against the project's Python. `str.join(itertools.chain(...))` is consumed whole, as CPython does. `importlib.import_module("literal")` of an installed module is the module.

## pickle

`pickle.dumps/loads` use CPython's format (protocol 5 when the project targets Python ≥ 3.14, else 4):
bytes written by the binary are read by CPython and the other way round (e.g. a Redis cache shared with
Python workers). Supported: scalars, str/bytes, containers (shared references kept), datetime types,
time zones, `Decimal`, `UUID`, enums, project class instances (`__dict__` or `__slots__`), dataclasses,
Pydantic models. Refused: mapped (ORM) objects, other globals, out-of-band buffers. Integers beyond 64 bits
read back as integral `Decimal`s.

## copy

`copy.copy(x)` and `copy.deepcopy(x)` follow CPython's `copy` module. Immutable values come back as they
are, and so does a tuple whose items deep-copy to the same objects. Lists, dicts, sets, `defaultdict`,
`Counter` and `deque` are rebuilt (a `defaultdict` keeps its factory, a deque its `maxlen`). `deepcopy` keeps
a memo by identity, so shared and cyclic references are copied once. Pydantic models follow
`BaseModel.__copy__`/`__deepcopy__` (fields, extra fields, `model_fields_set`). Dataclasses and plain class
instances are rebuilt with their `__dict__`, shallow or deep. A project class that defines `__copy__`,
`__deepcopy__`, `__reduce__` or `__getstate__` is refused at translation, so its method is never skipped.
`deepcopy(x, memo)` is refused. At run time, copying a mapped (ORM) object, an exception or a runtime handle
(request, client, session) raises `TypeError`.

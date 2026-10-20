# Python versions

py2axum supports projects running on Python 3.12, 3.13, 3.14 and 3.15. Messages and behaviours that changed
between these versions follow **the Python the project runs on**, so the binary answers what your Python
answers: `ZeroDivisionError` texts, the `TypeError` of an unhashable dict key, base64 and base32 decoding,
`datetime.strptime`, the MIME types `FileResponse` guesses, and more (the full list is below).

This page reviews every change of
[What's New in Python 3.14](https://docs.python.org/3/whatsnew/3.14.html) and
[What's New in Python 3.15](https://docs.python.org/3.15/whatsnew/3.15.html) that can show up in a FastAPI,
Pydantic or SQLAlchemy application. It also lists the message changes those pages do not mention, found by
running the same code on each Python. Python 3.15.0 was released on 9 October 2026; the latest 3.14 is 3.14.8.

## Which Python the binary follows

In order:

1. `--python X.Y` (on `py2axum`, `py2axum check` and `py2axum watch`), or the `PY2AXUM_TARGET_PYTHON`
   environment variable;
2. the project's `.python-version` (as uv and pyenv write it), at `--root` or above;
3. the Python running py2axum, when the project's `requires-python` (in `uv.lock` or `pyproject.toml`)
   allows it. This is the usual case of a Docker build, where py2axum runs on the image's Python;
4. else, the lowest Python that `requires-python` allows.

The Python running py2axum must be at least as recent as the project's: the parser only knows its own
syntax. A Python outside 3.12–3.15 is refused (P2A0601).

A few behaviours changed inside a minor: `strptime('%e')` is accepted from 3.13.6, and the security releases
3.13.13 and 3.14.4 rewrote base64's padding rules and a JSON escape message. Distributions backport such
fixes without changing the version (Debian's `3.13.5-2+deb13u5` has the new base64 rules, not the other two).
So when py2axum runs on the project's minor and no patch release is named, it probes these behaviours on
itself and the binary reproduces what it found. Otherwise the binary follows the patch release named by
`--python 3.13.5` or `.python-version`, else the newest; `PY2AXUM_PYTHON_VERSION` (e.g. `3.13.5`) overrides
both when the binary starts. `py2axum check` prints the Python it targets and what it probed.

| Probed behaviour | Changed in | Without a probe |
|---|---|---|
| `base64.b64decode` padding rules (data after `=` decoded) | 3.13.13, 3.14.4 | the named patch, else the newest |
| `datetime.strptime` accepts `%e` | 3.13.6 | the named patch, else the newest |
| `json.loads('"\\u0041')`: unterminated string (was "Invalid \\uXXXX escape") | 3.13.13, 3.14.4 | the named patch, else the newest |
| `html.parser` under reportlab's paragraph markup: the HTML5 rewrite, raw-text elements and `&#`, abrupt comments ([Generated documents](advanced/documents.md#pdf-reportlab)) | 3.12.12-3.12.13, 3.13.8-3.13.15, 3.14.1-3.14.7 | the named patch, else the newest |

The probe measures the Python that translates, not the one in production. **Translate with the same Python
as production**: the same image (the builder stage of a Dockerfile `FROM` the production image), or the same
`python:3.13` official build. Translating on Debian's `python3` for an application served by the official
`python:3.13` image (or the reverse) can pick the other side of these three behaviours; name the patch
release then (`--python 3.13.5` or `.python-version`).

### `sys.version_info`

`sys.version_info` is read at translation time, against the project's Python:

- comparisons with a tuple of integers (`sys.version_info >= (3, 14)`), `.major`, `.minor`, `[0]`, `[1]` and
  `[:2]` become constants;
- an `if` whose test is decided this way keeps only the branch taken. A module imported only under a test
  the project's Python fails is never translated, and never needs to parse. So a project can hold 3.15 syntax
  behind `if sys.version_info >= (3, 15):` and still be translated for 3.14;
- what depends on the patch level (`.micro`, `sys.version_info >= (3, 14, 2)` for a 3.14 project,
  `sys.version`, `sys.hexversion`) is refused with `file:line`.

## Status

| Status | Meaning |
|---|---|
| **Supported** | Translated with the behaviour of the project's Python; a conformance case proves it (`fixtures/langapp`: `pyver.py` on every Python, `py314.py` and `py315.py` on the Pythons that have their syntax) |
| **Refused** | Refused at translation with `file:line` and an error code, or by a run-time `TypeError` of py2axum where the value is only known then (noted). `--python-side` leaves the route to Python |
| **Difference** | Translated, with a documented difference |
| **N/A** | Not observable by an HTTP client of a web API (packaging, interpreter internals, command-line tools, platforms), or concerns a library py2axum does not translate |
| **To do** | Not reproduced or not checked yet: listed for a later version |

## Python 3.14

### Syntax and semantics

| Change | Status | Notes |
|---|---|---|
| Template strings `t"..."` (PEP 750) | Refused (P2A0117) | No `string.templatelib` in the runtime; use an f-string |
| `except A, B:` without brackets (PEP 758) | Supported | `/py314/except` |
| `return`/`break`/`continue` leaving a `finally` (PEP 765) | Supported | Only a `SyntaxWarning`, same behaviour; `/py314/finally` |
| Deferred evaluation of annotations (PEP 649, 749) | Supported | A model can name a class defined after it, unquoted (`/py314/forward`); an annotation naming an undefined name is never evaluated (`/py314/lazy-annotation`) |
| `annotationlib` | Refused (P2A0201) | Not in the library map |
| `from __future__ import annotations` (deprecated, unchanged) | Supported | As before |
| More `SyntaxError`s (`__debug__`, misplaced `await`) | N/A | The project does not parse on 3.14 either |
| Mixed real/complex arithmetic (C99 rules) | N/A | Complex arithmetic is not translated |
| `json` decoding is bounded by the C stack instead of 10 000 levels (`Stack overflow (used 8144 kB) while decoding a JSON array…` past ~50 000 levels on an 8 MiB stack) | Difference | The binary keeps 10 000 levels (`RecursionError: maximum recursion depth exceeded while decoding a JSON array from a unicode string`): a limit on untrusted input ([Supported subset](supported.md#deeply-nested-json-on-python-314), [Security](advanced/security.md)). A document between the two depths passes `json.loads` in Python, then usually exceeds the recursion limit elsewhere (FastAPI's `jsonable_encoder`): the same 500, another traceback |

### Builtins and error messages

| Change | Status | Notes |
|---|---|---|
| Unhashable dict key / set element: `cannot use 'list' as a dict key (unhashable type: 'list')` | Supported | Since 0.3 |
| `math` domain errors name the expected input (`expected a nonnegative input, got -1.0`) | Supported | Since 0.3 |
| `json` errors carry notes (`when serializing list item 1`, `when serializing dict item 'a'`) | Supported | `/pyver/json_notes`; `__notes__`, `add_note()` and the notes printed by `traceback.format_exception*` are supported on every Python (`/pyver/notes`) |
| `_` and `,` separators in the fractional part of a format spec (`f"{x:.6_f}"`) | Supported | `/pyver/fmt_frac`; a `ValueError` before 3.14, as in CPython |
| `bytes.fromhex()` accepts bytes | Refused (P2A0203) | `bytes.fromhex` is not in the runtime |
| `float.from_number()`, `complex.from_number()` | Refused (P2A0203) | |
| `map(..., strict=True)` | Refused (P2A0209) | |
| `NotImplemented` in a boolean context raises `TypeError` | Refused (P2A0105) | `NotImplemented` is not translated |
| `int()` no longer falls back to `__trunc__()` | N/A | Number protocols of project classes are not translated |
| Three-argument `pow()` tries `__rpow__`; `functools.partial` as a method descriptor; `super` objects copyable | N/A | Not translated |
| `repr(Union[int, str])` is `int \| str`; `Union` and `UnionType` are one type | Refused (run time) | The text of a type expression (`str()`, `repr()`, f-string) raises a py2axum `TypeError`: it depends on the Python and on the classes' modules |
| Clearer `SyntaxError`s (keyword typos, `elif` after `else`...) | N/A | The project parses |
| `with`/`async with` on a value without that protocol names the missing method and suggests the other statement | Supported | `/pyver/cm/{kind}`; the type is named like CPython (`_thread.lock`; a class written in Python with its module from 3.14, `asyncio.locks.Semaphore`). Difference: from 3.14, a project class is named without its module (`A`, where CPython says `app.main.A`) |

### Standard library

| Change | Status | Notes |
|---|---|---|
| `uuid.uuid7()` (RFC 9562) | Supported | CPython's algorithm (48-bit ms time, 42-bit counter monotonic within a millisecond); `/py314/uuid7`; refused for an older project (P2A0603) |
| `uuid.uuid6()`, `uuid.uuid8()`, `uuid.NIL`, `uuid.MAX` | Refused (P2A0201) | |
| `contextvars.Token` as a context manager (`with var.set(x):`) | Supported | `/pyver/token_with`; `TypeError` before 3.14, as in CPython |
| `Path.copy()`, `copy_into()`, `move()`, `move_into()` | Supported | `/pyver/path_copy`; `follow_symlinks=False` and `preserve_metadata=True` raise a py2axum `TypeError`. `Path.info` is refused |
| `re`: `\z` is `\Z`; `\B` matches an empty string | Supported | `/pyver/re_z`, `/pyver/re_B_empty`; `\z` is a bad escape before 3.14 |
| `mimetypes`: many new types (fonts, `.yaml`, `.7z`, `.docx`...) | Supported | `FileResponse` guesses with CPython's built-in table of the project's Python, then the host's `mime.types` files, like `mimetypes.init()`; `/pyver/mime/{name}`. `mimetypes.guess_type` itself is refused (P2A0201) |
| `pickle` default protocol 5 | Supported | Since 0.3 |
| `date.strptime()`, `time.strptime()` | Refused (P2A0201) | `datetime.strptime` is supported |
| `Decimal.from_number()`, `Fraction.from_number()`, `Fraction(obj with as_integer_ratio)` | Refused | |
| `functools.Placeholder`, `reduce(initial=)` | Refused (P2A0201) | `functools.reduce` is not in the library map |
| `heapq` max-heap functions | Refused (P2A0201) | |
| `operator.is_none()`, `is_not_none()` | Refused (P2A0201) | |
| `os.reload_environ()` | Refused (P2A0201) | |
| `compression.zstd`, `compression.gzip`/`zlib`/`bz2`/`lzma` | Refused (P2A0201) | `gzip` and `zlib` under their usual names are supported |
| `concurrent.interpreters`, `InterpreterPoolExecutor` | Refused (P2A0201) | |
| `asyncio`: `create_task(**kwargs)`, call graphs, `get_event_loop()` raising without a loop | N/A | A route always runs inside the loop; the call-graph functions are not translated |
| `logging.handlers.QueueListener` as a context manager | N/A | Not translated (the logging work of 0.8) |
| `inspect.signature(annotation_format=)`, `io.Reader`/`Writer`, `ast` additions | N/A | Not translated |
| `unicodedata` 16.0 | Supported | `unicodedata.normalize`/`combining` and `str.isalpha()`... follow the project's Python: the runtime's tables (Unicode 17.0) less what that Python's Unicode does not assign (`py2axum/ucdages.py`, from `tools/gen_ucdages.py`), even when the translating Python is newer; `/libs/unicodedata` |
| `hmac`, `ssl`, `http.server`, `tarfile`, `zipfile`, `configparser`, `argparse`, `sqlite3`, `unittest`, `getpass`, `faulthandler`... | N/A | Not used by a request handler, or not translated |

### Deprecations, removals, runtime

| Change | Status | Notes |
|---|---|---|
| Removals (`asyncio` child watchers, `ast.Num`/`Str`, `pathlib` extra arguments, `urllib` `URLopener`, `itertools` pickling, `email.utils.localtime(isdst=)`...) | N/A | None of them is translated |
| Deprecations (`asyncio.iscoroutinefunction`, the policy system, `codecs.open`...) | N/A | A warning only |
| Free-threaded build officially supported (PEP 779) | N/A | The binary has no GIL either; an endpoint's results do not depend on it |
| Incremental GC in 3.14.0–3.14.4 (back to generational in 3.14.5) | N/A | |
| Tail-call interpreter, JIT, `-X context_aware_warnings`, `-X thread_inherit_context`, remote debugging | N/A | |

## Python 3.15

### Syntax and semantics

| Change | Status | Notes |
|---|---|---|
| Unpacking in comprehensions: `[*x for x in xs]`, `{*x for ...}`, `(*x for ...)`, `{**m for m in ms}` (PEP 798) | Supported | `/py315/unpack`, with CPython's errors (`Value after * must be an iterable, not int`, `'int' object is not a mapping`); `**` of a mapping that is not a dict raises a py2axum `TypeError` |
| `lazy import x`, `lazy from m import n` (PEP 810) | Supported for libraries; Refused (P2A0117) for project modules | A library import has no observable start-up order; a project module's top-level code would run at start-up in the binary, at its first use in Python. `__lazy_modules__` and `sys.set_lazy_imports()` are not translated |
| Unary plus in match literal patterns (`case +1:`) | Supported | `/py315/match` |
| UTF-8 is the default encoding (PEP 686) | Supported | The binary always reads and writes UTF-8 without `encoding=` |
| `module.__cached__` removed, `ImportError` repr, import locks | N/A | |

### Builtins and error messages

| Change | Status | Notes |
|---|---|---|
| `frozendict` (PEP 814) | Refused (P2A0105) | |
| `sentinel` (PEP 661) | Refused (P2A0105) | |
| `bytes.replace(..., count=)` as a keyword | Supported | `/pyver/bytes_replace_kw` (a `TypeError` before 3.15, as in CPython); `bytes.replace` itself was missing from the runtime and is now supported on every Python |
| `bytearray.take_bytes()`, `slice[...]`, `memoryview` complex formats, `__slots__` changes | N/A | Not translated |
| Timestamps and timeouts accept any real number (`Decimal`, `Fraction`) | Supported | `datetime.fromtimestamp` (`/pyver/fromtimestamp_decimal`, with 3.14's reworded error before: `argument must be int or float, not decimal.Decimal`) and `socket.create_connection(timeout=)` (`/pyver/timeout_*`: `must be real number, not str`). The other C functions taking a timestamp or a timeout (`time.sleep`, `time.localtime`, `date.fromtimestamp`, `threading`, `select`...) are not translated: refused with P2A0201 and `file:line` (`tests/test_rejects_dyn.py::test_real_timestamp_functions_rejected`). asyncio's delays are Python code, unchanged: `asyncio.sleep`/`wait_for` add the delay to the loop's float time (a `Decimal` fails, except `asyncio.sleep` on uvloop), `/pyver/aio/*` |
| `AttributeError` hints from other languages (`Did you mean '.append'?`) | Difference | `str(exc)` is unchanged; `traceback.format_exception_only` adds the hint in Python (on every Python for a close name, from 3.15 for these), not in the binary. Reading an attribute no runtime type has is refused anyway (P2A0204) |

### Standard library

| Change | Status | Notes |
|---|---|---|
| `base64.urlsafe_b64decode` no longer requires padding; `padded=` | Supported | `/pyver/b64_unpadded`, `/py315/padded`; python-jose and PyJWT follow it (their `base64url_decode`) |
| `base64.b32decode` rewritten in C (new messages: `Only base32 data is allowed`, `Excess padding not allowed`...) | Supported | `/pyver/b32_*`; pyotp follows it |
| Other new base64/binascii parameters (`wrapcol=`, `ignorechars=`, `canonical=`), `binascii` base32/85 functions | Refused (P2A0209, P2A0201) | The base64 functions accept only the parameters the runtime reads |
| `re.prefixmatch()`, `Pattern.prefixmatch()` | Supported | `/py315/prefixmatch`; refused for an older project (P2A0603) |
| `datetime.strptime`: `%d` without a year is an error | Supported | `/pyver/strptime_d` (and `%e`, accepted from 3.13.6) |
| `os.urandom(-1)` (behind `secrets.token_*`): `size cannot be negative` | Supported | `/pyver/token_negative` |
| `json.loads(array_hook=)`, json accepting `frozendict` | Refused (P2A0209) | |
| `urllib.parse.urlsplit(missing_as_none=)`, `urlunsplit(keep_empty=)` | Refused (P2A0209) | |
| `math.integer`, `math.isnormal`/`issubnormal`/`fmax`/`fmin`/`signbit` | Refused (P2A0201) | |
| `asyncio.TaskGroup.cancel()` | Refused (P2A0201) | `asyncio.TaskGroup` is not translated |
| `contextlib.ContextDecorator` keeps the context open across a decorated generator or coroutine | N/A | `@contextmanager` used as a decorator is not translated |
| `collections.Counter` `^` (symmetric difference) | Supported | `/pyver/counter_xor`; before 3.15 the `TypeError` names `Counter` (the runtime said `dict` on every Python) |
| `functools.singledispatchmethod`, `threading` iterator helpers, `pprint(expand=)` | Refused | |
| `typing`: `TypeForm`, `TypedDict(closed=, extra_items=)` (PEP 728), `@disjoint_base` | Refused | A `TypedDict` as a request or response type is refused (P2A0410); elsewhere it is a `dict`, unchanged |
| `email` refuses to flatten a non-ASCII address it cannot encode (`HeaderWriteError`) | Supported | `EmailMessage` (dynrt/emailhdr.rs) folds with 3.15's rules on 3.15 (`/emailx/run`). The compat32 messages the runtime builds (`MIMEText`, `MIMEMultipart`...) encode such an address whole on every Python, unchanged (`/mail/build?to=...`) |
| `unicodedata` 17.0 and its new functions | Refused (P2A0201) for the new functions; Supported for `normalize`, `combining` | See 3.14 |
| `importlib.metadata`, `hashlib`, `http.client`, `http.cookies`, `ssl`, `sqlite3`, `tomllib` 1.1, `xml`, `zlib`, `os.statx`, `Path.mkdir(parent_mode=)`... | N/A | Not used by a request handler, or not translated |

### Deprecations, removals, runtime

| Change | Status | Notes |
|---|---|---|
| Removals (`typing.NamedTuple` keyword syntax, `TypedDict("TD")`, `sre_*`, `glob0`, CGI server, `PurePath.is_reserved`, `Loader.load_module`...) | N/A | None of them is translated |
| Deprecations (`profile`, `-b`, `.pth` imports, `hashlib(string=)`...) | N/A | |
| `profiling` package, upgraded JIT, mimalloc, frame pointers, free-threading by default on macOS installers | N/A | |

## Changes the What's New pages do not list

Found by running the same code on 3.12, 3.13, 3.14 and 3.15 (`/pyver/{name}` reproduces each one):

| Change | From | Status |
|---|---|---|
| `ZeroDivisionError` says `division by zero` for every operator and type (`integer modulo by zero`, `float floor division by zero`, `float divmod()`... before; `float modulo` became `float modulo by zero` in 3.13) | 3.14 | Supported |
| `0.0 ** -1`: `zero to a negative power` (was `0.0 cannot be raised to a negative power`) | 3.14 | Supported |
| `too many values to unpack (expected 2, got 3)` for a list, tuple or dict (a str or an iterator: unchanged) | 3.14 | Supported |
| `dict([1])`: `TypeError: object is not iterable` with the note `Cannot convert dictionary update sequence element #0 to a sequence` | 3.14 | Supported |
| `date`/`time`/`datetime` range errors name the value (`month must be in 1..12, not 13`, `day 29 must be in range 1..28 for month 2 in year 2025`) | 3.14 | Supported |
| `list.index`: `list.index(x): x not in list` | 3.14 | Supported |
| `in` on a non-container: `argument of type 'int' is not a container or iterable` | 3.14 | Supported |
| `str`/`bytes` methods called without their argument: `count expected at least 1 argument, got 0` (was `count() takes at least 1 argument (0 given)`) | 3.13 | Supported (and `bytes` from 3.15 for `replace`) |
| `str.replace(old, new, count=n)` accepted as a keyword | 3.13 | Supported (it was ignored by the runtime before) |
| `datetime.strptime` accepts `%e` | 3.13.6 | Supported |
| `base64.b64decode`: data after the padding is decoded, excess `=` skipped (non-strict) | 3.13.13, 3.14.4 | Supported (`/pyver/b64_after_padding`) |
| `%`-formatting errors name the argument and the position (`format argument 2: %d requires a real number, not str`, `not enough arguments for format string (got 1)`, `stray % at position 3`) | 3.15 | Supported (`/pyver/pct_*`) |
| `socket.getaddrinfo` (and `create_connection`, `asyncio.open_connection`): the `idna` codec's check of the host raises `UnicodeEncodeError` naming the position (`label too long`, `label empty`), where 3.12 raises `UnicodeError: label empty or too long` | 3.13 | Supported (`/libs/ssrf`, `/stdmore/sock`); before, the binary passed such a host to the C library |
| `re.error` is named `re.PatternError` (`re.error` an alias) | 3.13 | Supported: `type(e).__name__` and tracebacks follow the project's Python (`/pyver/re_error_name`, `/py314/pattern-error`); `re.PatternError` is refused for a 3.12 project (P2A0603) |
| `socket.create_connection(timeout=)` (C `settimeout`): `'Fraction' object cannot be interpreted as an integer` (3.13: `integer`, 3.14: `integer or float` and `fractions.Fraction`); out of range: `timestamp out of range for platform time_t` for a float, `too large to convert to C _PyTime_t` for an int (`PyTime_t` from 3.13), then `timestamp out of range for C PyTime_t` for both from the security releases 3.13.15 and 3.14.7 (probed on a distribution's Python: `pytime_range`) | 3.13, 3.14 | Supported (`/pyver/timeout_*`); before, a NaN or out-of-range timeout aborted the request |

## Differences found and fixed on every Python

The review also found places where the binary differed from Python whatever its version, now fixed:
`format()` (the builtin was not translated; `f"{n:_}"` grouped with commas; `Unknown format code` lacked the
code), `bytes.replace` (missing), `dict(seq)` element errors, `divmod()` by zero, `list.count()`/`"a".count()`
without an argument (a crash of the request), `base64.b32decode(map01=)` (ignored), the type named by an
operator error on a `Counter` or a `defaultdict`, the type named by a context-manager error (`_thread.lock`),
the `datetime.fromtimestamp` error, and the MIME types of `FileResponse` (a 20-entry table instead of
CPython's, and `text/plain` for an unknown type where Starlette 1.1+ says `application/octet-stream`).

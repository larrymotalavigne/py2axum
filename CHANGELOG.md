# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.9.1] — 2026-10-18

Patch release: an ammonia security update, fixes for differences seen in production (request bodies validated from
JSON bytes, SMTP over TLS with `validate_certs=False`, `has()`/`any()` after a join, datetimes and `strftime`), and
pytz, python-dateutil and cachetools translated natively.

### Security

- `ammonia` 4.2.4 in the shipped lock file and as the runtime's minimum (was 4.2.3): RUSTSEC-2026-0373,
  CSS `url()` matched case-sensitively when sanitizing (`nh3.clean`).

### Added

- pytz 2026.5 native (`dynrt/pytzrt.rs`, pytz's own zones dumped by tools/gen_pytz.py): `timezone()`, `utc`,
  `FixedOffset()`, the zone lists, `localize`/`normalize`/`fromutc`/`utcoffset`/`dst`/`tzname` with pytz's instances
  (`is` holds), croniter over pytz zones (its `localize` branch). fixtures/libapp/pytzx.py: every zone over its
  transitions.
- python-dateutil 2.9: `dateutil.parser.parse()` with its options, `ParserError`, `isoparse()`/`isoparser`
  (`dynrt/duparse.rs`), and `dateutil.tz` (`dynrt/dutz.rs`): `tzutc`, `tzoffset`, `tzlocal`, `gettz` (system zone
  files as dateutil reads them, dateutil's tarball, `tzstr`), `tzstr`, `enfold`, `datetime_exists`,
  `datetime_ambiguous`, `resolve_imaginary`. fixtures/libapp/duparsex.py, dutzx.py (3,000 generated strings for the
  parser). The python-dateutil range is checked when its parser or tz is imported; the binary reads `TZ` (`gettz()`).
- cachetools 7.2 (`dynrt/ctools.rs`): `Cache`, `FIFOCache`, `LRUCache`, `TTLCache`, `@cached`, `@cachedmethod`,
  `cachetools.func.lru_cache`/`fifo_cache`/`ttl_cache`, `keys.hashkey`/`methodkey`. fixtures/libapp/ctoolsx.py.
- Error codes P2A0210 (cachetools decorator not supported) and P2A0119 (strftime directive not supported).

### Fixed

- `Model.model_validate_json(await request.body())` always failed: the bytes went through `str()` (`"b'{...}'"`),
  so the JSON was refused at its first character (a route swallowing the error silently dropped every request).
  `model_validate_json` and `TypeAdapter.validate_json` now take str or bytes parsed by jiter (`json_invalid` and
  `json_type` validation errors, as pydantic) and validate in pydantic's JSON mode: its own type errors (`Input
  should be a valid array`, `Input should be an object`, `valid duration`), a JSON string as an exact input for
  date, datetime, time, timedelta, UUID, bytes and Decimal in a smart union, Decimal from a JSON float as pydantic
  writes it (`1.0` -> `Decimal('1')`). Strict fields, complex, Iterable, deque/Counter/OrderedDict/Sequence, unions
  discriminated by a function and models with wrap validators are refused in JSON mode when reached (never a wrong
  value). fixtures/dynapp/reqbody.py.
- `await request.json()` decodes the body as `json.loads(bytes)` does (UTF-8 BOM, UTF-16/32, `UnicodeDecodeError`):
  invalid UTF-8 was replaced by U+FFFD.
- `await request.form()` and `request.stream()` are refused at generation (file:line): they raised AttributeError
  at run time.
- `aiosmtplib.send(validate_certs=False)` now accepts a certificate issued for another host name, as aiosmtplib does
  (it checks neither the chain nor the name); the binary skipped the chain only, so a relay dialled by an internal name
  (e.g. `internal-mailserver` against a certificate for `mail.example.fr`) failed with "certificate not valid for name".
  `validate_certs=True` still refuses. Covered by `/small/mail-tls` (integapp) against `tests/smtp_sink.py --tls`.
- `Model.rel.has()` / `.any()` in a statement that joins the relationship's target
  (`select(A).join(A.b).where(A.b.has(...))`): the EXISTS kept its target in its FROM list like SQLAlchemy's
  `correlate_except(target)`; since 0.7.0 it auto-correlated it away and raised InvalidRequestError (500, Python 200).
  `correlate_except()` on `select()`, `exists()` and `scalar_subquery()`, and `correlate()` on `exists()`, are native.
- Datetimes: two values under the same tzinfo subtract and compare by their wall times (CPython), and between zones
  a value whose offset depends on its fold equals nothing (PEP 495); `astimezone()` of the same tzinfo returns the
  value; `dt.tzname()` and `dt.dst()`; `%Z` writes the zone's abbreviation (it wrote a ZoneInfo's key);
  `datetime(..., fold=)`; 3.14's `fold must be either 0 or 1, not N`; `timezone`/`ZoneInfo` values are hashable,
  `is` holds between tzinfos; `isinstance(x, timezone)`, `isinstance(x, tzinfo)`.
- A keyword argument a function does not take names the closest parameter on Python 3.13+ (`Did you mean 'dt'?`).
- `UnicodeDecodeError(encoding, object, start, end, reason)`'s `str()`; `<class 'decimal.DivisionImpossible'>` and
  the other library exception classes' `repr` with their module; a library named tuple's `repr`
  (`CacheInfo(hits=0, ...)`).
- A decorator named `lru_cache` or `cache` from another library than functools (cachetools.func) was compiled as
  functools' own.
- `strftime("%Y")` of a year below 1000 on Linux before Python 3.14 writes it unpadded (`1`), as glibc does through
  CPython; macOS and 3.14+ pad it (`0001`). Likewise `%G`, `%C` and `%F`; `%c` keeps glibc's unpadded year on every
  version.
- `strftime` copied the directives it did not know into the output (`%c`, `%e`, `%u`, `%V`, `%:z`...): every
  directive whose output is the same on glibc and macOS is rendered as CPython does, and `%P` as each C library does;
  flags (`%-d`), widths, `%E`/`%O`, `%s` (the local time zone) and unknown letters are refused at generation for a
  literal format (P2A0119) and raise ValueError for a computed one.
- `dateutil.parser.parse()` of a field beyond a C int raises 3.13's `Python int too large to convert to C int`
  (`datetime.replace` through Argument Clinic since 3.13.0; it was written for 3.14 only).

## [0.9.0] — 2026-10-16

The runtime becomes a library crate of its own, compiled once and shared by every generated crate (link-time
optimisation thin by default: a generated crate builds in under 3.1 GiB on Linux), and the binary allocates with
mimalloc. The documentation corpora reach FastAPI 96.9 %, Pydantic 96.6 %, SQLAlchemy ORM 99.2 % and Core 100 %
with no silent difference. Pure-Python libraries can be translated from their source (`--translate-libs`, off by
default), and itsdangerous' `URLSafeSerializer`, `mimetypes`, `email.message.EmailMessage` and dnspython's resolver
are translated natively.

### Added

- `--translate-libs none|auto|DIST,...` (generation, `check`, `watch`), with `--venv` and `--allow-license`:
  pure-Python libraries of the project's environment translated from their source, only the functions the routes
  reach (see Libraries translated from source). The project and the library map keep precedence; a distribution
  is refused with a code of the new 07xx family when its licence is not admitted (P2A0701), when it is not pure
  Python (P2A0702), or when it is not installed as the project locks it (P2A0703). `check` lists the translated
  libraries (functions translated and refused, lines, estimated gen.rs and rustc peak; new `libraries` key of its
  JSON output). Off by default. With it, a closed list of pure-Python modules of the standard library is
  translated from the project's Python's `Lib/` (`colorsys`, `gettext`, `html.entities`, `keyword`, never a module with a C accelerator such as `bisect`; P2A0704
  when py2axum runs on another minor); a library's module statements run at import, its `try: import x / except
  ImportError:` is decided by the environment, and the lazy imports of a package's `__getattr__` are followed.
- Protocol methods of plain classes and dataclasses: `__iter__`/`__next__` (a `for` loop and a comprehension call
  `__next__` between their bodies), `next()`, `__len__`, `__bool__`, `__contains__`, `__getitem__`/`__setitem__`/
  `__delitem__`, the arithmetic and bitwise operators with their reflected and in-place forms and
  `NotImplemented`, `__neg__`/`__pos__`/`__invert__`/`__abs__`, with CPython's errors. Pickle and copy methods
  (`__getstate__`, `__reduce__`, `__deepcopy__`...) are ignored when nothing imports copy or pickle.
- Bases `Generic[T]`, `Protocol[T]` and subscripted project bases; method defaults read in the class body;
  statements after `return`/`raise`/`break`/`continue` no longer translated; `mod.X = v` / `mod.X += v` rebind a
  module variable (they were refused).
- `threading.local()`, `operator.index()`, `sys.maxsize`, `str.isascii()`, `object()` as a sentinel, `bytearray`,
  `memoryview` and `complex` as `isinstance` members, `s.__contains__` (and `__getitem__`, `__len__`) of a builtin
  container read as a value (`keyword.iskeyword`), `@typing.overload` signatures dropped, module flags made of a
  version or platform test (`PY3 = sys.version_info[0] == 3`) folded, `isinstance(x, string_types)` with a module
  variable holding types, `__slots__` as one string, `if TYPE_CHECKING:` in a class body.
- `FastAPI(telemetry={...})` with its literal switches (the binary emits no OpenTelemetry data: documented).
- Pydantic `create_model("M", ...)` at module level, compiled as the class it creates (fields, `__base__`,
  validators and computed fields of `__namespace__` / `__validators__`).
- SQLModel (FastAPI's SQL databases tutorials): classes rewritten at translation into SQLAlchemy mapped classes and
  Pydantic models (same DDL as SQLModel), `Session(engine)` dependencies, `session.exec(select(...))`,
  `model_validate`, `sqlmodel_update`, `SQLModel.metadata.create_all(engine)`; `Base.metadata.create_all(engine)`
  with a synchronous engine. fixtures/sqlmodelapp.

- Pydantic: `datetime`/`date` bounds (`Field(gt=datetime(...))`, `annotated_types`), `AwareDatetime`,
  `NaiveDatetime`; `model_post_init(self, context)`; `PrivateAttr(default_factory=...)` computed (`datetime.now`);
  `@with_config(...)` on a stdlib dataclass; a method of a builtin type as a value (`MyAfterValidator(str.lower)`,
  `key=str.lower`).
- FastAPI: subclasses of `HTTPBearer`/`HTTPBasic`/`OAuth2PasswordBearer` overriding
  `make_not_authenticated_error` / `make_authenticate_headers`; `SecurityScopes` and `Security(dep, scopes=[...])`
  (scopes of the chain, cache per set of scopes); `AfterValidator` on path, query, header and cookie parameters;
  `yield from` over a file, a literal or a builtin's result; iterating an open file.

- `Model.__table__` as a Core table when the code uses it as one (`t.join(...)`, `t.c.col`, `select(t)`,
  `select_from(t)`): rows of values, columns by their column name. A value both used as a Core table and iterated
  for introspection (or given to other code), and the table of an inheriting class, are refused at translation.
- Core `Table.join(right, onclause=None, isouter=, full=)` and `outerjoin()`, chained; a Join given to
  `select_from()` supersedes the FROM entries of its tables, as in SQLAlchemy.
- `with_polymorphic(Base, [Sub, ...], aliased=True, flat=...)`: an alias of the base with its subclass tables
  LEFT OUTER JOINed under their own aliases, two of them joined to each other, `poly.Sub.col` in the criteria, the
  subclass columns loaded with the objects. SQLAlchemy ORM corpus 93.8 % → 95.4 % (0 differences).
- `Model.model_construct(...)` (pydantic 2.12 to 2.14: aliases, validation aliases, defaults, `_fields_set`, extras,
  private attributes) and `model_extra`. A value of another type than its field is refused when the instance is
  dumped (pydantic dumps it with a serializer warning), a required field left out at run time.
- A router of a library included last in the app (`app.include_router(lib.router)`) stays on the Python side
  with `--python-side mount` (or `auto`), like a final `app.mount()`: refused with `file:line` under the new code
  P2A0315 when something registers routes after it (also through a function given the app), when it is
  conditional, or when its prefix is not literal. It used to stop the generation of the whole application.
- FastAPI parameter markers kept in module variables (`RadiusQuery = Query(50.0, ge=5)` given as a default or
  in `Annotated`) were evaluated as values (a library call, refused); they now stand for the call they hold. A
  marker without `alias=` shared by parameters of different names is refused (FastAPI gives it the first one's
  name).
- A function held as a plain class attribute through `staticmethod(f)` (read through the class or an instance,
  never bound).
- Hybrid attributes with a custom comparator (`@x.comparator`, `@x.inplace.comparator`) and hybrid value objects:
  `sqlalchemy.ext.hybrid.Comparator` is translated from Python source of py2axum's own (`py2axum/shims/`, its frames
  left out of tracebacks like a library's); its subclasses, plain classes or `@dataclass(eq=False)`, are compared in
  Python and in SQL through `operate()`, coerced to SQL by `__clause_element__()` (also on an `aliased()` class).
  A hybrid setter runs for a keyword of the default constructor.
- Rich comparisons of project classes (`__ne__`, `__lt__`, `__le__`, `__gt__`, `__ge__`, reflected ones, the
  result as returned), `sorted()`/`min()`/`max()` of such instances, `__hash__ = object.__hash__`,
  `@dataclass(eq=False)`, a dataclass deriving from a plain class, `operator.eq` & co. as values
  (`<built-in function eq>`).
- `relationship(primaryjoin=fk == col)` (a foreign key equal to the column it references) and
  `post_update=True` on a many-to-one relationship: two tables referencing each other are flushed (INSERT, then
  UPDATE of the key; UPDATE to NULL before the DELETEs). `select(tuple_(...))` raises SQLAlchemy's CompileError.
- fixtures/ormapp (116 cases). SQLAlchemy ORM corpus 95.4 % → 99.2 % (257/259, 0 differences; all SQLAlchemy
  85.2 % → 87.4 %); the last ORM refusal is the documentation's own defect (`ext/associationproxy.rst:592`). The
  corpus leaves out a definition block Python fails to run (a fragment such as `class X(Base): # ...`), which made
  the Python reference of the later blocks of its page fail.
- itsdangerous `URLSafeSerializer` (the serializer without timestamp): `dumps(obj, salt)`, `loads(s, salt)`, same
  tokens byte for byte, key rotation, the library's exceptions; itsdangerous joins the tested version ranges (2.2).
- `mimetypes.guess_type`, `guess_file_type` (Python 3.13+), `guess_extension`, `guess_all_extensions` (`strict=`):
  the project's Python built-in tables, then the host's `mime.types` files, `data:` URLs, suffix and encoding maps,
  the order of `guess_all_extensions`.
- SQL of reporting queries: window functions (`func.f(...).over(partition_by=, order_by=)`, frames refused), CTEs
  (`select(...).cte(name)`, anonymous, chained, read in a subquery: all in the WITH clause of the outermost
  statement), LATERAL subqueries (`.lateral()`, correlated to their statement's FROM list), `correlate(*entities)`
  and `correlate(None)`, `union_all()`/`union()`/`intersect()`/`except_()` (and `_all`, functions or methods) made
  a subquery, a CTE or executed, `case(*whens)` from a list built at run time (no WHEN: built, then refused by
  PostgreSQL as with SQLAlchemy; a list of whens: SQLAlchemy's `ArgumentError`), `icontains`/`istartswith`/
  `iendswith`, `cast(x, Numeric)`, `literal_column(text)`, `postgresql.array([...])` with `any_()`/`all_()`,
  `bindparam(key, type_=ARRAY(String()))`, `Result.fetchone()`, `mapped_column(..., type_=T)`,
  `aliased(Model, subquery or cte())` (a union of such selects too), `aliased(Model, flat=True)` on a plain table,
  `scalar_subquery().correlate(t)`. SQLAlchemy corpus: Core 88.7 % → 100 %, all of SQLAlchemy 87.4 % → 92.6 %
  (0 differences); `text(...).columns(...)` is refused at translation (it failed at run time).
- Dependencies built by a factory (`Depends(make_dep(Query([], alias="id"), int))`): the function it returns, its
  defaults and annotations holding the factory's arguments (constants, builtin types, FastAPI markers of
  constants); tracebacks name it as CPython does (`make_dep.<locals>.inner`).
- `typing.NamedTuple` classes: built like a dataclass, a tuple everywhere else (indexing, unpacking, equality and
  hash with tuples, concatenation, `isinstance(x, tuple)`, JSON as an array), `_asdict()`, `_replace()`, `_make()`,
  `_fields`, `_field_defaults`, read-only fields with CPython's messages per version.
- `string.Formatter().parse(s)` consumed whole (`list(...)`, `tuple(...)`): CPython's parser, items and errors;
  `itertools.chain(*its)` / `chain.from_iterable(its)` consumed whole (`list()`, `tuple()`, `set()`, `sorted()`);
  `float.is_integer()` / `int.is_integer()`; `from builtins import list` (the builtins called by that name);
  `Ellipsis`.
- Dataclasses (stdlib and `pydantic.dataclasses`) as FastAPI request and response types and as model fields:
  validated like pydantic dataclasses (`dataclass_type`), into instances of the class.
- `pydantic.Json` / `Json[T]`, parsed with jiter (the crate pydantic-core uses: same error messages and positions),
  then validated as `T`. New runtime dependency: `jiter` 0.16 (no Python feature).
- `__get_pydantic_core_schema__` (a field type's classmethod, an Annotated metadata's) and `GetPydanticSchema`,
  read at translation as the equivalent annotation (scalar schemas and constraints, after/before/wrap/plain
  validator functions, `chain_schema`, plain serializers, `schema['max_length'] = 10`, `if`/`assert` known at
  translation): the documentation's CompressedString, RestrictCharacters, SmallString, CustomType examples.
- Custom route classes: `APIRouter(route_class=R)` and `app.router.route_class = R`, `R.get_route_handler()` wrapping
  the route's own handler (request validation errors caught, the Response object's headers changed), and a
  project `Request` subclass whose `body()` the route parses (FastAPI's GzipRequest). `repr()` of a Response and
  of its headers as Starlette writes them, `request.headers.getlist()`.
- Pydantic `Iterable[T]` fields (pydantic-core's ValidatorIterator): any iterable accepted, items validated when
  reached (`next`, loops, `list`, JSON dumps, responses), with pydantic's errors and titles.
- `email.message.EmailMessage` under the modern policies (`email.policy.default`, `SMTP`, `SMTPUTF8`, `HTTP`):
  unstructured and address headers set and read back (`msg[name] = value`, a list of addresses, `None`), `get`,
  `get_all`, `keys`/`values`/`items`, `del`, `in`, `len`, `set_content(str, subtype=, charset=, cte=)` (the text
  handler's choice of 7bit, 8bit, quoted-printable or base64), `as_bytes()`, `as_string()`, `str()` and `bytes()`:
  a port of `email._header_value_parser` (address lists, encoded words, folding), byte for byte on every CPython
  release from 3.12.12 to 3.15.0 (the folder changed in 3.12.13, 3.13.12 to 3.13.14, 3.14.3 to 3.14.6 and 3.15:
  each change follows the project's Python, probed on the transpiler's own when it is the same minor). Headers with
  a structured value of their own (Date, Message-ID, Content-*, MIME-Version, In-Reply-To, References) and the
  other methods (`add_attachment`, `get_content`, `iter_parts`...) are refused.
- dnspython 2.8's stub resolver: `dns.resolver.Resolver(filename=, configure=)` read from `/etc/resolv.conf` as
  dnspython reads it, its settings (`nameservers`, `nameserver_ports`, `port`, `timeout`, `lifetime`, `search`,
  `use_search_by_default`, `retry_servfail`, `rotate`, `ndots`, `use_edns()`), `resolve()` and the module's
  `resolve()` on the default resolver: the same queries over UDP (TCP when truncated or `tcp=True`), retries,
  backoff, timeouts and nameserver eviction, CNAME chains, answers of MX, TXT, A, AAAA, CNAME and NS records
  (`to_text()`, `str()`, `repr()`, their attributes), `dns.name.from_text`, and the exceptions with their messages
  (`NXDOMAIN`, `NoAnswer`, `NoNameservers`, `LifetimeTimeout`, `NoMetaqueries`, `dns.name` errors...). dnspython
  joins the tested version ranges (2.8). tests/dns_stub.py serves the conformance.

### Changed

- The runtime is a library crate of its own (`dynrt/`, next to the generated crate, its path dependency) instead
  of a module copied into `src/dynrt/`, and the release profile uses thin LTO instead of fat
  (`CARGO_PROFILE_RELEASE_LTO=fat` restores it). Throughput within the noise (−3.0 to +7.9 %), up to 3 % more
  server CPU per request, compilation peak of a large project 6.94 → 4.48 GiB; the runtime compiles once for every
  crate that shares a target directory (the Docker image precompiles it). The generated crate is a Cargo workspace; a `Cargo.lock`
  generated by an earlier version is replaced. The generated project stays outside the public contract.
- The generated binary allocates with mimalloc instead of the C library's `malloc` (a default Cargo feature of the
  generated crate; `cargo build --release --no-default-features` keeps the system's): 7 to 17 % less server CPU
  per request, up to 22 % more throughput, at the price of 16 to 28 MiB more RSS. Building needs a C compiler
  (already needed for zlib).
- Fewer instructions per request (callgrind, the dynapp fixture): a 20-row ORM list −6.8 %, a single row −4.9 %,
  a `selectinload` −7.2 %, a 404 −5.1 %. Table and column names are quoted at translation, a row's object is
  built once, the response model reads loaded attributes without a future per field, dict keys are hashed once.

### Fixed

- `float(str)` follows CPython's grammar: underscores between digits (`"1_000"`), Unicode decimal digits; it
  used to refuse the first and accept Rust-only forms.
- Iterating a plain class instance or a dataclass without `__iter__` raises CPython's `TypeError` (it iterated
  nothing).
- An ORM object whose autoincrement primary key was set to None was inserted with NULL (SQLAlchemy leaves the key
  out and lets the sequence give it).
- A missing parameter's default other than None is validated, as FastAPI does (`Query(0, ge=1)` is a 422 when the
  parameter is missing); it was returned as is.
- `model_post_init` was never called (now refused only with `model_construct`).
- A stdlib dataclass inside a model takes that model's `str_*` and `use_enum_values` config, as in pydantic (it
  was validated without it); under `strict=` or `extra=` it is refused.
- A `typing.NamedTuple` as a Pydantic type was validated like a model (other errors): refused.

- A method named like a library function (`stmt.compile()`, named like `re.compile`) passed the translation's
  check that some runtime value implements it, then failed at run time (AttributeError): refused at its line now
  (P2A0203). No route of the ATOM applications changes.
- `str()` of a SQL statement or expression (also in an f-string or `format()`) returned the PostgreSQL text the
  binary runs, never SQLAlchemy's (its default compiler: `:param_1`, line breaks before `FROM`...): it now raises
  TypeError; `str()` of a column (`table.col`) is unchanged.
- `tuple * int` (and `int * tuple`) raised TypeError: the repeated tuple, as in CPython.
- `int(x)`, `round(x)`, `math.floor/ceil/trunc(x)` of a float beyond 64 bits returned 2⁶³−1 (a saturating
  conversion) instead of failing: OverflowError, like the other integers py2axum does not represent; `int(inf)`
  raised ValueError instead of CPython's OverflowError, `round(nan)` / `round(inf)` returned 0 / 2⁶³−1.
- `from pkg import name` where `pkg/name.py` exists and `pkg/__init__.py` binds `name` (`from .name import name`,
  `from .name import *`) resolved to the submodule; it is now the object the package binds, as CPython reads the
  package's attribute first (python-slugify, tenacity, email-validator do it).
- A class held as a class attribute (`class G: NT = _NT`) called through the class held as a value
  (`alias = G; alias.NT()`) raised AttributeError at run time. A method read on its class (`Cls.method`, through
  a value too) now gives a function (taking the receiver first, none for a `@staticmethod`), or the method bound
  to the class for a `@classmethod`, instead of raising AttributeError; an `async def` read that way is refused at
  translation (P2A0101, `file:line`) when the class is named, at run time through a class held as a value
  (fixtures/resolveapp).
- An integer expression of literals beyond 64 bits (`10**100`, `1 << 70`, also through a comprehension variable
  over a literal tuple: `[10**x for x in (3, 100)]`, seen in humanize) raised OverflowError at run time, at
  startup for a module constant; it is now refused at translation with `file:line` (P2A0101).
- A function or lambda held as a plain class attribute was read unbound through an instance (CPython binds it to
  the instance): it is now refused, `staticmethod(f)` is the supported form.
- `from module import *` ignored the names the module imports itself (`HTTPException` imported there from
  FastAPI was an unknown name), and bound names that `__all__` leaves out or that start with `_`.
- `isinstance(x, Parent)` was false for an instance of a Pydantic model that subclasses another model of the
  project (and `Parent.model_validate(child)` raised a validation error).

- An exception raised while FastAPI's default handler encodes a 422 or an HTTPException's detail (`inf` in the
  input) carries the handled exception as its `__context__` (the traceback of the 500 differed).
- A StopIteration escaping an endpoint or a dependency is `RuntimeError: coroutine raised StopIteration` (PEP 479),
  with the StopIteration as its cause, as in CPython (the traceback of the 500 differed).
- `urllib.parse.urlparse/urlsplit`: a URL's trailing spaces were stripped (CPython strips only the leading ones),
  `;params` were split for every scheme (CPython: only those of `uses_params`), and the netloc checks were missing
  (unbalanced or invalid brackets, an IPv4 address in brackets, NFKC delimiters raise `ValueError`). `FileResponse`
  guessed its type with the same lenient split.
- Attribute reads that a runtime value provides (httpx `Response.reason_phrase`, `insert().excluded`,
  `__wrapped__`, `OAuth2PasswordRequestForm.grant_type`/`client_id`/...) or that a raw ASGI middleware sets through
  `scope["state"]` (`request.state.<key>`) were refused at generation as certain AttributeErrors, unless another
  module of the project had a `__getattr__`.
- With Starlette before 1.3, an exception raised while a `StreamingResponse` (or a generator endpoint) streams
  is logged as Python logs it: re-raised out of anyio's `ExceptionGroup`, which becomes its `__context__`.
- A `StreamingResponse` chunk that is neither `str` nor `bytes` (an `int`, a `float`) was sent as its `str()`;
  it now fails as in Starlette (`AttributeError: 'int' object has no attribute 'encode'`, logged, the connection
  dropped).
- A task that failed without being awaited, or an async generator, stored in the request's state
  (`request.state.t = asyncio.create_task(...)`) kept the whole request alive until the process exited (a few KiB
  per request), and asyncio's « Task exception was never retrieved » / the generator's `finally` never came. Both
  are now freed with the request, as in CPython; the log line carries the task's repr (`future: <Task finished
  name=... coro=<f() done, defined at file:line> exception=...>`).

## [0.8.0] — 2026-10-15

The public contract is frozen ahead of 1.0 (versioning and deprecation policy, tests that pin the command lines,
the binary's options and variables, the error codes and the JSON of `check`), after a second security review of
the runtime. The behaviour follows the project's Python from 3.12 to 3.15. Popular libraries are translated
natively (orjson, PyYAML, croniter, Unidecode, ecs-logging, aio-pika) and the binary applies the project's alembic
migrations itself (`<binary> migrate`). Upgrading is recommended: several fixes below close denials of service
that, depending on the application, need no authentication.

### Added

- [Versioning and deprecation](https://larrymotalavigne.github.io/py2axum/reference/versioning/): the public
  contract semver covers from 1.0 (command lines, the binary's options and environment variables, the supported
  subset, the JSON of `check`, the error codes) and how a deprecation works. The JSON output of `check` is
  published as a JSON Schema (`reference/check-schema.json`); tests freeze the contract.
- orjson 3.13: `loads` (the decoder's messages and positions, its depth and memory limits) and `dumps` with every
  `OPT_*`, `default=` and the types orjson serializes natively; FastAPI's `ORJSONResponse` (returned,
  `response_class=`, `default_response_class=`).
- Unidecode 1.4: `unidecode`, `unidecode_expect_ascii`, `unidecode_expect_nonascii`, `errors=`, `UnidecodeError`
  (tables generated from Unidecode by `tools/gen_unidecode.py`).
- ecs-logging 2.3: `StdlibFormatter`, called or from `dictConfig`.
- PyYAML 6.0 native (`import yaml`): `safe_load` / `load(Loader=yaml.SafeLoader)` of a str, bytes or file object
  and `safe_dump` / `dump` / `safe_dump_all` / `dump_all` with all their options, ported from PyYAML's pure-Python
  loader and dumper: same values, same text byte for byte, same exceptions (`str()`, `problem`, `context`, marks
  and snippets). The other loaders, libyaml's classes and the extension API are refused at translation; the
  differences (integers beyond 64 bits, recursion depth, `yaml.dump` of `!!python/object` values) are documented.
- aio-pika 10: exchanges of every type (`declare_exchange`, `get_exchange`, `bind`/`unbind`), `queue.consume`/`cancel`,
  `queue.iterator()`, `message.process()`, `ack`/`nack`/`reject`, `purge`, `delete`, `set_qos`, the broker's channel
  errors as aiormq's classes; a published message gets aiormq's defaults (delivery mode 1, priority 0, a `message_id`).
- croniter 6.2 native (`dynrt/croniter.rs`, a port of croniter.py): `croniter(...)` and its `get_next`/`get_prev`/
  `get_current`/`set_current`/`next()`/`all_next()`/`all_prev()`, `is_valid`, `match`, `match_range`, `expand`,
  `croniter_range`, its exceptions; 5, 6 and 7 fields, `L`, `W`, `#`, names, aliases, `H` hashing, time zones
  and DST transitions (`timezone`, `ZoneInfo`). `R` (random) fields are refused (see the known differences).
- `datetime` (the class) as a value: `ret_type=datetime`, `type(x) is datetime`, called later.
- `datetime.replace(fold=)`; `replace()` keeps the fold of the datetime, as CPython does.
- `asyncio.Future` (`loop.create_future()`), `asyncio.Event`, `asyncio.wait_for` of a future or task held in a variable.
- A plain class's `__new__` (a factory returning something else, a singleton through `super().__new__(cls)` /
  `object.__new__(cls)`), and `cls.X` read or assigned on a class held as a value.
- `<binary> migrate`: the project's alembic migrations applied by the binary, rendered at translation time by
  alembic's offline mode (docs/advanced/migrations.md); revisions that read the database are refused with file:line.
  New error code `P2A0510` (an alembic option or layout `migrate` cannot reproduce); the binary's subcommands
  (`migrate [upgrade [head|<revision>]]`, `migrate current`, `migrate --sql`) join the frozen public contract.
- `str.isprintable()`.

### Changed

- `unicodedata.normalize`/`combining` and `str.isalpha()`... follow the Unicode version of the project's Python
  (15.0 to 17.0), even when the translating Python is newer.
- `re.error` is named `PatternError` from Python 3.13 (`type(e).__name__`, tracebacks); `re.PatternError` is
  refused for a 3.12 project.
- `socket.create_connection(timeout=)` reads its timeout like CPython's `settimeout`: `Decimal`/`Fraction` from
  3.15, each version's messages (down to the patch release for an out-of-range delay, probed like the other
  backported fixes, so a distribution's Python is matched), `ValueError`/`OverflowError` for NaN and huge values
  (they made the request panic).
- A synchronous generator consumed synchronously (`list()`, `for`, `next()`, a comprehension) is refused at
  translation (P2A0118); it answered 500. `StreamingResponse(gen())` is unchanged.

### Deprecated

- `--backend` now prints a deprecation warning (it has no effect since 0.4); it is removed in 1.0, with the key
  `backend` of the JSON of `check`.
- `--report`: an internal coverage report, outside the stable interface; use `py2axum check --format json` or
  `--format markdown`. It prints a deprecation warning.

### Fixed

- `check --help` lists `mount` among the values of `--python-side` (it was accepted, not shown).
- `check`: the first reason of a route refused for several reasons, the order of its blockers and of the
  whole-application errors no longer change from run to run (hash order of the call graph): closest to the
  route first, then source order.
- `StreamingResponse(content)` started iterating its content as soon as the response object was built: the
  generator could read state before the endpoint finished or before a `Depends(scope="function")` dependency
  ran its exit code (seen as an intermittent conformance difference). It now starts when the body is sent,
  as Starlette does.
- `asyncio.sleep(delay, result)` returns `result`; `asyncio.sleep`/`wait_for` read their delay like asyncio
  (`TypeError` for a `str`, or a `Decimal` outside uvloop; a timeout <= 0 raises `TimeoutError` without running
  the awaitable, which used to run once).
- SQLAlchemy: objects INSERTed then rolled back are transient again, with their attributes and loaded
  relationships; pending objects a failed flush did not reach are transient too.
- `email.utils.formataddr` raises `UnicodeEncodeError` for a non-ASCII address, as CPython.
- Memory: a finished task stored in the request's state no longer keeps the request alive; anonymous subqueries
  are numbered per statement (`anon_1`...), so a query is the same SQL at every request.
- `repr()` of a string escapes non-printable characters as CPython does (`'\xa0'`, `'\u200b'`, `'\U000e0001'`);
  only ASCII control characters were escaped.
- `str()` of a Pydantic model is its fields separated by spaces (`BaseModel.__str__`), not its repr.
- `exc.__cause__` / `exc.__context__` of an exception without a chain is `None` (was an `AttributeError`).
- A dict keeps the first object of a key set again under an equal key (`{1: "a", True: "b"}` is `{1: "b"}`).
- `int(b"42")`.
- `repr(int)` / `str(int)` of a builtin type is `<class 'int'>`; `type(zoneinfo.ZoneInfo(...)).__name__` is
  `ZoneInfo`.





### Security

- `rustls` 0.23.46 in the shipped lock file (was 0.23.45): RUSTSEC-2026-0346 to RUSTSEC-2026-0349.
- Second review of the runtime, see [Security](https://larrymotalavigne.github.io/py2axum/advanced/security/):
  - `str.zfill`/`ljust`/`rjust`/`center`, format specs and `%`-formatting with a width or precision from the
    request raise `MemoryError`/`ValueError` like CPython (a negative width panicked, a huge one aborted);
    `range()` near the 64-bit edge no longer loops for ever, and a range too large to hold is `MemoryError`;
    `Fraction(text)` applies `int()`'s 4300-digit limit; a JSON integer past 4300 digits is a 400.
  - Raw ASGI apps and middlewares: `send` waits for a client that does not read (it buffered the whole response);
    a stream the application leaves unfinished closes the connection instead of holding it. WebSocket
    `send` waits likewise (the outgoing queue was unbounded).
  - A raw ASGI middleware next to WebSocket routes is refused at translation unless it hands non-HTTP scopes
    straight on (the binary does not run raw middlewares for WebSocket connections: an auth or rate-limit check
    written for websockets would have been skipped).
  - uvicorn's proxy headers: the last `X-Forwarded-Proto` is used, `*` trusts everyone only alone, networks with
    host bits set are literals (as uvicorn); `regexp_match(flags=)` binds its flags.

## [0.7.1] — 2026-10-14

Security release. Upgrading is strongly recommended.

### Security

- **Denial of service fixed (all earlier versions):** a request reaching a `Decimal` field with constraints, or
  `Decimal` arithmetic in the application, could make a generated binary spend minutes of CPU and then exhaust its
  memory until the process was killed, without authentication. `Decimal` now follows CPython's default context
  and its limits. Upgrading is strongly recommended.
- MCP (`mcp` SDK, stateless server): a `GET` event stream now ends when the client leaves. Before, each disconnect
  kept a pooled database connection open in a transaction until the pool was exhausted (all earlier versions serving MCP natively).

## [0.7.0] — 2026-10-14

Pydantic documentation corpus at 90.8 % and SQLAlchemy ORM documentation corpus at 93.8 % (both past the 1.0
threshold, none differing), logging identical to Python's, Python 3.15, and memory leaks found by a long soak.

### Added

- Pydantic: `validation_alias` (`AliasPath`, `AliasChoices`), `serialization_alias`, `alias_generator` (evaluated at
  translation), `validate_by_alias`/`validate_by_name`/`serialize_by_alias`; `Field(exclude=, exclude_if=,
  frozen=, multiple_of=, union_mode=, strict=, discriminator=)`; strict mode (config and field); discriminated
  unions (by a `Literal` field or by `Discriminator(fn)` with `Tag`); `AfterValidator`/`BeforeValidator`/
  `PlainValidator`/`WrapValidator`, `@field_validator(mode="plain"|"wrap")`, `@field_serializer`,
  `@model_serializer`, `PlainSerializer`/`WrapSerializer`; specialised generic models, `RootModel`, `SecretStr`,
  `UUID1`..`UUID8`, `Fraction`, `complex`, `deque`, `Counter`, `OrderedDict`, `Sequence`, fixed tuples,
  `default_factory` taking the validated data, `PydanticCustomError`. Pydantic documentation corpus 52.1 → 90.8 %
  (216/238); the messages of pydantic 2.12 and 2.13 are reproduced by the locked version.
- SQLAlchemy ORM inheritance: single table (`polymorphic_on` / `polymorphic_identity`), joined table (one table
  per class, flushed table by table), `of_type()`, `with_polymorphic()`, `selectin_polymorphic()`,
  `polymorphic_load="selectin"|"inline"`.
- SQLAlchemy ORM: deferred columns (`load_only`, `defer`, `undefer`, `undefer_group`, `defaultload`,
  `deferred(group=)`, raise-loading), `composite()`, `@hybrid_property` / `@hybrid_method` (with `.expression`
  and `.setter`), write-only collections (`WriteOnlyMapped`), `eager_defaults=True`, `Session(engine)` /
  `AsyncSession(engine)` in an endpoint, `session.expire(obj[, names])`, `__init__` on a mapped class,
  `filter_by()` on hybrids and composites, `and_()`/`with_parent()`/`add_columns()`/`aliased(name=)`, and Core
  `Table()` objects used as values (`t.c`, `alias()`, `bindparam()`, executemany, `insert().values([...])`,
  FULL OUTER JOIN). `dataclasses.astuple()` / `asdict()`, `typing.cast()`.
- Logging identical to Python's: the `logging` hierarchy (levels, propagation, handlers, `Formatter` with `%`, `{`
  and `$` styles, filters, `LoggerAdapter`, `basicConfig`, `dictConfig`, python-json-logger 4.0), uvicorn's
  startup, shutdown and access lines and its command-line options, and tracebacks with the project's frames,
  source lines and carets (`raise ... from`, `__context__`). The mcp SDK's own log lines are reproduced. Module
  and factory statements that configure logging are translated. See
  [Logging](https://larrymotalavigne.github.io/py2axum/advanced/logging/).
- SQLAlchemy ORM: `relationship(secondary=<Table>)` (many-to-many: joins, lazy and `selectin` loading, `any()`,
  association rows written at flush and deleted with their owner; `back_populates`/`backref`, cascades and
  `order_by=` on it are refused), `session.expunge()` / `expunge_all()` (`DetachedInstanceError`), `expire_all()`,
  `obj in session`, relationship criteria (`Child.parent == obj`, `Parent.children.contains(obj)`,
  `join(Target, Model.rel)`, aliases), `execution_options(populate_existing=, autoflush=, yield_per=)`; bulk
  `execute(insert(Model), [dicts])` (and with `returning(Model)`), bulk `update(Model)` by primary key,
  `insert().values([dicts])` as SQLAlchemy builds it; results are exhausted and closed like SQLAlchemy's
  (`ResourceClosedError`). `Model.__mapper__` and Core use of `Model.__table__` are refused at translation.
- SQLAlchemy documentation corpus: the doctests of the 2.1.4 documentation replayed as routes against the binary;
  ORM scope 93.8 % identical (243/276, the 1.0 threshold is 90 %), Core 88.7 %, none differing
  ([coverage](https://larrymotalavigne.github.io/py2axum/coverage/)).
- Python 3.15 is supported (3.12 to 3.15; [Python versions](https://larrymotalavigne.github.io/py2axum/python-versions/)
  reviews every change of 3.14 and 3.15). The binary follows the project's Python: `--python X.Y`, else its
  `.python-version`, else the Python running py2axum when `requires-python` allows it (it used to take the lowest
  Python `requires-python` allowed). `sys.version_info` is folded against it at translation, and a branch it
  rules out is never translated (version-gated imports work).
- Python 3.14 and 3.15 constructs: comprehension unpacking (`[*x for x in xs]`, `{**m for m in ms}`), `lazy import`
  of libraries, `case +1:`, `with var.set(x):`, `Path.copy()/move()`, `uuid.uuid7()`, `re.prefixmatch()`, `\z`,
  `Counter ^ Counter`, `bytes.replace(count=)`, `format()` separators in the fractional part, json's exception
  notes, `add_note()`/`__notes__`; and the messages that changed between 3.12 and 3.15 (ZeroDivisionError,
  unpacking, `dict(seq)`, context managers, `strptime('%d')`, base32, unpadded `urlsafe_b64decode`,
  `fromtimestamp`, `os.urandom`). Template strings and lazy imports of project modules are refused.

### Security

- `nh3.clean()` runs on `ammonia` 4.2.3 (was 4.2.1): RUSTSEC-2021-0157 (`url()` escaping in CSS style attributes)
  and RUSTSEC-2021-0158 (SVG animation tags), both XSS in the sanitizer.

### Fixed

- Memory: a nested function that calls itself no longer keeps its closure alive (113 KiB per request with 100 KB
  captured); ORM objects left by a failed flush, a rollback or a delete are released with their session; a
  streamed response with a `yield` dependency cut by the client released its connection only at shutdown (each
  cut kept a connection idle in transaction: 200 cuts blocked every route using the database); an abandoned raw
  ASGI app is completed.
- Sentry: an exception already captured for a request is not sent a second time by the `logging` integration
  (uvicorn logs it after the response), and a record's `extra` leaves out sentry-sdk's common record attributes.
- `session.close()` did nothing: objects are now detached and the transaction rolled back, like SQLAlchemy; a
  session dropped with an open transaction rolls it back at once. Reserved table and column names (`"user"`,
  `"order"`) are quoted.
- `base64.b64decode` (and python-jose's padding) follows the padding rules CPython rewrote in its 3.13.13 and
  3.14.4 security releases (data after the padding is decoded, excess `=` skipped), by the project's patch
  release. Tracebacks in the logs print the exception's notes, and the `ValueError` an enum's `_missing_`
  chains as `__context__` (`'x' is not a valid E`, "During handling of the above exception...").
- `socket.getaddrinfo`, `socket.create_connection` and `asyncio.open_connection` check the host like CPython's
  `idna` codec before any lookup (a label of 64 characters or an empty one: `UnicodeEncodeError`, `UnicodeError`
  on 3.12); the binary passed it to the C library. `UnicodeError` exists as the base of the encode/decode errors.
- Behaviours that changed inside a Python minor follow the project's patch release: `strptime('%e')` is a bad
  directive before 3.13.6. The patch is the one `--python X.Y.Z` or `.python-version` names, else the Python
  running py2axum when it runs the project's minor (`PY2AXUM_PYTHON_VERSION` overrides it at start).
- Differences with Python on every version: `"a".count()` or `[1].count()` without an argument aborted the
  request, `str.replace(count=)` was ignored, `bytes.replace` was missing, `base64.b32decode(map01=)` was ignored
  and the base64 functions dropped unknown keyword arguments, `f"{n:_}"` grouped with commas, the builtin
  `format()` was not translated, `FileResponse` guessed the media type from a table of 20 extensions (now
  CPython's table of the project's Python and the host's `mime.types`, as `mimetypes` reads them), and the types
  named by operator and context-manager errors (`Counter`, `_thread.lock`). The text of a type expression
  (`repr(int | str)`) is refused instead of `<GenericAlias>`.
- Async sessions: reading an attribute that needs IO (expired or deferred column, unloaded relationship) raises
  what the locked SQLAlchemy raises: `MissingGreenlet` up to 2.0, a `StatementError` wrapping it since 2.1
  (`except MissingGreenlet` is now translated, and does not catch it). `e.orig` is readable on SQLAlchemy errors
  (the psycopg exception of a database error, the cause of a missing bind parameter or an unknown enum value).
  The primary key of an object expired as a whole (commit, `expire_all()`) was read from memory; it now needs IO
  like the other columns.
- The declarative base's `type_annotation_map` was ignored: `Mapped[datetime]` became a `DateTime` without time
  zone where the map said otherwise. It is applied, and `Mapped[Decimal]` / `Mapped[time]` map to `Numeric` / `Time`.

## [0.6.0] — 2026-10-09

FastAPI documentation corpus at 90.6 %, raw ASGI middlewares three times cheaper, error codes and `py2axum
watch`, a builder Docker image, and the constructs the remaining routes of real applications needed.

### Performance

- Raw ASGI middlewares: the cost per layer drops from ~2.7 µs to ~0.85 µs. The binary advances the middleware's
  future itself instead of spawning a task per layer (code left after the response, or a client gone, still
  finishes in its own task, never aborted), the ASGI channels are a queue plus a waker, and a known-length body is
  forwarded without a oneshot. On the reference app with 11 layers, `POST /describe` goes from ~24k to ~40k req/s.

### Added

- Error codes: every refusal has a stable code (`P2A0201`: library call not supported, ...) and the command line
  prints it with the `file:line`, why the construct is refused, what to do and a link to its entry in the new
  [error code reference](https://larrymotalavigne.github.io/py2axum/reference/errors/). `py2axum check --explain
  CODE` prints the same in a terminal.
- `py2axum check`: a suggestion per refused route (the alternative py2axum knows, the `--python-side` flag that
  leaves it to Python), a summary of what would make the most routes native, `--format text|json|markdown`
  (`--json` stays a synonym).
- `py2axum watch`: regenerate, rebuild (debug profile, only the generated crate) and restart the binary on each
  change of the project, with debounce; a failed cycle keeps the previous binary running.
- Guide: [Migrating an existing application](https://larrymotalavigne.github.io/py2axum/getting-started/migration/),
  from `check` to a reversible switch in production.

- Coverage of the FastAPI documentation corpus: 79.6 % → 90.6 % of the exercised examples identical, none
  differing ([coverage](https://larrymotalavigne.github.io/py2axum/coverage/)). Newly native:
  `yield` dependencies with `try/except` around the `yield` (the request's exception is raised there, as FastAPI
  does; one swallowed is `FastAPIError`), `Depends(scope="function")`, an instance with `__call__` as a
  dependency, `app.dependency_overrides[f] = g` at module level, generator endpoints (JSON Lines,
  `EventSourceResponse` with `ServerSentEvent`, `StreamingResponse`), `response_model_include=` /
  `_exclude=` / `_exclude_unset=` / `_exclude_none=`, a `Response` class as the return annotation,
  `FastAPI(default_response_class=)`, `FastAPI(root_path=)`, unannotated parameters (`Any`),
  `fastapi.exception_handlers` defaults called from an application's handler, `TrustedHostMiddleware`,
  `HTTPSRedirectMiddleware`, `model_config` `val_json_bytes`/`ser_json_bytes`, pwdlib's Argon2
  `PasswordHash.recommended()`, `anyio.sleep`.

- SQLAlchemy's JSON index and its typed accessors, on `JSON` and `JSONB` columns: `col["k"]` / `col[0]` with
  `as_string()`, `as_integer()`, `as_float()`, `as_boolean()`, `as_numeric(precision, scale)`, `as_json()`
  (`CAST(col ->> 'k' AS VARCHAR)`, as SQLAlchemy's PostgreSQL compiler renders it); `sqlalchemy.true()` and
  `false()`. A JSON path index, `col[("a", "b")]`, is refused at translation.

- Constructs of real applications: Pydantic `serialization_alias=` / `validation_alias=` (distinct input and
  output names; `AliasChoices`/`AliasPath` refused), closures with cells as CPython builds them (late binding,
  comprehensions included) and `nonlocal`, `TypedDict` called as a `dict` (refused as a `response_model`),
  `Model.__table__` (columns and their real SQLAlchemy types, computed at translation), `StaleDataError` when a
  flushed `UPDATE` matches no row, `html.unescape` with CPython's HTML5 tables, icalendar 7.0
  (`Calendar`/`Event`/`Alarm`, `add`, `add_component`, `to_ical`, byte-identical output), `os.remove` /
  `os.unlink`; the runtime's `OSError`s carry `errno`, `strerror`, `filename` and the subclass of their errno.
- SQLAlchemy: `with engine.connect()` / `engine.begin()` on a synchronous `create_engine`, `executemany` through
  `execute(text(...), [dicts])`, `||` for a string `+` on SQL expressions; `execute/scalars/scalar(stmt, params)`
  outside `text()`, which failed at run time, is refused at translation with `file:line`.
- asyncpg 0.32 in the supported range.
- Docker: `ghcr.io/larrymotalavigne/py2axum`, a builder image (Python 3.14, py2axum, a pinned Rust toolchain and the
  runtime's crates precompiled with the release profile), multi-arch, with SBOM and provenance; a multi-stage
  example whose final image holds the binary alone ([Docker image](https://larrymotalavigne.github.io/py2axum/advanced/docker/)).
- SQLAlchemy documentation corpus: the doctests of the 2.1.4 documentation replayed as routes against the binary
  (480 examples, none differing; the starting point of the 0.7 work).

### Fixed

- A value stored in a `JSON` column (not `JSONB`, which PostgreSQL normalizes) was written as compact JSON,
  `{"a":1}`, where SQLAlchemy writes `json.dumps`' text, `{"a": 1}`; `->>` on an object or a list returned the
  other text.
- A request-scoped `yield` dependency of a streamed response closed before the stream instead of after it
  (without a session dependency).
- An `HTTPException` with a no-body status (204, 304) lost its `headers=`.
- Generator endpoints under FastAPI < 0.141 follow that version: JSON Lines and Server-Sent Events responses
  ignore `status_code=` (200), and the items of a route declared on an `APIRouter` are neither validated nor
  filtered by their `AsyncIterable[T]` type (`include_router` drops it), as FastAPI does.
- `x in y` on a value that is no container raises Python 3.14's message ("is not a container or iterable")
  when the project runs on 3.14.

## [0.5.1] — 2026-10-09

### Fixed

- **itsdangerous `loads(..., return_timestamp=True)` answered 500.** The keyword was accepted at transpile time and
  refused only at run time, so magic-link and 2FA logins built on it failed in production. `URLSafeTimedSerializer.loads`
  now takes `s, max_age, return_timestamp, salt` positionally or by keyword and returns `(payload, datetime)` with an
  aware UTC datetime, like itsdangerous 2.2; `BadSignature.payload`, `BadTimeSignature.date_signed` and
  `SignatureExpired.date_signed` are set as the library sets them. Any other argument is refused at transpile time
  with `file:line`.
- `isinstance(x, int | float)` (and `X | None`, `Optional[X]`, `Union[X, Y]`) raised "cannot be a parameterized
  generic" in the binary: a 2FA login of a production app answered 500. A union is now tested member by member, as
  CPython 3.10+ does.
- Arguments the runtime dropped or ignored are now implemented or refused at transpile time (an audit of the
  library map after the bug above):
  - `str.encode(encoding, errors)` always produced UTF-8; it now implements utf-8, utf-8-sig, latin-1 and ascii with
    `errors=` strict, ignore or replace (`UnicodeEncodeError` as CPython). `str(b, encoding)` decodes like
    `bytes.decode` instead of lossy UTF-8.
  - `str.find/rfind/index/rindex/count/startswith/endswith(sub, start, end)` and `list/tuple.index(x, start, stop)`
    ignored their bounds; `set.update(*others)` read only the first; `Match.groups/groupdict(default=)` ignored the
    keyword.
  - Starlette responses given positional arguments read `FileResponse`'s `filename` and `content_disposition_type` at
    wrong indexes and dropped a positional `background`; `background=`, `stat_result=` and `method=` are refused.
  - `MIMEApplication(data, Name=...)`, `MIMEMultipart(..., **params)` dropped their Content-Type parameters;
    `policy=`, `boundary=`, `_subparts=`, `_encoder=` and arguments of `as_string()`/`as_bytes()` are refused.
  - `Path.read_text/write_text(encoding=, errors=, newline=)` were ignored (and `read_text` decoded lossily).
  - csv: `DictReader(restval=, restkey=)` were ignored; a positional dialect is refused.
  - redis: `scan_iter("pattern:*")` ignored a positional pattern and scanned every key; `_type=` and
    `set(px=timedelta)` were dropped.
  - `model_validate_json(strict=...)` ignored its keywords; class methods such as `Model.model_validate(...)` now go
    through the transpile-time keyword check.
  - HTTP client responses (`resp.text(encoding=)`, `resp.json(loads=)`...), SQL `col.like(pattern, escape)` given
    positionally, a second positional of `deque` methods and `Pattern.search(s, pos)` raise instead of ignoring the
    argument.

## [0.5.0] — 2026-10-09

### Security

- **Denial of service fixed (all earlier versions):** a small request carrying a deeply nested JSON body could
  overflow the stack of a generated binary and abort the process, without authentication. The decoder is now
  iterative (400 past 10 000 levels, as CPython), the runtime threads get a larger stack (`PY2AXUM_STACK_SIZE`),
  and recursive walks raise `RecursionError` instead of overflowing, so the worst outcome is a 500, never an
  abort. Upgrading is strongly recommended.
- Security review of the runtime before 1.0: threat model, guarantees and differences with uvicorn/Starlette in
  the new [docs/security.md](https://larrymotalavigne.github.io/py2axum/advanced/security/). Hardening of SQL identifiers given as strings, request input
  handling (nesting, sizes, multipart limits, cookies), `Path.resolve()`, response headers, bcrypt off the event
  loop, the `--python-side` relay and the multiprocess Prometheus files. Upgrading is recommended.
- New settings: `PY2AXUM_MAX_BODY` (optional 413 on large bodies, off by default) and `PY2AXUM_STACK_SIZE`.
- CI audits the runtime's `Cargo.lock` with `cargo audit` (`tools/cargo_audit.sh`).

### Added

- Documentation site (MkDocs Material, <https://larrymotalavigne.github.io/py2axum/>), organised like FastAPI's:
  getting started, tutorial, advanced, reference, release notes. Its examples (`docs_src/`) are tested against the
  binary in CI.
- pydantic 2.14 / pydantic-core 2.50 supported (range `pydantic>=2.12.0,<2.15`, `pydantic-core>=2.41.1,<2.51`). The
  binary reproduces the minor the project locks: error URLs `…/2.14/v/…`; UUID messages of the uuid crate 1.23.4
  (0-based positions, `invalid length: found N`, the requested form); `EmailStr` refusing CR/LF; `Decimal`
  `max_digits`/`decimal_places` counted without `Decimal.normalize()`'s 28-digit rounding; an Enum `_missing_`
  that raises something other than a `ValueError` propagates. Projects on 2.12/2.13 keep their messages. See
  docs/supported.md, "Pydantic behaviours that follow the project's minor version".
- The project's library version is also read from an `==` pin in `requirements*.txt` / `pyproject.toml` (only
  `uv.lock` was read), or chosen inside the project's specifier when the installed one is outside it.
- CI: the `versions` matrix runs pydantic 2.12 (min), 2.13 (`python -m py2axum.versions pydantic2.13`) and 2.14 (max).

### Fixed

- `x is Enum.MEMBER` between Enum members was always `False`, and a header parameter `x: list[str] = Header()`
  answered 422 instead of collecting every occurrence: two silent wrong answers found by the documentation's
  examples.
- `model_config`'s `str_strip_whitespace` / `str_to_lower` / `str_to_upper` were ignored on `EmailStr` fields.
- An Enum `_missing_` raising a `ValueError` gave a `value_error` instead of pydantic's `enum` error; returning a
  value that is neither `None` nor a member gave the `enum` error instead of Enum's `TypeError`.
- `Decimal` digit bounds compared only the normalized value: a value whose written form fits passes, as in pydantic.
- Refused instead of mistranslated: a `default_factory` taking the validated data (`lambda data: ...`, called
  without arguments before), length constraints on `Iterable[T]`.

## [0.4.0] — 2026-10-08

### Breaking

- **The typed backend is removed** (`--backend typed`, `codegen.py`, `body.py`, `runtime/rt.rs`). It emitted
  statically-typed Rust for simple CRUD handlers only and covered none of the real applications measured; every
  application already went through the general backend. `--backend typed` now exits with an error
  ("removed in 0.4: use the default backend"); `--backend auto` and `--backend dyn` are accepted until 1.0 and
  change nothing. `py2axum check` and `--report` no longer try it first; `check --json` keeps `"backend": "dyn"`.

### Added

- Large list responses streamed at constant memory (what only the typed backend did): an endpoint with
  `response_model=list[Schema]` returning `(await session.execute(stmt)).scalars().all()` (or `session.scalars`,
  or through a local) reads the rows with a cursor and sends the JSON array in 64 KiB blocks — 57 MB served with
  13–16 MiB of RSS instead of 1.6 GiB, first byte after 16 ms instead of 2.9 s, identical bytes. Only when
  nothing observable changes (the session wrote nothing, one mapped class without loader options, a schema of
  plain columns...); `PY2AXUM_STREAM_CHUNK`, `PY2AXUM_STREAM_MIN_ROWS` and `--no-stream` keep their meaning. See
  docs/supported.md, "Large list responses".
- `app.state` (set in the lifespan, read through `request.app.state`).
- Raw ASGI middleware: a project class with `__init__(self, app, ...)` and `async def __call__(self, scope, receive,
  send)`, registered with `app.add_middleware(...)` or `FastAPI(middleware=[Middleware(...)])`, runs natively in
  Starlette's stack around the router (shared scope, uvicorn's `receive`, `send` chunked without a
  `content-length`, rewritten scope as a new request, wrapped `receive`/`send`, short-circuit answers, exceptions
  raised through the stack). `starlette_context`'s `RawContextMiddleware` with `RequestIdPlugin` and
  `CorrelationIdPlugin`, `FastAPI(strict_content_type=)` and `traceback.format_exception(...)` /
  `format_exception_only(exc)` are translated too. An application without raw middleware gets no extra layer;
  each one costs about 2.5 µs per request. See docs/supported.md.
- `create_engine(connect_args=...)` / `create_async_engine(connect_args=...)` applied to every pooled connection:
  libpq `options` (`-c name=value`, `-cname=value`, `--name=value`, split like libpq, `\` escapes) and asyncpg's
  `server_settings`. A `TimeZone` setting overrides the time zone discovered from the server, so timestamps decode
  in the session's zone as psycopg does (they kept the server's zone before). Any other literal libpq option is
  refused at transpile time.
- PyJWT 2.15 (`import jwt`) translated natively, like python-jose: `jwt.encode` (`algorithm=`, `headers=`,
  `sort_headers=`; `exp`/`iat`/`nbf` datetimes encoded from a copy), `jwt.decode` / `decode_complete` (every
  `options=` key, `audience=`, `issuer=`, `subject=`, `leeway=` as a number or a timedelta),
  `get_unverified_header`, with HS256/HS384/HS512 and `none`: identical tokens, the same checks in the same order,
  the `jwt.exceptions` hierarchy and messages. An asymmetric algorithm written as a literal, `json_encoder=`,
  `detached_payload=`, `verify=` and `PyJWK`/`PyJWKClient` are refused at transpile time.

### Changed

- Assigning another module's attribute (`config.LIMIT = 2`) is refused at transpile time with `file:line`; the
  binary used to fail at startup.
- `json.loads(bytes)` with invalid UTF-8 raises CPython's exact `UnicodeDecodeError` message.
- `JSONB` columns: `contains` (`@>`), `contained_by` (`<@`), `has_key` (`?`), `has_any` (`?|`) and `has_all`
  (`?&`) are compiled to PostgreSQL's operators, their argument typed as SQLAlchemy types it. They raised a
  `TypeError` at run time (a 500 where Python answered).
- A path left to Python (`--python-side`) answers FastAPI's 404 when `PY2AXUM_PYTHON_URL` is unset, as
  documented; it used to fall through to a translated route whose pattern also matched (`/books/export.zip`
  reached `/books/{book_id}` and answered 422).
- An exact pin equal to the lowest version a feature needs is accepted: `starlette==1.7.0` was refused for
  WebSocket routes ("need starlette>=1.7.0").
- `tests/conformance.py --scenario path/to/scenario.py` (and `tests/difftest.py`) load a scenario file from
  anywhere, so an application can keep its scenario in its own repository.
- New example, `examples/bookshelf`: users, JWT Bearer authentication (PyJWT), CRUD, relationships, 422s, a
  WebSocket and one route left to Python; its conformance scenario (75 steps) and `compare.sh` run in CI.
  `examples/docker`: a reference multi-stage Dockerfile, the Python sidecar and a Compose file for the hybrid
  mode. `bench/bench.py --target bookshelf` measures it against FastAPI.
- Documentation: a getting-started guide (`docs/getting-started.md`), the conformance guide rewritten for
  users, the README, `docs/supported.md` and `docs/how-it-works.md` reviewed for first-time readers.
- Translated natively: `validate_assignment=True` with field and model
  validators (before, type, after with `info.data`, model `after`, as pydantic-core; refused with a model
  `before` validator), `Field(min_items=, max_items=)`, `File(default=[])`, a SQL column name other than the
  attribute (`Column("metadata", JSON)`), the `undefer()` loader option (top level, `session.get(options=)`,
  `selectinload(...).options(undefer(...))`), `exists().where(...)` with SQLAlchemy's auto-correlation,
  `regexp_match`, `delete().returning(columns)`, `...` as a sentinel value, `collections.Counter`,
  `asyncio.open_connection` (asyncio's or uvloop's messages, after the project's lock),
  `socket.create_connection`, `cryptography` RSA key generation and serialization (DKIM), and
  `zipfile.ZipFile(io.BytesIO(), "w")` with CPython's bytes.
- A pydantic `ValidationError` raised by an assignment carries the model's `title` (and `str(e)` names it).
- A crate generated from an application with `GZipMiddleware` failed `cargo build --locked`: the shipped
  `Cargo.lock` did not list `tower-http` among the root crate's dependencies. It is now always declared.
- Fixed: a list of `UploadFile` holding an empty string was treated as absent; FastAPI reports it (422).
  `str.splitlines()` split on `\n` and `\r\n` only (CPython's line boundaries now: `\r`, `\v`, `\f`,
  `\x1c`-`\x1e`, `\x85`, U+2028/2029); iterating an `io.BytesIO` (a `StreamingResponse` over one) raised.

## [0.3.1] — 2026-10-08

- An Enum member given to a scalar field is validated as pydantic-core does: an ORM enum column read into
  `status: str` answered 500 (`string_type`) where Python returned the member's value (found in production). A
  `str`/`int` subclass member is its value for `str`, `int`, `float`, `bool` and `Literal` (an `int` one is
  `str(value)` in a `str`); a plain member is `str(value)` in a `str`, its value unchecked in an unconstrained `int`.
- Graceful shutdown in every generated binary (it only existed with a lifespan or `sentry_sdk`; otherwise
  SIGTERM killed in-flight requests): as uvicorn 0.54, new connections refused, idle keep-alive connections
  closed, in-flight requests finished with `connection: close`, WebSocket sessions closed with 1012 on both
  sides, a second signal forces the exit, and the process ends by the signal it received (status 143, uvicorn
  re-raises it; it was 0). The pool is closed before the exit.
- Memory leaked at every call (dyn backend), found by the endurance test: an MCP `tools/call` (its arguments'
  validator), `column.op("...")(value)` (the operator) and `json.dumps(separators=...)` (the separators). Each
  is now built once per distinct value; a test forbids `Box::leak` outside those caches.
- `bench/bench.py --target dynapp --compare`: a performance guard comparing a generated binary with a reference.
- Differential testing (`tests/difftest.py`, extra `difftest`): `replay` replays traffic recorded on the Python app
  (`py2axum.record`, an optional ASGI recorder writing anonymised JSONL) against both servers from the same
  database snapshot, with ddmin minimisation; `gen` generates valid and invalid requests from the OpenAPI schema
  (schemathesis 4 + hypothesis, fixed seed) plus a corpus of edge cases per operation.
- asyncpg (0.31) as the database driver, read from `DATABASE_URL`; `on_conflict_do_nothing/update(constraint=)`.
- `BaseHTTPMiddleware` without its own `__init__`; `deferred()` columns; Jinja2 `env.filters[name] = f`;
  `p: Model = Depends()` (one query parameter per field); `text(...).bindparams()`; `super()` in a model's
  classmethod; `aiosmtplib.send(sender=, recipients=)`; `desc("label")` and labels in GROUP BY / ORDER BY;
  `update(...).returning(Model)` refreshes the session's objects; `session.refresh()` keeps loaded relationships;
  `StreamingResponse` over a synchronous iterable.

### Fixed

- A route reading a module global filled by a function that does not translate (a registry filled in place by
  module-level statements: a loop, `REG[k] = lambda: f()`) was reported native and the error lost: it is now
  blocked (`--python-side auto` moves it). Module-level item and attribute assignments run at startup; an
  assignment to an unmapped library's attribute (`stripe.api_key = ...`) stays on the Python side.
- Request bodies decoded as CPython's `json.loads` does (BOM, UTF-16/32, NaN, exact positions and messages,
  too deep nesting); a `null` body is absent; FastAPI's `strict_content_type`; Starlette's form limits and
  multipart error message follow the locked Starlette version (1.4, 1.7); Starlette's redirect slashes.
- Pydantic: bool from float/Decimal/bytes, int from str, `int_parsing_size` and `bool_type` beyond 64 bits,
  timestamps parsed as speedate does, empty URL; `exclude_none` on extras; `response_model` serialised through
  `dump_json` (NaN and infinities become `null`).
- Dates: years 1–9999 (`OverflowError`), `timedelta.days`, `date()`/`datetime()` checks and messages of CPython
  3.12/3.14; `format()` of `-0.0`, `nan`, `inf`; Python whitespace U+001C–U+001F; `str.is*()` from the
  translating Python's Unicode data; paths containing NUL.
- SQL: an integer out of the column's range raises `DataError` as psycopg does, the exact psycopg class per
  SQLSTATE, `tuple_(...).in_(...)` without a cast; JSON columns keep float round-trips.
- `request.url.path` without tabs and newlines; AMQP names validated as pamqp does (no panic); a status code
  outside 100–599 drops the connection as uvicorn does.

## [0.3.0] — 2026-10-08

- `py2axum check <package>`: every route marked native, python-side or refused with the reason at `file:line`,
  the blockers ranked by routes touched, without generating a crate; `--json`, and an exit status for CI
  (1 when generation would fail, `--fail-under PCT` on the share of native routes).
- Supported version ranges (FastAPI 0.137–0.142, Starlette 1.0–1.7 (1.7 for WebSocket routes), Pydantic 2.12–2.13, pydantic-settings
  2.11–2.15, SQLAlchemy 2.0.44–2.1, psycopg 3.2.12–3.3, httpx 0.28, aiohttp 3.13–3.14, mcp 2.2, Python
  3.12–3.14), tested at both ends in CI. A project locking or pinning a version outside them (`uv.lock`,
  `requirements*.txt`, `pyproject.toml`) is refused with `file:line`; `--allow-untested-versions` overrides.

- `await conn.run_sync(Base.metadata.create_all)` (dyn backend), e.g. in the lifespan: the DDL is compiled at
  translation time by SQLAlchemy from the mapped classes rebuilt statically (no project code runs), then replayed
  with SQLAlchemy's checkfirst (enum types, tables, indexes, comments, foreign keys of cycles).
  `async with engine.begin() as conn` commits on exit. `--python-side lifespan` is no longer needed for it.
- WebSocket routes (dyn backend): `@app.websocket` / `@router.websocket` with dependencies and path/query/header
  parameters, Starlette's `WebSocket` (accept, receive/send in every form, close, `iter_*`, denial responses,
  states), uvicorn's handshake outcomes (403, 500, 1006), exception handlers called with the WebSocket;
  `async for` over async iterators. The typed backend refuses `@router.websocket` (it was skipped silently).
- `x: Annotated[T, Query()/Header()] = default` is optional (it was required); `request.url.path` is
  percent-decoded, as Starlette builds it from `scope["path"]`.

- `sentry_sdk` reports to Sentry (it was a no-op): a native client on the Rust SDK sends the same events as the
  Python SDK (scopes per request, FastAPI/Starlette and logging integrations, transactions, scrubbing,
  `before_send`), each tagged `py2axum.source` with the route's `file.py:line`. Options the binary cannot
  reproduce (`transport=`, `before_breadcrumb=`, profiling...) are refused. An app factory's `init_sentry()`
  runs at startup. `tests/sentry_check.py` compares what a fake Sentry server receives.

- `dateutil.relativedelta` (relative fields), `xmltodict.parse` (default options, namespaces) with
  `ExpatError`, Pydantic private attributes (`PrivateAttr`, `_name`),
  `session.connection()` and its `close()` (readiness probes), `__getattr__`/`__setattr__` on plain classes
  (`object.__setattr__`), enum members with list/tuple values (`str` mixin: `("A",)` is `"A"`), a module-level
  endpoint shadowed by a later function of the same name.

- Applications served next to Starlette: `response_class=` wraps the returned value (HTML, plain text, redirect,
  file); `uuid.UUID` validated as pydantic-core does and `Uuid` columns read and written as `uuid.UUID`;
  `HTTPBasic`; `Jinja2Templates(directory=)` and `TemplateResponse`; `importlib.metadata.version("literal")`
  resolved at compile time from `pyproject.toml` and `uv.lock`. An app wrapped in a project ASGI class
  (`app = Middleware(api)`) is refused (it was served without the wrapper).
- `app.mount(...)` registered last can stay on the Python side (`--python-side mount`, or `auto`).
- Synchronous SQLAlchemy (already in 0.2.0, not listed there): `sessionmaker`, a synchronous generator
  dependency, real lazy loads (also while a `response_model` reads the object), the legacy `session.query` API.
  Now also: self-referential relationships (`remote_side=`), bound parameters shared by an expression reused in
  `GROUP BY`, ORM `@property`s read by a response model, `await` on a synchronous `Session` (CPython's
  `TypeError`).
- `model_json_schema()` computed at translation time; `sqlalchemy.inspect(x, raiseerr=False)`;
  `unicodedata.normalize/combining`; `json.dumps(sort_keys=, indent=)`; `bytes.decode` and bytes methods;
  `BaseModel.dict()`; `request.base_url`; `join(subquery, on)`; `zip(strict=)`; `date/datetime.replace` and their
  methods read as values; f-string format specs through `__format__`; class attributes read on a plain or
  Pydantic class; `Annotated[T, Header()] = default`; python-side routes under a prefix read from the settings.
- `collections.deque` and `defaultdict(deque)`; single inheritance of plain classes, `abc.ABC` and
  `@abstractmethod`; `nh3` (`clean`, `clean_text`, `is_html`) on the `ammonia` crate; `socket.getaddrinfo`,
  `ipaddress.ip_address`/`ip_network`; `email.mime.base.MIMEBase` and `email.encoders.encode_base64`;
  `Fernet.generate_key()`; `string.Template` (`substitute`, `safe_substitute`).
- `break`/`continue` leaving a `try/finally` or a `with`: the `finally` (or `__exit__`) runs first, as in CPython.

### Fixed

- Length constraints of `list`/`set`/`tuple`/`dict` fields and keyword arguments of builtins were ignored
  silently: applied as pydantic-core does, refused otherwise. A validator's error carries the raw input.
- `email.encoders.encode_base64()` of a message without payload raises CPython's `TypeError` (it encoded nothing).
- An app factory's `init_sentry()` is refused only for the functions it reaches (functions queued by other code
  were blamed on it).

### Changed

- `sentry_sdk` is no longer a no-op (see above): a project whose `init_sentry()` passes `transport=` (a Python
  transport class) is refused at translation time; it used to be skipped silently.

## [0.2.0] — 2026-10-07

- `FastAPI(lifespan=...)` is translated: the code before `yield` runs before the server listens, the rest after a
  graceful shutdown on SIGTERM/SIGINT (`PY2AXUM_SHUTDOWN_TIMEOUT`). Async generators run in lockstep with their
  consumer (`anext`, `asend`, `athrow`, `aclose`), `@asynccontextmanager`, `contextlib.suppress`,
  `contextvars.ContextVar`, `Task.cancel()`.
- Raw ASGI routes: `app.add_route(path, obj)` with an object whose class defines `async __call__(scope, receive,
  send)`; `Request(scope, receive)`, response objects called as ASGI apps.
- MCP servers (`mcp` 2.2): `MCPServer`, `@server.tool` (async, structured `dict[str, Any]` output), `ToolError`,
  streamable HTTP without state and with JSON responses, the JSON-RPC envelope validated like the SDK. Tool
  argument schemas are computed at compile time (pydantic 2.13 JSON schema, `py2axum/jsonschema.py`).
- `str(ValidationError)` in pydantic-core's format where the model is known.

- Replacing `aiohttp.ClientSession._request` / `httpx.AsyncClient.request` with a wrapper (timing outgoing calls):
  the clients call it; other assignments to library attributes are refused.

- A project function given the app in the factory (`configure(app)`) runs while the middleware stack is built:
  `app.add_middleware` of project `BaseHTTPMiddleware` subclasses, `app.add_route` (it was skipped).
  Routing objects as values: `request.app`, `app.router.routes`, `route.matches(scope)`, `Match`,
  `request.scope`. `timeit.default_timer`.

- `--python-side auto`: every route that does not translate (and raw `add_route` routes with a literal path) is
  left to the Python application, each printed with the error that blocked it.

- Module-level `try` / `if` / `for` / `while` / `with` statements run at startup (they were skipped), their
  bindings becoming module variables; import fallbacks (`try: import x except ImportError: ...`).

- `sys.settrace` / `threading.settrace_all_threads`: the project's functions report call/return/exception
  events to the trace function (function call metrics); `traceback.extract_tb`. Module-level functions are
  single objects; nested functions may name themselves.
- prometheus_client: `start_http_server`, OpenMetrics output, exemplars, name escapings, restricted registries.
- httpx / aiohttp responses keep their `Content-Encoding` header (the body is decoded by the runtime, not reqwest).
- prometheus_client multiprocess mode: the per-process files (shared with Python workers) and
  `MultiProcessCollector`, `mark_process_dead`.
- prometheus_client 0.26: `Counter`, `Gauge`, `Summary`, `Histogram`, `Info`, `Enum`, labels, `time()`,
  `count_exceptions()`, `track_inprogress()`, `CollectorRegistry`, `REGISTRY`, `generate_latest` (text format byte
  for byte).

- Typed backend: request bodies above 2 MB are read like Starlette does (axum answered 413).

## [0.1.0] — 2026-10-07, first public release

- Ahead-of-time compilation of FastAPI + SQLAlchemy 2.0 (async, PostgreSQL) + Pydantic v2 applications to
  Rust (axum, tokio, sqlx): the **dyn** backend (general) and the **typed** backend (simple CRUD).
- Runtime reproducing CPython, Pydantic v2, SQLAlchemy and Starlette semantics, plus a closed list of
  standard and third-party libraries ([docs/supported.md](https://larrymotalavigne.github.io/py2axum/supported/)).
- `--report`: per-route coverage report with the first blocking construct (`file:line`).
- `--python-side`: paths and `lifespan` left to the Python application, relayed by the binary when
  `PY2AXUM_PYTHON_URL` is set.
- Differential conformance harness (`tests/conformance.py`) and reference applications.
- `pip install`-able package with a `py2axum` command; example application with a multi-stage Dockerfile.
- aio-pika 10: connect, channels, durable queues, publishing with confirms, `queue.get()`.
- `json.loads` accepts bytes (UTF-8/16/32 detection, like CPython).
- `importlib.import_module` of a literal project module (module objects), `callable()`.
- `collections.defaultdict` (builtin type factories), `string.Formatter().vformat/format`; `str.format`
  gains `!r`/`!s` and CPython's error messages.
- tenacity 9 (`@retry` and its strategies). `g`/`e`/`G`/`E`/`X`/`o`/`b` presentations and `#` in `format()`
  and `%`; `ConnectionError` and its subclasses; `logging` level constants and `Logger.log`.

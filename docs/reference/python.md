# Python semantics

py2axum compiles your project's code to Rust over a dynamic value type with CPython's semantics. This page
lists what is reproduced, what is refused (with `file:line`) and the known differences. The standard library is
in [Standard library](stdlib.md), third-party libraries in [Libraries](libraries.md).

## Values, functions, classes, modules

- Values and operators with CPython's semantics: int (64-bit: beyond is an `OverflowError`, refused at translation
  when the expression is made of literals), float formatting and `repr`, str methods (`isdigit`, `isdecimal`, `isnumeric`, `isalpha`, `isalnum`, `isspace` from the Unicode
  database of the Python that transpiles), `%`/`format`/f-strings (presentations `d f % e g x o b` and their upper-case forms, `#`;
  not `n`, `c`, `=` with non-numbers), slicing, comparisons, `**`, bit operators, truthiness,
  `hash()` rules (unhashable Pydantic models and dataclasses unless frozen, `__hash__ = None`, `__eq__`
  without `__hash__`); `hash(int)` is CPython's, other hashes are stable but not CPython's (CPython
  randomizes str hashes anyway). **Known difference:** a set iterates in insertion order; CPython's order
  for strings changes from one process to the next (randomized hashes), so text built from a set of
  strings (`", ".join(ALLOWED)`) cannot be reproduced.
- Builtins take CPython's keyword arguments where the runtime implements them (`sorted(key=, reverse=)`,
  `min/max(key=, default=)`, `enumerate(start=)`, `sum(start=)`, `round(ndigits=)`, `int(base=)`,
  `zip(strict=)`); any other is refused. **Difference:** `zip(strict=True)` over iterables of different
  lengths raises at the call, where CPython raises after yielding the common prefix. `range()` builds its items
  (CPython's range is lazy): a range too large to hold is a `MemoryError` at the call, even if the loop over it
  would have stopped early.
- Functions: keyword/default/`*args`/`**kwargs` binding with CPython's `TypeError`s, closures, lambdas,
  nested functions and decorators (`functools.wraps`, decorator factories; a decorated function is built
  once at startup), recursion, `global` (one cell per process), generators, `match`. A variable a nested
  function or lambda reads is a cell, as in CPython: read when the inner function runs, not when it is defined
  (`[lambda: i for i in range(3)]` all see the last `i`; a variable assigned after the `def` raises CPython's
  `NameError` if read before). `nonlocal` rebinds a local of the enclosing function.
  Refused: `nonlocal` of a parameter of the enclosing function, a project decorator on a method.
- Classes: plain classes (`__init__`, methods, properties, static/class methods, class attributes (a function
  held as one must be wrapped, `impl = staticmethod(f)`: it is then never bound, read through the class or an
  instance; a bare function or lambda, which CPython binds to the instance, and `classmethod(f)` are refused),
  `__slots__`; single inheritance from another plain class and/or `abc.ABC`, `@abstractmethod` (instantiating
  a class left abstract raises CPython's `TypeError` when called by name), `super().__init__(...)`), `@dataclass` (incl. `frozen=True`, `eq=False` (identity, or the base's `__eq__`), `__post_init__`, one plain class as base; `dataclasses.astuple()` / `asdict()`, recursive like CPython), `typing.NamedTuple` classes (fields with literal immutable defaults, methods and properties; a tuple in every sequence operation, compared and hashed as one, `json.dumps` and responses as an array; `_asdict()`, `_replace()`, `_make()`, `_fields`, `_field_defaults`, read-only fields; refused as a Pydantic field or response type, and with its own dunders or `_` methods), exceptions (class attributes,
  methods, `super().__init__` of `HTTPException`), Enums (methods, `_missing_`). Special methods:
  `__str__`, `__repr__`, `__eq__` (used by `str()`, f-strings, `==`, `in`, `index`, `count`, `remove`),
  `__ne__`, `__lt__`, `__le__`, `__gt__`, `__ge__` (CPython's rich comparison: the left operand's method, then the
  right one's reflected method, their result as is; `sorted()`, `min()`, `max()` compare with `<`),
  `__hash__ = object.__hash__`, `operator.eq`/`ne`/`lt`/`le`/`gt`/`ge` as values,
  `__enter__/__exit__`, `__aenter__/__aexit__`; they are `def`s (the only I/O one may do is a synchronous
  session's lazy load, a `__repr__` reading a deferred column: done blocking, as CPython does); other special methods
  are refused. In containers, an exception raised by `__eq__` counts as "not equal" (CPython propagates it).
  Protocol methods (`def`s too): `__iter__`/`__next__` (a `for` loop or a comprehension calls `__next__` between
  its bodies, `break` ends an endless iterator; `list()`, `sorted()`, `sum()`, `in`... consume the whole iteration;
  `__iter__` may return `self`, another iterator, or `iter(container)`, which iterates a snapshot of the container (difference: a container returned as such, which CPython refuses with a TypeError, is iterated too);
  the old protocol: `__getitem__(0)`, `__getitem__(1)`... until `IndexError`), `next(obj[, default])`, `__len__`,
  `__bool__`, `__contains__`, `__getitem__`/`__setitem__`/`__delitem__` (not with a slice: `obj[a:b]` raises a
  py2axum `TypeError`), the arithmetic and bitwise operators with their reflected (`__radd__`) and in-place
  (`__iadd__`) forms, `NotImplemented` passing the turn as in CPython (the right operand first when its class is a
  subclass of the left one's), `__neg__`, `__pos__`, `__invert__`, `__abs__`, with CPython's errors for a wrong
  result (`__bool__ should return bool`, `__len__() should return >= 0`, `iter() returned non-iterator`, and their 3.15 wording). `bool(NotImplemented)` raises TypeError from Python 3.14; before, it is true and CPython's DeprecationWarning is not printed.
  `__getstate__`, `__setstate__`, `__reduce__`, `__reduce_ex__`, `__copy__`, `__deepcopy__` and `__getnewargs__`
  are ignored when nothing translated imports `copy` or `pickle` nor reads them (only those call them), refused
  otherwise. Still refused: `__hash__` as a method, `__index__`, descriptors (`__get__`), `__getattribute__`.
- Bases that only type a class are dropped: `Generic[T]`, `Protocol[T]`, the parameters of a generic project base
  (`class Typed(Box[int])`). A method's default reads the class body first, as CPython evaluates it there
  (`def check(self, tolerance=DEFAULT_TOLERANCE)` after `DEFAULT_TOLERANCE = 300` in the class); a name the class
  body binds again after the method is refused. Statements after `return`, `raise`, `break` or `continue` are
  never run, and never translated.
- `@typing.overload` signatures are dropped (the implementation that follows is the function). A module constant
  made of a version or platform test (`PY3 = sys.version_info[0] == 3`) decides its `if PY3:` like the test itself.
  `object()` is a sentinel (identity, `is`); `bytearray`, `memoryview` and `complex` can be named (an `isinstance`
  member: the runtime never makes the first two), not called.
- `mod.X = v` and `mod.X += v` for a variable of a project module (or of a library translated from source,
  `stripe.api_key = ...`) rebind it, as `global X` in that module would; replacing a function or a class of a
  module stays refused.
- Types as values: `list[X]`, `X | None`, library classes (`BaseModel`, `AsyncSession`...),
  `isinstance`/`issubclass` with run-time types, `inspect.isclass`, `typing.get_args/get_origin/
  get_type_hints` (annotations kept on decorated functions).
- `type(x)`: compared with `==`/`is`/`in`, called (`type(x)(...)` for builtin types), `__name__`/`__qualname__`
  (CPython's names: `Pattern`, `UUID`, `builtin_function_or_method`; a Pydantic model class is a
  `ModelMetaclass`, an Enum class an `EnumType`). Differences: values the runtime does not tell apart report
  the type they are stored as (`frozenset` → `set`, iterators and `range` → `list`), a mapped class reports
  `type`. A project class obtained as a value (`cls(**data)` in a classmethod, `type(m)(a=1)`, a class passed as
  an argument) is called like the class named in the source: Pydantic model, settings (environment read again),
  dataclass, plain class, Enum, `SimpleNamespace`, with CPython's `TypeError`s. Difference: pydantic-settings
  init options (positional arguments, `_env_prefix=`…) on a settings class held as a value raise a `TypeError`.
- `from module import *` binds what CPython binds: the names of the module's literal `__all__`, else its names
  without a leading `_`, the ones it imports included (`HTTPException` imported there from FastAPI). A
  `__all__` that is not one literal list or tuple of strings exports nothing (the names read through the star
  import are refused as unknown).
- Module globals are evaluated at startup in import order, like importing the app; module-level calls too, and
  module-level `try`, `if`, `for`, `while` and `with` statements, whose bindings are module variables
  (`except E as e` names are deleted as in CPython). A `try` whose body only imports and assigns constants is an
  import fallback: its imports were resolved at compile time, so its `except` branches never run.
  `if __name__ == "__main__":` and `if TYPE_CHECKING:` blocks are skipped. Refused: a module variable bound
  by several module-level statements when one of them is compound (`X = 1` then `try: X = f()`).
  Attributes and methods of a library object such as `prometheus_client.REGISTRY` are resolved at run time.
  A module-level assignment to an attribute of a library the binary knows nothing of (`stripe.api_key = ...`)
  is left to the Python side: every read or call of that library is refused already.
  Known difference: a module-level call of a project function that does not translate (typically a logging
  setup with handlers, formatters and filters, which the binary does not reproduce: it logs in its own
  format) fails at startup with an `ERROR:py2axum:module global` line on stderr and the binary goes on
  without its effects; the routes that read a global it would have set raise that error.
- `importlib.import_module("pkg.mod")` with a literal name of a project module returns a module object
  (`getattr`/`hasattr` with run-time names, attribute calls, `__name__`). The module is compiled into the
  binary and its globals are evaluated at startup with the others (CPython: at the first import), so a
  "lazy import" saves neither memory nor startup time. A top-level name that does not translate raises
  when read. Refused: computed module names, library
  modules, modules with `import *`.
- `sys.settrace(f)`, `threading.settrace(f)`, `threading.settrace_all_threads(f)`, `sys.gettrace()`: when a
  project calls one of them, its compiled functions (methods, nested functions, lambdas) report `call`, then
  `return` or `exception` (raised there, coming from a callee, or caught there) followed by `return` with
  None, to a process-wide trace function that is not traced itself. Frames expose `f_globals["__name__"]`,
  `f_code.co_qualname`/`co_name`/`co_filename`/`co_firstlineno` and `f_lineno`; tracebacks `tb_lineno`,
  `tb_frame`, `tb_next`, and `traceback.extract_tb`. Differences: one `call`/`return` pair per invocation (a
  coroutine suspended by an `await` does not report each suspension and resumption), no `line`/`opcode`
  events, only project frames exist (library frames are absent from tracebacks, so depths count project
  frames), a frame's line is the line where its current statement starts. Without such a call the
  functions are compiled without any of this.
- A module-level function is one object (`is`, attributes set on it); a nested function naming itself reads
  its name when called, as CPython's closure cell.
- Memory: values are reference-counted, without CPython's cycle collector. A nested function that calls
  itself (bound once by its `def`) refers to itself weakly and is freed as in CPython; ORM objects linked by
  relationships are released when their session ends. Other reference cycles the application builds
  (`a.append(a)`, two nested functions calling each other, a decorated nested function naming itself, an
  object stored in a container it owns) are never freed: each request that builds one keeps it until the
  process exits. A task or an async generator stored in the request's state (`request.state.t =
  asyncio.create_task(...)`, `request.state.g = agen()`) is freed with the request, as in CPython: a failed task
  never awaited logs « Task exception was never retrieved » then, a suspended generator runs its `finally`. Unless
  a `Request` object outlives the request (kept by the task, the generator or a global): then they stay.
- « Task exception was never retrieved »: its `future: <Task finished ...>` line names the coroutine (`coro=<f()
  done, defined at file:line>`) when the task runs a module-level `async def` of the project; for any other
  coroutine (a method, a nested function) the line has no `coro=` part.
- Object addresses in reprs (`<X object at 0x...>`, SQLAlchemy's `<X at 0x...>`) are the binary's own: never
  CPython's, and on 64-bit Linux written on 12 hex digits (zero-padded), CPython's width there, so that a truncated
  repr (pydantic's `input_value`) is cut at the same place. On macOS addresses are not padded and their width
  depends on where each allocator places its heap (CPython's usually 9 digits, the binary's 9 to 11, with mimalloc
  or the system allocator alike): a truncated repr may be cut elsewhere in the logs. Compare logs on Linux.
- `typing.TypedDict` classes: calling one builds a plain `dict` (keys neither checked nor completed, as in
  CPython); as a Pydantic field or a response model, refused.
- `map`/`filter` return lists (materialized); `frozenset` behaves as `set`; `callable()`; a builtin exception

## asyncio and threading

- A call of an `async def` that is not awaited is a coroutine object (arguments evaluated at the call,
  body run at the first `await`; a second `await` raises like CPython). `asyncio.gather` (concurrent tasks,
  argument order, `return_exceptions`), `create_task` + `await task`, `add_done_callback`, `Semaphore`,
  `Lock`, `Queue`, `Event` (`set`, `clear`, `is_set`, `await wait()`), `Future` and `loop.create_future()` (`set_result`,
  `set_exception`, `result`, `exception`, `done`, `cancel`, `add_done_callback`, `await`; its `repr()` does not list the
  callbacks), `InvalidStateError`, `wait_for` (of a call, a task or a future), `sleep`, `to_thread`, `open_connection(host, port)` (the addresses of
  `getaddrinfo` tried in order like asyncio, its `ConnectionRefusedError`/`OSError("Multiple exceptions: ...")`
  messages: `[Errno 61] Connect call failed ('127.0.0.1', 9)` under asyncio, `[Errno 61] Connection refused`
  under uvloop, which uvicorn uses when the project's `uv.lock` installs it (`uvicorn[standard]`); `StreamWriter.write/drain/close/is_closing/wait_closed/get_extra_info("peername"|"sockname")`,
  `StreamReader.read/readline/at_eof`; `ssl=`, `limit=` and the other keywords are refused, and so are
  `readexactly`/`readuntil`). The runtime is multi-threaded: two tasks finishing at
  the same instant have no guaranteed order (asyncio follows creation order).
- The delays of `asyncio.sleep(delay, result)` and `asyncio.wait_for(aw, timeout)` are read like asyncio:
  `delay <= 0` first (a `str` raises its `TypeError`), then the loop's float time plus the delay (a `Decimal`
  raises `TypeError`, except for `asyncio.sleep` on uvloop, which rounds it); a timeout `<= 0` raises
  `TimeoutError` without running the awaitable. Differences: a NaN delay or timeout raises a py2axum
  `TypeError` (asyncio and the uvloop versions each do something else), and with a timeout `<= 0` the
  awaitable's arguments are not evaluated (asyncio evaluates them, then cancels the coroutine unstarted).
- `async with` / `with` follow CPython's protocol (the exception is passed to `__exit__`, a true result
  suppresses it).
- `threading.Lock/RLock/Event/Thread/get_ident`, `asyncio.new_event_loop()` + `run_forever()` in a
  thread, `call_soon`, `run_coroutine_threadsafe` + `wrap_future`, `loop.run_in_executor(None, f)`: emulated
  on tokio. The event loop is one "thread" (all coroutines share its ident); `Thread` and executor calls run
  on their own OS threads. Locks block like CPython's.
- `asyncio.run()` raises CPython's `RuntimeError` (always inside a running loop). Library calls that are not
  awaited (a bare `asyncio.sleep(1)`) run immediately.
- Async generators run in lockstep with their consumer like CPython: the body starts at the first
  `__anext__`, `anext()`, `asend`, `athrow`, `aclose` (`GeneratorExit` at the `yield`, `finally` blocks run).
  `@asynccontextmanager` follows `contextlib` (exception thrown in at the `yield`, `generator didn't yield`,
  `generator didn't stop`). `contextlib.suppress(*excs)`.
- `yield from it` as a statement of a generator, `it` a file opened by `with open(...) as f` in the same function,
  a literal or a builtin's result (`sorted`, `range`...): the same items as a `for` loop. Over a generator (which
  would also take `send()`/`throw()`/`close()` and give its return value) it is refused.
- A method of a builtin type read on the type (`str.lower`, `dict.get`) is a function: `sorted(xs, key=str.lower)`,
  a validator; called on another type it raises CPython's `descriptor ... doesn't apply to a ... object`.
- `task.cancel()`, `task.cancelled()`: `await task` then raises `CancelledError`. Difference: the task's
  coroutine is stopped at its current `await` without `CancelledError` being raised inside it (its own
  `except CancelledError` / `finally` blocks do not run).
- `contextvars.ContextVar` (`get` with or without default, `set`, `reset(token)`): values belong to the request
  being served (or to the lifespan). Difference: a task created with `create_task` shares its creator's
  values instead of a copy (a value it sets is seen by the creator).

The lifespan (`FastAPI(lifespan=...)`) is described in [Lifespan events](../tutorial/lifespan.md).

# Python semantics

py2axum compiles your project's code to Rust over a dynamic value type with CPython's semantics. This page
lists what is reproduced, what is refused (with `file:line`) and the known differences. The standard library is
in [Standard library](stdlib.md), third-party libraries in [Libraries](libraries.md).

## Values, functions, classes, modules

- Values and operators with CPython's semantics: int (64-bit: beyond is an `OverflowError`), float formatting
  and `repr`, str methods (`isdigit`, `isdecimal`, `isnumeric`, `isalpha`, `isalnum`, `isspace` from the Unicode
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
  lengths raises at the call, where CPython raises after yielding the common prefix.
- Functions: keyword/default/`*args`/`**kwargs` binding with CPython's `TypeError`s, closures, lambdas,
  nested functions and decorators (`functools.wraps`, decorator factories; a decorated function is built
  once at startup), recursion, `global` (one cell per process), generators, `match`.
  Refused: `nonlocal`, a project decorator on a method.
- Classes: plain classes (`__init__`, methods, properties, static/class methods, class attributes,
  `__slots__`; single inheritance from another plain class and/or `abc.ABC`, `@abstractmethod` (instantiating
  a class left abstract raises CPython's `TypeError` when called by name), `super().__init__(...)`), `@dataclass` (incl. `frozen=True`, `__post_init__`), exceptions (class attributes,
  methods, `super().__init__` of `HTTPException`), Enums (methods, `_missing_`). Special methods:
  `__str__`, `__repr__`, `__eq__` (used by `str()`, f-strings, `==`, `in`, `index`, `count`, `remove`),
  `__enter__/__exit__`, `__aenter__/__aexit__`; they must not perform I/O (`def`); other special methods
  are refused. In containers, an exception raised by `__eq__` counts as "not equal" (CPython propagates it).
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
- `map`/`filter` return lists (materialized); `frozenset` behaves as `set`; `callable()`; a builtin exception

## asyncio and threading

- A call of an `async def` that is not awaited is a coroutine object (arguments evaluated at the call,
  body run at the first `await`; a second `await` raises like CPython). `asyncio.gather` (concurrent tasks,
  argument order, `return_exceptions`), `create_task` + `await task`, `add_done_callback`, `Semaphore`,
  `Lock`, `Queue`, `wait_for`, `sleep`, `to_thread`, `open_connection(host, port)` (the addresses of
  `getaddrinfo` tried in order like asyncio, its `ConnectionRefusedError`/`OSError("Multiple exceptions: ...")`
  messages: `[Errno 61] Connect call failed ('127.0.0.1', 9)` under asyncio, `[Errno 61] Connection refused`
  under uvloop, which uvicorn uses when the project's `uv.lock` installs it (`uvicorn[standard]`); `StreamWriter.write/drain/close/is_closing/wait_closed/get_extra_info("peername"|"sockname")`,
  `StreamReader.read/readline/at_eof`; `ssl=`, `limit=` and the other keywords are refused, and so are
  `readexactly`/`readuntil`). The runtime is multi-threaded: two tasks finishing at
  the same instant have no guaranteed order (asyncio follows creation order).
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
- `task.cancel()`, `task.cancelled()`: `await task` then raises `CancelledError`. Difference: the task's
  coroutine is stopped at its current `await` without `CancelledError` being raised inside it (its own
  `except CancelledError` / `finally` blocks do not run).
- `contextvars.ContextVar` (`get` with or without default, `set`, `reset(token)`): values belong to the request
  being served (or to the lifespan). Difference: a task created with `create_task` shares its creator's
  values instead of a copy (a value it sets is seen by the creator).

The lifespan (`FastAPI(lifespan=...)`) is described in [Lifespan events](../tutorial/lifespan.md).

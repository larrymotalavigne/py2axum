- aiocache 0.12 translated from its source (`--translate-libs aiocache`): `Cache(Cache.REDIS / MEMORY, ...)`, `@cached`,
  serializers. What it needed, available to any translated library: `loop.call_later` (a TimerHandle), `obj.__getattribute__(name)`,
  `super().__init__(*args, **kwargs)` reaching `object`, a `try: import x / else: from ... import ...` guard (its
  `else` only when `x` is installed), `del` of an imported name at module level, library constructors called through a
  variable with keyword arguments, redis `ConnectionPool(...)`, `psetex`, `pexpire`, `execute_command`, `eval`,
  `pfadd`, `pfcount`.

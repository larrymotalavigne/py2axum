- `Request(scope)` without `receive` crashed the translator (`list index out of range`); it is now a Request
  whose body raises `RuntimeError` when read, as Starlette's `empty_receive`.

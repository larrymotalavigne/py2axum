- A Pydantic model's configuration given as class keywords (`class M(BaseModel, extra="allow")`) was ignored; it is
  applied after the body's `model_config`, as pydantic merges them.
- `deque.maxlen` could not be read; `is` between tuples, sets or frozensets compared nothing (always False).
